//! A profile, compiled to a filter for one container.
//!
//! The filter's shape:
//!
//! 1. **The architecture first.** `seccomp_data.arch` must be the native
//!    one, or the process is killed. A 64-bit x86 process can also make
//!    32-bit system calls (`int 0x80`), and those have *other numbers*: a
//!    filter that skipped this check would read `nr` 11 as x86-64's `munmap`
//!    when it is i386's `execve`. On x86-64, numbers with bit 30 set are the
//!    x32 ABI's (same architecture value, other calls): killed too, except
//!    -1, which a tracer sets to skip a call (as libseccomp lets it be).
//! 2. **A binary search over the call numbers** the rules name, so a call
//!    costs about a dozen comparisons (Docker's profile names about 300
//!    calls), not one per call named.
//! 3. **Each call's rules**, the most restrictive first (the kernel's own
//!    order between filters: kill, trap, errno, log, allow), so the verdict
//!    doesn't depend on the order rules are written in: of the rules whose
//!    arguments match, the most restrictive wins. A call no rule matches
//!    gets the default action.
//!
//! An argument is 64 bits and the machine has 32, so each comparison looks
//! at the high half first, then the low half (both architectures are
//! little-endian: the low half comes first in `seccomp_data`).

use std::collections::BTreeMap;

use super::bpf::{Assembler, Instruction, Label, Test};
use super::profile::{check_action, kernel_version};
use super::{Action, Arch, Arg, Condition, Operator, Profile, Rule, SeccompError};
use crate::security::CapabilitySet;

/// Where `seccomp_data` keeps the call's number…
const NR: u32 = 0;
/// …its architecture (`AUDIT_ARCH_*`)…
const ARCH: u32 = 4;
/// …and its six 64-bit arguments.
const ARGS: u32 = 16;

/// On x86-64, the bit that marks an x32 call (`__X32_SYSCALL_BIT`).
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
/// The number -1: no call (a tracer's way of skipping one).
const SKIPPED: u32 = u32::MAX;

/// `SECCOMP_RET_*`: the verdicts.
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_KILL_THREAD: u32 = 0;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

/// What an `SCMP_ACT_ERRNO` without an errno returns.
const EPERM: u32 = 1;

/// Who the filter is for: the rules' conditions depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The architecture.
    pub arch: Arch,
    /// The container's bounding set (what Docker's `caps` conditions see).
    pub bounding: CapabilitySet,
    /// The kernel's version, `(major, minor)`.
    pub kernel: (u32, u32),
}

/// A compiled filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// The program.
    pub program: Vec<Instruction>,
    /// The `SECCOMP_FILTER_FLAG_*` bits to install it with.
    pub flags: u32,
}

/// A rule that applies here, for one call number.
#[derive(Debug)]
struct Resolved<'a> {
    args: &'a [Arg],
    verdict: u32,
    rank: u8,
}

/// Compiles `profile` for `target`.
///
/// # Errors
///
/// The profile isn't valid ([`Profile::validate`]), or the filter is longer
/// than the kernel takes.
pub fn compile(profile: &Profile, target: &Target) -> Result<Filter, SeccompError> {
    profile.validate()?;
    let default = verdict(profile.default_action, profile.default_errno_ret)?;
    let calls = resolve(profile, target)?;

    let mut asm = Assembler::default();
    let mut returns = Returns::default();
    let kill = returns.label(&mut asm, RET_KILL_PROCESS);
    let otherwise = returns.label(&mut asm, default);
    check_architecture(&mut asm, target.arch, kill);

    // A call whose first rule has no conditions goes straight to its
    // verdict; the others get a block of their own.
    let mut leaves = Vec::with_capacity(calls.len());
    let mut blocks = Vec::new();
    for (&number, rules) in &calls {
        match rules.first() {
            Some(rule) if rule.args.is_empty() => {
                leaves.push((number, returns.label(&mut asm, rule.verdict)));
            }
            _ => {
                let block = asm.label();
                leaves.push((number, block));
                blocks.push((block, rules));
            }
        }
    }
    search(&mut asm, &leaves, otherwise);
    for (block, rules) in blocks {
        asm.bind(block);
        check_rules(&mut asm, rules, default);
    }
    returns.place(&mut asm);
    Ok(Filter {
        program: asm.assemble()?,
        flags: profile.flag_bits()?,
    })
}

/// The verdicts reached by a jump, each placed once at the end.
#[derive(Debug, Default)]
struct Returns(BTreeMap<u32, Label>);

impl Returns {
    /// Where `ret value` will be.
    fn label(&mut self, asm: &mut Assembler, value: u32) -> Label {
        *self.0.entry(value).or_insert_with(|| asm.label())
    }

    /// Places them.
    fn place(self, asm: &mut Assembler) {
        for (value, label) in self.0 {
            asm.bind(label);
            asm.ret(value);
        }
    }
}

/// Goes to `kill` unless the call is the native architecture's; leaves its
/// number in the accumulator.
fn check_architecture(asm: &mut Assembler, arch: Arch, kill: Label) {
    let native = asm.label();
    asm.load(ARCH);
    asm.jump(Test::Equal, arch.audit(), native, kill);
    asm.bind(native);
    asm.load(NR);
    if arch == Arch::X86_64 {
        let (high, not_x32) = (asm.label(), asm.label());
        asm.jump(Test::GreaterOrEqual, X32_SYSCALL_BIT, high, not_x32);
        asm.bind(high);
        asm.jump(Test::Equal, SKIPPED, not_x32, kill);
        asm.bind(not_x32);
    }
}

/// One call's rules, in order: the first whose arguments all hold gives the
/// verdict; if none does, `default`.
fn check_rules(asm: &mut Assembler, rules: &[Resolved<'_>], default: u32) {
    for rule in rules {
        let next = asm.label();
        for arg in rule.args {
            compare(asm, *arg, next);
        }
        asm.ret(rule.verdict);
        asm.bind(next);
    }
    // Unless the last rule had no conditions (its verdict is final).
    if rules.last().is_none_or(|rule| !rule.args.is_empty()) {
        asm.ret(default);
    }
}

/// The rules that apply to `target`, by call number, most restrictive
/// first (in the profile's order among equals). Rules after one without
/// argument conditions are dropped: they'd never be reached.
fn resolve<'a>(
    profile: &'a Profile,
    target: &Target,
) -> Result<BTreeMap<u32, Vec<Resolved<'a>>>, SeccompError> {
    let mut calls: BTreeMap<u32, Vec<Resolved<'a>>> = BTreeMap::new();
    for rule in &profile.syscalls {
        if !applies(rule, target)? {
            continue;
        }
        let verdict = verdict(rule.action, rule.errno_ret)?;
        for number in rule.all_names().filter_map(|name| target.arch.number(name)) {
            calls.entry(number).or_default().push(Resolved {
                args: &rule.args,
                verdict,
                rank: rank(rule.action),
            });
        }
    }
    for rules in calls.values_mut() {
        rules.sort_by_key(|rule| rule.rank);
        if let Some(last) = rules.iter().position(|rule| rule.args.is_empty()) {
            rules.truncate(last + 1);
        }
    }
    Ok(calls)
}

/// Whether Docker's conditions let `rule` apply to `target`: everything in
/// `includes` holds, and nothing in `excludes` does.
pub(super) fn applies(rule: &Rule, target: &Target) -> Result<bool, SeccompError> {
    if let Some(excludes) = &rule.excludes
        && any_holds(excludes, target)?
    {
        return Ok(false);
    }
    match &rule.includes {
        Some(includes) => all_hold(includes, target),
        None => Ok(true),
    }
}

/// Whether any part of `condition` holds (an `excludes`): the architecture
/// is one of its arches, a capability of its is in the bounding set, or the
/// kernel is at least its version.
fn any_holds(condition: &Condition, target: &Target) -> Result<bool, SeccompError> {
    if condition
        .arches
        .iter()
        .any(|name| name == target.arch.docker_name())
    {
        return Ok(true);
    }
    for cap in &condition.caps {
        if target.bounding.contains(cap.parse()?) {
            return Ok(true);
        }
    }
    Ok(kernel_at_least(condition, target)? == Some(true))
}

/// Whether every part of `condition` holds (an `includes`): the
/// architecture is among its arches (if it names any), every capability of
/// its is in the bounding set, and the kernel is at least its version.
fn all_hold(condition: &Condition, target: &Target) -> Result<bool, SeccompError> {
    let arch = target.arch.docker_name();
    if !condition.arches.is_empty() && !condition.arches.iter().any(|name| name == arch) {
        return Ok(false);
    }
    for cap in &condition.caps {
        if !target.bounding.contains(cap.parse()?) {
            return Ok(false);
        }
    }
    Ok(kernel_at_least(condition, target)? != Some(false))
}

/// Whether the kernel is at least `condition`'s `minKernel`, if it has one.
fn kernel_at_least(condition: &Condition, target: &Target) -> Result<Option<bool>, SeccompError> {
    condition
        .min_kernel
        .as_ref()
        .map(|version| {
            kernel_version(version)
                .map(|min| target.kernel >= min)
                .ok_or_else(|| SeccompError::MinKernel(version.clone()))
        })
        .transpose()
}

/// The `SECCOMP_RET_*` value for `action`.
pub(super) fn verdict(action: Action, errno: Option<u32>) -> Result<u32, SeccompError> {
    check_action(action, errno)?;
    Ok(match action {
        Action::Allow => RET_ALLOW,
        Action::Log => RET_LOG,
        Action::Errno => RET_ERRNO | errno.unwrap_or(EPERM),
        Action::Trap => RET_TRAP,
        Action::Kill | Action::KillThread => RET_KILL_THREAD,
        // Trace and Notify are refused by `check_action`: the safest
        // verdict all the same.
        Action::KillProcess | Action::Trace | Action::Notify => RET_KILL_PROCESS,
    })
}

/// How restrictive `action` is, 0 the most: the kernel's precedence
/// between filters (`seccomp(2)`).
pub(super) fn rank(action: Action) -> u8 {
    match action {
        Action::KillProcess | Action::Trace | Action::Notify => 0,
        Action::Kill | Action::KillThread => 1,
        Action::Trap => 2,
        Action::Errno => 3,
        Action::Log => 4,
        Action::Allow => 5,
    }
}

/// Jumps to the label of the number in the accumulator, or to `otherwise`:
/// a binary search over `leaves`, sorted by number, ending in a few
/// equality tests.
fn search(asm: &mut Assembler, leaves: &[(u32, Label)], otherwise: Label) {
    if leaves.len() > 4 {
        let middle = leaves.len() / 2;
        let (low, high) = (asm.label(), asm.label());
        asm.jump(Test::GreaterOrEqual, leaves[middle].0, high, low);
        asm.bind(low);
        search(asm, &leaves[..middle], otherwise);
        asm.bind(high);
        search(asm, &leaves[middle..], otherwise);
        return;
    }
    let Some(((last, last_target), rest)) = leaves.split_last() else {
        asm.goto(otherwise);
        return;
    };
    for &(number, target) in rest {
        let next = asm.label();
        asm.jump(Test::Equal, number, target, next);
        asm.bind(next);
    }
    asm.jump(Test::Equal, *last, *last_target, otherwise);
}

/// Falls through if `arg` holds, goes to `fail` if not.
fn compare(asm: &mut Assembler, arg: Arg, fail: Label) {
    let low = ARGS + 8 * arg.index;
    let high = low + 4;
    let halves = |value: u64| {
        let [l0, l1, l2, l3, h0, h1, h2, h3] = value.to_le_bytes();
        (
            u32::from_le_bytes([l0, l1, l2, l3]),
            u32::from_le_bytes([h0, h1, h2, h3]),
        )
    };
    let (value_low, value_high) = halves(arg.value);
    let (pass, low_half) = (asm.label(), asm.label());
    asm.load(high);
    match arg.op {
        Operator::Equal | Operator::NotEqual => {
            let (equal, unequal) = if arg.op == Operator::Equal {
                (pass, fail)
            } else {
                (fail, pass)
            };
            asm.jump(Test::Equal, value_high, low_half, unequal);
            asm.bind(low_half);
            asm.load(low);
            asm.jump(Test::Equal, value_low, equal, unequal);
        }
        Operator::Greater | Operator::GreaterOrEqual => {
            // High half above: yes. Equal: the low half decides. Below: no.
            let same_high = asm.label();
            asm.jump(Test::Greater, value_high, pass, same_high);
            asm.bind(same_high);
            asm.jump(Test::Equal, value_high, low_half, fail);
            asm.bind(low_half);
            asm.load(low);
            let test = if arg.op == Operator::Greater {
                Test::Greater
            } else {
                Test::GreaterOrEqual
            };
            asm.jump(test, value_low, pass, fail);
        }
        Operator::Less | Operator::LessOrEqual => {
            // High half above: no. Equal: the low half decides. Below: yes.
            let same_high = asm.label();
            asm.jump(Test::Greater, value_high, fail, same_high);
            asm.bind(same_high);
            asm.jump(Test::Equal, value_high, low_half, pass);
            asm.bind(low_half);
            asm.load(low);
            // `<` is "not `>=`", `<=` is "not `>`".
            let test = if arg.op == Operator::Less {
                Test::GreaterOrEqual
            } else {
                Test::Greater
            };
            asm.jump(test, value_low, fail, pass);
        }
        Operator::MaskedEqual => {
            let (wanted_low, wanted_high) = halves(arg.value_two);
            asm.and(value_high);
            asm.jump(Test::Equal, wanted_high, low_half, fail);
            asm.bind(low_half);
            asm.load(low);
            asm.and(value_low);
            asm.jump(Test::Equal, wanted_low, pass, fail);
        }
    }
    asm.bind(pass);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::super::bpf::run;
    use super::super::docker_default;
    use super::*;

    const I386: u32 = 0x4000_0003;
    const EACCES: u32 = 13;

    /// A call: its architecture value and number, and its arguments.
    type Call = ((u32, u32), [u64; 6]);

    /// A `struct seccomp_data`.
    fn data(arch: u32, nr: u32, args: [u64; 6]) -> [u8; 64] {
        let mut data = [0; 64];
        data[0..4].copy_from_slice(&nr.to_le_bytes());
        data[4..8].copy_from_slice(&arch.to_le_bytes());
        for (index, arg) in args.iter().enumerate() {
            let at = 16 + 8 * index;
            data[at..at + 8].copy_from_slice(&arg.to_le_bytes());
        }
        data
    }

    /// The verdict, worked out from the profile directly: the meaning the
    /// filter must have.
    fn reference(profile: &Profile, target: &Target, arch: u32, nr: u32, args: [u64; 6]) -> u32 {
        let x32 = target.arch == Arch::X86_64 && nr >= X32_SYSCALL_BIT && nr != SKIPPED;
        if arch != target.arch.audit() || x32 {
            return RET_KILL_PROCESS;
        }
        let holds = |arg: &Arg| {
            let value = args[usize::try_from(arg.index).unwrap()];
            match arg.op {
                Operator::NotEqual => value != arg.value,
                Operator::Less => value < arg.value,
                Operator::LessOrEqual => value <= arg.value,
                Operator::Equal => value == arg.value,
                Operator::GreaterOrEqual => value >= arg.value,
                Operator::Greater => value > arg.value,
                Operator::MaskedEqual => value & arg.value == arg.value_two,
            }
        };
        let mut best: Option<(u8, u32)> = None;
        for rule in &profile.syscalls {
            let names_it = rule
                .all_names()
                .any(|name| target.arch.number(name) == Some(nr));
            if names_it && applies(rule, target).unwrap() && rule.args.iter().all(holds) {
                let found = (
                    rank(rule.action),
                    verdict(rule.action, rule.errno_ret).unwrap(),
                );
                if best.is_none_or(|(rank, _)| found.0 < rank) {
                    best = Some(found);
                }
            }
        }
        best.map_or_else(
            || verdict(profile.default_action, profile.default_errno_ret).unwrap(),
            |(_, verdict)| verdict,
        )
    }

    fn target(arch: Arch, caps: &[&str]) -> Target {
        Target {
            arch,
            bounding: caps.iter().map(|cap| cap.parse().unwrap()).collect(),
            kernel: (6, 12),
        }
    }

    fn profile(json: &str) -> Profile {
        serde_json::from_str(json).unwrap()
    }

    fn verdict_of(filter: &Filter, arch: Arch, name: &str, args: [u64; 6]) -> u32 {
        run(
            &filter.program,
            &data(arch.audit(), arch.number(name).unwrap(), args),
        )
    }

    fn docker(arch: Arch, caps: &[&str]) -> Filter {
        compile(&docker_default().unwrap(), &target(arch, caps)).unwrap()
    }

    const DOCKER_CAPS: [&str; 14] = [
        "CHOWN",
        "DAC_OVERRIDE",
        "FSETID",
        "FOWNER",
        "MKNOD",
        "NET_RAW",
        "SETGID",
        "SETUID",
        "SETFCAP",
        "SETPCAP",
        "NET_BIND_SERVICE",
        "SYS_CHROOT",
        "KILL",
        "AUDIT_WRITE",
    ];

    #[test]
    fn dockers_profile_allows_what_programs_need() {
        for arch in [Arch::Aarch64, Arch::X86_64] {
            let filter = docker(arch, &DOCKER_CAPS);
            for name in ["read", "write", "openat", "execve", "exit_group", "futex"] {
                assert_eq!(
                    verdict_of(&filter, arch, name, [0; 6]),
                    RET_ALLOW,
                    "{arch:?} {name}"
                );
            }
            // `chroot` comes with CAP_SYS_CHROOT, in Docker's default set.
            assert_eq!(verdict_of(&filter, arch, "chroot", [0; 6]), RET_ALLOW);
        }
        let x86 = docker(Arch::X86_64, &DOCKER_CAPS);
        assert_eq!(
            verdict_of(&x86, Arch::X86_64, "arch_prctl", [0; 6]),
            RET_ALLOW
        );
    }

    #[test]
    fn dockers_profile_refuses_the_kernels_own_state() {
        for arch in [Arch::Aarch64, Arch::X86_64] {
            let filter = docker(arch, &DOCKER_CAPS);
            for name in [
                "keyctl",
                "add_key",
                "bpf",
                "mount",
                "unshare",
                "setns",
                "init_module",
            ] {
                let got = verdict_of(&filter, arch, name, [0; 6]);
                assert_eq!(got, RET_ERRNO | EPERM, "{arch:?} {name}");
            }
        }
    }

    #[test]
    fn dockers_profile_follows_the_capabilities() {
        const CLONE_NEWUSER: u64 = 0x1000_0000;
        const ENOSYS: u32 = 38;
        for arch in [Arch::Aarch64, Arch::X86_64] {
            let plain = docker(arch, &DOCKER_CAPS);
            // A thread (no namespace flags) is fine; a new user namespace isn't.
            assert_eq!(
                verdict_of(&plain, arch, "clone", [0x3d_0f00, 0, 0, 0, 0, 0]),
                RET_ALLOW
            );
            assert_eq!(
                verdict_of(&plain, arch, "clone", [CLONE_NEWUSER, 0, 0, 0, 0, 0]),
                RET_ERRNO | EPERM
            );
            // clone3's flags are in memory a filter can't read: ENOSYS, so
            // the C library falls back to clone.
            assert_eq!(
                verdict_of(&plain, arch, "clone3", [0; 6]),
                RET_ERRNO | ENOSYS
            );

            let mut admin_caps = DOCKER_CAPS.to_vec();
            admin_caps.push("SYS_ADMIN");
            let admin = docker(arch, &admin_caps);
            assert_eq!(
                verdict_of(&admin, arch, "clone", [CLONE_NEWUSER, 0, 0, 0, 0, 0]),
                RET_ALLOW
            );
            for name in ["clone3", "mount", "unshare", "setns"] {
                assert_eq!(verdict_of(&admin, arch, name, [0; 6]), RET_ALLOW, "{name}");
            }
        }
    }

    #[test]
    fn dockers_profile_checks_personality_on_64_bits() {
        let filter = docker(Arch::X86_64, &DOCKER_CAPS);
        for (persona, wanted) in [
            (0, RET_ALLOW),
            (8, RET_ALLOW),
            (0xffff_ffff, RET_ALLOW),
            // Same low half, other high half: not 0xffffffff.
            (0x1_ffff_ffff, RET_ERRNO | EPERM),
            (0x0040_0000, RET_ERRNO | EPERM),
        ] {
            assert_eq!(
                verdict_of(
                    &filter,
                    Arch::X86_64,
                    "personality",
                    [persona, 0, 0, 0, 0, 0]
                ),
                wanted,
                "{persona:#x}"
            );
        }
    }

    /// Docker's `socket` rules allow every domain but `AF_NETLINK` (38) and
    /// `AF_VSOCK` (40). The kernel reads the domain (an `int`) on 32 bits,
    /// so the same domains with high bits set are refused too, where
    /// 64-bit comparisons alone would allow them (`0x1_0000_0028 > 40`).
    /// The usual domains, which never have them, still go through.
    #[test]
    fn dockers_socket_rules_hold_with_high_bits_set() {
        for arch in [Arch::Aarch64, Arch::X86_64] {
            let filter = docker(arch, &DOCKER_CAPS);
            let socket = |domain: u64| verdict_of(&filter, arch, "socket", [domain, 0, 0, 0, 0, 0]);
            for domain in [1, 2, 10, 17, 39, 41, 45] {
                assert_eq!(socket(domain), RET_ALLOW, "{arch:?} domain {domain}");
            }
            for domain in [
                38,
                40,
                0x1_0000_0026,
                0x1_0000_0028,
                0xffff_ffff_0000_0028,
                0x8000_0000_0000_0002,
            ] {
                assert_eq!(
                    socket(domain),
                    RET_ERRNO | EPERM,
                    "{arch:?} domain {domain:#x}"
                );
            }
        }
    }

    /// A bundle's profile means exactly what it says, on 64 bits, as
    /// libseccomp has it: only the built-in profile adds the guard.
    #[test]
    fn a_bundles_comparisons_stay_on_64_bits() {
        let p = profile(
            r#"{"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [{"names": ["socket"],
                "action": "SCMP_ACT_ALLOW",
                "args": [{"index": 0, "value": 40, "op": "SCMP_CMP_GT"}]}]}"#,
        );
        let filter = compile(&p, &target(Arch::X86_64, &[])).unwrap();
        let socket = |domain| verdict_of(&filter, Arch::X86_64, "socket", [domain, 0, 0, 0, 0, 0]);
        assert_eq!(socket(0x1_0000_0028), RET_ALLOW);
        assert_eq!(socket(40), RET_ERRNO | EPERM);
    }

    /// Every rule of the built-in profile that allows a call when an
    /// argument is `>`, `>=` or `!=` a value also requires that argument's
    /// high half to be 0: high bits the kernel ignores (an `int`'s) would
    /// otherwise pass it. (They only make a value bigger, so an allowing
    /// `<`, `<=` or `==` can't be passed that way; and the profile's rules
    /// on arguments all allow, which the reasoning needs.) A newer copy of
    /// Docker's profile without the guard fails here. (There is such a rule:
    /// the check isn't vacuous.)
    #[test]
    fn dockers_profile_guards_the_high_half_of_compared_arguments() {
        const HIGH: u64 = 0xffff_ffff_0000_0000;
        let mut guarded = 0;
        for rule in docker_default().unwrap().syscalls {
            if rule.args.is_empty() {
                continue;
            }
            assert_eq!(rule.action, Action::Allow, "{:?}", rule.names);
            for arg in &rule.args {
                if !matches!(
                    arg.op,
                    Operator::Greater | Operator::GreaterOrEqual | Operator::NotEqual
                ) {
                    continue;
                }
                guarded += 1;
                assert!(
                    rule.args.iter().any(|guard| guard.index == arg.index
                        && guard.op == Operator::MaskedEqual
                        && guard.value == HIGH
                        && guard.value_two == 0),
                    "{:?}: argument {} has no high-half guard",
                    rule.names,
                    arg.index
                );
            }
        }
        assert!(guarded >= 1, "{guarded}");
    }

    #[test]
    fn other_architectures_and_x32_are_killed() {
        let x86 = docker(Arch::X86_64, &DOCKER_CAPS);
        // i386's execve is 11: x86-64's munmap. Killed, not allowed.
        assert_eq!(run(&x86.program, &data(I386, 11, [0; 6])), RET_KILL_PROCESS);
        let read = Arch::X86_64.number("read").unwrap();
        assert_eq!(
            run(
                &x86.program,
                &data(Arch::X86_64.audit(), X32_SYSCALL_BIT | read, [0; 6])
            ),
            RET_KILL_PROCESS
        );
        // -1 (a tracer skipping a call) isn't x32: the default action.
        assert_eq!(
            run(&x86.program, &data(Arch::X86_64.audit(), SKIPPED, [0; 6])),
            RET_ERRNO | EPERM
        );
        let arm = docker(Arch::Aarch64, &DOCKER_CAPS);
        assert_eq!(
            run(&arm.program, &data(Arch::X86_64.audit(), 0, [0; 6])),
            RET_KILL_PROCESS
        );
    }

    #[test]
    fn dockers_profile_fits_with_room_to_spare() {
        for arch in [Arch::Aarch64, Arch::X86_64] {
            for caps in [&[][..], &DOCKER_CAPS[..]] {
                let len = docker(arch, caps).program.len();
                assert!(len < 1500, "{arch:?}: {len} instructions");
            }
            let all: Vec<&str> = CapabilitySet::ALL
                .iter()
                .map(crate::security::Capability::name)
                .collect();
            let len = docker(arch, &all).program.len();
            assert!(len < 1500, "{arch:?}, every capability: {len} instructions");
        }
    }

    #[test]
    fn the_most_restrictive_matching_rule_wins_whatever_the_order() {
        for rules in [
            r#"[{"names": ["read"], "action": "SCMP_ACT_ALLOW"},
                {"names": ["read"], "action": "SCMP_ACT_ERRNO", "errnoRet": 13,
                 "args": [{"index": 0, "value": 3, "op": "SCMP_CMP_EQ"}]}]"#,
            r#"[{"names": ["read"], "action": "SCMP_ACT_ERRNO", "errnoRet": 13,
                 "args": [{"index": 0, "value": 3, "op": "SCMP_CMP_EQ"}]},
                {"names": ["read"], "action": "SCMP_ACT_ALLOW"}]"#,
        ] {
            let p = profile(&format!(
                r#"{{"defaultAction": "SCMP_ACT_KILL_PROCESS", "syscalls": {rules}}}"#
            ));
            let filter = compile(&p, &target(Arch::Aarch64, &[])).unwrap();
            assert_eq!(
                verdict_of(&filter, Arch::Aarch64, "read", [3, 0, 0, 0, 0, 0]),
                RET_ERRNO | EACCES
            );
            assert_eq!(
                verdict_of(&filter, Arch::Aarch64, "read", [4, 0, 0, 0, 0, 0]),
                RET_ALLOW
            );
            assert_eq!(
                verdict_of(&filter, Arch::Aarch64, "write", [0; 6]),
                RET_KILL_PROCESS
            );
        }
    }

    #[test]
    fn conditions_choose_the_rules() {
        let p = profile(
            r#"{"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [
                {"names": ["read"], "action": "SCMP_ACT_ALLOW", "includes": {"minKernel": "7.0"}},
                {"names": ["write"], "action": "SCMP_ACT_ALLOW", "excludes": {"minKernel": "6.12"}},
                {"names": ["close"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["arm64"]}},
                {"names": ["dup"], "action": "SCMP_ACT_ALLOW", "excludes": {"arches": ["arm64"]}},
                {"names": ["dup3"], "action": "SCMP_ACT_ALLOW",
                 "includes": {"caps": ["CAP_KILL", "CAP_CHOWN"]}},
                {"names": ["openat"], "action": "SCMP_ACT_ALLOW",
                 "excludes": {"caps": ["CAP_SYS_ADMIN", "CAP_CHOWN"]}}
            ]}"#,
        );
        let arm = Arch::Aarch64;
        let check = |caps: &[&str], name: &str| {
            let filter = compile(&p, &target(arm, caps)).unwrap();
            verdict_of(&filter, arm, name, [0; 6]) == RET_ALLOW
        };
        assert!(!check(&[], "read"), "kernel 6.12 < 7.0");
        assert!(!check(&[], "write"), "kernel 6.12 >= 6.12: excluded");
        assert!(check(&[], "close"));
        assert!(!check(&[], "dup"));
        assert!(check(&["KILL", "CHOWN"], "dup3"));
        assert!(!check(&["KILL"], "dup3"), "every capability is needed");
        assert!(check(&["KILL"], "openat"));
        assert!(!check(&["CHOWN"], "openat"), "any capability excludes");
    }

    #[test]
    fn names_this_architecture_lacks_are_skipped() {
        let p = profile(
            r#"{"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [
                {"names": ["open", "arm_fadvise64_64", "openat"], "action": "SCMP_ACT_ALLOW"}]}"#,
        );
        let filter = compile(&p, &target(Arch::Aarch64, &[])).unwrap();
        assert_eq!(
            verdict_of(&filter, Arch::Aarch64, "openat", [0; 6]),
            RET_ALLOW
        );
    }

    #[test]
    fn errno_defaults_to_eperm_and_flags_pass_through() {
        let p = profile(
            r#"{"defaultAction": "SCMP_ACT_ERRNO", "flags": ["SECCOMP_FILTER_FLAG_LOG"],
                "syscalls": [{"names": ["read"], "action": "SCMP_ACT_ERRNO"}]}"#,
        );
        let filter = compile(&p, &target(Arch::X86_64, &[])).unwrap();
        assert_eq!(
            verdict_of(&filter, Arch::X86_64, "read", [0; 6]),
            RET_ERRNO | EPERM
        );
        assert_eq!(
            verdict_of(&filter, Arch::X86_64, "write", [0; 6]),
            RET_ERRNO | EPERM
        );
        assert_eq!(filter.flags, 2);
    }

    #[test]
    fn a_profile_too_big_for_the_kernel_is_refused() {
        // 6 conditions of 5 instructions each, on 200 rules for one call.
        let arg = |index| format!(r#"{{"index": {index}, "value": 1, "op": "SCMP_CMP_LT"}}"#);
        let args: Vec<String> = (0..6).map(arg).collect();
        let rule = format!(
            r#"{{"names": ["read"], "action": "SCMP_ACT_ERRNO", "args": [{}]}}"#,
            args.join(",")
        );
        let rules = vec![rule; 200].join(",");
        let p = profile(&format!(
            r#"{{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{rules}]}}"#
        ));
        let err = compile(&p, &target(Arch::Aarch64, &[])).unwrap_err();
        assert!(matches!(err, SeccompError::TooLong(_)), "{err}");
    }

    // The filter means what the profile says, for random profiles and calls:
    // every operator, both halves of every argument, conditions, rule
    // order, both architectures, foreign architectures and x32.

    const NAMES: [&str; 8] = [
        "read",
        "write",
        "close",
        "openat",
        "open",
        "dup",
        "arch_prctl",
        "ioctl",
    ];
    const VALUES: [u64; 8] = [
        0,
        1,
        2,
        0xffff_ffff,
        0x1_0000_0000,
        0x1_0000_0001,
        u64::MAX,
        0x8000_0000,
    ];

    fn an_action() -> impl Strategy<Value = (Action, Option<u32>)> {
        prop_oneof![
            Just((Action::Allow, None)),
            Just((Action::Log, None)),
            Just((Action::Trap, None)),
            Just((Action::Kill, None)),
            Just((Action::KillProcess, None)),
            (0u32..40).prop_map(|errno| (Action::Errno, Some(errno))),
            Just((Action::Errno, None)),
        ]
    }

    fn an_operator() -> impl Strategy<Value = Operator> {
        prop_oneof![
            Just(Operator::NotEqual),
            Just(Operator::Less),
            Just(Operator::LessOrEqual),
            Just(Operator::Equal),
            Just(Operator::GreaterOrEqual),
            Just(Operator::Greater),
            Just(Operator::MaskedEqual),
        ]
    }

    fn a_value() -> impl Strategy<Value = u64> {
        prop_oneof![prop::sample::select(&VALUES[..]), any::<u64>()]
    }

    fn an_arg() -> impl Strategy<Value = Arg> {
        (0u32..3, a_value(), a_value(), an_operator()).prop_map(|(index, value, value_two, op)| {
            Arg {
                index,
                value,
                value_two,
                op,
            }
        })
    }

    fn a_condition() -> impl Strategy<Value = Option<Condition>> {
        let caps = prop::sample::subsequence(vec!["CAP_KILL", "CAP_CHOWN"], 0..=2);
        let arches = prop::sample::subsequence(vec!["arm64", "amd64", "s390x"], 0..=1);
        let kernel = prop::option::of(prop::sample::select(vec!["4.8", "6.12", "7.1"]));
        prop::option::of(
            (caps, arches, kernel).prop_map(|(caps, arches, kernel)| Condition {
                caps: caps.into_iter().map(String::from).collect(),
                arches: arches.into_iter().map(String::from).collect(),
                min_kernel: kernel.map(String::from),
            }),
        )
    }

    fn a_rule() -> impl Strategy<Value = Rule> {
        (
            prop::sample::subsequence(&NAMES[..], 1..=3),
            an_action(),
            prop::collection::vec(an_arg(), 0..=3),
            a_condition(),
            a_condition(),
        )
            .prop_map(
                |(names, (action, errno_ret), args, includes, excludes)| Rule {
                    names: names.into_iter().map(String::from).collect(),
                    name: None,
                    action,
                    errno_ret,
                    args,
                    comment: None,
                    includes,
                    excludes,
                },
            )
    }

    fn a_profile() -> impl Strategy<Value = Profile> {
        (an_action(), prop::collection::vec(a_rule(), 0..=12)).prop_map(
            |((default_action, default_errno_ret), syscalls)| Profile {
                default_action,
                default_errno_ret,
                architectures: Vec::new(),
                arch_map: Vec::new(),
                flags: Vec::new(),
                listener_path: None,
                listener_metadata: None,
                syscalls,
            },
        )
    }

    /// A call: its architecture value and number, mostly this
    /// architecture's and named by the rules, sometimes anything.
    fn a_call(arch: Arch) -> impl Strategy<Value = (u32, u32)> {
        let named = prop::sample::select(&NAMES[..])
            .prop_filter_map("not here", move |name| arch.number(name));
        let audit = prop_oneof![8 => Just(arch.audit()), 1 => Just(I386), 1 => any::<u32>()];
        let nr = prop_oneof![
            8 => named,
            1 => 0u32..500,
            1 => (0u32..500).prop_map(|n| X32_SYSCALL_BIT | n),
            1 => Just(SKIPPED),
        ];
        (audit, nr)
    }

    /// An architecture, and calls on it with their arguments.
    fn calls() -> impl Strategy<Value = (Arch, Vec<Call>)> {
        prop::sample::select(vec![Arch::Aarch64, Arch::X86_64]).prop_flat_map(|arch| {
            let args = (a_value(), a_value(), a_value()).prop_map(|(a, b, c)| [a, b, c, 0, 0, 0]);
            (
                Just(arch),
                prop::collection::vec((a_call(arch), args), 1..=8),
            )
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn the_filter_means_what_the_profile_says(
            profile in a_profile(),
            (arch, calls) in calls(),
            caps in prop::sample::subsequence(vec!["KILL", "CHOWN"], 0..=2),
            kernel in prop::sample::select(vec![(4, 7), (6, 12), (7, 3)]),
        ) {
            let target = Target { kernel, ..target(arch, &caps) };
            let filter = compile(&profile, &target).unwrap();
            for ((audit, nr), args) in calls {
                prop_assert_eq!(
                    run(&filter.program, &data(audit, nr, args)),
                    reference(&profile, &target, audit, nr, args),
                    "arch {:?} nr {} args {:x?}", arch, nr, args
                );
            }
        }
    }
}
