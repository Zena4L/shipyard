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
//! - `runtime.rs`: `the_monitor_stays_small_while_it_logs` (root: a
//!   monitor's own memory, and the time a flood takes to log).
//! - `runtime.rs`: `an_exec_starts_and_ends_quickly` (`exec`, rootless).
//! - `events.rs`: `fifty_thousand_events_are_read_in_linear_time`
//!   (`events --since`), and the engine's `an_emit_is_cheap` (an event
//!   written).
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
    let took = best(|| SHIPYARD.command(&["run", "--no-image", "--network", "none", "true"]));
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
            "--rm",
            "--network",
            "none",
            &image,
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
                "--rm",
                "--pull",
                "missing",
                "--network",
                "none",
                &image,
                "sh",
                "-c",
                "exit 0",
            ],
        )
    });
    let plain = best(|| SHIPYARD.command(&["run", "--no-image", "--network", "none", "true"]));
    assert!(
        cached < plain + Duration::from_millis(500),
        "cached image {cached:?}, plain {plain:?}"
    );
}

/// `run -d` is back at the prompt quickly: the container made, its monitor
/// started and the CLI gone, in well under two seconds (from an image
/// already stored). The containers are removed after.
#[test]
fn run_d_returns_quickly() {
    use std::os::unix::fs::PermissionsExt as _;
    if !runs_here() {
        return;
    }
    let (_serving, image) = fixture();
    let store = Scratch::new();
    for dir in ["containers", "networks", "xdg"] {
        let dir = store.0.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let with_stores = |args: &[&str]| {
        let mut command = with_store(&store, args);
        command
            .env("SHIPYARD_CONTAINER_DIR", store.0.join("containers"))
            .env("SHIPYARD_NETWORK_DIR", store.0.join("networks"));
        if !is_root() {
            command.env("XDG_RUNTIME_DIR", store.0.join("xdg"));
        }
        command
    };
    let first = with_stores(&[
        "run",
        "--rm",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "exit 0",
    ])
    .output()
    .expect("running shipyard");
    if !image_runs_here(&first) {
        return;
    }
    let took = best(|| with_stores(&["run", "-d", "--network", "none", &image, "sleep", "300"]));
    let ids = run(with_stores(&["ps", "-q"]));
    for id in ids.stdout.split_whitespace() {
        let _ = run(with_stores(&["rm", "-f", id]));
    }
    assert!(took < Duration::from_secs(2), "{took:?}");
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
        let first = networks.run(&["run", "--no-image", "--network", "bridge", "true"]);
        assert_eq!(first.code, Some(0), "{}", first.stderr);
        let bridge =
            best(|| networks.command(&["run", "--no-image", "--network", "bridge", "true"]));
        let none = best(|| networks.command(&["run", "--no-image", "--network", "none", "true"]));
        assert!(
            bridge < none + Duration::from_millis(500),
            "bridge {bridge:?}, none {none:?}"
        );
    });
}

/// Stores for containers: images, containers and (rootless) a runtime
/// directory, in a scratch directory.
fn container_stores() -> Scratch {
    let store = Scratch::new();
    for dir in ["containers", "xdg"] {
        let dir = store.0.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
    }
    store
}

/// `shipyard ARGS…` on [`container_stores`]' `store`.
fn on_container_stores(store: &Scratch, args: &[&str]) -> Command {
    let mut command = with_store(store, args);
    command.env("SHIPYARD_CONTAINER_DIR", store.0.join("containers"));
    if !is_root() {
        command.env("XDG_RUNTIME_DIR", store.0.join("xdg"));
    }
    command
}

/// `stop` of a container that ends on its stop signal takes well under a
/// second; of one that ignores it, its timeout (1 s here) and little more:
/// the monitor's SIGKILL comes on time, and nothing after it waits long
/// (9.4b).
#[test]
fn stop_takes_its_timeout_and_no_more() {
    if !runs_here() {
        return;
    }
    let (_serving, image) = fixture();
    let store = container_stores();
    let on_stores = |args: &[&str]| on_container_stores(&store, args);
    let started = |name: &str, command: &str| {
        let made = run(on_stores(&[
            "run",
            "-d",
            "--name",
            name,
            "--network",
            "none",
            &image,
            "sh",
            "-c",
            command,
        ]));
        assert_eq!(made.code, Some(0), "{}", made.stderr);
        made.stdout.trim().to_owned()
    };
    let probe = on_stores(&[
        "run",
        "--rm",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "exit 0",
    ])
    .output()
    .expect("running shipyard");
    if !image_runs_here(&probe) {
        return;
    }
    started("polite", "sleep 300");
    let began = Instant::now();
    let stopped = run(on_stores(&["stop", "polite"]));
    let polite = began.elapsed();
    assert_eq!(stopped.code, Some(0), "{}", stopped.stderr);
    assert!(polite < Duration::from_secs(1), "{polite:?}");
    let id = started("deaf", "trap '' TERM; touch /deaf; sleep 300");
    let deaf = store
        .store()
        .join("containers")
        .join(format!("ctr-{id}"))
        .join("upper")
        .join("deaf");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !deaf.exists() {
        assert!(Instant::now() < deadline, "never deaf");
        std::thread::sleep(Duration::from_millis(20));
    }
    let began = Instant::now();
    let stopped = run(on_stores(&["stop", "-t", "1", "deaf"]));
    let took = began.elapsed();
    assert_eq!(stopped.code, Some(0), "{}", stopped.stderr);
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(3),
        "{took:?}"
    );
    for name in ["polite", "deaf"] {
        let removed = run(on_stores(&["rm", "-f", name]));
        assert_eq!(removed.code, Some(0), "{}", removed.stderr);
    }
}

/// `ps -a` over 100 containers takes well under 200 ms: it reads records and
/// runtime states without taking a lock, so it never waits on another
/// command (step 9.4a). The containers are made, not started. A `create`
/// among them costs little more than one among none: its recovery pass
/// (step 9.5a) reads them as `ps` does.
#[test]
fn ps_over_a_hundred_containers_is_quick() {
    if !runs_here() {
        return;
    }
    let (_serving, image) = fixture();
    let store = container_stores();
    let on_stores = |args: &[&str]| on_container_stores(&store, args);
    let first = on_stores(&[
        "run",
        "--rm",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "exit 0",
    ])
    .output()
    .expect("running shipyard");
    if !image_runs_here(&first) {
        return;
    }
    let create = || on_stores(&["create", "--network", "none", &image, "sh", "-c", "exit 0"]);
    // The first few, timed: a create in a store all but empty.
    let mut few = Duration::MAX;
    for made in 0..100 {
        let started = Instant::now();
        let made_one = run(create());
        assert_eq!(made_one.code, Some(0), "{}", made_one.stderr);
        if made < TRIES {
            few = few.min(started.elapsed());
        }
    }
    let listed = run(on_stores(&["ps", "-a", "-q"]));
    assert_eq!(listed.stdout.lines().count(), 100, "{}", listed.stderr);
    let took = best(|| on_stores(&["ps", "-a"]));
    assert!(took < Duration::from_millis(200), "{took:?}");
    // Among a hundred, a create's recovery pass (stage 9, step 9.5a) reads
    // every record, as `ps` does, and takes no lock: a little more.
    let among = best(create);
    assert!(
        among < few + Duration::from_millis(100),
        "a create among a hundred {among:?}, among none {few:?}"
    );
}

/// A terminal of the container's own (`-t`: its monitor's, stage 9, step
/// 9.4c3b) adds little to a foreground container run: a pseudo-terminal
/// made and relayed, nothing waited for.
#[test]
fn a_terminal_adds_little_to_a_container_run() {
    if !runs_here() {
        return;
    }
    let (_serving, image) = fixture();
    let store = container_stores();
    let run_with = |flags: &[&str]| {
        let args = [
            &["run", "--rm", "--network", "none"][..],
            flags,
            &[image.as_str(), "sh", "-c", "exit 0"],
        ]
        .concat();
        on_container_stores(&store, &args)
    };
    let first = run_with(&[]).output().expect("running shipyard");
    if !image_runs_here(&first) {
        return;
    }
    let plain = best(|| run_with(&[]));
    let terminal = best(|| run_with(&["-t"]));
    assert!(
        terminal < plain + Duration::from_millis(100),
        "with -t {terminal:?}, without {plain:?}"
    );
}
