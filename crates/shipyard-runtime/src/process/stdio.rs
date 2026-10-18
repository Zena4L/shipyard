//! A command's standard descriptors, and the relay that serves the ones
//! Shipyard keeps for itself, and a pseudo-terminal's master (stage 8,
//! steps 8.2a and 8.2b).
//!
//! **Why a relay.** A container given the user's terminal as its stdin could
//! type into the user's shell (`TIOCSTI`, on kernels that still allow it) or
//! leave the terminal raw: whatever holds a terminal descriptor can change
//! the terminal. A pipe can't be used that way. So the caller can ask for a
//! [`Stream::Relay`]: the command gets a pipe, and Shipyard copies bytes
//! between the pipe and its own descriptor (TLPI ch. 44). (Which streams
//! are relayed is the caller's decision: the engine's, for `shipyard run`;
//! the runtime's own for `exec`, [`Stdio::sparing_terminals`].)
//!
//! **Never blocking the supervisor.** The copying happens in the
//! supervisor's own `poll` loop (TLPI §63.2), on non-blocking descriptors.
//! Not the terminal's own descriptors, though: fds 0–2 share their *open
//! file* with the shell that started Shipyard (TLPI §5.4), and `O_NONBLOCK`
//! belongs to the open file, so setting it there would leave the shell's
//! own reads failing with EAGAIN once Shipyard ends. The relay opens the
//! terminal again instead (`/proc/self/fd/N`), which makes an open file of
//! its own. The container's ends of the pipes stay blocking.
//!
//! **Job control.** A shell stops a background job that reads the terminal
//! (SIGTTIN), or writes to it with `stty tostop` (SIGTTOU), until `fg`
//! (TLPI §34.7). The relay checks first (`tcgetpgrp`) and leaves the
//! terminal alone from the background; the supervisor then stops Shipyard
//! itself, when something could resume it.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl, open};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;

use super::channel::{above_stdio, cloexec_pipe};
use crate::{Error, Result};

/// What one of a command's standard descriptors is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stream {
    /// Shipyard's own descriptor, passed on as it is.
    #[default]
    Inherit,
    /// `/dev/null`: reading gives end-of-file at once, writes are discarded.
    Null,
    /// A pipe, which Shipyard relays to or from its own descriptor: for a
    /// terminal the command mustn't hold.
    Relay,
}

/// A command's standard input, output and error. The default passes on
/// Shipyard's own, as `exec` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stdio {
    /// Descriptor 0.
    pub stdin: Stream,
    /// Descriptor 1.
    pub stdout: Stream,
    /// Descriptor 2.
    pub stderr: Stream,
    /// A pseudo-terminal of the command's own (`-t`), which becomes its
    /// controlling terminal and its 0, 1 and 2 when it starts (`stdin`,
    /// `stdout` and `stderr` are then what's in place before: an isolated
    /// command's setup, its init's messages).
    pub terminal: Option<Pty>,
}

/// A pseudo-terminal for the command (see [`Stdio::terminal`]): made inside
/// the container for an isolated one, from its own devpts, or on the host
/// without isolation; its master relayed to Shipyard's own descriptors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pty {
    /// Shipyard's stdin goes to it (`-i`).
    pub input: bool,
    /// Shipyard's own terminal goes into raw mode while it's relayed, so every
    /// key reaches the command's terminal as a byte (`-it` on a terminal).
    pub raw: bool,
    /// Its size (rows, columns) when none of Shipyard's stdout, stdin and
    /// stderr is a terminal to copy it from: a bundle's
    /// `process.consoleSize`. Without either, 24x80.
    pub size: Option<super::terminal::Size>,
}

impl Stdio {
    /// A joined command's (`exec` without a terminal of its own): Shipyard's
    /// own descriptors, as runc passes them, but never one that's a terminal
    /// (D16): a process left behind in the container would keep it after
    /// `exec` returns, and could read what's typed to the shell, or change
    /// the terminal's settings. Relayed instead while Shipyard supervises the
    /// command, `/dev/null` for one it doesn't (`detached`). Files and pipes
    /// (a shim's) are passed on as they are. A terminal stdin, when Shipyard
    /// isn't in the terminal's foreground (`exec … &`), is `/dev/null` too:
    /// relaying it would stop Shipyard (SIGTTIN), and the command's output
    /// with it, until `fg`.
    #[cfg(target_os = "linux")]
    pub(crate) fn sparing_terminals(detached: bool) -> Self {
        use std::io::IsTerminal as _;
        let instead = if detached {
            Stream::Null
        } else {
            Stream::Relay
        };
        let each = |terminal: bool| if terminal { instead } else { Stream::Inherit };
        let stdin = std::io::stdin().is_terminal();
        let background = stdin && !super::terminal::in_foreground(libc::STDIN_FILENO);
        Self {
            stdin: if background {
                Stream::Null
            } else {
                each(stdin)
            },
            stdout: each(std::io::stdout().is_terminal()),
            stderr: each(std::io::stderr().is_terminal()),
            terminal: None,
        }
    }
}

/// How many bytes one direction of the relay holds before it stops reading:
/// a full host terminal pauses the container (as a real terminal would), but
/// never the supervisor.
const BUFFER: usize = 64 * 1024;

/// How much is written at once to a blocking host descriptor (see
/// [`reopen`]): a terminal reports it can take more once fewer than 256
/// characters wait to be sent, so a write this small doesn't sleep.
const BLOCKING_CHUNK: usize = 256;

/// After the command has ended, how long the relay waits for the container's
/// pipes to close (everything in the container is gone by then, unless
/// there's no PID namespace and something escaped). What's already been
/// read is written out whatever the time.
pub(crate) const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// The child's ends: the descriptors to `dup2` onto its 0, 1 and 2, held
/// open until the spawn has passed them on, then closed (or the container
/// would never see end-of-file on its stdin).
#[derive(Debug, Default)]
pub(crate) struct ChildEnds {
    held: Vec<OwnedFd>,
    targets: [Option<RawFd>; 3],
    terminal: Option<ChildTerminal>,
}

/// The child's side of its pseudo-terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildTerminal {
    /// The slave, made here (no isolation): the command takes it.
    Slave(RawFd),
    /// A socket for the process that makes the pseudo-terminal inside the
    /// container (the init, or `exec`'s joiner) to send the master back on,
    /// `size` big, to shipyard's relay: which answers once it has it.
    Socket {
        fd: RawFd,
        size: super::terminal::Size,
    },
    /// The same, to a socket nothing answers on (runc's protocol): an OCI
    /// console socket's caller (`--console-socket`), or the monitor's own
    /// capture of a container's terminal.
    Console {
        fd: RawFd,
        size: super::terminal::Size,
    },
}

impl ChildTerminal {
    /// Where the master is sent, how big, and whether an answer comes:
    /// `None` for a slave made here.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "the init and the joiner are Linux's")
    )]
    pub(crate) fn socket(self) -> Option<(RawFd, super::terminal::Size, bool)> {
        match self {
            Self::Socket { fd, size } => Some((fd, size, true)),
            Self::Console { fd, size } => Some((fd, size, false)),
            Self::Slave(_) => None,
        }
    }
}

impl ChildEnds {
    /// The descriptor for each of 0, 1 and 2, where it isn't inherited.
    pub(crate) fn targets(&self) -> [Option<RawFd>; 3] {
        self.targets
    }

    /// The child's side of its pseudo-terminal, if it has one.
    pub(crate) fn terminal(&self) -> Option<ChildTerminal> {
        self.terminal
    }

    fn give(&mut self, target: usize, fd: OwnedFd) {
        if let Some(slot) = self.targets.get_mut(target) {
            *slot = Some(fd.as_raw_fd());
        }
        self.held.push(fd);
    }
}

/// Opens what `stdio` asks for: the child's ends, and the relay that serves
/// the other ends (none if nothing is relayed). `isolated`: a pseudo-terminal
/// is made inside the container, and its master comes back over a socket.
pub(crate) fn prepare(stdio: Stdio, isolated: bool) -> Result<(ChildEnds, Option<Relay>)> {
    let mut ends = ChildEnds::default();
    let mut relay = Relay::default();
    if let Some(request) = stdio.terminal {
        // The user's terminal's size, else the one asked for (a bundle's).
        let size = super::terminal::terminal_size()
            .or(request.size)
            .unwrap_or(super::terminal::DEFAULT_SIZE);
        let state = if isolated {
            let (ours, theirs) = super::terminal::socket_pair()
                .map_err(|e| Error::sys("making a socket for the container's terminal", e))?;
            let theirs = above_stdio(theirs)?;
            ends.terminal = Some(ChildTerminal::Socket {
                fd: theirs.as_raw_fd(),
                size,
            });
            ends.held.push(theirs);
            PtyState::Awaited(ours)
        } else {
            let (master, slave) = super::terminal::host_pty(size)
                .map_err(|e| Error::sys("making a pseudo-terminal for the command", e))?;
            let slave = above_stdio(slave)?;
            ends.terminal = Some(ChildTerminal::Slave(slave.as_raw_fd()));
            ends.held.push(slave);
            PtyState::Arrived {
                master,
                socket: None,
            }
        };
        relay.pty = Some(PtyRelay {
            request,
            state,
            raw: None,
        });
    }
    match stdio.stdin {
        Stream::Inherit => {}
        Stream::Null => ends.give(0, null(OFlag::O_RDONLY)?),
        Stream::Relay => {
            let (read, write) = pipe()?;
            ends.give(0, read);
            let (host, blocking) = reopen(0, OFlag::O_RDONLY)?;
            relay.input = Some(Direction::new(
                host,
                nonblocking(write)?,
                Host::From,
                blocking,
            ));
        }
    }
    // One pipe for both when they're the same terminal: two would let a
    // poll read them out of order (`echo a; echo b >&2`).
    let shared = stdio.stdout == Stream::Relay && stdio.stderr == Stream::Relay && same_file(1, 2);
    // (the slot in the child's targets, the host's descriptor)
    for ((slot, fd), stream) in [((1, 1), stdio.stdout), ((2, 2), stdio.stderr)] {
        match stream {
            Stream::Inherit => {}
            Stream::Null => ends.give(slot, null(OFlag::O_WRONLY)?),
            Stream::Relay if shared && slot == 2 => {
                ends.targets[2] = ends.targets[1];
            }
            Stream::Relay => {
                let (read, write) = pipe()?;
                ends.give(slot, write);
                let (host, blocking) = reopen(fd, OFlag::O_WRONLY)?;
                relay
                    .outputs
                    .push(Direction::new(nonblocking(read)?, host, Host::To, blocking));
            }
        }
    }
    let relay = (relay.input.is_some() || !relay.outputs.is_empty() || relay.pty.is_some())
        .then_some(relay);
    Ok((ends, relay))
}

/// `/dev/null`, opened above the standard descriptors (so it's never `dup2`ed
/// onto itself, which would leave it close-on-exec).
fn null(access: OFlag) -> Result<OwnedFd> {
    let fd = open("/dev/null", access | OFlag::O_CLOEXEC, Mode::empty())
        .map_err(|e| Error::sys("opening /dev/null for the command", e))?;
    above_stdio(fd)
}

/// A close-on-exec pipe, both ends above the standard descriptors.
fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let (read, write) = cloexec_pipe()?;
    Ok((above_stdio(read)?, above_stdio(write)?))
}

/// `fd`, made non-blocking. Only for Shipyard's own ends: the open file is
/// theirs alone.
fn nonblocking(fd: OwnedFd) -> Result<OwnedFd> {
    let flags =
        fcntl(&fd, FcntlArg::F_GETFL).map_err(|e| Error::sys("reading a relay pipe's flags", e))?;
    fcntl(
        &fd,
        FcntlArg::F_SETFL(OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK),
    )
    .map_err(|e| Error::sys("making a relay pipe non-blocking", e))?;
    Ok(fd)
}

/// Shipyard's descriptor `fd` for the relay; and whether it's the shared
/// open file, blocking. A terminal is opened again as a file of its own,
/// non-blocking (see the module's documentation). Anything else (a file or
/// a pipe `-t`'s output goes to) is shared: a file opened again would start
/// at offset 0 without `O_APPEND`, writing over what `>>` meant to keep.
fn reopen(fd: RawFd, access: OFlag) -> Result<(OwnedFd, bool)> {
    // SAFETY: borrowing `fd` (0, 1 or 2) for the calls below.
    let shared = unsafe { BorrowedFd::borrow_raw(fd) };
    if !std::io::IsTerminal::is_terminal(&shared) {
        return Ok((shared_duplicate(fd)?, true));
    }
    reopen_terminal(fd, access)
}

/// The terminal on `fd` opened again, non-blocking.
///
/// Opening it again checks the file's permissions, as any open does: a
/// terminal that isn't the caller's (Shipyard run as another user, on
/// root's terminal: `sudo -u`) can't be. Then the relay uses a duplicate of
/// the descriptor itself, its shared open file left blocking, and reads or
/// writes it only when `poll` says that won't block (see [`Direction`]).
#[cfg(target_os = "linux")]
fn reopen_terminal(fd: RawFd, access: OFlag) -> Result<(OwnedFd, bool)> {
    match open(
        format!("/proc/self/fd/{fd}").as_str(),
        access | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(reopened) => Ok((reopened, false)),
        Err(errno @ (Errno::EACCES | Errno::EPERM | Errno::ENXIO)) => {
            tracing::debug!(
                fd,
                %errno,
                "can't open the descriptor again; relaying through it, blocking"
            );
            Ok((shared_duplicate(fd)?, true))
        }
        Err(errno) => Err(Error::sys(
            format!("opening descriptor {fd} again to relay it"),
            errno,
        )),
    }
}

/// Elsewhere (macOS), a terminal is opened again by its name (`/dev/fd/N`
/// would share the open file); failing that, the blocking duplicate.
#[cfg(not(target_os = "linux"))]
fn reopen_terminal(fd: RawFd, access: OFlag) -> Result<(OwnedFd, bool)> {
    // SAFETY: borrowing `fd` (0, 1 or 2) for the calls below.
    let shared = unsafe { BorrowedFd::borrow_raw(fd) };
    if let Ok(name) = nix::unistd::ttyname(shared)
        && let Ok(reopened) = open(
            &name,
            access | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    {
        return Ok((reopened, false));
    }
    Ok((shared_duplicate(fd)?, true))
}

/// A duplicate of Shipyard's descriptor `fd` (0, 1 or 2), sharing its open
/// file: the blocking way to relay it.
fn shared_duplicate(fd: RawFd) -> Result<OwnedFd> {
    // SAFETY: borrowing `fd` (open: the caller relays it) for the duplicate.
    let shared = unsafe { BorrowedFd::borrow_raw(fd) };
    duplicate(shared).map_err(|e| Error::sys(format!("duplicating descriptor {fd} to relay it"), e))
}

/// A close-on-exec duplicate of `fd`, above the standard descriptors.
fn duplicate(fd: BorrowedFd<'_>) -> nix::Result<OwnedFd> {
    let duplicate = fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(3))?;
    // SAFETY: `F_DUPFD_CLOEXEC` just returned this descriptor, and nothing
    // else owns it.
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(duplicate) })
}

/// Whether descriptors `a` and `b` are the same file (`2>&1`, or both the
/// shell's terminal): the same inode on the same file system. (Not the same
/// device number alone: two devpts instances each have a `/dev/pts/0`.)
fn same_file(a: RawFd, b: RawFd) -> bool {
    // SAFETY: borrowing descriptors 1 and 2 for the duration of `fstat`;
    // they're open (a closed one makes `fstat` fail, which reads as "not the
    // same").
    let stat = |fd| nix::sys::stat::fstat(unsafe { BorrowedFd::borrow_raw(fd) }).ok();
    match (stat(a), stat(b)) {
        (Some(a), Some(b)) => (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino),
        _ => false,
    }
}

/// Which end of a direction is Shipyard's own (the host's) descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    /// It reads from the host (stdin).
    From,
    /// It writes to the host (stdout, stderr).
    To,
}

/// What the host's end of a direction is.
#[derive(Debug, Clone, Copy)]
struct HostEnd {
    /// A terminal, where job control applies.
    terminal: bool,
    /// The shared, blocking descriptor (see [`reopen`]): it's read or
    /// written only once `poll` says it's ready, and written a little at a
    /// time.
    blocking: bool,
}

/// One direction of the relay: bytes read from `from`, held, written to `to`.
#[derive(Debug)]
struct Direction {
    /// `None` once it reached end-of-file, or failed for good.
    from: Option<OwnedFd>,
    /// `None` once closed: the container's stdin, after the host's
    /// end-of-file.
    to: Option<OwnedFd>,
    host: Host,
    /// What the host's end is.
    host_end: HostEnd,
    /// Read, not yet written.
    pending: Vec<u8>,
    /// Writes to `to` failed for good (a hung-up terminal, a closed
    /// reader): what's read is thrown away from now on, so the container
    /// neither blocks on a full pipe nor gets a SIGPIPE it didn't cause.
    discarding: bool,
    /// The container's end is the pseudo-terminal's master (a duplicate of
    /// it): dropped when the terminal hangs up.
    on_pty: bool,
}

impl Direction {
    fn new(from: OwnedFd, to: OwnedFd, host: Host, host_blocking: bool) -> Self {
        let terminal = match host {
            Host::From => is_terminal(&from),
            Host::To => is_terminal(&to),
        };
        Self {
            from: Some(from),
            to: Some(to),
            host,
            host_end: HostEnd {
                terminal,
                blocking: host_blocking,
            },
            on_pty: false,
            pending: Vec::new(),
            discarding: false,
        }
    }

    fn host_fd(&self) -> Option<BorrowedFd<'_>> {
        match self.host {
            Host::From => self.from.as_ref(),
            Host::To => self.to.as_ref(),
        }
        .map(AsFd::as_fd)
    }

    /// The job-control signal the host's terminal would stop Shipyard with,
    /// were it to read or write it now: SIGTTIN for reading from the
    /// background, SIGTTOU for writing with `tostop` set. `None` where the
    /// kernel wouldn't stop it: in the foreground, or with that signal
    /// ignored or blocked (then a write just goes through, and a read fails
    /// with EIO, which ends the direction).
    fn held_back(&self) -> Option<libc::c_int> {
        if !self.host_end.terminal {
            return None;
        }
        let fd = self.host_fd()?;
        if super::terminal::in_foreground(fd.as_raw_fd()) {
            return None;
        }
        let signal = match self.host {
            Host::From => libc::SIGTTIN,
            Host::To if tostop(fd) => libc::SIGTTOU,
            Host::To => return None,
        };
        super::terminal::stops(signal).then_some(signal)
    }

    /// Whether this direction has something it would do with the host's
    /// terminal now, but for job control: read from it (room in the buffer,
    /// somewhere to put it), or write to it (something held).
    fn wants_host(&self) -> bool {
        match self.host {
            Host::From => self.from.is_some() && self.to.is_some() && self.pending.len() < BUFFER,
            Host::To => !self.pending.is_empty() && !self.discarding,
        }
    }

    /// Whether the host's end is `side` and job control holds it back.
    fn held(&self, side: Host) -> bool {
        self.host == side && self.held_back().is_some()
    }

    fn wants_read(&self) -> bool {
        self.from.is_some()
            && (self.discarding || self.pending.len() < BUFFER)
            && !self.held(Host::From)
    }

    fn wants_write(&self) -> bool {
        self.to.is_some() && !self.pending.is_empty() && !self.discarding && !self.held(Host::To)
    }

    /// Moves what it can without blocking, at most [`BUFFER`] bytes read.
    fn pump(&mut self) {
        let mut moved = 0;
        loop {
            self.flush();
            if moved >= BUFFER || !self.wants_read() {
                break;
            }
            match self.read_some() {
                Some(read) => moved += read,
                None => break,
            }
        }
        self.flush();
        // The host's input ended and everything was passed on: the
        // container's stdin reads end-of-file.
        if self.host == Host::From && self.from.is_none() && self.pending.is_empty() {
            self.to = None;
        }
    }

    /// Reads once into `pending`. `None` if nothing came (or nothing will).
    fn read_some(&mut self) -> Option<usize> {
        let fd = self.from.as_ref()?;
        if self.host == Host::From
            && self.host_end.blocking
            && !ready(fd.as_fd(), PollFlags::POLLIN)
        {
            return None;
        }
        let mut chunk = [0_u8; 16 * 1024];
        let room = if self.discarding {
            chunk.len()
        } else {
            BUFFER.saturating_sub(self.pending.len()).min(chunk.len())
        };
        let buf = chunk.get_mut(..room)?;
        loop {
            match nix::unistd::read(fd, buf) {
                Ok(0) => {
                    self.from = None;
                    return None;
                }
                Ok(read) => {
                    if !self.discarding {
                        self.pending.extend_from_slice(buf.get(..read)?);
                    }
                    return Some(read);
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => return None,
                // EIO: a terminal hung up, or a read from the background of
                // an orphaned group (nothing would resume a stop). Anything
                // else is as final.
                Err(errno) => {
                    tracing::debug!(%errno, "a relayed descriptor stopped reading");
                    self.from = None;
                    return None;
                }
            }
        }
    }

    /// Writes what's held, until it would block.
    fn flush(&mut self) {
        while self.wants_write() {
            let Some(to) = self.to.as_ref() else {
                return;
            };
            let mut chunk = self.pending.as_slice();
            if self.host == Host::To && self.host_end.blocking {
                if !ready(to.as_fd(), PollFlags::POLLOUT) {
                    return;
                }
                // A blocking write returns once all of it is written: a
                // little at a time, the supervisor stays responsive.
                chunk = chunk
                    .get(..chunk.len().min(BLOCKING_CHUNK))
                    .unwrap_or(chunk);
            }
            match nix::unistd::write(to, chunk) {
                Ok(written) => {
                    self.pending.drain(..written.min(self.pending.len()));
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => return,
                Err(errno) => {
                    tracing::debug!(%errno, "a relayed descriptor stopped taking writes");
                    self.pending.clear();
                    match self.host {
                        // The host's terminal or pipe is gone: keep the
                        // container's output flowing, into nothing.
                        Host::To => self.discarding = true,
                        // The container closed its stdin: stop reading the
                        // user's input, which isn't for it any more.
                        Host::From => {
                            self.from = None;
                            self.to = None;
                        }
                    }
                    return;
                }
            }
        }
    }

    /// What `poll` should wait for on this direction.
    fn poll_fds<'a>(&'a self, fds: &mut Vec<PollFd<'a>>) {
        if self.wants_read()
            && let Some(from) = &self.from
        {
            fds.push(PollFd::new(from.as_fd(), PollFlags::POLLIN));
        }
        if self.wants_write()
            && let Some(to) = &self.to
        {
            fds.push(PollFd::new(to.as_fd(), PollFlags::POLLOUT));
        }
    }

    /// Whether everything that will ever come has been passed on.
    fn done(&self) -> bool {
        self.from.is_none() && (self.pending.is_empty() || self.discarding || self.to.is_none())
    }
}

/// The relay between a command's pipes and Shipyard's own descriptors.
#[derive(Debug, Default)]
pub(crate) struct Relay {
    /// Shipyard's stdin into the command's.
    input: Option<Direction>,
    /// The command's stdout (and stderr, separately or together) out to
    /// Shipyard's.
    outputs: Vec<Direction>,
    /// The command's pseudo-terminal, if it has one.
    pty: Option<PtyRelay>,
}

/// The relay's side of the command's pseudo-terminal.
#[derive(Debug)]
struct PtyRelay {
    request: Pty,
    state: PtyState,
    /// Shipyard's own terminal in raw mode, while it is.
    raw: Option<super::terminal::RawMode>,
}

impl PtyRelay {
    /// Whether the relay waits for Shipyard to be in the user's terminal's
    /// foreground, to set it raw: the master is here, Shipyard is in the
    /// background, and SIGTTOU would stop it until `fg`. (If it wouldn't,
    /// the relay goes on without raw mode or input.)
    fn waits_for_foreground(&self) -> bool {
        self.request.raw
            && self.raw.is_none()
            && matches!(self.state, PtyState::Arrived { .. })
            && !super::terminal::in_foreground(libc::STDIN_FILENO)
            && super::terminal::stops(libc::SIGTTOU)
    }
}

/// Where the pseudo-terminal's master is.
#[derive(Debug)]
enum PtyState {
    /// The container's init will send it over this socket.
    Awaited(OwnedFd),
    /// Here; waiting for the user's terminal (raw mode is set only from its
    /// foreground) before the relay starts. The init waits on `socket` for
    /// a byte saying so.
    Arrived {
        master: OwnedFd,
        socket: Option<OwnedFd>,
    },
    /// Relayed (the directions hold duplicates); kept to resize.
    Connected(OwnedFd),
    /// Hung up, or never sent (the init failed: its report says why).
    Gone,
}

impl Relay {
    fn directions(&self) -> impl Iterator<Item = &Direction> {
        self.input.iter().chain(&self.outputs)
    }

    fn directions_mut(&mut self) -> impl Iterator<Item = &mut Direction> {
        self.input.iter_mut().chain(&mut self.outputs)
    }

    /// The descriptors to wait on, with what to wait for.
    pub(crate) fn poll_fds(&self) -> Vec<PollFd<'_>> {
        let mut fds = Vec::new();
        if let Some(PtyRelay {
            state: PtyState::Awaited(socket),
            ..
        }) = &self.pty
        {
            fds.push(PollFd::new(socket.as_fd(), PollFlags::POLLIN));
        }
        for direction in self.directions() {
            direction.poll_fds(&mut fds);
        }
        fds
    }

    /// Moves what can be moved without blocking.
    pub(crate) fn pump(&mut self) {
        self.advance_pty();
        for direction in self.directions_mut() {
            direction.pump();
        }
    }

    /// Takes the pseudo-terminal's master once the init has sent it, sets
    /// the user's terminal raw if asked (from its foreground only), tells the
    /// init, and starts relaying it.
    fn advance_pty(&mut self) {
        let Some(pty) = &mut self.pty else {
            return;
        };
        if let PtyState::Awaited(socket) = &pty.state {
            match super::terminal::receive_fd(socket.as_fd()) {
                Ok(None) => return,
                Ok(Some(master)) => {
                    let PtyState::Awaited(socket) =
                        std::mem::replace(&mut pty.state, PtyState::Gone)
                    else {
                        return;
                    };
                    pty.state = PtyState::Arrived {
                        master,
                        socket: Some(socket),
                    };
                }
                Err(errno) => {
                    tracing::debug!(%errno, "no terminal came from the container's init");
                    pty.state = PtyState::Gone;
                    return;
                }
            }
        }
        if !matches!(pty.state, PtyState::Arrived { .. }) {
            return;
        }
        if pty.request.raw && pty.raw.is_none() {
            match super::terminal::RawMode::set(libc::STDIN_FILENO) {
                Ok(Some(raw)) => pty.raw = Some(raw),
                // In the background: the supervisor stops Shipyard until
                // `fg` (see `stop_needed`), and this is tried again then;
                // unless SIGTTOU wouldn't stop it (ignored or blocked): then
                // without raw mode or input.
                Ok(None) if !super::terminal::stops(libc::SIGTTOU) => pty.request = Pty::default(),
                Ok(None) => return,
                Err(errno) => {
                    tracing::debug!(%errno, "the terminal can't go raw; relaying it as it is");
                    pty.request.raw = false;
                }
            }
        }
        let PtyState::Arrived { master, socket } =
            std::mem::replace(&mut pty.state, PtyState::Gone)
        else {
            return;
        };
        // The user's terminal may have changed size meanwhile. (Without one,
        // the size it was made with stands: a bundle's, or 24x80.)
        if let Some(size) = super::terminal::terminal_size() {
            let _ = super::terminal::set_window_size(master.as_raw_fd(), size);
        }
        match self::connect(&master, pty.request) {
            Ok((output, input)) => {
                if let Some(socket) = socket {
                    // The init starts the command on this byte: only once
                    // its terminal is relayed.
                    let _ = nix::unistd::write(&socket, b"r");
                }
                pty.state = PtyState::Connected(master);
                self.outputs.push(output);
                if let Some(input) = input {
                    self.input = Some(input);
                }
            }
            Err(err) => {
                // The socket closes unanswered: the init fails before
                // starting the command, and says so (`TerminalSend`).
                tracing::warn!("can't relay the command's terminal: {err}");
                pty.raw = None;
            }
        }
    }

    /// Whether the command has a pseudo-terminal being relayed.
    pub(crate) fn has_terminal(&self) -> bool {
        matches!(
            self.pty,
            Some(PtyRelay {
                state: PtyState::Connected(_),
                ..
            })
        )
    }

    /// The user's terminal changed size: so does the command's (its
    /// foreground job gets SIGWINCH from the kernel).
    pub(crate) fn resize(&self) {
        if let Some(PtyRelay {
            state: PtyState::Connected(master),
            ..
        }) = &self.pty
            && let Some(size) = super::terminal::terminal_size()
        {
            let _ = super::terminal::set_window_size(master.as_raw_fd(), size);
        }
    }

    /// The user's terminal hung up: so does the command's, by closing its
    /// master (the kernel then sends its session SIGHUP, probe T9).
    pub(crate) fn hang_up(&mut self) {
        let Some(pty) = &mut self.pty else {
            return;
        };
        pty.state = PtyState::Gone;
        pty.raw = None;
        self.outputs.retain(|output| !output.on_pty);
        if self.input.as_ref().is_some_and(|input| input.on_pty) {
            self.input = None;
        }
    }

    /// Stops Shipyard with `signal` for job control: the user's terminal is
    /// put back first, and raw again once resumed.
    pub(crate) fn stop_self(&self, signal: libc::c_int) {
        match self.pty.as_ref().and_then(|pty| pty.raw.as_ref()) {
            Some(raw) => raw.stop_with(signal),
            None => super::terminal::stop_self_with(signal),
        }
    }

    /// The user's terminal goes back to how it was (out of raw mode).
    pub(crate) fn release_terminal(&mut self) {
        if let Some(pty) = &mut self.pty {
            pty.raw = None;
        }
    }

    /// The job-control stop the host's terminal asks of Shipyard before the
    /// relay can go on: SIGTTIN to read it from the background, SIGTTOU to
    /// write to it there with `tostop` set. Only when the relay has
    /// something to do there.
    pub(crate) fn stop_needed(&self) -> Option<libc::c_int> {
        if self
            .pty
            .as_ref()
            .is_some_and(PtyRelay::waits_for_foreground)
        {
            return Some(libc::SIGTTOU);
        }
        self.directions()
            .filter(|direction| direction.wants_host())
            .find_map(Direction::held_back)
    }

    /// Gives up on whatever waits for the terminal from the background, when
    /// Shipyard can't stop (nothing would resume it, or it's stopping the
    /// command): input ends (the command's stdin reads end-of-file), output
    /// is discarded.
    pub(crate) fn give_up_held_back(&mut self) {
        if let Some(pty) = &mut self.pty
            && matches!(pty.state, PtyState::Arrived { .. })
            && pty.raw.is_none()
        {
            // Not the user's terminal to change, nor to read: the command's
            // output still comes.
            pty.request = Pty::default();
        }
        for direction in self.directions_mut() {
            if direction.held_back().is_some() {
                match direction.host {
                    Host::From => {
                        direction.from = None;
                        direction.pending.clear();
                    }
                    Host::To => {
                        direction.pending.clear();
                        direction.discarding = true;
                    }
                }
            }
        }
    }

    /// The command has ended: the user's input is no longer read (it's for
    /// whatever runs next on the terminal).
    pub(crate) fn end_input(&mut self) {
        self.input = None;
        // A terminal not yet relayed never will be: its command never
        // started (the init waits for the relay before starting it), and
        // nothing should wait for the user's terminal, nor stop Shipyard, for
        // it now.
        if let Some(pty) = &mut self.pty
            && matches!(pty.state, PtyState::Awaited(_) | PtyState::Arrived { .. })
        {
            pty.state = PtyState::Gone;
            pty.raw = None;
        }
    }

    /// Whether all the command's output has been passed on (or will never
    /// be: its pipes are closed, or the host's end is gone).
    pub(crate) fn outputs_done(&self) -> bool {
        self.outputs.iter().all(Direction::done)
    }

    /// No more waiting for the command's pipes to close: what was read still
    /// goes out.
    pub(crate) fn stop_waiting(&mut self) {
        for output in &mut self.outputs {
            output.from = None;
        }
    }

    /// Throws away whatever hasn't been passed on: Shipyard is told to stop
    /// (a signal) while the host's terminal takes nothing.
    pub(crate) fn abandon(&mut self) {
        self.input = None;
        for output in &mut self.outputs {
            output.from = None;
            output.pending.clear();
            output.discarding = true;
        }
    }
}

/// The directions relaying the pseudo-terminal's `master`: its output to
/// Shipyard's stdout, and with `request.input`, Shipyard's stdin into it.
/// Each holds a duplicate of the master, non-blocking (only Shipyard holds
/// it).
fn connect(master: &OwnedFd, request: Pty) -> Result<(Direction, Option<Direction>)> {
    let duplicate = || -> Result<OwnedFd> {
        nonblocking(
            duplicate(master.as_fd())
                .map_err(|e| Error::sys("duplicating the terminal's master", e))?,
        )
    };
    let (host_out, blocking) = reopen(libc::STDOUT_FILENO, OFlag::O_WRONLY)?;
    let mut output = Direction::new(duplicate()?, host_out, Host::To, blocking);
    output.on_pty = true;
    let input = if request.input {
        let (host_in, blocking) = reopen(libc::STDIN_FILENO, OFlag::O_RDONLY)?;
        let mut input = Direction::new(host_in, duplicate()?, Host::From, blocking);
        input.on_pty = true;
        Some(input)
    } else {
        None
    };
    Ok((output, input))
}

/// Whether `fd` is ready for `events` right now (or has hung up or failed,
/// which the next call reports at once).
fn ready(fd: BorrowedFd<'_>, events: PollFlags) -> bool {
    let mut fds = [PollFd::new(fd, events)];
    matches!(poll(&mut fds, PollTimeout::ZERO), Ok(n) if n > 0)
}

fn is_terminal(fd: &OwnedFd) -> bool {
    // SAFETY: `isatty` only asks about the descriptor, open for the call.
    unsafe { libc::isatty(fd.as_raw_fd()) == 1 }
}

/// Whether the terminal stops background writers (`stty tostop`).
fn tostop(fd: BorrowedFd<'_>) -> bool {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` fills in the `termios` it's given; it's read only
    // if the call succeeded.
    if unsafe { libc::tcgetattr(fd.as_raw_fd(), termios.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: `tcgetattr` succeeded, so it filled the structure in.
    let termios = unsafe { termios.assume_init() };
    termios.c_lflag & libc::TOSTOP != 0
}
