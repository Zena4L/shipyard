//! Locks on files, two kinds, told apart by where the lock goes.
//!
//! A `flock(2)` lock belongs to the open file description (TLPI §55.2):
//! every copy of the descriptor (a `dup`, a `fork`'s child, one passed over
//! `SCM_RIGHTS`) shares the one lock. It lasts while any copy is open, and
//! `LOCK_UN` from any copy lets it go for all of them.
//!
//! - **Scoped** ([`lock_until`]): a nix [`Flock`], let go when it drops. For
//!   a lock that never leaves this process: a command's own critical
//!   section. (One exception, waited for before it drops: the image store's
//!   lock lent to its storage helper, `StoreLock::inheritable`.)
//! - **Handed over** ([`try_lock_until`], [`HeldFile`]): std's lock on a
//!   plain [`File`], which dropping only closes. For a lock whose copy is
//!   given to another process (a container's monitor) that must keep it
//!   after this one lets go: the giver closes its copy, never unlocks it.
//!
//! These wait for a lock another holder has up to a deadline, trying again
//! every so often (`LOCK_NB`): a holder stopped or stuck mustn't keep every
//! other command waiting for good. (A few holds are waited for without one,
//! on purpose and said where: the run directories' sweep lock, a volume's
//! shared hold, the events journal's shared lock, a lease's range lock.)

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, Flockable};

/// How a lock is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// By one holder alone.
    Exclusive,
    /// By any number of holders at once (none of them exclusive).
    Shared,
}

/// A scoped lock on `fd`, held until the returned value drops, tried until
/// `deadline`, calling `between` before each try after the first (at least
/// one try is made, whatever the deadline).
///
/// # Errors
///
/// `EWOULDBLOCK` if another holder still has it at the deadline; any other
/// errno `flock(2)` gave.
pub fn lock_until_with<F: Flockable>(
    fd: F,
    mode: Mode,
    deadline: Instant,
    mut between: impl FnMut(),
) -> nix::Result<Flock<F>> {
    let how = match mode {
        Mode::Exclusive => FlockArg::LockExclusiveNonblock,
        Mode::Shared => FlockArg::LockSharedNonblock,
    };
    let mut fd = fd;
    loop {
        match Flock::lock(fd, how) {
            Ok(locked) => return Ok(locked),
            Err((back, Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                fd = back;
                between();
            }
            Err((_, errno)) => return Err(errno),
        }
    }
}

/// [`lock_until_with`], sleeping `poll` between tries.
///
/// # Errors
///
/// As [`lock_until_with`]'s.
pub fn lock_until<F: Flockable>(
    fd: F,
    mode: Mode,
    deadline: Instant,
    poll: Duration,
) -> nix::Result<Flock<F>> {
    lock_until_with(fd, mode, deadline, || std::thread::sleep(poll))
}

/// A lock on `file` to hand over (see the module's doc), tried until
/// `deadline` every `poll`: `true` once it's held, `false` if another holder
/// still has it at the deadline. Dropping `file` lets it go only once every
/// copy of it is closed too (or at once by [`File::unlock`], or a
/// [`HeldFile`]'s drop).
///
/// # Errors
///
/// The `flock(2)` error, other than the lock being held.
pub fn try_lock_until(
    file: &File,
    mode: Mode,
    deadline: Instant,
    poll: Duration,
) -> io::Result<bool> {
    loop {
        let tried = match mode {
            Mode::Exclusive => file.try_lock(),
            Mode::Shared => file.try_lock_shared(),
        };
        match tried {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(poll);
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

/// A locked file a value holds, to hand over: let go (`LOCK_UN`) when it
/// drops, so a copy another thread's child holds until it execs doesn't keep
/// it, unless it was [given up](Self::give_up): then it's only closed, and
/// the lock lasts while any copy (a container's monitor's
/// [`hold`](Self::hold)) is open.
#[derive(Debug)]
pub struct HeldFile {
    file: File,
    given_up: bool,
}

impl HeldFile {
    /// `file`, already locked (by [`try_lock_until`], say).
    #[must_use]
    pub fn new(file: File) -> Self {
        Self {
            file,
            given_up: false,
        }
    }

    /// The locked descriptor, for a process that keeps the lock once this
    /// value is given up.
    #[must_use]
    pub fn hold(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Gives the lock up to whoever holds a copy of [`hold`](Self::hold):
    /// this value's copy is closed when it drops, never unlocked.
    pub fn give_up(&mut self) {
        self.given_up = true;
    }
}

impl Drop for HeldFile {
    fn drop(&mut self) {
        if !self.given_up {
            let _ = self.file.unlock();
        }
    }
}

/// What a [`byte_range`] lock does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    /// Locks the whole file to read it (shared with other readers).
    Read,
    /// Locks it to rewrite it (alone).
    Write,
    /// Lets it go.
    Off,
}

/// An open-file-description lock (`F_OFD_SETLKW`) on the whole of `file`,
/// waited for: owned by the open file, as `flock` is, but apart from it, so
/// a file can be `flock`ed for one reason and range-locked for another.
/// Linux only; elsewhere, nothing (and `Ok`).
///
/// # Errors
///
/// The `fcntl(2)` error.
pub fn byte_range(file: &File, how: ByteRange) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `flock` is plain integers; all zeros is a valid value to
        // fill in (start 0, length 0: the whole file, however long).
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        let kind = match how {
            ByteRange::Read => libc::F_RDLCK,
            ByteRange::Write => libc::F_WRLCK,
            ByteRange::Off => libc::F_UNLCK,
        };
        lock.l_type = libc::c_short::try_from(kind).map_err(io::Error::other)?;
        lock.l_whence = libc::c_short::try_from(libc::SEEK_SET).map_err(io::Error::other)?;
        nix::fcntl::fcntl(file, nix::fcntl::FcntlArg::F_OFD_SETLKW(&lock))
            .map(drop)
            .map_err(io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, how);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
