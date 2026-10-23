//! Joining a running process's namespaces, as `exec` into a container does
//! (`setns(2)`, `namespaces(7)`; Linux).
//!
//! **Holding a namespace.** `/proc/PID/ns/<kind>` opens to a descriptor for
//! that namespace, and `setns` on it moves the calling thread in. They're all
//! opened through one descriptor for the `/proc/PID` directory, which stays
//! bound to that one process: if it dies and its PID is reused, opening
//! through it fails (ESRCH) instead of reaching the newcomer (checked:
//! `scripts/probes/join.c`). Its start time, read through the same
//! descriptor, must be the one recorded for it, if one was.
//!
//! **Who joins them.** Not shipyard's own thread: entering a user namespace
//! can't be undone, and a PID namespace's `setns` only decides where the
//! *next children* are born. So a short-lived process does it, the joiner
//! (`exec.rs`): it joins the user and PID namespaces, then clones the
//! command into the container (with `CLONE_PARENT`: shipyard's own child,
//! which joins the rest itself, in `child.rs`).

use std::os::fd::OwnedFd;

use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;

use super::plan::{JoinSetup, JoinedNamespace};
use crate::isolation::{Join, Namespace};
use crate::{Error, Result};

/// A target's namespaces, opened before anything starts.
#[derive(Debug)]
pub(crate) struct Joining {
    /// Its `/proc` directory, bound to that one process.
    pub(crate) directory: OwnedFd,
    /// Its user namespace, joined first (by the joiner), if joined.
    pub(crate) user: Option<OwnedFd>,
    /// Its PID namespace, which the joiner's clone is born in, if joined.
    pub(crate) pid: Option<OwnedFd>,
    /// The rest, in the order they're joined: the mount namespace last
    /// (joining it moves the root and working directory into the container).
    pub(crate) others: Vec<(Namespace, OwnedFd)>,
}

impl Joining {
    /// Opens `join.target`'s namespaces.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchProcess`] if the target doesn't exist (or its start
    /// time isn't [`Join::start_time`]: its PID was reused),
    /// [`Error::ContainerExiting`] if it has exited but not been reaped (its
    /// namespaces are gone), [`Error::ForeignProc`] if `/proc` numbers
    /// processes differently from us, and a system error otherwise.
    pub(crate) fn open(join: &Join) -> Result<Self> {
        let target = join.target;
        let directory = process_directory(target, join.start_time)?;
        let namespace = |kind: Namespace| namespace_at(&directory, target, kind);
        let user = join
            .has(Namespace::User)
            .then(|| namespace(Namespace::User))
            .transpose()?;
        let pid = join
            .has(Namespace::Pid)
            .then(|| namespace(Namespace::Pid))
            .transpose()?;
        let mut kinds: Vec<Namespace> = join
            .namespaces
            .iter()
            .copied()
            .filter(|&kind| kind != Namespace::Pid && kind != Namespace::User)
            .collect();
        kinds.sort_by_key(|&kind| kind == Namespace::Mount);
        let others = kinds
            .into_iter()
            .map(|kind| Ok((kind, namespace(kind)?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            directory,
            user,
            pid,
            others,
        })
    }
}

/// The `/proc` directory of the process `target`, bound to that one
/// process, if it started at `start_time` (when one is given).
///
/// # Errors
///
/// [`Error::ForeignProc`] if `/proc` numbers processes differently from us,
/// [`Error::NoSuchProcess`] if it doesn't exist or started at another time.
pub(crate) fn process_directory(target: u32, start_time: Option<u64>) -> Result<OwnedFd> {
    // The target is a PID as our /proc numbers it. With another namespace's
    // /proc, the same number is another process.
    if !super::proc_is_ours() {
        return Err(Error::ForeignProc);
    }
    let directory = open(
        format!("/proc/{target}").as_str(),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| match errno {
        Errno::ENOENT | Errno::ESRCH => Error::NoSuchProcess { pid: target },
        errno => Error::sys("opening the container's /proc entry", errno),
    })?;
    if let Some(expected) = start_time
        && start_time_at(&directory) != Some(expected)
    {
        return Err(Error::NoSuchProcess { pid: target });
    }
    Ok(directory)
}

/// The `kind` namespace of the process whose `/proc` directory (PID
/// `target`, for errors) is open at `directory`.
///
/// # Errors
///
/// [`Error::NoSuchProcess`] if it's gone, [`Error::ContainerExiting`] if it
/// has exited but not been reaped, a refusal if it isn't ours to open.
pub(crate) fn namespace_at(directory: &OwnedFd, target: u32, kind: Namespace) -> Result<OwnedFd> {
    openat(
        directory,
        format!("ns/{}", kind.proc_name()).as_str(),
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| match errno {
        // Gone since the directory was opened (and maybe its PID reused:
        // this descriptor never reaches the new process).
        Errno::ESRCH => Error::NoSuchProcess { pid: target },
        // A process that has exited but not been reaped has no namespaces
        // left.
        Errno::ENOENT => Error::ContainerExiting { pid: target },
        Errno::EACCES | Errno::EPERM => permission_denied(),
        errno => Error::sys("opening the container's namespaces", errno),
    })
}

/// What the command needs to join `namespaces` itself: plain descriptors
/// and flags.
pub(crate) fn join_setup(namespaces: &[(Namespace, std::os::fd::RawFd)]) -> JoinSetup {
    JoinSetup {
        namespaces: namespaces
            .iter()
            .map(|&(kind, fd)| JoinedNamespace {
                fd,
                flag: kind.clone_flag(),
                index: kind.index(),
            })
            .collect(),
    }
}

/// The start time of the process whose `/proc` directory is open at
/// `directory` (none if it has exited).
pub(crate) fn start_time_at(directory: &OwnedFd) -> Option<u64> {
    let fd = openat(
        directory,
        "stat",
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let mut stat = Vec::new();
    std::io::Read::read_to_end(&mut std::fs::File::from(fd), &mut stat).ok()?;
    super::start_time_in(&stat)
}

/// The kernel said no: missing privileges, or, as root, a policy (a
/// container without privileges, seccomp, an LSM).
pub(crate) fn permission_denied() -> Error {
    if nix::unistd::geteuid().is_root() {
        Error::JoinRefused
    } else {
        Error::JoinNotYours
    }
}
