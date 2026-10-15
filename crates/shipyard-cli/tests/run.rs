//! End-to-end tests of `shipyard run`, driving the real binary.
//!
//! Each test starts `shipyard` as its own process: spawning changes
//! process-wide state (signal handling, waiting for children), so it can't be
//! exercised inside the test harness itself. Every run has a hard timeout.
//!
//! "Hostile parent" tests start shipyard in a deliberately bad state (leaked
//! descriptors, ignored or blocked signals) and check that the command still
//! starts clean.
//!
//! Runs use `--no-isolation`: these test process behaviour that's the same
//! with or without namespaces, without needing root. `isolation.rs` tests
//! the namespaces.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::process::Command;

use shipyard_test_support::{Run, Shipyard, block, ignore, is_root, killed_by, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Executable files that aren't valid programs. Committed (with their mode)
/// rather than written during the test: a file written while other test
/// threads fork can make exec fail with "text file busy".
const NOT_A_PROGRAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/not-a-program");
const BAD_INTERPRETER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bad-interpreter"
);

fn shipyard_run(args: &[&str]) -> Run {
    run(SHIPYARD.command(&[&["run", "--no-isolation"], args].concat()))
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
    let home = format!("HOME={}", expected_home());
    let path = format!("PATH={DEFAULT_PATH}");
    let mut expected = vec!["A=1", "EMPTY=", &home, &path];
    expected.sort_unstable();
    assert_eq!(lines, expected);
}

/// `HOME` as shipyard sets it: the current user's home directory in
/// `/etc/passwd`, or `/root` for root and `/` for others without an entry.
fn expected_home() -> String {
    let uid = nix::unistd::getuid().as_raw().to_string();
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with(['#', '+', '-']))
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.get(2) == Some(&uid.as_str()))
        .and_then(|fields| {
            fields
                .get(5)
                .filter(|home| !home.is_empty())
                .map(|home| (*home).to_owned())
        })
        .unwrap_or_else(|| if is_root() { "/root" } else { "/" }.to_owned())
}

/// A bare `-e NAME` copies the caller's value; one the caller hasn't set
/// is left out (Docker's behaviour).
#[test]
fn a_bare_env_name_copies_the_callers_value() {
    let mut command = SHIPYARD.command(&[
        "run",
        "--no-isolation",
        "-e",
        "SHIPYARD_TEST_BARE",
        "-e",
        "SHIPYARD_TEST_UNSET",
        "env",
    ]);
    command
        .env("SHIPYARD_TEST_BARE", "from-the-caller")
        .env_remove("SHIPYARD_TEST_UNSET");
    let run = run(command);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(
        run.stdout.contains("SHIPYARD_TEST_BARE=from-the-caller"),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("SHIPYARD_TEST_UNSET"),
        "{}",
        run.stdout
    );
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

/// Runs shipyard with `setup` applied to its process just before it runs.
fn hostile(args: &[&str], setup: fn() -> std::io::Result<()>) -> Run {
    run(SHIPYARD.run_unisolated_with(args, setup))
}

#[test]
fn leaked_descriptors_do_not_reach_the_command() {
    let run = hostile(
        &[
            "sh",
            "-c",
            // `true`, not `:`: in dash a failed redirection on a special
            // builtin like `:` exits the whole shell.
            "if true 2>/dev/null <&9; then echo open; else echo closed; fi",
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

/// `--preserve-fds 1`: the command gets 3, and still not another leaked
/// descriptor.
#[test]
fn preserved_descriptors_reach_the_command() {
    let run = hostile(
        &[
            "--preserve-fds",
            "1",
            "sh",
            "-c",
            "if true 2>/dev/null <&3; then echo three; fi; \
             if true 2>/dev/null <&9; then echo nine; else echo no-nine; fi",
        ],
        || {
            // SAFETY: `dup2` is async-signal-safe.
            if unsafe { libc::dup2(0, 3) } == -1 || unsafe { libc::dup2(2, 9) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        },
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "three\nno-nine\n");
}

/// A descriptor to pass that isn't open: refused, the command never runs.
#[test]
fn preserving_a_closed_descriptor_is_refused() {
    let run = hostile(&["--preserve-fds", "2", "sh", "-c", "echo ran"], || {
        // SAFETY: `dup2` and `close` are async-signal-safe.
        unsafe {
            if libc::dup2(0, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(4);
        }
        Ok(())
    });
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("descriptor 4 isn't open"),
        "{}",
        run.stderr
    );
    assert!(run.stdout.is_empty());
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

#[test]
fn children_shipyard_had_before_are_left_running() {
    // `exec` keeps a process's children: here `sleep` becomes shipyard's
    // child, but it isn't the command's, so cleanup must leave it alone.
    let mut command = Command::new("sh");
    command.args([
        "-c",
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $!; exec '{}' run --no-isolation true",
            SHIPYARD.path()
        ),
    ]);
    let run = run(command);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let sleeper: libc::pid_t = run.stdout.trim().parse().unwrap();
    // SAFETY: signal 0 only checks; then this test's own `sleep` is killed.
    unsafe {
        let alive = libc::kill(sleeper, 0) == 0;
        libc::kill(sleeper, libc::SIGKILL);
        assert!(alive, "shipyard killed a child it didn't start");
    }
}

#[test]
fn without_a_terminal_the_command_leads_its_own_process_group() {
    let run = shipyard_run(&["sh", "-c", "echo $$; ps -o pgid= -p $$"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mut lines = run.stdout.lines().map(str::trim);
    assert_eq!(lines.next(), lines.next(), "pid and process group differ");
}

#[test]
fn descriptors_above_a_lowered_limit_do_not_reach_the_command() {
    let run = hostile(
        &[
            "sh",
            "-c",
            "if [ -e /dev/fd/60 ]; then echo open; else echo closed; fi",
        ],
        || {
            // A descriptor above the soft limit, as after a parent lowered it.
            // SAFETY: `dup2` and `setrlimit` are async-signal-safe.
            unsafe {
                if libc::dup2(2, 60) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut limit: libc::rlimit = std::mem::zeroed();
                libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit);
                limit.rlim_cur = 32;
                if libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        },
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "closed\n", "fd 60 leaked into the command");
}

#[test]
fn environment_values_never_reach_the_logs() {
    // Also a value given without `KEY=`, which looks like a name.
    let run = run(SHIPYARD.command(&[
        "--log-level",
        "trace",
        "run",
        "--no-isolation",
        "-e",
        "SECRET=hunter2-value",
        "-e",
        "ghp_BARETOKEN123",
        "true",
    ]));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    for secret in ["hunter2-value", "ghp_BARETOKEN123"] {
        assert!(
            !run.stderr.contains(secret),
            "{secret} logged:\n{}",
            run.stderr
        );
    }
}

#[test]
fn hostname_mistakes_are_usage_errors() {
    // Contradictory flags.
    let both = run(SHIPYARD.command(&["run", "--no-isolation", "--hostname", "box", "true"]));
    assert_eq!(both.code, Some(125), "{}", both.stderr);
    // An invalid name; on macOS, no namespaces to give one to.
    let bad = run(SHIPYARD.command(&["run", "--no-image", "--hostname", "bad_name", "true"]));
    assert_eq!(bad.code, Some(125), "{}", bad.stderr);
    assert!(bad.stderr.contains("hostname"), "{}", bad.stderr);
}

/// Without isolation too, the command gets its OOM score adjustment.
#[cfg(target_os = "linux")]
#[test]
fn the_oom_score_adjustment_reaches_the_command() {
    let run = shipyard_run(&["--oom-score-adj", "900", "cat", "/proc/self/oom_score_adj"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "900\n");
}

// --- `-i` and `-t` (stage 8, step 8.2a) ---

/// `shipyard run ARGS…` with `input` piped to its stdin.
fn run_fed(args: &[&str], input: &[u8]) -> Run {
    shipyard_test_support::run_with_input(
        SHIPYARD.command(&[shipyard_test_support::run_words(args), args].concat()),
        Some(input),
    )
}

/// `-it` needs a terminal to type on: Docker's rule, and its words, which
/// scripts look for. Refused before anything starts.
#[test]
fn it_needs_a_terminal_to_type_on() {
    let run = run_fed(&["-it", "true"], b"");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("the input device is not a TTY"),
        "{}",
        run.stderr
    );
}

/// A bundle says in its own config.json whether it wants a terminal.
#[test]
fn t_with_a_bundle_points_at_its_config() {
    let run = run_fed(&["-t", "--bundle", "/nonexistent"], b"");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("process.terminal"), "{}", run.stderr);
}

// --- `-t` without isolation (stage 8, step 8.2b) ---

/// With no terminal at all (no controlling terminal, input and output
/// pipes: CI's case), `-t` still gives the command a terminal of its own.
#[test]
fn unisolated_t_gives_a_terminal_without_one_around() {
    let run = run_fed(
        &[
            "--no-isolation",
            "-t",
            "sh",
            "-c",
            "[ -t 0 ] && [ -t 1 ] && tty",
        ],
        b"",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stdout.starts_with("/dev/"), "{}", run.stdout);
}

/// `shipyard run -t … >> file` appends: the output goes through the file as
/// it was opened, not one opened again (at offset 0, without `O_APPEND`).
#[test]
fn unisolated_t_appends_to_its_output_file() {
    let path = std::env::temp_dir().join(format!("shipyard-t-append-{}", std::process::id()));
    std::fs::write(&path, "header\n").expect("writing the file");
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opening the file");
    let output = SHIPYARD
        .command(&["run", "--no-isolation", "-t", "echo", "body"])
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .output()
        .expect("running shipyard");
    let written = std::fs::read_to_string(&path).expect("reading the file");
    std::fs::remove_file(&path).expect("removing the file");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(written, "header\nbody\r\n");
}

/// A command that takes a terminal (`-t`) leads its own session and group
/// without isolation too, and that group is cleaned up as any command's:
/// what it leaves running there goes when the run ends (macOS included,
/// where there's no subreaper to find it).
#[test]
fn unisolated_t_leftovers_in_its_group_are_cleaned_up() {
    let run = run_fed(
        // Deaf to the hang-up its terminal gets when the run ends: only the
        // group's cleanup ends it.
        &[
            "--no-isolation",
            "-t",
            "sh",
            "-c",
            "trap '' HUP; sleep 60 & echo $!",
        ],
        b"",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let pid: libc::pid_t = run.stdout.trim().parse().expect("the sleep's PID");
    assert!(
        shipyard_test_support::gone(pid),
        "the command's leftover {pid} is still running"
    );
}
