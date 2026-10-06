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

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::is_root;

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
        std::fs::create_dir_all(self.output_dir()).unwrap();
        // (An ID with a `/` is refused, but its file name must still work.)
        let name = id.replace('/', "_");
        let out = self.output_dir().join(format!("{name}.out"));
        let err = self.output_dir().join(format!("{name}.err"));
        let status = Command::new(RUNTIME)
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

#[test]
fn creating_needs_root() {
    if is_root() {
        return;
    }
    let runtime = Runtime::new();
    let call = runtime.create("c1", &Bundle::new(), &[]);
    assert_eq!(call.code, Some(1), "{}", call.stderr);
    assert!(call.stderr.contains("needs root"), "{}", call.stderr);
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
#[test]
#[ignore = "needs root"]
fn a_container_that_exits_is_stopped() {
    let runtime = Runtime::new();
    let bundle = bundle_running("exit 3");
    assert_eq!(runtime.create("exits", &bundle, &[]).code, Some(0));
    assert_eq!(runtime.call(&["start", "exits"]).code, Some(0));
    runtime.wait_for("exits", "stopped");
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = runtime.root.join("exits/exit");
    while !exit.exists() {
        assert!(Instant::now() < deadline, "no exit recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
    let recorded = std::fs::read_to_string(exit).unwrap();
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

/// Without a PID namespace, the runtime couldn't find or stop all the
/// container's processes: refused before anything exists.
#[test]
#[ignore = "needs root"]
fn a_bundle_without_a_pid_namespace_is_refused() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["linux"]["namespaces"] = json!([{"type": "mount"}]);
    bundle.write(&config);
    let call = runtime.create("flat", &bundle, &[]);
    assert_eq!(call.code, Some(1));
    assert!(call.stderr.contains("no pid namespace"), "{}", call.stderr);
    assert!(!runtime.root.join("flat").exists());
}

/// What a bundle asks for that is ignored is said, as by `shipyard run
/// --bundle`; warnings and errors both reach `--log`.
#[test]
#[ignore = "needs root"]
fn warnings_and_errors_reach_stderr_and_the_log() {
    let runtime = Runtime::new();
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["hooks"] = json!({"prestart": [{"path": "/bin/true"}]});
    bundle.write(&config);
    let log = runtime.root.with_extension("log");
    let log_flags = ["--log", log.to_str().unwrap(), "--log-format", "json"];
    let call = runtime.create("warned", &bundle, &log_flags);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert!(
        call.stderr.contains("shipyard-runtime: warning: ") && call.stderr.contains("hooks"),
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
    assert!(lines[0]["msg"].as_str().unwrap().contains("hooks"));
    assert_eq!(lines[1]["level"], "error");
    assert!(lines[1]["msg"].as_str().unwrap().contains("exists"));
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
/// through its monitor, and the container dies with the monitor.
#[test]
#[ignore = "needs root"]
fn deleting_a_container_whose_setup_hangs() {
    let runtime = Runtime::new();
    let bundle = bundle_running("sleep 300");
    let (mut create, monitor, init) = (0..20)
        .find_map(|_| hang_a_setup(&runtime, &bundle))
        .expect("the monitor won the race for the lock every time");
    let call = runtime.call(&["delete", "hung"]);
    assert_eq!(call.code, Some(0), "{}", call.stderr);
    assert_eq!(runtime.status("hung"), "gone");
    eventually("the monitor and the init gone", || {
        (!alive(i64::from(monitor)) && !alive(i64::from(init))).then_some(())
    });
    // `create` learns its monitor died, and fails.
    assert_ne!(create.wait().unwrap().code(), Some(0));
}

/// Starts `create` for container `hung` and makes its setup hang: the
/// monitor records the init under the container's lock, so the test takes
/// the lock as soon as the monitor is recorded (`create` writes that under
/// the lock too, just before it lets go), waits for the init, stops the
/// monitor, and lets go. Returns `create`, the monitor and the init.
///
/// The monitor can win the race for the lock (a setup takes milliseconds):
/// then the attempt is undone, and `None`.
fn hang_a_setup(runtime: &Runtime, bundle: &Bundle) -> Option<(std::process::Child, i32, i32)> {
    use std::os::unix::process::CommandExt as _;

    use nix::sys::signal::{Signal, kill};
    let create = Command::new(RUNTIME)
        .arg("--root")
        .arg(&runtime.root)
        .args(["create", "--bundle", bundle.arg(), "hung"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let dir = runtime.root.join("hung");
    let monitor = eventually("the monitor recorded", || {
        let record: Value =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).ok()?).ok()?;
        i32::try_from(record["monitor"][0].as_i64()?).ok()
    });
    let lock = nix::fcntl::Flock::lock(
        std::fs::File::open(&dir).unwrap(),
        nix::fcntl::FlockArg::LockExclusive,
    )
    .unwrap();
    let init = eventually("the init spawned", || {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .find(|&pid| parent_of(pid) == Some(monitor))
    });
    let monitor_pid = nix::unistd::Pid::from_raw(monitor);
    kill(monitor_pid, Signal::SIGSTOP).unwrap();
    drop(lock);
    if runtime.status("hung") == "creating" {
        return Some((create, monitor, init));
    }
    // Lost: the init was recorded first. Undo, for another try.
    kill(monitor_pid, Signal::SIGCONT).unwrap();
    let mut create = create;
    let _ = create.wait();
    let _ = runtime.call(&["delete", "--force", "hung"]);
    None
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
    let ended = std::fs::read_to_string(runtime.root.join("gated").join("exit")).unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = runtime.root.join("missing/exit");
    while !exit.exists() {
        assert!(Instant::now() < deadline, "no exit recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        std::fs::read_to_string(exit)
            .unwrap()
            .contains("exit code 127")
    );
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
