//! systemd's socket activation, from the service's side (`sd_listen_fds(3)`,
//! step 11.2): systemd listens on the socket, starts the service at the
//! first connection, and hands it the listening socket as descriptor 3
//! with `LISTEN_PID` (the process it's for) and `LISTEN_FDS` (how many,
//! from 3) in its environment.

use std::io;
use std::os::fd::{OwnedFd, RawFd};

/// The first descriptor systemd passes (`SD_LISTEN_FDS_START`).
pub const FIRST: RawFd = 3;

/// How many descriptors from [`FIRST`] systemd passed process `pid`, by
/// its `LISTEN_PID` (`listen_pid`) and `LISTEN_FDS` (`listen_fds`): none
/// unless `LISTEN_PID` names this very process (a child that inherited
/// the variables isn't the one they were meant for), or for anything that
/// isn't a number.
#[must_use]
pub fn passed(pid: u32, listen_pid: Option<&str>, listen_fds: Option<&str>) -> usize {
    let for_us = listen_pid
        .and_then(|text| text.parse::<u32>().ok())
        .is_some_and(|listen| listen == pid);
    if !for_us {
        return 0;
    }
    listen_fds
        .and_then(|text| text.parse::<usize>().ok())
        .unwrap_or(0)
}

/// The socket systemd passed this process (one: the service serves one),
/// owned and close-on-exec,
/// with `LISTEN_PID`, `LISTEN_FDS` and `LISTEN_FDNAMES` cleared (nothing
/// this process starts takes them for its own). `None` if none was passed.
///
/// # Errors
///
/// Descriptor 3 was promised but isn't open, or can't be made
/// close-on-exec; more than one was passed.
///
/// # Safety
///
/// Called once, at the start, while this process is still one thread:
/// nothing else has taken descriptor 3, and nothing reads the environment
/// while it's changed.
pub unsafe fn from_systemd() -> io::Result<Option<OwnedFd>> {
    let count = passed(
        std::process::id(),
        std::env::var("LISTEN_PID").ok().as_deref(),
        std::env::var("LISTEN_FDS").ok().as_deref(),
    );
    for name in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: one thread, as the caller promises.
        unsafe { std::env::remove_var(name) };
    }
    if count == 0 {
        return Ok(None);
    }
    if count > 1 {
        // One socket is served: more is a unit that isn't Shipyard's.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("systemd passed {count} sockets: one is served (ListenStream= once)"),
        ));
    }
    // SAFETY: systemd passed it for this process (`LISTEN_PID`), and the
    // caller hasn't taken it.
    unsafe { crate::fd::take_inherited(FIRST, crate::fd::OnExec::Close) }.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the process `LISTEN_PID` names takes the sockets, as many as
    /// `LISTEN_FDS` says; a child that inherited them, or variables that
    /// aren't numbers, take none.
    #[test]
    fn only_the_process_named_takes_the_sockets() {
        assert_eq!(passed(42, Some("42"), Some("1")), 1);
        assert_eq!(passed(42, Some("42"), Some("2")), 2);
        assert_eq!(passed(43, Some("42"), Some("1")), 0, "a child");
        assert_eq!(passed(42, None, Some("1")), 0);
        assert_eq!(passed(42, Some("42"), None), 0);
        assert_eq!(passed(42, Some("x"), Some("1")), 0);
        assert_eq!(passed(42, Some("42"), Some("-1")), 0);
    }
}
