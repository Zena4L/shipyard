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

/// How long `vm stop` waits for a host's PID to be written, and for an older
/// host (one with no `host.sock`) to be gone.
const STOP_LIMIT: Duration = Duration::from_secs(120);

/// How long the host has to answer on its own socket.
const HOST_ANSWER_WAIT: Duration = Duration::from_secs(10);

/// The most a host's answer on its own socket holds.
const MAX_HOST_ANSWER: usize = 256;

/// The most `vm stop` waits in all, its host's phases' waits included.
const STOP_MOST: Duration = Duration::from_mins(20);

/// The most a start waits in all, its host's phases' waits included (the
/// desktop app's bound on `vm start` too).
pub const START_MOST: Duration = Duration::from_mins(20);

/// The start's failure (`in_time`: it was given up on, not failed), with
/// the host's last lines.
fn didnt_start(paths: &VmPaths, in_time: bool) -> VmError {
    VmError::Host(format!(
        "the VM didn't start{}: {}",
        if in_time { " in time" } else { "" },
        last_lines(&paths.host_log(), 5)
    ))
}

/// The host's answer to `ask` ([`forward::HOST_PHASE_REQUEST`] or
/// [`forward::HOST_STOP_REQUEST`]) on its own socket: `None` if there's no
/// host, or it's older than this (no `host.sock`), or it didn't answer.
fn ask_host(paths: &VmPaths, ask: &str) -> Option<crate::phase::PhaseAnswer> {
    let stream = UnixStream::connect(paths.host_socket()).ok()?;
    let said = ask::answer_on(stream, &[ask], MAX_HOST_ANSWER, HOST_ANSWER_WAIT).ok()?;
    crate::phase::PhaseAnswer::decode(std::str::from_utf8(&said).ok()?)
}

/// Whether the VM's host is running, and its PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It runs (its lock is held), with this PID if it wrote one.
    Running(Option<u32>),
    /// It doesn't.
    Stopped,
}

/// The host's phase now, asked on its own socket (`host.sock`): asking
/// never starts anything, as a connection to `docker.sock` may (launchd's,
/// with `vm install`: the desktop app's way to know, step 14.2). `None` if
/// no host runs, or one older than 0.5 (no `host.sock`), or it didn't answer.
#[must_use]
pub fn phase(paths: &VmPaths) -> Option<crate::phase::PhaseAnswer> {
    ask_host(paths, forward::HOST_PHASE_REQUEST)
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
    // A Mac the VM can't run on, said by name before a host is started
    // (step 13.7; only a build made for an older macOS than this tree's
    // deployment target, 13.0, gets this far there: dyld refuses the rest).
    if let Some(why) = crate::doctor::this_macs_floor_refusal() {
        return Err(VmError::Unsupported(why));
    }
    let began = Instant::now();
    let mut deadline = began + START_LIMIT;
    let mut host = start_host(paths)?;
    loop {
        if let Ok(stream) = UnixStream::connect(paths.socket()) {
            return Ok(stream);
        }
        let ended = host.ended(paths);
        // (A launchd run that ended badly while another host holds the lock,
        // booting, isn't this start's failure: F144's review.)
        let failed =
            ended.is_some_and(|status| !status.success()) && status(paths)? == Status::Stopped;
        let now = Instant::now();
        // Stopped as it started (a `vm stop` came first): said so, and no
        // other start made (13.6b1's review).
        if ended.and_then(|status| status.code()) == Some(crate::phase::STOPPED_BEFORE_SERVING) {
            return Err(VmError::Refused(forward::Refusal::Stopping.to_string()));
        }
        if failed {
            return Err(didnt_start(paths, false));
        }
        if now > deadline {
            // By the host's own phase (F100): a healthy one is waited for,
            // never stopped by a CLI that gave up; and never signalled.
            let answer = phase(paths);
            // No host yet (launchd's throttle on its kick): looked at again.
            let no_host_yet = answer.is_none()
                && matches!(status(paths)?, Status::Stopped | Status::Running(None));
            let decided = if no_host_yet {
                crate::phase::AtDeadline::Wait(Duration::from_secs(5))
            } else {
                crate::phase::at_deadline(answer)
            };
            match decided {
                crate::phase::AtDeadline::Connect => deadline = now + Duration::from_secs(10),
                crate::phase::AtDeadline::Wait(more) => deadline = now + more,
                crate::phase::AtDeadline::GiveUp { ask_stop } => {
                    if ask_stop {
                        let _ = ask_host(paths, forward::HOST_STOP_REQUEST);
                    }
                    return Err(didnt_start(paths, true));
                }
            }
            if now.duration_since(began) > START_MOST {
                return Err(didnt_start(paths, true));
            }
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

/// Stops the VM in order (its host asked on its own socket, F93: the guest
/// stops its containers and powers off, once it's up: a boot's disk check
/// is never cut), and waits for that host to be gone by the host's own
/// bounds (a host another command starts meanwhile is another VM, not
/// waited for). An older host, with no socket of its own, is sent SIGTERM
/// by the PID its lock says, as before. Nothing to do if it isn't running.
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
    // A host binds its socket before it writes its PID: one with a PID
    // and no socket is older than this.
    if let Some(answer) = ask_host(paths, forward::HOST_STOP_REQUEST) {
        let deadline = Instant::now() + crate::phase::stop_wait(answer);
        return wait_for_host_gone(paths, Some(pid), deadline);
    }
    // A host of this build whose ask failed (its asks all taken, or it's
    // ending): asked once more, else waited for, never signalled (the
    // whole-stage review: its PID may be no one's by now).
    if paths.host_socket().exists() {
        std::thread::sleep(Duration::from_millis(500));
        let deadline = ask_host(paths, forward::HOST_STOP_REQUEST).map_or(deadline, |answer| {
            Instant::now() + crate::phase::stop_wait(answer)
        });
        return wait_for_host_gone(paths, Some(pid), deadline);
    }
    if status(paths)? != Status::Running(Some(pid)) {
        return Ok(());
    }
    let raw = i32::try_from(pid).map_err(|_| VmError::Host(format!("a PID of {pid}")))?;
    // (The lock is held just now by the PID it holds: an older host's.)
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(raw),
        nix::sys::signal::Signal::SIGTERM,
    );
    wait_for_host_gone(paths, Some(pid), deadline)
}

/// Whether the host `pid` (its PID, if known) is gone, as `status` says:
/// stopped, or another host runs (a VM a command started since, not the one
/// waited for). A host whose PID isn't known yet (it has the lock, not yet
/// its PID written) is still the one waited for.
pub(super) fn gone_or_replaced(status: Status, pid: Option<u32>) -> bool {
    match status {
        Status::Stopped => true,
        Status::Running(Some(now)) => pid.is_some_and(|pid| pid != now),
        Status::Running(None) => false,
    }
}

/// A stopping host's `deadline`, put back past it if the host, asked again,
/// is still within its own bound (a stop asked as it booted, waiting out a
/// disk check), so long as the wait `began` less than the longest stop ago
/// (13.6b1's review; a command refused as stopping too: the whole-stage
/// review).
fn waited_on(paths: &VmPaths, began: Instant, deadline: Instant) -> Instant {
    if Instant::now() > deadline
        && began.elapsed() < STOP_MOST
        && let Some(answer) = phase(paths)
        && answer.left > Duration::ZERO
    {
        Instant::now() + answer.left + crate::phase::MARGIN
    } else {
        deadline
    }
}

/// Waits for the host `pid` to be gone, by `deadline`: its lock free, or
/// held by another (a host a command started since is a VM this wait
/// wasn't asked about).
fn wait_for_host_gone(paths: &VmPaths, pid: Option<u32>, deadline: Instant) -> Result<(), VmError> {
    let began = Instant::now();
    let mut deadline = deadline;
    loop {
        if gone_or_replaced(status(paths)?, pid) {
            return Ok(());
        }
        deadline = waited_on(paths, began, deadline);
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
