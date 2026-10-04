//! How a running process is asked to stop.

use std::time::Duration;

use super::signal::Signal;

/// Seconds to wait after the stop signal before killing the process (Docker's
/// default).
pub const DEFAULT_STOP_TIMEOUT_SECS: u64 = 10;

/// Stopping a process gracefully: send `signal`, wait up to `timeout`, then
/// send `SIGKILL`.
///
/// `timeout` has no upper bound, so whoever turns it into a deadline must use
/// `Instant::checked_add` and treat an overflow as "wait forever"; plain `+`
/// would panic on absurd values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPolicy {
    /// Sent first, to ask the process to exit.
    pub signal: Signal,
    /// How long to wait before sending `SIGKILL`.
    pub timeout: Duration,
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            signal: Signal::TERM,
            timeout: Duration::from_secs(DEFAULT_STOP_TIMEOUT_SECS),
        }
    }
}
