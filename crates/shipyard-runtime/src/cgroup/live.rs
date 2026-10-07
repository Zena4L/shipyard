//! The live cgroup tree, read from the kernel (Linux).

use std::path::{Path, PathBuf};

use super::{Controller, ROOT, parse_controllers, parse_own_cgroup};

/// Whether `path` is a cgroup v2 mount (`statfs` says `CGROUP2_SUPER_MAGIC`,
/// the probe's G1). A v1 or hybrid host mounts something else there.
#[must_use]
pub fn is_cgroup2(path: &Path) -> bool {
    nix::sys::statfs::statfs(path)
        .is_ok_and(|fs| fs.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC)
}

/// This process's cgroup, relative to the root it can see (its cgroup
/// namespace's): `None` without a v2 line in `/proc/self/cgroup`.
#[must_use]
pub fn own_cgroup() -> Option<PathBuf> {
    std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| parse_own_cgroup(&text))
}

/// The controllers the root of the visible tree offers (its
/// `cgroup.controllers`); empty if it can't be read.
#[must_use]
pub fn available_controllers() -> Vec<Controller> {
    std::fs::read_to_string(Path::new(ROOT).join("cgroup.controllers"))
        .map(|text| parse_controllers(&text))
        .unwrap_or_default()
}
