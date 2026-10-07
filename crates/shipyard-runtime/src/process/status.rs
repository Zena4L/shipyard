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
    /// Decodes a raw status from `waitpid` (TLPI §26.1.3).
    ///
    /// Returns `None` for statuses that don't mean the process ended (stopped
    /// or continued). Decoded by hand with the `WIF*` macros because nix's
    /// `WaitStatus` can't represent Linux real-time signals.
    #[must_use]
    pub(crate) fn from_wait_status(raw: libc::c_int) -> Option<Self> {
        if libc::WIFEXITED(raw) {
            return u8::try_from(libc::WEXITSTATUS(raw)).ok().map(Self::Exited);
        }
        if libc::WIFSIGNALED(raw) {
            return Signal::from_raw(libc::WTERMSIG(raw))
                .ok()
                .map(|signal| Self::Signaled {
                    signal,
                    core_dumped: libc::WCOREDUMP(raw),
                });
        }
        None
    }

    /// The raw status `waitpid` would give for it: [`from_wait_status`]'s
    /// inverse (the layout Linux and macOS share: the exit code in the
    /// second byte, or the signal in the low 7 bits, with 0x80 for a core
    /// dump).
    ///
    /// [`from_wait_status`]: Self::from_wait_status
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only a container's init sends one")
    )]
    pub(crate) fn wait_status(self) -> libc::c_int {
        match self {
            Self::Exited(code) => libc::c_int::from(code) << 8,
            Self::Signaled {
                signal,
                core_dumped,
            } => signal.as_raw() | if core_dumped { 0x80 } else { 0 },
        }
    }

    /// Whether SIGKILL ended it: what the kernel's OOM killer sends.
    #[must_use]
    pub fn killed_by_sigkill(self) -> bool {
        matches!(self, Self::Signaled { signal, .. } if signal.as_raw() == libc::SIGKILL)
    }

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
    fn decodes_raw_wait_statuses() {
        // The encodings are the same on Linux and macOS (TLPI figure 26-1).
        assert_eq!(ExitStatus::from_wait_status(0), Some(ExitStatus::Exited(0)));
        assert_eq!(
            ExitStatus::from_wait_status(3 << 8),
            Some(ExitStatus::Exited(3))
        );
        assert_eq!(
            ExitStatus::from_wait_status(255 << 8),
            Some(ExitStatus::Exited(255))
        );
        assert_eq!(
            ExitStatus::from_wait_status(libc::SIGKILL),
            Some(killed(Signal::KILL))
        );
        assert_eq!(
            ExitStatus::from_wait_status(libc::SIGSEGV | 0x80),
            Some(ExitStatus::Signaled {
                signal: Signal::from_raw(libc::SIGSEGV).unwrap(),
                core_dumped: true,
            })
        );
        // Stopped by SIGSTOP: not an exit.
        assert_eq!(
            ExitStatus::from_wait_status(0x7f | (libc::SIGSTOP << 8)),
            None
        );
    }

    #[test]
    fn wait_statuses_round_trip() {
        for raw in [0, 3 << 8, 255 << 8, libc::SIGKILL, libc::SIGSEGV | 0x80] {
            let status = ExitStatus::from_wait_status(raw).unwrap();
            assert_eq!(status.wait_status(), raw);
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
