//! `__health`: a detached container's companion while it has a health
//! check (stage 9, step 9.6b).
//!
//! The container's monitor starts it once the container is created
//! (`create --companion`, the runtime's), and kills it with its process
//! group once the container's init has ended, before the end is recorded;
//! a monitor killed takes it too (its parent-death signal). It's this
//! binary again (`/proc/self/exe`), in a session of its own, with
//! `/dev/null` for its standard streams. It reads the check from the
//! container's record (never from its arguments: a host's `ps` shows
//! those), waits for the start, then runs the check every interval, each
//! in a process of its own (`__health-check`, which runs the command in the
//! container as `exec` does): its output on a pipe, waited for until it
//! exits or its time is up; then it's stopped, and stops the command with
//! all it started. A check whose companion dies is stopped too (its
//! parent-death signal). Each result goes into the start's
//! [`health::FILE`](crate::health::FILE) (atomically: a reader never sees
//! half of one), and each change of status is an event (`health_status`).
//! What it can't do is said in `health.log`, beside the record.

#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by Linux's detached containers only")
)]

use std::ffi::OsString;
use std::path::PathBuf;

/// The argument that makes this binary `__health`.
pub(crate) const HEALTH_ARG: &str = "__health";
/// The argument that makes this binary one check (`__health-check`).
pub(crate) const CHECK_ARG: &str = "__health-check";

/// Runs `__health` or `__health-check` if this process was started as
/// one (its first argument), and exits; returns otherwise. Call it in
/// `main`, right after [`cleanup_entry_point`](super::cleanup_entry_point),
/// in every binary that makes detached containers: their monitors run the
/// binary that made them as a container's companion.
pub fn health_entry_point() {
    let mut args = std::env::args_os().skip(1);
    let check = match args.next() {
        Some(first) if first == HEALTH_ARG => false,
        Some(first) if first == CHECK_ARG => true,
        _ => return,
    };
    // Not for a privileged start (setuid, file capabilities): its
    // arguments name a container to run a command in.
    if shipyard_runtime::process::refuse_privileged_start().is_err() {
        std::process::exit(1);
    }
    #[cfg(target_os = "linux")]
    std::process::exit(if check {
        linux::check_main(args)
    } else {
        linux::main(args)
    });
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (check, args);
        std::process::exit(1);
    }
}

/// What `__health` (and `__health-check`) is told, from its arguments:
/// where the stores are, and which container. The check itself is the
/// container's record's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Arguments {
    /// The container's ID.
    pub(crate) id: String,
    /// The container store.
    pub(crate) containers: PathBuf,
    /// The runtime's state root.
    pub(crate) runtime_root: PathBuf,
    /// The events journal's directory.
    pub(crate) events: Option<PathBuf>,
    /// This start's log directory (`__health`'s).
    pub(crate) start_dir: Option<PathBuf>,
}

impl Arguments {
    /// Reads them (after [`HEALTH_ARG`] or [`CHECK_ARG`]).
    pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let (mut id, mut containers, mut runtime_root) = (None, None, None);
        let (mut events, mut start_dir) = (None, None);
        while let Some(flag) = args.next() {
            let flag = flag.to_string_lossy().into_owned();
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let path = PathBuf::from(&value);
            match flag.as_str() {
                "--id" => id = Some(value.to_string_lossy().into_owned()),
                "--containers" => containers = Some(path),
                "--runtime-root" => runtime_root = Some(path),
                "--events" => events = Some(path),
                "--start-dir" => start_dir = Some(path),
                _ => return Err(format!("{flag}: no such argument")),
            }
        }
        let missing = |what: &str| format!("{what} is missing");
        Ok(Self {
            id: id.ok_or_else(|| missing("--id"))?,
            containers: containers.ok_or_else(|| missing("--containers"))?,
            runtime_root: runtime_root.ok_or_else(|| missing("--runtime-root"))?,
            events,
            start_dir,
        })
    }

    /// These, as arguments (after the first), as [`parse`](Self::parse)
    /// reads them.
    fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "--id".into(),
            self.id.clone().into(),
            "--containers".into(),
            self.containers.clone().into(),
            "--runtime-root".into(),
            self.runtime_root.clone().into(),
        ];
        if let Some(events) = &self.events {
            args.extend(["--events".into(), events.clone().into()]);
        }
        if let Some(start_dir) = &self.start_dir {
            args.extend(["--start-dir".into(), start_dir.clone().into()]);
        }
        args
    }
}

/// The companion that runs container `id`'s health check while its start
/// (log directory `start_dir`) runs: this binary as `__health`, told where
/// the stores are.
///
/// # Errors
///
/// A path that isn't UTF-8 (the runtime keeps a companion's arguments in
/// JSON).
#[cfg(target_os = "linux")]
pub(super) fn health_command(
    paths: &crate::Paths,
    id: &crate::containers::ContainerId,
    start_dir: &std::path::Path,
) -> crate::Result<shipyard_runtime::container::ExitCommand> {
    let text =
        |path: &std::path::Path| super::helper::arg_text(path, "a container's health checks");
    let arguments = Arguments {
        id: id.as_str().to_owned(),
        containers: PathBuf::from(text(paths.containers()?)?),
        runtime_root: PathBuf::from(text(paths.runtime_root()?)?),
        events: Some(PathBuf::from(text(&paths.events()?)?)),
        start_dir: Some(PathBuf::from(text(start_dir)?)),
    };
    let args = std::iter::once(HEALTH_ARG.to_owned())
        .chain(
            arguments
                .to_args()
                .into_iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        )
        .collect();
    Ok(shipyard_runtime::container::ExitCommand::new(
        "/proc/self/exe",
        args,
    )?)
}

/// `timeout` as Docker says it in a timed-out check's output
/// ("Health check exceeded timeout (30s)").
fn exceeded(timeout: std::time::Duration) -> String {
    let millis = timeout.as_millis();
    let said = if millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{millis}ms")
    };
    format!("Health check exceeded timeout ({said})")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read as _;
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use shipyard_runtime::container::{self as runtime, Status};

    use super::{Arguments, CHECK_ARG, exceeded};
    use crate::containers::{self, ContainerId};
    use crate::events;
    use crate::health::{self, Health, HealthCheck, Probe, Status as Said};
    use crate::report::Reporter;
    use crate::run::helper::Log;
    use crate::{Engine, Paths};

    /// How often the start is looked for.
    const START_POLL: Duration = Duration::from_millis(100);
    /// How often a check is looked at, while it runs.
    const CHECK_POLL: Duration = Duration::from_millis(20);
    /// Once a check has exited, how long what it left is still read for.
    const DRAIN: Duration = Duration::from_millis(100);
    /// How long a check asked to stop has before it's killed.
    const STOP_GRACE: Duration = Duration::from_secs(1);

    /// The engine on the stores `arguments` name, reporting to `log`.
    fn engine(arguments: &Arguments, log: Log) -> Engine {
        let mut paths = Paths::from_env()
            .with_containers(&arguments.containers)
            .with_runtime_root(&arguments.runtime_root);
        if let Some(events) = &arguments.events {
            paths = paths.with_events(events);
        }
        Engine::new(paths, Arc::new(log) as Arc<dyn Reporter>)
    }

    /// Container `id`'s record, its health check, and the command it runs.
    /// Its record alone, by its ID: no other is read (each check would read
    /// them all, and say every unreadable one in its log).
    fn check_of(
        engine: &Engine,
        id: &ContainerId,
    ) -> Result<(containers::Record, HealthCheck, Vec<String>), String> {
        let record = containers::read_one(engine, id).map_err(|error| format!("{error:#}"))?;
        let check = record
            .config
            .health
            .clone()
            .ok_or_else(|| "the container has no health check".to_owned())?;
        let command = check.command().map_err(|error| error.to_string())?;
        Ok((record, check, command))
    }

    /// `__health`'s work: wait for the start, then check every interval
    /// until the container has ended. Its exit code: 0, or 1 if it couldn't
    /// begin (said in `health.log`, where it can be).
    pub(super) fn main(args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
        let Ok(arguments) = Arguments::parse(args) else {
            return 1;
        };
        let log = Log::open(&arguments.containers, &arguments.id, "health.log");
        let (Some(start_dir), Ok(id)) = (
            arguments.start_dir.clone(),
            ContainerId::parse(&arguments.id),
        ) else {
            log.line("health checks: no start directory, or not a container's ID");
            return 1;
        };
        let engine = engine(&arguments, log.clone());
        let check = match check_of(&engine, &id) {
            Ok((_, check, _)) => check,
            Err(why) => {
                log.line(&format!("health checks: {why}"));
                return 1;
            }
        };
        let Some((started, init)) = wait_for_the_start(&engine, &id) else {
            return 0;
        };
        let mut health = Health::starting();
        write(&start_dir, &health, &log);
        loop {
            let in_start_period = started.elapsed() < check.start_period();
            // Docker's: the start period's quicker interval only while
            // nothing has passed yet.
            std::thread::sleep(if in_start_period && health.status == Said::Starting {
                check.start_interval()
            } else {
                check.interval()
            });
            match state(&engine, &id, init.as_ref()) {
                State::Running => {}
                // Not checked while it's frozen: the check would wait for
                // it, and time out.
                State::Paused => continue,
                State::Ended => return 0,
            }
            // Judged as the check begins, as Docker does.
            let in_start_period = started.elapsed() < check.start_period();
            let (probe, passed) = run_check(&arguments, &check);
            match state(&engine, &id, init.as_ref()) {
                State::Running => {}
                // Paused, or ended, meanwhile: the check may have failed
                // because it was, which says nothing of its health.
                State::Paused => continue,
                State::Ended => return 0,
            }
            let changed = health.record(probe, passed, in_start_period, check.retries());
            write(&start_dir, &health, &log);
            if changed && let Ok(record) = containers::read_one(&engine, &id) {
                events::emit(
                    &engine,
                    &events::container(&record, "health_status")
                        .with("healthStatus", health.status),
                );
            }
        }
    }

    /// Where the container is, for its checks.
    enum State {
        /// Running: checked.
        Running,
        /// Paused: not checked, and checked again once it's resumed.
        Paused,
        /// Ended (its init has, if not yet reaped; or its state is gone):
        /// the checks are over.
        Ended,
    }

    /// The container's state, for its checks: the runtime's, and its init
    /// (`init`: its pidfd, readable once it has exited, which the runtime
    /// still calls running until its monitor has reaped it).
    fn state(engine: &Engine, id: &ContainerId, init: Option<&OwnedFd>) -> State {
        let exited = init.is_some_and(|pidfd| {
            let mut fds = [PollFd::new(pidfd.as_fd(), PollFlags::POLLIN)];
            poll(&mut fds, PollTimeout::ZERO).is_ok_and(|ready| ready > 0)
        });
        let root = engine.paths().runtime_root().ok();
        let status = root.and_then(|root| runtime::peek_status(root, id.as_str()).ok().flatten());
        match status {
            _ if exited => State::Ended,
            Some(Status::Running) => State::Running,
            Some(Status::Paused) => State::Paused,
            _ => State::Ended,
        }
    }

    /// Waits for the container to start (the runtime's word: its gate
    /// opened): when it did, as near as can be told, with its init's pidfd
    /// (taken while it's still created: the runtime gives it then). None:
    /// it never will (it ended, or its state went, first).
    fn wait_for_the_start(engine: &Engine, id: &ContainerId) -> Option<(Instant, Option<OwnedFd>)> {
        let root = engine.paths().runtime_root().ok()?;
        let mut init = None;
        loop {
            match runtime::peek_status(root, id.as_str()).ok().flatten()? {
                Status::Running | Status::Paused => return Some((Instant::now(), init)),
                Status::Created | Status::Creating => {
                    if init.is_none() {
                        init = runtime::init_process(root, id.as_str())
                            .ok()
                            .and_then(|process| process.pidfd);
                    }
                    std::thread::sleep(START_POLL);
                }
                _ => return None,
            }
        }
    }

    /// `health` written to the start's directory (a failure said in the
    /// log).
    fn write(start_dir: &std::path::Path, health: &Health, log: &Log) {
        match crate::fs::write_atomically(start_dir, health::FILE, &health.to_json(), 0o600)
            .map(crate::fs::Placed::unsynced)
        {
            Ok(None) => {}
            Ok(Some(error)) | Err(error) => {
                log.line(&format!("warning: the health file: {error:#}"));
            }
        }
    }

    /// One check: `__health-check` in a process of its own (its own
    /// process group, and SIGTERM, which stops it, as its parent-death
    /// signal), its output (stdout and stderr together) on a pipe, the first
    /// [`health::MAX_OUTPUT`] bytes kept, waited for until it exits or the
    /// check's timeout. Its result, and whether it passed.
    fn run_check(arguments: &Arguments, check: &HealthCheck) -> (Probe, bool) {
        let start = SystemTime::now();
        let probe = |exit_code: i32, output: String| Probe {
            start: events::rfc3339(start),
            end: events::rfc3339(SystemTime::now()),
            exit_code,
            output,
        };
        let pipe = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).and_then(|(reader, writer)| {
            let also = writer.try_clone().map_err(|_| nix::errno::Errno::EMFILE)?;
            Ok((reader, writer, also))
        });
        let Ok((reader, writer, also)) = pipe else {
            return (probe(1, "no pipe for the check's output".to_owned()), false);
        };
        let Ok(companion) = libc::pid_t::try_from(std::process::id()) else {
            return (probe(1, "no PID".to_owned()), false);
        };
        let mut command = std::process::Command::new("/proc/self/exe");
        command
            .arg(CHECK_ARG)
            .args(
                Arguments {
                    start_dir: None,
                    ..arguments.clone()
                }
                .to_args(),
            )
            .stdin(std::process::Stdio::null())
            .stdout(writer)
            .stderr(also)
            // A group of its own: a check that won't stop goes with it.
            .process_group(0);
        // SAFETY: between `fork` and `exec`, the closure calls only `prctl`
        // and `getppid` (async-signal-safe), on a value known before.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != companion {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
        let spawned = command.spawn();
        // The builder holds the pipe's writing ends: they must be the
        // check's alone.
        drop(command);
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return (
                    probe(1, format!("the check couldn't start: {error}")),
                    false,
                );
            }
        };
        let deadline = Instant::now() + check.timeout();
        let (output, status) = wait(&mut child, reader, deadline);
        let Some(status) = status else {
            end(&mut child);
            let _ = child.wait();
            return (probe(-1, exceeded(check.timeout())), false);
        };
        // 128 + N for a check killed by signal N, as a shell says it.
        let code = status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(-1);
        (probe(code, output), code == 0)
    }

    /// Waits for `child` until it exits or `deadline`, reading its output
    /// from `reader` meanwhile: the first [`health::MAX_OUTPUT`] bytes (the
    /// rest read and dropped, so it never blocks on a full pipe). Once it
    /// has exited, what's left to read is read for at most [`DRAIN`] (what
    /// comes within it kept, waited for until the pipe ends or it's over),
    /// and no more: something it left in the container (out of its process
    /// group, which `exec` ends) may keep the pipe open, or keep writing.
    /// Its output, and how it exited (none: out of time).
    pub(super) fn wait(
        child: &mut std::process::Child,
        reader: OwnedFd,
        deadline: Instant,
    ) -> (String, Option<std::process::ExitStatus>) {
        let mut file = std::fs::File::from(reader);
        let mut kept = Vec::new();
        let mut open = true;
        let mut drained_by = None;
        loop {
            let exited = child.try_wait().ok().flatten();
            let wait = if exited.is_some() {
                // Drained: the pipe's end, or the drain's (F61: polled
                // for what's left of it, so a line written just after the
                // exit is kept).
                let by = *drained_by.get_or_insert_with(|| Instant::now() + DRAIN);
                let left = by.saturating_duration_since(Instant::now());
                if left.is_zero() || !open {
                    return (text(&kept), exited);
                }
                PollTimeout::try_from(left.as_millis()).unwrap_or(PollTimeout::ZERO)
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return (text(&kept), None);
                }
                PollTimeout::try_from(left.min(CHECK_POLL).as_millis()).unwrap_or(PollTimeout::ZERO)
            };
            let readable = open && {
                let mut fds = [PollFd::new(file.as_fd(), PollFlags::POLLIN)];
                poll(&mut fds, wait).is_ok_and(|ready| ready > 0)
            };
            if readable {
                let mut buffer = [0_u8; 4096];
                match file.read(&mut buffer) {
                    Ok(0) | Err(_) => open = false,
                    Ok(read) => {
                        let room = health::MAX_OUTPUT.saturating_sub(kept.len());
                        kept.extend_from_slice(&buffer[..read.min(room)]);
                        continue;
                    }
                }
            }
            if exited.is_none() && !open {
                std::thread::sleep(CHECK_POLL);
            }
        }
    }

    /// Ends a check out of time. Asked to stop (SIGTERM), it kills the
    /// command it runs in the container at once, with all that started
    /// (the command has a process group of its own there, which only the
    /// check can reach), and exits. One that hasn't within [`STOP_GRACE`]
    /// is killed with its process group. Reaped by the caller.
    fn end(child: &mut std::process::Child) {
        let Ok(pid) = i32::try_from(child.id()) else {
            let _ = child.kill();
            return;
        };
        let pid = nix::unistd::Pid::from_raw(pid);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
        let _ = child.kill();
    }

    /// Output as text (invalid UTF-8 replaced), its trailing newlines gone.
    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).trim_end().to_owned()
    }

    /// `__health-check`'s work: the container's check, from its record, run
    /// in it as `exec` runs a command (its user, environment and working
    /// directory the container's; no input). Its exit code: the command's;
    /// 127 or 126 if it couldn't be run (said on stderr, the check's
    /// output), 125 for anything else.
    pub(super) fn check_main(args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
        use std::io::Write as _;
        let Ok(arguments) = Arguments::parse(args) else {
            return 125;
        };
        let log = Log::open(&arguments.containers, &arguments.id, "health.log");
        let engine = engine(&arguments, log);
        let checked = ContainerId::parse(&arguments.id)
            .map_err(|error| format!("{error:#}"))
            .and_then(|id| check_of(&engine, &id));
        let (record, command) = match checked {
            Ok((record, _, command)) => (record, command),
            Err(why) => {
                let _ = writeln!(std::io::stderr(), "{why}");
                return 125;
            }
        };
        // Asked to stop (SIGTERM: out of time, or its companion gone), it
        // kills the command at once, with all it started.
        let request = crate::exec::ExecRequest {
            args: command.into_iter().map(Into::into).collect(),
            stop: shipyard_runtime::process::StopPolicy {
                signal: shipyard_runtime::process::Signal::KILL,
                timeout: Duration::ZERO,
            },
            ..crate::exec::ExecRequest::default()
        };
        match crate::exec::in_record(&engine, &record, &request) {
            Ok(crate::exec::Ran::Exited(code)) => i32::from(code),
            Ok(_) => 125,
            Err(error) => {
                let _ = writeln!(std::io::stderr(), "{error:#}");
                i32::from(crate::exec::exit_code(&error))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A check's output written just after it exits, by something it left
    /// running, is kept (F61: the drain waits up to its 100 ms for it); the
    /// control, its output before, read by the drain too: the check has
    /// ended before it's waited for. (20 ms against the drain's 100: a host
    /// slower than that loses the line, and fails this test.)
    #[cfg(target_os = "linux")]
    #[test]
    fn a_checks_late_output_is_kept() {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "echo early; (sleep 0.02; echo late) & exit 0"])
            .stdin(std::process::Stdio::null())
            .stdout(writer.try_clone().unwrap())
            .stderr(writer)
            .spawn()
            .unwrap();
        // Ended before it's waited for: what's read from here is the
        // drain's (the late line comes 20 ms after the start).
        child.wait().unwrap();
        let (text, status) = super::linux::wait(
            &mut child,
            std::os::fd::OwnedFd::from(reader),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
        assert!(status.is_some_and(|status| status.success()), "{status:?}");
        assert!(text.contains("early"), "{text:?}");
        assert!(text.contains("late"), "{text:?}");
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    /// `__health`'s arguments read back, and are written as read; one
    /// missing, or not known, is refused.
    #[test]
    fn arguments_read_back() {
        let given = args(&[
            "--id",
            "abc",
            "--containers",
            "/c",
            "--runtime-root",
            "/r",
            "--events",
            "/e",
            "--start-dir",
            "/c/abc/logs/0000000001",
        ]);
        let parsed = Arguments::parse(given.clone()).unwrap();
        assert_eq!(
            parsed.start_dir.as_deref(),
            Some(std::path::Path::new("/c/abc/logs/0000000001"))
        );
        assert_eq!(parsed.to_args(), given);
        assert!(Arguments::parse(args(&["--id", "abc"])).is_err());
        assert!(Arguments::parse(args(&["--colour", "red"])).is_err());
    }

    /// A timeout said as Docker says it.
    #[test]
    fn a_timeout_is_said_as_dockers() {
        assert_eq!(
            exceeded(std::time::Duration::from_secs(30)),
            "Health check exceeded timeout (30s)"
        );
        assert_eq!(
            exceeded(std::time::Duration::from_millis(1500)),
            "Health check exceeded timeout (1500ms)"
        );
    }
}
