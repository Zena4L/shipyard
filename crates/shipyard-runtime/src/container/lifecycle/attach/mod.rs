//! The monitor's side of attach (stage 8, step 8.3c): `attach.sock` in the
//! state directory, its clients, and the container's stdin.
//!
//! **Who may attach:** the socket file is 0600 in a 0700 directory, and each
//! connection's peer credentials (`SO_PEERCRED`, TLPI §61.13: the kernel
//! says who connected, the client can't lie) must be this monitor's user or
//! root: anyone else is closed at once.
//!
//! **Nothing an ordinary client does slows the container.** Output goes to
//! each client through a queue of frames, at most [`CLIENT_BUFFER`]: a client
//! that falls further behind is dropped (told so, between frames), never
//! waited for. What clients type goes to the container's stdin through a
//! queue of at most [`STDIN_QUEUE`]; typing beyond it (a container not
//! reading as fast) is discarded, but every client's frames are still read,
//! so its detach keys and resizes always arrive.
//!
//! **A held client** ([`Frame::HelloHeld`]: a foreground run's own, stage
//! 9, step 9.4c3a) is the exception: the container waits for it. While it's
//! more than [`CLIENT_BUFFER`] behind, the container's outputs aren't read
//! ([`Attachments::behind`]: the container waits as on a full pipe); while
//! the container's stdin queue is full, what it types isn't read. Nothing
//! it's sent or sends is lost.
//!
//! At most [`MAX_CLIENTS`] clients are let in, and as many may be waiting to
//! say hello; a connection that hasn't within [`HELLO_WAIT`] is closed. A
//! client on its way out gets [`LEAVE_WAIT`] to take its last frames.
//!
//! **`--stdin-once`:** the first client to type (or to say its input ended)
//! is the container's typist; when it goes, or its input ends, the
//! container's stdin is closed once what it typed has been written.
//!
//! **Control clients** (stage 9): a [`Frame::HelloControl`] is welcomed
//! apart from the attach clients (not counted in [`MAX_CLIENTS`];
//! connections still to say hello, of any kind, share one cap), and is
//! never sent output. A waiter (`wait`) waits for the end; a stopper
//! (`stop`) sends one stop ([`Frame::Stop`]) at once, kept here for the
//! monitor ([`StopRequest`]), which sees it through, and goes. Each kind has
//! its own cap ([`MAX_CONTROL`]), so waiters never keep a stop out.
//! Anything else from one isn't the protocol.
//!
//! **Holders** (stage 9, step 9.7b1: [`Frame::HelloHolder`]) are control
//! clients of a third kind: each asks the monitor to hold one descriptor
//! ([`Frame::Hold`], the descriptor passed with it, `SCM_RIGHTS`), or to let
//! one go ([`Frame::Release`]), is answered ([`Frame::Done`]), and goes.
//! Every client is read with `recvmsg`, so a descriptor passed is never lost
//! unseen: one from anyone but a holder, a second one, or more than fit
//! (`MSG_CTRUNC`) is closed, and its client let go of.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags};
use shipyard_sys::fdpass::Room;

use crate::attach::Frame;
use crate::logs::cri::Stream;

use connection::{Client, Leaving, Role};

mod connection;
mod frames;
mod roles;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests;

/// The socket's name in the state directory.
pub(in crate::container) const SOCKET: &str = "attach.sock";

/// The path to the attach socket in the directory open at `dir`, through
/// `/proc/self/fd`: `bind` and `connect` take a path, never a descriptor,
/// and this one fits a socket's 108 bytes whatever the directory's own path.
pub(in crate::container) fn socket_path(dir: &impl AsRawFd) -> String {
    format!("/proc/self/fd/{}/{SOCKET}", dir.as_raw_fd())
}

/// The most clients attached at once (the user's decision, 8.3c).
const MAX_CLIENTS: usize = 16;

/// The most control clients of each kind at once: waiters (`wait`), and
/// stoppers (`stop`), so waiters can never keep a stop out.
const MAX_CONTROL: usize = 16;

/// A stop a control client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StopRequest {
    /// The signal for the init now.
    pub(super) signal: i32,
    /// How long before SIGKILL.
    pub(super) timeout: Duration,
}

impl StopRequest {
    /// A stop, if its signal can stop the init's command
    /// ([`is_stop_signal`](crate::process::is_stop_signal): not one that
    /// stops or continues it, nor one the init can't pass on).
    fn of(signal: u8, timeout_secs: u32) -> Option<Self> {
        let signal = i32::from(signal);
        crate::process::is_stop_signal(signal).then_some(Self {
            signal,
            timeout: Duration::from_secs(u64::from(timeout_secs)),
        })
    }
}

/// The most output queued for one client before it's dropped.
const CLIENT_BUFFER: usize = 256 * 1024;

/// The most typing queued for the container's stdin.
const STDIN_QUEUE: usize = 64 * 1024;

/// How long a connection has to say hello.
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// How long the clients get to take their last frames, at the end.
const LAST_FLUSH: Duration = Duration::from_secs(1);

/// How long a client on its way out (detached, dropped, refused) gets to
/// take its last frames.
const LEAVE_WAIT: Duration = Duration::from_secs(5);

/// How long a held client (a foreground run's own: [`Frame::HelloHeld`])
/// gets to take its last frames, at the end or after it's asked to go: it
/// may be a slow reader's (`run … | xz`), and what it hasn't taken is lost
/// to it.
const HELD_WAIT: Duration = Duration::from_secs(60);

/// How many reads of one client in a pass, so one busy client can't keep
/// the others (and the container's output) waiting.
const READS_PER_PASS: usize = 4;

/// How long the listener rests when `accept` fails for want of descriptors
/// or memory (it would otherwise be ready again at once, forever).
const ACCEPT_REST: Duration = Duration::from_millis(100);

/// The most descriptors one message can pass (`SCM_MAX_FD`): one read of a
/// stream socket brings at most one message's, so with room for this many
/// none is cut off for want of room. (At the monitor's descriptor limit,
/// the kernel installs what it can and says `MSG_CTRUNC`: those it did are
/// read all the same, and closed with the client it drops: F54.)
pub(super) const MOST_PASSED: usize = 253;

/// The container's stdin, as the monitor holds it.
#[derive(Debug)]
pub(super) struct Stdin {
    /// A pipe's write end, or the terminal's master (non-blocking).
    fd: OwnedFd,
    /// Typing not written yet.
    queue: Vec<u8>,
    /// Close it when its typist goes (`--stdin-once`).
    once: bool,
    /// Whether a client has typed into it yet (the typist, for
    /// `--stdin-once`).
    typist_seen: bool,
    /// Closed once the queue is written: its typist has gone. Nothing more
    /// is taken.
    closing: bool,
}

impl Stdin {
    /// Whether a client typing now is the first to (`stdin`: the container's,
    /// if it takes input).
    fn first_typist(stdin: Option<&mut Self>) -> bool {
        stdin.is_some_and(|stdin| !std::mem::replace(&mut stdin.typist_seen, true))
    }

    pub(super) fn new(fd: OwnedFd, once: bool) -> Self {
        Self {
            fd,
            queue: Vec::new(),
            once,
            typist_seen: false,
            closing: false,
        }
    }
}

/// `attach.sock` and its clients.
#[derive(Debug)]
pub(super) struct Attachments {
    listener: Option<UnixListener>,
    clients: Vec<Client>,
    terminal: bool,
    /// The terminal's master, for resizing (a container with a terminal,
    /// once it has come).
    master: Option<OwnedFd>,
    /// The latest size asked for before the master came: set when it does.
    size: Option<crate::process::terminal::Size>,
    /// The latest size a client asked for in this read of it, set once it's
    /// read (a flood of sizes is one change, and one SIGWINCH, not many).
    asked: Option<crate::process::terminal::Size>,
    /// How many sizes were set (tests: one per read, whatever was asked).
    #[cfg(test)]
    sizes_set: usize,
    /// How many sizes were set when typing was last written (tests: the
    /// order of a size and the typing after it, F68).
    #[cfg(test)]
    typed_after_sizes: usize,
    /// Whether the container takes input (`create --stdin`), as clients are
    /// told: with a terminal, before its master (`stdin`) has come.
    takes_stdin: bool,
    stdin: Option<Stdin>,
    /// The container has ended: no one more is let in.
    ended: bool,
    /// Until when the listener rests (`accept` failed for want of
    /// descriptors).
    resting: Option<Instant>,
    read: Vec<u8>,
    /// Room for the descriptors a read may bring ([`MOST_PASSED`]).
    control: Room,
    /// The stops asked for, not yet taken by the monitor.
    stops: Vec<StopRequest>,
    /// What the monitor holds, while it's served here: taking more from
    /// when the monitor hands them over (the container created) until it
    /// begins to end.
    holds: super::holds::Holds,
}

impl Attachments {
    /// Listens on `attach.sock` in the state directory open at `dir`
    /// (through its descriptor: the path may be longer than a socket
    /// address holds). `stdin` is the container's pipe, if it takes input
    /// without a terminal; `takes_stdin` whether it takes input at all.
    pub(super) fn listen(
        dir: &OwnedFd,
        terminal: bool,
        takes_stdin: bool,
        stdin: Option<Stdin>,
    ) -> std::io::Result<Self> {
        let path = socket_path(dir);
        // A socket left by a container of the same ID before: not ours.
        let _ = nix::unistd::unlinkat(dir, SOCKET, nix::unistd::UnlinkatFlags::NoRemoveDir);
        let listener = UnixListener::bind(&path)?;
        // Following links: the name was just made by `bind`, in a directory
        // only this user can write. (`AT_SYMLINK_NOFOLLOW` isn't supported
        // before glibc 2.32: ENOTSUP, on Ubuntu 20.04 and RHEL 8.)
        nix::sys::stat::fchmodat(
            dir,
            SOCKET,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            nix::sys::stat::FchmodatFlags::FollowSymlink,
        )?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: Some(listener),
            clients: Vec::new(),
            terminal,
            master: None,
            size: None,
            asked: None,
            #[cfg(test)]
            sizes_set: 0,
            #[cfg(test)]
            typed_after_sizes: 0,
            takes_stdin,
            stdin,
            ended: false,
            resting: None,
            read: vec![0; 64 * 1024],
            control: Room::new(MOST_PASSED),
            stops: Vec::new(),
            holds: super::holds::Holds::default(),
        })
    }

    /// The terminal's master has come (a container with a terminal): copies
    /// for resizing (to the size a client asked for meanwhile), and for
    /// typing if it takes input.
    pub(super) fn set_master(&mut self, master: &OwnedFd) {
        self.master = master.try_clone().ok();
        if let (Some(master), Some(size)) = (&self.master, self.size) {
            let _ = crate::process::terminal::set_window_size(master.as_raw_fd(), size);
        }
        if self.takes_stdin
            && let Ok(typing) = master.try_clone()
        {
            self.stdin = Some(Stdin::new(typing, false));
        }
    }

    /// What to wait on.
    pub(super) fn poll_fds(&self) -> Vec<PollFd<'_>> {
        let mut fds: Vec<PollFd<'_>> = self
            .listener
            .iter()
            .filter(|_| self.resting.is_none())
            .map(|listener| PollFd::new(listener.as_fd(), PollFlags::POLLIN))
            .collect();
        let stdin_full = self.stdin_full();
        for client in &self.clients {
            fds.push(PollFd::new(
                client.stream.as_fd(),
                client.events(stdin_full),
            ));
        }
        if let Some(stdin) = self.stdin.as_ref().filter(|stdin| !stdin.queue.is_empty()) {
            fds.push(PollFd::new(stdin.fd.as_fd(), PollFlags::POLLOUT));
        }
        fds
    }

    /// The longest to wait before something is due: a connection's hello
    /// late, a leaving client's time up, the listener's rest over.
    pub(super) fn timeout(&self) -> Option<Duration> {
        self.clients
            .iter()
            .filter_map(|client| match client.leaving {
                Leaving::AfterFrames(since) => {
                    Some(client.leave_wait().saturating_sub(since.elapsed()))
                }
                _ if !client.welcomed || client.role.is_brief() => {
                    Some(HELLO_WAIT.saturating_sub(client.connected.elapsed()))
                }
                _ => None,
            })
            .chain(
                self.resting
                    .map(|until| until.saturating_duration_since(Instant::now())),
            )
            .min()
    }

    /// The stops control clients asked for since the last call.
    pub(super) fn take_stops(&mut self) -> Vec<StopRequest> {
        std::mem::take(&mut self.stops)
    }

    /// The monitor's `holds`, served here from now on: holders add to them
    /// and let them go.
    pub(super) fn serve_holds(&mut self, holds: super::holds::Holds) {
        self.holds = holds;
    }

    /// The container is ending: a hold now is refused (it could miss the
    /// exit command); a release still lets go.
    pub(super) fn stop_taking_holds(&mut self) {
        self.holds.stop_taking();
    }

    /// The holds, back to the monitor (for its exit command).
    pub(super) fn take_holds(&mut self) -> super::holds::Holds {
        std::mem::take(&mut self.holds)
    }

    /// Output from the container, for every attach client let in (control
    /// clients never get it).
    pub(super) fn output(&mut self, stream: Stream, bytes: &[u8]) {
        for client in self.clients.iter_mut().filter(|client| {
            client.welcomed && client.role == Role::Attach && client.leaving == Leaving::No
        }) {
            let mut frames = Vec::new();
            Frame::encode_output(stream == Stream::Stderr, bytes, &mut frames);
            client.queued += frames.len();
            client.frames.push_back(frames);
            // Held: the container waits for it instead (`behind`).
            if client.queued > CLIENT_BUFFER && !client.held {
                client.finish_with(&Frame::Dropped(format!(
                    "more than {} KiB of output behind",
                    CLIENT_BUFFER / 1024
                )));
            }
        }
    }

    /// Whether a held client is more than [`CLIENT_BUFFER`] behind: the
    /// container's output isn't read meanwhile (it waits, as on a full
    /// pipe), rather than the client let go of.
    pub(super) fn behind(&self) -> bool {
        self.clients.iter().any(|client| {
            client.held
                && client.welcomed
                && client.leaving == Leaving::No
                && client.queued > CLIENT_BUFFER
        })
    }

    /// Whether the container's input queue is full.
    fn stdin_full(&self) -> bool {
        self.stdin
            .as_ref()
            .is_some_and(|stdin| stdin.queue.len() >= STDIN_QUEUE)
    }

    /// Lets in who's connecting, reads and writes every client, and writes
    /// typing to the container's stdin: nothing waits.
    pub(super) fn service(&mut self) {
        self.accept();
        for at in 0..self.clients.len() {
            self.read_client(at);
        }
        self.write_stdin();
        let now = Instant::now();
        for client in &mut self.clients {
            let late = match client.leaving {
                Leaving::AfterFrames(since) => now.duration_since(since) >= client.leave_wait(),
                _ => {
                    (!client.welcomed || client.role.is_brief())
                        && now.duration_since(client.connected) >= HELLO_WAIT
                }
            };
            if late {
                client.leaving = Leaving::Now;
            }
            client.write();
        }
        self.remove_gone();
    }

    /// Writes queued typing to the container's stdin, as much as it takes.
    fn write_stdin(&mut self) {
        let Some(stdin) = &mut self.stdin else {
            return;
        };
        while !stdin.queue.is_empty() {
            // (Typing to a terminal that isn't being read: its input
            // queue fills, and writes wait (EAGAIN) until it's read.)
            match nix::unistd::write(&stdin.fd, &stdin.queue) {
                Ok(written) => {
                    stdin.queue.drain(..written);
                }
                Err(Errno::EAGAIN) => return,
                Err(Errno::EINTR) => {}
                // Nobody reads it any more (EPIPE): closed, and what's
                // typed from now on goes nowhere.
                Err(_) => {
                    self.stdin = None;
                    return;
                }
            }
        }
        if stdin.closing {
            self.stdin = None;
        }
    }

    /// With `--stdin-once`, the typist has gone or its input has ended: the
    /// container's stdin is closed once what's queued is written (end of
    /// file for the container, then).
    fn typist_done(&mut self) {
        if let Some(stdin) = self.stdin.as_mut().filter(|stdin| stdin.once) {
            stdin.closing = true;
        }
        self.write_stdin();
    }

    /// Lets go of clients that are gone; with `--stdin-once`, the typist
    /// going closes the container's stdin.
    fn remove_gone(&mut self) {
        let mut typist_gone = false;
        self.clients.retain(|client| {
            let gone = client.leaving == Leaving::Now;
            typist_gone |= gone && client.typist;
            !gone
        });
        if typist_gone {
            self.typist_done();
        }
    }

    /// The container has ended with `code` (its exit recorded already):
    /// every client let in is told so, those still to say hello are
    /// refused, and they get [`LAST_FLUSH`] to take it. (Those already on
    /// their way out, detached or dropped, have been told why.)
    pub(super) fn end(&mut self, code: i32) {
        self.ended = true;
        self.listener = None;
        let now = Instant::now();
        for client in self
            .clients
            .iter_mut()
            .filter(|client| client.leaving == Leaving::No)
        {
            if client.welcomed {
                client.send(&Frame::Exited(code));
                client.leaving = Leaving::AfterFrames(now);
            } else {
                client.finish_with(&Frame::Refused("the container has ended".to_owned()));
            }
        }
        // A held client (a slow reader's) gets longer: what it hasn't taken
        // is lost to it.
        let wait = if self.clients.iter().any(|client| client.held) {
            HELD_WAIT
        } else {
            LAST_FLUSH
        };
        let deadline = Instant::now() + wait;
        while !self.clients.is_empty() && Instant::now() < deadline {
            self.service();
            let mut fds = self.poll_fds();
            let _ = nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(50_u16));
        }
        self.clients.clear();
    }
}
