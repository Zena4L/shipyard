//! The lifecycle on Linux (see the module above): the state directory, the
//! monitor, and the calls.

use std::collections::BTreeMap;
use std::fs::DirBuilder;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, OFlag};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};

use super::plan::PLAN_FILE;
use super::{
    ContainerError, CreateOptions, Created, ExitRecord, LogOptions, LogsOf, OCI_VERSION, Plan,
    Result, State, Status, StdinMode, validate_id, versions,
};
use crate::cgroup;
use crate::hooks::{ContainerHooks, Phase};
use crate::isolation::Namespace;
use crate::logs::info::LogSettings;
use crate::oci::load_bundle_for_caller;
use crate::process::Signal;

/// The monitor's hidden first argument (see [`monitor_entry_point`]).
const MONITOR_ARG: &str = "__shipyard-monitor";

const STATE_FILE: &str = "state.json";
const FIFO: &str = "exec.fifo";
/// Held open for writing by the container's init until its command has
/// `execve`d (see `Gate::started`): made only for `poststart` hooks, which
/// `start` runs after that.
const STARTED: &str = "exec.started";
/// How the container ended ([`ExitRecord`]): written by the monitor, last.
const EXIT_FILE: &str = "exit";
/// Left by a call that sends SIGKILL (`kill`, `kill --all`, `delete`): the
/// container's end is theirs, not the OOM killer's, even if it struck
/// earlier (a process it killed, and the container went on).
const KILLED_FILE: &str = "killed";
/// Made, empty, by the monitor when its exit command couldn't be started
/// (after a few tries: the process limit, memory): whatever it was to do (a
/// policy's restart, a clean-up) wasn't done. Its caller's recovery looks
/// for it, by this name.
pub(crate) const EXIT_COMMAND_FAILED: &str = "exit-command-failed";
/// Made by `start` once its init has passed the start gate (9.4a): the
/// container's command ran. What its monitor records as `started` in the
/// exit record, so a container ended before it started is never taken for
/// one that ran (its exec FIFO gone says less: anything can remove it).
const GATE_OPENED: &str = "gate-opened";

/// How long `start` waits for the init's answer at the gate (and, for
/// `poststart` hooks, for the program to start, beyond its `startContainer`
/// hooks' timeouts), and `delete` for a killed container to stop.
const WAIT: Duration = Duration::from_secs(5);

/// How often `delete` looks whether a killed container has stopped, and a
/// call whether the lock is free.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a call waits for the lock: longer than any call holds it
/// (`delete` waits up to [`WAIT`] for the stop, then up to [`WAIT`] for the
/// cgroup to empty, with it).
const LOCK_WAIT: Duration = Duration::from_secs(15);

fn io(doing: impl Into<String>) -> impl FnOnce(std::io::Error) -> ContainerError {
    let doing = doing.into();
    move |error| ContainerError::Io { doing, error }
}

fn errno_io(doing: impl Into<String>) -> impl FnOnce(Errno) -> ContainerError {
    let doing = doing.into();
    move |errno| ContainerError::Io {
        doing,
        error: std::io::Error::from(errno),
    }
}

/// What `state.json` holds.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    bundle: PathBuf,
    /// Created from a plan (`create_planned`), kept beside this record as
    /// `plan.json`, not from `bundle`'s `config.json`. Records without it
    /// are a bundle's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    planned: bool,
    /// The monitor: its PID and start time, so that `delete` can stop a
    /// setup that hangs.
    #[serde(default)]
    monitor: Option<(i32, u64)>,
    /// The init, once created: its PID and its start time (in clock ticks
    /// since boot, `/proc/PID/stat` field 22).
    init: Option<(i32, u64)>,
    annotations: BTreeMap<String, String>,
    /// The container's cgroup, if it has one (made by `create`, removed by
    /// the monitor at the end, or by `delete`).
    #[serde(default)]
    cgroup: Option<PathBuf>,
    /// The base of the part of the cgroup tree it was made in (root's, or
    /// the user's delegated subtree): what [`cgroup::Cgroup::container`]
    /// checks it against. Records from before rootless cgroups have none:
    /// theirs were root's.
    #[serde(default)]
    cgroup_base: Option<PathBuf>,
    /// The bundle's hooks, as `create` read them, with what their state
    /// says: the monitor runs `create`'s, `start` `poststart`, `delete`
    /// `poststop`, whatever the bundle says by then.
    #[serde(default)]
    hooks: Option<ContainerHooks>,
    /// Its cgroup directory's inode, when made: a directory at that path
    /// with another (the container's own removed, and another's made there:
    /// a `cgroupsPath` naming it) isn't this container's, and is left alone.
    #[serde(default)]
    cgroup_inode: Option<u64>,
    /// `cgroup` is about to be made, not made yet: a cgroup at that path
    /// may be another's (`create` failed, "exists already") or this one's,
    /// its `create` killed before it could say so.
    #[serde(default)]
    cgroup_pending: bool,
    /// Where its output goes (`create --log-dir`), if it's kept in logs.
    /// Records from before 8.3b have none.
    #[serde(default)]
    logs: Option<RecordedLogs>,
    /// What the monitor runs once the container has ended (`create
    /// --exit-command`). Records from before 9.3a have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_command: Option<super::ExitCommand>,
    /// What the monitor runs beside the container (`create --companion`).
    /// Records from before 9.6b have none (an older monitor ignores it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    companion: Option<super::ExitCommand>,
}

/// A container's logs, as `create` made them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecordedLogs {
    /// The directory, canonical.
    dir: PathBuf,
    /// Their rotation.
    settings: LogSettings,
    /// Whether the container takes input through attach (`--stdin`).
    /// Records from before 8.3c have none.
    #[serde(default)]
    stdin: StdinMode,
    /// The directory made, by identity: its path may come to name another.
    /// Records from before 8.8c have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<super::DirIdentity>,
}

impl Record {
    /// Its cgroup, if it has one and it's a container's own (not just
    /// pending).
    fn container_cgroup(&self) -> Option<std::result::Result<cgroup::Cgroup, cgroup::CgroupError>> {
        use std::os::unix::fs::MetadataExt as _;
        if self.cgroup_pending {
            return None;
        }
        let path = self.cgroup.clone()?;
        if let Some(inode) = self.cgroup_inode
            && std::fs::metadata(&path).is_ok_and(|now| now.ino() != inode)
        {
            return None;
        }
        Some(cgroup::Cgroup::container(path, self.base()))
    }

    /// The base its cgroup was made below (see `cgroup_base`).
    fn base(&self) -> PathBuf {
        self.cgroup_base
            .clone()
            .unwrap_or_else(|| PathBuf::from(cgroup::ROOT))
    }
}

impl Record {
    /// Removes its cgroup if it's pending, there, and empty: a `create`
    /// killed between making it and recording it. Only `rmdir`, which the
    /// kernel refuses for a cgroup with processes or cgroups in it: one that
    /// isn't empty may be another container's (with this ID, under another
    /// `--root`), and nothing is killed.
    fn remove_pending_cgroup(&self) {
        if self.cgroup_pending
            && let Some(path) = &self.cgroup
            && cgroup::Cgroup::container(path.clone(), self.base()).is_ok()
        {
            let _ = std::fs::remove_dir(path);
        }
    }
}

// --- create -------------------------------------------------------------------

/// Creates container `id` from the bundle in `bundle`, under `root`, and
/// returns its init's PID (and the bundle's warnings) once it's set up and
/// waiting for [`start`]. The container's standard input, output and error
/// are the caller's, unless `options.logs` asks for its output to be kept
/// in logs (`--log-dir`): then the monitor reads it, and its stdin is
/// `/dev/null`.
///
/// A bundle that asks for a terminal (`process.terminal`) needs
/// `options.console` (or logs, which then hold the terminal's output),
/// a connected UNIX socket (runc's `--console-socket`): the container's init
/// makes the pseudo-terminal inside the container and sends its master there
/// (`SCM_RIGHTS`, with the name `/dev/ptmx`, no answer awaited), then closes
/// the socket. The container's stdio is then that terminal.
///
/// Starts a monitor process (this program again: its `main` must call
/// [`monitor_entry_point`] first), which stays the container's parent. A
/// long-lived caller should reap it once it exits.
///
/// # Errors
///
/// A bad ID, a taken one, a bundle that can't be run (or has no PID
/// namespace), a terminal without a console socket or the reverse
/// ([`ContainerError::TerminalWithoutConsoleSocket`],
/// [`ContainerError::ConsoleSocketWithoutTerminal`]), a console socket with
/// logs, a log directory that exists ([`ContainerError::LogDirExists`]) or
/// rotation out of bounds, or a setup failure (with its message).
///
/// Without root, the container is rootless: a bundle without a user
/// namespace gets one (with a warning), as `shipyard run --bundle` does.
pub fn create(
    root: &Path,
    id: &str,
    bundle: &Path,
    options: &CreateOptions<'_>,
) -> Result<Created> {
    create_from(root, id, Source::Bundle(bundle), options)
}

/// Creates container `id` from `plan` instead of a bundle: what Shipyard's
/// engine runs, with what OCI's `config.json` can't say (an image's overlay
/// as its root, users by name, Shipyard's defaults). Otherwise as
/// [`create`]: the same checks, the same monitor. The plan is kept in the
/// state directory (`plan.json`) for the monitor, and `state` gives that
/// directory as the bundle.
///
/// # Errors
///
/// As [`create`], and a plan that can't be written.
pub fn create_planned(
    root: &Path,
    id: &str,
    plan: &Plan,
    options: &CreateOptions<'_>,
) -> Result<Created> {
    create_from(root, id, Source::Plan(plan), options)
}

/// What a container is created from.
#[derive(Clone, Copy)]
enum Source<'a> {
    /// An OCI bundle's directory.
    Bundle(&'a Path),
    /// A plan.
    Plan(&'a Plan),
}

/// What a container's source gives: where it came from, and what it runs
/// as this caller runs it.
struct FromSource {
    origin: Origin,
    bundle: crate::oci::Bundle,
}

/// Where a container came from.
enum Origin {
    /// A bundle's directory.
    Bundle(PathBuf),
    /// A plan, as `plan.json` will hold it.
    Plan(Vec<u8>),
}

/// Everything a bundle (or a plan) can get wrong, found before anything
/// exists: a bundle read and checked; a plan checked, and written as JSON
/// (as checked: what the monitor reads is what was checked here).
fn load(source: Source<'_>) -> Result<FromSource> {
    Ok(match source {
        Source::Bundle(bundle) => {
            let bundle = std::fs::canonicalize(bundle)
                .map_err(io(format!("the bundle {}", bundle.display())))?;
            FromSource {
                bundle: load_bundle_for_caller(&bundle)?,
                origin: Origin::Bundle(bundle),
            }
        }
        Source::Plan(plan) => {
            let (plan, warning) = plan.for_caller()?;
            let json = plan.to_json()?;
            let mut bundle = plan.into_bundle();
            bundle.warnings.extend(warning);
            FromSource {
                origin: Origin::Plan(json),
                bundle,
            }
        }
    })
}

fn create_from(
    root: &Path,
    id: &str,
    source: Source<'_>,
    options: &CreateOptions<'_>,
) -> Result<Created> {
    validate_id(id)?;
    let FromSource {
        origin,
        bundle: loaded,
    } = load(source)?;
    check_options(&loaded, options)?;
    // The container gets a cgroup below this process's part of the tree
    // (root's, or the user's delegated subtree). Without a PID namespace,
    // it's what finds and kills every process of the container, so then
    // it's needed.
    let tree = cgroup::Tree::find();
    if let Err(why) = &tree {
        without_cgroup(&loaded.isolation, &why.to_string())?;
    }
    // A plan has none (`Plan::for_caller`).
    let hooks = match &origin {
        Origin::Bundle(bundle) if !loaded.hooks.is_empty() => Some(ContainerHooks::new(
            loaded.hooks,
            id,
            bundle,
            loaded.annotations.clone(),
        )),
        _ => None,
    };
    let (annotations, mut warnings) = (loaded.annotations, loaded.warnings);
    if let Err(why) = &tree {
        let given_a_path = loaded.cgroups_path.is_some();
        warnings.extend(without_a_cgroup(why, given_a_path, &loaded.isolation));
    }
    let (dir, dir_fd) = make_state_dir(root, id)?;
    let planned = matches!(origin, Origin::Plan(_));
    let bundle = match origin {
        Origin::Bundle(bundle) => bundle,
        Origin::Plan(json) => match write_plan(&json, &dir, &dir_fd) {
            Ok(dir) => dir,
            Err(err) => {
                remove_own(&dir, &dir_fd, None);
                return Err(err);
            }
        },
    };
    let mut record = Record {
        bundle,
        planned,
        monitor: None,
        init: None,
        annotations,
        cgroup: None,
        cgroup_base: None,
        hooks: hooks.clone(),
        cgroup_inode: None,
        cgroup_pending: false,
        logs: None,
        exit_command: options.exit_command.clone(),
        companion: options.companion.clone(),
    };
    let cgroup = if let Ok(tree) = &tree {
        let path = loaded
            .cgroups_path
            .unwrap_or_else(|| cgroup::runtime_cgroup(id));
        match limited_cgroup(tree, &path, &loaded.isolation, (id, &dir_fd), &mut record) {
            Ok((cgroup, limited)) => {
                warnings.extend(limited);
                Some(cgroup)
            }
            Err(err) => {
                remove_own(&dir, &dir_fd, None);
                return Err(err);
            }
        }
    } else {
        None
    };
    // Made last, so that nothing before can fail and leave it behind.
    let logs = match options.logs.as_ref().map(make_log_dir).transpose() {
        Ok(logs) => logs,
        Err(err) => {
            remove_own(&dir, &dir_fd, cgroup.as_ref());
            return Err(err);
        }
    };
    record.logs = logs.as_ref().map(|made| made.recorded.clone());
    let monitor_cgroup = monitor_procs_or_said(tree.as_ref().ok(), id, &mut warnings);
    let given = ForTheMonitor {
        console: options.console,
        logs: logs.as_ref().map(|made| made.dir.as_fd()),
        fds: options.passed.filter(|passed| !passed.is_empty()),
        holds: options.holds,
        cgroup: monitor_cgroup.as_ref().map(AsFd::as_fd),
    };
    match start_monitor(id, &dir_fd, record, given, fifo_owner(&loaded.isolation)) {
        Ok((pid, monitor)) => {
            if monitor_cgroup.is_some() {
                monitor_joined_or_said(tree.as_ref().ok(), id, monitor, &mut warnings);
            }
            Ok(Created { pid, warnings })
        }
        Err(err) => Err(failed_create(
            err,
            (&dir, &dir_fd),
            cgroup.as_ref(),
            logs.as_ref(),
            hooks,
        )),
    }
}

/// What `create` says of a container that can't have a cgroup of its own
/// here (`why`): the `linux.cgroupsPath` it was given (`given_a_path`) is
/// ignored, and the device rules of its `isolation` it can't be held to.
fn without_a_cgroup(
    why: &cgroup::NoTree,
    given_a_path: bool,
    isolation: &crate::isolation::Isolation,
) -> Vec<String> {
    let mut said = Vec::new();
    if given_a_path {
        said.push(format!(
            "linux.cgroupsPath is ignored: the container can't have a cgroup of its own here: \
             {why}"
        ));
    }
    said.extend(cgroup::devices_without_cgroup(isolation, why));
    said
}

/// After a `create` that failed (`err`) once its state directory `dir`
/// existed: what it made goes (its logs too, unless something wrote to
/// them), and its `poststop` hooks run, as the spec has it: a create that
/// fails is a container destroyed. Not if a `delete` got here first
/// (killing a setup that hung, say): it destroyed the container, and ran
/// them.
fn failed_create(
    err: ContainerError,
    (dir, dir_fd): (&Path, &OwnedFd),
    cgroup: Option<&cgroup::Cgroup>,
    logs: Option<&MadeLogDir>,
    hooks: Option<ContainerHooks>,
) -> ContainerError {
    let kept = logs.and_then(keep_or_remove);
    let removed = remove_own(dir, dir_fd, cgroup);
    let mut warnings = hooks
        .filter(|_| removed)
        .map(|hooks| hooks.run_all(Phase::Poststop, None))
        .unwrap_or_default();
    warnings.extend(kept);
    with_warnings(err, &warnings)
}

/// Refuses `options` that don't go with the bundle `loaded` (see
/// [`console_matches`]), or rotation out of bounds.
fn check_options(loaded: &crate::oci::Bundle, options: &CreateOptions<'_>) -> Result<()> {
    console_matches(
        loaded.terminal.is_some(),
        options.console.is_some(),
        options.logs.is_some(),
    )?;
    check_holds(options)?;
    if let Some(logs) = &options.logs {
        logs.settings.validate()?;
        // A terminal's input has no end that closing could give: closing
        // its master is a hang-up.
        if logs.stdin == StdinMode::Once && loaded.terminal.is_some() {
            return Err(ContainerError::StdinOnceWithTerminal);
        }
    }
    Ok(())
}

/// Makes container `id`'s state directory in `root` (made if it's missing,
/// and checked: the caller's alone), and opens it.
///
/// # Errors
///
/// [`ContainerError::Exists`]: the ID is taken. Or `root` isn't safe, or
/// something couldn't be made.
fn make_state_dir(root: &Path, id: &str) -> Result<(PathBuf, OwnedFd)> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(io(format!("making {}", root.display())))?;
    check_root(root)?;
    let dir = root.join(id);
    match DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ContainerError::Exists(id.to_owned()));
        }
        Err(err) => return Err(io(format!("making {}", dir.display()))(err)),
    }
    match open_directory(&dir) {
        Ok(fd) => Ok((dir, fd)),
        Err(err) => {
            let _ = std::fs::remove_dir(&dir);
            Err(err)
        }
    }
}

/// Writes a plan's `json` into the container's state directory `dir`,
/// through `dir_fd`. Returns the directory, resolved: the container's
/// bundle as `state` gives it.
fn write_plan(json: &[u8], dir: &Path, dir_fd: &OwnedFd) -> Result<PathBuf> {
    write_atomically_at(dir_fd, PLAN_FILE, json)
        .map_err(errno_io(format!("writing {PLAN_FILE}")))?;
    std::fs::canonicalize(dir).map_err(io(format!("resolving {}", dir.display())))
}

/// Refuses holds that aren't descriptors the monitor can keep: a standard
/// stream, one given twice, one of those passed to the container's process
/// (their targets, 3 and up, or this process's copies of them), or one
/// that isn't a file, a directory or a socket (a pipe, a device: as a hold
/// added since is checked, F73); or whose label isn't one. A socket is set
/// not to linger on close as it's checked, as the monitor would (on the
/// caller's open file too, which it shares).
fn check_holds(options: &CreateOptions<'_>) -> Result<()> {
    let (count, copies) = options.passed.map_or((0, Vec::new()), |passed| {
        (passed.len(), passed.plan().sources)
    });
    let first_free = 3 + RawFd::try_from(count).unwrap_or(RawFd::MAX - 3);
    if options.holds.len() > holds::MOST
        || !holds::labels_fit(options.holds.iter().map(|hold| hold.label))
    {
        return Err(ContainerError::TooManyHolds(options.holds.len()));
    }
    let mut seen = Vec::new();
    for hold in options.holds {
        let fd = hold.fd.as_raw_fd();
        if fd < first_free
            || seen.contains(&fd)
            || copies.contains(&fd)
            || holds::holdable(hold.fd).is_err()
        {
            return Err(ContainerError::BadHold(fd));
        }
        if !crate::attach::is_label(hold.label) {
            return Err(ContainerError::BadHoldLabel(hold.label.to_owned()));
        }
        seen.push(fd);
    }
    Ok(())
}

/// After a failed `create`: its logs go too, unless something wrote to them
/// (a hook that failed, say). Then they're kept, and this says where.
///
/// Through descriptors, never by its path: the parent may be anyone's to
/// write in, and a directory renamed and a symlink put in its place there
/// would turn a removal by path into one of whatever it leads to. Its files
/// (there are only files) are removed in the directory itself; the
/// directory, from its parent, only if that name is still it.
fn keep_or_remove(made: &MadeLogDir) -> Option<String> {
    keep_or_remove_at(&made.dir, &made.parent, &made.name, &made.recorded.dir)
}

/// After a `create` whose caller then failed it (its `--pid-file` couldn't
/// be written, say): container `logs` kept or removed as a failed
/// `create`'s are (kept if anything was written to them), and only the
/// directory it made,
/// found again by identity: one put at its path since is left as it is.
/// Returns what's said of them (kept, or left), if anything.
#[must_use]
pub fn remove_failed_logs(logs: &LogsOf) -> Option<String> {
    let left = || {
        Some(format!(
            "{} isn't the log directory its container was created with: left as it is",
            logs.dir.display()
        ))
    };
    let (Some(identity), Some(parent), Some(name)) =
        (logs.identity, logs.dir.parent(), logs.dir.file_name())
    else {
        // Recorded before identities were (8.8c), or no name to find it by:
        // only a path to go by.
        return left();
    };
    let parent = open_directory(parent).ok()?;
    let dir = match nix::fcntl::openat(
        &parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(dir) => dir,
        // Gone: nothing to remove.
        Err(Errno::ENOENT) => return None,
        // A link or a file in its place, say.
        Err(_) => return left(),
    };
    if super::DirIdentity::of(dir.as_fd()).ok() != Some(identity) {
        return left();
    }
    keep_or_remove_at(&dir, &parent, name, &logs.dir)
}

/// [`keep_or_remove`] for the directory open at `dir`, named `name` in
/// `parent` (`path` is where it was made, for messages).
fn keep_or_remove_at(
    dir: &OwnedFd,
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    path: &Path,
) -> Option<String> {
    let written = nix::sys::stat::fstatat(
        dir,
        crate::logs::writer::CURRENT,
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .is_ok_and(|output| output.st_size > 0);
    if written {
        return Some(format!(
            "its output so far is in {} (shipyard-runtime logs --log-dir)",
            path.display()
        ));
    }
    if let Ok(listed) = nix::dir::Dir::openat(
        dir,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        let names: Vec<std::ffi::CString> = listed
            .into_iter()
            .flatten()
            .map(|entry| entry.file_name().to_owned())
            .filter(|name| name.as_bytes() != b"." && name.as_bytes() != b"..")
            .collect();
        for name in names {
            let _ = nix::unistd::unlinkat(
                dir,
                name.as_c_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            );
        }
    }
    let ours = nix::sys::stat::fstat(dir).ok();
    let there =
        nix::sys::stat::fstatat(parent, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).ok();
    if let (Some(ours), Some(there)) = (ours, there)
        && (ours.st_dev, ours.st_ino) == (there.st_dev, there.st_ino)
    {
        let _ = nix::unistd::unlinkat(parent, name, nix::unistd::UnlinkatFlags::RemoveDir);
    }
    None
}

/// `err`, with `warnings` (hooks that failed after it) added as lines of its
/// own: the caller gets no `Created` to carry them.
fn with_warnings(err: ContainerError, warnings: &[String]) -> ContainerError {
    if warnings.is_empty() {
        return err;
    }
    let lines: Vec<String> = warnings
        .iter()
        .map(|warning| format!("warning: {warning}"))
        .collect();
    ContainerError::Setup(format!("{err}\n{}", lines.join("\n")))
}

/// Makes the container's cgroup at `path` (below `tree`'s base) with
/// `isolation`'s limits written in and its device program attached, before
/// anything runs in it; removed again if they can't be. Returns it, and the
/// warnings: limits that do nothing here, device rules that can't be
/// enforced.
///
/// It's put in `record`, written to container `id`'s state directory (open
/// at `dir`), as pending before it's made and as made right after: a
/// `create` killed at any point must leave a cgroup `delete` can find.
fn limited_cgroup(
    tree: &cgroup::Tree,
    path: &Path,
    isolation: &crate::isolation::Isolation,
    (id, dir): (&str, &OwnedFd),
    record: &mut Record,
) -> Result<(cgroup::Cgroup, Vec<String>)> {
    use std::os::unix::fs::MetadataExt as _;
    let write = |record: &Record| {
        lock_at(dir)
            .map_err(locking(id))
            .and_then(|_lock| write_record_at(dir, record))
    };
    // Never one of Shipyard's services' (`linux.cgroupsPath` naming another
    // container's monitor's, a DNS server's): its `delete` would kill them.
    cgroup::Cgroup::container(tree.base().join(path), tree.base().to_path_buf())?;
    // Pending first: a `create` killed between the `mkdir` and the record
    // leaves a cgroup `delete` can still find (and remove only if it's empty,
    // see [`Record::remove_pending_cgroup`]).
    record.cgroup = Some(tree.base().join(path));
    record.cgroup_base = Some(tree.base().to_path_buf());
    record.cgroup_pending = true;
    write(record)?;
    let made = tree.create(path)?;
    record.cgroup = Some(made.path().to_path_buf());
    record.cgroup_pending = false;
    record.cgroup_inode = std::fs::metadata(made.path()).ok().map(|meta| meta.ino());
    let recorded = write(record);
    // What `limit` skips or warns about (a bundle's swap limit is never a
    // default: only warnings), then the device program's.
    let confined = recorded.and_then(|()| {
        let mut warnings = made.limit(&isolation.resources)?;
        warnings.extend(cgroup::enforce_devices(&made, &isolation.devices)?);
        Ok(warnings)
    });
    match confined {
        Ok(warning) => Ok((made, warning)),
        Err(err) => {
            let _ = made.remove(WAIT);
            Err(err)
        }
    }
}

/// Removes the state directory at `dir`, and the container's `cgroup`,
/// if the directory is still the one open at `fd`: a `delete` may have
/// removed both meanwhile, and a `create` made another container's at the
/// same paths. Says whether it was still there to remove.
fn remove_own(dir: &Path, fd: &OwnedFd, cgroup: Option<&cgroup::Cgroup>) -> bool {
    let Ok(_lock) = lock_at(fd) else {
        return false;
    };
    if !same_directory(fd, dir) {
        return false;
    }
    if let Some(cgroup) = cgroup {
        let _ = cgroup.remove(WAIT);
    }
    let _ = std::fs::remove_dir_all(dir);
    true
}

/// Removes the container's `cgroup` if its state directory, open at `dir`,
/// is still there. Its path is the ID's: once `delete` has removed the
/// directory (and the cgroup, under the same lock), a `create` may have made
/// a newer container with this ID, and the path would be that one's cgroup.
fn remove_cgroup_at(dir: &OwnedFd, cgroup: &cgroup::Cgroup) {
    remove_cgroup_at_with(dir, cgroup, || std::thread::sleep(POLL_INTERVAL));
}

/// [`remove_cgroup_at`], calling `between` between tries for the lock (see
/// [`lock_at_with`]).
fn remove_cgroup_at_with(dir: &OwnedFd, cgroup: &cgroup::Cgroup, between: impl FnMut()) {
    if let Ok(_lock) = lock_at_with(dir, between)
        && nix::sys::stat::fstat(dir).is_ok_and(|stat| stat.st_nlink > 0)
    {
        let _ = cgroup.remove(WAIT);
    }
}

/// Whether the directory open at `fd` is still the one at `path`: not
/// removed (its link count drops to 0), and not replaced by another.
fn same_directory(fd: &impl AsFd, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let (Ok(open), Ok(there)) = (nix::sys::stat::fstat(fd), std::fs::symlink_metadata(path)) else {
        return false;
    };
    open.st_nlink > 0 && there.dev() == open.st_dev && there.ino() == open.st_ino
}

/// Opens the state directory `dir` itself (not a symlink to one).
fn open_directory(dir: &Path) -> Result<OwnedFd> {
    nix::fcntl::open(
        dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno_io(format!("opening {}", dir.display())))
}

/// What the monitor tells `create`, as one JSON line each: first who it is,
/// its PID and start time (its first process forks it and ends: see
/// `monitor_entry_point`), then whether the container was made.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Monitor((i32, u64)),
    Created(i32),
    Failed(String),
}

/// Who the exec FIFO is given to: in a user namespace of its own, the
/// container's root is another host user (root's container with mappings),
/// and the FIFO it opens at its gate must be that user's, as runc's is.
fn fifo_owner(isolation: &crate::isolation::Isolation) -> Option<(u32, u32)> {
    let maps = isolation.id_maps.as_ref()?;
    Some((maps.host_uid(0)?, maps.host_gid(0)?))
}

/// Fills the state directory open at `dir` in, starts the monitor and
/// waits for its answer: the init's PID, and the monitor's PID and start
/// time.
///
/// Locked only while the directory is filled in: the setup can take a
/// while, and meanwhile `state` must say "creating", and `delete` must be
/// able to remove a container whose setup hangs (it kills the monitor
/// recorded here, and the container with it).
fn start_monitor(
    id: &str,
    dir: &OwnedFd,
    mut record: Record,
    given: ForTheMonitor<'_>,
    fifo_owner: Option<(u32, u32)>,
) -> Result<(i32, Option<(i32, u64)>)> {
    let (mut answers, monitor) = {
        let _lock = lock_at(dir).map_err(locking(id))?;
        let poststart = record
            .hooks
            .as_ref()
            .is_some_and(|hooks| !hooks.hooks.of(Phase::Poststart).is_empty());
        for fifo in std::iter::once(FIFO).chain(poststart.then_some(STARTED)) {
            nix::unistd::mkfifoat(dir, fifo, Mode::S_IRUSR | Mode::S_IWUSR)
                .map_err(errno_io(format!("making {fifo}")))?;
            if let Some((uid, gid)) = fifo_owner
                && nix::unistd::geteuid().is_root()
            {
                nix::unistd::fchownat(
                    dir,
                    fifo,
                    Some(nix::unistd::Uid::from_raw(uid)),
                    Some(nix::unistd::Gid::from_raw(gid)),
                    nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
                )
                .map_err(errno_io(format!("giving {fifo} to the container's root")))?;
            }
        }
        let (answers, mut first) = spawn_monitor(dir, &record.bundle, given)?;
        let mut answers = Answers::new(answers);
        // The monitor's first process ends at once (reaped here, so no
        // caller keeps a child of it), and the monitor, its child, adopted by
        // init or a subreaper, names itself first. The first process ending
        // is no failure unless its status says so (a monitor that dies
        // without a word closes the pipe: end-of-file).
        let named = answers.next(
            &mut || {
                first
                    .try_wait()
                    .ok()
                    .flatten()
                    .filter(|status| !status.success())
                    .map(|status| status.to_string())
            },
            &mut poll,
        );
        let _ = first.wait();
        let monitor = match named? {
            Answer::Monitor(monitor) => monitor,
            Answer::Failed(message) => return Err(ContainerError::Setup(message)),
            Answer::Created(_) => {
                return Err(ContainerError::Setup(
                    "the container's monitor answered out of order".to_owned(),
                ));
            }
        };
        // Its PID and start time, recorded: they name it alone.
        record.monitor = Some(monitor);
        if let Err(err) = write_record_at(dir, &record) {
            end_monitor(record.monitor);
            return Err(err);
        }
        (answers, record.monitor)
    };
    let alive = || monitor.is_some_and(|(pid, started)| is_alive(pid, started));
    let answer = answers.next(
        &mut || (!alive()).then(|| "it's gone".to_owned()),
        &mut poll,
    );
    match answer {
        Ok(Answer::Created(_)) => {}
        // It ends by itself: what it made is gone with it before the state
        // is. One still there past the wait is ended (F82).
        Ok(Answer::Failed(_)) => after_failure(monitor, LOCK_WAIT),
        // Not heard (gone, or it said nothing that makes sense): it's
        // ended, with its setup and hooks, before the state is removed.
        _ => end_monitor(monitor),
    }
    match answer? {
        Answer::Created(pid) => Ok((pid, monitor)),
        Answer::Failed(message) => Err(ContainerError::Setup(message)),
        Answer::Monitor(_) => Err(ContainerError::Setup(
            "the container's monitor answered out of order".to_owned(),
        )),
    }
}

/// Kills the monitor `monitor` names (its PID and start time), if it's
/// still that process, and its process group (the hooks it runs: it leads
/// its own session), as `delete` stops a setup.
fn end_monitor(monitor: Option<(i32, u64)>) {
    if let Some((pid, started)) = monitor {
        if signal_process(pid, started, libc::SIGKILL).is_ok() {
            // A group's ID isn't given to a new process while the group
            // has members: this can't reach anyone else.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        wait_gone(Some((pid, started)), LOCK_WAIT);
    }
}

/// Waits up to `bound` for a monitor that said its setup failed to end by
/// itself, and ends it if it hasn't (F82): its caller, told the start
/// failed, lets go of the `running` lock it gave it a copy of, so no
/// monitor may outlive the failure.
fn after_failure(monitor: Option<(i32, u64)>, bound: Duration) {
    if !wait_gone(monitor, bound) {
        end_monitor(monitor);
    }
}

/// Waits (up to `bound`) for the monitor `monitor` names to have ended (it
/// isn't this process's child to reap): whether it has.
fn wait_gone(monitor: Option<(i32, u64)>, bound: Duration) -> bool {
    let Some((pid, started)) = monitor else {
        return true;
    };
    let deadline = std::time::Instant::now() + bound;
    loop {
        if !is_alive(pid, started) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The descriptors `create` gives the monitor besides the state directory.
#[derive(Debug, Clone, Copy)]
struct ForTheMonitor<'a> {
    /// The console socket, for the container's terminal.
    console: Option<BorrowedFd<'a>>,
    /// The log directory.
    logs: Option<BorrowedFd<'a>>,
    /// Descriptors for the container's process.
    fds: Option<&'a crate::process::passed::PassedFds>,
    /// Descriptors for the monitor to hold, each with its label.
    holds: &'a [super::Hold<'a>],
    /// The `cgroup.procs` of the cgroup the monitor runs in ([`monitor_procs`]),
    /// joined by its first process before it runs any code.
    cgroup: Option<BorrowedFd<'a>>,
}

/// The cgroup container `id`'s monitor runs in (`shipyard/monitors/<id>`
/// below `tree`'s base), made if it isn't there (a policy restart's monitor
/// starts from inside the last one's): its `cgroup.procs`, open for the
/// monitor's first process to join, or `None` if this process is in it
/// already (that restart's caller: the monitor is then born in it). On the
/// way, empty ones left over (a container removed from inside its
/// monitor's, by its own clean-up) are removed.
///
/// One of its own that's there and empty is removed and made again, not
/// reused: another `create`'s sweep removes empty ones older than a minute,
/// and could remove an old one between this opening its `cgroup.procs` and
/// the monitor writing to it (the write then fails, and the monitor stays in
/// the caller's cgroup). Made again, it's new, and spared for that minute.
/// One with a process in it (a live monitor's) can't be removed, and is kept
/// ([`after_removing`]).
fn monitor_procs(
    tree: &cgroup::Tree,
    id: &str,
) -> std::result::Result<Option<std::fs::File>, String> {
    /// How old an empty one must be to go: one made a moment ago is about
    /// to be joined.
    const LEFT_OVER: std::time::Duration = std::time::Duration::from_secs(60);
    let relative = cgroup::monitor_cgroup(id);
    let path = tree.base().join(&relative);
    let own = cgroup::own_cgroup()
        .map(|own| Path::new(cgroup::ROOT).join(own.strip_prefix("/").unwrap_or(&own)));
    if own.as_deref() == Some(path.as_path()) {
        return Ok(None);
    }
    cgroup::remove_empty_left_over(&tree.base().join(cgroup::MONITOR_PARENT), id, LEFT_OVER);
    let monitors = match after_removing(std::fs::remove_dir(&path)) {
        Leaf::Make => match tree.create(&relative) {
            // Made meanwhile by another `create` of this ID: that one's.
            Err(cgroup::CgroupError::Exists(_)) => {
                cgroup::Cgroup::recorded(path, tree.base().to_owned())
            }
            made => made,
        },
        Leaf::Keep => cgroup::Cgroup::recorded(path, tree.base().to_owned()),
    }
    .map_err(|error| error.to_string())?;
    monitors
        .procs_for_a_child()
        .map(Some)
        .map_err(|error| error.to_string())
}

/// What [`monitor_procs`] does with a monitor's cgroup, once it has tried to
/// remove it (`removed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leaf {
    /// Make it: it was empty and is gone now, or wasn't there.
    Make,
    /// Use it as it is: a process is in it (`EBUSY`), or it couldn't be
    /// removed for another reason (joining it says whether it can be used).
    Keep,
}

/// See [`Leaf`].
fn after_removing(removed: std::io::Result<()>) -> Leaf {
    match removed {
        Ok(()) => Leaf::Make,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Leaf::Make,
        Err(_) => Leaf::Keep,
    }
}

/// [`monitor_procs`] in `tree` (none without one), or `None` with a warning
/// in `warnings` saying why the monitor stays in this process's cgroup.
fn monitor_procs_or_said(
    tree: Option<&cgroup::Tree>,
    id: &str,
    warnings: &mut Vec<String>,
) -> Option<std::fs::File> {
    monitor_procs(tree?, id).unwrap_or_else(|why| {
        warnings.push(monitor_elsewhere(&why));
        None
    })
}

/// What a monitor left out of its own cgroup is said as, with `why`.
fn monitor_elsewhere(why: &str) -> String {
    format!(
        "its monitor stays in this process's cgroup (a login session's, a unit's): a shutdown \
         may end it before `system stop` reaches it ({why})"
    )
}

/// Whether a monitor's cgroup has it, by its `cgroup.procs` (`procs`): any
/// process there is it, or one it started.
fn monitor_joined(procs: &str) -> bool {
    procs.lines().any(|line| !line.trim().is_empty())
}

/// Whether a monitor's join of its own cgroup failed: its cgroup's
/// `procs` empty while it's still there (`alive`). One gone already (its
/// container ended at once) has left it, and says nothing (F110).
fn join_failed(procs: &str, alive: bool) -> bool {
    alive && !monitor_joined(procs)
}

/// Once the monitor has answered (`Created`: it's in its own cgroup, or
/// isn't), its join is said in `warnings` if it failed (F61): its cgroup in
/// `tree` read, and found empty while the monitor (its PID and start time)
/// is still there. A read that fails says nothing (a monitor gone since,
/// its cgroup removed, can't be told from one that failed).
fn monitor_joined_or_said(
    tree: Option<&cgroup::Tree>,
    id: &str,
    monitor: Option<(i32, u64)>,
    warnings: &mut Vec<String>,
) {
    let Some(tree) = tree else {
        return;
    };
    let procs = tree
        .base()
        .join(cgroup::monitor_cgroup(id))
        .join("cgroup.procs");
    if let Ok(procs) = std::fs::read_to_string(procs)
        && join_failed(
            &procs,
            monitor.is_some_and(|(pid, started)| is_alive(pid, started)),
        )
    {
        warnings.push(monitor_elsewhere("it couldn't join its own"));
    }
}

/// Removes container `id`'s monitor's cgroup. Its last processes (the
/// monitor's clean-up, ending as it lets go of the container) may take a
/// moment to leave it: waited for, briefly. From inside it (that clean-up
/// removing its own container, `--rm`) it can't be removed, so isn't tried:
/// a later `create` removes it, empty.
pub fn remove_monitor_cgroup(id: &str) {
    /// How long its last processes get to leave it.
    const LEAVING: std::time::Duration = std::time::Duration::from_secs(1);
    let Ok(tree) = cgroup::Tree::find() else {
        return;
    };
    let path = tree.base().join(cgroup::monitor_cgroup(id));
    let own = cgroup::own_cgroup()
        .map(|own| Path::new(cgroup::ROOT).join(own.strip_prefix("/").unwrap_or(&own)));
    if own.is_some_and(|own| own.starts_with(&path)) {
        return;
    }
    let deadline = std::time::Instant::now() + LEAVING;
    while let Err(error) = std::fs::remove_dir(&path) {
        if error.raw_os_error() != Some(libc::EBUSY) || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Starts the monitor for the state directory open at `dir`, with the
/// bundle at `bundle` (and the console socket, log directory and the
/// container's process's descriptors, `given`). Returns the pipe its
/// answers come on, and its first process (which forks the monitor and ends
/// at once: the caller reaps it).
fn spawn_monitor(
    dir: &OwnedFd,
    bundle: &Path,
    given: ForTheMonitor<'_>,
) -> Result<(OwnedFd, std::process::Child)> {
    let (answers, answer_end) =
        nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(errno_io("making the monitor's pipe"))?;
    // Its ends must survive its exec: copies without close-on-exec, closed
    // as soon as it's spawned. (Another thread spawning meanwhile could
    // inherit them, which is why `Answers::next` watches the monitor, not
    // end-of-file.)
    let inheritable = |fd: BorrowedFd<'_>| {
        nix::unistd::dup(fd).map_err(errno_io("passing the monitor its descriptors"))
    };
    let answer_end = inheritable(answer_end.as_fd())?;
    let dir = inheritable(dir.as_fd())?;
    let console = given.console.map(inheritable).transpose()?;
    let logs = given.logs.map(inheritable).transpose()?;
    // The container's process's: their own numbers, not copies (a copy
    // each would be a third set of them under the descriptor limit), made
    // inheritable in the child alone.
    let passing = given.fds.map(crate::process::passed::PassedFds::plan);
    // And the holds, their own numbers too: a copy in this process, even for
    // a moment, would be inherited by whatever another thread spawns
    // meanwhile, and keep a lock held for good.
    let holds: Vec<(RawFd, &str)> = given
        .holds
        .iter()
        .map(|hold| (hold.fd.as_raw_fd(), hold.label))
        .collect();
    let cgroup_procs = given.cgroup.map(|procs| procs.as_raw_fd());
    let inherit: Vec<RawFd> = passing
        .as_ref()
        .map(|passing| passing.sources.clone())
        .unwrap_or_default()
        .into_iter()
        .chain(holds.iter().map(|&(fd, _)| fd))
        .collect();
    let mut command = std::process::Command::new("/proc/self/exe");
    // In `/`, not the caller's directory: it lives as long as the container,
    // and a working directory pins its mount (unmounting it fails, busy).
    // It needs none: the bundle's path is absolute, the rest descriptors.
    command.current_dir("/");
    // A session of its own (and so a process group) from the start, before
    // it runs any code: the caller's terminal closing, or a Ctrl-C meant for
    // the caller, never reaches it. In the child, before the exec: `setsid`
    // fails for a process-group leader, so it can't come after
    // `process_group(0)`. (The monitor, forked from this first process, makes
    // a session of its own again: see `monitor_entry_point`.)
    // No signal blocked either: a signal mask survives `fork` and `exec`,
    // and a caller holding off Ctrl-C while it creates (Shipyard's engine)
    // would leave the monitor, and its exit command, deaf to SIGTERM.
    // SAFETY: the closure runs between `fork` and `exec`; it only calls
    // `write`, `sigemptyset`, `sigprocmask`, `setsid` and `fcntl`, which are
    // async-signal-safe, reads `errno`, and walks a list made before the
    // fork (no allocation).
    unsafe {
        command.pre_exec(move || {
            // Out of the caller's cgroup first: the monitor forked from
            // this process, and all it starts, live in its own. (Joined by
            // writing "0", this process; a failure leaves it where it is,
            // and `create` says so in its warnings, once it has answered.)
            if let Some(procs) = cgroup_procs {
                let _ = libc::write(procs, b"0".as_ptr().cast(), 1);
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut none);
            if libc::sigprocmask(libc::SIG_SETMASK, &raw const none, std::ptr::null_mut()) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for &fd in &inherit {
                // `fcntl` is async-signal-safe; the numbers were read
                // before the fork.
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let monitor = command
        .arg0("shipyard-monitor")
        .arg(MONITOR_ARG)
        .arg(bundle)
        .arg(dir.as_raw_fd().to_string())
        .arg(answer_end.as_raw_fd().to_string())
        .args(monitor::tagged(
            console.as_ref().map(AsRawFd::as_raw_fd),
            logs.as_ref().map(AsRawFd::as_raw_fd),
            passing.as_ref(),
            &holds,
        ))
        .spawn()
        .map_err(io("starting the container's monitor"))?;
    Ok((answers, monitor))
}

/// The monitor's answers, a line of JSON each: what's read past one line
/// (the next, arrived with it) is kept for the next.
struct Answers {
    fd: OwnedFd,
    buffered: Vec<u8>,
}

impl Answers {
    fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            buffered: Vec::new(),
        }
    }

    /// Reads the next answer. Watches the process that's to answer while
    /// waiting (`gone`: why it's ended, if it has), so one that died
    /// without answering is noticed even if its end of the pipe leaked
    /// elsewhere. `look` is `poll` (a test's may see nothing).
    fn next(
        &mut self,
        gone: &mut impl FnMut() -> Option<String>,
        look: &mut impl FnMut(&mut [PollFd<'_>], PollTimeout) -> nix::Result<libc::c_int>,
    ) -> Result<Answer> {
        // Once it's gone: what it wrote before it went (why it failed) is
        // still in the pipe, so it's read to the end before it counts as no
        // answer.
        let mut ended = None;
        loop {
            if let Some(end) = self.buffered.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffered.drain(..=end).collect();
                let text = line.get(..end).unwrap_or_default();
                return serde_json::from_slice(text).map_err(|err| {
                    ContainerError::Setup(format!(
                        "the container's monitor answered nonsense: {err}"
                    ))
                });
            }
            let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
            match look(&mut fds, PollTimeout::from(100_u16)) {
                Ok(0) | Err(Errno::EINTR) => {
                    if let Some(why) = ended {
                        return Err(ContainerError::Setup(format!(
                            "the container's monitor ended without answering ({why})"
                        )));
                    }
                    ended = gone();
                    continue;
                }
                Ok(_) => {}
                Err(errno) => return Err(errno_io("waiting for the monitor")(errno)),
            }
            let mut buf = [0_u8; 4096];
            let read = nix::unistd::read(self.fd.as_fd(), &mut buf)
                .map_err(errno_io("reading the monitor's answer"))?;
            if read == 0 {
                return Err(ContainerError::Setup(match ended {
                    Some(why) => {
                        format!("the container's monitor ended without answering ({why})")
                    }
                    None => "the container's monitor ended without answering".into(),
                }));
            }
            self.buffered
                .extend_from_slice(buf.get(..read).unwrap_or_default());
        }
    }
}

// --- The monitor (monitor.rs, capture.rs) ---------------------------------------

mod attach;
mod capture;
mod control;
mod exec;
mod helper;
mod holds;
mod monitor;

pub use control::{Waiter, hold, release, stop, wait, waiter};
pub use exec::exec;
pub use helper::{helper_entry_point, start_helper, start_helper_in_view};

pub use monitor::monitor_entry_point;

/// As runc: a terminal (the bundle's `process.terminal`) goes to a console
/// socket, and a console socket is for a terminal. With logs, the terminal
/// goes into them instead, and a console socket has nothing to receive.
fn console_matches(terminal: bool, console: bool, logs: bool) -> Result<()> {
    match (terminal, console, logs) {
        (_, true, true) => Err(ContainerError::ConsoleSocketWithLogs),
        (true, false, false) => Err(ContainerError::TerminalWithoutConsoleSocket),
        (false, true, false) => Err(ContainerError::ConsoleSocketWithoutTerminal),
        _ => Ok(()),
    }
}

/// Refuses `isolation` for a container without a cgroup (for the reason
/// `why`), if it needs one: without a PID namespace, nothing else can find
/// all its processes; and limits need one to be written in.
fn without_cgroup(isolation: &crate::isolation::Isolation, why: &str) -> Result<()> {
    if !isolation.namespaces.contains(&Namespace::Pid) {
        return Err(ContainerError::NoPidNamespace(why.to_owned()));
    }
    if !isolation.resources.is_empty() {
        return Err(ContainerError::LimitsNeedACgroup(why.to_owned()));
    }
    Ok(())
}

/// Whether child `pid` has exited (not reaping it).
fn exited(pid: i32) -> bool {
    let id = nix::sys::wait::Id::Pid(nix::unistd::Pid::from_raw(pid));
    let flags = nix::sys::wait::WaitPidFlag::WEXITED
        | nix::sys::wait::WaitPidFlag::WNOHANG
        | nix::sys::wait::WaitPidFlag::WNOWAIT;
    !matches!(
        nix::sys::wait::waitid(id, flags),
        Ok(nix::sys::wait::WaitStatus::StillAlive)
    )
}

/// A log directory `create` made: what's recorded of it, its descriptor, and
/// where it is (its parent's descriptor, and its name there).
struct MadeLogDir {
    recorded: RecordedLogs,
    dir: OwnedFd,
    parent: OwnedFd,
    name: std::ffi::OsString,
}

/// Makes the log directory `options.dir` (which mustn't exist: making it is
/// what claims it, so no two containers share one, and nobody else has
/// planted anything in it), 0700, and opens it. Returns what `state.json`
/// records of it, and its descriptor for the monitor.
fn make_log_dir(options: &LogOptions) -> Result<MadeLogDir> {
    let path = &options.dir;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io(format!("the log directory {}", path.display()))(
            std::io::Error::other("not a directory's path"),
        ));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = std::fs::canonicalize(parent).map_err(io(format!(
        "the log directory's parent {}",
        parent.display()
    )))?;
    let parent_fd = open_directory(&parent)?;
    match nix::sys::stat::mkdirat(&parent_fd, name, Mode::S_IRWXU) {
        Ok(()) => {}
        Err(Errno::EEXIST) => return Err(ContainerError::LogDirExists(path.clone())),
        Err(errno) => return Err(errno_io(format!("making {}", path.display()))(errno)),
    }
    let dir = parent.join(name);
    let made = (|| {
        let fd = nix::fcntl::openat(
            &parent_fd,
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_io(format!("opening {}", dir.display())))?;
        // The one made just now: not another put in its place meanwhile
        // (the parent may be anyone's to write in).
        let made =
            nix::sys::stat::fstat(&fd).map_err(errno_io(format!("checking {}", dir.display())))?;
        if made.st_uid != nix::unistd::geteuid().as_raw() {
            return Err(io(format!("the log directory {}", dir.display()))(
                std::io::Error::other("replaced by someone else's while it was made"),
            ));
        }
        // Exactly 0700, whatever the umask made of it.
        nix::sys::stat::fchmod(&fd, Mode::S_IRWXU)
            .map_err(errno_io(format!("setting {}'s mode", dir.display())))?;
        Ok((fd, super::DirIdentity::from_stat(&made)))
    })();
    match made {
        Ok((fd, identity)) => Ok(MadeLogDir {
            recorded: RecordedLogs {
                dir,
                settings: options.settings,
                stdin: options.stdin,
                identity: Some(identity),
            },
            dir: fd,
            parent: parent_fd,
            name: name.to_owned(),
        }),
        Err(err) => {
            let _ = nix::unistd::unlinkat(&parent_fd, name, nix::unistd::UnlinkatFlags::RemoveDir);
            Err(err)
        }
    }
}

/// Where container `id`'s logs are, and whether they're complete (the
/// monitor writes the exit record once it has drained the container's
/// output into them; or the monitor is gone).
///
/// # Errors
///
/// No such container, or [`ContainerError::NotCaptured`] for one created
/// without logs.
pub fn logs_of(root: &Path, id: &str) -> Result<LogsOf> {
    let dir = existing(root, id)?;
    let record = read_record(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    let logs = record
        .logs
        .ok_or_else(|| ContainerError::NotCaptured(id.to_owned()))?;
    // The monitor writes the exit once it has drained the output into the
    // logs; a monitor that died (killed) writes nothing more either.
    let finished = std::fs::symlink_metadata(dir.join(EXIT_FILE)).is_ok()
        || record
            .monitor
            .is_none_or(|(pid, started)| !is_alive(pid, started));
    Ok(LogsOf {
        dir: logs.dir,
        finished,
        identity: logs.identity,
        instance: record.monitor,
    })
}

/// Connects to container `id`'s `attach.sock`, through its state directory
/// (the path may be longer than a socket address holds).
///
/// # Errors
///
/// No such container, one created without logs
/// ([`ContainerError::NotAttachable`]), one that's stopped, or a monitor
/// that's gone.
pub fn attach_stream(root: &Path, id: &str) -> Result<std::os::unix::net::UnixStream> {
    let dir = existing(root, id)?;
    let record = read_record(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    if record.logs.is_none() {
        return Err(ContainerError::NotAttachable(id.to_owned()));
    }
    let status = status_of(&dir)?;
    if status == Status::Stopped {
        return Err(wrong(id, status, "attach", LIVE_WANTED));
    }
    let fd = nix::fcntl::open(
        &dir,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno_io(format!("opening {}", dir.display())))?;
    let path = attach::socket_path(&fd);
    std::os::unix::net::UnixStream::connect(path).map_err(|err| match err.kind() {
        // Created by a Shipyard before attach: no socket.
        std::io::ErrorKind::NotFound => ContainerError::NotAttachable(id.to_owned()),
        // The socket is there, nobody listens: its monitor was killed, or
        // has just let go of it as the container ended (see `exit_code`).
        std::io::ErrorKind::ConnectionRefused => ContainerError::MonitorGone(id.to_owned()),
        _ => io(format!("connecting to container {id}'s monitor"))(err),
    })
}

/// Container `id`'s exit code, if it has ended (as its monitor recorded it).
///
/// # Errors
///
/// No such container.
pub fn exit_code(root: &Path, id: &str) -> Result<Option<i32>> {
    Ok(exit_record(root, id)?.and_then(|ended| ended.code))
}

/// The network namespace of created container `id`'s init, open: for a
/// caller that connects the container to its networks between `create` and
/// `start` (Shipyard's engine). The init's `/proc` directory is opened
/// first and its start time checked against the record's, so a PID reused
/// since is refused; the namespace is opened through it.
///
/// # Errors
///
/// No such container, one that isn't created (not yet, or started), its
/// init gone, or the namespace couldn't be opened.
pub fn network_namespace(root: &Path, id: &str) -> Result<OwnedFd> {
    network_namespace_when(root, id, &[Status::Created], "created")
}

/// The network namespace of running (or paused) container `id`'s init,
/// open, as [`network_namespace`] opens it: for a caller that connects the
/// container to a network while it runs (Shipyard's `network connect`).
///
/// # Errors
///
/// No such container, one that isn't running or paused, its init gone, or
/// the namespace couldn't be opened.
pub fn network_namespace_live(root: &Path, id: &str) -> Result<OwnedFd> {
    network_namespace_when(
        root,
        id,
        &[Status::Running, Status::Paused],
        "running or paused",
    )
}

/// The network namespace of container `id`'s init, if its status is one
/// of `allowed` (said as `wanted`), under its lock; the init's start time
/// checked.
fn network_namespace_when(
    root: &Path,
    id: &str,
    allowed: &[Status],
    wanted: &'static str,
) -> Result<OwnedFd> {
    use nix::fcntl::{OFlag, open, openat};
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !allowed.contains(&status) {
        return Err(wrong(id, status, "connect", wanted));
    }
    let record = read_record(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    let Some((pid, started)) = record.init else {
        return Err(wrong(id, Status::Creating, "connect", wanted));
    };
    let gone = || wrong(id, Status::Stopped, "connect", wanted);
    let proc_dir = open(
        format!("/proc/{pid}").as_str(),
        OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| gone())?;
    if crate::process::join::start_time_at(&proc_dir) != Some(started) {
        return Err(gone());
    }
    openat(
        &proc_dir,
        "ns/net",
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno_io(format!("opening /proc/{pid}/ns/net")))
}

/// Created container `id`'s init: its PID, its start time (which, with the
/// PID, names it alone), and a pidfd for it where the kernel has them
/// (Linux 5.3: readable once it has exited), for a caller that follows it
/// (Shipyard's engine starts a detached container's pasta so). The pidfd
/// is opened first and the start time checked after, so a PID reused
/// between the two is refused, never followed.
///
/// # Errors
///
/// No such container, one that isn't created (not yet, or started), or
/// its init gone.
pub fn init_process(root: &Path, id: &str) -> Result<super::InitProcess> {
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if status != Status::Created {
        return Err(wrong(id, status, "follow", "created"));
    }
    let record = read_record(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    let Some((pid, started)) = record.init else {
        return Err(ContainerError::Setup(format!(
            "container {id} has no init recorded"
        )));
    };
    let pidfd = pidfd_open(pid);
    if !is_alive(pid, started) {
        return Err(wrong(id, Status::Stopped, "follow", "created"));
    }
    Ok(super::InitProcess {
        pid,
        start_time: started,
        pidfd,
    })
}

/// How container `id` ended, as its monitor recorded it, in any version
/// (see [`ExitRecord`]); `None` until it has. A record that can't be made
/// sense of (a newer Shipyard's, or damaged) still says it ended, with no
/// code. This is the way to read one: its versions are handled here.
///
/// # Errors
///
/// No such container, or the file couldn't be read.
pub fn exit_record(root: &Path, id: &str) -> Result<Option<ExitRecord>> {
    let dir = existing(root, id)?;
    let file = match shipyard_sys::fs::open_for_reading(&dir.join(EXIT_FILE)) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(io(format!("reading {}", dir.join(EXIT_FILE).display()))(
                err,
            ));
        }
    };
    // One larger than any the monitor writes isn't read: it's a record
    // that can't be made sense of, which still says it ended.
    let bytes = match shipyard_sys::fs::read_bounded_from(&file, versions::EXIT_MAX) {
        Err(err)
            if matches!(
                shipyard_sys::fs::refusal(&err),
                Some(shipyard_sys::fs::Refusal::TooLarge { .. })
            ) =>
        {
            Vec::new()
        }
        read => read.map_err(io(format!("reading {}", dir.join(EXIT_FILE).display())))?,
    };
    let mut ended = ExitRecord::parse(&bytes);
    // Version 0 says nothing of when (nor one that can't be read): the
    // monitor wrote it last, so the file's modification time is the end's.
    if ended.finished.is_none() {
        ended.finished = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
    }
    Ok(Some(ended))
}

// --- start, state, kill, delete -----------------------------------------------

/// Lets container `id`, created and waiting, run its program, then runs
/// its `poststart` hooks. Returns their failures, as warnings: the
/// container runs anyway, as the spec says.
///
/// Its `startContainer` hooks run in it once it's let go, before its
/// program: a failure there ends it (see its exit record), and isn't this
/// call's.
///
/// # Errors
///
/// No such container, one that isn't created (already started, or
/// stopped), or an init that doesn't answer at the gate.
pub fn start(root: &Path, id: &str) -> Result<Vec<String>> {
    let dir = existing(root, id)?;
    let lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if status != Status::Created {
        return Err(wrong(id, status, "start", "created"));
    }
    // Read before the container is let go: once it is, `start` must not
    // fail.
    let record = read_record(&dir)?;
    let fifo = nix::fcntl::open(
        &dir.join(FIFO),
        OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno_io("opening the exec FIFO"))?;
    // The init, blocked opening the other end, now writes its byte. One
    // that's dead writes nothing: give up after a while.
    let mut fds = [PollFd::new(fifo.as_fd(), PollFlags::POLLIN)];
    let ready = poll(
        &mut fds,
        PollTimeout::try_from(WAIT).unwrap_or(PollTimeout::MAX),
    )
    .map_err(errno_io("waiting for the container's init"))?;
    let mut byte = [0_u8; 1];
    if ready == 0 || nix::unistd::read(fifo.as_fd(), &mut byte) != Ok(1) {
        return Err(ContainerError::Setup(format!(
            "container {id}'s init didn't answer at the start gate"
        )));
    }
    // It's started: from here on, problems are only warnings.
    let mut warnings = Vec::new();
    // Made through the directory (never by path), the caller's alone, and a
    // symlink there isn't followed.
    if let Err(err) = nix::fcntl::openat(
        &*lock,
        GATE_OPENED,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    ) {
        warnings.push(format!(
            "recording the start ({err}): its end won't be told from a container that never ran"
        ));
    }
    // Closed at once: the init goes on only once no reader is left (see
    // `wait_at_gate`), and `poststart` below must not hold the program back.
    drop(fifo);
    if let Err(err) = std::fs::remove_file(dir.join(FIFO)) {
        warnings.push(format!("removing the exec FIFO: {err}"));
    }
    // Not under the lock: a hook may well ask for the container's state.
    drop(lock);
    if let Some((hooks, pid)) = record.and_then(|record| Some((record.hooks?, record.init?.0))) {
        // After the program has started, as the spec says: once nobody
        // holds the started FIFO for writing (the init's command has
        // `execve`d, or it's gone).
        // Its `startContainer` hooks run first: their time is theirs.
        let hooks_time: u64 = hooks
            .hooks
            .of(Phase::StartContainer)
            .iter()
            .filter_map(|hook| hook.timeout)
            .map(u64::from)
            .sum();
        warnings.extend(wait_started(
            &dir,
            id,
            WAIT + Duration::from_secs(hooks_time),
        ));
        warnings.extend(hooks.run_all(Phase::Poststart, Some(pid)));
    }
    Ok(warnings)
}

/// Waits until container `id`'s command has `execve`d: until nobody has
/// its started FIFO open for writing (its init holds it till then; see
/// `Gate::started`), up to `allowed`, then removes it. On a FIFO with no
/// writer left, a read gives end of file; with one, `EAGAIN` (non-blocking).
/// `poll` would report the hang-up here too (a writer was there when this
/// opened it), but not for a non-blocking reader opened with no writer yet:
/// a read has no such exception. A warning past `allowed`: the hooks run
/// anyway.
fn wait_started(dir: &Path, id: &str, allowed: Duration) -> Option<String> {
    let path = dir.join(STARTED);
    let fifo = match nix::fcntl::open(
        &path,
        OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fifo) => fifo,
        // None made (no `poststart` hooks when it was created).
        Err(Errno::ENOENT) => return None,
        Err(errno) => return Some(format!("opening the started FIFO: {}", errno.desc())),
    };
    let _ = std::fs::remove_file(&path);
    let deadline = std::time::Instant::now() + allowed;
    let mut byte = [0_u8; 1];
    loop {
        match nix::unistd::read(fifo.as_fd(), &mut byte) {
            Ok(0) => return None,
            // A writer is left (or wrote, which nobody should: ignored).
            Ok(_) | Err(Errno::EAGAIN | Errno::EINTR) => {}
            Err(errno) => {
                return Some(format!(
                    "waiting for the program to start: {}",
                    errno.desc()
                ));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Some(format!(
                "container {id}'s program hadn't started after {} s: its poststart hooks ran \
                 anyway",
                allowed.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Container `id`'s state.
///
/// # Errors
///
/// No such container, or a state directory that can't be read.
pub fn state(root: &Path, id: &str) -> Result<State> {
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let record = read_record(&dir)?;
    let status = status_of(&dir)?;
    let live = LIVE.contains(&status);
    Ok(State {
        oci_version: OCI_VERSION.to_owned(),
        id: id.to_owned(),
        // OCI's `stopped` is "the container process has exited": stopping
        // is that, to a caller polling for it (Podman, containerd), which
        // may know no other value. Shipyard's own refusals say stopping.
        status: if status == Status::Stopping {
            Status::Stopped
        } else {
            status
        },
        pid: record
            .as_ref()
            .and_then(|r| r.init)
            .map(|(pid, _)| pid)
            .filter(|_| live),
        bundle: record
            .as_ref()
            .map(|r| r.bundle.clone())
            .unwrap_or_default(),
        annotations: record.map(|r| r.annotations).unwrap_or_default(),
    })
}

/// Where container `id` is in its life, as Shipyard sees it: [`state`]'s
/// status, except that a container whose init has ended while its monitor
/// cleans up is [`Status::Stopping`] (OCI's `state` says stopped). `None`
/// if there's no such container.
///
/// # Errors
///
/// An invalid ID, or its state unreadable.
pub fn status(root: &Path, id: &str) -> Result<Option<Status>> {
    let dir = match existing(root, id) {
        Ok(dir) => dir,
        Err(ContainerError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let _lock = match lock(&dir) {
        Ok(lock) => lock,
        Err(ContainerError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    status_of(&dir).map(Some)
}

/// [`status`], read without taking the container's lock: for a listing
/// (`ps`), which must never wait behind a `delete` or a `create`. Every
/// file it reads is written whole (renamed into place), so it reads a state
/// that was, if not the one a moment later.
///
/// # Errors
///
/// As [`status`].
pub fn peek_status(root: &Path, id: &str) -> Result<Option<Status>> {
    let dir = match existing(root, id) {
        Ok(dir) => dir,
        Err(ContainerError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    match status_of(&dir) {
        // Removed meanwhile: gone.
        Err(_) if !dir.exists() => Ok(None),
        other => other.map(Some),
    }
}

/// Container `id`'s init's PID and start time, as its state records them,
/// read without taking its lock (as [`peek_status`]): `None` if it has none
/// (being made, or no such container). It may have ended since, and its PID
/// been reused: [`is_process`] says whether it's still the one.
///
/// # Errors
///
/// Its state can't be read.
pub fn peek_init(root: &Path, id: &str) -> Result<Option<(i32, u64)>> {
    let dir = match existing(root, id) {
        Ok(dir) => dir,
        Err(ContainerError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(read_record(&dir)?.and_then(|record| record.init))
}

/// Container `id`'s monitor's PID and start time, as its state records
/// them, read without taking its lock (as [`peek_init`]): `None` if it has
/// none (or no such container). Each start has a monitor of its own: what
/// tells one start's state from the next's.
///
/// # Errors
///
/// Its state can't be read.
pub fn peek_monitor(root: &Path, id: &str) -> Result<Option<(i32, u64)>> {
    let dir = match existing(root, id) {
        Ok(dir) => dir,
        Err(ContainerError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(read_record(&dir)?.and_then(|record| record.monitor))
}

/// Process `pid`'s start time (clock ticks since boot, as [`peek_init`]
/// and [`peek_monitor`] give them), if it's there (a zombie isn't).
#[must_use]
pub fn process_started(pid: i32) -> Option<u64> {
    start_time(pid)
}

/// Whether process `pid` is still the one that started at `started` (clock
/// ticks since boot, as [`peek_init`] gives them): not gone, nor its PID
/// reused.
#[must_use]
pub fn is_process(pid: i32, started: u64) -> bool {
    is_alive(pid, started)
}

/// Sends `signal` to container `id`'s init, which passes it on to the
/// program. While the container is only created, signals other than
/// SIGKILL wait until it starts; while it's paused, until it's resumed
/// (SIGKILL ends it at once).
///
/// # Errors
///
/// No such container, one that isn't created, running or paused, or SIGSTOP
/// (see [`ContainerError::StopSignal`]).
pub fn kill(root: &Path, id: &str, signal: Signal) -> Result<()> {
    refuse_stop(id, signal)?;
    let dir = existing(root, id)?;
    let lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !LIVE.contains(&status) {
        return Err(wrong(id, status, "kill", LIVE_WANTED));
    }
    let (pid, started) = read_record(&dir)?
        .and_then(|r| r.init)
        .ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    mark_if_killing(&lock, signal.as_raw());
    signal_process(pid, started, signal.as_raw())
        .map_err(errno_io(format!("signalling container {id}")))
}

/// Sends `signal` to every process in container `id`'s cgroup (runc's
/// `kill --all`), as [`kill`] sends it to the init.
///
/// # Errors
///
/// As [`kill`], and for a container without a cgroup.
pub fn kill_all(root: &Path, id: &str, signal: Signal) -> Result<()> {
    refuse_stop(id, signal)?;
    with_cgroup_locked(root, id, "kill", &LIVE, LIVE_WANTED, |cgroup, dir| {
        mark_if_killing(dir, signal.as_raw());
        cgroup.signal_all(signal.as_raw());
        Ok(())
    })
}

/// Leaves [`KILLED_FILE`] in the state directory open at `dir` before
/// `signal` is sent, if it's SIGKILL. Best effort: without it, an OOM kill
/// earlier in the container's life is all a later SIGKILL could be blamed
/// on. Made through the directory (never by path), the caller's alone, and
/// a symlink there isn't followed.
fn mark_if_killing(dir: &OwnedFd, signal: i32) {
    if signal == libc::SIGKILL {
        let _ = nix::fcntl::openat(
            dir,
            KILLED_FILE,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::S_IRUSR | Mode::S_IWUSR,
        );
    }
}

/// SIGSTOP can't be caught or passed on: it would stop the init, not the
/// program (see [`ContainerError::StopSignal`]).
fn refuse_stop(id: &str, signal: Signal) -> Result<()> {
    if signal.as_raw() == libc::SIGSTOP {
        return Err(ContainerError::StopSignal(id.to_owned()));
    }
    Ok(())
}

/// Freezes container `id`'s processes (`cgroup.freeze`): a created or
/// running one becomes paused (and `resume` brings it back to where it
/// was).
///
/// # Errors
///
/// No such container, one that isn't created or running, one without a
/// cgroup, or a freeze that didn't happen in time.
pub fn pause(root: &Path, id: &str) -> Result<()> {
    // As runc: a created container may be paused too.
    let pausable = [Status::Created, Status::Running];
    with_cgroup(
        root,
        id,
        "pause",
        &pausable,
        "created or running",
        |cgroup| Ok(cgroup.freeze(true, WAIT)?),
    )
}

/// Thaws a paused container `id`: it runs again.
///
/// # Errors
///
/// No such container, one that isn't paused, or a thaw that didn't happen
/// in time.
pub fn resume(root: &Path, id: &str) -> Result<()> {
    with_cgroup(root, id, "resume", &[Status::Paused], "paused", |cgroup| {
        Ok(cgroup.freeze(false, WAIT)?)
    })
}

/// Changes container `id`'s limits to `resources` (what's given; the rest
/// stays as it is). Returns what was skipped, to warn about.
///
/// # Errors
///
/// No such container, a stopped one, one without a cgroup, or a limit the
/// kernel refused.
pub fn update(
    root: &Path,
    id: &str,
    resources: &crate::resources::Resources,
) -> Result<Vec<String>> {
    with_cgroup(root, id, "update", &LIVE, LIVE_WANTED, |cgroup| {
        Ok(cgroup.limit(resources)?)
    })
}

/// Container `id`'s memory limit (`memory.max`), if it has one: what
/// `update --memory-swap` alone counts from.
///
/// # Errors
///
/// As [`update`].
pub fn memory_limit(root: &Path, id: &str) -> Result<Option<u64>> {
    with_cgroup(root, id, "update", &LIVE, LIVE_WANTED, |cgroup| {
        Ok(Some(cgroup.stats().memory.usage.limit).filter(|&limit| limit != u64::MAX))
    })
}

/// The processes in container `id`'s cgroup, by host PID.
///
/// # Errors
///
/// No such container, a stopped one, or one without a cgroup.
pub fn ps(root: &Path, id: &str) -> Result<Vec<i32>> {
    with_cgroup(root, id, "ps", &LIVE, LIVE_WANTED, |cgroup| {
        Ok(cgroup
            .pids()
            .into_iter()
            .map(nix::unistd::Pid::as_raw)
            .collect())
    })
}

/// What container `id` has used, and how many of its processes the OOM
/// killer has killed so far.
///
/// # Errors
///
/// No such container, a stopped one, or one without a cgroup.
pub fn stats(root: &Path, id: &str) -> Result<(crate::cgroup::Stats, u64)> {
    with_cgroup(root, id, "events", &LIVE, LIVE_WANTED, |cgroup| {
        Ok((cgroup.stats(), cgroup.oom_kills()))
    })
}

/// A container that has processes: created, running or paused.
const LIVE: [Status; 3] = [Status::Created, Status::Running, Status::Paused];
/// [`LIVE`], as an error says it.
const LIVE_WANTED: &str = "created, running or paused";

/// Runs `act` on container `id`'s cgroup, holding the container's lock (so a
/// `delete` can't remove it meanwhile), if the container is in one of the
/// `allowed` states (`wanted` says which, for `action`'s error).
fn with_cgroup<T>(
    root: &Path,
    id: &str,
    action: &'static str,
    allowed: &[Status],
    wanted: &'static str,
    act: impl FnOnce(&cgroup::Cgroup) -> Result<T>,
) -> Result<T> {
    with_cgroup_locked(root, id, action, allowed, wanted, |cgroup, _| act(cgroup))
}

/// [`with_cgroup`], `act` given the state directory too (open and locked).
fn with_cgroup_locked<T>(
    root: &Path,
    id: &str,
    action: &'static str,
    allowed: &[Status],
    wanted: &'static str,
    act: impl FnOnce(&cgroup::Cgroup, &OwnedFd) -> Result<T>,
) -> Result<T> {
    let dir = existing(root, id)?;
    let lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !allowed.contains(&status) {
        return Err(wrong(id, status, action, wanted));
    }
    act(&cgroup_of(id, read_record(&dir)?.as_ref())?, &lock)
}

/// Container `id`'s cgroup, from its `record`.
fn cgroup_of(id: &str, record: Option<&Record>) -> Result<cgroup::Cgroup> {
    match record.and_then(Record::container_cgroup) {
        Some(Ok(cgroup)) => Ok(cgroup),
        Some(Err(err)) => Err(err.into()),
        None => Err(ContainerError::NeedsCgroup(id.to_owned())),
    }
}

/// Removes container `id`, then runs its `poststop` hooks. A created one is
/// killed first (as runc does); a running one only with `force`. Returns
/// the hooks' failures, as warnings: the container is gone anyway.
///
/// # Errors
///
/// No such container, a running one without `force`, or one that won't
/// stop.
pub fn delete(root: &Path, id: &str, force: bool) -> Result<Vec<String>> {
    let dir = existing(root, id)?;
    let lock = lock(&dir)?;
    // Stopping (its init gone, its monitor still cleaning up) is stopped
    // enough here: this removes the cgroup itself, under the lock the
    // monitor waits for (and whose directory, once gone, it leaves alone).
    // So is an init that ended since `status_of` looked.
    let status = match status_of(&dir)? {
        Status::Created | Status::Running | Status::Paused if init_gone(&dir)? => Status::Stopped,
        status => status,
    };
    match status {
        Status::Running | Status::Paused if !force => {
            return Err(wrong(id, status, "delete", "stopped (or --force)"));
        }
        // A paused one too: SIGKILL ends a frozen process (G8).
        Status::Created | Status::Running | Status::Paused => {
            if let Some((pid, started)) = read_record(&dir)?.and_then(|r| r.init) {
                mark_if_killing(&lock, libc::SIGKILL);
                let _ = signal_process(pid, started, libc::SIGKILL);
            }
            let deadline = Instant::now() + WAIT;
            while !init_gone(&dir)? {
                if Instant::now() > deadline {
                    return Err(ContainerError::Setup(format!("container {id} didn't stop")));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        Status::Creating => {
            // A setup that hangs never answers: stop it through its monitor
            // (the init dies with it, by its parent-death signal), and the
            // hooks it runs (`prestart`, `createRuntime`) with it: they're
            // in its process group (it leads its own session), and must not
            // go on past the `poststop` hooks below.
            if let Some((pid, started)) = read_record(&dir)?.and_then(|r| r.monitor)
                && signal_process(pid, started, libc::SIGKILL).is_ok()
            {
                // A group's ID isn't given to a new process while the group
                // has members: this can't reach anyone else.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        // Its monitor still cleaning up is stopped enough too (see above).
        Status::Stopping | Status::Stopped => {}
    }
    // The monitor removes the cgroup at the container's end; if it died
    // first, this does, killing whatever is left (without a PID namespace,
    // the init's death doesn't take the rest along).
    // One that's gone already (with the user manager it was below, at a
    // logout) is nothing to remove: its base may be gone too.
    let record = read_record(&dir)?;
    if let Some(record) = &record {
        record.remove_pending_cgroup();
    }
    if let Some(recorded) = record
        .as_ref()
        .filter(|record| record.cgroup.as_ref().is_some_and(|path| path.exists()))
        .and_then(Record::container_cgroup)
    {
        recorded.and_then(|cgroup| cgroup.remove(WAIT))?;
    }
    std::fs::remove_dir_all(&dir).map_err(io(format!("removing {}", dir.display())))?;
    remove_monitor_cgroup(id);
    // Not under the lock (a hook may ask for the state: it's gone now).
    drop(lock);
    Ok(record
        .and_then(|record| record.hooks)
        .map(|hooks| hooks.run_all(Phase::Poststop, None))
        .unwrap_or_default())
}

// --- The state directory -------------------------------------------------------

/// Which of `kinds` process `target` has that this process doesn't share
/// (one it shares is joined already). One it has exited from reads as none.
///
/// # Errors
///
/// Its namespaces aren't this user's to look at
/// ([`crate::Error::JoinNotYours`], or as root, [`crate::Error::JoinRefused`]).
fn unshared(
    target: u32,
    kinds: &[crate::isolation::Namespace],
) -> std::result::Result<Vec<crate::isolation::Namespace>, crate::Error> {
    use std::os::unix::fs::MetadataExt as _;
    let inode = |pid: &str, kind: crate::isolation::Namespace| match std::fs::metadata(format!(
        "/proc/{pid}/ns/{}",
        kind.proc_name()
    )) {
        Ok(meta) => Ok(Some(meta.ino())),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(crate::process::join::permission_denied())
        }
        Err(_) => Ok(None),
    };
    let target = target.to_string();
    let mut found = Vec::new();
    for &kind in kinds {
        let theirs = inode(&target, kind)?;
        if theirs.is_some() && theirs != inode("self", kind)? {
            found.push(kind);
        }
    }
    Ok(found)
}

/// `ROOT/ID`, which must exist: a directory, not a symlink to one.
fn existing(root: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    if !root.exists() {
        return Err(ContainerError::NotFound(id.to_owned()));
    }
    check_root(root)?;
    let dir = root.join(id);
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.is_dir() => Ok(dir),
        _ => Err(ContainerError::NotFound(id.to_owned())),
    }
}

/// The state root must be root's alone: a real directory (not a symlink),
/// owned by this user, writable by nobody else. Otherwise another user
/// could plant files (or a symlink) where root's runtime reads, writes and
/// deletes.
fn check_root(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata =
        std::fs::symlink_metadata(root).map_err(io(format!("checking {}", root.display())))?;
    let reason = if !metadata.is_dir() {
        Some("it isn't a directory (or it's a symbolic link)")
    } else if metadata.uid() != nix::unistd::geteuid().as_raw() {
        Some("it belongs to another user")
    } else if metadata.mode() & 0o022 != 0 {
        Some("others can write to it")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ContainerError::UnsafeRoot {
            path: root.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Closes every descriptor from 3 up but those in `keep`.
fn close_inherited(keep: &[RawFd]) {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    // Listed first, closed after: the listing's own descriptor is among them.
    let open: Vec<RawFd> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2 && !keep.contains(&fd))
        .collect();
    for fd in open {
        let _ = nix::unistd::close(fd);
    }
}

/// An exclusive lock on the state directory `dir`, held until dropped.
///
/// Locked through a descriptor, then checked: a `delete` that held the lock
/// meanwhile may have removed the directory, and a `create` made a new one
/// at the path. Under the lock, the directory at the path stays what it is
/// (only `delete` removes it, and only with the lock).
fn lock(dir: &Path) -> Result<Flock<OwnedFd>> {
    let id = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A few times: each mismatch is a whole delete and create in between.
    for _ in 0..3 {
        let fd = match open_directory(dir) {
            Ok(fd) => fd,
            Err(_) if !dir.exists() => return Err(ContainerError::NotFound(id)),
            Err(err) => return Err(err),
        };
        let locked = lock_at(&fd).map_err(locking(&id))?;
        if same_directory(&*locked, dir) {
            return Ok(locked);
        }
        if !dir.exists() {
            return Err(ContainerError::NotFound(id));
        }
    }
    Err(ContainerError::Busy(id))
}

/// An exclusive lock on the state directory open at `dir`, held until
/// dropped. Waits up to [`LOCK_WAIT`], then fails with EWOULDBLOCK: a call
/// that's stopped holding the lock mustn't block every other call for good.
///
/// The lock is on an open file description of its own (`openat(".")`):
/// `flock` locks belong to descriptions, and the monitor's copy of `dir`
/// shares one with `create`'s, so locking it would never wait.
fn lock_at(dir: &OwnedFd) -> nix::Result<Flock<OwnedFd>> {
    lock_at_with(dir, || std::thread::sleep(POLL_INTERVAL))
}

/// [`lock_at`], calling `between` between tries instead of sleeping: the
/// monitor serves its clients meanwhile (F41), waiting on them for about
/// [`POLL_INTERVAL`].
fn lock_at_with(dir: &OwnedFd, between: impl FnMut()) -> nix::Result<Flock<OwnedFd>> {
    let deadline = Instant::now() + LOCK_WAIT;
    let fd = nix::fcntl::openat(
        dir,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    shipyard_sys::lock::lock_until_with(fd, shipyard_sys::lock::Mode::Exclusive, deadline, between)
}

/// The error for container `id` when [`lock_at`] fails: busy if it timed
/// out.
fn locking(id: &str) -> impl FnOnce(Errno) -> ContainerError {
    let id = id.to_owned();
    move |errno| match errno {
        Errno::EWOULDBLOCK => ContainerError::Busy(id),
        errno => errno_io("locking the state directory")(errno),
    }
}

fn wrong(id: &str, status: Status, action: &'static str, wanted: &'static str) -> ContainerError {
    ContainerError::WrongStatus {
        id: id.to_owned(),
        status,
        action,
        wanted,
    }
}

/// Where the container is: stopped once it ended (its exit recorded, or its
/// init gone or replaced and its monitor too), created while the gate is
/// there, running after.
///
/// An init that's gone isn't enough while its monitor lives: the monitor is
/// still removing the cgroup, and a stopped container has nothing left (a
/// `cgroupsPath` it names is free to make again).
fn status_of(dir: &Path) -> Result<Status> {
    let Some(record) = read_record(dir)? else {
        return Ok(Status::Creating);
    };
    let Some((pid, started)) = record.init else {
        return Ok(Status::Creating);
    };
    let init_alive = is_alive(pid, started);
    let monitor_alive = record
        .monitor
        .is_some_and(|(monitor, started)| is_alive(monitor, started));
    if dir.join(EXIT_FILE).exists() || !(init_alive || monitor_alive) {
        return Ok(Status::Stopped);
    }
    if !init_alive {
        // Its monitor lives, removing the cgroup and draining the output:
        // stopped once it records the end.
        return Ok(Status::Stopping);
    }
    if record
        .container_cgroup()
        .and_then(std::result::Result::ok)
        .is_some_and(|cgroup| cgroup.frozen())
    {
        return Ok(Status::Paused);
    }
    Ok(if dir.join(FIFO).exists() {
        Status::Created
    } else {
        Status::Running
    })
}

/// Whether the container's init ended (its exit recorded, or it's gone or
/// replaced), whatever its monitor is doing.
fn init_gone(dir: &Path) -> Result<bool> {
    Ok(dir.join(EXIT_FILE).exists()
        || read_record(dir)?
            .and_then(|record| record.init)
            .is_some_and(|(pid, started)| !is_alive(pid, started)))
}

fn read_record(dir: &Path) -> Result<Option<Record>> {
    match shipyard_sys::fs::read_bounded(&dir.join(STATE_FILE), versions::STATE_MAX) {
        Ok(bytes) => parse_record(&bytes, &dir.join(STATE_FILE).display()).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io(format!("reading {}", dir.join(STATE_FILE).display()))(
            err,
        )),
    }
}

/// `state.json` in the directory open at `dir` (`None` if it's missing).
fn read_record_at(dir: &OwnedFd) -> Result<Option<Record>> {
    match shipyard_sys::fs::read_bounded_at(dir.as_fd(), STATE_FILE, versions::STATE_MAX) {
        Ok(bytes) => parse_record(&bytes, &STATE_FILE).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io(format!("reading {STATE_FILE}"))(err)),
    }
}

/// A record of any version this Shipyard reads (`file` names it in an
/// error): 0 (stage 8's, without a `version`) has the same shape as 1.
fn parse_record(bytes: &[u8], file: &dyn std::fmt::Display) -> Result<Record> {
    versions::check_state_version(bytes)?;
    serde_json::from_slice(bytes).map_err(|err| ContainerError::Setup(format!("{file}: {err}")))
}

fn write_record_at(dir: &OwnedFd, record: &Record) -> Result<()> {
    /// What's written: the record, with its version.
    #[derive(Serialize)]
    struct Written<'a> {
        version: u32,
        #[serde(flatten)]
        record: &'a Record,
    }
    let json = serde_json::to_vec(&Written {
        version: versions::STATE_VERSION,
        record,
    })
    .map_err(|err| ContainerError::Setup(err.to_string()))?;
    write_atomically_at(dir, STATE_FILE, &json).map_err(errno_io(format!("writing {STATE_FILE}")))
}

/// Writes `name` in the directory open at `dir` whole or not at all
/// (`shipyard_sys::fs::write_atomically_at`): the runtime's own files, the
/// caller's alone. Not synced: the runtime's root is under `/run`, which a
/// restart empties anyway. In a removed directory, ENOENT.
fn write_atomically_at(dir: &OwnedFd, name: &str, bytes: &[u8]) -> nix::Result<()> {
    use shipyard_sys::fs::{Durability, Write};
    let how = Write::new(0o600).durability(Durability::Atomic);
    shipyard_sys::fs::write_atomically_at(dir.as_fd(), name, bytes, &how)
        .map_err(|error| crate::process::io_errno(&error.source))
}

/// A process's start time (`/proc/PID/stat` field 22), if it exists and
/// hasn't exited (a zombie counts as gone).
fn start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    crate::process::start_time_in(&stat)
}

/// Whether `pid` is still the process (an init or a monitor) recorded with
/// start time `started`: not gone, nor its PID reused.
fn is_alive(pid: i32, started: u64) -> bool {
    start_time(pid) == Some(started)
}

/// The signal's error when `pidfd_open` refused (`error`), or `None` where
/// pidfds can't be had at all ([`shipyard_sys::pidfd::unsupported`]: then
/// a checked `kill`). Any other refusal (`ESRCH`: gone; `EMFILE`) is
/// returned, never the PID-reuse race the pidfd closes (F110).
fn unpinned(error: &std::io::Error) -> Option<Errno> {
    if shipyard_sys::pidfd::unsupported(error) {
        return None;
    }
    Some(error.raw_os_error().map_or(Errno::EIO, Errno::from_raw))
}

/// A pidfd for `pid` (`pidfd_open`, Linux 5.3): readable once it has
/// exited. None where it can't be had (an older kernel, a seccomp policy
/// that refuses it): then the clock wakes the loop.
fn pidfd_open(pid: i32) -> Option<OwnedFd> {
    shipyard_sys::pidfd::open(pid).ok()
}

/// Sends `signal` to process `pid`, if it's still the one that started at
/// `started` (an init or a monitor). Through a pidfd where
/// the kernel has them (5.3): once open, it names that process even if it
/// dies and its PID is reused, so the check and the signal can't be split
/// by a reuse. Without pidfds (an old kernel, a policy refusing them), a
/// plain `kill` after the check; any other refusal is the error (F110).
fn signal_process(pid: i32, started: u64, signal: libc::c_int) -> nix::Result<()> {
    let pidfd = match shipyard_sys::pidfd::open(pid) {
        Ok(pidfd) => Some(pidfd),
        Err(error) => match unpinned(&error) {
            Some(errno) => return Err(errno),
            None => None,
        },
    };
    let Some(pidfd) = pidfd else {
        if !is_alive(pid, started) {
            return Err(Errno::ESRCH);
        }
        // By number: a real-time signal too (nix's `Signal` has none).
        // SAFETY: `kill` with plain integers.
        return Errno::result(unsafe { libc::kill(pid, signal) }).map(drop);
    };
    if !is_alive(pid, started) {
        return Err(Errno::ESRCH);
    }
    shipyard_sys::pidfd::send_signal(pidfd.as_fd(), signal)
        .map_err(|error| Errno::from_raw(error.raw_os_error().unwrap_or(libc::EIO)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only where pidfds can't be had does a signal go by number (F110):
    /// running out of descriptors, or the process gone, is the signal's
    /// error, not the PID-reuse race.
    #[test]
    fn only_a_kernel_without_pidfds_signals_by_number() {
        let refused = |code| unpinned(&std::io::Error::from_raw_os_error(code));
        assert_eq!(refused(libc::ENOSYS), None);
        assert_eq!(refused(libc::EPERM), None, "a policy's refusal");
        for code in [libc::EMFILE, libc::ESRCH, libc::ENOMEM] {
            assert_eq!(refused(code), Some(Errno::from_raw(code)), "{code}");
        }
        assert_eq!(
            unpinned(&std::io::Error::other("no number")),
            Some(Errno::EIO)
        );
    }

    /// A monitor that said its setup failed and is still there past the
    /// wait is ended (F82); one that ends by itself within it is left to
    /// (the control: not killed, and not waited for to the bound).
    #[test]
    fn a_monitor_that_outlives_its_failure_is_ended() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let spawn = |script: &str| {
            let child = std::process::Command::new("sh")
                .args(["-c", script])
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = i32::try_from(child.id()).unwrap();
            (child, Some((pid, start_time(pid).unwrap())))
        };
        let (mut stuck, monitor) = spawn("exec sleep 60");
        after_failure(monitor, Duration::from_millis(50));
        let status = stuck.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        let (mut ending, monitor) = spawn("sleep 0.3; exit 0");
        let began = std::time::Instant::now();
        after_failure(monitor, Duration::from_secs(10));
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert!(ending.wait().unwrap().success());
    }

    /// A monitor's cgroup that was there and empty (removed now), or wasn't
    /// there, is made again, new (a sweep spares it for a minute); one with
    /// a process in it, or that couldn't be removed, is kept as it is.
    #[test]
    fn a_monitors_empty_cgroup_is_made_again_and_a_live_one_kept() {
        let failed = |errno| Err(std::io::Error::from_raw_os_error(errno));
        assert_eq!(after_removing(Ok(())), Leaf::Make);
        assert_eq!(after_removing(failed(libc::ENOENT)), Leaf::Make);
        assert_eq!(after_removing(failed(libc::EBUSY)), Leaf::Keep);
        assert_eq!(after_removing(failed(libc::EACCES)), Leaf::Keep);
    }

    /// A monitor that answered (why the container can't be made), then
    /// ended, between a look at its pipe that saw nothing and the check
    /// that it's still there: its answer is read, not taken for none.
    #[test]
    fn an_answer_a_gone_monitor_left_in_its_pipe_is_read() {
        let (from, to) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let mut monitor = std::process::Command::new("sh")
            .args(["-c", r#"printf '{"failed":"no room"}\n'"#])
            .stdout(std::process::Stdio::from(to))
            .spawn()
            .unwrap();
        monitor.wait().unwrap();
        let mut first = true;
        let look = |fds: &mut [PollFd<'_>], wait: PollTimeout| {
            if std::mem::take(&mut first) {
                return Ok(0);
            }
            poll(fds, wait)
        };
        let mut look = look;
        let answer = Answers::new(from)
            .next(
                &mut || {
                    monitor
                        .try_wait()
                        .ok()
                        .flatten()
                        .map(|status| status.to_string())
                },
                &mut look,
            )
            .unwrap();
        assert!(matches!(answer, Answer::Failed(why) if why == "no room"));
    }

    /// Two answers come in one read (the monitor named, then the container
    /// made at once): both are read, in order.
    #[test]
    fn answers_in_one_read_are_read_in_order() {
        let (from, to) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        nix::unistd::write(&to, b"{\"monitor\":[42,9]}\n{\"created\":7}\n").unwrap();
        drop(to);
        let mut answers = Answers::new(from);
        let mut never = || None;
        assert!(matches!(
            answers.next(&mut never, &mut poll).unwrap(),
            Answer::Monitor((42, 9))
        ));
        assert!(matches!(
            answers.next(&mut never, &mut poll).unwrap(),
            Answer::Created(7)
        ));
        assert!(answers.next(&mut never, &mut poll).is_err(), "no more");
    }

    /// A monitor that names itself without its start time isn't taken at
    /// its word: a PID alone could be a stranger's by the time it's read.
    #[test]
    fn a_monitor_without_its_start_time_is_refused() {
        let (from, to) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        nix::unistd::write(&to, b"{\"monitor\":42}\n").unwrap();
        drop(to);
        assert!(Answers::new(from).next(&mut || None, &mut poll).is_err());
    }

    /// A failed create's logs, removed after it (`--pid-file`), only if the
    /// directory at their path is still the one made: one put there since,
    /// with its own files, is left as it is, and the empty one made goes.
    #[test]
    fn failed_logs_are_removed_only_from_the_directory_made() {
        let base =
            std::env::temp_dir().join(format!("shipyard-failed-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).unwrap();
        let path = base.join("logs");
        std::fs::create_dir(&path).unwrap();
        let made = open_directory(&path).unwrap();
        let logs = LogsOf {
            dir: path.clone(),
            finished: true,
            identity: Some(super::super::DirIdentity::of(made.as_fd()).unwrap()),
            instance: None,
        };
        drop(made);
        // Another put in its place: kept, with what's in it.
        std::fs::rename(&path, base.join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("theirs"), "keep me").unwrap();
        let said = remove_failed_logs(&logs).unwrap();
        assert!(said.contains("isn't the log directory"), "{said}");
        assert!(path.join("theirs").exists());
        // The one made, back at its path: removed.
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::rename(base.join("moved"), &path).unwrap();
        assert_eq!(remove_failed_logs(&logs), None);
        assert!(!path.exists());
        // A link in its place: left, and nothing removed through it.
        std::fs::create_dir(base.join("moved")).unwrap();
        std::os::unix::fs::symlink(base.join("moved"), &path).unwrap();
        assert!(remove_failed_logs(&logs).is_some());
        assert!(base.join("moved").exists());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(base.join("moved")).unwrap();
        // A record from before identities: only a path, so left.
        std::fs::create_dir(&path).unwrap();
        let unknown = LogsOf {
            identity: None,
            ..logs
        };
        assert!(remove_failed_logs(&unknown).is_some());
        assert!(path.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_start_time_is_found_after_any_command_name() {
        // Fields 3 (the state) to 21, then the start time, then more.
        let fields_after = std::iter::once("S".to_owned())
            .chain((4..=21).map(|n| n.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        let line = format!("42 (a ) b (c) {fields_after} 777 23\n");
        assert_eq!(crate::process::start_time_in(line.as_bytes()), Some(777));
        let zombie = line.replacen(") S", ") Z", 1);
        assert_eq!(
            crate::process::start_time_in(zombie.as_bytes()),
            None,
            "a zombie is gone"
        );
        assert_eq!(crate::process::start_time_in(b"garbage"), None);
    }

    #[test]
    fn this_process_is_its_own_init() {
        let me = i32::try_from(std::process::id()).unwrap();
        let started = start_time(me).unwrap();
        assert!(is_alive(me, started));
        assert!(
            !is_alive(me, started + 1),
            "a reused PID has another start time"
        );
        assert_eq!(signal_process(me, started + 1, 0), Err(Errno::ESRCH));
        assert_eq!(signal_process(me, started, 0), Ok(()));
    }

    #[test]
    fn the_status_follows_the_state_directory() {
        let dir = std::env::temp_dir().join(format!("shipyard-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(status_of(&dir).unwrap(), Status::Creating);
        let me = i32::try_from(std::process::id()).unwrap();
        let record = |init| Record {
            bundle: "/b".into(),
            planned: false,
            monitor: None,
            init,
            annotations: BTreeMap::new(),
            cgroup: None,
            cgroup_base: None,
            hooks: None,
            cgroup_inode: None,
            cgroup_pending: false,
            logs: None,
            exit_command: None,
            companion: None,
        };
        let fd = open_directory(&dir).unwrap();
        let write_record = |_: &Path, record: &Record| write_record_at(&fd, record);
        write_record(&dir, &record(Some((me, start_time(me).unwrap())))).unwrap();
        std::fs::write(dir.join(FIFO), "").unwrap();
        assert_eq!(status_of(&dir).unwrap(), Status::Created);
        std::fs::remove_file(dir.join(FIFO)).unwrap();
        assert_eq!(status_of(&dir).unwrap(), Status::Running);
        write_record(&dir, &record(Some((me, 1)))).unwrap();
        assert_eq!(status_of(&dir).unwrap(), Status::Stopped, "a reused PID");
        let finishing = Record {
            monitor: Some((me, start_time(me).unwrap())),
            ..record(Some((me, 1)))
        };
        write_record(&dir, &finishing).unwrap();
        assert_eq!(
            status_of(&dir).unwrap(),
            Status::Stopping,
            "an init gone, its monitor still cleaning up"
        );
        std::fs::write(dir.join(EXIT_FILE), "exited").unwrap();
        assert_eq!(
            status_of(&dir).unwrap(),
            Status::Stopped,
            "the end recorded"
        );
        std::fs::remove_file(dir.join(EXIT_FILE)).unwrap();
        let monitor_gone = Record {
            monitor: Some((me, 1)),
            ..record(Some((me, 1)))
        };
        write_record(&dir, &monitor_gone).unwrap();
        assert_eq!(
            status_of(&dir).unwrap(),
            Status::Stopped,
            "a monitor killed while cleaning up"
        );
        write_record(&dir, &record(Some((me, start_time(me).unwrap())))).unwrap();
        std::fs::write(dir.join(EXIT_FILE), "exited").unwrap();
        assert_eq!(status_of(&dir).unwrap(), Status::Stopped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `status` says what `state` hides: a container stopping. A missing
    /// one is none, not an error.
    #[test]
    fn status_tells_stopping_and_none() {
        let root = std::env::temp_dir().join(format!("shipyard-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join("ending"))
            .unwrap();
        assert_eq!(status(&root, "missing").unwrap(), None);
        let me = i32::try_from(std::process::id()).unwrap();
        let started = start_time(me).unwrap();
        let fd = open_directory(&root.join("ending")).unwrap();
        let record: Record = parse_record(
            format!(
                r#"{{"bundle":"/b","monitor":[{me},{started}],"init":[{me},1],"annotations":{{}}}}"#
            )
            .as_bytes(),
            &"test",
        )
        .unwrap();
        write_record_at(&fd, &record).unwrap();
        assert_eq!(status(&root, "ending").unwrap(), Some(Status::Stopping));
        assert_eq!(state(&root, "ending").unwrap().status, Status::Stopped);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A monitor's records go through its descriptor of the directory: once
    /// that's removed and a new container made at the same path, they land
    /// nowhere (ENOENT), not in the new one.
    #[test]
    fn a_removed_directory_takes_no_more_records() {
        let dir = std::env::temp_dir().join(format!("shipyard-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stale = open_directory(&dir).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            write_atomically_at(&stale, EXIT_FILE, b"exited"),
            Err(Errno::ENOENT)
        );
        assert!(read_record_at(&stale).unwrap().is_none());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir(&dir).unwrap();
    }

    /// `killed` is made through the directory, the caller's alone: a
    /// symlink planted in its place isn't followed (nothing is made where it
    /// leads).
    #[test]
    fn the_killed_marker_follows_no_symlink() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("shipyard-killed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let fd = open_directory(&dir.join("state")).unwrap();
        mark_if_killing(&fd, libc::SIGTERM);
        assert!(
            !dir.join("state").join(KILLED_FILE).exists(),
            "only for SIGKILL"
        );
        mark_if_killing(&fd, libc::SIGKILL);
        let made = std::fs::symlink_metadata(dir.join("state").join(KILLED_FILE)).unwrap();
        assert!(made.is_file());
        assert_eq!(made.permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(dir.join("state").join(KILLED_FILE)).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("state").join(KILLED_FILE))
            .unwrap();
        mark_if_killing(&fd, libc::SIGKILL);
        assert!(!dir.join("elsewhere").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A `state.json` larger than [`versions::STATE_MAX`] isn't read, by
    /// path or through its directory: refused, a failure to read it.
    #[test]
    fn a_state_file_over_its_bound_is_refused() {
        let dir = std::env::temp_dir().join(format!("shipyard-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fd = open_directory(&dir).unwrap();
        // The control: none there reads as none.
        assert!(read_record(&dir).unwrap().is_none());
        let file = std::fs::File::create(dir.join(STATE_FILE)).unwrap();
        file.set_len(versions::STATE_MAX + 1).unwrap();
        let refused = |err: ContainerError| match err {
            ContainerError::Io { error, .. } => shipyard_sys::fs::refusal(&error),
            other => panic!("{other}"),
        };
        let too_large = Some(shipyard_sys::fs::Refusal::TooLarge {
            cap: versions::STATE_MAX,
        });
        assert_eq!(refused(read_record(&dir).unwrap_err()), too_large);
        assert_eq!(refused(read_record_at(&fd).unwrap_err()), too_large);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An exit record is read up to [`versions::EXIT_MAX`]: a larger one
    /// isn't read at all, and counts as one that can't be made sense of (it
    /// ended, with no code); stage 8's text is given the time it was
    /// written.
    #[test]
    fn an_exit_record_is_read_within_bounds() {
        let root = std::env::temp_dir().join(format!("shipyard-exit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join("big"))
            .unwrap();
        assert_eq!(exit_record(&root, "big").unwrap(), None, "not ended yet");
        // The control: stage 8's text, within the bound, gives its code.
        std::fs::write(root.join("big").join(EXIT_FILE), b"exited (exit code 4)").unwrap();
        let ended = exit_record(&root, "big").unwrap().unwrap();
        assert_eq!((ended.version, ended.code), (0, Some(4)));
        let mut huge = b"exited (exit code 4)".to_vec();
        huge.resize(usize::try_from(versions::EXIT_MAX).unwrap() + 1, b' ');
        std::fs::write(root.join("big").join(EXIT_FILE), &huge).unwrap();
        let ended = exit_record(&root, "big").unwrap().unwrap();
        assert_eq!((ended.version, ended.code), (0, None));
        let written = std::fs::metadata(root.join("big").join(EXIT_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(ended.finished, Some(written));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A monitor's cgroup has it when its `cgroup.procs` lists anyone
    /// (F61: an empty one means its join failed, said in `create`'s
    /// warnings).
    #[test]
    fn a_monitor_has_joined_when_its_cgroup_lists_it() {
        assert!(!monitor_joined(""));
        assert!(!monitor_joined("\n"));
        assert!(monitor_joined("4242\n"));
        assert!(monitor_joined("4242\n4243\n"));
        // Said only while the monitor is there: one gone already (its
        // container ended at once) left it (F110).
        assert!(join_failed("", true));
        assert!(!join_failed("", false), "gone: nothing to say");
        assert!(!join_failed("4242\n", true));
    }

    /// A hold must be above the standard streams and the descriptors passed
    /// to the container, given once, and a file, a directory or a socket
    /// (F73: a pipe's end is refused, as a hold added since is).
    #[test]
    fn holds_are_checked() {
        use super::super::Hold;
        let one = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let other = std::fs::File::open("/").unwrap();
        let (socket, _) = std::os::unix::net::UnixStream::pair().unwrap();
        let (reading, _writing) = std::io::pipe().unwrap();
        let held = |fd, label| Hold { fd, label };
        let check = |holds: &[Hold<'_>]| {
            check_holds(&CreateOptions {
                holds,
                ..CreateOptions::default()
            })
        };
        assert!(check(&[held(one.as_fd(), "lease:app"), held(other.as_fd(), "port")]).is_ok());
        assert!(check(&[held(socket.as_fd(), "port")]).is_ok(), "a socket");
        assert!(matches!(
            check(&[held(reading.as_fd(), "a")]),
            Err(ContainerError::BadHold(_))
        ));
        // Two of a label are fine (two ports): only a hold since must be
        // the only one of its label.
        assert!(check(&[held(one.as_fd(), "port"), held(other.as_fd(), "port")]).is_ok());
        assert!(matches!(
            check(&[held(one.as_fd(), "a"), held(one.as_fd(), "b")]),
            Err(ContainerError::BadHold(_))
        ));
        // SAFETY: standard error is open for the test's whole life.
        let stderr = unsafe { BorrowedFd::borrow_raw(2) };
        assert!(matches!(
            check(&[held(stderr, "a")]),
            Err(ContainerError::BadHold(2))
        ));
        for label in ["", "a,b", "a=b", "a b"] {
            assert!(
                matches!(
                    check(&[held(one.as_fd(), label)]),
                    Err(ContainerError::BadHoldLabel(_))
                ),
                "{label:?}"
            );
        }
    }

    /// A record keeps its exit command; one from before 9.3a has none.
    #[test]
    fn records_keep_their_exit_command_and_companion() {
        let older = parse_record(
            br#"{"bundle": "/b", "init": null, "annotations": {}}"#,
            &"older",
        )
        .unwrap();
        assert_eq!(older.exit_command, None);
        assert_eq!(older.companion, None);
        let command = super::super::ExitCommand::new("/bin/cleanup", vec!["web".into()]).unwrap();
        let companion = super::super::ExitCommand::new("/bin/health", vec!["web".into()]).unwrap();
        let record = Record {
            exit_command: Some(command.clone()),
            companion: Some(companion.clone()),
            ..older
        };
        let dir = std::env::temp_dir().join(format!("shipyard-exitcmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fd = open_directory(&dir).unwrap();
        write_record_at(&fd, &record).unwrap();
        let read = read_record_at(&fd).unwrap().unwrap();
        assert_eq!(read.exit_command, Some(command));
        assert_eq!(read.companion, Some(companion));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn records_keep_their_hooks_and_older_ones_have_none() {
        // Stage 8's, without a version: read as version 0.
        let older = parse_record(
            br#"{"bundle": "/b", "init": null, "annotations": {}}"#,
            &"older",
        )
        .unwrap();
        assert!(older.hooks.is_none());
        let mut hooks = crate::hooks::Hooks::default();
        hooks.poststop.push(crate::hooks::Hook::new("/bin/cleanup"));
        let record = Record {
            hooks: Some(ContainerHooks::new(hooks, "c1", "/b", BTreeMap::new())),
            ..older
        };
        // Written as this Shipyard writes it, with its version.
        let dir = std::env::temp_dir().join(format!("shipyard-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fd = open_directory(&dir).unwrap();
        write_record_at(&fd, &record).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(written["version"], versions::STATE_VERSION);
        let back = read_record_at(&fd).unwrap().unwrap();
        assert_eq!(back.hooks, record.hooks);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
