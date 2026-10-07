//! End-to-end tests of `shipyard run --bundle`: OCI bundles (Linux).
//!
//! Each test writes a `config.json` around a tiny root filesystem
//! (`shipyard_test_support::bundle`). Running one needs root, so those
//! tests are `#[ignore]`d in a normal run (see `isolation.rs`). What's
//! decided before anything runs (refused fields, errors in the config) is
//! checked as any user.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::{Run, Shipyard, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// Writes `config` to `bundle` and runs it.
fn run_bundle(bundle: &Bundle, config: &Value) -> Run {
    bundle.write(config);
    run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]))
}

/// `bundle`'s config, running `sh -c SCRIPT`.
fn running(bundle: &Bundle, script: &str) -> Value {
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    config
}

/// Runs `script` in `bundle` with `change` made to its config; it must
/// succeed.
fn ok(bundle: &Bundle, script: &str, change: impl FnOnce(&mut Value)) -> String {
    let mut config = running(bundle, script);
    change(&mut config);
    let run = run_bundle(bundle, &config);
    assert_eq!(run.code, Some(0), "{script}: {}", run.stderr);
    run.stdout
}

/// The value of `field` in `/proc/self/status`-style output.
fn status_field<'a>(status: &'a str, field: &str) -> &'a str {
    status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .map_or("", str::trim)
}

/// A host directory with a file in it, removed on drop.
struct HostDir {
    path: PathBuf,
}

impl HostDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("shipyard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("file"), "from the host\n").unwrap();
        Self { path }
    }

    fn arg(&self) -> &str {
        self.path.to_str().unwrap()
    }
}

impl Drop for HostDir {
    fn drop(&mut self) {
        // A test may have mounted on `sub`: unmounted first, or the removal
        // would fail on it.
        let _ = Command::new("umount")
            .arg("-l")
            .arg(self.path.join("sub"))
            .status();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// --- As any user: decided before anything runs ------------------------------

#[test]
fn a_bundle_with_a_command_is_a_usage_error() {
    let bundle = Bundle::new();
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg(), "true"]));
    assert_eq!(run.code, Some(125), "{}", run.stderr);
}

#[test]
fn a_missing_config_is_named() {
    let bundle = Bundle::new();
    std::fs::remove_file(bundle.path().join("config.json")).unwrap();
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    assert_eq!(run.code, Some(125));
    assert!(run.stderr.contains("config.json"), "{}", run.stderr);
}

#[test]
fn bad_json_says_where() {
    let bundle = Bundle::new();
    std::fs::write(
        bundle.path().join("config.json"),
        "{\n  \"ociVersion\": \n}",
    )
    .unwrap();
    let run = run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]));
    assert_eq!(run.code, Some(125));
    assert!(run.stderr.contains("line 3"), "{}", run.stderr);
}

/// What `runc spec` writes asks for what Shipyard can't give yet (a
/// terminal, a network namespace): the bundle is refused, naming each field,
/// before anything starts. Its device rule is applied.
#[test]
fn a_runc_spec_config_is_refused_with_its_reasons() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["terminal"] = json!(true);
    config["process"]["capabilities"] = json!({"bounding": ["CAP_KILL"]});
    config["process"]["noNewPrivileges"] = json!(true);
    config["linux"]["namespaces"] =
        json!([{"type": "pid"}, {"type": "network"}, {"type": "mount"}]);
    config["linux"]["resources"] = json!({"devices": [{"allow": false, "access": "rwm"}]});
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    for field in ["process.terminal", "linux.namespaces (network)"] {
        assert!(run.stderr.contains(field), "{field}: {}", run.stderr);
    }
    // Applied since stage 4 (and the device rules since stage 5), so not
    // among the reasons any more.
    for applied in [
        "process.capabilities",
        "process.noNewPrivileges",
        "linux.resources",
    ] {
        assert!(!run.stderr.contains(applied), "{applied}: {}", run.stderr);
    }
    assert!(!run.stderr.contains("hint"), "{}", run.stderr);
}

#[test]
fn mounts_on_the_kernels_interfaces_are_refused() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["mounts"] = json!([{"destination": "/proc/sys", "type": "tmpfs"}]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125));
    assert!(run.stderr.contains("/proc/sys"), "{}", run.stderr);
}

#[test]
fn a_missing_bind_source_is_named() {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["mounts"] =
        json!([{"destination": "/data", "type": "bind", "source": "/no/such/source"}]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125));
    assert!(run.stderr.contains("/no/such/source"), "{}", run.stderr);
}

// --- As root: running bundles -------------------------------------------------

/// The process's arguments, environment, working directory, user, umask,
/// groups, limits and hostname are the config's.
#[test]
#[ignore = "needs root"]
fn the_process_is_the_configs() {
    let bundle = Bundle::new();
    let out = ok(
        &bundle,
        "echo \"$A\"; pwd; cat /proc/self/status; cat /proc/self/limits; cat /proc/sys/kernel/hostname",
        |config| {
            config["hostname"] = json!("bundled");
            config["process"]["env"] = json!(["PATH=/bin", "A=from the config"]);
            config["process"]["cwd"] = json!("/work");
            config["process"]["user"] =
                json!({"uid": 1000, "gid": 1000, "umask": 23, "additionalGids": [5]});
            config["process"]["rlimits"] = json!([
                {"type": "RLIMIT_NOFILE", "soft": 512, "hard": 1024},
                {"type": "RLIMIT_CORE", "soft": u64::MAX, "hard": u64::MAX}
            ]);
        },
    );
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("from the config"));
    assert_eq!(lines.next(), Some("/work"));
    assert!(status_field(&out, "Uid").starts_with("1000"), "{out}");
    assert!(status_field(&out, "Gid").starts_with("1000"), "{out}");
    assert_eq!(status_field(&out, "Groups"), "5");
    assert_eq!(status_field(&out, "Umask"), "0027");
    let limit = |name: &str| {
        out.lines()
            .find(|line| line.starts_with(name))
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    assert!(limit("Max open files").contains("512 1024"), "{out}");
    assert!(
        limit("Max core file size").contains("unlimited unlimited"),
        "{out}"
    );
    assert_eq!(out.lines().last(), Some("bundled"));
}

/// The config's masked paths are the ones masked: none of the defaults.
#[test]
#[ignore = "needs root"]
fn the_configs_masked_paths_replace_the_defaults() {
    let bundle = Bundle::new();
    if !Path::new("/proc/timer_list").exists() {
        return;
    }
    let out = ok(&bundle, "wc -c < /proc/timer_list", |config| {
        config["linux"]["maskedPaths"] = json!(["/proc/keys"]);
    });
    assert_ne!(out.trim(), "0", "not masked when the config doesn't say so");
}

#[test]
#[ignore = "needs root"]
fn a_read_only_root() {
    let bundle = Bundle::new();
    let mut config = running(&bundle, "touch /x");
    config["root"]["readonly"] = json!(true);
    let run = run_bundle(&bundle, &config);
    assert_ne!(run.code, Some(0));
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
}

/// A read-only bind of a host directory: readable, not writable, and the
/// host's directory untouched.
#[test]
#[ignore = "needs root"]
fn a_read_only_bind_from_the_host() {
    let bundle = Bundle::new();
    let host = HostDir::new("ro-bind");
    let mut config = running(&bundle, "cat /data/file; touch /data/new");
    config["mounts"] = json!([{"destination": "/data", "type": "bind", "source": host.arg(), "options": ["rbind", "ro"]}]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.stdout, "from the host\n");
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
    assert!(!host.path.join("new").exists());
}

/// `rbind` with `ro`: the mounts below the source are read-only too, not
/// only the top one (probe 8).
#[test]
#[ignore = "needs root"]
fn a_read_only_rbind_covers_the_mounts_below() {
    let bundle = Bundle::new();
    let host = HostDir::new("ro-rbind");
    let sub = host.path.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&sub)
        .status()
        .unwrap();
    assert!(mounted.success());
    let mut config = running(&bundle, "touch /data/sub/new");
    config["mounts"] = json!([{"destination": "/data", "type": "bind", "source": host.arg(), "options": ["rbind", "ro"]}]);
    let run = run_bundle(&bundle, &config);
    assert_ne!(run.code, Some(0));
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
    assert!(!sub.join("new").exists());
}

/// Mounts a tmpfs on `host`'s `sub` (unmounted when `host` is dropped),
/// then remounts it with `options` (`bind,ro`: read-only as a mount, not
/// as a file system).
fn mount_sub(host: &HostDir, options: Option<&str>) -> PathBuf {
    let sub = host.path.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let mount = |args: &[&str]| {
        let status = Command::new("mount").args(args).arg(&sub).status().unwrap();
        assert!(status.success(), "mount {args:?}");
    };
    mount(&["-t", "tmpfs", "tmpfs"]);
    if let Some(options) = options {
        mount(&["-o", &format!("remount,{options}")]);
    }
    sub
}

/// Restricting a bind (`nosuid` here) never lifts a restriction it came
/// with: a host mount that's read-only stays so, inside the bind or as its
/// source. (A bind remount sets the flags to exactly those it's given.)
#[test]
#[ignore = "needs root"]
fn restricting_a_bind_keeps_the_hosts_read_only_mounts() {
    let bundle = Bundle::new();
    let host = HostDir::new("ro-below");
    let sub = mount_sub(&host, Some("bind,ro"));
    let mut config = running(&bundle, "touch /data/sub/below; touch /sub/top; echo done");
    config["mounts"] = json!([
        {"destination": "/data", "type": "bind", "source": host.arg(), "options": ["rbind", "nosuid"]},
        {"destination": "/sub", "type": "bind", "source": sub, "options": ["bind", "nodev"]}
    ]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.stdout, "done\n", "{}", run.stderr);
    assert_eq!(
        run.stderr.matches("Read-only file system").count(),
        2,
        "{}",
        run.stderr
    );
}

/// `readonlyPaths` covers the mounts below the path too.
#[test]
#[ignore = "needs root"]
fn read_only_paths_cover_the_mounts_below() {
    let bundle = Bundle::new();
    let host = HostDir::new("ro-path");
    let sub = mount_sub(&host, None);
    let mut config = running(&bundle, "touch /data/top; touch /data/sub/below; echo done");
    config["mounts"] = json!([{"destination": "/data", "type": "bind", "source": host.arg(), "options": ["rbind"]}]);
    config["linux"]["readonlyPaths"] = json!(["/data"]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.stdout, "done\n", "{}", run.stderr);
    assert_eq!(
        run.stderr.matches("Read-only file system").count(),
        2,
        "{}",
        run.stderr
    );
    assert!(!sub.join("below").exists() && !host.path.join("top").exists());
}

/// A file bound onto a file the container doesn't have yet: it's made.
/// A read-only file bind: the restriction reaches a mount whose root is a
/// file, not a directory.
#[test]
#[ignore = "needs root"]
fn a_read_only_file_bind() {
    let bundle = Bundle::new();
    let host = HostDir::new("ro-file-bind");
    let source = host.path.join("file");
    let mut config = running(&bundle, "cat /settings; echo more >> /settings");
    config["mounts"] = json!([{"destination": "/settings", "type": "bind", "source": source, "options": ["bind", "ro"]}]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.stdout, "from the host\n", "{}", run.stderr);
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "from the host\n");
}

#[test]
#[ignore = "needs root"]
fn a_file_bind() {
    let bundle = Bundle::new();
    let host = HostDir::new("file-bind");
    let source = host.path.join("file");
    let out = ok(&bundle, "cat /etc/config/settings", |config| {
        config["mounts"] = json!([{
            "destination": "/etc/config/settings",
            "type": "bind",
            "source": source,
            "options": ["bind"]
        }]);
    });
    assert_eq!(out, "from the host\n");
}

#[test]
#[ignore = "needs root"]
fn a_tmpfs_with_options() {
    let bundle = Bundle::new();
    let out = ok(
        &bundle,
        "touch /cache/x && while read -r _ _ _ _ p o _ r; do [ \"$p\" = /cache ] && echo \"$o ${r#*- }\"; done < /proc/self/mountinfo; true",
        |config| {
            config["mounts"] = json!([{
                "destination": "/cache",
                "type": "tmpfs",
                "options": ["nosuid", "nodev", "size=1m"]
            }]);
        },
    );
    assert!(out.contains("nosuid") && out.contains("nodev"), "{out}");
    assert!(out.contains("size=1024k"), "{out}");
}

/// A destination whose text is fine but that a symlink in the root
/// filesystem leads into `/proc`: refused, as a destination in `/proc` is.
#[test]
#[ignore = "needs root"]
fn a_destination_led_into_proc_is_refused() {
    let bundle = Bundle::new();
    std::os::unix::fs::symlink("/proc/sys", bundle.root.host_path("/data")).unwrap();
    let mut config = running(&bundle, "echo ran");
    config["mounts"] = json!([{"destination": "/data", "type": "tmpfs", "source": "tmpfs"}]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("leads into /proc or /sys"),
        "{}",
        run.stderr
    );
    assert_eq!(run.stdout, "");
}

/// A destination through a symlink planted in the root filesystem, pointing
/// at the host's `/etc`: it's followed inside the root, never out of it.
#[test]
#[ignore = "needs root"]
fn a_destination_through_a_symlink_stays_inside() {
    let bundle = Bundle::new();
    let host = HostDir::new("planted");
    std::os::unix::fs::symlink("/etc", bundle.root.host_path("/evil")).unwrap();
    let out = ok(&bundle, "cat /etc/planted/file", |config| {
        config["mounts"] = json!([{"destination": "/evil/planted", "type": "bind", "source": host.arg(), "options": ["rbind"]}]);
    });
    assert_eq!(out, "from the host\n");
    assert!(!Path::new("/etc/planted").exists(), "made on the host");
    assert!(bundle.root.host_path("/etc/planted").is_dir());
}

/// What doesn't confine is ignored, with a warning, and the bundle runs.
#[test]
#[ignore = "needs root"]
fn ignored_fields_are_warned_about() {
    let bundle = Bundle::new();
    let mut config = running(&bundle, "echo ran");
    config["mounts"] = json!([
        {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["relatime"]}
    ]);
    let run = run_bundle(&bundle, &config);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (Some(0), "ran\n"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("warning") && run.stderr.contains("relatime"),
        "{}",
        run.stderr
    );
}

// --- Hooks ------------------------------------------------------------------------

/// A run is a container's whole life, as `runc run`: every phase's hooks
/// run, in order, with the state the spec says, the run named `run-PID`.
/// `startContainer` ran before the program, inside its root.
#[test]
#[ignore = "needs root"]
fn a_run_runs_every_hook() {
    let bundle = Bundle::new();
    let log = std::env::temp_dir().join(format!("shipyard-run-hooks-{}", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let mut config = running(&bundle, "cat /tmp/hooks");
    config["hooks"] = bundle.recording_hooks(&log);
    let run = run_bundle(&bundle, &config);
    let lines = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(
        Bundle::checked_start_container(&run.stdout).starts_with('{'),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(r#""status":"created""#),
        "{}",
        run.stdout
    );
    let lines: Vec<&str> = lines.lines().collect();
    let phases: Vec<&str> = lines
        .iter()
        .map(|line| line.split(' ').next().unwrap())
        .collect();
    assert_eq!(
        phases,
        [
            "prestart",
            "createRuntime",
            "createContainer",
            "poststart",
            "poststop"
        ],
        "{lines:?}"
    );
    let id = lines[0].split(' ').nth(1).unwrap();
    assert!(id.starts_with("id=run-"), "{id}");
    assert!(lines.iter().all(|line| line.contains(id)), "{lines:?}");
    assert!(lines[1].ends_with(" mounted"), "{lines:?}");
    assert!(lines[3].contains("status=running pid="), "{lines:?}");
    assert!(lines[4].contains("status=stopped pid= "), "{lines:?}");
}

/// A hook failing before the program is the run's failure (125), naming
/// it; the program never runs.
#[test]
#[ignore = "needs root"]
fn a_failing_hook_fails_the_run() {
    let bundle = Bundle::new();
    for phase in ["prestart", "createContainer", "startContainer"] {
        let mut config = running(&bundle, "echo ran");
        config["hooks"] = json!({phase: [{"path": "/bin/sh", "args": ["sh", "-c", "exit 3"]}]});
        let run = run_bundle(&bundle, &config);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (Some(125), ""),
            "{phase}: {}",
            run.stderr
        );
        let expected = format!("the {phase} hook /bin/sh (hooks.{phase}[0]) exited with code 3");
        assert!(run.stderr.contains(&expected), "{phase}: {}", run.stderr);
    }
}

/// Hooks that can't run as given are the bundle's errors, before anything
/// runs (as any user).
#[test]
fn hooks_that_cant_run_are_refused() {
    let bundle = Bundle::new();
    let mut config = running(&bundle, "echo ran");
    config["hooks"] = json!({"poststop": [{"path": "relative/hook"}]});
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("hooks.poststop[0]: relative/hook isn't an absolute path"),
        "{}",
        run.stderr
    );
}
