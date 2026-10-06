//! seccomp: which system calls a container's command may make
//! (`seccomp(2)`).
//!
//! A filter is a small program in classic BPF, the packet-filter bytecode,
//! that the kernel runs on every system call the process makes. Its input is
//! the call's `struct seccomp_data`: the call number, the architecture and
//! the six arguments. Its output is a verdict: allow the call, fail it with an
//! errno, kill the process, and a few more. A filter is inherited across
//! `fork` and `execve` and can never be removed, so a container's command and
//! everything it starts stay under it.
//!
//! Profiles say what to allow in JSON (Docker's format, a superset of OCI's
//! `linux.seccomp`, in [`Profile`]). Shipyard compiles them to BPF itself
//! ([`compile`]), with no C library: names become this architecture's numbers
//! (`syscalls.rs`, generated from the libc crate and the kernel's tables).

mod bpf;
mod compile;
mod profile;

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub use bpf::Instruction;
pub use compile::{Filter, Target, compile};
#[cfg(target_os = "linux")]
pub(crate) use profile::kernel_version;
pub use profile::{Action, ArchMap, Arg, Condition, Operator, Profile, Rule, SeccompError};

use super::syscalls;

/// Docker's default profile (`github.com/moby/profiles/seccomp` v0.2.3, as
/// vendored by Docker 29.8; Apache-2.0, by the Moby authors): every call
/// most programs need, and none of the ones that reach into the kernel's
/// own state (`keyctl`, `bpf`, `mount`, …) unless a capability grants them.
const DOCKER_DEFAULT: &str = include_str!("docker-default.json");

/// Which seccomp filter a container's command runs under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Seccomp {
    /// None: every system call is open (`--security-opt seccomp=unconfined`,
    /// or a bundle without `linux.seccomp`).
    Unconfined,
    /// Docker's default profile, built in.
    Default,
    /// A profile of its own (`--security-opt seccomp=FILE`, or a bundle's
    /// `linux.seccomp`).
    Profile(Box<Profile>),
}

impl Seccomp {
    /// The profile to compile, if any.
    ///
    /// # Errors
    ///
    /// The built-in profile can't be read (a bug: a test reads it).
    pub fn profile(&self) -> Result<Option<Cow<'_, Profile>>, SeccompError> {
        match self {
            Self::Unconfined => Ok(None),
            Self::Default => Ok(Some(Cow::Owned(docker_default()?))),
            Self::Profile(profile) => Ok(Some(Cow::Borrowed(profile))),
        }
    }
}

/// Docker's default profile.
///
/// # Errors
///
/// It can't be read (a bug: a test reads it).
pub fn docker_default() -> Result<Profile, SeccompError> {
    serde_json::from_str(DOCKER_DEFAULT).map_err(|err| SeccompError::BuiltIn(err.to_string()))
}

/// An architecture Shipyard builds for, as a filter sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Arch {
    /// 64-bit Arm.
    Aarch64,
    /// 64-bit x86.
    X86_64,
}

impl Arch {
    /// This build's architecture, on Linux (`None` elsewhere: no seccomp).
    pub const NATIVE: Option<Self> = if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(Self::Aarch64)
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(Self::X86_64)
    } else {
        None
    };

    /// The number of the system call `name` here, as a filter sees it
    /// (`openat` is 56 on aarch64, 257 on x86-64). `None` for a name this
    /// architecture doesn't have (`open` on aarch64).
    #[must_use]
    pub fn number(self, name: &str) -> Option<u32> {
        let table = self.table();
        table
            .binary_search_by_key(&name, |&(known, _)| known)
            .ok()
            .map(|index| table[index].1)
    }

    /// Every system call here, by name, sorted.
    fn table(self) -> &'static [(&'static str, u32)] {
        match self {
            Self::Aarch64 => syscalls::AARCH64,
            Self::X86_64 => syscalls::X86_64,
        }
    }

    /// `AUDIT_ARCH_*`: how `seccomp_data.arch` names it (`<linux/audit.h>`:
    /// the ELF machine, 64-bit, little-endian).
    pub(crate) fn audit(self) -> u32 {
        match self {
            Self::Aarch64 => 0xC000_00B7,
            Self::X86_64 => 0xC000_003E,
        }
    }

    /// Its name in a Docker profile's `includes.arches` and `excludes.arches`
    /// (Docker's, from Go's `GOARCH`).
    pub(crate) fn docker_name(self) -> &'static str {
        match self {
            Self::Aarch64 => "arm64",
            Self::X86_64 => "amd64",
        }
    }
}

/// Whether `name` is a system call of any Linux architecture. A profile
/// written for several architectures names calls this one doesn't have
/// (`arm_fadvise64_64`): skipped. A name no architecture has is a mistake.
#[must_use]
pub fn syscall_known(name: &str) -> bool {
    syscalls::ANY_ARCHITECTURE.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: [Arch; 2] = [Arch::Aarch64, Arch::X86_64];

    #[test]
    fn every_architecture_has_the_calls_containers_use() {
        for arch in BOTH {
            for name in [
                "read",
                "write",
                "openat",
                "close",
                "execve",
                "execveat",
                "clone",
                "clone3",
                "mount",
                "mount_setattr",
                "pivot_root",
                "seccomp",
                "prctl",
                "capset",
                "setns",
                "unshare",
                "pidfd_open",
                "pidfd_send_signal",
                "close_range",
                "openat2",
            ] {
                assert!(arch.number(name).is_some(), "{arch:?} {name}");
            }
            assert_eq!(arch.number("no_such_call"), None);
        }
    }

    /// The numbers are the kernel's (checked against Linux 7.3's tables when
    /// generated); a few spot checks on each architecture.
    #[test]
    fn the_numbers_are_the_kernels() {
        for (name, aarch64, x86_64) in [
            ("read", 63, 0),
            ("openat", 56, 257),
            ("execve", 221, 59),
            ("clone3", 435, 435),
            ("seccomp", 277, 317),
            ("cachestat", 451, 451),
            ("fchmodat2", 452, 452),
            ("mseal", 462, 462),
            ("fchroot", 472, 472),
        ] {
            assert_eq!(Arch::Aarch64.number(name), Some(aarch64), "{name}");
            assert_eq!(Arch::X86_64.number(name), Some(x86_64), "{name}");
        }
        assert_eq!(Arch::X86_64.number("uretprobe"), Some(335));
        assert_eq!(Arch::X86_64.number("uprobe"), Some(336));
        assert_eq!(Arch::Aarch64.number("uretprobe"), None);
    }

    /// The table's numbers are libc's, wherever libc names the call (the
    /// generator reads libc's source; this checks it read it right).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_numbers_agree_with_libc() {
        let native = Arch::NATIVE.unwrap();
        for (name, number) in [
            ("read", libc::SYS_read),
            ("openat", libc::SYS_openat),
            ("execve", libc::SYS_execve),
            ("clone3", libc::SYS_clone3),
            ("mount_setattr", libc::SYS_mount_setattr),
            ("pidfd_send_signal", libc::SYS_pidfd_send_signal),
            ("seccomp", libc::SYS_seccomp),
            ("futex_waitv", libc::SYS_futex_waitv),
        ] {
            assert_eq!(native.number(name).map(i64::from), Some(number), "{name}");
        }
    }

    #[test]
    fn names_are_sorted_and_unique_and_numbers_too() {
        for arch in BOTH {
            let table = arch.table();
            assert!(
                table.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "{arch:?}"
            );
            let mut numbers: Vec<_> = table.iter().map(|&(_, number)| number).collect();
            numbers.sort_unstable();
            numbers.dedup();
            assert_eq!(
                numbers.len(),
                table.len(),
                "{arch:?}: two names, one number"
            );
        }
        let any = syscalls::ANY_ARCHITECTURE;
        assert!(any.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn the_legacy_calls_are_x86_64_only() {
        // aarch64 (the generic table) never had the old open/stat/fork.
        for name in ["open", "stat", "fork", "arch_prctl"] {
            assert!(Arch::X86_64.number(name).is_some(), "{name}");
            assert_eq!(Arch::Aarch64.number(name), None, "{name}");
        }
    }

    #[test]
    fn a_name_is_known_if_any_architecture_has_it() {
        for name in [
            "read",
            "open",
            "arm_fadvise64_64",
            "s390_runtime_instr",
            "set_tls",
            "riscv_flush_icache",
            "fchroot",
        ] {
            assert!(syscall_known(name), "{name}");
        }
        for name in ["opne", "READ", "", "no_such_call"] {
            assert!(!syscall_known(name), "{name}");
        }
    }

    #[test]
    fn docker_default_reads_and_names_only_real_calls() {
        let profile = docker_default().unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.default_action, Action::Errno);
        assert_eq!(profile.default_errno_ret, Some(1));
    }
}
