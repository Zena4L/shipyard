//! End-to-end tests of `shipyard doctor` (Linux): on a host set up for
//! containers it finds nothing to fix, and says so with exit code 0; what
//! a run would trip over (no pasta, an image store on overlayfs) fails it,
//! naming the fix; and its probe leaves nothing behind.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use shipyard_test_support::{Rootless, Shipyard, delegated_cgroup, rootless};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// What a `doctor` said.
struct Report {
    code: Option<i32>,
    stdout: String,
    /// Its process ID, which names its probe's cgroup.
    pid: u32,
}

impl Report {
    /// The check line named `name`: its level and text.
    fn line(&self, name: &str) -> (&str, &str) {
        self.stdout
            .lines()
            .find_map(|line| {
                let (level, rest) = line.split_once(' ')?;
                let text = rest.trim_start().strip_prefix(name)?;
                matches!(level, "ok" | "warn" | "fail").then_some((level, text.trim_start()))
            })
            .unwrap_or_else(|| panic!("no {name} line in:\n{}", self.stdout))
    }

    fn fails(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with("fail "))
            .collect()
    }
}

/// `shipyard doctor`, as `change` sets it up.
fn doctor(change: impl FnOnce(&mut Command)) -> Report {
    let mut command = SHIPYARD.command(&["doctor"]);
    change(&mut command);
    let child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    Report {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        pid,
    }
}

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(below: &Path, name: &str) -> Self {
        let path = below.join(format!(
            "shipyard-doctor-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A host set up for rootless containers (this test container, CI): ready,
/// every probe passing; and nothing of the probe's left (its scratch
/// directory, its cgroup). Before the first pull, it probes where the store
/// will be, saying so.
#[test]
fn a_ready_host_is_ready_rootless() {
    if !rootless(Rootless::Full) {
        return;
    }
    let scratch = Scratch::new(&std::env::temp_dir(), "ready");
    let store = scratch.0.join("store");
    let report = doctor(|command| {
        command.env("SHIPYARD_IMAGE_STORE", &store);
    });
    assert_eq!(report.fails(), Vec::<&str>::new(), "{}", report.stdout);
    assert_eq!(report.code, Some(0), "{}", report.stdout);
    assert!(report.stdout.contains("(rootless)"), "{}", report.stdout);
    assert_eq!(report.line("user namespaces"), ("ok", "allowed (probed)"));
    assert_eq!(report.line("ID mapping").0, "ok");
    assert_eq!(report.line("network").0, "ok");
    let (level, text) = report.line("images");
    assert_eq!(level, "ok");
    assert!(text.contains("not made yet"), "{text}");
    assert!(!store.exists(), "doctor made the store");
    let left: Vec<_> = std::fs::read_dir(&scratch.0)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    if let Some(base) = delegated_cgroup() {
        // A warning where a controller isn't delegated (test-linux's
        // tester gets cpu, memory and pids only).
        assert_ne!(report.line("cgroups").0, "fail", "{}", report.stdout);
        // Its own: other tests' doctors may be probing meanwhile.
        let own = format!("doctor-{}-", report.pid);
        let left: Vec<_> = std::fs::read_dir(base.join("shipyard"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&own))
            .collect();
        assert!(left.is_empty(), "probe cgroups left: {left:?}");
    }
}

/// No pasta: a rootless run's default network can't be had, so doctor
/// fails, naming the package. (The ID helpers stay reachable.)
#[test]
fn rootless_without_pasta_isnt_ready() {
    if !rootless(Rootless::Full) {
        return;
    }
    let scratch = Scratch::new(&std::env::temp_dir(), "no-pasta");
    let bin = scratch.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for helper in ["newuidmap", "newgidmap"] {
        let found =
            shipyard_sys::path::find_program(helper, std::env::var_os("PATH").as_deref()).unwrap();
        std::os::unix::fs::symlink(found, bin.join(helper)).unwrap();
    }
    let report = doctor(|command| {
        command
            .env("PATH", &bin)
            .env("SHIPYARD_IMAGE_STORE", scratch.0.join("store"));
    });
    let (level, text) = report.line("network");
    assert_eq!(level, "fail");
    assert!(text.contains("install the passt package"), "{text}");
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert!(
        report
            .stdout
            .contains("containers can't run here as they are: 1 problem to fix"),
        "{}",
        report.stdout
    );
}

/// An image store on overlayfs (inside another container) can't hold a
/// container's writable layer: doctor finds out by trying, and points at
/// `SHIPYARD_IMAGE_STORE`. Where `/var/tmp` isn't on overlayfs (a real host),
/// there's nothing to show.
#[test]
fn a_store_on_overlayfs_isnt_ready() {
    if !rootless(Rootless::Full) {
        return;
    }
    let on_overlay = nix::sys::statfs::statfs("/var/tmp")
        .is_ok_and(|fs| fs.filesystem_type() == nix::sys::statfs::OVERLAYFS_SUPER_MAGIC);
    if !on_overlay {
        eprintln!("skipping: /var/tmp isn't on overlayfs here");
        return;
    }
    let scratch = Scratch::new(Path::new("/var/tmp"), "overlay");
    // Private, as a store must be: what's left to fail is the overlay.
    std::fs::set_permissions(
        &scratch.0,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let report = doctor(|command| {
        command.env("SHIPYARD_IMAGE_STORE", &scratch.0);
    });
    let (level, text) = report.line("images");
    assert_eq!(level, "fail", "{}", report.stdout);
    assert!(text.contains("in the store"), "{text}");
    assert!(text.contains("SHIPYARD_IMAGE_STORE"), "{text}");
    assert_eq!(report.code, Some(1), "{}", report.stdout);
}

/// A store a run would refuse (others can reach it) fails doctor too,
/// with the run's own message.
#[test]
fn a_store_a_run_refuses_isnt_ready() {
    use std::os::unix::fs::PermissionsExt as _;
    if !rootless(Rootless::Full) {
        return;
    }
    let scratch = Scratch::new(&std::env::temp_dir(), "open-store");
    std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(|command| {
        command.env("SHIPYARD_IMAGE_STORE", &scratch.0);
    });
    let fails = report.fails();
    assert!(
        fails
            .iter()
            .any(|line| line.contains("images") && line.contains(&*scratch.0.to_string_lossy())),
        "{}",
        report.stdout
    );
    assert_eq!(report.code, Some(1), "{}", report.stdout);
}

/// Where the store would go below someone else's directory (not sticky),
/// the overlay isn't probed: its owner could swap the scratch directory for
/// a link meanwhile. Said, as a limit; nothing is made there.
#[test]
fn someone_elses_directory_isnt_probed_in() {
    if !rootless(Rootless::Full) {
        return;
    }
    let store = Path::new("/usr/share/shipyard-doctor-test/store");
    let report = doctor(|command| {
        command.env("SHIPYARD_IMAGE_STORE", store);
    });
    let (level, text) = report.line("images");
    assert_eq!(level, "warn", "{}", report.stdout);
    assert!(text.contains("wasn't tried"), "{text}");
    assert!(text.contains("not to you"), "{text}");
}

/// Without `XDG_RUNTIME_DIR`, containers can't be kept (no `run -d`,
/// `create` or `start`), but a run still works, supervised by `run` itself:
/// a warning, not a failure.
#[test]
fn no_runtime_dir_only_limits() {
    if !rootless(Rootless::Full) {
        return;
    }
    let scratch = Scratch::new(&std::env::temp_dir(), "no-xdg");
    let report = doctor(|command| {
        command
            .env_remove("XDG_RUNTIME_DIR")
            .env("SHIPYARD_IMAGE_STORE", scratch.0.join("store"));
    });
    let (level, text) = report.line("runtime state");
    assert_eq!(level, "warn");
    assert!(text.contains("XDG_RUNTIME_DIR"), "{text}");
    assert!(report.fails().is_empty(), "{}", report.stdout);
}

/// `shipyard system stop` holds restart policies off until `shipyard system
/// recover`: doctor says so meanwhile, and not before or after (stage 9,
/// step 9.9a, L4). With no state root yet, a stop has nothing to do, and
/// succeeds. Rootless, on a runtime directory of its own (root's
/// state root is shared with other tests, whose restarts a stop would hold
/// off).
#[test]
fn a_stop_that_holds_restarts_off_is_said() {
    use std::os::unix::fs::PermissionsExt as _;
    if !rootless(Rootless::Full) {
        return;
    }
    let scratch = Scratch::new(&std::env::temp_dir(), "held");
    let xdg = scratch.0.join("xdg");
    for dir in [&xdg, &scratch.0.join("containers")] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let set_up = |command: &mut Command| {
        command
            .env("XDG_RUNTIME_DIR", &xdg)
            .env("SHIPYARD_CONTAINER_DIR", scratch.0.join("containers"))
            .env("SHIPYARD_IMAGE_STORE", scratch.0.join("store"));
    };
    let system = |verb: &str| {
        let mut command = SHIPYARD.command(&["system", verb]);
        set_up(&mut command);
        let out = command.output().unwrap();
        assert!(out.status.success(), "system {verb}: {out:?}");
    };
    let said = || {
        let report = doctor(set_up);
        report
            .stdout
            .lines()
            .any(|line| line.starts_with("warn ") && line.contains("restarts"))
    };
    // Before any container: no state root, nothing to stop or hold off
    // (the user unit's stop at a shutdown).
    system("stop");
    assert!(!said(), "a stop with no state root");
    // As a first container leaves it.
    let root = xdg.join("shipyard");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!said(), "before a stop");
    system("stop");
    assert!(said(), "after a stop");
    system("recover");
    assert!(!said(), "after the recovery");
}

/// Root: no user namespace to probe, a bridge network with `nf_tables`, and
/// the overlay probed in the store's place.
#[test]
#[ignore = "needs root"]
fn a_ready_host_is_ready_as_root() {
    shipyard_test_support::in_new_netns(|| {
        let scratch = Scratch::new(&std::env::temp_dir(), "root");
        let report = doctor(|command| {
            command.env("SHIPYARD_IMAGE_STORE", scratch.0.join("store"));
        });
        assert!(report.stdout.contains("(root)"), "{}", report.stdout);
        assert!(
            !report.stdout.contains("user namespaces"),
            "{}",
            report.stdout
        );
        assert_eq!(report.line("network").0, "ok", "{}", report.stdout);
        assert_eq!(report.line("images").0, "ok", "{}", report.stdout);
        assert_eq!(report.code, Some(0), "{}", report.stdout);
        let left: Vec<_> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        assert_eq!(report.line("firewall").0, "ok", "{}", report.stdout);
        // Docker's firewall: a warning (a run works; its containers can't
        // get out), with Docker's fix.
        for command in [
            "add table ip filter",
            "add chain ip filter FORWARD { type filter hook forward priority 0; policy drop; }",
            "add chain ip filter DOCKER-USER",
        ] {
            let made = std::process::Command::new("nft").arg(command).status();
            if !made.is_ok_and(|status| status.success()) {
                eprintln!("nft couldn't make Docker's chains; skipping the rest");
                return;
            }
        }
        // Bridged frames meet the chain as Docker makes them (`br_netfilter`,
        // where it's loaded: not on Fedora), and only then is a rule between
        // the containers asked for.
        let bridged = std::fs::write("/proc/sys/net/bridge/bridge-nf-call-iptables", "1").is_ok();
        let report = doctor(|command| {
            command.env("SHIPYARD_IMAGE_STORE", scratch.0.join("store"));
        });
        let (level, text) = report.line("firewall");
        assert_eq!(level, "warn", "{}", report.stdout);
        assert!(
            text.starts_with("Docker's firewall drops forwarded traffic"),
            "{text}"
        );
        assert_eq!(
            text.contains("sudo iptables -I DOCKER-USER -i shipyard0 -o shipyard0 -j ACCEPT"),
            bridged,
            "{text}"
        );
        assert!(
            text.contains("sudo iptables -I DOCKER-USER -o shipyard0 -m conntrack"),
            "{text}"
        );
        assert_eq!(report.code, Some(0), "a warning only: {}", report.stdout);
    });
}
