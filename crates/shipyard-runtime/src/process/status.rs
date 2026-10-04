//! How a process ended, and the exit code shipyard reports for it.
//!
//! Exit codes follow Docker so scripts behave the same: the command's own code
//! when it exits, `128 + N` when signal N kills it, and 125–127 when it never
//! got to run.

use std::fmt;

use super::signal::Signal;

/// Shipyard failed before the command could start (bad flags, setup error).
pub const EXIT_ENGINE_ERROR: u8 = 125;
/// The command exists but could not be executed (permission denied, bad format).
pub const EXIT_CANNOT_EXECUTE: u8 = 126;
/// The command (or its `#!` interpreter) was not found.
pub const EXIT_NOT_FOUND: u8 = 127;

/// How a process ended (TLPI §26.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitStatus {
    /// The process exited normally with this code.
    Exited(u8),
    /// The process was killed by a signal.
    Signaled {
        /// The signal that killed it.
        signal: Signal,
        /// Whether the kernel wrote a core dump.
        core_dumped: bool,
    },
}

impl ExitStatus {
    /// The code a shell would report: the exit code, or `128 + signal`.
    ///
    /// Signals never exceed 64 on Linux or macOS, so `128 + signal` always
    /// fits; the fallback to 255 only guards against that ever changing.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled { signal, .. } => u8::try_from(signal.as_raw())
                .ok()
                .and_then(|s| s.checked_add(128))
                .unwrap_or(u8::MAX),
        }
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Exited(code) => write!(f, "exited with code {code}"),
            Self::Signaled {
                signal,
                core_dumped,
            } => {
                match signal.name() {
                    Some(name) => write!(f, "killed by {name}")?,
                    None => write!(f, "killed by signal {}", signal.as_raw())?,
                }
                if core_dumped {
                    f.write_str(" (core dumped)")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn killed(signal: Signal) -> ExitStatus {
        ExitStatus::Signaled {
            signal,
            core_dumped: false,
        }
    }

    #[test]
    fn exit_code_is_passed_through() {
        assert_eq!(ExitStatus::Exited(0).code(), 0);
        assert_eq!(ExitStatus::Exited(3).code(), 3);
        assert_eq!(ExitStatus::Exited(255).code(), 255);
    }

    #[test]
    fn signals_map_to_128_plus_n() {
        assert_eq!(killed(Signal::KILL).code(), 137);
        assert_eq!(killed(Signal::TERM).code(), 143);
    }

    #[test]
    fn display_names_signals() {
        assert_eq!(ExitStatus::Exited(3).to_string(), "exited with code 3");
        let segv = ExitStatus::Signaled {
            signal: Signal::from_raw(libc::SIGSEGV).unwrap(),
            core_dumped: true,
        };
        assert_eq!(segv.to_string(), "killed by SIGSEGV (core dumped)");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn realtime_signals_are_named() {
        let rt = Signal::from_raw(37).unwrap();
        assert_eq!(killed(rt).to_string(), "killed by SIGRTMIN+3");
        assert_eq!(killed(rt).code(), 165);
    }
}
