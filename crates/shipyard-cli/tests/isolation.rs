//! End-to-end tests of isolation: `shipyard run` starting the command in new
//! PID, mount, UTS and IPC namespaces (Linux).
//!
//! Creating namespaces needs root, so most tests are `#[ignore]`d in a normal
//! run. CI's `root` job and the root pass of `scripts/test-linux.sh` run them
//! with `-- --include-ignored`. As a normal user, the tests check the clear
//! "needs root" error instead.
//!
//! The other test files use `--no-isolation`: they test process behaviour
//! that's the same with or without namespaces.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

mod common;

use std::time::{Duration, Instant};

use common::{gone, is_root, killed_by, run, shipyard, start_command};

/// `shipyard run ARGS…`, isolated (the default on Linux).
fn isolated(args: &[&str]) -> common::Run {
    run(shipyard(&[&["run"], args].concat()))
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

#[test]
fn without_root_isolation_is_refused_with_advice() {
    if is_root() {
        return;
    }
    let run = isolated(&["true"]);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("needs root"), "{}", run.stderr);
    assert!(run.stderr.contains("--no-isolation"), "{}", run.stderr);
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

#[test]
#[ignore = "needs root"]
fn the_command_is_pid_1_and_sees_only_its_own_processes() {
    let run = isolated(&["sh", "-c", "echo $$; ls /proc | grep -c '^[0-9][0-9]*$'"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let mut lines = run.stdout.lines();
    assert_eq!(lines.next(), Some("1"), "the command is PID 1");
    let processes: usize = lines.next().unwrap().trim().parse().unwrap();
    // sh, ls and grep: nothing from the host.
    assert!(
        processes <= 4,
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
    let before = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    // The container mounts its own /proc; also a tmpfs, for good measure.
    let run = isolated(&["sh", "-c", "mount -t tmpfs none /mnt && echo mounted"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "mounted");
    let after = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert_eq!(before, after, "a mount leaked to the host");
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
    let running = start_command(shipyard(&[
        "run",
        "sh",
        "-c",
        "trap 'exit 5' TERM; echo ready; sleep 100 & wait",
    ]));
    assert_eq!(running.next_line(), "ready");
    running.signal(libc::SIGTERM);
    assert_eq!(running.finish().0, 5);
}

/// As PID 1, a command ignores signals it has no handler for, so `sleep`
/// doesn't stop on SIGTERM: it's killed when `--stop-timeout` runs out.
/// (Until the init arrives: Docker without `--init` behaves the same.)
#[test]
#[ignore = "needs root"]
fn a_command_without_a_handler_is_killed_after_the_timeout() {
    let running = start_command(shipyard(&[
        "run",
        "--stop-timeout",
        "1",
        "sh",
        "-c",
        "echo ready; exec sleep 100",
    ]));
    assert_eq!(running.next_line(), "ready");
    let asked = Instant::now();
    running.signal(libc::SIGTERM);
    assert_eq!(running.finish().0, killed_by(libc::SIGKILL));
    assert!(asked.elapsed() >= Duration::from_millis(900));
}

#[test]
#[ignore = "needs root"]
fn the_container_dies_with_shipyard() {
    let running = start_command(shipyard(&["run", "sh", "-c", "echo ready; exec sleep 100"]));
    assert_eq!(running.next_line(), "ready");
    let command = common::child_of(running.pid());
    // SAFETY: `kill` only sends a signal to the shipyard this test started.
    assert_eq!(unsafe { libc::kill(running.pid(), libc::SIGKILL) }, 0);
    assert!(gone(command), "the container outlived shipyard");
}
