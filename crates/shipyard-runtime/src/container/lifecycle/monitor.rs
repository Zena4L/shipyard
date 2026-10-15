//! The container's monitor: this binary again, started by `create`, the
//! container's parent for its whole life (Podman's conmon). It spawns the
//! container, answers `create`, keeps its output in logs when asked
//! (`--log-dir`, [`super::capture`]), and records how it ended.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd as _, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;

use super::capture::Capture;
use super::{
    Answer, ContainerError, EXIT_COMMAND_FAILED, EXIT_FILE, ExitRecord, FIFO, GATE_OPENED,
    KILLED_FILE, MONITOR_ARG, POLL_INTERVAL, RecordedLogs, Result, STARTED, console_matches,
    errno_io, lock_at, lock_at_with, mark_if_killing, read_record_at, remove_cgroup_at,
    remove_cgroup_at_with, start_time, without_cgroup, write_atomically_at, write_record_at,
};
use crate::cgroup;
use crate::container::{EXIT_RECORD_ENV, ExitCommand, HOLD_LABELS_ENV, HOLDS_ENV, MONITOR_PID_ENV};
use crate::hooks::ContainerHooks;
use crate::oci::load_bundle_for_caller;
use crate::process::ExitStatus;
use crate::process::spawn::{Child, Placement, SpawnOptions, spawn};
use crate::process::stdio::ChildTerminal;

/// After the container's init has ended, how long its output may take to
/// end (something escaped, without a PID namespace, may hold it): the
/// supervisor's figure.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// The optional descriptors `create` passes, after the fixed ones: tagged,
/// since either may be there without the other.
const CONSOLE_TAG: &str = "console=";
const LOGS_TAG: &str = "logs=";
/// Descriptors for the container's process, and socket activation's part:
/// JSON (`passed.rs`'s `PassedPlan`), their numbers the monitor's.
const PASSING_TAG: &str = "passing=";
/// Descriptors the monitor holds until the container has ended, for its
/// exit command (`create --hold-fd`): `N:LABEL` each, comma-separated
/// (labels have no commas).
const HOLDS_TAG: &str = "holds=";

/// Where this binary starts as a container's monitor. Does nothing unless
/// started that way (by [`create`](super::create), with the hidden first
/// argument); otherwise it never returns. **Call it first thing in
/// `main`**, next to [`init_entry_point`](crate::process::init_entry_point).
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
    if let Some(given) = parse_arguments(&args) {
        // Forked once more, at once (single-threaded still): this first
        // process ends (`create` reaps it), and the monitor, its child,
        // adopted by init or a subreaper, is no caller's child: none keeps a
        // zombie when it ends. The monitor itself says who it is, first,
        // before any other answer: one writer, so they come in order.
        // SAFETY: single-threaded (this entry point runs first thing in
        // `main`); the parent only `_exit`s.
        match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Parent { .. }) => {
                // SAFETY: `_exit` ends the process at once (no destructors:
                // the child owns what was passed now).
                unsafe { libc::_exit(0) }
            }
            Ok(nix::unistd::ForkResult::Child) => {
                // A session (and process group) of its own again, as the
                // first process had: `delete` stops a setup by the monitor's
                // group, and no terminal's signals reach it. (Not a group
                // leader, so `setsid` can't fail with EPERM.)
                let _ = nix::unistd::setsid();
                // Its PID with its start time, read by itself (its own, so
                // never a stranger's: `create` couldn't read it safely, the
                // monitor no longer its child, its PID not pinned).
                let pid = nix::unistd::getpid().as_raw();
                let started = super::start_time(pid);
                let answer = match started {
                    Some(started) => super::Answer::Monitor((pid, started)),
                    None => super::Answer::Failed(
                        "the container's monitor couldn't read its start time".to_owned(),
                    ),
                };
                let line = serde_json::to_vec(&answer)
                    .map(|mut line| {
                        line.push(b'\n');
                        line
                    })
                    .unwrap_or_default();
                // SAFETY: `ready` was passed open by `create` (checked).
                let ready = unsafe { std::os::fd::BorrowedFd::borrow_raw(given.ready) };
                let _ = nix::unistd::write(ready, &line);
                // Said it failed: `create` now removes the container, so
                // nothing is left to monitor.
                if started.is_none() {
                    std::process::exit(125);
                }
            }
            Err(errno) => {
                let _ = writeln!(std::io::stderr(), "shipyard-monitor: forking: {errno}");
                std::process::exit(125);
            }
        }
        // SAFETY: all passed by `create` across the exec, and distinct
        // (checked); this process owns them.
        let owned = |fd: RawFd| unsafe { OwnedFd::from_raw_fd(fd) };
        monitor(
            &owned(given.dir),
            Path::new(&given.bundle),
            owned(given.ready),
            given.console.map(owned),
            given.logs.map(owned),
            given.passing,
            super::holds::Holds::new(
                given
                    .holds
                    .into_iter()
                    .map(|(fd, label)| (owned(fd), label))
                    .collect(),
            ),
        );
    }
    let _ = std::io::stderr()
        .write_all(b"shipyard: this is a container monitor's internal entry point\n");
    std::process::exit(125);
}

/// What `create` passed: `MONITOR_ARG BUNDLE DIR READY [console=N] [logs=N]
/// [passing=JSON] [holds=N:LABEL,…]`.
#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    bundle: OsString,
    dir: RawFd,
    ready: RawFd,
    console: Option<RawFd>,
    logs: Option<RawFd>,
    passing: Option<crate::process::passed::PassedPlan>,
    holds: Vec<(RawFd, String)>,
}

/// Reads the arguments, if they're what `create` passes: every descriptor
/// above 2 and distinct, each tag at most once, nothing else.
fn parse_arguments(args: &[OsString]) -> Option<Arguments> {
    let fd = |text: &str| text.parse::<RawFd>().ok().filter(|&fd| fd > 2);
    let [_, _, bundle, dir, ready, rest @ ..] = args else {
        return None;
    };
    let mut given = Arguments {
        bundle: bundle.clone(),
        dir: fd(dir.to_str()?)?,
        ready: fd(ready.to_str()?)?,
        console: None,
        logs: None,
        passing: None,
        holds: Vec::new(),
    };
    let mut holds_given = false;
    for arg in rest {
        let arg = arg.to_str()?;
        if let Some(list) = arg.strip_prefix(HOLDS_TAG) {
            if std::mem::replace(&mut holds_given, true) {
                return None;
            }
            given.holds = list
                .split(',')
                .map(|hold| {
                    let (number, label) = hold.split_once(':')?;
                    let fd = fd(number)?;
                    crate::attach::is_label(label).then(|| (fd, label.to_owned()))
                })
                .collect::<Option<_>>()?;
            continue;
        }
        if let Some(json) = arg.strip_prefix(PASSING_TAG) {
            let passing: crate::process::passed::PassedPlan = serde_json::from_str(json).ok()?;
            if passing.sources.iter().any(|&fd| fd <= 2) || given.passing.replace(passing).is_some()
            {
                return None;
            }
            continue;
        }
        let slot = if let Some(value) = arg.strip_prefix(CONSOLE_TAG) {
            (&mut given.console, value)
        } else {
            (&mut given.logs, arg.strip_prefix(LOGS_TAG)?)
        };
        if slot.0.replace(fd(slot.1)?).is_some() {
            return None;
        }
    }
    let mut all: Vec<RawFd> = [
        Some(given.dir),
        Some(given.ready),
        given.console,
        given.logs,
    ]
    .into_iter()
    .flatten()
    .chain(
        given
            .passing
            .iter()
            .flat_map(|passing| passing.sources.iter().copied()),
    )
    .chain(given.holds.iter().map(|&(fd, _)| fd))
    .collect();
    let count = all.len();
    all.sort_unstable();
    all.dedup();
    (all.len() == count).then_some(given)
}

/// The optional arguments for [`parse_arguments`].
pub(super) fn tagged(
    console: Option<RawFd>,
    logs: Option<RawFd>,
    passing: Option<&crate::process::passed::PassedPlan>,
    holds: &[(RawFd, &str)],
) -> Vec<String> {
    console
        .map(|fd| format!("{CONSOLE_TAG}{fd}"))
        .into_iter()
        .chain(logs.map(|fd| format!("{LOGS_TAG}{fd}")))
        .chain(
            passing
                .and_then(|passing| serde_json::to_string(passing).ok())
                .map(|json| format!("{PASSING_TAG}{json}")),
        )
        .chain((!holds.is_empty()).then(|| {
            let list: Vec<String> = holds
                .iter()
                .map(|(fd, label)| format!("{fd}:{label}"))
                .collect();
            format!("{HOLDS_TAG}{}", list.join(","))
        }))
        .collect()
}

/// Spawns the container, answers `create`, then waits for the container to
/// end (keeping its output in its logs meanwhile, if it has them) and
/// records how.
///
/// The state directory is `dir`, open since `create` made it, never a
/// path: once `delete` has removed it, a path could name a newer
/// container's directory (same ID), and this one's records would land
/// there. Through the descriptor, writing into a removed directory fails.
/// The same goes for the log directory, `logs`.
fn monitor(
    dir: &OwnedFd,
    bundle: &Path,
    ready: OwnedFd,
    console: Option<OwnedFd>,
    logs: Option<OwnedFd>,
    passing: Option<crate::process::passed::PassedPlan>,
    holds: super::holds::Holds,
) -> ! {
    // (A session of its own already: `monitor_entry_point`'s second
    // process made itself one.)
    // Whatever the caller left open (not close-on-exec) would be held for
    // the container's whole life: a pipe whose reader waits for end-of-file,
    // say. The monitor keeps nothing else but the descriptors it was given.
    let given: Vec<&OwnedFd> = [Some(dir), Some(&ready), console.as_ref(), logs.as_ref()]
        .into_iter()
        .flatten()
        .chain(holds.fds())
        .collect();
    let for_the_container = keep_only(&given, passing);
    // An ignored SIGCHLD survives `exec`: a caller that ignores it (`trap ''
    // CHLD`) would pass it on, and the kernel would then reap the
    // container's init (and the map helpers) itself, leaving nothing to
    // wait for: how the container ended would be lost (TLPI §26.3.3). The
    // container's own processes get the default back anyway (`child.rs`).
    // And SIGXFSZ ignored: a caller's file-size limit (`ulimit -f`) would
    // otherwise kill the monitor (and the container with it) at a log's
    // limit; a write then fails with EFBIG, and that record is lost.
    // SAFETY: installing SIG_DFL or SIG_IGN runs none of our code in a
    // signal context.
    let _ = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGCHLD,
            nix::sys::signal::SigHandler::SigDfl,
        )
    };
    // SAFETY: as above.
    let _ = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGXFSZ,
            nix::sys::signal::SigHandler::SigIgn,
        )
    };
    // None is the container's or its hooks', the holds above all. (The init
    // gets the console socket its own way: the spawn passes it on.)
    crate::process::close_on_exec(given.iter().map(|fd| fd.as_raw_fd()));
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
    let mut setup = match container_setup(dir) {
        Ok(setup) => setup,
        Err(err) => {
            answer(&Answer::Failed(format!(
                "reading the container's state: {err}"
            )));
            std::process::exit(1);
        }
    };
    let cgroup = setup.cgroup.clone();
    let exit_command = setup.exit_command.take();
    let companion = setup.companion.take();
    // The cgroup goes with the container: the monitor removes it, killing
    // whatever is left.
    let remove_cgroup = || {
        if let Some(cgroup) = &cgroup {
            remove_cgroup_at(dir, cgroup);
        }
    };
    let Spawned {
        mut child,
        pid,
        mut capture,
    } = match spawn_container(dir, bundle, setup, console, logs, for_the_container) {
        Ok(spawned) => spawned,
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
    // The container holds the caller's stdio now (or its logs' pipes); the
    // monitor lets go of the caller's, so a reader sees end-of-file when the
    // container closes it.
    null_stdio();
    // Beside the container (held at its gate) until its end is recorded.
    let companion = companion.as_ref().and_then(spawn_companion);
    let (status, kept) = wait_serving_holds(dir, pid, child, capture.as_mut(), companion, holds);
    // Read before the cgroup goes: whether the OOM killer struck, and the
    // SIGKILL wasn't someone's `kill`.
    let killed =
        nix::sys::stat::fstatat(dir, KILLED_FILE, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok();
    let oom = cgroup
        .as_ref()
        .map(cgroup::Cgroup::oom_events)
        .filter(|events| !killed && events.kills > 0);
    // Before "stopped" is recorded: a stopped container has nothing left.
    // Before the drain too: whatever escaped the init (without a PID
    // namespace) goes, and with it its hold on the output. Its clients are
    // served while the lock is waited for (F41).
    if let Some(cgroup) = &cgroup {
        remove_cgroup_serving(dir, cgroup, capture.as_mut());
    }
    if let Some(capture) = &mut capture {
        capture.drain(Instant::now() + DRAIN_WAIT);
    }
    // Started: `start` saw its init through the gate, and said so.
    let started =
        nix::sys::stat::fstatat(dir, GATE_OPENED, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok();
    let ended = ended(&status, oom, started);
    // Last: once it's there, the logs are complete (`logs_of`). Its
    // clients served meanwhile, as above.
    if let Ok(_lock) = lock_serving(dir, capture.as_mut()) {
        let _ = write_atomically_at(dir, EXIT_FILE, &ended.to_json());
    }
    // Then the attached clients: told once `state` says stopped.
    if let Some(capture) = &mut capture {
        // What `attach` ends with: the container's exit code (128 + a
        // signal), 125 if it's not known.
        capture.end(ended.code.unwrap_or(125));
    }
    // Then what cleans up after it, with what the caller left held (back
    // from the capture, which served them till now, if it did). Not waited
    // for: it may take its time, and the container has ended.
    let holds = match &mut capture {
        Some(capture) => capture.take_holds(),
        None => kept,
    };
    if let Some(command) = &exit_command {
        run_exit_command(command, dir, &holds, &ended);
    }
    std::process::exit(0);
}

/// [`wait_for_the_end`], the `holds` served meanwhile by `capture`'s
/// holders (added to, let go of), if it has a capture (an attach socket):
/// handed to it now, and none more taken from the moment the container
/// ends (a hold then could miss the exit command; a release still lets
/// go). They stay with the capture until the exit command
/// ([`Capture::take_holds`]); without a capture, they're given back.
fn wait_serving_holds(
    dir: &OwnedFd,
    pid: i32,
    child: Child,
    mut capture: Option<&mut Capture>,
    companion: Option<nix::unistd::Pid>,
    holds: super::holds::Holds,
) -> (crate::Result<ExitStatus>, super::holds::Holds) {
    let Some(serving) = capture.as_deref_mut() else {
        return (wait_for_the_end(dir, pid, child, None, companion), holds);
    };
    serving.serve_holds(holds);
    let status = wait_for_the_end(dir, pid, child, capture.as_deref_mut(), companion);
    if let Some(capture) = capture {
        capture.stop_taking_holds();
    }
    (status, super::holds::Holds::default())
}

/// Removes the container's `cgroup` ([`remove_cgroup_at`]), `capture` (if
/// any: its outputs and its clients) served while the lock is waited for
/// (F41).
fn remove_cgroup_serving(dir: &OwnedFd, cgroup: &cgroup::Cgroup, capture: Option<&mut Capture>) {
    match capture {
        Some(capture) => remove_cgroup_at_with(dir, cgroup, || serve_for_a_moment(capture, true)),
        None => remove_cgroup_at(dir, cgroup),
    }
}

/// The state directory's lock after the drain, `capture`'s clients (if
/// any) served while it's waited for (F41): its clients only, the logs
/// being complete.
fn lock_serving(
    dir: &OwnedFd,
    capture: Option<&mut Capture>,
) -> nix::Result<nix::fcntl::Flock<OwnedFd>> {
    match capture {
        Some(capture) => lock_at_with(dir, || serve_for_a_moment(capture, false)),
        None => lock_at(dir),
    }
}

/// Serves `capture` once (its clients, and its `outputs` too if asked),
/// then waits for it for up to [`POLL_INTERVAL`]: what the end does
/// between tries for the lock.
fn serve_for_a_moment(capture: &mut Capture, outputs: bool) {
    let timeout = PollTimeout::try_from(POLL_INTERVAL).unwrap_or(PollTimeout::ZERO);
    let waited = if outputs {
        capture.service();
        let mut fds = capture.poll_fds();
        poll(&mut fds, timeout)
    } else {
        capture.service_clients();
        let mut fds = capture.client_poll_fds();
        poll(&mut fds, timeout)
    };
    if waited.is_err() {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Keeps `capture` going until the init (`pid`, our child) has ended: woken
/// by its output, or by its end (a pidfd, TLPI-era kernels aside: Linux 5.3;
/// without one, by the clock). Leaves it unreaped.
///
/// A control client's `stop` is seen through here, whatever becomes of the
/// client: its signal to the init at once (each stop's), then SIGKILL when
/// the earliest stop's timeout is up. The init is this process's child,
/// unreaped, so its PID can't be another's; the signal goes by number, a
/// real-time one too. Killed, it's marked so first (`killed`: the OOM
/// killer isn't blamed).
fn watch(dir: &OwnedFd, pid: i32, capture: &mut Capture) {
    let pidfd = super::pidfd_open(pid);
    let mut kill_at: Option<Instant> = None;
    let signal = |number: i32| {
        mark_if_killing(dir, number);
        // SAFETY: `kill` with plain integers, to our own unreaped child.
        unsafe { libc::kill(pid, number) };
    };
    loop {
        capture.service();
        // Ended (unreaped): a stop now would mark it killed, hiding an OOM
        // kill.
        if super::exited(pid) {
            return;
        }
        for stop in capture.take_stops() {
            signal(stop.signal);
            // Too far off to be had: no SIGKILL, as `StopPolicy` says.
            if let Some(at) = Instant::now().checked_add(stop.timeout) {
                kill_at = Some(kill_at.map_or(at, |earlier| earlier.min(at)));
            }
        }
        if kill_at.is_some_and(|at| Instant::now() >= at) {
            signal(libc::SIGKILL);
            kill_at = None;
        }
        let mut fds = capture.poll_fds();
        fds.extend(
            pidfd
                .as_ref()
                .map(|fd| PollFd::new(fd.as_fd(), PollFlags::POLLIN)),
        );
        let mut wait = capture.timeout().unwrap_or(Duration::from_secs(60));
        if let Some(at) = kill_at {
            wait = wait.min(at.saturating_duration_since(Instant::now()));
        }
        if pidfd.is_none() {
            wait = wait.min(Duration::from_millis(100));
        }
        let timeout = PollTimeout::try_from(wait).unwrap_or(PollTimeout::NONE);
        match poll(&mut fds, timeout) {
            Ok(_) | Err(Errno::EINTR) => {}
            // Can't wait: don't spin either.
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// What the monitor needs from `state.json`.
struct Setup {
    cgroup: Option<cgroup::Cgroup>,
    hooks: Option<ContainerHooks>,
    logs: Option<RecordedLogs>,
    exit_command: Option<ExitCommand>,
    companion: Option<ExitCommand>,
    planned: bool,
}

/// Reads [`Setup`] from `state.json` in the state directory open at `dir`.
fn container_setup(dir: &OwnedFd) -> Result<Setup> {
    let _lock = lock_at(dir).map_err(errno_io("locking the state directory"))?;
    let record = read_record_at(dir)?
        .ok_or_else(|| ContainerError::Setup("the container was deleted".into()))?;
    Ok(Setup {
        cgroup: record.container_cgroup().transpose()?,
        hooks: record.hooks,
        logs: record.logs,
        exit_command: record.exit_command,
        companion: record.companion,
        planned: record.planned,
    })
}

/// The plan `create_planned` left in the state directory open at `dir`
/// (never by path, nor through a symlink).
fn read_plan(dir: &OwnedFd) -> Result<crate::container::Plan> {
    use super::super::plan::{MAX_PLAN_SIZE, PLAN_FILE};
    use std::io::Read as _;
    // Not blocking, so a FIFO put there can't stall the open: it's then
    // refused as not a regular file.
    let file = nix::fcntl::openat(
        dir,
        PLAN_FILE,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno_io(format!("reading {PLAN_FILE}")))?;
    let stat = nix::sys::stat::fstat(&file).map_err(errno_io(format!("reading {PLAN_FILE}")))?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(ContainerError::Setup(format!(
            "{PLAN_FILE} isn't a regular file"
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::from(file)
        .take(MAX_PLAN_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(super::io(format!("reading {PLAN_FILE}")))?;
    if bytes.len() as u64 > MAX_PLAN_SIZE {
        return Err(ContainerError::Setup(format!(
            "{PLAN_FILE} is more than {MAX_PLAN_SIZE} bytes"
        )));
    }
    crate::container::Plan::parse(&bytes)
}

/// The container, spawned and set up.
struct Spawned {
    child: Child,
    pid: i32,
    capture: Option<Capture>,
}

/// Closes everything this process inherited but `given` and the
/// descriptors for the container's process (`passing`'s), and holds their
/// targets from here on: nothing it opens before the spawn may take one of
/// their numbers.
fn keep_only(
    given: &[&OwnedFd],
    passing: Option<crate::process::passed::PassedPlan>,
) -> ForTheContainer {
    let sources: Vec<RawFd> = passing
        .iter()
        .flat_map(|passing| passing.sources.iter().copied())
        .collect();
    // SAFETY: passed by `create` across the exec, distinct from the others
    // (checked); this process owns them.
    let passed = unsafe { crate::process::passed::adopt(&sources) };
    // None is the container's or its hooks' but through the spawn.
    crate::process::close_on_exec(passed.iter().map(AsRawFd::as_raw_fd));
    let keep: Vec<RawFd> = given
        .iter()
        .map(|fd| fd.as_raw_fd())
        .chain(sources)
        .collect();
    super::close_inherited(&keep);
    let held = crate::process::passed::Held::take(passed).map_err(|err| err.to_string());
    ForTheContainer { passing, held }
}

/// Descriptors `create` handed on for the container's process: their
/// numbers and socket activation's part, and the descriptors themselves
/// with their targets held here (or why they couldn't be). All closed once
/// the container's init has its own.
struct ForTheContainer {
    passing: Option<crate::process::passed::PassedPlan>,
    held: std::result::Result<crate::process::passed::Held, String>,
}

/// The exec FIFO, and the started FIFO if `create` made one (for
/// `poststart` hooks: see `Gate::started`), `O_PATH`, for the init.
fn open_gate(dir: &OwnedFd) -> std::result::Result<(OwnedFd, Option<OwnedFd>), String> {
    let open = |name| {
        nix::fcntl::openat(
            dir,
            name,
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    };
    let gate = open(FIFO).map_err(|errno| format!("opening the exec FIFO: {}", errno.desc()))?;
    let started = match open(STARTED) {
        Ok(started) => Some(started),
        Err(Errno::ENOENT) => None,
        Err(errno) => return Err(format!("opening the started FIFO: {}", errno.desc())),
    };
    Ok((gate, started))
}

/// Spawns the container held at the start gate (running `create`'s hooks,
/// as `setup` lists them, on the way), and waits until it's set up. A
/// failure is the message for `create`.
///
/// With a terminal, its init sends the master to `console` once it has made
/// the pseudo-terminal; this process's copy of the socket is closed once the
/// spawn has passed it on, so the receiver sees end-of-file when the init
/// closes its own. With logs (the directory open at `logs`), they're started
/// first, and the container's output goes there from its setup on (its
/// hooks' too).
fn spawn_container(
    dir: &OwnedFd,
    bundle: &Path,
    setup: Setup,
    console: Option<OwnedFd>,
    logs: Option<OwnedFd>,
    for_the_container: ForTheContainer,
) -> std::result::Result<Spawned, String> {
    let held = for_the_container.held?;
    let mut bundle = if setup.planned {
        // From beside its record, as `create` checked it (as this caller
        // runs it): run as it is, nothing added.
        read_plan(dir).map_err(|err| err.to_string())?.into_bundle()
    } else {
        // Read again, and checked again: it may have changed since `create`.
        load_bundle_for_caller(bundle).map_err(|err| err.to_string())?
    };
    console_matches(
        bundle.terminal.is_some(),
        console.is_some(),
        setup.logs.is_some(),
    )
    .map_err(|err| err.to_string())?;
    let (mut capture, container_side) = match (logs, setup.logs) {
        (Some(logs), Some(recorded)) => {
            let (capture, side) = Capture::new(
                logs,
                recorded.settings,
                bundle.terminal.is_some(),
                recorded.stdin,
                dir,
            )?;
            (Some(capture), Some(side))
        }
        (None, None) => (None, None),
        _ => return Err("the log directory wasn't passed on as recorded".to_owned()),
    };
    let size = bundle
        .terminal
        .and_then(|terminal| terminal.size)
        .unwrap_or(crate::process::terminal::DEFAULT_SIZE);
    let terminal_socket = console.as_ref().or_else(|| {
        container_side
            .as_ref()
            .and_then(|side| side.terminal_socket.as_ref())
    });
    let terminal = bundle
        .terminal
        .and(terminal_socket)
        // An OCI console socket, or the capture's: neither answers.
        .map(|socket| ChildTerminal::Console {
            fd: socket.as_raw_fd(),
            size,
        });
    if setup.cgroup.is_none() {
        without_cgroup(&bundle.isolation, "none could be made when it was created")
            .map_err(|err| err.to_string())?;
    }
    bundle.isolation.hooks = setup.hooks;
    // What `exec` confines its processes as: this process, as it starts now
    // (not as `create` read the bundle: it may have changed since).
    let security = crate::process::container_security(&bundle.isolation, setup.cgroup.is_some());
    super::exec::write_snapshot(dir, &bundle.spec, &security)?;
    let (gate, started) = open_gate(dir)?;
    let options = SpawnOptions {
        new_process_group: false,
        keep_ignored: Vec::new(),
        placement: Placement::New(Box::new(bundle.isolation)),
        gate: Some(crate::process::spawn::Gate {
            fifo: gate.as_raw_fd(),
            started: started.as_ref().map(AsRawFd::as_raw_fd),
        }),
        cgroup: setup.cgroup,
        // The OCI runtime's: the container keeps the caller's stdio, or
        // takes its terminal's; with logs, their pipes (or /dev/null, with
        // a terminal).
        stdio: container_side
            .as_ref()
            .map_or([None; 3], super::capture::ContainerSide::targets),
        terminal,
        takes_terminal: terminal.is_some(),
        exec: crate::process::ExecSpawn::default(),
        passing: for_the_container.passing,
    };
    let spawned = spawn(&bundle.spec, &options, None, |_| {});
    // The init has its own copies (or there's no init): ours would keep a
    // passed socket open after the container's process closes it.
    drop(held);
    let mut child = spawned.map_err(|err| format!("{err:#}"))?;
    drop((gate, started));
    drop(console);
    // The container has its own copies: ours would keep its output from
    // ever ending.
    drop(container_side);
    // The report pipe's end-of-file means "set up, at the gate" — or that
    // the init died, which also closes it.
    let created = wait_for_report(&mut child, capture.as_mut());
    let pid = child.pid().as_raw();
    match created {
        Ok(()) if !super::exited(pid) => Ok(Spawned {
            child,
            pid,
            capture,
        }),
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

/// Waits until the report pipe resolves. No deadline, as runc's `create`
/// has none: a setup hung in the kernel (a mount on a dead NFS server) hangs
/// `create` too, and `delete --force` (or a signal) ends it; one that fails
/// reports at once, and a dead init closes the pipe. Meanwhile the output
/// goes to the logs, if it has them: a hook's output mustn't fill a pipe
/// nobody reads, and hang the setup.
fn wait_for_report(child: &mut Child, mut capture: Option<&mut Capture>) -> crate::Result<()> {
    loop {
        child.check_exec()?;
        let Some(report) = child.report_fd() else {
            return Ok(());
        };
        if let Some(capture) = capture.as_deref_mut() {
            capture.service();
        }
        let mut fds = vec![PollFd::new(report, PollFlags::POLLIN)];
        let mut timeout = PollTimeout::NONE;
        if let Some(capture) = capture.as_deref() {
            fds.extend(capture.poll_fds());
            if let Some(wait) = capture.timeout() {
                timeout = PollTimeout::try_from(wait).unwrap_or(PollTimeout::NONE);
            }
        }
        if let Err(errno) = poll(&mut fds, timeout)
            && errno != Errno::EINTR
        {
            // Can't wait: don't spin either (as `watch`).
            std::thread::sleep(Duration::from_millis(100));
        }
    }
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

/// The exit record for a container whose init ended as `status`, the
/// out-of-memory killer having struck as `oom` (if it wasn't a `kill`),
/// that had `started` or not.
fn ended(
    status: &crate::Result<ExitStatus>,
    oom: Option<cgroup::OomEvents>,
    started: bool,
) -> ExitRecord {
    let status = match status {
        Ok(status) => *status,
        Err(err) => return ExitRecord::new(format!("unknown ({err})"), None, None, started),
    };
    let code = Some(i32::from(status.code()));
    match oom {
        Some(oom) if status.killed_by_sigkill() => {
            let cause = oom.cause();
            ExitRecord::new(
                format!("{} ({cause})", describe_exit(status)),
                code,
                Some(cause.to_owned()),
                started,
            )
        }
        _ => ExitRecord::new(describe_exit(status), code, None, started),
    }
}

/// Starts `command` once the container has ended (`ended` is how): a
/// session of its own, `/dev/null` for its standard streams, in `/`, with
/// this process's environment, the `holds` at 3 and up ([`HOLDS_ENV`], their
/// labels in [`HOLD_LABELS_ENV`]) and nothing else open, how it ended in
/// [`EXIT_RECORD_ENV`], this monitor's PID in [`MONITOR_PID_ENV`] (it holds
/// its copies until it exits). A `spawn` that fails for want of a process
/// or memory (`EAGAIN` under `RLIMIT_NPROC`, `ENOMEM`) is tried again, a few
/// times ([`retry_after`]); one that can't start at all leaves the holds to
/// close with the monitor (the sweeps of whatever held them free what they
/// kept), and [`EXIT_COMMAND_FAILED`] in the state directory, open at
/// `dir`, to say so (a restart policy's restart didn't happen).
///
/// The descriptors are arranged here, in the monitor, which exits right
/// after (nothing of it is used again): the holds' copies placed at 3, 4, …,
/// everything else above them closed. So the command's process only has to
/// clear their close-on-exec flags, and what `spawn` opens for itself (its
/// `/dev/null`s, the pipe it reports an `exec` failure on) lands above
/// them, never on one.
fn run_exit_command(
    command: &ExitCommand,
    dir: &OwnedFd,
    holds: &super::holds::Holds,
    ended: &ExitRecord,
) {
    use std::os::unix::process::CommandExt as _;
    let Ok(count) = RawFd::try_from(holds.len()) else {
        say_exit_command_failed(dir);
        return;
    };
    let targets = 3..3 + count;
    // The state directory, above the targets too (its own number may be
    // one), to say a failure in: close-on-exec, so not the command's.
    let dir = nix::fcntl::fcntl(dir, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(targets.end))
        // SAFETY: `F_DUPFD_CLOEXEC` returned a new descriptor of ours.
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .ok();
    // Copies above the targets first, so that placing one can't overwrite
    // another not yet placed.
    let Ok(copies) = holds
        .fds()
        .map(|hold| {
            nix::fcntl::fcntl(hold, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(targets.end))
                // SAFETY: `F_DUPFD_CLOEXEC` returned a new descriptor of ours.
                .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .collect::<nix::Result<Vec<OwnedFd>>>()
    else {
        dir.iter().for_each(say_exit_command_failed);
        return;
    };
    for (copy, target) in copies.iter().zip(targets.clone()) {
        // SAFETY: `dup3` onto a number this process is done with (whatever
        // was there: nothing of the monitor's is used from here on, and it
        // exits without dropping anything).
        if unsafe { libc::dup3(copy.as_raw_fd(), target, libc::O_CLOEXEC) } == -1 {
            dir.iter().for_each(say_exit_command_failed);
            return;
        }
    }
    drop(copies);
    let placed: Vec<RawFd> = targets.clone().collect();
    let kept: Vec<RawFd> = placed
        .iter()
        .copied()
        .chain(dir.as_ref().map(AsRawFd::as_raw_fd))
        .collect();
    super::close_inherited(&kept);
    let mut program = helper(command);
    program
        .env(HOLDS_ENV, holds.len().to_string())
        .env(HOLD_LABELS_ENV, holds.labels())
        .env(MONITOR_PID_ENV, std::process::id().to_string())
        .env(
            EXIT_RECORD_ENV,
            String::from_utf8_lossy(&ended.to_json()).as_ref(),
        );
    // SAFETY: between `fork` and `exec`, the closure calls only `setsid`,
    // `sigaction` and `fcntl` (async-signal-safe), on numbers known before
    // the fork.
    unsafe {
        program.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The monitor ignores SIGXFSZ (its logs); the command needn't.
            let mut default: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGXFSZ, &raw const default, std::ptr::null_mut());
            for fd in targets.clone() {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    // Reaped by whoever adopts it once the monitor exits, at once.
    let mut attempt = 0;
    while let Err(error) = program.spawn() {
        let Some(wait) = retry_after(attempt, &error) else {
            dir.iter().for_each(say_exit_command_failed);
            return;
        };
        std::thread::sleep(wait);
        attempt += 1;
    }
}

/// How long to wait before trying again to start the exit command, whose
/// `attempt`th try (from 0) failed with `error`: only for want of a process
/// or memory, which may pass (`EAGAIN`, `ENOMEM`), 100 ms doubling, five
/// tries in all. `None`: give up (a program that isn't there, or can't be
/// run, never will be).
fn retry_after(attempt: u32, error: &std::io::Error) -> Option<Duration> {
    /// How many tries in all.
    const TRIES: u32 = 5;
    let passing = matches!(
        error.raw_os_error(),
        Some(libc::EAGAIN | libc::ENOMEM | libc::EINTR)
    );
    (passing && attempt.saturating_add(1) < TRIES)
        .then(|| Duration::from_millis(100).saturating_mul(1 << attempt.min(16)))
}

/// Makes [`EXIT_COMMAND_FAILED`], empty, in the state directory open at
/// `dir` (not through a symlink, never over anything): best effort, as the
/// directory may be gone (a `delete` meanwhile).
fn say_exit_command_failed(dir: &OwnedFd) {
    let _ = nix::fcntl::openat(
        dir,
        EXIT_COMMAND_FAILED,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    );
}

/// Waits for the container's init `pid` (`child`) to end, its output
/// captured (if it is) meanwhile, and ends its `companion` once it has:
/// before the init is reaped, while it can, so that nothing the companion
/// does then is taken for the container's (a zombie init still counts as
/// running). Without a capture, the monitor only waits for the reap: the
/// companion goes after it.
fn wait_for_the_end(
    dir: &OwnedFd,
    pid: i32,
    mut child: Child,
    capture: Option<&mut Capture>,
    companion: Option<nix::unistd::Pid>,
) -> crate::Result<ExitStatus> {
    if let Some(capture) = capture {
        watch(dir, pid, capture);
        end_companion(companion);
        return child.reap();
    }
    let status = child.reap();
    end_companion(companion);
    status
}

/// A helper the monitor runs (its exit command, or its companion): the
/// program and its arguments, in `/`, with `/dev/null` for its standard
/// streams. The caller adds its descriptors, environment and `pre_exec`.
fn helper(command: &ExitCommand) -> std::process::Command {
    let mut program = std::process::Command::new(&command.program);
    program
        .args(&command.args)
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    program
}

/// Starts `command` beside the container (`create --companion`): in a
/// session of its own (so its whole process group can be killed at the
/// end), with `/dev/null` for its standard streams, none of the monitor's
/// descriptors (all close-on-exec), and SIGKILL as its parent-death signal
/// (a monitor killed outright takes it). Its PID; none if it couldn't be
/// started (silent, as an exit command's failure: the container runs on
/// without it).
fn spawn_companion(command: &ExitCommand) -> Option<nix::unistd::Pid> {
    use std::os::unix::process::CommandExt as _;
    let monitor = libc::pid_t::try_from(std::process::id()).ok()?;
    let mut program = helper(command);
    // SAFETY: between `fork` and `exec`, the closure calls only `setsid`,
    // `prctl`, `getppid` and `sigaction` (async-signal-safe), on values
    // known before the fork.
    unsafe {
        program.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The monitor gone before the signal was asked for: it would
            // never come.
            if libc::getppid() != monitor {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            let mut default: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGXFSZ, &raw const default, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = program.spawn().ok()?;
    let pid = libc::pid_t::try_from(child.id()).ok()?;
    // Reaped by its PID in `end_companion`, never through the handle.
    drop(child);
    Some(nix::unistd::Pid::from_raw(pid))
}

/// Ends the companion `pid` (if there's one) once the container has: its
/// process group killed (whatever it started with it), then it's reaped.
/// Its PID can't have been reused meanwhile: it's this process's child,
/// not yet reaped (one that died early is a zombie until now).
fn end_companion(pid: Option<nix::unistd::Pid>) {
    let Some(pid) = pid else {
        return;
    };
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
    let _ = nix::sys::wait::waitpid(pid, None);
}

/// How `status` ends, in words: `exited (exit code 3)`.
fn describe_exit(status: ExitStatus) -> String {
    format!("{status} {}{})", super::versions::EXIT_CODE, status.code())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A spawn that failed for want of a process or memory is tried again,
    /// 100 ms doubling, five tries in all; any other failure isn't.
    #[test]
    fn an_exit_command_is_tried_again_only_while_it_may_pass() {
        let again = std::io::Error::from_raw_os_error(libc::EAGAIN);
        let waits: Vec<Option<Duration>> = (0..5).map(|n| retry_after(n, &again)).collect();
        assert_eq!(
            waits,
            [100, 200, 400, 800]
                .map(|ms| Some(Duration::from_millis(ms)))
                .into_iter()
                .chain([None])
                .collect::<Vec<_>>()
        );
        let memory = std::io::Error::from_raw_os_error(libc::ENOMEM);
        assert_eq!(retry_after(0, &memory), Some(Duration::from_millis(100)));
        assert_eq!(retry_after(u32::MAX, &memory), None);
        for never in [libc::ENOENT, libc::EACCES, libc::ENOEXEC] {
            let error = std::io::Error::from_raw_os_error(never);
            assert_eq!(retry_after(0, &error), None, "{never}");
        }
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_arguments_create_passes_are_read() {
        let base = [MONITOR_ARG, "x", "/bundle", "3", "4"];
        assert_eq!(
            parse_arguments(&args(&base)).map(|given| (given.console, given.logs)),
            Some((None, None))
        );
        for (console, logs) in [(Some(5), None), (None, Some(6)), (Some(5), Some(6))] {
            let mut all = args(&base);
            all.extend(
                tagged(console, logs, None, &[])
                    .into_iter()
                    .map(OsString::from),
            );
            let given = parse_arguments(&all).unwrap();
            assert_eq!((given.console, given.logs), (console, logs));
            assert!(given.holds.is_empty());
        }
        let mut all = args(&base);
        all.extend(
            tagged(Some(5), None, None, &[(7, "lease:app"), (9, "port")])
                .into_iter()
                .map(OsString::from),
        );
        let given = parse_arguments(&all).unwrap();
        assert_eq!(
            (given.console, given.holds),
            (
                Some(5),
                vec![(7, "lease:app".to_owned()), (9, "port".to_owned())]
            )
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for list in [
            &[MONITOR_ARG, "x", "/bundle", "3"][..],
            &[MONITOR_ARG, "x", "/bundle", "3", "3"],
            &[MONITOR_ARG, "x", "/bundle", "2", "4"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "5"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "console=4"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "logs=5", "logs=6"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "console=5", "logs=5"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "logs=x"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=2:a"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=4:a"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=7:a,7:b"],
            &[
                MONITOR_ARG,
                "x",
                "/bundle",
                "3",
                "4",
                "holds=7:a",
                "holds=8:b",
            ],
            // Without a label, or one that isn't.
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=7"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=7:"],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=7:a b"],
            &[
                MONITOR_ARG,
                "x",
                "/bundle",
                "3",
                "4",
                "console=7",
                "holds=7:a",
            ],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds="],
            &[MONITOR_ARG, "x", "/bundle", "3", "4", "holds=7:a,x:b"],
        ] {
            assert_eq!(parse_arguments(&args(list)), None, "{list:?}");
        }
    }

    /// The words are version 0's text exactly, so that a reader of either
    /// finds the code in them.
    #[test]
    fn the_exit_record_gives_attach_its_code() {
        for status in [ExitStatus::Exited(0), ExitStatus::Exited(42)] {
            let words = describe_exit(status);
            assert_eq!(
                ExitRecord::parse(words.as_bytes()).code,
                Some(i32::from(status.code()))
            );
            let record = ExitRecord::new(words, Some(i32::from(status.code())), None, true);
            assert_eq!(ExitRecord::parse(&record.to_json()), record);
        }
        let killed = ExitStatus::Signaled {
            signal: crate::process::Signal::KILL,
            core_dumped: false,
        };
        let words = format!("{}, out of memory", describe_exit(killed));
        assert_eq!(ExitRecord::parse(words.as_bytes()).code, Some(137));
    }

    /// The plan is read from the state directory itself: a symlink at
    /// `plan.json` (to a plan that would read well) isn't followed, and a
    /// FIFO there isn't waited on.
    #[test]
    fn a_plan_behind_a_symlink_is_not_read() {
        use crate::container::plan::PLAN_FILE;
        let dir = std::env::temp_dir().join(format!("shipyard-plan-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = crate::container::Plan::new(
            crate::process::ProcessSpec::new(vec!["true".into()]),
            crate::isolation::Isolation::standard(),
        );
        std::fs::write(dir.join("elsewhere.json"), plan.to_json().unwrap()).unwrap();
        let fd: OwnedFd = nix::fcntl::open(
            &dir,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        // The control: the same plan, as a file of its own, reads.
        std::fs::copy(dir.join("elsewhere.json"), dir.join(PLAN_FILE)).unwrap();
        assert!(read_plan(&fd).is_ok(), "the control");
        std::fs::remove_file(dir.join(PLAN_FILE)).unwrap();
        std::os::unix::fs::symlink("elsewhere.json", dir.join(PLAN_FILE)).unwrap();
        let err = read_plan(&fd).unwrap_err();
        assert!(err.to_string().contains(PLAN_FILE), "{err}");
        // Nor is a FIFO waited on: refused at once.
        std::fs::remove_file(dir.join(PLAN_FILE)).unwrap();
        nix::unistd::mkfifo(&dir.join(PLAN_FILE), Mode::S_IRWXU).unwrap();
        let err = read_plan(&fd).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(err.to_string().contains("isn't a regular file"), "{err}");
    }
}
