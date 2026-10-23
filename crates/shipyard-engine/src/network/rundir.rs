//! A run's directory: the `/etc` files bound into its container, and
//! pasta's files.

use shipyard_sys::lock::HeldFile;
use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use super::{NetworkPlan, host_root};
use crate::Paths;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};

/// A run's directory, `<runs dir>/<id>` (`runs_dirs` says where), with
/// the `/etc` files bound into its container (and pasta's files, for a
/// pasta run). Held (an exclusive `flock` on its file `lock`, and on it)
/// while the run lives; removed when it ends, or by the next run if it was
/// killed. A detached container's is given up to its monitor
/// (`give_up`), and removed by its `__cleanup`.
#[derive(Debug)]
pub struct RunDir {
    pub(super) path: PathBuf,
    /// Held while the run lives (see [`sweep_runs`]): its [`RUN_LOCK`],
    /// and the directory itself, which an older Shipyard's sweep probes.
    /// Handed over with the directory when it's given up.
    lock: Option<[HeldFile; 2]>,
    /// Given up to a container's monitor ([`give_up`](Self::give_up)):
    /// its holder removes it, not this value. (Whether its locks are let go
    /// is each [`HeldFile`]'s own, given up with it.)
    given_up: bool,
}

/// The file in a run's directory whose lock says the run lives: the run's
/// own (0600), so no other user can open it, and hold it to keep a dead
/// run's directory from being swept. (The directory is 0755.)
const RUN_LOCK: &str = "lock";

/// The run directories' directory in the runtime's state root, beside its
/// containers' state directories: `.runs`, which no container ID can be
/// (stage 8's `runs` could, `shipyard-runtime create runs`).
const RUNTIME_RUNS: &str = ".runs";

/// The files a run gets, by name: each goes in the container's `/etc`.
pub(crate) const ETC_FILES: [&str; 3] = ["hosts", "hostname", "resolv.conf"];

impl RunDir {
    /// Makes the run's directory with its `/etc` files, as `plan` says, for
    /// a container with `addresses` (on a bridge network), whose resolver is
    /// `resolver` (its network's DNS server, or pasta's) unless `--dns`
    /// names others. Stale directories of runs that are gone are swept
    /// first.
    ///
    /// # Errors
    ///
    /// If no runs directory can be made, or the run's can't be made and
    /// filled.
    pub(crate) fn make(
        paths: &Paths,
        plan: &NetworkPlan,
        addresses: &[std::net::IpAddr],
        resolver: Option<std::net::IpAddr>,
        for_pasta: bool,
    ) -> Result<RunDir> {
        // A file that can't be made here is the run's storage failing.
        Self::fill(paths, plan, addresses, resolver, for_pasta)
            .map_err(|error| error.io_as(ErrorKind::Storage))
    }

    /// [`make`](Self::make)'s work.
    fn fill(
        paths: &Paths,
        plan: &NetworkPlan,
        addresses: &[std::net::IpAddr],
        resolver: Option<std::net::IpAddr>,
        for_pasta: bool,
    ) -> Result<RunDir> {
        use shipyard_network::files;
        let mut failed = None;
        let runs = runs_dirs(paths, for_pasta)
            .into_iter()
            .find(|runs| match make_runs_dir(runs) {
                Ok(()) => true,
                Err(error) => {
                    failed.get_or_insert(error);
                    false
                }
            })
            .ok_or_else(|| {
                failed.unwrap_or_else(|| {
                    Error::new(ErrorKind::Storage, "nowhere to keep the run's files")
                })
            })?;
        // Sweeping, and making a directory then locking it, under one lock:
        // a sweep never finds a new run's directory before its lock.
        let sweeping = lock_runs(&runs)?;
        sweep_runs(&runs);
        let path = runs.join(plan.run.as_str());
        // Ours alone while it's filled (whatever the umask): nobody can put
        // a link in it for its files to be written through.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("making {}", path.display()))?;
        // Removed on drop from here on, even if what follows fails.
        let mut dir = RunDir {
            path,
            lock: None,
            given_up: false,
        };
        dir.lock = Some([
            HeldFile::new(lock_run(&dir.path, true)?),
            HeldFile::new(lock_dir(&dir.path)?),
        ]);
        drop(sweeping);
        let own = plan.in_a_network_namespace();
        // With the host's network, its files as they are (its resolvers work
        // from there); with one of its own, the real upstreams behind a
        // local stub.
        let resolv_source: &[&str] = if own {
            &files::HOST_RESOLV_CONF
        } else {
            &["/etc/resolv.conf"]
        };
        let host_resolv = resolv_source
            .iter()
            .find_map(|path| fs::read_to_string(path).ok())
            .unwrap_or_default();
        let host_hosts = if own {
            None
        } else {
            Some(fs::read_to_string("/etc/hosts").unwrap_or_default())
        };
        let mut dns = plan.dns.clone();
        if let Some(resolver) = resolver
            && dns.servers.is_empty()
        {
            dns.servers = vec![resolver];
        }
        let contents = [
            files::hosts(
                host_hosts.as_deref(),
                &plan.hostname,
                addresses,
                &plan.hosts,
            ),
            files::hostname(&plan.hostname),
            files::resolv_conf(&host_resolv, own, &dns),
        ];
        for (name, text) in ETC_FILES.iter().zip(contents) {
            let file = dir.path.join(name);
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&file)
                .and_then(|mut out| {
                    use std::io::Write as _;
                    out.write_all(text.as_bytes())
                })
                .with_context(|| format!("writing {}", file.display()))?;
            // Readable by the container's root even when it isn't the
            // host's (`--uidmap`): its binds are made with its credentials.
            set_mode(&file, 0o644)?;
        }
        set_mode(&dir.path, 0o755)?;
        Ok(dir)
    }

    /// Where it is.
    #[must_use]
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "detached containers are Linux's")
    )]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Where its file `name` is.
    #[must_use]
    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Its locks (its [`RUN_LOCK`], then the directory), for a container's
    /// monitor to keep the run's directory alive once this process has gone.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "detached containers are Linux's")
    )]
    pub(crate) fn holds(&self) -> impl Iterator<Item = std::os::fd::BorrowedFd<'_>> {
        self.lock.iter().flatten().map(HeldFile::hold)
    }

    /// Gives the directory up to whoever holds copies of its
    /// [`holds`](Self::holds): this process's are closed (never unlocked),
    /// and nothing is removed (the holder does that, told its
    /// [`path`](Self::path)).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "detached containers are Linux's")
    )]
    pub(crate) fn give_up(mut self) {
        self.given_up = true;
        for lock in self.lock.iter_mut().flatten() {
            lock.give_up();
        }
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        // Removed while it's still locked (the locks go after, with the
        // fields), so no sweep can take a directory half gone.
        if !self.given_up {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Where run directories can go, in order of preference: the runtime
/// state's ([`RUNTIME_RUNS`] in `/run/shipyard`, or in
/// `$XDG_RUNTIME_DIR/shipyard` rootless and for root in a user namespace);
/// the image store's (`runs`); a directory of this user's in the temporary
/// one ([`make_runs_dir`] checks it's theirs alone). The first that can be made
/// is used: a rootless caller's `HOME` may be someone else's (`sudo -u`).
/// A pasta run's never goes in the image store: under SELinux, pasta
/// (`pasta_t`) may not reach files under `~/.local` (F32).
fn runs_dirs(paths: &Paths, for_pasta: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Root in a user namespace of its own (`unshare -U -r`) isn't the
    // host's: `/run/shipyard` and `/var/lib/shipyard` aren't its own. Its
    // runtime directory is, if it has one.
    if nix::unistd::geteuid().is_root() && !host_root() {
        if let Some(runtime) = paths.runtime_dir() {
            dirs.push(runtime.join("shipyard").join(RUNTIME_RUNS));
        }
    } else {
        if let Ok(root) = shipyard_runtime::container::default_root() {
            dirs.push(root.join(RUNTIME_RUNS));
        }
        if let Ok(store) = paths.image_store()
            && !for_pasta
        {
            dirs.push(store.join("runs"));
        }
    }
    dirs.push(
        paths
            .temp_dir()
            .join(format!("shipyard-{}", nix::unistd::geteuid()))
            .join("runs"),
    );
    dirs
}

/// Makes `runs` if it's missing. As root, it and its parent (the runtime
/// state's directory) are made passable by anyone (`--x`): a container whose
/// root isn't the host's (`--uidmap`) reaches its `/etc` files through them.
/// Neither can be listed or written by others. Rootless, a container's root
/// is the user, who owns them: they stay private (the image store's, say).
fn make_runs_dir(runs: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let me = nix::unistd::geteuid();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(if me.is_root() { 0o711 } else { 0o700 })
        .create(runs)
        .with_context(|| format!("making {}", runs.display()))?;
    for dir in [runs.parent(), Some(runs)].into_iter().flatten() {
        let metadata =
            fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
        if !metadata.is_dir() || metadata.uid() != me.as_raw() {
            bail!(
                Storage,
                "{} must be a directory of this user's, not a symbolic link",
                dir.display()
            );
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 {
            bail!(Storage, "{}: others can write to it", dir.display());
        }
        if me.is_root() && mode & 0o011 != 0o011 {
            set_mode(dir, mode | 0o011)?;
        }
    }
    Ok(())
}

/// The lock on `runs` itself (its file [`RUNS_LOCK`]), held to sweep it or
/// add to it; each holder is brief, so it's waited for.
fn lock_runs(runs: &Path) -> Result<File> {
    let path = runs.join(RUNS_LOCK);
    let file = crate::fs::open_lock_file(&path)?;
    flock(file, &path, true)
}

/// The file in `runs` [`lock_runs`] locks: not a run's directory.
const RUNS_LOCK: &str = "lock";

/// Removes the run directories whose runs are gone (their lock is free).
/// Called with [`lock_runs`] held.
fn sweep_runs(runs: &Path) {
    let Ok(entries) = fs::read_dir(runs) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        // One without the file was made by an older Shipyard, which locked
        // the directory itself: probed that way, so an upgrade never sweeps
        // a live run's files.
        let lock = if path.join(RUN_LOCK).symlink_metadata().is_ok() {
            lock_run(&path, false)
        } else {
            lock_dir(&path)
        };
        if let Ok(_stale) = lock {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// An exclusive `flock` on the run directory `dir`'s [`RUN_LOCK`] file,
/// without waiting; the file is made (the run's alone) if `make`.
fn lock_run(dir: &Path, make: bool) -> Result<File> {
    let path = dir.join(RUN_LOCK);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(make)
        .create_new(make)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    flock(file, &path, false)
}

/// An exclusive `flock` on the directory `path`, without waiting: how an
/// older Shipyard held its runs' directories, and probes them still.
fn lock_dir(path: &Path) -> Result<File> {
    let dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    flock(dir, path, false)
}

/// `file` (open at `path`) locked exclusively, waiting for it if `wait`.
/// Its holder ([`RunDir`]) unlocks it when it goes, unless it was given up.
fn flock(file: File, path: &Path, wait: bool) -> Result<File> {
    let locked = if wait {
        file.lock()
    } else {
        file.try_lock().map_err(std::io::Error::from)
    };
    locked.map_err(|error| {
        Error::new(
            ErrorKind::Storage,
            format!("locking {}: {}", path.display(), describe(&error)),
        )
    })?;
    Ok(file)
}

/// An I/O error as `flock` would say it (`errno`'s description).
fn describe(error: &std::io::Error) -> String {
    error.raw_os_error().map_or_else(
        || error.to_string(),
        |raw| nix::errno::Errno::from_raw(raw).desc().to_owned(),
    )
}

/// Sets `path`'s permission bits.
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting {}'s mode", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    /// A pasta run's files never go in the image store (under `~/.local`,
    /// out of SELinux-confined pasta's reach: F32); other runs' may.
    #[test]
    fn a_pasta_runs_files_never_go_in_the_image_store() {
        let paths = Paths::from_env().with_image_store("/the/image/store");
        let store = Path::new("/the/image/store/runs").to_path_buf();
        assert!(!runs_dirs(&paths, true).contains(&store));
        if !nix::unistd::geteuid().is_root() || host_root() {
            assert!(runs_dirs(&paths, false).contains(&store));
        }
    }

    /// In the runtime's state root, run directories go where no container
    /// can be: `.runs` is no container ID (`runs` was, and collided with
    /// `shipyard-runtime create runs`).
    #[test]
    fn runs_in_the_runtime_root_are_no_container() {
        assert!(shipyard_runtime::container::validate_id("runs").is_ok());
        assert!(shipyard_runtime::container::validate_id(RUNTIME_RUNS).is_err());
        if let Ok(root) = shipyard_runtime::container::default_root()
            && (!nix::unistd::geteuid().is_root() || host_root())
        {
            let dirs = runs_dirs(&Paths::from_env(), false);
            assert_eq!(dirs.first(), Some(&root.join(RUNTIME_RUNS)));
        }
    }

    /// A run directory handed over (copies of its holds, as a container's
    /// monitor keeps) isn't removed, and isn't swept while the copies are
    /// open; once they're closed, the sweep takes it.
    #[test]
    fn a_run_dir_handed_over_outlives_its_holder() {
        let runs = std::env::temp_dir().join(format!("shipyard-runs-held-{}", std::process::id()));
        let _ = fs::remove_dir_all(&runs);
        let path = runs.join("handed");
        fs::create_dir_all(&path).unwrap();
        let dir = RunDir {
            lock: Some([
                HeldFile::new(lock_run(&path, true).unwrap()),
                HeldFile::new(lock_dir(&path).unwrap()),
            ]),
            path: path.clone(),
            given_up: false,
        };
        let copies: Vec<_> = dir
            .holds()
            .map(|fd| fd.try_clone_to_owned().unwrap())
            .collect();
        assert_eq!(copies.len(), 2);
        dir.give_up();
        assert!(path.is_dir(), "not removed: its holder does that");
        sweep_runs(&runs);
        assert!(path.is_dir(), "not swept while the copies are open");
        drop(copies);
        sweep_runs(&runs);
        assert!(!path.exists(), "swept once they're closed");
        fs::remove_dir_all(&runs).unwrap();
    }

    /// A run's lock is a file only it can open: a lock on its directory
    /// (anyone's) no longer keeps a dead run's files from the sweep. An older
    /// Shipyard's directory, without the file, is still probed by its own
    /// lock.
    #[test]
    fn the_sweep_goes_by_each_runs_own_lock_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let runs = std::env::temp_dir().join(format!("shipyard-runs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&runs);
        let dir = |name: &str| {
            let path = runs.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        };
        let live = dir("live");
        let _live = lock_run(&live, true).unwrap();
        let mode = fs::metadata(live.join(RUN_LOCK))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        let dead = dir("dead");
        drop(lock_run(&dead, true).unwrap());
        let _squatter = lock_dir(&dead).unwrap();
        let old_live = dir("old-live");
        let _old_live = lock_dir(&old_live).unwrap();
        let old_dead = dir("old-dead");
        let _sweeping = lock_runs(&runs).unwrap();

        sweep_runs(&runs);
        assert!(live.exists());
        assert!(!dead.exists());
        assert!(old_live.exists());
        assert!(!old_dead.exists());
        assert!(
            runs.join(RUNS_LOCK).exists(),
            "the runs' own lock is no run"
        );
        fs::remove_dir_all(&runs).unwrap();
    }
}
