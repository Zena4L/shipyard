//! The parent's part of a container's network namespace: joining an
//! existing one (an OCI bundle's `path`), and moving the host's network
//! devices into it (`linux.netDevices`).
//!
//! A namespace is joined the way runc joins one: the parent enters it,
//! clones the child without `CLONE_NEWNET` (so the child starts in it), and
//! goes back to its own. The child couldn't join it itself: once its new
//! user namespace exists, it has no privileges over a network namespace the
//! host's user namespace owns (`setns(2)` needs `CAP_SYS_ADMIN` over the
//! namespace's owner).
//!
//! If the parent can't go back (a kernel that refuses the way back after
//! allowing the way there), it aborts (SIGABRT): every socket and device it
//! touched afterwards would be the container's.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sched::{CloneFlags, setns};
use nix::sys::stat::Mode;
use nix::sys::statfs::fstatfs;
use nix::unistd::Pid;
use shipyard_network::route::{Rtnetlink, RtnetlinkError};

use crate::error::{Error, Result};
use crate::isolation::NetDevice;

/// The magic number of `nsfs`, the filesystem of `/proc/PID/ns/*` (and of
/// a bind mount of one).
const NSFS_MAGIC: u32 = 0x6e73_6673;

/// A network namespace to start the child in, and the parent's own to come
/// back to.
#[derive(Debug)]
pub(crate) struct NetworkJoin {
    path: PathBuf,
    target: OwnedFd,
    own: File,
}

impl NetworkJoin {
    /// Opens the namespace at `path` (and the parent's own), before the
    /// clone: a path that can't be opened, or isn't a namespace, fails the
    /// run before anything starts. Opened without blocking and without
    /// taking a terminal, and checked to be on `nsfs` before it's used: a
    /// path to a FIFO or a device can't hang the run or reach the device.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let opening = || format!("opening the network namespace {}", path.display());
        let flags = OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOCTTY | OFlag::O_CLOEXEC;
        let target =
            open(path, flags, Mode::empty()).map_err(|errno| Error::sys(opening(), errno))?;
        let filesystem = fstatfs(&target)
            .map_err(|errno| Error::sys(opening(), errno))?
            .filesystem_type();
        if u64::try_from(filesystem.0).ok() != Some(u64::from(NSFS_MAGIC)) {
            return Err(Error::Network(format!(
                "joining {}: it isn't a namespace",
                path.display()
            )));
        }
        let own = File::open("/proc/thread-self/ns/net").map_err(|error| {
            Error::sys(
                "opening shipyard's own network namespace",
                super::io_errno(&error),
            )
        })?;
        Ok(Self {
            path: path.to_owned(),
            target,
            own,
        })
    }

    /// Whether it's shipyard's own network namespace (the host's, usually):
    /// joining it gives the container nothing of its own.
    pub(crate) fn is_shipyards_own(&self) -> bool {
        match (nix::sys::stat::fstat(&self.target), self.own.metadata()) {
            (Ok(target), Ok(own)) => target.st_dev == own.dev() && target.st_ino == own.ino(),
            // Unknown: assume the worst.
            _ => true,
        }
    }

    /// The path, for messages.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Enters the namespace. `setns` checks it's a network namespace.
    pub(crate) fn enter(&self) -> Result<()> {
        setns(self.target.as_fd(), CloneFlags::CLONE_NEWNET).map_err(|errno| {
            let path = self.path.display();
            match errno {
                Errno::EINVAL => {
                    Error::Network(format!("joining {path}: it isn't a network namespace"))
                }
                Errno::EPERM if !nix::unistd::geteuid().is_root() => Error::Network(format!(
                    "joining the network namespace {path}: it needs root"
                )),
                Errno::EPERM => Error::Network(format!(
                    "joining the network namespace {path}: it belongs to a user namespace \
                     shipyard has no privileges over"
                )),
                _ => Error::sys(format!("joining the network namespace {path}"), errno),
            }
        })
    }

    /// Goes back to the parent's own namespace, after the clone (`child`,
    /// if it was made). If it can't, every socket and device this process
    /// touches from now on would be the container's: the child is killed,
    /// and the process aborts rather than go on there.
    pub(crate) fn leave(&self, child: Option<Pid>) {
        if let Err(errno) = setns(self.own.as_fd(), CloneFlags::CLONE_NEWNET) {
            if let Some(child) = child {
                let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
            }
            tracing::error!(
                %errno,
                "couldn't return to shipyard's own network namespace after joining a container's; \
                 aborting rather than go on in it"
            );
            std::process::abort();
        }
    }
}

/// The network namespace of the process `pid` (still waiting for its go
/// byte), opened by descriptor from our own /proc (the caller checked it
/// numbers our children): a reused PID can't redirect what's done to it.
pub(crate) fn open_netns(pid: Pid) -> Result<File> {
    File::open(format!("/proc/{pid}/ns/net")).map_err(|error| {
        Error::sys(
            "opening the container's network namespace",
            super::io_errno(&error),
        )
    })
}

/// Moves `devices` from this process's network namespace into `container`
/// (a network namespace), each renamed as asked, in one request each: a
/// device never appears there under its host name.
///
/// If one can't be moved, the ones already moved are moved back, under their
/// host names (best effort): otherwise they'd be stranded in a namespace
/// about to die, and come back (a physical device) to the host's first
/// network namespace under their container names, or (a virtual one) be
/// destroyed.
pub(crate) fn move_devices(container: BorrowedFd<'_>, devices: &[NetDevice]) -> Result<()> {
    let mut moved = Vec::new();
    for device in devices {
        if let Err(error) = move_device(device, container) {
            move_back(&moved, container);
            return Err(error);
        }
        moved.push(device);
    }
    Ok(())
}

/// Moves `device` into `container` (a network namespace), renamed.
fn move_device(device: &NetDevice, container: BorrowedFd<'_>) -> Result<()> {
    let moving = format!(
        "moving the network device {} into the container",
        device.host
    );
    let fail = |error: &RtnetlinkError| Error::Network(format!("{moving}: {}", cause(error)));
    let mut rtnetlink = Rtnetlink::new().map_err(|error| fail(&error))?;
    let link = rtnetlink
        .link(&device.host)
        .map_err(|error| fail(&error))?
        .ok_or_else(|| Error::Network(format!("{moving}: there's no such device on the host")))?;
    rtnetlink
        .move_to(link.index, container, Some(&device.name))
        .map_err(|error| {
            Error::Network(match error.errno() {
                Some(Errno::EPERM) => {
                    format!("{moving}: it needs root (CAP_NET_ADMIN over the host's network)")
                }
                Some(Errno::EEXIST) => format!(
                    "{moving}: the container already has a device named {}",
                    device.name
                ),
                _ => format!("{moving}: {}", cause(&error)),
            })
        })
}

/// Moves `moved` back out of `container`, renamed as they were on the host:
/// best effort, after a later device failed.
fn move_back(moved: &[&NetDevice], container: BorrowedFd<'_>) {
    if moved.is_empty() {
        return;
    }
    let back = || -> Result<(), RtnetlinkError> {
        // This thread's namespace is the one they came from.
        let own = open(
            "/proc/thread-self/ns/net",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(RtnetlinkError::Namespace)?;
        let mut inside = Rtnetlink::in_namespace(container)?;
        for device in moved {
            if let Some(link) = inside.link(&device.name)? {
                inside.move_to(link.index, own.as_fd(), Some(&device.host))?;
            }
        }
        Ok(())
    };
    if let Err(error) = back() {
        tracing::warn!(
            %error,
            "couldn't move the container's network devices back to the host after a failure"
        );
    }
}

/// What went wrong, for a message that already says what was being done:
/// the errno, and the kernel's own words where it gave them.
fn cause(error: &RtnetlinkError) -> String {
    match (error.errno(), error.kernel_reason()) {
        (Some(errno), Some(reason)) => format!("{} (the kernel says: {reason})", errno.desc()),
        (Some(errno), None) => errno.desc().to_owned(),
        (None, _) => error.to_string(),
    }
}
