//! Attaching to a container (stage 8, step 8.3c; here since stage 10): the
//! client of its monitor's `attach.sock` ([the protocol](super)'s frames).
//!
//! What the user types goes to the container (if it takes input), and its
//! output comes back: to this process's stdout and stderr, or, for a
//! container with a terminal, to the user's terminal in raw mode (each key a
//! byte, Ctrl-C included, as with `run -it`), sized as it is and resized
//! with it. Typing the detach keys (Ctrl-P Ctrl-Q) leaves; the container
//! goes on. The client ends with the container, with its exit code.
//!
//! One loop, `poll` over the user's input, the socket and a self-pipe for
//! SIGWINCH (the session's own, or its caller's). Writes to the socket are buffered, never waited for: the
//! monitor drops a client that falls behind (unless it's held, a
//! foreground run's own: then the container waits for it), and a client
//! blocked on a write would stop reading.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::{Decoder, DetachKeys, Frame, FrameError, VERSION};
use crate::container::ContainerError;
use crate::process::ResizeWatch;
use crate::process::{Signal, SignalWatch};
use crate::terminal::{RawMode, Size, terminal_size};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

/// How long the monitor may take to answer the hello.
const WELCOME_WAIT: Duration = Duration::from_secs(10);

/// What to attach with.
#[derive(Debug, Clone)]
pub struct AttachOptions {
    /// The keys that detach.
    pub detach_keys: DetachKeys,
    /// Pass the user's input on (`--no-stdin` turns it off).
    pub stdin: bool,
    /// The container waits for this client rather than let it go when it
    /// falls behind ([`Frame::HelloHeld`]): a foreground run's own (stage 9,
    /// step 9.4c3a), so a slow reader loses no output and a fast writer no
    /// input.
    pub held: bool,
}

/// How an attachment ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The detach keys were typed: the container goes on.
    Detached,
    /// The container ended, with this exit code.
    Exited(i32),
    /// The monitor let go of this client: it fell too far behind the
    /// container's output (the reason).
    Dropped(String),
    /// The user's terminal hung up (closed): the container goes on, as
    /// after a detach.
    HungUp,
    /// A signalled session's caller chose to leave (its `decide` said
    /// [`Act::Leave`]), with this exit code.
    Left(i32),
}

/// What a signalled session ([`Welcomed::session_signalled`]) does with a
/// signal its watch caught (other than SIGWINCH, which resizes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Pass it on, by this number (the other end's: a Mac's are sent as
    /// Linux's).
    Send(u8),
    /// Leave it.
    Skip,
    /// End the session, with this exit code ([`Ended::Left`]).
    Leave(i32),
}

/// A signalled session's watch and what's decided for each signal.
type Signalled<'a> = (&'a SignalWatch, &'a mut dyn FnMut(Signal) -> Act);

/// An attachment that couldn't go on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttachError {
    /// The monitor didn't let this client in (the reason: too many clients).
    #[error("the container's monitor refused: {0}")]
    Refused(String),
    /// The monitor speaks another version of the protocol.
    #[error(
        "the container's monitor speaks attach protocol version {0}, this Shipyard {VERSION}: \
         attach with the Shipyard that created it"
    )]
    Version(u32),
    /// The monitor closed the connection before letting this client in:
    /// the container isn't this user's, or it was just ending.
    #[error(
        "the container's monitor closed the connection without letting this client in (not \
         your container to attach to, or it was ending)"
    )]
    NotLetIn,
    /// The monitor didn't answer the hello in time: busy setting the
    /// container up (its hooks), or stopped.
    #[error(
        "the container's monitor didn't answer within {} s (still setting the container up?)",
        WELCOME_WAIT.as_secs()
    )]
    NoAnswer,
    /// The monitor sent a frame of a kind it never sends at that point.
    #[error("the container's monitor sent an unexpected {0} frame")]
    Unexpected(&'static str),
    /// The monitor went away (killed) before the container ended.
    #[error("the container's monitor is gone: nothing more can come from the container")]
    MonitorGone,
    /// The monitor sent something that isn't the protocol.
    #[error(transparent)]
    Protocol(#[from] FrameError),
    /// A read or write failed (the user's terminal, the socket).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<Errno> for AttachError {
    fn from(errno: Errno) -> Self {
        Self::Io(errno.into())
    }
}

/// Attaches through `stream` (connected to the container's `attach.sock`),
/// as `options` say, until the container ends, the detach keys are typed,
/// or the monitor lets go.
///
/// # Errors
///
/// [`AttachError`].
pub fn attach(stream: UnixStream, options: &AttachOptions) -> Result<Ended, AttachError> {
    welcomed(stream, options)?.session()
}

/// A client the monitor has let in, its session not begun: what the
/// container writes from now on comes to it (the monitor sends output to
/// clients it has let in). `start -a` is let in before the container's
/// start gate opens, so nothing its command writes is missed.
#[derive(Debug)]
pub struct Welcomed {
    stream: UnixStream,
    decoder: Decoder,
    terminal: bool,
    stdin: bool,
    /// What its session goes on with.
    options: AttachOptions,
}

/// Says hello through `stream` (connected to the container's
/// `attach.sock`) as `options` say, and waits to be let in.
///
/// # Errors
///
/// [`AttachError`]: refused, another version, no answer.
pub fn welcomed(stream: UnixStream, options: &AttachOptions) -> Result<Welcomed, AttachError> {
    let (version, detach_keys) = (VERSION, options.detach_keys.bytes().to_vec());
    let hello = if options.held {
        Frame::HelloHeld {
            version,
            detach_keys,
        }
    } else {
        Frame::Hello {
            version,
            detach_keys,
        }
    };
    welcomed_with(stream, &hello, options)
}

/// As [`welcomed`], with `hello` said instead of an attach hello: a
/// forwarded command's ([`Frame::HelloRun`], to the macOS guest's agent,
/// step 10.3b2), whose answer is a session as an attachment's is.
///
/// # Errors
///
/// [`AttachError`]: refused, another version, no answer.
pub fn welcomed_with(
    mut stream: UnixStream,
    hello: &Frame,
    options: &AttachOptions,
) -> Result<Welcomed, AttachError> {
    let mut bytes = Vec::new();
    hello.encode(&mut bytes);
    let hello = bytes;
    // Closed at once: someone else's container, or refused before the hello
    // was read (said in a frame waiting to be read: `welcome` reads it).
    let said = stream.write_all(&hello);
    let mut decoder = Decoder::new();
    let (terminal, stdin) = welcome(&mut stream, &mut decoder, said.is_ok())?;
    Ok(Welcomed {
        stream,
        decoder,
        terminal,
        stdin,
        options: options.clone(),
    })
}

impl Welcomed {
    /// The session, with the options it said hello with, until the
    /// container ends, the detach keys are typed, or the monitor lets go.
    /// A terminal's session watches SIGWINCH and SIGCONT itself: a caller
    /// with a signal watch of its own (one per process) passes its resizes
    /// with [`session_resized`](Self::session_resized) instead.
    ///
    /// # Errors
    ///
    /// [`AttachError`]; for a terminal's session, a signal watch already in
    /// this process.
    pub fn session(self) -> Result<Ended, AttachError> {
        self.session_with(None, None)
    }

    /// [`session`](Self::session), with this process's signals watched by
    /// `watch` (the one per process): SIGWINCH resizes the window, and each
    /// other signal is `decide`d: passed on as a [`Frame::Signal`], left,
    /// or the session left ([`Ended::Left`]). For a forwarded command
    /// (step 10.3b2), whose signals are its own.
    ///
    /// # Errors
    ///
    /// [`AttachError`].
    pub fn session_signalled(
        self,
        watch: &SignalWatch,
        decide: &mut dyn FnMut(Signal) -> Act,
    ) -> Result<Ended, AttachError> {
        self.session_with(Some(watch.fd()), Some((watch, decide)))
    }

    /// [`session`](Self::session), the window's resizes told by `resizes`
    /// (readable once one or more came: drained, then the size sent) rather
    /// than a SIGWINCH watch of the session's own: for a caller whose
    /// signal watch has SIGWINCH already (a foreground run's, stage 9, step
    /// 9.4c3b: one watch per process).
    ///
    /// # Errors
    ///
    /// [`AttachError`].
    pub fn session_resized(self, resizes: BorrowedFd<'_>) -> Result<Ended, AttachError> {
        self.session_with(Some(resizes), None)
    }

    fn session_with(
        self,
        resizes: Option<BorrowedFd<'_>>,
        signalled: Option<Signalled<'_>>,
    ) -> Result<Ended, AttachError> {
        let Self {
            stream,
            decoder,
            terminal,
            stdin,
            options,
        } = self;
        let typing = options.stdin && stdin;
        // Raw only for a terminal, typed into from one, in the foreground
        // (`RawMode::set_in_foreground` waits for it: stopped until `fg`):
        // each key a byte, for the container's terminal to make sense of.
        // Typing from a terminal into a container without one: lines, as
        // the user edits them, but without flow control, which would take
        // Ctrl-Q (the detach keys' second) for itself.
        let (raw, wants_raw) = if !typing {
            (None, false)
        } else if terminal {
            match RawMode::set_in_foreground(libc::STDIN_FILENO) {
                Ok(Some(raw)) => (Some(raw), false),
                // Still in the background (`bg`): raw once it's in the
                // foreground.
                Ok(None) => (None, true),
                Err(_) => (None, false),
            }
        } else {
            let raw = RawMode::without_flow_control(libc::STDIN_FILENO)
                .ok()
                .flatten();
            (raw, false)
        };
        stream.set_nonblocking(true)?;
        // The window's resizes: the caller's watch's, else one of the
        // session's own.
        let own = match resizes {
            Some(_) => None,
            None => terminal.then(own_resizes).transpose()?,
        };
        let resizes = resizes.or_else(|| own.as_ref().map(ResizeWatch::fd));
        let mut session = Session {
            stream,
            decoder,
            out: Vec::new(),
            stdin_open: typing,
            // A terminal's end (closed: a hang-up) ends the session; a
            // file's or a pipe's ends only the input.
            hangs_up: typing && terminal && std::io::stdin().is_terminal(),
            raw,
            wants_raw,
            window: if terminal {
                Window::Unsent
            } else {
                Window::NotATerminal
            },
        };
        session.resize_if_changed();
        // A signalled session's watch is woken for every signal, terminal
        // or not; a resize watch only matters for a terminal's.
        let resizes = if signalled.is_some() {
            resizes
        } else {
            terminal.then_some(resizes).flatten()
        };
        session.run(resizes, signalled)
    }
}

/// Waits for the monitor's answer to the hello (`said`: whether the hello
/// went; if not, only a refusal already sent can be there).
fn welcome(
    stream: &mut UnixStream,
    decoder: &mut Decoder,
    said: bool,
) -> Result<(bool, bool), AttachError> {
    stream.set_read_timeout(Some(WELCOME_WAIT))?;
    let mut buffer = [0_u8; 4096];
    loop {
        if let Some(frame) = decoder.next_frame()? {
            return match frame {
                Frame::Refused(reason) => Err(AttachError::Refused(reason)),
                _ if !said => Err(AttachError::NotLetIn),
                Frame::Welcome {
                    version,
                    terminal,
                    stdin,
                } if (1..=VERSION).contains(&version) => Ok((terminal, stdin)),
                Frame::Welcome { version, .. } => Err(AttachError::Version(version)),
                other => Err(AttachError::Unexpected(name(&other))),
            };
        }
        match stream.read(&mut buffer) {
            Ok(read) if read > 0 => decoder.push(buffer.get(..read).unwrap_or_default()),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err)
                if said
                    && matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
            {
                return Err(AttachError::NoAnswer);
            }
            Ok(_) | Err(_) => return Err(AttachError::NotLetIn),
        }
    }
}

/// A session's window, as its size was last sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// Not a terminal's session: no size is sent.
    NotATerminal,
    /// A terminal's, none sent yet.
    Unsent,
    /// A terminal's, this size sent last.
    Sent(Size),
}

struct Session {
    stream: UnixStream,
    decoder: Decoder,
    /// Frames for the monitor, not written yet.
    out: Vec<u8>,
    /// Whether the user's input is still read.
    stdin_open: bool,
    /// Whether the user's input is a terminal typed into a container's: its
    /// hang-up ends the session.
    hangs_up: bool,
    /// The terminal in raw mode, while the session is.
    raw: Option<RawMode>,
    /// Raw mode wanted and not had yet (left in the background by `bg`).
    wants_raw: bool,
    /// The window's size, as last sent.
    window: Window,
}

impl Session {
    fn run(
        &mut self,
        resizes: Option<BorrowedFd<'_>>,
        mut signalled: Option<Signalled<'_>>,
    ) -> Result<Ended, AttachError> {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            self.flush()?;
            // Raw mode due (left in the background by `bg`): looked for
            // again each time round. A resume wakes the loop (SIGCONT, on the
            // resize pipe: `fg` of a stopped job), and so does a quarter of
            // a second (`fg` of a running one sends none).
            self.go_raw_if_due();
            let raw_due = self.raw_due();
            let socket_events = if self.out.is_empty() {
                PollFlags::POLLIN
            } else {
                PollFlags::POLLIN | PollFlags::POLLOUT
            };
            // SAFETY: fd 0 stays open for the whole call.
            let stdin = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) };
            let mut fds = vec![PollFd::new(self.stream.as_fd(), socket_events)];
            // Typing waits while frames are queued (the socket is full).
            // The monitor reads every frame (so the detach keys always
            // arrive) and keeps 64 KiB of typing for a container slow to
            // read it: past that, typing is lost (`attach < big-file` into
            // a slow reader; runtime.md says so).
            // Not while raw mode is due: from the background a read stops
            // this process (SIGTTIN) inside it, and after `fg` the read,
            // restarted, would wait for a whole line, cooked.
            let reading_stdin = self.stdin_open && self.out.len() < 64 * 1024 && !raw_due;
            if reading_stdin {
                fds.push(PollFd::new(stdin, PollFlags::POLLIN));
            }
            if let Some(resizes) = resizes {
                fds.push(PollFd::new(resizes, PollFlags::POLLIN));
            }
            let timeout = if raw_due {
                PollTimeout::from(250_u16)
            } else {
                PollTimeout::NONE
            };
            match poll(&mut fds, timeout) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno.into()),
            }
            let ready = |at: usize| {
                fds.get(at)
                    .and_then(PollFd::revents)
                    .is_some_and(|events| !events.is_empty())
            };
            let (socket_ready, stdin_ready) = (ready(0), reading_stdin && ready(1));
            let stdin_hung_up = reading_stdin
                && fds
                    .get(1)
                    .and_then(PollFd::revents)
                    .is_some_and(|events| events.contains(PollFlags::POLLHUP));
            let resized = resizes.is_some() && ready(if reading_stdin { 2 } else { 1 });
            drop(fds);
            if resized && let Some(resizes) = resizes {
                match signalled.as_mut() {
                    Some((watch, decide)) => {
                        if let Some(left) = self.pass_on(watch, decide) {
                            return Ok(left);
                        }
                    }
                    None => drain(resizes),
                }
                self.resize_if_changed();
            }
            if stdin_ready && self.hangs_up && stdin_hung_up {
                return Ok(Ended::HungUp);
            }
            if stdin_ready {
                match nix::unistd::read(stdin, &mut buffer) {
                    Ok(read) if read > 0 => {
                        // A resize before the typing that follows it, even
                        // one whose SIGWINCH hasn't reached the resize pipe
                        // yet (a caller's watch passes it on from a thread
                        // of its own: late, under load, F68).
                        self.resize_if_changed();
                        Frame::Stdin(buffer.get(..read).unwrap_or_default().to_vec())
                            .encode(&mut self.out);
                    }
                    Err(Errno::EAGAIN | Errno::EINTR) => {}
                    // The user's terminal is gone.
                    Err(Errno::EIO) if self.hangs_up => return Ok(Ended::HungUp),
                    // The user's input ended: the monitor is told (with
                    // `--stdin-once`, the container's stdin then ends too),
                    // and the output goes on.
                    Ok(_) | Err(_) => {
                        self.stdin_open = false;
                        Frame::StdinEnd.encode(&mut self.out);
                    }
                }
            }
            if socket_ready && let Some(ended) = self.receive(&mut buffer)? {
                return Ok(ended);
            }
        }
    }

    /// The signals `watch` caught, each as `decide` says (SIGWINCH aside:
    /// the caller resizes); the session's end if one is to leave it.
    fn pass_on(
        &mut self,
        watch: &SignalWatch,
        decide: &mut dyn FnMut(Signal) -> Act,
    ) -> Option<Ended> {
        for signal in watch.take().signals {
            if signal.as_raw() == libc::SIGWINCH {
                continue;
            }
            match decide(signal) {
                Act::Send(number) => Frame::Signal(number).encode(&mut self.out),
                Act::Skip => {}
                Act::Leave(code) => return Some(Ended::Left(code)),
            }
        }
        None
    }

    /// For a terminal's session, the window's size sent if it isn't the
    /// last one sent (`TIOCGWINSZ`): at the start, at each resize, and
    /// before each piece of typing, so typing never reaches the container
    /// before the size it was typed at.
    fn resize_if_changed(&mut self) {
        self.resize_to(terminal_size());
    }

    /// [`resize_if_changed`](Self::resize_if_changed) with the window's
    /// size (`size`, none if it can't be read) given.
    fn resize_to(&mut self, size: Option<Size>) {
        if self.window == Window::NotATerminal {
            return;
        }
        if let Some(size @ Size { rows, cols }) = size
            && self.window != Window::Sent(size)
        {
            self.window = Window::Sent(size);
            Frame::Resize { rows, cols }.encode(&mut self.out);
        }
    }

    /// Whether raw mode is wanted and not on: left or resumed in the
    /// background (`bg`).
    fn raw_due(&self) -> bool {
        self.wants_raw || self.raw.as_ref().is_some_and(|raw| !raw.is_applied())
    }

    /// Sets the terminal raw if that's due and it's in the foreground now.
    fn go_raw_if_due(&mut self) {
        if self.wants_raw {
            match RawMode::set(libc::STDIN_FILENO) {
                Ok(Some(raw)) => {
                    self.raw = Some(raw);
                    self.wants_raw = false;
                }
                Ok(None) => {}
                Err(_) => self.wants_raw = false,
            }
        } else if let Some(raw) = &self.raw
            && !raw.is_applied()
        {
            raw.reapply();
        }
    }

    /// Writes what the socket takes now.
    fn flush(&mut self) -> Result<(), AttachError> {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(written) => {
                    self.out.drain(..written);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Err(AttachError::MonitorGone);
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// Reads what the monitor sent, handing on the output.
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<Ended>, AttachError> {
        match self.stream.read(buffer) {
            Ok(0) => return Err(AttachError::MonitorGone),
            Ok(read) => self.decoder.push(buffer.get(..read).unwrap_or_default()),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        }
        loop {
            let frame = match self.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(None),
                // A newer monitor's kind: skipped.
                Err(FrameError::Kind(_)) => continue,
                Err(err) => return Err(err.into()),
            };
            match frame {
                Frame::Stdout(bytes) => write_all(libc::STDOUT_FILENO, &bytes)?,
                Frame::Stderr(bytes) => write_all(libc::STDERR_FILENO, &bytes)?,
                Frame::Detached => return Ok(Some(Ended::Detached)),
                Frame::Exited(code) => return Ok(Some(Ended::Exited(code))),
                Frame::Dropped(reason) => return Ok(Some(Ended::Dropped(reason))),
                other => return Err(AttachError::Unexpected(name(&other))),
            }
        }
    }
}

/// A frame's name, for [`AttachError::Unexpected`].
fn name(frame: &Frame) -> &'static str {
    match frame {
        Frame::Hello { .. } => "hello",
        Frame::Stdin(_) => "stdin",
        Frame::StdinEnd => "end-of-stdin",
        Frame::Resize { .. } => "resize",
        Frame::Welcome { .. } => "welcome",
        Frame::Refused(_) => "refused",
        Frame::Stdout(_) => "stdout",
        Frame::Stderr(_) => "stderr",
        Frame::Detached => "detached",
        Frame::Dropped(_) => "dropped",
        Frame::Exited(_) => "exited",
        _ => "unknown",
    }
}

/// Writes all of `bytes` to `fd` (the user's stdout or stderr), waiting as
/// needed (a terminal takes its time).
fn write_all(fd: i32, mut bytes: &[u8]) -> Result<(), AttachError> {
    // SAFETY: the standard descriptors stay open for the whole call.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    while !bytes.is_empty() {
        match nix::unistd::write(fd, bytes) {
            Ok(written) => bytes = bytes.get(written..).unwrap_or_default(),
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) => {
                let mut fds = [PollFd::new(fd, PollFlags::POLLOUT)];
                let _ = poll(&mut fds, PollTimeout::from(100_u16));
            }
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}

/// A session's own resize watch (SIGWINCH and SIGCONT only: a resize, or
/// a resume, raw mode looked at again), its failure as the session's.
fn own_resizes() -> Result<ResizeWatch, AttachError> {
    ResizeWatch::install().map_err(|error| match error.errno() {
        Some(errno) => errno.into(),
        None => std::io::Error::other(error).into(),
    })
}

/// Empties the non-blocking pipe `read` (a resize watch's): one resize for
/// any number of signals.
fn drain(read: BorrowedFd<'_>) {
    let mut sink = [0_u8; 64];
    while matches!(nix::unistd::read(read, &mut sink), Ok(read) if read > 0) {}
}

/// What an attachment came to, for its caller's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// Detached (the keys typed), or its reader stopped reading (`attach
    /// … | head`): the container goes on (0).
    Left,
    /// The container ended, with this exit code.
    Exited(i32),
}

/// Why an attachment came to nothing (see [`settle`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettleError {
    /// Its container couldn't be reached (no such container, not up, its
    /// monitor gone with no end recorded).
    #[error(transparent)]
    Container(#[from] ContainerError),
    /// The attachment failed.
    #[error(transparent)]
    Attach(#[from] AttachError),
    /// The monitor let go of this client: it fell behind (the reason).
    #[error("the container's monitor let go of this attach: {0}")]
    Dropped(String),
}

/// What an attachment comes to (F38: one answer, for every attach). A
/// container that ended just as this attached (its monitor gone, or
/// refusing, before the container was seen as stopped) ends with the code
/// `ended_code` finds recorded; without one, that's the failure.
///
/// # Errors
///
/// The attachment's failure, the end race aside; a client the monitor let
/// go of (it fell behind).
pub fn settle(
    attached: Result<Result<Ended, AttachError>, ContainerError>,
    ended_code: impl FnOnce() -> Option<i32>,
) -> Result<Settled, SettleError> {
    match attached {
        Err(error @ ContainerError::MonitorGone(_)) => ended_code()
            .map(Settled::Exited)
            .ok_or_else(|| error.into()),
        Err(error) => Err(error.into()),
        Ok(Ok(Ended::Detached | Ended::HungUp | Ended::Left(_))) => Ok(Settled::Left),
        Ok(Ok(Ended::Exited(code))) => Ok(Settled::Exited(code)),
        Ok(Ok(Ended::Dropped(reason))) => Err(SettleError::Dropped(reason)),
        Ok(Err(
            error @ (AttachError::Refused(_) | AttachError::MonitorGone | AttachError::NotLetIn),
        )) => ended_code()
            .map(Settled::Exited)
            .ok_or_else(|| error.into()),
        // `attach … | head`: the reader has seen what it wanted.
        Ok(Err(AttachError::Io(error))) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            Ok(Settled::Left)
        }
        Ok(Err(error)) => Err(error.into()),
    }
}

/// Attaches to runtime container `id` under `root` (`shipyard-runtime
/// attach`): what [`settle`] says, the end race settled by the runtime's
/// exit record.
///
/// # Errors
///
/// As [`settle`].
pub fn to_runtime(
    root: &std::path::Path,
    id: &str,
    options: &AttachOptions,
) -> Result<Settled, SettleError> {
    use crate::container;
    let attached = container::attach_stream(root, id).map(|stream| attach(stream, options));
    settle(attached, || container::exit_code(root, id).ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal session's size is sent once per change (F68: it's
    /// looked at before every piece of typing): the first, not the same
    /// again, a new one; a session that isn't a terminal's sends none.
    #[test]
    fn a_window_size_is_sent_once_per_change() {
        let (stream, _other) = UnixStream::pair().unwrap();
        let mut session = Session {
            stream,
            decoder: Decoder::new(),
            out: Vec::new(),
            stdin_open: true,
            hangs_up: false,
            raw: None,
            wants_raw: false,
            window: Window::Unsent,
        };
        let sent = |session: &mut Session, size| {
            session.out.clear();
            session.resize_to(size);
            let mut decoder = Decoder::new();
            decoder.push(&session.out);
            let mut frames = Vec::new();
            while let Ok(Some(frame)) = decoder.next_frame() {
                frames.push(frame);
            }
            frames
        };
        assert_eq!(
            sent(&mut session, Some(Size::new(24, 80))),
            [Frame::Resize { rows: 24, cols: 80 }],
            "the first"
        );
        assert!(
            sent(&mut session, Some(Size::new(24, 80))).is_empty(),
            "the same again"
        );
        assert!(
            sent(&mut session, None).is_empty(),
            "one that can't be read"
        );
        assert_eq!(
            sent(&mut session, Some(Size::new(33, 99))),
            [Frame::Resize { rows: 33, cols: 99 }],
            "a new one"
        );
        session.window = Window::NotATerminal;
        assert!(
            sent(&mut session, Some(Size::new(10, 10))).is_empty(),
            "not a terminal's"
        );
    }

    /// Every way an attachment goes, and what it comes to: among them the
    /// race where the container ends as it connects (its monitor gone or
    /// refusing before the container is seen as stopped), which ends with
    /// the recorded code, and fails only where there's none.
    #[test]
    fn attachments_settle_as_they_went() {
        let code = |code| move || code;
        let gone = || Err(ContainerError::MonitorGone("c".into()));
        assert_eq!(settle(gone(), code(Some(3))).unwrap(), Settled::Exited(3));
        assert!(settle(gone(), code(None)).is_err());
        for late in [
            AttachError::Refused("full".into()),
            AttachError::MonitorGone,
            AttachError::NotLetIn,
        ] {
            assert_eq!(
                settle(Ok(Err(late)), code(Some(7))).unwrap(),
                Settled::Exited(7)
            );
        }
        assert!(settle(Ok(Err(AttachError::NotLetIn)), code(None)).is_err());
        assert_eq!(
            settle(Ok(Ok(Ended::Detached)), code(Some(1))).unwrap(),
            Settled::Left
        );
        assert_eq!(
            settle(Ok(Ok(Ended::Exited(4))), code(None)).unwrap(),
            Settled::Exited(4)
        );
        assert!(settle(Ok(Ok(Ended::Dropped("slow".into()))), code(Some(0))).is_err());
        let pipe = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert_eq!(
            settle(Ok(Err(AttachError::Io(pipe))), code(None)).unwrap(),
            Settled::Left
        );
        // Not the end race: never the recorded code.
        let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(settle(Ok(Err(AttachError::Io(other))), code(Some(0))).is_err());
        assert!(settle(Err(ContainerError::NotFound("c".into())), code(Some(0))).is_err());
    }
}
