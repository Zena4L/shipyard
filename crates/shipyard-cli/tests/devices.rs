//! End-to-end tests of device access control (stage 5, step 7): the device
//! program on the container's cgroup, `--device`, `--device-cgroup-rule`,
//! and a bundle's `linux.resources.devices` and `linux.devices` (Linux).
//!
//! The program is loaded and attached as root, so most tests need it and
//! are `#[ignore]`d in a normal run; the rootless ones run as any user.
//! Device nodes are made in a scratch directory on the host (`mknod`, as
//! root) where a test needs one the defaults don't allow: loop devices
//! (`b 7:N`), whose open has no check of its own. (`/dev/kmsg`'s, say,
//! refuses without `CAP_SYSLOG`, with the same EPERM the program gives.)

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::json;
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Rootless, Run, Shipyard, rootless, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));
const RUNTIME: &str = env!("CARGO_BIN_EXE_shipyard-runtime");

/// What a refused `open` says.
const REFUSED: &str = "Operation not permitted";

/// `shipyard run FLAGS… sh -c SCRIPT`.
fn shipyard(flags: &[&str], script: &str) -> Run {
    let args = [&["run"], flags, &["sh", "-c", script]].concat();
    run(SHIPYARD.command(&args))
}

/// A scratch directory on the host, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-devices-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Makes the node `name` (`c` or `b`, `major`, `minor`) in it.
    fn node(&self, name: &str, kind: &str, major: u32, minor: u32) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let made = Command::new("mknod")
            .arg(&path)
            .arg(kind)
            .arg(major.to_string())
            .arg(minor.to_string())
            .status()
            .unwrap();
        assert!(made.success(), "mknod {}", path.display());
        path
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

// --- As root: shipyard run ----------------------------------------------------------

/// Docker's defaults: the usual devices open; any node may be made (root
/// has `CAP_MKNOD`), but one outside the list can't be opened, nor can the
/// host's own (without a root filesystem, `/dev` is the host's).
#[test]
#[ignore = "needs root"]
fn the_defaults_allow_only_the_usual_devices() {
    let scratch = Scratch::new();
    let loop_node = scratch.path().join("loop");
    let host = scratch.node("loop1", "b", 7, 1);
    let script = format!(
        "head -c 1 /dev/zero > /dev/null && echo zero; mknod {loop} b 7 0 && echo made; \
         head -c 0 {loop} 2>&1; head -c 0 {host} 2>&1",
        loop = arg(&loop_node),
        host = arg(&host),
    );
    let run = shipyard(&[], &script);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines[..2], ["zero", "made"], "{}", run.stderr);
    assert!(
        lines[2].contains(REFUSED),
        "the made node opened: {lines:?}"
    );
    assert!(
        lines[3].contains(REFUSED),
        "a host node outside the list opened: {lines:?}"
    );
}

/// `--device`: the host's device is made in the container's root, with its
/// numbers and mode, for the access given: `:r` reads, but can't write.
/// Without a root filesystem, it's used where it is.
#[test]
#[ignore = "needs root"]
fn a_device_given_can_be_used_as_far_as_allowed() {
    let scratch = Scratch::new();
    let host = scratch.node("loop0", "b", 7, 0);
    std::fs::set_permissions(&host, std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
    let root = RootFs::minimal();
    let device = format!("{}:/dev/mine:r", arg(&host));
    let args = [
        "run",
        "--rootfs",
        root.arg(),
        "--device",
        &device,
        "sh",
        "-c",
        "stat -c '%F %t:%T %a' /dev/mine; head -c 0 /dev/mine && echo read; \
         (: > /dev/mine) 2> /dev/null || echo no write",
    ];
    let run = run(SHIPYARD.command(&args));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines[0], "block special file 7:0 640");
    assert_eq!(lines[1], "read");
    assert!(run.stdout.contains("no write"), "{}", run.stdout);
    // Without a root filesystem: the rule only.
    let script = format!("head -c 0 {} && echo read", arg(&host));
    let run = shipyard(&["--device", arg(&host)], &script);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "read\n"),
        "{}",
        run.stderr
    );
    let run = shipyard(&["--device", &format!("{}:/dev/x", arg(&host))], "true");
    assert_eq!(run.code, Some(125));
    assert!(run.stderr.contains("needs --rootfs"), "{}", run.stderr);
}

/// `--device DIR` gives every device below it, at the same place below the
/// container path.
#[test]
#[ignore = "needs root"]
fn a_directory_gives_its_devices() {
    let scratch = Scratch::new();
    scratch.node("set/a", "b", 7, 0);
    scratch.node("set/sub/b", "b", 7, 1);
    let root = RootFs::minimal();
    let device = format!("{}:/dev/set", arg(&scratch.path().join("set")));
    let args = [
        "run",
        "--rootfs",
        root.arg(),
        "--device",
        &device,
        "sh",
        "-c",
        "head -c 0 /dev/set/a && head -c 0 /dev/set/sub/b && echo both",
    ];
    let run = run(SHIPYARD.command(&args));
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "both\n"),
        "{}",
        run.stderr
    );
}

/// `--device-cgroup-rule` lets a kind of device be used: a node made inside
/// opens.
#[test]
#[ignore = "needs root"]
fn a_rule_lets_made_nodes_open() {
    let scratch = Scratch::new();
    let node = scratch.path().join("loop");
    let script = format!(
        "mknod {node} b 7 0 && head -c 0 {node} && echo opened",
        node = arg(&node)
    );
    let run = shipyard(&["--device-cgroup-rule", "b 7:* rw"], &script);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "opened\n"),
        "{}",
        run.stderr
    );
}

/// Starts `binary ARGS…` where no cgroup tree can be found: a mount
/// namespace of its own with an empty tmpfs on `/sys/fs/cgroup`, as on a
/// cgroup v1 host (to Shipyard).
fn without_cgroups(binary: &str, args: &[&str]) -> Command {
    let mut command = Command::new("unshare");
    command
        .args(["--mount", "--propagation", "private", "sh", "-c"])
        .arg("mount -t tmpfs tmpfs /sys/fs/cgroup && exec \"$@\"")
        .args(["sh", binary])
        .args(args);
    command
}

/// What a container with no device program is told it goes without.
const WITHOUT_MKNOD: &str = "it goes without CAP_MKNOD, so it can't make device nodes";

/// Real root with no cgroup has no device program to keep root inside
/// from the host's disks: the container goes without `CAP_MKNOD`, so it
/// can't make a node, while the usual devices, and one given, still work.
/// With a cgroup (the control), it can make one (which the program then
/// keeps it from opening).
#[test]
#[ignore = "needs root"]
fn without_a_device_program_root_inside_cant_make_nodes() {
    let root = RootFs::minimal();
    let script = "mknod /dev/disk b 7 0 2> /dev/null && echo made; \
                  head -c 1 /dev/zero > /dev/null && echo zero; \
                  echo x > /dev/null && echo null; echo x > /dev/given && echo given; \
                  while read -r key value; do [ \"$key\" != CapBnd: ] || echo \"$value\"; \
                  done < /proc/self/status";
    let args = [
        "run",
        "--rootfs",
        root.arg(),
        "--device",
        "/dev/null:/dev/given",
        "sh",
        "-c",
        script,
    ];
    let bounding = |run: &Run| {
        let line = run.stdout.lines().last().unwrap_or_default().to_owned();
        let bits = line.split_whitespace().last().unwrap_or_default();
        u64::from_str_radix(bits, 16).unwrap_or_else(|_| panic!("{line}"))
    };
    let mknod = 1_u64 << 27;
    let guarded = run(SHIPYARD.command(&args));
    assert_eq!(guarded.code, Some(0), "{}", guarded.stderr);
    assert!(
        guarded.stdout.starts_with("made\nzero\nnull\ngiven\n"),
        "{}",
        guarded.stdout
    );
    assert_ne!(bounding(&guarded) & mknod, 0);
    assert!(
        !guarded.stderr.contains(WITHOUT_MKNOD),
        "{}",
        guarded.stderr
    );
    let unguarded = run(without_cgroups(env!("CARGO_BIN_EXE_shipyard"), &args));
    assert_eq!(unguarded.code, Some(0), "{}", unguarded.stderr);
    assert!(
        unguarded.stdout.starts_with("zero\nnull\ngiven\n"),
        "{}: {}",
        unguarded.stdout,
        unguarded.stderr
    );
    assert_eq!(bounding(&unguarded) & mknod, 0);
    assert!(
        unguarded.stderr.contains(WITHOUT_MKNOD),
        "{}",
        unguarded.stderr
    );
    // Without a root filesystem, `/dev` is the host's: the warning says
    // what that leaves open.
    let host_dev = run(without_cgroups(
        env!("CARGO_BIN_EXE_shipyard"),
        &["run", "true"],
    ));
    assert_eq!(host_dev.code, Some(0), "{}", host_dev.stderr);
    assert!(
        host_dev
            .stderr
            .contains("it sees the host's /dev (no root filesystem), whose devices all open"),
        "{}",
        host_dev.stderr
    );
}

// --- As root: bundles -------------------------------------------------------------

/// A bundle's config with `script`, its device rules and nodes.
fn device_bundle(bundle: &Bundle, script: &str, linux: &serde_json::Value) -> serde_json::Value {
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    for (key, value) in linux.as_object().unwrap() {
        config["linux"][key] = value.clone();
    }
    config
}

/// A bundle's rules exactly, then runc's own list, as runc builds them:
/// deny-all leaves `/dev/null` working, and a node from `linux.devices` is
/// made with its mode and owner, and opens as its rule allows.
#[test]
#[ignore = "needs root"]
fn a_bundles_rules_and_nodes() {
    let bundle = Bundle::new();
    let config = device_bundle(
        &bundle,
        "head -c 0 /dev/null && echo null; stat -c '%t:%T %a %u %g' /dev/mine; \
         head -c 0 /dev/mine && echo mine",
        &json!({
            "resources": {"devices": [
                {"allow": false, "access": "rwm"},
                {"allow": true, "type": "b", "major": 7, "minor": 0, "access": "rw"}
            ]},
            "devices": [{"path": "/dev/mine", "type": "b", "major": 7, "minor": 0,
                         "fileMode": 420, "uid": 1, "gid": 2}]
        }),
    );
    bundle.write(&config);
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "null\n7:0 644 1 2\nmine\n");
    // The same with shipyard-runtime: the container keeps `create`'s output.
    let state = Scratch::new();
    assert!(
        runtime(state.path(), &["create", "--bundle", bundle.arg(), "dev"]),
        "{}",
        output(state.path(), "create.err")
    );
    assert!(runtime(state.path(), &["start", "dev"]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while output(state.path(), "create.out") != "null\n7:0 644 1 2\nmine\n" {
        assert!(
            std::time::Instant::now() < deadline,
            "{:?}: {}",
            output(state.path(), "create.out"),
            output(state.path(), "create.err")
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = runtime(state.path(), &["delete", "--force", "dev"]);
}

/// A bundle given `CAP_MKNOD` goes without it too where no device program
/// can guard it (no cgroup), run by `shipyard run --bundle` or by
/// `shipyard-runtime create`, which says so. With a cgroup (the control),
/// it keeps it.
#[test]
#[ignore = "needs root"]
fn without_a_device_program_a_bundle_loses_cap_mknod() {
    let bundle = Bundle::new();
    let mut config = device_bundle(
        &bundle,
        "mknod /tmp/a b 7 0 2> /dev/null && echo made; head -c 0 /dev/null && echo null",
        &json!({}),
    );
    for set in ["bounding", "effective", "permitted"] {
        config["process"]["capabilities"][set] = json!(["CAP_MKNOD"]);
    }
    bundle.write(&config);
    let args = ["run", "--bundle", bundle.arg()];
    let guarded = run(SHIPYARD.command(&args));
    assert_eq!(guarded.stdout, "made\nnull\n", "{}", guarded.stderr);
    let unguarded = run(without_cgroups(env!("CARGO_BIN_EXE_shipyard"), &args));
    assert_eq!(unguarded.stdout, "null\n", "{}", unguarded.stderr);
    assert!(
        unguarded.stderr.contains(WITHOUT_MKNOD),
        "{}",
        unguarded.stderr
    );
    // The runtime: `create` warns, and the container can't make the node.
    let state = Scratch::new();
    let root = state.path().join("root");
    let created = without_cgroups(RUNTIME, &["--root", arg(&root)])
        .args(["create", "--bundle", bundle.arg(), "nomknod"])
        .stdout(std::fs::File::create(state.path().join("create.out")).unwrap())
        .stderr(std::fs::File::create(state.path().join("create.err")).unwrap())
        .status()
        .unwrap();
    assert!(created.success(), "{}", output(state.path(), "create.err"));
    assert!(
        output(state.path(), "create.err").contains(WITHOUT_MKNOD),
        "{}",
        output(state.path(), "create.err")
    );
    assert!(runtime(state.path(), &["start", "nomknod"]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while output(state.path(), "create.out") != "null\n" {
        assert!(
            std::time::Instant::now() < deadline,
            "{:?}: {}",
            output(state.path(), "create.out"),
            output(state.path(), "create.err")
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = runtime(state.path(), &["delete", "--force", "nomknod"]);
}

/// `shipyard-runtime --root STATE/root ARGS…`, its output in `STATE/CMD.out`
/// and `.err` (`CMD` being `ARGS`' first: for `create`, the container's
/// output too). Whether it succeeded.
fn runtime(state: &Path, args: &[&str]) -> bool {
    let file = |extension: &str| {
        std::fs::File::create(state.join(format!("{}.{extension}", args[0]))).unwrap()
    };
    Command::new(RUNTIME)
        .arg("--root")
        .arg(state.join("root"))
        .args(args)
        .stdout(file("out"))
        .stderr(file("err"))
        .status()
        .unwrap()
        .success()
}

/// What `STATE/NAME` holds (nothing if it isn't there).
fn output(state: &Path, name: &str) -> String {
    std::fs::read_to_string(state.join(name)).unwrap_or_default()
}

/// A bundle that allows a device class explicitly: nodes of it made inside
/// open, others don't.
#[test]
#[ignore = "needs root"]
fn a_bundles_allow_rule_is_enforced() {
    let bundle = Bundle::new();
    let mut config = device_bundle(
        &bundle,
        "mknod /tmp/a b 7 0 && mknod /tmp/b b 7 1 && head -c 0 /tmp/a && echo a; \
         head -c 0 /tmp/b 2>&1",
        &json!({"resources": {"devices": [
            {"allow": false, "access": "rwm"},
            {"allow": true, "type": "b", "access": "m"},
            {"allow": true, "type": "b", "major": 7, "minor": 0, "access": "rw"}
        ]}}),
    );
    // `mknod` needs the capability; the test bundle's set doesn't have it.
    config["process"]["capabilities"]["bounding"] = json!(["CAP_MKNOD"]);
    config["process"]["capabilities"]["effective"] = json!(["CAP_MKNOD"]);
    config["process"]["capabilities"]["permitted"] = json!(["CAP_MKNOD"]);
    bundle.write(&config);
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.first(), Some(&"a"), "{}", run.stderr);
    assert!(lines[1].contains(REFUSED), "{lines:?}");
}

// --- Rootless ---------------------------------------------------------------------

/// Rootless, a device rule is enforced if this kernel lets a user load the
/// program, and goes without, saying so, if it doesn't; the run goes on
/// either way. A node given is the host's, bound in.
#[test]
fn rootless_devices() {
    if !rootless(Rootless::Single) {
        return;
    }
    let ruled = shipyard(&["--device-cgroup-rule", "c 1:11 r"], "echo ran");
    assert_eq!(
        (ruled.code, ruled.stdout.as_str()),
        (Some(0), "ran\n"),
        "{}",
        ruled.stderr
    );
    let warned = ruled.stderr.contains("device rules can't be enforced");
    if warned {
        // The command's own warning line, not tracing's format (other
        // warnings may come too: a host whose resolver is on its loopback
        // gets one about DNS).
        assert!(
            ruled
                .stderr
                .lines()
                .any(|line| line.starts_with("shipyard: warning: the device rules")),
            "{}",
            ruled.stderr
        );
    }
    eprintln!(
        "this kernel: rootless device programs {}",
        if warned { "refused" } else { "allowed" }
    );
    let root = RootFs::minimal();
    let args = [
        "run",
        "--rootfs",
        root.arg(),
        "--device",
        "/dev/null:/dev/mine",
        "sh",
        "-c",
        "stat -c '%t:%T' /dev/mine; echo x > /dev/mine && echo wrote",
    ];
    let run = run(SHIPYARD.command(&args));
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "1:3\nwrote\n"),
        "{}",
        run.stderr
    );
}

/// Rootless, a bundle's node is the host's at the same path, bound in: one
/// the host doesn't have is named.
#[test]
fn rootless_bundle_nodes_must_be_on_the_host() {
    if !rootless(Rootless::Single) {
        return;
    }
    let bundle = Bundle::new();
    let config = device_bundle(
        &bundle,
        "true",
        &json!({"devices": [{"path": "/dev/no-such-device", "type": "c", "major": 1, "minor": 3}]}),
    );
    bundle.write(&config);
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    assert_eq!(run.code, Some(125), "{}", run.stdout);
    assert!(
        run.stderr.contains("/dev/no-such-device")
            && run.stderr.contains("no such node on the host"),
        "{}",
        run.stderr
    );
}
