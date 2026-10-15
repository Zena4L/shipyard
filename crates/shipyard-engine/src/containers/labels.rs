//! A container's labels: `KEY=VALUE` notes of the caller's, kept with it
//! and bounded, so that no number or size of them can swell its record (or
//! everything that reads every record, `ps`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Result, bail};

/// Labels, by key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<String, String>",
    into = "BTreeMap<String, String>"
)]
pub struct Labels(BTreeMap<String, String>);

impl Labels {
    /// The most labels a container may have.
    pub const MAX_COUNT: usize = 256;
    /// The longest a key may be, in bytes.
    pub const MAX_KEY: usize = 128;
    /// The longest a value may be, in bytes.
    pub const MAX_VALUE: usize = 4096;

    /// `labels`, if they're within bounds: at most [`MAX_COUNT`](Self::MAX_COUNT);
    /// each key 1 to [`MAX_KEY`](Self::MAX_KEY) bytes of printable ASCII
    /// with no space or `=`; each value at most
    /// [`MAX_VALUE`](Self::MAX_VALUE) bytes, without control characters.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest),
    /// naming the label.
    pub fn new(labels: BTreeMap<String, String>) -> Result<Self> {
        if labels.len() > Self::MAX_COUNT {
            bail!(
                InvalidRequest,
                "{} labels: a container may have at most {}",
                labels.len(),
                Self::MAX_COUNT
            );
        }
        for (key, value) in &labels {
            let key_ok = (1..=Self::MAX_KEY).contains(&key.len())
                && key.bytes().all(|b| b.is_ascii_graphic() && b != b'=');
            if !key_ok {
                bail!(
                    InvalidRequest,
                    "{key:?} isn't a label key: 1 to {} printable ASCII characters, no space or =",
                    Self::MAX_KEY
                );
            }
            if value.len() > Self::MAX_VALUE || value.chars().any(char::is_control) {
                bail!(
                    InvalidRequest,
                    "label {key}'s value: at most {} bytes, without control characters",
                    Self::MAX_VALUE
                );
            }
        }
        Ok(Self(labels))
    }

    /// The labels.
    #[must_use]
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }
}

impl TryFrom<BTreeMap<String, String>> for Labels {
    type Error = crate::Error;

    fn try_from(labels: BTreeMap<String, String>) -> Result<Self> {
        Self::new(labels)
    }
}

impl From<Labels> for BTreeMap<String, String> {
    fn from(labels: Labels) -> Self {
        labels.0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn one(key: &str, value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(key.to_owned(), value.to_owned())])
    }

    #[test]
    fn labels_are_bounded() {
        assert!(Labels::new(one("app.tier", "web front")).is_ok());
        assert!(Labels::new(one(&"k".repeat(Labels::MAX_KEY), &"v".repeat(4096))).is_ok());
        for (key, value) in [
            ("", "v"),
            ("a key", "v"),
            ("a=b", "v"),
            ("k\u{7f}", "v"),
            ("ké", "v"),
            (&"k".repeat(Labels::MAX_KEY + 1)[..], "v"),
            ("k", &"v".repeat(Labels::MAX_VALUE + 1)[..]),
            ("k", "line\nbreak"),
            ("k", "\u{1b}[31m"),
        ] {
            let err = Labels::new(one(key, value)).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::InvalidRequest, "{key:?}");
        }
        let many: BTreeMap<_, _> = (0..=Labels::MAX_COUNT)
            .map(|n| (format!("k{n}"), String::new()))
            .collect();
        assert!(Labels::new(many).is_err());
    }
}
