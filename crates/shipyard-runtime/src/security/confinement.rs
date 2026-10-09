//! What a container's command may do: its five capability sets, and
//! `no_new_privs` (TLPI ch. 39, `capabilities(7)`).
//!
//! A process has five sets:
//!
//! - **bounding:** the most it can ever have, even through a setuid-root
//!   program or a file capability (step 1's probe, C5);
//! - **permitted:** what it may use;
//! - **effective:** what it's using now (what the kernel checks);
//! - **inheritable:** what may pass to a program it runs, with ambient;
//! - **ambient:** what a non-root program keeps across `exec` (C4).

use serde::{Deserialize, Serialize};

use super::Capability;
use super::CapabilitySet;
use super::Seccomp;
#[cfg(target_os = "linux")]
use super::seccomp::{Arch, Instruction, SeccompError, Target, compile};

/// A process's five capability sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilitySets {
    /// The most the process can ever have.
    pub bounding: CapabilitySet,
    /// What it's using now.
    pub effective: CapabilitySet,
    /// What it may use.
    pub permitted: CapabilitySet,
    /// What may pass to a program it runs (with ambient).
    pub inheritable: CapabilitySet,
    /// What a non-root program keeps across `exec`.
    pub ambient: CapabilitySet,
}

impl CapabilitySets {
    /// Checks the sets against `capset(2)`'s rules, which the kernel would
    /// otherwise enforce only at the start, with a bare EPERM:
    ///
    /// - effective is part of permitted (a process uses only what it's
    ///   permitted);
    /// - inheritable is part of bounding (nothing passes on beyond it);
    /// - ambient is part of both permitted and inheritable.
    ///
    /// # Errors
    ///
    /// The first rule broken, naming the capability.
    pub fn check(self) -> Result<(), String> {
        let outside =
            |set: CapabilitySet, of: CapabilitySet| set.iter().find(|cap| !of.contains(*cap));
        if let Some(cap) = outside(self.effective, self.permitted) {
            return Err(format!(
                "effective has {cap}, which permitted hasn't (a process can only use what \
                 it's permitted)"
            ));
        }
        if let Some(cap) = outside(self.inheritable, self.bounding) {
            return Err(format!(
                "inheritable has {cap}, which bounding hasn't (nothing passes on beyond the \
                 bounding set)"
            ));
        }
        let both: CapabilitySet = self
            .permitted
            .iter()
            .filter(|cap| self.inheritable.contains(*cap))
            .collect();
        if let Some(cap) = outside(self.ambient, both) {
            return Err(format!(
                "ambient has {cap}, which isn't in both permitted and inheritable (the kernel \
                 needs it in both)"
            ));
        }
        Ok(())
    }

    /// The sets with only the capabilities up to `last_cap`.
    #[cfg(target_os = "linux")]
    fn up_to(self, last_cap: u8) -> Self {
        let keep = |set: CapabilitySet| set.iter().filter(|cap| cap.number() <= last_cap).collect();
        Self {
            bounding: keep(self.bounding),
            effective: keep(self.effective),
            permitted: keep(self.permitted),
            inheritable: keep(self.inheritable),
            ambient: keep(self.ambient),
        }
    }

    /// Every capability in any of the sets.
    #[must_use]
    pub fn all(self) -> CapabilitySet {
        [
            self.bounding,
            self.effective,
            self.permitted,
            self.inheritable,
            self.ambient,
        ]
        .into_iter()
        .flat_map(CapabilitySet::iter)
        .collect()
    }
}

/// How a container's command gets its capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Capabilities {
    /// Exactly these sets, whoever the user is (an OCI bundle's).
    Exact(CapabilitySets),
    /// Docker's rule (`shipyard run`): `kept` are what's left of the default
    /// set after `--cap-drop`, `added` what `--cap-add` asked for. Root gets
    /// both; a non-root user gets `added` only, raised ambient so it lasts
    /// across `exec` (as Podman does).
    Default {
        /// The default set, after `--cap-drop`.
        kept: CapabilitySet,
        /// `--cap-add`.
        added: CapabilitySet,
    },
}

impl Capabilities {
    /// The same, without `capability` in any set.
    #[must_use]
    pub fn without(self, capability: Capability) -> Self {
        match self {
            Self::Exact(mut sets) => {
                for set in [
                    &mut sets.bounding,
                    &mut sets.effective,
                    &mut sets.permitted,
                    &mut sets.inheritable,
                    &mut sets.ambient,
                ] {
                    set.remove(capability);
                }
                Self::Exact(sets)
            }
            Self::Default {
                mut kept,
                mut added,
            } => {
                kept.remove(capability);
                added.remove(capability);
                Self::Default { kept, added }
            }
        }
    }

    /// The sets for a command running as `uid`.
    #[must_use]
    pub fn resolve(self, uid: u32) -> CapabilitySets {
        match self {
            Self::Exact(sets) => sets,
            Self::Default { kept, added } => {
                let all: CapabilitySet = kept.iter().chain(added.iter()).collect();
                if uid == 0 {
                    // Inheritable stays empty (Docker since CVE-2022-24769):
                    // nothing passes on through file inheritable bits.
                    CapabilitySets {
                        bounding: all,
                        effective: all,
                        permitted: all,
                        ..CapabilitySets::default()
                    }
                } else {
                    CapabilitySets {
                        bounding: all,
                        effective: added,
                        permitted: added,
                        inheritable: added,
                        ambient: added,
                    }
                }
            }
        }
    }
}

/// A container's confinement: its capabilities, `no_new_privs`, and its
/// seccomp filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Security {
    /// The command's capabilities.
    pub capabilities: Capabilities,
    /// Set `no_new_privs`: nothing the command runs can gain privileges,
    /// through a setuid program or file capabilities (step 1's N1).
    pub no_new_privileges: bool,
    /// The system calls it may make.
    pub seccomp: Seccomp,
}

impl Security {
    /// Shipyard's default: Docker's 14 capabilities, `no_new_privs`, and
    /// Docker's seccomp profile.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            capabilities: Capabilities::Default {
                kept: CapabilitySet::DOCKER_DEFAULT,
                added: CapabilitySet::EMPTY,
            },
            no_new_privileges: true,
            seccomp: Seccomp::Default,
        }
    }
}

/// Why a confinement can't be applied on this kernel.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfinementError {
    /// A capability this kernel doesn't have (past its `cap_last_cap`).
    UnknownToKernel(Capability),
    /// A kernel with more than 64 capabilities: the sets are 64-bit.
    KernelTooNew(u8),
    /// The seccomp profile can't be compiled.
    Seccomp(SeccompError),
}

#[cfg(target_os = "linux")]
impl From<SeccompError> for ConfinementError {
    fn from(err: SeccompError) -> Self {
        Self::Seccomp(err)
    }
}

/// Everything the command's last steps need, worked out before the fork
/// (the after-fork code only reads it): see `child.rs`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Confinement {
    /// `PR_SET_KEEPCAPS` before switching to a non-root user (C3).
    pub(crate) keep_caps: bool,
    /// The capabilities to drop from the bounding set: every one up to the
    /// kernel's last that isn't kept, including any Shipyard can't name.
    pub(crate) bounding_drop: Vec<u8>,
    /// `capset`'s data: effective, permitted, inheritable, low 32 bits, then
    /// the same, high 32 bits (the kernel's two `__user_cap_data_struct`s).
    pub(crate) capset_data: [u32; 6],
    /// The capabilities to raise ambient, one call each.
    pub(crate) ambient: Vec<u8>,
    /// Set `no_new_privs`.
    pub(crate) no_new_privileges: bool,
    /// The seccomp filter to install, if any.
    pub(crate) seccomp: Option<SeccompFilter>,
}

/// A compiled seccomp filter, ready for `seccomp(2)`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeccompFilter {
    /// The program (at most 4096 instructions).
    pub(crate) program: Box<[Instruction]>,
    /// Its length, as `struct sock_fprog` has it.
    pub(crate) len: u16,
    /// `SECCOMP_FILTER_FLAG_*`.
    pub(crate) flags: u32,
}

#[cfg(target_os = "linux")]
impl SeccompFilter {
    /// `seccomp`'s profile compiled for this architecture, a container whose
    /// bounding set is `bounding`, and `kernel` (`(major, minor)`); `None`
    /// for no filter.
    fn compile(
        seccomp: &Seccomp,
        bounding: CapabilitySet,
        kernel: (u32, u32),
    ) -> Result<Option<Self>, SeccompError> {
        let Some(profile) = seccomp.profile()? else {
            return Ok(None);
        };
        let arch = Arch::NATIVE.ok_or(SeccompError::Architecture)?;
        let filter = compile(
            &profile,
            &Target {
                arch,
                bounding,
                kernel,
            },
        )?;
        let len = u16::try_from(filter.program.len())
            .map_err(|_| SeccompError::TooLong(filter.program.len()))?;
        Ok(Some(Self {
            program: filter.program.into_boxed_slice(),
            len,
            flags: filter.flags,
        }))
    }
}

#[cfg(target_os = "linux")]
impl Confinement {
    /// The confinement for a command running as `uid`, on a kernel whose last
    /// capability is `last_cap` (`/proc/sys/kernel/cap_last_cap`).
    ///
    /// `--cap-add ALL` means all this kernel has: an older kernel lacks the
    /// newest capabilities. One named explicitly that it lacks is refused.
    ///
    /// The seccomp profile is compiled for this architecture, the
    /// container's bounding set and `kernel` (`(major, minor)`).
    pub(crate) fn new(
        security: &Security,
        uid: u32,
        last_cap: u8,
        kernel: (u32, u32),
    ) -> Result<Self, ConfinementError> {
        let mut sets = security.capabilities.resolve(uid);
        if let Capabilities::Default { added, .. } = security.capabilities
            && added == CapabilitySet::ALL
        {
            sets = sets.up_to(last_cap);
        }
        let seccomp = SeccompFilter::compile(&security.seccomp, sets.bounding, kernel)?;
        Ok(Self {
            keep_caps: uid != 0,
            no_new_privileges: security.no_new_privileges,
            seccomp,
            ..Self::from_sets(sets, last_cap)?
        })
    }

    /// The filter to install first, before the switch of user: without
    /// `no_new_privs`, installing one takes `CAP_SYS_ADMIN` (step 1's S1),
    /// which only that early the process still has (as runc does). The
    /// profile must then allow the setup's own calls; Docker's does.
    pub(crate) fn filter_before_setup(&self) -> Option<&SeccompFilter> {
        self.seccomp.as_ref().filter(|_| !self.no_new_privileges)
    }

    /// The filter to install last, right before `exec`: with
    /// `no_new_privs`, so only `exec` (and a failure's report) runs under it
    /// here.
    pub(crate) fn filter_before_exec(&self) -> Option<&SeccompFilter> {
        self.seccomp.as_ref().filter(|_| self.no_new_privileges)
    }

    /// The init's own confinement once its command runs: only `CAP_KILL`, to
    /// signal a command running as another user (TLPI §20.5), and
    /// `no_new_privs`.
    pub(crate) fn for_init(last_cap: u8) -> Result<Self, ConfinementError> {
        let kill: CapabilitySet = std::iter::once(Capability::KILL).collect();
        let sets = CapabilitySets {
            bounding: kill,
            effective: kill,
            permitted: kill,
            ..CapabilitySets::default()
        };
        Ok(Self {
            no_new_privileges: true,
            ..Self::from_sets(sets, last_cap)?
        })
    }

    /// The sets on a kernel whose last capability is `last_cap`; no
    /// keep-caps, no `no_new_privs` (the callers say).
    fn from_sets(sets: CapabilitySets, last_cap: u8) -> Result<Self, ConfinementError> {
        if last_cap > 63 {
            return Err(ConfinementError::KernelTooNew(last_cap));
        }
        if let Some(unknown) = sets.all().iter().find(|cap| cap.number() > last_cap) {
            return Err(ConfinementError::UnknownToKernel(unknown));
        }
        let bounding = sets.bounding.bits();
        let halves = |set: CapabilitySet| {
            let low = u32::try_from(set.bits() & u64::from(u32::MAX)).unwrap_or_default();
            let high = u32::try_from(set.bits() >> 32).unwrap_or_default();
            (low, high)
        };
        let (effective, permitted, inheritable) = (
            halves(sets.effective),
            halves(sets.permitted),
            halves(sets.inheritable),
        );
        Ok(Self {
            keep_caps: false,
            bounding_drop: (0..=last_cap)
                .filter(|&number| bounding & (1 << number) == 0)
                .collect(),
            capset_data: [
                effective.0,
                permitted.0,
                inheritable.0,
                effective.1,
                permitted.1,
                inheritable.1,
            ],
            ambient: sets.ambient.iter().map(Capability::number).collect(),
            no_new_privileges: false,
            seccomp: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    const KERNEL: (u32, u32) = (6, 12);

    fn set(names: &[&str]) -> CapabilitySet {
        names
            .iter()
            .map(|name| name.parse::<Capability>().unwrap())
            .collect()
    }

    /// `without` takes the capability out of every set, of either kind,
    /// and leaves the rest as they were.
    #[test]
    fn without_takes_a_capability_out_of_every_set() {
        let both = set(&["CAP_MKNOD", "CAP_CHOWN"]);
        let exact = Capabilities::Exact(CapabilitySets {
            bounding: both,
            effective: both,
            permitted: both,
            inheritable: both,
            ambient: both,
        });
        let Capabilities::Exact(sets) = exact.without(Capability::MKNOD) else {
            panic!("not exact any more");
        };
        let chown = set(&["CAP_CHOWN"]);
        for kept in [
            sets.bounding,
            sets.effective,
            sets.permitted,
            sets.inheritable,
            sets.ambient,
        ] {
            assert_eq!(kept, chown);
        }
        let default = Capabilities::Default {
            kept: CapabilitySet::DOCKER_DEFAULT,
            added: both,
        };
        let without = default.without(Capability::MKNOD).resolve(0);
        assert!(!without.bounding.contains(Capability::MKNOD));
        assert!(without.bounding.contains(Capability::KILL));
        assert!(
            without
                .bounding
                .contains(set(&["CAP_CHOWN"]).iter().next().unwrap())
        );
        // The control: as given, root has it.
        assert!(default.resolve(0).bounding.contains(Capability::MKNOD));
    }

    #[test]
    fn root_gets_kept_and_added_and_nothing_inheritable() {
        let rule = Capabilities::Default {
            kept: set(&["CHOWN", "KILL"]),
            added: set(&["NET_ADMIN"]),
        };
        let sets = rule.resolve(0);
        let all = set(&["CHOWN", "KILL", "NET_ADMIN"]);
        assert_eq!(sets.bounding, all);
        assert_eq!(sets.effective, all);
        assert_eq!(sets.permitted, all);
        assert!(sets.inheritable.is_empty() && sets.ambient.is_empty());
    }

    #[test]
    fn a_non_root_user_gets_only_what_was_added_and_ambient() {
        let rule = Capabilities::Default {
            kept: set(&["CHOWN", "KILL"]),
            added: set(&["NET_BIND_SERVICE"]),
        };
        let sets = rule.resolve(1000);
        let added = set(&["NET_BIND_SERVICE"]);
        assert_eq!(sets.bounding, set(&["CHOWN", "KILL", "NET_BIND_SERVICE"]));
        for got in [
            sets.effective,
            sets.permitted,
            sets.inheritable,
            sets.ambient,
        ] {
            assert_eq!(got, added);
        }
        // Nothing added: nothing effective.
        let none = Capabilities::Default {
            kept: set(&["CHOWN"]),
            added: CapabilitySet::EMPTY,
        };
        assert!(none.resolve(1000).effective.is_empty());
    }

    #[test]
    fn exact_sets_are_kept_whoever_the_user_is() {
        let sets = CapabilitySets {
            bounding: set(&["KILL"]),
            ..CapabilitySets::default()
        };
        assert_eq!(Capabilities::Exact(sets).resolve(0), sets);
        assert_eq!(Capabilities::Exact(sets).resolve(1000), sets);
    }

    #[test]
    fn the_standard_is_dockers_fourteen_and_no_new_privs() {
        let standard = Security::standard();
        assert!(standard.no_new_privileges);
        assert_eq!(
            standard.capabilities.resolve(0).effective,
            CapabilitySet::DOCKER_DEFAULT
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_bounding_drop_reaches_the_kernels_last_capability() {
        let security = Security::standard();
        // This kernel's 40: every capability but the 14 is dropped.
        let on_40 = Confinement::new(&security, 0, 40, KERNEL).unwrap();
        assert_eq!(on_40.bounding_drop.len(), 41 - 14);
        assert!(!on_40.bounding_drop.contains(&0), "CAP_CHOWN is kept");
        assert!(on_40.bounding_drop.contains(&21), "CAP_SYS_ADMIN goes");
        // A newer kernel's capabilities Shipyard can't name go too.
        let on_45 = Confinement::new(&security, 0, 45, KERNEL).unwrap();
        assert!(on_45.bounding_drop.contains(&45));
        // An older kernel: nothing past its last is touched.
        let on_37 = Confinement::new(&security, 0, 37, KERNEL).unwrap();
        assert_eq!(on_37.bounding_drop.iter().max(), Some(&37));
        assert_eq!(
            Confinement::new(&security, 0, 64, KERNEL),
            Err(ConfinementError::KernelTooNew(64))
        );
    }

    #[test]
    fn inconsistent_sets_are_caught_with_the_rule() {
        let base = CapabilitySets {
            bounding: set(&["KILL", "CHOWN"]),
            permitted: set(&["KILL"]),
            ..CapabilitySets::default()
        };
        assert_eq!(base.check(), Ok(()));
        let effective = CapabilitySets {
            effective: set(&["CHOWN"]),
            ..base
        };
        assert!(
            effective
                .check()
                .unwrap_err()
                .contains("effective has CAP_CHOWN")
        );
        let inheritable = CapabilitySets {
            inheritable: set(&["SYS_ADMIN"]),
            ..base
        };
        assert!(
            inheritable
                .check()
                .unwrap_err()
                .contains("inheritable has CAP_SYS_ADMIN")
        );
        let ambient = CapabilitySets {
            ambient: set(&["KILL"]),
            ..base
        };
        assert!(
            ambient
                .check()
                .unwrap_err()
                .contains("ambient has CAP_KILL")
        );
        let fine = CapabilitySets {
            inheritable: set(&["KILL"]),
            ambient: set(&["KILL"]),
            ..base
        };
        assert_eq!(fine.check(), Ok(()));
        // The CLI's rule always gives consistent sets.
        let rule = Capabilities::Default {
            kept: set(&["CHOWN"]),
            added: set(&["NET_RAW"]),
        };
        assert_eq!(rule.resolve(0).check(), Ok(()));
        assert_eq!(rule.resolve(1000).check(), Ok(()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cap_add_all_means_all_the_kernel_has() {
        let all = Security {
            capabilities: Capabilities::Default {
                kept: CapabilitySet::DOCKER_DEFAULT,
                added: CapabilitySet::ALL,
            },
            no_new_privileges: true,
            seccomp: Seccomp::Unconfined,
        };
        // Linux 5.4's last capability is 37 (CAP_AUDIT_READ).
        let old = Confinement::new(&all, 0, 37, KERNEL).unwrap();
        assert!(
            old.bounding_drop.is_empty(),
            "everything this kernel has is kept"
        );
        assert_eq!(
            old.capset_data[3], 0b11_1111,
            "32 to 37 effective, nothing past"
        );
        // A capability named explicitly that the kernel lacks is still refused.
        let named = Security {
            capabilities: Capabilities::Default {
                kept: CapabilitySet::EMPTY,
                added: set(&["BPF"]),
            },
            no_new_privileges: true,
            seccomp: Seccomp::Unconfined,
        };
        assert!(Confinement::new(&named, 0, 37, KERNEL).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_capability_the_kernel_lacks_is_refused() {
        let sets = CapabilitySets {
            bounding: set(&["CHECKPOINT_RESTORE"]),
            ..CapabilitySets::default()
        };
        let security = Security {
            capabilities: Capabilities::Exact(sets),
            no_new_privileges: true,
            seccomp: Seccomp::Unconfined,
        };
        assert_eq!(
            Confinement::new(&security, 0, 37, KERNEL),
            Err(ConfinementError::UnknownToKernel(
                "CHECKPOINT_RESTORE".parse().unwrap()
            ))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capset_data_and_ambient_follow_the_sets() {
        let rule = Capabilities::Default {
            kept: set(&["CHOWN"]),
            added: set(&["NET_BIND_SERVICE"]),
        };
        let security = Security {
            capabilities: rule,
            no_new_privileges: false,
            seccomp: Seccomp::Unconfined,
        };
        let non_root = Confinement::new(&security, 1000, 40, KERNEL).unwrap();
        assert!(non_root.keep_caps && !non_root.no_new_privileges);
        assert_eq!(non_root.capset_data, [1 << 10, 1 << 10, 1 << 10, 0, 0, 0]);
        assert_eq!(non_root.ambient, vec![10]);
        let root = Confinement::new(&security, 0, 40, KERNEL).unwrap();
        assert!(!root.keep_caps && root.ambient.is_empty());
        assert_eq!(root.capset_data, [1 | 1 << 10, 1 | 1 << 10, 0, 0, 0, 0]);
        // A capability above 31 lands in the high words.
        let high = Security {
            capabilities: Capabilities::Exact(CapabilitySets {
                effective: set(&["BPF"]),
                ..CapabilitySets::default()
            }),
            no_new_privileges: true,
            seccomp: Seccomp::Unconfined,
        };
        assert_eq!(
            Confinement::new(&high, 0, 40, KERNEL).unwrap().capset_data,
            [0, 0, 0, 1 << (39 - 32), 0, 0]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_init_keeps_only_kill() {
        let init = Confinement::for_init(40).unwrap();
        assert_eq!(init.bounding_drop.len(), 40);
        assert!(!init.bounding_drop.contains(&5), "CAP_KILL stays");
        assert_eq!(init.capset_data, [1 << 5, 1 << 5, 0, 0, 0, 0]);
        assert!(init.no_new_privileges && init.ambient.is_empty());
    }

    /// With `no_new_privs`, the filter goes on last; without, first (it
    /// needs `CAP_SYS_ADMIN` then); never twice.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_filter_goes_on_first_only_without_no_new_privs() {
        let mut security = Security::standard();
        let late = Confinement::new(&security, 0, 40, KERNEL).unwrap();
        assert!(late.filter_before_exec().is_some());
        assert!(late.filter_before_setup().is_none());
        security.no_new_privileges = false;
        let early = Confinement::new(&security, 0, 40, KERNEL).unwrap();
        assert!(early.filter_before_setup().is_some());
        assert!(early.filter_before_exec().is_none());
        security.seccomp = Seccomp::Unconfined;
        let none = Confinement::new(&security, 0, 40, KERNEL).unwrap();
        assert!(none.filter_before_setup().is_none() && none.filter_before_exec().is_none());
    }
}
