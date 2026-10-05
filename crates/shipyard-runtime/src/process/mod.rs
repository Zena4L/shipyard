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
#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod join;
mod plan;
mod rlimit;
mod signal;
mod signals;
mod spawn;
mod spec;
mod startup;
mod status;
mod stop;
mod supervisor;

pub use env::{DEFAULT_PATH, Env, EnvError};
pub use failure::{SpawnError, SpawnErrorKind};
pub use rlimit::{Rlimit, RlimitError, RlimitKind, RlimitValue};
pub use signal::{Signal, SignalError};
pub use spawn::die_from;
pub use spec::{MAX_GROUPS, ProcessSpec, SpecError, User};
pub use startup::refuse_privileged_start;
pub use status::{EXIT_CANNOT_EXECUTE, EXIT_ENGINE_ERROR, EXIT_NOT_FOUND, ExitStatus};
pub use stop::{DEFAULT_STOP_TIMEOUT_SECS, StopPolicy, StopSignalError};
pub use supervisor::Supervisor;

/// The hidden argument that makes the shipyard binary run as a container's
/// init (see [`init_entry_point`]).
#[cfg(target_os = "linux")]
pub(crate) const INIT_ENTRY_ARG: &std::ffi::CStr = c"__shipyard-init";

/// Runs this process as a container's init, if that's what it was started
/// as; otherwise returns at once. **Call it first thing in `main`** of any
/// binary that supervises isolated commands.
///
/// With isolation, the container's PID 1 starts as a copy of the
/// supervising process, then re-execs that same binary (as
/// `/proc/self/exe`, with a hidden argument) to become a clean process that
/// carries none of its command line, environment or memory. This is where
/// that new process takes over; it never returns there.
///
/// Isolated runs with a new PID namespace (the ones that get an init) are
/// refused ([`Error::NoInitEntryPoint`](crate::Error::NoInitEntryPoint))
/// until it has been called: otherwise the re-exec would run the binary's
/// own `main` as the container's PID 1.
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
