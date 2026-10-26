//! The time now, as whole seconds since the epoch (step 12.5b1, F145: open
//! coded across crates, two widths).

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch now; 0 for a clock set before it.
#[must_use]
pub fn unix_seconds() -> u64 {
    seconds_at(SystemTime::now())
}

/// [`unix_seconds`] as Docker's APIs give times (an `i64`), saturating.
#[must_use]
pub fn unix_now() -> i64 {
    i64::try_from(unix_seconds()).unwrap_or(i64::MAX)
}

/// Seconds since the epoch at `time`; 0 before it.
fn seconds_at(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// Whole seconds; a time before the epoch, 0.
    #[test]
    fn a_time_s_seconds_since_the_epoch() {
        let at = UNIX_EPOCH + std::time::Duration::from_millis(5_999);
        assert_eq!(seconds_at(at), 5);
        let before = UNIX_EPOCH
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(seconds_at(before), 0);
        assert!(unix_now() > 1_700_000_000);
    }
}
