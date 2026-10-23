//! The parent's part of a container's network namespace: moving the host's
//! network devices into it (`linux.netDevices`). Joining one is
//! `joins.rs`'s.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use nix::unistd::Pid;
use shipyard_network::route::{Rtnetlink, RtnetlinkError};

use crate::error::{Error, Result};
use crate::isolation::NetDevice;

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
