//! A request and its answer: what a netlink socket does with the datagrams
//! it sends and receives, apart from the socket itself.
//!
//! The rules live here, over a [`Transport`] (the socket on Linux; a script
//! of the kernel's side in the tests): each request gets a fresh sequence
//! number; only the kernel's messages for that number count; a request ends
//! at its acknowledgement or error, a dump at `DONE`; a dump the kernel flags
//! as interrupted is redone; a datagram cut short, a timeout and an overrun
//! are errors, never a silently shorter answer.
//!
//! A *batch* (nfnetlink's, for `nf_tables`) is several requests in one
//! datagram between a `BEGIN` and an `END` marker: the kernel applies them
//! all, or (if any fails) none, in one transaction.

use nix::errno::Errno;
use thiserror::Error;

use super::message::{
    Ack, BuildError, Message, MessageBuilder, Messages, ParseError, flag, msg_type,
};
use super::reply::{Reply, classify};

/// How many times a dump is redone when the table changed under it.
pub(crate) const DUMP_TRIES: u32 = 3;

/// How long a receive waits for the kernel, in seconds. It bounds a lost
/// answer, not a request: the kernel does a request's work inside `sendto`.
pub(crate) const TIMEOUT_SECS: u64 = 5;

/// The largest datagram the kernel sends: a dump's parts are at most 32 KiB.
pub(crate) const RECEIVE_BUFFER: usize = 64 * 1024;

/// nfnetlink's batch markers (`NFNL_MSG_BATCH_BEGIN`, `NFNL_MSG_BATCH_END`).
pub(crate) const BATCH_BEGIN: u16 = 0x10;
pub(crate) const BATCH_END: u16 = 0x11;

/// The header every nfnetlink message starts with (`nfgenmsg`): the address
/// family the message is about, the protocol version (0), and a resource ID
/// (big-endian; a batch marker's is the subsystem the batch is for).
#[must_use]
pub(crate) fn nfgenmsg(family: u8, resource: u16) -> [u8; NFGENMSG_LEN] {
    let [high, low] = resource.to_be_bytes();
    [family, 0, high, low]
}

/// The size of `nfgenmsg`.
pub(crate) const NFGENMSG_LEN: usize = 4;

/// An nfnetlink message: type `msg` of `subsystem` (`NFNL_SUBSYS_*`), about
/// `family`, with its `nfgenmsg`.
pub(crate) fn nfnl_message(subsystem: u16, msg: u16, flags: u16, family: u8) -> MessageBuilder {
    let mut message = MessageBuilder::new((subsystem << 8) | msg, flags);
    message.raw(&nfgenmsg(family, 0));
    message
}

/// One request of a batch, and what it does (for its error).
#[derive(Debug, Clone)]
pub(crate) struct BatchItem {
    /// What it does: "adding the chain forward".
    pub(crate) what: String,
    /// The request (`REQUEST` and `ACK` are added).
    pub(crate) message: MessageBuilder,
}

/// What went wrong talking to the kernel. `operation` says what was being done
/// ("creating the bridge shipyard0").
#[derive(Debug, Error)]
pub enum NetlinkError {
    /// The kernel refused the request.
    #[error("{operation}: {}{}", errno.desc(), match reason {
        Some(reason) => format!(" (the kernel says: {reason})"),
        None => String::new(),
    })]
    Refused {
        /// What was being done.
        operation: String,
        /// Why, as an errno.
        errno: Errno,
        /// The kernel's own words (its extended ACK), where it gave them.
        reason: Option<String>,
    },
    /// A system call on the socket failed.
    #[error("{operation}: {}", source.desc())]
    Socket {
        /// What was being done.
        operation: String,
        /// The error.
        source: Errno,
    },
    /// The kernel's answer didn't come in time.
    #[error("{operation}: no answer from the kernel within {TIMEOUT_SECS} s")]
    Timeout {
        /// What was being done.
        operation: String,
    },
    /// The kernel's answer couldn't be read.
    #[error("{operation}: {source}")]
    Parse {
        /// What was being done.
        operation: String,
        /// What was wrong with it.
        source: ParseError,
    },
    /// The request couldn't be built.
    #[error("{operation}: {source}")]
    Build {
        /// What was being done.
        operation: String,
        /// Why.
        source: BuildError,
    },
    /// A datagram was longer than the buffer: part of it would be lost.
    #[error("{operation}: the kernel's answer was longer than {RECEIVE_BUFFER} bytes")]
    Truncated {
        /// What was being done.
        operation: String,
    },
    /// The kernel dropped messages for the socket (its buffer overran).
    #[error("{operation}: the kernel dropped part of its answer")]
    Overrun {
        /// What was being done.
        operation: String,
    },
    /// An earlier answer on this socket broke off (a timeout, lost or
    /// unreadable messages): what it receives next could be the rest of that
    /// answer, so it's not used again.
    #[error("{operation}: an earlier answer on this netlink socket broke off, so it can't be used")]
    Lost {
        /// What was being done.
        operation: String,
    },
    /// What was being listed kept changing while it was listed.
    #[error("{operation}: it changed while it was being listed, {DUMP_TRIES} times in a row")]
    Interrupted {
        /// What was being done.
        operation: String,
    },
}

impl NetlinkError {
    /// The errno the kernel refused with (or a socket call failed with).
    #[must_use]
    pub fn errno(&self) -> Option<Errno> {
        match self {
            NetlinkError::Refused { errno, .. } => Some(*errno),
            NetlinkError::Socket { source, .. } => Some(*source),
            _ => None,
        }
    }

    pub(crate) fn socket(operation: &str, source: Errno) -> Self {
        NetlinkError::Socket {
            operation: operation.to_owned(),
            source,
        }
    }

    fn parse(operation: &str, source: ParseError) -> Self {
        NetlinkError::Parse {
            operation: operation.to_owned(),
            source,
        }
    }

    pub(crate) fn refused(operation: &str, errno: i32, reason: Option<String>) -> Self {
        NetlinkError::Refused {
            operation: operation.to_owned(),
            errno: Errno::from_raw(errno),
            reason,
        }
    }
}

/// Where datagrams go and come from.
pub(crate) trait Transport {
    /// Sends one datagram to the kernel.
    fn send(&mut self, bytes: &[u8]) -> Result<(), Errno>;

    /// Receives one datagram into `buffer`: its real length (which may be
    /// more than the buffer held: `MSG_TRUNC`) and the sender's port ID.
    fn receive(&mut self, buffer: &mut [u8]) -> Result<(usize, u32), Errno>;

    /// Like [`receive`](Self::receive), but EAGAIN at once if nothing is
    /// waiting.
    fn receive_now(&mut self, buffer: &mut [u8]) -> Result<(usize, u32), Errno>;
}

/// How an answer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// With an acknowledgement.
    Acked,
    /// With a dump's `DONE`; `interrupted` if the dump was inconsistent.
    Done { interrupted: bool },
}

/// What one datagram did to an answer being collected.
enum Scanned {
    /// The answer goes on in the next datagram.
    More,
    /// The answer is complete.
    Ended(Result<End, NetlinkError>),
    /// The answer can't be followed to its end (a message that can't be
    /// read, lost messages): this socket can't tell the next answer from the
    /// rest of this one.
    Lost(NetlinkError),
}

/// Requests and their answers over a transport.
#[derive(Debug)]
pub(crate) struct Conversation<T> {
    pub(crate) transport: T,
    /// This end's port ID.
    pub(crate) port: u32,
    /// The last sequence number used.
    seq: u32,
    buffer: Vec<u8>,
    /// An earlier answer broke off: what arrives next may be the rest of it.
    lost: bool,
}

impl<T: Transport> Conversation<T> {
    pub(crate) fn new(transport: T, port: u32) -> Self {
        Conversation {
            transport,
            port,
            seq: 0,
            buffer: vec![0; RECEIVE_BUFFER],
            lost: false,
        }
    }

    /// Sends `message`, passes each object the kernel answers with to `each`,
    /// and waits for the acknowledgement.
    pub(crate) fn exchange(
        &mut self,
        operation: &str,
        message: &MessageBuilder,
        each: impl FnMut(&Message<'_>) -> Result<(), ParseError>,
    ) -> Result<(), NetlinkError> {
        let mut message = message.clone();
        message.add_flags(flag::REQUEST | flag::ACK);
        let seq = self.send(operation, &message)?;
        self.collect(operation, seq, each).map(drop)
    }

    /// Sends `message` as a dump request and returns what `parse` makes of
    /// each object the kernel lists (`None` skips one), redoing a dump the
    /// kernel flags as interrupted (on any part, or on its end).
    pub(crate) fn dump<V>(
        &mut self,
        operation: &str,
        message: &MessageBuilder,
        mut parse: impl FnMut(&Message<'_>) -> Result<Option<V>, ParseError>,
    ) -> Result<Vec<V>, NetlinkError> {
        let mut message = message.clone();
        message.add_flags(flag::REQUEST | flag::DUMP);
        for _ in 0..DUMP_TRIES {
            let mut found = Vec::new();
            let mut interrupted = false;
            let seq = self.send(operation, &message)?;
            let end = self.collect(operation, seq, |item| {
                interrupted |= item.flags & flag::DUMP_INTR != 0;
                found.extend(parse(item)?);
                Ok(())
            })?;
            if !interrupted && end == (End::Done { interrupted: false }) {
                return Ok(found);
            }
        }
        Err(NetlinkError::Interrupted {
            operation: operation.to_owned(),
        })
    }

    /// Sends `items` as one nfnetlink batch for `subsystem`, and waits for
    /// every item's acknowledgement: the kernel applied them all. If it
    /// refused one, it applied none, and the first refusal is returned,
    /// named by its item.
    pub(crate) fn batch(
        &mut self,
        operation: &str,
        subsystem: u16,
        items: &[BatchItem],
    ) -> Result<(), NetlinkError> {
        self.usable(operation)?;
        if items.is_empty() {
            return Ok(());
        }
        let build = |source| build_error(operation, source);
        let marker = |kind| {
            let mut marker = MessageBuilder::new(kind, flag::REQUEST);
            marker.raw(&nfgenmsg(0, subsystem));
            marker
        };
        // BEGIN, the items, END: consecutive sequence numbers from `first`.
        let first = self.seq.wrapping_add(1);
        let mut seq = first;
        let mut bytes = marker(BATCH_BEGIN).finish(seq, self.port).map_err(build)?;
        for item in items {
            seq = seq.wrapping_add(1);
            let mut message = item.message.clone();
            message.add_flags(flag::REQUEST | flag::ACK);
            bytes.extend(message.finish(seq, self.port).map_err(build)?);
        }
        seq = seq.wrapping_add(1);
        bytes.extend(marker(BATCH_END).finish(seq, self.port).map_err(build)?);
        self.seq = seq;
        self.send_bytes(operation, &bytes)?;
        self.collect_batch(operation, first, items)
    }

    /// Receives the answers to the batch whose `BEGIN` was `first`: one per
    /// item (an acknowledgement, or a refusal), or one refusing the batch
    /// itself (on `first`). The kernel does a batch's work inside the send,
    /// so once it has refused an item, whatever else it has to say is
    /// already queued: from then on, an empty socket is the end.
    fn collect_batch(
        &mut self,
        operation: &str,
        first: u32,
        items: &[BatchItem],
    ) -> Result<(), NetlinkError> {
        let mut answered = vec![false; items.len()];
        let mut refused: Option<NetlinkError> = None;
        loop {
            if answered.iter().all(|&done| done) {
                return refused.map_or(Ok(()), Err);
            }
            let (len, sender) = match self.receive(operation, refused.is_some()) {
                Ok(received) => received,
                // Nothing more is coming: the kernel stopped at the refusal.
                Err(NetlinkError::Timeout { .. }) if refused.is_some() => {
                    return refused.map_or(Ok(()), Err);
                }
                Err(error) => {
                    self.lost = true;
                    // The refusal is what matters, if there was one.
                    return Err(refused.unwrap_or(error));
                }
            };
            if sender != 0 {
                continue;
            }
            let datagram = self.buffer.get(..len).unwrap_or_default();
            for message in Messages::new(datagram) {
                let answer = message
                    .map_err(|error| NetlinkError::parse(operation, error))
                    .and_then(|message| {
                        batch_answer(&message, operation, first, items.len(), self.port)
                    });
                match answer {
                    Err(error) => {
                        self.lost = true;
                        return Err(error);
                    }
                    Ok(None) => {}
                    // The batch itself was refused (no permission, no
                    // nf_tables): nothing in it was looked at.
                    Ok(Some(BatchAnswer::Batch(ack))) => {
                        if ack.errno != 0 {
                            return Err(NetlinkError::refused(operation, ack.errno, ack.reason));
                        }
                    }
                    Ok(Some(BatchAnswer::Item(index, ack))) => {
                        if let Some(done) = answered.get_mut(index) {
                            *done = true;
                        }
                        if ack.errno != 0
                            && refused.is_none()
                            && let Some(item) = items.get(index)
                        {
                            let what = format!("{operation}: {}", item.what);
                            refused = Some(NetlinkError::refused(&what, ack.errno, ack.reason));
                        }
                    }
                }
            }
        }
    }

    /// [`NetlinkError::Lost`] if an earlier answer broke off.
    fn usable(&self, operation: &str) -> Result<(), NetlinkError> {
        if self.lost {
            return Err(NetlinkError::Lost {
                operation: operation.to_owned(),
            });
        }
        Ok(())
    }

    /// Receives the answer to request `seq` up to its end, passing each
    /// object to `each`. If `each` fails, the rest of the answer is still
    /// read (and dropped) to its end: the kernel only goes on with a dump as
    /// the socket is read, and refuses another (EBUSY) until it's finished.
    fn collect(
        &mut self,
        operation: &str,
        seq: u32,
        mut each: impl FnMut(&Message<'_>) -> Result<(), ParseError>,
    ) -> Result<End, NetlinkError> {
        self.usable(operation)?;
        let mut failed = None;
        loop {
            let scanned = match self.receive(operation, false) {
                // Only the kernel answers; another process that learned the
                // port ID must not be able to forge, or break, an answer.
                Ok((_, sender)) if sender != 0 => Scanned::More,
                Ok((len, _)) => {
                    let datagram = self.buffer.get(..len).unwrap_or_default();
                    scan(datagram, operation, seq, self.port, &mut each, &mut failed)
                }
                Err(error) => Scanned::Lost(error),
            };
            match scanned {
                Scanned::More => {}
                Scanned::Ended(end) => return end,
                Scanned::Lost(error) => {
                    self.lost = true;
                    return Err(error);
                }
            }
        }
    }

    /// Sends `message` with the next sequence number; returns that number.
    fn send(&mut self, operation: &str, message: &MessageBuilder) -> Result<u32, NetlinkError> {
        self.usable(operation)?;
        self.seq = self.seq.wrapping_add(1);
        let bytes = message
            .finish(self.seq, self.port)
            .map_err(|source| build_error(operation, source))?;
        self.send_bytes(operation, &bytes)?;
        Ok(self.seq)
    }

    /// Sends one datagram.
    fn send_bytes(&mut self, operation: &str, bytes: &[u8]) -> Result<(), NetlinkError> {
        loop {
            match self.transport.send(bytes) {
                Ok(()) => return Ok(()),
                Err(Errno::EINTR) => {}
                Err(error) => return Err(NetlinkError::socket(operation, error)),
            }
        }
    }

    /// Receives one datagram into the buffer: its length and the sender's
    /// port ID. `now`: only one that's already waiting (else a timeout at
    /// once).
    fn receive(&mut self, operation: &str, now: bool) -> Result<(usize, u32), NetlinkError> {
        loop {
            let received = if now {
                self.transport.receive_now(&mut self.buffer)
            } else {
                self.transport.receive(&mut self.buffer)
            };
            match received {
                Ok((len, _)) if len > self.buffer.len() => {
                    return Err(NetlinkError::Truncated {
                        operation: operation.to_owned(),
                    });
                }
                Ok(received) => return Ok(received),
                // A signal: the wait starts again (so a stream of signals
                // can stretch it).
                Err(Errno::EINTR) => {}
                // The receive timeout (`SO_RCVTIMEO`) ran out.
                Err(Errno::EAGAIN) => {
                    return Err(NetlinkError::Timeout {
                        operation: operation.to_owned(),
                    });
                }
                Err(Errno::ENOBUFS) => {
                    return Err(NetlinkError::Overrun {
                        operation: operation.to_owned(),
                    });
                }
                Err(error) => return Err(NetlinkError::socket(operation, error)),
            }
        }
    }
}

/// A request that couldn't be built.
fn build_error(operation: &str, source: BuildError) -> NetlinkError {
    NetlinkError::Build {
        operation: operation.to_owned(),
        source,
    }
}

/// The kernel's answer to (part of) a batch.
enum BatchAnswer {
    /// To its `BEGIN`: about the batch as a whole.
    Batch(Ack),
    /// To its item `index`.
    Item(usize, Ack),
}

/// What `message` says about the batch whose `BEGIN` was `first`, of
/// `count` items, sent from `port`: `None` if it isn't about it (or isn't an
/// acknowledgement). An overrun, or an answer that can't be read, is an
/// error: where the answers end can't be known any more.
fn batch_answer(
    message: &Message<'_>,
    operation: &str,
    first: u32,
    count: usize,
    port: u32,
) -> Result<Option<BatchAnswer>, NetlinkError> {
    let Ok(offset) = usize::try_from(message.seq.wrapping_sub(first)) else {
        return Ok(None);
    };
    if message.port != port || offset > count {
        return Ok(None);
    }
    let ack = match message.kind {
        msg_type::ERROR => message
            .error()
            .map_err(|error| NetlinkError::parse(operation, error))?,
        msg_type::OVERRUN => {
            return Err(NetlinkError::Overrun {
                operation: operation.to_owned(),
            });
        }
        _ => return Ok(None),
    };
    Ok(Some(match offset.checked_sub(1) {
        None => BatchAnswer::Batch(ack),
        Some(index) => BatchAnswer::Item(index, ack),
    }))
}

/// What the kernel's `datagram` does to the answer to request `seq`: each
/// object goes to `each` until it fails (the first failure is kept in
/// `failed`, and reported when the answer ends).
fn scan(
    datagram: &[u8],
    operation: &str,
    seq: u32,
    port: u32,
    each: &mut impl FnMut(&Message<'_>) -> Result<(), ParseError>,
    failed: &mut Option<NetlinkError>,
) -> Scanned {
    for message in Messages::new(datagram) {
        let reply = match message.and_then(|message| classify(message, seq, port)) {
            Ok(reply) => reply,
            // Where this answer ends can't be known any more.
            Err(error) => return Scanned::Lost(NetlinkError::parse(operation, error)),
        };
        let end = match reply {
            Reply::Item(item) => {
                if failed.is_none()
                    && let Err(error) = each(&item)
                {
                    *failed = Some(NetlinkError::parse(operation, error));
                }
                continue;
            }
            Reply::Ignore => continue,
            Reply::Acked => End::Acked,
            Reply::Done { interrupted } => End::Done { interrupted },
            Reply::Failed(ack) => {
                return Scanned::Ended(Err(NetlinkError::refused(
                    operation, ack.errno, ack.reason,
                )));
            }
            Reply::Overrun => {
                return Scanned::Lost(NetlinkError::Overrun {
                    operation: operation.to_owned(),
                });
            }
        };
        return Scanned::Ended(failed.take().map_or(Ok(end), Err));
    }
    Scanned::More
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    const PORT: u32 = 77;

    /// The kernel's side, scripted: what each `receive` returns, in order,
    /// and every datagram sent.
    #[derive(Default)]
    struct Script {
        replies: VecDeque<Result<(Vec<u8>, u32), Errno>>,
        sent: Vec<Vec<u8>>,
        /// A `receive` returns this length instead of the datagram's.
        claimed_len: Option<usize>,
        /// What `send` fails with first, in order.
        send_errors: VecDeque<Errno>,
    }

    impl Transport for Script {
        fn send(&mut self, bytes: &[u8]) -> Result<(), Errno> {
            if let Some(error) = self.send_errors.pop_front() {
                return Err(error);
            }
            self.sent.push(bytes.to_vec());
            Ok(())
        }

        fn receive(&mut self, buffer: &mut [u8]) -> Result<(usize, u32), Errno> {
            let (datagram, sender) = self.replies.pop_front().expect("the script has a reply")?;
            buffer[..datagram.len()].copy_from_slice(&datagram);
            Ok((self.claimed_len.unwrap_or(datagram.len()), sender))
        }

        fn receive_now(&mut self, buffer: &mut [u8]) -> Result<(usize, u32), Errno> {
            if self.replies.is_empty() {
                return Err(Errno::EAGAIN);
            }
            self.receive(buffer)
        }
    }

    fn message(kind: u16, flags: u16, payload: &[u8], seq: u32) -> Vec<u8> {
        let mut message = MessageBuilder::new(kind, flags);
        message.raw(payload);
        message.finish(seq, PORT).unwrap()
    }

    fn ack(seq: u32, errno: i32) -> Vec<u8> {
        let mut payload = (-errno).to_ne_bytes().to_vec();
        payload.extend(
            MessageBuilder::new(16, flag::REQUEST)
                .finish(seq, PORT)
                .unwrap(),
        );
        message(msg_type::ERROR, flag::CAPPED, &payload, seq)
    }

    fn item(seq: u32, value: u8, flags: u16) -> Vec<u8> {
        message(16, flag::MULTI | flags, &[value; 4], seq)
    }

    fn done(seq: u32) -> Vec<u8> {
        message(msg_type::DONE, flag::MULTI, &0i32.to_ne_bytes(), seq)
    }

    fn conversation(replies: Vec<Result<(Vec<u8>, u32), Errno>>) -> Conversation<Script> {
        Conversation::new(
            Script {
                replies: replies.into(),
                ..Script::default()
            },
            PORT,
        )
    }

    #[allow(clippy::unnecessary_wraps, reason = "a dump's parser returns a Result")]
    fn first_byte(message: &Message<'_>) -> Result<Option<u8>, ParseError> {
        Ok(message.payload.first().copied())
    }

    #[test]
    fn a_request_is_numbered_flagged_and_acknowledged() {
        let mut talk = conversation(vec![Ok((ack(1, 0), 0)), Ok((ack(2, 0), 0))]);
        let request = MessageBuilder::new(16, flag::CREATE);
        talk.exchange("x", &request, |_| Ok(())).unwrap();
        talk.exchange("x", &request, |_| Ok(())).unwrap();
        let sent: Vec<_> = talk
            .transport
            .sent
            .iter()
            .map(|bytes| Messages::new(bytes).next().unwrap().unwrap())
            .collect();
        assert_eq!(sent.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(sent[0].flags, flag::CREATE | flag::REQUEST | flag::ACK);
        assert_eq!(sent[0].port, PORT);
    }

    #[test]
    fn a_refusal_carries_the_errno() {
        let mut talk = conversation(vec![Ok((ack(1, 1), 0))]);
        let error = talk
            .exchange("making x", &MessageBuilder::new(16, 0), |_| Ok(()))
            .unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EPERM));
        assert_eq!(
            error.to_string(),
            format!("making x: {}", Errno::EPERM.desc())
        );
    }

    #[test]
    fn a_refusal_names_the_kernels_reason() {
        let error = NetlinkError::refused(
            "creating the bridge x",
            34,
            Some("Attribute failed policy validation".into()),
        );
        assert_eq!(
            error.to_string(),
            format!(
                "creating the bridge x: {} (the kernel says: Attribute failed policy validation)",
                Errno::ERANGE.desc()
            )
        );
    }

    #[test]
    fn what_isnt_the_kernels_answer_is_skipped() {
        let mut talk = conversation(vec![
            Ok((ack(1, 1), 4242)), // a forged refusal from another process
            Ok((ack(9, 1), 0)),    // the answer to another request
            Ok((item(1, 5, 0), 0)),
            Ok((ack(1, 0), 0)),
        ]);
        let mut items = Vec::new();
        talk.exchange("x", &MessageBuilder::new(18, 0), |message| {
            items.push(message.payload[0]);
            Ok(())
        })
        .unwrap();
        assert_eq!(items, [5]);
    }

    #[test]
    fn a_dump_collects_every_part_until_done() {
        let mut two = item(1, 1, 0);
        two.extend(item(1, 2, 0));
        let mut talk = conversation(vec![Ok((two, 0)), Ok((item(1, 3, 0), 0)), Ok((done(1), 0))]);
        let found = talk
            .dump("x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap();
        assert_eq!(found, [1, 2, 3]);
        let sent = Messages::new(&talk.transport.sent[0])
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(sent.flags, flag::REQUEST | flag::DUMP);
    }

    #[test]
    fn an_interrupted_dump_is_redone_from_scratch() {
        let mut talk = conversation(vec![
            Ok((item(1, 1, 0), 0)),
            Ok((item(1, 2, flag::DUMP_INTR), 0)),
            Ok((done(1), 0)),
            Ok((item(2, 7, 0), 0)),
            Ok((done(2), 0)),
        ]);
        let found = talk
            .dump("x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap();
        assert_eq!(found, [7]);
        assert_eq!(talk.transport.sent.len(), 2);
    }

    #[test]
    fn a_dump_interrupted_every_time_is_an_error() {
        let mut replies = Vec::new();
        for seq in 1..=DUMP_TRIES {
            replies.push(Ok((item(seq, 1, flag::DUMP_INTR), 0)));
            replies.push(Ok((done(seq), 0)));
        }
        let mut talk = conversation(replies);
        let error = talk
            .dump("listing x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap_err();
        assert!(matches!(error, NetlinkError::Interrupted { .. }), "{error}");
    }

    #[test]
    fn a_dump_that_ends_in_an_error_is_refused() {
        let mut talk = conversation(vec![
            Ok((item(1, 1, 0), 0)),
            Ok((
                message(msg_type::DONE, flag::MULTI, &(-22i32).to_ne_bytes(), 1),
                0,
            )),
        ]);
        let error = talk
            .dump("x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EINVAL));
    }

    #[test]
    fn a_cut_datagram_is_an_error() {
        let mut talk = conversation(vec![Ok((item(1, 1, 0), 0))]);
        talk.transport.claimed_len = Some(RECEIVE_BUFFER + 1);
        let error = talk
            .dump("x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap_err();
        assert!(matches!(error, NetlinkError::Truncated { .. }), "{error}");
    }

    fn request(talk: &mut Conversation<Script>) -> Result<(), NetlinkError> {
        talk.exchange("x", &MessageBuilder::new(16, 0), |_| Ok(()))
    }

    #[test]
    fn a_signal_while_receiving_is_retried() {
        let mut talk = conversation(vec![Err(Errno::EINTR), Ok((ack(1, 0), 0))]);
        request(&mut talk).unwrap();
    }

    #[test]
    fn a_signal_while_sending_is_retried() {
        let mut talk = conversation(vec![Ok((ack(1, 0), 0))]);
        talk.transport.send_errors.push_back(Errno::EINTR);
        request(&mut talk).unwrap();
        assert_eq!(talk.transport.sent.len(), 1);
    }

    #[test]
    fn a_failed_send_names_its_errno() {
        let mut talk = conversation(vec![]);
        talk.transport.send_errors.push_back(Errno::ECONNREFUSED);
        assert_eq!(
            request(&mut talk).unwrap_err().errno(),
            Some(Errno::ECONNREFUSED)
        );
    }

    #[test]
    fn a_request_that_cant_be_built_is_never_sent() {
        let mut talk = conversation(vec![]);
        let mut message = MessageBuilder::new(16, 0);
        message.attr(1, &vec![0; usize::from(u16::MAX)]);
        let error = talk.exchange("x", &message, |_| Ok(())).unwrap_err();
        assert!(matches!(error, NetlinkError::Build { .. }), "{error}");
        assert!(talk.transport.sent.is_empty());
    }

    #[test]
    fn no_answer_in_time_is_a_timeout_and_the_socket_is_not_used_again() {
        // The answer may still come: the next request must not take it for
        // its own.
        let mut talk = conversation(vec![Err(Errno::EAGAIN), Ok((ack(1, 0), 0))]);
        let error = request(&mut talk).unwrap_err();
        assert!(matches!(error, NetlinkError::Timeout { .. }), "{error}");
        let error = request(&mut talk).unwrap_err();
        assert!(matches!(error, NetlinkError::Lost { .. }), "{error}");
        assert_eq!(talk.transport.sent.len(), 1);
    }

    #[test]
    fn lost_messages_are_an_overrun() {
        let mut talk = conversation(vec![Err(Errno::ENOBUFS)]);
        let error = request(&mut talk).unwrap_err();
        assert!(matches!(error, NetlinkError::Overrun { .. }), "{error}");
    }

    #[test]
    fn another_receive_error_names_its_errno() {
        let mut talk = conversation(vec![Err(Errno::EBADF)]);
        assert_eq!(request(&mut talk).unwrap_err().errno(), Some(Errno::EBADF));
    }

    #[test]
    fn a_cut_answer_to_a_request_is_an_error() {
        let mut talk = conversation(vec![Ok((ack(1, 0), 0))]);
        talk.transport.claimed_len = Some(RECEIVE_BUFFER + 1);
        let error = request(&mut talk).unwrap_err();
        assert!(matches!(error, NetlinkError::Truncated { .. }), "{error}");
    }

    #[test]
    fn a_dump_interrupted_only_at_its_end_is_redone() {
        // The kernel can flag the DONE alone (the table changed after the
        // last part was filled).
        let interrupted_done = message(
            msg_type::DONE,
            flag::MULTI | flag::DUMP_INTR,
            &0i32.to_ne_bytes(),
            1,
        );
        let mut talk = conversation(vec![
            Ok((item(1, 1, 0), 0)),
            Ok((interrupted_done, 0)),
            Ok((item(2, 2, 0), 0)),
            Ok((done(2), 0)),
        ]);
        let found = talk
            .dump("x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap();
        assert_eq!(found, [2]);
    }

    #[test]
    fn a_part_that_cant_be_read_still_lets_the_dump_finish() {
        // The kernel only finishes a dump as it's read, and refuses another
        // until it has: the rest is read and dropped, then the error comes,
        // and the socket goes on working.
        let mut talk = conversation(vec![
            Ok((item(1, 1, 0), 0)),
            Ok((item(1, 2, 0), 0)),
            Ok((item(1, 3, 0), 0)),
            Ok((done(1), 0)),
            Ok((ack(2, 0), 0)),
        ]);
        let mut seen = Vec::new();
        let error = talk
            .dump("listing x", &MessageBuilder::new(18, 0), |message| {
                seen.push(message.payload[0]);
                if message.payload[0] == 2 {
                    Err(ParseError::Missing("name"))
                } else {
                    Ok(Some(()))
                }
            })
            .unwrap_err();
        assert!(matches!(error, NetlinkError::Parse { .. }), "{error}");
        assert_eq!(seen, [1, 2], "nothing after the failure is parsed");
        assert!(
            talk.transport.replies.len() == 1,
            "the dump was read to its DONE"
        );
        request(&mut talk).unwrap();
    }

    #[test]
    fn a_datagram_from_another_process_is_dropped_unread() {
        // Even one that can't be parsed: it isn't the kernel's.
        let mut talk = conversation(vec![Ok((vec![3, 0, 0, 0], 4242)), Ok((ack(1, 0), 0))]);
        request(&mut talk).unwrap();
    }

    #[test]
    fn an_overrun_message_is_an_error() {
        let mut talk = conversation(vec![Ok((message(msg_type::OVERRUN, 0, &[], 1), 0))]);
        let error = talk
            .dump("x", &MessageBuilder::new(18, 0), first_byte)
            .unwrap_err();
        assert!(matches!(error, NetlinkError::Overrun { .. }), "{error}");
    }

    fn items(count: usize) -> Vec<BatchItem> {
        (0..count)
            .map(|n| BatchItem {
                what: format!("item {n}"),
                message: MessageBuilder::new(0x0a00 + 6, flag::CREATE),
            })
            .collect()
    }

    #[test]
    fn a_batch_is_one_datagram_between_markers_and_every_item_is_acked() {
        // BEGIN is 1, the items 2 to 4, END 5.
        let mut acks = ack(2, 0);
        acks.extend(ack(3, 0));
        let mut talk = conversation(vec![Ok((acks, 0)), Ok((ack(4, 0), 0))]);
        talk.batch("x", 10, &items(3)).unwrap();
        assert_eq!(talk.transport.sent.len(), 1);
        let sent: Vec<_> = Messages::new(&talk.transport.sent[0])
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            sent.iter().map(|m| (m.kind, m.seq)).collect::<Vec<_>>(),
            [
                (BATCH_BEGIN, 1),
                (0x0a06, 2),
                (0x0a06, 3),
                (0x0a06, 4),
                (BATCH_END, 5)
            ]
        );
        assert_eq!(sent[0].payload, nfgenmsg(0, 10));
        assert_eq!(sent[1].flags, flag::CREATE | flag::REQUEST | flag::ACK);
        assert_eq!(sent[0].flags, flag::REQUEST);
        // The next request goes on after END.
        talk.transport.replies.push_back(Ok((ack(6, 0), 0)));
        request(&mut talk).unwrap();
    }

    #[test]
    fn a_refused_item_names_itself_and_the_rest_is_read_without_waiting() {
        // The kernel answers each item, then has nothing more: no wait.
        let mut answers = ack(2, 0);
        answers.extend(ack(3, 22));
        let mut talk = conversation(vec![Ok((answers, 0))]);
        let error = talk.batch("installing", 10, &items(3)).unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EINVAL));
        assert!(
            error.to_string().starts_with("installing: item 1: "),
            "{error}"
        );
        // Nothing was lost: the socket goes on.
        talk.transport.replies.push_back(Ok((ack(6, 0), 0)));
        request(&mut talk).unwrap();
    }

    #[test]
    fn a_refused_batch_is_refused_at_its_begin() {
        let mut talk = conversation(vec![Ok((ack(1, 1), 0))]);
        let error = talk.batch("installing", 10, &items(2)).unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EPERM));
        assert_eq!(
            error.to_string(),
            format!("installing: {}", Errno::EPERM.desc())
        );
    }

    #[test]
    fn a_batch_without_all_its_answers_is_a_timeout() {
        let mut talk = conversation(vec![Ok((ack(2, 0), 0)), Err(Errno::EAGAIN)]);
        let error = talk.batch("x", 10, &items(2)).unwrap_err();
        assert!(matches!(error, NetlinkError::Timeout { .. }), "{error}");
        assert!(matches!(
            request(&mut talk).unwrap_err(),
            NetlinkError::Lost { .. }
        ));
    }

    #[test]
    fn answers_outside_the_batch_are_skipped() {
        let mut answers = ack(9, 1); // another request's
        answers.extend(ack(2, 0));
        let mut talk = conversation(vec![Ok((answers, 0))]);
        talk.batch("x", 10, &items(1)).unwrap();
    }

    #[test]
    fn an_unreadable_answer_is_an_error_and_the_socket_is_not_used_again() {
        let mut talk = conversation(vec![Ok((vec![3, 0, 0, 0], 0))]);
        let error = request(&mut talk).unwrap_err();
        assert!(matches!(error, NetlinkError::Parse { .. }), "{error}");
        assert!(matches!(
            request(&mut talk).unwrap_err(),
            NetlinkError::Lost { .. }
        ));
    }
}
