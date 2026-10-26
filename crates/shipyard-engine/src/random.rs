//! The kernel's random numbers, for what must be unguessable or unlikely to
//! repeat: run and container IDs, a container's serial.

use crate::error::{Context as _, Result};

/// `N` random bytes, the kernel's (`shipyard_sys::random`).
///
/// # Errors
///
/// `/dev/urandom` couldn't be read.
pub(crate) fn bytes<const N: usize>() -> Result<[u8; N]> {
    shipyard_sys::random::bytes().context("reading /dev/urandom")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_random() {
        assert_ne!(bytes::<32>().unwrap(), bytes::<32>().unwrap());
    }
}
