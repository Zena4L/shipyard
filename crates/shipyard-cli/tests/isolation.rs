//! End-to-end tests of isolation: `shipyard run` starting the command in new
//! PID, mount, UTS and IPC namespaces (Linux).
//!
//! Most tests here run rootful, so they're `#[ignore]`d in a normal run. CI's
//! `root` job and the root pass of `scripts/test-linux.sh` run them with
//! `-- --include-ignored`. As a normal user, an isolated run is rootless
//! (`rootless.rs` tests that in depth).
//!
//! The other test files use `--no-isolation`: they test process behaviour
//! that's the same with or without namespaces.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};

use shipyard_test_support::{
    Run, Shipyard, block, child_of, gone, ignore, killed_by, run, start_command,
};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `shipyard run ARGS…`, isolated (the default on Linux).
fn isolated(args: &[&str]) -> Run {
    run(SHIPYARD.command(&[&["run"], args].concat()))
}

/// Runs `shipyard run ARGS…` able to look into the container's init
/// (`/proc/1/fd`, `environ`, `cwd`): root inside can't without
/// `CAP_SYS_PTRACE` (the init isn't dumpable), and these tests look at what
/// the init holds, or at what reaching its pipes would do.
fn inspecting_the_init(args: &[&str]) -> Run {
    isolated(&[&["--cap-add", "SYS_PTRACE"], args].concat())
}

/// The namespace IDs (`/proc/self/ns/*` links) of the test process itself.
fn own_namespaces() -> Vec<String> {
    ["pid", "mnt", "uts", "ipc"]
        .iter()
        .map(|kind| {
            std::fs::read_link(format!("/proc/self/ns/{kind}"))
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// A script printing the command's namespace IDs, one per line.
const PRINT_NAMESPACES: &str = "for n in pid mnt uts ipc; do readlink /proc/self/ns/$n; done";

/// Without root, an isolated run is rootless: in a user namespace whose
/// root is the caller.
#[test]
fn without_root_isolation_is_rootless() {
    if !shipyard_test_support::rootless(shipyard_test_support::Rootless::Single) {
        return;
    }
    let run = isolated(&["sh", "-c", "head -1 /proc/self/uid_map; id -u"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    // SAFETY: `getuid` has no preconditions.
    let me = unsafe { libc::getuid() }.to_string();
    let words: Vec<&str> = run.stdout.split_whitespace().collect();
    assert_eq!(&words[..], ["0", me.as_str(), "1", "0"], "{}", run.stdout);
}

#[test]
#[ignore = "needs root"]
fn every_namespace_is_new() {
    let run = isolated(&["sh", "-c", PRINT_NAMESPACES]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let inside: Vec<&str> = run.stdout.lines().collect();
    for (inside, outside) in inside.iter().zip(own_namespaces()) {
        assert_ne!(*inside, outside, "the command shares a namespace");
    }
    assert_eq!(inside.len(), 4, "{}", run.stdout);
}

#[test]
#[ignore = "needs root"]
fn without_isolation_the_namespaces_are_shared() {
    let run = isolated(&["--no-isolation", "sh", "-c", PRINT_NAMESPACES]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let inside: Vec<String> = run.stdout.lines().map(str::to_owned).collect();
    assert_eq!(inside, own_namespaces());
}

/// The container's PID 1 is shipyard's init; the command is PID 2.
#[test]
#[ignore = "needs root"]
fn the_command_runs_under_an_init_and_sees_only_its_own_processes() {
    let run = isolated(&[
        "sh",
        "-c",
        "echo $$; cat /proc/1/comm; ls /proc | grep -c '^[0-9][0-9]*$'",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mut lines = run.stdout.lines();
    assert_eq!(lines.next(), Some("2"), "the command is PID 2");
    assert_eq!(lines.next(), Some("shipyard-init"), "PID 1 is the init");
    let processes: usize = lines.next().unwrap().trim().parse().unwrap();
    // The init, sh, ls and grep: nothing from the host.
    assert!(
        processes <= 5,
        "{processes} processes visible: /proc is the host's"
    );
}

#[test]
#[ignore = "needs root"]
fn the_hostname_is_the_containers_own() {
    let host = nix::unistd::gethostname().unwrap();
    let run = isolated(&["--hostname", "box", "hostname"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "box");
    assert_eq!(
        nix::unistd::gethostname().unwrap(),
        host,
        "the host's hostname changed"
    );
}

#[test]
#[ignore = "needs root"]
fn mounts_made_inside_do_not_reach_the_host() {
    // Only its own mount points are compared: other tests (and the machine)
    // may mount things meanwhile.
    let dir = format!("/tmp/shipyard-test-leak-{}", std::process::id());
    std::fs::create_dir_all(&dir).unwrap();
    let procs_before = mounts_at("/proc");
    // The container mounts its own /proc; also a tmpfs, for good measure.
    // Mounting needs CAP_SYS_ADMIN, which a container doesn't have by default.
    let run = isolated(&[
        "--cap-add",
        "SYS_ADMIN",
        "sh",
        "-c",
        &format!("mount -t tmpfs none {dir} && echo mounted"),
    ]);
    std::fs::remove_dir(&dir).unwrap();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "mounted");
    assert_eq!(mounts_at(&dir), 0, "the tmpfs leaked to the host");
    assert_eq!(
        mounts_at("/proc"),
        procs_before,
        "a /proc leaked to the host"
    );
}

/// How many mounts this process sees at `path` (`mountinfo`'s fifth field).
fn mounts_at(path: &str) -> usize {
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .filter(|line| line.split(' ').nth(4) == Some(path))
        .count()
}

#[test]
#[ignore = "needs root"]
fn results_come_back_through_the_namespaces() {
    assert_eq!(isolated(&["sh", "-c", "exit 3"]).code, Some(3));
    let missing = isolated(&["no-such-command"]);
    assert_eq!(missing.code, Some(127), "{}", missing.stderr);
    let workdir = isolated(&["-w", "/no/such/dir", "true"]);
    assert_eq!(workdir.code, Some(125), "{}", workdir.stderr);
}

#[test]
#[ignore = "needs root"]
fn a_stop_request_reaches_a_command_that_handles_it() {
    let running = start_command(SHIPYARD.command(&[
        "run",
        "sh",
        "-c",
        "trap 'exit 5' TERM; echo ready; sleep 100 & wait",
    ]));
    assert_eq!(running.next_line(), "ready");
    running.signal(libc::SIGTERM);
    assert_eq!(running.finish().0, 5);
}

/// The command isn't PID 1, so a signal it has no handler for has its
/// normal effect: `sleep` stops on SIGTERM at once, not at the timeout.
#[test]
#[ignore = "needs root"]
fn sigterm_stops_a_command_without_a_handler() {
    let running = start_command(SHIPYARD.command(&[
        "run",
        "--stop-timeout",
        "10",
        "sh",
        "-c",
        "echo ready; exec sleep 100",
    ]));
    assert_eq!(running.next_line(), "ready");
    let asked = Instant::now();
    running.signal(libc::SIGTERM);
    assert_eq!(running.finish().0, killed_by(libc::SIGTERM));
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "waited for the timeout"
    );
}

/// What Ctrl-C does: SIGINT reaches the command through the init, and
/// shipyard then dies of SIGINT too (so a shell loop around it stops).
#[test]
#[ignore = "needs root"]
fn sigint_ends_the_command_and_shipyard() {
    let running =
        start_command(SHIPYARD.command(&["run", "sh", "-c", "echo ready; exec sleep 100"]));
    assert_eq!(running.next_line(), "ready");
    running.signal(libc::SIGINT);
    let status = running.finish_status();
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
}

/// A command that ignores the stop signal is killed when the timeout runs
/// out: the init dies with it, so shipyard reports the SIGKILL.
#[test]
#[ignore = "needs root"]
fn a_command_ignoring_the_stop_signal_is_killed_after_the_timeout() {
    let running = start_command(SHIPYARD.command(&[
        "run",
        "--stop-timeout",
        "1",
        "sh",
        "-c",
        "trap '' TERM; echo ready; while :; do sleep 0.1; done",
    ]));
    assert_eq!(running.next_line(), "ready");
    let asked = Instant::now();
    running.signal(libc::SIGTERM);
    assert_eq!(running.finish().0, killed_by(libc::SIGKILL));
    assert!(asked.elapsed() >= Duration::from_millis(900));
}

/// The command's death by a signal comes through exactly, although the
/// init (PID 1) can't die by one itself.
#[test]
#[ignore = "needs root"]
fn death_by_a_signal_comes_through_the_init() {
    let term = isolated(&["sh", "-c", "kill -TERM $$"]);
    assert_eq!(term.code, Some(killed_by(libc::SIGTERM)), "{}", term.stderr);
    let int = isolated(&["sh", "-c", "kill -INT $$"]);
    assert_eq!(int.signal, Some(libc::SIGINT), "{}", int.stderr);
}

/// Orphans re-parented to the init are reaped: no zombies pile up.
#[test]
#[ignore = "needs root"]
fn the_init_reaps_orphans() {
    let run = isolated(&[
        "sh",
        "-c",
        "(sleep 0.1 &); sleep 1; ps -A -o stat= | grep -c '^Z' || true",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "0", "zombies left behind");
}

/// When the command exits, everything else in the container goes with it,
/// even a process that left the command's session.
#[test]
#[ignore = "needs root"]
fn the_containers_other_processes_end_with_the_command() {
    let run = isolated(&["sh", "-c", "setsid sleep 300.42 >/dev/null 2>&1 & exit 0"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = std::process::Command::new("pgrep")
            .args(["-f", "sleep 300.42"])
            .output()
            .unwrap();
        if found.stdout.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the container's sleep outlived it"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A descriptor leaked by shipyard's parent isn't held open by the
/// container's PID 1 for its whole life.
#[test]
#[ignore = "needs root"]
fn the_init_does_not_hold_leaked_descriptors() {
    let run = run(SHIPYARD.command_with(
        &[
            "run",
            // To look into the init at all (see `inspecting_the_init`):
            // without it, every fd reads as "closed".
            "--cap-add",
            "SYS_PTRACE",
            "sh",
            "-c",
            "if [ -e /proc/1/fd/60 ]; then echo pinned; else echo closed; fi",
        ],
        || {
            // SAFETY: `dup2` is async-signal-safe.
            if unsafe { libc::dup2(2, 60) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        },
    ));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "closed");
}

/// A parent that ignored or blocked SIGCHLD doesn't stop the init from
/// noticing the command's exit.
#[test]
#[ignore = "needs root"]
fn the_init_survives_a_hostile_parent() {
    let hostile: [fn() -> std::io::Result<()>; 2] =
        [|| ignore(libc::SIGCHLD), || block(libc::SIGCHLD)];
    for setup in hostile {
        let run = run(SHIPYARD.command_with(&["run", "sh", "-c", "sleep 0.2; exit 3"], setup));
        assert_eq!(run.code, Some(3), "{}", run.stderr);
    }
}

#[test]
#[ignore = "needs root"]
fn the_container_dies_with_shipyard() {
    let running =
        start_command(SHIPYARD.command(&["run", "sh", "-c", "echo ready; exec sleep 100"]));
    assert_eq!(running.next_line(), "ready");
    let command = child_of(running.pid());
    // SAFETY: `kill` only sends a signal to the shipyard this test started.
    assert_eq!(unsafe { libc::kill(running.pid(), libc::SIGKILL) }, 0);
    assert!(gone(command), "the container outlived shipyard");
}

/// The init starts as a copy of shipyard, with shipyard's command line
/// (`-e` values included: `/proc/1/cmdline` is readable by every user) and
/// shipyard's own environment. It must re-exec itself clean before the
/// container can look.
#[test]
#[ignore = "needs root"]
fn the_init_carries_none_of_shipyards_secrets() {
    let mut command = SHIPYARD.command(&[
        "run",
        "-e",
        "TOKEN=hunter2-from-the-command-line",
        "-u",
        "1000:1000",
        "sh",
        "-c",
        "tr '\\0' ' ' < /proc/1/cmdline",
    ]);
    command.env("SHIPYARD_TEST_SECRET", "hunter2-from-the-environment");
    let cmdline = run(command);
    assert_eq!(cmdline.code, Some(0), "{}", cmdline.stderr);
    assert!(!cmdline.stdout.contains("hunter2"), "{}", cmdline.stdout);
    assert!(
        cmdline.stdout.contains("__shipyard-init"),
        "not the clean init: {}",
        cmdline.stdout
    );

    // The environment is root-only; read it as root.
    let mut command = SHIPYARD.command(&[
        "run",
        "--cap-add",
        "SYS_PTRACE",
        "sh",
        "-c",
        "tr '\\0' '\\n' < /proc/1/environ",
    ]);
    command.env("SHIPYARD_TEST_SECRET", "hunter2-from-the-environment");
    let environ = run(command);
    assert_eq!(environ.code, Some(0), "{}", environ.stderr);
    assert!(!environ.stdout.contains("hunter2"), "{}", environ.stdout);
}

/// The init gives up the terminal or pipes it inherited, so they belong to
/// the command alone (a reader sees end-of-file when the command closes).
#[test]
#[ignore = "needs root"]
fn the_init_does_not_hold_stdio() {
    let run = inspecting_the_init(&[
        "sh",
        "-c",
        "for fd in 0 1 2; do readlink /proc/1/fd/$fd; done",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let targets: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(targets, ["/dev/null"; 3], "{}", run.stdout);
}

/// Signals sent to the init from inside reach the command too, including
/// SIGPIPE, which Rust's startup sets to ignored in the re-exec'd init (the
/// init resets it; Linux would queue it anyway while it's blocked).
#[test]
#[ignore = "needs root"]
fn signals_sent_to_the_init_reach_the_command() {
    let run = isolated(&[
        "sh",
        "-c",
        "trap 'exit 9' PIPE; (sleep 0.2; kill -PIPE 1) & sleep 100 & wait",
    ]);
    assert_eq!(run.code, Some(9), "{}", run.stderr);
}

/// A shell loop that runs `action` (with `$fd` set) for each pipe the
/// container's init holds: its status pipe, reached through `/proc/1/fd`.
/// It prints `reached-the-pipe` each time, so a test can tell its forgery
/// really happened: a container that can't look into the init (no
/// `CAP_SYS_PTRACE`) would pass these tests without forging anything.
fn on_the_inits_pipes(action: &str) -> String {
    format!(
        "for fd in /proc/1/fd/*; do case $(readlink $fd) in pipe:*) {action}\n echo {REACHED} ;; esac; done"
    )
}

/// What [`on_the_inits_pipes`] prints when it reaches a pipe.
const REACHED: &str = "reached-the-pipe";

/// Writes a well-formed status record for "exited 0" to `$fd`.
const WRITE_EXIT_0: &str = r"printf '\000\000\000\000' > $fd";

/// Whatever is in the container can reach the init's status pipe through
/// `/proc/1/fd` (as root inside). A forged record mustn't change the result:
/// shipyard only believes a record that agrees with the init's own exit.
#[test]
#[ignore = "needs root"]
fn a_forged_status_record_is_not_believed() {
    let run = inspecting_the_init(&[
        "sh",
        "-c",
        &format!("{}; exit 7", on_the_inits_pipes(WRITE_EXIT_0)),
    ]);
    assert!(run.stdout.contains(REACHED), "no forgery: {}", run.stderr);
    assert_eq!(run.code, Some(7), "{}", run.stderr);
}

/// Filling the status pipe from inside mustn't freeze the init: its writes
/// don't block, so it still reports, forwards and exits at once.
#[test]
#[ignore = "needs root"]
fn a_full_status_pipe_does_not_freeze_the_init() {
    let started = Instant::now();
    let run = inspecting_the_init(&[
        "sh",
        "-c",
        &format!(
            "{}; sleep 0.5; exit 7",
            on_the_inits_pipes("head -c 100000 /dev/zero > $fd &")
        ),
    ]);
    assert!(run.stdout.contains(REACHED), "no flood: {}", run.stderr);
    assert_eq!(run.code, Some(7), "{}", run.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the init blocked until the stop timeout"
    );
}

/// The init doesn't keep shipyard's working directory busy: an unmount of
/// it on the host would fail for as long as the container runs.
#[test]
#[ignore = "needs root"]
fn the_init_does_not_hold_shipyards_working_directory() {
    let mut command =
        SHIPYARD.command(&["run", "--cap-add", "SYS_PTRACE", "readlink", "/proc/1/cwd"]);
    command.current_dir("/tmp");
    let run = run(command);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "/");
}

/// The container's mounts are slaves of the host's (not private): when the
/// host unmounts something, the container's copy goes too, so the device
/// isn't kept busy. (Mounts made inside still never reach the host:
/// `mounts_made_inside_do_not_reach_the_host`.)
///
/// The unmount event belongs to the parent mount, so the test makes its own
/// shared parent (a bind mount), whatever `/` is on this machine.
#[test]
#[ignore = "needs root"]
fn a_host_unmount_reaches_the_container() {
    let parent = format!("/tmp/shipyard-test-propagation-{}", std::process::id());
    let child = format!("{parent}/disk");
    let sh = |script: &str| {
        let status = std::process::Command::new("sh")
            .args(["-c", script])
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    };
    // Even if the test fails half-way: leftover mounts would confuse later
    // runs and other tests.
    let _cleanup = Cleanup(format!(
        "umount {child} 2>/dev/null; umount {parent} 2>/dev/null; rm -rf {parent} {parent}.unmounted"
    ));
    // Private first, then shared: a peer group of its own, not `/`'s
    // (where `/` is shared, a bind mount joins its group, and the tmpfs
    // would be propagated a second time).
    sh(&format!(
        "mkdir -p {parent} && mount --bind {parent} {parent} && mount --make-private {parent} \
         && mount --make-shared {parent} && mkdir -p {child} && mount -t tmpfs none {child}"
    ));
    // The container sees the host's files (no root filesystem of its own
    // yet), so a flag file tells it when the host has unmounted.
    let unmounted = format!("{parent}.unmounted");
    let container = start_command(SHIPYARD.command(&[
        "run",
        "sh",
        "-c",
        &format!(
            "awk '$5 == \"{child}\"' /proc/self/mountinfo | wc -l; \
             while [ ! -e {unmounted} ]; do sleep 0.05; done; \
             awk '$5 == \"{child}\"' /proc/self/mountinfo | wc -l"
        ),
    ]));
    let before = container.next_line();
    sh(&format!("umount {child} && touch {unmounted}"));
    let (code, after) = container.finish();
    assert_eq!(before, "1", "the container didn't see the mount");
    assert_eq!(code, 0);
    assert_eq!(
        after,
        ["0"],
        "the host's unmount didn't reach the container"
    );
}

/// Runs a shell script when dropped, whatever happened.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("sh")
            .args(["-c", &self.0])
            .status();
    }
}

/// Shipyard started through the dynamic loader (`ld.so ./shipyard`): the
/// re-exec'd init is then the loader, which can't start. Shipyard must say
/// so (125) rather than report the init's exit as the command's.
#[test]
#[ignore = "needs root"]
fn an_init_that_cannot_start_is_reported() {
    let Some(loader) = ["/lib/ld-linux-aarch64.so.1", "/lib64/ld-linux-x86-64.so.2"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        eprintln!("no known dynamic loader here; skipping");
        return;
    };
    // A static shipyard (musl) can't be started through the loader at all.
    let dynamic = std::process::Command::new(loader)
        .args(["--verify", env!("CARGO_BIN_EXE_shipyard")])
        .status()
        .is_ok_and(|status| status.success());
    if !dynamic {
        eprintln!("shipyard isn't dynamically linked; skipping");
        return;
    }
    let mut command = std::process::Command::new(loader);
    command.args([env!("CARGO_BIN_EXE_shipyard"), "run", "echo", "hi"]);
    let run = run(command);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("init failed to start"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("hint: --no-isolation"),
        "{}",
        run.stderr
    );
}

fn ignore_hup() -> std::io::Result<()> {
    ignore(libc::SIGHUP)
}

/// A signal ignored when shipyard started (`nohup`) stays ignored in the
/// container's command too, as without isolation.
#[test]
#[ignore = "needs root"]
fn signals_ignored_at_startup_stay_ignored_inside() {
    let run = run(SHIPYARD.command_with(
        &["run", "sh", "-c", "grep SigIgn /proc/self/status"],
        ignore_hup,
    ));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mask = run.stdout.trim().trim_start_matches("SigIgn:").trim();
    let mask = u64::from_str_radix(mask, 16).unwrap();
    assert_ne!(mask & 1, 0, "SIGHUP isn't ignored: {}", run.stdout);
}

/// The one forgery the record's shape can't reveal: the command writes a
/// well-formed "exited 0" record, then ignores the stop request, so the stop
/// timeout kills the init before it writes the real one. Only checking the
/// record against the init's own end (SIGKILL) gives the true result, 137.
#[test]
#[ignore = "needs root"]
fn a_forged_record_cannot_hide_a_kill() {
    let running = start_command(SHIPYARD.command(&[
        "run",
        "--cap-add",
        "SYS_PTRACE",
        "--stop-timeout",
        "1",
        "sh",
        "-c",
        &format!(
            "{}; trap '' TERM; echo ready; while :; do sleep 0.1; done",
            on_the_inits_pipes(WRITE_EXIT_0)
        ),
    ]));
    assert_eq!(running.next_line(), REACHED, "no forgery");
    assert_eq!(running.next_line(), "ready");
    running.signal(libc::SIGTERM);
    assert_eq!(running.finish().0, killed_by(libc::SIGKILL));
}

/// The `--no-isolation` hint is only for what isolation causes: a missing
/// working directory fails the same way without it, so no hint.
#[test]
#[ignore = "needs root"]
fn only_isolation_failures_suggest_no_isolation() {
    let run = isolated(&["--workdir", "/no/such/directory", "true"]);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("/no/such/directory"), "{}", run.stderr);
    assert!(!run.stderr.contains("hint"), "{}", run.stderr);
}

/// The setup reaches the container's init in a memfd, not a pipe: an
/// environment past a pipe's 64 KiB buffer arrives whole. (Four variables:
/// one string over 128 KiB is refused by `execve` itself, `MAX_ARG_STRLEN`.)
#[test]
#[ignore = "needs root"]
fn a_large_environment_reaches_the_command() {
    let vars: Vec<String> = (0..4)
        .map(|i| format!("BIG{i}={}", "x".repeat(50 * 1024)))
        .collect();
    let mut args = Vec::new();
    for var in &vars {
        args.extend(["-e", var.as_str()]);
    }
    args.extend([
        "sh",
        "-c",
        "echo $((${#BIG0} + ${#BIG1} + ${#BIG2} + ${#BIG3}))",
    ]);
    let run = isolated(&args);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), (200 * 1024).to_string());
}

/// Arguments and environment values that aren't UTF-8 survive the trip
/// through the init's setup byte for byte.
#[test]
#[ignore = "needs root"]
fn non_utf8_arguments_and_environment_survive() {
    use std::os::unix::ffi::OsStrExt;
    let mut command = SHIPYARD.command(&["run", "-e"]);
    command.arg(std::ffi::OsStr::from_bytes(b"BYTES=\xff\xfe"));
    command.args(["sh", "-c", "printf %s \"$1$BYTES\" | od -An -tx1", "sh"]);
    command.arg(std::ffi::OsStr::from_bytes(b"\xfd"));
    let run = run(command);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(
        run.stdout.split_whitespace().collect::<Vec<_>>(),
        ["fd", "ff", "fe"]
    );
}

/// The command doesn't inherit the init's own signal dispositions: Rust's
/// startup ignores SIGPIPE in the init, but the command gets the default.
#[test]
#[ignore = "needs root"]
fn the_command_does_not_inherit_the_inits_dispositions() {
    let run = isolated(&["sh", "-c", "grep SigIgn /proc/self/status"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mask = run.stdout.trim().trim_start_matches("SigIgn:").trim();
    let mask = u64::from_str_radix(mask, 16).unwrap();
    assert_eq!(
        mask & (1 << (libc::SIGPIPE - 1)),
        0,
        "SIGPIPE is ignored: {}",
        run.stdout
    );
}

/// The container init's hidden entry point, started by hand with
/// descriptors that aren't a sealed setup (stdio here), touches nothing: no
/// start mark or failure record written to them, just the message.
#[test]
fn the_init_entry_point_by_hand_does_nothing() {
    for fds in [&["0", "1", "2"][..], &["-1", "1", "2"], &[]] {
        let run = run(SHIPYARD.command(&[&["__shipyard-init"], fds].concat()));
        assert_eq!(run.code, Some(125), "{fds:?}: {}", run.stderr);
        assert_eq!(run.stdout, "", "{fds:?}");
        assert_eq!(
            run.stderr, "shipyard: this is the container init's internal entry point\n",
            "{fds:?}"
        );
    }
}
