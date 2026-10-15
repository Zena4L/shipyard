//! Containers' names: the rule a given one follows, and the generator of
//! one when none is given (`brave_tugboat`).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::id::{Serial, is_id};
use crate::error::{Result, bail};

/// A container's name: a letter or digit, then letters, digits, `_`, `.`
/// or `-` (Docker's rule), at most [`MAX`](Self::MAX) bytes. Unique among
/// the caller's containers. Never a path: a container's files are named
/// by its ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    /// The longest a name may be.
    pub const MAX: usize = 128;

    /// `text`, if it's a name a container may have.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): it
    /// breaks the rule, or would read as an ID or a serial (which are
    /// looked up first).
    pub fn parse(text: &str) -> Result<Self> {
        let valid = text.len() <= Self::MAX
            && text.starts_with(|c: char| c.is_ascii_alphanumeric())
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !valid {
            bail!(
                InvalidRequest,
                "{text:?} isn't a container name: a letter or digit, then letters, digits, _ . \
                 or -, at most {} in all",
                Self::MAX
            );
        }
        if is_id(text) || matches!(Serial::parse(text), Ok(Some(_)) | Err(_)) {
            bail!(
                InvalidRequest,
                "{text:?} can't be a container name: it reads as a container's ID or serial"
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A generated name, `adjective_noun`, from `random` (two numbers, one
    /// for each word), with `suffix` after it if given (`_2`): for when
    /// the plain names drawn were all taken.
    pub(crate) fn generated(random: [u16; 2], suffix: Option<u32>) -> Self {
        let adjective = ADJECTIVES[usize::from(random[0]) % ADJECTIVES.len()];
        let noun = NOUNS[usize::from(random[1]) % NOUNS.len()];
        Self(match suffix {
            Some(suffix) => format!("{adjective}_{noun}_{suffix}"),
            None => format!("{adjective}_{noun}"),
        })
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Name {
    type Error = crate::Error;

    fn try_from(text: String) -> Result<Self> {
        Self::parse(&text)
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

/// The first word of a generated name.
const ADJECTIVES: [&str; 48] = [
    "able", "amber", "ample", "bold", "brave", "brisk", "calm", "clever", "coastal", "cobalt",
    "crisp", "daring", "deep", "eager", "early", "fair", "fast", "fearless", "gentle", "golden",
    "grand", "hardy", "honest", "jolly", "keen", "kind", "lively", "loyal", "lucky", "mighty",
    "nimble", "noble", "patient", "proud", "quick", "quiet", "rapid", "salty", "silver", "sleek",
    "steady", "stout", "sturdy", "swift", "tidal", "trusty", "vivid", "windy",
];

/// The second word of a generated name: things of a harbour, a ship or the
/// sea.
const NOUNS: [&str; 48] = [
    "anchor",
    "barge",
    "beacon",
    "berth",
    "bollard",
    "bow",
    "breakwater",
    "buoy",
    "cable",
    "capstan",
    "cargo",
    "crane",
    "current",
    "davit",
    "deck",
    "dock",
    "drydock",
    "ferry",
    "flag",
    "freighter",
    "galley",
    "gantry",
    "harbour",
    "hatch",
    "helm",
    "hull",
    "jetty",
    "keel",
    "lantern",
    "lighthouse",
    "mast",
    "mooring",
    "pier",
    "pilot",
    "quay",
    "reefer",
    "rudder",
    "sail",
    "skiff",
    "spar",
    "stern",
    "tanker",
    "tide",
    "trawler",
    "tugboat",
    "wake",
    "wharf",
    "winch",
];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_rule() {
        for good in ["web", "a", "db.1", "my_app-2", "9lives"] {
            assert_eq!(Name::parse(good).unwrap().as_str(), good);
        }
        let longest = "a".repeat(Name::MAX);
        assert!(Name::parse(&longest).is_ok());
        for bad in [
            "",
            "-web",
            ".web",
            "_web",
            "we b",
            "web/x",
            "../x",
            "x\n",
            "wéb",
            &"a".repeat(Name::MAX + 1),
        ] {
            let err = Name::parse(bad).unwrap_err();
            assert_eq!(err.kind(), crate::ErrorKind::InvalidRequest, "{bad:?}");
        }
    }

    /// A name never shadows what's looked up before names: a full ID, or
    /// a serial written with its owner code.
    #[test]
    fn a_name_cant_read_as_an_id_or_a_serial() {
        for confusing in [
            &"ab".repeat(32)[..],
            "SHPU3048122",
            "shpu304812",
            "SHPU3048125",
        ] {
            assert!(Name::parse(confusing).is_err(), "{confusing:?}");
        }
        // Short hex is a name like any other (it loses to nothing: an ID
        // prefix is looked up after names).
        assert!(Name::parse("abc123").is_ok());
    }

    #[test]
    fn generated_names_are_valid_names() {
        let mut seen = std::collections::BTreeSet::new();
        for a in 0..48 {
            for n in 0..48 {
                let name = Name::generated([a, n], None);
                assert!(Name::parse(name.as_str()).is_ok(), "{name}");
                seen.insert(name);
            }
        }
        assert_eq!(seen.len(), 48 * 48, "no two lists' words collide");
        let suffixed = Name::generated([0, 0], Some(2));
        assert_eq!(suffixed.as_str(), "able_anchor_2");
        assert!(Name::parse(suffixed.as_str()).is_ok());
    }
}
