//! End-to-end tests of `shipyard run`, driving the real binary.
//!
//! Each test starts `shipyard` as its own process: spawning changes
//! process-wide state (signal handling, waiting for children), so it can't be
//! exercised inside the test harness itself. Every run has a hard timeout.
//!
//! "Hostile parent" tests start shipyard in a deliberately bad state (leaked
//! descriptors, ignored or blocked signals) and check that the command still
//! starts clean.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

mod common;

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use common::{TIMEOUT, block, ignore, killed_by, shipyard};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Executable files that aren't valid programs. Committed (with their mode)
/// rather than written during the test: a file written while other test
/// threads fork can make exec fail with "text file busy".
const NOT_A_PROGRAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/not-a-program");
const BAD_INTERPRETER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bad-interpreter"
);

struct Run {
    /// The exit code; `None` if shipyard itself was killed by a signal.
    code: Option<i32>,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs shipyard to completion, killing it if it takes longer than `TIMEOUT`.
fn run(mut command: Command) -> Run {
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

fn shipyard_run(args: &[&str]) -> Run {
    run(shipyard(&[&["run"], args].concat()))
}

fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn exit_codes_pass_through() {
    for code in [0, 3, 255] {
        let run = shipyard_run(&["sh", "-c", &format!("exit {code}")]);
        assert_eq!(run.code, Some(code), "{}", run.stderr);
    }
}

#[test]
fn death_by_signal_reports_128_plus_n() {
    for (name, number) in [("TERM", libc::SIGTERM), ("USR1", libc::SIGUSR1)] {
        let run = shipyard_run(&["sh", "-c", &format!("kill -{name} $$")]);
        assert_eq!(
            run.code,
            Some(killed_by(number)),
            "SIG{name}: {}",
            run.stderr
        );
    }
}

#[test]
fn sigpipe_is_back_to_default() {
    // Rust ignores SIGPIPE in shipyard itself; the command must not inherit that.
    let run = shipyard_run(&["sh", "-c", "kill -PIPE $$"]);
    assert_eq!(run.code, Some(killed_by(libc::SIGPIPE)), "{}", run.stderr);
}

#[test]
fn a_missing_command_is_127() {
    let run = shipyard_run(&["no-such-command-9f3a"]);
    assert_eq!(run.code, Some(127));
    assert!(run.stderr.contains("command not found"), "{}", run.stderr);
}

#[test]
fn a_missing_interpreter_is_127_and_says_so() {
    let run = shipyard_run(&[BAD_INTERPRETER]);
    assert_eq!(run.code, Some(127));
    assert!(run.stderr.contains("interpreter"), "{}", run.stderr);
}

#[test]
fn files_that_cannot_run_are_126() {
    let cases = [
        ("/etc/hosts", "permission denied"),
        ("/tmp", "is a directory"),
        (NOT_A_PROGRAM, "not a program this system can run"),
    ];
    for (path, message) in cases {
        let run = shipyard_run(&[path]);
        assert_eq!(run.code, Some(126), "{path}: {}", run.stderr);
        assert!(run.stderr.contains(message), "{path}: {}", run.stderr);
    }
}

#[test]
fn a_missing_working_directory_is_125() {
    let run = shipyard_run(&["-w", "/no/such/dir", "true"]);
    assert_eq!(run.code, Some(125));
    assert!(
        run.stderr.contains("changing directory to /no/such/dir"),
        "{}",
        run.stderr
    );
}

#[test]
fn the_environment_is_exactly_what_was_asked_for() {
    // `env` directly, not through a shell, which would add its own variables.
    let run = shipyard_run(&["-e", "A=1", "-e", "EMPTY=", "env"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mut lines: Vec<&str> = run.stdout.lines().collect();
    lines.sort_unstable();
    let home = if is_root() { "HOME=/root" } else { "HOME=/" };
    let path = format!("PATH={DEFAULT_PATH}");
    let mut expected = vec!["A=1", "EMPTY=", home, &path];
    expected.sort_unstable();
    assert_eq!(lines, expected);
}

#[test]
fn the_working_directory_is_applied() {
    let run = shipyard_run(&["-w", "/tmp", "pwd", "-P"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let tmp = std::fs::canonicalize("/tmp").unwrap();
    assert_eq!(run.stdout.trim(), tmp.to_str().unwrap());
}

#[test]
fn resource_limits_are_applied() {
    // Only lower limits: raising them as non-root would fail.
    let run = shipyard_run(&["--ulimit", "nofile=64:128", "sh", "-c", "ulimit -n"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "64");
}

#[test]
fn the_umask_defaults_to_022() {
    let run = shipyard_run(&["sh", "-c", "umask"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    // Shells print it as 0022 or 022.
    assert_eq!(u32::from_str_radix(run.stdout.trim(), 8), Ok(0o022));
}

#[test]
fn switching_user_without_root_is_refused_up_front() {
    if is_root() {
        return;
    }
    let run = shipyard_run(&["-u", "12345", "true"]);
    assert_eq!(run.code, Some(125));
    assert!(run.stderr.contains("user 12345 differs"), "{}", run.stderr);
}

#[test]
#[ignore = "needs root; CI runs it with sudo (the `root` job)"]
fn root_switches_user_and_drops_its_own_groups() {
    assert!(is_root(), "run with --include-ignored as root");
    let run = shipyard_run(&[
        "-u",
        "1000:1000",
        "--group-add",
        "20",
        "sh",
        "-c",
        "id -u; id -G",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mut lines = run.stdout.lines();
    assert_eq!(lines.next(), Some("1000"));
    let mut groups: Vec<&str> = lines.next().unwrap_or_default().split(' ').collect();
    groups.sort_unstable();
    assert_eq!(groups, ["1000", "20"], "root's own groups must be gone");
}

// --- A hostile parent: shipyard is started in a deliberately bad state. ---

/// Starts shipyard with `setup` applied to its process just before it runs.
fn hostile(args: &[&str], setup: fn() -> std::io::Result<()>) -> Run {
    let mut command = shipyard(&[&["run"], args].concat());
    // SAFETY: each `setup` below only makes async-signal-safe libc calls,
    // which is what `pre_exec` requires.
    unsafe { command.pre_exec(setup) };
    run(command)
}

#[test]
fn leaked_descriptors_do_not_reach_the_command() {
    let run = hostile(
        &[
            "sh",
            "-c",
            "if : <&9 2>/dev/null; then echo open; else echo closed; fi",
        ],
        || {
            // A descriptor without close-on-exec, as a careless parent would
            // leave.
            // SAFETY: `dup2` is async-signal-safe.
            if unsafe { libc::dup2(2, 9) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        },
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "closed\n", "fd 9 leaked into the command");
}

#[test]
fn ignored_signals_are_reset_for_the_command() {
    // A shell can't un-ignore a signal ignored at startup, so if the reset
    // were missing, `kill -USR1` would do nothing and sh would exit 0.
    let run = hostile(&["sh", "-c", "kill -USR1 $$"], || ignore(libc::SIGUSR1));
    assert_eq!(run.code, Some(killed_by(libc::SIGUSR1)), "{}", run.stderr);
}

#[test]
fn blocked_signals_are_unblocked_for_the_command() {
    let run = hostile(&["sh", "-c", "kill -TERM $$"], || block(libc::SIGTERM));
    assert_eq!(run.code, Some(killed_by(libc::SIGTERM)), "{}", run.stderr);
}

#[test]
fn an_ignored_sigchld_does_not_lose_the_exit_status() {
    // With SIGCHLD ignored, Linux discards the child's exit status (auto-reaps
    // it). macOS doesn't, so this test can only fail on Linux, where CI runs it.
    let run = hostile(&["sh", "-c", "exit 7"], || ignore(libc::SIGCHLD));
    assert_eq!(run.code, Some(7), "{}", run.stderr);
}

#[test]
fn a_blocked_sigchld_does_not_hang_shipyard() {
    // Shipyard learns that the command exited from SIGCHLD; inherited
    // blocked, it would wait forever. The sleep makes the exit happen while
    // shipyard is already waiting. (The loop's periodic check would still
    // notice within a second; `a_blocked_sigterm_still_stops_the_command`
    // is the test that proves caught signals get unblocked.)
    let run = hostile(&["sh", "-c", "sleep 0.2; exit 3"], || block(libc::SIGCHLD));
    assert_eq!(run.code, Some(3), "{}", run.stderr);
}

#[test]
fn nohup_still_protects_the_command() {
    // Started as `nohup` would start it: the command keeps ignoring SIGHUP,
    // so a hangup can't kill it.
    let run = hostile(&["sh", "-c", "kill -HUP $$; echo survived"], || {
        ignore(libc::SIGHUP)
    });
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "survived\n");
}

#[test]
fn shipyard_dies_from_sigint_when_the_command_did() {
    // So a shell loop around `shipyard run` stops on Ctrl-C.
    let run = shipyard_run(&["sh", "-c", "kill -INT $$"]);
    assert_eq!(run.signal, Some(libc::SIGINT), "{}", run.stderr);
}

#[test]
fn ignoring_sigint_is_respected_like_a_background_job_in_a_script() {
    // `cmd &` in a script starts cmd with SIGINT ignored: Ctrl-C must
    // neither kill the command nor make shipyard re-raise SIGINT on itself.
    let run = hostile(&["sh", "-c", "kill -INT $$; echo survived"], || {
        ignore(libc::SIGINT)
    });
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "survived\n");
}

#[test]
fn shipyard_does_not_re_raise_a_sigint_it_was_told_to_ignore() {
    // The command undoes the inherited "ignore" and dies of SIGINT; shipyard,
    // still ignoring it, must exit 130 rather than force its own death.
    let run = hostile(
        &[
            "perl",
            "-e",
            "$SIG{INT} = 'DEFAULT'; kill 'INT', $$; sleep 5",
        ],
        || ignore(libc::SIGINT),
    );
    assert_eq!(run.signal, None, "{}", run.stderr);
    assert_eq!(run.code, Some(killed_by(libc::SIGINT)), "{}", run.stderr);
}
