//! The namespaces a new container joins instead of making them (step
//! 11.4b2): an OCI bundle's `path`s, another container's network or IPC
//! namespace (`--network container:NAME`), each opened before anything
//! starts.
//!
//! A namespace is joined the way runc joins one: the parent enters it,
//! clones the child without that kind's `CLONE_NEW*` flag (so the child
//! starts in it), and goes back to its own. The child couldn't join it
//! itself: once its new user namespace exists, it has no privileges over a
//! namespace the host's user namespace owns (`setns(2)` needs
//! `CAP_SYS_ADMIN` over the namespace's owner). Network, IPC and UTS
//! namespaces are a thread's own, so a multithreaded parent can enter and
//! leave them (`setns(2)`); a user or mount namespace it couldn't. A PID
//! namespace's `setns` only places the next children, and what's cloned
//! there is the other container's to see: the clean image joins it, and
//! every other with it, before it clones the container's init (`init.rs`'s
//! `join_then_clone`, F134).
//!
//! If the parent can't go back (a kernel that refuses the way back after
//! allowing the way there), it aborts (SIGABRT): every socket, segment and
//! name it touched afterwards would be the container's.
//!
//! **Another container's user namespace** (rootless: its network and IPC
//! namespaces are its user namespace's, and `setns` into them needs
//! `CAP_SYS_ADMIN` there) can't be entered by the parent: nothing leaves a
//! user namespace, and a multithreaded process can't enter one. So the
//! parent forks a short-lived process (`enterer.rs`),
//! single-threaded, which enters it and then the rest, and clones the child
//! with `CLONE_PARENT` (the parent's own child) before it exits. The child
//! is born in a PID namespace of its own, so nothing in the other
//! container can see or trace it while it still holds the parent's
//! descriptors. The namespace's maps must be the ones the container was
//! given (`Isolation::id_maps`), checked here: the setup goes by them.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sched::{CloneFlags, setns};
use nix::sys::stat::Mode;
use nix::sys::statfs::{NSFS_MAGIC, fstatfs};
use nix::unistd::Pid;

use crate::error::{Error, Result};
use crate::id_map::IdMaps;
use crate::isolation::{Isolation, JoinSource, Namespace};

/// The namespaces a new container starts in, each with the parent's own to
/// come back to, in the order they're entered.
#[derive(Debug, Default)]
pub(crate) struct NamespaceJoins {
    /// A process's user namespace, entered first, and only by a process of
    /// its own.
    user: Option<OwnedFd>,
    joined: Vec<Joined>,
    /// The mount namespace of the process whose IPC namespace is joined:
    /// its `/dev/shm` is shared too, cloned there by the setup (Docker's
    /// `--ipc container:`; a mount can't be bound from a namespace the
    /// binder isn't in).
    shm_mount_ns: Option<OwnedFd>,
}

/// One namespace to start the child in.
#[derive(Debug)]
struct Joined {
    kind: Namespace,
    /// Whose it is, for messages: a path, or a process.
    whose: String,
    target: OwnedFd,
    own: File,
}

impl NamespaceJoins {
    /// Opens every namespace `isolation` joins (its `joins`, and its
    /// `network_path`), and the parent's own of each kind, before the
    /// clone: one that can't be opened, or isn't a namespace, fails the run
    /// before anything starts. A process's are opened through its `/proc`
    /// directory, if it started when it should have (a reused PID is never
    /// joined); a path's as a path first, checked to be on `nsfs` (no
    /// device's `open` runs, a FIFO can't hang the run). With a process's
    /// IPC namespace, its mount namespace too (its `/dev/shm` is cloned
    /// there). Nothing is looked up again afterwards.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchProcess`] or [`Error::ContainerExiting`] for a
    /// process gone or going, [`Error::Network`] or [`Error::Namespace`]
    /// for a path that can't be joined, a system error otherwise.
    pub(crate) fn open(isolation: &Isolation) -> Result<Self> {
        let user = isolation
            .joins
            .iter()
            .find(|join| join.kind == Namespace::User)
            .map(|join| match join.from {
                JoinSource::Process { pid, start_time } => {
                    user_namespace(pid, start_time, isolation.id_maps.as_ref())
                }
                // `Isolation::check` refuses it.
                JoinSource::Path(_) => Err(Error::Namespace(
                    "a user namespace is joined only as a process's".to_owned(),
                )),
            })
            .transpose()?;
        let mut joined = Vec::new();
        if let Some(path) = &isolation.network_path {
            joined.push(Joined::by_path(Namespace::Network, path)?);
        }
        for join in isolation
            .joins
            .iter()
            .filter(|join| join.kind != Namespace::User)
        {
            joined.push(match &join.from {
                JoinSource::Path(path) => Joined::by_path(join.kind, path)?,
                JoinSource::Process { pid, start_time } => {
                    Joined::of_process(join.kind, *pid, *start_time)?
                }
            });
        }
        let shm_mount_ns = isolation
            .joins
            .iter()
            .find_map(|join| match join.from {
                JoinSource::Process { pid, start_time } if join.kind == Namespace::Ipc => {
                    Some((pid, start_time))
                }
                _ => None,
            })
            .map(|(pid, start_time)| {
                let directory = super::join::process_directory(pid, Some(start_time))?;
                super::join::namespace_at(&directory, pid, Namespace::Mount)
            })
            .transpose()?;
        Ok(Self {
            user,
            joined,
            shm_mount_ns,
        })
    }

    /// The mount namespace whose `/dev/shm` the container shares, if it
    /// joins a process's IPC namespace.
    pub(crate) fn shm_mount_ns(&self) -> Option<std::os::fd::RawFd> {
        self.shm_mount_ns.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Whether a user namespace is joined: then only a process of its own
    /// enters them ([`enter_for_good`](Self::enter_for_good)).
    pub(crate) fn joins_user(&self) -> bool {
        self.user.is_some()
    }

    /// Whether a PID namespace is joined: then the clean image enters them
    /// all, before the clone (`init.rs`'s `join_then_clone`).
    pub(crate) fn joins_pid(&self) -> bool {
        self.joined
            .iter()
            .any(|joined| joined.kind == Namespace::Pid)
    }

    /// The descriptors the clean image enters, for a PID join: the user
    /// namespace's, then each other one's in order, the PID namespace's
    /// last (it places only the clone).
    pub(crate) fn for_the_clean_image(&self) -> (Option<RawFd>, Vec<(Namespace, RawFd)>) {
        let mut joined: Vec<(Namespace, RawFd)> = self
            .joined
            .iter()
            .map(|joined| (joined.kind, joined.target.as_raw_fd()))
            .collect();
        joined.sort_by_key(|&(kind, _)| kind == Namespace::Pid);
        (self.user.as_ref().map(AsRawFd::as_raw_fd), joined)
    }

    /// Enters the user namespace, then each other one, for good: in a
    /// process forked to start the child, that exits right after.
    /// Async-signal-safe (`setns` only, no allocation), as it runs after
    /// `fork`. Which one failed, as its place (`None` for the user
    /// namespace), for [`refusal`](Self::refusal).
    #[deny(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )]
    pub(crate) fn enter_for_good(&self) -> std::result::Result<(), (Option<usize>, Errno)> {
        if let Some(user) = &self.user {
            setns(user.as_fd(), CloneFlags::CLONE_NEWUSER).map_err(|errno| (None, errno))?;
        }
        for (place, joined) in self.joined.iter().enumerate() {
            setns(joined.target.as_fd(), flag(joined.kind))
                .map_err(|errno| (Some(place), errno))?;
        }
        Ok(())
    }

    /// What [`enter_for_good`](Self::enter_for_good)'s failure at `place`
    /// means.
    pub(crate) fn refusal(&self, place: Option<usize>, errno: Errno) -> Error {
        match place.and_then(|place| self.joined.get(place)) {
            Some(joined) => joined.refusal(errno),
            None => Error::sys("joining the other container's user namespace", errno),
        }
    }

    /// Whose `kind` namespace is joined, if it's shipyard's own (the host's,
    /// usually): joining it gives the container nothing of its own.
    pub(crate) fn shipyards_own_joined(&self, kind: Namespace) -> Option<&str> {
        self.joined
            .iter()
            .find(|joined| joined.kind == kind && joined.is_shipyards_own())
            .map(|joined| joined.whose.as_str())
    }

    /// Enters each namespace. `setns` checks each is of its kind. If one
    /// can't be entered, those already entered are left.
    pub(crate) fn enter(&self) -> Result<()> {
        for (entered, joined) in self.joined.iter().enumerate() {
            if let Err(error) = joined.enter() {
                Self::leave_these(&self.joined[..entered], None);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Goes back to the parent's own namespaces, after the clone (`child`,
    /// if it was made). If it can't, every socket and device this process
    /// touches from now on would be the container's: the child is killed,
    /// and the process aborts rather than go on there.
    pub(crate) fn leave(&self, child: Option<Pid>) {
        Self::leave_these(&self.joined, child);
    }

    fn leave_these(joined: &[Joined], child: Option<Pid>) {
        for joined in joined.iter().rev() {
            if let Err(errno) = setns(joined.own.as_fd(), flag(joined.kind)) {
                if let Some(child) = child {
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                }
                tracing::error!(
                    %errno,
                    namespace = joined.kind.proc_name(),
                    "couldn't return to shipyard's own namespace after joining a container's; \
                     aborting rather than go on in it"
                );
                std::process::abort();
            }
        }
    }
}

impl Joined {
    /// The namespace at `path`.
    fn by_path(kind: Namespace, path: &Path) -> Result<Self> {
        let whose = path.display().to_string();
        let opening = || format!("opening the {} namespace {whose}", noun(kind));
        // Opened as a path first (no driver's `open` runs: a device whose
        // opening does something never sees it), checked to be on `nsfs`,
        // then opened for `setns` through it.
        let place = open(path, OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty())
            .map_err(|errno| Error::sys(opening(), errno))?;
        let filesystem = fstatfs(&place)
            .map_err(|errno| Error::sys(opening(), errno))?
            .filesystem_type();
        // `nsfs`: the filesystem of `/proc/PID/ns/*` (and of a bind mount of
        // one).
        if filesystem != NSFS_MAGIC {
            return Err(refused(
                kind,
                format!("joining {whose}: it isn't a namespace"),
            ));
        }
        let target = open(
            format!("/proc/self/fd/{}", place.as_raw_fd()).as_str(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| Error::sys(opening(), errno))?;
        refuse_a_tracer(kind, &target, &whose)?;
        Ok(Self {
            kind,
            own: own(kind)?,
            whose,
            target,
        })
    }

    /// The process `pid`'s, if it started at `start_time`. A PID namespace
    /// only as its first process's (a container's init): another that
    /// joined it (a chained join) would land this one there unseen. And
    /// only where no process may trace (`refuse_a_tracer`).
    fn of_process(kind: Namespace, pid: u32, start_time: u64) -> Result<Self> {
        let directory = super::join::process_directory(pid, Some(start_time))?;
        if kind == Namespace::Pid && !first_in_its_pid_namespace(&directory, pid)? {
            return Err(Error::Namespace(format!(
                "joining process {pid}'s PID namespace: it isn't that namespace's first process \
                 (it joined another's): join that one's"
            )));
        }
        let target = super::join::namespace_at(&directory, pid, kind)?;
        let whose = format!("process {pid}'s");
        refuse_a_tracer(kind, &target, &whose)?;
        Ok(Self {
            kind,
            own: own(kind)?,
            whose,
            target,
        })
    }

    fn is_shipyards_own(&self) -> bool {
        match (nix::sys::stat::fstat(&self.target), self.own.metadata()) {
            (Ok(target), Ok(own)) => target.st_dev == own.dev() && target.st_ino == own.ino(),
            // Unknown: assume the worst.
            _ => true,
        }
    }

    fn enter(&self) -> Result<()> {
        setns(self.target.as_fd(), flag(self.kind)).map_err(|errno| self.refusal(errno))
    }

    /// What a refusal to enter it means.
    fn refusal(&self, errno: Errno) -> Error {
        let (whose, noun) = (&self.whose, noun(self.kind));
        match errno {
            Errno::EINVAL => refused(
                self.kind,
                format!(
                    "joining {whose}: it isn't {} {noun} namespace",
                    if self.kind == Namespace::Ipc {
                        "an"
                    } else {
                        "a"
                    }
                ),
            ),
            Errno::EPERM if !nix::unistd::geteuid().is_root() => refused(
                self.kind,
                format!("joining the {noun} namespace {whose}: it needs root"),
            ),
            Errno::EPERM => refused(
                self.kind,
                format!(
                    "joining the {noun} namespace {whose}: it belongs to a user namespace \
                         shipyard has no privileges over"
                ),
            ),
            _ => Error::sys(format!("joining the {noun} namespace {whose}"), errno),
        }
    }
}

/// Process `pid`'s user namespace, if it started at `start_time` and its
/// maps are `maps` (the ones the container's setup goes by).
fn user_namespace(pid: u32, start_time: u64, maps: Option<&IdMaps>) -> Result<OwnedFd> {
    let directory = super::join::process_directory(pid, Some(start_time))?;
    let target = super::join::namespace_at(&directory, pid, Namespace::User)?;
    let read = |name: &str| -> Result<String> {
        let failed = |error: &std::io::Error| {
            Error::sys(
                format!("reading process {pid}'s {name}"),
                super::io_errno(error),
            )
        };
        let bytes = shipyard_sys::fs::read_bounded_at(&directory, name, MAPS_MAX)
            .map_err(|error| failed(&error))?;
        String::from_utf8(bytes)
            .map_err(|_| failed(&std::io::Error::from_raw_os_error(libc::EINVAL)))
    };
    let its = IdMaps::from_proc(&read("uid_map")?, &read("gid_map")?);
    if its.as_ref() != maps {
        return Err(Error::JoinedMapsDiffer { pid });
    }
    Ok(target)
}

/// The most of a map file read: the kernel takes at most 340 lines.
const MAPS_MAX: u64 = 64 * 1024;

/// Whether the process whose `/proc` directory is open at `directory` is
/// PID 1 of its own PID namespace: its `NSpid` line's last number (its PID
/// in its innermost namespace, Linux 4.1) is 1.
fn first_in_its_pid_namespace(directory: &OwnedFd, pid: u32) -> Result<bool> {
    let bytes =
        shipyard_sys::fs::read_bounded_at(directory, "status", STATUS_MAX).map_err(|error| {
            Error::sys(
                format!("reading process {pid}'s status"),
                super::io_errno(&error),
            )
        })?;
    first_by_status(&String::from_utf8_lossy(&bytes)).ok_or_else(|| {
        Error::Namespace(format!(
            "joining process {pid}'s PID namespace: its status says no NSpid (Linux 4.1)"
        ))
    })
}

/// [`first_in_its_pid_namespace`] from a `/proc/PID/status`'s text: its
/// `NSpid` line's last number is 1. `None` without that line.
fn first_by_status(status: &str) -> Option<bool> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .and_then(|numbers| numbers.split_whitespace().next_back())
        .map(|innermost| innermost == "1")
}

/// A PID namespace (`target`, whose it is `whose`) is joined only where no
/// process may hold `CAP_SYS_PTRACE` (in its bounding set; one whose set
/// can't be read is taken to): the joining container's set-up is in that
/// namespace, kept out of its processes' reach by being non-dumpable alone,
/// which that capability (in the user namespace its memory belongs to: the
/// host's as root, the other's rootless) overrides (D15.2b-10, D15.2b-12).
/// Every process there: its container's, its `exec`s (cloned by another),
/// other containers' that joined it, a set-up still under way (its init,
/// unconfined at its start gate). Another kind: nothing to check.
fn refuse_a_tracer(kind: Namespace, target: &OwnedFd, whose: &str) -> Result<()> {
    if kind != Namespace::Pid {
        return Ok(());
    }
    match tracer_in(target) {
        Some(pid) => Err(Error::Namespace(format!(
            "joining a PID namespace ({whose}): its process {pid} may hold CAP_SYS_PTRACE, with \
             which it could trace this container as it's set up: join it once that one has \
             ended, or without that capability there"
        ))),
        None => Ok(()),
    }
}

/// A process in the PID namespace open at `namespace` that may hold
/// `CAP_SYS_PTRACE`, if any: every process this one can see is looked at
/// (by its `ns/pid`, the namespace's device and inode); one it may not look
/// at, or gone meanwhile, is none (it's no process of a container of this
/// user's then).
fn tracer_in(namespace: &OwnedFd) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    let theirs = nix::sys::stat::fstat(namespace).ok()?;
    let ptrace = 1_u64 << crate::security::Capability::SYS_PTRACE.number();
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::metadata(format!("/proc/{pid}/ns/pid"))
                .is_ok_and(|ns| ns.dev() == theirs.st_dev && ns.ino() == theirs.st_ino)
        })
        .find(|pid| {
            shipyard_sys::fs::read_bounded(
                std::path::Path::new(&format!("/proc/{pid}/status")),
                STATUS_MAX,
            )
            .is_ok_and(|status| {
                bounding_set(&String::from_utf8_lossy(&status)).is_none_or(|set| set & ptrace != 0)
            })
        })
}

/// A `/proc/PID/status`'s `CapBnd`, if it says one.
fn bounding_set(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
}

/// The most of `/proc/PID/status` read (a few KiB).
const STATUS_MAX: u64 = 64 * 1024;

/// This thread's own namespace of `kind`.
fn own(kind: Namespace) -> Result<File> {
    File::open(format!("/proc/thread-self/ns/{}", kind.proc_name())).map_err(|error| {
        Error::sys(
            format!("opening shipyard's own {} namespace", noun(kind)),
            super::io_errno(&error),
        )
    })
}

fn flag(kind: Namespace) -> CloneFlags {
    CloneFlags::from_bits_retain(kind.clone_flag())
}

/// The kind's name in a sentence.
fn noun(kind: Namespace) -> &'static str {
    match kind {
        Namespace::Network => "network",
        Namespace::Ipc => "IPC",
        Namespace::Uts => "UTS",
        other => other.proc_name(),
    }
}

/// A refusal: a network's is [`Error::Network`], as it always was.
fn refused(kind: Namespace, message: String) -> Error {
    if kind == Namespace::Network {
        Error::Network(message)
    } else {
        Error::Namespace(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PID namespace is joined only through its first process: the last
    /// number of its `NSpid`, its PID in its innermost namespace, is 1.
    #[test]
    fn a_pid_namespace_is_joined_through_its_first_process() {
        let status = |nspid: &str| format!("Name:\tsh\nPid:\t4021\n{nspid}\nPPid:\t1\n");
        assert_eq!(first_by_status(&status("NSpid:\t4021\t1")), Some(true));
        assert_eq!(first_by_status(&status("NSpid:\t4021\t7\t1")), Some(true));
        assert_eq!(
            first_by_status(&status("NSpid:\t4021\t12")),
            Some(false),
            "chained"
        );
        assert_eq!(
            first_by_status(&status("NSpid:\t1")),
            Some(true),
            "the host's init"
        );
        assert_eq!(
            first_by_status(&status("NSpid:\t4021")),
            Some(false),
            "the host's"
        );
        assert_eq!(first_by_status("Name:\tsh\nPid:\t4021\n"), None, "no NSpid");
    }

    /// A status's bounding set, read as the kernel writes it.
    #[test]
    fn a_bounding_set_is_read_from_its_status() {
        assert_eq!(
            bounding_set("Name:\tsleep\nCapBnd:\t00000000a80425fb\nCapAmb:\t0\n"),
            Some(0xa804_25fb)
        );
        assert_eq!(bounding_set("Name:\tsleep\n"), None);
        // Docker's default set lacks CAP_SYS_PTRACE (19); with it, bit 19.
        let ptrace = 1_u64 << crate::security::Capability::SYS_PTRACE.number();
        assert_eq!(0xa804_25fb & ptrace, 0);
        assert_ne!((0xa804_25fb | 0x8_0000) & ptrace, 0);
    }
}
