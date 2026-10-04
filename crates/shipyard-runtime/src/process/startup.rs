//! Checks that run first thing in `main`, before anything else opens a file.

use std::os::fd::{AsRawFd, IntoRawFd, RawFd};

use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use nix::unistd::{getegid, geteuid, getgid, getuid};

use crate::{Error, Result};

/// Makes sure file descriptors 0, 1 and 2 are open, pointing any closed one at
/// `/dev/null`.
///
/// The kernel always hands out the lowest free descriptor number (TLPI §4.3).
/// If shipyard is started with, say, fd 1 closed (by a daemon that closed its
/// stdio, or `shipyard run … >&-`), the next file we open becomes fd 1. That
/// could be one of our internal pipes, which the container would then inherit
/// as its stdout. Filling the gaps first rules this out.
///
/// Call this before anything else in the process opens a file.
pub fn ensure_std_fds_open() -> Result<()> {
    for fd in 0..=2 {
        if is_open(fd) {
            continue;
        }
        // Deliberately not O_CLOEXEC: the container inherits these.
        let null = open("/dev/null", OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty())
            .map_err(|e| Error::sys("opening /dev/null for a closed standard fd", e))?;
        if null.as_raw_fd() == fd {
            // Keep it open for the life of the process.
            let _ = null.into_raw_fd();
        }
        // Otherwise another thread took `fd` first, so it's open after all.
        // Dropping `null` closes our spare copy without touching theirs.
    }
    Ok(())
}

/// Whether `fd` refers to an open file description.
fn is_open(fd: RawFd) -> bool {
    // Raw `fcntl` rather than nix: nix's version needs a `BorrowedFd`, and
    // borrowing a descriptor that may be closed is exactly what I/O safety forbids.
    // SAFETY: F_GETFD only reads the descriptor's flags and has no side effects,
    // even when `fd` is closed (it then fails with EBADF).
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Refuses to run if shipyard was started through a setuid or setgid bit.
///
/// Shipyard is never meant to be installed that way. If it were, any user
/// could run commands with the owner's privileges, so we stop immediately.
pub fn refuse_setid_execution() -> Result<()> {
    let (real_uid, effective_uid) = (getuid(), geteuid());
    let (real_gid, effective_gid) = (getgid(), getegid());
    if real_uid == effective_uid && real_gid == effective_gid {
        return Ok(());
    }
    Err(Error::PrivilegedExecutable {
        uid: real_uid.as_raw(),
        euid: effective_uid.as_raw(),
        gid: real_gid.as_raw(),
        egid: effective_gid.as_raw(),
    })
}
