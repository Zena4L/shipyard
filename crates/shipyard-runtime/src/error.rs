use nix::errno::Errno;

/// Errors produced by the runtime.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("{feature} requires a Linux kernel (current host: {os})")]
    LinuxOnly {
        feature: &'static str,
        os: &'static str,
    },

    /// A system call failed. `op` says what we were doing, in words; the
    /// errno is the error's source, so full-chain printing shows both once.
    #[error("{op}")]
    Sys {
        op: &'static str,
        #[source]
        source: Errno,
    },

    #[error(
        "refusing to run: shipyard is installed setuid or setgid \
         (real uid {uid}, effective uid {euid}, real gid {gid}, effective gid {egid}); \
         reinstall it without the setuid/setgid bit"
    )]
    PrivilegedExecutable {
        uid: u32,
        euid: u32,
        gid: u32,
        egid: u32,
    },
}

impl Error {
    pub(crate) fn sys(op: &'static str, source: Errno) -> Self {
        Self::Sys { op, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
