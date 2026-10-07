//! End-to-end tests of shipyard on a terminal: Ctrl-C, Ctrl-\, Ctrl-Z,
//! `fg`/`bg`, hangup, and signals sent with `kill`.
//!
//! Each test starts a fake interactive shell on a pseudo-terminal (see
//! `shipyard_test_support::pty`), which runs shipyard as a job, and then types keys into the
//! terminal like a user would.
//!
//! **This binary must stay single-threaded.** The fake shell is forked from it
//! and keeps running ordinary Rust code. After `fork` in a multi-threaded
//! process, only async-signal-safe calls are allowed (a lock held by another
//! thread stays locked forever in the copy). That's why these tests don't use
//! the standard test harness, which runs tests on threads: `main` below runs
//! them one by one. Never start a thread here.
//!
//! Most runs use `--no-isolation`: they test process behaviour that's the
//! same with or without namespaces. The isolated ones ([`ISOLATED_TESTS`],
//! Linux) test what a session of its own changes for the command: it's
//! never in the terminal's foreground group and has no controlling
//! terminal, so keys, window-size changes and hangups reach it only through
//! shipyard. They run as root (the root passes, with `--include-ignored`),
//! and as a normal user too wherever rootless containers work: then the
//! container is rootless, in a user namespace. `isolation.rs` tests the
//! namespaces themselves.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test binary, a panic is a test failure with a clear message"
)]

use std::io::Write as _;
use std::process::ExitCode;

use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::pty::{
    Mode, Session, Shell, Status, TERM, process_state, wait_until_stopped,
};
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Shipyard, child_of, is_root, killed_by};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));
const RUNTIME: &str = env!("CARGO_BIN_EXE_shipyard-runtime");

/// A fake shell on a new pseudo-terminal, running `script` (see
/// `shipyard_test_support::pty`).
fn session(script: fn(&mut Shell)) -> Session {
    // SAFETY: this binary is single-threaded (see the top of the file).
    unsafe { Session::start(SHIPYARD.path(), script) }
}

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
    window_size_changes_reach_the_command_once,
];

/// Tests of isolated runs (Linux, for namespaces). As root, like libtest's
/// `#[ignore = "needs root"]`, they run only with `--ignored` or
/// `--include-ignored`, as the root passes do. As a normal user, they run
/// whenever rootless containers work here (see [`select`]).
const ISOLATED_TESTS: &[(&str, fn())] = tests![
    isolated_ctrl_c_is_forwarded_exactly_once,
    isolated_ctrl_c_ends_shipyard_by_sigint,
    isolated_ctrl_backslash_quits_the_command,
    isolated_ctrl_z_stops_only_shipyard,
    an_isolated_command_that_stops_itself_resumes_with_ctrl_z_and_fg,
    isolated_command_reads_the_terminal,
    a_background_isolated_command_still_reads_the_terminal,
    isolated_window_size_changes_reach_the_command,
    isolated_command_has_no_controlling_terminal,
    isolated_hangup_ends_the_command_and_shipyard,
    an_isolated_command_ignoring_hangup_keeps_running,
    with_a_root_filesystem_ctrl_c_is_forwarded_exactly_once,
    with_a_root_filesystem_window_size_changes_reach_the_command,
    a_root_filesystem_has_its_own_terminals,
    a_container_created_from_a_terminal_keeps_it,
    ctrl_c_during_a_hook_ends_the_run,
    ctrl_z_and_fg_with_limits,
    an_oom_kill_on_a_terminal,
    pause_on_a_terminal,
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
/// matches. [`ISOLATED_TESTS`] count as ignored: `--ignored` picks only
/// them, `--include-ignored` adds them (on Linux; there's no isolation
/// elsewhere), and so does `rootless` (a normal user who can run rootless
/// containers here). Other libtest options are accepted and ignored.
fn select(args: &[String], rootless: bool) -> Vec<&'static (&'static str, fn())> {
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
    let with_ignored = cfg!(target_os = "linux")
        && (only_ignored || rootless || args.iter().any(|arg| arg == "--include-ignored"));
    let matches = |name: &str, pattern: &str| {
        if exact {
            name == pattern
        } else {
            name.contains(pattern)
        }
    };
    let ordinary = TESTS.iter().filter(|_| !only_ignored);
    let isolated = ISOLATED_TESTS.iter().filter(|_| with_ignored);
    ordinary
        .chain(isolated)
        .filter(|(name, _)| {
            (filters.is_empty() || filters.iter().any(|f| matches(name, f)))
                && !skips.iter().any(|skip| matches(name, skip))
        })
        .collect()
}

/// Runs the selected tests in order, printing results the way libtest does.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Whether rootless containers work here (as a normal user): asked once,
    // before any fake shell is forked. (`rootless` runs `unshare`, a child
    // process: no thread here.)
    let rootless = cfg!(target_os = "linux")
        && !is_root()
        && shipyard_test_support::rootless(shipyard_test_support::Rootless::Single);
    let selected = select(&args, rootless);
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in &selected {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    shipyard_test_support::pty::kill_everything_on_alarm();

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
        TESTS.len() + ISOLATED_TESTS.len() - selected.len(),
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
            let job = shell.spawn(
                Mode::Foreground,
                &["run", "--no-isolation", "sh", "-c", $script],
            );
            shell.wait(job);
        }
    };
}

/// Starts an isolated `shipyard run sh -c SCRIPT` in the foreground and
/// waits for it.
macro_rules! foreground_isolated {
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "--no-isolation",
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
    let mut session = session(foreground_sh!("echo ready; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_C);
    assert_eq!(session.status(), Status::Signaled(libc::SIGINT));
    session.finish();
}

/// Ctrl-\ sends SIGQUIT to the command; shipyard reports the death as
/// 128 + 3.
fn ctrl_backslash_quits_the_command() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
                "sh",
                "-c",
                "echo ready; sleep 3; echo done",
            ],
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &[
                "run",
                "--no-isolation",
                "sh",
                "-c",
                "read x; echo \"got $x\"",
            ],
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
                "sh",
                "-c",
                "echo ready; kill -STOP $$; echo resumed",
            ],
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
                "sh",
                "-c",
                "echo ready; kill -STOP $$; echo resumed",
            ],
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
    let mut session = session(foreground_sh!("echo ready; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGHUP)));
    session.finish();
}

// --- Signals sent with `kill` ---

/// A signal sent to a background shipyard's PID reaches the command.
fn kill_to_a_background_shipyard_is_forwarded() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &[
                "run",
                "--no-isolation",
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
    let mut session = session(foreground_sh!(
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
    let mut session = session(foreground_sh!("echo \"term=$TERM\""));
    session.job();
    session.expect(&format!("term={TERM}"));
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The command can read the terminal: it's in the foreground group with
/// shipyard, so reading doesn't stop it.
fn the_command_can_read_the_terminal() {
    let mut session = session(foreground_sh!("read x; echo \"got $x\""));
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
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
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
    let mut session = session(|shell| {
        let job = shell.spawn_program(
            Mode::Foreground,
            "sh",
            &[
                "-c",
                concat!(
                    "'",
                    env!("CARGO_BIN_EXE_shipyard"),
                    "' --debug run --no-isolation sh -c 'sleep 0.5; kill -STOP $$' & echo \"shipyard $!\""
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

/// A window-size change reaches the command once: the kernel sends SIGWINCH
/// to the foreground group, which holds the command, so shipyard must not
/// pass it on again (as for Ctrl-C).
fn window_size_changes_reach_the_command_once() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "--no-isolation",
                "sh",
                "-c",
                "trap 'stty size; exit 0' WINCH; echo re\"\"ady; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("sharing shipyard's process group");
    session.expect("ready");
    session.resize(40, 100);
    session.expect("40 100");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(
        session.count("forwarding a signal"),
        0,
        "{}",
        session.transcript()
    );
    session.finish();
}

// --- Isolated runs (root, Linux) ---
//
// The command is in a session of its own: never in the terminal's
// foreground group, and without a controlling terminal. What the terminal
// sends shipyard's group (keys, window size, hangup) reaches the command
// only because shipyard forwards it, once.

/// Ctrl-C reaches the command exactly once, through shipyard: the debug log
/// shows one forwarded signal (see `ctrl_c_reaches_the_command_exactly_once`
/// for the markers split with `""`).
fn isolated_ctrl_c_is_forwarded_exactly_once() {
    let mut session = session(|shell| {
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
    session.expect("giving the command its own process group");
    session.expect("ready");
    session.type_keys(CTRL_C);
    session.expect("got-int");
    session.expect("end");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(session.count("got-int"), 1, "{}", session.transcript());
    assert_eq!(
        session.count("forwarding a signal"),
        1,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// Ctrl-C kills the command, and shipyard dies of SIGINT too (the init
/// passes the exact status on), so the shell sees an interrupted job.
fn isolated_ctrl_c_ends_shipyard_by_sigint() {
    let mut session = session(foreground_isolated!("echo ready; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_C);
    assert_eq!(session.status(), Status::Signaled(libc::SIGINT));
    session.finish();
}

/// Ctrl-\\ quits the command (131), forwarded once (see
/// `isolated_ctrl_c_is_forwarded_exactly_once` for the split markers).
fn isolated_ctrl_backslash_quits_the_command() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "--ulimit",
                "core=0",
                "sh",
                "-c",
                "echo re\"\"ady; exec sleep 100",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("giving the command its own process group");
    session.expect("ready");
    session.type_keys(CTRL_BACKSLASH);
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGQUIT)));
    assert_eq!(
        session.count("forwarding a signal"),
        1,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// Ctrl-Z stops shipyard only. The command isn't in the terminal's group,
/// so it keeps running, and its output keeps coming. When it finishes while
/// shipyard is stopped, nothing reaps it until `fg` resumes shipyard, which
/// then reports the result.
fn isolated_ctrl_z_stops_only_shipyard() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "sh",
                "-c",
                // About 3 s of ticks: time enough for Ctrl-Z to land first.
                "echo ready; i=0; while [ $i -lt 30 ]; do echo tick$i; i=$((i+1)); sleep 0.1; done; \
                 echo finished",
            ],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("ready");
    let init = child_of(shipyard);
    session.type_keys(CTRL_Z);
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    wait_until_stopped(shipyard);
    assert!(
        !process_state(init).starts_with('T'),
        "the container was stopped too"
    );
    // Still running: its last lines arrive while shipyard is stopped.
    session.expect("finished");
    assert!(
        process_state(shipyard).starts_with('T'),
        "shipyard was resumed"
    );
    session.go();
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// In the foreground, the command reads the terminal. (Reading a terminal
/// that isn't your controlling one is just reading a file.)
fn isolated_command_reads_the_terminal() {
    let mut session = session(foreground_isolated!("read x; echo \"got $x\""));
    session.job();
    session.type_keys(b"hi\n");
    session.expect("got hi");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A known limitation (docs/user/run.md): in the background, the command
/// isn't stopped by SIGTTIN when it reads the terminal (only a process whose
/// controlling terminal it is gets that), so it reads what's typed, competing
/// with the shell. Without isolation it would stop
/// (`a_background_job_reading_the_terminal_waits_for_fg`).
fn a_background_isolated_command_still_reads_the_terminal() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &["run", "sh", "-c", "read x; echo \"got $x\""],
        );
        shell.wait(job);
    });
    session.job();
    session.type_keys(b"x\n");
    session.expect("got x");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// Window-size changes are forwarded once, and the command sees the new
/// size through its descriptor: `stty size` asks with `TIOCGWINSZ`, which
/// works on any terminal descriptor.
fn isolated_window_size_changes_reach_the_command() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "sh",
                "-c",
                "trap 'stty size; exit 0' WINCH; echo re\"\"ady; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("giving the command its own process group");
    session.expect("ready");
    session.resize(40, 100);
    session.expect("40 100");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(
        session.count("forwarding a signal"),
        1,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// The command has no controlling terminal: `/dev/tty` can't be opened
/// (ENXIO), though its descriptors are still the terminal. Programs that
/// open `/dev/tty` to ask for a password fail (docs/user/run.md).
fn isolated_command_has_no_controlling_terminal() {
    let mut session = session(foreground_isolated!(
        "tty; if (: < /dev/tty) 2>/dev/null; then echo has-c\"\"tty; else echo no-c\"\"tty; fi"
    ));
    session.job();
    session.expect("/dev/pts/");
    session.expect("no-ctty");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(session.count("has-ctty"), 0, "{}", session.transcript());
    session.finish();
}

/// Closing the terminal: SIGHUP reaches the command only through shipyard,
/// and shipyard exits with 128 + 1.
fn isolated_hangup_ends_the_command_and_shipyard() {
    let mut session = session(foreground_isolated!("echo ready; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGHUP)));
    session.finish();
}

/// A command that ignores SIGHUP outlives the terminal (its writes fail,
/// which this one doesn't mind), and shipyard keeps supervising it to the end.
fn an_isolated_command_ignoring_hangup_keeps_running() {
    let mut session = session(foreground_isolated!(
        "trap '' HUP; echo ready; sleep 1; echo gone 2>/dev/null; exit 5"
    ));
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(5));
    session.finish();
}

/// A command that stops itself doesn't stop shipyard (it isn't in the
/// terminal's group, so shipyard doesn't follow it), and the terminal seems
/// to hang. The documented way out: Ctrl-Z stops shipyard, and `fg` resumes
/// shipyard, which resumes the command (docs/user/run.md).
fn an_isolated_command_that_stops_itself_resumes_with_ctrl_z_and_fg() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "sh", "-c", "echo ready; kill -STOP $$; echo resumed"],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("ready");
    let command = child_of(child_of(shipyard));
    wait_until_stopped(command);
    assert!(
        !process_state(shipyard).starts_with('T'),
        "shipyard stopped along with the command"
    );
    session.type_keys(CTRL_Z);
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    session.go();
    session.expect("resumed");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

// --- With a root filesystem (root, Linux) ---

/// Where the `--rootfs` tests' root filesystem is. A session's script runs
/// in the forked fake shell and can't capture a variable, so the path
/// travels in the environment, which the fork inherits.
const ROOTFS_VAR: &str = "SHIPYARD_TEST_ROOTFS";

/// A root filesystem for one test, named in [`ROOTFS_VAR`] until dropped.
fn rootfs() -> RootFs {
    let root = RootFs::minimal();
    // SAFETY: this test binary is single-threaded (see the module docs), so
    // nothing reads the environment concurrently.
    unsafe { std::env::set_var(ROOTFS_VAR, root.arg()) };
    root
}

/// `shipyard ARGS…` with `--rootfs` (from [`ROOTFS_VAR`]) after `run`, in
/// the foreground, waited for.
fn foreground_in_rootfs(shell: &mut Shell, before_run: &[&str], script: &str) {
    let path = std::env::var(ROOTFS_VAR).expect("the test sets the root filesystem");
    let args = [before_run, &["run", "--rootfs", &path, "sh", "-c", script]].concat();
    let job = shell.spawn(Mode::Foreground, &args);
    shell.wait(job);
}

/// As [`isolated_ctrl_c_is_forwarded_exactly_once`], in a root filesystem:
/// its own `/dev` changes nothing about how Ctrl-C arrives.
fn with_a_root_filesystem_ctrl_c_is_forwarded_exactly_once() {
    let _root = rootfs();
    let mut session = session(|shell| {
        foreground_in_rootfs(
            shell,
            &["--debug"],
            "trap 'echo got-\"\"int; kill $!' INT; echo re\"\"ady; sleep 5 & wait; echo e\"\"nd",
        );
    });
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_C);
    session.expect("got-int");
    session.expect("end");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(session.count("got-int"), 1, "{}", session.transcript());
    assert_eq!(
        session.count("forwarding a signal"),
        1,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// As [`isolated_window_size_changes_reach_the_command`], in a root
/// filesystem: the size is the terminal's, set on the descriptor the
/// command holds, wherever its `/dev` is.
fn with_a_root_filesystem_window_size_changes_reach_the_command() {
    let _root = rootfs();
    let mut session = session(|shell| {
        foreground_in_rootfs(
            shell,
            &["--debug"],
            "trap 'stty size; exit 0' WINCH; echo re\"\"ady; while :; do sleep 0.1; done",
        );
    });
    session.job();
    session.expect("ready");
    session.resize(40, 100);
    session.expect("40 100");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A root filesystem's `/dev/pts` is its own: the terminal the command was
/// given (the host's) isn't in it, so `tty` can't name it. The command
/// still writes to it.
fn a_root_filesystem_has_its_own_terminals() {
    let _root = rootfs();
    let mut session = session(|shell| {
        foreground_in_rootfs(shell, &[], "tty; echo wr\"\"ote");
    });
    session.job();
    session.expect("not a tty");
    session.expect("wrote");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

// --- The OCI runtime (root, Linux) ---

/// Where [`a_container_created_from_a_terminal_keeps_it`] keeps its bundle
/// and its state root, for the forked shell (as [`ROOTFS_VAR`]).
const BUNDLE_VAR: &str = "SHIPYARD_TEST_BUNDLE";
const STATE_ROOT_VAR: &str = "SHIPYARD_TEST_STATE_ROOT";

/// The container [`a_container_created_from_a_terminal_keeps_it`] makes.
const ON_A_TTY: &str = "on-a-tty";

/// A state root, its containers deleted and itself removed when dropped,
/// so a failed test leaves no container running.
struct StateRoot(std::path::PathBuf);

impl Drop for StateRoot {
    fn drop(&mut self) {
        let _ = std::process::Command::new(RUNTIME)
            .arg("--root")
            .arg(&self.0)
            .args(["delete", "--force", ON_A_TTY])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The container's standard streams are `create`'s, here the terminal: it
/// keeps writing to it, and reading it, after `create` and `start` have
/// returned and the shell has the terminal back (runtime.md says to
/// redirect). `delete --force` ends it.
fn a_container_created_from_a_terminal_keeps_it() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!([
        "sh",
        "-c",
        "echo hel\"\"lo; read line; echo \"got $line\"; sleep 30"
    ]);
    bundle.write(&config);
    let state =
        StateRoot(std::env::temp_dir().join(format!("shipyard-tty-{}", std::process::id())));
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe {
        std::env::set_var(BUNDLE_VAR, bundle.arg());
        std::env::set_var(STATE_ROOT_VAR, &state.0);
    }
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let runtime = |shell: &mut Shell, args: &[&str]| {
            let args = [&["--root", root.as_str()], args].concat();
            let job = shell.spawn_program(Mode::Foreground, RUNTIME, &args);
            shell.wait(job);
        };
        runtime(shell, &["create", "--bundle", &bundle, ON_A_TTY]);
        runtime(shell, &["start", ON_A_TTY]);
        shell.await_go();
        runtime(shell, &["delete", "--force", ON_A_TTY]);
    });
    session.job();
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "create\n{}",
        session.transcript()
    );
    session.job();
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "start\n{}",
        session.transcript()
    );
    session.expect("hello");
    session.type_keys(b"typed\n");
    session.expect("got typed");
    session.go();
    session.job();
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "delete\n{}",
        session.transcript()
    );
    session.finish();
}

// --- Stage 5: cgroups, hooks and the terminal -------------------------------------

const HOOK_MARK_VAR: &str = "SHIPYARD_TEST_HOOK_MARK";

/// Whether a container can have a cgroup of its own here (root, or a
/// delegated subtree): what limits and `pause` need. Says why not when not.
fn cgroups_here() -> bool {
    #[cfg(target_os = "linux")]
    let delegated = shipyard_test_support::delegated_cgroup().is_some();
    // (These tests run only on Linux: see `ISOLATED_TESTS`.)
    #[cfg(not(target_os = "linux"))]
    let delegated = false;
    let here = is_root() || delegated;
    if !here {
        eprintln!("skipping: no cgroup of a container's own here");
    }
    here
}

/// The cgroup of process `pid`, as a directory of the tree (from its
/// `/proc/PID/cgroup`, which shows it from this process's cgroup namespace,
/// even for a process in its own).
fn cgroup_of(pid: libc::pid_t) -> std::path::PathBuf {
    let line = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let relative = line.trim().strip_prefix("0::/").unwrap_or_default();
    std::path::Path::new("/sys/fs/cgroup").join(relative)
}

/// The children of `parent`, from `/proc`.
fn children_of(parent: libc::pid_t) -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&pid: &libc::pid_t| {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit(')')
                    .next()
                    .and_then(|rest| rest.split_whitespace().nth(1))
                    == Some(&parent.to_string())
            })
        })
        .collect()
}

/// Waits (up to 10 s) for `done`.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "never: {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A bundle's `createRuntime` hook that never ends holds `run --bundle` up.
/// Ctrl-C reaches the hook as it does shipyard: the hook is in shipyard's
/// process group, the terminal's foreground one. The hook fails, the run
/// ends naming it (125), and nothing of the container is left: not its
/// init, not its cgroup.
fn ctrl_c_during_a_hook_ends_the_run() {
    let bundle = Bundle::new();
    let mark = std::env::temp_dir().join(format!("shipyard-tty-hook-{}", std::process::id()));
    let _ = std::fs::remove_file(&mark);
    let mut config = bundle.config();
    config["hooks"] = serde_json::json!({"createRuntime": [{
        "path": "/bin/sh",
        "args": ["sh", "-c", format!("echo $$ > {}; exec sleep 300", mark.display())]
    }]});
    bundle.write(&config);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe {
        std::env::set_var(BUNDLE_VAR, bundle.arg());
        std::env::set_var(HOOK_MARK_VAR, &mark);
    }
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let job = shell.spawn(Mode::Foreground, &["run", "--bundle", &bundle]);
        shell.wait(job);
    });
    let shipyard = session.job();
    eventually("the hook running", || mark.exists());
    let init = children_of(shipyard)
        .into_iter()
        .find(|&pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.trim() == "shipyard-init")
        })
        .expect("the container's init, waiting for the hook");
    let cgroup = cgroup_of(init);
    session.type_keys(CTRL_C);
    assert_eq!(
        session.status(),
        Status::Exited(125),
        "{}",
        session.transcript()
    );
    session.expect("createRuntime hook");
    let _ = std::fs::remove_file(&mark);
    assert!(
        !process_state(init).starts_with(['R', 'S', 'D', 'T']),
        "the init is left"
    );
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
    session.finish();
}

/// Ctrl-Z stops shipyard (the isolated command, in a session of its own,
/// goes on), and the run's cgroup and limits stay through the stop; `fg`
/// resumes shipyard, which reports the end, and the cgroup goes with the run.
fn ctrl_z_and_fg_with_limits() {
    if !cgroups_here() {
        return;
    }
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--memory",
                "64m",
                "--pids-limit",
                "40",
                "sh",
                "-c",
                "echo ready; sleep 2; echo done",
            ],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("ready");
    let cgroup = cgroup_of(child_of(shipyard));
    let limit = |file: &str| std::fs::read_to_string(cgroup.join(file)).unwrap_or_default();
    assert_eq!(limit("memory.max").trim(), "67108864");
    assert_eq!(limit("pids.max").trim(), "40");
    session.type_keys(CTRL_Z);
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    wait_until_stopped(shipyard);
    assert_eq!(
        limit("memory.max").trim(),
        "67108864",
        "the limit through the stop"
    );
    session.expect("done");
    session.go();
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
    session.finish();
}

/// A command the OOM killer ends in the foreground: the terminal says so,
/// the shell gets 137, and the terminal still works (the next job reads it).
fn an_oom_kill_on_a_terminal() {
    if !cgroups_here() {
        return;
    }
    let mut session = session(|shell| {
        // `tail` keeps the last line, and /dev/zero has no end of line: it
        // grows until the limit.
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--memory",
                "32m",
                "--memory-swap",
                "32m",
                "tail",
                "/dev/zero",
            ],
        );
        shell.wait(job);
        shell.await_go();
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "sh", "-c", "read x; echo \"got $x\""],
        );
        shell.wait(job);
    });
    session.job();
    assert_eq!(
        session.status(),
        Status::Exited(137),
        "{}",
        session.transcript()
    );
    session.expect("shipyard: warning: the command was killed: out of memory");
    session.go();
    session.job();
    session.type_keys(b"after\n");
    session.expect("got after");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A runtime container created from a terminal, echoing what it reads:
/// paused, what's typed waits (frozen, it reads nothing); resumed, it reads
/// and echoes it. `kill --all` ends it.
fn pause_on_a_terminal() {
    if !cgroups_here() {
        return;
    }
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!([
        "sh",
        "-c",
        "echo re\"\"ady; while read line; do echo \"got $line\"; done"
    ]);
    bundle.write(&config);
    let state =
        StateRoot(std::env::temp_dir().join(format!("shipyard-tty-pause-{}", std::process::id())));
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe {
        std::env::set_var(BUNDLE_VAR, bundle.arg());
        std::env::set_var(STATE_ROOT_VAR, &state.0);
    }
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let runtime = |shell: &mut Shell, args: &[&str]| {
            let args = [&["--root", root.as_str()], args].concat();
            let job = shell.spawn_program(Mode::Foreground, RUNTIME, &args);
            shell.wait(job);
        };
        runtime(shell, &["create", "--bundle", &bundle, ON_A_TTY]);
        runtime(shell, &["start", ON_A_TTY]);
        shell.await_go();
        runtime(shell, &["pause", ON_A_TTY]);
        shell.await_go();
        runtime(shell, &["state", ON_A_TTY]);
        shell.await_go();
        runtime(shell, &["resume", ON_A_TTY]);
        shell.await_go();
        runtime(shell, &["kill", "--all", ON_A_TTY, "KILL"]);
        runtime(shell, &["delete", "--force", ON_A_TTY]);
    });
    let ok = |session: &mut Session, what: &str| {
        session.job();
        assert_eq!(
            session.status(),
            Status::Exited(0),
            "{what}\n{}",
            session.transcript()
        );
    };
    ok(&mut session, "create");
    ok(&mut session, "start");
    session.expect("ready");
    session.type_keys(b"one\n");
    session.expect("got one");
    session.go();
    ok(&mut session, "pause");
    session.type_keys(b"two\n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    session.go();
    ok(&mut session, "state");
    // Everything printed before the state was read with it: no echo.
    session.expect("\"paused\"");
    assert_eq!(
        session.count("got two"),
        0,
        "a paused container read\n{}",
        session.transcript()
    );
    session.go();
    ok(&mut session, "resume");
    session.expect("got two");
    session.go();
    ok(&mut session, "kill --all");
    ok(&mut session, "delete");
    session.finish();
}
