//! One record of a CRI ("k8s-file") log: `TIME STREAM TAG TEXT`, a line.
//!
//! The format Kubernetes' kubelet reads, and containerd and Podman write
//! (D15):
//!
//! ```text
//! 2026-10-10T06:16:19.123456789Z stdout F hello
//! 2026-10-10T06:16:19.123500000Z stderr P a long line, cut here
//! ```
//!
//! - `TIME` is RFC 3339 ([`Timestamp`]);
//! - `STREAM` is `stdout` or `stderr`;
//! - `TAG` is `F` for a full line, or `P` for part of one, the rest in the
//!   records that follow (a line too long for one record, or one the
//!   program hadn't ended when its output ended); fields after a `:` are
//!   allowed for later use, and ignored;
//! - `TEXT` is the program's bytes, without the newline that ended them.
//!   Never parsed: whatever the program wrote, other than a newline, is text.

use super::time::Timestamp;

/// The most of a line one record holds; longer lines go in several, `P`
/// then `F` (containerd's figure).
pub const MAX_TEXT: usize = 16 * 1024;

/// Which of the program's outputs a record is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Its standard output (and a terminal's, which has one).
    Stdout,
    /// Its standard error.
    Stderr,
}

impl Stream {
    /// Its place in a pair indexed by stream: stdout's 0, stderr's 1.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }

    /// The name in a record.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// A record, read from a line of a log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// When it was read from the program.
    pub time: Timestamp,
    /// Which output.
    pub stream: Stream,
    /// Part of a line, whose rest follows (`P`), or a line's end (`F`).
    pub partial: bool,
    /// The program's bytes.
    pub text: &'a [u8],
}

/// A line that isn't a CRI record.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CriError {
    /// Its time isn't RFC 3339.
    #[error("a record's time isn't RFC 3339")]
    Time,
    /// Its stream is missing, or neither `stdout` nor `stderr`.
    #[error("a record's stream is neither stdout nor stderr")]
    Stream,
    /// Its tag is missing, or neither `F` nor `P`.
    #[error("a record's tag is neither F nor P")]
    Tag,
}

/// Appends the record (`TIME STREAM TAG TEXT` and a newline) to `out`.
/// `text` must hold no newline.
pub fn write_record(
    out: &mut Vec<u8>,
    time: Timestamp,
    stream: Stream,
    partial: bool,
    text: &[u8],
) {
    use std::io::Write as _;
    debug_assert!(!text.contains(&b'\n'), "a record's text holds no newline");
    let tag = if partial { 'P' } else { 'F' };
    // Writing to a Vec can't fail.
    let _ = write!(out, "{time} {} {tag} ", stream.name());
    out.extend_from_slice(text);
    out.push(b'\n');
}

/// Reads one record from `line` (without its newline).
///
/// # Errors
///
/// [`CriError`]: which field is wrong.
pub fn parse_record(line: &[u8]) -> Result<Record<'_>, CriError> {
    let (time, rest) = split_field(line).ok_or(CriError::Time)?;
    let time = std::str::from_utf8(time)
        .ok()
        .and_then(|time| Timestamp::parse(time).ok())
        .ok_or(CriError::Time)?;
    let (stream, rest) = split_field(rest).ok_or(CriError::Stream)?;
    let stream = match stream {
        b"stdout" => Stream::Stdout,
        b"stderr" => Stream::Stderr,
        _ => return Err(CriError::Stream),
    };
    // The text may be empty: then the tag may end the line (as kubelet
    // reads it), with or without the space.
    let (tag, text) = split_field(rest).unwrap_or((rest, &[]));
    let partial = match tag.split(|&byte| byte == b':').next() {
        Some(b"P") => true,
        Some(b"F") => false,
        _ => return Err(CriError::Tag),
    };
    Ok(Record {
        time,
        stream,
        partial,
        text,
    })
}

/// The bytes before the first space, and those after it.
fn split_field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let space = line.iter().position(|&byte| byte == b' ')?;
    Some((line.get(..space)?, line.get(space + 1..)?))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn time(text: &str) -> Timestamp {
        Timestamp::parse(text).unwrap()
    }

    /// Lines as kubelet's own tests have them (`pkg/kubelet/kuberuntime/logs`).
    #[test]
    fn kubelets_examples_are_read() {
        let full = parse_record(b"2016-10-20T18:39:20.57606443Z stdout F cri log content").unwrap();
        assert_eq!(full.time, time("2016-10-20T18:39:20.57606443Z"));
        assert_eq!(full.stream, Stream::Stdout);
        assert!(!full.partial);
        assert_eq!(full.text, b"cri log content");
        let partial =
            parse_record(b"2016-10-20T18:39:20.57606443Z stderr P cri log content").unwrap();
        assert_eq!((partial.stream, partial.partial), (Stream::Stderr, true));
        // An empty line, and a tag with fields after it.
        assert_eq!(
            parse_record(b"2016-10-20T18:39:20.57606443Z stdout F")
                .unwrap()
                .text,
            b""
        );
        assert_eq!(
            parse_record(b"2016-10-20T18:39:20.57606443Z stdout F ")
                .unwrap()
                .text,
            b""
        );
        assert!(
            !parse_record(b"2016-10-20T18:39:20.57606443Z stdout F:x y")
                .unwrap()
                .partial
        );
    }

    #[test]
    fn a_record_is_written_as_cri() {
        let mut out = Vec::new();
        let at = time("2026-10-10T06:16:19.5Z");
        write_record(&mut out, at, Stream::Stdout, false, b"hello world");
        write_record(&mut out, at, Stream::Stderr, true, b"");
        assert_eq!(
            out,
            b"2026-10-10T06:16:19.500000000Z stdout F hello world\n\
              2026-10-10T06:16:19.500000000Z stderr P \n"
        );
    }

    #[test]
    fn what_isnt_a_record_is_refused() {
        for (line, error) in [
            (&b""[..], CriError::Time),
            (b"hello", CriError::Time),
            (b"yesterday stdout F x", CriError::Time),
            (b"2016-10-20T18:39:20Z", CriError::Time),
            (b"2016-10-20T18:39:20Z stdin F x", CriError::Stream),
            (b"2016-10-20T18:39:20Z stdout", CriError::Stream),
            (b"2016-10-20T18:39:20Z stdout X x", CriError::Tag),
            (b"2016-10-20T18:39:20Z stdout  x", CriError::Tag),
        ] {
            assert_eq!(
                parse_record(line),
                Err(error),
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    proptest! {
        #[test]
        fn any_text_round_trips(
            secs in 0..253_402_300_799_i64,
            nanos in 0..1_000_000_000_u32,
            stderr: bool,
            partial: bool,
            text in proptest::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 0..100),
        ) {
            let at = Timestamp::from_unix(secs, nanos).unwrap();
            let stream = if stderr { Stream::Stderr } else { Stream::Stdout };
            let mut out = Vec::new();
            write_record(&mut out, at, stream, partial, &text);
            prop_assert_eq!(out.last(), Some(&b'\n'));
            let line = &out[..out.len() - 1];
            prop_assert_eq!(
                parse_record(line),
                Ok(Record { time: at, stream, partial, text: &text })
            );
        }

        #[test]
        fn any_line_is_read_or_refused(line in proptest::collection::vec(any::<u8>(), 0..80)) {
            let _ = parse_record(&line);
        }
    }
}
