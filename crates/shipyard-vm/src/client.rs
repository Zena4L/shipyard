//! The Mac CLI's side (step 10.3): starting the VM's host if it isn't
//! running, forwarding a command through it, and its status and stop.

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use shipyard_runtime::attach::client::AttachError;
use shipyard_runtime::attach::forward;
use shipyard_runtime::process::SignalWatch;

use crate::{VmError, VmPaths};

mod ask;
mod command;
mod host;
mod session;

pub use ask::{ask, until_guest_ready};
pub use command::forward;
pub(crate) use host::last_lines;
use host::{PID_CAP, start_host};
pub use session::{TERM_GRACE, linux_number};

/// How long a first command waits for the VM: longer than its host waits
/// for the guest (`host::BOOT_LIMIT`), and the VM's own start.
const START_LIMIT: Duration = Duration::from_secs(150);

/// How long `vm stop` waits for the host to be gone.
const STOP_LIMIT: Duration = Duration::from_secs(120);

/// Whether the VM's host is running, and its PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It runs (its lock is held), with this PID if it wrote one.
    Running(Option<u32>),
    /// It doesn't.
    Stopped,
}

/// Whether the host runs: its lock held is the only sign (a PID alone
/// could be another process's by now).
///
/// # Errors
///
/// [`VmError::Io`] if the lock file can't be read.
pub fn status(paths: &VmPaths) -> Result<Status, VmError> {
    let path = paths.lock();
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Status::Stopped),
        Err(error) => return Err(VmError::io(&path, error)),
    };
    // (A host starting in this moment finds the lock taken, and leaves:
    // its CLI starts another, `connect` below.)
    match file.try_lock_shared() {
        Ok(()) => Ok(Status::Stopped),
        Err(std::fs::TryLockError::WouldBlock) => {
            let pid = shipyard_sys::fs::read_bounded(&path, PID_CAP)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| text.trim().parse().ok());
            Ok(Status::Running(pid))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(VmError::io(&path, error)),
    }
}

/// A connection to the host, starting it first if it isn't running (and
/// waiting for its guest to boot). A host started here that finds the lock
/// taken leaves: then this waits for that other host's socket, or, if it
/// was stopping, for its lock to be free, and starts one again.
///
/// # Errors
///
/// [`VmError::Host`]: it didn't start, or its socket didn't come; with its
/// log's last lines.
pub fn connect(paths: &VmPaths) -> Result<UnixStream, VmError> {
    connect_watching(paths, None)
}

/// The refusal an attach client's `error` is, if it's one of the VM's: by
/// its code (version 5), or an older host's or guest's words.
fn refusal_of(error: &AttachError) -> Option<forward::Refusal> {
    match error {
        AttachError::Declined(refusal) => Some(refusal.clone()),
        AttachError::Refused(why) => Some(forward::Refusal::from_words(why)),
        _ => None,
    }
}

/// Starts the VM if it isn't running, and waits for its guest to be ready
/// (D10.4-4: its data disk checked first): what `vm start` and `shipyard
/// compose` mean by started.
///
/// # Errors
///
/// As [`connect`]'s and [`until_guest_ready`]'s.
pub fn start(paths: &VmPaths) -> Result<(), VmError> {
    drop(connect(paths)?);
    until_guest_ready(paths)
}

/// [`connect`], given up on as `watch` catches a signal that ends a
/// wait ([`interrupted`]).
fn connect_watching(paths: &VmPaths, watch: Option<&SignalWatch>) -> Result<UnixStream, VmError> {
    paths.prepare(nix::unistd::getuid().as_raw())?;
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        return Ok(stream);
    }
    let deadline = Instant::now() + START_LIMIT;
    let mut host = start_host(paths)?;
    loop {
        if let Ok(stream) = UnixStream::connect(paths.socket()) {
            return Ok(stream);
        }
        let ended = host.ended(paths);
        let failed = ended.is_some_and(|status| !status.success());
        if failed || Instant::now() > deadline {
            if !failed {
                // Not killed: asked to stop, it stops its guest first.
                host.terminate(paths);
            }
            return Err(VmError::Host(format!(
                "the VM didn't start{}: {}",
                if failed { "" } else { " in time" },
                last_lines(&paths.host_log(), 5)
            )));
        }
        // Ours left (another host had the lock), and there's still no
        // socket: once no host holds the lock, start one again (launchd's
        // job: started again, a no-op while it runs).
        if host.may_start_again(ended, Instant::now()) && status(paths)? == Status::Stopped {
            host = start_host(paths)?;
        }
        if let Some(code) = interrupted(watch) {
            return Err(VmError::Interrupted(code));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How a forwarded command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// With this exit code.
    Exited(i32),
    /// This process's stdout reader went (`… | head`): not a failure.
    ReaderGone,
}

/// How long the guest may be not ready before the wait is said.
const SAY_NOT_READY_AFTER: Duration = Duration::from_secs(3);

/// Between two asks of a guest that isn't ready.
const NOT_READY_REST: Duration = Duration::from_millis(250);

/// Why a try at [`forward`](fn@forward) didn't run its command.
enum Tried {
    /// The host refused it as it stopped: that host's PID, if known.
    Stopping(Option<u32>),
    /// The guest refused it, not ready yet (its data disk being checked).
    NotReady,
    /// Anything else.
    Failed(VmError),
}

impl Tried {
    fn into_error(self) -> VmError {
        match self {
            Self::Stopping(_) => VmError::Refused(forward::Refusal::Stopping.to_string()),
            Self::NotReady => VmError::NotReady,
            Self::Failed(error) => error,
        }
    }
}

impl From<VmError> for Tried {
    fn from(error: VmError) -> Self {
        Self::Failed(error)
    }
}

/// [`Tried::Stopping`], with the stopping host's PID.
fn stopping(paths: &VmPaths) -> Tried {
    match status(paths) {
        Ok(Status::Running(pid)) => Tried::Stopping(pid),
        _ => Tried::Stopping(None),
    }
}

/// The exit code for a signal `watch` caught that ends a wait for the VM
/// (SIGINT, SIGTERM, SIGHUP: 128 + its number), the others dropped (none
/// reaches a command not yet sent).
fn interrupted(watch: Option<&SignalWatch>) -> Option<i32> {
    let caught = watch?.take().signals;
    caught
        .iter()
        .map(|signal| signal.as_raw())
        .find(|&raw| [libc::SIGINT, libc::SIGTERM, libc::SIGHUP].contains(&raw))
        .map(|raw| 128 + raw)
}

/// Stops the VM in order (its host asked by SIGTERM: the guest stops its
/// containers and powers off), and waits for that host to be gone (a host
/// another command starts meanwhile is another VM, not waited for).
/// Nothing to do if it isn't running.
///
/// # Errors
///
/// [`VmError::Host`]: it's still running after the wait.
pub fn stop(paths: &VmPaths) -> Result<(), VmError> {
    let deadline = Instant::now() + STOP_LIMIT;
    // (A host that has just taken the lock writes its PID next.)
    let pid = loop {
        match status(paths)? {
            Status::Stopped => return Ok(()),
            Status::Running(Some(pid)) => break pid,
            Status::Running(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Status::Running(None) => {
                return Err(VmError::Host(
                    "the VM's host runs, but its PID isn't known".to_owned(),
                ));
            }
        }
    };
    let pid = i32::try_from(pid).map_err(|_| VmError::Host(format!("a PID of {pid}")))?;
    // (The lock was held just now: the PID is still its holder's.)
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    );
    wait_for_host_gone(paths, u32::try_from(pid).ok(), deadline)
}

/// Waits for the host `pid` to be gone, by `deadline`: its lock free, or
/// held by another (a host a command started since is a VM this wait
/// wasn't asked about).
fn wait_for_host_gone(paths: &VmPaths, pid: Option<u32>, deadline: Instant) -> Result<(), VmError> {
    let began = Instant::now();
    loop {
        match status(paths)? {
            Status::Stopped => return Ok(()),
            Status::Running(Some(now)) if Some(now) != pid => return Ok(()),
            Status::Running(_) => {}
        }
        if Instant::now() > deadline {
            return Err(VmError::Host(format!(
                "the VM's host (PID {}) still runs after {} s",
                pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                began.elapsed().as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests;
