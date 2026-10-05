//! End-to-end tests of the supervisor: forwarding signals, stopping, and
//! cleaning up after the command.
//!
//! Each test starts a fresh `shipyard` in a new session without a terminal,
//! so the command gets its own process group, as in CI or under a service
//! manager. Tests synchronise on output, not on time: the command prints
//! `ready` once its traps are set, and the test signals only after reading
//! that line. (Sleeps appear only as generous upper bounds.)
//! Scripts wait with `sleep 100 & wait`, because a shell runs traps between
//! commands, so a foreground `sleep` would delay them.
//!
//! Runs use `--no-isolation`: these test process behaviour that's the same
//! with or without namespaces, without needing root. `isolation.rs` tests
//! the namespaces.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::time::{Duration, Instant};

use shipyard_test_support::{Running, Shipyard, block, gone, ignore, killed_by, start_command};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

fn start(args: &[&str]) -> Running {
    start_with(args, || Ok(()))
}

/// Starts shipyard, with `setup` applied to its process just before it runs.
fn start_with(args: &[&str], setup: fn() -> std::io::Result<()>) -> Running {
    start_command(SHIPYARD.run_unisolated_with(args, setup))
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
    // The command sends shipyard USR1, then the test sends USR2. Shipyard
    // handles pending signals in numeric order (USR1 before USR2), so a
    // bounced USR1 would be printed before the sentinel, never after.
    let run = start(&[
        "sh",
        "-c",
        "trap 'echo bounced' USR1; trap 'echo sentinel; kill $!; exit 0' USR2; \
         kill -USR1 $PPID; echo sent; while :; do sleep 100 & wait; done",
    ]);
    assert_eq!(run.next_line(), "sent");
    // macOS keeps one sender slot per process: with USR1 and USR2 pending in
    // shipyard at the same time, the test's PID can overwrite the command's,
    // and shipyard would bounce the USR1 (a documented macOS limitation, not
    // what this tests). Give it time to take USR1 first. Linux needs no pause.
    if cfg!(target_os = "macos") {
        std::thread::sleep(Duration::from_millis(300));
    }
    run.signal(libc::SIGUSR2);
    assert_eq!(
        run.next_line(),
        "sentinel",
        "the command's own USR1 came back"
    );
    let (code, rest) = run.finish();
    assert_eq!(code, 0);
    assert!(rest.is_empty(), "{rest:?}");
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

#[test]
fn a_command_paused_and_resumed_by_someone_else_carries_on() {
    // On macOS, `waitid` reports a stopped child as if it had exited; taken
    // at face value, shipyard killed the paused command (exit 137).
    let run = start(&["sh", "-c", "echo pid $$; sleep 2; echo done"]);
    let command: libc::pid_t = run
        .next_line()
        .strip_prefix("pid ")
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: `kill` only sends signals to the command this test started.
    unsafe {
        assert_eq!(libc::kill(command, libc::SIGSTOP), 0);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(libc::kill(command, libc::SIGCONT), 0);
    }
    let (code, lines) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(lines, ["done"]);
}

#[test]
fn a_sigterm_ignored_at_startup_still_stops_the_command() {
    // SIGTERM is the stop request: even a parent that ignored it mustn't
    // make shipyard (and so the command) unstoppable.
    let run = start_with(
        &[
            "sh",
            "-c",
            "trap 'exit 5' TERM; echo ready; sleep 100 & wait",
        ],
        || ignore(libc::SIGTERM),
    );
    assert_eq!(run.next_line(), "ready");
    run.signal(libc::SIGTERM);
    assert_eq!(run.finish().0, 5);
}
