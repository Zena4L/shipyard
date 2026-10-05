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

/// The two signals below the real-time range that glibc keeps for itself
/// (thread cancellation and `setxid`): `sigfillset` leaves them out, so a
/// process can't wait for them.
const GLIBC_INTERNAL_SIGNALS: [libc::c_int; 2] = [32, 33];

/// Signals the container's init can't pass on to the command, so they'd
/// never arrive (see `init.rs`): it takes any SIGCHLD as news of an exited
/// child, and it can't wait for glibc's internal signals. Refused
/// everywhere, so a stop signal means the same with or without isolation.
const NOT_FORWARDED_BY_THE_INIT: [libc::c_int; 3] = [
    libc::SIGCHLD,
    GLIBC_INTERNAL_SIGNALS[0],
    GLIBC_INTERNAL_SIGNALS[1],
];

/// A stop signal that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{signal} can't be a stop signal: {reason}")]
pub struct StopSignalError {
    /// The signal asked for.
    pub signal: Signal,
    /// Why it can't be used.
    pub reason: &'static str,
}

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
        let signal = self.signal;
        let raw = signal.as_raw();
        if NOT_STOP_SIGNALS.contains(&raw) {
            return Err(StopSignalError {
                signal,
                reason: "it stops or continues the command instead of ending it",
            });
        }
        if NOT_FORWARDED_BY_THE_INIT.contains(&raw) {
            return Err(StopSignalError {
                signal,
                reason: "the container's init can't pass it on to the command",
            });
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
        // What the container's init can't pass on.
        for name in ["CHLD", "32", "33"] {
            let Ok(signal) = name.parse() else {
                continue; // no such signal on this system (macOS has no 32)
            };
            let policy = StopPolicy {
                signal,
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
