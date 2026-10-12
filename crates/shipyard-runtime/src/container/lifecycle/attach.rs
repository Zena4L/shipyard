//! The monitor's side of attach (stage 8, step 8.3c): `attach.sock` in the
//! state directory, its clients, and the container's stdin.
//!
//! **Who may attach:** the socket file is 0600 in a 0700 directory, and each
//! connection's peer credentials (`SO_PEERCRED`, TLPI §61.13: the kernel
//! says who connected, the client can't lie) must be this monitor's user or
//! root: anyone else is closed at once.
//!
//! **Nothing a client does slows the container.** Output goes to each client
//! through a queue of frames, at most [`CLIENT_BUFFER`]: a client that falls
//! further behind is dropped (told so, between frames), never waited for.
//! What clients type goes to the container's stdin through a queue of at
//! most [`STDIN_QUEUE`]; typing beyond it (a container not reading as fast)
//! is discarded, but every client's frames are still read, so its detach
//! keys and resizes always arrive.
//!
//! At most [`MAX_CLIENTS`] clients are let in, and as many may be waiting to
//! say hello; a connection that hasn't within [`HELLO_WAIT`] is closed. A
//! client on its way out gets [`LEAVE_WAIT`] to take its last frames.
//!
//! **`--stdin-once`:** the first client to type (or to say its input ended)
//! is the container's typist; when it goes, or its input ends, the
//! container's stdin is closed once what it typed has been written.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags};

use crate::attach::{Decoder, DetachKeys, Frame, Matcher, negotiate};
use crate::logs::cri::Stream;

/// The socket's name in the state directory.
pub(in crate::container) const SOCKET: &str = "attach.sock";

/// The most clients attached at once (the user's decision, 8.3c).
const MAX_CLIENTS: usize = 16;

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

    /// What to wait for on its socket: what it sends, and room for what it's
    /// sent, if anything. A client on its way out isn't read any more (its
    /// unread input would make it ready forever).
    fn events(&self) -> PollFlags {
        let mut events = PollFlags::empty();
        if self.leaving == Leaving::No {
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
        let path = format!("/proc/self/fd/{}/{SOCKET}", dir.as_raw_fd());
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
            takes_stdin,
            stdin,
            ended: false,
            resting: None,
            read: vec![0; 64 * 1024],
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
        for client in &self.clients {
            fds.push(PollFd::new(client.stream.as_fd(), client.events()));
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
                Leaving::AfterFrames(since) => Some(LEAVE_WAIT.saturating_sub(since.elapsed())),
                _ if !client.welcomed => {
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

    /// Output from the container, for every client let in.
    pub(super) fn output(&mut self, stream: Stream, bytes: &[u8]) {
        for client in self
            .clients
            .iter_mut()
            .filter(|client| client.welcomed && client.leaving == Leaving::No)
        {
            let mut frames = Vec::new();
            Frame::encode_output(stream == Stream::Stderr, bytes, &mut frames);
            client.queued += frames.len();
            client.frames.push_back(frames);
            if client.queued > CLIENT_BUFFER {
                client.finish_with(&Frame::Dropped(format!(
                    "more than {} KiB of output behind",
                    CLIENT_BUFFER / 1024
                )));
            }
        }
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
                Leaving::AfterFrames(since) => now.duration_since(since) >= LEAVE_WAIT,
                _ => !client.welcomed && now.duration_since(client.connected) >= HELLO_WAIT,
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
                    "{MAX_CLIENTS} connections are waiting to attach already"
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
            });
        }
    }

    fn read_client(&mut self, at: usize) {
        for _ in 0..READS_PER_PASS {
            let Some(client) = self.clients.get_mut(at) else {
                return;
            };
            if client.leaving != Leaving::No {
                return;
            }
            match client.stream.read(&mut self.read) {
                Ok(0) => {
                    client.leaving = Leaving::Now;
                    return;
                }
                Ok(read) => client
                    .decoder
                    .push(self.read.get(..read).unwrap_or_default()),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return,
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
        let welcomed_count = self.clients.iter().filter(|client| client.welcomed).count();
        let (terminal, takes_stdin, ended) = (self.terminal, self.takes_stdin, self.ended);
        let Some(client) = self.clients.get_mut(at) else {
            return;
        };
        match (client.welcomed, frame) {
            (
                false,
                Frame::Hello {
                    version,
                    detach_keys,
                },
            ) => {
                let refused = if ended {
                    Some("the container has ended".to_owned())
                } else if welcomed_count >= MAX_CLIENTS {
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
                    let room = STDIN_QUEUE.saturating_sub(stdin.queue.len());
                    // Beyond the queue (a container not reading): dropped.
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
            (true, Frame::Resize { rows, cols }) if terminal => match &self.master {
                Some(master) => {
                    let _ =
                        crate::process::terminal::set_window_size(master.as_raw_fd(), (rows, cols));
                }
                None => self.size = Some((rows, cols)),
            },
            (true, Frame::Resize { .. }) => {}
            // Anything else, or a hello twice: not the protocol.
            _ => client.leaving = Leaving::Now,
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
        let deadline = Instant::now() + LAST_FLUSH;
        while !self.clients.is_empty() && Instant::now() < deadline {
            self.service();
            let mut fds = self.poll_fds();
            let _ = nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(50_u16));
        }
        self.clients.clear();
    }
}
