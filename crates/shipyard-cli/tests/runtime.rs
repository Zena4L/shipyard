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
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::console::{Console, ConsoleSocket};
use shipyard_test_support::{
    Rootless, Shipyard, TIMEOUT, delegated_cgroup, rootless, under_rosetta,
};

const RUNTIME: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard-runtime"));

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
        let output = Command::new(RUNTIME.path())
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
        self.create_with(id, bundle, extra, Command::new(RUNTIME.path()))
    }

    /// `create` like [`Runtime::create`], run by `command` (the runtime,
    /// set up as a test needs it).
    fn create_with(&self, id: &str, bundle: &Bundle, extra: &[&str], mut command: Command) -> Call {
        std::fs::create_dir_all(self.output_dir()).unwrap();
        // (An ID with a `/` is refused, but its file name must still work.)
        let name = id.replace('/', "_");
        let out = self.output_dir().join(format!("{name}.out"));
        let err = self.output_dir().join(format!("{name}.err"));
        let mut create = command
            .arg("--root")
            .arg(&self.root)
            .args(["create", "--bundle", bundle.arg()])
            .args(extra)
            .arg(id)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&out).unwrap())
            .stderr(std::fs::File::create(&err).unwrap())
            .spawn()
            .unwrap();
        // `create` returns once the container waits at its gate: one that
        // waits for something else (an answer on its console socket) hangs.
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = create.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = create.kill();
                let _ = create.wait();
                panic!("create {id} hung");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Call {
            code: status.code(),
            stdout: std::fs::read_to_string(&out).unwrap_or_default(),
            stderr: std::fs::read_to_string(&err).unwrap_or_default(),
        }
    }

    /// A log directory for container `id` that doesn't exist yet (`create
    /// --log-dir` makes it).
    fn log_dir(&self, id: &str) -> PathBuf {
        std::fs::create_dir_all(self.output_dir()).unwrap();
        self.output_dir().join(format!("{id}-logs"))
    }

    /// `create --log-dir` (and `extra`); returns the call and the directory.
    fn create_logged(&self, id: &str, bundle: &Bundle, extra: &[&str]) -> (Call, PathBuf) {
        let dir = self.log_dir(id);
        let mut args = vec!["--log-dir", dir.to_str().unwrap()];
        args.extend_from_slice(extra);
        (self.create(id, bundle, &args), dir)
    }

    /// Creates, starts and waits for container `id` (logging), then returns
    /// `logs ID ARGS…`.
    fn run_logged(&self, id: &str, bundle: &Bundle, args: &[&str]) -> Call {
        let (created, _) = self.create_logged(id, bundle, &[]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        let started = self.call(&["start", id]);
        assert_eq!(started.code, Some(0), "{}", started.stderr);
        self.wait_for(id, "stopped");
        let mut all = vec!["logs"];
        all.extend_from_slice(args);
        all.push(id);
        self.call(&all)
    }

    /// `shipyard-runtime --root ROOT ARGS…`, which must end within `limit`
    /// (killed and failed otherwise).
    fn call_within(&self, args: &[&str], limit: Duration) -> Call {
        let mut child = Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + limit;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{args:?} didn't end within {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        Call {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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
    /// 5 s for the record): its words. The record is this Shipyard's
    /// version, and its code is the one the words give.
    fn exit_record(&self, id: &str) -> String {
        let record = self.exit_json(id);
        assert_eq!(record["version"], 2, "{record}");
        let description = record["description"].as_str().unwrap().to_owned();
        if let Some(code) = record["code"].as_i64() {
            assert!(
                description.contains(&format!("(exit code {code})")),
                "{record}"
            );
        }
        description
    }

    /// Container `id`'s exit record, as written (waiting up to 5 s for it).
    fn exit_json(&self, id: &str) -> Value {
        let exit = self.root.join(id).join("exit");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exit.exists() {
            assert!(Instant::now() < deadline, "no exit recorded for {id}");
            std::thread::sleep(Duration::from_millis(20));
        }
        serde_json::from_slice(&std::fs::read(exit).unwrap()).unwrap()
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

/// A bundle running `sh -c SCRIPT` on a terminal of its own
/// (`process.terminal`), `consoleSize` big if given (rows, columns).
fn terminal_bundle(script: &str, size: Option<(u16, u16)>) -> Bundle {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    config["process"]["terminal"] = json!(true);
    if let Some((height, width)) = size {
        config["process"]["consoleSize"] = json!({"height": height, "width": width});
    }
    bundle.write(&config);
    bundle
}

/// `create`s container `id` from `bundle` with its terminal sent to a new
/// console socket, and receives it.
fn create_on_console(runtime: &Runtime, id: &str, bundle: &Bundle) -> Console {
    let socket = ConsoleSocket::new();
    let call = runtime.create(
        id,
        bundle,
        &["--console-socket", socket.path().to_str().unwrap()],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    socket.receive()
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
        let mut command = Command::new(RUNTIME.path());
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

// --- Terminals: console sockets, as any user -------------------------------------

/// As runc: a bundle that asks for a terminal needs a console socket to send
/// it to, and a console socket needs a bundle that asks for one. Nothing is
/// left either way.
#[test]
fn a_terminal_and_a_console_socket_go_together() {
    let runtime = Runtime::new();
    let call = runtime.create("tty", &terminal_bundle("true", None), &[]);
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr
            .contains("there's no console socket to send it to"),
        "{}",
        call.stderr
    );
    let socket = ConsoleSocket::new();
    let call = runtime.create(
        "notty",
        &bundle_running("true"),
        &["--console-socket", socket.path().to_str().unwrap()],
    );
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr.contains("the bundle asks for no terminal"),
        "{}",
        call.stderr
    );
    assert_eq!(runtime.state("tty"), None);
    assert_eq!(runtime.state("notty"), None);
}

/// `--console-socket` must name a socket: a plain file (or nothing) is
/// refused, naming the path.
#[test]
fn a_console_socket_that_isnt_one_is_refused() {
    let runtime = Runtime::new();
    let bundle = terminal_bundle("true", None);
    let file = bundle.path().join("not-a-socket");
    std::fs::write(&file, "").unwrap();
    let call = runtime.create(
        "tty",
        &bundle,
        &["--console-socket", file.to_str().unwrap()],
    );
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr.contains(&format!(
            "--console-socket {}: not a socket",
            file.display()
        )),
        "{}",
        call.stderr
    );
    let missing = bundle.path().join("missing");
    let call = runtime.create(
        "tty",
        &bundle,
        &["--console-socket", missing.to_str().unwrap()],
    );
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr
            .contains(&format!("--console-socket {}", missing.display())),
        "{}",
        call.stderr
    );
    assert_eq!(runtime.state("tty"), None);
}

/// A rootless container gets its terminal on its console socket too, and
/// types and prints through it.
#[test]
fn a_rootless_container_gets_its_console() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = terminal_bundle("tty; read line; echo got-$line", None);
    let mut console = create_on_console(&runtime, "rootless-tty", &bundle);
    assert_eq!(console.name, b"/dev/ptmx");
    assert_eq!(runtime.call(&["start", "rootless-tty"]).code, Some(0));
    console.expect("/dev/pts/0");
    console.type_keys(b"hello\n");
    console.expect("got-hello");
    runtime.wait_for("rootless-tty", "stopped");
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
    let mut command = Command::new(RUNTIME.path());
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
    let record = runtime.exit_json("exits");
    assert_eq!(record["code"], 3, "{record}");
    assert!(record.get("oom").is_none(), "{record}");
    let finished = record["finished"]["secs_since_epoch"].as_u64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(finished <= now && now - finished < 60, "{record}");
    let state: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("exits/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["version"], 1, "{state}");
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
    // Its logs too: the directory it made, empty, goes.
    let dir = runtime.log_dir("unpidded-logs");
    let call = runtime.create(
        "unpidded-logs",
        &bundle,
        &[
            "--pid-file",
            "/no/such/dir/c.pid",
            "--log-dir",
            dir.to_str().unwrap(),
        ],
    );
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(!dir.exists(), "its logs are left: {}", call.stderr);
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
    // The marker is the caller's alone.
    let killed = std::fs::symlink_metadata(runtime.root.join("survivor/killed")).unwrap();
    assert!(killed.is_file());
    assert_eq!(killed.mode() & 0o777, 0o600);
}

/// Whether `path` is locked (`flock`) by someone: a fresh open of it can't
/// take the lock.
fn locked(path: &Path) -> bool {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path).unwrap();
    // SAFETY: `flock` on a descriptor this function owns.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !taken
}

/// A file locked here (`flock`, exclusive), and the command that passes it
/// on as descriptor 9 (for `--hold-fd 9`).
fn held_file(path: &Path) -> (std::fs::File, Command) {
    held_file_at(path, 9)
}

/// [`held_file`], passed on as descriptor `at`.
fn held_file_at(path: &Path, at: i32) -> (std::fs::File, Command) {
    let (mut files, command) = held_files(&[(path, at)]);
    (files.remove(0), command)
}

/// Files locked here (`flock`, exclusive), and the command that passes each
/// on at its number.
fn held_files(files: &[(&Path, i32)]) -> (Vec<std::fs::File>, Command) {
    use std::os::fd::AsRawFd as _;
    let opened: Vec<std::fs::File> = files.iter().map(|(path, _)| locked_file(path)).collect();
    let moves: Vec<(i32, i32)> = opened
        .iter()
        .zip(files)
        .map(|(file, (_, at))| (file.as_raw_fd(), *at))
        .collect();
    let mut command = Command::new(RUNTIME.path());
    // Where each copy landed: made before the fork, written after it.
    let mut copies = vec![0; moves.len()];
    // SAFETY: `fcntl` and `dup2` are async-signal-safe, on lists made before
    // the fork. Each is copied out of the way first (200 and up: the lowest
    // free there, which needn't be 200 itself, with the test process's
    // other descriptors inherited), then put at its number, which leaves it
    // inheritable.
    unsafe {
        command.pre_exec(move || {
            for (copy, &(raw, _)) in copies.iter_mut().zip(&moves) {
                *copy = libc::fcntl(raw, libc::F_DUPFD, 200);
                if *copy == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            for (&copy, &(_, at)) in copies.iter().zip(&moves) {
                if libc::dup2(copy, at) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    (opened, command)
}

/// An exit command (a shell script) that records what it was given, a
/// `key=value` line each: how many holds `SHIPYARD_HOLDS` says, its
/// descriptors (a line of its own), their labels (`SHIPYARD_HOLD_LABELS`),
/// whether `lock` is still held from outside, the inodes of its
/// descriptors 3 and 4, and the exit record it was given; then lingers a
/// second.
fn recording_exit_command(dir: &Path, lock: &Path) -> (PathBuf, PathBuf) {
    let record = dir.join("exit-command.txt");
    let script = dir.join("exit-command.sh");
    std::fs::write(
        &script,
        format!(
            "{{ echo holds=$SHIPYARD_HOLDS; ls /proc/self/fd | tr '\\n' ' '; echo; \
             echo labels=$SHIPYARD_HOLD_LABELS; \
             if flock -n {lock} true; then echo lock=free; else echo lock=held; fi; \
             echo i3=$(stat -L -c %i /proc/self/fd/3 2>/dev/null); \
             echo i4=$(stat -L -c %i /proc/self/fd/4 2>/dev/null); \
             echo \"exit=$SHIPYARD_EXIT_RECORD\"; }} > {record}.tmp \
             && mv {record}.tmp {record}\nsleep 1\n",
            lock = lock.display(),
            record = record.display(),
        ),
    )
    .unwrap();
    (script, record)
}

/// The value after `key=` on a line of the exit command's record.
fn recorded<'a>(record: &'a str, key: &str) -> &'a str {
    record
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_default()
}

/// A lock handed to the monitor (`--hold-fd`) is held while the container
/// runs, though `create`'s caller has let its copy go; the container never
/// has it; once the container has ended, the exit command gets it at 3
/// (still held), and once that's done it's free.
#[test]
#[ignore = "needs root"]
fn a_hold_outlives_create_and_is_the_exit_commands() {
    use std::os::unix::fs::MetadataExt as _;
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let lock = runtime.output_dir().join("held.lock");
    let (script, record) = recording_exit_command(&runtime.output_dir(), &lock);
    let (file, command) = held_file(&lock);
    let bundle = bundle_running("exec sleep 300");
    // A hook the monitor runs (createRuntime) lists the inodes of its
    // descriptors: the hold mustn't be among them.
    let hook_fds = runtime.output_dir().join("hook-fds.txt");
    // A fresh config: its command set again (`bundle.config()` is the
    // default one, not what `bundle_running` wrote).
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "exec sleep 300"]);
    config["hooks"]["createRuntime"] = json!([{
        "path": "/bin/sh",
        "args": ["sh", "-c", format!(
            "for fd in /proc/$$/fd/*; do stat -L -c '%d %i' $fd 2>/dev/null; done > {}; true",
            hook_fds.display()
        )],
    }]);
    bundle.write(&config);
    let script = script.to_str().unwrap();
    let extra = [
        "--hold-fd",
        "9",
        "--exit-command",
        "/bin/sh",
        "--exit-command-arg",
        script,
    ];
    let call = runtime.create_with("held", &bundle, &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    // Closed, not unlocked: the monitor's copy holds it now.
    drop(file);
    assert!(locked(&lock), "held by the monitor");
    assert_eq!(runtime.call(&["start", "held"]).code, Some(0));
    assert!(locked(&lock));
    // None of the container's processes (its init, its program), nor its
    // hook, has a descriptor of it.
    let inode = std::fs::metadata(&lock).unwrap().ino();
    let hooked = std::fs::read_to_string(&hook_fds).unwrap();
    let lock_meta = std::fs::metadata(&lock).unwrap();
    // A file is its device and inode: an inode number alone may be another
    // file system's.
    let lock_id = format!("{} {}", lock_meta.dev(), lock_meta.ino());
    assert!(
        !hooked.lines().any(|line| line == lock_id),
        "the hook has the hold: {hooked}"
    );
    // Its init, and the command it forks once started (not at once).
    let processes = eventually("the container's init and command", || {
        Some(cgroup_procs(&cgroup_of("held"))).filter(|procs| procs.len() >= 2)
    });
    for pid in processes {
        for fd in std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .flatten()
        {
            if let Ok(target) = std::fs::metadata(fd.path()) {
                assert_ne!(
                    (target.dev(), target.ino()),
                    (lock_meta.dev(), lock_meta.ino()),
                    "process {pid} has the hold"
                );
            }
        }
    }
    assert!(!record.exists(), "no exit command while it runs");
    assert_eq!(runtime.call(&["kill", "held", "KILL"]).code, Some(0));
    let ran = eventually("the exit command", || std::fs::read_to_string(&record).ok());
    let lines: Vec<&str> = ran.lines().collect();
    assert_eq!(lines[0], "holds=1", "{ran}");
    assert!(lines[1].split(' ').any(|fd| fd == "3"), "{ran}");
    assert!(!lines[1].split(' ').any(|fd| fd == "9"), "only at 3: {ran}");
    // Its label: `fd-9`, none given.
    assert_eq!(recorded(&ran, "labels"), "fd-9", "{ran}");
    assert_eq!(
        recorded(&ran, "lock"),
        "held",
        "the exit command holds it: {ran}"
    );
    assert_eq!(
        recorded(&ran, "i3"),
        inode.to_string(),
        "3 is the hold: {ran}"
    );
    assert!(recorded(&ran, "exit").contains("\"code\":137"), "{ran}");
    eventually("the hold let go", || (!locked(&lock)).then_some(()));
    assert_eq!(runtime.status("held"), "stopped");
    // It started: nothing said.
    assert!(
        !runtime
            .root
            .join("held")
            .join("exit-command-failed")
            .exists()
    );
}

/// Two holds, with the container's output in logs (more descriptors open
/// in the monitor): the exit command gets each at its place, 3 and 4, in
/// the order given; and an exit command that can't start leaves them to go
/// with the monitor, and says so (`exit-command-failed`).
#[test]
#[ignore = "needs root"]
fn holds_reach_the_exit_command_in_order() {
    use std::os::unix::fs::MetadataExt as _;
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let first = runtime.output_dir().join("first.lock");
    let second = runtime.output_dir().join("second.lock");
    let (script, record) = recording_exit_command(&runtime.output_dir(), &first);
    let (files, command) = held_files(&[(&second, 8), (&first, 9)]);
    let bundle = bundle_running("echo out; exec sleep 300");
    let logs = runtime.log_dir("ordered");
    let extra = [
        "--log-dir",
        logs.to_str().unwrap(),
        "--hold-fd",
        "9=lease:first",
        "--hold-fd",
        "8",
        "--exit-command",
        "/bin/sh",
        "--exit-command-arg",
        script.to_str().unwrap(),
    ];
    let call = runtime.create_with("ordered", &bundle, &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    drop(files);
    assert_eq!(runtime.call(&["start", "ordered"]).code, Some(0));
    assert_eq!(runtime.call(&["kill", "ordered", "KILL"]).code, Some(0));
    let ran = eventually("the exit command", || std::fs::read_to_string(&record).ok());
    let inode = |path: &Path| std::fs::metadata(path).unwrap().ino().to_string();
    assert_eq!(recorded(&ran, "holds"), "2", "{ran}");
    assert_eq!(recorded(&ran, "labels"), "lease:first,fd-8", "{ran}");
    assert_eq!(recorded(&ran, "i3"), inode(&first), "9 first, at 3: {ran}");
    assert_eq!(recorded(&ran, "i4"), inode(&second), "8 next, at 4: {ran}");
    eventually("the holds let go", || {
        (!locked(&first) && !locked(&second)).then_some(())
    });

    let lock = runtime.output_dir().join("nowhere.lock");
    let (file, command) = held_file(&lock);
    let extra = ["--hold-fd", "9", "--exit-command", "/nonexistent/cleanup"];
    let call = runtime.create_with("nowhere", &bundle_running("true"), &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    drop(file);
    assert_eq!(runtime.call(&["start", "nowhere"]).code, Some(0));
    runtime.wait_for("nowhere", "stopped");
    eventually("the hold let go", || (!locked(&lock)).then_some(()));
    // Said, for the caller's recovery: made before the monitor exits (and
    // its hold with it), empty. Not tried again: it never would start.
    let failed = runtime.root.join("nowhere").join("exit-command-failed");
    let made = std::fs::symlink_metadata(&failed).unwrap();
    assert!(made.is_file() && made.len() == 0, "{made:?}");
    assert_eq!(made.mode() & 0o777, 0o600);
}

/// While the container runs (protocol version 3, stage 9, step 9.7b1): a
/// lock given to its monitor (`hold`) outlives its giver's copy, and reaches
/// the exit command after those from `create`, with its label; one from
/// `create` let go of (`release`) is free at once, and the exit command
/// doesn't get it. A label held already, a release of nothing, and a hold
/// once the container has ended are refused.
#[test]
#[ignore = "needs root"]
fn holds_are_added_and_let_go_of_while_it_runs() {
    use shipyard_runtime::container::{self as runtime, ContainerError};
    use std::os::fd::AsFd as _;
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let first = runtime.output_dir().join("first.lock");
    let second = runtime.output_dir().join("second.lock");
    let later = runtime.output_dir().join("later.lock");
    let (script, record) = recording_exit_command(&runtime.output_dir(), &later);
    let (files, command) = held_files(&[(&first, 8), (&second, 9)]);
    let logs = runtime.log_dir("holder");
    let extra = [
        "--log-dir",
        logs.to_str().unwrap(),
        "--hold-fd",
        "8=lease:first",
        "--hold-fd",
        "9=lease:second",
        "--exit-command",
        "/bin/sh",
        "--exit-command-arg",
        script.to_str().unwrap(),
    ];
    let call = runtime.create_with("holder", &bundle_running("exec sleep 300"), &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    drop(files);
    assert_eq!(runtime.call(&["start", "holder"]).code, Some(0));
    // Held since: the giver's copy closed, still locked.
    let given = locked_file(&later);
    runtime::hold(&runtime.root, "holder", "lease:later", given.as_fd()).unwrap();
    let again = runtime::hold(&runtime.root, "holder", "lease:later", given.as_fd());
    assert!(
        matches!(again, Err(ContainerError::NotHeld { .. })),
        "{again:?}"
    );
    drop(given);
    assert!(locked(&later), "held by the monitor");
    // One from `create`, let go of: free at once.
    assert!(locked(&first));
    runtime::release(&runtime.root, "holder", "lease:first").unwrap();
    eventually("the released hold free", || (!locked(&first)).then_some(()));
    let nothing = runtime::release(&runtime.root, "holder", "lease:first");
    assert!(
        matches!(nothing, Err(ContainerError::NotHeld { .. })),
        "{nothing:?}"
    );
    assert!(locked(&second));
    let killed = runtime.call(&["kill", "holder", "KILL"]);
    assert_eq!(killed.code, Some(0), "{}", killed.stderr);
    let ran = eventually("the exit command", || std::fs::read_to_string(&record).ok());
    assert_eq!(recorded(&ran, "holds"), "2", "{ran}");
    assert_eq!(
        recorded(&ran, "labels"),
        "lease:second,lease:later",
        "{ran}"
    );
    assert_eq!(
        recorded(&ran, "lock"),
        "held",
        "the exit command holds the later one: {ran}"
    );
    eventually("the holds let go", || {
        (!locked(&second) && !locked(&later)).then_some(())
    });
    // Ended: nothing more is held.
    let late = locked_file(&runtime.output_dir().join("late.lock"));
    let refused = runtime::hold(&runtime.root, "holder", "lease:late", late.as_fd());
    assert!(
        matches!(
            &refused,
            Err(ContainerError::NotHeld {
                reason: shipyard_runtime::attach::HoldRefusal::Ending,
                ..
            })
        ),
        "{refused:?}"
    );
    // A label that isn't one; a container without logs (no attach
    // socket: nothing can be handed to its monitor).
    let bad = runtime::hold(&runtime.root, "holder", "a b", late.as_fd());
    assert!(
        matches!(bad, Err(ContainerError::BadHoldLabel(_))),
        "{bad:?}"
    );
    let call = runtime.create("unlogged", &bundle_running("exec sleep 300"), &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let unlogged = runtime::hold(&runtime.root, "unlogged", "lease:x", late.as_fd());
    assert!(
        matches!(unlogged, Err(ContainerError::NotAttachable(_))),
        "{unlogged:?}"
    );
    assert_eq!(
        runtime.call(&["delete", "--force", "unlogged"]).code,
        Some(0)
    );
}

/// A monitor raises its own descriptor limit once its container is
/// spawned (F49): holds added since go past the soft limit its caller had
/// (128 here, the hard limit left alone), where its caller's limit refused
/// them. The container's init and the companion keep the caller's 128;
/// the exit command, given 300 holds at 3 and up, gets room for them and
/// 64 more (and can write its record).
#[test]
#[ignore = "needs root"]
fn holds_go_past_the_callers_limit_and_its_helpers_keep_it() {
    use nix::sys::resource::{Resource, getrlimit};
    use shipyard_runtime::container as runtime;
    use std::os::fd::AsFd as _;
    const CALLERS: u64 = 128;
    const HELD: usize = 300;
    let runtime = Runtime::new();
    let dir = runtime.output_dir();
    std::fs::create_dir_all(&dir).unwrap();
    // Each helper writes its soft limit down, then the companion waits.
    let limit_of = |name: &str, then: &str| {
        let script = dir.join(format!("{name}.sh"));
        let record = dir.join(format!("{name}.txt"));
        std::fs::write(
            &script,
            format!(
                "ulimit -Sn > {record}.tmp && mv {record}.tmp {record}\n{then}\n",
                record = record.display()
            ),
        )
        .unwrap();
        (script, record)
    };
    let (exit_script, exit_record) = limit_of("exit-command", "");
    let (companion_script, companion_record) = limit_of("companion", "exec sleep 300");
    let logs = runtime.log_dir("limited");
    let extra = [
        "--log-dir",
        logs.to_str().unwrap(),
        "--exit-command",
        "/bin/sh",
        "--exit-command-arg",
        exit_script.to_str().unwrap(),
        "--companion",
        "/bin/sh",
        "--companion-arg",
        companion_script.to_str().unwrap(),
    ];
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    // A precondition, said loudly: the test container's and a root
    // session's hard limit are far above it.
    assert!(hard > 3_000, "a hard limit with room to raise to: {hard}");
    let mut command = Command::new(RUNTIME.path());
    // SAFETY: `setrlimit` is async-signal-safe, on values known before the
    // fork.
    unsafe {
        command.pre_exec(move || {
            nix::sys::resource::setrlimit(Resource::RLIMIT_NOFILE, CALLERS, hard)
                .map_err(std::io::Error::from)
        });
    }
    let bundle = bundle_running("exec sleep 300");
    let call = runtime.create_with("limited", &bundle, &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "limited"]).code, Some(0));
    let files: Vec<std::fs::File> = (0..HELD)
        .map(|n| std::fs::File::create(dir.join(format!("held-{n}"))).unwrap())
        .collect();
    for (n, file) in files.iter().enumerate() {
        runtime::hold(
            &runtime.root,
            "limited",
            &format!("lease:h{n}"),
            file.as_fd(),
        )
        .unwrap_or_else(|error| panic!("hold {n}: {error}"));
    }
    let init = runtime.state("limited").unwrap()["pid"].as_i64().unwrap();
    // The monitor, as the runtime's own state file records it.
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("limited/state.json")).unwrap())
            .unwrap();
    let monitor = recorded["monitor"][0].as_i64().unwrap();
    let open = std::fs::read_dir(format!("/proc/{monitor}/fd"))
        .unwrap()
        .count();
    assert!(open > HELD, "past the caller's {CALLERS}: {open} open");
    let soft_of = |pid: &str| {
        std::fs::read_to_string(format!("/proc/{pid}/limits"))
            .unwrap()
            .lines()
            .find(|line| line.starts_with("Max open files"))
            .and_then(|line| line.split_whitespace().nth(3).map(str::to_owned))
            .unwrap()
    };
    assert_eq!(
        soft_of(&init.to_string()),
        CALLERS.to_string(),
        "the init's"
    );
    let companion = eventually("the companion's limit", || {
        std::fs::read_to_string(&companion_record).ok()
    });
    assert_eq!(companion.trim(), CALLERS.to_string(), "the companion's");
    assert_eq!(runtime.call(&["kill", "limited", "KILL"]).code, Some(0));
    let exit = eventually("the exit command's limit", || {
        std::fs::read_to_string(&exit_record).ok()
    });
    // Its holds at 3 and up, and the monitor's `OWN_DESCRIPTORS` (64) more.
    assert_eq!(
        exit.trim(),
        (3 + HELD + 64).to_string(),
        "the exit command's"
    );
}

/// A monitor hands its exit command more holds than its first raise has
/// room for twice over (1,300: their copies sit above them while they're
/// placed): it raises its limit again first, and the exit command runs
/// with them all (before, it couldn't be started, and a restart policy's
/// restart was lost).
#[test]
#[ignore = "needs root"]
fn many_holds_are_handed_to_the_exit_command() {
    use nix::sys::resource::{Resource, getrlimit};
    use shipyard_runtime::container as runtime;
    use std::os::fd::AsFd as _;
    const CALLERS: u64 = 128;
    const HELD: usize = 1_300;
    let runtime = Runtime::new();
    let dir = runtime.output_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let record = dir.join("exit-command.txt");
    let script = dir.join("exit-command.sh");
    std::fs::write(
        &script,
        format!(
            "echo \"$SHIPYARD_HOLDS $SHIPYARD_MONITOR_STARTED\" > {record}.tmp && mv {record}.tmp {record}\n",
            record = record.display()
        ),
    )
    .unwrap();
    let logs = runtime.log_dir("many");
    let extra = [
        "--log-dir",
        logs.to_str().unwrap(),
        "--exit-command",
        "/bin/sh",
        "--exit-command-arg",
        script.to_str().unwrap(),
    ];
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    // A precondition, said loudly: room for the holds twice over.
    assert!(
        hard >= u64::try_from(runtime::exit_command_descriptors(HELD)).unwrap(),
        "a hard limit with room: {hard}"
    );
    let mut command = Command::new(RUNTIME.path());
    // SAFETY: `setrlimit` is async-signal-safe, on values known before the
    // fork.
    unsafe {
        command.pre_exec(move || {
            nix::sys::resource::setrlimit(Resource::RLIMIT_NOFILE, CALLERS, hard)
                .map_err(std::io::Error::from)
        });
    }
    let bundle = bundle_running("exec sleep 300");
    let call = runtime.create_with("many", &bundle, &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "many"]).code, Some(0));
    let held = dir.join("held");
    std::fs::create_dir_all(&held).unwrap();
    for n in 0..HELD {
        // Each closed here once held: only the monitor's copies stay.
        let file = std::fs::File::create(held.join(n.to_string())).unwrap();
        runtime::hold(&runtime.root, "many", &format!("lease:h{n}"), file.as_fd())
            .unwrap_or_else(|error| panic!("hold {n}: {error}"));
    }
    // The monitor, as the runtime's state records it: its start time is
    // told to the exit command too (F102: what tells its start's state
    // from another's).
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("many/state.json")).unwrap())
            .unwrap();
    let monitor_started = recorded["monitor"][1].as_u64().unwrap();
    assert_eq!(runtime.call(&["kill", "many", "KILL"]).code, Some(0));
    let handed = eventually("the exit command's record", || {
        std::fs::read_to_string(&record).ok()
    });
    assert_eq!(
        handed.trim(),
        format!("{HELD} {monitor_started}"),
        "the holds it was given, and its monitor's start time"
    );
}

/// A file, made and locked here (`flock`, exclusive).
fn locked_file(path: &Path) -> std::fs::File {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::create(path).unwrap();
    // SAFETY: `flock` on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    file
}

/// Without an exit command, a hold goes once the container has stopped;
/// with its monitor killed, at once, and no exit command runs.
#[test]
#[ignore = "needs root"]
fn holds_go_with_the_container_or_its_monitor() {
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let lock = runtime.output_dir().join("plain.lock");
    let (file, command) = held_file(&lock);
    let bundle = bundle_running("sleep 1");
    let call = runtime.create_with("plain", &bundle, &["--hold-fd", "9"], command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    drop(file);
    assert!(locked(&lock));
    assert_eq!(runtime.call(&["start", "plain"]).code, Some(0));
    runtime.wait_for("plain", "stopped");
    eventually("the hold let go", || (!locked(&lock)).then_some(()));

    let lock = runtime.output_dir().join("killed.lock");
    let (script, record) = recording_exit_command(&runtime.output_dir(), &lock);
    let (file, command) = held_file(&lock);
    let bundle = bundle_running("exec sleep 300");
    let extra = [
        "--hold-fd",
        "9",
        "--exit-command",
        "/bin/sh",
        "--exit-command-arg",
        script.to_str().unwrap(),
    ];
    let call = runtime.create_with("orphaned", &bundle, &extra, command);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    drop(file);
    let state: Value =
        serde_json::from_slice(&std::fs::read(runtime.root.join("orphaned/state.json")).unwrap())
            .unwrap();
    let monitor = state["monitor"][0].as_i64().unwrap();
    // SAFETY: `kill` of the monitor this test's `create` started.
    unsafe { libc::kill(i32::try_from(monitor).unwrap(), libc::SIGKILL) };
    eventually("the hold let go", || (!locked(&lock)).then_some(()));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!record.exists(), "no exit command for a killed monitor");
}

/// `delete --force` of a running container ends it: its exit command runs.
/// A `create` that fails after its monitor started runs none.
#[test]
#[ignore = "needs root"]
fn the_exit_command_runs_after_delete_and_not_after_a_failed_create() {
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let lock = runtime.output_dir().join("deleted.lock");
    let (script, record) = recording_exit_command(&runtime.output_dir(), &lock);
    let script = script.to_str().unwrap().to_owned();
    let exit = ["--exit-command", "/bin/sh", "--exit-command-arg", &script];
    let bundle = bundle_running("exec sleep 300");
    let call = runtime.create("deleted", &bundle, &exit);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "deleted"]).code, Some(0));
    assert_eq!(
        runtime.call(&["delete", "--force", "deleted"]).code,
        Some(0)
    );
    let ran = eventually("the exit command", || std::fs::read_to_string(&record).ok());
    assert!(ran.starts_with("holds=0"), "{ran}");
    std::fs::remove_file(&record).unwrap();

    let failing = Bundle::new();
    let (mut config, _log) = hooked(&runtime, &failing, "echo ran");
    config["hooks"]["createRuntime"] = json!([{"path": "/bin/sh", "args": ["sh", "-c", "exit 3"]}]);
    failing.write(&config);
    let call = runtime.create("failing", &failing, &exit);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!record.exists(), "no exit command after a failed create");
}

/// A companion (a shell script) that records its PID, its parent's and its
/// session's, starts a child of its own and records its PID, then waits.
fn recording_companion(dir: &Path) -> (PathBuf, PathBuf) {
    let record = dir.join("companion.txt");
    let script = dir.join("companion.sh");
    std::fs::write(
        &script,
        format!(
            "sleep 300 & child=$!\n\
             {{ echo pid=$$; echo child=$child; \
             echo ppid=$(awk '{{print $4}}' /proc/$$/stat); \
             echo sid=$(awk '{{print $6}}' /proc/$$/stat); }} > {record}.tmp \
             && mv {record}.tmp {record}\nwait\n",
            record = record.display(),
        ),
    )
    .unwrap();
    (script, record)
}

/// The value after `key=` on a line of a record.
fn field(record: &str, key: &str) -> i64 {
    record
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_default()
        .parse()
        .unwrap_or(0)
}

/// A companion runs beside the container from `create` on: the monitor's
/// child, in a session of its own, with only its standard streams. Once the
/// container has ended, it's gone, with what it started, before the end is
/// recorded (`state` says stopped): with its output in logs (the monitor
/// watching the init) or not (the monitor only waiting for it).
#[test]
#[ignore = "needs root"]
fn a_companion_runs_beside_the_container_and_ends_with_it() {
    let runtime = Runtime::new();
    companion_ends_with_its_container(&runtime, "beside", false);
    companion_ends_with_its_container(&runtime, "beside-logged", true);
}

/// [`a_companion_runs_beside_the_container_and_ends_with_it`]'s for
/// container `id`, `logged` or not.
fn companion_ends_with_its_container(runtime: &Runtime, id: &str, logged: bool) {
    let dir = runtime.output_dir().join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let (script, record) = recording_companion(&dir);
    let script = script.to_str().unwrap();
    let extra = ["--companion", "/bin/sh", "--companion-arg", script];
    let bundle = bundle_running("exec sleep 300");
    let call = if logged {
        runtime.create_logged(id, &bundle, &extra).0
    } else {
        runtime.create(id, &bundle, &extra)
    };
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    // Before the start: it waits for it itself, if it needs to.
    let ran = eventually("the companion", || std::fs::read_to_string(&record).ok());
    let (pid, child) = (field(&ran, "pid"), field(&ran, "child"));
    assert!(alive(pid) && alive(child), "{ran}");
    assert_eq!(field(&ran, "sid"), pid, "a session of its own: {ran}");
    // Its standard streams on /dev/null; nothing else but the script the
    // shell reads.
    for fd in std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .flatten()
    {
        let target = std::fs::read_link(fd.path()).unwrap();
        assert!(
            target == Path::new("/dev/null") || target == Path::new(script),
            "{}: {}",
            fd.file_name().to_string_lossy(),
            target.display()
        );
    }
    let monitor = field(&ran, "ppid");
    let cmdline = std::fs::read_to_string(format!("/proc/{monitor}/cmdline")).unwrap();
    assert!(
        cmdline.split('\0').any(|arg| arg == "__shipyard-monitor"),
        "the monitor's child: {cmdline:?}"
    );
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    assert!(alive(pid), "on while the container runs");
    assert_eq!(runtime.call(&["kill", id, "KILL"]).code, Some(0));
    runtime.wait_for(id, "stopped");
    // Killed before the end was recorded (reaped: no zombie of the
    // monitor's); its child with its process group.
    assert!(!alive(pid), "the companion outlived the end");
    assert!(!alive(child), "its child outlived the end");
    assert_eq!(runtime.call(&["delete", id]).code, Some(0));
}

/// A monitor killed outright takes its companion (its parent-death
/// signal), as it takes the container.
#[test]
#[ignore = "needs root"]
fn a_monitor_killed_takes_its_companion() {
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let (script, record) = recording_companion(&runtime.output_dir());
    let script = script.to_str().unwrap();
    let extra = ["--companion", "/bin/sh", "--companion-arg", script];
    let bundle = bundle_running("exec sleep 300");
    assert_eq!(runtime.create("orphan", &bundle, &extra).code, Some(0));
    assert_eq!(runtime.call(&["start", "orphan"]).code, Some(0));
    let ran = eventually("the companion", || std::fs::read_to_string(&record).ok());
    let (pid, monitor) = (field(&ran, "pid"), field(&ran, "ppid"));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(monitor).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    eventually("the companion gone", || (!alive(pid)).then_some(()));
    runtime.wait_for("orphan", "stopped");
    let _ = runtime.call(&["delete", "--force", "orphan"]);
}

/// What can't be held is refused: a standard stream, a number that isn't
/// open, one given twice, one passed to the container; and an exit command
/// that isn't an absolute path.
#[test]
fn bad_holds_and_exit_commands_are_refused() {
    let runtime = Runtime::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let bundle = bundle_running("true");
    for extra in [&["--hold-fd", "1"][..], &["--hold-fd", "77"]] {
        let call = runtime.create("bad", &bundle, extra);
        assert_eq!(call.code, Some(1), "{extra:?}: {}", call.stderr);
        assert!(
            call.stderr.contains("--hold-fd"),
            "{extra:?}: {}",
            call.stderr
        );
    }
    // Descriptor 3, open, but passed to the container.
    let (_file, command) = held_file_at(&runtime.output_dir().join("passed.lock"), 3);
    let extra = ["--preserve-fds", "1", "--hold-fd", "3"];
    let call = runtime.create_with("bad", &bundle, &extra, command);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(call.stderr.contains("--hold-fd 3"), "{}", call.stderr);
    let (_file, command) = held_file(&runtime.output_dir().join("twice.lock"));
    let call = runtime.create_with(
        "bad",
        &bundle,
        &["--hold-fd", "9", "--hold-fd", "9"],
        command,
    );
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(call.stderr.contains("--hold-fd 9"), "{}", call.stderr);
    let call = runtime.create("bad", &bundle, &["--exit-command", "sh"]);
    assert!(
        call.stderr.contains("it must be an absolute path"),
        "{}",
        call.stderr
    );
    assert!(!runtime.root.join("bad").exists());
}

/// Between its init's end and its monitor's cleanup, a container is
/// stopping: OCI's `state` says stopped (its process has exited), with no
/// PID, and what needs it running refuses it as stopping. Made by
/// hand (the window is short): a live process as its monitor, and as its
/// init a PID whose start time isn't the recorded one (gone, its PID
/// reused).
#[test]
#[ignore = "needs root"]
fn a_stopping_container_is_said_to_be() {
    use std::os::unix::fs::DirBuilderExt as _;
    let runtime = Runtime::new();
    let mut monitor = Command::new("sleep").arg("300").spawn().unwrap();
    let pid = i64::from(monitor.id());
    let started = start_time_of(pid).unwrap();
    let dir = runtime.root.join("ending");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    // As stage 8 wrote it, without a version: read as version 0.
    let record = json!({
        "bundle": "/nonexistent",
        "monitor": [pid, started],
        "init": [pid, started + 1],
        "annotations": {},
    });
    std::fs::write(dir.join("state.json"), record.to_string()).unwrap();
    let state = runtime.state("ending").unwrap();
    assert_eq!(state["status"], "stopped", "{state}");
    assert!(state.get("pid").is_none(), "{state}");
    for call in [
        &["kill", "ending", "TERM"][..],
        &["kill", "--all", "ending", "KILL"],
        &["pause", "ending"],
        &["resume", "ending"],
        &["exec", "ending", "true"],
        &["cp", "ending:/etc/hostname", "-"],
    ] {
        let refused = runtime.call(call);
        assert_ne!(refused.code, Some(0), "{call:?}");
        assert!(
            refused.stderr.contains("container ending is stopping"),
            "{call:?}: {}",
            refused.stderr
        );
    }
    // The control: the same container once its end is recorded (as stage
    // 8's monitor wrote it, a line of text) is stopped, and said to be.
    std::fs::write(dir.join("exit"), "exited (exit code 0)").unwrap();
    let refused = runtime.call(&["kill", "ending", "TERM"]);
    assert!(
        refused.stderr.contains("container ending is stopped"),
        "{}",
        refused.stderr
    );
    std::fs::remove_file(dir.join("exit")).unwrap();
    // Stopped enough to delete: its monitor has nothing left to clean up
    // that delete doesn't.
    let deleted = runtime.call(&["delete", "ending"]);
    assert_eq!(deleted.code, Some(0), "{}", deleted.stderr);
    assert!(!dir.exists());
    let _ = monitor.kill();
    let _ = monitor.wait();
}

/// A state record a newer Shipyard wrote is refused, never misread.
#[test]
#[ignore = "needs root"]
fn a_newer_state_record_is_refused() {
    use std::os::unix::fs::DirBuilderExt as _;
    let runtime = Runtime::new();
    let dir = runtime.root.join("future");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let record = json!({"version": 2, "bundle": "/b", "init": null, "annotations": {}});
    std::fs::write(dir.join("state.json"), record.to_string()).unwrap();
    let call = runtime.call(&["state", "future"]);
    assert_ne!(call.code, Some(0));
    assert!(
        call.stderr
            .contains("state.json has version 2, which this Shipyard doesn't know"),
        "{}",
        call.stderr
    );
    std::fs::remove_dir_all(&dir).unwrap();
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
        // Refused as one that isn't a container's own (this process's cgroup
        // is never one), before it's found to exist.
        assert!(
            call.stderr
                .contains("isn't one of Shipyard's container cgroups"),
            "{}",
            call.stderr
        );
        assert!(!runtime.root.join("taken").exists());
    }
    // Nor one below Shipyard's services' parents (another container's
    // monitor's, a network's DNS server's), there or not: its `delete`
    // would kill them. Refused before anything is made.
    for service in [
        format!("/shipyard/monitors/{parent}"),
        format!("/shipyard/dns/{parent}-0a1b2c3d"),
        format!("/shipyard/monitors/{parent}/below"),
    ] {
        config["linux"]["cgroupsPath"] = json!(service);
        bundle.write(&config);
        let call = runtime.create("service", &bundle, &[]);
        assert_eq!(call.code, Some(1), "{service}");
        assert!(
            call.stderr
                .contains("isn't one of Shipyard's container cgroups"),
            "{service}: {}",
            call.stderr
        );
        assert!(!runtime.root.join("service").exists(), "{service}");
        let place = PathBuf::from("/sys/fs/cgroup").join(service.trim_start_matches('/'));
        assert!(!place.exists(), "{service} was made");
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
            .arg(RUNTIME.path());
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
    let mut events = Command::new(RUNTIME.path())
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

/// The mount points of `/proc/self/mountinfo`'s `text`, with each one's
/// options, in order.
fn mount_points(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            Some(((*fields.get(4)?).to_owned(), (*fields.get(5)?).to_owned()))
        })
        .collect()
}

/// Mounts are made in the order the config lists them (the spec's "MUST"),
/// the standard ones included, as runtime-tools' default config lists
/// them; each standard one with Shipyard's own options, wherever it is.
#[test]
#[ignore = "needs root"]
fn mounts_are_made_in_the_order_listed() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["cat", "/proc/self/mountinfo"]);
    config["mounts"] = json!([
        {"destination": "/proc", "type": "proc", "source": "proc"},
        {"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
         "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]},
        {"destination": "/dev/pts", "type": "devpts", "source": "devpts"},
        {"destination": "/dev/shm", "type": "tmpfs", "source": "shm"},
        {"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue"},
        {"destination": "/mnt", "type": "tmpfs", "source": "tmpfs"},
        {"destination": "/sys", "type": "sysfs", "source": "sysfs"},
    ]);
    bundle.write(&config);
    let call = runtime.create("ordered", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.call(&["start", "ordered"]).code, Some(0));
    runtime.wait_for("ordered", "stopped");
    let out = runtime.output_of("ordered");
    let points = mount_points(&out);
    let at = |point: &str| {
        points
            .iter()
            .position(|(at, _)| at == point)
            .unwrap_or_else(|| panic!("no {point} in {out}"))
    };
    let order = [
        "/proc",
        "/dev",
        "/dev/pts",
        "/dev/shm",
        "/dev/mqueue",
        "/mnt",
        "/sys",
    ]
    .map(at);
    assert!(order.is_sorted(), "{order:?} in {out}");
    // Shipyard's options, not the config's (it gave /sys none).
    let options = |point: &str| &points[at(point)].1;
    assert!(options("/sys").starts_with("ro,"), "{out}");
    assert!(options("/proc").contains("nosuid,nodev,noexec"), "{out}");
    assert_eq!(runtime.call(&["delete", "ordered"]).code, Some(0));
}

/// A mount listed before `/sys` that a symlink in the root filesystem leads
/// into `sys` (`/s -> /sys`, a mount at `/s/x`): refused by its place, not
/// made there to be hidden by sysfs (F36). (Its mount point stays made, as
/// a mount point is.)
#[test]
#[ignore = "needs root"]
fn a_mount_a_symlink_leads_into_a_later_standard_mount_is_refused() {
    steered_into_a_later_sys("steered");
}

/// The same rootless, where `/sys` is the host's, bound, with its mounts
/// covered: the mount isn't made, so nothing is left to cover.
#[test]
fn a_rootless_mount_a_symlink_leads_into_sys_is_refused() {
    if !rootless(Rootless::Single) {
        return;
    }
    steered_into_a_later_sys("steered-rootless");
}

fn steered_into_a_later_sys(id: &str) {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    std::os::unix::fs::symlink("/sys", bundle.root.path().join("s")).unwrap();
    let mut config = bundle.config();
    config["mounts"] = json!([
        {"destination": "/s/x", "type": "tmpfs", "source": "tmpfs"},
        {"destination": "/proc", "type": "proc", "source": "proc"},
        {"destination": "/sys", "type": "sysfs", "source": "sysfs"},
    ]);
    bundle.write(&config);
    let call = runtime.create(id, &bundle, &[]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        call.stderr
            .contains("the mount point /s/x leads into a kernel interface"),
        "{}",
        call.stderr
    );
    assert!(runtime.state(id).is_none(), "nothing left");
}

/// An order that can't work is refused, saying what to move: a mount below
/// `/dev` listed before `/dev`, whose tmpfs would hide it.
#[test]
#[ignore = "needs root"]
fn a_mount_a_later_one_would_hide_is_refused() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["mounts"] = json!([
        {"destination": "/dev/hidden", "type": "tmpfs", "source": "tmpfs"},
        {"destination": "/dev", "type": "tmpfs", "source": "tmpfs"},
    ]);
    bundle.write(&config);
    let call = runtime.create("hidden", &bundle, &[]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        call.stderr
            .contains("/dev/hidden is listed before /dev, which would hide it"),
        "{}",
        call.stderr
    );
    assert!(runtime.state("hidden").is_none(), "nothing left");
}

/// `poststart` runs once the program has started (`execve`d), as the spec
/// says, not as soon as the init is let go (and while it runs: see
/// `poststart_runs_while_the_program_does`): its hook finds the init's child
/// to be the program, not a copy of Shipyard still on its way there. Even
/// when a `startContainer` hook makes it wait.
#[test]
#[ignore = "needs root"]
fn poststart_runs_once_the_program_has_started() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let log = runtime.output_dir().join("poststart-exe");
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sleep", "30"]);
    config["hooks"] = json!({
        "startContainer": [{"path": "/bin/sh", "args": ["sh", "-c", "sleep 1"]}],
        "poststart": [{"path": "/bin/sh", "args": ["sh", "-c", format!(
            "pid=$(sed -n 's/.*\"pid\":\\([0-9]*\\).*/\\1/p'); \
             for child in $(cat /proc/$pid/task/$pid/children); do \
             readlink /proc/$child/exe; done > {}",
            log.display()
        )]}],
    });
    bundle.write(&config);
    let call = runtime.create("started", &bundle, &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let start = runtime.call(&["start", "started"]);
    assert_eq!(start.code, Some(0), "{}", start.stderr);
    let exe = std::fs::read_to_string(&log).unwrap();
    assert!(
        exe.trim().ends_with("sleep") || exe.contains("busybox"),
        "{exe:?}"
    );
    assert!(!exe.contains("shipyard"), "not the program yet: {exe:?}");
    let _ = std::fs::remove_file(&log);
    assert_eq!(
        runtime.call(&["delete", "--force", "started"]).code,
        Some(0)
    );
}

/// What a `startContainer` hook leaves running (a process it started in the
/// background) doesn't hold `poststart` back: the started FIFO is closed
/// at the hook's `execve`, so nothing it starts has it.
#[test]
#[ignore = "needs root"]
fn a_hooks_leftover_doesnt_hold_poststart_back() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sleep", "30"]);
    config["hooks"] = json!({
        "startContainer": [{"path": "/bin/sh", "args": ["sh", "-c", "sleep 30 >/dev/null 2>&1 &"]}],
        "poststart": [{"path": "/bin/true"}],
    });
    bundle.write(&config);
    let created = runtime.create("leftover", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let started = Instant::now();
    let start = runtime.call(&["start", "leftover"]);
    assert_eq!(start.code, Some(0), "{}", start.stderr);
    assert!(!start.stderr.contains("hadn't started"), "{}", start.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        runtime.call(&["delete", "--force", "leftover"]).code,
        Some(0)
    );
}

/// Without `poststart` hooks, `create` makes no started FIFO, and the
/// program never sees one: nothing changes for them.
#[test]
#[ignore = "needs root"]
fn only_poststart_hooks_make_a_started_fifo() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "ls -l /proc/self/fd"]);
    bundle.write(&config);
    assert_eq!(runtime.create("fifo-plain", &bundle, &[]).code, Some(0));
    assert!(!runtime.root.join("fifo-plain/exec.started").exists());
    assert_eq!(runtime.call(&["start", "fifo-plain"]).code, Some(0));
    runtime.wait_for("fifo-plain", "stopped");
    assert!(!runtime.output_of("fifo-plain").contains("exec.started"));
    // With one, the program doesn't inherit it either (close-on-exec).
    config["hooks"] = json!({"poststart": [{"path": "/bin/true"}]});
    bundle.write(&config);
    let created = runtime.create("fifo-hooked", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert!(runtime.root.join("fifo-hooked/exec.started").exists());
    assert_eq!(runtime.call(&["start", "fifo-hooked"]).code, Some(0));
    assert!(
        !runtime.root.join("fifo-hooked/exec.started").exists(),
        "removed"
    );
    runtime.wait_for("fifo-hooked", "stopped");
    let fds = runtime.output_of("fifo-hooked");
    assert!(!fds.contains("exec."), "{fds}");
    for id in ["fifo-plain", "fifo-hooked"] {
        assert_eq!(runtime.call(&["delete", id]).code, Some(0));
    }
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
        "{} --root {} state asked > {}",
        RUNTIME.path(),
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
    let mut create = Command::new(RUNTIME.path())
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

/// When process `pid` started (`/proc/PID/stat`'s 22nd field, in clock
/// ticks since boot): with its PID, which process it is.
fn start_time_of(pid: i64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
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
    let mut create = Command::new(RUNTIME.path())
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
        .arg(RUNTIME.path())
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
        "{} --root {} create --bundle {} survivor >/dev/null 2>&1; kill -KILL 0",
        RUNTIME.path(),
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
        let output = Command::new(RUNTIME.path()).arg(hidden).output().unwrap();
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

// --- As root: terminals ------------------------------------------------------------

/// With a console socket, `create` returns (no answer is awaited at the
/// gate, as runc's protocol has none) once the master is sent: a
/// pseudo-terminal's, named `/dev/ptmx` (runc names it too), with the
/// connection closed after it. Nothing runs before `start`; then what's
/// typed reaches the container and its output comes back.
#[test]
#[ignore = "needs root"]
fn create_with_a_console_socket_sends_the_master() {
    let runtime = Runtime::new();
    let bundle = terminal_bundle("echo ready; read line; echo got-$line", None);
    let mut console = create_on_console(&runtime, "tty", &bundle);
    let _ = console.pty_number();
    assert_eq!(console.name, b"/dev/ptmx");
    assert!(
        console.connection_closed(),
        "the runtime and the init let go of the console socket"
    );
    assert_eq!(runtime.status("tty"), "created");
    console.read_for(Duration::from_millis(200));
    assert_eq!(console.output(), "", "nothing runs before start");
    assert_eq!(runtime.call(&["start", "tty"]).code, Some(0));
    console.expect("ready");
    console.type_keys(b"hello\n");
    console.expect("got-hello");
    runtime.wait_for("tty", "stopped");
    assert_eq!(
        runtime.output_of("tty"),
        "",
        "the output went to the terminal"
    );
}

/// With a terminal, `create`'s caller gets its output back when `create`
/// returns, before `start`: a shim (go-runc) reads `create`'s stdout and
/// stderr to their end before it calls `start`. The terminal is the
/// container's stdio instead, as runc's.
#[test]
#[ignore = "needs root"]
fn create_on_a_console_lets_go_of_its_callers_output() {
    use std::io::Read as _;
    let runtime = Runtime::new();
    let bundle = terminal_bundle("echo ran", None);
    let socket = ConsoleSocket::new();
    let mut create = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["create", "--bundle", bundle.arg(), "--console-socket"])
        .arg(socket.path())
        .arg("piped")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stdout, mut stderr) = (create.stdout.take().unwrap(), create.stderr.take().unwrap());
    let (done, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = stderr.read_to_string(&mut text);
        let _ = done.send(text);
    });
    let output = ended
        .recv_timeout(Duration::from_secs(10))
        .expect("create's output never reached its end: something still holds it");
    assert!(create.wait().unwrap().success(), "{output}");
    assert_eq!(runtime.status("piped"), "created");
    let mut console = socket.receive();
    assert_eq!(runtime.call(&["start", "piped"]).code, Some(0));
    console.expect("ran");
}

/// A command that can't be executed, found out after `start`: explained on
/// the terminal, where whoever holds it sees it.
#[test]
#[ignore = "needs root"]
fn a_failure_after_start_is_told_on_the_console() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    // A plain file, not executable.
    config["process"]["args"] = json!(["/marker"]);
    config["process"]["terminal"] = json!(true);
    bundle.write(&config);
    let mut console = create_on_console(&runtime, "noexec", &bundle);
    assert_eq!(runtime.call(&["start", "noexec"]).code, Some(0));
    console.expect("shipyard: ");
    console.expect("/marker");
    runtime.wait_for("noexec", "stopped");
}

/// The terminal is `consoleSize` big, else 24x80 (where runc leaves it 0x0).
#[test]
#[ignore = "needs root"]
fn the_console_size_is_the_terminals() {
    let runtime = Runtime::new();
    for (id, size, expected) in [
        ("sized", Some((30, 100)), "30 100"),
        ("default", None, "24 80"),
    ] {
        let bundle = terminal_bundle("stty size", size);
        let mut console = create_on_console(&runtime, id, &bundle);
        assert_eq!(runtime.call(&["start", id]).code, Some(0));
        console.expect(expected);
    }
}

/// The container's command holds its terminal and nothing else: neither
/// the master nor the console socket.
#[test]
#[ignore = "needs root"]
fn a_console_containers_descriptors_are_its_terminal_only() {
    let runtime = Runtime::new();
    let bundle = terminal_bundle("ls -l /proc/self/fd/; echo do''ne", None);
    let mut console = create_on_console(&runtime, "fds", &bundle);
    assert_eq!(runtime.call(&["start", "fds"]).code, Some(0));
    console.expect("done");
    let output = console.output();
    assert!(output.contains("/dev/pts/0"), "{output}");
    for held in ["ptmx", "socket:", "pipe:"] {
        assert!(!output.contains(held), "{held}: {output}");
    }
}

/// A console socket at a path longer than a socket address can hold is
/// reached through its directory.
#[test]
#[ignore = "needs root"]
fn a_long_console_socket_path_is_reached() {
    let runtime = Runtime::new();
    let bundle = terminal_bundle("echo long-ok", None);
    let socket = ConsoleSocket::with_a_long_path();
    // Longer than `sun_path` (108 bytes, with its NUL) holds.
    assert!(socket.path().as_os_str().len() > 108);
    let call = runtime.create(
        "long",
        &bundle,
        &["--console-socket", socket.path().to_str().unwrap()],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let mut console = socket.receive();
    assert_eq!(runtime.call(&["start", "long"]).code, Some(0));
    console.expect("long-ok");
}

// --- Logs (--log-dir), as any user ----------------------------------------------

/// What can't go together, or can't be: a console socket with logs, a log
/// directory that exists already (a link planted there too), rotation out of
/// bounds, and `logs` for a container without them. Nothing is left.
#[test]
fn log_options_that_cant_work_are_refused() {
    let runtime = Runtime::new();
    let socket = ConsoleSocket::new();
    let terminal = terminal_bundle("true", None);
    let (call, dir) = runtime.create_logged(
        "both",
        &terminal,
        &["--console-socket", socket.path().to_str().unwrap()],
    );
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr
            .contains("a console socket and a log directory both"),
        "{}",
        call.stderr
    );
    assert!(!dir.exists(), "no log directory left");
    let plain = bundle_running("true");
    for (existing, made) in [("dir", true), ("link", false)] {
        let dir = runtime.log_dir(existing);
        if made {
            std::fs::create_dir(&dir).unwrap();
        } else {
            std::os::unix::fs::symlink("/tmp", &dir).unwrap();
        }
        let call = runtime.create(existing, &plain, &["--log-dir", dir.to_str().unwrap()]);
        assert_eq!(call.code, Some(1), "{existing}");
        assert!(
            call.stderr.contains("exists: give one that doesn't"),
            "{}",
            call.stderr
        );
        assert_eq!(runtime.state(existing), None);
    }
    let (call, dir) = runtime.create_logged("one", &plain, &["--log-max-files", "1"]);
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr.contains("between 2 and 100 files"),
        "{}",
        call.stderr
    );
    assert!(!dir.exists());
    let (call, dir) = runtime.create_logged("tiny", &plain, &["--log-max-size", "4k"]);
    assert_eq!(call.code, Some(1));
    assert!(!dir.exists());
    assert!(
        call.stderr.contains("between 64 KiB and 1 GiB"),
        "{}",
        call.stderr
    );
}

/// A rootless container's output is in its logs too.
#[test]
fn a_rootless_containers_output_is_logged() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let logs = runtime.run_logged(
        "rootless-logs",
        &bundle_running("echo out; echo err >&2"),
        &[],
    );
    assert_eq!(logs.code, Some(0), "{}", logs.stderr);
    assert_eq!(logs.stdout, "out\n");
    assert!(logs.stderr.contains("err\n"), "{}", logs.stderr);
}

// --- Logs, as root ------------------------------------------------------------------

/// The monitor lives in `/`, not in the caller's working directory: it
/// lives as long as the container, and would keep that directory's mount
/// busy all along.
#[test]
#[ignore = "needs root"]
fn the_monitor_holds_no_working_directory() {
    let runtime = Runtime::new();
    let id = "monitor-cwd";
    let bundle = bundle_running("exec sleep 60");
    let created = runtime.create(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let state = std::fs::read(runtime.root.join(id).join("state.json")).unwrap();
    let record: Value = serde_json::from_slice(&state).unwrap();
    let monitor = record["monitor"][0].as_i64().unwrap();
    assert_eq!(
        std::fs::read_link(format!("/proc/{monitor}/cwd")).unwrap(),
        PathBuf::from("/")
    );
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// `logs --follow ID` ends when that container is deleted, even if a new
/// one takes its ID at once: it never follows the old directory for the
/// new container's life.
#[test]
#[ignore = "needs root"]
fn following_ends_when_the_id_is_another_containers() {
    let runtime = Runtime::new();
    let id = "followed-again";
    let bundle = bundle_running("echo first; exec sleep 60");
    let (created, _) = runtime.create_logged(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    let follow = Attached::running(&runtime, &["logs", "--follow", id]);
    follow.expect("first");
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
    let again = runtime.create(
        id,
        &bundle,
        &[
            "--log-dir",
            runtime.log_dir("followed-again-2").to_str().unwrap(),
        ],
    );
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    assert_eq!(follow.end().0, Some(0));
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// `logs --follow` ends, quietly, once its reader has gone (`… | head -n1`),
/// while the container still runs and writes nothing more: between its
/// looks it asks whether stdout's reader is there (F78). By ID, and by
/// `--log-dir`, which had no end of its own.
#[test]
#[ignore = "needs root"]
fn following_ends_when_its_reader_goes() {
    use std::io::BufRead as _;
    let runtime = Runtime::new();
    let id = "followed-unread";
    let bundle = bundle_running("echo first; exec sleep 60");
    let (created, dir) = runtime.create_logged(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    for target in [vec![id], vec!["--log-dir", dir.to_str().unwrap()]] {
        let mut follow = Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&runtime.root)
            .args(["logs", "--follow"])
            .args(&target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Its first line read (the control: it follows), then its reader
        // gone, the pipe's read end closed.
        let mut stdout = std::io::BufReader::new(follow.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line, "first\n", "{target:?}");
        drop(stdout);
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = follow.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = follow.kill();
                panic!("{target:?}: it followed with no reader");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        std::io::Read::read_to_string(follow.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert!(status.success(), "{target:?}: {status:?}: {stderr}");
        assert_eq!(stderr, "", "{target:?}");
    }
    // Still running: the follow ended for its reader, not its container.
    assert_eq!(runtime.status(id), "running");
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// `logs --follow ID` ends when that container is deleted, even if a new
/// one takes its ID and its log directory's path at once (the old directory
/// removed, a new one made there): what's at the path is another directory.
#[test]
#[ignore = "needs root"]
fn following_ends_when_the_id_and_path_are_reused() {
    let runtime = Runtime::new();
    let id = "followed-at-the-same-path";
    let bundle = bundle_running("echo first; exec sleep 60");
    let (created, dir) = runtime.create_logged(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    let follow = Attached::running(&runtime, &["logs", "--follow", id]);
    follow.expect("first");
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
    std::fs::remove_dir_all(&dir).unwrap();
    let (again, again_dir) = runtime.create_logged(id, &bundle, &[]);
    assert_eq!(again.code, Some(0), "{}", again.stderr);
    assert_eq!(again_dir, dir, "the same path");
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    assert_eq!(follow.end().0, Some(0));
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// `logs ID` reads only the directory its container was created with: one
/// put at its path since (the real one moved away), with logs of its own,
/// is refused, not shown as the container's.
#[test]
#[ignore = "needs root"]
fn logs_refuses_a_directory_put_in_its_place() {
    let runtime = Runtime::new();
    let id = "logs-swapped";
    let bundle = bundle_running("echo real; exec sleep 60");
    let (created, dir) = runtime.create_logged(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runtime.call(&["logs", id]).stdout.contains("real") {
        assert!(Instant::now() < deadline, "the real line never came");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A copy of the real logs, edited, in their place.
    let moved = dir.with_extension("moved");
    std::fs::rename(&dir, &moved).unwrap();
    std::fs::create_dir(&dir).unwrap();
    for name in ["log.json", "container.log"] {
        let text = std::fs::read_to_string(moved.join(name)).unwrap();
        std::fs::write(dir.join(name), text.replace("real", "fake")).unwrap();
    }
    let call = runtime.call(&["logs", id]);
    assert_ne!(call.code, Some(0));
    assert!(!call.stdout.contains("fake"), "{}", call.stdout);
    assert!(
        call.stderr
            .contains("isn't the log directory its container was created with"),
        "{}",
        call.stderr
    );
    // Back in place: read.
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::rename(&moved, &dir).unwrap();
    assert!(runtime.call(&["logs", id]).stdout.contains("real"));
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// A command for `shipyard-runtime` whose file-size limit (`RLIMIT_FSIZE`)
/// is `soft` and `hard`: as `ulimit -f` would leave it.
fn file_size_limited(soft: libc::rlim_t, hard: libc::rlim_t) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut command = Command::new(RUNTIME.path());
    // SAFETY: only `setrlimit`, async-signal-safe, between fork and exec.
    unsafe {
        command.pre_exec(move || {
            let cap = libc::rlimit {
                rlim_cur: soft,
                rlim_max: hard,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &raw const cap) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// A rotation that fails (its new file can't be made) leaves the writer in
/// the file it has, writing on up to a file-size limit (the caller's
/// `ulimit -S -f`, inherited by the monitor): the file reaches the limit and
/// ends there with a whole record, not cut back early nor torn.
#[test]
#[ignore = "needs root"]
fn a_failed_rotation_keeps_the_files_length() {
    let runtime = Runtime::new();
    let id = "rotation-blocked";
    let bundle = bundle_running(
        "i=0; while [ $i -lt 5000 ]; do echo line-$i-0123456789012345678901234567890123456789; \
         i=$((i+1)); done; exec sleep 60",
    );
    let max = 64 * 1024;
    let limit = 3 * max;
    // The soft limit only (`ulimit -S -f`): the monitor's writes meet it;
    // shipyard's sealed copy of itself doesn't.
    let command = file_size_limited(limit, libc::RLIM_INFINITY);
    let dir = runtime.log_dir(id);
    let max_text = max.to_string();
    let created = runtime.create_with(
        id,
        &bundle,
        &[
            "--log-dir",
            dir.to_str().unwrap(),
            "--log-max-size",
            &max_text,
        ],
        command,
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    // In the way of every rotation: its new file can't be made.
    std::fs::create_dir(dir.join(".container.log.next")).unwrap();
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    let current = dir.join("container.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut size = 0;
    while Instant::now() < deadline {
        size = std::fs::metadata(&current).unwrap().len();
        if size > 2 * max {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let the writes past the limit come (cut short, then refused).
    std::thread::sleep(Duration::from_millis(500));
    let size_after = std::fs::metadata(&current).unwrap().len();
    assert!(
        size_after > 2 * max,
        "cut back to {size_after} (it reached {size})"
    );
    assert!(size_after <= limit, "{size_after}");
    // The limit was reached: the last record didn't fit.
    assert!(size_after > limit - 1024, "{size_after} of {limit}");
    let text = std::fs::read_to_string(&current).unwrap();
    assert!(text.ends_with('\n'), "a whole record last");
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// A hard file-size limit (bash's `ulimit -f` sets both) smaller than
/// shipyard's binary leaves no room for its sealed copy: a root container,
/// whose root could write the binary's file, is refused, and the reason
/// names the limit, not the host's memfd policy.
#[test]
#[ignore = "needs root"]
fn a_hard_file_size_limit_below_the_binary_is_named() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 60");
    let command = file_size_limited(1024 * 1024, 1024 * 1024);
    let created = runtime.create_with("small-fsize", &bundle, &[], command);
    assert_eq!(created.code, Some(1), "{}", created.stderr);
    assert!(
        created
            .stderr
            .contains("a file-size limit (ulimit -f) smaller than the binary"),
        "{}",
        created.stderr
    );
    assert_eq!(runtime.status("small-fsize"), "gone");
}

/// `container.log` cut short by hand (`: > container.log`) while the
/// monitor writes it: a write a file-size limit then cuts short is cut back
/// from the file's own length, not from what the writer thought it was, so
/// the file ends with a whole record and has no hole of NUL bytes.
#[test]
#[ignore = "needs root"]
fn a_file_truncated_outside_gets_no_hole() {
    let runtime = Runtime::new();
    let id = "truncated-outside";
    let line = "0123456789012345678901234567890123456789";
    let bundle = bundle_running(&format!(
        "i=0; while [ $i -lt 2000 ]; do echo first-$i-{line}; i=$((i+1)); done; sleep 2; \
         i=0; while [ $i -lt 6000 ]; do echo second-$i-{line}; i=$((i+1)); done; exec sleep 60"
    ));
    let max = 64 * 1024;
    // Off every record's end: the output is the same each run, so its
    // records end at the same offsets, and one ends at 3 * max exactly (a
    // limit there is met by a whole record, and nothing is ever cut back).
    let limit = 3 * max + 50;
    // The soft limit only (`ulimit -S -f`): the monitor's writes meet it;
    // shipyard's sealed copy of itself doesn't.
    let command = file_size_limited(limit, libc::RLIM_INFINITY);
    let dir = runtime.log_dir(id);
    let max_text = max.to_string();
    let created = runtime.create_with(
        id,
        &bundle,
        &[
            "--log-dir",
            dir.to_str().unwrap(),
            "--log-max-size",
            &max_text,
        ],
        command,
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    // Every rotation fails: the file grows to the limit.
    std::fs::create_dir(dir.join(".container.log.next")).unwrap();
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    let current = dir.join("container.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&current)
        .unwrap()
        .contains("first-1999-")
    {
        assert!(Instant::now() < deadline, "the first lines never came");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Cut short by hand, while the program sleeps.
    std::fs::File::options()
        .write(true)
        .open(&current)
        .unwrap()
        .set_len(0)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut size = 0;
    while Instant::now() < deadline {
        size = std::fs::metadata(&current).unwrap().len();
        if size > limit - 1024 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let the writes past the limit come (cut short, then refused).
    std::thread::sleep(Duration::from_millis(500));
    let bytes = std::fs::read(&current).unwrap();
    assert!(size > limit - 1024, "the limit was never reached ({size})");
    assert!(!bytes.contains(&0), "a hole of NUL bytes");
    // Short of the limit: the record that met it was cut back.
    assert!((bytes.len() as u64) < limit, "{}", bytes.len());
    assert!(bytes.ends_with(b"\n"), "a whole record last");
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}

/// A failed `create` removes the logs it made through descriptors, never by
/// their path: the log directory's parent swapped for a symlink to another
/// directory meanwhile (by whoever may write where it is) doesn't make it
/// remove that one's `logs`. The one it made, moved with its parent, goes.
#[test]
#[ignore = "needs root"]
fn a_failed_creates_logs_go_by_descriptor_not_path() {
    let runtime = Runtime::new();
    let base = runtime.output_dir().join("swap");
    let parent = base.join("parent");
    let victim = base.join("victim/logs");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("precious"), "keep me").unwrap();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["hooks"] = json!({"createRuntime": [{"path": "/bin/sh", "args": ["sh", "-c", format!(
        "mv {parent} {moved} && ln -s {victim} {parent}; exit 1",
        parent = parent.display(),
        moved = base.join("moved").display(),
        victim = base.join("victim").display(),
    )]}]});
    bundle.write(&config);
    let log_dir = parent.join("logs");
    let call = runtime.create(
        "swapped",
        &bundle,
        &["--log-dir", log_dir.to_str().unwrap()],
    );
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        victim.join("precious").exists(),
        "another directory's logs were removed"
    );
    assert!(!base.join("moved/logs").exists(), "its own were kept");
}

/// What the container wrote is in its logs, each line on its stream, and
/// `logs` gives it back on the same streams; `create`'s own output gets
/// none of it. The directory is the owner's alone, and versioned.
#[test]
#[ignore = "needs root"]
fn logs_hold_what_the_container_wrote() {
    use std::os::unix::fs::PermissionsExt as _;
    let runtime = Runtime::new();
    let bundle = bundle_running("echo out1; echo err1 >&2; echo out2; printf no-newline");
    let logs = runtime.run_logged("logged", &bundle, &[]);
    assert_eq!(logs.code, Some(0), "{}", logs.stderr);
    assert_eq!(logs.stdout, "out1\nout2\nno-newline\n");
    assert_eq!(logs.stderr, "err1\n");
    assert_eq!(
        runtime.output_of("logged"),
        "",
        "create's stdout got none of it"
    );
    let dir = runtime.log_dir("logged");
    let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.clone()), 0o700);
    assert_eq!(mode(dir.join("container.log")), 0o600);
    let info: Value =
        serde_json::from_slice(&std::fs::read(dir.join("log.json")).unwrap()).unwrap();
    assert_eq!(
        (info["format"].as_str(), info["version"].as_u64()),
        (Some("cri"), Some(1))
    );
    // Pure CRI: every line a record.
    let text = std::fs::read_to_string(dir.join("container.log")).unwrap();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(4, ' ').collect();
        assert!(fields[0].ends_with('Z') && fields[0].len() == 30, "{line}");
        assert!(["stdout", "stderr"].contains(&fields[1]), "{line}");
        assert!(["F", "P"].contains(&fields[2]), "{line}");
    }
    // The logs outlive the container, and are read by their directory.
    assert_eq!(runtime.call(&["delete", "logged"]).code, Some(0));
    let after = runtime.call(&["logs", "--log-dir", dir.to_str().unwrap()]);
    assert_eq!(after.stdout, "out1\nout2\nno-newline\n", "{}", after.stderr);
}

/// `--tail`, `--since` and `--timestamps`.
#[test]
#[ignore = "needs root"]
fn logs_tail_since_and_timestamps() {
    let runtime = Runtime::new();
    let bundle = bundle_running("for n in 1 2 3 4 5; do echo line$n; done");
    let all = runtime.run_logged("picked", &bundle, &["--tail", "2"]);
    assert_eq!(all.stdout, "line4\nline5\n", "{}", all.stderr);
    let stamped = runtime.call(&["logs", "--timestamps", "--tail", "1", "picked"]);
    let (time, text) = stamped.stdout.trim_end().split_once(' ').unwrap();
    assert_eq!(text, "line5");
    assert!(time.len() == 30 && time.ends_with('Z'), "{time}");
    let recent = runtime.call(&["logs", "--since", "1h", "picked"]);
    assert_eq!(recent.stdout.lines().count(), 5);
    let future = runtime.call(&["logs", "--since", "2999-01-01T00:00:00Z", "picked"]);
    assert_eq!(future.stdout, "");
    // With --follow too, the tail is of lines.
    let followed = runtime.call_within(
        &["logs", "--tail", "1", "--follow", "picked"],
        Duration::from_secs(5),
    );
    assert_eq!(followed.stdout, "line5\n", "{}", followed.stderr);
    let bad = runtime.call(&["logs", "--since", "yesterday", "picked"]);
    assert_ne!(bad.code, Some(0));
}

/// `logs --follow` prints lines as they come, across rotations, and ends
/// by itself once the container has ended and everything it wrote is read.
#[test]
#[ignore = "needs root"]
fn follow_goes_on_across_rotations_until_the_end() {
    let runtime = Runtime::new();
    // About 220 KB: three rotations at 64 KiB, so the first file has left
    // the chain of 3 by the end (the follower reads on through the
    // descriptor it holds).
    let bundle = bundle_running(
        "n=0; while [ $n -lt 3000 ]; do echo \"line $n of the flood............\"; \
         n=$((n+1)); done",
    );
    let (created, _) = runtime.create_logged(
        "followed",
        &bundle,
        &["--log-max-size", "64k", "--log-max-files", "3"],
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let mut follower = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["logs", "--follow", "followed"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Read as it comes: a pipe not read would fill and stop the follower.
    let mut stdout = follower.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut out).unwrap();
        out
    });
    assert_eq!(runtime.call(&["start", "followed"]).code, Some(0));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = follower.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "logs --follow never ended");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    let out = reader.join().unwrap();
    let numbers: Vec<u32> = out
        .lines()
        .map(|line| line.split(' ').nth(1).unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        numbers,
        (0..3000).collect::<Vec<_>>(),
        "every line, once, in order"
    );
    let rotated = runtime.log_dir("followed").join("container.log.1");
    assert!(rotated.exists(), "it did rotate");
}

/// A flood with no newline at all: cut into 16 KiB partial records, and
/// the files never hold more than their bound.
#[test]
#[ignore = "needs root"]
fn a_flood_stays_within_the_rotation_bound() {
    let runtime = Runtime::new();
    let bundle = bundle_running("head -c 3000000 /dev/zero");
    let (created, dir) = runtime.create_logged(
        "flood",
        &bundle,
        &["--log-max-size", "64k", "--log-max-files", "3"],
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "flood"]).code, Some(0));
    runtime.wait_for("flood", "stopped");
    let mut total = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let size = entry.metadata().unwrap().len();
        if name.starts_with("container.log") {
            assert!(size <= 64 * 1024 + 16 * 1024 + 64, "{name}: {size}");
            total += size;
        }
    }
    assert!(total <= 3 * (64 * 1024 + 16 * 1024 + 64), "{total}");
    let text = std::fs::read(dir.join("container.log")).unwrap();
    let records: Vec<&[u8]> = text
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    assert!(records.len() > 1);
    for record in &records[..records.len() - 1] {
        assert_eq!(
            record.splitn(4, |&b| b == b' ').nth(2),
            Some(&b"P"[..]),
            "partials"
        );
        assert_eq!(
            record.len(),
            30 + 1 + 6 + 1 + 1 + 1 + 16 * 1024,
            "16 KiB of text each"
        );
    }
}

/// A line longer than a record holds is cut into partials, and `logs`
/// joins it back into the line.
#[test]
#[ignore = "needs root"]
fn a_long_line_is_joined_back() {
    let runtime = Runtime::new();
    let logs = runtime.run_logged(
        "long-line",
        &bundle_running("head -c 40000 /dev/zero; echo"),
        &[],
    );
    assert_eq!(logs.stdout.len(), 40001, "{}", logs.stderr);
    assert!(logs.stdout.ends_with('\n'));
    let text = std::fs::read_to_string(runtime.log_dir("long-line").join("container.log")).unwrap();
    let tags: Vec<&str> = text
        .lines()
        .map(|line| line.split(' ').nth(2).unwrap())
        .collect();
    assert_eq!(tags, ["P", "P", "F"]);
}

/// A container with a terminal and logs: the terminal's output is its
/// stdout in the logs (no console socket needed).
#[test]
#[ignore = "needs root"]
fn a_terminal_goes_into_the_logs() {
    let runtime = Runtime::new();
    let logs = runtime.run_logged("tty-logged", &terminal_bundle("tty; echo hi", None), &[]);
    assert!(
        logs.stdout.contains("/dev/pts/0"),
        "{} {}",
        logs.stdout,
        logs.stderr
    );
    // The terminal's own newline: `\r\n` (ONLCR), as documented.
    assert!(logs.stdout.contains("hi\r\n"), "{:?}", logs.stdout);
}

/// A `createContainer` hook that writes more than a pipe holds: its output
/// goes into the logs while `create` waits, and `create` returns.
#[test]
#[ignore = "needs root"]
fn a_chatty_hook_doesnt_hang_create() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "echo ran"]);
    config["hooks"] = json!({"createContainer": [{
        "path": "/bin/sh",
        "args": ["sh", "-c", "head -c 1000000 /dev/zero; echo hook-done"]
    }]});
    bundle.write(&config);
    let (created, _) = runtime.create_logged("chatty-hook", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "chatty-hook"]).code, Some(0));
    runtime.wait_for("chatty-hook", "stopped");
    let logs = runtime.call(&["logs", "chatty-hook"]);
    assert!(logs.stdout.contains("hook-done\nran\n"), "{}", logs.stderr);
}

/// With logs, `create`'s caller gets its own stdout and stderr back when
/// `create` returns: a reader of them sees their end before `start`.
#[test]
#[ignore = "needs root"]
fn create_with_logs_lets_go_of_its_callers_output() {
    use std::io::Read as _;
    let runtime = Runtime::new();
    let dir = runtime.log_dir("piped-logs");
    let bundle = bundle_running("echo ran");
    let mut create = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["create", "--bundle", bundle.arg(), "--log-dir"])
        .arg(&dir)
        .arg("piped-logs")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stdout, mut stderr) = (create.stdout.take().unwrap(), create.stderr.take().unwrap());
    let (done, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = stderr.read_to_string(&mut text);
        let _ = done.send(text);
    });
    let output = ended
        .recv_timeout(Duration::from_secs(10))
        .expect("create's output never reached its end");
    assert!(create.wait().unwrap().success(), "{output}");
    assert_eq!(runtime.status("piped-logs"), "created");
}

/// When the monitor dies, the container's init dies with it (its parent-
/// death signal); a process outside the init (no PID namespace) then gets
/// a broken pipe (EPIPE): nothing reads its output any more. What was
/// written before is in the logs, whole records.
#[test]
#[ignore = "needs root"]
fn a_dead_monitor_means_a_broken_pipe() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] = json!([{"type": "mount"}]);
    config["process"]["args"] = json!([
        "sh",
        "-c",
        "(trap '' PIPE; while echo tick; do sleep 0.1; done; echo broken > /tmp/epipe) & exec sleep 300"
    ]);
    bundle.write(&config);
    let (created, dir) = runtime.create_logged("orphaned", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "orphaned"]).code, Some(0));
    std::thread::sleep(Duration::from_millis(600));
    let init = runtime.state("orphaned").unwrap()["pid"].as_i64().unwrap();
    let monitor = parent_of(i32::try_from(init).unwrap()).unwrap();
    // SAFETY: `kill` has no memory-safety preconditions.
    assert_eq!(unsafe { libc::kill(monitor, libc::SIGKILL) }, 0);
    let marker = bundle.root.host_path("/tmp/epipe");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the writer never got EPIPE");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(init), "the init died with its monitor");
    let logs = runtime.call(&["logs", "--log-dir", dir.to_str().unwrap()]);
    assert!(logs.stdout.lines().count() >= 3, "{}", logs.stdout);
    assert!(
        logs.stdout.lines().all(|line| line == "tick"),
        "{}",
        logs.stdout
    );
    assert_eq!(logs.stderr, "", "nothing skipped: whole records");
    // Nothing more can come: following ends.
    let followed = runtime.call_within(&["logs", "--follow", "orphaned"], Duration::from_secs(5));
    assert_eq!(followed.code, Some(0), "{}", followed.stderr);
}

/// Something that escaped the init (no PID namespace) and holds the output
/// open doesn't keep the exit unrecorded: the cgroup goes first, then the
/// output is drained, then the exit is recorded.
#[test]
#[ignore = "needs root"]
fn the_exit_is_recorded_while_a_child_holds_the_pipe() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] = json!([{"type": "mount"}]);
    // The escaped one leaves part of a line on stderr, which only the drain
    // (after the cgroup's end) can write.
    config["process"]["args"] = json!([
        "sh",
        "-c",
        "(printf held-back >&2; exec sleep 60) & sleep 0.3; echo bye"
    ]);
    bundle.write(&config);
    let (created, _) = runtime.create_logged("escaped", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let started = Instant::now();
    assert_eq!(runtime.call(&["start", "escaped"]).code, Some(0));
    let _ = runtime.exit_record("escaped");
    // Under the drain's own limit (2 s): the cgroup went first, and with it
    // the escaped `sleep`'s hold on the output.
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    let logs = runtime.call(&["logs", "escaped"]);
    assert_eq!(logs.stdout, "bye\n");
    assert_eq!(logs.stderr, "held-back\n");
}

/// The monitor stays small while it logs a flood (H7, FEATURES' 1 MB per
/// monitor): its own memory under 1 MiB, and 30 MB logged within 10
/// seconds. Its own memory is its anonymous pages (heap, stack): RSS counts
/// the binary's shared text, and `Private_Dirty` also counts a just-built
/// binary's pages not yet written back (8.1a's probe found 1.7 MB of that).
#[test]
#[ignore = "needs root"]
fn the_monitor_stays_small_while_it_logs() {
    let runtime = Runtime::new();
    let bundle = bundle_running("head -c 30000000 /dev/zero; echo flooded >&2; exec sleep 60");
    let (created, dir) = runtime.create_logged("small", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let init = runtime.state("small").unwrap()["pid"].as_i64().unwrap();
    let monitor = parent_of(i32::try_from(init).unwrap()).unwrap();
    let started = Instant::now();
    assert_eq!(runtime.call(&["start", "small"]).code, Some(0));
    let mut most = 0;
    loop {
        let rollup = std::fs::read_to_string(format!("/proc/{monitor}/smaps_rollup")).unwrap();
        let anonymous: u64 = rollup
            .lines()
            .find_map(|line| line.strip_prefix("Anonymous:"))
            .unwrap()
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .unwrap();
        most = most.max(anonymous);
        let logged = std::fs::read_to_string(dir.join("container.log")).unwrap_or_default();
        if logged.contains("stderr F flooded") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "30 MB not logged in 10 s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(most < 1024, "the monitor's own memory reached {most} kB");
    eprintln!(
        "the monitor's own memory: at most {most} kB; logged in {:?}",
        started.elapsed()
    );
}

/// A `create` that fails after its container wrote something (a hook that
/// failed) keeps the logs, saying where: what the hook said is there.
#[test]
#[ignore = "needs root"]
fn a_failed_create_keeps_what_was_written() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["hooks"] = json!({"createContainer": [{
        "path": "/bin/sh",
        "args": ["sh", "-c", "echo the-hook-says-why >&2; exit 3"]
    }]});
    bundle.write(&config);
    let (created, dir) = runtime.create_logged("failing-logged", &bundle, &[]);
    assert_eq!(created.code, Some(1), "{}", created.stderr);
    assert!(
        created.stderr.contains("its output so far is in"),
        "{}",
        created.stderr
    );
    let logs = runtime.call(&["logs", "--log-dir", dir.to_str().unwrap()]);
    assert!(logs.stderr.contains("the-hook-says-why"), "{}", logs.stderr);
    // Nothing written: nothing kept.
    let quiet = Bundle::new();
    let mut config = quiet.config();
    config["hooks"] =
        json!({"createContainer": [{"path": "/bin/sh", "args": ["sh", "-c", "exit 3"]}]});
    quiet.write(&config);
    let (created, dir) = runtime.create_logged("failing-unlogged", &quiet, &[]);
    assert_eq!(created.code, Some(1));
    assert!(!dir.exists());
}

/// `logs` into a pipe its reader closed early (`logs ID | head`) ends
/// quietly, with no error.
#[test]
#[ignore = "needs root"]
fn logs_into_a_closed_pipe_end_quietly() {
    let runtime = Runtime::new();
    let bundle = bundle_running("n=0; while [ $n -lt 20000 ]; do echo line $n; n=$((n+1)); done");
    let (created, _) = runtime.create_logged("headed", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "headed"]).code, Some(0));
    runtime.wait_for("headed", "stopped");
    let mut logs = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["logs", "headed"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(logs.stdout.take());
    let output = logs.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

/// `shipyard-runtime logs` ends as its readers do (F70): 1 when stderr's
/// reader goes while stdout's is read.
#[test]
#[ignore = "needs root"]
fn runtime_logs_end_as_their_readers_do() {
    let runtime = Runtime::new();
    let bundle = bundle_running(
        "n=0; while [ $n -lt 20000 ]; do echo out $n; echo err $n >&2; n=$((n+1)); done",
    );
    let (created, _) = runtime.create_logged("both", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "both"]).code, Some(0));
    runtime.wait_for("both", "stopped");
    shipyard_test_support::pipes::logs_end_as_their_readers_do(
        || {
            let mut command = Command::new(RUNTIME.path());
            command
                .arg("--root")
                .arg(&runtime.root)
                .args(["logs", "both"]);
            command
        },
        1,
    );
}

/// `logs --log-dir` reads a directory: no state root is needed (none
/// without `XDG_RUNTIME_DIR`, for anyone but root).
#[test]
fn logs_of_a_directory_need_no_state_root() {
    let dir = std::env::temp_dir().join(format!("shipyard-logdir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("log.json"),
        r#"{"format":"cri","version":1,"maxSize":65536,"maxFiles":2}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("container.log"),
        "2026-10-10T06:16:19.000000000Z stdout F from-the-file\n",
    )
    .unwrap();
    let output = Command::new(RUNTIME.path())
        .args(["logs", "--log-dir", dir.to_str().unwrap()])
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "from-the-file\n");
}

// --- Attach (8.3c) ---------------------------------------------------------------------

/// `create --log-dir --stdin` of `script`, started. Its bundle, to keep as
/// long as the container runs.
#[must_use = "the bundle's files go when it's dropped: keep it as long as the container"]
fn attachable(runtime: &Runtime, id: &str, script: &str, extra: &[&str]) -> Bundle {
    let bundle = bundle_running(script);
    let mut args = vec!["--stdin"];
    args.extend_from_slice(extra);
    let (created, _) = runtime.create_logged(id, &bundle, &args);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", id]).code, Some(0));
    bundle
}

/// `shipyard-runtime attach ARGS… ID`, stdin and stdout piped; stdout read
/// as it comes (a thread), so a test can wait for text.
struct Attached {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    output: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Attached {
    fn start(runtime: &Runtime, id: &str, args: &[&str]) -> Self {
        Self::running(runtime, &[&["attach"], args, &[id]].concat())
    }

    /// Any `shipyard-runtime --root ROOT ARGS…`, read the same way
    /// (`logs --follow`, say).
    fn running(runtime: &Runtime, args: &[&str]) -> Self {
        let mut child = Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&runtime.root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut stdout = child.stdout.take().unwrap();
        let shared = output.clone();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = std::io::Read::read(&mut stdout, &mut buffer) {
                if read == 0 {
                    break;
                }
                shared
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            output,
        }
    }

    fn type_keys(&mut self, keys: &[u8]) {
        std::io::Write::write_all(self.stdin.as_mut().unwrap(), keys).unwrap();
    }

    fn expect(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.output.lock().unwrap().contains(text) {
            assert!(
                Instant::now() < deadline,
                "never saw {text:?}: {:?}",
                self.output.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits (up to 10 s) for it to end: its exit code and stderr.
    fn end(mut self) -> (Option<i32>, String) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("attach didn't end: {:?}", self.output.lock().unwrap());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        (status.code(), stderr)
    }
}

const DETACH: &[u8] = b"\x10\x11";

/// A raw connection to container `id`'s `attach.sock` that has said hello
/// (no detach keys) and, if `frames`, sent them in the same write.
fn raw_client(
    runtime: &Runtime,
    id: &str,
    frames: &[shipyard_runtime::attach::Frame],
) -> UnixStream {
    use shipyard_runtime::attach::{Frame, VERSION};
    let mut stream = UnixStream::connect(runtime.root.join(id).join("attach.sock")).unwrap();
    let mut bytes = Vec::new();
    Frame::Hello {
        version: VERSION,
        detach_keys: Vec::new(),
    }
    .encode(&mut bytes);
    for frame in frames {
        frame.encode(&mut bytes);
    }
    std::io::Write::write_all(&mut stream, &bytes).unwrap();
    stream
}

/// The frames read from `stream` until `stop` says so, or it ends (up to 10 s).
fn frames_until(
    stream: &mut UnixStream,
    stop: impl Fn(&shipyard_runtime::attach::Frame) -> bool,
) -> Vec<shipyard_runtime::attach::Frame> {
    let mut decoder = shipyard_runtime::attach::Decoder::new();
    let mut frames = Vec::new();
    let mut buffer = vec![0_u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(10);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    loop {
        while let Some(frame) = decoder.next_frame().unwrap() {
            let done = stop(&frame);
            frames.push(frame);
            if done {
                return frames;
            }
        }
        assert!(Instant::now() < deadline, "{} frames", frames.len());
        match std::io::Read::read(stream, &mut buffer) {
            Ok(0) | Err(_) => return frames,
            Ok(read) => decoder.push(&buffer[..read]),
        }
    }
}

/// What's typed reaches the container and its output comes back; the
/// detach keys leave (exit 0) and the container goes on, its stdin open:
/// attached again, typing still reaches it.
#[test]
#[ignore = "needs root"]
fn attach_relays_and_detaching_leaves_stdin_open() {
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-talker",
        "while read line; do echo got-$line; done",
        &[],
    );
    let mut first = Attached::start(&runtime, "attach-talker", &[]);
    first.type_keys(b"one\n");
    first.expect("got-one");
    first.type_keys(DETACH);
    assert_eq!(first.end().0, Some(0));
    assert_eq!(runtime.status("attach-talker"), "running");
    let mut second = Attached::start(&runtime, "attach-talker", &[]);
    second.type_keys(b"two\n");
    second.expect("got-two");
    second.type_keys(DETACH);
    assert_eq!(second.end().0, Some(0));
    let logs = runtime.call(&["logs", "attach-talker"]);
    assert_eq!(logs.stdout, "got-one\ngot-two\n");
}

/// With `--stdin-once`, the first client going closes the container's
/// stdin: its program sees the end of its input.
#[test]
#[ignore = "needs root"]
fn stdin_once_closes_stdin_when_the_first_client_goes() {
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-once",
        "cat; echo input-ended",
        &["--stdin-once"],
    );
    let mut client = Attached::start(&runtime, "attach-once", &[]);
    client.type_keys(b"line\n");
    client.expect("line");
    client.type_keys(DETACH);
    assert_eq!(client.end().0, Some(0));
    runtime.wait_for("attach-once", "stopped");
    assert_eq!(
        runtime.call(&["logs", "attach-once"]).stdout,
        "line\ninput-ended\n"
    );
}

/// With `--stdin-once`, the end of attach's own input (`attach < file`)
/// ends the container's: its program sees what was typed, then the end.
#[test]
#[ignore = "needs root"]
fn the_end_of_attach_input_ends_stdin_once() {
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-piped",
        "cat; echo input-ended",
        &["--stdin-once"],
    );
    let mut client = Attached::start(&runtime, "attach-piped", &[]);
    client.type_keys(b"hi\n");
    drop(client.stdin.take());
    client.expect("input-ended");
    assert_eq!(client.end().0, Some(0));
    assert_eq!(
        runtime.call(&["logs", "attach-piped"]).stdout,
        "hi\ninput-ended\n"
    );
}

/// With `--stdin-once`, a client that only watches (`--no-stdin`) isn't the
/// typist: its going leaves stdin open; the first client to type is.
#[test]
#[ignore = "needs root"]
fn a_watcher_doesnt_end_stdin_once() {
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-watched",
        "(while :; do echo tick; sleep 0.2; done) & \
         while read line; do echo got-$line; done; echo input-ended; kill $!",
        &["--stdin-once"],
    );
    let mut watcher = Attached::start(&runtime, "attach-watched", &["--no-stdin"]);
    watcher.expect("tick");
    watcher.child.kill().unwrap();
    let _ = watcher.child.wait();
    let mut typist = Attached::start(&runtime, "attach-watched", &[]);
    typist.type_keys(b"x\n");
    typist.expect("got-x");
    typist.type_keys(DETACH);
    assert_eq!(typist.end().0, Some(0));
    runtime.wait_for("attach-watched", "stopped");
    let logs = runtime.call(&["logs", "attach-watched"]).stdout;
    assert!(logs.contains("got-x\ninput-ended\n"), "{logs}");
}

/// With `--stdin-once`, typing still queued when the typist goes (the
/// container not reading yet) is written before the end of input.
#[test]
#[ignore = "needs root"]
fn stdin_once_writes_whats_queued_first() {
    use shipyard_runtime::attach::Frame;
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-queued",
        "sleep 2; wc -c; echo input-ended",
        &["--stdin-once"],
    );
    // More than the pipe holds (64 KiB), less than it and the queue.
    let typed = vec![b'x'; 100_000];
    let mut client = raw_client(&runtime, "attach-queued", &[Frame::Stdin(typed)]);
    let frames = frames_until(&mut client, |frame| matches!(frame, Frame::Welcome { .. }));
    assert!(
        matches!(frames.last(), Some(Frame::Welcome { .. })),
        "{frames:?}"
    );
    drop(client);
    runtime.wait_for("attach-queued", "stopped");
    assert_eq!(
        runtime.call(&["logs", "attach-queued"]).stdout,
        "100000\ninput-ended\n"
    );
}

/// What a client sends after its detach keys, in the same read, isn't
/// passed on: it has left.
#[test]
#[ignore = "needs root"]
fn nothing_after_the_detach_keys_is_passed_on() {
    use shipyard_runtime::attach::{Frame, VERSION};
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-left",
        "while read line; do echo got-$line; done",
        &[],
    );
    let mut stream = UnixStream::connect(runtime.root.join("attach-left/attach.sock")).unwrap();
    let mut bytes = Vec::new();
    Frame::Hello {
        version: VERSION,
        detach_keys: DETACH.to_vec(),
    }
    .encode(&mut bytes);
    Frame::Stdin(b"a\x10\x11".to_vec()).encode(&mut bytes);
    Frame::Stdin(b"after\n".to_vec()).encode(&mut bytes);
    std::io::Write::write_all(&mut stream, &bytes).unwrap();
    let frames = frames_until(&mut stream, |frame| matches!(frame, Frame::Detached));
    assert!(matches!(frames.last(), Some(Frame::Detached)), "{frames:?}");
    // What came before the keys was passed on: `a`, then this.
    let mut client = Attached::start(&runtime, "attach-left", &[]);
    client.type_keys(b"check\n");
    client.expect("got-acheck");
    client.type_keys(DETACH);
    assert_eq!(client.end().0, Some(0));
    assert!(
        !runtime
            .call(&["logs", "attach-left"])
            .stdout
            .contains("after")
    );
}

/// `attach` ends with the container, with its exit code.
#[test]
#[ignore = "needs root"]
fn attach_ends_with_the_containers_exit_code() {
    let runtime = Runtime::new();
    let _bundle = attachable(&runtime, "attach-seven", "read go; echo bye; exit 7", &[]);
    let mut client = Attached::start(&runtime, "attach-seven", &[]);
    client.type_keys(b"go\n");
    client.expect("bye");
    assert_eq!(client.end().0, Some(7));
    // Attached to once it has ended: its code all the same.
    let late = runtime.call(&["attach", "attach-seven"]);
    assert_eq!(late.code, Some(255), "{}", late.stderr);
    assert!(late.stderr.contains("is stopped"), "{}", late.stderr);
}

/// A connection to `attach.sock` that says hello and never reads: the
/// container's flood goes on unslowed (it ends on time), and that client is
/// let go.
#[test]
#[ignore = "needs root"]
fn a_stalled_client_doesnt_slow_the_container() {
    use shipyard_runtime::attach::{Frame, VERSION};
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 1; head -c 30000000 /dev/zero; echo done");
    let (created, _) = runtime.create_logged("attach-flooded", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let mut stalled =
        std::os::unix::net::UnixStream::connect(runtime.root.join("attach-flooded/attach.sock"))
            .unwrap();
    let mut hello = Vec::new();
    Frame::Hello {
        version: VERSION,
        detach_keys: Vec::new(),
    }
    .encode(&mut hello);
    std::io::Write::write_all(&mut stalled, &hello).unwrap();
    let started = Instant::now();
    assert_eq!(runtime.call(&["start", "attach-flooded"]).code, Some(0));
    runtime.wait_for("attach-flooded", "stopped");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // Let go: what it was sent ends (no more than a socket's buffer and a
    // client's allowance), then end-of-file. With `Dropped`, or (under
    // load, its socket full by then) without it, as
    // `a_dropped_client_doesnt_keep_the_monitor_busy` expects; that it's
    // sent at all is the attach unit
    // `a_client_too_far_behind_is_dropped_with_a_frame`'s.
    let frames = frames_until(&mut stalled, |_| false);
    let output: usize = frames
        .iter()
        .map(|frame| match frame {
            Frame::Stdout(bytes) => bytes.len(),
            _ => 0,
        })
        .sum();
    assert!(output < 4 * 1024 * 1024, "{output}");
    match frames.last() {
        Some(Frame::Dropped(reason)) => assert!(reason.contains("256 KiB"), "{reason}"),
        Some(Frame::Stdout(_)) => {}
        other => panic!("{other:?}"),
    }
}

/// A client let go (fallen behind) whose socket is full, so that even its
/// `Dropped` can't be written, and that keeps sending: the monitor stops
/// reading it rather than waking for it over and over, and closes it once
/// its time to leave is up (its `Dropped` never sent).
#[test]
#[ignore = "needs root"]
fn a_dropped_client_doesnt_keep_the_monitor_busy() {
    use shipyard_runtime::attach::Frame;
    let runtime = Runtime::new();
    // A trickle (8 KB at a time), not a flood: the socket fills first, then
    // the client's allowance, so its `Dropped` waits behind a full socket.
    let bundle = bundle_running(
        "sleep 1; i=0; while [ $i -lt 100 ]; do head -c 8192 /dev/zero; i=$((i+1)); done; \
         exec sleep 60",
    );
    let (created, _) = runtime.create_logged("attach-busy", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let mut stalled = raw_client(&runtime, "attach-busy", &[]);
    let started = Instant::now();
    assert_eq!(runtime.call(&["start", "attach-busy"]).code, Some(0));
    let init = runtime.state("attach-busy").unwrap()["pid"]
        .as_i64()
        .unwrap();
    let monitor = parent_of(i32::try_from(init).unwrap()).unwrap();
    let cpu = || {
        let stat = std::fs::read_to_string(format!("/proc/{monitor}/stat")).unwrap();
        let fields: Vec<&str> = stat.rsplit_once(") ").unwrap().1.split(' ').collect();
        // utime and stime, in clock ticks (fields 14 and 15).
        fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
    };
    // Dropped about a second in; from 2 s to 5 s, it sends a frame every
    // 100 ms, never reading.
    stalled.set_nonblocking(true).unwrap();
    let mut resize = Vec::new();
    Frame::Resize { rows: 1, cols: 1 }.encode(&mut resize);
    std::thread::sleep(Duration::from_secs(2).saturating_sub(started.elapsed()));
    let before = cpu();
    for _ in 0..30 {
        let _ = std::io::Write::write(&mut stalled, &resize);
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: `sysconf` has no preconditions.
    let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap();
    let used = cpu() - before;
    assert!(used < ticks / 2, "{used} ticks of {ticks}/s in 3 s");
    // Its 5 s to leave are up by 8 s: closed. It reads what reached its
    // socket, then end-of-file at once; its `Dropped` never came.
    std::thread::sleep(Duration::from_secs(8).saturating_sub(started.elapsed()));
    stalled.set_nonblocking(false).unwrap();
    let reading = Instant::now();
    let frames = frames_until(&mut stalled, |_| false);
    assert!(reading.elapsed() < Duration::from_secs(2), "not closed");
    assert!(
        matches!(frames.last(), Some(Frame::Stdout(_))),
        "{} frames, the last {:?}",
        frames.len(),
        frames
            .last()
            .map(|frame| matches!(frame, Frame::Dropped(_)))
    );
}

/// The 17th client is refused, saying so; the 16 before are let in.
#[test]
#[ignore = "needs root"]
fn the_seventeenth_client_is_refused() {
    use shipyard_runtime::attach::{Decoder, Frame, VERSION};
    let runtime = Runtime::new();
    let _bundle = attachable(&runtime, "attach-crowded", "exec sleep 60", &[]);
    let socket = runtime.root.join("attach-crowded/attach.sock");
    let answer = |stream: &mut std::os::unix::net::UnixStream| {
        let mut hello = Vec::new();
        Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        }
        .encode(&mut hello);
        std::io::Write::write_all(stream, &hello).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut decoder = Decoder::new();
        let mut buffer = [0_u8; 1024];
        loop {
            if let Some(frame) = decoder.next_frame().unwrap() {
                return frame;
            }
            let read = std::io::Read::read(stream, &mut buffer).unwrap();
            assert_ne!(read, 0, "closed without an answer");
            decoder.push(&buffer[..read]);
        }
    };
    let mut kept = Vec::new();
    for _ in 0..16 {
        let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        assert!(matches!(answer(&mut stream), Frame::Welcome { .. }));
        kept.push(stream);
    }
    let mut seventeenth = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    match answer(&mut seventeenth) {
        Frame::Refused(reason) => assert!(reason.contains("16 clients"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // As many waiting to say hello: one more is refused before its hello.
    let silent: Vec<_> = (0..16)
        .map(|_| std::os::unix::net::UnixStream::connect(&socket).unwrap())
        .collect();
    let mut one_more = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    match frames_until(&mut one_more, |_| true).first() {
        Some(Frame::Refused(reason)) => {
            assert!(reason.contains("waiting to say hello"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    // The CLI says it too.
    let refused = runtime.call(&["attach", "attach-crowded"]);
    assert_eq!(refused.code, Some(255));
    assert!(
        refused.stderr.contains("waiting to say hello"),
        "{}",
        refused.stderr
    );
    drop(silent);
}

/// Another user can't attach: the monitor checks who connected (its peer
/// credentials), whatever the socket's mode. The state directory and socket
/// are opened up here to let `nobody` reach the socket at all (the CLI
/// would refuse root's state root anyway); its connection is closed without
/// a welcome. Root's attach (the control) works.
#[test]
#[ignore = "needs root"]
fn another_user_cant_attach() {
    use shipyard_runtime::attach::{Frame, VERSION};
    use std::os::unix::fs::PermissionsExt as _;
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-private",
        "while read line; do echo got-$line; done",
        &[],
    );
    let open_up = |path: PathBuf, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    open_up(runtime.root.clone(), 0o755);
    open_up(runtime.root.join("attach-private"), 0o755);
    open_up(runtime.root.join("attach-private/attach.sock"), 0o666);
    // Everything the child needs, made before the fork (it mustn't allocate:
    // other threads may hold the allocator's lock).
    let path = std::ffi::CString::new(
        runtime
            .root
            .join("attach-private/attach.sock")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let mut hello = Vec::new();
    Frame::Hello {
        version: VERSION,
        detach_keys: Vec::new(),
    }
    .encode(&mut hello);
    // SAFETY: `sockaddr_un` is plain data; filled below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).unwrap();
    for (to, from) in address.sun_path.iter_mut().zip(path.as_bytes_with_nul()) {
        *to = libc::c_char::try_from(*from).unwrap();
    }
    let size = u32::try_from(std::mem::size_of::<libc::sockaddr_un>()).unwrap();
    let wait = libc::timeval {
        tv_sec: 5,
        tv_usec: 0,
    };
    let wait_size = u32::try_from(std::mem::size_of::<libc::timeval>()).unwrap();
    // A forked child connects (as `nobody`, or as root): 1 if a welcome
    // came (1 byte or more), 0 if not (end-of-file, or a reset: closed with
    // the hello unread), 2 or 3 if it couldn't try.
    let connect_as = |nobody: bool| {
        // SAFETY: the child only makes system calls (no allocation, no
        // locks) and ends with `_exit`.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: plain system calls on values made before the fork.
            unsafe {
                if nobody && (libc::setgid(65534) != 0 || libc::setuid(65534) != 0) {
                    libc::_exit(2);
                }
                let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                if libc::connect(fd, (&raw const address).cast(), size) != 0 {
                    libc::_exit(3);
                }
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&raw const wait).cast(),
                    wait_size,
                );
                let _ = libc::write(fd, hello.as_ptr().cast(), hello.len());
                let mut byte = [0_u8; 1];
                let read = libc::read(fd, byte.as_mut_ptr().cast(), 1);
                libc::_exit(i32::from(read > 0));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = 0;
        // SAFETY: our child, waited for until it has ended.
        while unsafe { libc::waitpid(child, &raw mut status, libc::WNOHANG) } == 0 {
            assert!(Instant::now() < deadline, "the connecting child hangs");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(libc::WIFEXITED(status));
        libc::WEXITSTATUS(status)
    };
    // The control: the same connection, as root, is let in.
    assert_eq!(connect_as(false), 1, "root wasn't let in");
    assert_eq!(
        connect_as(true),
        0,
        "nobody was let in (1), or couldn't connect (3)"
    );
    // Root's own, through the CLI.
    let mut root = Attached::start(&runtime, "attach-private", &[]);
    root.type_keys(b"x\n");
    root.expect("got-x");
    root.type_keys(DETACH);
    assert_eq!(root.end().0, Some(0));
}

/// The longest ID allowed: the socket's path is far past a socket
/// address's 108 bytes, and attach works (through the state directory).
#[test]
#[ignore = "needs root"]
fn attach_with_the_longest_id() {
    let runtime = Runtime::new();
    let id = "a".repeat(255);
    let bundle = bundle_running("read go; echo far-away");
    let dir = runtime.log_dir("longest");
    // (Not `Runtime::create`: its output files are named after the ID.)
    let status = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["create", "--bundle", bundle.arg(), "--stdin", "--log-dir"])
        .arg(&dir)
        .arg(&id)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(runtime.root.join(&id).join("attach.sock").as_os_str().len() > 255);
    assert_eq!(runtime.call(&["start", &id]).code, Some(0));
    let mut client = Attached::start(&runtime, &id, &[]);
    client.type_keys(b"go\n");
    client.expect("far-away");
    assert_eq!(client.end().0, Some(0));
}

/// The monitor killed: `attach` ends at once, saying so, not waiting. (Its
/// container is then stopped: a later `attach` says so.)
#[test]
#[ignore = "needs root"]
fn a_dead_monitor_ends_attach() {
    let runtime = Runtime::new();
    let _bundle = attachable(
        &runtime,
        "attach-abandoned",
        "while read line; do echo got-$line; done",
        &[],
    );
    let mut client = Attached::start(&runtime, "attach-abandoned", &[]);
    // Let in: its typing comes back.
    client.type_keys(b"x\n");
    client.expect("got-x");
    let init = runtime.state("attach-abandoned").unwrap()["pid"]
        .as_i64()
        .unwrap();
    let monitor = parent_of(i32::try_from(init).unwrap()).unwrap();
    // SAFETY: `kill` has no memory-safety preconditions.
    assert_eq!(unsafe { libc::kill(monitor, libc::SIGKILL) }, 0);
    let (code, stderr) = client.end();
    assert_ne!(code, Some(0));
    assert!(stderr.contains("monitor is gone"), "{stderr}");
    // Its init follows the monitor (its parent-death signal), a moment
    // later: until then, a new attach finds the monitor gone, not the
    // container stopped.
    assert!(
        shipyard_test_support::gone(i32::try_from(init).unwrap()),
        "the init outlived its monitor"
    );
    let later = runtime.call(&["attach", "attach-abandoned"]);
    assert_eq!(later.code, Some(255));
    assert!(later.stderr.contains("is stopped"), "{}", later.stderr);
}

/// What can't be: `--stdin` without logs, `--stdin-once` without `--stdin`
/// or with a terminal, attach to a container without logs, detach keys
/// that aren't keys.
#[test]
#[ignore = "needs root"]
fn attach_refusals() {
    let runtime = Runtime::new();
    let call = runtime.create("attach-no-logs", &bundle_running("true"), &["--stdin"]);
    assert_ne!(call.code, Some(0));
    assert!(call.stderr.contains("--log-dir"), "{}", call.stderr);
    let (call, _) = runtime.create_logged(
        "attach-once-alone",
        &bundle_running("true"),
        &["--stdin-once"],
    );
    assert_eq!(call.code, Some(2));
    assert!(call.stderr.contains("--stdin"), "{}", call.stderr);
    let (call, _) = runtime.create_logged(
        "attach-tty-once",
        &terminal_bundle("true", None),
        &["--stdin", "--stdin-once"],
    );
    assert_eq!(call.code, Some(1));
    assert!(
        call.stderr
            .contains("--stdin-once can't go with a terminal"),
        "{}",
        call.stderr
    );
    let call = runtime.create("attach-plain", &bundle_running("exec sleep 60"), &[]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let attach = runtime.call(&["attach", "attach-plain"]);
    assert_eq!(attach.code, Some(255), "attach's own failure");
    assert!(
        attach.stderr.contains("can't be attached to"),
        "{}",
        attach.stderr
    );
    let attach = runtime.call(&["attach", "--detach-keys", "ctrl-1", "attach-plain"]);
    assert_eq!(attach.code, Some(255), "attach's own failure");
    assert!(attach.stderr.contains("ctrl-1"), "{}", attach.stderr);
    // A socket nobody listens on (its monitor gone as the container
    // ended, before its status says so): said so, not a raw error.
    let _bundle = attachable(&runtime, "attach-orphaned", "exec sleep 60", &[]);
    let socket = runtime.root.join("attach-orphaned/attach.sock");
    std::fs::remove_file(&socket).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let attach = runtime.call(&["attach", "attach-orphaned"]);
    assert_eq!(attach.code, Some(255), "attach's own failure");
    assert!(
        attach.stderr.contains("monitor is gone"),
        "{}",
        attach.stderr
    );
}

// --- Exec (8.4a) -----------------------------------------------------------------------

/// Container `id` running `sh -c SCRIPT`, its config changed by `tweak`,
/// created and started. The bundle is the caller's to keep.
fn exec_target(
    runtime: &Runtime,
    id: &str,
    script: &str,
    tweak: impl FnOnce(&mut Value),
) -> Bundle {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    tweak(&mut config);
    // In a user namespace, root inside is host 100000: the root filesystem
    // is its.
    if config["linux"]["uidMappings"].is_array() {
        bundle.root.chown_all(100_000, 100_000);
    }
    bundle.write(&config);
    let created = runtime.create(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let started = runtime.call(&["start", id]);
    assert_eq!(
        started.code,
        Some(0),
        "{} (it ended: {}; it said: {})",
        started.stderr,
        std::fs::read_to_string(runtime.root.join(id).join("exit")).unwrap_or_default(),
        std::fs::read_to_string(runtime.output_dir().join(format!("{id}.err"))).unwrap_or_default()
    );
    bundle
}

/// `shipyard-runtime exec ARGS…`, which must end within 20 s.
fn exec(runtime: &Runtime, args: &[&str]) -> Call {
    let mut all = vec!["exec"];
    all.extend_from_slice(args);
    runtime.call_within(&all, Duration::from_secs(20))
}

/// A program the container doesn't have is "not found", even where the
/// host has one of that name (`true`): what exec says is about the
/// container's files, not the host's.
#[test]
#[ignore = "needs root"]
fn exec_of_a_program_the_container_lacks_is_not_found() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-lacks", "exec sleep 60", |_| {});
    assert!(
        std::path::Path::new("/bin/true").exists(),
        "the host has one"
    );
    let call = exec(&runtime, &["exec-lacks", "true"]);
    assert_eq!(call.code, Some(255), "{}", call.stderr);
    assert!(
        call.stderr.contains("true: command not found"),
        "{}",
        call.stderr
    );
    assert!(
        !call.stderr.contains("interpreter or loader"),
        "{}",
        call.stderr
    );
}

/// The command runs inside, and its exit code is exec's: 128 + N for a
/// signal. exec's own failures are 255 (runc's).
#[test]
#[ignore = "needs root"]
fn exec_runs_a_command_and_passes_its_exit_code() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-codes", "exec sleep 60", |_| {});
    let call = exec(&runtime, &["exec-codes", "sh", "-c", "cat /marker; exit 3"]);
    assert_eq!(call.code, Some(3), "{}", call.stderr);
    assert_eq!(call.stdout, shipyard_test_support::rootfs::MARKER);
    let killed = exec(&runtime, &["exec-codes", "sh", "-c", "kill -9 $$"]);
    assert_eq!(killed.code, Some(137), "{}", killed.stderr);
    // Its own failures: no such program, no such container.
    let missing = exec(&runtime, &["exec-codes", "/no/such/program"]);
    assert_eq!(missing.code, Some(255), "{}", missing.stderr);
    assert!(
        missing.stderr.contains("/no/such/program"),
        "{}",
        missing.stderr
    );
    let nobody = exec(&runtime, &["exec-nothing", "sh", "-c", ":"]);
    assert_eq!(nobody.code, Some(255), "{}", nobody.stderr);
}

/// A created container (at its gate) can be exec'd into; a stopped one and
/// a paused one can't (a paused one would freeze the command).
#[test]
#[ignore = "needs root"]
fn exec_wants_a_created_or_running_container() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 60");
    assert_eq!(runtime.create("exec-gate", &bundle, &[]).code, Some(0));
    let call = exec(&runtime, &["exec-gate", "sh", "-c", "echo $$"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.status("exec-gate"), "created");
    assert_eq!(runtime.call(&["start", "exec-gate"]).code, Some(0));
    assert_eq!(runtime.call(&["pause", "exec-gate"]).code, Some(0));
    let paused = exec(&runtime, &["exec-gate", "sh", "-c", ":"]);
    assert_eq!(paused.code, Some(255));
    assert!(paused.stderr.contains("is paused"), "{}", paused.stderr);
    assert_eq!(runtime.call(&["resume", "exec-gate"]).code, Some(0));
    let ended = bundle_running("true");
    assert_eq!(runtime.create("exec-ended", &ended, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "exec-ended"]).code, Some(0));
    runtime.wait_for("exec-ended", "stopped");
    let stopped = exec(&runtime, &["exec-ended", "sh", "-c", ":"]);
    assert_eq!(stopped.code, Some(255));
    assert!(stopped.stderr.contains("is stopped"), "{}", stopped.stderr);
}

/// The command gets the container's process's environment, working
/// directory and user, flags and a `process.json` change them.
#[test]
#[ignore = "needs root"]
fn exec_starts_from_the_containers_process() {
    let runtime = Runtime::new();
    let bundle = exec_target(&runtime, "exec-process", "exec sleep 60", |config| {
        config["process"]["env"] = json!(["PATH=/bin", "FROM=container"]);
        config["process"]["cwd"] = json!("/work");
        config["process"]["user"] = json!({"uid": 7, "gid": 8});
    });
    let call = exec(
        &runtime,
        &[
            "exec-process",
            "sh",
            "-c",
            "echo $FROM $(pwd) $(stat -c %u:%g /proc/$$)",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(call.stdout, "container /work 7:8\n");
    let call = exec(
        &runtime,
        &[
            "--env",
            "FROM=flag",
            "--cwd",
            "/tmp",
            "--user",
            "9:10",
            "exec-process",
            "sh",
            "-c",
            "echo $FROM $(pwd) $(stat -c %u:%g /proc/$$)",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(call.stdout, "flag /tmp 9:10\n");
    let process = bundle.path().join("process.json");
    std::fs::write(
        &process,
        json!({
            "args": ["sh", "-c", "echo $FROM $(pwd)"],
            "env": ["PATH=/bin", "FROM=file"],
            "cwd": "/",
            "user": {"uid": 0, "gid": 0},
            "noNewPrivileges": true,
        })
        .to_string(),
    )
    .unwrap();
    let call = exec(
        &runtime,
        &["--process", process.to_str().unwrap(), "exec-process"],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(call.stdout, "file /\n");
}

/// `shipyard-runtime exec -d ARGS…`, which must end within 20 s. The
/// detached command keeps exec's standard descriptors (as runc's does): its
/// output goes nowhere, so that nothing waits for it to end.
fn exec_detached(runtime: &Runtime, args: &[&str]) -> Call {
    let errors = runtime.root.with_extension("exec-errors");
    let mut child = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["exec", "-d"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&errors).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "exec -d {args:?} hung");
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = std::fs::read_to_string(&errors).unwrap_or_default();
    let _ = std::fs::remove_file(errors);
    Call {
        code: status.code(),
        stdout: String::new(),
        stderr,
    }
}

/// `shipyard-runtime exec ARGS…` started (not waited for), with `env` set.
fn exec_started(runtime: &Runtime, args: &[&str], env: &[(&str, &str)]) -> std::process::Child {
    Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .arg("exec")
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// A config's process in a user namespace of its own (root inside is host
/// 100000).
fn in_a_user_namespace(config: &mut Value) {
    config["linux"]["namespaces"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "user"}));
    config["linux"]["uidMappings"] = json!([{"containerID": 0, "hostID": 100_000, "size": 65536}]);
    config["linux"]["gidMappings"] = json!([{"containerID": 0, "hostID": 100_000, "size": 65536}]);
}

/// The process exec started (it's exec's child), as the host numbers it,
/// waiting up to 10 s for it.
fn exec_child(exec: &std::process::Child) -> i32 {
    let parent = i32::try_from(exec.id()).unwrap();
    let own = std::fs::read_link("/proc/self/ns/pid").unwrap();
    eventually("exec's process", || {
        std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .find_map(|entry| {
                let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
                // Inside the container (the joiner, also exec's child, isn't).
                let inside = std::fs::read_link(format!("/proc/{pid}/ns/pid")).ok()? != own;
                (parent_of(pid)? == parent && inside).then_some(pid)
            })
    })
}

/// What a container's process does to find exec's before it runs (its
/// command line is the joiner's, `shipyard-exec …`), then tries to reach it:
/// its program, descriptors and environment. First the control: the same on
/// a process of its own.
const REACH_EXEC: &str = r#"
sleep 30 & s=$!
readlink /proc/$s/exe >/dev/null 2>&1 && echo control-exe-readable
cat /proc/$s/environ >/dev/null 2>&1 && echo control-environ-readable
kill $s
while :; do
  for d in /proc/[0-9]*; do
    case "$(cat $d/cmdline 2>/dev/null)" in
      shipyard-exec*)
        readlink $d/exe >/dev/null 2>&1 && echo exe-readable || echo exe-denied
        ls $d/fd >/dev/null 2>&1 && echo fd-listable || echo fd-denied
        cat $d/environ >/dev/null 2>&1 && echo environ-readable || echo environ-denied
        echo probed
        exec sleep 60;;
    esac
  done
done
"#;

/// The container (root in its own user namespace, with `CAP_SYS_PTRACE`
/// there) can't reach exec's process before it runs: not its program, its
/// descriptors nor its environment. It's non-dumpable from before it enters
/// the container (F6; probe E4). Paused there by the test hook, with every
/// capability still: the most it's exposed.
#[test]
#[ignore = "needs root"]
fn the_container_cant_reach_exec_before_it_runs() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-reach", REACH_EXEC, |config| {
        in_a_user_namespace(config);
        let caps = json!(["CAP_SYS_PTRACE", "CAP_KILL", "CAP_DAC_OVERRIDE"]);
        config["process"]["capabilities"] =
            json!({"bounding": caps, "effective": caps, "permitted": caps});
    });
    let mut paused = exec_started(
        &runtime,
        &["exec-reach", "sh", "-c", ":"],
        &[("SHIPYARD_TEST_EXEC_PAUSE", "3000")],
    );
    let out = eventually("the container's probe", || {
        let out = runtime.output_of("exec-reach");
        out.contains("probed").then_some(out)
    });
    let _ = paused.wait();
    assert!(out.contains("control-exe-readable"), "{out}");
    assert!(out.contains("control-environ-readable"), "{out}");
    assert!(out.contains("exe-denied"), "{out}");
    assert!(out.contains("fd-denied"), "{out}");
    assert!(out.contains("environ-denied"), "{out}");
}

/// The same without a user namespace and without `CAP_SYS_PTRACE` (runc's
/// default capabilities): its program and environment are out of reach too.
#[test]
#[ignore = "needs root"]
fn a_plain_container_cant_reach_exec_before_it_runs() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-reach-plain", REACH_EXEC, |_| {});
    let mut paused = exec_started(
        &runtime,
        &["exec-reach-plain", "sh", "-c", ":"],
        &[("SHIPYARD_TEST_EXEC_PAUSE", "3000")],
    );
    let out = eventually("the container's probe", || {
        let out = runtime.output_of("exec-reach-plain");
        out.contains("probed").then_some(out)
    });
    let _ = paused.wait();
    assert!(out.contains("control-exe-readable"), "{out}");
    assert!(out.contains("exe-denied"), "{out}");
    assert!(out.contains("environ-denied"), "{out}");
}

/// exec's process is a clone of the sealed copy of shipyard's binary, never
/// the file on disk: until its own `exec`, its program is the copy, which
/// nothing can change (CVE-2019-5736).
#[test]
#[ignore = "needs root"]
fn exec_never_runs_the_binary_on_disk() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-sealed", "exec sleep 60", |_| {});
    let mut paused = exec_started(
        &runtime,
        &["exec-sealed", "sh", "-c", ":"],
        &[("SHIPYARD_TEST_EXEC_PAUSE", "2000")],
    );
    let pid = exec_child(&paused);
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    let _ = paused.wait();
    assert!(
        exe.to_string_lossy().starts_with("/memfd:shipyard-init"),
        "{}",
        exe.display()
    );
}

/// Nothing of the host's reaches exec's process: a host directory exec had
/// open (60, inherited) isn't there, so it can't serve as its working
/// directory either (CVE-2016-9962, CVE-2024-21626): only 0, 1 and 2 are
/// open.
#[test]
#[ignore = "needs root"]
fn exec_holds_no_host_descriptors() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-fds", "exec sleep 60", |_| {});
    // `exec ARGS…` with the host's `/` open at 60, inherited.
    let with_leak = |args: &[&str]| {
        let mut command = Command::new(RUNTIME.path());
        command
            .arg("--root")
            .arg(&runtime.root)
            .arg("exec")
            .args(args);
        // SAFETY: `open`, `dup2` and `close` are async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                let fd = libc::open(c"/".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                if fd == -1 || libc::dup2(fd, 60) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(fd);
                Ok(())
            });
        }
        command.output().unwrap()
    };
    let listed = with_leak(&["exec-fds", "ls", "/proc/self/fd"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    // 3: the directory `ls` reads.
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "0\n1\n2\n3\n");
    let escaped = with_leak(&[
        "--cwd",
        "/proc/self/fd/60",
        "exec-fds",
        "sh",
        "-c",
        "pwd; ls",
    ]);
    assert_eq!(
        escaped.status.code(),
        Some(255),
        "{}{}",
        String::from_utf8_lossy(&escaped.stdout),
        String::from_utf8_lossy(&escaped.stderr)
    );
    // Refused for that working directory (there's no descriptor 60 to
    // enter), before anything ran.
    assert!(escaped.stdout.is_empty(), "it ran");
    assert!(
        String::from_utf8_lossy(&escaped.stderr).contains("changing directory to /proc/self/fd/60"),
        "{}",
        String::from_utf8_lossy(&escaped.stderr)
    );
}

/// `exec` without `-t` passes a caller's pipes and files on as they are (a
/// shim's): only a terminal is relayed. The command's stdin is the very pipe
/// the caller made.
#[test]
#[ignore = "needs root"]
fn exec_passes_a_callers_pipe_on_as_it_is() {
    use std::os::fd::AsRawFd as _;
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-pipes", "exec sleep 60", |_| {});
    let child = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["exec", "exec-pipes", "readlink", "/proc/self/fd/0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ours = std::fs::read_link(format!(
        "/proc/self/fd/{}",
        child.stdin.as_ref().unwrap().as_raw_fd()
    ))
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(ours.to_string_lossy().starts_with("pipe:["), "{ours:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        ours.to_string_lossy(),
        "not the caller's pipe"
    );
}

/// exec's process is confined exactly as the container's own: the same
/// namespaces, cgroup, user, capabilities, `no_new_privs`, seccomp filter
/// and limits. The filter's refusal shows.
#[test]
#[ignore = "needs root"]
fn exec_is_confined_as_the_container() {
    const LOOK: &str = "cat /proc/self/status; ulimit -a; \
        for n in pid mnt uts ipc net cgroup user; do readlink /proc/self/ns/$n; done; \
        cat /proc/self/cgroup";
    let runtime = Runtime::new();
    let bundle = exec_target(
        &runtime,
        "exec-confined",
        &format!("({LOOK}) > /tmp/container; exec sleep 60"),
        |config| {
            config["process"]["noNewPrivileges"] = json!(true);
            config["process"]["rlimits"] =
                json!([{"type": "RLIMIT_NOFILE", "hard": 512, "soft": 256}]);
            config["linux"]["resources"] = json!({"pids": {"limit": 50}});
            config["linux"]["seccomp"] = json!({
                "defaultAction": "SCMP_ACT_ALLOW",
                "syscalls": [{"names": ["mkdir", "mkdirat"], "action": "SCMP_ACT_ERRNO"}]
            });
        },
    );
    let theirs = eventually("the container's look", || {
        std::fs::read_to_string(bundle.root.path().join("tmp/container"))
            .ok()
            .filter(|text| text.contains("::"))
    });
    let ours = exec(&runtime, &["exec-confined", "sh", "-c", LOOK]);
    assert_eq!(ours.code, Some(0), "{}", ours.stderr);
    let lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| {
                ![
                    "Name:",
                    "NS",
                    "Pid:",
                    "PPid:",
                    "Tgid:",
                    "Ngid:",
                    "State:",
                    "Vm",
                    "Rss",
                    "voluntary",
                    "nonvoluntary",
                    "Sig",
                    "Threads:",
                    "Hugetlb",
                    "Kthread",
                    "Untag",
                ]
                .iter()
                .any(|skip| line.trim_start().starts_with(skip))
            })
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(lines(&ours.stdout), lines(&theirs));
    assert!(ours.stdout.contains("NoNewPrivs:\t1"), "{}", ours.stdout);
    assert!(ours.stdout.contains("Seccomp:\t2"), "{}", ours.stdout);
    let refused = exec(&runtime, &["exec-confined", "mkdir", "/tmp/x"]);
    assert_ne!(refused.code, Some(0));
    assert!(
        refused.stderr.contains("Operation not permitted"),
        "{}",
        refused.stderr
    );
}

/// What exec gets was taken when the container started: editing its
/// `config.json` afterwards changes nothing.
#[test]
#[ignore = "needs root"]
fn editing_config_json_after_create_changes_nothing() {
    let runtime = Runtime::new();
    let bundle = exec_target(&runtime, "exec-edited", "exec sleep 60", |config| {
        config["process"]["env"] = json!(["PATH=/bin", "FROM=create"]);
    });
    let mut config = bundle.config();
    config["process"]["env"] = json!(["PATH=/bin", "FROM=edited"]);
    let all = json!(["CAP_SYS_ADMIN", "CAP_SYS_PTRACE", "CAP_KILL"]);
    config["process"]["capabilities"] =
        json!({"bounding": all, "effective": all, "permitted": all});
    bundle.write(&config);
    let call = exec(
        &runtime,
        &[
            "exec-edited",
            "sh",
            "-c",
            "echo $FROM; cat /proc/self/status",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(call.stdout.starts_with("create\n"), "{}", call.stdout);
    // runc's three: AUDIT_WRITE, KILL, NET_BIND_SERVICE.
    assert!(
        call.stdout.contains("CapBnd:\t0000000020000420"),
        "{}",
        call.stdout
    );
}

/// Flags and a `process.json` may narrow the container's process, never
/// widen it: a capability outside its bounding set, `no_new_privs` off, a
/// higher limit, a lower OOM score adjustment are refused.
#[test]
#[ignore = "needs root"]
fn exec_cant_widen_the_container() {
    let runtime = Runtime::new();
    let bundle = exec_target(&runtime, "exec-narrow", "exec sleep 60", |config| {
        config["process"]["noNewPrivileges"] = json!(true);
        config["process"]["rlimits"] = json!([{"type": "RLIMIT_NOFILE", "hard": 512, "soft": 512}]);
        config["process"]["oomScoreAdj"] = json!(100);
    });
    let refused = |call: Call, why: &str| {
        assert_eq!(call.code, Some(255), "{}", call.stderr);
        assert!(
            call.stderr.contains("more than the container has"),
            "{}",
            call.stderr
        );
        assert!(call.stderr.contains(why), "{}", call.stderr);
    };
    refused(
        exec(
            &runtime,
            &["--cap", "SYS_ADMIN", "exec-narrow", "sh", "-c", ":"],
        ),
        "CAP_SYS_ADMIN isn't in its bounding set",
    );
    let process = |fields: Value| {
        let mut process = json!({"args": ["sh", "-c", ":"], "cwd": "/", "user": {"uid": 0, "gid": 0}, "noNewPrivileges": true});
        for (key, value) in fields.as_object().unwrap() {
            process[key] = value.clone();
        }
        let path = bundle.path().join("process.json");
        std::fs::write(&path, process.to_string()).unwrap();
        exec(
            &runtime,
            &["--process", path.to_str().unwrap(), "exec-narrow"],
        )
    };
    refused(process(json!({"noNewPrivileges": false})), "no_new_privs");
    refused(
        process(json!({"rlimits": [{"type": "RLIMIT_NOFILE", "hard": 1024, "soft": 512}]})),
        "nofile limit",
    );
    refused(process(json!({"oomScoreAdj": 0})), "OOM score");
    // Within it: fine (a capability it has, a lower limit).
    let within = process(json!({"rlimits": [{"type": "RLIMIT_NOFILE", "hard": 256, "soft": 256}]}));
    assert_eq!(within.code, Some(0), "{}", within.stderr);
    let kill = exec(&runtime, &["--cap", "KILL", "exec-narrow", "sh", "-c", ":"]);
    assert_eq!(kill.code, Some(0), "{}", kill.stderr);
}

/// exec's process starts in the container's cgroup (`CLONE_INTO_CGROUP`):
/// its limits hold it, `kill --all` and `delete --force` end it.
#[test]
#[ignore = "needs root"]
fn exec_runs_in_the_containers_cgroup() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-cgroup", "exec sleep 60", |config| {
        config["linux"]["resources"] = json!({"pids": {"limit": 6}});
    });
    let cgroup = cgroup_of("exec-cgroup");
    let call = exec(&runtime, &["exec-cgroup", "cat", "/proc/self/cgroup"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(call.stdout, "0::/shipyard/runtime/exec-cgroup\n");
    // More than its 6 processes: some forks fail. (Short sleeps, their
    // output elsewhere: nothing keeps exec's pipes open after.)
    let crowded = exec(
        &runtime,
        &[
            "exec-cgroup",
            "sh",
            "-c",
            "for i in 1 2 3 4 5 6 7 8; do sleep 1 >/dev/null 2>&1 & done; wait",
        ],
    );
    assert!(crowded.stderr.contains("fork"), "{}", crowded.stderr);
    let pid_file = runtime.root.with_extension("cgroup.pid");
    let detached = exec_detached(
        &runtime,
        &[
            "--pid-file",
            pid_file.to_str().unwrap(),
            "exec-cgroup",
            "sleep",
            "300",
        ],
    );
    assert_eq!(detached.code, Some(0), "{}", detached.stderr);
    let pid: i64 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = std::fs::remove_file(pid_file);
    let procs = cgroup_procs(&cgroup);
    assert!(procs.contains(&pid), "{procs:?}");
    assert_eq!(
        runtime.call(&["kill", "--all", "exec-cgroup", "KILL"]).code,
        Some(0)
    );
    runtime.wait_for("exec-cgroup", "stopped");
    for pid in procs {
        eventually("killed", || (!alive(pid)).then_some(()));
    }
}

/// A container whose cgroup has sub-cgroups with controllers (made inside
/// it) holds no process itself: exec says so instead of starting elsewhere.
#[test]
#[ignore = "needs root"]
fn exec_refuses_a_cgroup_with_sub_cgroups() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-nested", "exec sleep 60", |config| {
        config["linux"]["resources"] = json!({"pids": {"limit": 50}});
    });
    let cgroup = cgroup_of("exec-nested");
    std::fs::create_dir(cgroup.join("inner")).unwrap();
    // Moved until the cgroup holds none: a process forked meanwhile (the
    // init starting its command) joins it after a first look, and the
    // kernel refuses controllers below a cgroup with a process (EBUSY).
    eventually("the container's processes moved below", || {
        for pid in cgroup_procs(&cgroup) {
            let _ = std::fs::write(cgroup.join("inner/cgroup.procs"), pid.to_string());
        }
        std::fs::write(cgroup.join("cgroup.subtree_control"), "+pids").ok()
    });
    let call = exec(&runtime, &["exec-nested", "sh", "-c", ":"]);
    assert_eq!(call.code, Some(255), "{}", call.stderr);
    assert!(call.stderr.contains("sub-cgroups"), "{}", call.stderr);
}

/// `-d` returns once the command runs, leaving it running (exec's own
/// cleanup doesn't kill it); `--pid-file` names it.
#[test]
#[ignore = "needs root"]
fn exec_detached_returns_at_once() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-detached", "exec sleep 60", |_| {});
    let pid_file = runtime.root.with_extension("exec.pid");
    let started = Instant::now();
    let call = exec_detached(
        &runtime,
        &[
            "--pid-file",
            pid_file.to_str().unwrap(),
            "exec-detached",
            "sleep",
            "300",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid: i64 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid));
    assert!(cgroup_procs(&cgroup_of("exec-detached")).contains(&pid));
    assert_eq!(
        runtime.call(&["delete", "--force", "exec-detached"]).code,
        Some(0)
    );
    eventually("killed with the container", || (!alive(pid)).then_some(()));
    let _ = std::fs::remove_file(pid_file);
}

/// exec's command dies with its container: 137 (SIGKILL).
#[test]
#[ignore = "needs root"]
fn exec_is_killed_with_its_container() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-dies", "exec sleep 60", |_| {});
    let mut running = exec_started(&runtime, &["exec-dies", "sleep", "300"], &[]);
    let _ = exec_child(&running);
    assert_eq!(runtime.call(&["kill", "exec-dies", "KILL"]).code, Some(0));
    let status = eventually("exec's end", || running.try_wait().unwrap());
    assert_eq!(status.code(), Some(137));
}

/// A `pause` while exec starts its command waits until it has: never a
/// command frozen half-way (exec holds the container's lock till then).
#[test]
#[ignore = "needs root"]
fn pause_waits_for_exec() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-pause", "exec sleep 60", |_| {});
    let mut running = exec_started(
        &runtime,
        &["exec-pause", "sleep", "300"],
        &[("SHIPYARD_TEST_EXEC_PAUSE", "1500")],
    );
    let pid = exec_child(&running);
    assert_eq!(runtime.call(&["pause", "exec-pause"]).code, Some(0));
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    assert_eq!(runtime.call(&["resume", "exec-pause"]).code, Some(0));
    let _ = running.kill();
    let _ = running.wait();
    assert!(
        exe.ends_with("bin/sleep"),
        "frozen before its exec: {}",
        exe.display()
    );
}

/// exec sets the container's limits before entering its user namespace: a
/// hard limit above exec's own is raised (with the host's powers).
#[test]
#[ignore = "needs root"]
fn exec_with_limits_above_the_callers() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-limits", "exec sleep 60", |config| {
        in_a_user_namespace(config);
        config["process"]["rlimits"] =
            json!([{"type": "RLIMIT_NOFILE", "hard": 4096, "soft": 4096}]);
    });
    let mut command = Command::new(RUNTIME.path());
    command.arg("--root").arg(&runtime.root).args([
        "exec",
        "exec-limits",
        "sh",
        "-c",
        "ulimit -Hn",
    ]);
    // SAFETY: `setrlimit` is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            let low = libc::rlimit {
                rlim_cur: 1024,
                rlim_max: 1024,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raw const low) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "4096\n");
}

/// Another user can't exec into root's container; root can (the control).
#[test]
#[ignore = "needs root"]
fn another_user_cant_exec() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-private", "exec sleep 60", |_| {});
    let out = Command::new("setpriv")
        .args(["--reuid", "65534", "--regid", "65534", "--clear-groups"])
        .arg(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["exec", "exec-private", "sh", "-c", ":"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(255),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Its state root isn't theirs: refused before anything is opened.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refusing the state root"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        exec(&runtime, &["exec-private", "sh", "-c", ":"]).code,
        Some(0)
    );
}

/// exec's command takes no longer to start than a moment (H7).
#[test]
#[ignore = "needs root"]
fn exec_starts_at_once() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-quick", "exec sleep 60", |_| {});
    let started = Instant::now();
    let call = exec(&runtime, &["exec-quick", "sh", "-c", ":"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// Rootless: exec joins the container's user namespace (root inside is the
/// caller), and everything else the container has.
#[test]
fn a_rootless_exec_joins_its_user_namespace() {
    const LOOK: &str = "cat /proc/self/uid_map; \
        for n in user pid mnt uts ipc net cgroup; do readlink /proc/self/ns/$n; done; \
        cat /proc/self/cgroup; stat -c %u /proc/$$";
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!([
        "sh",
        "-c",
        format!("({LOOK}) > /tmp/container; exec sleep 60")
    ]);
    bundle.write(&config);
    let created = runtime.create("exec-rootless", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "exec-rootless"]).code, Some(0));
    let theirs = eventually("the container's look", || {
        std::fs::read_to_string(bundle.root.path().join("tmp/container"))
            .ok()
            // All of it: the ID map (2 lines), 7 namespaces, the cgroup, the
            // owner.
            .filter(|text| text.lines().count() >= 11)
    });
    let ours = exec(&runtime, &["exec-rootless", "sh", "-c", LOOK]);
    assert_eq!(ours.code, Some(0), "{}", ours.stderr);
    assert_eq!(ours.stdout, theirs);
}

/// Rootless, from a cgroup that isn't delegated to the user (an SSH
/// login's): exec can't move its process into the container's cgroup, and
/// says how to run it instead.
#[test]
#[ignore = "needs root"]
fn a_rootless_exec_from_an_undelegated_cgroup() {
    let session = PathBuf::from("/sys/fs/cgroup/user.slice/tester/session");
    let uid = Command::new("id").args(["-u", "tester"]).output().ok();
    let uid = uid
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
    let (Some(uid), true) = (uid, session.exists()) else {
        eprintln!("skipping: needs test-linux.sh's tester and its delegated cgroup");
        return;
    };
    let undelegated = PathBuf::from(format!(
        "/sys/fs/cgroup/shipyard-ssh-exec-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&undelegated).unwrap();
    let runtime = Runtime::new();
    std::fs::create_dir_all(&runtime.root).unwrap();
    let tester: u32 = uid.parse().unwrap();
    std::os::unix::fs::chown(&runtime.root, Some(tester), Some(tester)).unwrap();
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
            .env("XDG_RUNTIME_DIR", runtime.root.parent().unwrap())
            .arg(RUNTIME.path());
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
    bundle.root.chown_all(tester, tester);
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "exec sleep 60"]);
    bundle.write(&config);
    let created = runtime.create_with("exec-ssh", &bundle, &[], as_tester_in(&session));
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let started = as_tester_in(&session)
        .arg("--root")
        .arg(&runtime.root)
        .args(["start", "exec-ssh"])
        .output()
        .unwrap();
    assert!(started.status.success());
    let from_ssh = as_tester_in(&undelegated)
        .arg("--root")
        .arg(&runtime.root)
        .args(["exec", "exec-ssh", "sh", "-c", ":"])
        .output()
        .unwrap();
    let from_session = as_tester_in(&session)
        .arg("--root")
        .arg(&runtime.root)
        .args(["exec", "exec-ssh", "sh", "-c", ":"])
        .output()
        .unwrap();
    let _ = as_tester_in(&session)
        .arg("--root")
        .arg(&runtime.root)
        .args(["delete", "--force", "exec-ssh"])
        .output();
    let _ = std::fs::remove_dir(&undelegated);
    let stderr = String::from_utf8_lossy(&from_ssh.stderr);
    assert_eq!(from_ssh.status.code(), Some(255), "{stderr}");
    assert!(stderr.contains("systemd-run --user --scope"), "{stderr}");
    assert!(
        from_session.status.success(),
        "{}",
        String::from_utf8_lossy(&from_session.stderr)
    );
}

/// The container stops exec's command before it runs (it may: the command
/// is in its PID namespace): exec fails, at once, rather than hold the
/// container's lock; the lock is free after (`pause` works).
#[test]
#[ignore = "needs root"]
fn a_command_stopped_before_it_runs_fails_exec() {
    const STOP_EXEC: &str = r#"
while :; do
  for d in /proc/[0-9]*; do
    case "$(cat $d/comm 2>/dev/null)" in
      shipyard-exec) kill -STOP ${d#/proc/}; echo stopped; exec sleep 60;;
    esac
  done
done
"#;
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-stopped", STOP_EXEC, |_| {});
    let started = Instant::now();
    let mut held = exec_started(
        &runtime,
        &["exec-stopped", "sh", "-c", ":"],
        &[("SHIPYARD_TEST_EXEC_PAUSE", "3000")],
    );
    let status = eventually("exec's end", || held.try_wait().unwrap());
    let mut stderr = String::new();
    std::io::Read::read_to_string(held.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(runtime.output_of("exec-stopped").contains("stopped"));
    assert_eq!(status.code(), Some(255), "{stderr}");
    assert!(stderr.contains("stopped before it ran"), "{stderr}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(runtime.call(&["pause", "exec-stopped"]).code, Some(0));
    assert_eq!(runtime.call(&["resume", "exec-stopped"]).code, Some(0));
}

/// A container whose cgroup was made again since (another directory at its
/// path): exec refuses, rather than run its command without one.
#[test]
#[ignore = "needs root"]
fn exec_refuses_a_replaced_cgroup() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-replaced", "exec sleep 60", |config| {
        config["linux"]["resources"] = json!({"pids": {"limit": 50}});
    });
    let cgroup = cgroup_of("exec-replaced");
    let aside = cgroup.with_file_name("exec-replaced-aside");
    std::fs::create_dir(&aside).unwrap();
    let procs = cgroup_procs(&cgroup);
    for pid in &procs {
        std::fs::write(aside.join("cgroup.procs"), pid.to_string()).unwrap();
    }
    // Empty once the kernel says so (`populated 0`), not when its list is:
    // an rmdir before then is refused (EBUSY).
    let events = || std::fs::read_to_string(cgroup.join("cgroup.events")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events().contains("populated 0") {
        assert!(Instant::now() < deadline, "never emptied: {}", events());
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir(&cgroup).unwrap();
    std::fs::create_dir(&cgroup).unwrap();
    for pid in &procs {
        std::fs::write(cgroup.join("cgroup.procs"), pid.to_string()).unwrap();
    }
    let _ = std::fs::remove_dir(&aside);
    let call = exec(&runtime, &["exec-replaced", "sh", "-c", ":"]);
    // Its processes and cgroup gone whatever was said: the runtime won't
    // delete a container whose cgroup was replaced, and another run here
    // would find them.
    for pid in &procs {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(*pid).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    // (Its monitor may remove it first, its init ended: gone is done.)
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::remove_dir(&cgroup)
        .is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(call.code, Some(255), "{}", call.stderr);
    assert!(
        call.stderr.contains("isn't the one made for it"),
        "{}",
        call.stderr
    );
}

// --- Exec with a terminal (8.4b) ----------------------------------------------------------

/// `exec -d -t --console-socket`: the command's terminal, made in the
/// container, goes to the socket (named `/dev/ptmx`, runc's message), and exec
/// returns at once. Typing reaches the command; its terminal is the
/// container's.
#[test]
#[ignore = "needs root"]
fn exec_d_t_sends_the_master_to_the_console_socket() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-console", "exec sleep 60", |_| {});
    let socket = ConsoleSocket::new();
    let started = Instant::now();
    let call = exec_detached(
        &runtime,
        &[
            "-t",
            "--console-socket",
            socket.path().to_str().unwrap(),
            "exec-console",
            "sh",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut console = socket.receive();
    assert_eq!(console.name, b"/dev/ptmx");
    console.type_keys(b"tty; echo typed-$((40+2))\n");
    console.expect("/dev/pts/");
    console.expect("typed-42");
    console.type_keys(b"exit\n");
}

/// `-t` when exec's own input isn't a terminal: relayed as it is (no raw
/// mode), the terminal sized 24x80 (or the process's `consoleSize`).
#[test]
#[ignore = "needs root"]
fn exec_t_without_a_terminal() {
    let runtime = Runtime::new();
    let bundle = exec_target(&runtime, "exec-t-pipe", "exec sleep 60", |_| {});
    let call = exec(
        &runtime,
        &[
            "-t",
            "exec-t-pipe",
            "sh",
            "-c",
            "tty; stty size; ls -1 /proc/self/fd",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let out = call.stdout.replace('\r', "");
    assert!(out.starts_with("/dev/pts/"), "{out}");
    assert!(out.contains("\n24 80\n"), "{out}");
    // Its terminal on 0, 1 and 2, and nothing else of the host's.
    assert!(out.ends_with("\n0\n1\n2\n3\n"), "{out}");
    // A process.json's terminal, with its size.
    let process = bundle.path().join("process.json");
    std::fs::write(
        &process,
        json!({
            "args": ["stty", "size"], "cwd": "/", "user": {"uid": 0, "gid": 0},
            "noNewPrivileges": true, "terminal": true,
            "consoleSize": {"height": 30, "width": 100},
        })
        .to_string(),
    )
    .unwrap();
    let call = exec(
        &runtime,
        &["--process", process.to_str().unwrap(), "exec-t-pipe"],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(call.stdout.replace('\r', ""), "30 100\n");
}

/// The command's terminal is its user's (so it can open it by name).
#[test]
#[ignore = "needs root"]
fn exec_t_as_another_user() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-t-user", "exec sleep 60", |_| {});
    let call = exec(
        &runtime,
        &[
            "-t",
            "--user",
            "7:8",
            "exec-t-user",
            "sh",
            "-c",
            "stat -c %u:%g $(tty); stty size",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(
        call.stdout.replace('\r', "").starts_with("7:8\n"),
        "{}",
        call.stdout
    );
}

/// runc's rules: `-d -t` needs a console socket (nothing else would hold the
/// terminal), a console socket needs a terminal, and it must be a socket.
#[test]
#[ignore = "needs root"]
fn exec_terminal_refusals() {
    let runtime = Runtime::new();
    let bundle = exec_target(&runtime, "exec-t-rules", "exec sleep 60", |_| {});
    let detached = exec(&runtime, &["-d", "-t", "exec-t-rules", "sh"]);
    assert_eq!(detached.code, Some(255));
    assert!(
        detached.stderr.contains("needs --console-socket"),
        "{}",
        detached.stderr
    );
    let socket = ConsoleSocket::new();
    let without = exec(
        &runtime,
        &[
            "--console-socket",
            socket.path().to_str().unwrap(),
            "exec-t-rules",
            "true",
        ],
    );
    assert_eq!(without.code, Some(255));
    assert!(without.stderr.contains("no terminal"), "{}", without.stderr);
    let not_one = exec(
        &runtime,
        &[
            "-d",
            "-t",
            "--console-socket",
            bundle.arg(),
            "exec-t-rules",
            "sh",
        ],
    );
    assert_eq!(not_one.code, Some(255));
    assert!(
        not_one.stderr.contains("not a socket"),
        "{}",
        not_one.stderr
    );
    // A process.json without a terminal: the CLI can't tell, the runtime
    // refuses it.
    let process = bundle.path().join("process.json");
    std::fs::write(
        &process,
        json!({"args": ["true"], "cwd": "/", "user": {"uid": 0, "gid": 0}, "noNewPrivileges": true})
            .to_string(),
    )
    .unwrap();
    let plain = exec(
        &runtime,
        &[
            "--process",
            process.to_str().unwrap(),
            "--console-socket",
            socket.path().to_str().unwrap(),
            "exec-t-rules",
        ],
    );
    assert_eq!(plain.code, Some(255));
    assert!(plain.stderr.contains("no terminal"), "{}", plain.stderr);
    // Without -d, a console socket's terminal is its receiver's, and exec
    // waits for the command (runc's). (A socket of its own: the refused
    // process.json's connection is still queued on the other.)
    let socket = ConsoleSocket::new();
    let mut waiting = exec_started(
        &runtime,
        &[
            "-t",
            "--console-socket",
            socket.path().to_str().unwrap(),
            "exec-t-rules",
            "sh",
            "-c",
            "sleep 1; exit 4",
        ],
        &[],
    );
    let _console = socket.receive();
    let status = eventually("exec's end", || waiting.try_wait().unwrap());
    assert_eq!(status.code(), Some(4));
}

/// The container controls its `/dev/pts`: exec never follows a link there,
/// and takes only a real pseudo-terminal master: anything else is refused,
/// nothing outside the container opened.
#[test]
#[ignore = "needs root"]
fn exec_t_refuses_a_ptmx_that_isnt_one() {
    let runtime = Runtime::new();
    let _bundle = exec_target(&runtime, "exec-t-fake", "exec sleep 60", |_| {});
    let init = runtime.state("exec-t-fake").unwrap()["pid"]
        .as_i64()
        .unwrap();
    let mounts = std::fs::File::open(format!("/proc/{init}/ns/mnt")).unwrap();
    // Inside its mount namespace (a forked child: system calls only).
    let inside = |change: fn() -> bool| {
        // SAFETY: the child makes system calls only, and ends with `_exit`.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: plain system calls; the descriptor was opened before.
            unsafe {
                let ok = libc::setns(std::os::fd::AsRawFd::as_raw_fd(&mounts), libc::CLONE_NEWNS)
                    == 0
                    && change();
                libc::_exit(i32::from(!ok));
            }
        }
        let mut status = 0;
        // SAFETY: our child, waited for once.
        assert_eq!(unsafe { libc::waitpid(child, &raw mut status, 0) }, child);
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "changing the container's /dev/pts"
        );
    };
    // No devpts; a link where its ptmx was.
    inside(|| {
        // SAFETY: plain system calls on static strings.
        unsafe {
            libc::umount2(c"/dev/pts".as_ptr(), libc::MNT_DETACH) == 0
                && libc::symlink(
                    c"/proc/1/root/etc/hostname".as_ptr(),
                    c"/dev/pts/ptmx".as_ptr(),
                ) == 0
        }
    });
    let refused = |what: &str| {
        let call = exec(&runtime, &["-t", "exec-t-fake", "sh", "-c", "echo ran"]);
        assert_eq!(
            call.code,
            Some(255),
            "{what}: {}{}",
            call.stdout,
            call.stderr
        );
        assert!(!call.stdout.contains("ran"), "{what}");
        call.stderr
    };
    // (Looked at, not followed: the link itself isn't the multiplexer.)
    let linked = refused("a link");
    assert!(linked.contains("terminal multiplexer"), "{linked}");
    // A plain file where its ptmx was.
    inside(|| {
        // SAFETY: plain system calls on static strings.
        unsafe {
            libc::unlink(c"/dev/pts/ptmx".as_ptr()) == 0
                && libc::open(
                    c"/dev/pts/ptmx".as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY,
                    0o666,
                ) >= 0
        }
    });
    let file = refused("a file");
    assert!(file.contains("terminal multiplexer"), "{file}");
    // Another device (1:3, /dev/null's): never opened, refused.
    inside(|| {
        // SAFETY: plain system calls on static strings.
        unsafe {
            libc::unlink(c"/dev/pts/ptmx".as_ptr()) == 0
                && libc::mknod(
                    c"/dev/pts/ptmx".as_ptr(),
                    libc::S_IFCHR | 0o666,
                    libc::makedev(1, 3),
                ) == 0
        }
    });
    let device = refused("a device");
    assert!(device.contains("terminal multiplexer"), "{device}");
}

/// Rootless, `exec -d -t --console-socket` too.
#[test]
fn a_rootless_exec_gets_its_terminal() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 60");
    let created = runtime.create("exec-rootless-t", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "exec-rootless-t"]).code, Some(0));
    let socket = ConsoleSocket::new();
    let call = exec_detached(
        &runtime,
        &[
            "-t",
            "--console-socket",
            socket.path().to_str().unwrap(),
            "exec-rootless-t",
            "sh",
        ],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let mut console = socket.receive();
    console.type_keys(b"tty\n");
    console.expect("/dev/pts/");
    console.type_keys(b"exit\n");
}

/// runc's terminal rules, checked before anything connects to the socket
/// (nor the container is looked at): `-d -t` needs a console socket, and a
/// console socket needs `-t`.
#[test]
fn exec_terminal_rules_come_first() {
    let runtime = Runtime::new();
    let socket = ConsoleSocket::new();
    let detached = exec(&runtime, &["-d", "-t", "nobody-here", "sh"]);
    assert_eq!(detached.code, Some(255));
    assert!(
        detached.stderr.contains("needs --console-socket"),
        "{}",
        detached.stderr
    );
    let without = exec(
        &runtime,
        &[
            "--console-socket",
            socket.path().to_str().unwrap(),
            "nobody-here",
            "true",
        ],
    );
    assert_eq!(without.code, Some(255));
    assert!(without.stderr.contains("no terminal"), "{}", without.stderr);
}

// --- cp (8.5a) -----------------------------------------------------------------------------

/// A scratch directory on the host, removed on drop.
struct HostDir(PathBuf);

impl HostDir {
    fn new(name: &str) -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-cp-{name}-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn join(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }

    fn arg(&self, path: &str) -> String {
        self.join(path).to_str().unwrap().to_owned()
    }
}

impl Drop for HostDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Container `id` running `sh -c SCRIPT`, its root filesystem prepared by
/// `prepare` and its config changed by `tweak`, created and started.
fn cp_target(
    runtime: &Runtime,
    id: &str,
    script: &str,
    prepare: impl FnOnce(&Bundle),
    tweak: impl FnOnce(&mut Value),
) -> Bundle {
    let bundle = Bundle::new();
    prepare(&bundle);
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    tweak(&mut config);
    bundle.write(&config);
    let created = runtime.create(id, &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    let started = runtime.call(&["start", id]);
    assert_eq!(started.code, Some(0), "{}", started.stderr);
    bundle
}

/// `shipyard-runtime cp ARGS…`, which must end within 20 s.
fn cp(runtime: &Runtime, args: &[&str]) -> Call {
    let mut all = vec!["cp"];
    all.extend_from_slice(args);
    runtime.call_within(&all, Duration::from_secs(20))
}

/// `shipyard-runtime cp ARGS…` with `input` on its standard input: its
/// exit code, standard output (bytes) and error.
fn cp_bytes(runtime: &Runtime, args: &[&str], input: &[u8]) -> (Option<i32>, Vec<u8>, String) {
    let mut command = Command::new(RUNTIME.path());
    command
        .arg("--root")
        .arg(&runtime.root)
        .arg("cp")
        .args(args);
    let output = shipyard_test_support::run_raw(command, Some(input));
    (
        output.status.code(),
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// What `cat PATH` prints in container `id`.
fn read_inside(runtime: &Runtime, id: &str, path: &str) -> String {
    let call = exec(runtime, &[id, "cat", path]);
    assert_eq!(call.code, Some(0), "cat {path}: {}", call.stderr);
    call.stdout
}

/// The names in a tar stream, in order.
fn tar_names(archive: &[u8]) -> Vec<String> {
    tar::Archive::new(archive)
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().display().to_string())
        .collect()
}

/// A tar stream of raw entries (any name, `..` included): `(name, kind,
/// data)`.
fn raw_tar(entries: &[(&[u8], tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (name, kind, data) in entries {
        let mut header = tar::Header::new_ustar();
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(*kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
    }
    bytes.extend_from_slice(&[0; 1024]);
    bytes
}

/// Docker's path rules, both ways: files and directories, renamed or into a
/// directory, a directory's contents (`SRC/.`), and the errors.
#[test]
#[ignore = "needs root"]
fn cp_follows_dockers_path_rules() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-rules", "exec sleep 60", |_| {}, |_| {});
    let host = rules_files();
    let ok = |args: &[&str]| {
        let call = cp(&runtime, args);
        assert_eq!(call.code, Some(0), "cp {args:?}: {}", call.stderr);
    };
    // In.
    ok(&[&host.arg("a"), "cp-rules:/work"]);
    assert_eq!(read_inside(&runtime, "cp-rules", "/work/a"), "alpha");
    ok(&[&host.arg("a"), "cp-rules:/work/renamed"]);
    assert_eq!(read_inside(&runtime, "cp-rules", "/work/renamed"), "alpha");
    ok(&[&host.arg("d"), "cp-rules:/work"]);
    assert_eq!(read_inside(&runtime, "cp-rules", "/work/d/sub/y"), "y");
    ok(&[&host.arg("d"), "cp-rules:/work/newd"]);
    assert_eq!(read_inside(&runtime, "cp-rules", "/work/newd/x"), "x");
    ok(&[&format!("{}/.", host.arg("d")), "cp-rules:/work/contents"]);
    assert_eq!(
        read_inside(&runtime, "cp-rules", "/work/contents/sub/y"),
        "y"
    );
    // Overwritten.
    std::fs::write(host.join("a"), "again").unwrap();
    ok(&[&host.arg("a"), "cp-rules:/work/renamed"]);
    assert_eq!(read_inside(&runtime, "cp-rules", "/work/renamed"), "again");

    // Out.
    ok(&["cp-rules:/work/a", &host.arg("out-a")]);
    assert_eq!(
        std::fs::read_to_string(host.join("out-a")).unwrap(),
        "alpha"
    );
    std::fs::create_dir(host.join("outdir")).unwrap();
    ok(&["cp-rules:/work/d", &host.arg("outdir")]);
    assert_eq!(
        std::fs::read_to_string(host.join("outdir/d/sub/y")).unwrap(),
        "y"
    );
    ok(&["cp-rules:/work/d", &host.arg("newout")]);
    assert_eq!(std::fs::read_to_string(host.join("newout/x")).unwrap(), "x");
    ok(&["cp-rules:/work/d/.", &host.arg("outdir")]);
    assert_eq!(std::fs::read_to_string(host.join("outdir/x")).unwrap(), "x");
    ok(&["cp-rules:/marker", &format!("{}/", host.arg("outdir"))]);
    assert_eq!(
        std::fs::read_to_string(host.join("outdir/marker")).unwrap(),
        shipyard_test_support::rootfs::MARKER
    );
}

/// A host file `a` and a directory `d` (`x`, `sub/y`) to copy.
fn rules_files() -> HostDir {
    let host = HostDir::new("rules");
    std::fs::write(host.join("a"), "alpha").unwrap();
    std::fs::create_dir_all(host.join("d/sub")).unwrap();
    std::fs::write(host.join("d/x"), "x").unwrap();
    std::fs::write(host.join("d/sub/y"), "y").unwrap();
    host
}

/// What Docker's rules refuse, each with its words; and what failed made
/// nothing.
#[test]
#[ignore = "needs root"]
fn cp_refuses_what_dockers_rules_refuse() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-refused", "exec sleep 60", |_| {}, |_| {});
    let host = rules_files();
    for (source, dest) in [
        (host.arg("a"), "cp-refused:/work/a"),
        (host.arg("d"), "cp-refused:/work"),
    ] {
        let call = cp(&runtime, &[&source, dest]);
        assert_eq!(call.code, Some(0), "{}", call.stderr);
    }
    std::fs::write(host.join("out-a"), "a file").unwrap();
    // The errors, each with its words.
    let refused = |args: &[&str], words: &str| {
        let call = cp(&runtime, args);
        assert_eq!(call.code, Some(1), "cp {args:?}: {}", call.stderr);
        assert!(call.stderr.contains(words), "cp {args:?}: {}", call.stderr);
    };
    refused(
        &[&host.arg("d"), "cp-refused:/work/a"],
        "cannot copy a directory onto a file",
    );
    refused(
        &["cp-refused:/work/d", &host.arg("out-a")],
        "a directory and a file can't replace each other",
    );
    refused(
        &["cp-refused:/nothing", &host.arg("x")],
        "no such file or directory",
    );
    refused(
        &[&host.arg("nothing"), "cp-refused:/work"],
        "no such file or directory",
    );
    refused(
        &[&host.arg("a"), "cp-refused:/no/such/x"],
        "directory doesn't exist",
    );
    refused(
        &["cp-refused:/marker", &host.arg("no/such/x")],
        "directory doesn't exist",
    );
    refused(
        &[&host.arg("a"), "cp-refused:/work/missing/"],
        "destination directory doesn't exist",
    );
    refused(
        &[&host.arg("a"), "cp-refused:/work/missing/."],
        "destination directory doesn't exist",
    );
    // A directory where a file of its name is.
    let clash = cp(&runtime, &[&host.arg("a"), "cp-refused:/work/clash"]);
    assert_eq!(clash.code, Some(0), "{}", clash.stderr);
    std::fs::create_dir(host.join("clash")).unwrap();
    refused(
        &[&host.arg("clash"), "cp-refused:/work"],
        "can't replace each other",
    );
    // What failed made nothing: a directory made for a directory's contents
    // is gone again.
    refused(
        &["cp-refused:/work/nothing/.", &host.arg("nodir")],
        "no such file or directory",
    );
    assert!(!host.join("nodir").exists());
    refused(
        &["cp-refused:/marker", &host.arg("missing/")],
        "doesn't exist",
    );
    refused(
        &[&host.arg("a"), &host.arg("b")],
        "one side must be a container's path",
    );
    refused(
        &["cp-nobody:/x", &host.arg("b")],
        "no container with ID cp-nobody",
    );
}

/// `-` is a tar stream: out on stdout, in from stdin (extracted into the
/// destination directory).
#[test]
#[ignore = "needs root"]
fn cp_streams_tar_both_ways() {
    let runtime = Runtime::new();
    let _bundle = cp_target(
        &runtime,
        "cp-stream",
        "mkdir -p /work/d/sub && echo x > /work/d/sub/x && exec sleep 60",
        |_| {},
        |_| {},
    );
    let (code, out, err) = cp_bytes(&runtime, &["cp-stream:/work/d", "-"], b"");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(tar_names(&out), ["d", "d/sub", "d/sub/x"]);

    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(8);
    header.set_mode(0o640);
    builder
        .append_data(&mut header, "streamed/file", b"streamed".as_slice())
        .unwrap();
    let archive = builder.into_inner().unwrap();
    let (code, _, err) = cp_bytes(&runtime, &["-", "cp-stream:/work"], &archive);
    // Its directory wasn't in the stream: nothing is implied.
    assert_eq!(code, Some(1), "{err}");
    let mut builder = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    builder
        .append_data(&mut dir, "streamed", std::io::empty())
        .unwrap();
    builder
        .append_data(&mut header, "streamed/file", b"streamed".as_slice())
        .unwrap();
    let archive = builder.into_inner().unwrap();
    let (code, _, err) = cp_bytes(&runtime, &["-", "cp-stream:/work"], &archive);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        read_inside(&runtime, "cp-stream", "/work/streamed/file"),
        "streamed"
    );
    // A stream goes into a directory.
    let (code, _, err) = cp_bytes(&runtime, &["-", "cp-stream:/marker"], &archive);
    assert_eq!(code, Some(1));
    assert!(err.contains("not a directory"), "{err}");
}

/// Copied in, files become the container's root's; `-a` keeps the
/// source's owners. Copied out, the caller's, without setuid.
#[test]
#[ignore = "needs root"]
fn cp_gives_files_their_owners() {
    let runtime = Runtime::new();
    let _bundle = cp_target(
        &runtime,
        "cp-owners",
        "exec sleep 60",
        |bundle| {
            let suid = bundle.root.path().join("work/suid");
            std::fs::write(&suid, "x").unwrap();
            std::os::unix::fs::chown(&suid, Some(5), Some(6)).unwrap();
            std::fs::set_permissions(&suid, std::os::unix::fs::PermissionsExt::from_mode(0o4755))
                .unwrap();
        },
        |_| {},
    );
    let host = HostDir::new("owners");
    std::fs::write(host.join("owned"), "x").unwrap();
    std::os::unix::fs::chown(host.join("owned"), Some(7), Some(8)).unwrap();
    let owner = |path: &str| {
        let call = exec(&runtime, &["cp-owners", "stat", "-c", "%u:%g", path]);
        assert_eq!(call.code, Some(0), "{}", call.stderr);
        call.stdout.trim().to_owned()
    };
    assert_eq!(
        cp(&runtime, &[&host.arg("owned"), "cp-owners:/work/plain"]).code,
        Some(0)
    );
    assert_eq!(owner("/work/plain"), "0:0");
    assert_eq!(
        cp(
            &runtime,
            &["-a", &host.arg("owned"), "cp-owners:/work/kept"]
        )
        .code,
        Some(0)
    );
    assert_eq!(owner("/work/kept"), "7:8");

    let call = cp(&runtime, &["cp-owners:/work/suid", &host.arg("out")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let meta = std::fs::metadata(host.join("out")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (0, 0), "the caller's");
    assert_eq!(meta.mode() & 0o7777, 0o755, "setuid dropped");
    // In a stream, the container's own owners and mode.
    let (code, out, err) = cp_bytes(&runtime, &["cp-owners:/work/suid", "-"], b"");
    assert_eq!(code, Some(0), "{err}");
    let mut archive = tar::Archive::new(out.as_slice());
    let entry = archive.entries().unwrap().next().unwrap().unwrap();
    assert_eq!(entry.header().uid().unwrap(), 5);
    assert_eq!(entry.header().mode().unwrap() & 0o7777, 0o4755);
}

/// A container's symlinks lead only into the container: `/link ->
/// /etc/passwd` is its own passwd, whichever way it's copied.
#[test]
#[ignore = "needs root"]
fn cp_never_follows_a_containers_link_out_of_it() {
    let runtime = Runtime::new();
    let name = format!("cp-link-{}", std::process::id());
    let bundle = cp_target(
        &runtime,
        "cp-links",
        "exec sleep 60",
        |bundle| {
            let root = bundle.root.path();
            std::fs::create_dir_all(root.join("etc")).unwrap();
            std::fs::write(root.join("etc/passwd"), "the container's\n").unwrap();
            std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
            std::os::unix::fs::symlink("/etc", root.join("dir")).unwrap();
            std::os::unix::fs::symlink("../../../../../../etc", root.join("climb")).unwrap();
        },
        |_| {},
    );
    let host = HostDir::new("links");
    let copied = |source: &str, to: &str| {
        let call = cp(&runtime, &["-L", source, &host.arg(to)]);
        assert_eq!(call.code, Some(0), "{source}: {}", call.stderr);
        std::fs::read_to_string(host.join(to)).unwrap()
    };
    assert_eq!(copied("cp-links:/link", "l"), "the container's\n");
    assert_eq!(copied("cp-links:/dir/passwd", "p"), "the container's\n");
    assert_eq!(copied("cp-links:/climb/passwd", "c"), "the container's\n");
    // Without -L, the link itself.
    let call = cp(&runtime, &["cp-links:/link", &host.arg("raw")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(
        std::fs::read_link(host.join("raw")).unwrap(),
        Path::new("/etc/passwd")
    );
    // In, through them: the container's /etc.
    std::fs::write(host.join(&name), "in").unwrap();
    for dest in ["cp-links:/dir/", "cp-links:/climb/"] {
        let call = cp(&runtime, &[&host.arg(&name), dest]);
        assert_eq!(call.code, Some(0), "{dest}: {}", call.stderr);
        assert!(bundle.root.path().join("etc").join(&name).exists());
        assert!(
            !Path::new("/etc").join(&name).exists(),
            "written on the host"
        );
    }
}

/// CVE-2018-15664: the container swaps a directory for a link to `/` while
/// cp copies into it, again and again. Whenever the swap lands, the copy
/// lands in the container's `/`, never the host's.
#[test]
#[ignore = "needs root"]
fn cp_through_a_swapped_link_stays_inside() {
    /// Removes a host file on drop.
    struct Remove(String);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let runtime = Runtime::new();
    let name = format!("cp-race-{}", std::process::id());
    let host_only = format!("/cp-host-only-{}", std::process::id());
    std::fs::write(&host_only, "the host's").unwrap();
    let _remove = Remove(host_only.clone());
    let bundle = cp_target(
        &runtime,
        "cp-race",
        "while :; do rm -rf /w/d; mkdir /w/d; touch /w/d/inner; rm -rf /w/d; ln -s / /w/d; done",
        |bundle| {
            for program in ["rm", "ln"] {
                bundle.root.add_host_program(program);
            }
            std::fs::create_dir_all(bundle.root.path().join("w")).unwrap();
        },
        |_| {},
    );
    let host = HostDir::new("race");
    std::fs::write(host.join(&name), "raced").unwrap();
    let mut copied = 0;
    for round in 0..60 {
        if cp(&runtime, &[&host.arg(&name), "cp-race:/w/d/"]).code == Some(0) {
            copied += 1;
        }
        // Out, following the link (`/.`: its contents) every few rounds
        // (the container's whole root is a few megabytes).
        let out = host.join(&format!("out{round}"));
        let source = if round % 10 == 0 {
            "cp-race:/w/d/."
        } else {
            "cp-race:/w/d"
        };
        if cp(&runtime, &[source, out.to_str().unwrap()]).code == Some(0) {
            let host_only_name = Path::new(&host_only).file_name().unwrap();
            // A link copied as one leads where the host says: not a copy.
            let is_link = std::fs::symlink_metadata(&out).unwrap().is_symlink();
            assert!(
                is_link || !out.join(host_only_name).exists(),
                "the host's / was copied out"
            );
        }
        assert!(!Path::new("/").join(&name).exists(), "written on the host");
    }
    assert!(copied > 0, "the control: some copies landed");
    let inside = bundle.root.path();
    assert!(
        inside.join(&name).exists() || inside.join("w/d").join(&name).exists(),
        "the control: the copy is in the container"
    );
}

/// The PID of the helper `cp` (PID `parent`) started, waiting for it.
fn helper_of(parent: u32) -> u32 {
    let parent = i32::try_from(parent).unwrap();
    eventually("cp's helper", || {
        std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .find_map(|entry| {
                let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
                (stat.contains("(shipyard-helper)") && parent_of(pid) == Some(parent))
                    .then(|| u32::try_from(pid).ok())
                    .flatten()
            })
    })
}

/// The helper is shipyard's sealed binary, loads nothing of the
/// container's, holds only its standard streams and report pipe, has only
/// its file capabilities and `no_new_privs`, and isn't in the container's
/// PID namespace.
#[test]
#[ignore = "needs root"]
fn cp_helper_holds_nothing_of_the_hosts() {
    let runtime = Runtime::new();
    let bundle = cp_target(&runtime, "cp-helper", "exec sleep 60", |_| {}, |_| {});
    let host = HostDir::new("helper");
    let child = Command::new(RUNTIME.path())
        .env("SHIPYARD_TEST_HELPER_PAUSE", "3000")
        .arg("--root")
        .arg(&runtime.root)
        .args(["cp", "cp-helper:/marker", &host.arg("m")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let helper = helper_of(child.id());
    let proc = PathBuf::from(format!("/proc/{helper}"));
    let listed = || -> Vec<String> {
        std::fs::read_dir(proc.join("fd"))
            .unwrap()
            .flatten()
            .filter_map(|entry| Some(std::fs::read_link(entry.path()).ok()?.display().to_string()))
            .collect()
    };
    // Set up (it pauses for the test after its sweep): its descriptors
    // settle; a helper that kept more never gets there.
    let deadline = Instant::now() + Duration::from_secs(5);
    while listed().len() > 5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // (The count can pass 5 half way through its sweep: then it pauses for
    // seconds, long after this.)
    std::thread::sleep(Duration::from_millis(300));
    let fds = listed();
    assert_eq!(
        fds.len(),
        5,
        "0, 1, 2, its report pipe, its /proc/self/fd: {fds:?}"
    );
    for fd in &fds {
        assert!(
            fd.starts_with("pipe:") || fd == "/dev/null" || fd.ends_with("/fd"),
            "{fd} ({fds:?})"
        );
    }
    let exe = std::fs::read_link(proc.join("exe")).unwrap();
    assert!(
        exe.to_string_lossy().starts_with("/memfd:shipyard"),
        "{}",
        exe.display()
    );
    let maps = std::fs::read_to_string(proc.join("maps")).unwrap();
    assert!(
        !maps.contains(bundle.root.arg()),
        "loaded the container's: {maps}"
    );
    let status = std::fs::read_to_string(proc.join("status")).unwrap();
    assert!(status.contains("NoNewPrivs:\t1"), "{status}");
    assert!(status.contains("CapEff:\t000000000000001f"), "{status}");
    let init = runtime.state("cp-helper").unwrap()["pid"].as_i64().unwrap();
    let ns =
        |pid: String, kind: &str| std::fs::read_link(format!("/proc/{pid}/ns/{kind}")).unwrap();
    assert_ne!(ns(helper.to_string(), "pid"), ns(init.to_string(), "pid"));
    assert_eq!(ns(helper.to_string(), "mnt"), ns(init.to_string(), "mnt"));
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What cp copies out never opens a FIFO or device (only headers), and a
/// file growing meanwhile is cut at its size: cp ends, at once. Devices
/// aren't made on the host.
#[test]
#[ignore = "needs root"]
fn cp_out_opens_no_fifo_and_makes_no_device() {
    let runtime = Runtime::new();
    let _bundle = cp_target(
        &runtime,
        "cp-special",
        "while :; do echo grown >> /w/grow; sleep 0.01; done",
        |bundle| {
            let w = bundle.root.path().join("w");
            std::fs::create_dir_all(&w).unwrap();
            nix::unistd::mkfifo(
                &w.join("fifo"),
                nix::sys::stat::Mode::from_bits_truncate(0o600),
            )
            .unwrap();
            nix::sys::stat::mknod(
                &w.join("null"),
                nix::sys::stat::SFlag::S_IFCHR,
                nix::sys::stat::Mode::from_bits_truncate(0o666),
                nix::sys::stat::makedev(1, 3),
            )
            .unwrap();
        },
        |_| {},
    );
    let host = HostDir::new("special");
    let started = Instant::now();
    let call = cp(&runtime, &["cp-special:/w", &host.arg("out")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        call.stderr.contains("1 device node(s) weren't copied"),
        "{}",
        call.stderr
    );
    assert!(!host.join("out/null").exists());
    let fifo = std::fs::symlink_metadata(host.join("out/fifo")).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_fifo(&fifo.file_type()));
    assert!(host.join("out/grow").is_file());
}

/// `.wh.` names are names (no whiteout deletes anything), and an archive
/// reaching out of its destination is refused.
#[test]
#[ignore = "needs root"]
fn cp_in_keeps_names_and_refuses_escapes() {
    let runtime = Runtime::new();
    let bundle = cp_target(&runtime, "cp-names", "exec sleep 60", |_| {}, |_| {});
    let whiteout = raw_tar(&[(b".wh.marker", tar::EntryType::Regular, b"a name")]);
    let (code, _, err) = cp_bytes(&runtime, &["-", "cp-names:/"], &whiteout);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        read_inside(&runtime, "cp-names", "/marker"),
        shipyard_test_support::rootfs::MARKER
    );
    assert_eq!(read_inside(&runtime, "cp-names", "/.wh.marker"), "a name");
    for (name, kind, link) in [
        (b"../escape".as_slice(), tar::EntryType::Regular, None),
        (
            b"work/../../escape".as_slice(),
            tar::EntryType::Regular,
            None,
        ),
        (
            b"link".as_slice(),
            tar::EntryType::Link,
            Some(b"../outside".as_slice()),
        ),
    ] {
        let mut archive = raw_tar(&[(name, kind, b"")]);
        if let Some(link) = link {
            let mut header = tar::Header::from_byte_slice(&archive[..512]).clone();
            header.as_old_mut().linkname[..link.len()].copy_from_slice(link);
            header.set_cksum();
            archive[..512].copy_from_slice(header.as_bytes());
        }
        let (code, _, err) = cp_bytes(&runtime, &["-", "cp-names:/work"], &archive);
        assert_eq!(code, Some(1), "{}: {err}", String::from_utf8_lossy(name));
        assert!(!bundle.root.path().join("escape").exists());
    }
}

/// A created container (at its gate) and a paused one can be copied into
/// and out of (the helper isn't in its cgroup); a stopped one can't.
#[test]
#[ignore = "needs root"]
fn cp_wants_a_live_container() {
    let runtime = Runtime::new();
    let host = HostDir::new("live");
    std::fs::write(host.join("f"), "f").unwrap();
    let bundle = bundle_running("exec sleep 60");
    assert_eq!(runtime.create("cp-gate", &bundle, &[]).code, Some(0));
    let call = cp(&runtime, &[&host.arg("f"), "cp-gate:/work"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.status("cp-gate"), "created");
    assert_eq!(runtime.call(&["start", "cp-gate"]).code, Some(0));
    assert_eq!(runtime.call(&["pause", "cp-gate"]).code, Some(0));
    let call = cp(&runtime, &["cp-gate:/work/f", &host.arg("back")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(std::fs::read_to_string(host.join("back")).unwrap(), "f");
    assert_eq!(runtime.call(&["resume", "cp-gate"]).code, Some(0));
    let ended = bundle_running("true");
    assert_eq!(runtime.create("cp-ended", &ended, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "cp-ended"]).code, Some(0));
    runtime.wait_for("cp-ended", "stopped");
    let stopped = cp(&runtime, &[&host.arg("f"), "cp-ended:/work"]);
    assert_eq!(stopped.code, Some(1));
    assert!(stopped.stderr.contains("is stopped"), "{}", stopped.stderr);
}

/// Into `/proc/sys`: refused (nothing is ever opened for writing in place),
/// the value unchanged. Out of `/proc`: the directory, not its contents.
#[test]
#[ignore = "needs root"]
fn cp_leaves_the_kernels_files_alone() {
    let runtime = Runtime::new();
    let _bundle = cp_target(
        &runtime,
        "cp-proc",
        "exec sleep 60",
        |bundle| std::fs::create_dir_all(bundle.root.path().join("proc")).unwrap(),
        |config| {
            config["mounts"] = json!([{"destination": "/proc", "type": "proc", "source": "proc"}]);
        },
    );
    let before = read_inside(&runtime, "cp-proc", "/proc/sys/kernel/hostname");
    let host = HostDir::new("proc");
    std::fs::write(host.join("hostname"), "overwritten\n").unwrap();
    let call = cp(
        &runtime,
        &[&host.arg("hostname"), "cp-proc:/proc/sys/kernel/"],
    );
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert_eq!(
        read_inside(&runtime, "cp-proc", "/proc/sys/kernel/hostname"),
        before
    );
    let call = cp(&runtime, &["cp-proc:/proc", &host.arg("proc")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(std::fs::read_dir(host.join("proc")).unwrap().count(), 0);
}

/// A copy of a small file takes a moment, either way (H7).
#[test]
#[ignore = "needs root"]
fn cp_of_a_small_file_is_quick() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-quick", "exec sleep 60", |_| {}, |_| {});
    let host = HostDir::new("quick");
    std::fs::write(host.join("f"), "f").unwrap();
    for args in [
        [host.arg("f"), "cp-quick:/work".to_owned()],
        ["cp-quick:/work/f".to_owned(), host.arg("back")],
    ] {
        let started = Instant::now();
        let call = cp(&runtime, &[&args[0], &args[1]]);
        assert_eq!(call.code, Some(0), "{}", call.stderr);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{args:?} took {:?}",
            started.elapsed()
        );
    }
}

/// H8: what cp writes is what GNU tar extracts, and what GNU tar writes is
/// what cp extracts.
#[test]
#[ignore = "needs root"]
fn cp_streams_agree_with_gnu_tar() {
    let runtime = Runtime::new();
    let _bundle = cp_target(
        &runtime,
        "cp-gnu",
        "mkdir -p /work/t/sub && echo one > /work/t/one && echo two > /work/t/sub/two \
         && chmod 640 /work/t/one && exec sleep 60",
        |bundle| bundle.root.add_host_program("ln"),
        |_| {},
    );
    let link = exec(&runtime, &["cp-gnu", "ln", "-s", "one", "/work/t/alias"]);
    assert_eq!(link.code, Some(0), "{}", link.stderr);
    let hard = exec(
        &runtime,
        &["cp-gnu", "ln", "/work/t/one", "/work/t/sub/hard"],
    );
    assert_eq!(hard.code, Some(0), "{}", hard.stderr);
    let host = HostDir::new("gnu");
    // Out: GNU tar extracts cp's stream as cp itself does.
    let (code, stream, err) = cp_bytes(&runtime, &["cp-gnu:/work/t", "-"], b"");
    assert_eq!(code, Some(0), "{err}");
    std::fs::create_dir(host.join("gnu")).unwrap();
    let mut tar = Command::new("tar")
        .args(["-x", "-C", &host.arg("gnu")])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut tar.stdin.take().unwrap(), &stream).unwrap();
    assert!(tar.wait().unwrap().success());
    let call = cp(&runtime, &["cp-gnu:/work/t", &host.arg("ours")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let tree = |root: PathBuf| {
        let mut rows = Vec::new();
        for path in ["", "one", "alias", "sub", "sub/two", "sub/hard"] {
            let path = root.join(path);
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let content = if meta.is_file() {
                std::fs::read_to_string(&path).unwrap()
            } else if meta.is_symlink() {
                std::fs::read_link(&path).unwrap().display().to_string()
            } else {
                String::new()
            };
            rows.push((meta.mode() & 0o777, meta.nlink().min(2), content));
        }
        rows
    };
    assert_eq!(tree(host.join("gnu/t")), tree(host.join("ours")));
    // In: GNU tar's stream, extracted by cp.
    let made = Command::new("tar")
        .args(["-c", "-C", &host.arg("gnu"), "t"])
        .output()
        .unwrap();
    assert!(made.status.success());
    let (code, _, err) = cp_bytes(&runtime, &["-", "cp-gnu:/tmp"], &made.stdout);
    assert_eq!(code, Some(0), "{err}");
    let call = cp(&runtime, &["cp-gnu:/tmp/t", &host.arg("round")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(tree(host.join("round")), tree(host.join("ours")));
}

/// Another user can't copy from root's container.
#[test]
#[ignore = "needs root"]
fn another_user_cant_cp() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-private", "exec sleep 60", |_| {}, |_| {});
    let out = Command::new("setpriv")
        .args(["--reuid", "65534", "--regid", "65534", "--clear-groups"])
        .arg(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["cp", "cp-private:/marker", "-"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let host = HostDir::new("private");
    let call = cp(&runtime, &["cp-private:/marker", &host.arg("m")]);
    assert_eq!(call.code, Some(0), "the control: {}", call.stderr);
}

/// Rootless, both ways; and the helper is out of its own user's reach
/// (non-dumpable), where cp itself isn't (the control).
#[test]
fn a_rootless_cp_goes_both_ways_and_hides_its_helper() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 60");
    let created = runtime.create("cp-rootless", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "cp-rootless"]).code, Some(0));
    let host = HostDir::new("rootless");
    std::fs::write(host.join("f"), "rootless").unwrap();
    let call = cp(&runtime, &[&host.arg("f"), "cp-rootless:/work"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let call = cp(&runtime, &["cp-rootless:/work/f", &host.arg("back")]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(
        std::fs::read_to_string(host.join("back")).unwrap(),
        "rootless"
    );

    let child = Command::new(RUNTIME.path())
        .env("SHIPYARD_TEST_HELPER_PAUSE", "2000")
        .arg("--root")
        .arg(&runtime.root)
        .args(["cp", "cp-rootless:/marker", &host.arg("m")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let helper = helper_of(child.id());
    // Hidden from its first step: given a moment to get there.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut hidden = std::fs::read_dir(format!("/proc/{helper}/fd"));
    while hidden.is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        hidden = std::fs::read_dir(format!("/proc/{helper}/fd"));
    }
    assert_eq!(
        hidden.map(drop).map_err(|err| err.kind()),
        Err(std::io::ErrorKind::PermissionDenied),
        "the helper's descriptors"
    );
    assert!(
        std::fs::read_dir(format!("/proc/{}/fd", child.id())).is_ok(),
        "the control"
    );
    assert!(child.wait_with_output().unwrap().status.success());

    // A source this side can't read: its error, not the helper's
    // "cut off" (it only saw the stream end); the directory made in the
    // container for the contents is gone again.
    std::fs::create_dir(host.join("unreadable")).unwrap();
    std::fs::write(host.join("unreadable/a-secret"), "x").unwrap();
    std::fs::set_permissions(
        host.join("unreadable/a-secret"),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let call = cp(
        &runtime,
        &[
            &format!("{}/.", host.arg("unreadable")),
            "cp-rootless:/work/new",
        ],
    );
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        call.stderr.contains("a-secret: Permission denied"),
        "{}",
        call.stderr
    );
    assert!(!bundle.root.path().join("work/new").exists());
}

/// Rootless with the full ID map: a file a subordinate ID owns, which its
/// user can't read from the host, is copied out (the helper is root in the
/// container's user namespace); and copied in, it's the container root's.
#[test]
fn a_rootless_cp_reaches_files_of_subordinate_ids() {
    if !rootless(Rootless::Full) {
        return;
    }
    let uid = nix::unistd::getuid();
    let name = nix::unistd::User::from_uid(uid).unwrap().unwrap().name;
    let range = |file: &str| -> (u64, u64) {
        let text = std::fs::read_to_string(file).unwrap();
        let line = text
            .lines()
            .find(|line| line.starts_with(&format!("{name}:")))
            .unwrap_or_else(|| panic!("no range for {name} in {file}"));
        let mut fields = line.split(':').skip(1).map(|n| n.parse::<u64>().unwrap());
        (fields.next().unwrap(), fields.next().unwrap())
    };
    let ((uids, uid_count), (gids, gid_count)) = (range("/etc/subuid"), range("/etc/subgid"));
    let runtime = Runtime::new();
    let bundle = cp_target(
        &runtime,
        "cp-subordinate",
        "touch /work/private && chown 1:1 /work/private && chmod 600 /work/private \
         && echo ready > /work/ready && exec sleep 60",
        |_| {},
        |config| {
            let caps = json!(["CAP_CHOWN", "CAP_FOWNER", "CAP_DAC_OVERRIDE"]);
            for set in ["bounding", "effective", "permitted"] {
                config["process"]["capabilities"][set] = caps.clone();
            }
            config["linux"]["namespaces"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "user"}));
            config["linux"]["uidMappings"] = json!([
                {"containerID": 0, "hostID": uid.as_raw(), "size": 1},
                {"containerID": 1, "hostID": uids, "size": uid_count.min(65535)},
            ]);
            config["linux"]["gidMappings"] = json!([
                {"containerID": 0, "hostID": nix::unistd::getgid().as_raw(), "size": 1},
                {"containerID": 1, "hostID": gids, "size": gid_count.min(65535)},
            ]);
        },
    );
    let ready = bundle.root.path().join("work/ready");
    eventually("the container's file", || ready.exists().then_some(()));
    let private = bundle.root.path().join("work/private");
    assert_eq!(
        std::fs::read(&private).map_err(|err| err.kind()),
        Err(std::io::ErrorKind::PermissionDenied),
        "the control: its user can't read it from the host"
    );
    let host = HostDir::new("subordinate");
    let call = cp(
        &runtime,
        &["cp-subordinate:/work/private", &host.arg("out")],
    );
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(host.join("out").is_file());
    std::fs::write(host.join("in"), "in").unwrap();
    let call = cp(&runtime, &[&host.arg("in"), "cp-subordinate:/work/in"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    let meta = std::fs::metadata(bundle.root.path().join("work/in")).unwrap();
    assert_eq!(meta.uid(), uid.as_raw(), "the container's root: its user");
}

/// A device, FIFO or socket is never replaced, either way: `cp ID:F
/// /dev/null` as root must leave the host's `/dev/null` a device (it once
/// replaced it with a file, and every later `/dev/null` was that file).
#[test]
#[ignore = "needs root"]
fn cp_never_replaces_a_device() {
    use std::os::unix::fs::FileTypeExt as _;
    let runtime = Runtime::new();
    let _bundle = cp_target(
        &runtime,
        "cp-device",
        "exec sleep 60",
        |bundle| {
            let w = bundle.root.path().join("w");
            std::fs::create_dir_all(&w).unwrap();
            nix::unistd::mkfifo(
                &w.join("fifo"),
                nix::sys::stat::Mode::from_bits_truncate(0o600),
            )
            .unwrap();
        },
        |_| {},
    );
    let call = cp(&runtime, &["cp-device:/marker", "/dev/null"]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        call.stderr.contains("cp never replaces one"),
        "{}",
        call.stderr
    );
    let null = std::fs::metadata("/dev/null").unwrap().file_type();
    assert!(null.is_char_device(), "the host's /dev/null was replaced");
    let host = HostDir::new("device");
    std::fs::write(host.join("f"), "f").unwrap();
    let call = cp(&runtime, &[&host.arg("f"), "cp-device:/w/fifo"]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(
        call.stderr.contains("never replaces one"),
        "{}",
        call.stderr
    );
}

/// A `-` stream with padding after its end-of-archive marker (tar pads to
/// 10 KiB), written late: read to its end, not left to a broken pipe. One
/// cut off before its marker is refused.
#[test]
#[ignore = "needs root"]
fn cp_reads_a_stream_to_its_end_and_refuses_a_cut_one() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-padded", "exec sleep 60", |_| {}, |_| {});
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, "padded", b"x".as_slice())
        .unwrap();
    let archive = builder.into_inner().unwrap();
    let mut child = Command::new(RUNTIME.path())
        .arg("--root")
        .arg(&runtime.root)
        .args(["cp", "-", "cp-padded:/work"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    std::io::Write::write_all(&mut stdin, &archive).unwrap();
    // The padding, once the helper has had the end marker.
    std::thread::sleep(Duration::from_millis(1500));
    let _ = std::io::Write::write_all(&mut stdin, &[0; 9 * 1024]);
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(read_inside(&runtime, "cp-padded", "/work/padded"), "x");

    let cut = &archive[..archive.len() - 1024];
    let (code, _, err) = cp_bytes(&runtime, &["-", "cp-padded:/work"], cut);
    assert_eq!(code, Some(1));
    assert!(err.contains("cut off"), "{err}");
}

/// The helper dies with `cp`: killed, `cp` leaves nothing running.
#[test]
#[ignore = "needs root"]
fn cp_helper_dies_with_cp() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-orphan", "exec sleep 60", |_| {}, |_| {});
    let host = HostDir::new("orphan");
    let mut child = Command::new(RUNTIME.path())
        .env("SHIPYARD_TEST_HELPER_PAUSE", "10000")
        .arg("--root")
        .arg(&runtime.root)
        .args(["cp", "cp-orphan:/marker", &host.arg("m")])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let helper = helper_of(child.id());
    child.kill().unwrap();
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Path::new(&format!("/proc/{helper}/stat")).exists()
        && std::fs::read_to_string(format!("/proc/{helper}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    {
        assert!(Instant::now() < deadline, "the helper outlived cp");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What a message quotes can be the container's choosing (a path): its
/// control characters are shown escaped, never sent to the terminal it's
/// printed on, whichever command says it.
#[test]
#[ignore = "needs root"]
fn messages_show_control_characters_escaped() {
    let runtime = Runtime::new();
    let _bundle = cp_target(&runtime, "cp-escaped", "exec sleep 60", |_| {}, |_| {});
    let out = runtime.output_dir().join("escaped-out");
    std::fs::create_dir_all(runtime.output_dir()).unwrap();
    let call = cp(
        &runtime,
        &["cp-escaped:/missing\u{1b}[31m", out.to_str().unwrap()],
    );
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(!call.stderr.contains('\u{1b}'), "{:?}", call.stderr);
    assert!(call.stderr.contains("\\u{1b}[31m"), "{:?}", call.stderr);
    // exec's too: the program it was asked for, named in its refusal.
    let exec = exec(&runtime, &["cp-escaped", "/no\u{1b}[2Jsuch"]);
    assert_eq!(exec.code, Some(255), "{}", exec.stderr);
    assert!(!exec.stderr.contains('\u{1b}'), "{:?}", exec.stderr);
    assert_eq!(
        runtime.call(&["delete", "--force", "cp-escaped"]).code,
        Some(0)
    );
}

// --- fd passing (8.5b) ------------------------------------------------------------------------

/// A bundle running `sh -c SCRIPT` with `/proc` mounted (to list its own
/// descriptors), its rootfs owned for a user namespace if it has one.
fn bundle_with_proc(script: &str) -> Bundle {
    let bundle = Bundle::new();
    std::fs::create_dir_all(bundle.root.path().join("proc")).unwrap();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    config["mounts"] = json!([{"destination": "/proc", "type": "proc", "source": "proc"}]);
    bundle.write(&config);
    bundle
}

/// `shipyard-runtime` run through `sh -c PREFIX; exec "$@"`: `PREFIX`
/// opens what's to be passed (`exec 3<FILE`), sets `LISTEN_*`, … for the
/// runtime, which `exec` makes the same process (so `$$` is its PID).
fn runtime_after(prefix: &str) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", &format!("{prefix}; exec \"$@\""), "sh"])
        .arg(RUNTIME.path());
    command
}

/// Has `command`'s process get `fd` (this process's, close-on-exec) at 3,
/// not close-on-exec, whatever number it has here (`dup2` onto itself would
/// leave the flag).
fn give_at_3(command: &mut Command, fd: std::os::fd::RawFd) {
    // SAFETY: `dup2` and `fcntl` are async-signal-safe; `fd` outlives the
    // spawn.
    unsafe {
        command.pre_exec(move || {
            let done = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, 3)
            };
            if done == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Two host files, `a` and `b`, to pass on.
fn passed_files() -> HostDir {
    let host = HostDir::new("passed");
    std::fs::write(host.join("a"), "from a\n").unwrap();
    std::fs::write(host.join("b"), "from b\n").unwrap();
    host
}

/// `create --preserve-fds 2`: the container's process has the caller's 3
/// and 4 (read from them), and nothing else of the caller's.
#[test]
#[ignore = "needs root"]
fn create_preserves_fds_and_nothing_else() {
    let runtime = Runtime::new();
    let host = passed_files();
    let bundle = bundle_with_proc("cat <&3; cat <&4; ls /proc/self/fd | tr '\\n' ' '");
    bundle.root.add_host_program("tr");
    let prefix = format!(
        "exec 3<{} 4<{} 5<{}",
        host.arg("a"),
        host.arg("b"),
        host.arg("a")
    );
    let created = runtime.create_with(
        "pass-create",
        &bundle,
        &["--preserve-fds", "2"],
        runtime_after(&prefix),
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-create"]).code, Some(0));
    runtime.wait_for("pass-create", "stopped");
    // `ls` itself has its listing open (5): 3 and 4 are the passed ones,
    // and the caller's 5 isn't there.
    assert_eq!(
        runtime.output_of("pass-create"),
        "from a\nfrom b\n0 1 2 3 4 5 "
    );
}

/// With descriptors passed, a program that can't run still says so: the
/// pipe its failure goes back on is never where a passed one is put (the
/// init makes it after the gate, when 3 and up would otherwise be free).
#[test]
#[ignore = "needs root"]
fn a_failing_program_with_passed_fds_still_says_so() {
    let runtime = Runtime::new();
    let host = passed_files();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["no-such-program"]);
    bundle.write(&config);
    let created = runtime.create_with(
        "pass-missing",
        &bundle,
        // Enough that the init's own pipes, made before and after the gate,
        // would land on targets were they not held.
        &["--preserve-fds", "6"],
        runtime_after(&format!(
            "exec 3<{a} 4<{a} 5<{a} 6<{a} 7<{a} 8<{a}",
            a = host.arg("a")
        )),
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-missing"]).code, Some(0));
    runtime.wait_for("pass-missing", "stopped");
    let stderr = std::fs::read_to_string(runtime.output_dir().join("pass-missing.err")).unwrap();
    assert!(
        stderr.contains("no-such-program: command not found"),
        "{stderr:?}"
    );
}

/// What's passed is checked before anything is made: a descriptor that
/// isn't open is refused, and the container doesn't exist.
#[test]
#[ignore = "needs root"]
fn create_refuses_fds_that_arent_open() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exec sleep 60");
    let created = runtime.create_with(
        "pass-closed",
        &bundle,
        &["--preserve-fds", "3"],
        runtime_after("exec 3</dev/null"),
    );
    assert_eq!(created.code, Some(1), "{}", created.stderr);
    assert!(
        created.stderr.contains("descriptor 4 isn't open"),
        "{}",
        created.stderr
    );
    assert!(runtime.state("pass-closed").is_none());
}

/// Socket activation through `create`: the container's process gets the
/// socket at 3, `LISTEN_FDS`, `LISTEN_FDNAMES`, and its own PID as
/// `LISTEN_PID` (not the caller's, nor 1: Shipyard's init is 1).
#[test]
#[ignore = "needs root"]
fn create_passes_socket_activation_on() {
    use std::os::fd::AsRawFd as _;
    let runtime = Runtime::new();
    let host = HostDir::new("activation");
    let listener = std::os::unix::net::UnixListener::bind(host.join("sock")).unwrap();
    let inode = std::fs::metadata(format!("/proc/self/fd/{}", listener.as_raw_fd()))
        .map(|meta| meta.ino())
        .unwrap();
    let bundle = bundle_with_proc(
        "echo $LISTEN_FDS $LISTEN_FDNAMES $([ \"$LISTEN_PID\" = $$ ] && echo own-pid); \
         readlink /proc/self/fd/3",
    );
    bundle.root.add_host_program("readlink");
    let fd = listener.as_raw_fd();
    let mut command = runtime_after("LISTEN_PID=$$; export LISTEN_PID");
    command.env("LISTEN_FDS", "1").env("LISTEN_FDNAMES", "web");
    give_at_3(&mut command, fd);
    let created = runtime.create_with("pass-listen", &bundle, &[], command);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-listen"]).code, Some(0));
    runtime.wait_for("pass-listen", "stopped");
    assert_eq!(
        runtime.output_of("pass-listen"),
        format!("1 web own-pid\nsocket:[{inode}]\n")
    );
}

/// `LISTEN_FDS` meant for another process (`LISTEN_PID` not the runtime's):
/// nothing passed, nothing set.
#[test]
#[ignore = "needs root"]
fn socket_activation_for_another_process_is_ignored() {
    let runtime = Runtime::new();
    let bundle = bundle_with_proc("echo ${LISTEN_FDS-none}; ls /proc/self/fd | tr '\\n' ' '");
    bundle.root.add_host_program("tr");
    let mut command = runtime_after("exec 3</dev/null; LISTEN_PID=1; export LISTEN_PID");
    command.env("LISTEN_FDS", "1");
    let created = runtime.create_with("pass-other", &bundle, &[], command);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-other"]).code, Some(0));
    runtime.wait_for("pass-other", "stopped");
    assert_eq!(runtime.output_of("pass-other"), "none\n0 1 2 3 ");
}

/// Only the container's process holds what's passed once it runs: a pipe's
/// write end, passed and closed inside, reads end-of-file outside (no copy
/// left in `create`, the monitor or the init).
#[test]
#[ignore = "needs root"]
fn only_the_containers_process_holds_what_is_passed() {
    use std::os::fd::AsRawFd as _;
    let runtime = Runtime::new();
    // Close-on-exec: the test's other children must not hold it.
    let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
    let fd = write.as_raw_fd();
    let mut command = runtime_after(":");
    give_at_3(&mut command, fd);
    let bundle = bundle_running("echo passed >&3; exec 3>&-; exec sleep 60");
    let created = runtime.create_with("pass-hold", &bundle, &["--preserve-fds", "1"], command);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    drop(write);
    assert_eq!(runtime.call(&["start", "pass-hold"]).code, Some(0));
    let mut file = std::fs::File::from(read);
    let mut got = String::new();
    let reader = std::thread::spawn(move || {
        std::io::Read::read_to_string(&mut file, &mut got).unwrap();
        got
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reader.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a copy of the passed pipe is held elsewhere"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(reader.join().unwrap(), "passed\n");
}

/// `exec --preserve-fds`: the command gets them at 3 and up, its own
/// failures still reported (Shipyard's pipes aren't where the passed ones
/// go), and `LISTEN_PID` is its PID inside the container.
#[test]
#[ignore = "needs root"]
fn exec_preserves_fds() {
    let runtime = Runtime::new();
    let host = passed_files();
    let bundle = bundle_with_proc("exec sleep 60");
    bundle.root.add_host_program("tr");
    let created = runtime.create("pass-exec", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-exec"]).code, Some(0));
    let exec_with = |prefix: &str, args: &[&str]| {
        let mut command = runtime_after(prefix);
        command
            .arg("--root")
            .arg(&runtime.root)
            .arg("exec")
            .args(args)
            .stdin(Stdio::null());
        let output = command.output().unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let prefix = format!("exec 3<{} 4<{}", host.arg("a"), host.arg("b"));
    let (code, out, err) = exec_with(
        &prefix,
        &[
            "--preserve-fds",
            "2",
            "pass-exec",
            "sh",
            "-c",
            "cat <&3; cat <&4; ls /proc/self/fd | tr '\\n' ' '",
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "from a\nfrom b\n0 1 2 3 4 5 ");
    let (code, _, err) = exec_with(
        &prefix,
        &["--preserve-fds", "2", "pass-exec", "/no/such/program"],
    );
    assert_eq!(code, Some(255));
    assert!(err.contains("/no/such/program"), "{err}");
    let (code, out, err) = exec_with(
        &format!("{prefix}; LISTEN_PID=$$ LISTEN_FDS=1; export LISTEN_PID LISTEN_FDS"),
        &[
            "pass-exec",
            "sh",
            "-c",
            "echo $LISTEN_FDS $([ \"$LISTEN_PID\" = $$ ] && echo own-pid); cat <&3",
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "1 own-pid\nfrom a\n");
}

/// Socket activation's descriptors come first, then `--preserve-fds`'s;
/// and a bundle's own `LISTEN_FDS` with `--preserve-fds` (Podman's way)
/// gets the process's own PID as `LISTEN_PID`, not the bundle's 1.
#[test]
#[ignore = "needs root"]
fn activation_comes_before_preserved_and_a_bundles_own_gets_its_pid() {
    let runtime = Runtime::new();
    let host = passed_files();
    let bundle = bundle_running(
        "echo $LISTEN_FDS $([ \"$LISTEN_PID\" = $$ ] && echo own-pid); cat <&3; cat <&4",
    );
    let mut command = runtime_after(&format!(
        "exec 3<{} 4<{}; LISTEN_PID=$$ LISTEN_FDS=1; export LISTEN_PID LISTEN_FDS",
        host.arg("a"),
        host.arg("b")
    ));
    command.env_remove("LISTEN_FDNAMES");
    let created = runtime.create_with("pass-order", &bundle, &["--preserve-fds", "1"], command);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-order"]).code, Some(0));
    runtime.wait_for("pass-order", "stopped");
    assert_eq!(
        runtime.output_of("pass-order"),
        "1 own-pid\nfrom a\nfrom b\n"
    );

    let podman = Bundle::new();
    let mut config = podman.config();
    config["process"]["args"] = json!([
        "sh",
        "-c",
        "echo $LISTEN_FDS $([ \"$LISTEN_PID\" = $$ ] && echo own-pid); cat <&3"
    ]);
    config["process"]["env"] = json!(["PATH=/bin", "LISTEN_FDS=1", "LISTEN_PID=1"]);
    podman.write(&config);
    let created = runtime.create_with(
        "pass-podman",
        &podman,
        &["--preserve-fds", "1"],
        runtime_after(&format!("exec 3<{}", host.arg("a"))),
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-podman"]).code, Some(0));
    runtime.wait_for("pass-podman", "stopped");
    assert_eq!(runtime.output_of("pass-podman"), "1 own-pid\nfrom a\n");
}

/// The command's own descriptor limit must leave room above what's passed:
/// refused before anything runs, by `exec` too, in words.
#[test]
#[ignore = "needs root"]
fn a_descriptor_limit_without_room_is_refused() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", "exec sleep 60"]);
    config["process"]["rlimits"] = json!([{"type": "RLIMIT_NOFILE", "soft": 20, "hard": 20}]);
    bundle.write(&config);
    let created = runtime.create("pass-nofile", &bundle, &[]);
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-nofile"]).code, Some(0));
    let output = runtime_after("exec 3</dev/null 4</dev/null")
        .arg("--root")
        .arg(&runtime.root)
        .args([
            "exec",
            "--preserve-fds",
            "2",
            "pass-nofile",
            "sh",
            "-c",
            ":",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(255), "{stderr}");
    assert!(stderr.contains("raise its RLIMIT_NOFILE"), "{stderr}");
}

/// Rootless too.
#[test]
fn a_rootless_create_preserves_fds() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let host = passed_files();
    let bundle = bundle_running("cat <&3");
    let created = runtime.create_with(
        "pass-rootless",
        &bundle,
        &["--preserve-fds", "1"],
        runtime_after(&format!("exec 3<{}", host.arg("a"))),
    );
    assert_eq!(created.code, Some(0), "{}", created.stderr);
    assert_eq!(runtime.call(&["start", "pass-rootless"]).code, Some(0));
    runtime.wait_for("pass-rootless", "stopped");
    assert_eq!(runtime.output_of("pass-rootless"), "from a\n");
}

// --- Stage 8 as a whole (8.8a) ---------------------------------------------------------

/// Stage 8's milestone, rootless, on one container: created with its output
/// in logs and input through attach, then started. `logs --follow` sees what
/// it writes as it writes it; attach types to it and detaches, and it runs
/// on; `exec -t` runs a command on a terminal of its own inside it; `cp`
/// puts a file in and takes it back out; killed and deleted, nothing of it
/// is left.
#[test]
fn the_stage_8_milestone_one_rootless_container_end_to_end() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let id = "milestone-8";
    let _bundle = attachable(
        &runtime,
        id,
        "echo ready; while read line; do echo got-$line; done",
        &[],
    );
    let follow = Attached::running(&runtime, &["logs", "--follow", id]);
    follow.expect("ready");
    let mut attached = Attached::start(&runtime, id, &[]);
    attached.type_keys(b"one\n");
    attached.expect("got-one");
    attached.type_keys(DETACH);
    assert_eq!(attached.end().0, Some(0));
    assert_eq!(runtime.status(id), "running");
    follow.expect("got-one");
    let tty = exec(&runtime, &["-t", id, "sh", "-c", "tty"]);
    assert_eq!(tty.code, Some(0), "{}", tty.stderr);
    assert!(tty.stdout.starts_with("/dev/pts/"), "{:?}", tty.stdout);
    let host = runtime.root.with_extension("cp");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(host.join("in"), "carried in\n").unwrap();
    let inside = format!("{id}:/tmp/carried");
    let copied_in = cp(&runtime, &[host.join("in").to_str().unwrap(), &inside]);
    assert_eq!(copied_in.code, Some(0), "{}", copied_in.stderr);
    let copied_out = cp(&runtime, &[&inside, host.join("out").to_str().unwrap()]);
    assert_eq!(copied_out.code, Some(0), "{}", copied_out.stderr);
    assert_eq!(
        std::fs::read_to_string(host.join("out")).unwrap(),
        "carried in\n"
    );
    let state = std::fs::read(runtime.root.join(id).join("state.json")).unwrap();
    let record: Value = serde_json::from_slice(&state).unwrap();
    let monitor = record["monitor"][0].as_i64().unwrap();
    let monitor_started = record["monitor"][1].as_u64().unwrap();
    let cgroup = record["cgroup"].as_str().map(PathBuf::from);
    // With a cgroup delegated to this user, the container has one of its
    // own there (and it's checked below).
    if delegated_cgroup().is_some() {
        assert!(cgroup.is_some(), "no cgroup recorded: {record}");
    }
    assert_eq!(runtime.call(&["kill", id, "KILL"]).code, Some(0));
    runtime.wait_for(id, "stopped");
    // `logs --follow` ends with the container.
    assert_eq!(follow.end().0, Some(0));
    assert_eq!(runtime.call(&["delete", id]).code, Some(0));
    assert!(runtime.state(id).is_none());
    assert!(!runtime.root.join(id).exists(), "its state is gone");
    // Its cgroup, if it had one of its own here, and its monitor too (that
    // process, not another given its PID since).
    if let Some(cgroup) = cgroup {
        assert!(!cgroup.exists(), "{} is left", cgroup.display());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(monitor) && start_time_of(monitor) == Some(monitor_started) {
        assert!(
            Instant::now() < deadline,
            "its monitor {monitor} is still there"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&host);
}

/// A timing budget (H7): `exec` of a command that does nothing starts and
/// ends in under a second (tens of milliseconds in the test container), the
/// best of five, rootless. A second gained on its path (joining, the
/// confinement, the relay) fails it.
#[test]
fn an_exec_starts_and_ends_quickly() {
    if !rootless(Rootless::Single) {
        return;
    }
    let runtime = Runtime::new();
    let id = "exec-budget";
    let _bundle = attachable(&runtime, id, "exec sleep 60", &[]);
    let took = (0..5)
        .map(|_| {
            let started = Instant::now();
            // `:`, the shell's own: nothing but the exec itself.
            let call = exec(&runtime, &[id, "sh", "-c", ":"]);
            assert_eq!(call.code, Some(0), "{}", call.stderr);
            started.elapsed()
        })
        .min()
        .unwrap();
    assert!(took < Duration::from_secs(1), "{took:?}");
    assert_eq!(runtime.call(&["delete", "--force", id]).code, Some(0));
}
