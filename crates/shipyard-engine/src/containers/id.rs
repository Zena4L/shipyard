//! A container's identities: its full ID, and its serial, the number shown
//! the way freight containers are numbered (ISO 6346).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Result, bail};

/// A container's ID: 64 lowercase hex characters, 32 random bytes (Docker's
/// length). Its runtime state directory has the same name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContainerId(String);

// The networks' DNS answers to the same short ID (`ps`'s, the default
// hostname): one length, checked as it's built.
const _: () = assert!(ContainerId::SHORT == shipyard_network::store::SHORT_CONTAINER_ID);

impl ContainerId {
    /// The ID's length.
    pub const LEN: usize = 64;

    /// How much of it [`short`](Self::short) shows.
    pub const SHORT: usize = 12;

    /// A new, random one.
    ///
    /// # Errors
    ///
    /// The kernel's random numbers couldn't be read.
    pub(crate) fn new() -> Result<Self> {
        Ok(Self(shipyard_sys::hex::lower(
            &crate::random::bytes::<32>()?
        )))
    }

    /// `text`, if it's an ID: 64 lowercase hex characters.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): it
    /// isn't one.
    pub fn parse(text: &str) -> Result<Self> {
        if !is_id(text) {
            bail!(
                InvalidRequest,
                "{text:?} isn't a container ID (64 lowercase hex characters)"
            );
        }
        Ok(Self(text.to_owned()))
    }

    /// The whole ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Its first [`SHORT`](Self::SHORT) characters, as `ps` shows it.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..Self::SHORT]
    }
}

/// Whether `text` has an ID's form.
pub(crate) fn is_id(text: &str) -> bool {
    text.len() == ContainerId::LEN && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ContainerId {
    type Error = crate::Error;

    fn try_from(text: String) -> Result<Self> {
        Self::parse(&text)
    }
}

impl From<ContainerId> for String {
    fn from(id: ContainerId) -> Self {
        id.0
    }
}

/// A container's serial: six digits, shown with Shipyard's owner code and
/// a check digit as freight containers are (ISO 6346): `SHPU 304812 2`.
/// Drawn at random when the container is made, and unique among the
/// caller's containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Serial(u32);

impl Serial {
    /// The owner code (`SHP`) and the category (`U`, a freight container).
    pub const OWNER: &'static str = "SHPU";

    /// How many serials there are.
    pub(crate) const COUNT: u32 = 1_000_000;

    /// The serial `number` (below a million).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): it
    /// has more than six digits.
    pub fn new(number: u32) -> Result<Self> {
        if number >= Self::COUNT {
            bail!(InvalidRequest, "a serial has six digits, not {number}");
        }
        Ok(Self(number))
    }

    /// A random one.
    ///
    /// # Errors
    ///
    /// The kernel's random numbers couldn't be read.
    pub(crate) fn random() -> Result<Self> {
        // Uniform enough: 2^32 isn't a multiple of a million, which makes
        // the first 967,296 serials a 4,294th more likely than the rest.
        Ok(Self(
            u32::from_le_bytes(crate::random::bytes::<4>()?) % Self::COUNT,
        ))
    }

    /// Its six digits.
    #[must_use]
    pub fn number(self) -> u32 {
        self.0
    }

    /// Its check digit, by ISO 6346: each of the ten characters of the
    /// owner code and serial valued (digits as themselves, letters from 10
    /// up, skipping multiples of 11), weighted by 2 to the power of its
    /// place, summed; the sum's remainder by 11, its last digit.
    #[must_use]
    pub fn check_digit(self) -> u8 {
        check_digit(&format!("{}{:06}", Self::OWNER, self.0))
    }

    /// A serial as written: `SHPU 304812 2`, any case, spaces anywhere, the
    /// check digit optional. `None` for text that isn't one at all (no
    /// owner code, or not six digits after it).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a
    /// serial with the wrong check digit (a typing mistake: no container is
    /// guessed at).
    pub fn parse(text: &str) -> Result<Option<Self>> {
        let compact: String = text
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        let Some(digits) = compact.strip_prefix(Self::OWNER) else {
            return Ok(None);
        };
        if !(6..=7).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(None);
        }
        let (number, check) = digits.split_at(6);
        let Ok(number) = number.parse() else {
            return Ok(None);
        };
        let serial = Self(number);
        if let Some(given) = check.bytes().next()
            && given - b'0' != serial.check_digit()
        {
            bail!(
                InvalidRequest,
                "{text:?}: the check digit of {} {number:06} is {}, not {}",
                Self::OWNER,
                serial.check_digit(),
                char::from(given)
            );
        }
        Ok(Some(serial))
    }
}

/// ISO 6346's check digit of `code`: an owner code, a category and six
/// digits (ten characters, uppercase).
fn check_digit(code: &str) -> u8 {
    let sum: u32 = code
        .bytes()
        .zip(0u32..)
        .map(|(byte, place)| letter_or_digit_value(byte) << place)
        .sum();
    // `% 11` is below 11, and 10 becomes 0: one digit.
    u8::try_from(sum % 11 % 10).unwrap_or(0)
}

/// A character's value: a digit's own; a letter's from 10 (A) up, the
/// multiples of 11 left out (so B is 12, L 23, V 34).
fn letter_or_digit_value(byte: u8) -> u32 {
    match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'A'..=b'Z' => {
            let mut value = 10;
            for _ in b'A'..byte {
                value += 1;
                if value % 11 == 0 {
                    value += 1;
                }
            }
            value
        }
        _ => 0,
    }
}

impl fmt::Display for Serial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:06} {}", Self::OWNER, self.0, self.check_digit())
    }
}

impl TryFrom<u32> for Serial {
    type Error = crate::Error;

    fn try_from(number: u32) -> Result<Self> {
        Self::new(number)
    }
}

impl From<Serial> for u32 {
    fn from(serial: Serial) -> Self {
        serial.0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The letters' values are ISO 6346's table (A 10, B 12, … Z 38), and
    /// the check digits real containers carry come out.
    #[test]
    fn check_digits_are_iso_6346s() {
        let values: Vec<u32> = (b'A'..=b'Z').map(letter_or_digit_value).collect();
        assert_eq!(
            values,
            [
                10, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
                34, 35, 36, 37, 38
            ]
        );
        // Wikipedia's example, and other real ones.
        assert_eq!(check_digit("CSQU305438"), 3);
        assert_eq!(check_digit("MSKU907032"), 3);
        // Every serial, against the rule written out apart: the table
        // above, powers of two, and a remainder of 10 written as 0.
        let mut zeros_from_ten = 0;
        for number in 0..Serial::COUNT {
            let code = format!("SHPU{number:06}");
            let sum: u32 = code
                .bytes()
                .enumerate()
                .map(|(place, byte)| {
                    let value = if byte.is_ascii_digit() {
                        u32::from(byte - b'0')
                    } else {
                        values[usize::from(byte - b'A')]
                    };
                    value * 2u32.pow(u32::try_from(place).unwrap())
                })
                .sum();
            let expected = if sum % 11 == 10 { 0 } else { sum % 11 };
            zeros_from_ten += usize::from(sum % 11 == 10);
            assert_eq!(
                u32::from(Serial::new(number).unwrap().check_digit()),
                expected,
                "{code}"
            );
        }
        assert!(zeros_from_ten > 0, "the remainder of 10 was tried");
        assert_eq!(Serial::new(304_812).unwrap().check_digit(), 2);
        assert_eq!(Serial::new(304_812).unwrap().to_string(), "SHPU 304812 2");
        assert_eq!(Serial::new(7).unwrap().to_string(), "SHPU 000007 0");
    }

    #[test]
    fn serials_are_read_as_written() {
        let serial = Serial::new(304_812).unwrap();
        for text in [
            "SHPU 304812 2",
            "shpu3048122",
            "SHPU 304812",
            " shpu 304 812 ",
        ] {
            assert_eq!(Serial::parse(text).unwrap(), Some(serial), "{text:?}");
        }
        assert!(
            Serial::parse("SHPU 304812 5").is_err(),
            "a wrong check digit"
        );
        for text in [
            "304812",
            "SHPU 30481",
            "SHPU 30481222",
            "SHPX 304812",
            "SHPU 3048a2",
        ] {
            assert_eq!(Serial::parse(text).unwrap(), None, "{text:?}");
        }
        assert!(Serial::new(1_000_000).is_err());
    }

    #[test]
    fn ids_are_64_lowercase_hex() {
        let id = ContainerId::new().unwrap();
        assert_eq!(id.as_str().len(), 64);
        assert_eq!(ContainerId::parse(id.as_str()).unwrap(), id);
        assert_eq!(id.short().len(), 12);
        assert!(id.as_str().starts_with(id.short()));
        assert!(ContainerId::parse(&id.as_str().to_uppercase()).is_err());
        assert!(ContainerId::parse(&id.as_str()[1..]).is_err());
        assert!(ContainerId::parse(&"g".repeat(64)).is_err());
        assert_ne!(ContainerId::new().unwrap(), id);
        // As the runtime needs its IDs.
        assert!(shipyard_runtime::container::validate_id(id.as_str()).is_ok());
    }
}
