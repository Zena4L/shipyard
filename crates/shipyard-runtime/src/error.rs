/// Errors produced by the runtime.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("{feature} requires a Linux kernel (current host: {os})")]
    LinuxOnly {
        feature: &'static str,
        os: &'static str,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
