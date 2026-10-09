//! Process descriptors (`pidfd_open(2)`, Linux 5.3): a handle on one
//! process that never comes to mean another, as its PID can once it's
//! reused.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

/// A pidfd for `pid`.
pub(crate) fn open(pid: libc::pid_t) -> std::io::Result<OwnedFd> {
    // SAFETY: no pointers; the kernel checks `pid`.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(|_| std::io::Error::from_raw_os_error(libc::EBADF))?;
    // SAFETY: the kernel just returned this descriptor, ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Sends `signal` to the process `pidfd` is (0: none, only whether it
/// still runs).
pub(crate) fn send_signal(pidfd: &OwnedFd, signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: a pidfd we own; a null `info` is allowed.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
