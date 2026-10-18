//! Process descriptors (`pidfd_open(2)`, Linux 5.3): a handle on one
//! process that never comes to mean another, as its PID can once it ends and
//! the number is handed out again (TLPI §20.5 for `kill`'s race). A signal
//! sent through one reaches that process or none.

use std::io;
use std::os::fd::{AsFd, AsRawFd as _, OwnedFd};

/// A pidfd for the process `pid`.
///
/// # Errors
///
/// `ESRCH` if there's no such process (gone, and reaped); `EINVAL` for a
/// PID that can't be one (0, negative); `ENOSYS` before Linux 5.3; `EMFILE`.
pub fn open(pid: i32) -> io::Result<OwnedFd> {
    // Refused here, as the kernel would: `Pid` takes no negative number.
    if pid <= 0 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // (Never `None` past the check above.)
    let pid = rustix::process::Pid::from_raw(pid)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).map_err(io::Error::from)
}

/// Whether `error`, from [`open`], says this process can't have pidfds:
/// a kernel before 5.3 (`ENOSYS`), or a seccomp or LSM policy refusing the
/// call (`EPERM`: `pidfd_open` itself never returns it). The caller falls
/// back to the PID, checked first; any other error is its own (`ESRCH`:
/// gone; `EMFILE`).
#[must_use]
pub fn unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EPERM))
}

/// Sends signal `signal`, by number, to the process `pidfd` is: 0 sends
/// none, only asks whether it's still there.
///
/// # Errors
///
/// `ESRCH` once it has ended; `EINVAL` for a number that's no signal;
/// `EPERM` for one this process may not signal.
pub fn send_signal(pidfd: impl AsFd, signal: libc::c_int) -> io::Result<()> {
    // Raw: neither nix nor rustix sends these. rustix's `Signal` can't be 0
    // and, by its contract, mustn't send a real-time signal (libc reserves
    // them for itself in this process); a container's process may be sent
    // any, and Shipyard sends them by number.
    // SAFETY: plain integers and a descriptor borrowed for the call; a null
    // `info` and no flags ask nothing more of the kernel.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_fd().as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
