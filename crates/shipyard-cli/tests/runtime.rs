//! End-to-end tests of `shipyard-runtime`: the OCI lifecycle, one call at a
//! time (Linux, root).
//!
//! A created container outlives `create` and keeps the standard output and
//! error `create` had, so `create` writes to files here: a pipe would stay
//! open, and a reader waiting for its end would wait for the container.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::{Rootless, delegated_cgroup, rootless, under_rosetta};

const RUNTIME: &str = env!("CARGO_BIN_EXE_shipyard-runtime");

/// A state root for one test, and a place for `create`'s output; removed on
/// drop (with every container in it killed and deleted first).
struct Runtime {
    root: PathBuf,
}

/// What a call printed, and how it ended.
#[derive(Debug)]
struct Call {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Runtime {
    fn new() -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "shipyard-runtime-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        Self { root }
    }

    /// `shipyard-runtime --root ROOT ARGS…`, for calls that don't leave a
    /// container holding their output.
    fn call(&self, args: &[&str]) -> Call {
        let output = Command::new(RUNTIME)
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Call {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `create`, with its (and so the container's) output going to files.
    fn create(&self, id: &str, bundle: &Bundle, extra: &[&str]) -> Call {
        self.create_with(id, bundle, extra, Command::new(RUNTIME))
    }

    /// `create` like [`Runtime::create`], run by `command` (the runtime,
    /// set up as a test needs it).
    fn create_with(&self, id: &str, bundle: &Bundle, extra: &[&str], mut command: Command) -> Call {
        std::fs::create_dir_all(self.output_dir()).unwrap();
        // (An ID with a `/` is refused, but its file name must still work.)
        let name = id.replace('/', "_");
        let out = self.output_dir().join(format!("{name}.out"));
        let err = self.output_dir().join(format!("{name}.err"));
        let status = command
            .arg("--root")
            .arg(&self.root)
            .args(["create", "--bundle", bundle.arg()])
            .args(extra)
            .arg(id)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&out).unwrap())
            .stderr(std::fs::File::create(&err).unwrap())
            .status()
            .unwrap();
        Call {
            code: status.code(),
            stdout: std::fs::read_to_string(&out).unwrap_or_default(),
            stderr: std::fs::read_to_string(&err).unwrap_or_default(),
        }
    }

    /// What the container `id` wrote to its standard output so far.
    fn output_of(&self, id: &str) -> String {
        std::fs::read_to_string(self.output_dir().join(format!("{id}.out"))).unwrap_or_default()
    }

    fn output_dir(&self) -> PathBuf {
        self.root.with_extension("out")
    }

    /// `state ID`, parsed (`None` if there's no such container).
    fn state(&self, id: &str) -> Option<Value> {
        let call = self.call(&["state", id]);
        (call.code == Some(0)).then(|| serde_json::from_str(&call.stdout).unwrap())
    }

    fn status(&self, id: &str) -> String {
        self.state(id)
            .and_then(|state| state["status"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "gone".to_owned())
    }

    /// How container `id` ended, as its monitor recorded it (waiting up to
    /// 5 s for the record).
    fn exit_record(&self, id: &str) -> String {
        let exit = self.root.join(id).join("exit");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exit.exists() {
            assert!(Instant::now() < deadline, "no exit recorded for {id}");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(exit).unwrap()
    }

    /// Waits (up to 10 s) until container `id` is `status`.
    fn wait_for(&self, id: &str, status: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.status(id) != status {
            assert!(
                Instant::now() < deadline,
                "{id} never became {status}: {}",
                self.status(id)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let id = entry.file_name().to_string_lossy().into_owned();
                let _ = self.call(&["delete", "--force", &id]);
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(self.output_dir());
    }
}

/// A bundle running `sh -c SCRIPT`.
fn bundle_running(script: &str) -> Bundle {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    bundle.write(&config);
    bundle
}

/// Container `id`'s cgroup, which root's containers get (cgroup v2).
fn cgroup_of(id: &str) -> PathBuf {
    PathBuf::from("/sys/fs/cgroup/shipyard/runtime").join(id)
}

/// The processes in the cgroup at `path` (none if it's gone).
fn cgroup_procs(path: &std::path::Path) -> Vec<i64> {
    std::fs::read_to_string(path.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

fn alive(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit(')')
            .next()
            .unwrap_or_default()
            .trim_start()
            .starts_with('Z')
    })
}

// --- As any user ---------------------------------------------------------------

/// Without root, a container is rootless: created (its bundle given a user
/// namespace, with a warning), started, stopped and deleted, all as the
/// caller. Root inside is the caller.
#[test]
fn a_rootless_container_lives_its_life() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = bundle_running("stat -L -c %u /proc/self; head -1 /proc/self/uid_map");
    let created = runtime.create("rootless", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert!(
        created.stderr.contains("no user namespace") && created.stderr.contains("added"),
        "{}",
        created.stderr
    );
    assert_eq!(runtime.call(&["start", "rootless"]).code, Some(0));
    runtime.wait_for("rootless", "stopped");
    let out = runtime.output_of("rootless");
    let words: Vec<&str> = out.split_whitespace().collect();
    // SAFETY: `getuid` has no preconditions.
    let me = unsafe { libc::getuid() }.to_string();
    assert_eq!(words, ["0", "0", me.as_str(), "1"], "{out}");
    assert_eq!(runtime.call(&["delete", "rootless"]).code, Some(0));
    assert_eq!(runtime.state("rootless"), None);
}

/// A rootless container's hooks run as the caller, every phase, and the
/// runtime's see the container's mounts as root's do (the caller owns its
/// user namespace).
#[test]
fn a_rootless_containers_hooks_run() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let (config, log) = hooked(&runtime, &bundle, "cat /tmp/hooks");
    bundle.write(&config);
    let call = runtime.create("hooked", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "hooked"]).code, Some(0));
    runtime.wait_for("hooked", "stopped");
    let _ = Bundle::checked_start_container(&runtime.output_of("hooked"));
    assert_eq!(runtime.call(&["delete", "hooked"]).code, Some(0));
    let phases: Vec<String> = lines_of(&log)
        .iter()
        .map(|line| line.split(' ').next().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases,
        [
            "prestart",
            "createRuntime",
            "createContainer",
            "poststart",
            "poststop"
        ]
    );
    assert!(
        lines_of(&log)[1].ends_with(" mounted"),
        "{:?}",
        lines_of(&log)
    );
    // Written by the caller.
    // SAFETY: `getuid` has no preconditions.
    assert_eq!(std::fs::metadata(&log).unwrap().uid(), unsafe {
        libc::getuid()
    });
}

/// A rootless container's cgroup follows delegation: where the user was
/// delegated a subtree, the container is placed there (with its limits, its
/// cgroup namespace and mount), and the cgroup goes at its end. Where nothing
/// is delegated, the cgroup mount is left out, with a warning, and the
/// container still runs. Without a `pid` namespace, a rootless container's
/// `/proc` can't be mounted: refused, saying so.
#[test]
fn a_rootless_containers_cgroup_follows_delegation() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] = json!([{"type": "mount"}, {"type": "cgroup"}]);
    bundle.write(&config);
    let refused = runtime.create("pidless", &bundle, &[]);
    assert_eq!(refused.code, Some(1));
    assert!(
        refused.stderr.contains("needs a PID namespace too"),
        "{}",
        refused.stderr
    );
    config["linux"]["namespaces"] = json!([{"type": "pid"}, {"type": "mount"}, {"type": "cgroup"}]);
    config["mounts"] = json!([{
        "destination": "/sys/fs/cgroup", "type": "cgroup", "source": "cgroup", "options": ["ro"]
    }]);
    config["process"]["args"] =
        json!(["sh", "-c", "cat /proc/self/cgroup /sys/fs/cgroup/pids.max"]);
    let Some(base) = delegated_cgroup() else {
        bundle.write(&config);
        let created = runtime.create("ungrouped", &bundle, &[]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        assert!(
            created
                .stderr
                .contains("the cgroup mount at /sys/fs/cgroup is left out"),
            "{}",
            created.stderr
        );
        return;
    };
    config["linux"]["resources"] = json!({"pids": {"limit": 40}});
    bundle.write(&config);
    let created = runtime.create("grouped", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert!(!created.stderr.contains("left out"), "{}", created.stderr);
    let cgroup = base.join("shipyard/runtime/grouped");
    let record: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("grouped/state.json")).unwrap())
            .unwrap();
    assert_eq!(record["cgroup"], cgroup.to_str().unwrap());
    assert_eq!(runtime.call(&["start", "grouped"]).code, Some(0));
    let ended = runtime.exit_record("grouped");
    assert!(ended.contains("exit code 0"), "{ended}");
    assert_eq!(runtime.output_of("grouped"), "0::/\n40\n");
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
    assert_eq!(runtime.call(&["delete", "grouped"]).code, Some(0));
}

/// A rootless container's cgroup commands follow delegation: with a
/// delegated subtree, pause, resume, update, ps and events work on it; without
/// one, they're refused, saying why.
#[test]
fn rootless_cgroup_commands_follow_delegation() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 300");
    let created = runtime.create("tended", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "tended"]).code, Some(0));
    let Some(base) = delegated_cgroup() else {
        let refused = runtime.call(&["pause", "tended"]);
        assert_eq!(refused.code, Some(1));
        assert!(
            refused.stderr.contains("has no cgroup of its own"),
            "{}",
            refused.stderr
        );
        return;
    };
    assert_eq!(runtime.call(&["pause", "tended"]).code, Some(0));
    assert_eq!(runtime.status("tended"), "paused");
    assert_eq!(runtime.call(&["resume", "tended"]).code, Some(0));
    let call = runtime.call(&["update", "--pids-limit", "20", "tended"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let pids_max = base.join("shipyard/runtime/tended/pids.max");
    assert_eq!(std::fs::read_to_string(pids_max).unwrap(), "20\n");
    let ps = runtime.call(&["ps", "--format", "json", "tended"]);
    let pids: Vec<i64> = serde_json::from_str(&ps.stdout).unwrap();
    assert!(!pids.is_empty(), "{}", ps.stderr);
    let stats = runtime.call(&["events", "--stats", "tended"]);
    let event: Value = serde_json::from_str(stats.stdout.trim()).unwrap();
    assert_eq!(event["data"]["pids"]["limit"], 20);
}

/// Without `--root`, a non-root caller's state is in
/// `$XDG_RUNTIME_DIR/shipyard`; without that, `--root` must be given.
#[test]
fn rootless_state_is_in_the_runtime_dir() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime_dir = std::env::temp_dir().join(format!("shipyard-xdg-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let command = |args: &[&str], xdg: Option<&PathBuf>| {
        let mut command = Command::new(RUNTIME);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        match xdg {
            Some(dir) => command.env("XDG_RUNTIME_DIR", dir),
            None => command.env_remove("XDG_RUNTIME_DIR"),
        };
        command
    };
    let bundle = bundle_running("sleep 300");
    // Only the status: the container holds `create`'s stderr open while it
    // lives, so reading it to its end would wait for the container.
    let created = command(
        &["create", "--bundle", bundle.arg(), "xdg"],
        Some(&runtime_dir),
    )
    .stderr(Stdio::null())
    .status()
    .unwrap();
    assert!(created.success());
    assert!(runtime_dir.join("shipyard/xdg/state.json").is_file());
    let deleted = command(&["delete", "--force", "xdg"], Some(&runtime_dir))
        .status()
        .unwrap();
    assert!(deleted.success());
    let without = command(&["state", "xdg"], None).output().unwrap();
    let said = String::from_utf8_lossy(&without.stderr);
    assert!(
        !without.status.success() && said.contains("XDG_RUNTIME_DIR"),
        "{said}"
    );
    std::fs::remove_dir_all(&runtime_dir).unwrap();
}

#[test]
fn unknown_containers_and_ids_are_errors() {
    let runtime = Runtime::new();
    for args in [
        &["state", "nope"][..],
        &["start", "nope"],
        &["kill", "nope"],
        &["delete", "nope"],
    ] {
        let call = runtime.call(args);
        assert_eq!(call.code, Some(1), "{args:?}");
        assert!(
            call.stderr.contains("no container with ID nope"),
            "{args:?}: {}",
            call.stderr
        );
    }
    let call = runtime.call(&["state", "../etc"]);
    assert!(
        call.stderr.contains("invalid container ID"),
        "{}",
        call.stderr
    );
}

/// SIGSTOP would stop the init, which can't pass it on: refused, whatever
/// the container.
#[test]
fn stop_is_refused() {
    let call = Runtime::new().call(&["kill", "any", "STOP"]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("send TSTP"), "{}", call.stderr);
}

// --- As root: the lifecycle ------------------------------------------------------

/// create → created (the program hasn't run) → start → running → kill →
/// stopped → delete → gone.
#[test]
#[ignore = "needs root"]
fn the_lifecycle() {
    let runtime = Runtime::new();
    let bundle = bundle_running("echo started; exec sleep 300");
    let call = runtime.create("life", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let state = runtime.state("life").unwrap();
    assert_eq!(state["status"], "created");
    assert_eq!(state["ociVersion"], "1.2.0");
    assert_eq!(state["bundle"], bundle.arg());
    let pid = state["pid"].as_i64().unwrap();
    assert!(alive(pid));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(runtime.output_of("life"), "", "nothing runs before start");

    assert_eq!(runtime.call(&["start", "life"]).code, Some(0));
    assert_eq!(runtime.status("life"), "running");
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.output_of("life") != "started\n" {
        assert!(
            Instant::now() < deadline,
            "no output: {:?}",
            runtime.output_of("life")
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(runtime.call(&["kill", "life", "KILL"]).code, Some(0));
    runtime.wait_for("life", "stopped");
    assert!(
        runtime.state("life").unwrap().get("pid").is_none(),
        "no PID once stopped"
    );
    assert!(!alive(pid));

    assert_eq!(runtime.call(&["delete", "life"]).code, Some(0));
    assert_eq!(runtime.status("life"), "gone");
    assert!(!runtime.root.join("life").exists());
}

/// A program that ends by itself: stopped, with its exit recorded.
/// A caller that ignores SIGCHLD (passed on across `exec`) doesn't blind
/// the monitor: with it ignored, the kernel would reap the container's init
/// itself, and how the container ended would be lost.
#[test]
#[ignore = "needs root"]
fn a_caller_ignoring_sigchld_doesnt_lose_the_exit() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exit 3");
    let mut command = Command::new(RUNTIME);
    // SAFETY: `ignore` only calls `sigaction`, as `pre_exec` requires.
    unsafe { command.pre_exec(|| shipyard_test_support::ignore(libc::SIGCHLD)) };
    let call = runtime.create_with("ignoring", &bundle, &[], command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "ignoring"]).code, Some(0));
    runtime.wait_for("ignoring", "stopped");
    let recorded = runtime.exit_record("ignoring");
    assert!(recorded.contains("exit code 3"), "{recorded}");
}

#[test]
#[ignore = "needs root"]
fn a_container_that_exits_is_stopped() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exit 3");
    assert_eq!(runtime.create("exits", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "exits"]).code, Some(0));
    runtime.wait_for("exits", "stopped");
    let recorded = runtime.exit_record("exits");
    assert!(
        recorded.contains("exit code 3"),
        "{recorded}; stderr: {:?}",
        std::fs::read_to_string(runtime.output_dir().join("exits.err"))
    );
    // Started already: not again.
    let call = runtime.call(&["start", "exits"]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("is stopped"), "{}", call.stderr);
    let call = runtime.call(&["kill", "exits", "TERM"]);
    assert!(call.stderr.contains("is stopped"), "{}", call.stderr);
}

#[test]
#[ignore = "needs root"]
fn the_pid_file_names_the_init() {
    let runtime = Runtime::new();
    let file = runtime.root.with_extension("pid");
    let file_arg = file.to_str().unwrap();
    let bundle = bundle_running("sleep 300");
    let call = runtime.create("pidded", &bundle, &["--pid-file", file_arg]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let written: i64 = std::fs::read_to_string(&file).unwrap().parse().unwrap();
    assert_eq!(
        Some(written),
        runtime.state("pidded").unwrap()["pid"].as_i64()
    );
    let _ = std::fs::remove_file(file);
}

/// A PID file that can't be written fails `create`, and then nothing is
/// left: a caller told "failed" would otherwise find the ID taken.
#[test]
#[ignore = "needs root"]
fn a_pid_file_that_cant_be_written_leaves_nothing() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    let call = runtime.create("unpidded", &bundle, &["--pid-file", "/no/such/dir/c.pid"]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("PID file"), "{}", call.stderr);
    assert_eq!(runtime.status("unpidded"), "gone");
}

#[test]
#[ignore = "needs root"]
fn ids_are_unique_and_checked() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    assert_eq!(runtime.create("twice", &bundle, &[]).code, Some(0));
    let call = runtime.create("twice", &bundle, &[]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("exists"), "{}", call.stderr);
    assert_eq!(runtime.status("twice"), "created", "the first is untouched");
    let call = runtime.create("bad/id", &bundle, &[]);
    assert!(
        call.stderr.contains("invalid container ID"),
        "{}",
        call.stderr
    );
}

/// A bundle that can't run: an error, and nothing left behind.
#[test]
#[ignore = "needs root"]
fn a_failed_create_leaves_nothing() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["no-such-program"]);
    config["process"]["user"] = json!({"uid": 0, "gid": 0});
    config["mounts"] = json!([{"destination": "/data", "type": "bind", "source": "/no/such/dir"}]);
    bundle.write(&config);
    let call = runtime.create("broken", &bundle, &[]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("/no/such/dir"), "{}", call.stderr);
    assert!(!runtime.root.join("broken").exists());
    // A failure inside the container's setup, too.
    config["mounts"] = json!([]);
    config["linux"]["sysctl"] = json!({"fs.mqueue.msg_max": "0"});
    bundle.write(&config);
    let call = runtime.create("broken", &bundle, &[]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(!runtime.root.join("broken").exists());
}

/// Root's container is born into a cgroup of its own, recorded in its
/// state; the cgroup goes when the container ends, before its end is
/// recorded.
#[test]
#[ignore = "needs root"]
fn a_container_has_a_cgroup_for_its_life() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 300");
    let call = runtime.create("grouped", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let cgroup = cgroup_of("grouped");
    let record: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("grouped/state.json")).unwrap())
            .unwrap();
    assert_eq!(record["cgroup"], cgroup.to_str().unwrap());
    let pid = runtime.state("grouped").unwrap()["pid"].as_i64().unwrap();
    assert_eq!(cgroup_procs(&cgroup), [pid], "the init, at the gate");

    assert_eq!(runtime.call(&["start", "grouped"]).code, Some(0));
    assert_eq!(runtime.call(&["kill", "grouped", "KILL"]).code, Some(0));
    let _ = runtime.exit_record("grouped");
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
    assert_eq!(runtime.call(&["delete", "grouped"]).code, Some(0));
}

/// A `create` killed between making its cgroup and recording it leaves the
/// cgroup pending in its state: `delete` removes it (only `rmdir`: one with a
/// process in it is left, and nothing is killed), and the ID can be created
/// again.
#[test]
#[ignore = "needs root"]
fn delete_removes_a_killed_creates_pending_cgroup() {
    use std::os::unix::fs::DirBuilderExt as _;
    let runtime = Runtime::new();
    let pending = |id: &str| {
        let dir = runtime.root.join(id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let record = json!({
            "bundle": "/nonexistent",
            "init": null,
            "annotations": {},
            "cgroup": cgroup_of(id),
            "cgroup_base": "/sys/fs/cgroup",
            "cgroup_pending": true,
        });
        std::fs::write(dir.join("state.json"), record.to_string()).unwrap();
        std::fs::create_dir_all(cgroup_of(id)).unwrap();
    };
    pending("leftover");
    assert_eq!(runtime.call(&["delete", "leftover"]).code, Some(0));
    assert!(!cgroup_of("leftover").exists());
    let bundle = bundle_running("exec sleep 300");
    let call = runtime.create("leftover", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(
        runtime.call(&["delete", "--force", "leftover"]).code,
        Some(0)
    );

    // The control: one with a process in it (not the killed `create`'s).
    pending("occupied");
    let mut inside = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    std::fs::write(
        cgroup_of("occupied").join("cgroup.procs"),
        inside.id().to_string(),
    )
    .unwrap();
    assert_eq!(runtime.call(&["delete", "occupied"]).code, Some(0));
    assert_eq!(
        cgroup_procs(&cgroup_of("occupied")),
        [i64::from(inside.id())]
    );
    inside.kill().unwrap();
    let _ = inside.wait();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::fs::remove_dir(cgroup_of("occupied")).is_err() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Without a PID namespace, the init's death doesn't end the rest: the
/// cgroup does. A process the init left behind is killed by `delete
/// --force`, and the cgroup goes.
#[test]
#[ignore = "needs root"]
fn a_bundle_without_a_pid_namespace_is_ended_by_its_cgroup() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] = json!([{"type": "mount"}]);
    config["process"]["args"] = json!(["sh", "-c", "sleep 300 & echo $!; exec sleep 301"]);
    bundle.write(&config);
    let call = runtime.create("flat", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "flat"]).code, Some(0));
    let left: i64 = eventually("the background PID", || {
        runtime.output_of("flat").trim().parse().ok()
    });
    assert!(cgroup_procs(&cgroup_of("flat")).contains(&left));

    let call = runtime.call(&["delete", "--force", "flat"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(!alive(left), "the process the init left behind lives on");
    assert!(!cgroup_of("flat").exists());
}

/// A bundle's cgroup namespace and mount: the container sees its own
/// cgroup, read-only, as its config asks.
#[test]
#[ignore = "needs root"]
fn a_bundles_cgroup_mount_is_its_own_cgroup() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] = json!([{"type": "pid"}, {"type": "mount"}, {"type": "cgroup"}]);
    config["mounts"] = json!([{
        "destination": "/sys/fs/cgroup", "type": "cgroup", "source": "cgroup",
        "options": ["nosuid", "noexec", "nodev", "relatime", "ro"]
    }]);
    config["process"]["args"] = json!([
        "sh",
        "-c",
        "cat /proc/self/cgroup; stat -f -c %T /sys/fs/cgroup; mkdir /sys/fs/cgroup/x 2>&1"
    ]);
    bundle.write(&config);
    let call = runtime.create("viewed", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(call.stderr.is_empty(), "no warning: {}", call.stderr);
    assert_eq!(runtime.call(&["start", "viewed"]).code, Some(0));
    let _ = runtime.exit_record("viewed");
    let out = runtime.output_of("viewed");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[..2], ["0::/", "cgroup2fs"], "{out}");
    assert!(lines[2].contains("Read-only file system"), "{out}");
}

/// A bundle's `linux.resources` are written into its cgroup before
/// anything runs there.
#[test]
#[ignore = "needs root"]
fn a_bundles_limits_are_its_cgroups() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["resources"] = json!({"pids": {"limit": 32}, "memory": {"limit": 67_108_864}});
    bundle.write(&config);
    let call = runtime.create("limited", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let read = |file: &str| std::fs::read_to_string(cgroup_of("limited").join(file)).unwrap();
    assert_eq!(read("pids.max"), "32\n");
    assert_eq!(read("memory.max"), "67108864\n");
}

/// A limit file the kernel doesn't have is said to be missing (written
/// without `O_CREAT`: with it, cgroupfs answers EACCES, "Permission denied",
/// which hid that), and nothing is left behind.
#[test]
#[ignore = "needs root"]
fn a_missing_limit_file_is_named() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["resources"] = json!({"unified": {"memory.bogus": "1"}});
    bundle.write(&config);
    let call = runtime.create("bogus", &bundle, &[]);
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr.contains("has no memory.bogus"),
        "{}",
        call.stderr
    );
    assert!(!cgroup_of("bogus").exists());
    assert!(!runtime.root.join("bogus").exists());
}

/// Killed by the OOM killer: the exit record says so.
#[test]
#[ignore = "needs root"]
fn an_oom_kill_is_recorded() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    // 32 MiB, no swap (memory plus swap is the memory alone).
    config["linux"]["resources"] = json!({"memory": {"limit": 33_554_432, "swap": 33_554_432}});
    config["process"]["args"] = json!(["sh", "-c", "x=$(head -c 200m /dev/urandom)"]);
    bundle.write(&config);
    assert_eq!(runtime.create("hungry", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "hungry"]).code, Some(0));
    let ended = runtime.exit_record("hungry");
    assert!(ended.contains("(out of memory)"), "{ended}");
}

/// A SIGKILL from `kill` is the container's end, not the OOM killer's,
/// though it struck earlier (a process of the container's, which went on).
#[test]
#[ignore = "needs root"]
fn a_kill_after_an_oom_kill_is_a_kill() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["resources"] = json!({"memory": {"limit": 33_554_432, "swap": 33_554_432}});
    let script = "sh -c 'x=$(head -c 200m /dev/urandom)'; echo survived; sleep 30";
    config["process"]["args"] = json!(["sh", "-c", script]);
    bundle.write(&config);
    assert_eq!(runtime.create("survivor", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "survivor"]).code, Some(0));
    eventually("the OOM kill", || {
        runtime
            .output_of("survivor")
            .contains("survived")
            .then_some(())
    });
    assert_eq!(runtime.call(&["kill", "survivor", "KILL"]).code, Some(0));
    let ended = runtime.exit_record("survivor");
    assert!(!ended.contains("out of memory"), "{ended}");
}

/// `linux.cgroupsPath` places the cgroup: absolute below the root,
/// relative below `shipyard/runtime/`. It must be a new one.
#[test]
#[ignore = "needs root"]
fn a_cgroups_path_is_honored() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    let parent = format!("shipyard-test-{}", std::process::id());
    for (id, cgroups_path, place) in [
        (
            "placed",
            format!("/{parent}/placed"),
            format!("{parent}/placed"),
        ),
        (
            "nested",
            format!("{parent}/nested"),
            format!("shipyard/runtime/{parent}/nested"),
        ),
    ] {
        config["linux"]["cgroupsPath"] = json!(cgroups_path);
        bundle.write(&config);
        let call = runtime.create(id, &bundle, &[]);
        assert_eq!(call.code, Some(0), "{cgroups_path}: {}", call.stderr);
        let cgroup = PathBuf::from("/sys/fs/cgroup").join(&place);
        let pid = runtime.state(id).unwrap()["pid"].as_i64().unwrap();
        assert_eq!(cgroup_procs(&cgroup), [pid], "{place}");
        // A parent that isn't Shipyard's own gets only what limits need:
        // none here.
        let parent_subtree =
            std::fs::read_to_string(cgroup.parent().unwrap().join("cgroup.subtree_control"))
                .unwrap();
        if !place.starts_with("shipyard/") {
            assert_eq!(parent_subtree.trim(), "", "{place}'s parent");
        }
        assert_eq!(runtime.call(&["delete", id]).code, Some(0));
        assert!(!cgroup.exists(), "{place} is left");
        let _ = std::fs::remove_dir(cgroup.parent().unwrap());
    }
    // An existing cgroup (this test's own) is never taken over.
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let own = own.trim().trim_start_matches("0::");
    if own != "/" {
        config["linux"]["cgroupsPath"] = json!(own);
        bundle.write(&config);
        let call = runtime.create("taken", &bundle, &[]);
        assert_eq!(call.code, Some(1));
        assert!(call.stderr.contains("exists already"), "{}", call.stderr);
        assert!(!runtime.root.join("taken").exists());
    }
}

/// A container whose cgroup is gone (removed when it ended), and made again
/// at its path by another container's `cgroupsPath`, doesn't take the new
/// one with it when it's deleted.
#[test]
#[ignore = "needs root"]
fn a_cgroup_made_again_is_left_to_its_owner() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "true"]);
    let parent = format!("shipyard-test-reused-{}", std::process::id());
    config["linux"]["cgroupsPath"] = json!(format!("/{parent}/shared"));
    bundle.write(&config);
    assert_eq!(runtime.create("first", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "first"]).code, Some(0));
    runtime.wait_for("first", "stopped");
    config["process"]["args"] = json!(["sleep", "30"]);
    bundle.write(&config);
    let second = runtime.create("second", &bundle, &[]);
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    let delete = runtime.call(&["delete", "first"]);
    assert_eq!(delete.code, Some(0), "{}", delete.stderr);
    let cgroup = PathBuf::from("/sys/fs/cgroup").join(&parent).join("shared");
    let pid = runtime.state("second").unwrap()["pid"].as_i64().unwrap();
    assert_eq!(cgroup_procs(&cgroup), [pid], "the second's cgroup");
    assert_eq!(runtime.status("second"), "created");
    assert_eq!(runtime.call(&["delete", "--force", "second"]).code, Some(0));
    let _ = std::fs::remove_dir(cgroup.parent().unwrap());
}

/// A cgroup that holds processes (another container's, say) takes no
/// container below it, with limits or without: the container would be under
/// its limits, devices, freezing and kill. And it never hands a controller
/// down: even a threaded one (`pids`) would make the kernel refuse every
/// placement below it from then on.
#[test]
#[ignore = "needs root"]
fn a_busy_cgroup_takes_no_container_below_it() {
    let busy = PathBuf::from(format!(
        "/sys/fs/cgroup/shipyard-busy-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&busy).unwrap();
    let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
    std::fs::write(busy.join("cgroup.procs"), sleeper.id().to_string()).unwrap();
    let subtree = || std::fs::read_to_string(busy.join("cgroup.subtree_control")).unwrap();

    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    let name = busy.file_name().unwrap().to_str().unwrap().to_owned();
    config["linux"]["cgroupsPath"] = json!(format!("/{name}/limited"));
    config["linux"]["resources"] = json!({"pids": {"limit": 16}});
    bundle.write(&config);
    let limited = runtime.create("limited", &bundle, &[]);
    let limited_subtree = subtree();

    config["linux"]["cgroupsPath"] = json!(format!("/{name}/plain"));
    config["linux"]["resources"] = json!({});
    bundle.write(&config);
    let plain = runtime.create("plain", &bundle, &[]);
    let plain_subtree = subtree();
    drop(runtime);
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let _ = std::fs::remove_dir(busy.join("limited"));
    let _ = std::fs::remove_dir(busy.join("plain"));
    let _ = std::fs::remove_dir(&busy);

    for refused in [&limited, &plain] {
        assert_eq!(refused.code, Some(1));
        assert!(
            refused.stderr.contains("holds processes"),
            "{}",
            refused.stderr
        );
    }
    assert_eq!(limited_subtree.trim(), "", "nothing handed down");
    assert_eq!(plain_subtree.trim(), "", "nothing handed down");
}

/// A rootless container made from a delegated session (a desktop
/// terminal) is deleted from one without delegation (an SSH login): the
/// record's base says where its cgroup is, and the kernel lets its user
/// remove it from anywhere. Needs `test-linux.sh`'s `tester` and its
/// delegated `user.slice/tester`; skips elsewhere.
#[test]
#[ignore = "needs root"]
fn a_rootless_container_is_deleted_from_another_session() {
    let session = PathBuf::from("/sys/fs/cgroup/user.slice/tester/session");
    let uid = Command::new("id").args(["-u", "tester"]).output().ok();
    let uid = uid
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
    let (Some(uid), true) = (uid, session.exists()) else {
        eprintln!("skipping: needs test-linux.sh's tester and its delegated cgroup");
        return;
    };
    // An SSH login's session scope: a cgroup root owns.
    let undelegated = PathBuf::from(format!(
        "/sys/fs/cgroup/shipyard-ssh-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&undelegated).unwrap();
    let runtime = Runtime::new();
    std::fs::create_dir_all(&runtime.root).unwrap();
    let tester: u32 = uid.parse().unwrap();
    std::os::unix::fs::chown(&runtime.root, Some(tester), Some(tester)).unwrap();
    // As `tester`, started in `cgroup` (moved there before dropping root).
    let as_tester_in = |cgroup: &std::path::Path| {
        let procs = std::ffi::CString::new(
            cgroup
                .join("cgroup.procs")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        let mut command = Command::new("setpriv");
        command
            .args(["--reuid", &uid, "--regid", &uid, "--clear-groups"])
            .arg(RUNTIME);
        // SAFETY: open, write and close are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let written = libc::write(fd, b"0".as_ptr().cast(), 1);
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                if written == 1 { Ok(()) } else { Err(error) }
            });
        }
        command
    };
    let bundle = Bundle::new();
    // Root inside is `tester` outside: the root filesystem is theirs.
    bundle.root.chown_all(tester, tester);
    let created = runtime.create_with("travelled", &bundle, &[], as_tester_in(&session));
    let cgroup = PathBuf::from("/sys/fs/cgroup/user.slice/tester/shipyard/runtime/travelled");
    let made = cgroup.exists();
    let deleted = as_tester_in(&undelegated)
        .arg("--root")
        .arg(&runtime.root)
        .args(["delete", "--force", "travelled"])
        .output()
        .unwrap();
    let left = cgroup.exists();
    let _ = std::fs::remove_dir(&undelegated);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert!(made, "{} wasn't made", cgroup.display());
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(!left, "{} is left", cgroup.display());
}

/// Pause freezes every process of the container; resume lets it go on.
/// While paused, `start` and a second `pause` are refused, and SIGKILL still
/// ends it.
#[test]
#[ignore = "needs root"]
fn pause_and_resume() {
    let runtime = Runtime::new();
    let bundle = bundle_running("while :; do echo tick; sleep 0.05; done");
    assert_eq!(runtime.create("frozen", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "frozen"]).code, Some(0));
    let ticks = || runtime.output_of("frozen").lines().count();
    eventually("ticks", || (ticks() > 2).then_some(()));
    let paused = runtime.call(&["pause", "frozen"]);
    assert_eq!(paused.code, Some(0), "{}", paused.stderr);
    assert_eq!(runtime.status("frozen"), "paused");
    let before = ticks();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(ticks(), before, "nothing runs while paused");
    for refused in [&["pause", "frozen"][..], &["start", "frozen"]] {
        let call = runtime.call(refused);
        assert_eq!(call.code, Some(1), "{refused:?}");
        assert!(call.stderr.contains("is paused"), "{}", call.stderr);
    }
    assert_eq!(runtime.call(&["resume", "frozen"]).code, Some(0));
    assert_eq!(runtime.status("frozen"), "running");
    eventually("ticks again", || (ticks() > before + 2).then_some(()));
    // SIGKILL ends a paused container at once.
    assert_eq!(runtime.call(&["pause", "frozen"]).code, Some(0));
    assert_eq!(runtime.call(&["kill", "frozen", "KILL"]).code, Some(0));
    runtime.wait_for("frozen", "stopped");
    assert_eq!(runtime.call(&["resume", "frozen"]).code, Some(1));
}

/// As with runc, a created container may be paused: resumed, it's created
/// again, and starts.
#[test]
#[ignore = "needs root"]
fn a_created_container_pauses() {
    let runtime = Runtime::new();
    let bundle = bundle_running("echo ran");
    assert_eq!(runtime.create("waiting", &bundle, &[]).code, Some(0));
    let paused = runtime.call(&["pause", "waiting"]);
    assert_eq!(paused.code, Some(0), "{}", paused.stderr);
    assert_eq!(runtime.status("waiting"), "paused");
    assert_eq!(runtime.call(&["resume", "waiting"]).code, Some(0));
    assert_eq!(runtime.status("waiting"), "created");
    assert_eq!(runtime.call(&["start", "waiting"]).code, Some(0));
    runtime.wait_for("waiting", "stopped");
    assert_eq!(runtime.output_of("waiting"), "ran\n");
}

/// `delete --force` ends a paused container and removes its cgroup.
#[test]
#[ignore = "needs root"]
fn a_paused_container_is_deleted_by_force() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 300");
    assert_eq!(runtime.create("still", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "still"]).code, Some(0));
    assert_eq!(runtime.call(&["pause", "still"]).code, Some(0));
    let call = runtime.call(&["delete", "still"]);
    assert_eq!(call.code, Some(1), "not without --force");
    let call = runtime.call(&["delete", "--force", "still"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(!cgroup_of("still").exists());
}

/// `kill --all` signals every process in the cgroup; without it, only the
/// init (which passes it on to the command).
#[test]
#[ignore = "needs root"]
fn kill_all_reaches_every_process() {
    let runtime = Runtime::new();
    let bundle = bundle_running(
        "trap 'echo command-got' USR1; \
         sh -c 'trap \"echo child-got\" USR1; while :; do sleep 0.05; done' & \
         echo ready; while :; do sleep 0.05; done",
    );
    assert_eq!(runtime.create("everyone", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "everyone"]).code, Some(0));
    eventually("ready", || {
        runtime
            .output_of("everyone")
            .contains("ready")
            .then_some(())
    });
    assert_eq!(runtime.call(&["kill", "everyone", "USR1"]).code, Some(0));
    eventually("the command's", || {
        runtime
            .output_of("everyone")
            .contains("command-got")
            .then_some(())
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!runtime.output_of("everyone").contains("child-got"));
    let call = runtime.call(&["kill", "--all", "everyone", "USR1"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    eventually("the child's", || {
        runtime
            .output_of("everyone")
            .contains("child-got")
            .then_some(())
    });
}

/// `update` changes a running container's limits, from flags or a
/// `linux.resources` file; a stopped one is refused.
#[test]
#[ignore = "needs root"]
fn update_changes_limits_live() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 300");
    assert_eq!(runtime.create("growing", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "growing"]).code, Some(0));
    let read = |file: &str| std::fs::read_to_string(cgroup_of("growing").join(file)).unwrap();
    let call = runtime.call(&["update", "--memory", "64m", "--pids-limit", "40", "growing"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(
        (read("memory.max"), read("pids.max")),
        ("67108864\n".into(), "40\n".into())
    );
    assert_eq!(
        read("memory.swap.max"),
        "max\n",
        "no Docker default swap in an update"
    );
    // Memory plus swap alone: counted from the memory limit now (64 MiB).
    let call = runtime.call(&["update", "--memory-swap", "96m", "growing"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(read("memory.swap.max"), "33554432\n");
    assert_eq!(read("memory.max"), "67108864\n");
    let file = runtime.root.with_extension("resources.json");
    std::fs::write(&file, r#"{"pids": {"limit": 30}}"#).unwrap();
    let call = runtime.call(&["update", "--resources", file.to_str().unwrap(), "growing"]);
    let _ = std::fs::remove_file(&file);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(read("pids.max"), "30\n");
    assert_eq!(read("memory.max"), "67108864\n", "the rest stays");
    // A quota alone keeps the period set before (as runc's).
    let call = runtime.call(&[
        "update",
        "--cpu-quota",
        "25000",
        "--cpu-period",
        "50000",
        "growing",
    ]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let call = runtime.call(&["update", "--cpu-quota", "40000", "growing"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(read("cpu.max"), "40000 50000\n");
    assert_eq!(
        runtime.call(&["update", "growing"]).code,
        Some(1),
        "nothing given"
    );
    assert_eq!(runtime.call(&["kill", "growing", "KILL"]).code, Some(0));
    runtime.wait_for("growing", "stopped");
    let call = runtime.call(&["update", "--pids-limit", "10", "growing"]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("is stopped"), "{}", call.stderr);
}

/// `ps` lists the container's processes by host PID: its init and the
/// command.
#[test]
#[ignore = "needs root"]
fn ps_lists_the_containers_processes() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 300");
    assert_eq!(runtime.create("listed", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "listed"]).code, Some(0));
    let init = runtime.state("listed").unwrap()["pid"].as_i64().unwrap();
    let json = eventually("two processes", || {
        let call = runtime.call(&["ps", "--format", "json", "listed"]);
        let pids: Vec<i64> = serde_json::from_str(&call.stdout).ok()?;
        (pids.len() == 2).then_some(pids)
    });
    assert!(json.contains(&init), "{json:?}");
    let table = runtime.call(&["ps", "listed"]).stdout;
    assert!(table.starts_with("     PID      UID  COMMAND"), "{table}");
    assert!(table.contains("sleep 300"), "{table}");
}

/// `events --stats`: the container's use once, as runc's JSON.
#[test]
#[ignore = "needs root"]
fn events_stats_are_runcs_json() {
    let runtime = Runtime::new();
    let bundle = bundle_running("i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done; exec sleep 300");
    assert_eq!(runtime.create("measured", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "measured"]).code, Some(0));
    std::thread::sleep(Duration::from_millis(300));
    let call = runtime.call(&["events", "--stats", "measured"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let event: Value = serde_json::from_str(call.stdout.trim()).unwrap();
    assert_eq!(
        (event["type"].as_str(), event["id"].as_str()),
        (Some("stats"), Some("measured"))
    );
    let data = &event["data"];
    assert!(
        data["cpu"]["usage"]["total"].as_u64().unwrap() > 0,
        "{data}"
    );
    assert!(
        data["memory"]["usage"]["usage"].as_u64().unwrap() > 0,
        "{data}"
    );
    assert_eq!(data["pids"]["current"].as_u64(), Some(2), "{data}");
}

/// `events` streams the use, and an `oom` line when the OOM killer kills a
/// process in the container; it ends with the container.
#[test]
#[ignore = "needs root"]
fn events_report_an_oom_kill() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["resources"] = json!({"memory": {"limit": 33_554_432, "swap": 33_554_432}});
    config["process"]["args"] = json!([
        "sh",
        "-c",
        "sleep 0.5; sh -c 'x=$(head -c 200m /dev/urandom)'; exec sleep 300"
    ]);
    bundle.write(&config);
    assert_eq!(runtime.create("watched", &bundle, &[]).code, Some(0));
    let mut events = Command::new(RUNTIME)
        .arg("--root")
        .arg(&runtime.root)
        .args(["events", "--interval", "100ms", "watched"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(events.stdout.take().unwrap()));
    // Its first tick counts the OOM kills so far: started after it, or a
    // kill that comes first (a slow `events` on a busy machine) is counted
    // as one from before, and never reported.
    assert!(matches!(lines.next(), Some(Ok(_))), "no first tick");
    assert_eq!(runtime.call(&["start", "watched"]).code, Some(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_oom = false;
    while Instant::now() < deadline {
        let Some(Ok(line)) = lines.next() else { break };
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["type"] == "oom" {
            saw_oom = true;
            break;
        }
    }
    assert!(saw_oom, "no oom event");
    // Deleted, not only stopped, between two ticks: the stream still just
    // ends.
    assert_eq!(
        runtime.call(&["delete", "--force", "watched"]).code,
        Some(0)
    );
    let status = events.wait().unwrap();
    assert!(status.success(), "events ends with the container: {status}");
}

/// A bundle's `process.oomScoreAdj` is the command's.
#[test]
#[ignore = "needs root"]
fn the_oom_score_adjustment_is_the_bundles() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["oomScoreAdj"] = json!(500);
    config["process"]["args"] = json!(["cat", "/proc/self/oom_score_adj"]);
    bundle.write(&config);
    assert_eq!(runtime.create("scored", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "scored"]).code, Some(0));
    let _ = runtime.exit_record("scored");
    assert_eq!(runtime.output_of("scored"), "500\n");
}

/// What a bundle asks for that is ignored is said, as by `shipyard run
/// --bundle`; warnings and errors both reach `--log`.
#[test]
#[ignore = "needs root"]
fn warnings_and_errors_reach_stderr_and_the_log() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["mounts"] = json!([
        {"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["relatime"]}
    ]);
    bundle.write(&config);
    let log = runtime.root.with_extension("log");
    let log_flags = ["--log", log.to_str().unwrap(), "--log-format", "json"];
    let call = runtime.create("warned", &bundle, &log_flags);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(
        call.stderr.contains("shipyard-runtime: warning: ") && call.stderr.contains("relatime"),
        "{}",
        call.stderr
    );
    // The same ID again: an error.
    let call = runtime.create("warned", &bundle, &log_flags);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    let lines: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let _ = std::fs::remove_file(&log);
    // The first create's warning, then the second's error (a create that
    // fails says only why).
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["level"], "warning");
    assert!(lines[0]["msg"].as_str().unwrap().contains("relatime"));
    assert_eq!(lines[1]["level"], "error");
    assert!(lines[1]["msg"].as_str().unwrap().contains("exists"));
}

// --- Hooks ------------------------------------------------------------------------

/// `bundle`'s config, running `sh -c SCRIPT`, with
/// [`Bundle::recording_hooks`] logging to `runtime`'s hooks log (emptied),
/// whose path comes along.
fn hooked(runtime: &Runtime, bundle: &Bundle, script: &str) -> (Value, PathBuf) {
    let log = runtime.root.with_extension("hooks");
    let _ = std::fs::remove_file(&log);
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    config["hooks"] = bundle.recording_hooks(&log);
    (config, log)
}

/// The lines of `log` (none if there's none yet).
fn lines_of(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every phase runs when and where the spec says, with the state it says:
/// `prestart` and `createRuntime` from the runtime, once the container's
/// mounts are made but before its root is switched; `createContainer` in
/// the container's namespaces; `startContainer` inside its root, before the
/// program; `poststart` once it was let go; `poststop` once it's deleted.
#[test]
#[ignore = "needs root"]
fn hooks_run_at_their_phases() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let (config, log) = hooked(&runtime, &bundle, "cat /tmp/hooks");
    bundle.write(&config);
    let call = runtime.create("hooked", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let pid = runtime.state("hooked").unwrap()["pid"].as_i64().unwrap();
    let uts = |pid: &str| std::fs::read_link(format!("/proc/{pid}/ns/uts")).unwrap();
    assert_ne!(
        uts(&pid.to_string()),
        uts("self"),
        "a UTS namespace of its own"
    );
    // Ours, which `create` passed on: not the 0 the setup works with.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let umask = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .unwrap()
        .trim();
    let creating = format!("id=hooked status=creating pid={pid}");
    // Inside the container's namespaces, the PID is the one seen there.
    let inside = "id=hooked status=creating pid=1";
    assert_eq!(
        lines_of(&log),
        [
            format!("prestart {creating} "),
            format!("createRuntime {creating} mounted"),
            format!(
                "createContainer {inside} uts={} umask={umask}",
                uts(&pid.to_string()).display()
            ),
        ]
    );
    assert_eq!(runtime.call(&["start", "hooked"]).code, Some(0));
    runtime.wait_for("hooked", "stopped");
    let out = runtime.output_of("hooked");
    let state = Bundle::checked_start_container(&out);
    assert!(state.contains(r#""status":"created""#), "{out}");
    assert!(state.contains(r#""pid":1,"#), "the PID inside: {out}");
    assert_eq!(
        lines_of(&log)[3],
        format!("poststart id=hooked status=running pid={pid} ")
    );
    assert_eq!(runtime.call(&["delete", "hooked"]).code, Some(0));
    assert_eq!(lines_of(&log)[4], "poststop id=hooked status=stopped pid= ");
    assert_eq!(lines_of(&log).len(), 5);
}

/// A hook that fails while the container is set up fails `create`, naming
/// it; nothing is left, and `poststop` still runs (the spec's "continue at
/// destroy"). The runtime's own, and the one inside.
#[test]
#[ignore = "needs root"]
fn a_failing_create_hook_fails_create() {
    let runtime = Runtime::new();
    for (phase, expected) in [
        (
            "createRuntime",
            "the createRuntime hook /bin/sh (hooks.createRuntime[0]) exited with code 3",
        ),
        (
            "createContainer",
            "the createContainer hook /bin/sh (hooks.createContainer[0]) exited with code 3",
        ),
    ] {
        let bundle = Bundle::new();
        let (mut config, log) = hooked(&runtime, &bundle, "echo ran");
        config["hooks"][phase] = json!([{"path": "/bin/sh", "args": ["sh", "-c", "exit 3"]}]);
        bundle.write(&config);
        let call = runtime.create("failing", &bundle, &[]);
        assert_eq!(call.code, Some(1), "{phase}: {}", call.stderr);
        assert!(call.stderr.contains(expected), "{phase}: {}", call.stderr);
        assert!(!runtime.root.join("failing").exists(), "{phase}");
        assert!(!cgroup_of("failing").exists(), "{phase}");
        assert_eq!(runtime.output_of("failing"), "", "{phase}: nothing ran");
        let lines = lines_of(&log);
        assert!(
            lines.last().unwrap().starts_with("poststop "),
            "{phase}: {lines:?}"
        );
    }
}

/// A hook past its timeout is killed, and fails its phase: here `create`,
/// at once rather than after the hook's 30 s.
#[test]
#[ignore = "needs root"]
fn a_hook_past_its_timeout_fails() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["hooks"] = json!({"createRuntime": [
        {"path": "/bin/sh", "args": ["sh", "-c", "sleep 30"], "timeout": 1}
    ]});
    bundle.write(&config);
    let started = Instant::now();
    let call = runtime.create("slow", &bundle, &[]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        call.stderr
            .contains("didn't finish within its timeout (1 s)"),
        "{}",
        call.stderr
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!runtime.root.join("slow").exists());
}

/// `startContainer` fails after `start` has let the container go: the
/// container ends without running its program (125), saying why on its
/// stderr. `poststart` and `poststop` failing are only warnings.
#[test]
#[ignore = "needs root"]
fn failures_after_create_end_or_warn() {
    let runtime = Runtime::new();
    let bundle = bundle_running("echo ran");
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "echo ran"]);
    let fail =
        |code: i32| json!([{"path": "/bin/sh", "args": ["sh", "-c", format!("exit {code}")]}]);
    config["hooks"] = json!({"startContainer": fail(4)});
    bundle.write(&config);
    assert_eq!(runtime.create("unstarted", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "unstarted"]).code, Some(0));
    let ended = runtime.exit_record("unstarted");
    assert!(ended.contains("exit code 125"), "{ended}");
    assert_eq!(runtime.output_of("unstarted"), "", "the program never ran");
    let err = std::fs::read_to_string(runtime.output_dir().join("unstarted.err")).unwrap();
    assert!(
        err.contains(
            "the startContainer hook /bin/sh (hooks.startContainer[0]) exited with code 4"
        ),
        "{err}"
    );
    config["hooks"] = json!({"poststart": fail(5), "poststop": fail(6)});
    bundle.write(&config);
    assert_eq!(runtime.create("warned", &bundle, &[]).code, Some(0));
    let start = runtime.call(&["start", "warned"]);
    assert_eq!(start.code, Some(0), "{}", start.stderr);
    assert!(
        start.stderr.contains("warning: the poststart hook"),
        "{}",
        start.stderr
    );
    runtime.wait_for("warned", "stopped");
    assert_eq!(runtime.output_of("warned"), "ran\n");
    let delete = runtime.call(&["delete", "warned"]);
    assert_eq!(delete.code, Some(0), "{}", delete.stderr);
    assert!(
        delete.stderr.contains("warning: the poststop hook"),
        "{}",
        delete.stderr
    );
    assert!(
        delete.stderr.contains("exited with code 6"),
        "{}",
        delete.stderr
    );
}

/// Hooks run what `create` read: a bundle changed afterwards changes
/// nothing. And a hook may ask for the container's state (its lock is free).
#[test]
#[ignore = "needs root"]
fn hooks_are_what_create_read() {
    let runtime = Runtime::new();
    let bundle = bundle_running("echo ran");
    let out = runtime.root.with_extension("state");
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "echo ran"]);
    let ask = format!(
        "{RUNTIME} --root {} state asked > {}",
        runtime.root.display(),
        out.display()
    );
    config["hooks"] = json!({"poststart": [{"path": "/bin/sh", "args": ["sh", "-c", ask]}]});
    bundle.write(&config);
    assert_eq!(runtime.create("asked", &bundle, &[]).code, Some(0));
    config["hooks"] = json!({"poststart": [{"path": "/bin/sh", "args": ["sh", "-c", "exit 1"]}]});
    bundle.write(&config);
    let start = runtime.call(&["start", "asked"]);
    assert_eq!((start.code, start.stderr.as_str()), (Some(0), ""));
    let state: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let _ = std::fs::remove_file(&out);
    assert_eq!(state["id"], "asked");
}

/// `poststart` runs while the program does: `start` has let the init go by
/// then, so a hook may wait for what the program does.
#[test]
#[ignore = "needs root"]
fn poststart_runs_while_the_program_does() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let out = runtime.output_dir().join("going.out");
    let wait = format!(
        "for i in $(seq 50); do grep -q ran {} && exit 0; sleep 0.1; done; exit 1",
        out.display()
    );
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "echo ran; sleep 30"]);
    config["hooks"] = json!({"poststart": [
        {"path": "/bin/sh", "args": ["sh", "-c", wait], "timeout": 10}
    ]});
    bundle.write(&config);
    assert_eq!(runtime.create("going", &bundle, &[]).code, Some(0));
    let start = runtime.call(&["start", "going"]);
    assert_eq!((start.code, start.stderr.as_str()), (Some(0), ""));
}

/// Deleting a container whose setup is held up in a hook (`createRuntime`,
/// run by its monitor) stops the hook too, before `poststop` runs: cleanup
/// never races setup. And `poststop` runs once, though `create` fails too.
#[test]
#[ignore = "needs root"]
fn deleting_during_a_hook_stops_it() {
    use std::os::unix::process::CommandExt as _;

    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let (mut config, log) = hooked(&runtime, &bundle, "echo ran");
    let pid_file = runtime.root.with_extension("hook-pid");
    let _ = std::fs::remove_file(&pid_file);
    let hang = format!("echo $$ > {}; exec sleep 300", pid_file.display());
    config["hooks"]["createRuntime"] = json!([{"path": "/bin/sh", "args": ["sh", "-c", hang]}]);
    bundle.write(&config);
    let mut create = Command::new(RUNTIME)
        .arg("--root")
        .arg(&runtime.root)
        .args(["create", "--bundle", bundle.arg(), "held"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let hook: i64 = eventually("the hook running", || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    });
    let _ = std::fs::remove_file(&pid_file);
    assert_eq!(runtime.status("held"), "creating");
    let call = runtime.call(&["delete", "held"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    eventually("the hook gone", || (!alive(hook)).then_some(()));
    assert_ne!(create.wait().unwrap().code(), Some(0));
    let lines = lines_of(&log);
    let poststops = lines
        .iter()
        .filter(|line| line.starts_with("poststop "))
        .count();
    assert_eq!(poststops, 1, "{lines:?}");
}

/// A hook gets its standard input, output and error, and nothing else of
/// whoever runs it (the monitor's answer pipe, say); it runs in the bundle,
/// as with runc, or at `/` inside the container.
#[test]
#[ignore = "needs root"]
fn hooks_get_only_their_stdio_and_run_in_the_bundle() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let seen = runtime.root.with_extension("seen");
    let _ = std::fs::remove_dir_all(&seen);
    std::fs::create_dir_all(&seen).unwrap();
    // `ls` lists its own descriptors, with what they are: the three it was
    // given, and the one it reads the list from.
    let look = |dir: &str| {
        json!([{
            "path": "/bin/sh",
            "args": ["sh", "-c", format!("ls -l /proc/self/fd > {dir}/$0.fds; pwd > {dir}/$0.cwd"), "PHASE"],
            "env": ["PATH=/usr/bin:/bin"]
        }])
    };
    let host = seen.display().to_string();
    let mut hooks = json!({});
    for phase in [
        "prestart",
        "createRuntime",
        "createContainer",
        "poststart",
        "poststop",
    ] {
        let mut hook = look(&host);
        hook[0]["args"][3] = json!(phase);
        hooks[phase] = hook;
    }
    hooks["startContainer"] = look("/tmp");
    hooks["startContainer"][0]["args"][3] = json!("startContainer");
    let mut config = bundle.config();
    config["hooks"] = hooks;
    bundle.write(&config);
    assert_eq!(runtime.create("looked", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "looked"]).code, Some(0));
    runtime.wait_for("looked", "stopped");
    assert_eq!(runtime.call(&["delete", "looked"]).code, Some(0));
    let bundle_dir = std::fs::canonicalize(bundle.path()).unwrap();
    let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
    // The descriptors' numbers. Rosetta, the x86-64 emulator, keeps the
    // binary of each program it runs open, and they pile up down a chain of
    // `exec`s (checked: shipyard's, `sh`'s, `ls`'s): not Shipyard's to close,
    // so under it those are left out. A leaked pipe still shows.
    let fds = |listed: String| {
        listed
            .lines()
            .filter_map(|line| {
                let (left, target) = line.split_once(" -> ")?;
                let program = std::fs::metadata(target).is_ok_and(|meta| {
                    meta.is_file()
                        && std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
                });
                (!(under_rosetta() && program)).then(|| left.rsplit(' ').next().unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    for phase in [
        "prestart",
        "createRuntime",
        "createContainer",
        "poststart",
        "poststop",
    ] {
        assert_eq!(
            fds(read(seen.join(format!("{phase}.fds")))),
            "0 1 2 3",
            "{phase}"
        );
        assert_eq!(
            read(seen.join(format!("{phase}.cwd"))).trim(),
            bundle_dir.to_str().unwrap(),
            "{phase}"
        );
    }
    let inside = |name: &str| {
        read(
            bundle
                .root
                .host_path(&format!("/tmp/startContainer.{name}")),
        )
    };
    assert_eq!(fds(inside("fds")), "0 1 2 3");
    assert_eq!(inside("cwd").trim(), "/");
    let _ = std::fs::remove_dir_all(&seen);
}

/// The parent of process `pid`, from `/proc/PID/stat`.
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Polls `found` until it finds something, for up to 30 s (generous: under
/// x86-64 emulation, setting a container up takes a while).
fn eventually<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(it) = found() {
            return it;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A setup that never finishes can still be deleted: `delete` stops it
/// through its monitor, and the container dies with the monitor. (The setup
/// is held up by a `createRuntime` hook that doesn't end, which the monitor
/// waits for, with the init stopped before `pivot_root`.)
#[test]
#[ignore = "needs root"]
fn deleting_a_container_whose_setup_hangs() {
    use std::os::unix::process::CommandExt as _;

    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["hooks"] = json!({"createRuntime": [{"path": "/bin/sleep", "args": ["sleep", "300"]}]});
    bundle.write(&config);
    let mut create = Command::new(RUNTIME)
        .arg("--root")
        .arg(&runtime.root)
        .args(["create", "--bundle", bundle.arg(), "hung"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let monitor = eventually("the monitor recorded", || {
        let state = std::fs::read(runtime.root.join("hung").join("state.json")).ok()?;
        let record: Value = serde_json::from_slice(&state).ok()?;
        i32::try_from(record["monitor"][0].as_i64()?).ok()
    });
    // The monitor's children are the init and the hook: the init is the one
    // that says what it is.
    let init = eventually("the init waiting", || {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&pid| parent_of(pid) == Some(monitor))
            .find(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|comm| comm.trim() == "shipyard-init")
            })
    });
    assert_eq!(runtime.status("hung"), "creating");
    let call = runtime.call(&["delete", "hung"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.status("hung"), "gone");
    eventually("the monitor and the init gone", || {
        (!alive(i64::from(monitor)) && !alive(i64::from(init))).then_some(())
    });
    // `create` learns its monitor died, and fails.
    assert_ne!(create.wait().unwrap().code(), Some(0));
}

/// A `start` that dies after opening the gate, before reading it, starts
/// nothing: the program runs once a `start` has taken the gate's byte, and
/// gets no SIGPIPE from the init's write to the dead reader.
#[test]
#[ignore = "needs root"]
fn a_start_that_dies_at_the_gate_starts_nothing() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let runtime = Runtime::new();
    let bundle = bundle_running("echo ran");
    assert_eq!(runtime.create("gated", &bundle, &[]).code, Some(0));
    let fifo = runtime.root.join("gated").join("exec.fifo");
    for held in [Duration::from_millis(200), Duration::ZERO] {
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        std::thread::sleep(held);
        drop(reader);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(runtime.status("gated"), "created");
        assert_eq!(runtime.output_of("gated"), "");
    }
    let call = runtime.call(&["start", "gated"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    runtime.wait_for("gated", "stopped");
    assert_eq!(runtime.output_of("gated"), "ran\n");
    // `stopped` comes with the init's end; the record a little after it.
    let ended = runtime.exit_record("gated");
    assert!(ended.contains("exit code 0"), "{ended}");
}

/// `create` started with its standard input, output and error closed (by
/// a careless daemon, say): the descriptors it passes to the monitor must
/// not take those numbers, or the monitor refuses them and the container
/// gets them as its stdio. (They can't: Rust's startup opens `/dev/null`
/// on any of the three that's closed. This keeps it so.)
#[test]
#[ignore = "needs root"]
fn create_works_with_its_stdio_closed() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    let status = Command::new("sh")
        .arg("-c")
        .arg(r#"exec 0<&- 1>&- 2>&-; exec "$0" --root "$1" create --bundle "$2" closed"#)
        .arg(RUNTIME)
        .arg(&runtime.root)
        .arg(bundle.arg())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(runtime.status("closed"), "created");
    assert_eq!(runtime.call(&["start", "closed"]).code, Some(0));
    assert_eq!(runtime.status("closed"), "running");
}

/// Deleted and made again at once, a container is the new one: nothing of
/// the old one's (its monitor recording how it ended) reaches the new
/// directory.
///
/// Made certain, not left to timing: the old monitor is stopped before the
/// delete, so it records the old container's end only once the new
/// container exists.
#[test]
#[ignore = "needs root"]
fn a_container_made_again_at_once_is_the_new_one() {
    use nix::sys::signal::{Signal, kill};
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    assert_eq!(runtime.create("again", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "again"]).code, Some(0));
    let record: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("again/state.json")).unwrap())
            .unwrap();
    let old_monitor = i32::try_from(record["monitor"][0].as_i64().unwrap()).unwrap();
    let old_monitor = nix::unistd::Pid::from_raw(old_monitor);
    kill(old_monitor, Signal::SIGSTOP).unwrap();
    let call = runtime.call(&["delete", "--force", "again"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let call = runtime.create("again", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    kill(old_monitor, Signal::SIGCONT).unwrap();
    eventually("the old monitor done", || {
        (!alive(i64::from(old_monitor.as_raw()))).then_some(())
    });
    assert_eq!(runtime.status("again"), "created");
    // The old monitor didn't take the new container's cgroup (same path).
    let pid = runtime.state("again").unwrap()["pid"].as_i64().unwrap();
    assert_eq!(cgroup_procs(&cgroup_of("again")), [pid]);
}

/// A runtime container gets its bundle's capabilities and `noNewPrivileges`,
/// as `shipyard run --bundle` does: through the monitor and the start gate
/// too.
#[test]
#[ignore = "needs root"]
fn a_containers_capabilities_are_its_bundles() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "cat /proc/self/status"]);
    config["process"]["capabilities"] = json!({
        "bounding": ["CAP_KILL"], "effective": ["CAP_KILL"], "permitted": ["CAP_KILL"]
    });
    config["process"]["noNewPrivileges"] = json!(true);
    bundle.write(&config);
    assert_eq!(runtime.create("capped", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "capped"]).code, Some(0));
    runtime.wait_for("capped", "stopped");
    let status = runtime.output_of("capped");
    for expected in [
        "CapEff:\t0000000000000020",
        "CapBnd:\t0000000000000020",
        "NoNewPrivs:\t1",
    ] {
        assert!(status.contains(expected), "{expected}: {status}");
    }
}

/// A bundle's seccomp profile, through the runtime too.
#[test]
#[ignore = "needs root"]
fn a_containers_seccomp_is_its_bundles() {
    if shipyard_test_support::under_rosetta() {
        eprintln!("under Rosetta emulation, where no seccomp filter can be installed; skipping");
        return;
    }
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "cat /proc/self/status"]);
    config["linux"]["seccomp"] = json!({"defaultAction": "SCMP_ACT_ALLOW"});
    bundle.write(&config);
    assert_eq!(runtime.create("filtered", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "filtered"]).code, Some(0));
    runtime.wait_for("filtered", "stopped");
    let status = runtime.output_of("filtered");
    assert!(status.contains("Seccomp:\t2"), "{status}");
}

#[test]
#[ignore = "needs root"]
fn deleting_a_running_container_needs_force() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    assert_eq!(runtime.create("busy", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "busy"]).code, Some(0));
    let call = runtime.call(&["delete", "busy"]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("is running"), "{}", call.stderr);
    let pid = runtime.state("busy").unwrap()["pid"].as_i64().unwrap();
    assert_eq!(runtime.call(&["delete", "--force", "busy"]).code, Some(0));
    assert_eq!(runtime.status("busy"), "gone");
    assert!(!alive(pid));
}

/// A created container is killed by `delete`, never having run.
#[test]
#[ignore = "needs root"]
fn deleting_a_created_container() {
    let runtime = Runtime::new();
    let bundle = bundle_running("echo ran");
    assert_eq!(runtime.create("idle", &bundle, &[]).code, Some(0));
    let pid = runtime.state("idle").unwrap()["pid"].as_i64().unwrap();
    assert_eq!(runtime.call(&["delete", "idle"]).code, Some(0));
    assert!(!alive(pid));
    assert_eq!(runtime.output_of("idle"), "");
}

/// The monitor keeps the container: kill it, and the container goes too.
#[test]
#[ignore = "needs root"]
fn the_container_dies_with_its_monitor() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    assert_eq!(runtime.create("kept", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "kept"]).code, Some(0));
    let pid = runtime.state("kept").unwrap()["pid"].as_i64().unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let monitor: i32 = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let comm = std::fs::read_to_string(format!("/proc/{monitor}/cmdline")).unwrap();
    // The hidden argument, not the first word: under x86-64 emulation the
    // emulator puts the binary's path first in `cmdline`.
    assert!(
        comm.split('\0').any(|arg| arg == "__shipyard-monitor"),
        "{comm:?}"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(monitor),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    runtime.wait_for("kept", "stopped");
    assert!(!alive(pid));
}

/// The monitor runs in a session of its own: the creator's going away
/// (here, its whole process group being killed) doesn't touch it.
#[test]
#[ignore = "needs root"]
fn the_container_outlives_its_creator() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    // `create` from a shell in its own process group, then that group killed.
    let script = format!(
        "{RUNTIME} --root {} create --bundle {} survivor >/dev/null 2>&1; kill -KILL 0",
        runtime.root.display(),
        bundle.arg()
    );
    let _ = Command::new("setsid").args(["sh", "-c", &script]).status();
    runtime.wait_for("survivor", "created");
    assert_eq!(runtime.call(&["start", "survivor"]).code, Some(0));
    assert_eq!(runtime.status("survivor"), "running");
}

/// The monitor leads a session of its own (and so a process group): the
/// creator's terminal closing, or its session being signalled, can't reach
/// it. (`setsid` fails for a process-group leader, so it must come before
/// anything makes the monitor one.)
#[test]
#[ignore = "needs root"]
fn the_monitor_has_a_session_of_its_own() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    assert_eq!(runtime.create("alone", &bundle, &[]).code, Some(0));
    let record: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("alone/state.json")).unwrap())
            .unwrap();
    let monitor = record["monitor"][0].as_i64().unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{monitor}/stat")).unwrap();
    // After the name: state, parent, process group, session.
    let fields: Vec<i64> = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .skip(1)
        .take(3)
        .map(|field| field.parse().unwrap())
        .collect();
    assert_eq!(fields[1..], [monitor, monitor], "group and session: {stat}");
}

#[test]
fn the_runtime_binary_is_its_own_monitor_and_init() {
    // Started by hand with the hidden arguments, it refuses politely.
    for hidden in ["__shipyard-monitor", "__shipyard-init"] {
        let output = Command::new(RUNTIME).arg(hidden).output().unwrap();
        assert_eq!(output.status.code(), Some(125), "{hidden}");
    }
}

/// A program that can't run once started: nobody is left to report to but
/// the container's own stderr, and its exit says why (127).
#[test]
#[ignore = "needs root"]
fn a_program_that_cant_run_says_so_on_the_containers_stderr() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["no-such-program"]);
    bundle.write(&config);
    assert_eq!(runtime.create("missing", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "missing"]).code, Some(0));
    runtime.wait_for("missing", "stopped");
    let recorded = runtime.exit_record("missing");
    assert!(recorded.contains("exit code 127"), "{recorded}");
    let stderr = std::fs::read_to_string(runtime.output_dir().join("missing.err")).unwrap();
    assert!(
        stderr.contains("no-such-program: command not found"),
        "{stderr:?}"
    );
}

/// A descriptor the caller leaves open (not close-on-exec) is let go by the
/// monitor: a reader waiting for its end isn't held up by the container.
#[test]
#[ignore = "needs root"]
fn the_monitor_keeps_nothing_of_the_callers() {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    let (read, write) = nix::unistd::pipe().unwrap();
    // Inherited across `create`'s exec, as a careless caller's would be.
    nix::fcntl::fcntl(
        &write,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .unwrap();
    let fd = write.as_raw_fd();
    let call = runtime.create("leaky", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    drop(write);
    // `create` has exited; only the monitor (or the container) could hold
    // the pipe now. Read until end-of-file, with a deadline.
    let (done, finished) = std::sync::mpsc::channel();
    let mut reader = std::fs::File::from(read);
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        let _ = done.send(());
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(5)).is_ok(),
        "something still holds the caller's fd {fd}"
    );
}

/// A state root someone else could change is refused.
#[test]
#[ignore = "needs root"]
fn an_unsafe_state_root_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let runtime = Runtime::new();
    let bundle = bundle_running("true");
    std::fs::create_dir_all(&runtime.root).unwrap();
    std::fs::set_permissions(&runtime.root, std::fs::Permissions::from_mode(0o777)).unwrap();
    let call = runtime.create("c1", &bundle, &[]);
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr.contains("others can write to it"),
        "{}",
        call.stderr
    );
    std::fs::set_permissions(&runtime.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A container directory that's a symlink is not a container.
    std::os::unix::fs::symlink("/etc", runtime.root.join("victim")).unwrap();
    let call = runtime.call(&["state", "victim"]);
    assert!(
        call.stderr.contains("no container with ID victim"),
        "{}",
        call.stderr
    );
}
