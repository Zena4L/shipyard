//! Process lifecycle: start a program the way a container runtime must,
//! supervise it, and report how it ended.
//!
//! This is POSIX-only code (TLPI ch. 6, 20–28), so it runs on macOS and Linux.
//! Later stages wrap the same pipeline in namespaces (stage 2) and reuse the
//! supervisor in the per-container monitor (stage 8).

mod channel;
mod child;
#[cfg(target_os = "linux")]
mod clone;
mod env;
mod failure;
mod identity;
#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod join;
#[cfg(target_os = "linux")]
mod network;
mod plan;
#[cfg(target_os = "linux")]
pub mod probe;
#[cfg(target_os = "linux")]
mod resolve;
mod rlimit;
mod rootfs;

#[cfg(target_os = "linux")]
mod setup;
mod signal;
mod signals;
pub(crate) mod spawn;
mod spec;
mod startup;
mod status;
pub(crate) mod stdio;
mod stop;
mod supervisor;
#[cfg(target_os = "linux")]
mod sysctl;
mod terminal;
#[cfg(target_os = "linux")]
mod user_namespace;

pub use env::{DEFAULT_PATH, Env, EnvError};
pub use failure::{SpawnError, SpawnErrorKind, overlay_refusal};
pub use identity::{LookupError, Which};
pub use rlimit::{Rlimit, RlimitError, RlimitKind, RlimitValue};
#[cfg(target_os = "linux")]
pub(crate) use signal::discard_pending;
pub(crate) use signal::is_pending;
pub use signal::{Signal, SignalError};
pub use spawn::die_from;
pub use spec::{MAX_GROUPS, Names, ProcessSpec, SpecError, User};
pub use startup::refuse_privileged_start;
pub use status::{EXIT_CANNOT_EXECUTE, EXIT_ENGINE_ERROR, EXIT_NOT_FOUND, ExitStatus};
pub use stdio::{Pty, Stdio, Stream};
pub use stop::{DEFAULT_STOP_TIMEOUT_SECS, StopPolicy, StopSignalError};
pub use supervisor::{NetworkHook, NetworkTarget, Supervisor};
#[cfg(target_os = "linux")]
pub(crate) use user_namespace::bundle_user_namespace;
#[cfg(target_os = "linux")]
pub use user_namespace::{
    become_namespace_root, current_id_maps, map_user_namespace, rootless_id_maps,
    unshare_user_namespace, userns_restriction,
};

/// The hidden argument that makes the shipyard binary run as a container's
/// init (see [`init_entry_point`]).
#[cfg(target_os = "linux")]
pub(crate) const INIT_ENTRY_ARG: &std::ffi::CStr = c"__shipyard-init";

/// The init's last argument when a SIGPIPE was pending at its re-exec: the
/// clean image's Rust startup sets SIGPIPE to "ignore", which discards a
/// pending one, so it's raised again from this (see `init::run`).
#[cfg(target_os = "linux")]
pub(crate) const SIGPIPE_PENDING_ARG: &std::ffi::CStr = c"sigpipe";

/// Runs this process as a container's init, if that's what it was started
/// as; otherwise returns at once. **Call it first thing in `main`** of any
/// binary that supervises isolated commands.
///
/// With isolation, the process cloned into the new namespaces re-execs that
/// same binary at once (as `/proc/self/exe`, with a hidden argument). The
/// clean process that starts carries none of the supervisor's command line,
/// environment or memory: it sets the container up, and becomes its init.
/// This is where it takes over; it never returns there.
///
/// Isolated runs are refused
/// ([`Error::NoInitEntryPoint`](crate::Error::NoInitEntryPoint)) until it
/// has been called: otherwise the re-exec would run the binary's own `main`
/// inside the container.
pub fn init_entry_point() {
    #[cfg(target_os = "linux")]
    {
        init::entry_point();
        INIT_ENTRY_POINT_CALLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether `/proc` shows this process's own PID namespace, so that a PID
/// read from it, or given by someone who read it, means what we think. It
/// may not: shipyard can run in a PID namespace with another one's `/proc`
/// mounted (a container given the host's).
#[cfg(target_os = "linux")]
pub(crate) fn proc_is_ours() -> bool {
    std::fs::read_link("/proc/self")
        .ok()
        .and_then(|link| link.to_str()?.parse::<u32>().ok())
        == Some(std::process::id())
}

/// Whether [`init_entry_point`] has run in this process.
#[cfg(target_os = "linux")]
static INIT_ENTRY_POINT_CALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether this binary can serve as a container's init (see
/// [`init_entry_point`]).
#[cfg(target_os = "linux")]
pub(crate) fn has_init_entry_point() -> bool {
    INIT_ENTRY_POINT_CALLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The errno behind an I/O error from `std` (EIO if it has none).
#[cfg(target_os = "linux")]
pub(crate) fn io_errno(error: &std::io::Error) -> nix::errno::Errno {
    error
        .raw_os_error()
        .map_or(nix::errno::Errno::EIO, nix::errno::Errno::from_raw)
}
