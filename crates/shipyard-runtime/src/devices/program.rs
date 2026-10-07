//! The device program: [`Devices`]' rules as eBPF, for cgroup v2.
//!
//! The kernel calls it with a `struct bpf_cgroup_dev_ctx`:
//!
//! | Offset | Field | |
//! |---|---|---|
//! | 0 | `access_type` | the type (`BPF_DEVCG_DEV_*`) in the low 16 bits, the access asked for (`BPF_DEVCG_ACC_*`) in the high 16 |
//! | 4 | `major` | |
//! | 8 | `minor` | |
//!
//! and takes 1 as "allowed", 0 as "denied" (EPERM).
//!
//! The access asked for is decided **bit by bit** (mknod, read, write),
//! one pass each, for the bits asked for. In a pass, the rules are walked
//! **from the last to the first**, and the first that covers the device and
//! that bit decides it: an allow goes on to the next bit, a deny ends with
//! 0. No rule covering it is a 0 too. All passes through, the answer is 1.
//! That's exactly [`Devices::allows`], which the tests check on random
//! rules.
//!
//! **Shaped for the kernel's verifier.** It walks every branch, and only
//! stops following a path where it meets another one whose registers it
//! can't tell apart. So no register's value may depend on which way a rule
//! went:
//!
//! - nothing is changed as rules match (a first version cleared the
//!   undecided bits in a register: no two paths looked alike, and a thousand
//!   rules took it past its million instructions);
//! - each rule loads the fields it compares from the context itself. After
//!   `if major != 8` falls through, the verifier knows the register holds 8,
//!   and a register kept for the next rule would carry that difference
//!   along. Loaded fresh, the old value is dead, and the paths meet again.

use super::ebpf::{Assembler, Instruction, Label, R1, Reg, Test};
use super::{Access, DeviceError, DeviceRule, Devices};

/// The access asked for.
const ACCESS: Reg = Reg(2);
/// Scratch: a field of the context a rule compares, or one bit of the
/// access, tested. Loaded right before each use.
const FIELD: Reg = Reg(3);

/// The context's fields: their offsets.
const ACCESS_TYPE: i16 = 0;
const MAJOR: i16 = 4;
const MINOR: i16 = 8;
/// `access_type`: the type is in its low 16 bits, the access above them.
const TYPE_MASK: u32 = 0xffff;
const ACCESS_SHIFT: u32 = 16;

/// `devices`' rules as a device program.
///
/// # Errors
///
/// Rules that don't validate, or a program too long (more rules than
/// [`super::MAX_RULES`] never get here).
pub(crate) fn program(devices: &Devices) -> Result<Vec<Instruction>, DeviceError> {
    devices.validate()?;
    let mut asm = Assembler::default();
    let deny = asm.label();
    asm.load_word(ACCESS, R1, ACCESS_TYPE);
    asm.rsh(ACCESS, ACCESS_SHIFT);
    // The verifier refuses a program with an instruction nothing reaches:
    // after a rule that covers every device, a pass ends, and the deny is
    // only there if something jumps to it.
    let mut denies = false;
    for bit in [Access::MKNOD, Access::READ, Access::WRITE] {
        let allowed = asm.label();
        // Not asked for: nothing to decide.
        asm.mov_reg(FIELD, ACCESS);
        asm.and(FIELD, bit.bits());
        asm.jump_if(Test::Equal, FIELD, 0, allowed);
        let mut covered = false;
        for rule in devices
            .rules
            .iter()
            .rev()
            .filter(|rule| rule.access.contains(bit))
        {
            let next = asm.label();
            skip_unless_covered(&mut asm, rule, next);
            asm.goto(if rule.allow { allowed } else { deny });
            denies |= !rule.allow;
            asm.bind(next);
            if rule.covers_every_device() {
                // The rules before it are never reached.
                covered = true;
                break;
            }
        }
        if !covered {
            // No rule covers it: denied.
            asm.goto(deny);
            denies = true;
        }
        asm.bind(allowed);
    }
    asm.ret(1);
    if denies {
        asm.bind(deny);
        asm.ret(0);
    }
    asm.assemble()
        .map_err(|_| DeviceError::TooManyRules(devices.rules.len()))
}

/// Goes to `next` (the rule before) unless the device is one `rule` covers:
/// each field it names, loaded fresh and compared.
fn skip_unless_covered(asm: &mut Assembler, rule: &DeviceRule, next: Label) {
    if let Some(kind) = rule.kind {
        asm.load_word(FIELD, R1, ACCESS_TYPE);
        asm.and(FIELD, TYPE_MASK);
        asm.jump_if(Test::NotEqual, FIELD, kind.code(), next);
    }
    if let Some(major) = rule.major {
        asm.load_word(FIELD, R1, MAJOR);
        asm.jump_if(Test::NotEqual, FIELD, major, next);
    }
    if let Some(minor) = rule.minor {
        asm.load_word(FIELD, R1, MINOR);
        asm.jump_if(Test::NotEqual, FIELD, minor, next);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::super::DeviceKind;
    use super::super::ebpf::{all_reachable, run};
    use super::*;

    /// The verdict `program` gives for `access` to `kind` `major`:`minor`.
    fn verdict(
        program: &[Instruction],
        kind: DeviceKind,
        major: u32,
        minor: u32,
        access: Access,
    ) -> bool {
        assert!(all_reachable(program), "unreachable code");
        let access_type = (access.bits() << ACCESS_SHIFT) | kind.code();
        match run(program, &[access_type, major, minor]) {
            0 => false,
            1 => true,
            other => panic!("a verdict that's neither: {other}"),
        }
    }

    #[test]
    fn dockers_defaults_as_a_program() {
        let devices = Devices::docker_default();
        let program = program(&devices).unwrap();
        let char = DeviceKind::Char;
        assert!(verdict(&program, char, 1, 3, Access::parse("rw").unwrap()));
        assert!(verdict(&program, char, 136, 9, Access::READ), "a terminal");
        assert!(verdict(&program, DeviceKind::Block, 8, 0, Access::MKNOD));
        assert!(!verdict(&program, DeviceKind::Block, 8, 0, Access::READ));
        assert!(!verdict(&program, char, 1, 11, Access::WRITE));
        // A few instructions a rule and bit: Docker's dozen stays small.
        assert!(program.len() < 250, "{} instructions", program.len());
    }

    /// Rules that never deny, or cover every device, still make a program
    /// with nothing out of reach.
    #[test]
    fn nothing_is_out_of_reach() {
        let mut devices = Devices::docker_default();
        devices.rules = vec![DeviceRule::allow(None, None, None, Access::ALL)];
        let allows = program(&devices).unwrap();
        assert!(verdict(&allows, DeviceKind::Block, 8, 0, Access::ALL));
        devices.rules = vec![DeviceRule::deny_all()];
        let denies = program(&devices).unwrap();
        assert!(!verdict(&denies, DeviceKind::Block, 8, 0, Access::READ));
    }

    #[test]
    fn no_rules_denies_everything() {
        let mut devices = Devices::docker_default();
        devices.rules.clear();
        let program = program(&devices).unwrap();
        assert!(!verdict(&program, DeviceKind::Char, 1, 3, Access::READ));
    }

    fn kind() -> impl Strategy<Value = DeviceKind> {
        prop_oneof![Just(DeviceKind::Char), Just(DeviceKind::Block)]
    }

    /// Small numbers, so that random rules and requests often meet.
    fn rule() -> impl Strategy<Value = DeviceRule> {
        (
            any::<bool>(),
            proptest::option::of(kind()),
            proptest::option::of(0u32..3),
            proptest::option::of(0u32..3),
            1u32..8,
        )
            .prop_map(|(allow, kind, major, minor, access)| {
                DeviceRule::new(allow, kind, major, minor, Access::from_bits(access))
            })
    }

    proptest! {
        /// The program means what the rules mean: for any rules and any
        /// request, its verdict is the model's.
        #[test]
        fn the_program_agrees_with_the_rules(
            rules in proptest::collection::vec(rule(), 0..12),
            kind in kind(),
            major in 0u32..3,
            minor in 0u32..3,
            access in 1u32..8,
        ) {
            let mut devices = Devices::docker_default();
            devices.rules = rules;
            let access = Access::from_bits(access);
            let program = program(&devices).unwrap();
            prop_assert_eq!(
                verdict(&program, kind, major, minor, access),
                devices.allows(kind, major, minor, access),
                "rules: {:?}", devices.rules
            );
        }
    }

    /// The longest program the most rules make (each with every access,
    /// type and number) still fits.
    #[test]
    fn the_longest_program_fits() {
        let mut devices = Devices::docker_default();
        devices.rules =
            vec![
                DeviceRule::allow(Some(DeviceKind::Block), Some(8), Some(0), Access::ALL);
                crate::devices::MAX_RULES
            ];
        assert!(program(&devices).is_ok());
    }

    #[test]
    fn the_most_rules_still_make_a_program() {
        let mut devices = Devices::docker_default();
        devices.rules =
            vec![
                DeviceRule::allow(Some(DeviceKind::Char), Some(1), Some(2), Access::ALL);
                crate::devices::MAX_RULES
            ];
        let program = program(&devices).unwrap();
        assert!(verdict(&program, DeviceKind::Char, 1, 2, Access::ALL));
    }
}
