//! Low-level container primitives for Shipyard.
//!
//! Code here is split by how portable it is:
//!
//! - `cfg(unix)` — POSIX process primitives (fork/exec, wait, signals). These run
//!   natively on both macOS and Linux.
//! - `cfg(target_os = "linux")` — kernel isolation features (namespaces, mounts,
//!   cgroups). These only exist on Linux; on macOS they run inside a lightweight
//!   Linux VM managed by Shipyard.

#[cfg(not(unix))]
compile_error!("shipyard only supports UNIX platforms (Linux, macOS)");

mod error;

pub use error::{Error, Result};

/// How this host runs containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// Isolation runs directly on the host kernel.
    Native,
    /// Isolation runs inside a lightweight Linux VM (macOS).
    LinuxVm,
}

/// Returns the container backend for the current host.
#[must_use]
pub fn host_backend() -> Backend {
    if cfg!(target_os = "linux") {
        Backend::Native
    } else {
        Backend::LinuxVm
    }
}
