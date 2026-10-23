//! The kernel's random numbers (`random(7)`), for what must be unguessable
//! or unlikely to repeat: IDs, addresses, serials.

use std::fs::File;
use std::io::Read as _;

/// `N` random bytes, read from `/dev/urandom`: the kernel's generator,
/// which never blocks once it has been seeded at boot.
///
/// # Errors
///
/// `/dev/urandom` couldn't be read.
pub fn bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    /// Two reads differ (32 bytes the same twice is no generator).
    #[test]
    fn reads_differ() {
        assert_ne!(super::bytes::<32>().unwrap(), super::bytes::<32>().unwrap());
    }
}
