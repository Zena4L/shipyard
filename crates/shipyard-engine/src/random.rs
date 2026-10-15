//! The kernel's random numbers, for what must be unguessable or unlikely to
//! repeat: run and container IDs, a container's serial.

use std::fs::File;
use std::io::Read as _;

use crate::error::{Context as _, Result};

/// `N` random bytes, read from `/dev/urandom` (`random(7)`): the kernel's
/// generator, which never blocks once it has been seeded at boot.
///
/// # Errors
///
/// `/dev/urandom` couldn't be read.
pub(crate) fn bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes)
}

/// `bytes` as lowercase hex, two characters a byte.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_random_and_hex_is_lowercase() {
        assert_ne!(bytes::<32>().unwrap(), bytes::<32>().unwrap());
        assert_eq!(hex(&[0x00, 0xab, 0x7f]), "00ab7f");
    }
}
