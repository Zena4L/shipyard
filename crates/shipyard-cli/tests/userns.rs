//! End-to-end tests of user namespaces (stage 4, step 4): `--uidmap` and
//! `--gidmap` for a root caller (Linux).
//!
//! What the tests look at is the host's view: a file the command creates is
//! owned by the mapped host IDs, and "root" inside can't read what host root
//! owns. Running a container needs root, so those tests are `#[ignore]`d in
//! a normal run (see `isolation.rs`); the usage errors run as any user.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::PathBuf;

use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Run, Shipyard, killed_by, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// Root inside is host user 100000, and so on for 65536 IDs.
const MAP: &str = "0:100000:65536";

/// `shipyard run ARGS… sh -c SCRIPT`.
fn shipyard_run(args: &[&str], script: &str) -> Run {
    run(SHIPYARD.command(&[&["run"], args, &["sh", "-c", script]].concat()))
}

/// The same, which must succeed: its output.
fn ok(args: &[&str], script: &str) -> String {
    let run = shipyard_run(args, script);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    run.stdout
}

/// A world-writable host directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("shipyard-userns-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- As any user: usage errors --------------------------------------------------

#[test]
fn id_map_flags_are_checked() {
    for (args, expected) in [
        (&["--uidmap", "0:100000"][..], "isn't CONTAINER:HOST:SIZE"),
        (&["--uidmap", "0:x:1"], "isn't CONTAINER:HOST:SIZE"),
        (&["--gidmap", MAP], "--uidmap"),
        (&["--no-isolation", "--uidmap", MAP], "cannot be used with"),
    ] {
        let run = shipyard_run(args, "true");
        assert_eq!(run.code, Some(125), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{args:?}: {}", run.stderr);
    }
}

/// What the kernel would refuse is refused first (as root: without, the
/// maps are chosen automatically, and `--uidmap` is refused outright).
#[test]
#[ignore = "needs root"]
fn maps_the_kernel_would_refuse_are_refused() {
    for (args, expected) in [
        (&["--uidmap", "1:100000:65536"][..], "must map ID 0"),
        (
            &["--uidmap", "0:100000:10", "--uidmap", "5:200000:10"],
            "overlap inside the container",
        ),
        (&["--uidmap", "0:4294967290:10"], "past the last ID"),
    ] {
        let run = shipyard_run(args, "true");
        assert_eq!(run.code, Some(125), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{args:?}: {}", run.stderr);
    }
}

// --- As root ------------------------------------------------------------------

/// The maps are the kernel's, and the command is root inside.
#[test]
#[ignore = "needs root"]
fn the_maps_are_in_place_and_root_inside_is_uid_0() {
    let out = ok(
        &["--uidmap", MAP],
        "cat /proc/self/uid_map /proc/self/gid_map; grep -E '^(Uid|Gid):' /proc/self/status",
    );
    let words: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        &words[..6],
        ["0", "100000", "65536", "0", "100000", "65536"],
        "{out}"
    );
    assert!(
        out.contains("Uid:\t0\t0\t0\t0") && out.contains("Gid:\t0\t0\t0\t0"),
        "{out}"
    );
}

/// What the command creates on the host belongs to the mapped host IDs:
/// root inside is user 100000 outside, user 1000 is 101000.
#[test]
#[ignore = "needs root"]
fn root_inside_is_an_ordinary_user_outside() {
    let scratch = Scratch::new("owned");
    let (as_root, as_1000) = (scratch.file("root"), scratch.file("user"));
    ok(&["--uidmap", MAP], &format!("touch {as_root}"));
    ok(
        &["--uidmap", MAP, "--user", "1000:1000"],
        &format!("touch {as_1000}"),
    );
    let owner = |path: &str| {
        let meta = std::fs::metadata(path).unwrap();
        (meta.uid(), meta.gid())
    };
    assert_eq!(owner(&as_root), (100_000, 100_000));
    assert_eq!(owner(&as_1000), (101_000, 101_000));
}

/// Root inside has `CAP_DAC_OVERRIDE`, but only over files whose owner is
/// mapped: the host's `/etc/shadow` (host root's) stays closed.
#[test]
#[ignore = "needs root"]
fn root_inside_cant_read_what_host_root_owns() {
    let run = shipyard_run(&["--uidmap", MAP], "cat /etc/shadow");
    assert_ne!(run.code, Some(0), "{}", run.stdout);
    assert!(run.stderr.contains("Permission denied"), "{}", run.stderr);
    // Rootful, the same command reads it.
    assert_eq!(
        shipyard_run(&[], "cat /etc/shadow >/dev/null").code,
        Some(0)
    );
}

#[test]
#[ignore = "needs root"]
fn a_separate_gidmap() {
    let out = ok(
        &["--uidmap", MAP, "--gidmap", "0:200000:65536"],
        "cat /proc/self/uid_map /proc/self/gid_map",
    );
    let words: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        &words[..6],
        ["0", "100000", "65536", "0", "200000", "65536"],
        "{out}"
    );
}

/// An ID outside the maps doesn't exist inside: a clear error, not a bare
/// EINVAL from the switch of user.
#[test]
#[ignore = "needs root"]
fn unmapped_ids_are_refused_by_name() {
    for (args, expected) in [
        (&["--user", "70000"][..], "user 70000 isn't mapped"),
        (&["--user", "0:70000"], "group 70000 isn't mapped"),
        (
            &["--group-add", "70000"],
            "supplementary group 70000 isn't mapped",
        ),
    ] {
        let run = shipyard_run(&[&["--uidmap", MAP], args].concat(), "true");
        assert_eq!(run.code, Some(125), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{args:?}: {}", run.stderr);
    }
}

/// With a root filesystem of its own (given to the mapped range): `/dev`'s
/// nodes are the host's, bound; `/sys` is the host's, read-only; `/dev/pts`,
/// the hostname and an IPC sysctl work as rootful; a read-only root holds.
#[test]
#[ignore = "needs root"]
fn a_root_filesystem_in_a_user_namespace() {
    let root = RootFs::minimal();
    root.chown_all(100_000, 100_000);
    let args = [
        "--uidmap",
        MAP,
        "--rootfs",
        root.arg(),
        "--hostname",
        "box",
        "--sysctl",
        "kernel.shmmax=1000000",
    ];
    let out = ok(
        &args,
        "echo x > /dev/null && echo null-ok; head -c 3 /dev/urandom | wc -c; \
         head -c 2 /dev/zero | wc -c; touch /sys/x 2>&1; ls /dev/pts; \
         cat /proc/sys/kernel/hostname /proc/sys/kernel/shmmax; \
         touch /tmp/w && echo writable",
    );
    for expected in [
        "null-ok",
        "3\n2\n",
        "Read-only file system",
        "ptmx",
        "box\n1000000",
        "writable",
    ] {
        assert!(out.contains(expected), "{expected}: {out}");
    }
    let read_only = shipyard_run(&[&args[..], &["--read-only"]].concat(), "touch /x");
    assert_ne!(read_only.code, Some(0));
    assert!(
        read_only.stderr.contains("Read-only file system"),
        "{}",
        read_only.stderr
    );
}

/// `devpts` takes `gid=5` (the `tty` group) only if group 5 is mapped: with
/// a map of group 0 alone, the mount would be refused (EINVAL) and the
/// container wouldn't start.
#[test]
#[ignore = "needs root"]
fn dev_pts_without_the_tty_group_mapped() {
    let root = RootFs::minimal();
    root.chown_all(100_000, 100_000);
    let out = ok(
        &[
            "--uidmap",
            MAP,
            "--gidmap",
            "0:100000:1",
            "--rootfs",
            root.arg(),
        ],
        "ls /dev/pts",
    );
    assert!(out.contains("ptmx"), "{out}");
}

/// Capabilities and the seccomp filter apply as without a user namespace,
/// including the filter's early install (`no-new-privileges=false`), which
/// needs `CAP_SYS_ADMIN` in the namespace.
#[test]
#[ignore = "needs root"]
fn confinement_holds_in_a_user_namespace() {
    let status = ok(&["--uidmap", MAP], "cat /proc/self/status");
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .unwrap()
            .trim()
            .to_owned()
    };
    assert_eq!(
        u64::from_str_radix(&field("CapEff"), 16).unwrap(),
        shipyard_runtime::security::CapabilitySet::DOCKER_DEFAULT.bits()
    );
    assert_eq!(field("NoNewPrivs"), "1");
    if !shipyard_test_support::under_rosetta() {
        assert_eq!(field("Seccomp"), "2");
        let early = ok(
            &["--uidmap", MAP, "--security-opt", "no-new-privileges=false"],
            "grep -E '^(Seccomp|NoNewPrivs):' /proc/self/status",
        );
        assert!(
            early.contains("Seccomp:\t2") && early.contains("NoNewPrivs:\t0"),
            "{early}"
        );
    }
}

/// Exit codes and signals come through the init as without one.
#[test]
#[ignore = "needs root"]
fn exit_codes_and_signals_come_through() {
    assert_eq!(shipyard_run(&["--uidmap", MAP], "exit 7").code, Some(7));
    assert_eq!(
        shipyard_run(&["--uidmap", MAP], "kill -TERM $$").code,
        Some(killed_by(libc::SIGTERM))
    );
}

// --- As root: what goes wrong, said clearly --------------------------------------

/// A map including host root works, but warns: root inside is then root on
/// the host's files.
#[test]
#[ignore = "needs root"]
fn mapping_host_root_warns() {
    let run = shipyard_run(&["--uidmap", "0:0:65536"], "true");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("map host ID 0"), "{}", run.stderr);
    let quiet = shipyard_run(&["--uidmap", MAP], "true");
    assert!(!quiet.stderr.contains("warning"), "{}", quiet.stderr);
}

/// What the host mounted inside `/sys` (its cgroup tree, among others)
/// stays out: covered, as a fresh `sysfs` has nothing there. At
/// `/sys/fs/cgroup`, the container's own cgroup goes on top of the cover:
/// not the host's tree, which holds `shipyard/`.
#[test]
#[ignore = "needs root"]
fn the_hosts_mounts_in_sys_stay_out() {
    let root = RootFs::minimal();
    root.chown_all(100_000, 100_000);
    let out = ok(
        &["--uidmap", MAP, "--rootfs", root.arg()],
        "ls -d /sys/fs/cgroup/shipyard 2>/dev/null | wc -l; ls /sys",
    );
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("0"), "{out}");
    assert!(out.contains("kernel"), "the host's sysfs is there: {out}");
}

/// A root filesystem the mapped root can't reach: the error says who was
/// refused.
#[test]
#[ignore = "needs root"]
fn a_permission_error_names_the_mapped_user() {
    let parent = Scratch::new("private");
    std::fs::set_permissions(&parent.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = RootFs::minimal_in(&parent.0);
    root.chown_all(100_000, 100_000);
    let run = shipyard_run(&["--uidmap", MAP, "--rootfs", root.arg()], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("Permission denied")
            && run
                .stderr
                .contains("the container's root is host user 100000"),
        "{}",
        run.stderr
    );
}

/// The maps go to `/proc/PID`: with another PID namespace's `/proc` (after
/// `unshare -p -f` without `--mount-proc`), that would be another process.
/// Refused before anything starts.
#[test]
#[ignore = "needs root"]
fn a_foreign_proc_is_refused() {
    let mut command = std::process::Command::new("unshare");
    command.args([
        "-p",
        "-f",
        env!("CARGO_BIN_EXE_shipyard"),
        "run",
        "--uidmap",
        MAP,
        "true",
    ]);
    let run = run(command);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("another PID namespace"),
        "{}",
        run.stderr
    );
}

/// As root, a bundle's own user namespace and mappings: the maps inside, and
/// the host's view of what root inside creates.
#[test]
#[ignore = "needs root"]
fn a_bundles_user_namespace_as_root() {
    let bundle = shipyard_test_support::bundle::Bundle::new();
    // Root inside is host 100000: its root filesystem must be that user's.
    bundle.root.chown_all(100_000, 100_000);
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!(["sh", "-c", "cat /proc/self/uid_map"]);
    config["linux"]["namespaces"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"type": "user"}));
    let mappings = serde_json::json!([{"containerID": 0, "hostID": 100_000, "size": 65536}]);
    config["linux"]["uidMappings"] = mappings.clone();
    config["linux"]["gidMappings"] = mappings;
    bundle.write(&config);
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let words: Vec<&str> = run.stdout.split_whitespace().collect();
    assert_eq!(words, ["0", "100000", "65536"], "{}", run.stdout);
}

/// Root's container in a user namespace can have its OOM score lowered:
/// shipyard sets it with its own privileges, which the container's root
/// lacks (`CAP_SYS_RESOURCE` in the host's user namespace).
#[test]
#[ignore = "needs root"]
fn the_oom_score_is_lowered_from_outside() {
    let root = RootFs::minimal();
    root.chown_all(100_000, 100_000);
    let out = ok(
        &[
            "--uidmap",
            MAP,
            "--rootfs",
            root.arg(),
            "--oom-score-adj",
            "-500",
        ],
        "cat /proc/self/oom_score_adj",
    );
    assert_eq!(out, "-500\n");
}
