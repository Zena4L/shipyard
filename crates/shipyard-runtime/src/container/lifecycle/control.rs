//! A control client of a container's monitor (stage 9, step 9.4b): how
//! `stop` and `wait` reach a container another process started.
//!
//! It connects to the container's `attach.sock` and says
//! [`Frame::HelloControl`]: let in apart from the attach clients, it's sent
//! no output and types nothing. [`stop`] asks the monitor to stop the
//! container ([`Frame::Stop`]): the signal now, SIGKILL at the timeout. The
//! monitor sees that through itself, so a caller killed meanwhile changes
//! nothing. [`wait`] stays until the monitor says the container ended
//! ([`Frame::Exited`]).
//!
//! [`hold`] and [`release`] are a holder's (protocol version 3, stage 9,
//! step 9.7b1): one descriptor given to the monitor to keep until the
//! container has ended (passed with the frame: `SCM_RIGHTS`, TLPI §61.13),
//! or one let go of, each by its label; the monitor answers
//! ([`Frame::Done`]).

use std::io::{Read as _, Write as _};
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use super::{ContainerError, Result, Status, io};
use crate::attach::{CONTROL_VERSION, Decoder, Frame, FrameError, HOLD_VERSION, VERSION};
use crate::process::Signal;

/// How long the monitor has to let the client in.
const WELCOME_WAIT: Duration = Duration::from_secs(5);

/// A control client, let in.
struct Control {
    stream: UnixStream,
    decoder: Decoder,
    id: String,
}

/// How connecting went, short of an error.
enum Connected {
    /// Let in.
    In(Control),
    /// The container has ended, or is ending: its monitor no longer lets
    /// anyone in (or is gone, its init with it).
    Ended,
}

/// Whether an I/O error on the socket means the monitor let go of it: it
/// does so at the container's end (an embryo connection is reset).
fn let_go(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
    )
}

impl Control {
    /// Connects to container `id`'s monitor as a control client: `hello`
    /// says which kind, `first` is the first version with that kind (an
    /// older monitor refuses it: [`ContainerError::NoControl`]). The read
    /// timeout ([`WELCOME_WAIT`]) stays if `brief` (the client is answered
    /// at once), else it's cleared (a waiter waits for the end).
    fn connect(root: &Path, id: &str, hello: &Frame, first: u32, brief: bool) -> Result<Connected> {
        let stream = match super::attach_stream(root, id) {
            Ok(stream) => stream,
            // Gone with its init, stopped, or its state deleted meanwhile
            // (its clean-up): ended, for a caller that saw it up.
            Err(
                ContainerError::MonitorGone(_)
                | ContainerError::NotFound(_)
                | ContainerError::WrongStatus {
                    status: Status::Stopped,
                    ..
                },
            ) => return Ok(Connected::Ended),
            Err(error) => return Err(error),
        };
        let mut control = Self {
            stream,
            decoder: Decoder::new(),
            id: id.to_owned(),
        };
        control
            .stream
            .set_read_timeout(Some(WELCOME_WAIT))
            .map_err(io("waiting for the monitor"))?;
        let welcome = match control.send(hello) {
            Ok(()) => control.next(),
            Err(Some(error)) => return Err(error),
            Err(None) => Ok(None),
        };
        let up = || up(root, id);
        match welcome {
            Ok(Some(Frame::Welcome { version, .. })) if version >= first => {}
            // Let go: as the container ends, or (still up) a monitor with no
            // control channel, which drops a control hello.
            Ok(Some(Frame::Welcome { .. }) | None) if up() => {
                return Err(ContainerError::NoControl(id.to_owned()));
            }
            Ok(Some(Frame::Refused(reason))) if up() => {
                return Err(ContainerError::MonitorRefused {
                    id: id.to_owned(),
                    reason,
                });
            }
            Ok(_) => return Ok(Connected::Ended),
            Err(error) => return Err(error),
        }
        if !brief {
            control
                .stream
                .set_read_timeout(None)
                .map_err(io("waiting for the monitor"))?;
        }
        Ok(Connected::In(control))
    }

    /// An error writing to the monitor (one that isn't its letting go).
    fn write_error(&self, error: std::io::Error) -> ContainerError {
        io(format!("writing to container {}'s monitor", self.id))(error)
    }

    /// Sends `frame`. `Err(None)`: the monitor let go of the socket (the
    /// container's end).
    fn send(&mut self, frame: &Frame) -> std::result::Result<(), Option<ContainerError>> {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        match self.stream.write_all(&bytes) {
            Ok(()) => Ok(()),
            Err(error) if let_go(&error) => Err(None),
            Err(error) => Err(Some(self.write_error(error))),
        }
    }

    /// Sends `frame` with `fd` passed alongside (`SCM_RIGHTS`: the monitor
    /// gets its own copy), in one message. `Err(None)`: as
    /// [`send`](Self::send).
    fn send_with_fd(
        &mut self,
        frame: &Frame,
        fd: BorrowedFd<'_>,
    ) -> std::result::Result<(), Option<ContainerError>> {
        use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
        use std::os::fd::AsRawFd as _;
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        let fds = [fd.as_raw_fd()];
        let iov = [std::io::IoSlice::new(&bytes)];
        let sent = loop {
            match sendmsg::<()>(
                self.stream.as_raw_fd(),
                &iov,
                &[ControlMessage::ScmRights(&fds)],
                MsgFlags::MSG_NOSIGNAL,
                None,
            ) {
                Ok(sent) => break sent,
                Err(nix::errno::Errno::EINTR) => {}
                Err(errno) => {
                    let error = std::io::Error::from(errno);
                    return Err((!let_go(&error)).then(|| self.write_error(error)));
                }
            }
        };
        // The descriptor went with the first byte: the rest, if the socket
        // took less (a frame is short: it doesn't), goes plain.
        match bytes.get(sent..).filter(|rest| !rest.is_empty()) {
            Some(rest) => self
                .stream
                .write_all(rest)
                .map_err(|error| (!let_go(&error)).then(|| self.write_error(error))),
            None => Ok(()),
        }
    }

    /// The next frame, skipping kinds this side doesn't know; `None` at the
    /// end of the stream (or a reset one, the monitor letting go).
    ///
    /// # Errors
    ///
    /// A read that failed otherwise; no word within [`WELCOME_WAIT`] while
    /// a timeout is set (waiting to be let in, or a holder's answer:
    /// [`ContainerError::MonitorRefused`]).
    fn next(&mut self) -> Result<Option<Frame>> {
        let mut buffer = [0_u8; 4096];
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => return Ok(Some(frame)),
                Ok(None) | Err(FrameError::Kind(_)) => {}
                Err(_) => return Ok(None),
            }
            match self.stream.read(&mut buffer) {
                Ok(0) => return Ok(None),
                Ok(read) => self.decoder.push(buffer.get(..read).unwrap_or_default()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if let_go(&error) => return Ok(None),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(ContainerError::MonitorRefused {
                        id: self.id.clone(),
                        reason: format!("no answer in {} s", WELCOME_WAIT.as_secs()),
                    });
                }
                Err(error) => {
                    return Err(io(format!("reading container {}'s monitor", self.id))(
                        error,
                    ));
                }
            }
        }
    }
}

/// Asks container `id`'s monitor to stop it: `signal` to its init now, and
/// SIGKILL once `timeout` has passed if it's still there. Returns once the
/// monitor has the request (not once the container has ended): the monitor
/// sees it through. `Ok(false)`: the container has ended or is ending (a
/// monitor that's gone took its init with it): nothing to stop.
///
/// # Errors
///
/// No such container, one that can't be attached to, a signal that can't
/// stop it ([`ContainerError::CantStop`]), a monitor with no control channel
/// ([`ContainerError::NoControl`]), or one refusing
/// ([`ContainerError::MonitorRefused`]).
pub fn stop(root: &Path, id: &str, signal: Signal, timeout: Duration) -> Result<bool> {
    let number = signal.as_raw();
    // (Every signal is below 256: `Signal` is a real one.)
    let byte = u8::try_from(number)
        .ok()
        .filter(|_| crate::process::is_stop_signal(number))
        .ok_or_else(|| ContainerError::CantStop {
            id: id.to_owned(),
            signal: number,
        })?;
    let Connected::In(mut control) =
        Control::connect(root, id, &control_hello(true), CONTROL_VERSION, true)?
    else {
        return Ok(false);
    };
    let stop = Frame::Stop {
        signal: byte,
        timeout_secs: u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX),
    };
    match control.send(&stop) {
        Ok(()) => Ok(true),
        Err(None) => Ok(false),
        Err(Some(error)) => Err(error),
    }
}

/// A control client's hello: a `stopper`'s, or a waiter's.
fn control_hello(stopper: bool) -> Frame {
    Frame::HelloControl {
        version: VERSION,
        stopper,
    }
}

/// Whether container `id` is up, as its state says (not ended).
fn up(root: &Path, id: &str) -> bool {
    matches!(
        super::peek_status(root, id),
        Ok(Some(Status::Running | Status::Paused | Status::Created))
    )
}

/// What a holder asks of the monitor.
enum Request<'a> {
    /// Hold this descriptor, by this label.
    Hold(&'a str, BorrowedFd<'a>),
    /// Let go of what's held by this label.
    Release(&'a str),
}

/// What [`Control::connect`]'s refusal means to a holder: a monitor with no
/// control channel, or one from before holders, drops a holder's hello
/// (`NoControl`): too old.
fn holder_error(error: ContainerError) -> ContainerError {
    match error {
        ContainerError::NoControl(id) => ContainerError::MonitorTooOld(id),
        other => other,
    }
}

/// Connects to container `id`'s monitor as a holder, for `request`, and
/// returns once the monitor has answered.
///
/// If no answer comes (a timeout, the connection lost), the request may or
/// may not have been done: [`ContainerError::HoldUnanswered`] says so, and
/// the caller must allow for both. [`ContainerError::NotHeld`] is a definite
/// no.
fn as_holder(root: &Path, id: &str, request: &Request<'_>) -> Result<()> {
    let not_held = |why: &str| ContainerError::NotHeld {
        id: id.to_owned(),
        why: why.to_owned(),
    };
    let unanswered = |why: &str| ContainerError::HoldUnanswered {
        id: id.to_owned(),
        why: why.to_owned(),
    };
    // Gone, or still up: a monitor that let go without answering.
    let gone = || {
        if up(root, id) {
            unanswered("it let go without answering")
        } else {
            not_held("the container has ended")
        }
    };
    let hello = Frame::HelloHolder { version: VERSION };
    let mut control = match Control::connect(root, id, &hello, HOLD_VERSION, true) {
        Ok(Connected::In(control)) => control,
        Ok(Connected::Ended) => return Err(not_held("the container has ended")),
        Err(error) => return Err(holder_error(error)),
    };
    let sent = match *request {
        Request::Hold(label, fd) => control.send_with_fd(
            &Frame::Hold {
                label: label.to_owned(),
            },
            fd,
        ),
        Request::Release(label) => control.send(&Frame::Release {
            label: label.to_owned(),
        }),
    };
    match sent {
        Ok(()) => {}
        Err(None) => return Err(gone()),
        Err(Some(error)) => return Err(error),
    }
    loop {
        match control.next() {
            Ok(Some(Frame::Done { ok: true, .. })) => return Ok(()),
            Ok(Some(Frame::Done { ok: false, why })) => return Err(not_held(&why)),
            Ok(Some(Frame::Exited(_)) | None) => return Err(gone()),
            Ok(Some(_)) => {}
            Err(ContainerError::MonitorRefused { reason, .. }) => {
                return Err(unanswered(&reason));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Gives container `id`'s monitor `fd` to hold as `label` until the
/// container has ended: then it's the exit command's, with its label
/// ([`HOLD_LABELS_ENV`](super::super::HOLD_LABELS_ENV)). The monitor has
/// its own copy once this returns: the caller may close its own.
///
/// # Errors
///
/// A label that isn't one ([`ContainerError::BadHoldLabel`]); no such
/// container, one without an attach socket (no logs:
/// [`ContainerError::NotAttachable`]), one whose monitor is older
/// ([`ContainerError::MonitorTooOld`]), or the monitor not holding it
/// ([`ContainerError::NotHeld`]: a label it holds already, too many held,
/// a descriptor that isn't a file, directory or socket, the container
/// starting or ending, or no answer).
pub fn hold(root: &Path, id: &str, label: &str, fd: BorrowedFd<'_>) -> Result<()> {
    if !crate::attach::is_label(label) {
        return Err(ContainerError::BadHoldLabel(label.to_owned()));
    }
    as_holder(root, id, &Request::Hold(label, fd))
}

/// Has container `id`'s monitor let go of (close) what it holds as `label`
/// (given at `create`, or by [`hold`]). What that kept is free once the
/// other copies are closed too.
///
/// # Errors
///
/// As [`hold`]; [`ContainerError::NotHeld`] for a label it holds none of,
/// or more than one of.
pub fn release(root: &Path, id: &str, label: &str) -> Result<()> {
    if !crate::attach::is_label(label) {
        return Err(ContainerError::BadHoldLabel(label.to_owned()));
    }
    as_holder(root, id, &Request::Release(label))
}

/// Waits for container `id` to end, as its monitor tells it: its exit code
/// (125: not known). `Ok(None)`: it has ended (or is ending) already, or its
/// monitor is gone: what's recorded says how it ended.
///
/// # Errors
///
/// No such container, one that can't be attached to, a monitor with no
/// control channel ([`ContainerError::NoControl`]), or one refusing
/// ([`ContainerError::MonitorRefused`]: too many waiters).
pub fn wait(root: &Path, id: &str) -> Result<Option<i32>> {
    match waiter(root, id)? {
        Some(waiter) => waiter.exit_code(),
        None => Ok(None),
    }
}

/// A waiter let in by container `id`'s monitor, to be told its exit code
/// later ([`Waiter::exit_code`]): connected now, so the code reaches it
/// however far behind its other clients fall (a foreground run's own
/// attachment, let go of, still gets it; stage 9, step 9.4c3a). `None`: it
/// has ended (or is ending) already, or its monitor is gone.
///
/// # Errors
///
/// As [`wait`].
pub fn waiter(root: &Path, id: &str) -> Result<Option<Waiter>> {
    match Control::connect(root, id, &control_hello(false), CONTROL_VERSION, false)? {
        Connected::In(control) => Ok(Some(Waiter(control))),
        Connected::Ended => Ok(None),
    }
}

/// A waiter ([`waiter`]): the monitor tells it the container's exit code.
pub struct Waiter(Control);

impl Waiter {
    /// Waits for the container to end: its exit code (125: not known).
    /// `Ok(None)`: its monitor let go without saying.
    ///
    /// # Errors
    ///
    /// The monitor's connection failing.
    pub fn exit_code(mut self) -> Result<Option<i32>> {
        loop {
            match self.0.next()? {
                Some(Frame::Exited(code)) => return Ok(Some(code)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }
}

impl std::fmt::Debug for Waiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waiter").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor that drops a holder's hello (no control channel, or one
    /// from before holders) is too old for one; other errors are as they
    /// are.
    #[test]
    fn a_monitor_dropping_a_holder_is_too_old() {
        assert!(matches!(
            holder_error(ContainerError::NoControl("web".to_owned())),
            ContainerError::MonitorTooOld(id) if id == "web"
        ));
        assert!(matches!(
            holder_error(ContainerError::NotFound("web".to_owned())),
            ContainerError::NotFound(_)
        ));
    }
}
