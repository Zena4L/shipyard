//! The lifecycle on Linux (see the module above): the state directory, the
//! monitor, and the five calls.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd as _, OwnedFd, RawFd};
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
use crate::isolation::Namespace;
use crate::oci::load_bundle_for_caller;
use crate::process::spawn::{Placement, SpawnOptions, spawn};
use crate::process::{ExitStatus, Signal};

/// The monitor's hidden first argument (see [`monitor_entry_point`]).
const MONITOR_ARG: &str = "__shipyard-monitor";

const STATE_FILE: &str = "state.json";
const FIFO: &str = "exec.fifo";
const EXIT_FILE: &str = "exit";

/// How long `start` waits for the init's answer at the gate, and `delete`
/// for a killed container to stop.
const WAIT: Duration = Duration::from_secs(5);

/// How often `delete` looks whether a killed container has stopped, and a
/// call whether the lock is free.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a call waits for the lock: longer than any call holds it
/// (`delete` waits up to [`WAIT`] with it).
const LOCK_WAIT: Duration = Duration::from_secs(10);

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
}

// --- create -------------------------------------------------------------------

/// Creates container `id` from the bundle in `bundle`, under `root`, and
/// returns its init's PID (and the bundle's warnings) once it's set up and
/// waiting for [`start`]. The container's standard input, output and error
/// are the caller's.
///
/// Starts a monitor process (this program again: its `main` must call
/// [`monitor_entry_point`] first), which stays the container's parent. A
/// long-lived caller should reap it once it exits.
///
/// # Errors
///
/// A bad ID, a taken one, a bundle that can't be run (or has no PID
/// namespace), or a setup failure (with its message).
///
/// Without root, the container is rootless: a bundle without a user
/// namespace gets one (with a warning), as `shipyard run --bundle` does.
pub fn create(root: &Path, id: &str, bundle: &Path) -> Result<Created> {
    validate_id(id)?;
    let bundle =
        std::fs::canonicalize(bundle).map_err(io(format!("the bundle {}", bundle.display())))?;
    // Everything a bundle can get wrong, before anything exists.
    let loaded = load_bundle_for_caller(&bundle)?;
    if !loaded.isolation.namespaces.contains(&Namespace::Pid) {
        return Err(ContainerError::NoPidNamespace);
    }
    let (annotations, warnings) = (loaded.annotations, loaded.warnings);
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
    let record = Record {
        bundle,
        monitor: None,
        init: None,
        annotations,
    };
    let dir_fd = match open_directory(&dir) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(err);
        }
    };
    let result = start_monitor(id, &dir_fd, record);
    if result.is_err() {
        remove_own(&dir, &dir_fd);
    }
    result.map(|pid| Created { pid, warnings })
}

/// Removes the state directory at `dir` if it's still the one open at
/// `fd`: a `delete` may have removed it meanwhile, and a `create` made
/// another container's at the same path.
fn remove_own(dir: &Path, fd: &OwnedFd) {
    if let Ok(_lock) = lock_at(fd)
        && same_directory(fd, dir)
    {
        let _ = std::fs::remove_dir_all(dir);
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
fn start_monitor(id: &str, dir: &OwnedFd, mut record: Record) -> Result<i32> {
    let (answers, mut monitor) = {
        let _lock = lock_at(dir).map_err(locking(id))?;
        nix::unistd::mkfifoat(dir, FIFO, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(errno_io("making the exec FIFO"))?;
        let (answers, mut monitor) = spawn_monitor(dir, &record.bundle)?;
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
/// bundle at `bundle`. Returns the pipe its answer comes on.
fn spawn_monitor(dir: &OwnedFd, bundle: &Path) -> Result<(OwnedFd, std::process::Child)> {
    let (answers, answer_end) =
        nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(errno_io("making the monitor's pipe"))?;
    // Its ends must survive its exec: copies without close-on-exec, closed
    // as soon as it's spawned. (Another thread spawning meanwhile could
    // inherit them, which is why `read_answer` watches the monitor, not
    // end-of-file.)
    let inheritable = |fd: &OwnedFd| {
        nix::unistd::dup(fd).map_err(errno_io("passing the monitor its descriptors"))
    };
    let answer_end = inheritable(&answer_end)?;
    let dir = inheritable(dir)?;
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
    let fd = |at: usize| {
        args.get(at)
            .and_then(|fd| fd.to_str()?.parse::<RawFd>().ok())
            .filter(|&fd| fd > 2)
    };
    if let ([_, _, bundle, ..], Some(dir), Some(ready)) = (args.as_slice(), fd(3), fd(4))
        && dir != ready
    {
        // SAFETY: both passed by `create` across the exec; this process
        // owns them.
        let (dir, ready) = unsafe { (OwnedFd::from_raw_fd(dir), OwnedFd::from_raw_fd(ready)) };
        monitor(&dir, Path::new(bundle), ready);
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
fn monitor(dir: &OwnedFd, bundle: &Path, ready: OwnedFd) -> ! {
    // (A session of its own already: `spawn_monitor` made it one.)
    // Whatever the caller left open (not close-on-exec) would be held for
    // the container's whole life: a pipe whose reader waits for end-of-file,
    // say. The container gets the caller's stdio; the monitor keeps nothing
    // else but the directory and the answer pipe.
    close_inherited(&[dir.as_raw_fd(), ready.as_raw_fd()]);
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
    let _ = nix::fcntl::fcntl(
        dir,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    );
    let answer = |answer: &Answer| {
        let mut line = serde_json::to_vec(answer).unwrap_or_default();
        line.push(b'\n');
        // One write: well under the pipe's buffer, so it lands whole.
        let _ = nix::unistd::write(&ready, &line);
    };
    let (mut child, pid) = match spawn_container(dir, bundle) {
        Ok(created) => created,
        Err(message) => {
            answer(&Answer::Failed(message));
            std::process::exit(1);
        }
    };
    if let Err(err) = record_init(dir, pid) {
        child.kill_all();
        let _ = child.reap();
        answer(&Answer::Failed(err.to_string()));
        std::process::exit(1);
    }
    answer(&Answer::Created(pid));
    drop(ready);
    // The container holds the caller's stdio now; the monitor lets go of
    // it, so a reader sees end-of-file when the container closes it.
    null_stdio();
    let status = child.reap();
    let ended = match status {
        Ok(status) => describe_exit(status),
        Err(err) => format!("unknown ({err})"),
    };
    if let Ok(_lock) = lock_at(dir) {
        let _ = write_atomically_at(dir, EXIT_FILE, ended.as_bytes());
    }
    std::process::exit(0);
}

/// Spawns the container held at the start gate, and waits until it's set
/// up. A failure is the message for `create`.
fn spawn_container(
    dir: &OwnedFd,
    bundle: &Path,
) -> std::result::Result<(crate::process::spawn::Child, i32), String> {
    let bundle = load_bundle_for_caller(bundle).map_err(|err| err.to_string())?;
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
    };
    let mut child = spawn(&bundle.spec, &options, |_| {}).map_err(|err| format!("{err:#}"))?;
    drop(gate);
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

/// Waits (blocking) until the report pipe resolves.
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

/// Lets container `id`, created and waiting, run its program.
///
/// # Errors
///
/// No such container, one that isn't created (already started, or
/// stopped), or an init that doesn't answer at the gate.
pub fn start(root: &Path, id: &str) -> Result<()> {
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if status != Status::Created {
        return Err(wrong(id, status, "start", "created"));
    }
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
    std::fs::remove_file(dir.join(FIFO)).map_err(io("removing the exec FIFO"))
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
    let live = matches!(status, Status::Created | Status::Running);
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
/// SIGKILL wait until it starts.
///
/// # Errors
///
/// No such container, one that isn't created or running, or SIGSTOP (see
/// [`ContainerError::StopSignal`]).
pub fn kill(root: &Path, id: &str, signal: Signal) -> Result<()> {
    if signal.as_raw() == libc::SIGSTOP {
        return Err(ContainerError::StopSignal(id.to_owned()));
    }
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !matches!(status, Status::Created | Status::Running) {
        return Err(wrong(id, status, "kill", "created or running"));
    }
    let (pid, started) = read_record(&dir)?
        .and_then(|r| r.init)
        .ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    signal_process(pid, started, signal.as_raw())
        .map_err(errno_io(format!("signalling container {id}")))
}

/// Removes container `id`. A created one is killed first (as runc does); a
/// running one only with `force`.
///
/// # Errors
///
/// No such container, a running one without `force`, or one that won't
/// stop.
pub fn delete(root: &Path, id: &str, force: bool) -> Result<()> {
    let dir = existing(root, id)?;
    let _lock = lock(&dir)?;
    let status = status_of(&dir)?;
    match status {
        Status::Running if !force => {
            return Err(wrong(id, status, "delete", "stopped (or --force)"));
        }
        Status::Created | Status::Running => {
            if let Some((pid, started)) = read_record(&dir)?.and_then(|r| r.init) {
                let _ = signal_process(pid, started, libc::SIGKILL);
            }
            let deadline = Instant::now() + WAIT;
            while status_of(&dir)? != Status::Stopped {
                if Instant::now() > deadline {
                    return Err(ContainerError::Setup(format!("container {id} didn't stop")));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        Status::Creating => {
            // A setup that hangs never answers: stop it through its monitor
            // (the init dies with it, by its parent-death signal).
            if let Some((pid, started)) = read_record(&dir)?.and_then(|r| r.monitor) {
                let _ = signal_process(pid, started, libc::SIGKILL);
            }
        }
        Status::Stopped => {}
    }
    std::fs::remove_dir_all(&dir).map_err(io(format!("removing {}", dir.display())))
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
/// init gone or replaced), created while the gate is there, running after.
fn status_of(dir: &Path) -> Result<Status> {
    let Some(Record {
        init: Some((pid, started)),
        ..
    }) = read_record(dir)?
    else {
        return Ok(Status::Creating);
    };
    if dir.join(EXIT_FILE).exists() || !is_init(pid, started) {
        return Ok(Status::Stopped);
    }
    Ok(if dir.join(FIFO).exists() {
        Status::Created
    } else {
        Status::Running
    })
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

/// Whether `pid` is still the init recorded with start time `started`.
fn is_init(pid: i32, started: u64) -> bool {
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
        if !is_init(pid, started) {
            return Err(Errno::ESRCH);
        }
        return nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::try_from(signal)?,
        );
    }
    // SAFETY: `pidfd_open` just returned it; nothing else owns it.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    if !is_init(pid, started) {
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
        assert!(is_init(me, started));
        assert!(
            !is_init(me, started + 1),
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
}
