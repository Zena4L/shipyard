//! Control groups, version 2 (stage 5; `cgroups(7)`, the kernel's
//! `Documentation/admin-guide/cgroup-v2.rst`).
//!
//! A **cgroup** is a directory under `/sys/fs/cgroup`. Every process belongs
//! to exactly one, and its files set what the processes in it may use
//! (`memory.max`, `pids.max`, …) and report what they did use
//! (`memory.events`, `cpu.stat`). Shipyard writes them itself (the
//! "cgroupfs" way, as runc's default driver does), without systemd.
//!
//! This module makes and removes containers' cgroups (`tree.rs`,
//! `manage.rs`), writes their limits, freezes, signals and reads them, and
//! attaches their device programs (`devices.rs`). Its parsers are pure, so
//! they're tested on any platform; what touches the live tree is Linux's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the cgroup v2 tree is mounted.
pub const ROOT: &str = "/sys/fs/cgroup";

/// Below the base ([`ROOT`] for root, the delegated subtree for anyone else),
/// where `shipyard run`'s containers go (`<pid>`).
pub const RUN_PARENT: &str = "shipyard/run";

/// Below the base, where `shipyard-runtime`'s containers go (`<id>`, or a
/// bundle's relative `linux.cgroupsPath`).
pub const RUNTIME_PARENT: &str = "shipyard/runtime";

/// The controllers Shipyard uses: each limits one kind of resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Controller {
    /// CPUs and memory nodes a cgroup may use.
    Cpuset,
    /// CPU time.
    Cpu,
    /// Block I/O.
    Io,
    /// Memory and swap.
    Memory,
    /// The number of processes.
    Pids,
}

impl Controller {
    /// Every controller Shipyard uses, in the kernel's order.
    pub const ALL: [Self; 5] = [Self::Cpuset, Self::Cpu, Self::Io, Self::Memory, Self::Pids];

    /// The kernel's name for it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpuset => "cpuset",
            Self::Cpu => "cpu",
            Self::Io => "io",
            Self::Memory => "memory",
            Self::Pids => "pids",
        }
    }

    /// The controller with the kernel's name `name`, if Shipyard uses it.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

impl std::fmt::Display for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The controllers listed in a `cgroup.controllers` (or
/// `cgroup.subtree_control`) file: names separated by spaces. Ones Shipyard
/// doesn't use (`hugetlb`, `rdma`, `misc`) are left out.
#[must_use]
pub fn parse_controllers(text: &str) -> Vec<Controller> {
    let mut found: Vec<Controller> = text
        .split_whitespace()
        .filter_map(Controller::from_name)
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// The process's own cgroup, from `/proc/self/cgroup`: the path of its
/// cgroup v2 line, `0::PATH`, relative to the cgroup root it can see. A v1
/// or hybrid host lists other lines too (`N:controllers:PATH`), which are
/// ignored; `None` if there's no v2 line.
#[must_use]
pub fn parse_own_cgroup(text: &str) -> Option<PathBuf> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| PathBuf::from(path.trim_end()))
}

/// A flat-keyed file (`memory.events`, `cgroup.events`, `cpu.stat`,
/// `pids.events`): one `key value` pair per line. Lines that don't fit are
/// skipped.
#[must_use]
pub fn parse_keyed(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            Some((key.to_owned(), value.trim().parse().ok()?))
        })
        .collect()
}

/// Where an ordinary user may make cgroups: the highest ancestor of its own
/// cgroup (`own`, relative to the root it can see) that it still owns, as
/// systemd's `Delegate=` leaves `user@UID.service` (the probe's G11). `owner`
/// gives the uid owning a cgroup directory, relative to that root, or `None`
/// if it can't be read. `None` if not even its own cgroup is the user's.
///
/// The root itself counts too: a host's is root's, but inside a cgroup
/// namespace the root is the namespace's, and a container runtime may have
/// delegated it whole (Shipyard inside a rootless Podman container).
///
/// Above the delegated cgroup the directories are root's, and the kernel
/// refuses to move a process between two cgroups unless the user owns their
/// common ancestor, so containers must live below it.
#[must_use]
pub fn delegation_root(
    own: &Path,
    uid: u32,
    owner: impl Fn(&Path) -> Option<u32>,
) -> Option<PathBuf> {
    let mut found = None;
    for candidate in own.ancestors() {
        if candidate.as_os_str().is_empty() || owner(candidate) != Some(uid) {
            break;
        }
        found = Some(candidate.to_path_buf());
    }
    found
}

mod stats;

pub use stats::{
    Blkio, BlkioEntry, Cpu, CpuUsage, Memory, MemoryEntry, Pids, Stats, Throttling, parse_io_stat,
};

#[cfg(target_os = "linux")]
mod devices;
#[cfg(target_os = "linux")]
pub(crate) use devices::{devices_without_cgroup, enforce_devices, is_host_root};
#[cfg(target_os = "linux")]
mod live;
#[cfg(target_os = "linux")]
mod manage;
#[cfg(target_os = "linux")]
mod tree;
#[cfg(target_os = "linux")]
pub use live::{available_controllers, is_cgroup2, own_cgroup};
#[cfg(target_os = "linux")]
pub use manage::{Cgroup, CgroupError, RunCgroup, run_cgroup, runtime_cgroup};
#[cfg(target_os = "linux")]
pub use tree::{NoTree, Tree};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controllers_are_read_by_name() {
        assert_eq!(
            parse_controllers("cpuset cpu io memory hugetlb pids rdma misc\n"),
            Controller::ALL.to_vec()
        );
        assert_eq!(
            parse_controllers("memory pids memory"),
            [Controller::Memory, Controller::Pids]
        );
        assert!(parse_controllers("").is_empty());
    }

    #[test]
    fn the_v2_line_is_the_own_cgroup() {
        assert_eq!(parse_own_cgroup("0::/\n"), Some(PathBuf::from("/")));
        assert_eq!(
            parse_own_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n"),
            Some(PathBuf::from("/user.slice/user-1000.slice/session-2.scope"))
        );
        // A hybrid host lists v1 hierarchies first.
        assert_eq!(
            parse_own_cgroup("12:memory:/x\n1:name=systemd:/y\n0::/z\n"),
            Some(PathBuf::from("/z"))
        );
        assert_eq!(parse_own_cgroup("12:memory:/x\n"), None, "v1 only");
    }

    #[test]
    fn keyed_files_are_read_as_numbers() {
        let events = parse_keyed("low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\noom_group_kill 0\n");
        assert_eq!(events.get("oom_kill"), Some(&1));
        assert_eq!(events.get("max"), Some(&3));
        assert_eq!(parse_keyed("populated 1\nfrozen 0\nbroken\nx y\n").len(), 2);
    }

    #[test]
    fn the_delegation_root_is_the_highest_cgroup_the_user_owns() {
        let own = Path::new("/user.slice/user-1000.slice/user@1000.service/app.slice/term.scope");
        let owner = |path: &Path| {
            Some(
                if path.starts_with("/user.slice/user-1000.slice/user@1000.service") {
                    1000
                } else {
                    0
                },
            )
        };
        assert_eq!(
            delegation_root(own, 1000, owner),
            Some(PathBuf::from(
                "/user.slice/user-1000.slice/user@1000.service"
            ))
        );
        // A session scope systemd didn't delegate: root's.
        assert_eq!(
            delegation_root(
                Path::new("/user.slice/user-1000.slice/session-2.scope"),
                1000,
                owner
            ),
            None
        );
        // A cgroup namespace's root delegated whole: the root is the user's.
        assert_eq!(
            delegation_root(Path::new("/session"), 1000, |_| Some(1000)),
            Some(PathBuf::from("/"))
        );
        // A host's root is root's.
        assert_eq!(delegation_root(Path::new("/"), 1000, |_| Some(0)), None);
        // An unreadable owner stops the walk.
        assert_eq!(delegation_root(own, 1000, |_| None), None);
    }
}
