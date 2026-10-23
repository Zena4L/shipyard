//! The enterer: the short-lived process that starts a new container in
//! another container's user namespace (rootless joins, step 11.4b2a;
//! `joins.rs`'s module doc says why it's a process of its own).
//!
//! ```text
//! monitor ── fork ──► enterer (single-threaded, the monitor's memory)
//!   1. setns: the other's user namespace, then its network and IPC ones
//!   2. clone3(CLONE_PARENT | the new namespaces | CLONE_INTO_CGROUP)
//!        ──► the container's child: the monitor's own (CLONE_PARENT),
//!            in a PID namespace of its own; it goes on as any (child.rs)
//!   3. says what happened (three 64-bit words on a pipe), and `_exit`s
//! ```
//!
//! Not `exec`'s joiner (`exec.rs`), which execs a sealed copy of the
//! binary: the enterer runs only system calls between its `fork` and its
//! `_exit`, so it keeps `child.rs`'s rules, denied below as there.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::os::fd::{AsRawFd, RawFd};

use nix::errno::Errno;
use nix::unistd::Pid;

use super::clone::{Cloned, clone_process};
use super::joins::NamespaceJoins;
use crate::{Error, Result};

/// What [`start`] did, on each side.
pub(crate) enum Started {
    /// This is the container's child: it goes on as any.
    InTheChild,
    /// The clone's outcome, in the monitor.
    Cloned(std::result::Result<Cloned, Errno>),
}

/// What the enterer says.
#[derive(Debug, PartialEq, Eq)]
enum Said {
    /// The child started: its PID, and whether it was born in its cgroup.
    Started { pid: Pid, in_cgroup: bool },
    /// The clone failed.
    CloneFailed(Errno),
    /// A namespace refused it: its place in the joins (`None`: the user
    /// namespace), and why.
    Refused { place: Option<usize>, errno: Errno },
}

/// Its size on the pipe: three 64-bit words.
const SAID: usize = 24;

impl Said {
    fn words(&self) -> [i64; 3] {
        match *self {
            Self::Started { pid, in_cgroup } => [1, i64::from(pid.as_raw()), i64::from(in_cgroup)],
            Self::CloneFailed(errno) => [2, 0, i64::from(errno as i32)],
            Self::Refused { place, errno } => [
                3,
                place.map_or(-1, |place| i64::try_from(place).unwrap_or(i64::MAX)),
                i64::from(errno as i32),
            ],
        }
    }

    /// What `bytes` say, if it's something the enterer says.
    fn read(bytes: &[u8; SAID]) -> Option<Self> {
        let mut words = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .copied()
            .map(i64::from_ne_bytes);
        let (what, number, last) = (words.next()?, words.next()?, words.next()?);
        let errno = || Some(Errno::from_raw(i32::try_from(last).ok()?));
        match what {
            1 => Some(Self::Started {
                pid: Pid::from_raw(libc::pid_t::try_from(number).ok()?),
                in_cgroup: last != 0,
            }),
            2 => Some(Self::CloneFailed(errno()?)),
            3 => Some(Self::Refused {
                place: usize::try_from(number).ok(),
                errno: errno()?,
            }),
            _ => None,
        }
    }
}

/// Starts the container's child through the enterer, which enters `joins`
/// (another container's user namespace first) for good, then clones it
/// with `flags` and `CLONE_PARENT` (this process's own child), into
/// `cgroup` where it can.
///
/// # Errors
///
/// What the enterer couldn't do: start, enter a namespace, or say.
///
/// # Safety
///
/// As for `fork`: the child may only run async-signal-safe code.
pub(crate) unsafe fn start(
    joins: &NamespaceJoins,
    flags: libc::c_int,
    cgroup: Option<RawFd>,
) -> Result<Started> {
    let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .map_err(|errno| Error::sys("making the enterer's pipe", errno))?;
    // SAFETY: the caller's contract; no namespace flags: a plain fork.
    match unsafe { clone_process(0, None) } {
        Err(errno) => Err(Error::sys(
            "starting the process that enters the other container's namespaces",
            errno,
        )),
        Ok(Cloned::Child) => {
            let pipe = (read.as_raw_fd(), write.as_raw_fd());
            std::mem::forget((read, write));
            // SAFETY: right after `fork`, as `enterer` needs.
            unsafe { enterer(joins, flags, cgroup, pipe) };
            Ok(Started::InTheChild)
        }
        Ok(Cloned::Parent { pid: enterer, .. }) => {
            drop(write);
            let mut bytes = [0_u8; SAID];
            let heard = std::io::Read::read_exact(&mut std::fs::File::from(read), &mut bytes);
            // It has exited, or is about to. A supervisor's reaping may have
            // taken it first (ECHILD).
            let _ = nix::sys::wait::waitpid(enterer, None);
            heard.map_err(|error| {
                Error::sys(
                    "the process that enters the other container's namespaces ended without a \
                     word",
                    super::io_errno(&error),
                )
            })?;
            match Said::read(&bytes) {
                Some(Said::Started { pid, in_cgroup }) => {
                    Ok(Started::Cloned(Ok(Cloned::Parent { pid, in_cgroup })))
                }
                Some(Said::CloneFailed(errno)) => Ok(Started::Cloned(Err(errno))),
                Some(Said::Refused { place, errno }) => Err(joins.refusal(place, errno)),
                None => Err(Error::Namespace(
                    "the process that enters the other container's namespaces said what it \
                     can't have"
                        .to_owned(),
                )),
            }
        }
    }
}

/// The enterer itself, right after `fork`: enters `joins` and clones the
/// container's child, says what happened on `pipe`'s write end, and
/// `_exit`s. Returns only in the child (the pipe closed there).
///
/// # Safety
///
/// Called only in the forked process, as its only code: system calls and
/// no allocation (`child.rs`'s rules).
unsafe fn enterer(
    joins: &NamespaceJoins,
    flags: libc::c_int,
    cgroup: Option<RawFd>,
    (read, write): (RawFd, RawFd),
) {
    let say = |said: &Said| {
        let words = said.words();
        // SAFETY: a write of a local buffer, its size, to our pipe.
        unsafe { libc::write(write, words.as_ptr().cast(), SAID) };
    };
    if let Err((place, errno)) = joins.enter_for_good() {
        say(&Said::Refused { place, errno });
        // SAFETY: `_exit` is async-signal-safe.
        unsafe { libc::_exit(1) };
    }
    // SAFETY: the caller's contract; the flags are namespace flags and
    // `CLONE_PARENT`.
    match unsafe { clone_process(flags | libc::CLONE_PARENT, cgroup) } {
        Ok(Cloned::Child) => {
            // The container's child: the enterer's pipe isn't its.
            // SAFETY: closing descriptors this process owns, once.
            unsafe {
                libc::close(read);
                libc::close(write);
            }
            return;
        }
        Ok(Cloned::Parent { pid, in_cgroup }) => say(&Said::Started { pid, in_cgroup }),
        Err(errno) => say(&Said::CloneFailed(errno)),
    }
    // SAFETY: `_exit` is async-signal-safe.
    unsafe { libc::_exit(0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the enterer says is read back as it was said; anything else is
    /// nothing it says.
    #[test]
    fn what_the_enterer_says_is_read_back() {
        let bytes = |said: &Said| {
            let mut bytes = [0_u8; SAID];
            for (to, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(said.words()) {
                to.copy_from_slice(&word.to_ne_bytes());
            }
            bytes
        };
        for said in [
            Said::Started {
                pid: Pid::from_raw(4242),
                in_cgroup: true,
            },
            Said::Started {
                pid: Pid::from_raw(7),
                in_cgroup: false,
            },
            Said::CloneFailed(Errno::ENOSPC),
            Said::Refused {
                place: None,
                errno: Errno::EPERM,
            },
            Said::Refused {
                place: Some(1),
                errno: Errno::EINVAL,
            },
        ] {
            assert_eq!(Said::read(&bytes(&said)), Some(said));
        }
        let mut garbled = [0_u8; SAID];
        garbled[..8].copy_from_slice(&9_i64.to_ne_bytes());
        assert_eq!(Said::read(&garbled), None);
        let mut huge = bytes(&Said::CloneFailed(Errno::EPERM));
        huge[16..].copy_from_slice(&i64::MAX.to_ne_bytes());
        assert_eq!(Said::read(&huge), None, "an errno out of range");
    }
}
