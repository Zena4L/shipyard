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
    Answer, ContainerError, DEFAULT_CONSOLE_SIZE, EXIT_CODE, EXIT_FILE, FIFO, KILLED_FILE,
    MONITOR_ARG, RecordedLogs, Result, STARTED, console_matches, errno_io, lock_at, read_record_at,
    remove_cgroup_at, start_time, without_cgroup, write_atomically_at, write_record_at,
};
use crate::cgroup;
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
/// JSON (`passed.rs`'s `Passing`), their numbers the monitor's.
const PASSING_TAG: &str = "passing=";

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
    if let Some(passed) = parse_arguments(&args) {
        // SAFETY: all passed by `create` across the exec, and distinct
        // (checked); this process owns them.
        let owned = |fd: RawFd| unsafe { OwnedFd::from_raw_fd(fd) };
        monitor(
            &owned(passed.dir),
            Path::new(&passed.bundle),
            owned(passed.ready),
            passed.console.map(owned),
            passed.logs.map(owned),
            passed.passing,
        );
    }
    let _ = std::io::stderr()
        .write_all(b"shipyard: this is a container monitor's internal entry point\n");
    std::process::exit(125);
}

/// What `create` passed: `MONITOR_ARG BUNDLE DIR READY [console=N] [logs=N]
/// [passing=JSON]`.
#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    bundle: OsString,
    dir: RawFd,
    ready: RawFd,
    console: Option<RawFd>,
    logs: Option<RawFd>,
    passing: Option<crate::process::passed::Passing>,
}

/// Reads the arguments, if they're what `create` passes: every descriptor
/// above 2 and distinct, each tag at most once, nothing else.
fn parse_arguments(args: &[OsString]) -> Option<Arguments> {
    let fd = |text: &str| text.parse::<RawFd>().ok().filter(|&fd| fd > 2);
    let [_, _, bundle, dir, ready, rest @ ..] = args else {
        return None;
    };
    let mut passed = Arguments {
        bundle: bundle.clone(),
        dir: fd(dir.to_str()?)?,
        ready: fd(ready.to_str()?)?,
        console: None,
        logs: None,
        passing: None,
    };
    for arg in rest {
        let arg = arg.to_str()?;
        if let Some(json) = arg.strip_prefix(PASSING_TAG) {
            let passing: crate::process::passed::Passing = serde_json::from_str(json).ok()?;
            if passing.sources.iter().any(|&fd| fd <= 2)
                || passed.passing.replace(passing).is_some()
            {
                return None;
            }
            continue;
        }
        let slot = if let Some(value) = arg.strip_prefix(CONSOLE_TAG) {
            (&mut passed.console, value)
        } else {
            (&mut passed.logs, arg.strip_prefix(LOGS_TAG)?)
        };
        if slot.0.replace(fd(slot.1)?).is_some() {
            return None;
        }
    }
    let mut all: Vec<RawFd> = [
        Some(passed.dir),
        Some(passed.ready),
        passed.console,
        passed.logs,
    ]
    .into_iter()
    .flatten()
    .chain(
        passed
            .passing
            .iter()
            .flat_map(|passing| passing.sources.iter().copied()),
    )
    .collect();
    let count = all.len();
    all.sort_unstable();
    all.dedup();
    (all.len() == count).then_some(passed)
}

/// The optional arguments for [`parse_arguments`].
pub(super) fn tagged(
    console: Option<RawFd>,
    logs: Option<RawFd>,
    passing: Option<&crate::process::passed::Passing>,
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
    passing: Option<crate::process::passed::Passing>,
) -> ! {
    // (A session of its own already: `spawn_monitor` made it one.)
    // Whatever the caller left open (not close-on-exec) would be held for
    // the container's whole life: a pipe whose reader waits for end-of-file,
    // say. The monitor keeps nothing else but the descriptors it was given.
    let given: Vec<&OwnedFd> = [Some(dir), Some(&ready), console.as_ref(), logs.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    let handed = keep_only(&given, passing);
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
    // None is the container's or its hooks'. (The init gets the console
    // socket its own way: the spawn passes it on.)
    for fd in given.into_iter().chain(&handed.passed) {
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
    let setup = match container_setup(dir) {
        Ok(setup) => setup,
        Err(err) => {
            answer(&Answer::Failed(format!(
                "reading the container's state: {err}"
            )));
            std::process::exit(1);
        }
    };
    let cgroup = setup.cgroup.clone();
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
    } = match spawn_container(dir, bundle, setup, console, logs, handed) {
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
    if let Some(capture) = &mut capture {
        watch(pid, capture);
    }
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
    // Before the drain too: whatever escaped the init (without a PID
    // namespace) goes, and with it its hold on the output.
    remove_cgroup();
    if let Some(capture) = &mut capture {
        capture.drain(Instant::now() + DRAIN_WAIT);
    }
    // What `attach` ends with: the container's exit code (128 + a signal).
    let code = status
        .as_ref()
        .map_or(125, |status| i32::from(status.code()));
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
    // Last: once it's there, the logs are complete (`logs_of`).
    if let Ok(_lock) = lock_at(dir) {
        let _ = write_atomically_at(dir, EXIT_FILE, ended.as_bytes());
    }
    // Then the attached clients: told once `state` says stopped.
    if let Some(capture) = &mut capture {
        capture.end(code);
    }
    std::process::exit(0);
}

/// Keeps `capture` going until the init (`pid`, our child) has ended: woken
/// by its output, or by its end (a pidfd, TLPI-era kernels aside: Linux 5.3;
/// without one, by the clock). Leaves it unreaped.
fn watch(pid: i32, capture: &mut Capture) {
    let pidfd = pidfd_open(pid);
    loop {
        capture.service();
        if super::exited(pid) {
            return;
        }
        let mut fds = capture.poll_fds();
        fds.extend(
            pidfd
                .as_ref()
                .map(|fd| PollFd::new(fd.as_fd(), PollFlags::POLLIN)),
        );
        let mut wait = capture.timeout().unwrap_or(Duration::from_secs(60));
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

/// A pidfd for `pid` (`pidfd_open`, Linux 5.3): readable once it has
/// exited. None where it can't be had (an older kernel, a seccomp policy
/// that refuses it): then the clock wakes the loop.
fn pidfd_open(pid: i32) -> Option<OwnedFd> {
    // SAFETY: `pidfd_open` takes a PID and flags, and returns a new
    // descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let fd = RawFd::try_from(fd).ok().filter(|&fd| fd >= 0)?;
    // SAFETY: just made by the kernel, ours alone.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What the monitor needs from `state.json`.
struct Setup {
    cgroup: Option<cgroup::Cgroup>,
    hooks: Option<ContainerHooks>,
    logs: Option<RecordedLogs>,
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
    })
}

/// The container, spawned and set up.
struct Spawned {
    child: Child,
    pid: i32,
    capture: Option<Capture>,
}

/// Closes everything this process inherited but `given` and the handed
/// descriptors (`passing`'s), and holds their targets from here on: nothing
/// it opens before the spawn may take one of their numbers.
fn keep_only(given: &[&OwnedFd], passing: Option<crate::process::passed::Passing>) -> Handed {
    // SAFETY: passed by `create` across the exec, distinct from the others
    // (checked); this process owns them.
    let passed: Vec<OwnedFd> = passing
        .iter()
        .flat_map(|passing| passing.sources.iter())
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    let keep: Vec<RawFd> = given
        .iter()
        .map(|fd| fd.as_raw_fd())
        .chain(passed.iter().map(AsRawFd::as_raw_fd))
        .collect();
    super::close_inherited(&keep);
    let reserved = crate::process::passed::reserve(passed.len()).map_err(|err| err.to_string());
    Handed {
        passing,
        passed,
        reserved,
    }
}

/// Descriptors `create` handed on for the container's process: their
/// numbers and socket activation's part, the descriptors, and their targets
/// held here (or why they couldn't be). All closed once the container's
/// init has its own.
struct Handed {
    passing: Option<crate::process::passed::Passing>,
    passed: Vec<OwnedFd>,
    reserved: std::result::Result<Vec<OwnedFd>, String>,
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
    handed: Handed,
) -> std::result::Result<Spawned, String> {
    let reserved = handed.reserved?;
    // Read again, it may have changed since `create` checked it.
    let mut bundle = load_bundle_for_caller(bundle).map_err(|err| err.to_string())?;
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
        .unwrap_or(DEFAULT_CONSOLE_SIZE);
    let terminal_socket = console.as_ref().or_else(|| {
        container_side
            .as_ref()
            .and_then(|side| side.terminal_socket.as_ref())
    });
    let terminal = bundle
        .terminal
        .and(terminal_socket)
        .map(|socket| ChildTerminal::Socket {
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
        passing: handed.passing,
    };
    let spawned = spawn(&bundle.spec, &options, None, |_| {});
    // The init has its own copies (or there's no init): ours would keep a
    // passed socket open after the container's process closes it.
    drop(handed.passed);
    drop(reserved);
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

fn describe_exit(status: ExitStatus) -> String {
    format!("{status} {EXIT_CODE}{})", status.code())
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

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_arguments_create_passes_are_read() {
        let base = [MONITOR_ARG, "x", "/bundle", "3", "4"];
        assert_eq!(
            parse_arguments(&args(&base)).map(|passed| (passed.console, passed.logs)),
            Some((None, None))
        );
        for (console, logs) in [(Some(5), None), (None, Some(6)), (Some(5), Some(6))] {
            let mut all = args(&base);
            all.extend(tagged(console, logs, None).into_iter().map(OsString::from));
            let passed = parse_arguments(&all).unwrap();
            assert_eq!((passed.console, passed.logs), (console, logs));
        }
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
        ] {
            assert_eq!(parse_arguments(&args(list)), None, "{list:?}");
        }
    }

    #[test]
    fn the_exit_record_gives_attach_its_code() {
        for status in [ExitStatus::Exited(0), ExitStatus::Exited(42)] {
            assert_eq!(
                super::super::recorded_code(&describe_exit(status)),
                Some(i32::from(status.code()))
            );
        }
        let killed = ExitStatus::Signaled {
            signal: crate::process::Signal::KILL,
            core_dumped: false,
        };
        assert_eq!(
            super::super::recorded_code(&format!("{}, out of memory", describe_exit(killed))),
            Some(137)
        );
        assert_eq!(super::super::recorded_code("unknown (no status)"), None);
    }
}
