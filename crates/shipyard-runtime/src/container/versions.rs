//! The versions of the files in a container's state directory, and how
//! each version is read.
//!
//! Every file gets a version on its first write (the ROADMAP's H2), and a
//! reader keeps the previous one: a container made by the previous
//! Shipyard can still be running (its monitor is the previous binary) when
//! this one is installed. A file with no version is stage 8's, version 0.
//!
//! - `state.json`: [`STATE_VERSION`]. Version 0 has the same shape (serde
//!   defaults fill in what it lacks); a version above this one's is
//!   refused ([`ContainerError::UnknownVersion`]), never misread or
//!   rewritten.
//! - `exit`: [`ExitRecord`], JSON since version 1; version 0 was a line of
//!   text.
//! - `killed`: a marker, its existence the whole format. A marker that
//!   ever carries data gets a new name.
//! - `exec.json` and a log directory's `log.json` keep their own versions
//!   (`lifecycle/exec.rs`, `logs/info.rs`).
//! - `plan.json` (a planned container's, `plan.rs`): exactly this binary's
//!   version, as it's read only by the monitor the same binary starts.

#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "written and read by the Linux lifecycle only")
)]

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::{ContainerError, Result};

/// `state.json`'s version.
pub(crate) const STATE_VERSION: u32 = 1;

/// The exit record's version: 2 since 9.4a (`started`).
pub(crate) const EXIT_VERSION: u32 = 2;

/// The most of an exit record read: the monitor writes a few dozen bytes.
pub(crate) const EXIT_MAX: u64 = 64 * 1024;

/// How the words give the code, `… (exit code N)…`: version 0's text,
/// and a later record's `description` (the monitor's `describe_exit`).
pub(super) const EXIT_CODE: &str = "(exit code ";

/// A file's version: its `version` field, 0 if it has none. Not a JSON
/// object: `None`.
pub(super) fn version_of(bytes: &[u8]) -> Option<u32> {
    #[derive(Deserialize)]
    struct Versioned {
        #[serde(default)]
        version: u32,
    }
    serde_json::from_slice::<Versioned>(bytes)
        .ok()
        .map(|versioned| versioned.version)
}

/// Refuses `state.json`'s `bytes` if a newer Shipyard wrote them.
///
/// # Errors
///
/// [`ContainerError::UnknownVersion`]. (Bytes that aren't a record at all
/// are left to the record's own parse, which says what's wrong.)
pub(crate) fn check_state_version(bytes: &[u8]) -> Result<()> {
    match version_of(bytes) {
        Some(found) if found > STATE_VERSION => Err(ContainerError::UnknownVersion {
            file: "state.json",
            found,
        }),
        _ => Ok(()),
    }
}

/// How a container ended, as its monitor recorded it (the state
/// directory's `exit`, written last, once everything else is done). Read
/// one with [`exit_record`](super::exit_record), which knows its versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExitRecord {
    /// The record's version: 2 for one this Shipyard wrote, 1 for 9.2a's
    /// (no `started`), 0 for stage 8's text, more for a newer Shipyard's
    /// (of which only that is known).
    pub version: u32,
    /// How it ended, in words: `exited (exit code 3)`, `killed by SIGKILL
    /// (exit code 137)` (with the out-of-memory killer's part after it if it
    /// struck), or `unknown (why)`.
    pub description: String,
    /// Its exit code (128 + N for a signal), if known.
    pub code: Option<i32>,
    /// What the out-of-memory killer's kill was, if it ended the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom: Option<String>,
    /// When the monitor recorded the end, written as serde writes a
    /// `SystemTime`: `{"secs_since_epoch": …, "nanos_since_epoch": …}`.
    /// Version 0 has none in it (nor a record that can't be read): the
    /// file's modification time is given instead, when it can be read.
    #[serde(default)]
    pub finished: Option<SystemTime>,
    /// Whether it was started (`start` saw its init through the gate: its
    /// command ran); unknown before version 2. A container ended before it started (deleted while
    /// created) ran nothing: its code is its init's, never its command's.
    #[serde(default)]
    pub started: Option<bool>,
}

impl ExitRecord {
    /// A record this Shipyard writes, for a container that `started` or
    /// not.
    pub(crate) fn new(
        description: String,
        code: Option<i32>,
        oom: Option<String>,
        started: bool,
    ) -> Self {
        Self {
            version: EXIT_VERSION,
            description,
            code,
            oom,
            finished: Some(SystemTime::now()),
            started: Some(started),
        }
    }

    /// What's written.
    pub(crate) fn to_json(&self) -> Vec<u8> {
        // Plain data (strings, numbers, a time): only a time before 1970
        // fails (a clock never set), and then it's left out.
        serde_json::to_vec(self)
            .or_else(|_| {
                serde_json::to_vec(&Self {
                    finished: None,
                    ..self.clone()
                })
            })
            .unwrap_or_default()
    }

    /// Reads an exit record of any version: the `exit` file's, or an exit
    /// command's [`EXIT_RECORD_ENV`](super::EXIT_RECORD_ENV). One a newer
    /// Shipyard wrote still says the container ended (that it exists
    /// does), with nothing made up about how.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        match version_of(bytes) {
            Some(version @ (1 | EXIT_VERSION)) => {
                if let Ok(record) = serde_json::from_slice::<Self>(bytes) {
                    // Version 1 can't say whether it started.
                    let started = record.started.filter(|_| version == EXIT_VERSION);
                    return Self { started, ..record };
                }
                Self::unreadable(version, "unreadable exit record")
            }
            Some(found) if found > EXIT_VERSION => {
                Self::unreadable(found, "recorded by a newer Shipyard")
            }
            // JSON without a version: no Shipyard wrote that.
            Some(found) => Self::unreadable(found, "unreadable exit record"),
            None => {
                // Version 0: one line of text (stage 8).
                let text = String::from_utf8_lossy(bytes).trim_end().to_owned();
                Self {
                    version: 0,
                    code: code_in_text(&text),
                    description: text,
                    oom: None,
                    finished: None,
                    started: None,
                }
            }
        }
    }

    fn unreadable(version: u32, why: &str) -> Self {
        Self {
            version,
            description: format!("unknown ({why})"),
            code: None,
            oom: None,
            finished: None,
            started: None,
        }
    }
}

/// The code in version 0's text (none in one that doesn't know it).
fn code_in_text(ended: &str) -> Option<i32> {
    ended
        .split(EXIT_CODE)
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|code| code.parse().ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// Stage 8's text is read for its code: a container made by it may
    /// still be running when this Shipyard is installed, and its monitor
    /// writes the text.
    #[test]
    fn version_0s_text_gives_its_code() {
        let exited = ExitRecord::parse(b"exited (exit code 3)");
        assert_eq!((exited.version, exited.code), (0, Some(3)));
        assert_eq!(exited.description, "exited (exit code 3)");
        let killed = ExitRecord::parse(
            b"killed by signal 9 (SIGKILL) (exit code 137) (the out-of-memory killer)",
        );
        assert_eq!(killed.code, Some(137));
        let unknown = ExitRecord::parse(b"unknown (waiting: ECHILD)");
        assert_eq!((unknown.version, unknown.code), (0, None));
    }

    #[test]
    fn version_2_and_1_read_as_written() {
        let record = ExitRecord::new(
            "exited (exit code 3)".to_owned(),
            Some(3),
            Some("killed by the out-of-memory killer".to_owned()),
            true,
        );
        let json = record.to_json();
        assert_eq!(ExitRecord::parse(&json), record);
        assert!(record.finished.is_some());
        assert_eq!(record.version, 2);
        assert_eq!(record.started, Some(true));
        let never = ExitRecord::new("killed by SIGKILL".to_owned(), Some(137), None, false);
        assert_eq!(ExitRecord::parse(&never.to_json()).started, Some(false));
        // As a reader of an older Shipyard's record would see it.
        let literal = br#"{"version":1,"description":"exited (exit code 0)","code":0,
            "finished":{"secs_since_epoch":1760000000,"nanos_since_epoch":5}}"#;
        let read = ExitRecord::parse(literal);
        assert_eq!((read.version, read.code, read.oom), (1, Some(0), None));
        assert_eq!(read.started, None, "version 1 can't say");
        // Nor is a `started` in a version 1 record believed.
        let claimed =
            ExitRecord::parse(br#"{"version":1,"description":"x","code":0,"started":true}"#);
        assert_eq!(claimed.started, None);
        assert_eq!(
            read.finished,
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(1_760_000_000, 5))
        );
    }

    /// A newer Shipyard's record isn't guessed at: no code.
    #[test]
    fn a_newer_exit_record_gives_no_code() {
        let newer = ExitRecord::parse(br#"{"version":3,"code":3,"description":"exited"}"#);
        assert_eq!((newer.version, newer.code), (3, None));
        assert_eq!(newer.description, "unknown (recorded by a newer Shipyard)");
        let broken = ExitRecord::parse(br#"{"version":1,"code":"three"}"#);
        assert_eq!((broken.version, broken.code), (1, None));
        let unversioned = ExitRecord::parse(br#"{"description":"exited","code":3}"#);
        assert_eq!((unversioned.version, unversioned.code), (0, None));
        assert_eq!(unversioned.description, "unknown (unreadable exit record)");
    }

    /// Stage 8's reader, given this version's record (a downgrade): it
    /// finds the code in the words, as it did in its own text.
    #[test]
    fn stage_8s_reader_finds_the_code_in_a_version_2_record() {
        let record = ExitRecord::new("exited (exit code 3)".to_owned(), Some(3), None, true);
        let json = String::from_utf8(record.to_json()).unwrap();
        assert_eq!(code_in_text(&json), Some(3));
    }

    /// A clock before 1970 can't be written: the time is left out, the rest
    /// kept.
    #[test]
    fn a_record_with_a_time_before_1970_keeps_its_code() {
        let mut record = ExitRecord::new("exited (exit code 3)".to_owned(), Some(3), None, true);
        record.finished = SystemTime::UNIX_EPOCH.checked_sub(std::time::Duration::from_secs(1));
        let read = ExitRecord::parse(&record.to_json());
        assert_eq!((read.code, read.finished), (Some(3), None));
    }

    #[test]
    fn a_newer_state_json_is_refused_by_kind() {
        assert!(check_state_version(br#"{"bundle":"/b","init":null,"annotations":{}}"#).is_ok());
        assert!(check_state_version(br#"{"version":1,"bundle":"/b"}"#).is_ok());
        assert!(matches!(
            check_state_version(br#"{"version":2,"bundle":"/b"}"#),
            Err(ContainerError::UnknownVersion {
                file: "state.json",
                found: 2
            })
        ));
        // Not a record at all: the record's own parse says what's wrong.
        assert!(check_state_version(b"not json").is_ok());
    }
}
