//! The monitor's capture of a container's output into its logs (stage 8,
//! step 8.3b, with `create --log-dir`): conmon's job, in Shipyard's
//! single-threaded monitor.
//!
//! The container's stdout and stderr are pipes the monitor reads (or, for a
//! container with a terminal, the terminal's master, which its init sends
//! here as it would to a console socket). Each read goes to the
//! [`LogWriter`] as soon as it's taken: the log file's write is the only
//! thing that can hold the container's output back (TLPI ch. 44: a pipe
//! nobody empties fills, and then its writer blocks).
//!
//! Its stdin is `/dev/null`, unless it takes input (`create --stdin`): then
//! a pipe the monitor writes what attached clients type into (or, with a
//! terminal, the terminal's master). The clients are served by the same
//! loop ([`Attachments`]), from the container's setup to after its end.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use super::attach::{Attachments, Stdin};
use crate::container::StdinMode;
use crate::logs::cri::Stream;
use crate::logs::info::LogSettings;
use crate::logs::writer::LogWriter;
use crate::process::terminal;

/// How long part of a line waits before it's written as a partial record,
/// so a prompt (which doesn't end its line) shows in `logs --follow`.
const IDLE: Duration = Duration::from_secs(1);

/// How much is read at once.
const READ: usize = 64 * 1024;

/// One of the container's outputs, being read.
#[derive(Debug)]
struct Source {
    fd: OwnedFd,
    stream: Stream,
}

/// The container's outputs and their logs.
#[derive(Debug)]
pub(super) struct Capture {
    writer: LogWriter,
    sources: Vec<Source>,
    /// Where the terminal's master comes from, until it has.
    socket: Option<OwnedFd>,
    buffer: Vec<u8>,
    /// Since when each stream has had part of a line not written yet.
    pending_since: [Option<Instant>; 2],
    /// `attach.sock` and its clients.
    attach: Attachments,
}

/// The container's side of the capture, for the spawn: what its 0, 1 and 2
/// are, and the socket its init sends its terminal on. Kept open until the
/// spawn has passed them on, then dropped (or the pipes would never end).
#[derive(Debug)]
pub(super) struct ContainerSide {
    pub(super) stdio: [OwnedFd; 3],
    pub(super) terminal_socket: Option<OwnedFd>,
}

impl ContainerSide {
    /// The descriptors for the spawn's 0, 1 and 2.
    pub(super) fn targets(&self) -> [Option<std::os::fd::RawFd>; 3] {
        self.stdio.each_ref().map(|fd| Some(fd.as_raw_fd()))
    }
}

impl Capture {
    /// Starts the logs in the directory open at `dir` (written before the
    /// container exists: `log.json` first), makes the container's outputs
    /// (pipes, or with `terminal` a socket for its terminal's master) and
    /// stdin (`/dev/null`, or a pipe), and listens on `attach.sock` in the
    /// state directory open at `state`.
    pub(super) fn new(
        dir: OwnedFd,
        settings: LogSettings,
        terminal: bool,
        stdin: StdinMode,
        state: &OwnedFd,
    ) -> Result<(Self, ContainerSide), String> {
        let writer = LogWriter::create(dir, settings)
            .map_err(|err| format!("starting the container's logs: {err}"))?;
        let null = || {
            nix::fcntl::open(
                "/dev/null",
                OFlag::O_RDWR | OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(|errno| format!("opening /dev/null: {}", errno.desc()))
        };
        let mut sources = Vec::new();
        let (stdio, socket, terminal_socket) = if terminal {
            // Its terminal is all of its 0, 1 and 2 (its init's too, at the
            // gate: 8.3a's `command_terminal`).
            let (ours, theirs) = terminal::socket_pair()
                .map_err(|errno| format!("making a socket for the terminal: {}", errno.desc()))?;
            ([null()?, null()?, null()?], Some(ours), Some(theirs))
        } else {
            let mut pipe = |stream| -> Result<OwnedFd, String> {
                let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
                    .map_err(|errno| format!("making a pipe for the output: {}", errno.desc()))?;
                nonblocking(read.as_fd()).map_err(|errno| errno.desc().to_owned())?;
                sources.push(Source { fd: read, stream });
                Ok(write)
            };
            let stdout = pipe(Stream::Stdout)?;
            let stderr = pipe(Stream::Stderr)?;
            ([null()?, stdout, stderr], None, None)
        };
        let mut stdio = stdio;
        // Its stdin: a pipe whose write end the monitor keeps (not with a
        // terminal: its master is its stdin, written to once it has come).
        let mut kept = None;
        if !terminal && stdin != StdinMode::Closed {
            let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
                .map_err(|errno| format!("making a pipe for the input: {}", errno.desc()))?;
            nonblocking(write.as_fd()).map_err(|errno| errno.desc().to_owned())?;
            stdio[0] = read;
            kept = Some(Stdin::new(write, stdin == StdinMode::Once));
        }
        let attach = Attachments::listen(state, terminal, stdin != StdinMode::Closed, kept)
            .map_err(|err| format!("listening on the attach socket: {err}"))?;
        Ok((
            Self {
                writer,
                sources,
                socket,
                buffer: vec![0; READ],
                pending_since: [None, None],
                attach,
            },
            ContainerSide {
                stdio,
                terminal_socket,
            },
        ))
    }

    /// What to wait on: the outputs still open, the terminal's socket, and
    /// the attach socket and its clients.
    pub(super) fn poll_fds(&self) -> Vec<PollFd<'_>> {
        let mut fds: Vec<PollFd<'_>> = self
            .sources
            .iter()
            .map(|source| source.fd.as_fd())
            .chain(self.socket.as_ref().map(AsFd::as_fd))
            .map(|fd| PollFd::new(fd, PollFlags::POLLIN))
            .collect();
        fds.extend(self.attach.poll_fds());
        fds
    }

    /// The longest to wait before [`service`](Self::service) has something
    /// to do on its own (a pending line going stale).
    pub(super) fn timeout(&self) -> Option<Duration> {
        self.pending_since
            .iter()
            .flatten()
            .map(|since| IDLE.saturating_sub(since.elapsed()))
            .chain(self.attach.timeout())
            .min()
    }

    /// Takes what's there to read (without waiting), the terminal's master
    /// if it has come, and writes stale partial lines.
    pub(super) fn service(&mut self) {
        if let Some(socket) = &self.socket {
            match terminal::receive_fd(socket.as_fd()) {
                Ok(Some(master)) => {
                    if nonblocking(master.as_fd()).is_ok() {
                        self.attach.set_master(&master);
                        self.sources.push(Source {
                            fd: master,
                            stream: Stream::Stdout,
                        });
                    }
                    self.socket = None;
                }
                Ok(None) => {}
                // The init died before sending it: nothing will come.
                Err(_) => self.socket = None,
            }
        }
        let mut ended = Vec::new();
        for (at, source) in self.sources.iter().enumerate() {
            // A few reads each, so that one busy output can't keep the
            // others (and the init's exit) waiting.
            for _ in 0..4 {
                match nix::unistd::read(&source.fd, &mut self.buffer) {
                    Ok(0) => {
                        ended.push(at);
                        break;
                    }
                    Ok(read) => {
                        let bytes = self.buffer.get(..read).unwrap_or_default();
                        self.writer.push(source.stream, bytes);
                        self.attach.output(source.stream, bytes);
                    }
                    Err(Errno::EAGAIN | Errno::EINTR) => break,
                    // EIO: a terminal whose every slave is closed.
                    Err(_) => {
                        ended.push(at);
                        break;
                    }
                }
            }
        }
        for at in ended.into_iter().rev() {
            let source = self.sources.remove(at);
            if !self
                .sources
                .iter()
                .any(|other| other.stream == source.stream)
            {
                self.writer.finish(source.stream);
            }
        }
        self.track_pending();
        self.attach.service();
    }

    /// Writes the partial lines that have waited [`IDLE`].
    fn track_pending(&mut self) {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let pending = self.writer.has_pending(stream);
            let since = &mut self.pending_since[stream.index()];
            match (pending, *since) {
                (true, None) => *since = Some(Instant::now()),
                (true, Some(at)) if at.elapsed() >= IDLE => {
                    *since = None;
                    self.writer.flush(stream);
                }
                (false, _) => *since = None,
                _ => {}
            }
        }
    }

    /// After the container's end: reads what's left until every output
    /// ends, or `deadline` (something escaped, without a PID namespace, may
    /// hold one open), then writes any line still pending.
    pub(super) fn drain(&mut self, deadline: Instant) {
        // The master can't come any more.
        self.socket = None;
        while !self.sources.is_empty() {
            self.service();
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.sources.is_empty() {
                break;
            }
            let mut fds = self.poll_fds();
            let timeout = PollTimeout::try_from(left.min(Duration::from_millis(100)))
                .unwrap_or(PollTimeout::ZERO);
            let _ = poll(&mut fds, timeout);
        }
        for stream in [Stream::Stdout, Stream::Stderr] {
            self.writer.finish(stream);
        }
        // The clients get what's left before they're told the end.
        self.attach.service();
        if self.writer.dropped() > 0 {
            tracing::debug!(
                dropped = self.writer.dropped(),
                "log records lost to write errors"
            );
        }
    }
}

impl Capture {
    /// The container has ended with `code`, its exit recorded: the clients
    /// are told, and let go.
    pub(super) fn end(&mut self, code: i32) {
        self.attach.end(code);
    }
}

/// Makes reads on `fd` return EAGAIN instead of waiting. (The container's
/// ends of the pipes stay blocking: they're other open files.)
fn nonblocking(fd: BorrowedFd<'_>) -> nix::Result<()> {
    let flags = OFlag::from_bits_truncate(nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL)?);
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(drop)
}
