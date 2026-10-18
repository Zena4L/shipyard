//! The events journal (stage 9, step 9.6a): what happened to containers,
//! images, volumes and networks, for `shipyard events` (Docker's).
//!
//! There's no daemon to hear about everything, so every process that
//! changes something appends what it did to one journal per store: a
//! command, a container's clean-up as it ends, a restart that waits. Each
//! event is one line of JSON, written with one `write` to a file opened
//! with `O_APPEND`: the kernel puts each such write whole at the file's end,
//! whoever else appends (TLPI §5.1), and a line is kept small (at most
//! [`MAX_LINE`]) so it's one write. The journal is rotated by size into one
//! older file: appenders hold a shared `flock` on `events.lock` while they
//! write, and the one that rotates takes it exclusively, without waiting
//! (busy: the next appender rotates). Each line carries its format's
//! version (the ROADMAP's H2): a reader skips the ones it doesn't know.
//!
//! An event's time is when it happened, not when it was written: a
//! container's end is written by its clean-up with the time its monitor saw
//! it, and one nothing recorded with the time recovery gives it. Readers
//! keep the journal's order and filter by time.

mod emit;
mod filter;
mod read;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[cfg_attr(
    not(target_os = "linux"),
    allow(
        unused_imports,
        reason = "only Linux code and the tests call it from outside"
    )
)]
pub(crate) use emit::emit;
#[cfg(target_os = "linux")]
pub(crate) use emit::{container, container_ended};
pub(crate) use emit::{emit_about, emit_pruned};
pub use filter::Filters;
pub use read::{EventsOptions, read};

/// The journal's line format version: what's written.
pub(crate) const VERSION: u32 = 1;
/// The longest line written: one `write` puts it whole.
pub const MAX_LINE: usize = 4096;
/// The journal's file.
const FILE: &str = "events.log";
/// Its one older file, after a rotation.
const ROTATED: &str = "events.log.1";
/// The lock appenders share and a rotation takes alone.
const LOCK: &str = "events.lock";
/// The size past which the journal is rotated.
const ROTATE_AT: u64 = 1024 * 1024;
/// How often a follower looks for new events.
const POLL: Duration = Duration::from_millis(200);

/// What an event is about (Docker's types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Kind {
    /// A container.
    Container,
    /// An image.
    Image,
    /// A named volume.
    Volume,
    /// A network.
    Network,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Container => "container",
            Self::Image => "image",
            Self::Volume => "volume",
            Self::Network => "network",
        })
    }
}

impl std::str::FromStr for Kind {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        Ok(match text {
            "container" => Self::Container,
            "image" => Self::Image,
            "volume" => Self::Volume,
            "network" => Self::Network,
            _ => crate::error::bail!(
                InvalidRequest,
                "event type {text:?}: give container, image, volume or network"
            ),
        })
    }
}

/// One event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Event {
    /// When it happened.
    pub time: SystemTime,
    /// What it's about.
    pub kind: Kind,
    /// What happened (`start`, `die`, `pull`, …: Docker's words).
    pub action: String,
    /// Its subject's ID (a container's, an image's digest, a volume's or a
    /// network's name).
    pub id: String,
    /// More about it (`name`, `image`, `exitCode`, `signal`, …).
    pub attributes: BTreeMap<String, String>,
}

impl Event {
    /// An event about `kind` `id`: `action`, now.
    #[must_use]
    pub fn new(kind: Kind, action: &str, id: &str) -> Self {
        Self {
            time: SystemTime::now(),
            kind,
            action: action.to_owned(),
            id: id.to_owned(),
            attributes: BTreeMap::new(),
        }
    }

    /// This event, with `key` said to be `value`.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        self.attributes.insert(key.to_owned(), value.to_string());
        self
    }

    /// This event, as having happened at `time`.
    #[must_use]
    pub fn at(mut self, time: SystemTime) -> Self {
        self.time = time;
        self
    }

    /// Its line, as written (without the newline): under [`MAX_LINE`]
    /// bytes with it. Its ID and action are cut to [`MAX_ID`] and
    /// [`MAX_ACTION`] bytes (even escaped, they fit), then its attributes'
    /// values, the longest first, until it fits.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line = V1 {
            v: VERSION,
            time: rfc3339(self.time),
            kind: self.kind,
            action: cut(&self.action, MAX_ACTION).to_owned(),
            id: cut(&self.id, MAX_ID).to_owned(),
            attributes: self.attributes.clone(),
        };
        loop {
            let text = serde_json::to_string(&line).unwrap_or_default();
            if text.len() < MAX_LINE {
                return text;
            }
            let Some(longest) = line
                .attributes
                .values_mut()
                .max_by_key(|value| value.len())
                .filter(|value| !value.is_empty())
            else {
                // Can't be (the rest is bounded); it's written as it is.
                return text;
            };
            let half = cut(longest, longest.len() / 2).len();
            longest.truncate(half);
        }
    }

    /// The event a line says, if it's one this Shipyard reads (its
    /// version known, its time readable). Parsed once, as version 1: a line
    /// of another version says another `v`, or has fields version 1 doesn't
    /// (refused: `deny_unknown_fields`), and is skipped either way.
    #[must_use]
    pub(crate) fn from_line(line: &str) -> Option<Self> {
        let read: V1 = serde_json::from_str(line).ok()?;
        if read.v != VERSION {
            return None;
        }
        let time = shipyard_runtime::logs::time::Timestamp::parse(&read.time).ok()?;
        Some(Self {
            time: system_time(time)?,
            kind: read.kind,
            action: read.action,
            id: read.id,
            attributes: read.attributes,
        })
    }
}

/// The longest ID a line keeps, in bytes.
pub const MAX_ID: usize = 512;
/// The longest action a line keeps, in bytes.
pub const MAX_ACTION: usize = 64;

/// `text`'s first `budget` bytes at most, ending on a character.
fn cut(text: &str, budget: usize) -> &str {
    let mut end = budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Version 1 of a line, as it's written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    v: u32,
    time: String,
    #[serde(rename = "type")]
    kind: Kind,
    action: String,
    id: String,
    attributes: BTreeMap<String, String>,
}

/// `time` in RFC 3339, UTC, to the nanosecond (as the journal writes it).
#[must_use]
pub fn rfc3339(time: SystemTime) -> String {
    let since = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    shipyard_runtime::logs::time::Timestamp::from_unix(
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        since.subsec_nanos(),
    )
    .map(|stamp| stamp.to_string())
    .unwrap_or_default()
}

/// `time` as the system's (none before 1970).
#[must_use]
pub fn system_time(time: shipyard_runtime::logs::time::Timestamp) -> Option<SystemTime> {
    let since = Duration::new(u64::try_from(time.secs()).ok()?, time.nanos());
    Some(SystemTime::UNIX_EPOCH + since)
}
