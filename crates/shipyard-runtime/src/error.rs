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
}

/// A `Result` whose error defaults to the runtime's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
