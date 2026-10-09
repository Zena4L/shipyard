//! Trying, in this process, what a container needs of the host
//! (`shipyard doctor`'s probe helper, Linux).
//!
//! Reading the host's settings can't say everything: the AppArmor restriction
//! on unprivileged user namespaces lets one be made, then takes its
//! capabilities away, so the first sign is a mount that fails. The probe
//! does what a rootless run does, in miniature: a user namespace (rootless),
//! a mount namespace, a tmpfs, and an overlay where the image store is.
//!
//! Everything it changes is this process's: its namespaces, and the mounts
//! in them, go when it ends. So it runs in a helper process of its own,
//! single-threaded (`unshare(CLONE_NEWUSER)` needs that), never in the CLI.

use std::path::Path;

use nix::errno::Errno;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};

/// One step's result: `None` where it wasn't tried (an earlier one failed,
/// or it isn't needed).
pub type Step = Option<Result<(), Errno>>;

/// What the probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Probe {
    /// A user namespace with this process's own IDs mapped (rootless only).
    pub user_namespace: Step,
    /// A mount namespace of its own, and a tmpfs mounted in it: what a
    /// policy that takes a user namespace's capabilities away refuses.
    pub mount: Step,
    /// An overlay mounted as a container's root filesystem is, in the
    /// scratch directory.
    pub overlay: Step,
}

/// Tries each step in turn, stopping at the first that fails. `scratch` is
/// an empty directory of this user's on the image store's filesystem, with
/// `lower`, `upper`, `work`, `merged` and `tmp` in it (made by the caller,
/// which removes it after). Rootless (`rootless`), the steps run in a new
/// user namespace, as a rootless run's do.
#[must_use]
pub fn probe(scratch: &Path, rootless: bool) -> Probe {
    let mut probe = Probe::default();
    if rootless {
        let made = own_user_namespace();
        probe.user_namespace = Some(made);
        if made.is_err() {
            return probe;
        }
    }
    let mounted = private_mounts().and_then(|()| {
        mount(
            Some("tmpfs"),
            &scratch.join("tmp"),
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )
    });
    probe.mount = Some(mounted);
    if mounted.is_err() {
        return probe;
    }
    probe.overlay = Some(super::rootfs::overlay::mount_overlay(
        &scratch.join("merged"),
        &[&scratch.join("lower")],
        &scratch.join("upper"),
        &scratch.join("work"),
        rootless,
    ));
    probe
}

/// A user namespace of this process's own, with its own IDs mapped to root
/// in it, as an unprivileged process may (`setgroups` denied first).
fn own_user_namespace() -> Result<(), Errno> {
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    unshare(CloneFlags::CLONE_NEWUSER)?;
    let write = |file: &str, text: &str| {
        std::fs::write(format!("/proc/self/{file}"), text).map_err(|err| super::io_errno(&err))
    };
    write("setgroups", "deny")?;
    write("uid_map", &format!("0 {uid} 1"))?;
    write("gid_map", &format!("0 {gid} 1"))
}

/// A mount namespace of this process's own, where no mount propagates back
/// to the host's.
fn private_mounts() -> Result<(), Errno> {
    unshare(CloneFlags::CLONE_NEWNS)?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
}
