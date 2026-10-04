//! End-to-end tests of the supervisor: forwarding signals, stopping, and
//! cleaning up after the command.
//!
//! Each test starts a fresh `shipyard` in a new session without a terminal,
//! so the command gets its own process group, as in CI or under a service
//! manager. No test sleeps to synchronise: the command prints `ready` once its
//! traps are set, and the test signals only after reading that line.
//! Scripts wait with `sleep 100 & wait`, because a shell runs traps between
//! commands, so a foreground `sleep` would delay them.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use common::{TIMEOUT, block, ignore, killed_by, shipyard};

/// A running shipyard whose stdout is read line by line.
struct Running {
    shipyard: Child,
    lines: Receiver<String>,
}

fn start(args: &[&str]) -> Running {
    start_with(args, || Ok(()))
}

/// Starts shipyard, with `setup` applied to its process just before it runs.
fn start_with(args: &[&str], setup: fn() -> std::io::Result<()>) -> Running {
    let mut command = shipyard(&[&["run"], args].concat());
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    // SAFETY: every `setup` below only makes async-signal-safe calls.
    unsafe { command.pre_exec(setup) };
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
    fn pid(&self) -> libc::pid_t {
        libc::pid_t::try_from(self.shipyard.id()).unwrap()
    }

    /// The next line the command prints.
    fn next_line(&self) -> String {
        self.lines
            .recv_timeout(TIMEOUT)
            .expect("the command printed nothing")
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: `pid` is the shipyard process this test started.
        assert_eq!(unsafe { libc::kill(self.pid(), signal) }, 0);
    }

    /// Waits for shipyard to exit; returns its exit code and the rest of the
    /// command's output.
    fn finish(mut self) -> (i32, Vec<String>) {
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
        let deadline = Instant::now() + Duration::from_secs(15);
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

#[test]
fn signals_sent_to_shipyard_are_forwarded() {
    for (signal, name) in [(libc::SIGUSR1, "USR1"), (libc::SIGINT, "INT")] {
        let run = start(&[
            "sh",
            "-c",
            &format!("trap 'exit 42' {name}; echo ready; sleep 100 & wait"),
        ]);
        assert_eq!(run.next_line(), "ready");
        run.signal(signal);
        let (code, _) = run.finish();
        assert_eq!(code, 42, "SIG{name} was not forwarded");
    }
}

#[test]
fn sigterm_sends_the_stop_signal() {
    let run = start(&[
        "--stop-signal",
        "USR2",
        "sh",
        "-c",
        "trap 'exit 7' USR2; echo ready; sleep 100 & wait",
    ]);
    assert_eq!(run.next_line(), "ready");
    run.signal(libc::SIGTERM);
    assert_eq!(run.finish().0, 7);
}

#[test]
fn a_command_that_ignores_the_stop_signal_is_killed_after_the_timeout() {
    let run = start(&[
        "--stop-timeout",
        "1",
        "sh",
        "-c",
        "trap '' TERM; echo ready; sleep 100 & wait",
    ]);
    assert_eq!(run.next_line(), "ready");
    let asked = Instant::now();
    run.signal(libc::SIGTERM);
    let (code, _) = run.finish();
    let waited = asked.elapsed();
    assert_eq!(code, killed_by(libc::SIGKILL));
    assert!(
        waited >= Duration::from_millis(900),
        "killed too early: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "killed too late: {waited:?}"
    );
}

#[test]
fn signals_ignored_when_shipyard_started_are_not_forwarded() {
    let run = start_with(
        &[
            "sh",
            "-c",
            "trap 'echo hup' HUP; trap 'echo usr1' USR1; echo ready; \
             while :; do sleep 100 & wait; done",
        ],
        || ignore(libc::SIGHUP), // as `nohup` does
    );
    assert_eq!(run.next_line(), "ready");
    run.signal(libc::SIGHUP);
    // A sentinel: USR1 is forwarded, so its line proves the HUP (sent first)
    // would have shown up before it if it had been forwarded too.
    run.signal(libc::SIGUSR1);
    assert_eq!(run.next_line(), "usr1", "SIGHUP was forwarded");
    run.signal(libc::SIGTERM);
    assert_eq!(run.finish().0, killed_by(libc::SIGTERM));
}

#[test]
fn signals_from_the_command_are_not_bounced_back() {
    let run = start(&[
        "sh",
        "-c",
        "trap 'echo bounced' USR1; kill -USR1 $PPID; sleep 1; echo done",
    ]);
    let (code, lines) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(lines, ["done"]);
}

/// Waits up to 5 seconds for `pid` to be gone (not running, not a zombie).
fn gone(pid: libc::pid_t) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        // SAFETY: `kill` with signal 0 only checks whether the process exists.
        if unsafe { libc::kill(pid, 0) } == -1 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn processes_left_behind_are_cleaned_up() {
    let run = start(&["sh", "-c", "sleep 300 & echo $!"]);
    let leftover: libc::pid_t = run.next_line().parse().unwrap();
    assert_eq!(run.finish().0, 0);
    assert!(
        gone(leftover),
        "background process {leftover} outlived the command"
    );
}

/// On Linux even a process that left the command's group (`setsid`) is found,
/// because shipyard is a subreaper. On macOS that one would leak.
#[cfg(target_os = "linux")]
#[test]
fn escaped_processes_are_cleaned_up_on_linux() {
    let run = start(&["sh", "-c", "setsid sleep 300 & echo $!"]);
    let escaped: libc::pid_t = run.next_line().parse().unwrap();
    assert_eq!(run.finish().0, 0);
    assert!(
        gone(escaped),
        "setsid process {escaped} outlived the command"
    );
}

#[test]
fn a_blocked_sigterm_still_stops_the_command() {
    // A parent that blocked SIGTERM must not make shipyard unstoppable.
    let run = start_with(
        &[
            "sh",
            "-c",
            "trap 'exit 5' TERM; echo ready; sleep 100 & wait",
        ],
        || block(libc::SIGTERM),
    );
    assert_eq!(run.next_line(), "ready");
    run.signal(libc::SIGTERM);
    assert_eq!(run.finish().0, 5);
}
