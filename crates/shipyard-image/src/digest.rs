//! Content digests: how a blob is named and verified.
//!
//! An OCI image refers to every blob — a manifest, a config, a layer — by its
//! digest, written `algorithm:hex` (for example
//! `sha256:2c3e...`, 64 hex characters for sha256). The digest *is* the name:
//! the store keeps a blob at a path derived from it, and anyone with the
//! digest can ask for exactly those bytes and check they got them. A blob
//! whose contents don't hash to its digest is refused, never used — a
//! registry (or a corrupted download) cannot slip in different bytes under a
//! name we trust.
//!
//! Only sha256 is implemented: it's what the OCI image spec requires and what
//! every registry serves. sha512 is allowed by the spec but rare; an
//! unknown algorithm is a clear error rather than a silent accept.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// The number of hex characters in a sha256 digest (32 bytes, two chars each).
const SHA256_HEX_LEN: usize = 64;

/// A content digest: an algorithm and the hash of some bytes under it.
///
/// Parsed from and displayed as `sha256:<64 lowercase hex>`. Equality is on
/// the algorithm and the hash, so two digests of the same bytes are equal
/// whatever their source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    /// The 32 raw bytes of the sha256 hash.
    bytes: [u8; 32],
}

/// Why a string isn't a valid digest.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DigestError {
    /// No `algorithm:hex` separator.
    #[error("a digest is `algorithm:hex`, with no colon in `{0}`")]
    NoSeparator(String),
    /// An algorithm other than sha256.
    #[error("unsupported digest algorithm `{0}` (only sha256 is supported)")]
    UnsupportedAlgorithm(String),
    /// The hex part is the wrong length for the algorithm.
    #[error("a sha256 digest needs {SHA256_HEX_LEN} hex characters, not {0}")]
    WrongLength(usize),
    /// The hex part has a character that isn't a hex digit.
    #[error("`{0}` is not a lowercase hex digit in the digest")]
    NotHex(char),
}

impl Digest {
    /// The sha256 digest of `bytes`, computed in full.
    #[must_use]
    pub fn sha256(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self::from_hasher(hasher)
    }

    /// The digest a finished sha256 hasher has computed.
    fn from_hasher(hasher: Sha256) -> Self {
        Self {
            bytes: hasher.finalize().into(),
        }
    }

    /// Parses a digest written `sha256:<hex>`.
    ///
    /// The hex must be lowercase, as the OCI spec writes it and as [`Display`]
    /// renders it, so a digest round-trips through its string form.
    ///
    /// # Errors
    ///
    /// Returns a [`DigestError`] if there's no separator, the algorithm isn't
    /// sha256, the hex is the wrong length, or a character isn't lowercase hex.
    ///
    /// [`Display`]: std::fmt::Display
    pub fn parse(text: &str) -> Result<Self, DigestError> {
        let (algorithm, hex) = text
            .split_once(':')
            .ok_or_else(|| DigestError::NoSeparator(text.to_owned()))?;
        if algorithm != "sha256" {
            return Err(DigestError::UnsupportedAlgorithm(algorithm.to_owned()));
        }
        if hex.len() != SHA256_HEX_LEN {
            return Err(DigestError::WrongLength(hex.len()));
        }
        let mut bytes = [0u8; 32];
        let mut chars = hex.chars();
        for byte in &mut bytes {
            // Two chars per byte; the length check above guarantees they exist.
            let high = next_nibble(&mut chars)?;
            let low = next_nibble(&mut chars)?;
            *byte = (high << 4) | low;
        }
        Ok(Self { bytes })
    }

    /// The algorithm part of the digest. Always `"sha256"` for now.
    #[must_use]
    pub fn algorithm(&self) -> &'static str {
        "sha256"
    }

    /// The hex part of the digest, lowercase, without the `sha256:` prefix.
    /// This is the file name a blob takes in the store.
    #[must_use]
    pub fn hex(&self) -> String {
        shipyard_sys::hex::lower(&self.bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", self.hex())
    }
}

impl serde::Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Hashes bytes as they pass through, so a blob is verified while it streams
/// to disk rather than held whole in memory.
///
/// Feed it with [`update`](Self::update) (or by writing to it — it implements
/// [`std::io::Write`]), then [`finish`](Self::finish) to get the digest, or
/// [`verify`](Self::verify) to check it against an expected one.
#[derive(Debug, Clone)]
pub struct Verifier {
    hasher: Sha256,
}

impl Verifier {
    /// A fresh verifier, hashing nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    /// Adds `bytes` to the running hash.
    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    /// The digest of everything fed in so far, consuming the verifier.
    #[must_use]
    pub fn finish(self) -> Digest {
        Digest::from_hasher(self.hasher)
    }

    /// Checks the bytes fed in against `expected`.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError`] with both digests if they differ, so a caller
    /// can report what it got against what it asked for.
    pub fn verify(self, expected: &Digest) -> Result<(), VerifyError> {
        let actual = self.finish();
        if &actual == expected {
            Ok(())
        } else {
            Err(VerifyError {
                expected: expected.clone(),
                actual,
            })
        }
    }
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new()
    }
}

impl std::io::Write for Verifier {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A blob's contents did not hash to the digest it was asked for.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("digest mismatch: expected {expected}, got {actual}")]
pub struct VerifyError {
    /// The digest the caller asked for.
    pub expected: Digest,
    /// The digest the bytes actually hash to.
    pub actual: Digest,
}

/// The next hex character as its 4-bit value, or which character wasn't hex.
fn next_nibble(chars: &mut std::str::Chars<'_>) -> Result<u8, DigestError> {
    let character = chars.next().unwrap_or('\0');
    match character {
        '0'..='9' => Ok(character as u8 - b'0'),
        'a'..='f' => Ok(character as u8 - b'a' + 10),
        other => Err(DigestError::NotHex(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sha256 of the empty input, as every sha256 implementation agrees.
    const EMPTY_SHA256: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn digests_round_trip_through_their_string_form() {
        let digest = Digest::sha256(b"");
        assert_eq!(digest.to_string(), EMPTY_SHA256);
        let parsed = Digest::parse(EMPTY_SHA256).expect("the empty digest parses");
        assert_eq!(parsed, digest);
        assert_eq!(parsed.hex().len(), SHA256_HEX_LEN);
        assert_eq!(parsed.algorithm(), "sha256");
    }

    #[test]
    fn the_hash_is_sha256() {
        // Checked against `printf 'abc' | sha256sum`.
        assert_eq!(
            Digest::sha256(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bad_digests_are_rejected_by_reason() {
        assert_eq!(
            Digest::parse("deadbeef"),
            Err(DigestError::NoSeparator("deadbeef".to_owned()))
        );
        assert_eq!(
            Digest::parse("md5:abc"),
            Err(DigestError::UnsupportedAlgorithm("md5".to_owned()))
        );
        assert_eq!(
            Digest::parse("sha256:abcd"),
            Err(DigestError::WrongLength(4))
        );
        // 64 characters, but one is not lowercase hex (uppercase A).
        let not_hex = format!("sha256:{}", "A".repeat(SHA256_HEX_LEN));
        assert_eq!(Digest::parse(&not_hex), Err(DigestError::NotHex('A')));
        // Right length, a non-hex symbol.
        let symbol = format!("sha256:{}{}", "0".repeat(63), "!");
        assert_eq!(Digest::parse(&symbol), Err(DigestError::NotHex('!')));
    }

    #[test]
    fn equal_bytes_give_equal_digests() {
        assert_eq!(Digest::sha256(b"same"), Digest::sha256(b"same"));
        assert_ne!(Digest::sha256(b"one"), Digest::sha256(b"two"));
    }

    #[test]
    fn the_verifier_streams_to_the_same_digest() {
        use std::io::Write as _;

        let mut verifier = Verifier::new();
        verifier.update(b"ab");
        verifier.update(b"c");
        assert_eq!(verifier.finish(), Digest::sha256(b"abc"));

        // Writing to it (the io::Write path) hashes the same.
        let mut writer = Verifier::new();
        writer
            .write_all(b"abc")
            .expect("writing to a verifier never fails");
        assert_eq!(writer.finish(), Digest::sha256(b"abc"));
    }

    #[test]
    fn verify_accepts_matching_and_names_a_mismatch() {
        let expected = Digest::sha256(b"abc");
        let mut good = Verifier::new();
        good.update(b"abc");
        assert_eq!(good.verify(&expected), Ok(()));

        let mut bad = Verifier::new();
        bad.update(b"abd");
        let error = bad.verify(&expected).expect_err("different bytes mismatch");
        assert_eq!(error.expected, expected);
        assert_eq!(error.actual, Digest::sha256(b"abd"));
    }
}
