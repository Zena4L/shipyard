//! The attach protocol (stage 8, step 8.3c): what goes over a container's
//! `attach.sock` between its monitor and `shipyard-runtime attach`.
//!
//! A stream socket carries a byte stream, not messages: frames give it
//! boundaries. Each is a kind (one byte), a length (four bytes, big-endian),
//! and that many bytes of payload, at most [`MAX_PAYLOAD`]:
//!
//! - the client sends [`Frame::Hello`] first (its version, its detach keys),
//!   then [`Frame::Stdin`], [`Frame::Resize`] and, when its own input ends,
//!   [`Frame::StdinEnd`];
//! - the monitor answers [`Frame::Welcome`] (or [`Frame::Refused`]), then
//!   sends the container's output ([`Frame::Stdout`], [`Frame::Stderr`]) as
//!   it comes, and at the end [`Frame::Detached`], [`Frame::Dropped`] or
//!   [`Frame::Exited`].
//!
//! A **control client** (version 2, stage 9) says [`Frame::HelloControl`]
//! instead: it's welcomed the same way, never sent output and never types,
//! and may ask the monitor to stop the container ([`Frame::Stop`]); like the
//! others it's told [`Frame::Exited`] at the end. That's how `stop` and
//! `wait` reach a container another process started, and why the stop goes
//! on (its SIGKILL at the timeout) if their caller dies.
//!
//! A **holder** (version 3, stage 9, step 9.7b1) says [`Frame::HelloHolder`],
//! then asks the monitor to keep a descriptor until the container has ended
//! ([`Frame::Hold`], the descriptor passed with it: `SCM_RIGHTS`, TLPI
//! §61.13), or to let one go ([`Frame::Release`]), each by its label; the
//! monitor answers [`Frame::Done`], and the holder goes. What the monitor
//! holds at the end is its exit command's, with the labels. From version 4
//! (stage 10, step 10.2c2, F73), a refusal says why as a
//! [`HoldRefusal`] ([`Frame::NotDone`]); a version 3 holder still gets
//! [`Frame::Done`] with only words.
//!
//! **Versions** (H2): a monitor lives as long as its container, so a client
//! may be newer than it. The hello carries the client's highest version; the
//! monitor answers with the one they'll use ([`negotiate`]), and a client
//! skips frame kinds it doesn't know ([`FrameError::Kind`], after which the
//! stream goes on). The header (kind, length) never changes.
//!
//! **Detach keys** (Ctrl-P Ctrl-Q by default, Docker's) are the client's to
//! name and the monitor's to watch for in what the client types
//! ([`DetachKeys::matcher`]), so every client detaches the same way.
//!
//! [`client`] is the client's side: `shipyard-runtime attach`'s, and the
//! engine's by name.

pub mod client;
pub mod forward;

/// The protocol's version: what [`Frame::Hello`] and [`Frame::Welcome`] carry.
pub const VERSION: u32 = 4;

/// The first version with control clients ([`Frame::HelloControl`]).
pub const CONTROL_VERSION: u32 = 2;

/// The first version with holders ([`Frame::HelloHolder`]).
pub const HOLD_VERSION: u32 = 3;

/// The first version whose holders are refused with a reason
/// ([`Frame::NotDone`]).
pub const REASON_VERSION: u32 = 4;

/// Why a monitor refused a hold or a release (version 4: [`Frame::NotDone`]).
/// Its numbers never change; one this side doesn't know (a newer monitor's)
/// is [`Other`](Self::Other).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HoldRefusal {
    /// The container is starting or ending: nothing more is held.
    Ending,
    /// Something is held under that label already.
    LabelTaken,
    /// The monitor holds all it can ([`MOST_HOLDS`](crate::container::MOST_HOLDS)).
    Full,
    /// The labels held already come to all the room there is for them.
    LabelsTooLong,
    /// Nothing is held under that label (a release).
    NotHeld,
    /// More than one descriptor is held under that label (a release).
    HeldTwice,
    /// A hold came with no descriptor.
    NoDescriptor,
    /// What came isn't a file, a directory or a socket.
    NotHoldable,
    /// A release came with a descriptor.
    DescriptorWithRelease,
    /// A reason this side doesn't know (by its number: only
    /// [`from_code`](Self::from_code) makes one, never with a known
    /// number), or, as `Other(0)`, none said (a version 3 monitor's words
    /// that don't say one). 0 is never assigned.
    Other(u8),
}

impl HoldRefusal {
    /// Its number in a frame.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Ending => 1,
            Self::LabelTaken => 2,
            Self::Full => 3,
            Self::LabelsTooLong => 4,
            Self::NotHeld => 5,
            Self::HeldTwice => 6,
            Self::NoDescriptor => 7,
            Self::NotHoldable => 8,
            Self::DescriptorWithRelease => 9,
            Self::Other(code) => code,
        }
    }

    /// The refusal numbered `code`.
    #[must_use]
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Ending,
            2 => Self::LabelTaken,
            3 => Self::Full,
            4 => Self::LabelsTooLong,
            5 => Self::NotHeld,
            6 => Self::HeldTwice,
            7 => Self::NoDescriptor,
            8 => Self::NotHoldable,
            9 => Self::DescriptorWithRelease,
            other => Self::Other(other),
        }
    }
}

/// The longest label a hold may have.
pub const MAX_LABEL: usize = 128;

/// Whether `label` can name a hold: 1 to [`MAX_LABEL`] bytes of ASCII
/// letters, digits, `:`, `_`, `.` and `-` (so it fits in the monitor's
/// arguments and the exit command's environment, comma-separated).
#[must_use]
pub fn is_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'-'))
}

/// The version a monitor of this protocol uses with a client whose highest is
/// `client`: the lower of the two, if both know it (none below 1).
#[must_use]
pub fn negotiate(client: u32) -> Option<u32> {
    (client >= 1).then(|| client.min(VERSION))
}

/// The most one frame carries: longer output goes in several.
pub const MAX_PAYLOAD: usize = 32 * 1024;

/// A frame's kind, length: the bytes before its payload.
const HEADER: usize = 5;

/// One message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Frame {
    /// Client, first: its version, and the keys that detach it.
    Hello {
        /// The client's protocol version.
        version: u32,
        /// The bytes that detach it (empty: no keys detach it).
        detach_keys: Vec<u8>,
    },
    /// Client: as [`Hello`](Self::Hello), for a client the container waits
    /// for (a foreground run's own, stage 9, step 9.4c3a): never let go of
    /// for falling behind. While it's behind, the container's output isn't
    /// read (the container waits, as on a full pipe); while the container's
    /// input is full, what it types isn't read.
    HelloHeld {
        /// The client's protocol version.
        version: u32,
        /// The bytes that detach it (empty: no keys detach it).
        detach_keys: Vec<u8>,
    },
    /// Client: what it typed, for the container's stdin.
    Stdin(Vec<u8>),
    /// Client: its own input has ended (`attach < file`): it types no more.
    /// With `create --stdin-once`, the container's stdin is then closed,
    /// once what the client typed has been written.
    StdinEnd,
    /// Client: its terminal's size, for the container's.
    Resize {
        /// Rows.
        rows: u16,
        /// Columns.
        cols: u16,
    },
    /// Client, first (version 2): a control client, with its version. It's
    /// sent no output, and takes no input. A stopper is there to send one
    /// [`Frame::Stop`] at once (stoppers and waiters are capped apart, so
    /// waiters never keep a stop out). A holder has its own hello
    /// ([`Frame::HelloHolder`]).
    HelloControl {
        /// The client's protocol version.
        version: u32,
        /// Whether it's a stopper (else a waiter).
        stopper: bool,
    },
    /// Control client: stop the container: `signal` to its init now, and
    /// SIGKILL once `timeout_secs` have passed (0: at once) if it's still
    /// there. The monitor sees it through, whatever becomes of the client.
    Stop {
        /// The signal's number.
        signal: u8,
        /// Seconds before the SIGKILL.
        timeout_secs: u32,
    },
    /// Client, first (version 3): a holder, with its version. It sends one
    /// [`Frame::Hold`] or [`Frame::Release`], is answered [`Frame::Done`],
    /// and goes.
    HelloHolder {
        /// The client's protocol version.
        version: u32,
    },
    /// Holder: keep the descriptor passed with this frame (`SCM_RIGHTS`)
    /// until the container has ended, by this label ([`is_label`]); then
    /// it's the exit command's.
    Hold {
        /// What it's called.
        label: String,
    },
    /// Holder: let go of the descriptor held by this label (one given at
    /// `create` too).
    Release {
        /// What it's called.
        label: String,
    },
    /// Client of a forwarded command (step 10.3b): a signal this side got
    /// (a Ctrl-C, a kill), for the command; only a forwardable one
    /// ([`crate::process::is_forwardable`]) is passed on.
    Signal(u8),
    /// Client, first: a command to run, for the macOS guest's agent (step
    /// 10.3: the Mac CLI's commands, forwarded). A container's monitor
    /// never lets one in. Its request is a [`forward::Request`], encoded:
    /// at most [`forward::MAX_REQUEST`] bytes.
    HelloRun {
        /// The client's protocol version.
        version: u32,
        /// The request, as [`forward::Request::encode`] makes it.
        request: Vec<u8>,
    },
    /// Monitor, to a holder: whether its hold or release was done, and if
    /// not, why; then closed. A refusal to a version 4 holder is
    /// [`NotDone`](Self::NotDone) instead.
    Done {
        /// Whether it was.
        ok: bool,
        /// Why not (empty if it was).
        why: String,
    },
    /// Monitor, to a holder of version 4 or later: its hold or release
    /// refused, why, and in words; then closed.
    NotDone {
        /// Why.
        reason: HoldRefusal,
        /// Why, in words.
        why: String,
    },
    /// Monitor, to a client let in.
    Welcome {
        /// The monitor's protocol version.
        version: u32,
        /// Whether the container has a terminal (its output is one stream,
        /// and raw mode makes sense).
        terminal: bool,
        /// Whether it takes input (`create --stdin`).
        stdin: bool,
    },
    /// Monitor, to a client not let in (or a control client whose stop
    /// can't be), then closed.
    Refused(String),
    /// Monitor: the container's standard output (or its terminal's).
    Stdout(Vec<u8>),
    /// Monitor: its standard error.
    Stderr(Vec<u8>),
    /// Monitor: the client typed its detach keys; then closed.
    Detached,
    /// Monitor: the client fell too far behind the output; then closed.
    Dropped(String),
    /// Monitor: the container ended, with this exit code; then closed.
    Exited(i32),
}

/// A frame that can't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// A kind this protocol doesn't have.
    #[error("an attach frame of unknown kind {0}")]
    Kind(u8),
    /// Longer than [`MAX_PAYLOAD`].
    #[error("an attach frame of {0} bytes, more than {MAX_PAYLOAD}")]
    TooLong(u32),
    /// A payload of the wrong size for its kind, or text that isn't UTF-8.
    #[error("a malformed attach frame (kind {0})")]
    Malformed(u8),
}

mod kind {
    pub(super) const HELLO: u8 = 1;
    pub(super) const STDIN: u8 = 2;
    pub(super) const RESIZE: u8 = 3;
    pub(super) const STDIN_END: u8 = 4;
    pub(super) const HELLO_CONTROL: u8 = 5;
    pub(super) const STOP: u8 = 6;
    pub(super) const HELLO_HELD: u8 = 7;
    pub(super) const HELLO_HOLDER: u8 = 8;
    pub(super) const HOLD: u8 = 9;
    pub(super) const RELEASE: u8 = 10;
    pub(super) const HELLO_RUN: u8 = 11;
    pub(super) const SIGNAL: u8 = 12;
    pub(super) const WELCOME: u8 = 16;
    pub(super) const REFUSED: u8 = 17;
    pub(super) const STDOUT: u8 = 18;
    pub(super) const STDERR: u8 = 19;
    pub(super) const DETACHED: u8 = 20;
    pub(super) const DROPPED: u8 = 21;
    pub(super) const EXITED: u8 = 22;
    pub(super) const DONE: u8 = 23;
    pub(super) const NOT_DONE: u8 = 24;
}

impl Frame {
    /// Appends the frame to `out`. A payload longer than [`MAX_PAYLOAD`]
    /// (output, input) goes as several frames of the same kind.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Hello {
                version,
                detach_keys,
            }
            | Self::HelloHeld {
                version,
                detach_keys,
            } => {
                let mut payload = version.to_be_bytes().to_vec();
                payload.extend_from_slice(detach_keys);
                let kind = if matches!(self, Self::HelloHeld { .. }) {
                    kind::HELLO_HELD
                } else {
                    kind::HELLO
                };
                put(out, kind, &payload);
            }
            Self::Stdin(bytes) => put_split(out, kind::STDIN, bytes),
            Self::Resize { rows, cols } => {
                let mut payload = rows.to_be_bytes().to_vec();
                payload.extend_from_slice(&cols.to_be_bytes());
                put(out, kind::RESIZE, &payload);
            }
            Self::HelloControl { version, stopper } => {
                let mut payload = version.to_be_bytes().to_vec();
                payload.push(u8::from(*stopper));
                put(out, kind::HELLO_CONTROL, &payload);
            }
            Self::Stop {
                signal,
                timeout_secs,
            } => {
                let mut payload = vec![*signal];
                payload.extend_from_slice(&timeout_secs.to_be_bytes());
                put(out, kind::STOP, &payload);
            }
            Self::HelloHolder { version } => put(out, kind::HELLO_HOLDER, &version.to_be_bytes()),
            Self::Hold { label } => put(out, kind::HOLD, truncated_to(label, MAX_LABEL)),
            Self::Release { label } => put(out, kind::RELEASE, truncated_to(label, MAX_LABEL)),
            Self::Signal(signal) => put(out, kind::SIGNAL, &[*signal]),
            Self::HelloRun { version, request } => {
                // (A request past `forward::MAX_REQUEST` makes a frame too
                // long to read: its encoder refuses one first.)
                let mut payload = version.to_be_bytes().to_vec();
                payload.extend_from_slice(request);
                put(out, kind::HELLO_RUN, &payload);
            }
            Self::Done { ok, why } => {
                let mut payload = vec![u8::from(*ok)];
                payload.extend_from_slice(truncated_to(why, MAX_PAYLOAD - 1));
                put(out, kind::DONE, &payload);
            }
            Self::NotDone { reason, why } => {
                let mut payload = vec![reason.code()];
                payload.extend_from_slice(truncated_to(why, MAX_PAYLOAD - 1));
                put(out, kind::NOT_DONE, &payload);
            }
            Self::Welcome {
                version,
                terminal,
                stdin,
            } => {
                let mut payload = version.to_be_bytes().to_vec();
                payload.push(u8::from(*terminal) | (u8::from(*stdin) << 1));
                put(out, kind::WELCOME, &payload);
            }
            Self::Refused(reason) => put(out, kind::REFUSED, truncated(reason)),
            Self::Stdout(bytes) => put_split(out, kind::STDOUT, bytes),
            Self::Stderr(bytes) => put_split(out, kind::STDERR, bytes),
            Self::StdinEnd => put(out, kind::STDIN_END, &[]),
            Self::Detached => put(out, kind::DETACHED, &[]),
            Self::Dropped(reason) => put(out, kind::DROPPED, truncated(reason)),
            Self::Exited(code) => put(out, kind::EXITED, &code.to_be_bytes()),
        }
    }

    /// Appends output (`stderr` or not) as frames, without making a frame
    /// first: what the monitor does for every read of the container.
    pub fn encode_output(stderr: bool, bytes: &[u8], out: &mut Vec<u8>) {
        put_split(out, if stderr { kind::STDERR } else { kind::STDOUT }, bytes);
    }

    fn decode(kind: u8, payload: &[u8]) -> Result<Self, FrameError> {
        let malformed = || FrameError::Malformed(kind);
        let text = || {
            std::str::from_utf8(payload)
                .map(str::to_owned)
                .map_err(|_| malformed())
        };
        Ok(match kind {
            kind::HELLO | kind::HELLO_HELD => {
                let (version, keys) = payload.split_first_chunk::<4>().ok_or_else(malformed)?;
                if keys.len() > DetachKeys::MAX {
                    return Err(malformed());
                }
                let (version, detach_keys) = (u32::from_be_bytes(*version), keys.to_vec());
                if kind == kind::HELLO_HELD {
                    Self::HelloHeld {
                        version,
                        detach_keys,
                    }
                } else {
                    Self::Hello {
                        version,
                        detach_keys,
                    }
                }
            }
            kind::SIGNAL => {
                let [signal] = payload else {
                    return Err(malformed());
                };
                Self::Signal(*signal)
            }
            kind::HELLO_RUN => {
                let (version, request) = payload.split_first_chunk::<4>().ok_or_else(malformed)?;
                Self::HelloRun {
                    version: u32::from_be_bytes(*version),
                    request: request.to_vec(),
                }
            }
            kind::STDIN => Self::Stdin(payload.to_vec()),
            kind::RESIZE => {
                let [r0, r1, c0, c1] = payload else {
                    return Err(malformed());
                };
                Self::Resize {
                    rows: u16::from_be_bytes([*r0, *r1]),
                    cols: u16::from_be_bytes([*c0, *c1]),
                }
            }
            kind::HELLO_CONTROL => {
                let [v0, v1, v2, v3, stopper @ (0 | 1)] = payload else {
                    return Err(malformed());
                };
                Self::HelloControl {
                    version: u32::from_be_bytes([*v0, *v1, *v2, *v3]),
                    stopper: *stopper == 1,
                }
            }
            kind::STOP => {
                let [signal, t0, t1, t2, t3] = payload else {
                    return Err(malformed());
                };
                Self::Stop {
                    signal: *signal,
                    timeout_secs: u32::from_be_bytes([*t0, *t1, *t2, *t3]),
                }
            }
            kind::HELLO_HOLDER | kind::HOLD | kind::RELEASE | kind::DONE | kind::NOT_DONE => {
                return Self::decode_holders(kind, payload);
            }
            kind::WELCOME => {
                let [v0, v1, v2, v3, flags] = payload else {
                    return Err(malformed());
                };
                if flags & !0b11 != 0 {
                    return Err(malformed());
                }
                Self::Welcome {
                    version: u32::from_be_bytes([*v0, *v1, *v2, *v3]),
                    terminal: flags & 1 != 0,
                    stdin: flags & 2 != 0,
                }
            }
            kind::REFUSED => Self::Refused(text()?),
            kind::STDOUT => Self::Stdout(payload.to_vec()),
            kind::STDERR => Self::Stderr(payload.to_vec()),
            kind::STDIN_END if payload.is_empty() => Self::StdinEnd,
            kind::DETACHED if payload.is_empty() => Self::Detached,
            kind::DROPPED => Self::Dropped(text()?),
            kind::EXITED => {
                let code: [u8; 4] = payload.try_into().map_err(|_| malformed())?;
                Self::Exited(i32::from_be_bytes(code))
            }
            kind::DETACHED | kind::STDIN_END => return Err(malformed()),
            other => return Err(FrameError::Kind(other)),
        })
    }
}

impl Frame {
    /// A holder's frames, and the monitor's answer to one (version 3; a
    /// refusal with a reason, version 4).
    fn decode_holders(kind: u8, payload: &[u8]) -> Result<Self, FrameError> {
        let malformed = || FrameError::Malformed(kind);
        let label = || {
            std::str::from_utf8(payload)
                .ok()
                .filter(|label| is_label(label))
                .map(str::to_owned)
                .ok_or_else(malformed)
        };
        Ok(match kind {
            kind::HELLO_HOLDER => {
                let version: [u8; 4] = payload.try_into().map_err(|_| malformed())?;
                Self::HelloHolder {
                    version: u32::from_be_bytes(version),
                }
            }
            kind::HOLD => Self::Hold { label: label()? },
            kind::RELEASE => Self::Release { label: label()? },
            kind::DONE => {
                let (&ok @ (0 | 1), why) = payload.split_first().ok_or_else(malformed)? else {
                    return Err(malformed());
                };
                Self::Done {
                    ok: ok == 1,
                    why: std::str::from_utf8(why)
                        .map_err(|_| malformed())?
                        .to_owned(),
                }
            }
            kind::NOT_DONE => {
                let (&code, why) = payload.split_first().ok_or_else(malformed)?;
                Self::NotDone {
                    reason: HoldRefusal::from_code(code),
                    why: std::str::from_utf8(why)
                        .map_err(|_| malformed())?
                        .to_owned(),
                }
            }
            _ => return Err(malformed()),
        })
    }
}

/// A reason, cut to what one frame holds (on a character boundary).
fn truncated(reason: &str) -> &[u8] {
    truncated_to(reason, MAX_PAYLOAD)
}

/// `reason`, cut to at most `max` bytes (on a character boundary).
fn truncated_to(reason: &str, max: usize) -> &[u8] {
    let mut end = reason.len().min(max);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason.as_bytes().get(..end).unwrap_or_default()
}

fn put(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.push(kind);
    // (At most MAX_PAYLOAD: it fits.)
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(payload);
}

fn put_split(out: &mut Vec<u8>, kind: u8, bytes: &[u8]) {
    for chunk in bytes.chunks(MAX_PAYLOAD) {
        put(out, kind, chunk);
    }
}

/// Reads frames from a byte stream as it comes. Holds what isn't yet a
/// whole frame ([`Decoder::held`]), and, until the next push, the frames
/// already taken from the last one.
///
/// Frames are read from an offset, and what they took is dropped once, at
/// the next push: dropped frame by frame, the rest would move each time,
/// and a read of many small frames would cost the square of its length.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    /// Where the next frame starts in `buffer`: before it, frames taken.
    start: usize,
}

impl Decoder {
    /// A decoder with nothing read yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.drain(..self.start);
        self.start = 0;
        self.buffer.extend_from_slice(bytes);
    }

    /// How many bytes are held, not yet a whole frame.
    #[must_use]
    pub fn held(&self) -> usize {
        self.unread().len()
    }

    /// What's pushed and not yet taken as a frame: once a stream carries
    /// raw bytes after its frames, the first of them.
    #[must_use]
    pub fn unread(&self) -> &[u8] {
        self.buffer.get(self.start..).unwrap_or_default()
    }

    /// The next whole frame, if one is there.
    ///
    /// # Errors
    ///
    /// [`FrameError`]. The frame's bytes are taken either way: after
    /// [`FrameError::Kind`] (a newer peer's frame) the stream goes on; after
    /// the others it isn't this protocol, and nothing after can be trusted.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let unread = self.unread();
        let Some((&kind, rest)) = unread.split_first() else {
            return Ok(None);
        };
        let Some((length, _)) = rest.split_first_chunk::<4>() else {
            return Ok(None);
        };
        let length = u32::from_be_bytes(*length);
        let size = usize::try_from(length).unwrap_or(usize::MAX);
        if size > MAX_PAYLOAD {
            return Err(FrameError::TooLong(length));
        }
        let Some(payload) = unread.get(HEADER..HEADER + size) else {
            return Ok(None);
        };
        let frame = Frame::decode(kind, payload);
        self.start += HEADER + size;
        frame.map(Some)
    }
}

/// The keys that detach a client, as bytes typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachKeys(Vec<u8>);

/// Detach keys that can't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "detach keys {0:?}: give keys separated by commas, each a character or ctrl- and a \
     letter or one of @[\\]^_ (like ctrl-p,ctrl-q), at most 8"
)]
pub struct DetachKeysError(pub String);

impl DetachKeys {
    /// The most keys.
    pub const MAX: usize = 8;

    /// No keys: nothing typed detaches (a client whose input may be any
    /// bytes, such as a foreground run's piped input).
    #[must_use]
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// Ctrl-P Ctrl-Q, Docker's.
    #[must_use]
    pub fn default_keys() -> Self {
        Self(vec![0x10, 0x11])
    }

    /// Reads Docker's form: keys separated by commas, each a single
    /// character or `ctrl-` and a letter or one of `@[\]^_`. Empty: no keys,
    /// nothing detaches (Podman's).
    ///
    /// # Errors
    ///
    /// [`DetachKeysError`]: anything else, or more than [`MAX`](Self::MAX).
    pub fn parse(text: &str) -> Result<Self, DetachKeysError> {
        let bad = || DetachKeysError(text.to_owned());
        if text.is_empty() {
            return Ok(Self(Vec::new()));
        }
        let mut keys = Vec::new();
        for key in text.split(',') {
            let byte = if let Some(name) = key.strip_prefix("ctrl-") {
                let [byte] = name.as_bytes() else {
                    return Err(bad());
                };
                match byte.to_ascii_uppercase() {
                    upper @ (b'@' | b'A'..=b'Z' | b'[' | b'\\' | b']' | b'^' | b'_') => {
                        upper - b'@'
                    }
                    _ => return Err(bad()),
                }
            } else {
                match key.as_bytes() {
                    [byte] if byte.is_ascii() && !byte.is_ascii_control() => *byte,
                    _ => return Err(bad()),
                }
            };
            keys.push(byte);
        }
        if keys.len() > Self::MAX {
            return Err(bad());
        }
        Ok(Self(keys))
    }

    /// The keys, as typed.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Keys a client sent ([`Frame::Hello`]): any bytes, at most
    /// [`MAX`](Self::MAX); none means nothing detaches it.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() <= Self::MAX).then(|| Self(bytes.to_vec()))
    }

    /// A matcher watching a stream of typing for these keys.
    #[must_use]
    pub fn matcher(&self) -> Matcher {
        Matcher {
            keys: self.0.clone(),
            matched: 0,
        }
    }
}

/// Watches what a client types for its detach keys.
#[derive(Debug, Clone)]
pub struct Matcher {
    keys: Vec<u8>,
    /// How many of the keys the latest typing ends with, held back.
    matched: usize,
}

impl Matcher {
    /// Takes `typed`, appending to `out` what goes to the container. Returns
    /// whether the keys were typed: then `out` has what came before them,
    /// and nothing of them (nor what came after, in `typed`). Keys that
    /// begin to match are held back, across calls, until they match or not.
    pub fn feed(&mut self, typed: &[u8], out: &mut Vec<u8>) -> bool {
        if self.keys.is_empty() {
            out.extend_from_slice(typed);
            return false;
        }
        // What's held back is always the keys' first `matched` bytes. On a
        // mismatch, the held bytes and the new one are given up from the
        // front only until what's left begins the keys again (so `a,a,b`
        // typed as `a a a b` still detaches). The keys are at most 8 bytes:
        // the simple search is enough.
        let mut held = [0_u8; DetachKeys::MAX];
        for &byte in typed {
            let mut len = self.matched;
            held[..len].copy_from_slice(&self.keys[..len]);
            let Some(slot) = held.get_mut(len) else {
                return false;
            };
            *slot = byte;
            len += 1;
            let mut start = 0;
            while !self.keys.starts_with(&held[start..len]) {
                out.push(held[start]);
                start += 1;
            }
            self.matched = len - start;
            if self.matched == self.keys.len() {
                self.matched = 0;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn all() -> Vec<Frame> {
        vec![
            Frame::Hello {
                version: VERSION,
                detach_keys: vec![0x10, 0x11],
            },
            Frame::HelloHeld {
                version: VERSION,
                detach_keys: Vec::new(),
            },
            Frame::Stdin(b"typed".to_vec()),
            Frame::StdinEnd,
            Frame::Resize {
                rows: 30,
                cols: 100,
            },
            Frame::HelloControl {
                version: VERSION,
                stopper: true,
            },
            Frame::Stop {
                signal: 15,
                timeout_secs: 10,
            },
            Frame::HelloHolder { version: VERSION },
            Frame::HelloRun {
                version: VERSION,
                request: br#"{"version":1}"#.to_vec(),
            },
            Frame::Signal(2),
            Frame::Hold {
                label: "lease:app".to_owned(),
            },
            Frame::Release {
                label: "lease:app".to_owned(),
            },
            Frame::Done {
                ok: true,
                why: String::new(),
            },
            Frame::Done {
                ok: false,
                why: "no hold lease:x".to_owned(),
            },
            Frame::NotDone {
                reason: HoldRefusal::Full,
                why: "2100 descriptors are held already".to_owned(),
            },
            Frame::NotDone {
                reason: HoldRefusal::Other(200),
                why: String::new(),
            },
            Frame::Welcome {
                version: VERSION,
                terminal: true,
                stdin: false,
            },
            Frame::Refused("16 clients already".to_owned()),
            Frame::Stdout(b"out".to_vec()),
            Frame::Stderr(b"err".to_vec()),
            Frame::Detached,
            Frame::Dropped("behind".to_owned()),
            Frame::Exited(-9),
        ]
    }

    /// A flood of the smallest frames in one push comes out whole, without
    /// moving the rest after each frame: what's taken stays until the next
    /// push drops it at once, and the time is a decoder's that moves
    /// nothing (moving the rest each time, 4 MiB of 5-byte frames would
    /// move about 1.6 TiB) (F40).
    #[test]
    fn a_flood_of_small_frames_is_read_in_one_pass() {
        let mut stream = Vec::new();
        while stream.len() < 4 << 20 {
            Frame::StdinEnd.encode(&mut stream);
        }
        let frames = stream.len() / HEADER;
        let mut decoder = Decoder::new();
        let started = std::time::Instant::now();
        decoder.push(&stream);
        let mut read = 0;
        while let Some(frame) = decoder.next_frame().unwrap() {
            assert_eq!(frame, Frame::StdinEnd);
            read += 1;
        }
        assert_eq!(read, frames);
        // Taken, not moved: still there until the next push.
        assert_eq!(decoder.buffer.len(), stream.len());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(decoder.held(), 0);
        // Half a frame after: held, and the rest completes it.
        let mut one = Vec::new();
        Frame::Resize { rows: 24, cols: 80 }.encode(&mut one);
        decoder.push(&one[..3]);
        assert_eq!(decoder.held(), 3);
        assert_eq!(decoder.next_frame().unwrap(), None);
        decoder.push(&one[3..]);
        assert_eq!(
            decoder.next_frame().unwrap(),
            Some(Frame::Resize { rows: 24, cols: 80 })
        );
        assert_eq!(decoder.held(), 0);
        assert!(decoder.buffer.len() <= one.len(), "taken bytes dropped");
    }

    /// Every refusal's number reads back as itself; one not known is
    /// `Other`, kept; an empty `NotDone` is malformed.
    #[test]
    fn hold_refusals_keep_their_numbers() {
        for code in 0..=u8::MAX {
            assert_eq!(HoldRefusal::from_code(code).code(), code);
        }
        assert_eq!(HoldRefusal::from_code(3), HoldRefusal::Full);
        assert_eq!(HoldRefusal::from_code(99), HoldRefusal::Other(99));
        assert_eq!(
            Frame::decode(kind::NOT_DONE, &[]),
            Err(FrameError::Malformed(kind::NOT_DONE))
        );
    }

    /// A refusal's reason too long for one frame is cut to fit it (at a
    /// character's boundary): the frame is still the protocol, and reads
    /// back with as much of the reason as fits.
    #[test]
    fn a_long_reason_is_cut_to_one_frame() {
        let why = "é".repeat(MAX_PAYLOAD);
        let mut stream = Vec::new();
        Frame::Done {
            ok: false,
            why: why.clone(),
        }
        .encode(&mut stream);
        assert!(stream.len() <= HEADER + MAX_PAYLOAD, "{}", stream.len());
        let mut decoder = Decoder::new();
        decoder.push(&stream);
        let Ok(Some(Frame::Done {
            ok: false,
            why: read,
        })) = decoder.next_frame()
        else {
            panic!("not read back");
        };
        assert!(!read.is_empty() && why.starts_with(&read));
        assert!(read.len() >= MAX_PAYLOAD - 2, "{}", read.len());
    }

    #[test]
    fn frames_round_trip_whole_or_in_pieces() {
        let mut stream = Vec::new();
        for frame in all() {
            frame.encode(&mut stream);
        }
        // One byte at a time: frames come out as they're completed.
        let mut decoder = Decoder::new();
        let mut read = Vec::new();
        for byte in &stream {
            decoder.push(std::slice::from_ref(byte));
            while let Some(frame) = decoder.next_frame().unwrap() {
                read.push(frame);
            }
        }
        assert_eq!(read, all());
        assert_eq!(decoder.held(), 0);
    }

    #[test]
    fn long_output_goes_in_several_frames() {
        let mut stream = Vec::new();
        Frame::encode_output(false, &vec![b'x'; MAX_PAYLOAD * 2 + 1], &mut stream);
        let mut decoder = Decoder::new();
        decoder.push(&stream);
        let mut sizes = Vec::new();
        while let Some(Frame::Stdout(bytes)) = decoder.next_frame().unwrap() {
            sizes.push(bytes.len());
        }
        assert_eq!(sizes, [MAX_PAYLOAD, MAX_PAYLOAD, 1]);
    }

    /// A hold's label: letters, digits and `:_.-`, 1 to 128 bytes.
    #[test]
    fn a_label_is_short_and_plain() {
        for good in [
            "lease:app",
            "lease:App",
            "port",
            "run-dir",
            "area:a.b_c",
            "fd-3",
        ] {
            assert!(is_label(good), "{good}");
        }
        let long = "a".repeat(MAX_LABEL + 1);
        for bad in ["", "a,b", "a=b", "a b", "é", "a/b", long.as_str()] {
            assert!(!is_label(bad), "{bad}");
        }
        assert!(is_label(&"a".repeat(MAX_LABEL)));
    }

    #[test]
    fn what_isnt_the_protocol_is_refused() {
        let decode = |bytes: &[u8]| {
            let mut decoder = Decoder::new();
            decoder.push(bytes);
            decoder.next_frame()
        };
        assert_eq!(decode(&[99, 0, 0, 0, 0]), Err(FrameError::Kind(99)));
        assert_eq!(
            decode(&[kind::STDOUT, 0, 0, 0x80, 1]),
            Err(FrameError::TooLong(0x8001))
        );
        assert_eq!(
            decode(&[kind::RESIZE, 0, 0, 0, 1, 7]),
            Err(FrameError::Malformed(kind::RESIZE))
        );
        assert_eq!(
            decode(&[kind::DETACHED, 0, 0, 0, 1, 7]),
            Err(FrameError::Malformed(kind::DETACHED))
        );
        assert_eq!(
            decode(&[kind::STOP, 0, 0, 0, 4, 15, 0, 0, 0]),
            Err(FrameError::Malformed(kind::STOP))
        );
        assert_eq!(
            decode(&[kind::HELLO_CONTROL, 0, 0, 0, 5, 0, 0, 0, 2, 2]),
            Err(FrameError::Malformed(kind::HELLO_CONTROL))
        );
        assert_eq!(
            decode(&[kind::REFUSED, 0, 0, 0, 1, 0xff]),
            Err(FrameError::Malformed(kind::REFUSED))
        );
        // A holder's: its version is four bytes; a label is one (see
        // `is_label`); `Done` says yes or no.
        // A signal is one byte.
        assert_eq!(
            decode(&[kind::SIGNAL, 0, 0, 0, 2, 2, 2]),
            Err(FrameError::Malformed(kind::SIGNAL))
        );
        assert_eq!(
            decode(&[kind::HELLO_HOLDER, 0, 0, 0, 3, 0, 0, 3]),
            Err(FrameError::Malformed(kind::HELLO_HOLDER))
        );
        for label in [&b""[..], b"a,b", b"a=b", b"a b"] {
            let mut frame = vec![kind::HOLD, 0, 0, 0, u8::try_from(label.len()).unwrap()];
            frame.extend_from_slice(label);
            assert_eq!(
                decode(&frame),
                Err(FrameError::Malformed(kind::HOLD)),
                "{label:?}"
            );
        }
        let mut long = vec![kind::RELEASE, 0, 0, 0, 129];
        long.extend([b'a'; 129]);
        assert_eq!(decode(&long), Err(FrameError::Malformed(kind::RELEASE)));
        assert_eq!(
            decode(&[kind::DONE, 0, 0, 0, 1, 2]),
            Err(FrameError::Malformed(kind::DONE))
        );
        assert_eq!(
            decode(&[kind::DONE, 0, 0, 0, 0]),
            Err(FrameError::Malformed(kind::DONE))
        );
        let mut hello = vec![kind::HELLO, 0, 0, 0, 13, 0, 0, 0, 1];
        hello.extend([1; 9]);
        assert_eq!(
            decode(&hello),
            Err(FrameError::Malformed(kind::HELLO)),
            "more than 8 keys"
        );
        // A newer peer's kind is skipped: what follows is read.
        let mut decoder = Decoder::new();
        decoder.push(&[99, 0, 0, 0, 2, 7, 7, kind::DETACHED, 0, 0, 0, 0]);
        assert_eq!(decoder.next_frame(), Err(FrameError::Kind(99)));
        assert_eq!(decoder.next_frame(), Ok(Some(Frame::Detached)));
        assert_eq!(negotiate(VERSION + 3), Some(VERSION));
        assert_eq!(negotiate(1), Some(1));
        assert_eq!(negotiate(2), Some(2));
        assert_eq!(negotiate(3), Some(HOLD_VERSION));
        assert_eq!(negotiate(4), Some(REASON_VERSION));
        assert_eq!(negotiate(0), None);
        // Not all there yet: nothing, and no error.
        assert_eq!(decode(&[kind::STDOUT, 0, 0, 0, 9, 1]), Ok(None));
    }

    #[test]
    fn detach_keys_are_read_as_docker_writes_them() {
        assert_eq!(
            DetachKeys::parse("ctrl-p,ctrl-q").unwrap(),
            DetachKeys::default_keys()
        );
        assert_eq!(DetachKeys::parse("ctrl-a").unwrap().bytes(), [1]);
        assert_eq!(
            DetachKeys::parse("ctrl-@,ctrl-_,ctrl-\\").unwrap().bytes(),
            [0, 0x1f, 0x1c]
        );
        assert_eq!(DetachKeys::parse("a,ctrl-B").unwrap().bytes(), [b'a', 2]);
        assert_eq!(
            DetachKeys::parse("").unwrap().bytes(),
            b"",
            "none: never detaches"
        );
        for bad in ["ctrl-", "ctrl-1", "ab", "ctrl-p,", "é", "a,a,a,a,a,a,a,a,a"] {
            assert!(DetachKeys::parse(bad).is_err(), "{bad:?}");
        }
    }

    fn feed_all(keys: &[u8], typed: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut matcher = DetachKeys(keys.to_vec()).matcher();
        let mut out = Vec::new();
        for piece in typed {
            if matcher.feed(piece, &mut out) {
                return (out, true);
            }
        }
        (out, false)
    }

    #[test]
    fn detach_keys_are_found_and_never_passed_on() {
        let keys = [0x10, 0x11];
        assert_eq!(feed_all(&keys, &[b"ab\x10\x11cd"]), (b"ab".to_vec(), true));
        // Across reads.
        assert_eq!(
            feed_all(&keys, &[b"ab\x10", b"\x11"]),
            (b"ab".to_vec(), true)
        );
        // The first key alone, then something else: passed on, nothing held.
        assert_eq!(
            feed_all(&keys, &[b"a\x10", b"b"]),
            (b"a\x10b".to_vec(), false)
        );
        // The first key twice: the second may start the keys.
        assert_eq!(
            feed_all(&keys, &[b"\x10\x10\x11"]),
            (b"\x10".to_vec(), true)
        );
        // No keys: everything goes on.
        assert_eq!(feed_all(&[], &[b"\x10\x11"]), (b"\x10\x11".to_vec(), false));
        assert_eq!(feed_all(b"q", &[b"abq"]), (b"ab".to_vec(), true));
        // A mismatch keeps what can still begin the keys: `a a a b` holds
        // `a a` after the third `a`, and detaches on the `b`.
        assert_eq!(feed_all(b"aab", &[b"xaaab"]), (b"xa".to_vec(), true));
        assert_eq!(
            feed_all(b"aab", &[b"a", b"a", b"a", b"b"]),
            (b"a".to_vec(), true)
        );
        assert_eq!(feed_all(b"abac", &[b"ababac"]), (b"ab".to_vec(), true));
    }

    /// What the matcher must do, the slow way: everything before the keys'
    /// first appearance goes on; without them, all but the longest end that
    /// begins the keys (held back, waiting).
    fn reference(keys: &[u8], typed: &[u8]) -> (Vec<u8>, bool) {
        if let Some(at) = typed.windows(keys.len()).position(|window| window == keys) {
            return (typed[..at].to_vec(), true);
        }
        let held = (0..keys.len().min(typed.len() + 1))
            .rev()
            .find(|&len| typed.ends_with(&keys[..len]))
            .unwrap_or(0);
        (typed[..typed.len() - held].to_vec(), false)
    }

    proptest! {
        #[test]
        fn any_bytes_are_read_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let mut decoder = Decoder::new();
            decoder.push(&bytes);
            while let Ok(Some(_)) = decoder.next_frame() {}
        }

        /// Without the keys in it, typing goes on whole, however it's cut.
        #[test]
        fn typing_without_the_keys_goes_on_whole(
            typed in proptest::collection::vec(any::<u8>().prop_filter("not ctrl-q", |b| *b != 0x11), 0..100),
            cut in 0..100_usize,
        ) {
            let cut = cut.min(typed.len());
            let (out, detached) = feed_all(&[0x10, 0x11], &[&typed[..cut], &typed[cut..]]);
            // A trailing ctrl-p is held back, waiting for its ctrl-q.
            let expected = typed.strip_suffix(&[0x10]).unwrap_or(&typed);
            prop_assert!(!detached);
            prop_assert_eq!(out, expected.to_vec());
        }

        /// Over a small alphabet (so the keys repeat themselves), however
        /// the typing is cut, the matcher does what the slow way does.
        #[test]
        fn the_matcher_finds_the_keys_wherever_they_are(
            keys in proptest::collection::vec(0..3_u8, 1..=DetachKeys::MAX),
            typed in proptest::collection::vec(0..3_u8, 0..60),
            cut in 0..60_usize,
        ) {
            let cut = cut.min(typed.len());
            prop_assert_eq!(
                feed_all(&keys, &[&typed[..cut], &typed[cut..]]),
                reference(&keys, &typed)
            );
        }
    }
}
