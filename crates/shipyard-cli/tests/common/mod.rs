//! Helpers shared by the end-to-end test files.

// Each test binary uses only part of these.
#![allow(dead_code, reason = "each test binary uses a different part")]

use std::io::{BufRead, BufReader};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// The longest any one shipyard run in a test may take.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);

/// `shipyard run ARGS…` like [`shipyard`], with `setup` applied to the
/// process just before shipyard starts: to start it in a deliberately bad
/// state (a "hostile parent"). `setup` must only make async-signal-safe
/// calls, as `pre_exec` requires (like [`block`] and [`ignore`]).
pub(crate) fn shipyard_run_with(args: &[&str], setup: fn() -> std::io::Result<()>) -> Command {
    let mut command = shipyard(&[&["run", "--no-isolation"], args].concat());
    // SAFETY: `setup` only makes async-signal-safe calls (see above).
    unsafe { command.pre_exec(setup) };
    command
}

/// The exit code for a death by `signal`.
pub(crate) fn killed_by(signal: libc::c_int) -> i32 {
    128 + signal
}

/// Blocks `signal` in the calling process. For `pre_exec` hooks: only
/// async-signal-safe calls.
pub(crate) fn block(signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `sigset_t` operations and `sigprocmask` are async-signal-safe,
    // and `set` is valid storage.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, signal);
        if libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Ignores `signal` in the calling process, as `nohup` does for SIGHUP. For
/// `pre_exec` hooks: only async-signal-safe calls.
pub(crate) fn ignore(signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `signal` is async-signal-safe.
    if unsafe { libc::signal(signal, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A `shipyard` command, started in a new session with no controlling
/// terminal, so it behaves the same whether the tests run from a terminal or
/// in CI.
pub(crate) fn shipyard(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shipyard"));
    command.args(args);
    // SAFETY: `setsid` is async-signal-safe, as `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// How long a leftover shipyard gets to stop after SIGTERM before it's
/// killed: longer than the default `--stop-timeout` (10 s).
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(15);
/// How long a process gets to disappear once it should be gone.
pub(crate) const GONE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Run {
    /// The exit code; `None` if shipyard itself was killed by a signal.
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Runs shipyard to completion, killing it if it takes longer than `TIMEOUT`.
pub(crate) fn run(mut command: Command) -> Run {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("starting shipyard");
    let pid = libc::pid_t::try_from(child.id()).expect("pid fits in pid_t");
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let Ok(output) = finished.recv_timeout(TIMEOUT) else {
        // SAFETY: `pid` is the shipyard process this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("shipyard did not finish within {TIMEOUT:?}");
    };
    let output = output.expect("waiting for shipyard");
    Run {
        code: output.status.code(),
        signal: output.status.signal(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

pub(crate) fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// A running shipyard whose stdout is read line by line.
pub(crate) struct Running {
    shipyard: Child,
    lines: Receiver<String>,
}

/// Starts `command` (a shipyard), reading its stdout line by line.
pub(crate) fn start_command(mut command: Command) -> Running {
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    let mut shipyard = command.spawn().expect("starting shipyard");
    let stdout = shipyard.stdout.take().unwrap();
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    Running { shipyard, lines }
}

impl Running {
    pub(crate) fn pid(&self) -> libc::pid_t {
        libc::pid_t::try_from(self.shipyard.id()).unwrap()
    }

    /// The next line the command prints.
    pub(crate) fn next_line(&self) -> String {
        self.lines
            .recv_timeout(TIMEOUT)
            .expect("the command printed nothing")
    }

    pub(crate) fn signal(&self, signal: libc::c_int) {
        // SAFETY: `pid` is the shipyard process this test started.
        assert_eq!(unsafe { libc::kill(self.pid(), signal) }, 0);
    }

    /// Waits for shipyard to exit; returns its exit code and the rest of the
    /// command's output.
    pub(crate) fn finish(mut self) -> (i32, Vec<String>) {
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.shipyard.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.shipyard.kill();
                panic!("shipyard did not finish within {TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut rest = Vec::new();
        // Lines still in flight; leftover background processes may keep the
        // pipe open, so stop after a quiet second rather than at end-of-file.
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(1)) {
            rest.push(line);
        }
        let code = status.code().expect("shipyard exited normally");
        (code, rest)
    }
}

/// If a test fails half-way, its shipyard must not outlive it: a leftover
/// shipyard keeps the test's output pipes open and the test run never ends.
impl Drop for Running {
    fn drop(&mut self) {
        if matches!(self.shipyard.try_wait(), Ok(Some(_))) {
            return;
        }
        // SIGTERM first, so shipyard stops and cleans up the command too.
        // SAFETY: `kill` only sends a signal to the process this test started.
        unsafe { libc::kill(self.pid(), libc::SIGTERM) };
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if matches!(self.shipyard.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.shipyard.kill();
        let _ = self.shipyard.wait();
    }
}

/// Waits up to `GONE_TIMEOUT` for `pid` to be gone (not running, not a zombie).
pub(crate) fn gone(pid: libc::pid_t) -> bool {
    let deadline = Instant::now() + GONE_TIMEOUT;
    while Instant::now() < deadline {
        // SAFETY: `kill` with signal 0 only checks whether the process exists.
        if unsafe { libc::kill(pid, 0) } == -1 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// The PID of `parent`'s child, once it has one (`pgrep -P`).
pub(crate) fn child_of(parent: libc::pid_t) -> libc::pid_t {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let output = Command::new("pgrep")
            .args(["-P", &parent.to_string()])
            .output()
            .expect("running pgrep");
        if let Some(pid) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().parse().ok())
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "process {parent} never had a child"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
