//! The lifecycle on Linux (see the module above): the state directory, the
//! monitor, and the calls.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};

use super::{ContainerError, Created, OCI_VERSION, Result, State, Status, validate_id};
use crate::cgroup;
use crate::hooks::{ContainerHooks, Phase};
use crate::isolation::Namespace;
use crate::oci::load_bundle_for_caller;
use crate::process::spawn::{Placement, SpawnOptions, spawn};
use crate::process::stdio::ChildTerminal;
use crate::process::{ExitStatus, Signal};

/// The monitor's hidden first argument (see [`monitor_entry_point`]).
const MONITOR_ARG: &str = "__shipyard-monitor";

const STATE_FILE: &str = "state.json";
const FIFO: &str = "exec.fifo";
const EXIT_FILE: &str = "exit";
/// Left by a call that sends SIGKILL (`kill`, `kill --all`, `delete`): the
/// container's end is theirs, not the OOM killer's, even if it struck
/// earlier (a process it killed, and the container went on).
const KILLED_FILE: &str = "killed";

/// A console's size when the bundle gives none (`process.consoleSize`):
/// rows, columns. runc leaves it 0x0, which full-screen programs can't
/// draw in; a shim resizes it anyway.
const DEFAULT_CONSOLE_SIZE: (u16, u16) = (24, 80);

/// How long `start` waits for the init's answer at the gate, and `delete`
/// for a killed container to stop.
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
    /// the user's delegated subtree): what [`cgroup::Cgroup::recorded`]
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
        Some(cgroup::Cgroup::recorded(path, self.base()))
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
            && cgroup::Cgroup::recorded(path.clone(), self.base()).is_ok()
        {
            let _ = std::fs::remove_dir(path);
        }
    }
}

// --- create -------------------------------------------------------------------

/// Creates container `id` from the bundle in `bundle`, under `root`, and
/// returns its init's PID (and the bundle's warnings) once it's set up and
/// waiting for [`start`]. The container's standard input, output and error
/// are the caller's.
///
/// A bundle that asks for a terminal (`process.terminal`) needs `console`,
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
/// [`ContainerError::ConsoleSocketWithoutTerminal`]), or a setup failure
/// (with its message).
///
/// Without root, the container is rootless: a bundle without a user
/// namespace gets one (with a warning), as `shipyard run --bundle` does.
pub fn create(
    root: &Path,
    id: &str,
    bundle: &Path,
    console: Option<BorrowedFd<'_>>,
) -> Result<Created> {
    validate_id(id)?;
    let bundle =
        std::fs::canonicalize(bundle).map_err(io(format!("the bundle {}", bundle.display())))?;
    // Everything a bundle can get wrong, before anything exists.
    let loaded = load_bundle_for_caller(&bundle)?;
    console_matches(loaded.terminal.is_some(), console.is_some())?;
    // The container gets a cgroup below this process's part of the tree
    // (root's, or the user's delegated subtree). Without a PID namespace,
    // it's what finds and kills every process of the container, so then
    // it's needed.
    let tree = cgroup::Tree::find();
    if let Err(why) = &tree {
        without_cgroup(&loaded.isolation, &why.to_string())?;
    }
    let hooks = (!loaded.hooks.is_empty())
        .then(|| ContainerHooks::new(loaded.hooks, id, &bundle, loaded.annotations.clone()));
    let (annotations, mut warnings) = (loaded.annotations, loaded.warnings);
    if let (Err(why), Some(_)) = (&tree, &loaded.cgroups_path) {
        warnings.push(format!(
            "linux.cgroupsPath is ignored: the container can't have a cgroup of its own here: \
             {why}"
        ));
    }
    if let Err(why) = &tree {
        warnings.extend(cgroup::devices_without_cgroup(&loaded.isolation, why));
    }
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
    let dir_fd = match open_directory(&dir) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(err);
        }
    };
    let mut record = Record {
        bundle,
        monitor: None,
        init: None,
        annotations,
        cgroup: None,
        cgroup_base: None,
        hooks: hooks.clone(),
        cgroup_inode: None,
        cgroup_pending: false,
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
    match start_monitor(id, &dir_fd, record, console) {
        Ok(pid) => Ok(Created { pid, warnings }),
        Err(err) => {
            // As the spec has it: a create that fails is a container
            // destroyed, and `poststop` follows. Not if a `delete` got here
            // first (killing a setup that hung, say): it destroyed the
            // container, and ran them.
            let removed = remove_own(&dir, &dir_fd, cgroup.as_ref());
            let warnings = hooks
                .filter(|_| removed)
                .map(|hooks| hooks.run_all(Phase::Poststop, None))
                .unwrap_or_default();
            Err(with_warnings(err, &warnings))
        }
    }
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
    if let Ok(_lock) = lock_at(dir)
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

/// What the monitor tells `create`, as one JSON line.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
    Created(i32),
    Failed(String),
}

/// Fills the state directory open at `dir` in, starts the monitor and
/// waits for its answer.
///
/// Locked only while the directory is filled in: the setup can take a
/// while, and meanwhile `state` must say "creating", and `delete` must be
/// able to remove a container whose setup hangs (it kills the monitor
/// recorded here, and the container with it).
fn start_monitor(
    id: &str,
    dir: &OwnedFd,
    mut record: Record,
    console: Option<BorrowedFd<'_>>,
) -> Result<i32> {
    let (answers, mut monitor) = {
        let _lock = lock_at(dir).map_err(locking(id))?;
        nix::unistd::mkfifoat(dir, FIFO, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(errno_io("making the exec FIFO"))?;
        let (answers, mut monitor) = spawn_monitor(dir, &record.bundle, console)?;
        let pid = i32::try_from(monitor.id()).unwrap_or(-1);
        // Our child: its PID can't be reused before we reap it.
        record.monitor = start_time(pid).map(|started| (pid, started));
        if let Err(err) = write_record_at(dir, &record) {
            let _ = monitor.kill();
            let _ = monitor.wait();
            return Err(err);
        }
        (answers, monitor)
    };
    let answer = read_answer(&answers, &mut monitor);
    if !matches!(answer, Ok(Answer::Created(_))) {
        let _ = monitor.wait();
    }
    match answer? {
        Answer::Created(pid) => Ok(pid),
        Answer::Failed(message) => Err(ContainerError::Setup(message)),
    }
}

/// Starts the monitor for the state directory open at `dir`, with the
/// bundle at `bundle` (and the console socket `console`, for its terminal).
/// Returns the pipe its answer comes on.
fn spawn_monitor(
    dir: &OwnedFd,
    bundle: &Path,
    console: Option<BorrowedFd<'_>>,
) -> Result<(OwnedFd, std::process::Child)> {
    let (answers, answer_end) =
        nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(errno_io("making the monitor's pipe"))?;
    // Its ends must survive its exec: copies without close-on-exec, closed
    // as soon as it's spawned. (Another thread spawning meanwhile could
    // inherit them, which is why `read_answer` watches the monitor, not
    // end-of-file.)
    let inheritable = |fd: BorrowedFd<'_>| {
        nix::unistd::dup(fd).map_err(errno_io("passing the monitor its descriptors"))
    };
    let answer_end = inheritable(answer_end.as_fd())?;
    let dir = inheritable(dir.as_fd())?;
    let console = console.map(inheritable).transpose()?;
    let mut command = std::process::Command::new("/proc/self/exe");
    // A session of its own (and so a process group) from the start, before
    // it runs any code: the caller's terminal closing, or a Ctrl-C meant for
    // the caller, never reaches it. In the child, before the exec: `setsid`
    // fails for a process-group leader, so it can't come after
    // `process_group(0)`, nor in the monitor once something made it one.
    // SAFETY: the closure runs between `fork` and `exec`; it only calls
    // `setsid`, which is async-signal-safe, and reads `errno`.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
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
        .args(console.as_ref().map(|fd| fd.as_raw_fd().to_string()))
        .spawn()
        .map_err(io("starting the container's monitor"))?;
    Ok((answers, monitor))
}

/// Reads the monitor's answer: a line of JSON. Watches the monitor while
/// waiting, so a monitor that died without answering is noticed even if
/// its end of the pipe leaked elsewhere.
fn read_answer(answers: &OwnedFd, monitor: &mut std::process::Child) -> Result<Answer> {
    let mut line = Vec::new();
    loop {
        let mut fds = [PollFd::new(answers.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(100_u16)) {
            Ok(0) | Err(Errno::EINTR) => {
                if let Some(status) = monitor.try_wait().map_err(io("waiting for the monitor"))? {
                    return Err(ContainerError::Setup(format!(
                        "the container's monitor ended without answering ({status})"
                    )));
                }
                continue;
            }
            Ok(_) => {}
            Err(errno) => return Err(errno_io("waiting for the monitor")(errno)),
        }
        let mut buf = [0_u8; 4096];
        let read = nix::unistd::read(answers.as_fd(), &mut buf)
            .map_err(errno_io("reading the monitor's answer"))?;
        if read == 0 {
            return Err(ContainerError::Setup(
                "the container's monitor ended without answering".into(),
            ));
        }
        line.extend_from_slice(buf.get(..read).unwrap_or_default());
        if let Some(end) = line.iter().position(|&b| b == b'\n') {
            let text = line.get(..end).unwrap_or_default();
            return serde_json::from_slice(text).map_err(|err| {
                ContainerError::Setup(format!("the container's monitor answered nonsense: {err}"))
            });
        }
    }
}

// --- The monitor -------------------------------------------------------------

/// Where this binary starts as a container's monitor. Does nothing unless
/// started that way (by [`create`], with the hidden first argument);
/// otherwise it never returns. **Call it first thing in `main`**, next to
/// [`init_entry_point`](crate::process::init_entry_point).
pub fn monitor_entry_point() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).map(OsString::as_os_str) != Some(MONITOR_ARG.as_ref()) {
        return;
    }
    // As for the CLI itself: a privileged start (setuid, file capabilities)
    // gets no further. Anyone can start this entry point by hand.
    if let Err(err) = crate::process::refuse_privileged_start() {
        let _ = writeln!(std::io::stderr(), "shipyard-monitor: {err}");
        std::process::exit(125);
    }
    let fd = |at: usize| {
        args.get(at)
            .and_then(|fd| fd.to_str()?.parse::<RawFd>().ok())
            .filter(|&fd| fd > 2)
    };
    // The console socket, the fifth, is there when the bundle asks for a
    // terminal.
    let console = match args.get(5) {
        None => Some(None),
        Some(_) => fd(5).map(Some),
    };
    if let ([_, _, bundle, ..], Some(dir), Some(ready), Some(console)) =
        (args.as_slice(), fd(3), fd(4), console)
        && dir != ready
        && console.is_none_or(|console| console != dir && console != ready)
        && args.len() <= 6
    {
        // SAFETY: all passed by `create` across the exec, and distinct; this
        // process owns them.
        let (dir, ready, console) = unsafe {
            (
                OwnedFd::from_raw_fd(dir),
                OwnedFd::from_raw_fd(ready),
                console.map(|fd| OwnedFd::from_raw_fd(fd)),
            )
        };
        monitor(&dir, Path::new(bundle), ready, console);
    }
    let _ = std::io::stderr()
        .write_all(b"shipyard: this is a container monitor's internal entry point\n");
    std::process::exit(125);
}

/// Spawns the container, answers `create`, then waits for the container to
/// end and records how.
///
/// The state directory is `dir`, open since `create` made it, never a
/// path: once `delete` has removed it, a path could name a newer
/// container's directory (same ID), and this one's records would land
/// there. Through the descriptor, writing into a removed directory fails.
fn monitor(dir: &OwnedFd, bundle: &Path, ready: OwnedFd, console: Option<OwnedFd>) -> ! {
    // (A session of its own already: `spawn_monitor` made it one.)
    // Whatever the caller left open (not close-on-exec) would be held for
    // the container's whole life: a pipe whose reader waits for end-of-file,
    // say. The container gets the caller's stdio; the monitor keeps nothing
    // else but the directory, the answer pipe and the console socket.
    let mut keep = vec![dir.as_raw_fd(), ready.as_raw_fd()];
    keep.extend(console.as_ref().map(AsRawFd::as_raw_fd));
    close_inherited(&keep);
    // An ignored SIGCHLD survives `exec`: a caller that ignores it (`trap ''
    // CHLD`) would pass it on, and the kernel would then reap the
    // container's init (and the map helpers) itself, leaving nothing to
    // wait for: how the container ended would be lost (TLPI §26.3.3). The
    // container's own processes get the default back anyway (`child.rs`).
    // SAFETY: installing SIG_DFL runs none of our code in a signal context.
    let _ = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGCHLD,
            nix::sys::signal::SigHandler::SigDfl,
        )
    };
    // None is the container's or its hooks'. (The init gets the console
    // socket its own way: the spawn passes it on.)
    for fd in [
        Some(dir.as_fd()),
        Some(ready.as_fd()),
        console.as_ref().map(AsFd::as_fd),
    ]
    .into_iter()
    .flatten()
    {
        let _ = nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
    }
    let answer = |answer: &Answer| {
        let mut line = serde_json::to_vec(answer).unwrap_or_default();
        line.push(b'\n');
        // One write: well under the pipe's buffer, so it lands whole.
        let _ = nix::unistd::write(&ready, &line);
    };
    // `create` wrote it while holding the lock, so it's there once the lock
    // is free. Without it, or with a cgroup that doesn't check out, the
    // container would run without its cgroup (and limits) or its hooks:
    // better not at all.
    let (cgroup, hooks) = match container_setup(dir) {
        Ok(setup) => setup,
        Err(err) => {
            answer(&Answer::Failed(format!(
                "reading the container's state: {err}"
            )));
            std::process::exit(1);
        }
    };
    // The cgroup goes with the container: the monitor removes it, killing
    // whatever is left.
    let remove_cgroup = || {
        if let Some(cgroup) = &cgroup {
            remove_cgroup_at(dir, cgroup);
        }
    };
    let (mut child, pid) = match spawn_container(dir, bundle, cgroup.clone(), hooks, console) {
        Ok(created) => created,
        Err(message) => {
            remove_cgroup();
            answer(&Answer::Failed(message));
            std::process::exit(1);
        }
    };
    if let Err(err) = record_init(dir, pid) {
        child.kill_all();
        let _ = child.reap();
        remove_cgroup();
        answer(&Answer::Failed(err.to_string()));
        std::process::exit(1);
    }
    answer(&Answer::Created(pid));
    drop(ready);
    // The container holds the caller's stdio now; the monitor lets go of
    // it, so a reader sees end-of-file when the container closes it.
    null_stdio();
    let status = child.reap();
    // Read before the cgroup goes: whether the OOM killer struck, and the
    // SIGKILL wasn't someone's `kill`.
    let killed =
        nix::sys::stat::fstatat(dir, KILLED_FILE, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok();
    let oom = cgroup
        .as_ref()
        .map(cgroup::Cgroup::oom_events)
        .filter(|events| !killed && events.kills > 0);
    // Before "stopped" is recorded: a stopped container has nothing left.
    remove_cgroup();
    let ended = match status {
        Ok(status)
            if status.killed_by_sigkill()
                && let Some(oom) = oom =>
        {
            format!("{} ({})", describe_exit(status), oom.cause())
        }
        Ok(status) => describe_exit(status),
        Err(err) => format!("unknown ({err})"),
    };
    if let Ok(_lock) = lock_at(dir) {
        let _ = write_atomically_at(dir, EXIT_FILE, ended.as_bytes());
    }
    std::process::exit(0);
}

/// What the monitor needs from `state.json` in the state directory open at
/// `dir`: the container's cgroup, if it has one, and its hooks.
fn container_setup(dir: &OwnedFd) -> Result<(Option<cgroup::Cgroup>, Option<ContainerHooks>)> {
    let _lock = lock_at(dir).map_err(errno_io("locking the state directory"))?;
    let record = read_record_at(dir)?
        .ok_or_else(|| ContainerError::Setup("the container was deleted".into()))?;
    let cgroup = record.container_cgroup().transpose()?;
    Ok((cgroup, record.hooks))
}

/// As runc: a terminal (the bundle's `process.terminal`) goes to a console
/// socket, and a console socket is for a terminal.
fn console_matches(terminal: bool, console: bool) -> Result<()> {
    match (terminal, console) {
        (true, false) => Err(ContainerError::TerminalWithoutConsoleSocket),
        (false, true) => Err(ContainerError::ConsoleSocketWithoutTerminal),
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

/// Spawns the container held at the start gate (running `create`'s hooks,
/// as `hooks` lists them, on the way), and waits until it's set up. A
/// failure is the message for `create`.
///
/// With a terminal, its init sends the master to `console` once it has made
/// the pseudo-terminal; this process's copy of the socket is closed once the
/// spawn has passed it on, so the receiver sees end-of-file when the init
/// closes its own.
fn spawn_container(
    dir: &OwnedFd,
    bundle: &Path,
    cgroup: Option<cgroup::Cgroup>,
    hooks: Option<ContainerHooks>,
    console: Option<OwnedFd>,
) -> std::result::Result<(crate::process::spawn::Child, i32), String> {
    // Read again, it may have changed since `create` checked it.
    let mut bundle = load_bundle_for_caller(bundle).map_err(|err| err.to_string())?;
    console_matches(bundle.terminal.is_some(), console.is_some()).map_err(|err| err.to_string())?;
    let terminal = bundle
        .terminal
        .zip(console.as_ref())
        .map(|(terminal, console)| ChildTerminal::Socket {
            fd: console.as_raw_fd(),
            size: terminal.size.unwrap_or(DEFAULT_CONSOLE_SIZE),
        });
    if cgroup.is_none() {
        without_cgroup(&bundle.isolation, "none could be made when it was created")
            .map_err(|err| err.to_string())?;
    }
    bundle.isolation.hooks = hooks;
    let gate = nix::fcntl::openat(
        dir,
        FIFO,
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| format!("opening the exec FIFO: {}", errno.desc()))?;
    let options = SpawnOptions {
        new_process_group: false,
        keep_ignored: Vec::new(),
        placement: Placement::New(Box::new(bundle.isolation)),
        gate: Some(gate.as_raw_fd()),
        cgroup,
        // The OCI runtime's: the container keeps the caller's stdio, or
        // takes its terminal's.
        stdio: [None; 3],
        terminal,
        takes_terminal: terminal.is_some(),
    };
    let mut child =
        spawn(&bundle.spec, &options, None, |_| {}).map_err(|err| format!("{err:#}"))?;
    drop(gate);
    drop(console);
    // The report pipe's end-of-file means "set up, at the gate" — or that
    // the init died, which also closes it.
    let created = wait_for_report(&mut child);
    let pid = child.pid().as_raw();
    match created {
        Ok(()) if !exited(pid) => Ok((child, pid)),
        Ok(()) => {
            let status = child
                .reap()
                .map_or_else(|err| err.to_string(), describe_exit);
            Err(format!("the container ended while being set up ({status})"))
        }
        Err(err) => {
            let _ = child.reap();
            Err(err.to_string())
        }
    }
}

/// Waits (blocking) until the report pipe resolves. No deadline, as runc's
/// `create` has none: a setup hung in the kernel (a mount on a dead NFS
/// server) hangs `create` too, and `delete --force` (or a signal) ends it;
/// one that fails reports at once, and a dead init closes the pipe.
fn wait_for_report(child: &mut crate::process::spawn::Child) -> crate::Result<()> {
    loop {
        child.check_exec()?;
        let Some(report) = child.report_fd() else {
            return Ok(());
        };
        let mut fds = [PollFd::new(report, PollFlags::POLLIN)];
        let _ = poll(&mut fds, PollTimeout::NONE);
    }
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

/// Records the init in `state.json`: its PID and start time. Fails if the
/// container was deleted meanwhile.
fn record_init(dir: &OwnedFd, pid: i32) -> Result<()> {
    let _lock = lock_at(dir).map_err(errno_io("locking the state directory"))?;
    let mut record = read_record_at(dir)?
        .ok_or_else(|| ContainerError::Setup("the container was deleted".into()))?;
    let started = start_time(pid)
        .ok_or_else(|| ContainerError::Setup("the container's init vanished".into()))?;
    record.init = Some((pid, started));
    write_record_at(dir, &record)
}

fn describe_exit(status: ExitStatus) -> String {
    format!("{status} (exit code {})", status.code())
}

fn null_stdio() {
    if let Ok(null) = nix::fcntl::open("/dev/null", OFlag::O_RDWR, Mode::empty()) {
        for fd in 0..=2 {
            // SAFETY: `dup2` onto the standard descriptors, which nothing in
            // this process uses any more.
            unsafe { libc::dup2(null.as_raw_fd(), fd) };
        }
    }
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
    // Closed at once: the init goes on only once no reader is left (see
    // `wait_at_gate`), and `poststart` below must not hold the program back.
    drop(fifo);
    // It's started: from here on, problems are only warnings.
    let mut warnings = Vec::new();
    if let Err(err) = std::fs::remove_file(dir.join(FIFO)) {
        warnings.push(format!("removing the exec FIFO: {err}"));
    }
    // Not under the lock: a hook may well ask for the container's state.
    drop(lock);
    if let Some((hooks, pid)) = record.and_then(|record| Some((record.hooks?, record.init?.0))) {
        warnings.extend(hooks.run_all(Phase::Poststart, Some(pid)));
    }
    Ok(warnings)
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
        status,
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
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !LIVE.contains(&status) {
        return Err(wrong(id, status, "kill", LIVE_WANTED));
    }
    let (pid, started) = read_record(&dir)?
        .and_then(|r| r.init)
        .ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    mark_if_killing(&dir, signal.as_raw());
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
    with_cgroup(root, id, "kill", &LIVE, LIVE_WANTED, |cgroup| {
        mark_if_killing(&root.join(id), signal.as_raw());
        cgroup.signal_all(signal.as_raw());
        Ok(())
    })
}

/// Leaves [`KILLED_FILE`] in the state directory `dir` before `signal` is
/// sent, if it's SIGKILL. Best effort: without it, an OOM kill earlier in
/// the container's life is all a later SIGKILL could be blamed on.
fn mark_if_killing(dir: &Path, signal: i32) {
    if signal == libc::SIGKILL {
        let _ = std::fs::write(dir.join(KILLED_FILE), "");
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
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !allowed.contains(&status) {
        return Err(wrong(id, status, action, wanted));
    }
    act(&cgroup_of(id, read_record(&dir)?.as_ref())?)
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
    // An init that's gone is stopped enough here, its monitor still cleaning
    // up or not: this removes the cgroup itself, under the lock the monitor
    // waits for (and whose directory, once gone, it leaves alone).
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
                mark_if_killing(&dir, libc::SIGKILL);
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
        Status::Stopped => {}
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
    // Not under the lock (a hook may ask for the state: it's gone now).
    drop(lock);
    Ok(record
        .and_then(|record| record.hooks)
        .map(|hooks| hooks.run_all(Phase::Poststop, None))
        .unwrap_or_default())
}

// --- The state directory -------------------------------------------------------

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
    let deadline = Instant::now() + LOCK_WAIT;
    let mut fd = nix::fcntl::openat(
        dir,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    loop {
        match Flock::lock(fd, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => return Ok(locked),
            Err((unlocked, Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                fd = unlocked;
                std::thread::sleep(POLL_INTERVAL);
            }
            Err((_, errno)) => return Err(errno),
        }
    }
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
    let monitor_alive = record
        .monitor
        .is_some_and(|(monitor, started)| is_alive(monitor, started));
    if dir.join(EXIT_FILE).exists() || !(is_alive(pid, started) || monitor_alive) {
        return Ok(Status::Stopped);
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
    match std::fs::read(dir.join(STATE_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|err| {
            ContainerError::Setup(format!("{}: {err}", dir.join(STATE_FILE).display()))
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io(format!("reading {}", dir.join(STATE_FILE).display()))(
            err,
        )),
    }
}

/// `state.json` in the directory open at `dir` (`None` if it's missing).
fn read_record_at(dir: &OwnedFd) -> Result<Option<Record>> {
    let file = match nix::fcntl::openat(
        dir,
        STATE_FILE,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => std::fs::File::from(fd),
        Err(Errno::ENOENT) => return Ok(None),
        Err(errno) => return Err(errno_io(format!("reading {STATE_FILE}"))(errno)),
    };
    serde_json::from_reader(std::io::BufReader::new(file))
        .map(Some)
        .map_err(|err| ContainerError::Setup(format!("{STATE_FILE}: {err}")))
}

fn write_record_at(dir: &OwnedFd, record: &Record) -> Result<()> {
    let json = serde_json::to_vec(record).map_err(|err| ContainerError::Setup(err.to_string()))?;
    write_atomically_at(dir, STATE_FILE, &json).map_err(errno_io(format!("writing {STATE_FILE}")))
}

/// Writes `name` in the directory open at `dir` whole or not at all: a
/// temporary file, then `rename`. In a removed directory, ENOENT.
fn write_atomically_at(dir: &OwnedFd, name: &str, bytes: &[u8]) -> nix::Result<()> {
    let temporary = format!("{name}.tmp");
    let fd = nix::fcntl::openat(
        dir,
        temporary.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?;
    std::fs::File::from(fd)
        .write_all(bytes)
        .map_err(|err| crate::process::io_errno(&err))?;
    nix::fcntl::renameat(dir, temporary.as_str(), dir, name)
}

/// A process's start time (`/proc/PID/stat` field 22), if it exists and
/// hasn't exited (a zombie counts as gone).
fn start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat)
}

/// The start time in a `/proc/PID/stat` line, unless the process is a
/// zombie. The command name (field 2) can hold anything, spaces and `)`
/// included, so the fields after it are counted from its last `)`.
fn parse_stat(stat: &[u8]) -> Option<u64> {
    let after = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(stat.get(after + 1..)?).ok()?;
    let mut fields = rest.split_whitespace();
    // Fields 3 (the state) to 22 (the start time).
    let state = fields.next()?;
    if state == "Z" || state == "X" {
        return None;
    }
    fields.nth(18)?.parse().ok()
}

/// Whether `pid` is still the process (an init or a monitor) recorded with
/// start time `started`: not gone, nor its PID reused.
fn is_alive(pid: i32, started: u64) -> bool {
    start_time(pid) == Some(started)
}

/// Sends `signal` to process `pid`, if it's still the one that started at
/// `started` (an init or a monitor). Through a pidfd where
/// the kernel has them (5.3): once open, it names that process even if it
/// dies and its PID is reused, so the check and the signal can't be split
/// by a reuse. Without pidfds, a plain `kill` after the check.
fn signal_process(pid: i32, started: u64, signal: libc::c_int) -> nix::Result<()> {
    // SAFETY: `pidfd_open` takes a PID and flags, and returns a new
    // descriptor (or -1).
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let pidfd = libc::c_int::try_from(pidfd).unwrap_or(-1);
    if pidfd < 0 {
        if !is_alive(pid, started) {
            return Err(Errno::ESRCH);
        }
        return nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::try_from(signal)?,
        );
    }
    // SAFETY: `pidfd_open` just returned it; nothing else owns it.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    if !is_alive(pid, started) {
        return Err(Errno::ESRCH);
    }
    // SAFETY: a valid pidfd, a signal number, no siginfo, no flags.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    Errno::result(sent).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_time_is_found_after_any_command_name() {
        // Fields 3 (the state) to 21, then the start time, then more.
        let fields_after = std::iter::once("S".to_owned())
            .chain((4..=21).map(|n| n.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        let line = format!("42 (a ) b (c) {fields_after} 777 23\n");
        assert_eq!(parse_stat(line.as_bytes()), Some(777));
        let zombie = line.replacen(") S", ") Z", 1);
        assert_eq!(parse_stat(zombie.as_bytes()), None, "a zombie is gone");
        assert_eq!(parse_stat(b"garbage"), None);
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
            monitor: None,
            init,
            annotations: BTreeMap::new(),
            cgroup: None,
            cgroup_base: None,
            hooks: None,
            cgroup_inode: None,
            cgroup_pending: false,
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
            Status::Running,
            "an init gone, its monitor still cleaning up"
        );
        write_record(&dir, &record(Some((me, start_time(me).unwrap())))).unwrap();
        std::fs::write(dir.join(EXIT_FILE), "exited").unwrap();
        assert_eq!(status_of(&dir).unwrap(), Status::Stopped);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn records_keep_their_hooks_and_older_ones_have_none() {
        let older: Record =
            serde_json::from_str(r#"{"bundle": "/b", "init": null, "annotations": {}}"#).unwrap();
        assert!(older.hooks.is_none());
        let mut hooks = crate::hooks::Hooks::default();
        hooks.poststop.push(crate::hooks::Hook::new("/bin/cleanup"));
        let record = Record {
            hooks: Some(ContainerHooks::new(hooks, "c1", "/b", BTreeMap::new())),
            ..older
        };
        let back: Record = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(back.hooks, record.hooks);
    }
}
