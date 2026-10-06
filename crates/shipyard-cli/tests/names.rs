//! End-to-end tests of names and identity (Linux): `--user` and
//! `--group-add` by name, resolved in the container's own `/etc/passwd`
//! and `/etc/group`; `HOME` from there; `--domainname`; `--sysctl`.
//!
//! Most need root, so they're `#[ignore]`d in a normal run (see
//! `isolation.rs`). Refused flags are checked as any user.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::collections::BTreeSet;

use serde_json::json;
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Run, Shipyard, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
alice:x:1000:1000:Alice:/home/alice:/bin/sh
";
const GROUP: &str = "\
root:x:0:
wheel:x:10:
audio:x:29:alice
staff:x:50:alice,bob
alice:x:1000:
";

/// A root filesystem with this file's `/etc/passwd` and `/etc/group`.
fn with_users() -> RootFs {
    let root = RootFs::minimal();
    std::fs::create_dir_all(root.host_path("/etc")).unwrap();
    std::fs::write(root.host_path("/etc/passwd"), PASSWD).unwrap();
    std::fs::write(root.host_path("/etc/group"), GROUP).unwrap();
    root
}

/// `shipyard run FLAGS… sh -c SCRIPT`.
fn shipyard(flags: &[&str], script: &str) -> Run {
    run(SHIPYARD.command(&[&["run"], flags, &["sh", "-c", script]].concat()))
}

/// Prints `HOME`, then `/proc/self/status`.
const IDENTITY: &str = "echo \"$HOME\"; cat /proc/self/status";

/// What `IDENTITY` printed: HOME, uid, gid and the supplementary groups.
fn identity(run: &Run) -> (String, u32, u32, BTreeSet<u32>) {
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let field = |name: &str| {
        run.stdout
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .unwrap_or_default()
            .split_whitespace()
            .map(|n| n.parse::<u32>().unwrap())
            .collect::<Vec<_>>()
    };
    let home = run.stdout.lines().next().unwrap_or_default().to_owned();
    let first = |name| field(name).first().copied().unwrap();
    (
        home,
        first("Uid"),
        first("Gid"),
        field("Groups").into_iter().collect(),
    )
}

// --- As any user --------------------------------------------------------------

#[test]
fn sysctls_that_would_change_the_host_are_refused() {
    for (sysctl, reason) in [
        ("net.ipv4.ip_forward=1", "network namespace"),
        ("vm.swappiness=10", "isn't namespaced"),
        ("kernel.hostname=x", "own settings"),
    ] {
        let run = shipyard(&["--sysctl", sysctl], "true");
        assert_eq!(run.code, Some(125), "{sysctl}: {}", run.stderr);
        assert!(run.stderr.contains(reason), "{sysctl}: {}", run.stderr);
    }
}

#[test]
fn malformed_names_and_sysctls_are_usage_errors() {
    for flags in [
        &["--user", "a b"][..],
        &["--group-add", "a:b"],
        &["--sysctl", "kernel.shmmax"],
        &["--domainname", "bad_name"],
    ] {
        let run = shipyard(flags, "true");
        assert_eq!(run.code, Some(125), "{flags:?}: {}", run.stderr);
    }
}

// --- As root: names -----------------------------------------------------------

/// A name gives the uid, the group and the home directory from the
/// container's `/etc/passwd`, and the groups listing the user from its
/// `/etc/group`, as Docker does.
#[test]
#[ignore = "needs root"]
fn a_user_name_resolves_in_the_containers_files() {
    let root = with_users();
    let (home, uid, gid, groups) = identity(&shipyard(
        &["--rootfs", root.arg(), "--user", "alice"],
        IDENTITY,
    ));
    assert_eq!((home.as_str(), uid, gid), ("/home/alice", 1000, 1000));
    assert_eq!(groups, BTreeSet::from([29, 50]));
}

#[test]
#[ignore = "needs root"]
fn a_group_and_more_groups_by_name() {
    let root = with_users();
    let (_, uid, gid, groups) = identity(&shipyard(
        &[
            "--rootfs",
            root.arg(),
            "--user",
            "alice:wheel",
            "--group-add",
            "root",
        ],
        IDENTITY,
    ));
    assert_eq!((uid, gid), (1000, 10));
    assert_eq!(groups, BTreeSet::from([0, 29, 50]));
}

/// A number is used as it is, with its entry's group and home.
#[test]
#[ignore = "needs root"]
fn a_user_number_with_an_entry() {
    let root = with_users();
    let (home, uid, gid, _) = identity(&shipyard(
        &["--rootfs", root.arg(), "--user", "1000"],
        IDENTITY,
    ));
    assert_eq!((home.as_str(), uid, gid), ("/home/alice", 1000, 1000));
    let (home, uid, gid, groups) = identity(&shipyard(
        &["--rootfs", root.arg(), "--user", "4242"],
        IDENTITY,
    ));
    assert_eq!((home.as_str(), uid, gid), ("/", 4242, 0));
    assert!(groups.is_empty());
}

#[test]
#[ignore = "needs root"]
fn the_users_own_home_wins() {
    let root = with_users();
    let (home, ..) = identity(&shipyard(
        &[
            "--rootfs",
            root.arg(),
            "--user",
            "alice",
            "-e",
            "HOME=/mine",
        ],
        IDENTITY,
    ));
    assert_eq!(home, "/mine");
    let (home, ..) = identity(&shipyard(&["--rootfs", root.arg()], IDENTITY));
    assert_eq!(home, "/root", "root, from its entry");
}

#[test]
#[ignore = "needs root"]
fn unknown_names_are_named() {
    let root = with_users();
    let run = shipyard(&["--rootfs", root.arg(), "--user", "mallory"], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("user \"mallory\" isn't in the container's /etc/passwd"),
        "{}",
        run.stderr
    );
    let run = shipyard(&["--rootfs", root.arg(), "--group-add", "nogroup"], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("group \"nogroup\""), "{}", run.stderr);
}

/// Without a root filesystem, the container's /etc is the host's.
#[test]
#[ignore = "needs root"]
fn without_a_root_filesystem_names_are_the_hosts() {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap();
    let Some(nobody) = passwd.lines().find_map(|line| {
        line.strip_prefix("nobody:x:")?
            .split(':')
            .next()?
            .parse::<u32>()
            .ok()
    }) else {
        return;
    };
    let (_, uid, ..) = identity(&shipyard(&["--user", "nobody"], IDENTITY));
    assert_eq!(uid, nobody);
}

// --- As root: domain name and sysctls -----------------------------------------

/// A sysctl the kernel refuses: the error says which, and doesn't suggest
/// `--no-isolation`, which can't be combined with `--sysctl`.
#[test]
#[ignore = "needs root"]
fn a_refused_sysctl_value_is_named() {
    let run = shipyard(&["--sysctl", "fs.mqueue.msg_max=0"], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("setting sysctl fs.mqueue.msg_max=0"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("--no-isolation"), "{}", run.stderr);
}

#[test]
#[ignore = "needs root"]
fn the_domain_name_is_the_containers_own() {
    let host = std::fs::read_to_string("/proc/sys/kernel/domainname").unwrap();
    let run = shipyard(
        &["--domainname", "example.org"],
        "cat /proc/sys/kernel/domainname",
    );
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "example.org\n"),
        "{}",
        run.stderr
    );
    assert_eq!(
        std::fs::read_to_string("/proc/sys/kernel/domainname").unwrap(),
        host
    );
}

/// An IPC sysctl, with and without a root filesystem (whose `/proc/sys`
/// becomes read-only after it's set): the container's value, not the
/// host's.
#[test]
#[ignore = "needs root"]
fn an_ipc_sysctl_is_the_containers_own() {
    let host = std::fs::read_to_string("/proc/sys/kernel/shmmax").unwrap();
    let root = RootFs::minimal();
    for flags in [&[][..], &["--rootfs", root.arg()]] {
        let flags = [flags, &["--sysctl", "kernel.shmmax=1048576"]].concat();
        let run = shipyard(&flags, "cat /proc/sys/kernel/shmmax");
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (Some(0), "1048576\n"),
            "{flags:?}: {}",
            run.stderr
        );
    }
    assert_eq!(
        std::fs::read_to_string("/proc/sys/kernel/shmmax").unwrap(),
        host
    );
}

/// A bundle's `domainname`, `linux.sysctl`, and `HOME` from its root
/// filesystem's `/etc/passwd`.
#[test]
#[ignore = "needs root"]
fn a_bundles_identity() {
    let bundle = Bundle::new();
    std::fs::create_dir_all(bundle.root.host_path("/etc")).unwrap();
    std::fs::write(bundle.root.host_path("/etc/passwd"), PASSWD).unwrap();
    let mut config = bundle.config();
    config["domainname"] = json!("bundle.example");
    config["linux"]["sysctl"] = json!({"kernel.msgmax": "4096"});
    config["process"]["user"] = json!({"uid": 1000, "gid": 1000});
    config["process"]["args"] = json!([
        "sh",
        "-c",
        "echo \"$HOME\"; cat /proc/sys/kernel/domainname /proc/sys/kernel/msgmax"
    ]);
    bundle.write(&config);
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "/home/alice\nbundle.example\n4096\n"),
        "{}",
        run.stderr
    );
}
