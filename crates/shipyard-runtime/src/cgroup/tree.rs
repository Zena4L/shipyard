//! The part of the cgroup tree this process manages (stage 5, step 4).
//!
//! For root, that's the whole tree, `/sys/fs/cgroup`. For anyone else, it's the
//! subtree delegated to them: `user@UID.service` on a systemd host, handed over
//! as `Delegate=` does it (the directory and its `cgroup.procs`,
//! `cgroup.subtree_control` and `cgroup.threads` chowned to the user; the
//! probe's G11). Shipyard's cgroups go below that base, in `shipyard/run/<pid>`
//! and `shipyard/runtime/<id>`.
//!
//! One rule finds both: the highest cgroup above this process's own that it
//! owns ([`delegation_root`]). Root owns the whole tree, so its answer is the
//! root itself; root inside a user namespace that was delegated a cgroup
//! namespace (Shipyard in a rootless Podman container) gets that namespace's
//! root. Nothing above the base is ever written: a user can't, and the
//! kernel's rule for moving processes (write access to the common ancestor)
//! keeps every container below it anyway.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::stat::Mode;
use nix::unistd::AccessFlags;

use super::manage::{
    CLAIM_TRIES, POLL, SHIPYARD, absolute, hand_down_controllers, holds_processes, lock_dir,
    remove_if_stale,
};
use super::{
    Cgroup, CgroupError, ROOT, RUN_PARENT, RunCgroup, delegation_root, is_cgroup2, own_cgroup,
    run_cgroup,
};

/// Why this process has no part of the tree to manage.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NoTree {
    /// No cgroup v2 at the root (cgroup v1, or nothing mounted).
    #[error("there's no cgroup v2 tree at {ROOT}")]
    NotV2,
    /// `/proc/self/cgroup` has no v2 line, or can't be read.
    #[error("this process's own cgroup can't be found (/proc/self/cgroup)")]
    NoOwnCgroup,
    /// Not even this process's own cgroup is the user's: nothing was
    /// delegated (an SSH login's session scope is root's).
    #[error(
        "this process's cgroup ({}) isn't delegated to this user; run it under your systemd \
         user manager: `systemd-run --user --scope shipyard …`",
        .0.display()
    )]
    NotDelegated(PathBuf),
    /// The base, or the files a delegation hands over, can't be written (a
    /// read-only tree, as inside a container that wasn't given one).
    #[error("{} can't be written (it and its cgroup.procs and cgroup.subtree_control)", .0.display())]
    NotWritable(PathBuf),
}

/// The part of the cgroup tree this process manages: [`ROOT`] for root, the
/// delegated subtree for anyone else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    base: PathBuf,
}

impl Tree {
    /// This process's part of the tree.
    ///
    /// # Errors
    ///
    /// [`NoTree`], saying why there's none.
    pub fn find() -> Result<Self, NoTree> {
        if !is_cgroup2(Path::new(ROOT)) {
            return Err(NoTree::NotV2);
        }
        let own = own_cgroup().ok_or(NoTree::NoOwnCgroup)?;
        let uid = nix::unistd::geteuid().as_raw();
        let owner = |cgroup: &Path| std::fs::metadata(absolute(cgroup)).ok().map(|m| m.uid());
        let writable = |cgroup: &Path| {
            let dir = absolute(cgroup);
            [
                dir.clone(),
                dir.join("cgroup.procs"),
                dir.join("cgroup.subtree_control"),
            ]
            .iter()
            .all(|path| nix::unistd::access(path, AccessFlags::W_OK).is_ok())
        };
        choose_base(&own, uid, owner, writable).map(|base| Self {
            base: absolute(&base),
        })
    }

    /// Makes the cgroup at `relative` (below the base), with its parents.
    ///
    /// The kernel's order (the probe's G2): a cgroup that holds processes
    /// can't hand domain controllers (memory, I/O) to children, so the
    /// parents, which never hold any, hand down every controller they have
    /// before the leaf is made; the container's processes go only into the
    /// leaf. Only Shipyard's own parents hand everything down: another's (a
    /// bundle's `cgroupsPath`) gets only what limits need ([`Cgroup::enable`]).
    ///
    /// # Errors
    ///
    /// A directory that can't be made, or a leaf that exists already.
    pub fn create(&self, relative: &Path) -> Result<Cgroup, CgroupError> {
        let components: Vec<_> = relative.components().collect();
        let (leaf, parents) = components.split_last().ok_or(CgroupError::Io {
            doing: "making the cgroup",
            path: relative.to_path_buf(),
            errno: Errno::EINVAL,
        })?;
        let mut parent = self.base.clone();
        for component in parents {
            parent.push(component);
            match nix::unistd::mkdir(&parent, Mode::from_bits_truncate(0o755)) {
                // One that holds processes is a container's (another
                // `cgroupsPath` inside it, say): a cgroup made below it would
                // be under that container's limits, devices and freezing,
                // and its kill.
                Err(Errno::EEXIST) if holds_processes(&parent) => {
                    return Err(CgroupError::InsideAnother(parent));
                }
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(errno) => {
                    return Err(CgroupError::Io {
                        doing: "making the cgroup",
                        path: parent,
                        errno,
                    });
                }
            }
            if parent.starts_with(self.base.join(SHIPYARD)) {
                hand_down_controllers(&parent);
            }
        }
        let path = parent.join(leaf);
        match nix::unistd::mkdir(&path, Mode::from_bits_truncate(0o755)) {
            Ok(()) => Ok(Cgroup::at(path, self.base.clone())),
            Err(Errno::EEXIST) => Err(CgroupError::Exists(path)),
            Err(errno) => Err(CgroupError::Io {
                doing: "making the cgroup",
                path,
                errno,
            }),
        }
    }

    /// Makes and locks `run/<pid>`, for the shipyard process `pid` (this
    /// one). A leftover of that name (a gone process that had the PID) is
    /// removed first.
    ///
    /// # Errors
    ///
    /// A cgroup that can't be made, or one of that name a live run holds (a
    /// shipyard with the same PID in another PID namespace).
    pub fn claim_run(&self, pid: u32) -> Result<RunCgroup, CgroupError> {
        let relative = run_cgroup(pid);
        let mut last = CgroupError::RunInUse(self.base.join(&relative));
        for _ in 0..CLAIM_TRIES {
            match self.create(&relative) {
                Ok(cgroup) => match lock_dir(cgroup.path()) {
                    Ok(lock) => return Ok(RunCgroup::new(cgroup, lock)),
                    // A sweep found it empty and unlocked, and locked it
                    // first: it's going. Again.
                    Err(errno) => last = cgroup.error("locking the cgroup", errno),
                },
                Err(CgroupError::Exists(path)) => {
                    if !remove_if_stale(&path) {
                        last = CgroupError::RunInUse(path);
                    }
                }
                Err(err) => return Err(err),
            }
            std::thread::sleep(POLL);
        }
        Err(last)
    }

    /// Removes the `run/` cgroups left by shipyard processes that are gone
    /// (killed before they could clean up): those whose lock nobody holds.
    /// Best effort.
    pub fn sweep_stale_runs(&self) {
        let Ok(entries) = std::fs::read_dir(self.base.join(RUN_PARENT)) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            // Names that aren't PIDs aren't shipyard's: left alone.
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.parse::<u32>().is_ok())
            {
                remove_if_stale(&entry.path());
            }
        }
    }
}

/// The base for a process in cgroup `own` running as `uid`, given each
/// cgroup's `owner` and whether a cgroup is `writable` (as `Delegate=` hands
/// it over): for root, the root if it can write it; otherwise the highest
/// cgroup above `own` the user owns. Pure, so every case is tested.
fn choose_base(
    own: &Path,
    uid: u32,
    owner: impl Fn(&Path) -> Option<u32>,
    writable: impl Fn(&Path) -> bool,
) -> Result<PathBuf, NoTree> {
    // Root manages the whole tree, whoever owns its own cgroup: `sudo` from a
    // desktop terminal keeps the terminal's cgroup, which is the user's.
    let root = Path::new("/");
    if uid == 0 && writable(root) {
        return Ok(root.to_path_buf());
    }
    let base =
        delegation_root(own, uid, owner).ok_or_else(|| NoTree::NotDelegated(absolute(own)))?;
    if writable(&base) {
        Ok(base)
    } else {
        Err(NoTree::NotWritable(absolute(&base)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A systemd host: root owns the tree down to the user's slice; the user
    /// manager (`user@1000.service`) and everything in it is the user's.
    fn owner(path: &Path) -> Option<u32> {
        let user = path.starts_with("/user.slice/user-1000.slice/user@1000.service");
        user.then_some(1000).or(Some(0))
    }

    #[test]
    fn root_manages_the_whole_tree() {
        let own = Path::new("/user.slice/user-0.slice/session-2.scope");
        assert_eq!(choose_base(own, 0, owner, |_| true), Ok(PathBuf::from("/")));
        // `sudo` from a desktop terminal: still in the user's own scope.
        let terminal =
            Path::new("/user.slice/user-1000.slice/user@1000.service/app.slice/app-terminal.scope");
        assert_eq!(
            choose_base(terminal, 0, owner, |_| true),
            Ok(PathBuf::from("/"))
        );
    }

    #[test]
    fn a_user_manages_what_systemd_delegated() {
        let own =
            Path::new("/user.slice/user-1000.slice/user@1000.service/app.slice/run-u12.scope");
        assert_eq!(
            choose_base(own, 1000, owner, |_| true),
            Ok(PathBuf::from(
                "/user.slice/user-1000.slice/user@1000.service"
            ))
        );
    }

    #[test]
    fn a_session_scope_is_not_delegated() {
        let own = Path::new("/user.slice/user-1000.slice/session-2.scope");
        let refused = choose_base(own, 1000, owner, |_| true);
        assert_eq!(
            refused,
            Err(NoTree::NotDelegated(PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/session-2.scope"
            )))
        );
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("systemd-run --user --scope"), "{message}");
    }

    #[test]
    fn a_delegated_namespace_root_is_the_base() {
        // Shipyard inside a container whose cgroup namespace root was handed
        // to its user: the root it can see is the user's.
        assert_eq!(
            choose_base(Path::new("/session"), 1000, |_| Some(1000), |_| true),
            Ok(PathBuf::from("/"))
        );
    }

    #[test]
    fn what_cant_be_written_or_read_is_no_base() {
        let own = Path::new("/user.slice/user-0.slice/session-2.scope");
        assert_eq!(
            choose_base(own, 0, owner, |_| false),
            Err(NoTree::NotWritable(PathBuf::from("/sys/fs/cgroup/")))
        );
        // An owner that can't be read is no delegation (root needs none).
        assert!(matches!(
            choose_base(own, 1000, |_| None, |_| true),
            Err(NoTree::NotDelegated(_))
        ));
    }
}
