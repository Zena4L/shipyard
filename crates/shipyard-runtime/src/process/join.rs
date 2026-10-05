//! Joining a running process's namespaces, as `exec` into a container does
//! (`setns(2)`, `namespaces(7)`; Linux).
//!
//! **Holding a namespace.** `/proc/PID/ns/<kind>` opens to a descriptor for
//! that namespace, and `setns` on it moves the calling thread in. They're all
//! opened through one descriptor for the `/proc/PID` directory, which stays
//! bound to that one process: if it dies and its PID is reused, opening
//! through it fails (ESRCH) instead of reaching the newcomer (checked:
//! `scripts/probes/join.c`).
//!
//! **The PID namespace is the odd one.** `setns` never moves the caller into
//! a PID namespace; it only decides where the caller's *next children* are
//! born. And the joined process must be shipyard's own child: an orphan is
//! re-parented to the container's init, not to a subreaper outside, so its
//! exit status would be lost. So shipyard switches *this thread's* children
//! into the container, forks, and switches straight back:
//!
//! ```text
//! setns(container's pid ns) → fork → setns(our own pid ns)
//!                               └─► child: PID n inside, our child; it joins
//!                                   the other namespaces itself (child.rs)
//! ```
//!
//! `setns` affects only the calling thread, so other threads keep starting
//! processes on the host. While switched, this thread can't start threads
//! (the kernel refuses `CLONE_THREAD`), so nothing but the fork happens in
//! between. The other namespaces are joined in the child: a mount namespace
//! can't be joined by a thread that shares its filesystem context with
//! others, and every thread does.

use std::os::fd::{AsRawFd, OwnedFd};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat};
use nix::sched::{CloneFlags, setns};
use nix::sys::stat::Mode;

use super::plan::{JoinSetup, JoinedNamespace};
use super::spawn::fork_error;
use crate::isolation::{Join, Namespace};
use crate::{Error, Result};

/// A target's namespaces, opened before the fork.
#[derive(Debug)]
pub(crate) struct Joining {
    target: u32,
    /// Its `/proc` directory, bound to that one process.
    directory: OwnedFd,
    /// The target's PID namespace and our own, to switch this thread's
    /// children into the target's and back.
    pid_switch: Option<PidSwitch>,
    /// The rest, for the child to join, in the order it joins them.
    others: Vec<(Namespace, OwnedFd)>,
}

#[derive(Debug)]
struct PidSwitch {
    target: OwnedFd,
    own: OwnedFd,
}

impl Joining {
    /// Opens `join.target`'s namespaces.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchProcess`] if the target doesn't exist,
    /// [`Error::ContainerExiting`] if it has exited but not been reaped (its
    /// namespaces are gone), [`Error::ForeignProc`] if `/proc` numbers
    /// processes differently from us, and a system error otherwise.
    pub(crate) fn open(join: &Join) -> Result<Self> {
        let target = join.target;
        // The target is a PID as our /proc numbers it. With another
        // namespace's /proc, the same number is another process.
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
        let namespace = |kind: Namespace| {
            openat(
                &directory,
                format!("ns/{}", kind.proc_name()).as_str(),
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|errno| match errno {
                // Gone since the directory was opened (and maybe its PID
                // reused: this descriptor never reaches the new process).
                Errno::ESRCH => Error::NoSuchProcess { pid: target },
                // A process that has exited but not been reaped has no
                // namespaces left.
                Errno::ENOENT => Error::ContainerExiting { pid: target },
                Errno::EACCES | Errno::EPERM => permission_denied(),
                errno => Error::sys("opening the container's namespaces", errno),
            })
        };
        let pid_switch = if join.has(Namespace::Pid) {
            Some(PidSwitch {
                target: namespace(Namespace::Pid)?,
                own: open(
                    "/proc/self/ns/pid",
                    OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|errno| Error::sys("opening shipyard's own PID namespace", errno))?,
            })
        } else {
            None
        };
        let mut kinds: Vec<Namespace> = join
            .namespaces
            .iter()
            .copied()
            .filter(|&kind| kind != Namespace::Pid)
            .collect();
        // The mount namespace last: joining it moves the root and working
        // directory into the container.
        kinds.sort_by_key(|&kind| kind == Namespace::Mount);
        let others = kinds
            .into_iter()
            .map(|kind| Ok((kind, namespace(kind)?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            target,
            directory,
            pid_switch,
            others,
        })
    }

    /// What the child needs to join the rest: plain descriptors and flags.
    pub(crate) fn setup(&self) -> JoinSetup {
        JoinSetup {
            namespaces: self
                .others
                .iter()
                .map(|(kind, fd)| JoinedNamespace {
                    fd: fd.as_raw_fd(),
                    flag: kind.clone_flag(),
                    index: kind.index(),
                })
                .collect(),
        }
    }

    /// Forks with this thread's children born in the target's PID namespace
    /// (if joined), then switches back. Returns what `fork` returned.
    ///
    /// # Errors
    ///
    /// [`Error::JoinNeedsRoot`] or [`Error::JoinRefused`] if the switch was
    /// refused, [`Error::ContainerExiting`] if the namespace's init has
    /// ended, or the fork's error. If switching back fails, the process
    /// aborts: this thread must never go on starting processes inside.
    ///
    /// # Safety
    ///
    /// As for `fork`: in the child, only async-signal-safe code may run.
    pub(crate) unsafe fn fork(&self) -> Result<nix::unistd::ForkResult> {
        let Some(switch) = &self.pid_switch else {
            // SAFETY: the caller's contract.
            return unsafe { nix::unistd::fork() }.map_err(fork_error);
        };
        setns(&switch.target, CloneFlags::CLONE_NEWPID).map_err(|errno| match errno {
            Errno::EPERM => permission_denied(),
            // Only namespaces nested in our own can be entered (a container
            // started from inside another one can't be joined from outside
            // it, and the other way round).
            Errno::EINVAL => Error::sys(
                "entering the container's PID namespace (it isn't nested in shipyard's own)",
                errno,
            ),
            errno => Error::sys("entering the container's PID namespace", errno),
        })?;
        // SAFETY: the caller's contract. Nothing but this runs while the
        // thread is switched.
        let forked = match unsafe { nix::unistd::fork() } {
            // In the child, which is inside: nothing to switch back.
            Ok(nix::unistd::ForkResult::Child) => return Ok(nix::unistd::ForkResult::Child),
            Ok(nix::unistd::ForkResult::Parent { child }) => Ok(child),
            Err(errno) => Err(errno),
        };
        // Our own active namespace: the kernel always lets a process re-enter
        // it. If even that fails, this thread would start every later process
        // (any `Command`, in a program embedding this) inside the container,
        // and nothing can undo that: stop the whole process instead.
        if let Err(errno) = setns(&switch.own, CloneFlags::CLONE_NEWPID) {
            if let Ok(child) = forked {
                // It's still waiting for the go signal, so it never ran.
                let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                let _ = nix::sys::wait::waitpid(child, None);
            }
            tracing::error!(
                %errno,
                "couldn't return to shipyard's own PID namespace after joining a \
                 container's; aborting rather than start processes inside it"
            );
            std::process::abort();
        }
        match forked {
            Ok(child) => Ok(nix::unistd::ForkResult::Parent { child }),
            // A PID namespace whose init has ended takes no new processes;
            // otherwise ENOMEM is just what it says.
            Err(Errno::ENOMEM) if self.target_is_exiting() => {
                Err(Error::ContainerExiting { pid: self.target })
            }
            Err(errno) => Err(fork_error(errno)),
        }
    }

    /// Whether the target has ended (or is ending): from the moment it
    /// starts to exit, its namespaces are gone from `/proc`, though its PID
    /// namespace lives on while anything is in it (checked:
    /// `scripts/probes/join.c`).
    fn target_is_exiting(&self) -> bool {
        matches!(
            openat(
                &self.directory,
                "ns/mnt",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            ),
            Err(Errno::ENOENT | Errno::ESRCH)
        )
    }
}

/// The kernel said no: missing privileges, or, as root, a policy (a
/// container without privileges, seccomp, an LSM).
fn permission_denied() -> Error {
    if nix::unistd::geteuid().is_root() {
        Error::JoinRefused
    } else {
        Error::JoinNeedsRoot
    }
}
