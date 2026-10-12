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
const RUNTIME: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard-runtime"));

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
    unisolated_t_gives_a_terminal_of_its_own,
    unisolated_it_ctrl_c_reaches_the_command,
    unisolated_t_hangup_ends_the_input_of_a_command_ignoring_it,
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
    with_i_the_command_reads_what_is_typed,
    a_background_isolated_reader_waits_for_fg,
    kill_of_a_run_stopped_for_input_ends_it,
    an_isolated_command_never_holds_the_host_terminal,
    a_bundle_run_never_holds_the_host_terminal,
    a_terminal_bundle_runs_on_its_own_terminal,
    attach_to_a_terminal_container,
    attach_from_a_terminal_to_a_plain_container,
    a_killed_attach_puts_the_terminal_back,
    exec_t_on_a_terminal,
    exec_from_a_terminal_never_gets_it,
    exec_t_in_the_background_starts_its_command,
    a_bundle_without_a_pid_namespace_cant_reach_shipyards_descriptors,
    a_poststart_hook_doesnt_hold_up_the_output,
    a_stalled_terminal_doesnt_keep_shipyard_after_a_stop_request,
    with_t_tty_names_the_containers_own_terminal,
    with_t_stty_size_follows_the_window,
    with_t_dev_tty_opens,
    with_it_ctrl_c_reaches_the_inner_job_only,
    with_it_ctrl_z_stops_the_inner_job_not_shipyard,
    with_t_exit_codes_pass_through,
    the_terminal_is_restored_after_kill_9_of_the_command,
    the_terminal_is_restored_while_shipyard_is_stopped,
    with_t_the_containers_descriptors_are_its_terminal_only,
    with_it_in_the_background_waits_for_fg,
    with_it_in_the_background_ignoring_sigttou_leaves_the_terminal_alone,
    kill_of_a_run_waiting_for_the_terminal_ends_it,
    a_background_run_whose_container_dies_doesnt_wait_for_fg,
    a_raw_run_continued_in_the_background_leaves_the_terminal_alone,
    a_flood_through_the_terminal_ends_promptly,
    a_background_job_holding_the_terminal_doesnt_hang_the_run,
    with_t_script_finds_its_pty_without_a_root_filesystem,
    with_t_hangup_ends_the_command,
    with_t_hangup_ends_the_input_of_a_command_ignoring_it,
    with_t_kill_hup_is_forwarded,
    with_t_the_terminal_is_the_commands_user,
    the_terminal_survives_stops_around_the_end_of_runs,
    the_shells_terminal_stays_blocking_after_kill_9_of_shipyard,
    isolated_window_size_changes_are_forwarded,
    isolated_command_has_no_controlling_terminal,
    isolated_hangup_ends_the_command_and_shipyard,
    an_isolated_command_ignoring_hangup_keeps_running,
    with_a_root_filesystem_ctrl_c_is_forwarded_exactly_once,
    with_a_root_filesystem_window_size_changes_are_forwarded,
    in_a_root_filesystem_the_command_has_no_terminal,
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
/// (documented in docs/user/run.md). USR1, sent after, is forwarded on both
/// and marks the point by which a forwarded INT would have shown up. On
/// Linux the INT's trap is waited for first: a shell with both pending may
/// run USR1's (which exits) before INT's, as bash does.
fn kill_to_a_foreground_shipyard() {
    let mut session = session(foreground_sh!(
        "trap 'echo got-int; kill $!' INT; trap 'echo got-usr1; kill $!; exit 0' USR1; echo ready; \
         while :; do sleep 100 & wait; done"
    ));
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` only sends signals to the shipyard this test started.
    assert_eq!(unsafe { libc::kill(shipyard, libc::SIGINT) }, 0);
    if cfg!(target_os = "linux") {
        session.expect("got-int");
    }
    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(shipyard, libc::SIGUSR1) }, 0);
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
        let script = format!(
            "'{}' --debug run --no-isolation sh -c 'sleep 0.5; kill -STOP $$' & echo \"shipyard $!\"",
            SHIPYARD.path()
        );
        let job = shell.spawn_program(Mode::Foreground, "sh", &["-c", &script]);
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
/// so it keeps running; its output, which shipyard relays (it never holds
/// the terminal: D16), waits in the pipe until `fg`. When it finishes while
/// shipyard is stopped, nothing reaps it until `fg` resumes shipyard, which
/// then passes on its last lines and reports the result.
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
    // Still running, and done within its 3 s; its last lines wait for
    // shipyard.
    session.settle(std::time::Duration::from_secs(4));
    assert_eq!(session.count("finished"), 0, "{}", session.transcript());
    assert!(
        process_state(shipyard).starts_with('T'),
        "shipyard was resumed"
    );
    session.go();
    session.expect("finished");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// With `-i`, what's typed reaches the command (through shipyard's relay:
/// D16), and ^D ends its input.
fn with_i_the_command_reads_what_is_typed() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "-i",
                "sh",
                "-c",
                "read x; echo \"got $x\"; if read y; then echo more; else echo e\"\"nd; fi",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.type_keys(b"hi\n");
    session.expect("got hi");
    session.type_keys(b"\x04");
    session.expect("end");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// In the background, `-i` doesn't take the shell's keys: shipyard, which
/// reads the terminal for the command, is stopped by SIGTTIN as any program
/// would be, until `fg`. (Before D16, the command held the terminal and read
/// what was typed, competing with the shell.)
fn a_background_isolated_reader_waits_for_fg() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &["run", "-i", "sh", "-c", "read x; echo \"got $x\""],
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

/// A run stopped for input (SIGTTIN, above) still ends on `kill %1`: the
/// shell's SIGTERM, then SIGCONT. Shipyard must not stop itself again for
/// the terminal while it's stopping the command.
fn kill_of_a_run_stopped_for_input_ends_it() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &[
                "run",
                "-i",
                "--stop-timeout",
                "1",
                "sh",
                "-c",
                "read x; echo \"got $x\"",
            ],
        );
        shell.wait(job);
        shell.await_go();
        shell.wait(job);
    });
    let shipyard = session.job();
    assert_eq!(session.status(), Status::Stopped(libc::SIGTTIN));
    // As bash's `kill %1`: SIGTERM, then SIGCONT to the job.
    // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the job.
    unsafe {
        libc::kill(shipyard, libc::SIGTERM);
        libc::kill(shipyard, libc::SIGCONT);
    }
    session.go();
    assert_eq!(
        session.status(),
        Status::Exited(killed_by(libc::SIGTERM)),
        "{}",
        session.transcript()
    );
    assert_eq!(session.count("got"), 0, "{}", session.transcript());
    session.finish();
}

/// D16: an isolated command never holds the host's terminal, so it can't
/// type into it (`TIOCSTI`) or change its settings: its descriptors are
/// pipes (and `/dev/null` for stdin without `-i`), relayed by shipyard. Nor
/// can it open the terminal by path: without a root filesystem of its own,
/// its `/dev/pts` is a devpts instance of its own, which holds no terminal
/// yet (only `ptmx`). The terminal's settings are the same after as before
/// (F17), and the output keeps its order (stdout and stderr share one pipe
/// on one terminal).
fn an_isolated_command_never_holds_the_host_terminal() {
    let mut session = session(|shell| {
        let before = shell.terminal_settings();
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "sh",
                "-c",
                "for fd in 0 1 2; do [ -t $fd ] && echo \"fd $fd: a t\"\"ty\"; done; \
                 stty raw 2>/dev/null || echo cannot-change; \
                 echo \"pts: $(ls /dev/pts | tr '\\n' ' ')\"; \
                 echo \"term=[$(printenv TERM)]\"; \
                 echo one; echo two >&2; echo three",
            ],
        );
        shell.wait(job);
        shell.report_settings(&before);
    });
    session.job();
    session.expect("cannot-change");
    session.expect("pts: ptmx \r\n");
    // No terminal, so no `TERM` to tempt it into sending a terminal's escapes.
    session.expect("term=[]");
    session.expect("one");
    session.expect("two");
    session.expect("three");
    assert_eq!(session.status(), Status::Exited(0));
    session.expect("settings unchanged");
    assert_eq!(session.count("a tty"), 0, "{}", session.transcript());
    session.finish();
}

/// The same for a bundle's run (`--bundle` takes the same path).
fn a_bundle_run_never_holds_the_host_terminal() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!([
        "sh",
        "-c",
        "for fd in 0 1 2; do [ -t $fd ] && echo \"fd $fd: a t\"\"ty\"; done; \
         stty raw 2>/dev/null || echo cannot-change; \
         echo \"fd1=$(readlink /proc/self/fd/1)\"; echo do\"\"ne"
    ]);
    bundle.write(&config);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(BUNDLE_VAR, bundle.arg()) };
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let job = shell.spawn(Mode::Foreground, &["run", "--bundle", &bundle]);
        shell.wait(job);
    });
    session.job();
    // What it has instead: pipes, and no terminal to change.
    session.expect("cannot-change");
    session.expect("fd1=pipe:");
    session.expect("done");
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    assert_eq!(session.count("a tty"), 0, "{}", session.transcript());
    session.finish();
}

/// `shipyard-runtime attach` to a container with a terminal (`create
/// --log-dir --stdin`): the user's terminal goes raw, what's typed reaches
/// the container's terminal (Ctrl-C too, as a byte), a resize follows,
/// Ctrl-P Ctrl-Q detaches (exit 0), and the terminal is put back as it was.
fn attach_to_a_terminal_container() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!([
        "sh",
        "-c",
        // (Ctrl-C interrupts `read`: the loop goes on.)
        "trap 'echo caught-int' INT; while :; do read line && echo got-$line && stty size; done"
    ]);
    config["process"]["terminal"] = serde_json::json!(true);
    bundle.write(&config);
    let state =
        StateRoot(std::env::temp_dir().join(format!("shipyard-attach-{}", std::process::id())));
    let logs = state.0.with_extension("logs");
    let runtime = |args: &[&str]| {
        let status = std::process::Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&state.0)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    runtime(&[
        "create",
        "--bundle",
        bundle.arg(),
        "--log-dir",
        logs.to_str().unwrap(),
        "--stdin",
        ON_A_TTY,
    ]);
    runtime(&["start", ON_A_TTY]);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(STATE_ROOT_VAR, &state.0) };
    let mut session = session(|shell| {
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let before = shell.terminal_settings();
        let job = shell.spawn_program(
            Mode::Foreground,
            RUNTIME.path(),
            &["--root", &root, "attach", ON_A_TTY],
        );
        shell.wait(job);
        shell.report_settings(&before);
    });
    session.job();
    session.type_keys(b"one\r");
    session.expect("got-one");
    session.resize(33, 99);
    session.type_keys(b"two\r");
    session.expect("got-two");
    session.expect("33 99");
    // Ctrl-C is a byte in raw mode: it reaches the container's terminal,
    // which interrupts its shell, and `attach` goes on.
    session.type_keys(b"\x03");
    session.expect("caught-int");
    session.type_keys(b"three\r");
    session.expect("got-three");
    session.type_keys(b"\x10\x11");
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    session.expect("settings unchanged");
    session.finish();
    let _ = std::fs::remove_dir_all(&logs);
}

/// `shipyard-runtime attach` from a terminal to a container without one: the
/// user types lines, as their terminal edits them, but Ctrl-Q reaches
/// attach (the terminal's flow control is off while attached: it would take
/// the key for itself), so Ctrl-P Ctrl-Q and Enter detach (exit 0); the
/// terminal is put back as it was, and so it is when Ctrl-C ends attach.
fn attach_from_a_terminal_to_a_plain_container() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] =
        serde_json::json!(["sh", "-c", "while read line; do echo got-$line; done"]);
    bundle.write(&config);
    let state = StateRoot(
        std::env::temp_dir().join(format!("shipyard-attach-plain-{}", std::process::id())),
    );
    let logs = state.0.with_extension("logs");
    let runtime = |args: &[&str]| {
        let status = std::process::Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&state.0)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    runtime(&[
        "create",
        "--bundle",
        bundle.arg(),
        "--log-dir",
        logs.to_str().unwrap(),
        "--stdin",
        PLAIN_ON_A_TTY,
    ]);
    runtime(&["start", PLAIN_ON_A_TTY]);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(STATE_ROOT_VAR, &state.0) };
    let attach = |shell: &mut Shell| {
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let before = shell.terminal_settings();
        let job = shell.spawn_program(
            Mode::Foreground,
            RUNTIME.path(),
            &["--root", &root, "attach", PLAIN_ON_A_TTY],
        );
        shell.wait(job);
        shell.report_settings(&before);
    };
    let mut detaching = session(attach);
    detaching.job();
    detaching.type_keys(b"one\r");
    detaching.expect("got-one");
    detaching.type_keys(b"\x10\x11\r");
    assert_eq!(
        detaching.status(),
        Status::Exited(0),
        "{}",
        detaching.transcript()
    );
    detaching.expect("settings unchanged");
    detaching.finish();
    // Ctrl-C ends attach (a signal, from its terminal's own keys): the
    // terminal is put back first.
    let mut interrupted = session(attach);
    interrupted.job();
    interrupted.type_keys(b"two\r");
    interrupted.expect("got-two");
    interrupted.type_keys(CTRL_C);
    assert_eq!(
        interrupted.status(),
        Status::Signaled(libc::SIGINT),
        "{}",
        interrupted.transcript()
    );
    interrupted.expect("settings unchanged");
    interrupted.finish();
    runtime(&["delete", "--force", PLAIN_ON_A_TTY]);
    let _ = std::fs::remove_dir_all(&logs);
}

/// `shipyard-runtime attach` ended by a signal from outside (a `kill`, a
/// hang-up), not a key: the user's terminal is put back first, raw (a
/// container with a terminal) or with only its flow control off (one
/// without), and attach ends of that signal.
fn a_killed_attach_puts_the_terminal_back() {
    fn attach(shell: &mut Shell, id: &str) {
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let before = shell.terminal_settings();
        let job = shell.spawn_program(
            Mode::Foreground,
            RUNTIME.path(),
            &["--root", &root, "attach", id],
        );
        shell.wait(job);
        shell.report_settings(&before);
    }
    let make = |terminal: bool| {
        let bundle = Bundle::new();
        let mut config = bundle.config();
        config["process"]["args"] =
            serde_json::json!(["sh", "-c", "while read line; do echo got-$line; done"]);
        config["process"]["terminal"] = serde_json::json!(terminal);
        bundle.write(&config);
        bundle
    };
    let state = StateRoot(
        std::env::temp_dir().join(format!("shipyard-attach-killed-{}", std::process::id())),
    );
    let logs = state.0.with_extension("logs");
    let runtime = |args: &[&str]| {
        let status = std::process::Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&state.0)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    let bundles = [make(true), make(false)];
    for (bundle, id) in bundles.iter().zip([KILLED_ON_A_TTY, KILLED_PLAIN]) {
        let dir = logs.join(id);
        std::fs::create_dir_all(&logs).unwrap();
        runtime(&[
            "create",
            "--bundle",
            bundle.arg(),
            "--log-dir",
            dir.to_str().unwrap(),
            "--stdin",
            id,
        ]);
        runtime(&["start", id]);
    }
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(STATE_ROOT_VAR, &state.0) };
    let cases: [(Script, libc::c_int, &str); 3] = [
        (
            |shell| attach(shell, KILLED_ON_A_TTY),
            libc::SIGTERM,
            "raw, SIGTERM",
        ),
        (
            |shell| attach(shell, KILLED_ON_A_TTY),
            libc::SIGHUP,
            "raw, SIGHUP",
        ),
        (
            |shell| attach(shell, KILLED_PLAIN),
            libc::SIGTERM,
            "flow control off, SIGTERM",
        ),
    ];
    for (script, signal, what) in cases {
        let mut session = session(script);
        let pid = session.job();
        session.type_keys(b"one\r");
        session.expect("got-one");
        // SAFETY: `kill` has no memory-safety conditions; `pid` is the
        // session's attach.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "{what}");
        assert_eq!(
            session.status(),
            Status::Signaled(signal),
            "{what}: {}",
            session.transcript()
        );
        session.expect("settings unchanged");
        session.finish();
    }
    runtime(&["delete", "--force", KILLED_ON_A_TTY]);
    runtime(&["delete", "--force", KILLED_PLAIN]);
    let _ = std::fs::remove_dir_all(&logs);
}

/// `shipyard-runtime exec` (no `-t`) from a terminal: the command never
/// gets the user's terminal (D16), not even as a descriptor a process left
/// behind would keep: its 0, 1 and 2 are pipes Shipyard relays, and what's
/// typed still reaches it, and its output the user.
fn exec_from_a_terminal_never_gets_it() {
    const ID: &str = "exec-plain-on-a-tty";
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!(["sh", "-c", "exec sleep 120"]);
    bundle.write(&config);
    let state =
        StateRoot(std::env::temp_dir().join(format!("shipyard-exec-plain-{}", std::process::id())));
    let runtime = |args: &[&str]| {
        std::process::Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&state.0)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    };
    assert!(runtime(&["create", "--bundle", bundle.arg(), ID]).success());
    assert!(runtime(&["start", ID]).success());
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(STATE_ROOT_VAR, &state.0) };
    let mut session = session(|shell| {
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let before = shell.terminal_settings();
        let job = shell.spawn_program(
            Mode::Foreground,
            RUNTIME.path(),
            &[
                "--root",
                &root,
                "exec",
                ID,
                "sh",
                "-c",
                "tty; for fd in 0 1 2; do echo fd$fd=$(readlink /proc/self/fd/$fd); done; \
                 read line; echo got-$line; exit 4",
            ],
        );
        shell.wait(job);
        shell.report_settings(&before);
    });
    session.job();
    session.expect("not a tty");
    for fd in 0..3 {
        session.expect(&format!("fd{fd}=pipe:"));
    }
    session.type_keys(b"typed\r");
    session.expect("got-typed");
    assert_eq!(
        session.status(),
        Status::Exited(4),
        "{}",
        session.transcript()
    );
    session.expect("settings unchanged");
    session.finish();
    let _ = runtime(&["delete", "--force", ID]);
}

/// `shipyard-runtime exec -t` from a terminal: the command gets a terminal
/// of its own, made in the container (its `/dev/pts/0`), the user's goes raw
/// (Ctrl-C reaches the command as a byte), follows a resize, and is put back
/// as it was; exec ends with the command's code.
fn exec_t_on_a_terminal() {
    const ID: &str = "exec-on-a-tty";
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!(["sh", "-c", "exec sleep 120"]);
    bundle.write(&config);
    let state =
        StateRoot(std::env::temp_dir().join(format!("shipyard-exec-t-{}", std::process::id())));
    let runtime = |args: &[&str]| {
        std::process::Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&state.0)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    };
    assert!(runtime(&["create", "--bundle", bundle.arg(), ID]).success());
    assert!(runtime(&["start", ID]).success());
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(STATE_ROOT_VAR, &state.0) };
    let mut session = session(|shell| {
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let before = shell.terminal_settings();
        let job = shell.spawn_program(
            Mode::Foreground,
            RUNTIME.path(),
            &[
                "--root",
                &root,
                "exec",
                "-t",
                ID,
                "sh",
                "-c",
                // (Ctrl-C interrupts `read`: the loop goes on.)
                "tty; trap 'echo caught-int' INT; \
                 while :; do read line && echo got-$line && stty size; \
                 [ \"$line\" = bye ] && exit 3; done",
            ],
        );
        shell.wait(job);
        shell.report_settings(&before);
    });
    session.job();
    session.expect("/dev/pts/0");
    session.type_keys(b"one\r");
    session.expect("got-one");
    session.resize(33, 99);
    session.type_keys(b"two\r");
    session.expect("got-two");
    session.expect("33 99");
    session.type_keys(b"\x03");
    session.expect("caught-int");
    session.type_keys(b"bye\r");
    assert_eq!(
        session.status(),
        Status::Exited(3),
        "{}",
        session.transcript()
    );
    session.expect("settings unchanged");
    session.finish();
    let _ = runtime(&["delete", "--force", ID]);
}

/// `exec -t … &`: started in the background, it doesn't stop for the
/// terminal before its command starts (exec holds the container's lock
/// until then): its terminal is relayed without raw mode or input, and the
/// command runs to its end.
fn exec_t_in_the_background_starts_its_command() {
    const ID: &str = "exec-in-the-background";
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!(["sh", "-c", "exec sleep 120"]);
    bundle.write(&config);
    let state =
        StateRoot(std::env::temp_dir().join(format!("shipyard-exec-bg-{}", std::process::id())));
    let runtime = |args: &[&str]| {
        std::process::Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&state.0)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    };
    assert!(runtime(&["create", "--bundle", bundle.arg(), ID]).success());
    assert!(runtime(&["start", ID]).success());
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(STATE_ROOT_VAR, &state.0) };
    let mut session = session(|shell| {
        let root = std::env::var(STATE_ROOT_VAR).expect("the test sets the state root");
        let job = shell.spawn_program(
            Mode::Background,
            RUNTIME.path(),
            &[
                "--root",
                &root,
                "exec",
                "-t",
                ID,
                "sh",
                "-c",
                "echo ran-$((40+2))",
            ],
        );
        shell.wait(job);
    });
    session.job();
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    session.expect("ran-42");
    session.finish();
}

/// A bundle that asks for a terminal (`process.terminal`) gets one of its
/// own, as `-t` gives, with the user's typing under `-i`.
fn a_terminal_bundle_runs_on_its_own_terminal() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!([
        "sh",
        "-c",
        "tty; echo re\"\"ady; read line; echo \"got $line\""
    ]);
    config["process"]["terminal"] = serde_json::json!(true);
    bundle.write(&config);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(BUNDLE_VAR, bundle.arg()) };
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let job = shell.spawn(Mode::Foreground, &["run", "-i", "--bundle", &bundle]);
        shell.wait(job);
    });
    session.job();
    session.expect("/dev/pts/0");
    session.expect("ready");
    session.type_keys(b"typed\n");
    session.expect("got typed");
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    session.finish();
}

/// A bundle without a PID namespace sees shipyard in its `/proc`. While
/// shipyard relays the terminal for it, the container's processes (the same
/// user, here root) mustn't reach it through `/proc/<shipyard>/fd/N`.
/// Following those links is checked as a debugger's access would be, and the
/// kernel refuses a process that lacks any capability its target has (the
/// container has fewer than shipyard; rootless, it has none in shipyard's
/// user namespace), whether or not shipyard is dumpable. (Listing the
/// directory isn't checked so: that's only its mode, and root owns it
/// either way.) The control: shipyard is there to be seen (its `status` is
/// readable by anyone).
fn a_bundle_without_a_pid_namespace_cant_reach_shipyards_descriptors() {
    if !is_root() {
        println!("skipping: a bundle without a PID namespace runs as root here");
        return;
    }
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] =
        serde_json::json!([{"type": "mount"}, {"type": "uts"}, {"type": "ipc"}]);
    config["mounts"] =
        serde_json::json!([{"destination": "/proc", "type": "proc", "source": "proc"}]);
    config["process"]["args"] = serde_json::json!([
        "sh",
        "-c",
        "case \"$(head -1 /proc/$PPID/status)\" in *shipyard*) echo sees-ship\"\"yard;; esac; \
         if readlink /proc/$PPID/fd/0 >/dev/null 2>&1 || cat /proc/$PPID/fd/0 </dev/null >/dev/null 2>&1; \
         then echo re\"\"ached; else echo hid\"\"den; fi"
    ]);
    bundle.write(&config);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(BUNDLE_VAR, bundle.arg()) };
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let job = shell.spawn(Mode::Foreground, &["run", "--bundle", &bundle]);
        shell.wait(job);
    });
    session.job();
    session.expect("sees-shipyard");
    session.expect("hidden");
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    assert_eq!(session.count("reached"), 0, "{}", session.transcript());
    session.finish();
}

/// A terminal that takes nothing (Ctrl-S: the user paused it) can't keep
/// shipyard once the command has ended: its last output waits for the
/// terminal, but a stop request (SIGTERM) throws that away. Twice: output
/// that fits in the pipe and the relay (the command ends; SIGTERM reaches
/// shipyard as it waits on the terminal), and output that doesn't (the
/// command is blocked; SIGTERM stops it first).
fn a_stalled_terminal_doesnt_keep_shipyard_after_a_stop_request() {
    for (bytes, ends) in [
        ("100000", Status::Exited(0)),
        ("400000", Status::Exited(killed_by(libc::SIGTERM))),
    ] {
        // SAFETY: this test binary is single-threaded (see the module docs).
        unsafe { std::env::set_var(STALL_BYTES_VAR, bytes) };
        let mut session = session(|shell| {
            let bytes = std::env::var(STALL_BYTES_VAR).expect("the test sets the size");
            let script = format!("echo re\"\"ady; sleep 1; head -c {bytes} /dev/zero | tr '\\0' x");
            let job = shell.spawn(Mode::Foreground, &["run", "sh", "-c", &script]);
            shell.wait(job);
        });
        let shipyard = session.job();
        session.expect("ready");
        // XOFF: the terminal stops taking output.
        session.type_keys(b"\x13");
        std::thread::sleep(std::time::Duration::from_secs(3));
        // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the
        // job.
        unsafe { libc::kill(shipyard, libc::SIGTERM) };
        assert_eq!(session.status(), ends, "{bytes}: {}", session.transcript());
        // (The shell is done and its terminal closed: nothing to resume.)
        session.finish();
    }
}

/// The size of [`a_stalled_terminal_doesnt_keep_shipyard_after_a_stop_request`]'s
/// output, for the forked shell.
const STALL_BYTES_VAR: &str = "SHIPYARD_TEST_STALL_BYTES";

/// A `poststart` hook runs while the relay waits (it runs in shipyard's
/// loop, between its turns: docs/user/bundles.md): the command's output
/// still arrives, all of it, once the hook is done.
fn a_poststart_hook_doesnt_hold_up_the_output() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["hooks"] = serde_json::json!({"poststart": [{
        "path": "/bin/sh",
        "args": ["sh", "-c", "sleep 1"]
    }]});
    config["process"]["args"] =
        serde_json::json!(["sh", "-c", "echo fir\"\"st; sleep 2; echo la\"\"st"]);
    bundle.write(&config);
    // SAFETY: this test binary is single-threaded (see the module docs).
    unsafe { std::env::set_var(BUNDLE_VAR, bundle.arg()) };
    let mut session = session(|shell| {
        let bundle = std::env::var(BUNDLE_VAR).expect("the test sets the bundle");
        let job = shell.spawn(Mode::Foreground, &["run", "--bundle", &bundle]);
        shell.wait(job);
    });
    session.job();
    session.expect("first");
    session.expect("last");
    assert_eq!(
        session.status(),
        Status::Exited(0),
        "{}",
        session.transcript()
    );
    session.finish();
}

/// The relay reads and writes the terminal through an open file of its own:
/// the shell's stays blocking even when shipyard is killed outright, with no
/// chance to put anything back (a shared `O_NONBLOCK` would make the shell's
/// next read fail).
fn the_shells_terminal_stays_blocking_after_kill_9_of_shipyard() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "-i", "sh", "-c", "echo re\"\"ady; read x"],
        );
        shell.wait(job);
        shell.report_blocking();
    });
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the job.
    unsafe { libc::kill(shipyard, libc::SIGKILL) };
    assert_eq!(session.status(), Status::Signaled(libc::SIGKILL));
    session.expect("terminal blocking");
    session.finish();
}

/// Window-size changes are forwarded once. (The command has no terminal to
/// read the size from: D16. With `-t`, the size is its own terminal's.)
fn isolated_window_size_changes_are_forwarded() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "sh",
                "-c",
                "trap 'echo got-w\"\"inch; exit 0' WINCH; echo re\"\"ady; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("giving the command its own process group");
    session.expect("ready");
    session.resize(40, 100);
    session.expect("got-winch");
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
/// (ENXIO), and its descriptors aren't a terminal either (D16). Programs
/// that open `/dev/tty` to ask for a password fail without `-t`
/// (docs/user/run.md).
fn isolated_command_has_no_controlling_terminal() {
    let mut session = session(foreground_isolated!(
        "tty; if (: < /dev/tty) 2>/dev/null; then echo has-c\"\"tty; else echo no-c\"\"tty; fi"
    ));
    session.job();
    session.expect("not a tty");
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

/// As [`isolated_window_size_changes_are_forwarded`], in a root filesystem.
fn with_a_root_filesystem_window_size_changes_are_forwarded() {
    let _root = rootfs();
    let mut session = session(|shell| {
        foreground_in_rootfs(
            shell,
            &["--debug"],
            "trap 'echo got-w\"\"inch; exit 0' WINCH; echo re\"\"ady; while :; do sleep 0.1; done",
        );
    });
    session.job();
    session.expect("ready");
    session.resize(40, 100);
    session.expect("got-winch");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// In a root filesystem the command has no terminal either (D16): `tty`
/// says so, and its output still arrives, relayed. (Its `/dev/pts` is its
/// own; `-t` gives it a terminal there.)
fn in_a_root_filesystem_the_command_has_no_terminal() {
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
const PLAIN_ON_A_TTY: &str = "plain-on-a-tty";
const KILLED_ON_A_TTY: &str = "killed-on-a-tty";
const KILLED_PLAIN: &str = "killed-plain";
/// What a test's fake shell runs.
type Script = fn(&mut Shell);

/// A state root, its containers deleted and itself removed when dropped,
/// so a failed test leaves no container running.
struct StateRoot(std::path::PathBuf);

impl Drop for StateRoot {
    fn drop(&mut self) {
        // Every container in it, whichever test made it.
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            let _ = std::process::Command::new(RUNTIME.path())
                .arg("--root")
                .arg(&self.0)
                .arg("delete")
                .arg("--force")
                .arg(entry.file_name())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
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
            let job = shell.spawn_program(Mode::Foreground, RUNTIME.path(), &args);
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
/// goes on, its output waiting for the relay), and the run's cgroup and
/// limits stay through the stop; `fg` resumes shipyard, which passes the
/// output on and reports the end, and the cgroup goes with the run.
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
    // Its output waits in the relay's pipe until shipyard is resumed (D16).
    session.go();
    session.expect("done");
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
            &["run", "-i", "sh", "-c", "read x; echo \"got $x\""],
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
            let job = shell.spawn_program(Mode::Foreground, RUNTIME.path(), &args);
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

// --- `-t`: a terminal of the container's own (stage 8, step 8.2b) ---

/// Starts `shipyard run ARGS… sh -c SCRIPT` in the foreground and waits.
macro_rules! foreground_t {
    ([$($arg:expr),*], $script:expr) => {
        |shell: &mut Shell| {
            let job = shell.spawn(Mode::Foreground, &["run", $($arg,)* "sh", "-c", $script]);
            shell.wait(job);
        }
    };
}

/// `tty` names the container's own terminal: the first of its own devpts,
/// with a root filesystem and without one (8.2a's devpts).
fn with_t_tty_names_the_containers_own_terminal() {
    let mut without = session(foreground_t!(["-it"], "tty"));
    without.job();
    without.expect("/dev/pts/0");
    assert_eq!(without.status(), Status::Exited(0));
    without.finish();
    let _root = rootfs();
    let mut session = session(|shell| {
        let path = std::env::var(ROOTFS_VAR).expect("the test sets the root filesystem");
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "-it", "--rootfs", &path, "sh", "-c", "tty"],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("/dev/pts/0");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The container's terminal starts at the user's size (30 by 90 here: not
/// the default 24 by 80, nor 0 by 0 as a new one would be) and follows it.
fn with_t_stty_size_follows_the_window() {
    let mut session = session(|shell| {
        shell.await_go();
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "-it",
                "sh",
                "-c",
                "stty size; trap 'stty size; exit 0' WINCH; echo re\"\"ady; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    session.resize(30, 90);
    session.go();
    session.job();
    session.expect("30 90");
    session.expect("ready");
    session.resize(40, 100);
    session.expect("40 100");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// `/dev/tty` opens: the terminal is the command's controlling terminal (how
/// `sudo` and `ssh` ask for a password).
fn with_t_dev_tty_opens() {
    let mut session = session(foreground_t!(["-it"], "echo via-dev-t\"\"ty > /dev/tty"));
    session.job();
    session.expect("via-dev-tty");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// With `-it` the user's terminal is raw: Ctrl-C is a byte the container's
/// terminal turns into SIGINT for its foreground job. Shipyard gets no
/// signal and forwards none.
fn with_it_ctrl_c_reaches_the_inner_job_only() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "-it",
                "sh",
                "-c",
                "trap 'echo got-i\"\"nt; exit 0' INT; echo re\"\"ady; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_C);
    session.expect("got-int");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(
        session.count("forwarding a signal"),
        0,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// Ctrl-Z stops the container's foreground job (job control inside), not
/// Shipyard: the inner shell carries on at once.
fn with_it_ctrl_z_stops_the_inner_job_not_shipyard() {
    let mut session = session(foreground_t!(
        ["-it"],
        "set -m; echo re\"\"ady; sleep 5; echo after-sl\"\"eep"
    ));
    let shipyard = session.job();
    session.expect("ready");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let stopped_at = std::time::Instant::now();
    session.type_keys(CTRL_Z);
    // Long before the 5 s sleep would have ended: it was stopped.
    session.expect("after-sleep");
    assert!(stopped_at.elapsed() < std::time::Duration::from_secs(3));
    assert!(
        !process_state(shipyard).starts_with('T'),
        "shipyard was stopped"
    );
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The command's exit code is the run's.
fn with_t_exit_codes_pass_through() {
    let mut session = session(foreground_t!(["-it"], "exit 7"));
    session.job();
    assert_eq!(session.status(), Status::Exited(7));
    session.finish();
}

/// The user's terminal is put back after the run, even when the command
/// dies of SIGKILL (F17's case, now with Shipyard the only one to change it).
fn the_terminal_is_restored_after_kill_9_of_the_command() {
    let mut session = session(|shell| {
        let before = shell.terminal_settings();
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "-it", "sh", "-c", "echo re\"\"ady; kill -9 $$"],
        );
        shell.wait(job);
        shell.report_settings(&before);
    });
    session.job();
    session.expect("ready");
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGKILL)));
    session.expect("settings unchanged");
    session.finish();
}

/// Stopped (`kill -TSTP`: Ctrl-Z can't reach it in raw mode), Shipyard puts
/// the terminal back for the shell; `fg` makes it raw again, and the
/// container's shell is still there.
fn the_terminal_is_restored_while_shipyard_is_stopped() {
    let mut session = session(|shell| {
        let before = shell.terminal_settings();
        let job = shell.spawn(Mode::Foreground, &["run", "-it", "sh"]);
        shell.wait(job);
        shell.report_settings(&before);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("# ");
    // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the job.
    unsafe { libc::kill(shipyard, libc::SIGTSTP) };
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    session.expect("settings unchanged");
    session.go();
    session.type_keys(b"exit 4\r");
    assert_eq!(
        session.status(),
        Status::Exited(4),
        "{}",
        session.transcript()
    );
    session.finish();
}

/// The command's descriptors are its own terminal's: no master (`ptmx`),
/// nothing of the user's.
fn with_t_the_containers_descriptors_are_its_terminal_only() {
    let mut session = session(foreground_t!(
        ["-it"],
        "ls -l /proc/self/fd/; echo do\"\"ne"
    ));
    session.job();
    session.expect("/dev/pts/0");
    session.expect("done");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(session.count("ptmx"), 0, "{}", session.transcript());
    session.finish();
}

/// `-it` in the background waits for `fg` before making the terminal raw
/// (job control: SIGTTOU), then runs.
fn with_it_in_the_background_waits_for_fg() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &["run", "-it", "sh", "-c", "echo started-\"\"ok"],
        );
        shell.wait(job);
        shell.await_go();
        shell.fg(job);
        shell.wait(job);
    });
    session.job();
    assert_eq!(session.status(), Status::Stopped(libc::SIGTTOU));
    session.go();
    session.expect("started-ok");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// With SIGTTOU ignored nothing would stop Shipyard in the background, and a
/// background program mustn't change the shell's terminal: the run goes on
/// without raw mode or input.
fn with_it_in_the_background_ignoring_sigttou_leaves_the_terminal_alone() {
    let mut session = session(|shell| {
        let before = shell.terminal_settings();
        let script = format!(
            "trap '' TTOU; exec {} run -it sh -c 'echo ran-an\"\"yway'",
            SHIPYARD.path()
        );
        let job = shell.spawn_program(Mode::Background, "/bin/sh", &["-c", &script]);
        shell.wait(job);
        shell.report_settings(&before);
    });
    session.job();
    session.expect("ran-anyway");
    assert_eq!(session.status(), Status::Exited(0));
    session.expect("settings unchanged");
    session.finish();
}

/// `kill %1` (SIGTERM, then SIGCONT) ends a background `-it` stopped while
/// waiting for the terminal: it doesn't stop again for it.
fn kill_of_a_run_waiting_for_the_terminal_ends_it() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Background,
            &["run", "-it", "--stop-timeout", "1", "sh", "-c", "sleep 100"],
        );
        shell.wait(job);
        shell.await_go();
        shell.wait(job);
    });
    let shipyard = session.job();
    assert_eq!(session.status(), Status::Stopped(libc::SIGTTOU));
    // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the job.
    unsafe {
        libc::kill(shipyard, libc::SIGTERM);
        libc::kill(shipyard, libc::SIGCONT);
    }
    session.go();
    let ended = session.status();
    assert!(
        matches!(ended, Status::Exited(code) if code == killed_by(libc::SIGTERM) || code == killed_by(libc::SIGKILL)),
        "{ended:?}\n{}",
        session.transcript()
    );
    session.finish();
}

/// A background `-it` waiting for `fg` whose container dies meanwhile (its
/// init killed from outside): once continued, it ends, rather than stopping
/// again for a terminal its command will never use.
fn a_background_run_whose_container_dies_doesnt_wait_for_fg() {
    let mut session = session(|shell| {
        // No network: no pasta, so the init is Shipyard's only child.
        let job = shell.spawn(
            Mode::Background,
            &["run", "-it", "--network", "none", "sh", "-c", "sleep 100"],
        );
        shell.wait(job);
        shell.await_go();
        shell.wait(job);
    });
    let shipyard = session.job();
    assert_eq!(session.status(), Status::Stopped(libc::SIGTTOU));
    let init = child_of(shipyard);
    // SAFETY: `kill` has no memory-safety conditions; `init` is the
    // container's init.
    unsafe { libc::kill(init, libc::SIGKILL) };
    // Dead (a zombie, for Shipyard to reap) before Shipyard goes on.
    eventually("the init to die", || process_state(init).starts_with('Z'));
    // SAFETY: as above; `shipyard` is the job.
    unsafe { libc::kill(shipyard, libc::SIGCONT) };
    session.go();
    let ended = session.status();
    assert!(
        matches!(ended, Status::Exited(_)),
        "{ended:?}\n{}",
        session.transcript()
    );
    session.finish();
}

/// Stopped with `kill -TSTP`, then continued in the background (`bg`), a
/// raw run leaves the shell's terminal as it is, even where nothing would
/// stop it for changing it (SIGTTOU and SIGTTIN ignored): raw again only in
/// the foreground.
fn a_raw_run_continued_in_the_background_leaves_the_terminal_alone() {
    let mut session = session(|shell| {
        let before = shell.terminal_settings();
        let script = format!(
            "trap '' TTOU TTIN; exec {} run -it sh -c 'echo re\"\"ady; sleep 3'",
            SHIPYARD.path()
        );
        let job = shell.spawn_program(Mode::Foreground, "/bin/sh", &["-c", &script]);
        shell.wait(job);
        shell.await_go();
        shell.bg(job);
        std::thread::sleep(std::time::Duration::from_millis(500));
        shell.report_settings(&before);
        shell.wait(job);
    });
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the job.
    unsafe { libc::kill(shipyard, libc::SIGTSTP) };
    assert_eq!(session.status(), Status::Stopped(libc::SIGTSTP));
    session.go();
    session.expect("settings unchanged");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A lot of output through the terminal: all of it, and the run ends.
fn a_flood_through_the_terminal_ends_promptly() {
    let mut session = session(foreground_t!(
        ["-it"],
        "head -c 3000000 /dev/zero | tr '\\0' x; echo; echo do\"\"ne"
    ));
    session.job();
    session.expect("done");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// A background job keeping the terminal open doesn't keep the run: it ends
/// when the command does (the container goes with it).
fn a_background_job_holding_the_terminal_doesnt_hang_the_run() {
    let mut session = session(foreground_t!(["-it"], "(sleep 100 &); echo do\"\"ne"));
    session.job();
    session.expect("done");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// Programs that make a terminal of their own (`script`, `tmux`, `ssh`) make
/// it on the container's devpts, where they find it, without a root
/// filesystem too.
fn with_t_script_finds_its_pty_without_a_root_filesystem() {
    let mut session = session(foreground_t!(["-it"], "script -qc tty /dev/null"));
    session.job();
    session.expect("/dev/pts/1");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The user's terminal hangs up: so does the container's (its session gets
/// SIGHUP), and the run ends 129.
fn with_t_hangup_ends_the_command() {
    let mut session = session(foreground_t!(["-it"], "echo re\"\"ady; exec sleep 100"));
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(killed_by(libc::SIGHUP)));
    session.finish();
}

/// A command deaf to SIGHUP that reads its terminal until it ends, then
/// exits 5.
const HUP_DEAF: &str = "trap '' HUP; echo re\"\"ady; while read x; do :; done; exit 5";

/// A hang-up is the terminal's, not only a signal: a command ignoring
/// SIGHUP still finds its terminal gone (its reads end), as on a real
/// terminal, rather than waiting on one nobody can type on.
fn with_t_hangup_ends_the_input_of_a_command_ignoring_it() {
    let mut session = session(|shell| {
        let job = shell.spawn(Mode::Foreground, &["run", "-it", "sh", "-c", HUP_DEAF]);
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(5));
    session.finish();
}

/// `kill -HUP` (a reload, not a hang-up) is forwarded to the command, and
/// its terminal stays.
fn with_t_kill_hup_is_forwarded() {
    let mut session = session(foreground_t!(
        ["-it"],
        "trap 'echo got-h\"\"up; exit 0' HUP; echo re\"\"ady; while :; do sleep 0.1; done"
    ));
    let shipyard = session.job();
    session.expect("ready");
    // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the job.
    unsafe { libc::kill(shipyard, libc::SIGHUP) };
    session.expect("got-hup");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The terminal belongs to the command's user, so programs that open it by
/// name can (`GPG_TTY`, `screen`).
fn with_t_the_terminal_is_the_commands_user() {
    // As root: a rootless run here gets only the single-ID map (the fake
    // shell's jobs have no PATH to find newuidmap), where 1000 doesn't exist.
    if !is_root() {
        println!("skipping: only root's runs map user 1000 here");
        return;
    }
    let mut session = session(foreground_t!(
        ["-it", "--user", "1000"],
        "test -O \"$(tty)\" && echo owns-it || echo not-its-own"
    ));
    session.job();
    session.expect("owns-it");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// The name of the terminal the fake shell runs on (its stdin).
fn shells_terminal() -> String {
    // SAFETY: `ttyname` returns a string in static storage (or null), read
    // at once; this binary is single-threaded.
    let name = unsafe { libc::ttyname(libc::STDIN_FILENO) };
    assert!(!name.is_null(), "the fake shell runs on a terminal");
    // SAFETY: non-null, so a C string.
    unsafe { std::ffi::CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

/// Without isolation `-t` makes a terminal on the host: the command's own,
/// not the user's (Linux and macOS).
fn unisolated_t_gives_a_terminal_of_its_own() {
    let mut session = session(|shell| {
        let users = format!("USERS_TTY={}", shells_terminal());
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "run",
                "--no-isolation",
                "-it",
                "-e",
                &users,
                "sh",
                "-c",
                "t=$(tty); [ -t 0 ] && case $t in /dev/*) [ \"$t\" != \"$USERS_TTY\" ] && echo own-te\"\"rminal;; esac",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("own-terminal");
    assert_eq!(session.status(), Status::Exited(0));
    session.finish();
}

/// Without isolation too (Linux and macOS), closing the user's terminal
/// hangs up the command's: one deaf to SIGHUP finds its input ended.
fn unisolated_t_hangup_ends_the_input_of_a_command_ignoring_it() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &["run", "--no-isolation", "-it", "sh", "-c", HUP_DEAF],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    session.hang_up();
    assert_eq!(session.status(), Status::Exited(5));
    session.finish();
}

/// Without isolation, `-it`'s Ctrl-C reaches the command through its own
/// terminal: a byte its line discipline makes SIGINT, not a signal Shipyard
/// forwards.
fn unisolated_it_ctrl_c_reaches_the_command() {
    let mut session = session(|shell| {
        let job = shell.spawn(
            Mode::Foreground,
            &[
                "--debug",
                "run",
                "--no-isolation",
                "-it",
                "sh",
                "-c",
                "trap 'echo got-i\"\"nt; exit 0' INT; echo re\"\"ady; while :; do sleep 0.1; done",
            ],
        );
        shell.wait(job);
    });
    session.job();
    session.expect("ready");
    session.type_keys(CTRL_C);
    session.expect("got-int");
    assert_eq!(session.status(), Status::Exited(0));
    assert_eq!(
        session.count("forwarding a signal"),
        0,
        "{}",
        session.transcript()
    );
    session.finish();
}

/// SIGTSTP sent to `-it` runs at every point from their start to their end,
/// once or twice in a row, then `fg`: the terminal is never left raw (nor
/// raw settings saved as the ones to put back).
fn the_terminal_survives_stops_around_the_end_of_runs() {
    const RUNS: u64 = 24;
    let mut session = session(|shell| {
        let before = shell.terminal_settings();
        for _ in 0..RUNS {
            let job = shell.spawn(Mode::Foreground, &["run", "-it", "sh", "-c", "sleep 0.05"]);
            while let Status::Stopped(_) = shell.wait(job) {
                shell.await_go();
                shell.fg(job);
            }
        }
        shell.report_settings(&before);
    });
    for run in 0..RUNS {
        let shipyard = session.job();
        std::thread::sleep(std::time::Duration::from_millis(run * 10));
        // SAFETY: `kill` has no memory-safety conditions; `shipyard` is the
        // job (or has ended: then ESRCH).
        unsafe {
            libc::kill(shipyard, libc::SIGTSTP);
            if run % 2 == 1 {
                libc::kill(shipyard, libc::SIGTSTP);
            }
        }
        loop {
            match session.status() {
                Status::Stopped(_) => session.go(),
                Status::Exited(0) => break,
                other => panic!("{other:?}\n{}", session.transcript()),
            }
        }
    }
    session.expect("settings unchanged");
    session.finish();
}
