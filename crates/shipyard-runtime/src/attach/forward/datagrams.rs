//! A forwarded UDP port's stream, read and written without ever waiting
//! (13.8c): each side of it has datagrams of its own to send while the
//! other's come in, and a side that waited to write (or to read the rest of
//! a frame) while the other did the same would stall both, then reset the
//! stream, every peer's source port changing with it. So frames go out
//! through an [`Outbox`], written when the stream takes them (a full one
//! drops the new datagram, as UDP drops it), and come in through an
//! [`Inbox`], handed on only whole.
//!
//! The stream must be non-blocking (`O_NONBLOCK`, its own connection's):
//! macOS's `MSG_DONTWAIT` alone doesn't keep a UNIX socket's `send` from
//! waiting for room (13.8c's first run hung in `sendto`).

use std::os::fd::{AsFd, AsRawFd as _};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::socket::MsgFlags;

use super::{MAX_DATAGRAM, write_datagram};

/// The most bytes of frames an [`Outbox`] holds: past it, a datagram is
/// dropped.
pub const OUTBOX_LIMIT: usize = 256 * 1024;

/// How long a frame begun may stay unfinished in an [`Inbox`]: a peer that
/// takes longer has broken the stream.
pub const FRAME_WAIT: Duration = Duration::from_secs(5);

/// The most bytes an [`Inbox`] reads ahead: two whole frames of the most a
/// datagram holds.
const INBOX_LIMIT: usize = 2 * (HEAD + MAX_DATAGRAM);

/// A frame's head: the peer's ID (four bytes), the datagram's length (two).
const HEAD: usize = 6;

/// No wait (the stream's own `O_NONBLOCK` is what holds on macOS), and no
/// `SIGPIPE` where the flag exists (Linux; std ignores the signal in a
/// program of its own on macOS).
fn no_wait() -> MsgFlags {
    #[cfg(target_os = "linux")]
    {
        MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL
    }
    #[cfg(not(target_os = "linux"))]
    {
        MsgFlags::MSG_DONTWAIT
    }
}

/// Frames waiting for the stream to take them, whole, in order.
#[derive(Debug, Default)]
pub struct Outbox {
    bytes: Vec<u8>,
    sent: usize,
    /// Since when bytes have waited with none taken.
    waiting: Option<Instant>,
}

impl Outbox {
    /// Queues `datagram` of the peer `peer`: `false` if it's dropped (past
    /// [`OUTBOX_LIMIT`], or past [`MAX_DATAGRAM`]).
    pub fn push(&mut self, peer: u32, datagram: &[u8]) -> bool {
        if self.bytes.len() - self.sent + HEAD + datagram.len() > OUTBOX_LIMIT {
            return false;
        }
        if self.sent > 0 && self.sent * 2 >= self.bytes.len() {
            self.bytes.drain(..self.sent);
            self.sent = 0;
        }
        write_datagram(&mut self.bytes, peer, datagram).is_ok()
    }

    /// Writes what `stream` (non-blocking) takes now, waiting for nothing: a
    /// part of a frame written, the rest goes at the next.
    ///
    /// # Errors
    ///
    /// The stream's, but that it's full.
    pub fn flush(&mut self, stream: impl AsFd) -> std::io::Result<()> {
        let fd = stream.as_fd().as_raw_fd();
        let before = self.sent;
        while self.sent < self.bytes.len() {
            match nix::sys::socket::send(fd, &self.bytes[self.sent..], no_wait()) {
                Ok(written) => self.sent += written,
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
        if self.sent == self.bytes.len() {
            self.bytes.clear();
            self.sent = 0;
            self.waiting = None;
        } else if self.sent > before || self.waiting.is_none() {
            self.waiting = Some(Instant::now());
        }
        Ok(())
    }

    /// Whether bytes have waited past [`FRAME_WAIT`] with none taken: the
    /// stream's other side has stopped reading.
    #[must_use]
    pub fn stalled(&self) -> bool {
        self.waiting
            .is_some_and(|since| since.elapsed() >= FRAME_WAIT)
    }

    /// Whether everything's been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sent == self.bytes.len()
    }
}

/// Frames read from a stream as they come, handed on whole.
#[derive(Debug, Default)]
pub struct Inbox {
    bytes: Vec<u8>,
    read: usize,
    /// When the frame at the front was begun, while it's unfinished.
    begun: Option<Instant>,
    ended: bool,
}

impl Inbox {
    /// One holding `bytes` already (those read past the stream's welcome).
    #[must_use]
    pub fn with(bytes: &[u8]) -> Self {
        let mut inbox = Self::default();
        inbox.feed(bytes);
        inbox
    }

    /// Takes `bytes` as read from the stream.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        self.mark();
    }

    /// Reads what `stream` (non-blocking) has now, waiting for nothing, up to
    /// what the inbox holds ahead.
    ///
    /// # Errors
    ///
    /// The stream's, but that it has nothing now.
    pub fn fill(&mut self, stream: impl AsFd) -> std::io::Result<()> {
        let fd = stream.as_fd().as_raw_fd();
        if self.read > 0 {
            self.bytes.drain(..self.read);
            self.read = 0;
        }
        let mut chunk = [0u8; 16 * 1024];
        while !self.ended && self.bytes.len() < INBOX_LIMIT {
            let room = (INBOX_LIMIT - self.bytes.len()).min(chunk.len());
            match nix::sys::socket::recv(fd, &mut chunk[..room], no_wait()) {
                Ok(0) => self.ended = true,
                Ok(got) => self.bytes.extend_from_slice(&chunk[..got]),
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
        self.mark();
        Ok(())
    }

    /// The next whole frame: its datagram in `buffer`, its peer's ID
    /// returned; `None` until one is whole.
    pub fn next(&mut self, buffer: &mut Vec<u8>) -> Option<u32> {
        let (peer, length) = self.whole()?;
        let start = self.read + HEAD;
        buffer.clear();
        buffer.extend_from_slice(&self.bytes[start..start + length]);
        self.read = start + length;
        self.begun = None;
        self.mark();
        Some(peer)
    }

    /// Whether a whole frame is there to be taken.
    #[must_use]
    pub fn has_frame(&self) -> bool {
        self.whole().is_some()
    }

    /// Whether the stream has ended and every whole frame's been taken
    /// (what's left of a frame cut short goes with it).
    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended && !self.has_frame()
    }

    /// Whether a frame begun has stayed unfinished past [`FRAME_WAIT`].
    #[must_use]
    pub fn stuck(&self) -> bool {
        !self.has_frame()
            && self
                .begun
                .is_some_and(|begun| begun.elapsed() >= FRAME_WAIT)
    }

    /// The frame at the front's peer and length, if it's whole.
    fn whole(&self) -> Option<(u32, usize)> {
        let rest = self.bytes.get(self.read..)?;
        let [a, b, c, d, high, low] = *rest.first_chunk::<HEAD>()?;
        let length = usize::from(u16::from_be_bytes([high, low]));
        (rest.len() >= HEAD + length).then_some((u32::from_be_bytes([a, b, c, d]), length))
    }

    /// Notes when the frame at the front was begun, while it's unfinished.
    fn mark(&mut self) {
        if self.read == self.bytes.len() || self.has_frame() {
            self.begun = None;
        } else if self.begun.is_none() {
            self.begun = Some(Instant::now());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A frame fed a byte at a time comes out whole, once, and only then;
    /// frames come out in order; a stream's end with a frame cut short is
    /// an end.
    #[test]
    fn frames_come_out_whole_and_in_order() {
        let mut bytes = Vec::new();
        write_datagram(&mut bytes, 7, b"one").unwrap();
        write_datagram(&mut bytes, 8, b"").unwrap();
        write_datagram(&mut bytes, 9, &[5; 300]).unwrap();
        let mut inbox = Inbox::default();
        let mut buffer = Vec::new();
        let mut got = Vec::new();
        for byte in &bytes {
            inbox.feed(std::slice::from_ref(byte));
            while let Some(peer) = inbox.next(&mut buffer) {
                got.push((peer, buffer.clone()));
            }
        }
        assert_eq!(
            got,
            [(7, b"one".to_vec()), (8, Vec::new()), (9, vec![5; 300])]
        );
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let mut cut = Vec::new();
        write_datagram(&mut cut, 1, b"cut short").unwrap();
        std::io::Write::write_all(&mut &theirs, &cut[..5]).unwrap();
        drop(theirs);
        let mut inbox = Inbox::default();
        inbox.fill(&ours).unwrap();
        assert!(inbox.next(&mut buffer).is_none());
        assert!(inbox.ended());
    }

    /// An outbox writes what the stream takes and keeps the rest, a frame
    /// cut where the stream filled finished at the next flush; past its
    /// limit a datagram's dropped, never one queued; nothing's reordered.
    #[test]
    fn an_outbox_never_waits_and_loses_no_frame_it_took() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        theirs.set_nonblocking(true).unwrap();
        let mut outbox = Outbox::default();
        let mut taken = 0u32;
        while outbox.push(taken, &[taken.to_be_bytes()[3]; 1000]) {
            taken += 1;
        }
        assert!(taken > 200, "{taken}");
        // The stream fills long before: the flush returns, the rest waits.
        outbox.flush(&ours).unwrap();
        assert!(!outbox.is_empty());
        let mut inbox = Inbox::default();
        let mut buffer = Vec::new();
        let mut next = 0;
        while next < taken {
            inbox.fill(&theirs).unwrap();
            while let Some(peer) = inbox.next(&mut buffer) {
                assert_eq!(peer, next);
                assert_eq!(buffer, [next.to_be_bytes()[3]; 1000]);
                next += 1;
            }
            outbox.flush(&ours).unwrap();
        }
        assert!(outbox.is_empty());
        assert!(!inbox.stuck());
        assert!(!outbox.stalled());
    }

    /// An outbox whose stream takes nothing is stalled once [`FRAME_WAIT`]
    /// has passed since it last took something, not before; one that's
    /// emptied isn't.
    #[test]
    fn an_outbox_no_one_reads_is_stalled_in_time() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let mut outbox = Outbox::default();
        while outbox.push(1, &[0; 1000]) {}
        outbox.flush(&ours).unwrap();
        outbox.flush(&ours).unwrap();
        assert!(!outbox.is_empty() && !outbox.stalled());
        outbox.waiting = Some(Instant::now().checked_sub(FRAME_WAIT).unwrap());
        outbox.flush(&ours).unwrap();
        assert!(outbox.stalled(), "nothing taken since");
        theirs.set_nonblocking(true).unwrap();
        let mut inbox = Inbox::default();
        while !outbox.is_empty() {
            inbox.fill(&theirs).unwrap();
            while inbox.next(&mut Vec::new()).is_some() {}
            outbox.flush(&ours).unwrap();
            assert!(!outbox.stalled(), "taken again");
        }
    }

    /// A frame begun and left unfinished is stuck once [`FRAME_WAIT`] has
    /// passed, not before; a whole one never is.
    #[test]
    fn an_unfinished_frame_is_stuck_in_time() {
        let mut bytes = Vec::new();
        write_datagram(&mut bytes, 1, b"whole").unwrap();
        let mut inbox = Inbox::with(&bytes[..4]);
        assert!(!inbox.stuck());
        inbox.begun = Some(Instant::now().checked_sub(FRAME_WAIT).unwrap());
        assert!(inbox.stuck());
        inbox.feed(&bytes[4..]);
        assert!(!inbox.stuck(), "whole now");
        assert_eq!(inbox.next(&mut Vec::new()), Some(1));
        assert!(!inbox.stuck());
    }
}
