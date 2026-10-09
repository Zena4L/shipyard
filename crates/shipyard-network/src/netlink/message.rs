//! Netlink messages as bytes: building requests and reading what the kernel
//! sends back.
//!
//! A netlink message (netlink(7)) is a 16-byte header (`nlmsghdr`: length,
//! type, flags, sequence number, sender's port ID), then a fixed header of the
//! message's family (`ifinfomsg` for links, `ifaddrmsg`, `rtmsg`, …), then
//! attributes: each a 4-byte header (length, type) and a value, padded to 4
//! bytes. An attribute can hold more attributes (a *nested* one). Everything
//! is in the host's byte order, except values the family says are in network
//! order.
//!
//! What the kernel sends is read through [`Messages`] and [`Attrs`], which
//! check every length against what's actually there: a malformed or hostile
//! buffer is an error, never a read past the end or a panic (the fuzz target
//! `netlink` holds it to that). Nothing here touches a socket, so it builds,
//! runs and is tested on any UNIX.

// Bytes from the kernel (or a fuzzer) are only ever read through `get`.
#![deny(clippy::indexing_slicing)]

use std::fmt;

use thiserror::Error;

/// The size of a netlink message header (`nlmsghdr`).
pub(crate) const HEADER_LEN: usize = 16;
/// The size of an attribute header (`nlattr`).
const ATTR_HEADER_LEN: usize = 4;
/// Messages and attributes are padded to this many bytes (`NLMSG_ALIGNTO`,
/// `NLA_ALIGNTO`).
const ALIGN: usize = 4;

/// Message types every netlink family shares.
pub(crate) mod msg_type {
    /// Nothing; skipped.
    pub(crate) const NOOP: u16 = 1;
    /// An error, or (with error 0) the acknowledgement of a request.
    pub(crate) const ERROR: u16 = 2;
    /// The end of a multipart message (a dump).
    pub(crate) const DONE: u16 = 3;
    /// Data was lost.
    pub(crate) const OVERRUN: u16 = 4;
}

/// Message header flags.
pub(crate) mod flag {
    /// A request (every message to the kernel).
    pub(crate) const REQUEST: u16 = 0x1;
    /// Part of a multipart message, ended by `DONE`.
    #[cfg(test)]
    pub(crate) const MULTI: u16 = 0x2;
    /// Asks for an acknowledgement (an `ERROR` message with error 0).
    pub(crate) const ACK: u16 = 0x4;
    /// The dump was interrupted by a change: its parts are inconsistent.
    pub(crate) const DUMP_INTR: u16 = 0x10;
    /// Every entry, not one (`NLM_F_ROOT | NLM_F_MATCH`), on a get request.
    pub(crate) const DUMP: u16 = 0x300;
    /// Fail if it exists already (on a new request).
    pub(crate) const EXCL: u16 = 0x200;
    /// Create it if it doesn't exist (on a new request).
    pub(crate) const CREATE: u16 = 0x400;
    /// Add it at the end (a rule, on a new request).
    pub(crate) const APPEND: u16 = 0x800;
    /// On an `ERROR`: the request was cut to its header (`NETLINK_CAP_ACK`).
    pub(crate) const CAPPED: u16 = 0x100;
    /// On an `ERROR` or `DONE`: extended ACK attributes follow
    /// (`NETLINK_EXT_ACK`).
    pub(crate) const ACK_TLVS: u16 = 0x200;
}

/// Attribute type flags (the top two bits of an attribute's type).
pub(crate) mod attr_flag {
    /// The attribute holds attributes.
    pub(crate) const NESTED: u16 = 1 << 15;
    /// The value is in network byte order.
    const NET_BYTEORDER: u16 = 1 << 14;
    /// The type without the flags.
    pub(crate) const TYPE_MASK: u16 = !(NESTED | NET_BYTEORDER);
}

/// Extended ACK attributes (`NLMSGERR_ATTR_*`).
mod ext_ack {
    pub(super) const MSG: u16 = 1;
}

/// `n` rounded up to the next multiple of [`ALIGN`], or `None` past
/// `usize::MAX`.
fn aligned(n: usize) -> Option<usize> {
    n.checked_add(ALIGN - 1).map(|n| n & !(ALIGN - 1))
}

/// What's wrong with bytes that should have been netlink messages.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// A header or a value says it's longer than what's there, or shorter
    /// than its own header.
    #[error("a netlink {0} has an impossible length")]
    Length(&'static str),
    /// A fixed-size value has the wrong size.
    #[error("a netlink attribute of type {kind} is {len} bytes, not {expected}")]
    Size {
        /// The attribute's type.
        kind: u16,
        /// Its length.
        len: usize,
        /// What its type needs.
        expected: usize,
    },
    /// Something every message of its kind has is missing.
    #[error("a netlink message has no {0}")]
    Missing(&'static str),
    /// A value that can't be what it says (a prefix longer than its address).
    #[error("a netlink message has an impossible {0}")]
    Invalid(&'static str),
    /// A string attribute isn't NUL-terminated UTF-8.
    #[error("a netlink attribute of type {0} isn't a valid string")]
    String(u16),
}

/// What can't be built into a message.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BuildError {
    /// An attribute (with what it nests) or the message is longer than its
    /// length field can say.
    #[error("a netlink {0} is too long")]
    TooLong(&'static str),
}

/// A request being built: a header, a family header, then attributes.
#[derive(Debug, Clone)]
pub(crate) struct MessageBuilder {
    kind: u16,
    flags: u16,
    /// Everything after the header.
    body: Vec<u8>,
    error: Option<BuildError>,
}

impl MessageBuilder {
    /// A message of `kind` with `flags` (`REQUEST` and `ACK` are the sender's
    /// to add).
    pub(crate) fn new(kind: u16, flags: u16) -> Self {
        MessageBuilder {
            kind,
            flags,
            body: Vec::with_capacity(256),
            error: None,
        }
    }

    /// Adds `flags` to the header's.
    pub(crate) fn add_flags(&mut self, flags: u16) -> &mut Self {
        self.flags |= flags;
        self
    }

    /// Appends `bytes` as they are, padded: a family header, or the fixed
    /// header that starts a nested message (a veth peer's `ifinfomsg`).
    pub(crate) fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.body.extend_from_slice(bytes);
        self.pad();
        self
    }

    /// Appends an attribute of type `kind` holding `value`.
    pub(crate) fn attr(&mut self, kind: u16, value: &[u8]) -> &mut Self {
        let Some(len) = value
            .len()
            .checked_add(ATTR_HEADER_LEN)
            .and_then(|len| u16::try_from(len).ok())
        else {
            self.error.get_or_insert(BuildError::TooLong("attribute"));
            return self;
        };
        self.body.extend_from_slice(&len.to_ne_bytes());
        self.body.extend_from_slice(&kind.to_ne_bytes());
        self.body.extend_from_slice(value);
        self.pad();
        self
    }

    /// Appends a one-byte attribute.
    pub(crate) fn attr_u8(&mut self, kind: u16, value: u8) -> &mut Self {
        self.attr(kind, &[value])
    }

    /// Appends a four-byte attribute in the host's byte order.
    pub(crate) fn attr_u32(&mut self, kind: u16, value: u32) -> &mut Self {
        self.attr(kind, &value.to_ne_bytes())
    }

    /// Appends a two-byte attribute in network byte order (`nf_tables` and
    /// conntrack keep their numbers big-endian).
    pub(crate) fn attr_be16(&mut self, kind: u16, value: u16) -> &mut Self {
        self.attr(kind, &value.to_be_bytes())
    }

    /// Appends a four-byte attribute in network byte order.
    pub(crate) fn attr_be32(&mut self, kind: u16, value: u32) -> &mut Self {
        self.attr(kind, &value.to_be_bytes())
    }

    /// Appends a string attribute, with the NUL the kernel expects.
    pub(crate) fn attr_str(&mut self, kind: u16, value: &str) -> &mut Self {
        let mut bytes = Vec::with_capacity(value.len() + 1);
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
        self.attr(kind, &bytes)
    }

    /// Appends a nested attribute of type `kind` (flagged as nested), holding
    /// what `inside` adds.
    pub(crate) fn nest(&mut self, kind: u16, inside: impl FnOnce(&mut Self)) -> &mut Self {
        let start = self.body.len();
        self.attr(kind | attr_flag::NESTED, &[]);
        inside(self);
        let len = u16::try_from(self.body.len() - start);
        match (len, self.body.get_mut(start..start + 2)) {
            (Ok(len), Some(slot)) => slot.copy_from_slice(&len.to_ne_bytes()),
            _ => {
                self.error
                    .get_or_insert(BuildError::TooLong("nested attribute"));
            }
        }
        self
    }

    /// How long the finished message is, header included.
    pub(crate) fn encoded_len(&self) -> usize {
        HEADER_LEN.saturating_add(self.body.len())
    }

    /// The finished message, with its length, sequence number `seq` and the
    /// sender's port ID `port` filled in.
    pub(crate) fn finish(&self, seq: u32, port: u32) -> Result<Vec<u8>, BuildError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        // (A length that saturated doesn't fit either.)
        let len = u32::try_from(self.encoded_len()).map_err(|_| BuildError::TooLong("message"))?;
        let mut bytes = Vec::with_capacity(self.encoded_len());
        bytes.extend_from_slice(&len.to_ne_bytes());
        bytes.extend_from_slice(&self.kind.to_ne_bytes());
        bytes.extend_from_slice(&self.flags.to_ne_bytes());
        bytes.extend_from_slice(&seq.to_ne_bytes());
        bytes.extend_from_slice(&port.to_ne_bytes());
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }

    fn pad(&mut self) {
        let len = self.body.len();
        // A Vec can't come within `ALIGN` of usize::MAX.
        let padded = aligned(len).unwrap_or(len);
        self.body.resize(padded, 0);
    }
}

/// One message the kernel sent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Message<'a> {
    /// Its type: a family's (`RTM_NEWLINK`, …) or one of [`msg_type`]'s.
    pub(crate) kind: u16,
    /// Its flags ([`flag`]).
    pub(crate) flags: u16,
    /// The sequence number of the request it answers.
    pub(crate) seq: u32,
    /// The port ID it's for (the receiving socket's).
    pub(crate) port: u32,
    /// What follows the header: the family header and attributes.
    pub(crate) payload: &'a [u8],
}

impl fmt::Debug for Message<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("kind", &self.kind)
            .field("flags", &format_args!("{:#x}", self.flags))
            .field("seq", &self.seq)
            .field("port", &self.port)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl<'a> Message<'a> {
    /// The fixed family header of `len` bytes, and the attributes after it.
    pub(crate) fn split(&self, len: usize) -> Result<(&'a [u8], Attrs<'a>), ParseError> {
        let header = self
            .payload
            .get(..len)
            .ok_or(ParseError::Length("family header"))?;
        let rest = aligned(len)
            .and_then(|start| self.payload.get(start..))
            .unwrap_or_default();
        Ok((header, Attrs::new(rest)))
    }

    /// An `ERROR` message's contents.
    pub(crate) fn error(&self) -> Result<Ack, ParseError> {
        let code = read_i32(self.payload, 0).ok_or(ParseError::Length("error message"))?;
        // The request follows the code: its header alone where it was capped
        // (or for an acknowledgement), else the whole request.
        let request_len = if self.flags & flag::CAPPED != 0 || code == 0 {
            HEADER_LEN
        } else {
            read_u32(self.payload, 4)
                .and_then(|len| usize::try_from(len).ok())
                .ok_or(ParseError::Length("error message"))?
        };
        Ok(self.ack(code, request_len.checked_add(4)))
    }

    /// A `DONE` message's contents: errno 0, or the error a dump ended with
    /// (an empty payload, from older kernels, is 0).
    pub(crate) fn done(&self) -> Ack {
        self.ack(read_i32(self.payload, 0).unwrap_or(0), Some(4))
    }

    /// An acknowledgement with `code`, its extended ACK (if flagged) starting
    /// at the aligned `tlvs` offset. A malformed extended ACK is dropped: the
    /// errno is what matters, and callers act on it.
    fn ack(&self, code: i32, tlvs: Option<usize>) -> Ack {
        let errno = code.checked_neg().unwrap_or(i32::MAX);
        let reason = (self.flags & flag::ACK_TLVS != 0)
            .then(|| {
                tlvs.and_then(aligned)
                    .and_then(|start| self.payload.get(start..))
            })
            .flatten()
            .and_then(|tlvs| {
                Attrs::new(tlvs)
                    .map_while(Result::ok)
                    .find(|attr| attr.kind == ext_ack::MSG)
                    .and_then(|attr| attr.string().ok().map(str::to_owned))
            });
        Ack { errno, reason }
    }
}

/// The kernel's answer to a request: an `ERROR` message, or a dump's `DONE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ack {
    /// 0 for success, else the errno (positive).
    pub(crate) errno: i32,
    /// The extended ACK's message: the kernel's own words on what was wrong.
    pub(crate) reason: Option<String>,
}

/// One step of reading items from `rest`: `parse` reads the next item and
/// how far it reaches; after an error nothing more is read.
fn step<'a, T>(
    rest: &mut &'a [u8],
    parse: impl FnOnce(&'a [u8]) -> Result<(T, usize), ParseError>,
) -> Option<Result<T, ParseError>> {
    if rest.is_empty() {
        return None;
    }
    match parse(rest) {
        Ok((item, next)) => {
            *rest = rest.get(next..).unwrap_or_default();
            Some(Ok(item))
        }
        Err(error) => {
            *rest = &[];
            Some(Err(error))
        }
    }
}

/// The messages in a buffer the kernel sent (one datagram can hold several).
#[derive(Debug, Clone)]
pub(crate) struct Messages<'a> {
    rest: &'a [u8],
}

impl<'a> Messages<'a> {
    /// The messages in `bytes`.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Messages { rest: bytes }
    }

    /// The message at the start of `rest`, and where the next one starts.
    fn parse(rest: &'a [u8]) -> Result<(Message<'a>, usize), ParseError> {
        let len = read_u32(rest, 0)
            .and_then(|len| usize::try_from(len).ok())
            .filter(|len| (HEADER_LEN..=rest.len()).contains(len))
            .ok_or(ParseError::Length("message"))?;
        let bytes = rest.get(..len).ok_or(ParseError::Length("message"))?;
        let field = |value: Option<u32>| value.ok_or(ParseError::Length("message"));
        let message = Message {
            kind: read_u16(bytes, 4).ok_or(ParseError::Length("message"))?,
            flags: read_u16(bytes, 6).ok_or(ParseError::Length("message"))?,
            seq: field(read_u32(bytes, 8))?,
            port: field(read_u32(bytes, 12))?,
            payload: bytes.get(HEADER_LEN..).unwrap_or_default(),
        };
        // The next one starts at the padded end (the last may be unpadded).
        Ok((message, aligned(len).unwrap_or(len).min(rest.len())))
    }
}

impl<'a> Iterator for Messages<'a> {
    type Item = Result<Message<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        step(&mut self.rest, Messages::parse)
    }
}

/// One attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attr<'a> {
    /// Its type, without the [`attr_flag`] bits.
    pub(crate) kind: u16,
    /// Whether it was flagged as nested.
    pub(crate) nested: bool,
    /// Its value.
    pub(crate) value: &'a [u8],
}

impl<'a> Attr<'a> {
    /// The value as one byte.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn u8(&self) -> Result<u8, ParseError> {
        let [byte] = self.fixed()?;
        Ok(byte)
    }

    /// The value as a 32-bit number in the host's byte order.
    pub(crate) fn u32(&self) -> Result<u32, ParseError> {
        Ok(u32::from_ne_bytes(self.fixed()?))
    }

    /// The value as a 16-bit number in network byte order.
    pub(crate) fn be16(&self) -> Result<u16, ParseError> {
        Ok(u16::from_be_bytes(self.fixed()?))
    }

    /// The value as a 32-bit number in network byte order.
    pub(crate) fn be32(&self) -> Result<u32, ParseError> {
        Ok(u32::from_be_bytes(self.fixed()?))
    }

    /// The value's bytes up to its NUL (which must be there): a C string.
    pub(crate) fn c_bytes(&self) -> Result<&'a [u8], ParseError> {
        let end = self
            .value
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(ParseError::String(self.kind))?;
        self.value.get(..end).ok_or(ParseError::String(self.kind))
    }

    /// The value as a string: UTF-8 up to its NUL (which must be there).
    pub(crate) fn string(&self) -> Result<&'a str, ParseError> {
        std::str::from_utf8(self.c_bytes()?).map_err(|_| ParseError::String(self.kind))
    }

    /// The attributes the value holds.
    pub(crate) fn nested(&self) -> Attrs<'a> {
        Attrs::new(self.value)
    }

    /// The value as exactly `N` bytes.
    pub(crate) fn fixed<const N: usize>(&self) -> Result<[u8; N], ParseError> {
        <[u8; N]>::try_from(self.value).map_err(|_| ParseError::Size {
            kind: self.kind,
            len: self.value.len(),
            expected: N,
        })
    }
}

/// The attributes in a buffer.
#[derive(Debug, Clone)]
pub(crate) struct Attrs<'a> {
    rest: &'a [u8],
}

impl<'a> Attrs<'a> {
    /// The attributes in `bytes`.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Attrs { rest: bytes }
    }

    /// The attribute at the start of `rest`, and where the next one starts.
    fn parse(rest: &'a [u8]) -> Result<(Attr<'a>, usize), ParseError> {
        let len = read_u16(rest, 0)
            .map(usize::from)
            .filter(|len| (ATTR_HEADER_LEN..=rest.len()).contains(len))
            .ok_or(ParseError::Length("attribute"))?;
        let raw_kind = read_u16(rest, 2).ok_or(ParseError::Length("attribute"))?;
        let attr = Attr {
            kind: raw_kind & attr_flag::TYPE_MASK,
            nested: raw_kind & attr_flag::NESTED != 0,
            value: rest
                .get(ATTR_HEADER_LEN..len)
                .ok_or(ParseError::Length("attribute"))?,
        };
        Ok((attr, aligned(len).unwrap_or(len).min(rest.len())))
    }
}

impl<'a> Iterator for Attrs<'a> {
    type Item = Result<Attr<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        step(&mut self.rest, Attrs::parse)
    }
}

/// The `N` bytes at `at`, if they're all there.
pub(crate) fn read_array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    let end = at.checked_add(N)?;
    bytes.get(at..end)?.try_into().ok()
}

pub(crate) fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    read_array(bytes, at).map(u16::from_ne_bytes)
}

pub(crate) fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    read_array(bytes, at).map(u32::from_ne_bytes)
}

fn read_i32(bytes: &[u8], at: usize) -> Option<i32> {
    read_array(bytes, at).map(i32::from_ne_bytes)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, reason = "tests index what they built")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn attrs(bytes: &[u8]) -> Vec<Attr<'_>> {
        Attrs::new(bytes).collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn big_endian_numbers_round_trip() {
        let mut message = MessageBuilder::new(18, 0);
        message.attr_be16(1, 0x1234).attr_be32(2, 0x1234_5678);
        let bytes = message.finish(1, 1).unwrap();
        let only = Messages::new(&bytes).next().unwrap().unwrap();
        let found = attrs(only.payload);
        assert_eq!(found[0].value, [0x12, 0x34]);
        assert_eq!(found[0].be16(), Ok(0x1234));
        assert_eq!(found[1].be32(), Ok(0x1234_5678));
        assert!(found[1].be16().is_err());
    }

    #[test]
    fn a_built_message_has_its_header_filled_in() {
        let mut message = MessageBuilder::new(18, flag::REQUEST);
        message.raw(&[7; 16]).attr_str(3, "lo");
        let bytes = message.finish(42, 99).unwrap();
        let parsed: Vec<_> = Messages::new(&bytes).collect::<Result<_, _>>().unwrap();
        assert_eq!(parsed.len(), 1);
        let only = parsed[0];
        assert_eq!(
            (only.kind, only.flags, only.seq, only.port),
            (18, flag::REQUEST, 42, 99)
        );
        assert_eq!(
            read_u32(&bytes, 0),
            Some(u32::try_from(bytes.len()).unwrap())
        );
        let (header, rest) = only.split(16).unwrap();
        assert_eq!(header, &[7; 16]);
        let rest: Vec<_> = rest.collect::<Result<_, _>>().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!((rest[0].kind, rest[0].string().unwrap()), (3, "lo"));
    }

    #[test]
    fn attributes_are_padded_to_four_bytes() {
        let mut message = MessageBuilder::new(1, 0);
        message.attr(1, &[0xaa]).attr(2, &[0xbb; 5]);
        let bytes = message.finish(0, 0).unwrap();
        // header, then 4+1 padded to 8, then 4+5 padded to 12
        assert_eq!(bytes.len(), HEADER_LEN + 8 + 12);
        assert_eq!(
            &bytes[HEADER_LEN..HEADER_LEN + 8],
            &[5, 0, 1, 0, 0xaa, 0, 0, 0]
        );
        let found = attrs(&bytes[HEADER_LEN..]);
        assert_eq!(found[1].value, &[0xbb; 5]);
    }

    #[test]
    fn a_nest_says_how_long_it_is_and_is_flagged() {
        let mut message = MessageBuilder::new(1, 0);
        message.nest(18, |inner| {
            inner.attr_str(1, "veth").nest(2, |deeper| {
                deeper.attr_u32(7, 9);
            });
        });
        let bytes = message.finish(0, 0).unwrap();
        let outer = attrs(&bytes[HEADER_LEN..]);
        assert_eq!(outer.len(), 1);
        assert!(outer[0].nested);
        assert_eq!(outer[0].kind, 18);
        // its length covers everything inside: 4 + (4+5 → 12) + (4 + 8)
        assert_eq!(read_u16(&bytes, HEADER_LEN), Some(4 + 12 + 12));
        let inner = attrs(outer[0].value);
        assert_eq!(inner[0].string().unwrap(), "veth");
        let deeper = attrs(inner[1].value);
        assert_eq!(deeper[0].u32().unwrap(), 9);
    }

    #[test]
    fn an_attribute_too_long_for_its_length_field_is_an_error() {
        let mut message = MessageBuilder::new(1, 0);
        message.attr(1, &vec![0; usize::from(u16::MAX)]);
        assert_eq!(message.finish(0, 0), Err(BuildError::TooLong("attribute")));
        let mut message = MessageBuilder::new(1, 0);
        message.nest(1, |inner| {
            for _ in 0..3 {
                inner.attr(2, &vec![0; 30_000]);
            }
        });
        assert_eq!(
            message.finish(0, 0),
            Err(BuildError::TooLong("nested attribute"))
        );
    }

    #[test]
    fn flags_can_be_added() {
        let mut message = MessageBuilder::new(5, flag::REQUEST);
        message.add_flags(flag::ACK);
        let bytes = message.finish(0, 0).unwrap();
        let parsed = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!((parsed.kind, parsed.flags), (5, flag::REQUEST | flag::ACK));
    }

    #[test]
    fn impossible_message_lengths_are_errors() {
        // shorter than a header
        let mut bytes = MessageBuilder::new(1, 0).finish(0, 0).unwrap();
        bytes[0..4].copy_from_slice(&15u32.to_ne_bytes());
        assert_eq!(
            Messages::new(&bytes).next(),
            Some(Err(ParseError::Length("message")))
        );
        // longer than the buffer
        bytes[0..4].copy_from_slice(&17u32.to_ne_bytes());
        assert_eq!(
            Messages::new(&bytes).next(),
            Some(Err(ParseError::Length("message")))
        );
        // a truncated header
        assert_eq!(
            Messages::new(&bytes[..10]).next(),
            Some(Err(ParseError::Length("message")))
        );
        // after an error, nothing more
        let mut messages = Messages::new(&bytes[..10]);
        messages.next();
        assert_eq!(messages.next(), None);
    }

    #[test]
    fn two_messages_in_one_buffer_are_both_read() {
        let mut first = MessageBuilder::new(16, flag::MULTI);
        first.attr(1, &[1]);
        let mut both = first.finish(1, 0).unwrap();
        both.extend(
            MessageBuilder::new(msg_type::DONE, flag::MULTI)
                .finish(1, 0)
                .unwrap(),
        );
        let parsed: Vec<_> = Messages::new(&both).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            parsed.iter().map(|m| m.kind).collect::<Vec<_>>(),
            [16, msg_type::DONE]
        );
    }

    #[test]
    fn impossible_attribute_lengths_are_errors() {
        for bad in [&[3u8, 0, 1, 0][..], &[9, 0, 1, 0, 0, 0, 0, 0], &[4, 0, 1]] {
            assert_eq!(
                Attrs::new(bad).next(),
                Some(Err(ParseError::Length("attribute"))),
                "{bad:?}"
            );
        }
        // a zero-length value is fine
        assert_eq!(attrs(&[4, 0, 1, 0])[0].value, &[] as &[u8]);
        // the last attribute may be unpadded
        assert_eq!(attrs(&[5, 0, 1, 0, 7])[0].value, &[7]);
    }

    #[test]
    fn typed_values_check_their_size() {
        let attr = Attr {
            kind: 4,
            nested: false,
            value: &[1, 0, 0],
        };
        assert_eq!(
            attr.u32(),
            Err(ParseError::Size {
                kind: 4,
                len: 3,
                expected: 4
            })
        );
        let attr = Attr {
            kind: 4,
            nested: false,
            value: &[0, 0, 1, 0],
        };
        assert_eq!(attr.u32(), Ok(u32::from_ne_bytes([0, 0, 1, 0])));
    }

    #[test]
    fn strings_need_their_nul_and_utf8() {
        let ok = Attr {
            kind: 3,
            nested: false,
            value: b"eth0\0\0",
        };
        assert_eq!(ok.string(), Ok("eth0"));
        for bad in [&b"eth0"[..], b"\xff\xfe\0"] {
            let attr = Attr {
                kind: 3,
                nested: false,
                value: bad,
            };
            assert_eq!(attr.string(), Err(ParseError::String(3)));
        }
    }

    /// An `ERROR` message as the kernel builds it: the code, the request's
    /// header (capped), then extended ACK attributes.
    fn error_message(code: i32, flags: u16, reason: Option<&str>) -> Vec<u8> {
        let mut message = MessageBuilder::new(msg_type::ERROR, flags);
        let mut payload = code.to_ne_bytes().to_vec();
        payload.extend(MessageBuilder::new(16, flag::REQUEST).finish(7, 0).unwrap());
        message.raw(&payload);
        if let Some(reason) = reason {
            message.attr_str(ext_ack::MSG, reason);
        }
        // NLMSGERR_ATTR_OFFS: the offending attribute's offset, not used
        message.attr_u32(2, 36);
        message.finish(7, 1234).unwrap()
    }

    #[test]
    fn an_error_carries_the_kernels_reason() {
        let bytes = error_message(
            -34,
            flag::CAPPED | flag::ACK_TLVS,
            Some("Attribute failed policy validation"),
        );
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(
            message.error().unwrap(),
            Ack {
                errno: 34,
                reason: Some("Attribute failed policy validation".into()),
            }
        );
    }

    #[test]
    fn a_capped_error_skips_only_the_echoed_header() {
        // With NETLINK_CAP_ACK the kernel echoes the request's header alone,
        // whose length still says how long the whole request was.
        let mut header = MessageBuilder::new(16, flag::REQUEST).finish(7, 0).unwrap();
        header[0..4].copy_from_slice(&40u32.to_ne_bytes());
        let mut payload = (-17i32).to_ne_bytes().to_vec();
        payload.extend(&header);
        let mut message = MessageBuilder::new(msg_type::ERROR, flag::CAPPED | flag::ACK_TLVS);
        message.raw(&payload).attr_str(ext_ack::MSG, "File exists");
        let bytes = message.finish(7, 0).unwrap();
        let ack = Messages::new(&bytes)
            .next()
            .unwrap()
            .unwrap()
            .error()
            .unwrap();
        assert_eq!(
            (ack.errno, ack.reason.as_deref()),
            (17, Some("File exists"))
        );
    }

    #[test]
    fn a_message_with_an_unpadded_length_is_followed_at_the_padded_end() {
        // nlmsg_len counts the bytes used, not the padding after them.
        let mut first = MessageBuilder::new(16, flag::MULTI);
        first.attr(1, &[9]); // 4 + 1, padded to 8
        let mut bytes = first.finish(1, 0).unwrap();
        let unpadded = u32::try_from(bytes.len() - 3).unwrap();
        bytes[0..4].copy_from_slice(&unpadded.to_ne_bytes());
        bytes.extend(
            MessageBuilder::new(msg_type::DONE, flag::MULTI)
                .finish(1, 0)
                .unwrap(),
        );
        let kinds: Vec<_> = Messages::new(&bytes).map(|m| m.unwrap().kind).collect();
        assert_eq!(kinds, [16, msg_type::DONE]);
    }

    #[test]
    fn an_acknowledgement_is_errno_zero() {
        let bytes = error_message(0, flag::CAPPED, None);
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(
            message.error().unwrap(),
            Ack {
                errno: 0,
                reason: None
            }
        );
    }

    #[test]
    fn an_uncapped_error_skips_the_whole_request() {
        // The echoed request is longer than its header: the extended ACK
        // starts after all of it.
        let mut request = MessageBuilder::new(16, flag::REQUEST);
        request.raw(&[0; 16]).attr_str(3, "a-very-long-name");
        let request = request.finish(7, 0).unwrap();
        let mut message = MessageBuilder::new(msg_type::ERROR, flag::ACK_TLVS);
        let mut payload = (-22i32).to_ne_bytes().to_vec();
        payload.extend(&request);
        message.raw(&payload).attr_str(ext_ack::MSG, "bad");
        let bytes = message.finish(7, 0).unwrap();
        let ack = Messages::new(&bytes)
            .next()
            .unwrap()
            .unwrap()
            .error()
            .unwrap();
        assert_eq!((ack.errno, ack.reason.as_deref()), (22, Some("bad")));
    }

    #[test]
    fn a_short_error_is_an_error() {
        let mut message = MessageBuilder::new(msg_type::ERROR, 0);
        message.raw(&[1, 2]);
        let bytes = message.finish(0, 0).unwrap();
        let parsed = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(parsed.error(), Err(ParseError::Length("error message")));
    }

    #[test]
    fn a_done_message_carries_a_code_and_a_reason() {
        let mut message = MessageBuilder::new(msg_type::DONE, flag::MULTI | flag::ACK_TLVS);
        message
            .raw(&(-19i32).to_ne_bytes())
            .attr_str(ext_ack::MSG, "Can not find device");
        let bytes = message.finish(0, 0).unwrap();
        let done = Messages::new(&bytes).next().unwrap().unwrap().done();
        assert_eq!(
            (done.errno, done.reason.as_deref()),
            (19, Some("Can not find device"))
        );
        let bytes = MessageBuilder::new(msg_type::DONE, flag::MULTI)
            .finish(0, 0)
            .unwrap();
        assert_eq!(
            Messages::new(&bytes).next().unwrap().unwrap().done().errno,
            0
        );
    }

    #[test]
    fn a_malformed_extended_ack_keeps_the_errno() {
        // The errno is what callers act on (ENODEV, EEXIST): a reason that
        // isn't a string doesn't hide it.
        let mut payload = (-17i32).to_ne_bytes().to_vec();
        payload.extend(MessageBuilder::new(16, flag::REQUEST).finish(7, 0).unwrap());
        let mut message = MessageBuilder::new(msg_type::ERROR, flag::CAPPED | flag::ACK_TLVS);
        message.raw(&payload).attr(ext_ack::MSG, b"no NUL");
        let bytes = message.finish(7, 0).unwrap();
        let ack = Messages::new(&bytes)
            .next()
            .unwrap()
            .unwrap()
            .error()
            .unwrap();
        assert_eq!(
            ack,
            Ack {
                errno: 17,
                reason: None
            }
        );
    }

    #[test]
    fn a_short_family_header_is_an_error() {
        let mut message = MessageBuilder::new(16, 0);
        message.raw(&[0; 8]);
        let bytes = message.finish(0, 0).unwrap();
        let parsed = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(
            parsed.split(16).map(|_| ()),
            Err(ParseError::Length("family header"))
        );
    }

    proptest! {
        /// Whatever the bytes, reading them never panics, and every message
        /// and attribute found lies inside them.
        #[test]
        fn reading_any_bytes_is_safe(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            for message in Messages::new(&bytes).flatten() {
                prop_assert!(message.payload.len() <= bytes.len());
                let _ = message.error();
                let _ = message.done();
                if let Ok((_, attrs)) = message.split(16) {
                    for attr in attrs.flatten() {
                        let _ = (attr.u8(), attr.u32(), attr.string());
                        for inner in attr.nested().flatten() {
                            prop_assert!(inner.value.len() <= attr.value.len());
                        }
                    }
                }
            }
        }
    }
}
