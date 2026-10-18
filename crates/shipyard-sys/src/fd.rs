//! Descriptors a process is handed across `exec` (TLPI §27.4): by number, on
//! its command line or in its environment, for it to own.

use std::io;
use std::os::fd::{BorrowedFd, FromRawFd as _, OwnedFd, RawFd};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

/// Whether a descriptor taken over stays open across this process's own
/// `exec`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnExec {
    /// Made close-on-exec: nothing this process runs inherits it by chance.
    Close,
    /// Left as it came: this process hands it on in turn.
    Keep,
}

/// Takes over descriptor `fd`, inherited from the parent for this process
/// to own: checked open (`F_GETFD`), then made close-on-exec or not.
///
/// # Errors
///
/// `EBADF` if no descriptor `fd` is open; the `fcntl(2)` error.
///
/// # Safety
///
/// Nothing else in this process owns `fd` (it was left for this process to
/// take, and nothing here has opened anything that could be it yet), nor
/// takes it again.
pub unsafe fn take_inherited(fd: RawFd, on_exec: OnExec) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    // SAFETY: borrowed only for these calls; an `fd` that isn't open is
    // EBADF to `fcntl`, and touched no further.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    fcntl(borrowed, FcntlArg::F_GETFD)?;
    // SAFETY: open, as just checked, and ours alone (the caller's promise).
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // Owned first: should this fail, it's closed (dropped), not left open
    // for an `exec` to hand on.
    if on_exec == OnExec::Close {
        fcntl(&owned, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok(owned)
}

#[cfg(test)]
mod tests;
