//! Shipyard's macOS VM (step 10.3, D24, D29): on a Mac, containers run in
//! one small Linux VM, booted with Apple's Virtualization.framework, whose
//! guest is Shipyard itself (an Alpine kernel and an initramfs root:
//! `scripts/guest/build.sh`).
//!
//! The VM lives in a host process of its own, `__shipyard-vm` (`host`): it
//! starts on first use and holds the VM for as long as it runs (D20), behind
//! a lock (liveness is the lock) and a Unix socket only its user can use.
//! The Mac CLI (`client`) starts it if it isn't running, and forwards
//! commands through it: each connection to the socket is joined to the
//! guest's agent over vsock, and carries the attach protocol's frames.
//!
//! The parts that touch Virtualization.framework are macOS's; the places,
//! the manifest and the checks are everyone's (and tested everywhere).

#[cfg(target_os = "macos")]
mod api;
#[cfg(target_os = "macos")]
pub mod client;
#[cfg(target_os = "macos")]
pub mod doctor;
#[cfg(target_os = "macos")]
mod forwarding;
#[cfg(target_os = "macos")]
mod host;
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "the host's; tested everywhere")
)]
mod idle;
#[cfg(target_os = "macos")]
pub mod launchd;
#[cfg(target_os = "macos")]
mod machine;
mod manifest;
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "the host's; tested everywhere")
)]
mod network;
mod paths;
mod peer;
pub mod ports;
#[cfg(target_os = "macos")]
mod serving;
mod state_file;

pub use manifest::{
    Build, Guest, Hashed, Kernel, MANIFEST, MANIFEST_VERSION, Manifest, ManifestError,
};
pub use paths::{MAX_SOCKET_PATH, VmPaths};
pub use peer::same_user;

/// The argument that makes this binary the VM's host process.
pub const HOST_ARG: &str = "__shipyard-vm";

/// Why the VM can't be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VmError {
    /// A place that can't be the VM's.
    #[error("{0}")]
    Place(String),
    /// No guest to boot, or one that can't be read.
    #[error("{0}")]
    Guest(String),
    /// The binary can't run a VM: it isn't signed with the virtualization
    /// entitlement, or the Mac can't run one.
    #[error("{0}")]
    Unsupported(String),
    /// The VM didn't start, or stopped.
    #[error("{0}")]
    Machine(String),
    /// The host process didn't come up, or went.
    #[error("{0}")]
    Host(String),
    /// The guest refused a forwarded command, saying why.
    #[error("the VM refused the command: {0}")]
    Refused(String),
    /// The guest is still starting (its data disk checked or repaired at
    /// boot): asked again, it answers once it's ready.
    #[error("{}", shipyard_runtime::attach::forward::NOT_READY)]
    NotReady,
    /// The VM went (or answered nonsense) before a forwarded command ended.
    #[error("{0}")]
    Gone(String),
    /// A signal came while waiting for the VM, before the command reached
    /// it: the command never ran. The exit code a shell gives that signal
    /// (128 + its number).
    #[error("interrupted while waiting for the VM: the command didn't run")]
    Interrupted(i32),
    /// A file of the VM's.
    #[error("{}: {source}", path.display())]
    Io {
        /// Which.
        path: std::path::PathBuf,
        /// Why.
        source: std::io::Error,
    },
}

impl VmError {
    /// An I/O error on `path`.
    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

/// The VM's host process, if this process was started as it: never returns
/// then. Call it first thing in `main`, with the other entry points.
pub fn host_entry_point() {
    #[cfg(target_os = "macos")]
    if std::env::args_os().nth(1).as_deref() == Some(HOST_ARG.as_ref()) {
        host::run();
    }
}
