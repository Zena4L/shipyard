//! End-to-end tests of shipyard on a terminal: Ctrl-C, Ctrl-\, Ctrl-Z,
//! `fg`/`bg`, hangup, and signals sent with `kill`.
//!
//! Each test starts a fake interactive shell on a pseudo-terminal (see
//! `pty/mod.rs`), which runs shipyard as a job, and then types keys into the
//! terminal like a user would.
//!
//! **This binary must stay single-threaded.** The fake shell is forked from it
//! and keeps running ordinary Rust code. After `fork` in a multi-threaded
//! process, only async-signal-safe calls are allowed (a lock held by another
//! thread stays locked forever in the copy). That's why these tests don't use
//! the standard test harness, which runs tests on threads: `main` below runs
//! them one by one. Never start a thread here.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test binary, a panic is a test failure with a clear message"
)]

// Each test binary uses only part of the shared helpers.
#[allow(dead_code, reason = "only `killed_by` is used here")]
mod common;
mod pty;

use std::io::Write as _;
use std::process::ExitCode;

use common::killed_by;
use pty::{Mode, Session, Shell, Status, TERM, process_state, wait_until_stopped};

const CTRL_C: &[u8] = b"\x03";
const CTRL_BACKSLASH: &[u8] = b"\x1c";
const CTRL_Z: &[u8] = b"\x1a";

/// `[(name, function), …]` from a list of test functions, so a name can't
/// drift from its function.
macro_rules! tests {
    ($($test:ident),* $(,)?) => {
        &[$((stringify!($test), $test as fn())),*]
    };
}

/// Every test, by name.
const TESTS: &[(&str, fn())] = tests![
    ctrl_c_reaches_the_command_exactly_once,
    ctrl_c_ends_shipyard_by_sigint,
    ctrl_backslash_quits_the_command,
    ctrl_z_stops_both_and_fg_resumes,
    bg_resumes_a_stopped_job,
    a_background_job_reading_the_terminal_waits_for_fg,
    a_command_that_stops_itself_stops_shipyard_too,
    continuing_shipyard_alone_continues_the_command,
    a_stop_request_still_kills_a_command_that_stops_itself,
    without_job_control_shipyard_does_not_stop_itself,
    hangup_ends_the_command_and_shipyard,
    kill_to_a_background_shipyard_is_forwarded,
    kill_to_a_foreground_shipyard,
    term_is_passed_through_on_a_terminal,
    the_command_can_read_the_terminal,
];

/// libtest options that take a value; their value is skipped so it isn't
/// taken for a name filter.
const OPTIONS_WITH_VALUES: &[&str] = &[
    "--test-threads",
    "--color",
    "--format",
    "--logfile",
    "--shuffle-seed",
    "-Z",
];

/// The tests to run, as libtest would pick them from `args`: names
/// containing any filter (or equal to one, with `--exact`), minus `--skip`
/// matches. These tests have no ignored ones, so `--ignored` picks none.
/// Other libtest options are accepted and ignored, so
/// `cargo test -- --include-ignored` works.
fn select(args: &[String]) -> Vec<&'static (&'static str, fn())> {
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--skip" => skips.extend(rest.next()),
            option if OPTIONS_WITH_VALUES.contains(&option) => {
                rest.next();
            }
            option if option.starts_with('-') => {}
            filter => filters.push(filter),
        }
    }
    let exact = args.iter().any(|arg| arg == "--exact");
    let only_ignored = args.iter().any(|arg| arg == "--ignored");
    let matches = |name: &str, pattern: &str| {
        if exact {
            name == pattern
        } else {
            name.contains(pattern)
        }
    };
    TESTS
        .iter()
        .filter(|(name, _)| {
            !only_ignored
                && (filters.is_empty() || filters.iter().any(|f| matches(name, f)))
                && !skips.iter().any(|skip| matches(name, skip))
        })
        .collect()
}

/// Runs the selected tests in order, printing results the way libtest does.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let selected = select(&args);
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in &selected {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    pty::kill_everything_on_alarm();

    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in &selected {
        // The name goes out first, so a hang shows which test it was.
        print!("test {name} ... ");
        let _ = std::io::stdout().flush();
        // Last resort against a hang: if one test runs for more than 90
        // seconds (each wait inside has its own, much shorter deadline),
        // SIGALRM kills its processes and this binary.
        // SAFETY: `alarm` has no preconditions.
        unsafe { libc::alarm(90) };
        let passed = std::panic::catch_unwind(test).is_ok();
        println!("{}", if passed { "ok" } else { "FAILED" });
        if !passed {
            failed.push(*name);
        }
    }
    if !failed.is_empty() {
        println!("\nfailures:");
        for name in &failed {
            println!("    {name}");
        }
    }
    let result = if failed.is_empty() { "ok" } else { "FAILED" };
    println!(
        "\ntest result: {result}. {} passed; {} failed; 0 ignored; 0 measured; {} filtered out\n",
        selected.len() - failed.len(),
        failed.len(),
        TESTS.len() - selected.len(),
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Starts `shipyard run sh -c SCRIPT` in the foreground and waits for it.
macro_rules! foreground_sh {
    ($script:expr) => {
        |shell: &mut Shell| {
            let job = shell.spawn(Mode::Foreground, &["run", "sh", "-c", $script]);
            shell.wait(job);
        }
    };
}

// --- Keys typed at the terminal ---

/// The terminal delivers Ctrl-C to the command itself; shipyard must not
/// pass it on a second time. (Before step 4's review, macOS got it twice.)
///
/// The command's trap can't count deliveries: two SIGINTs arriving
/// microseconds apart merge into one pending signal. So shipyard runs with
/// `--debug`, which logs every signal it forwards, and the log must show
/// none.
///
/// With `--debug`, shipyard also logs the command's arguments. So the
/// markers the test waits for are split with `""` in the script: `re""ady`
/// prints `ready`, but the logged arguments never contain `ready`.
fn ctrl_c_reaches_the_command_exactly_once() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "sh",
                "-c",
                "trap 'echo got-\"\"int; kill $!' INT; echo re\"\"ady; sleep 5 & wait; echo e\"\"nd",
            ],
        );
        shell.wait(job);
    });
    session.job();
    // Positive evidence that the debug log is on, so its silence about
    // forwarding below means something.
    session.expect("sharing shipyard's process group");
    session.expect("ready");
    session.type_keys(CTRL_C);
    session.expect("got-int");
    session.expect("end");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(session.count("got-int"), 1, "{}", session.transcript());
    assert_eq!(
        session.count("forwarding a signal"),
        0,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// When Ctrl-C kills the command, shipyard dies of SIGINT too, so the shell
/// sees an interrupted job (and stops a loop around it).
fn ctrl_c_ends_shipyard_by_sigint() {
    let mut session = Session::start(foreground_sh!("echo ready; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_C);
    assert_eq!(session.status(), Status::Signaled(libc::SIGINT));
    session.finish();
}

/// Ctrl-\ sends SIGQUIT to the command; shipyard reports the death as
/// 128 + 3.
fn ctrl_backslash_quits_the_command() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--ulimit",
                "core=0",
                "sh",
                "-c",
                "echo ready; exec sleep 100",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_BACKSLASH);
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGQUIT)));
    session.finish();
}

// --- Job control ---

/// Ctrl-Z stops shipyard and the command together; `fg` resumes both, and
/// the command still owns the terminal.
fn ctrl_z_stops_both_and_fg_resumes() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "sh",
                "-c",
                "echo pid $$; read line; echo \"got $line\"",
            ],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    let shipyard = session.job();
    let command: libc::pid_t = session.expect_line_starting("pid ").parse().unwrap();
    session.type_keys(CTRL_Z);
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    wait_until_stopped(shipyard);
    wait_until_stopped(command);
    session.go();
    session.type_keys(b"hello\n");
    session.expect("got hello");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// `bg` resumes a stopped job without the terminal; it runs to the end.
fn bg_resumes_a_stopped_job() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "sh", "-c", "echo ready; sleep 3; echo done"],
        );
        shell.wait(job);
        shell.bg(job);
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_Z);
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    session.expect("done");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A background job that reads the terminal is stopped by SIGTTIN (shipyard
/// with it, since they share a group) until `fg` gives it the terminal.
fn a_background_job_reading_the_terminal_waits_for_fg() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &["run", "sh", "-c", "read x; echo \"got $x\""],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    session.job();
    assert_eq!(session.status(), Status::Stopped(libc::SIGTTIN));
    session.go();
    session.type_keys(b"x\n");
    session.expect("got x");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A command that stops only itself makes shipyard stop too, so the shell
/// sees a stopped job instead of a frozen terminal; `fg` resumes both.
fn a_command_that_stops_itself_stops_shipyard_too() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "sh", "-c", "echo ready; kill -STOP $$; echo resumed"],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    assert_eq!(session.status(), Status::Stopped(libc::SIGSTOP));
    session.go();
    session.expect("resumed");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// When only shipyard is continued, it continues the command as well.
fn continuing_shipyard_alone_continues_the_command() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "sh", "-c", "echo ready; kill -STOP $$; echo resumed"],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg_shipyard_only(job);
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    assert_eq!(session.status(), Status::Stopped(libc::SIGSTOP));
    session.go();
    session.expect("resumed");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// Closing the terminal: the shell sends the job SIGHUP, which ends the
/// command, and shipyard exits with 128 + 1 instead of lingering.
fn hangup_ends_the_command_and_shipyard() {
    let mut session = Session::start(foreground_sh!("echo ready; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGHUP)));
    session.finish();
}

// --- Signals sent with `kill` ---

/// A signal sent to a background shipyard's PID reaches the command.
fn kill_to_a_background_shipyard_is_forwarded() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &[
                "run",
                "sh",
                "-c",
                "trap 'echo got INT; kill $!; exit 3' INT; echo ready; sleep 100 & wait",
            ],
        );
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` only sends a signal to the shipyard this test started.
    assert_eq!(unsafe { libc::kill(shipyard, libc::SIGINT) }, 0);
    session.expect("got INT");
    assert_eq!(session.status(), Status::Exited(3));
    session.finish();
}

/// A `kill -INT` to a *foreground* shipyard: Linux tells it apart from a
/// typed Ctrl-C and forwards it; macOS can't, so it isn't forwarded there
/// (documented in docs/user/run.md). USR1, sent second, is forwarded on both
/// and marks the point by which a forwarded INT would have shown up.
fn kill_to_a_foreground_shipyard() {
    let mut session = Session::start(foreground_sh!(
        "trap 'echo got-int; kill $!' INT; trap 'echo got-usr1; kill $!; exit 0' USR1; echo ready; \
         while :; do sleep 100 & wait; done"
    ));
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` only sends signals to the shipyard this test started.
    unsafe {
        assert_eq!(libc::kill(shipyard, libc::SIGINT), 0);
        assert_eq!(libc::kill(shipyard, libc::SIGUSR1), 0);
    }
    session.expect("got-usr1");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(
        session.count("got-int"),
        usize::from(cfg!(target_os = "linux")),
        "{}",
        session.transcript()
    );
    session.finish();
}

// --- The terminal itself ---

/// On a terminal, the host's `TERM` is passed on to the command.
fn term_is_passed_through_on_a_terminal() {
    let mut session = Session::start(foreground_sh!("echo \"term=$TERM\""));
    session.job();
    session.expect(&format!("term={TERM}"));
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The command can read the terminal: it's in the foreground group with
/// shipyard, so reading doesn't stop it.
fn the_command_can_read_the_terminal() {
    let mut session = Session::start(foreground_sh!("read x; echo \"got $x\""));
    session.job();
    session.type_keys(b"hi\n");
    session.expect("got hi");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A command that answers the stop request by stopping itself must still be
/// killed when `--stop-timeout` runs out. Shipyard mustn't follow it into a
/// stop: stopped, it couldn't send the SIGKILL.
fn a_stop_request_still_kills_a_command_that_stops_itself() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--stop-timeout",
                "1",
                "sh",
                "-c",
                // A short foreground sleep, so the trap runs at once.
                "trap 'kill -STOP $$' TERM; echo ready; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` only sends a signal to the shipyard this test started.
    assert_eq!(unsafe { libc::kill(shipyard, libc::SIGTERM) }, 0);
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGKILL)));
    session.finish();
}

/// With no job-control shell above it, nothing would ever resume a stopped
/// shipyard: it must not follow the command into a stop.
///
/// The setup: an intermediate shell starts shipyard in the background and
/// exits, so shipyard's parent is gone and its process group is orphaned.
/// The fake shell leaves that group in the foreground (a background one
/// can't use the terminal at all on macOS, and shipyard would then run
/// without one), and stays alive until the end so the terminal does too.
fn without_job_control_shipyard_does_not_stop_itself() {
    let mut session = Session::start(|shell| {
        let job = shell.spawn_program(
            Mode::Foreground,
            "sh",
            &[
                "-c",
                concat!(
                    "'",
                    env!("CARGO_BIN_EXE_shipyard"),
                    "' --debug run sh -c 'sleep 0.5; kill -STOP $$' & echo \"shipyard $!\""
                ),
            ],
        );
        shell.wait_leaving_terminal(job);
        shell.await_go();
    });
    session.job();
    let shipyard: libc::pid_t = session.expect_line_starting("shipyard ").parse().unwrap();
    assert_eq!(session.status(), Status::Exited(0));
    // The scenario only means something with a terminal.
    session.expect("sharing shipyard's process group");
    let command = child_of(shipyard);
    wait_until_stopped(command);
    // Give a (wrong) self-stop time to happen.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let state = process_state(shipyard);
    // SAFETY: `kill` only sends signals to processes this test started.
    unsafe {
        libc::kill(shipyard, libc::SIGKILL);
        libc::kill(command, libc::SIGKILL);
    }
    assert!(
        !state.starts_with('T'),
        "shipyard stopped itself: {state:?}"
    );
    session.go();
    session.finish();
}

/// The PID of `parent`'s child, once it has one (`pgrep -P`).
fn child_of(parent: libc::pid_t) -> libc::pid_t {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let output = std::process::Command::new("pgrep")
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
            std::time::Instant::now() < deadline,
            "process {parent} never had a child"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
