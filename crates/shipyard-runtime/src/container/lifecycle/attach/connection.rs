//! An attach socket's client: what it's there for ([`Role`]), the frames
//! queued for it, and whether it's on its way out ([`Leaving`]).

use std::collections::VecDeque;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use nix::poll::PollFlags;

use crate::attach::{Decoder, Frame, Matcher};

use super::{HELD_WAIT, LEAVE_WAIT};

/// A client on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leaving {
    /// It isn't.
    No,
    /// Once its last frames are written, or [`LEAVE_WAIT`] after this.
    AfterFrames(Instant),
    /// Now.
    Now,
}

/// One connection to the attach socket, as the monitor keeps it: its
/// stream and decoder, what it said it is, and what is queued for it.
#[derive(Debug)]
pub(super) struct Client {
    pub(super) stream: UnixStream,
    pub(super) decoder: Decoder,
    /// Its detach keys, watched for in what it types; set by its hello.
    pub(super) matcher: Option<Matcher>,
    pub(super) welcomed: bool,
    pub(super) connected: Instant,
    /// Frames for it, whole; the first may be partly written (`offset`).
    pub(super) frames: VecDeque<Vec<u8>>,
    pub(super) offset: usize,
    pub(super) queued: usize,
    /// Whether it's on its way out.
    pub(super) leaving: Leaving,
    /// The first client to type (`--stdin-once` closes stdin when it goes,
    /// or its input ends).
    pub(super) typist: bool,
    /// What it's there for.
    pub(super) role: Role,
    /// Held ([`Frame::HelloHeld`]): never let go of for falling behind; the
    /// container waits for it instead.
    pub(super) held: bool,
    /// A descriptor a holder sent, for its [`Frame::Hold`].
    pub(super) sent: Option<OwnedFd>,
    /// The protocol version agreed with it (0 until it's welcomed): a
    /// holder's refusal says its reason from
    /// [`REASON_VERSION`](crate::attach::REASON_VERSION) on.
    pub(super) version: u32,
}

/// What a client is there for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    /// Attached: sent the output, and may type.
    Attach,
    /// A control client waiting for the end (`wait`): sent no output, takes
    /// no input.
    Waiter,
    /// A control client there to stop the container (`stop`): its one
    /// [`Frame::Stop`] is due within [`HELLO_WAIT`](super::HELLO_WAIT) of
    /// its connecting, and it goes after it.
    Stopper,
    /// A control client there to hold a descriptor, or let one go (version
    /// 3): its one [`Frame::Hold`] or [`Frame::Release`] is due within
    /// [`HELLO_WAIT`](super::HELLO_WAIT) of its connecting, and it goes once
    /// answered.
    Holder,
}

impl Role {
    /// Whether it's there for one request, due within
    /// [`HELLO_WAIT`](super::HELLO_WAIT).
    pub(super) fn is_brief(self) -> bool {
        matches!(self, Self::Stopper | Self::Holder)
    }
}

impl Client {
    pub(super) fn send(&mut self, frame: &Frame) {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        self.queued += bytes.len();
        self.frames.push_back(bytes);
    }

    /// Says `frame` and then closes, or (in the middle of a frame) just
    /// closes: a frame after a partly written one would be garbage.
    pub(super) fn finish_with(&mut self, frame: &Frame) {
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
    pub(super) fn write(&mut self) {
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
    pub(super) fn leave_wait(&self) -> Duration {
        if self.held { HELD_WAIT } else { LEAVE_WAIT }
    }

    /// Whether what it sends waits: it's held, and the container's input
    /// (`stdin_full`) has no room.
    pub(super) fn input_waits(&self, stdin_full: bool) -> bool {
        self.held && stdin_full
    }

    /// What to wait for on its socket: what it sends (unless that waits:
    /// `stdin_full`), and room for what it's sent, if anything. A client on
    /// its way out isn't read any more (its unread input would make it ready
    /// forever).
    pub(super) fn events(&self, stdin_full: bool) -> PollFlags {
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
