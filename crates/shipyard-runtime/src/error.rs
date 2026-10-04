use std::borrow::Cow;

use nix::errno::Errno;

use crate::process::{EXIT_ENGINE_ERROR, SpawnError, SpecError, StopSignalError};

/// Errors produced by the runtime.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The feature needs Linux kernel support this host doesn't have.
    #[error("{feature} requires a Linux kernel (current host: {os})")]
    LinuxOnly {
        /// What was attempted, e.g. "PID namespaces".
        feature: &'static str,
        /// The host operating system (`std::env::consts::OS`).
        os: &'static str,
    },

    /// Shipyard was started with privileges its caller doesn't have (a
    /// setuid/setgid bit or file capabilities), which would let any user run
    /// commands with elevated privileges.
    #[error(
        "refusing to run: shipyard was started with elevated privileges ({reason}); \
         reinstall it without setuid/setgid bits or file capabilities"
    )]
    PrivilegedStart {
        /// What made the start privileged.
        reason: String,
    },

    /// The process spec is invalid; nothing was started.
    #[error(transparent)]
    InvalidSpec(#[from] SpecError),

    /// The stop signal can't end a process (it stops or continues it).
    #[error(transparent)]
    StopSignal(#[from] StopSignalError),

    /// Running as another user or group, or adding groups, needs root.
    #[error("running as a different user or group needs root: {0}")]
    NeedsRoot(String),

    /// A [`Supervisor`](crate::process::Supervisor) already exists in this
    /// process. It owns the process's signal handlers, so there can be only
    /// one at a time.
    #[error("a supervisor is already running in this process")]
    AlreadySupervising,

    /// The command was started but never got to run.
    #[error(transparent)]
    Spawn(#[from] SpawnError),

    /// A system call failed in shipyard itself. `op` says what we were doing;
    /// the errno is the error's source, so full-chain printing shows both once.
    #[error("{op}")]
    Sys {
        /// What shipyard was doing, in words.
        op: Cow<'static, str>,
        /// The error number the kernel returned.
        #[source]
        source: Errno,
    },
}

impl Error {
    pub(crate) fn sys(op: impl Into<Cow<'static, str>>, source: Errno) -> Self {
        Self::Sys {
            op: op.into(),
            source,
        }
    }

    /// The exit code that reports this error: 126 or 127 when the command
    /// itself couldn't be executed or found, 125 for everything else.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Spawn(err) => err.exit_code(),
            _ => EXIT_ENGINE_ERROR,
        }
    }
}

/// A `Result` whose error defaults to the runtime's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
