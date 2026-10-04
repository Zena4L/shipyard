//! How a running process is asked to stop.

use std::time::Duration;

use super::signal::Signal;

/// Signals that make no sense as a stop signal: they stop or continue the
/// process rather than ask it to exit, and the stop is followed by SIGCONT.
const NOT_STOP_SIGNALS: [libc::c_int; 5] = [
    libc::SIGSTOP,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGCONT,
];

/// A stop signal that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} can't be a stop signal: it stops or continues the command instead of ending it")]
pub struct StopSignalError(pub Signal);

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

impl StopPolicy {
    /// Rejects stop signals that can't end a process.
    pub fn validate(&self) -> Result<(), StopSignalError> {
        if NOT_STOP_SIGNALS.contains(&self.signal.as_raw()) {
            return Err(StopSignalError(self.signal));
        }
        Ok(())
    }
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            signal: Signal::TERM,
            timeout: Duration::from_secs(DEFAULT_STOP_TIMEOUT_SECS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_control_signals_cannot_stop_a_command() {
        for name in ["STOP", "TSTP", "TTIN", "TTOU", "CONT"] {
            let policy = StopPolicy {
                signal: name.parse().unwrap(),
                ..StopPolicy::default()
            };
            assert!(policy.validate().is_err(), "{name}");
        }
        for name in ["TERM", "INT", "QUIT", "USR1", "KILL"] {
            let policy = StopPolicy {
                signal: name.parse().unwrap(),
                ..StopPolicy::default()
            };
            assert!(policy.validate().is_ok(), "{name}");
        }
    }
}
