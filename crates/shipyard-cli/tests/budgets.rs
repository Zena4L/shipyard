//! Timing budgets (H7): what a run costs, as tests a regression trips.
//!
//! Each takes the best of a few runs, against a generous bound: the bound is
//! there to catch a second gained by accident (pasta's one-second exit delay
//! was found that way, by chance), not to measure. Where a cost can be compared
//! with a plain run, the bound is on the difference, so a slow machine
//! doesn't fail it.
//!
//! The other budgets live with what they time:
//! - `pasta.rs`: `a_rootless_run_ends_as_soon_as_its_command_does` (pasta's
//!   network against none);
//! - `rootless.rs`: `relaying_adds_little_to_a_run` (a terminal relayed)
//!   and `a_terminal_adds_little_to_a_run` (`-t`);
//! - `security.rs`: `an_isolated_run_still_starts_quickly` (root).
//!
//! Linux only: these are isolated runs.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::process::Command;
use std::time::{Duration, Instant};

use shipyard_test_support::registry::{Scratch, fixture, image_runs_here};
use shipyard_test_support::{Rootless, Shipyard, is_root, rootless, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// How many times each is timed; the best counts.
const TRIES: usize = 5;

/// The best of [`TRIES`] runs of what `command` makes, each of which must
/// succeed (and is killed, failing the test, if it hangs: test-support's
/// `run`).
fn best(mut command: impl FnMut() -> Command) -> Duration {
    (0..TRIES)
        .map(|_| {
            let started = Instant::now();
            let ran = run(command());
            let took = started.elapsed();
            assert_eq!(ran.code, Some(0), "{}", ran.stderr);
            took
        })
        .min()
        .expect("at least one run")
}

/// `shipyard ARGS…` with its image store in `store`.
fn with_store(store: &Scratch, args: &[&str]) -> Command {
    let mut command = SHIPYARD.command(args);
    command.env("SHIPYARD_IMAGE_STORE", store.store());
    command
}

/// Whether isolated runs work here as this user: root, or rootless.
fn runs_here() -> bool {
    is_root() || rootless(Rootless::Single)
}

/// A run with nothing to set up but its namespaces starts and ends in under
/// a second (tens of milliseconds in the test container): a second gained
/// anywhere on its path fails it.
#[test]
fn a_run_starts_and_ends_quickly() {
    if !runs_here() {
        return;
    }
    let took = best(|| SHIPYARD.command(&["run", "--network", "none", "true"]));
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// A run from an image already in the store adds little to a plain run:
/// its layers are stacked, not fetched or unpacked again. (The fixture
/// image has a shell, not `true`.)
#[test]
fn a_cached_image_adds_little_to_a_run() {
    if !runs_here() {
        return;
    }
    let (_serving, image) = fixture();
    let store = Scratch::new();
    let first = with_store(
        &store,
        &[
            "run",
            "--image",
            &image,
            "--network",
            "none",
            "sh",
            "-c",
            "exit 0",
        ],
    )
    .output()
    .expect("running shipyard");
    if !image_runs_here(&first) {
        return;
    }
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let cached = best(|| {
        with_store(
            &store,
            &[
                "run",
                "--image",
                &image,
                "--pull",
                "missing",
                "--network",
                "none",
                "sh",
                "-c",
                "exit 0",
            ],
        )
    });
    let plain = best(|| SHIPYARD.command(&["run", "--network", "none", "true"]));
    assert!(
        cached < plain + Duration::from_millis(500),
        "cached image {cached:?}, plain {plain:?}"
    );
}

/// Pulling an image that's already stored only asks the registry for its
/// manifest: well under a second.
#[test]
fn a_cached_pull_only_checks_the_manifest() {
    // (A rootless pull unpacks in a user namespace of its own.)
    if !runs_here() {
        return;
    }
    let (_serving, image) = fixture();
    let store = Scratch::new();
    let first = run(with_store(&store, &["pull", &image]));
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let took = best(|| with_store(&store, &["pull", &image]));
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// Joining a bridge network (an address, a veth pair, firewall elements)
/// adds little to a run without a network. Root, in a network namespace of
/// the test's own.
#[test]
#[ignore = "needs root"]
fn a_bridge_network_adds_little_to_a_run() {
    shipyard_test_support::in_new_netns(|| {
        let networks = shipyard_test_support::net::NetworkDir::new(SHIPYARD);
        // The first run makes the bridge and the firewall table: not timed.
        let first = networks.run(&["run", "--network", "bridge", "true"]);
        assert_eq!(first.code, Some(0), "{}", first.stderr);
        let bridge = best(|| networks.command(&["run", "--network", "bridge", "true"]));
        let none = best(|| networks.command(&["run", "--network", "none", "true"]));
        assert!(
            bridge < none + Duration::from_millis(500),
            "bridge {bridge:?}, none {none:?}"
        );
    });
}
