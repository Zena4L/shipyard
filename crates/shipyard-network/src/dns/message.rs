//! DNS messages as bytes (RFC 1035, section 4): reading queries and
//! responses, and writing the answers a network's DNS server gives.
//!
//! A message is a 12-byte header (an ID, flags, and how many records each
//! section has), then its sections: the questions (a name, a type, a
//! class), then the answers, authority and additional records (a name, a
//! type, a class, a time to live, and the data). A name is a list of labels,
//! each a length byte and up to 63 bytes, ended by an empty one; to save
//! room, a name can end with a *pointer* instead (two bytes, the top two
//! bits set) to where the rest of it was written earlier in the message.
//!
//! Pointers are the dangerous part: one can point at itself, or two at
//! each other, and a careless reader goes round forever. Here, each pointer
//! must point before where the last one did, so a name is read in a bounded
//! number of steps whatever the bytes say. Everything else is checked
//! against what's there too: a malformed or hostile message is an error,
//! never a read past the end or a panic (the fuzz target `dns` holds it to
//! that).

// Bytes from the network (or a fuzzer) are only ever read through `get`.
#![deny(clippy::indexing_slicing)]

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use thiserror::Error;

/// The size of a message's header.
pub const HEADER_LEN: usize = 12;

/// The largest message over UDP without EDNS (RFC 1035, 2.3.4). The
/// server's own answers always fit: one question and one record.
pub const UDP_MAX: usize = 512;

/// The longest label.
const MAX_LABEL: usize = 63;

/// The longest name, as written in a message (each label's length byte and
/// bytes, and the final empty label).
const MAX_NAME: usize = 255;

/// Record types.
pub mod rtype {
    /// An IPv4 address.
    pub const A: u16 = 1;
    /// A name for an address (a reverse lookup).
    pub const PTR: u16 = 12;
    /// An IPv6 address.
    pub const AAAA: u16 = 28;
    /// Any type (a question only).
    pub const ANY: u16 = 255;
}

/// The Internet class, the only one in use.
pub const CLASS_IN: u16 = 1;

/// Any class (a question only).
pub const CLASS_ANY: u16 = 255;

/// Response codes.
pub mod rcode {
    /// No error.
    pub const NOERROR: u8 = 0;
    /// The query couldn't be read.
    pub const FORMERR: u8 = 1;
    /// The server failed (no upstream answered).
    pub const SERVFAIL: u8 = 2;
    /// The name doesn't exist.
    pub const NXDOMAIN: u8 = 3;
    /// A kind of query the server doesn't do.
    pub const NOTIMP: u8 = 4;
    /// The server won't answer it.
    pub const REFUSED: u8 = 5;
}

/// The header's flags.
mod flag {
    /// A response (not a query).
    pub(super) const QR: u16 = 0x8000;
    /// The answer is the server's own (it's authoritative for the name).
    pub(super) const AA: u16 = 0x0400;
    /// Truncated: the whole answer didn't fit.
    pub(super) const TC: u16 = 0x0200;
    /// Recursion desired (copied from the query).
    pub(super) const RD: u16 = 0x0100;
    /// Recursion available.
    pub(super) const RA: u16 = 0x0080;
}

/// What's wrong with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DnsError {
    /// It ends before what it says is there.
    #[error("the message is cut short")]
    Short,
    /// A label longer than 63 bytes, or one whose top bits are reserved.
    #[error("a label in the message is malformed")]
    BadLabel,
    /// A name longer than 255 bytes.
    #[error("a name in the message is too long")]
    NameTooLong,
    /// A compression pointer that doesn't point before the last one (a
    /// loop, or a pointer forward).
    #[error("a name in the message points in a loop")]
    PointerLoop,
    /// A name that can't be written: an empty or over-long label.
    #[error("{0:?} isn't a name DNS can carry")]
    InvalidName(&'static str),
    /// A message over 65535 bytes, which TCP's length can't frame.
    #[error("the message is too long")]
    TooLong,
}

/// A domain name: its labels, as bytes (DNS doesn't say what they hold).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name(Vec<Vec<u8>>);

impl Name {
    /// The name written `text` (labels separated by dots; a final dot is
    /// allowed).
    pub fn from_text(text: &str) -> Result<Name, DnsError> {
        let text = text.strip_suffix('.').unwrap_or(text);
        if text.is_empty() {
            return Ok(Name(Vec::new()));
        }
        let labels: Vec<Vec<u8>> = text
            .split('.')
            .map(|label| label.as_bytes().to_vec())
            .collect();
        if labels
            .iter()
            .any(|label| label.is_empty() || label.len() > MAX_LABEL)
        {
            return Err(DnsError::InvalidName("a label is empty or over 63 bytes"));
        }
        let name = Name(labels);
        if name.wire_len() > MAX_NAME {
            return Err(DnsError::InvalidName("the name is over 255 bytes"));
        }
        Ok(name)
    }

    /// Its labels.
    #[must_use]
    pub fn labels(&self) -> &[Vec<u8>] {
        &self.0
    }

    /// Its only label, if it has exactly one (`db`, not `db.example.com`).
    #[must_use]
    pub fn single_label(&self) -> Option<&[u8]> {
        match self.0.as_slice() {
            [label] => Some(label),
            _ => None,
        }
    }

    /// Its length in a message, uncompressed.
    fn wire_len(&self) -> usize {
        self.0.iter().map(|label| label.len() + 1).sum::<usize>() + 1
    }

    /// Appends it to `out`, uncompressed.
    fn write(&self, out: &mut Vec<u8>) {
        for label in &self.0 {
            // At most 63 (checked when it was made or read).
            out.push(u8::try_from(label.len()).unwrap_or(0));
            out.extend_from_slice(label);
        }
        out.push(0);
    }
}

/// Written as dig writes names: labels and dots, with a dot, a backslash or
/// a byte that isn't printable written `\DDD`.
impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, label) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            for &byte in label {
                if byte.is_ascii_graphic() && byte != b'.' && byte != b'\\' {
                    write!(f, "{}", char::from(byte))?;
                } else {
                    write!(f, "\\{byte:03}")?;
                }
            }
        }
        Ok(())
    }
}

/// A message's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Its ID: a response carries its query's.
    pub id: u16,
    /// Its flags, opcode and response code.
    pub flags: u16,
    /// How many questions.
    pub questions: u16,
    /// How many answers.
    pub answers: u16,
}

impl Header {
    /// Reads the header at the start of `message`.
    pub fn read(message: &[u8]) -> Result<Header, DnsError> {
        if message.len() < HEADER_LEN {
            return Err(DnsError::Short);
        }
        Ok(Header {
            id: be16(message, 0)?,
            flags: be16(message, 2)?,
            questions: be16(message, 4)?,
            answers: be16(message, 6)?,
        })
    }

    /// Whether it's a response's.
    #[must_use]
    pub fn is_response(&self) -> bool {
        self.flags & flag::QR != 0
    }

    /// Its kind of query (0, a standard query, is the only one answered).
    #[must_use]
    pub fn opcode(&self) -> u8 {
        // Four bits.
        u8::try_from((self.flags >> 11) & 0xf).unwrap_or(0)
    }

    /// Its response code.
    #[must_use]
    pub fn rcode(&self) -> u8 {
        u8::try_from(self.flags & 0xf).unwrap_or(0)
    }

    /// Whether the answer is the server's own.
    #[must_use]
    pub fn authoritative(&self) -> bool {
        self.flags & flag::AA != 0
    }

    /// Whether the answer was cut short.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.flags & flag::TC != 0
    }
}

/// A question: what's asked about which name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The name.
    pub name: Name,
    /// The record type ([`rtype`]).
    pub qtype: u16,
    /// The class ([`CLASS_IN`]).
    pub qclass: u16,
}

/// A query to answer: its header, and its one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Its header.
    pub header: Header,
    /// Its question.
    pub question: Question,
}

/// What a message received as a query is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// A query to answer.
    Query(Query),
    /// One that can't be answered: the response is only this code (with the
    /// query's ID).
    Unanswerable {
        /// Its header.
        header: Header,
        /// The response code.
        rcode: u8,
    },
    /// Nothing to answer: too short to have an ID, or a response (never
    /// answered, so two servers can't keep answering each other).
    Ignored,
}

/// Reads `message`, received as a query.
#[must_use]
pub fn read_query(message: &[u8]) -> Received {
    let Ok(header) = Header::read(message) else {
        return Received::Ignored;
    };
    if header.is_response() {
        return Received::Ignored;
    }
    let refuse = |rcode| Received::Unanswerable { header, rcode };
    if header.opcode() != 0 {
        return refuse(rcode::NOTIMP);
    }
    // Every resolver asks one question at a time; a message with another
    // count has no one answer.
    if header.questions != 1 {
        return refuse(rcode::FORMERR);
    }
    match read_question(message, HEADER_LEN) {
        Ok((question, _)) => Received::Query(Query { header, question }),
        Err(_) => refuse(rcode::FORMERR),
    }
}

/// Reads the question at `at` in `message`, and where it ends.
fn read_question(message: &[u8], at: usize) -> Result<(Question, usize), DnsError> {
    let (name, at) = read_name(message, at)?;
    let question = Question {
        name,
        qtype: be16(message, at)?,
        qclass: be16(message, at + 2)?,
    };
    Ok((question, at + 4))
}

/// Reads the name at `start` in `message`: the name, and where it ends
/// (after its final label, or after the first pointer).
///
/// Each pointer must point before the place the last jump led to (the name's
/// start, for the first): the places only go down, so a loop can't be
/// written, and the name is read in a bounded number of steps.
pub fn read_name(message: &[u8], start: usize) -> Result<(Name, usize), DnsError> {
    let mut labels = Vec::new();
    let mut wire_len = 1;
    let mut at = start;
    // Where the name ends in the message: set at the first pointer.
    let mut end = None;
    // A pointer must point before this.
    let mut limit = start;
    loop {
        let length = *message.get(at).ok_or(DnsError::Short)?;
        match length & 0xc0 {
            0x00 => {
                let length = usize::from(length);
                if length == 0 {
                    return Ok((Name(labels), end.unwrap_or(at + 1)));
                }
                let label = message
                    .get(at + 1..at + 1 + length)
                    .ok_or(DnsError::Short)?;
                wire_len += length + 1;
                if wire_len > MAX_NAME {
                    return Err(DnsError::NameTooLong);
                }
                labels.push(label.to_vec());
                at += 1 + length;
            }
            0xc0 => {
                let target = usize::from(be16(message, at)? & 0x3fff);
                if target >= limit {
                    return Err(DnsError::PointerLoop);
                }
                end.get_or_insert(at + 2);
                limit = target;
                at = target;
            }
            // 0x40 and 0x80: kinds of label RFC 1035 reserved.
            _ => return Err(DnsError::BadLabel),
        }
    }
}

/// The two bytes at `at`, in network order.
fn be16(message: &[u8], at: usize) -> Result<u16, DnsError> {
    match message.get(at..at + 2) {
        Some(&[a, b]) => Ok(u16::from_be_bytes([a, b])),
        _ => Err(DnsError::Short),
    }
}

/// The four bytes at `at`, in network order.
fn be32(message: &[u8], at: usize) -> Result<u32, DnsError> {
    match message.get(at..at + 4) {
        Some(&[a, b, c, d]) => Ok(u32::from_be_bytes([a, b, c, d])),
        _ => Err(DnsError::Short),
    }
}

/// A record's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RData {
    /// An IPv4 address.
    A(Ipv4Addr),
    /// An IPv6 address.
    Aaaa(Ipv6Addr),
    /// A name (for a reverse lookup).
    Ptr(Name),
    /// Another type's, as bytes.
    Other {
        /// Its type.
        rtype: u16,
        /// Its bytes.
        data: Vec<u8>,
    },
}

impl RData {
    /// Its type.
    #[must_use]
    pub fn rtype(&self) -> u16 {
        match self {
            RData::A(_) => rtype::A,
            RData::Aaaa(_) => rtype::AAAA,
            RData::Ptr(_) => rtype::PTR,
            RData::Other { rtype, .. } => *rtype,
        }
    }
}

/// The response to `query`: `rcode`, and `answers` for its question's name
/// (each with a time to live of 0: containers come and go, so no resolver
/// should remember them). `authoritative` for the server's own names.
#[must_use]
pub fn response(query: &Query, rcode: u8, authoritative: bool, answers: &[RData]) -> Vec<u8> {
    let mut flags = flag::QR | flag::RA | (query.header.flags & flag::RD) | u16::from(rcode & 0xf);
    if authoritative {
        flags |= flag::AA;
    }
    let mut out = Vec::with_capacity(UDP_MAX);
    write_header(
        &mut out,
        query.header.id,
        flags,
        1,
        u16::try_from(answers.len()).unwrap_or(u16::MAX),
    );
    query.question.name.write(&mut out);
    out.extend_from_slice(&query.question.qtype.to_be_bytes());
    out.extend_from_slice(&query.question.qclass.to_be_bytes());
    for answer in answers {
        // The owner is the question's name: a pointer to it, at the end of
        // the header.
        out.extend_from_slice(&(0xc000 | u16::try_from(HEADER_LEN).unwrap_or(0)).to_be_bytes());
        out.extend_from_slice(&answer.rtype().to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        let mut data = Vec::new();
        match answer {
            RData::A(ip) => data.extend_from_slice(&ip.octets()),
            RData::Aaaa(ip) => data.extend_from_slice(&ip.octets()),
            RData::Ptr(name) => name.write(&mut data),
            RData::Other { data: bytes, .. } => data.extend_from_slice(bytes),
        }
        out.extend_from_slice(&u16::try_from(data.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&data);
    }
    out
}

/// The response to a query that can't be answered (`header` is its): only
/// `rcode`, with no question.
#[must_use]
pub fn refusal(header: Header, rcode: u8) -> Vec<u8> {
    let flags = flag::QR | flag::RA | (header.flags & flag::RD) | u16::from(rcode & 0xf);
    let mut out = Vec::with_capacity(HEADER_LEN);
    write_header(&mut out, header.id, flags, 0, 0);
    out
}

/// Appends a header to `out` (no authority or additional records).
fn write_header(out: &mut Vec<u8>, id: u16, flags: u16, questions: u16, answers: u16) {
    for field in [id, flags, questions, answers, 0, 0] {
        out.extend_from_slice(&field.to_be_bytes());
    }
}

/// A query for `name`'s records of type `qtype` (recursion desired), with
/// the ID `id`: what a resolver sends.
#[must_use]
pub fn query(id: u16, name: &Name, qtype: u16) -> Vec<u8> {
    let mut out = Vec::new();
    write_header(&mut out, id, flag::RD, 1, 0);
    name.write(&mut out);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out
}

/// A record in a response's answer section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Whose it is.
    pub name: Name,
    /// What it says.
    pub data: RData,
}

/// A response, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Its header.
    pub header: Header,
    /// Its answers.
    pub answers: Vec<Answer>,
}

/// Reads `message` as a response: its header and its answers (the
/// questions are skipped, and the other sections left unread).
pub fn read_response(message: &[u8]) -> Result<Response, DnsError> {
    let header = Header::read(message)?;
    let mut at = HEADER_LEN;
    for _ in 0..header.questions {
        at = read_question(message, at)?.1;
    }
    let mut answers = Vec::new();
    for _ in 0..header.answers {
        let (name, after) = read_name(message, at)?;
        let rtype = be16(message, after)?;
        // The class and the time to live (`be32`, checked present).
        be32(message, after + 4)?;
        let length = usize::from(be16(message, after + 8)?);
        let start = after + 10;
        let bytes = message.get(start..start + length).ok_or(DnsError::Short)?;
        let data = match rtype {
            rtype::A => match bytes {
                &[a, b, c, d] => RData::A(Ipv4Addr::new(a, b, c, d)),
                _ => return Err(DnsError::Short),
            },
            rtype::AAAA => RData::Aaaa(Ipv6Addr::from(
                <[u8; 16]>::try_from(bytes).map_err(|_| DnsError::Short)?,
            )),
            // A compressed name points into the message, not the data.
            rtype::PTR => RData::Ptr(read_name(message, start)?.0),
            rtype => RData::Other {
                rtype,
                data: bytes.to_vec(),
            },
        };
        answers.push(Answer { name, data });
        at = start + length;
    }
    Ok(Response { header, answers })
}

/// `message` framed for TCP: its length in two bytes, then it (RFC 1035,
/// 4.2.2).
pub fn frame(message: &[u8]) -> Result<Vec<u8>, DnsError> {
    let length = u16::try_from(message.len()).map_err(|_| DnsError::TooLong)?;
    let mut out = Vec::with_capacity(message.len() + 2);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(message);
    Ok(out)
}

/// Whether `reply` is a response to `query` (both as received): its ID,
/// and its one question, the same (the name in any case: a resolver may
/// mix its case, and the answer carries it back as asked).
#[must_use]
pub fn answers(reply: &[u8], query: &[u8]) -> bool {
    let (Ok(reply_header), Ok(query_header)) = (Header::read(reply), Header::read(query)) else {
        return false;
    };
    if !reply_header.is_response()
        || reply_header.id != query_header.id
        || reply_header.questions != 1
        || query_header.questions != 1
    {
        return false;
    }
    match (
        read_question(reply, HEADER_LEN),
        read_question(query, HEADER_LEN),
    ) {
        (Ok((asked, _)), Ok((ours, _))) => {
            asked.qtype == ours.qtype
                && asked.qclass == ours.qclass
                && asked.name.0.len() == ours.name.0.len()
                && asked
                    .name
                    .0
                    .iter()
                    .zip(&ours.name.0)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
        }
        _ => false,
    }
}

/// A message's ID (its first two bytes), if it has one.
#[must_use]
pub fn id_of(message: &[u8]) -> Option<u16> {
    be16(message, 0).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        Name::from_text(text).unwrap()
    }

    fn query_for(text: &str, qtype: u16) -> Query {
        match read_query(&query(0x1234, &name(text), qtype)) {
            Received::Query(query) => query,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_query_reads_back() {
        let query = query_for("db.example.com.", rtype::A);
        assert_eq!(query.header.id, 0x1234);
        assert_eq!(query.question.name, name("db.example.com"));
        assert_eq!(query.question.name.to_string(), "db.example.com");
        assert_eq!(query.question.qtype, rtype::A);
        assert_eq!(query.question.qclass, CLASS_IN);
        assert_eq!(name("db").single_label(), Some(&b"db"[..]));
        assert_eq!(name("db.app").single_label(), None);
    }

    #[test]
    fn a_response_carries_the_question_and_its_answers() {
        let query = query_for("db", rtype::A);
        let bytes = response(
            &query,
            rcode::NOERROR,
            true,
            &[RData::A(Ipv4Addr::new(10, 89, 0, 2))],
        );
        assert!(bytes.len() <= UDP_MAX);
        let read = read_response(&bytes).unwrap();
        assert_eq!(read.header.id, 0x1234);
        assert!(read.header.is_response());
        assert!(read.header.authoritative());
        assert!(!read.header.truncated());
        assert_eq!(read.header.rcode(), rcode::NOERROR);
        // Recursion desired, copied; available.
        assert_eq!(
            read.header.flags & (flag::RD | flag::RA),
            flag::RD | flag::RA
        );
        assert_eq!(
            read.answers,
            vec![Answer {
                name: name("db"),
                data: RData::A(Ipv4Addr::new(10, 89, 0, 2)),
            }]
        );
        // A time to live of 0.
        assert_eq!(&bytes[bytes.len() - 10..bytes.len() - 6], &[0, 0, 0, 0]);

        let aaaa = query_for("db", rtype::AAAA);
        let ip6: Ipv6Addr = "fd00:1::2".parse().unwrap();
        let bytes = response(&aaaa, rcode::NOERROR, true, &[RData::Aaaa(ip6)]);
        assert_eq!(
            read_response(&bytes).unwrap().answers[0].data,
            RData::Aaaa(ip6)
        );

        let ptr = query_for("2.0.89.10.in-addr.arpa", rtype::PTR);
        let bytes = response(&ptr, rcode::NOERROR, true, &[RData::Ptr(name("db"))]);
        assert_eq!(
            read_response(&bytes).unwrap().answers[0].data,
            RData::Ptr(name("db"))
        );

        let bytes = response(&query, rcode::NXDOMAIN, false, &[]);
        let read = read_response(&bytes).unwrap();
        assert_eq!(read.header.rcode(), rcode::NXDOMAIN);
        assert!(!read.header.authoritative());
        assert!(read.answers.is_empty());
    }

    #[test]
    fn what_isnt_a_query_isnt_answered() {
        assert_eq!(read_query(&[0; 11]), Received::Ignored);
        // A response.
        let query = query_for("db", rtype::A);
        let answer = response(&query, rcode::NOERROR, false, &[]);
        assert_eq!(read_query(&answer), Received::Ignored);
        // Another opcode (2, a status request).
        let mut status = super::query(7, &name("db"), rtype::A);
        status[2] |= 2 << 3;
        assert!(matches!(
            read_query(&status),
            Received::Unanswerable { rcode: rcode::NOTIMP, header } if header.id == 7
        ));
        // Two questions.
        let mut two = super::query(7, &name("db"), rtype::A);
        two[5] = 2;
        assert!(matches!(
            read_query(&two),
            Received::Unanswerable {
                rcode: rcode::FORMERR,
                ..
            }
        ));
        // A question cut short.
        let whole = super::query(7, &name("db"), rtype::A);
        assert!(matches!(
            read_query(&whole[..whole.len() - 1]),
            Received::Unanswerable {
                rcode: rcode::FORMERR,
                ..
            }
        ));
        // Its refusal has the ID, and no question.
        let header = Header::read(&whole).unwrap();
        let refused = read_response(&refusal(header, rcode::REFUSED)).unwrap();
        assert_eq!(refused.header.id, 7);
        assert_eq!(refused.header.rcode(), rcode::REFUSED);
        assert_eq!(refused.header.questions, 0);
    }

    #[test]
    fn compressed_names_are_followed_backwards() {
        // "example.com" at 12, then "db" and a pointer to it.
        let mut message = vec![0; HEADER_LEN];
        message.extend_from_slice(b"\x07example\x03com\x00");
        let db = message.len();
        message.extend_from_slice(b"\x02db\xc0\x0c");
        let (read, end) = read_name(&message, db).unwrap();
        assert_eq!(read, name("db.example.com"));
        // It ends after the pointer.
        assert_eq!(end, message.len());
        // A pointer to a pointer, further back still.
        let again = message.len();
        message.extend_from_slice(&(0xc000 | u16::try_from(db).unwrap()).to_be_bytes());
        assert_eq!(
            read_name(&message, again).unwrap().0,
            name("db.example.com")
        );
    }

    #[test]
    fn pointers_that_loop_are_refused() {
        // A pointer to itself.
        let mut message = vec![0; HEADER_LEN];
        message.extend_from_slice(&[0xc0, 12]);
        assert_eq!(read_name(&message, 12), Err(DnsError::PointerLoop));
        // Forwards.
        let mut message = vec![0; HEADER_LEN];
        message.extend_from_slice(&[0xc0, 14, 0]);
        assert_eq!(read_name(&message, 12), Err(DnsError::PointerLoop));
        // Two pointing at each other through labels: at 12, "a" then a
        // pointer to 16; at 16, "b" then a pointer back to 12.
        let mut message = vec![0; HEADER_LEN];
        message.extend_from_slice(&[1, b'a', 0xc0, 16, 1, b'b', 0xc0, 12]);
        assert_eq!(read_name(&message, 16), Err(DnsError::PointerLoop));
        // A pointer back to a label whose own pointer leads to where the
        // first started.
        let mut message = vec![0; HEADER_LEN];
        message.extend_from_slice(&[1, b'a', 0xc0, 14, 0xc0, 12]);
        assert_eq!(read_name(&message, 16), Err(DnsError::PointerLoop));
    }

    #[test]
    fn labels_and_names_are_bounded() {
        let mut message = vec![0; HEADER_LEN];
        // 0x40 and 0x80: reserved kinds of label.
        message.push(0x40);
        assert_eq!(read_name(&message, 12), Err(DnsError::BadLabel));
        message[12] = 0x80;
        assert_eq!(read_name(&message, 12), Err(DnsError::BadLabel));
        // A label longer than what's left.
        message[12] = 5;
        message.extend_from_slice(b"ab");
        assert_eq!(read_name(&message, 12), Err(DnsError::Short));
        // No end.
        let message = [vec![0; HEADER_LEN], b"\x02db".to_vec()].concat();
        assert_eq!(read_name(&message, 12), Err(DnsError::Short));
        // Four 63-byte labels: 257 bytes written, past the 255 there can be.
        let label = [vec![63], vec![b'x'; 63]].concat();
        let mut long = vec![0; HEADER_LEN];
        for _ in 0..4 {
            long.extend_from_slice(&label);
        }
        long.push(0);
        assert_eq!(read_name(&long, 12), Err(DnsError::NameTooLong));
        // Three and a 62-byte one: 256, one too many.
        let mut over = vec![0; HEADER_LEN];
        for _ in 0..3 {
            over.extend_from_slice(&label);
        }
        over.push(62);
        over.extend_from_slice(&[b'y'; 62]);
        over.push(0);
        assert_eq!(read_name(&over, 12), Err(DnsError::NameTooLong));
        // Three and a 61-byte one: 255, the most there can be.
        let mut longest = vec![0; HEADER_LEN];
        for _ in 0..3 {
            longest.extend_from_slice(&label);
        }
        longest.push(61);
        longest.extend_from_slice(&[b'y'; 61]);
        longest.push(0);
        assert_eq!(read_name(&longest, 12).unwrap().0.labels().len(), 4);

        assert!(Name::from_text(&"x".repeat(64)).is_err());
        assert!(Name::from_text(&"x".repeat(63)).is_ok());
        assert!(Name::from_text("a..b").is_err());
        assert!(Name::from_text(&vec!["x".repeat(63); 4].join(".")).is_err());
        assert!(Name::from_text("").unwrap().labels().is_empty());
    }

    #[test]
    fn odd_bytes_in_names_are_escaped() {
        let mut message = vec![0; HEADER_LEN];
        message.extend_from_slice(b"\x03a.\x5c\x01\xff\x00");
        assert_eq!(
            read_name(&message, 12).unwrap().0.to_string(),
            "a\\046\\092.\\255"
        );
    }

    #[test]
    fn an_answer_must_be_to_the_question_asked() {
        let query = query_for("example.com", rtype::A);
        let bytes = super::query(0x1234, &name("example.com"), rtype::A);
        let reply = response(&query, rcode::NOERROR, false, &[]);
        assert!(answers(&reply, &bytes));
        // The case may differ.
        let upper = response(
            &query_for("EXAMPLE.com", rtype::A),
            rcode::NOERROR,
            false,
            &[],
        );
        assert!(answers(&upper, &bytes));
        // Another name, type or ID; a query; nothing.
        for other in [
            response(
                &query_for("example.org", rtype::A),
                rcode::NOERROR,
                false,
                &[],
            ),
            response(
                &query_for("example.com", rtype::AAAA),
                rcode::NOERROR,
                false,
                &[],
            ),
            {
                let mut other = query.clone();
                other.header.id = 8;
                response(&other, rcode::NOERROR, false, &[])
            },
            bytes.clone(),
            Vec::new(),
        ] {
            assert!(!answers(&other, &bytes), "{other:?}");
        }
    }

    #[test]
    fn tcp_frames_have_the_length_first() {
        assert_eq!(frame(b"abc").unwrap(), b"\x00\x03abc");
        assert_eq!(frame(&vec![0; 65_536]), Err(DnsError::TooLong));
        assert_eq!(id_of(&[0x12, 0x34, 0]), Some(0x1234));
        assert_eq!(id_of(&[0x12]), None);
    }

    proptest::proptest! {
        #[test]
        fn reading_never_panics(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..600)) {
            let _ = read_query(&bytes);
            let _ = read_response(&bytes);
            for start in 0..bytes.len().min(40) {
                let _ = read_name(&bytes, start);
            }
        }
    }
}
