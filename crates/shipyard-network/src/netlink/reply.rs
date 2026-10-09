//! What one message from the kernel means to a request waiting for its
//! answer. Kept apart from the socket so the rules (whose message it is, when
//! the answer is complete) are tested on any UNIX.

use super::message::{Ack, Message, ParseError, flag, msg_type};

/// One message from the kernel, seen from a request waiting on sequence
/// number `seq` at port ID `port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply<'a> {
    /// Part of the answer: an object asked for.
    Item(Message<'a>),
    /// Not for this request (an old sequence number: an answer to an earlier
    /// request that ended early) or nothing at all (`NOOP`).
    Ignore,
    /// The request succeeded (an `ERROR` with errno 0).
    Acked,
    /// The end of a dump; `interrupted` if the kernel flagged the end itself
    /// as inconsistent (it can, with no part flagged before it).
    Done {
        /// The kernel's `NLM_F_DUMP_INTR` on the `DONE`.
        interrupted: bool,
    },
    /// The kernel refused the request.
    Failed(Ack),
    /// The kernel dropped messages for this socket.
    Overrun,
}

/// What `message` from the kernel means for the request with sequence
/// number `seq` from port ID `port`.
pub(crate) fn classify(message: Message<'_>, seq: u32, port: u32) -> Result<Reply<'_>, ParseError> {
    if message.seq != seq || message.port != port {
        return Ok(Reply::Ignore);
    }
    Ok(match message.kind {
        msg_type::NOOP => Reply::Ignore,
        msg_type::ERROR => {
            let ack = message.error()?;
            if ack.errno == 0 {
                Reply::Acked
            } else {
                Reply::Failed(ack)
            }
        }
        msg_type::DONE => {
            let done = message.done();
            if done.errno == 0 {
                Reply::Done {
                    interrupted: message.flags & flag::DUMP_INTR != 0,
                }
            } else {
                Reply::Failed(done)
            }
        }
        msg_type::OVERRUN => Reply::Overrun,
        _ => Reply::Item(message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::message::{MessageBuilder, Messages};

    fn bytes(kind: u16, flags: u16, payload: &[u8], seq: u32, port: u32) -> Vec<u8> {
        let mut message = MessageBuilder::new(kind, flags);
        message.raw(payload);
        message.finish(seq, port).unwrap()
    }

    fn reply(bytes: &[u8]) -> Reply<'_> {
        let message = Messages::new(bytes).next().unwrap().unwrap();
        classify(message, 5, 77).unwrap()
    }

    #[test]
    fn another_requests_answer_is_ignored() {
        assert!(matches!(
            reply(&bytes(16, 0, &[0; 16], 5, 77)),
            Reply::Item(_)
        ));
        assert_eq!(reply(&bytes(16, 0, &[0; 16], 4, 77)), Reply::Ignore);
        assert_eq!(reply(&bytes(16, 0, &[0; 16], 5, 78)), Reply::Ignore);
    }

    #[test]
    fn errors_and_acknowledgements() {
        let mut ack = 0i32.to_ne_bytes().to_vec();
        ack.extend(
            MessageBuilder::new(16, flag::REQUEST)
                .finish(5, 77)
                .unwrap(),
        );
        assert_eq!(
            reply(&bytes(msg_type::ERROR, flag::CAPPED, &ack, 5, 77)),
            Reply::Acked
        );
        let mut refused = (-1i32).to_ne_bytes().to_vec();
        refused.extend(
            MessageBuilder::new(16, flag::REQUEST)
                .finish(5, 77)
                .unwrap(),
        );
        assert_eq!(
            reply(&bytes(msg_type::ERROR, flag::CAPPED, &refused, 5, 77)),
            Reply::Failed(Ack {
                errno: 1,
                reason: None
            })
        );
    }

    #[test]
    fn the_end_of_a_dump_its_code_and_its_interruption() {
        let ok = 0i32.to_ne_bytes();
        assert_eq!(
            reply(&bytes(msg_type::DONE, flag::MULTI, &ok, 5, 77)),
            Reply::Done { interrupted: false }
        );
        assert_eq!(
            reply(&bytes(
                msg_type::DONE,
                flag::MULTI | flag::DUMP_INTR,
                &ok,
                5,
                77
            )),
            Reply::Done { interrupted: true }
        );
        assert_eq!(
            reply(&bytes(
                msg_type::DONE,
                flag::MULTI,
                &(-22i32).to_ne_bytes(),
                5,
                77
            )),
            Reply::Failed(Ack {
                errno: 22,
                reason: None
            })
        );
    }

    #[test]
    fn noop_and_overrun() {
        assert_eq!(reply(&bytes(msg_type::NOOP, 0, &[], 5, 77)), Reply::Ignore);
        assert_eq!(
            reply(&bytes(msg_type::OVERRUN, 0, &[], 5, 77)),
            Reply::Overrun
        );
    }
}
