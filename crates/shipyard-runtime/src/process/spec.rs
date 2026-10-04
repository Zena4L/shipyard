//! What to run: the program, its environment, user, working directory and
//! limits. Mirrors the `process` object of the OCI runtime spec, so stage 3 can
//! fill it straight from a container's `config.json`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use nix::unistd::{getgid, getuid};

use super::env::Env;
use super::rlimit::{Rlimit, RlimitError, RlimitKind};

/// The most supplementary groups a process may have (`NGROUPS_MAX`).
#[cfg(target_os = "linux")]
pub const MAX_GROUPS: usize = 65_536;
/// The most supplementary groups a process may have. macOS allows 16 entries
/// (`NGROUPS_MAX`), but stores the primary group in the same list, which leaves
/// 15 for supplementary groups.
#[cfg(not(target_os = "linux"))]
pub const MAX_GROUPS: usize = 15;

/// A process to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    /// The program and its arguments; `args[0]` is the program.
    pub args: Vec<OsString>,
    /// The complete environment; nothing is inherited from shipyard.
    pub env: Env,
    /// Absolute working directory.
    pub cwd: PathBuf,
    /// Who the process runs as.
    pub user: User,
    /// Resource limits, at most one per kind.
    pub rlimits: Vec<Rlimit>,
}

/// Who the process runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// User ID.
    pub uid: u32,
    /// Primary group ID.
    pub gid: u32,
    /// File mode creation mask; `None` means the default, `0o022`.
    pub umask: Option<u32>,
    /// Supplementary groups. When shipyard runs as root these replace root's
    /// own groups entirely, so an empty list means "no extra groups".
    pub additional_gids: Vec<u32>,
}

impl User {
    /// The user shipyard itself is running as, with no extra groups.
    #[must_use]
    pub fn current() -> Self {
        Self {
            uid: getuid().as_raw(),
            gid: getgid().as_raw(),
            umask: None,
            additional_gids: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), SpecError> {
        if self.uid == u32::MAX {
            return Err(SpecError::ReservedId { what: "uid" });
        }
        if self.gid == u32::MAX {
            return Err(SpecError::ReservedId { what: "gid" });
        }
        if self.additional_gids.contains(&u32::MAX) {
            return Err(SpecError::ReservedId {
                what: "supplementary gid",
            });
        }
        if self.additional_gids.len() > MAX_GROUPS {
            return Err(SpecError::TooManyGroups {
                count: self.additional_gids.len(),
                max: MAX_GROUPS,
            });
        }
        match self.umask {
            Some(mask) if mask > 0o777 => Err(SpecError::UmaskOutOfRange(mask)),
            _ => Ok(()),
        }
    }
}

/// Why a process spec was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpecError {
    /// `args` is empty, so there is nothing to run.
    #[error("no command given")]
    EmptyArgs,
    /// `args[0]` is an empty string.
    #[error("the command name is empty")]
    EmptyProgram,
    /// An argument contains a NUL byte, which C strings can't hold.
    #[error("argument {index} contains a NUL byte")]
    NulInArg {
        /// Position in `args`.
        index: usize,
    },
    /// The working directory is relative.
    #[error("working directory \"{}\" must be an absolute path", .0.display())]
    CwdNotAbsolute(PathBuf),
    /// A path or string that must become a C string contains a NUL byte.
    #[error("{field} contains a NUL byte")]
    ContainsNul {
        /// Which field, in words.
        field: &'static str,
    },
    /// An ID of `u32::MAX`, which the kernel treats as "leave unchanged".
    /// Allowing it would silently keep the process's current (maybe root) ID.
    #[error(
        "{what} {} is reserved: the kernel reads it as \"leave unchanged\"",
        u32::MAX
    )]
    ReservedId {
        /// Which ID: "uid", "gid" or "supplementary gid".
        what: &'static str,
    },
    /// More supplementary groups than the platform allows.
    #[error("{count} supplementary groups given, but this platform allows at most {max}")]
    TooManyGroups {
        /// How many were given.
        count: usize,
        /// The platform's limit, [`MAX_GROUPS`].
        max: usize,
    },
    /// A umask with bits above `0o777`.
    #[error("umask {0:#o} is out of range (at most 0o777)")]
    UmaskOutOfRange(u32),
    /// The same resource limit appears twice.
    #[error("resource limit \"{0}\" is given more than once")]
    DuplicateRlimit(RlimitKind),
    /// One of the resource limits is invalid.
    #[error(transparent)]
    Rlimit(#[from] RlimitError),
}

impl ProcessSpec {
    /// Checks everything that can be checked before a process exists.
    ///
    /// Spawning calls this unconditionally, because after `fork` the child can
    /// no longer report problems in detail. Specs may also come from config
    /// files, so this never assumes the CLI already checked anything.
    pub fn validate(&self) -> Result<(), SpecError> {
        self.validate_args()?;
        self.validate_cwd()?;
        self.user.validate()?;
        validate_rlimits(&self.rlimits)
    }

    fn validate_args(&self) -> Result<(), SpecError> {
        let program = self.args.first().ok_or(SpecError::EmptyArgs)?;
        if program.is_empty() {
            return Err(SpecError::EmptyProgram);
        }
        match self.args.iter().position(|arg| arg.as_bytes().contains(&0)) {
            Some(index) => Err(SpecError::NulInArg { index }),
            None => Ok(()),
        }
    }

    fn validate_cwd(&self) -> Result<(), SpecError> {
        if self.cwd.as_os_str().as_bytes().contains(&0) {
            return Err(SpecError::ContainsNul {
                field: "working directory",
            });
        }
        if !self.cwd.is_absolute() {
            return Err(SpecError::CwdNotAbsolute(self.cwd.clone()));
        }
        Ok(())
    }
}

fn validate_rlimits(rlimits: &[Rlimit]) -> Result<(), SpecError> {
    let mut seen = HashSet::new();
    for limit in rlimits {
        limit.validate()?;
        if !seen.insert(limit.kind) {
            return Err(SpecError::DuplicateRlimit(limit.kind));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStringExt;

    use super::*;
    use crate::process::rlimit::RlimitValue::Finite;

    fn valid() -> ProcessSpec {
        ProcessSpec {
            args: vec!["/bin/true".into()],
            env: Env::new(),
            cwd: "/".into(),
            user: User {
                uid: 1000,
                gid: 1000,
                umask: None,
                additional_gids: vec![10, 20],
            },
            rlimits: vec!["nofile=1024:2048".parse().unwrap()],
        }
    }

    fn rejects(change: impl FnOnce(&mut ProcessSpec)) -> SpecError {
        let mut spec = valid();
        change(&mut spec);
        spec.validate().expect_err("spec should be rejected")
    }

    #[test]
    fn a_valid_spec_passes() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn args() {
        assert_eq!(rejects(|s| s.args.clear()), SpecError::EmptyArgs);
        assert_eq!(
            rejects(|s| s.args = vec![OsString::new()]),
            SpecError::EmptyProgram
        );
        assert_eq!(
            rejects(|s| s.args.push(OsString::from_vec(b"a\0b".to_vec()))),
            SpecError::NulInArg { index: 1 }
        );
    }

    #[test]
    fn cwd() {
        assert_eq!(
            rejects(|s| s.cwd = "relative/dir".into()),
            SpecError::CwdNotAbsolute("relative/dir".into())
        );
        assert_eq!(
            rejects(|s| s.cwd = OsStr::from_bytes(b"/a\0b").into()),
            SpecError::ContainsNul {
                field: "working directory"
            }
        );
    }

    #[test]
    fn reserved_ids_are_rejected() {
        assert_eq!(
            rejects(|s| s.user.uid = u32::MAX),
            SpecError::ReservedId { what: "uid" }
        );
        assert_eq!(
            rejects(|s| s.user.gid = u32::MAX),
            SpecError::ReservedId { what: "gid" }
        );
        assert_eq!(
            rejects(|s| s.user.additional_gids.push(u32::MAX)),
            SpecError::ReservedId {
                what: "supplementary gid"
            }
        );
    }

    #[test]
    fn group_count_is_capped() {
        let too_many = vec![1; MAX_GROUPS + 1];
        assert_eq!(
            rejects(|s| s.user.additional_gids = too_many),
            SpecError::TooManyGroups {
                count: MAX_GROUPS + 1,
                max: MAX_GROUPS
            }
        );
    }

    #[test]
    fn umask_range() {
        assert_eq!(
            rejects(|s| s.user.umask = Some(0o1000)),
            SpecError::UmaskOutOfRange(0o1000)
        );
        let mut spec = valid();
        spec.user.umask = Some(0o777);
        assert_eq!(spec.validate(), Ok(()));
    }

    #[test]
    fn rlimits() {
        assert_eq!(
            rejects(|s| s.rlimits.push("nofile=1".parse().unwrap())),
            SpecError::DuplicateRlimit(RlimitKind::Nofile)
        );
        // Fields are public, so a hand-built limit can be invalid. Validation
        // must still catch it.
        let backwards = Rlimit {
            kind: RlimitKind::Core,
            soft: Finite(10),
            hard: Finite(5),
        };
        assert!(matches!(
            rejects(|s| s.rlimits.push(backwards)),
            SpecError::Rlimit(RlimitError::SoftAboveHard { .. })
        ));
    }
}
