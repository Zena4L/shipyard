//! A container's cgroup: filled, watched and removed (Linux; the probe's
//! G2, G3 and G9). Where it's made is [`Tree`](super::Tree)'s.
//!
//! Shipyard keeps its containers below its part of the tree (the root, or a
//! user's delegated subtree), under `shipyard/`: `run/<pid>` for `shipyard
//! run` (named after shipyard's own PID) and `runtime/<id>` for
//! `shipyard-runtime`. Two parents, so a container ID can never be mistaken
//! for a run.
//!
//! A run holds an `flock` on its cgroup's directory for as long as it lives
//! ([`RunCgroup`]): that's how a later run tells a leftover from a live one.
//! Not by the PID in the name: another PID namespace sharing this tree may
//! have a live shipyard with a PID this one's `/proc` doesn't show, and a
//! PID is reused once its process is gone.

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::Pid;

use super::{Controller, ROOT, RUN_PARENT, RUNTIME_PARENT, parse_controllers, parse_keyed};
use crate::resources::Resources;

/// Shipyard's own part of the tree, below the base.
pub(super) const SHIPYARD: &str = "shipyard";

/// How often `remove` looks whether the processes are gone, `freeze`
/// whether they're frozen, and how long a run waits between tries at
/// claiming its cgroup.
pub(super) const POLL: Duration = Duration::from_millis(10);

/// How many passes [`Cgroup::signal_all`] makes at most, without
/// `cgroup.kill`: each signals what the last one hadn't seen.
const SIGNAL_PASSES: u32 = 5;

/// How many times a run tries to claim its cgroup: a sweep (by another run)
/// may be removing a leftover of the same name meanwhile.
pub(super) const CLAIM_TRIES: u32 = 10;

/// Why a cgroup couldn't be made, filled or removed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CgroupError {
    /// A file or directory of the tree.
    #[error("{doing} {}: {errno}", path.display())]
    Io {
        /// What was being done.
        doing: &'static str,
        /// The cgroup file or directory.
        path: PathBuf,
        /// The kernel's answer.
        errno: Errno,
    },
    /// The cgroup is already there: another container has the name.
    #[error("the cgroup {} exists already: another container's (from another --root?), or not a container's at all", .0.display())]
    Exists(PathBuf),
    /// Limits applied in part: the files written before the refusal stay.
    #[error("{refused} (already changed, and kept: {})", written.join(", "))]
    PartlyApplied {
        /// The files written.
        written: Vec<String>,
        /// Why the next one wasn't.
        refused: Box<CgroupError>,
    },
    /// A cgroup's place is inside one that holds processes: another
    /// container's.
    #[error(
        "the cgroup can't be made inside {}: it holds processes (another container's cgroup?)",
        .0.display()
    )]
    InsideAnother(PathBuf),
    /// `shipyard run`'s cgroup for this PID is held by a live run: a shipyard
    /// with the same PID in another PID namespace, sharing this cgroup tree.
    #[error(
        "the cgroup {} is held by another shipyard run with this PID (in another PID namespace \
         sharing the cgroup tree)",
        .0.display()
    )]
    RunInUse(PathBuf),
    /// A controller a limit needs can't be handed down to the cgroup.
    #[error("the {controller} controller can't be handed down to {}: {reason}", dir.display())]
    Controller {
        /// The controller.
        controller: Controller,
        /// The cgroup that would have handed it down.
        dir: PathBuf,
        /// Why not.
        reason: String,
    },
    /// A limit the kernel refused.
    #[error("setting {file} to {value:?} in {}: {errno}", path.display())]
    Limit {
        /// The cgroup.
        path: PathBuf,
        /// The file (`memory.max`).
        file: String,
        /// What was written.
        value: String,
        /// The kernel's answer.
        errno: Errno,
    },
    /// A limit whose file this kernel doesn't have (`io.weight` comes only
    /// from the iocost controller, say).
    #[error(
        "the container's cgroup has no {file}: this kernel doesn't have it, or its controller \
         isn't one Shipyard hands down; that limit can't be set here"
    )]
    NoSuchLimit {
        /// The file.
        file: String,
    },
    /// An I/O limit on something that isn't a whole disk.
    #[error(
        "the I/O limit {line:?} can't be set: I/O limits take a whole disk (such as /dev/sda), \
         not a partition (/dev/sda1)"
    )]
    NotADisk {
        /// The `io.max` line.
        line: String,
    },
    /// A swap limit asked for, on a kernel that doesn't account swap.
    #[error(
        "a swap limit can't be set here: the kernel doesn't account swap (it needs \
         swapaccount=1, or a swap-enabled memory controller)"
    )]
    NoSwapAccounting,
    /// A recorded path that isn't one of Shipyard's container cgroups:
    /// never acted on (killing "everything in it" could be the whole host).
    #[error("{} isn't one of Shipyard's container cgroups; left alone", .0.display())]
    NotShipyards(PathBuf),
    /// The kernel didn't say it froze (or thawed) in time.
    #[error(
        "the cgroup {} didn't {} within {}s",
        path.display(),
        if *frozen { "freeze" } else { "thaw" },
        waited.as_secs()
    )]
    NotFrozen {
        /// The cgroup.
        path: PathBuf,
        /// Whether it was to freeze.
        frozen: bool,
        /// How long it was given.
        waited: Duration,
    },
    /// Device rules that can't make a program (checked before, by
    /// `Isolation::validate`: only a library caller skipping that gets it).
    #[error(transparent)]
    InvalidDevices(#[from] crate::devices::DeviceError),
    /// The device program couldn't be loaded or attached.
    #[error(
        "{doing} for {}: {}{}",
        path.display(),
        errno.desc(),
        if log.is_empty() { String::new() } else { format!(" ({log})") }
    )]
    Devices {
        /// What was being done.
        doing: &'static str,
        /// The cgroup.
        path: PathBuf,
        /// The kernel's answer (EPERM: no privilege to).
        errno: Errno,
        /// What the verifier said, if it refused the program.
        log: String,
    },
    /// Processes still in it after the deadline: stuck in the kernel.
    #[error("the cgroup {} still holds processes after {}s; it's left in place", path.display(), waited.as_secs())]
    StillPopulated {
        /// The cgroup.
        path: PathBuf,
        /// How long it was given.
        waited: Duration,
    },
}

/// `shipyard run`'s cgroup, for the shipyard process `pid`, relative to the
/// base.
#[must_use]
pub fn run_cgroup(pid: u32) -> PathBuf {
    Path::new(RUN_PARENT).join(pid.to_string())
}

/// A `shipyard-runtime` container's cgroup, relative to the base. `id` is
/// a valid container ID (a letter or digit, then letters, digits, `_ . + -`),
/// so one path component.
#[must_use]
pub fn runtime_cgroup(id: &str) -> PathBuf {
    Path::new(RUNTIME_PARENT).join(id)
}

/// Removes the run cgroup at `path` if no live run holds its lock, and
/// nothing is in it (`rmdir` refuses then). Whether it went, or was gone.
pub(super) fn remove_if_stale(path: &Path) -> bool {
    let _lock = match lock_dir(path) {
        Ok(lock) => lock,
        Err(errno) => return errno == Errno::ENOENT,
    };
    // Locked, and still the directory at the path: nobody else removes it
    // while the lock is held.
    std::fs::remove_dir(path).is_ok()
}

/// `shipyard run`'s cgroup, held: its lock tells later runs it's alive.
/// The lock goes with this value (or with the process, if it's killed);
/// [`Cgroup::remove`] it first.
#[derive(Debug)]
pub struct RunCgroup {
    cgroup: Cgroup,
    _lock: Flock<OwnedFd>,
}

impl RunCgroup {
    /// `cgroup`, held by `lock`.
    pub(super) fn new(cgroup: Cgroup, lock: Flock<OwnedFd>) -> Self {
        Self {
            cgroup,
            _lock: lock,
        }
    }

    /// The cgroup.
    #[must_use]
    pub fn cgroup(&self) -> &Cgroup {
        &self.cgroup
    }
}

/// A cgroup of the tree, by its path, and the base of the part of the tree
/// it's in (nothing above that is ever written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgroup {
    path: PathBuf,
    base: PathBuf,
}

impl Cgroup {
    /// The cgroup at `path`, below `base`.
    pub(super) fn at(path: PathBuf, base: PathBuf) -> Self {
        Self { path, base }
    }

    /// The cgroup at `path`, made earlier below `base` (both absolute, as
    /// recorded): only a container's own. Whatever else a record might say
    /// (the root, `..`, shipyard's parents, the cgroup this process runs in
    /// or one above it, a base this user doesn't own), killing all in it
    /// could take the host, or this very process, along.
    ///
    /// The record's base, not this process's own [`Tree`](super::Tree): a
    /// rootless container made from a desktop terminal is deleted from an SSH
    /// login too, which has no delegated tree of its own; the kernel lets the
    /// user act on cgroups it made from anywhere.
    ///
    /// # Errors
    ///
    /// `NotShipyards` for any other path or base.
    pub fn recorded(path: PathBuf, base: PathBuf) -> Result<Self, CgroupError> {
        use std::os::unix::fs::MetadataExt as _;
        let euid = nix::unistd::geteuid();
        let plain_base = base.starts_with(ROOT)
            && base.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            });
        let users_base = euid.is_root()
            || std::fs::metadata(&base).is_ok_and(|meta| meta.uid() == euid.as_raw());
        let own = super::own_cgroup().map(|own| absolute(&own));
        if plain_base && users_base && is_container_cgroup(&path, &base, own.as_deref()) {
            Ok(Self { path, base })
        } else {
            Err(CgroupError::NotShipyards(path))
        }
    }

    /// Its directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The base of the part of the tree it's in (see [`Tree`](super::Tree)).
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Its directory, open, for `clone3`'s `CLONE_INTO_CGROUP`. (Any
    /// descriptor of it would do, `O_PATH` too: the kernel checks that the
    /// caller may write its `cgroup.procs`, not how it was opened.)
    pub(crate) fn open_dir(&self) -> Result<OwnedFd, CgroupError> {
        open_dir(&self.path).map_err(|errno| self.error("opening the cgroup", errno))
    }

    /// Puts `resources` in place: the controllers they need handed down to
    /// it ([`Cgroup::enable`]), then the files written ([`Cgroup::apply`]).
    /// Returns what was skipped, to warn about.
    ///
    /// # Errors
    ///
    /// As those two.
    pub fn limit(&self, resources: &Resources) -> Result<Vec<String>, CgroupError> {
        self.enable(&resources.controllers())?;
        self.apply(resources)
    }

    /// Makes sure the controllers `wanted` reach it: each ancestor, from the
    /// base down, hands down what's missing (one write each: G2), as runc's
    /// cgroupfs driver does. What's handed down already is no write at all.
    ///
    /// # Errors
    ///
    /// A controller the kernel doesn't have here, a cgroup on the way that
    /// holds processes, or a write the kernel refused.
    pub fn enable(&self, wanted: &BTreeSet<Controller>) -> Result<(), CgroupError> {
        for dir in handing_down(&self.path, &self.base) {
            let read = |file| std::fs::read_to_string(dir.join(file)).unwrap_or_default();
            let handed = parse_controllers(&read("cgroup.subtree_control"));
            let missing: Vec<Controller> = wanted
                .iter()
                .copied()
                .filter(|controller| !handed.contains(controller))
                .collect();
            let Some(&first) = missing.first() else {
                continue;
            };
            let refused = |controller, reason: String| CgroupError::Controller {
                controller,
                dir: dir.to_path_buf(),
                reason,
            };
            let available = parse_controllers(&read("cgroup.controllers"));
            if let Some(&absent) = missing.iter().find(|c| !available.contains(c)) {
                let reason = if dir == self.base && dir != Path::new(ROOT) {
                    "it wasn't delegated to this user (on systemd, `systemctl edit \
                     user@.service` can delegate more: Delegate=cpu cpuset io memory pids)"
                } else {
                    "it doesn't have it (cgroup.controllers)"
                };
                return Err(refused(absent, reason.into()));
            }
            if holds_processes(dir) {
                return Err(refused(
                    first,
                    "it holds processes, and handing a controller down from there would stop \
                     the kernel placing any below it (cgroup v2's no-internal-processes rule); \
                     move them into a child cgroup first, as docker-in-docker setups do"
                        .into(),
                ));
            }
            let line: Vec<String> = missing.iter().map(|c| format!("+{c}")).collect();
            write_file(&dir.join("cgroup.subtree_control"), &line.join(" ")).map_err(|err| {
                refused(
                    first,
                    format!("writing cgroup.subtree_control: {}", io_errno(&err).desc()),
                )
            })?;
        }
        Ok(())
    }

    /// Writes `resources` into it ([`Resources::files`], in order). Returns
    /// what was skipped, to warn about: Docker's default swap limit where the
    /// kernel doesn't account swap.
    ///
    /// # Errors
    ///
    /// A value the kernel refused, naming the file; a file this kernel
    /// doesn't have; a swap limit asked for where there's no swap accounting.
    pub fn apply(&self, resources: &Resources) -> Result<Vec<String>, CgroupError> {
        let mut skipped = Vec::new();
        let mut written: Vec<String> = Vec::new();
        for (file, value) in resources.files() {
            let Err(err) = write_file(&self.path.join(&file), &value) else {
                written.push(file);
                continue;
            };
            let refused = match (file.as_str(), io_errno(&err)) {
                ("memory.swap.max", Errno::ENOENT) if resources.swap_is_default => {
                    skipped.push(
                        "no swap limit: the kernel doesn't account swap here (swapaccount=1)"
                            .to_owned(),
                    );
                    continue;
                }
                ("memory.swap.max", Errno::ENOENT) => CgroupError::NoSwapAccounting,
                (_, Errno::ENOENT) => CgroupError::NoSuchLimit { file },
                ("io.max", Errno::ENODEV) => CgroupError::NotADisk { line: value },
                (_, errno) => CgroupError::Limit {
                    path: self.path.clone(),
                    file,
                    value,
                    errno,
                },
            };
            // Nothing is undone: say what changed already (an `update`'s
            // container keeps running with it).
            return Err(if written.is_empty() {
                refused
            } else {
                CgroupError::PartlyApplied {
                    written,
                    refused: Box::new(refused),
                }
            });
        }
        Ok(skipped)
    }

    /// How many processes in it the OOM killer has killed
    /// (`memory.events`' `oom_kill`); 0 without a memory controller.
    #[must_use]
    pub fn oom_kills(&self) -> u64 {
        std::fs::read_to_string(self.path.join("memory.events"))
            .ok()
            .and_then(|text| parse_keyed(&text).get("oom_kill").copied())
            .unwrap_or(0)
    }

    /// What it has used (see [`Stats`](super::Stats)).
    #[must_use]
    pub fn stats(&self) -> super::Stats {
        super::Stats::read(|file| std::fs::read_to_string(self.path.join(file)).ok())
    }

    /// Every process in it and below.
    #[must_use]
    pub fn pids(&self) -> Vec<Pid> {
        walk(&self.path)
            .iter()
            .filter_map(|dir| std::fs::read_to_string(dir.join("cgroup.procs")).ok())
            .flat_map(|procs| {
                procs
                    .lines()
                    .filter_map(|line| line.parse().ok())
                    .map(Pid::from_raw)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Sends `signal` (any number) to every process in it and below. SIGKILL goes
    /// through `cgroup.kill` where the kernel has it (5.14): atomic, so a
    /// process forking meanwhile can't slip past. Otherwise each process,
    /// again until a pass finds none it hadn't signalled (a fork mid-pass),
    /// a few passes at most.
    pub fn signal_all(&self, signal: libc::c_int) {
        if signal == libc::SIGKILL && write_file(&self.path.join("cgroup.kill"), "1").is_ok() {
            return;
        }
        let mut sent: Vec<Pid> = Vec::new();
        for _ in 0..SIGNAL_PASSES {
            let new: Vec<Pid> = self
                .pids()
                .into_iter()
                .filter(|pid| !sent.contains(pid))
                .collect();
            if new.is_empty() {
                return;
            }
            // Only what was signalled (or is gone) counts as sent: one that
            // couldn't be pinned this pass (out of descriptors) is tried again.
            sent.extend(
                new.into_iter()
                    .filter(|&pid| self.signal_member(pid, signal)),
            );
        }
    }

    /// Sends `signal` to process `pid` if it's still one of its own. Listed
    /// a moment ago, it may have exited since, and its PID gone to another
    /// process anywhere on the host: the process is pinned first (a pidfd),
    /// then checked, and signalled through the pin, so a PID reused after
    /// the check isn't hit either. Before Linux 5.3 (no `pidfd_open`), the
    /// check and a plain `kill`. Best effort: a process gone is skipped.
    /// Says whether it's done with: signalled, or no longer one of its own;
    /// `false` if it couldn't be pinned.
    fn signal_member(&self, pid: Pid, signal: libc::c_int) -> bool {
        // SAFETY: `pidfd_open` takes a PID and flags, and returns a new
        // descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), 0) };
        let pidfd = libc::c_int::try_from(raw)
            .ok()
            .filter(|&fd| fd >= 0)
            // SAFETY: `pidfd_open` just returned it; nothing else owns it.
            .map(|fd| unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
        if pidfd.is_none() {
            match Errno::last() {
                Errno::ENOSYS => {}
                // Gone already.
                Errno::ESRCH => return true,
                _ => return false,
            }
        }
        if !self.holds(pid) {
            return true;
        }
        match pidfd {
            // SAFETY: a valid pidfd, a signal number (any, a real-time one
            // too), no siginfo, no flags.
            Some(pidfd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    std::os::fd::AsRawFd::as_raw_fd(&pidfd),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            },
            // SAFETY: `kill` takes any PID and signal number.
            None => unsafe {
                libc::kill(pid.as_raw(), signal);
            },
        }
        true
    }

    /// Whether process `pid` is in it or below (`/proc/PID/cgroup`).
    fn holds(&self, pid: Pid) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .ok()
            .and_then(|text| super::parse_own_cgroup(&text))
            .is_some_and(|cgroup| absolute(&cgroup).starts_with(&self.path))
    }

    /// Freezes it (`true`) or thaws it, and waits up to `deadline` until the
    /// kernel says so (`cgroup.events`' `frozen`): a process in a system call
    /// stops only when it returns (G8).
    ///
    /// # Errors
    ///
    /// `cgroup.freeze` can't be written (before Linux 5.2, it doesn't
    /// exist), or the deadline passed.
    pub fn freeze(&self, frozen: bool, deadline: Duration) -> Result<(), CgroupError> {
        let file = self.path.join("cgroup.freeze");
        let doing = if frozen { "freezing" } else { "thawing" };
        write_file(&file, if frozen { "1" } else { "0" })
            .map_err(|err| self.error(doing, io_errno(&err)))?;
        let start = Instant::now();
        while self.frozen() != frozen {
            if start.elapsed() >= deadline {
                // A freeze that didn't finish (a process stuck in the
                // kernel) is undone: half frozen, the container would be
                // neither running nor paused, and `resume` couldn't reach it.
                if frozen {
                    let _ = write_file(&file, "0");
                }
                return Err(CgroupError::NotFrozen {
                    path: self.path.clone(),
                    frozen,
                    waited: deadline,
                });
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    }

    /// Whether it's frozen (`cgroup.events`' `frozen`).
    #[must_use]
    pub fn frozen(&self) -> bool {
        std::fs::read_to_string(self.path.join("cgroup.events"))
            .is_ok_and(|text| parse_keyed(&text).get("frozen").copied() == Some(1))
    }

    /// Moves process `pid` into it (its `cgroup.procs`).
    pub(crate) fn add(&self, pid: Pid) -> Result<(), CgroupError> {
        write_file(&self.path.join("cgroup.procs"), &pid.as_raw().to_string())
            .map_err(|err| self.error("moving the container into its cgroup", io_errno(&err)))
    }

    /// Kills whatever is left in it and removes it, with any cgroups made
    /// inside: `cgroup.kill` (Linux 5.14; before that, SIGKILL to each
    /// process listed), then waits up to `deadline` for it to empty (a
    /// killed process stuck in the kernel goes only when its I/O does), then
    /// `rmdir`, deepest first. A cgroup that's gone already is fine.
    ///
    /// # Errors
    ///
    /// Processes still there at the deadline, or a directory that can't be
    /// removed.
    pub fn remove(&self, deadline: Duration) -> Result<(), CgroupError> {
        if !self.path.exists() {
            return Ok(());
        }
        let start = Instant::now();
        // Again each time: without `cgroup.kill`, a process forking while
        // the PIDs are killed one by one leaves a child to kill.
        while {
            self.kill_all();
            self.populated()
        } {
            if start.elapsed() >= deadline {
                return Err(CgroupError::StillPopulated {
                    path: self.path.clone(),
                    waited: deadline,
                });
            }
            std::thread::sleep(POLL);
        }
        remove_tree(&self.path)
    }

    /// SIGKILL to every process in it and below.
    fn kill_all(&self) {
        self.signal_all(libc::SIGKILL);
    }

    /// Whether any process is in it or below (`cgroup.events`).
    fn populated(&self) -> bool {
        std::fs::read_to_string(self.path.join("cgroup.events"))
            .is_ok_and(|text| parse_keyed(&text).get("populated").copied() != Some(0))
    }

    pub(super) fn error(&self, doing: &'static str, errno: Errno) -> CgroupError {
        CgroupError::Io {
            doing,
            path: self.path.clone(),
            errno,
        }
    }
}

/// Hands every controller `dir` has down to its children, in one write: one
/// at a time, an accepted threaded controller would turn a domain one's
/// refusal into a different error (the probe's G2). Never from a cgroup
/// that holds processes ([`holds_processes`]). Best effort: without them,
/// the cgroups below still freeze, list and kill.
pub(super) fn hand_down_controllers(dir: &Path) {
    if holds_processes(dir) {
        return;
    }
    let Ok(available) = std::fs::read_to_string(dir.join("cgroup.controllers")) else {
        return;
    };
    let handed = std::fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap_or_default();
    let missing: Vec<String> = available
        .split_whitespace()
        .filter(|name| !handed.split_whitespace().any(|done| done == *name))
        .map(|name| format!("+{name}"))
        .collect();
    if missing.is_empty() {
        return;
    }
    if let Err(err) = write_file(&dir.join("cgroup.subtree_control"), &missing.join(" ")) {
        tracing::debug!(dir = %dir.display(), %err, "couldn't hand controllers down");
    }
}

/// Whether the cgroup `dir` holds processes and must therefore never hand
/// a controller down (the probe's G2, and the step 3 lesson): even a
/// threaded one the kernel accepts there (`pids`, `cpuset`) makes every move
/// into a cgroup below it fail with EOPNOTSUPP from then on. The hierarchy's
/// real root is exempt from that rule; it's the one cgroup without a
/// `cgroup.type` file. A cgroup namespace's root isn't exempt.
pub(super) fn holds_processes(dir: &Path) -> bool {
    dir.join("cgroup.type").exists()
        && std::fs::read_to_string(dir.join("cgroup.procs"))
            .is_ok_and(|procs| !procs.trim().is_empty())
}

/// `dir` and every cgroup below it, parents first.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = vec![dir.to_path_buf()];
    let mut next = 0;
    while let Some(current) = found.get(next).cloned() {
        next += 1;
        if let Ok(entries) = std::fs::read_dir(&current) {
            found.extend(
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path()),
            );
        }
    }
    found
}

/// Removes `dir` and the cgroups below it, deepest first (a cgroup's own
/// files go with its directory).
fn remove_tree(dir: &Path) -> Result<(), CgroupError> {
    for path in walk(dir).iter().rev() {
        match std::fs::remove_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(CgroupError::Io {
                    doing: "removing the cgroup",
                    path: path.clone(),
                    errno: io_errno(&err),
                });
            }
        }
    }
    Ok(())
}

/// The cgroups that hand controllers down to the one at `path`: its
/// ancestors, from `base` itself down to its parent.
fn handing_down<'a>(path: &'a Path, base: &Path) -> Vec<&'a Path> {
    let mut ancestors: Vec<&Path> = path
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(base))
        .collect();
    ancestors.reverse();
    ancestors
}

/// Whether `path` can be a container's cgroup: strictly below `base` (a
/// [`Tree`](super::Tree)'s, so itself below the root), by plain names (no
/// `.`, `..`), not one of shipyard's parents, and not `own` (this process's
/// cgroup, absolute) or above it.
pub(super) fn is_container_cgroup(path: &Path, base: &Path, own: Option<&Path>) -> bool {
    let Ok(below) = path.strip_prefix(base) else {
        return false;
    };
    let plain = below
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)));
    let parents = [
        Path::new(SHIPYARD).to_path_buf(),
        Path::new(RUN_PARENT).to_path_buf(),
        Path::new(RUNTIME_PARENT).to_path_buf(),
    ];
    plain
        && !below.as_os_str().is_empty()
        && !parents.iter().any(|parent| below == parent)
        && !own.is_some_and(|own| own.starts_with(path))
}

/// A cgroup as `/proc/self/cgroup` names it (from the root it can see),
/// below [`ROOT`]: `/` is the root itself.
pub(super) fn absolute(cgroup: &Path) -> PathBuf {
    let relative = cgroup.strip_prefix("/").unwrap_or(cgroup);
    if relative.as_os_str().is_empty() {
        PathBuf::from(ROOT)
    } else {
        Path::new(ROOT).join(relative)
    }
}

/// Writes `value` to the cgroup file at `path`, in one write. Opened for
/// writing only, never created: the kernel makes a cgroup's files, and with
/// `O_CREAT` (as `std::fs::write` opens) a missing one fails with EACCES,
/// which would hide that it's missing (ENOENT).
fn write_file(path: &Path, value: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value.as_bytes())
}

/// A cgroup's directory, open for reading (what `CLONE_INTO_CGROUP` and
/// `flock` take).
fn open_dir(path: &Path) -> nix::Result<OwnedFd> {
    nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

/// The cgroup directory at `path`, locked (`flock`, without waiting), and
/// still the directory at `path` once locked.
///
/// The lock alone doesn't say that: a sweep may remove the directory between
/// this `open` and this `flock` (and let go of its own lock), and a lock on
/// a removed directory still succeeds. Then the path is gone (ENOENT) or is
/// another directory, made since (ESTALE).
pub(super) fn lock_dir(path: &Path) -> nix::Result<Flock<OwnedFd>> {
    let lock = Flock::lock(open_dir(path)?, FlockArg::LockExclusiveNonblock)
        .map_err(|(_, errno)| errno)?;
    let locked = nix::sys::stat::fstat(&*lock)?;
    let there = nix::sys::stat::stat(path)?;
    if (locked.st_ino, locked.st_dev) == (there.st_ino, there.st_dev) {
        Ok(lock)
    } else {
        Err(Errno::ESTALE)
    }
}

fn io_errno(err: &std::io::Error) -> Errno {
    err.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_runtime_containers_have_their_own_parents() {
        assert_eq!(run_cgroup(4321), Path::new("shipyard/run/4321"));
        assert_eq!(runtime_cgroup("web-1"), Path::new("shipyard/runtime/web-1"));
        // An ID that looks like a PID still can't be a run's.
        assert_ne!(runtime_cgroup("4321"), run_cgroup(4321));
    }

    /// A user's delegated subtree, as systemd hands it over.
    const USER: &str = "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service";

    #[test]
    fn controllers_are_handed_down_from_the_base() {
        let root = Path::new(ROOT);
        assert_eq!(
            handing_down(Path::new("/sys/fs/cgroup/shipyard/run/42"), root),
            [
                Path::new("/sys/fs/cgroup"),
                Path::new("/sys/fs/cgroup/shipyard"),
                Path::new("/sys/fs/cgroup/shipyard/run"),
            ]
        );
        assert_eq!(
            handing_down(Path::new("/sys/fs/cgroup/default"), root),
            [Path::new("/sys/fs/cgroup")]
        );
        // A user's: never above what was delegated (root's, unwritable).
        let run = Path::new(USER).join("shipyard/run/42");
        assert_eq!(
            handing_down(&run, Path::new(USER)),
            [
                Path::new(USER).to_path_buf(),
                Path::new(USER).join("shipyard"),
                Path::new(USER).join("shipyard/run"),
            ]
        );
    }

    #[test]
    fn only_a_containers_own_cgroup_is_acted_on() {
        let root = Path::new(ROOT);
        let own = Path::new("/sys/fs/cgroup/user.slice/user-0.slice/session-1.scope");
        for ok in [
            "/sys/fs/cgroup/shipyard/run/42",
            "/sys/fs/cgroup/shipyard/runtime/web-1",
            // A bundle's linux.cgroupsPath, absolute or relative.
            "/sys/fs/cgroup/default/web-1",
            "/sys/fs/cgroup/shipyard/runtime/pods/web-1",
        ] {
            assert!(is_container_cgroup(Path::new(ok), root, Some(own)), "{ok}");
        }
        for bad in [
            "/sys/fs/cgroup",
            "/sys/fs/cgroup/",
            "/sys/fs/cgroup/shipyard",
            "/sys/fs/cgroup/shipyard/run",
            "/sys/fs/cgroup/shipyard/runtime",
            "/sys/fs/cgroup/shipyard/runtime/..",
            "/sys/fs/cgroup/shipyard/runtime/../..",
            "/sys/fs/cgroup2/web",
            "shipyard/runtime/web",
            // This process's own cgroup, and those above it.
            "/sys/fs/cgroup/user.slice",
            "/sys/fs/cgroup/user.slice/user-0.slice/session-1.scope",
        ] {
            assert!(
                !is_container_cgroup(Path::new(bad), root, Some(own)),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_users_containers_are_below_what_was_delegated() {
        let base = Path::new(USER);
        let own = base.join("app.slice/run-u12.scope");
        for ok in ["shipyard/run/42", "shipyard/runtime/web-1", "pods/web-1"] {
            assert!(
                is_container_cgroup(&base.join(ok), base, Some(&own)),
                "{ok}"
            );
        }
        for bad in [
            // The delegated root itself, and shipyard's parents in it.
            USER.to_owned(),
            format!("{USER}/shipyard"),
            format!("{USER}/shipyard/run"),
            format!("{USER}/shipyard/runtime"),
            // Above or beside it: root's, or another user's.
            "/sys/fs/cgroup/user.slice/user-1000.slice".to_owned(),
            "/sys/fs/cgroup/shipyard/runtime/web-1".to_owned(),
            "/sys/fs/cgroup/user.slice/user-1001.slice/user@1001.service/x".to_owned(),
            // Its own cgroup, and the slice it's in.
            own.display().to_string(),
            format!("{USER}/app.slice"),
        ] {
            assert!(
                !is_container_cgroup(Path::new(&bad), base, Some(&own)),
                "{bad}"
            );
        }
    }
}
