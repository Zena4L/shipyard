//! A forwarded command's control (step 13.6a, F94): its signals, its
//! window's size and its hang-up on a connection of their own
//! ([`forward::CONTROL_REQUEST`]), named by a token the agent gave the Mac
//! right after the command's welcome ([`Frame::Session`]). Read on a thread
//! of its own, they reach the command whatever its input waits on: a command
//! that doesn't read what's typed, or a pasted screenful its terminal can't
//! take, never holds back a Ctrl-C.
//!
//! [`forward::CONTROL_REQUEST`]: shipyard_runtime::attach::forward::CONTROL_REQUEST

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::forward::Connection;

/// What a command's signals and resizes reach: its process group, its
/// terminal's master (a copy) if it has one, and whether it has ended
/// (then nothing is signalled: its group's number may be another's).
pub(super) struct Controlled {
    group: Option<Pid>,
    master: Option<OwnedFd>,
    ended: Arc<AtomicBool>,
}

impl Controlled {
    /// The control of a command in `group`, on `master` if it has a
    /// terminal, `ended` when it has.
    pub(super) fn new(group: Option<Pid>, master: Option<OwnedFd>, ended: Arc<AtomicBool>) -> Self {
        Self {
            group,
            master,
            ended,
        }
    }

    /// Sends `signal` to the command: its terminal's foreground group (what
    /// a key would signal), else its own group; nothing once it has ended.
    pub(super) fn signal(&self, signal: Signal) {
        if self.ended.load(Ordering::SeqCst) {
            return;
        }
        let foreground = self
            .master
            .as_ref()
            .and_then(|master| nix::unistd::tcgetpgrp(master).ok());
        if let Some(target) = signal_target(foreground, self.group) {
            let _ = killpg(target, signal);
        }
    }

    /// Sets its terminal's size (none without a terminal).
    pub(super) fn resize(&self, rows: u16, cols: u16) {
        if let Some(master) = &self.master {
            let size = shipyard_runtime::terminal::Size::new(rows, cols);
            let _ = shipyard_runtime::terminal::set_window_size(master.as_raw_fd(), size);
        }
    }

    /// Applies one of the client's frames: a signal it may pass on, a
    /// resize; anything else is nothing.
    pub(super) fn apply(&self, frame: &Frame) {
        match frame {
            Frame::Signal(number) => {
                if let Some(signal) = forwarded(*number) {
                    self.signal(signal);
                }
            }
            Frame::Resize { rows, cols } => self.resize(*rows, *cols),
            _ => {}
        }
    }
}

/// A new random token (the kernel's 128 bits), never 0 (which says "none"
/// on the wire); `None` if none can be had.
pub(super) fn new_token() -> Option<u128> {
    let token = u128::from_be_bytes(crate::random::bytes::<16>().ok()?);
    (token != 0).then_some(token)
}

/// Whose group a client's signal goes to: the command's terminal's
/// `foreground` group, else the command's own `group`. A terminal whose
/// session has ended has none, and says 0, which `killpg` would take for
/// this process's own group (and PID 1's, were they one): never a target,
/// nor is 1.
fn signal_target(foreground: Option<Pid>, group: Option<Pid>) -> Option<Pid> {
    foreground.filter(|pgrp| pgrp.as_raw() > 1).or(group)
}

/// The signal a client's `Signal` frame asks for, if it's one a client may
/// pass on ([`shipyard_runtime::process::is_forwardable`]: never SIGKILL
/// or SIGSTOP, never a fault's).
fn forwarded(number: u8) -> Option<Signal> {
    let number = i32::from(number);
    if !shipyard_runtime::process::is_forwardable(number) {
        return None;
    }
    Signal::try_from(number).ok()
}

/// The commands running, by their tokens.
static SESSIONS: LazyLock<Mutex<HashMap<u128, Arc<Controlled>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One command's place in [`SESSIONS`], its token's, let go when dropped
/// (the command's end).
pub(super) struct Registered {
    token: u128,
}

impl Registered {
    /// `controlled` registered under a new random token (the kernel's 128
    /// bits): `None` if none can be had (the command then has no control
    /// connection: its signals stay on its own).
    pub(super) fn new(controlled: Arc<Controlled>) -> Option<Self> {
        let token = new_token()?;
        SESSIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token, controlled);
        Some(Self { token })
    }

    /// Its token.
    pub(super) fn token(&self) -> u128 {
        self.token
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        SESSIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.token);
    }
}

/// The command `token` names, if it's running.
pub(super) fn find(token: u128) -> Option<Arc<Controlled>> {
    SESSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&token)
        .cloned()
}

/// Reads the client's frames from `connection` and applies each to
/// `controlled`, until the connection ends (that's no hang-up: the client
/// says one with `Signal(SIGHUP)`).
pub(super) fn serve(mut connection: Connection<std::fs::File>, controlled: &Controlled) {
    while let Ok(Some(frame)) = connection.next_frame() {
        controlled.apply(&frame);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A signal goes to the terminal's foreground group, else the command's
    /// own; a terminal with no session left (0), or 1, is never a target.
    #[test]
    fn a_signal_never_goes_to_this_group_or_init() {
        let (job, command) = (Pid::from_raw(40), Some(Pid::from_raw(30)));
        assert_eq!(signal_target(Some(job), command), Some(job));
        assert_eq!(signal_target(None, command), command);
        assert_eq!(signal_target(Some(Pid::from_raw(0)), command), command);
        assert_eq!(signal_target(Some(Pid::from_raw(1)), command), command);
        assert_eq!(signal_target(Some(Pid::from_raw(0)), None), None);
    }

    /// A client may pass on an interrupt, a stop request, a hang-up; not a
    /// kill, a stop, a fault, or a number that isn't a signal.
    #[test]
    fn only_forwardable_signals_pass() {
        for (number, signal) in [
            (2, Signal::SIGINT),
            (15, Signal::SIGTERM),
            (1, Signal::SIGHUP),
        ] {
            assert_eq!(forwarded(number), Some(signal), "{number}");
        }
        for number in [0, 9, 19, 11, 17, 200] {
            assert_eq!(forwarded(number), None, "{number}");
        }
    }

    /// A command's group is signalled while it runs, never once it has
    /// ended (its number may be another's by then): the control, the same
    /// signal to a live one, ends it.
    #[test]
    fn an_ended_command_is_signalled_no_more() {
        use std::os::unix::process::CommandExt as _;
        let spawn = || {
            std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let group =
            |child: &std::process::Child| Some(Pid::from_raw(i32::try_from(child.id()).unwrap()));
        let mut ended = spawn();
        let control = Controlled::new(group(&ended), None, Arc::new(AtomicBool::new(true)));
        control.signal(Signal::SIGTERM);
        // A SIGTERM sent would be what it died of: a fatal signal's
        // exit is settled as it's sent, before a later SIGKILL.
        ended.kill().unwrap();
        let status = ended.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL),
            "signalled once ended"
        );
        let mut running = spawn();
        let control = Controlled::new(group(&running), None, Arc::new(AtomicBool::new(false)));
        control.signal(Signal::SIGTERM);
        let status = running.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGTERM)
        );
    }

    /// A registered command is found by its token, once, until its end;
    /// another token finds nothing; an ended one is signalled no more.
    #[test]
    fn a_command_is_found_by_its_token_while_it_runs() {
        let ended = Arc::new(AtomicBool::new(false));
        let controlled = Arc::new(Controlled::new(None, None, Arc::clone(&ended)));
        let registered = Registered::new(Arc::clone(&controlled)).unwrap();
        let token = registered.token();
        assert!(find(token).is_some());
        assert!(find(token ^ 1).is_none());
        // No group: nothing to signal (and nothing panics).
        controlled.signal(Signal::SIGINT);
        drop(registered);
        assert!(find(token).is_none(), "gone at its end");
    }
}
