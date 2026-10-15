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

use std::collections::VecDeque;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags};

use crate::attach::{
    CONTROL_VERSION, Decoder, DetachKeys, Frame, HOLD_VERSION, Matcher, negotiate,
};
use crate::logs::cri::Stream;

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

/// A client on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leaving {
    /// It isn't.
    No,
    /// Once its last frames are written, or [`LEAVE_WAIT`] after this.
    AfterFrames(Instant),
    /// Now.
    Now,
}

#[derive(Debug)]
struct Client {
    stream: UnixStream,
    decoder: Decoder,
    /// Its detach keys, watched for in what it types; set by its hello.
    matcher: Option<Matcher>,
    welcomed: bool,
    connected: Instant,
    /// Frames for it, whole; the first may be partly written (`offset`).
    frames: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    /// Whether it's on its way out.
    leaving: Leaving,
    /// The first client to type (`--stdin-once` closes stdin when it goes,
    /// or its input ends).
    typist: bool,
    /// What it's there for.
    role: Role,
    /// Held ([`Frame::HelloHeld`]): never let go of for falling behind; the
    /// container waits for it instead.
    held: bool,
    /// A descriptor a holder sent, for its [`Frame::Hold`].
    sent: Option<OwnedFd>,
}

/// What a client is there for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Attached: sent the output, and may type.
    Attach,
    /// A control client waiting for the end (`wait`): sent no output, takes
    /// no input.
    Waiter,
    /// A control client there to stop the container (`stop`): its one
    /// [`Frame::Stop`] is due within [`HELLO_WAIT`] of its connecting, and it
    /// goes after it.
    Stopper,
    /// A control client there to hold a descriptor, or let one go (version
    /// 3): its one [`Frame::Hold`] or [`Frame::Release`] is due within
    /// [`HELLO_WAIT`] of its connecting, and it goes once answered.
    Holder,
}

impl Role {
    /// Whether it's there for one request, due within [`HELLO_WAIT`].
    fn is_brief(self) -> bool {
        matches!(self, Self::Stopper | Self::Holder)
    }
}

impl Client {
    fn send(&mut self, frame: &Frame) {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        self.queued += bytes.len();
        self.frames.push_back(bytes);
    }

    /// Says `frame` and then closes, or (in the middle of a frame) just
    /// closes: a frame after a partly written one would be garbage.
    fn finish_with(&mut self, frame: &Frame) {
        if self.offset == 0 {
            self.frames.clear();
            self.queued = 0;
            self.send(frame);
            self.leaving = Leaving::AfterFrames(Instant::now());
        } else {
            self.leaving = Leaving::Now;
        }
    }

    /// Writes what the socket takes now.
    fn write(&mut self) {
        while let Some(front) = self.frames.front() {
            match self
                .stream
                .write(front.get(self.offset..).unwrap_or_default())
            {
                Ok(written) => {
                    self.offset += written;
                    self.queued = self.queued.saturating_sub(written);
                    if self.offset == front.len() {
                        self.frames.pop_front();
                        self.offset = 0;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.leaving = Leaving::Now;
                    return;
                }
            }
        }
        if matches!(self.leaving, Leaving::AfterFrames(_)) {
            self.leaving = Leaving::Now;
        }
    }

    /// How long it gets to take its last frames once asked to go.
    fn leave_wait(&self) -> Duration {
        if self.held { HELD_WAIT } else { LEAVE_WAIT }
    }

    /// Whether what it sends waits: it's held, and the container's input
    /// (`stdin_full`) has no room.
    fn input_waits(&self, stdin_full: bool) -> bool {
        self.held && stdin_full
    }

    /// What to wait for on its socket: what it sends (unless that waits:
    /// `stdin_full`), and room for what it's sent, if anything. A client on
    /// its way out isn't read any more (its unread input would make it ready
    /// forever).
    fn events(&self, stdin_full: bool) -> PollFlags {
        let mut events = PollFlags::empty();
        if self.leaving == Leaving::No && !self.input_waits(stdin_full) {
            events |= PollFlags::POLLIN;
        }
        if !self.frames.is_empty() {
            events |= PollFlags::POLLOUT;
        }
        events
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
    size: Option<(u16, u16)>,
    /// The latest size a client asked for in this read of it, set once it's
    /// read (a flood of sizes is one change, and one SIGWINCH, not many).
    asked: Option<(u16, u16)>,
    /// How many sizes were set (tests: one per read, whatever was asked).
    #[cfg(test)]
    sizes_set: usize,
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
    control: Vec<u8>,
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
            takes_stdin,
            stdin,
            ended: false,
            resting: None,
            read: vec![0; 64 * 1024],
            control: nix::cmsg_space!([std::os::fd::RawFd; MOST_PASSED]),
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

    fn accept(&mut self) {
        if self.resting.is_some_and(|until| Instant::now() < until) {
            return;
        }
        self.resting = None;
        let Some(listener) = &self.listener else {
            return;
        };
        loop {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
                // Gone before it was taken: the next.
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue;
                }
                // EMFILE, ENFILE, ENOMEM: the connection stays queued, and
                // the listener ready; resting, rather than trying again at
                // once, forever.
                Err(_) => {
                    self.resting = Some(Instant::now() + ACCEPT_REST);
                    return;
                }
            };
            // Who connected, as the kernel says: this user, or root.
            let allowed =
                nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                    .is_ok_and(|peer| {
                        let me = nix::unistd::geteuid().as_raw();
                        peer.uid() == me || peer.uid() == 0
                    });
            if !allowed || stream.set_nonblocking(true).is_err() {
                continue;
            }
            // Those still to say hello: as many as may be let in, no more.
            let waiting = self
                .clients
                .iter()
                .filter(|client| !client.welcomed && client.leaving == Leaving::No)
                .count();
            if waiting >= MAX_CLIENTS {
                // Said before its hello is read: the client reads it even
                // if its hello found the socket closed.
                let mut refused = Vec::new();
                Frame::Refused(format!(
                    "{MAX_CLIENTS} connections are waiting to say hello already"
                ))
                .encode(&mut refused);
                let _ = (&stream).write(&refused);
                continue;
            }
            self.clients.push(Client {
                stream,
                decoder: Decoder::new(),
                matcher: None,
                welcomed: false,
                connected: Instant::now(),
                frames: VecDeque::new(),
                offset: 0,
                queued: 0,
                leaving: Leaving::No,
                typist: false,
                role: Role::Attach,
                held: false,
                sent: None,
            });
        }
    }

    fn read_client(&mut self, at: usize) {
        self.read_frames(at);
        // The last size it asked for, once, whatever made the reading stop
        // (it was sent before; a size is harmless).
        if let Some(size) = self.asked.take() {
            self.set_size(size);
        }
    }

    /// The terminal's size, now (or once the master comes).
    fn set_size(&mut self, size: (u16, u16)) {
        #[cfg(test)]
        {
            self.sizes_set += 1;
        }
        match &self.master {
            Some(master) => {
                let _ = crate::process::terminal::set_window_size(master.as_raw_fd(), size);
            }
            None => self.size = Some(size),
        }
    }

    /// What client `at` sent, frame by frame ([`Self::read_client`]).
    fn read_frames(&mut self, at: usize) {
        for _ in 0..READS_PER_PASS {
            let stdin_full = self.stdin_full();
            let Some(client) = self.clients.get_mut(at) else {
                return;
            };
            if client.leaving != Leaving::No || client.input_waits(stdin_full) {
                return;
            }
            match receive(&client.stream, &mut self.read, &mut self.control) {
                Ok(Received { read: 0, .. }) => {
                    client.leaving = Leaving::Now;
                    return;
                }
                Ok(Received {
                    read,
                    fds,
                    truncated,
                }) => {
                    // A descriptor only from a holder, one at a time, and
                    // none cut off: else it's closed, and its client goes.
                    if !fds.is_empty() || truncated {
                        let one = fds.len() == 1 && !truncated;
                        let fits = one && client.role == Role::Holder && client.sent.is_none();
                        if !fits {
                            client.leaving = Leaving::Now;
                            return;
                        }
                        client.sent = fds.into_iter().next();
                    }
                    client
                        .decoder
                        .push(self.read.get(..read).unwrap_or_default());
                }
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => return,
                Err(_) => {
                    client.leaving = Leaving::Now;
                    return;
                }
            }
            loop {
                let Some(client) = self.clients.get_mut(at) else {
                    return;
                };
                // Detached (or dropped): what it sent after isn't taken.
                if client.leaving != Leaving::No {
                    return;
                }
                match client.decoder.next_frame() {
                    Ok(Some(frame)) => self.handle(at, frame),
                    Ok(None) => break,
                    // A client that isn't the protocol: dropped, alone.
                    Err(_) => {
                        client.leaving = Leaving::Now;
                        return;
                    }
                }
            }
        }
    }

    fn handle(&mut self, at: usize, frame: Frame) {
        let attached = self.welcomed(Role::Attach);
        let (terminal, takes_stdin, ended) = (self.terminal, self.takes_stdin, self.ended);
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        if client.role != Role::Attach {
            self.handle_control(at, frame);
            return;
        }
        match (client.welcomed, frame) {
            (false, Frame::HelloControl { version, stopper }) => {
                let role = if stopper { Role::Stopper } else { Role::Waiter };
                self.welcome_control(at, version, role);
            }
            (false, Frame::HelloHolder { version }) => {
                self.welcome_control(at, version, Role::Holder);
            }
            (false, hello @ (Frame::Hello { .. } | Frame::HelloHeld { .. })) => {
                let held = matches!(hello, Frame::HelloHeld { .. });
                let (Frame::Hello {
                    version,
                    detach_keys,
                }
                | Frame::HelloHeld {
                    version,
                    detach_keys,
                }) = hello
                else {
                    return;
                };
                let refused = if ended {
                    Some("the container has ended".to_owned())
                } else if attached >= MAX_CLIENTS {
                    Some(format!("{MAX_CLIENTS} clients are attached already"))
                } else if negotiate(version).is_none() {
                    Some(format!("attach protocol version {version}"))
                } else {
                    None
                };
                if let Some(reason) = refused {
                    client.finish_with(&Frame::Refused(reason));
                    return;
                }
                client.matcher = DetachKeys::from_bytes(&detach_keys).map(|keys| keys.matcher());
                client.welcomed = true;
                client.held = held;
                client.send(&Frame::Welcome {
                    version: negotiate(version).unwrap_or(1),
                    terminal,
                    stdin: takes_stdin,
                });
            }
            (true, Frame::Stdin(typed)) => {
                client.typist |= Stdin::first_typist(self.stdin.as_mut());
                let mut passed = Vec::new();
                let detached = client
                    .matcher
                    .as_mut()
                    .is_some_and(|matcher| matcher.feed(&typed, &mut passed));
                if detached {
                    client.finish_with(&Frame::Detached);
                }
                if let Some(stdin) = self.stdin.as_mut().filter(|stdin| !stdin.closing) {
                    // Beyond the queue (a container not reading): dropped;
                    // a held client's all kept (it isn't read while the
                    // queue is full: at most a read's worth beyond it).
                    let room = if client.held {
                        passed.len()
                    } else {
                        STDIN_QUEUE.saturating_sub(stdin.queue.len())
                    };
                    stdin.queue.extend_from_slice(
                        passed.get(..room.min(passed.len())).unwrap_or_default(),
                    );
                }
                // Into the pipe at once, as far as it takes: the queue is
                // for what it doesn't.
                self.write_stdin();
            }
            (true, Frame::StdinEnd) => {
                client.typist |= Stdin::first_typist(self.stdin.as_mut());
                if client.typist {
                    self.typist_done();
                }
            }
            // Set once the read is done ([`Self::read_client`]).
            (true, Frame::Resize { rows, cols }) if terminal => self.asked = Some((rows, cols)),
            (true, Frame::Resize { .. }) => {}
            // Anything else, or a hello twice: not the protocol.
            _ => client.leaving = Leaving::Now,
        }
    }

    /// How many clients of `role` are let in.
    fn welcomed(&self, role: Role) -> usize {
        self.clients
            .iter()
            .filter(|client| client.welcomed && client.role == role)
            .count()
    }

    /// Client `at` said [`Frame::HelloControl`] or [`Frame::HelloHolder`]
    /// with `version`: let in as a control client of `role`, unless the
    /// container has ended, [`MAX_CONTROL`] of its kind are in already, or
    /// its version has none of that kind.
    fn welcome_control(&mut self, at: usize, version: u32, role: Role) {
        let first = if role == Role::Holder {
            HOLD_VERSION
        } else {
            CONTROL_VERSION
        };
        let controlling = self.welcomed(role);
        let (terminal, takes_stdin, ended) = (self.terminal, self.takes_stdin, self.ended);
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        let refused = if ended {
            Some("the container has ended".to_owned())
        } else if controlling >= MAX_CONTROL {
            Some(format!(
                "{MAX_CONTROL} control clients are connected already"
            ))
        } else {
            None
        };
        match (
            refused,
            negotiate(version).filter(|agreed| *agreed >= first),
        ) {
            (None, Some(agreed)) => {
                client.role = role;
                client.welcomed = true;
                client.send(&Frame::Welcome {
                    version: agreed,
                    terminal,
                    stdin: takes_stdin,
                });
            }
            (Some(reason), _) => client.finish_with(&Frame::Refused(reason)),
            (None, None) => client.finish_with(&Frame::Refused(format!(
                "control protocol version {version}"
            ))),
        }
    }

    /// A frame from control client `at`, welcomed: a stopper's stop, kept
    /// for the monitor (then the stopper goes), or a stop that can't be
    /// (refused); a holder's hold or release, done or not, and answered.
    /// Anything else (a waiter's stop too) isn't the protocol.
    fn handle_control(&mut self, at: usize, frame: Frame) {
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        match client.role {
            Role::Holder => self.handle_holder(at, frame),
            Role::Stopper => self.handle_stop(at, &frame),
            Role::Attach | Role::Waiter => client.leaving = Leaving::Now,
        }
    }

    /// Holder `at`'s one request: a hold (of the descriptor that came with
    /// it, if it's a file, a directory or a socket) or a release (with none
    /// passed), done or not, and answered.
    fn handle_holder(&mut self, at: usize, frame: Frame) {
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        let done = match (frame, client.sent.take()) {
            (Frame::Hold { .. }, None) => Err("no descriptor came with the hold".to_owned()),
            (Frame::Hold { label }, Some(fd)) => match holdable(fd) {
                Ok(fd) => self.holds.add(fd, label),
                Err(why) => Err(why),
            },
            (Frame::Release { label }, None) => self.holds.release(&label),
            (Frame::Release { .. }, Some(_)) => {
                Err("a descriptor came with the release: nothing let go of".to_owned())
            }
            _ => {
                client.leaving = Leaving::Now;
                return;
            }
        };
        client.finish_with(&match done {
            Ok(()) => Frame::Done {
                ok: true,
                why: String::new(),
            },
            Err(why) => Frame::Done { ok: false, why },
        });
    }

    /// Stopper `at`'s one stop, kept for the monitor (then the stopper
    /// goes), or one that can't be (refused); anything else isn't the
    /// protocol.
    fn handle_stop(&mut self, at: usize, frame: &Frame) {
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        let &Frame::Stop {
            signal,
            timeout_secs,
        } = frame
        else {
            client.leaving = Leaving::Now;
            return;
        };
        match StopRequest::of(signal, timeout_secs) {
            Some(request) => {
                self.stops.push(request);
                client.leaving = Leaving::Now;
            }
            None => client.finish_with(&Frame::Refused(format!(
                "signal {signal} can't stop a container"
            ))),
        }
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

/// Whether a passed descriptor `fd` can be held: a file, a directory or a
/// socket (what a caller's resources are: a lock file, a directory, a bound
/// port), as `fd` back. A socket is made to close at once, never lingering
/// over unsent data (a close that waits would hold up the monitor's one
/// loop). Anything else (a pipe, a device) is refused, and closed.
fn holdable(fd: OwnedFd) -> Result<OwnedFd, String> {
    let mode = nix::sys::stat::fstat(&fd).map_or(0, |stat| stat.st_mode) & libc::S_IFMT;
    if mode == libc::S_IFSOCK {
        let linger = libc::linger {
            l_onoff: 0,
            l_linger: 0,
        };
        let _ = nix::sys::socket::setsockopt(&fd, nix::sys::socket::sockopt::Linger, &linger);
    }
    if [libc::S_IFREG, libc::S_IFDIR, libc::S_IFSOCK].contains(&mode) {
        Ok(fd)
    } else {
        Err("only a file, a directory or a socket is held".to_owned())
    }
}

/// What one read of a client brought: bytes, and descriptors passed with
/// them.
#[derive(Debug)]
struct Received {
    /// How many bytes (into the buffer read into); 0 at end-of-file.
    read: usize,
    /// Descriptors passed (`SCM_RIGHTS`), close-on-exec, owned now.
    fds: Vec<OwnedFd>,
    /// More were passed than there was room for: those were closed by the
    /// kernel (`MSG_CTRUNC`).
    truncated: bool,
}

/// The most descriptors one message can pass (`SCM_MAX_FD`): one read of a
/// stream socket brings at most one message's, so with room for this many
/// none is ever cut off (`MSG_CTRUNC`), and every one the kernel installs is
/// seen, and closed if it isn't wanted. (With less room, the kernel installs
/// what fits and says `MSG_CTRUNC`; nix then gives no messages to read, and
/// those installed would never be closed.)
const MOST_PASSED: usize = 253;

/// Reads what `stream` has into `buffer`, with any descriptors passed with
/// it (`control`: room for [`MOST_PASSED`]), close-on-exec as they're
/// received (`MSG_CMSG_CLOEXEC`, TLPI §61.13.3): never inherited by what
/// the monitor starts. A holder passes one, with its hold.
#[allow(clippy::ptr_arg, reason = "nix's `recvmsg` takes its room as a `Vec`")]
fn receive(
    stream: &UnixStream,
    buffer: &mut [u8],
    control: &mut Vec<u8>,
) -> Result<Received, Errno> {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    use std::os::fd::FromRawFd as _;
    let mut iov = [std::io::IoSliceMut::new(buffer)];
    let message = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(control),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )?;
    let mut fds = Vec::new();
    // (Never `MSG_CTRUNC` for descriptors, with room for all of one
    // message's; were it said anyway, nix gives none to read: refused.)
    for cmsg in message.cmsgs().into_iter().flatten() {
        if let ControlMessageOwned::ScmRights(received) = cmsg {
            // SAFETY: the kernel just installed these in this process: ours.
            fds.extend(
                received
                    .into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
            );
        }
    }
    Ok(Received {
        read: message.bytes,
        fds,
        truncated: message.flags.contains(MsgFlags::MSG_CTRUNC),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;
    use crate::attach::VERSION;
    use std::os::fd::BorrowedFd;
    use std::os::unix::fs::MetadataExt as _;

    /// An attach socket of a test's own, in a scratch directory, with what
    /// the monitor holds (taking more) or not.
    fn listening(
        name: &str,
        holds: Option<super::super::holds::Holds>,
    ) -> (Attachments, std::path::PathBuf) {
        listening_with(name, holds, false)
    }

    /// [`listening`], for a container with a terminal or without.
    fn listening_with(
        name: &str,
        holds: Option<super::super::holds::Holds>,
        terminal: bool,
    ) -> (Attachments, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("shipyard-attach-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fd = OwnedFd::from(std::fs::File::open(&dir).unwrap());
        let mut attachments = Attachments::listen(&fd, terminal, false, None).unwrap();
        if let Some(holds) = holds {
            attachments.serve_holds(holds);
        }
        (attachments, dir)
    }

    /// A client of `attachments`, connected.
    fn connect(dir: &std::path::Path) -> UnixStream {
        let stream = UnixStream::connect(dir.join(SOCKET)).unwrap();
        stream.set_nonblocking(true).unwrap();
        stream
    }

    fn send(stream: &UnixStream, frame: &Frame, fd: Option<BorrowedFd<'_>>) {
        send_fds(stream, frame, fd.as_slice());
    }

    /// Sends `frame` with `fds` passed alongside, in one message.
    fn send_fds(stream: &UnixStream, frame: &Frame, fds: &[BorrowedFd<'_>]) {
        use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        let fds: Vec<std::os::fd::RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let cmsgs: Vec<ControlMessage<'_>> = if fds.is_empty() {
            Vec::new()
        } else {
            vec![ControlMessage::ScmRights(&fds)]
        };
        let sent = sendmsg::<()>(
            stream.as_raw_fd(),
            &[std::io::IoSlice::new(&bytes)],
            &cmsgs,
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        assert_eq!(sent, bytes.len());
    }

    /// The next frame for `stream`, `attachments` served meanwhile; `None`
    /// if it's let go of (or nothing comes within a few seconds).
    fn next(
        attachments: &mut Attachments,
        stream: &UnixStream,
        decoder: &mut Decoder,
    ) -> Option<Frame> {
        use std::io::Read as _;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buffer = [0_u8; 1024];
        while Instant::now() < deadline {
            if let Ok(Some(frame)) = decoder.next_frame() {
                return Some(frame);
            }
            attachments.service();
            match (&*stream).read(&mut buffer) {
                Ok(0) => return decoder.next_frame().ok().flatten(),
                Ok(read) => decoder.push(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// A holder let in, with `version`.
    fn holder(
        attachments: &mut Attachments,
        dir: &std::path::Path,
        version: u32,
    ) -> (UnixStream, Decoder, Option<Frame>) {
        let stream = connect(dir);
        let mut decoder = Decoder::new();
        send(&stream, &Frame::HelloHolder { version }, None);
        let welcome = next(attachments, &stream, &mut decoder);
        (stream, decoder, welcome)
    }

    /// A regular file to pass (this test's own program: there, and not a
    /// device, which isn't held).
    fn file() -> std::fs::File {
        std::fs::File::open(std::env::current_exe().unwrap()).unwrap()
    }

    /// A flood of sizes in one read sets the terminal's size once, to the
    /// last asked for: one SIGWINCH for the container, not one each (F40).
    #[test]
    fn a_flood_of_sizes_is_one_change() {
        use std::io::Write as _;
        let (mut attachments, dir) = listening_with("sizes", None, true);
        let (master, _slave) = crate::process::terminal::host_pty((24, 80)).unwrap();
        attachments.set_master(&master);
        let stream = connect(&dir);
        let mut decoder = Decoder::new();
        send(
            &stream,
            &Frame::Hello {
                version: VERSION,
                detach_keys: Vec::new(),
            },
            None,
        );
        assert!(
            matches!(
                next(&mut attachments, &stream, &mut decoder),
                Some(Frame::Welcome { .. })
            ),
            "welcomed"
        );
        let before = attachments.sizes_set;
        let mut flood = Vec::new();
        for n in 0..200_u16 {
            Frame::Resize {
                rows: 10 + n,
                cols: 40 + n,
            }
            .encode(&mut flood);
        }
        (&stream).write_all(&flood).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while attachments.sizes_set == before && Instant::now() < deadline {
            attachments.service();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(attachments.sizes_set - before, 1, "one change");
        assert_eq!(
            crate::process::terminal::window_size(master.as_raw_fd()),
            Some((209, 239))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A holder's hold: the descriptor passed with it is held by its label
    /// (the same file), answered `Done`; a release lets go of it.
    #[test]
    fn a_holder_holds_and_releases() {
        let (mut attachments, dir) =
            listening("hold", Some(super::super::holds::Holds::new(Vec::new())));
        let (stream, mut decoder, welcome) = holder(&mut attachments, &dir, VERSION);
        assert!(
            matches!(
                welcome,
                Some(Frame::Welcome {
                    version: HOLD_VERSION,
                    ..
                })
            ),
            "{welcome:?}"
        );
        let given = std::fs::File::open(&dir).unwrap();
        send(
            &stream,
            &Frame::Hold {
                label: "lease:app".to_owned(),
            },
            Some(given.as_fd()),
        );
        assert_eq!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Done {
                ok: true,
                why: String::new()
            })
        );
        let held = attachments.take_holds();
        assert_eq!(held.labels(), "lease:app");
        let same = |fd: &OwnedFd| {
            nix::sys::stat::fstat(fd).unwrap().st_ino == given.metadata().unwrap().ino()
        };
        assert!(held.fds().all(same));
        attachments.serve_holds(held);
        let (stream, mut decoder, _) = holder(&mut attachments, &dir, VERSION);
        send(
            &stream,
            &Frame::Release {
                label: "lease:app".to_owned(),
            },
            None,
        );
        assert_eq!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Done {
                ok: true,
                why: String::new()
            })
        );
        assert_eq!(attachments.take_holds().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Refused: a hold with no descriptor, one before the monitor's holds
    /// are here or once it ends, a release of nothing; a holder of an older
    /// version isn't let in.
    #[test]
    fn what_a_holder_cant_do() {
        let (mut attachments, dir) = listening("refused", None);
        let refused = |attachments: &mut Attachments, frame: &Frame, fd: Option<BorrowedFd<'_>>| {
            let (stream, mut decoder, _) = holder(attachments, &dir, VERSION);
            send(&stream, frame, fd);
            next(attachments, &stream, &mut decoder)
        };
        let hold = Frame::Hold {
            label: "lease:app".to_owned(),
        };
        let dev_null = file();
        // Not handed its holds yet: none taken.
        assert!(matches!(
            refused(&mut attachments, &hold, Some(dev_null.as_fd())),
            Some(Frame::Done { ok: false, .. })
        ));
        attachments.serve_holds(super::super::holds::Holds::new(Vec::new()));
        assert!(matches!(
            refused(&mut attachments, &hold, None),
            Some(Frame::Done { ok: false, why }) if why.contains("no descriptor")
        ));
        assert!(matches!(
            refused(&mut attachments, &Frame::Release { label: "lease:x".to_owned() }, None),
            Some(Frame::Done { ok: false, why }) if why.contains("nothing is held as lease:x")
        ));
        attachments.stop_taking_holds();
        assert!(matches!(
            refused(&mut attachments, &hold, Some(dev_null.as_fd())),
            Some(Frame::Done { ok: false, why }) if why.contains("starting or ending")
        ));
        assert_eq!(attachments.take_holds().len(), 0);
        let (_stream, _decoder, welcome) = holder(&mut attachments, &dir, 2);
        assert!(matches!(welcome, Some(Frame::Refused(_))), "{welcome:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether `stream`'s connection is let go of (end-of-file) within two
    /// seconds, `attachments` served meanwhile (what's sent to it read and
    /// dropped).
    fn let_go(attachments: &mut Attachments, stream: &UnixStream) -> bool {
        use std::io::Read as _;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buffer = [0_u8; 1024];
        while Instant::now() < deadline {
            attachments.service();
            match (&*stream).read(&mut buffer) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return true,
            }
        }
        false
    }

    /// A descriptor from anyone but a holder (an attached client typing
    /// here, which would otherwise stay), or two at once, is closed, and
    /// its client let go of.
    #[test]
    fn a_descriptor_from_anyone_else_is_refused() {
        let (mut attachments, dir) =
            listening("waiter", Some(super::super::holds::Holds::new(Vec::new())));
        let dev_null = file();
        // Typing, plain: it stays.
        let stream = connect(&dir);
        let mut decoder = Decoder::new();
        send(
            &stream,
            &Frame::Hello {
                version: VERSION,
                detach_keys: Vec::new(),
            },
            None,
        );
        assert!(matches!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Welcome { .. })
        ));
        send(&stream, &Frame::Stdin(b"x".to_vec()), None);
        assert!(!let_go(&mut attachments, &stream), "a plain client stays");
        // Typing with a descriptor: let go of.
        let stream = connect(&dir);
        let mut decoder = Decoder::new();
        send(
            &stream,
            &Frame::Hello {
                version: VERSION,
                detach_keys: Vec::new(),
            },
            None,
        );
        assert!(matches!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Welcome { .. })
        ));
        send(
            &stream,
            &Frame::Stdin(b"x".to_vec()),
            Some(dev_null.as_fd()),
        );
        assert!(
            let_go(&mut attachments, &stream),
            "a descriptor from an attached client"
        );
        // A holder passing two in one message, or three (more than one
        // message's room once was: none may be left open here, the
        // monitor's process): let go of, nothing held.
        // (Files of this test's own: whether one is still open anywhere in
        // this process is told by its name, whatever other tests open.)
        let passed: Vec<std::path::PathBuf> =
            (0..5).map(|at| dir.join(format!("passed-{at}"))).collect();
        for (count, paths) in [(2, &passed[..2]), (3, &passed[2..])] {
            let given: Vec<std::fs::File> = paths
                .iter()
                .map(|path| std::fs::File::create(path).unwrap())
                .collect();
            let fds: Vec<BorrowedFd<'_>> = given.iter().map(AsFd::as_fd).collect();
            let (stream, _decoder, _) = holder(&mut attachments, &dir, VERSION);
            send_fds(
                &stream,
                &Frame::Hold {
                    label: "a".to_owned(),
                },
                &fds,
            );
            assert!(
                let_go(&mut attachments, &stream),
                "{count} descriptors at once"
            );
        }
        assert_eq!(attachments.take_holds().len(), 0);
        let still_open: Vec<std::path::PathBuf> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .filter(|target| passed.contains(target))
            .collect();
        assert!(still_open.is_empty(), "left open: {still_open:?}");
        // Only a file, a directory or a socket is held: a pipe's end isn't.
        attachments.serve_holds(super::super::holds::Holds::new(Vec::new()));
        let (reading, _writing) = nix::unistd::pipe().unwrap();
        let (stream, mut decoder, _) = holder(&mut attachments, &dir, VERSION);
        send(
            &stream,
            &Frame::Hold {
                label: "a".to_owned(),
            },
            Some(reading.as_fd()),
        );
        assert!(matches!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Done { ok: false, why }) if why.contains("only a file")
        ));
        // A release that brings a descriptor: refused.
        let (stream, mut decoder, _) = holder(&mut attachments, &dir, VERSION);
        send(
            &stream,
            &Frame::Release {
                label: "a".to_owned(),
            },
            Some(dev_null.as_fd()),
        );
        assert!(matches!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Done { ok: false, why }) if why.contains("came with the release")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stop's signal must end the init's command: a real signal (to the
    /// last real-time one), not one that stops or continues it, nor one the
    /// init can't pass on; its timeout is in seconds.
    #[test]
    fn a_stop_is_only_a_signal_that_ends() {
        assert_eq!(
            StopRequest::of(15, 10),
            Some(StopRequest {
                signal: libc::SIGTERM,
                timeout: Duration::from_secs(10),
            })
        );
        let number = |signal: i32| u8::try_from(signal).unwrap();
        assert!(StopRequest::of(number(libc::SIGKILL), 0).is_some());
        assert!(
            StopRequest::of(number(libc::SIGRTMAX()), 0).is_some(),
            "the last real-time one"
        );
        for refused in [
            0,
            libc::SIGSTOP,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGCONT,
            libc::SIGCHLD,
            32,
            33,
            libc::SIGRTMAX() + 1,
            255,
        ] {
            assert_eq!(StopRequest::of(number(refused), 10), None, "{refused}");
        }
    }
}
