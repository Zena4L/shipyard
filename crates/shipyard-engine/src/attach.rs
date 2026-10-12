//! Attaching to a container (stage 8, step 8.3c): the client of its
//! monitor's `attach.sock` ([`shipyard_runtime::attach`]'s frames).
//!
//! What the user types goes to the container (if it takes input), and its
//! output comes back: to this process's stdout and stderr, or, for a
//! container with a terminal, to the user's terminal in raw mode (each key a
//! byte, Ctrl-C included, as with `run -it`), sized as it is and resized
//! with it. Typing the detach keys (Ctrl-P Ctrl-Q) leaves; the container
//! goes on. The client ends with the container, with its exit code.
//!
//! One loop, `poll` over the user's input, the socket and a self-pipe for
//! SIGWINCH. Writes to the socket are buffered, never waited for: the
//! monitor drops a client that falls behind, and a client blocked on a
//! write would stop reading.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use shipyard_runtime::attach::{Decoder, DetachKeys, Frame, FrameError, VERSION};
use shipyard_runtime::terminal::{RawMode, terminal_size};

/// How long the monitor may take to answer the hello.
const WELCOME_WAIT: Duration = Duration::from_secs(10);

/// What to attach with.
#[derive(Debug, Clone)]
pub struct Options {
    /// The keys that detach.
    pub detach_keys: DetachKeys,
    /// Pass the user's input on (`--no-stdin` turns it off).
    pub stdin: bool,
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
}

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
pub fn attach(mut stream: UnixStream, options: &Options) -> Result<Ended, AttachError> {
    let mut hello = Vec::new();
    Frame::Hello {
        version: VERSION,
        detach_keys: options.detach_keys.bytes().to_vec(),
    }
    .encode(&mut hello);
    // Closed at once: someone else's container, or refused before the hello
    // was read (said in a frame waiting to be read: `welcome` reads it).
    let said = stream.write_all(&hello);
    let mut decoder = Decoder::new();
    let (terminal, stdin) = welcome(&mut stream, &mut decoder, said.is_ok())?;
    let typing = options.stdin && stdin;
    // Raw only for a terminal, typed into from one, in the foreground
    // (`RawMode::set` says so): each key a byte, for the container's
    // terminal to make sense of. Typing from a terminal into a container
    // without one: lines, as the user edits them, but without flow control,
    // which would take Ctrl-Q (the detach keys' second) for itself.
    let _raw = if !typing {
        None
    } else if terminal {
        RawMode::set(libc::STDIN_FILENO).ok().flatten()
    } else {
        RawMode::without_flow_control(libc::STDIN_FILENO)
            .ok()
            .flatten()
    };
    stream.set_nonblocking(true)?;
    let winch = terminal.then(Winch::install).transpose()?;
    let mut session = Session {
        stream,
        decoder,
        out: Vec::new(),
        stdin_open: typing,
    };
    if terminal && let Some((rows, cols)) = terminal_size() {
        Frame::Resize { rows, cols }.encode(&mut session.out);
    }
    session.run(winch.as_ref())
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

struct Session {
    stream: UnixStream,
    decoder: Decoder,
    /// Frames for the monitor, not written yet.
    out: Vec<u8>,
    /// Whether the user's input is still read.
    stdin_open: bool,
}

impl Session {
    fn run(&mut self, winch: Option<&Winch>) -> Result<Ended, AttachError> {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            self.flush()?;
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
            let reading_stdin = self.stdin_open && self.out.len() < 64 * 1024;
            if reading_stdin {
                fds.push(PollFd::new(stdin, PollFlags::POLLIN));
            }
            if let Some(winch) = winch {
                fds.push(PollFd::new(winch.read.as_fd(), PollFlags::POLLIN));
            }
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno.into()),
            }
            let ready = |at: usize| {
                fds.get(at)
                    .and_then(PollFd::revents)
                    .is_some_and(|events| !events.is_empty())
            };
            let (socket_ready, stdin_ready) = (ready(0), reading_stdin && ready(1));
            let winch_ready = winch.is_some() && ready(if reading_stdin { 2 } else { 1 });
            drop(fds);
            if winch_ready && let Some(winch) = winch {
                winch.clear();
                if let Some((rows, cols)) = terminal_size() {
                    Frame::Resize { rows, cols }.encode(&mut self.out);
                }
            }
            if stdin_ready {
                match nix::unistd::read(stdin, &mut buffer) {
                    Ok(read) if read > 0 => {
                        Frame::Stdin(buffer.get(..read).unwrap_or_default().to_vec())
                            .encode(&mut self.out);
                    }
                    Err(Errno::EAGAIN | Errno::EINTR) => {}
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

/// SIGWINCH turned into a readable pipe (the self-pipe trick, TLPI
/// §63.5.2), for the loop's `poll`; the previous handling put back on drop.
struct Winch {
    read: OwnedFd,
    _write: OwnedFd,
    previous: nix::sys::signal::SigAction,
}

/// The pipe's write end, for the handler (-1: none).
static WINCH_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_winch(_: libc::c_int) {
    // The interrupted code may be about to read `errno`: put back as found.
    let saved = Errno::last_raw();
    let fd = WINCH_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: `write` is async-signal-safe; one byte from a static.
        unsafe { libc::write(fd, b"w".as_ptr().cast(), 1) };
    }
    Errno::set_raw(saved);
}

impl Winch {
    fn install() -> Result<Self, AttachError> {
        use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
        // (`pipe2` isn't on every UNIX: the flags after.)
        let (read, write) = nix::unistd::pipe()?;
        for fd in [&read, &write] {
            fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
            fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        }
        WINCH_PIPE.store(write.as_raw_fd(), Ordering::Relaxed);
        let action = SigAction::new(
            SigHandler::Handler(on_winch),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        // SAFETY: the handler only writes to a pipe (async-signal-safe).
        let previous = unsafe { sigaction(Signal::SIGWINCH, &action) }?;
        Ok(Self {
            read,
            _write: write,
            previous,
        })
    }

    /// Empties the pipe: one resize for any number of signals.
    fn clear(&self) {
        let mut sink = [0_u8; 64];
        while matches!(nix::unistd::read(&self.read, &mut sink), Ok(read) if read > 0) {}
    }
}

impl Drop for Winch {
    fn drop(&mut self) {
        // SAFETY: putting back the handling found when installed.
        let _ = unsafe {
            nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGWINCH, &self.previous)
        };
        WINCH_PIPE.store(-1, Ordering::Relaxed);
    }
}
