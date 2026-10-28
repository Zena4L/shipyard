//! The Docker API end to end (step 11.2a): `shipyard system service` on a
//! socket of its own, driven by Docker's own CLI (`docker`, D11.2-4) and by
//! a raw HTTP client for the exact statuses; socket activation as systemd
//! does it; the idle exit; another user refused.
//!
//! Each test has stores of its own and the fixture registry's image, as
//! tests/containers.rs's. Linux only; a host where an image can't run says
//! why and returns, and so does one without `docker` (unless
//! `SHIPYARD_TEST_TOOLS=required`, as test-linux.sh sets).

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use shipyard_test_support::registry::fixture_requiring;
use shipyard_test_support::{Shipyard, api, eventually, is_root, reference_tool};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// How long the service may take to listen.
const LISTENING: Duration = Duration::from_secs(10);

/// One test's stores, fixture image, and service: test-support's stores,
/// with what only the API's tests do.
struct Stores(shipyard_test_support::stores::Stores);

impl std::ops::Deref for Stores {
    type Target = shipyard_test_support::stores::Stores;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Stores {
    /// Its stores, if an image runs here (else it says why), with the
    /// image pulled (the API's create never pulls), and a Docker config
    /// directory of its own.
    fn new() -> Option<Self> {
        Some(Self::wrapping(shipyard_test_support::stores::Stores::new(
            SHIPYARD,
        )?))
    }

    /// [`new`](Self::new), the fixture image's config's `config` object
    /// `runtime` (JSON: its `Cmd`).
    fn with_config(runtime: &str) -> Option<Self> {
        Some(Self::wrapping(
            shipyard_test_support::stores::Stores::serving(
                SHIPYARD,
                shipyard_test_support::registry::fixture_with_config(runtime),
            )?,
        ))
    }

    /// `stores`, with a Docker config directory of its own.
    fn wrapping(stores: shipyard_test_support::stores::Stores) -> Self {
        std::fs::create_dir_all(stores.scratch.0.join("docker")).unwrap();
        Self(stores)
    }

    /// The service's socket.
    fn socket(&self) -> PathBuf {
        self.scratch.0.join("run").join("api.sock")
    }

    /// `shipyard system service` on [`socket`](Self::socket), listening.
    /// As root, in a network namespace of its own: the bridge networks its
    /// containers get go with it, where they'd pile up in the tests' shared
    /// one (the default network has twelve subnets to choose from).
    fn serve(&self, idle_seconds: u32) -> Service {
        self.serve_in(idle_seconds, is_root(), None)
    }

    /// [`serve`](Self::serve) in the tests' own network namespace: where
    /// the fixture registry on loopback can be reached (a pull), for a test
    /// that starts no container on a network.
    fn serve_here(&self, idle_seconds: u32) -> Service {
        self.serve_in(idle_seconds, false, None)
    }

    /// [`serve`](Self::serve), started with the descriptor limit `nofile`
    /// (soft, hard), as systemd starts it with its own.
    fn serve_limited(&self, idle_seconds: u32, nofile: (u64, u64)) -> Service {
        self.serve_in(idle_seconds, is_root(), Some(nofile))
    }

    /// The service, in a network namespace of its own if `own_network`,
    /// with the descriptor limit `nofile` if one is given.
    fn serve_in(
        &self,
        idle_seconds: u32,
        own_network: bool,
        nofile: Option<(u64, u64)>,
    ) -> Service {
        use std::os::unix::process::CommandExt as _;
        let mut command = self.command(&["system", "service", "--socket"]);
        // Its log kept in the stores, where a test can read it.
        let log = std::fs::File::create(self.scratch.0.join("service.log")).unwrap();
        command
            .arg(self.socket())
            .env("SHIPYARD_API_IDLE", idle_seconds.to_string())
            .stdout(Stdio::null())
            .stderr(log);
        if own_network {
            // SAFETY: `unshare` is a system call, async-signal-safe;
            // nothing else runs between the fork and the exec.
            unsafe {
                command.pre_exec(|| {
                    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNET)
                        .map_err(std::io::Error::from)
                });
            }
        }
        if let Some((soft, hard)) = nofile {
            // SAFETY: `setrlimit` is a system call, async-signal-safe.
            unsafe {
                command.pre_exec(move || {
                    nix::sys::resource::setrlimit(
                        nix::sys::resource::Resource::RLIMIT_NOFILE,
                        soft,
                        hard,
                    )
                    .map_err(std::io::Error::from)
                });
            }
        }
        let child = command.spawn().unwrap();
        api::wait_for(&self.socket(), LISTENING);
        Running(child)
    }

    /// `docker ARGS` against the service, with a configuration directory of
    /// its own, not yet run.
    fn docker_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("docker");
        command
            .args(args)
            .env("DOCKER_HOST", format!("unix://{}", self.socket().display()))
            .env("DOCKER_CONFIG", self.scratch.0.join("docker"))
            .stdin(Stdio::null());
        command
    }

    /// `docker ARGS` against the service.
    fn docker(&self, args: &[&str]) -> Output {
        self.docker_command(args).output().expect("running docker")
    }

    /// `docker ARGS`, running: it (killed when dropped), and its stdout's
    /// lines as they come.
    fn docker_lines(&self, args: &[&str]) -> (Running, std::sync::mpsc::Receiver<String>) {
        use std::io::BufRead as _;
        let mut child = self
            .docker_command(args)
            .stdout(Stdio::piped())
            .spawn()
            .expect("running docker");
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        let (lines, read) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        (Running(child), read)
    }

    /// `docker ARGS`, which must succeed: its stdout, trimmed.
    fn docker_ok(&self, args: &[&str]) -> String {
        let output = self.docker(args);
        assert!(output.status.success(), "docker {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

/// A running command, killed at the end.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Docker's frames in `body`: each one's stream and payload.
fn frames(mut body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    while body.len() >= 8 {
        assert_eq!(body[1..4], [0, 0, 0], "a frame's header");
        let length = usize::try_from(u32::from_be_bytes(body[4..8].try_into().unwrap())).unwrap();
        assert!(body.len() >= 8 + length, "a frame cut short");
        frames.push((body[0], body[8..8 + length].to_vec()));
        body = &body[8 + length..];
    }
    assert!(body.is_empty(), "a frame cut short");
    frames
}

/// A running service, killed at the end.
type Service = Running;

/// `docker pause`, `unpause` and `update` against the API (step 11.3d1):
/// Docker's routes, its 409 for a second pause, the limits changed at once
/// (`inspect`'s `HostConfig`). Root, or a delegated cgroup.
#[test]
fn docker_pauses_and_updates() {
    if !reference_tool("docker") {
        return;
    }
    if !shipyard_test_support::is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "live",
        "--restart",
        "always",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(stores.docker_ok(&["pause", "live"]), "live");
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.State.Status}}", "live"]),
        "paused"
    );
    let again = stores.docker(&["pause", "live"]);
    assert_eq!(again.status.code(), Some(1), "{again:?}");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("is already paused"),
        "{again:?}"
    );
    assert_eq!(stores.docker_ok(&["unpause", "live"]), "live");
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.State.Status}}", "live"]),
        "running"
    );
    // Without `--restart`, Docker's CLI sends `{"Name": ""}`: the policy
    // is kept.
    stores.docker_ok(&["update", "--memory", "80m", "live"]);
    assert_eq!(
        stores.docker_ok(&[
            "inspect",
            "-f",
            "{{.HostConfig.RestartPolicy.Name}} {{.HostConfig.Memory}}",
            "live"
        ]),
        format!("always {}", 80_u64 << 20)
    );
    stores.docker_ok(&[
        "update",
        "--memory",
        "96m",
        "--memory-swap",
        "192m",
        "--pids-limit",
        "60",
        "--restart",
        "on-failure:2",
        "live",
    ]);
    assert_eq!(
        stores.docker_ok(&[
            "inspect",
            "-f",
            "{{.HostConfig.Memory}} {{.HostConfig.PidsLimit}} {{.HostConfig.RestartPolicy.Name}}",
            "live"
        ]),
        format!("{} 60 on-failure", 96_u64 << 20)
    );
    stores.docker_ok(&["rm", "-f", "live"]);
}

/// `docker top` and `docker stats` against the API (step 11.3d2):
/// Docker's `ps -ef` titles and the container's processes, `ps_args`
/// refused; `stats --no-stream`'s row; the streamed `GET /stats`, each
/// object carrying the one before as its `precpu_stats`, zeros once it's
/// stopped, its end once it's removed. Root, or a delegated cgroup.
#[test]
fn docker_tops_and_stats() {
    use std::io::{Read as _, Write as _};
    if !reference_tool("docker") {
        return;
    }
    if !shipyard_test_support::is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "watched",
        "--memory",
        "64m",
        &stores.image,
        "sh",
        "-c",
        "sleep 300 & wait",
    ]);
    // `run -d` returns once its init runs: its child comes after.
    let mut top = String::new();
    eventually(Duration::from_secs(10), "its sleep forked", || {
        top = stores.docker_ok(&["top", "watched"]);
        top.lines().any(|line| line.ends_with("sleep 300"))
    });
    assert!(top.starts_with("UID "), "{top}");
    let socket = stores.socket();
    let refused = api::get(&socket, "/containers/watched/top?ps_args=aux").unwrap();
    assert_eq!(refused.status, 400, "{refused:?}");
    let asked = api::get(&socket, "/containers/watched/top?ps_args=-ef").unwrap();
    assert_eq!(asked.status, 200, "Docker's daemon's default: {asked:?}");
    let row: serde_json::Value = serde_json::from_str(&stores.docker_ok(&[
        "stats",
        "--no-stream",
        "--format",
        "{{json .}}",
        "watched",
    ]))
    .unwrap();
    assert_eq!(row["Name"], "watched", "{row}");
    assert!(
        row["MemUsage"].as_str().unwrap().ends_with(" / 64MiB"),
        "{row}"
    );
    // Streamed: an object a second, the one before as its `precpu_stats`.
    let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET /containers/watched/stats HTTP/1.1\r\nHost: d\r\n\r\n")
        .unwrap();
    let mut read = Vec::new();
    let objects = |read: &[u8]| -> Vec<serde_json::Value> {
        String::from_utf8_lossy(read)
            .split("\r\n")
            // A chunk is an object and its line break: one cut short by
            // the read isn't whole yet.
            .filter(|part| part.starts_with('{') && part.ends_with('\n'))
            .map(|object| serde_json::from_str(object.trim()).unwrap())
            .collect()
    };
    while objects(&read).len() < 2 {
        let mut chunk = [0_u8; 4096];
        let got = stream.read(&mut chunk).expect("a second object");
        assert!(
            got > 0,
            "the stream ended: {}",
            String::from_utf8_lossy(&read)
        );
        read.extend_from_slice(&chunk[..got]);
    }
    let first_two = objects(&read);
    assert_eq!(first_two[0]["preread"], "0001-01-01T00:00:00Z");
    assert_eq!(first_two[1]["preread"], first_two[0]["read"]);
    assert_eq!(first_two[1]["precpu_stats"], first_two[0]["cpu_stats"]);
    assert_eq!(first_two[1]["memory_stats"]["limit"], 64_u64 << 20);
    assert!(
        first_two[1]["networks"]
            .as_object()
            .is_some_and(|networks| !networks.contains_key("lo") && !networks.is_empty()),
        "its interfaces, lo left out: {}",
        first_two[1]
    );
    // Stopped, it streams zeros (Docker's); removed, the stream ends.
    stores.docker_ok(&["stop", "-t", "0", "watched"]);
    let mut read_more = |read: &mut Vec<u8>| {
        let mut chunk = [0_u8; 4096];
        let got = stream.read(&mut chunk).expect("more of the stream");
        assert!(got > 0, "closed before its last chunk");
        read.extend_from_slice(&chunk[..got]);
    };
    while !objects(&read)
        .last()
        .is_some_and(|last| last["pids_stats"]["current"] == 0)
    {
        read_more(&mut read);
    }
    stores.docker_ok(&["rm", "-f", "watched"]);
    while !read.ends_with(b"\r\n0\r\n\r\n") {
        read_more(&mut read);
    }
}

/// `GET /containers/{id}/stats` not streamed (step 11.3d2): two samples a
/// second apart, or one with `one-shot`; without a limit, the host's memory
/// is its limit (Docker's); no networks on the host's. Root, or a delegated
/// cgroup.
#[test]
fn docker_stats_once() {
    if !reference_tool("docker") {
        return;
    }
    if !shipyard_test_support::is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    stores.docker_ok(&["run", "-d", "--name", "free", &stores.image, "sleep", "300"]);
    let once = |path: &str| -> serde_json::Value {
        let answer = api::get(&socket, path).unwrap();
        assert_eq!(answer.status, 200, "{answer:?}");
        serde_json::from_slice(&answer.body).unwrap()
    };
    let twice = once("/containers/free/stats?stream=0");
    assert_ne!(twice["preread"], "0001-01-01T00:00:00Z", "{twice}");
    assert_eq!(
        twice["precpu_stats"]["online_cpus"],
        twice["cpu_stats"]["online_cpus"]
    );
    let shot = once("/containers/free/stats?stream=0&one-shot=1");
    assert_eq!(shot["preread"], "0001-01-01T00:00:00Z", "{shot}");
    let total = std::fs::read_to_string("/proc/meminfo")
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemTotal:")?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .unwrap();
    assert_eq!(shot["memory_stats"]["limit"], total * 1024, "{shot}");
    assert!(shot["networks"].is_object(), "its own network's: {shot}");
    stores.docker_ok(&["rm", "-f", "free"]);
    // On the host's network: no counters of its own (the host's aren't).
    if shipyard_test_support::is_root() {
        stores.docker_ok(&[
            "run",
            "-d",
            "--name",
            "hosted",
            "--network",
            "host",
            &stores.image,
            "sleep",
            "300",
        ]);
        let hosted = once("/containers/hosted/stats?stream=0&one-shot=1");
        assert!(hosted.get("networks").is_none(), "{hosted}");
        assert!(
            hosted["pids_stats"]["current"].as_u64() > Some(0),
            "a running one's: {hosted}"
        );
        stores.docker_ok(&["rm", "-f", "hosted"]);
    }
}

/// `docker run --tmpfs` and `--mount` (its three types) against the API
/// (step 11.3e2): Docker's CLI sends `HostConfig.Tmpfs` and `Mounts`, and
/// the container gets each, the tmpfs as Docker's (`noexec`, its size and
/// mode); Docker's `inspect` reads them back.
#[test]
fn docker_runs_with_tmpfs_and_mounts() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let shared = stores.scratch.0.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("file"), "from the host\n").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(shared.join("file"), std::fs::Permissions::from_mode(0o644)).unwrap();
    stores.docker_ok(&["volume", "create", "named"]);
    let shared_mount = format!("type=bind,source={},target=/d,readonly", shared.display());
    let out = stores.docker_ok(&[
        "run",
        "--name",
        "mounted",
        "--tmpfs",
        "/scratch:size=1m",
        "--mount",
        "type=tmpfs,target=/t,tmpfs-size=2097152,tmpfs-mode=1770",
        "--mount",
        &shared_mount,
        "--mount",
        "type=volume,source=named,target=/v",
        &stores.image,
        "sh",
        "-c",
        "cat /proc/mounts; cat /d/file; echo kept > /v/x && echo written; \
         (touch /d/new 2>/dev/null && echo bind-written) || echo bind-read-only",
    ]);
    let scratch = out
        .lines()
        .find(|line| line.contains(" /scratch "))
        .unwrap();
    assert!(
        scratch.contains("noexec") && scratch.contains("size=1024k"),
        "{scratch}"
    );
    let t = out.lines().find(|line| line.contains(" /t ")).unwrap();
    assert!(t.contains("size=2048k") && t.contains("mode=1770"), "{t}");
    assert!(
        out.contains("from the host") && out.contains("written"),
        "{out}"
    );
    assert!(out.lines().any(|line| line == "bind-read-only"), "{out}");
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{json .HostConfig.Tmpfs}}", "mounted"]),
        r#"{"/scratch":"size=1m","/t":"size=2097152,mode=1770"}"#
    );
}

/// A bind's missing host path, made as Docker's daemon makes it (step
/// 11.4a2b): at the create (`docker create -v /missing:/x`), and again at
/// each start once it's gone; the CLI's `-v` still refuses a missing one
/// (the control: its start fails where the API's makes it).
#[test]
fn a_bind_s_source_is_made_again_at_each_start() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let made = stores.scratch.0.join("made/here");
    let bind = format!("{}:/x", made.display());
    stores.docker_ok(&[
        "create",
        "--name",
        "api",
        "-v",
        &bind,
        &stores.image,
        "true",
    ]);
    assert!(made.is_dir(), "made at the create");
    std::fs::remove_dir(&made).unwrap();
    stores.docker_ok(&["start", "api"]);
    assert!(made.is_dir(), "made again at the start");
    let kept = stores.scratch.0.join("kept");
    std::fs::create_dir(&kept).unwrap();
    let cli_bind = format!("{}:/x", kept.display());
    let created = stores.shipyard(&[
        "create",
        "--name",
        "cli",
        "-v",
        &cli_bind,
        &stores.image,
        "true",
    ]);
    assert!(created.status.success(), "{created:?}");
    std::fs::remove_dir(&kept).unwrap();
    let started = stores.shipyard(&["start", "cli"]);
    assert!(
        !started.status.success(),
        "the CLI's -v makes none: {started:?}"
    );
    assert!(!kept.exists());
    // `start` doesn't wait for its end: forced.
    stores.docker_ok(&["rm", "-f", "api"]);
    stores.shipyard(&["rm", "-f", "cli"]);
}

/// A bind source Shipyard made is reached, at each later start, without
/// following a symlink (11.7): one put where it made a directory (a
/// container sharing a directory above it, here the test) refuses the
/// start, and nothing is made or bound through it. The controls: its first
/// start binds it (what's written there is on the host), and a source that
/// was there already is bound through a symlink as Docker binds it.
#[test]
fn a_made_bind_source_is_never_reached_through_a_symlink() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let shared = stores.scratch.0.join("shared");
    std::fs::create_dir(&shared).unwrap();
    let made = shared.join("made/data");
    let bind = format!("{}:/x", made.display());
    stores.docker_ok(&[
        "create",
        "--name",
        "made",
        "--network",
        "none",
        "-v",
        &bind,
        &stores.image,
        "sh",
        "-c",
        "echo written > /x/note",
    ]);
    stores.docker_ok(&["start", "-a", "made"]);
    assert_eq!(
        std::fs::read_to_string(made.join("note")).unwrap(),
        "written\n"
    );

    let outside = stores.scratch.0.join("outside");
    std::fs::create_dir_all(outside.join("data")).unwrap();
    std::fs::remove_dir_all(shared.join("made")).unwrap();
    std::os::unix::fs::symlink(&outside, shared.join("made")).unwrap();
    let refused = stores.docker(&["start", "-a", "made"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("has a symlink or a file where Shipyard makes a directory"),
        "{refused:?}"
    );
    assert!(
        !outside.join("data/note").exists(),
        "nothing written through the link"
    );

    // Docker's own: a source that's there, through a symlink, is bound.
    let real = stores.scratch.0.join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, stores.scratch.0.join("alias")).unwrap();
    let alias = format!("{}:/y", stores.scratch.0.join("alias").display());
    stores.docker_ok(&[
        "run",
        "--rm",
        "--network",
        "none",
        "-v",
        &alias,
        &stores.image,
        "sh",
        "-c",
        "echo through > /y/note",
    ]);
    assert_eq!(
        std::fs::read_to_string(real.join("note")).unwrap(),
        "through\n"
    );
    stores.docker_ok(&["rm", "-f", "made"]);
}

/// Docker's anonymous volumes against the API (step 11.4a2a): `docker run
/// -v /data` (`Volumes`) and a `--mount` volume without a source each get
/// one, labelled anonymous; `docker rm` keeps them, `docker rm -v`
/// (`DELETE ?v=1`) takes them, `docker run --rm` takes its own.
#[test]
fn docker_s_anonymous_volumes_come_and_go() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let anonymous = || -> Vec<String> {
        stores
            .docker_ok(&[
                "volume",
                "ls",
                "-q",
                "--filter",
                "label=com.docker.volume.anonymous",
            ])
            .lines()
            .map(str::to_owned)
            .collect()
    };
    stores.docker_ok(&[
        "run",
        "--name",
        "kept",
        "-v",
        "/data",
        "--mount",
        "type=volume,target=/m",
        &stores.image,
        "sh",
        "-c",
        "echo k > /data/f && echo k > /m/f",
    ]);
    let made = anonymous();
    assert_eq!(made.len(), 2, "{made:?}");
    assert!(made.iter().all(|name| name.len() == 64), "{made:?}");
    stores.docker_ok(&["rm", "kept"]);
    assert_eq!(anonymous(), made, "kept by a plain rm");
    stores.docker_ok(&["create", "--name", "gone", "-v", "/data", &stores.image]);
    assert_eq!(anonymous().len(), 3);
    stores.docker_ok(&["rm", "-v", "gone"]);
    assert_eq!(anonymous(), made, "rm -v takes its own");
    stores.docker_ok(&[
        "run",
        "-d",
        "--rm",
        "--name",
        "brief",
        "-v",
        "/data",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(anonymous().len(), 3, "there while it runs");
    stores.docker_ok(&["stop", "-t", "1", "brief"]);
    eventually(Duration::from_secs(10), "--rm takes its own", || {
        anonymous() == made
    });
    for name in &made {
        stores.docker_ok(&["volume", "rm", name]);
    }
}

/// `docker container prune`, `image prune -a` and `system prune` against
/// the API (step 11.3e1): the stopped container removed and the running
/// one kept, Docker's answers' keys (`null` for nothing deleted), the
/// image kept while a container uses it and removed after.
#[test]
fn docker_prunes() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let image = stores.image.clone();
    stores.docker_ok(&["create", "--name", "stopped", &image, "sh", "-c", "exit 0"]);
    stores.docker_ok(&["run", "-d", "--name", "up", &image, "sleep", "300"]);
    let stopped = stores.docker_ok(&["inspect", "-f", "{{.Id}}", "stopped"]);
    let pruned = stores.docker_ok(&["container", "prune", "-f"]);
    assert!(pruned.contains(&stopped), "{pruned}");
    assert_eq!(
        stores.docker_ok(&["ps", "-a", "--format", "{{.Names}}"]),
        "up"
    );
    let answer = api::post(&socket, "/containers/prune", None).unwrap();
    assert_eq!(answer.status, 200, "{answer:?}");
    let body: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(
        body,
        json!({"ContainersDeleted": null, "SpaceReclaimed": 0})
    );
    let refused = api::post(
        &socket,
        "/containers/prune?filters=%7B%22status%22%3A%5B%22exited%22%5D%7D",
        None,
    )
    .unwrap();
    assert_eq!(refused.status, 400, "a filter not read: {refused:?}");
    // Used by `up`: kept.
    stores.docker_ok(&["image", "prune", "-a", "-f"]);
    assert!(!stores.docker_ok(&["images", "-q"]).is_empty());
    stores.docker_ok(&["rm", "-f", "up"]);
    let answer = api::post(
        &socket,
        "/images/prune?filters=%7B%22dangling%22%3A%5B%22false%22%5D%7D",
        None,
    )
    .unwrap();
    assert_eq!(answer.status, 200, "{answer:?}");
    let body: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
    let deleted = body["ImagesDeleted"].as_array().expect("deleted");
    assert!(deleted[0]["Untagged"].is_string(), "{body}");
    assert!(
        deleted.iter().any(|entry| entry["Deleted"].is_string()),
        "{body}"
    );
    assert!(body["SpaceReclaimed"].as_u64() > Some(0), "{body}");
    assert_eq!(stores.docker_ok(&["images", "-q"]), "");
    // Docker's `system prune` asks each kind, the build cache too (none:
    // nothing deleted), with `until` for each.
    let built = api::post(&socket, "/build/prune", None).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&built.body).unwrap(),
        json!({"CachesDeleted": null, "SpaceReclaimed": 0})
    );
    for args in [
        &["system", "prune", "-f"][..],
        &["system", "prune", "-f", "--filter", "until=24h"],
    ] {
        let system = stores.docker(args);
        assert!(system.status.success(), "{args:?}: {system:?}");
    }
}

/// `docker login` and `docker push` through the API (step 12.4): the
/// sign-in checked by the service (`POST /auth`) and kept by Docker's CLI,
/// which hands it in with the push (`X-Registry-Auth`); the image on the
/// registry under its ID, every write signed in; Docker's CLI reads the
/// push's lines (its `digest: … size: …`).
#[test]
fn docker_pushes_through_the_api() {
    use shipyard_test_support::push_registry::{Auth, PushRegistry};
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    // The loopback registry reachable (a push, no container).
    let _service = stores.serve_here(60);
    let basic = {
        use base64::Engine as _;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("me:pw")
        )
    };
    let registry = PushRegistry::start(Auth::Basic(basic.clone()));
    let name = format!("{}/app:1", registry.base);
    stores.docker_ok(&["tag", &stores.image, &name]);
    let mut login =
        stores.docker_command(&["login", "-u", "me", "--password-stdin", &registry.base]);
    let mut child = login
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(b"pw\n").unwrap();
    }
    let signed = child.wait_with_output().unwrap();
    assert!(signed.status.success(), "{signed:?}");
    let pushed = stores.docker_ok(&["push", &name]);
    let id = stores.docker_ok(&["image", "inspect", "--format", "{{.Id}}", &name]);
    assert!(
        pushed.contains(&format!("1: digest: {id} size: ")),
        "{pushed}"
    );
    let state = registry.state();
    assert!(
        state
            .manifests
            .contains_key(&("app".to_owned(), "1".to_owned()))
    );
    assert!(
        state
            .log
            .iter()
            .filter(|seen| seen.method != "GET" && seen.method != "HEAD")
            .all(|seen| seen.authorization.as_deref() == Some(basic.as_str())),
        "{:?}",
        state.log
    );
}

/// `docker save`, `load` and `history` through the API (step 12.2a): two
/// names in one archive (`/images/get?names=…&names=…`), the archive loaded
/// back once both are removed, the same IDs; a cut archive refused, as
/// Docker's CLI reads the failure; `history`'s rows.
#[test]
fn docker_saves_loads_and_shows_history() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let image = stores.image.clone();
    stores.docker_ok(&["tag", &image, "saved/second:2"]);
    let ids = || {
        let mut lines: Vec<String> = stores
            .docker_ok(&[
                "images",
                "--no-trunc",
                "--format",
                "{{.Repository}}:{{.Tag}} {{.ID}}",
            ])
            .lines()
            .map(str::to_owned)
            .collect();
        // One load gives its names one time: in either order.
        lines.sort();
        lines
    };
    let before = ids();
    let archive = stores.scratch.0.join("saved.tar");
    let archive = archive.to_str().unwrap();
    stores.docker_ok(&["save", "-o", archive, &image, "saved/second:2"]);
    // One image, two names: Docker's `manifest.json` has both tags.
    let listed = Command::new("tar")
        .args(["-xOf", archive, "manifest.json"])
        .output()
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(manifest.as_array().map(Vec::len), Some(1), "{manifest}");
    assert_eq!(
        manifest[0]["RepoTags"].as_array().map(Vec::len),
        Some(2),
        "{manifest}"
    );

    // A repository alone is every tag of it (here only `:2`, no
    // `:latest`): found as the save finds it, not refused 404.
    let repository = stores.scratch.0.join("repository.tar");
    stores.docker_ok(&["save", "-o", repository.to_str().unwrap(), "saved/second"]);
    // (Docker's CLI refuses a device for `-o` itself: a file.)
    let nothing = stores.scratch.0.join("nothing.tar");
    let missing = stores.docker(&["save", "-o", nothing.to_str().unwrap(), "no/such"]);
    assert!(!missing.status.success(), "{missing:?}");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("No such image: no/such"),
        "{missing:?}"
    );
    stores.docker_ok(&["rmi", &image, "saved/second:2"]);
    assert_eq!(stores.docker_ok(&["images", "-q"]), "");
    let said = stores.docker_ok(&["load", "-i", archive]);
    assert!(said.contains("Loaded image: saved/second:2"), "{said}");
    assert_eq!(ids(), before, "the same names and IDs");

    // Cut short: refused, nothing more loaded, and Docker's CLI fails.
    let whole = std::fs::read(archive).unwrap();
    let cut = stores.scratch.0.join("cut.tar");
    std::fs::write(&cut, &whole[..whole.len() / 2]).unwrap();
    let refused = stores.docker(&["load", "-i", cut.to_str().unwrap()]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("cut short"),
        "{refused:?}"
    );

    let history = stores.docker_ok(&["history", "--no-trunc", "-q", "saved/second:2"]);
    let rows: Vec<&str> = history.lines().collect();
    assert!(rows[0].starts_with("sha256:"), "{history}");
    assert_eq!(rows.len(), 1, "one layer: {history}");
    let table = stores.docker_ok(&["history", "saved/second:2"]);
    assert!(table.starts_with("IMAGE"), "{table}");
}

/// `docker export` and `docker import` through the API (step 12.2b): a
/// container's own filesystem out, imported back with a change, run; an
/// import with no name refused before anything is kept (its body several
/// MiB: Docker's CLI reads the refusal, not a broken pipe); an export of a
/// container that isn't there, Docker's 404.
#[test]
fn docker_exports_and_imports() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let image = stores.image.clone();
    stores.docker_ok(&["create", "--name", "src", &image]);
    let archive = stores.scratch.0.join("rootfs.tar");
    let archive = archive.to_str().unwrap();
    stores.docker_ok(&["export", "-o", archive, "src"]);
    assert!(std::fs::metadata(archive).unwrap().len() > 1024 * 1024);
    let id = stores.docker_ok(&[
        "import",
        "-c",
        r#"CMD ["/bin/cat", "/marker"]"#,
        "-m",
        "via the API",
        archive,
        "imported:1",
    ]);
    assert!(id.starts_with("sha256:"), "{id}");
    let ran = stores.docker_ok(&["run", "--rm", "--network", "none", "imported:1"]);
    assert!(ran.contains("shipyard test root filesystem"), "{ran}");

    // No `-m`: Docker's CLI sends `message` empty, and the comment is
    // Docker's own.
    stores.docker_ok(&["import", archive, "imported:2"]);
    let comment = stores.docker_ok(&["history", "--format", "{{.Comment}}", "imported:2"]);
    assert_eq!(comment, "Imported from -");
    let unnamed = stores.docker(&["import", archive]);
    assert!(!unnamed.status.success(), "{unnamed:?}");
    assert!(
        String::from_utf8_lossy(&unnamed.stderr).contains("give the image a name"),
        "{unnamed:?}"
    );
    let missing = stores.docker(&["export", "-o", archive, "no-such"]);
    assert!(!missing.status.success(), "{missing:?}");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("No such container"),
        "{missing:?}"
    );
}

/// Docker's CLI drives a container's whole life through the API: create,
/// start, list, inspect, stop, wait, remove; `run -d`, kill and a forced
/// removal. Shipyard's own CLI sees what it made: one store.
#[test]
fn docker_drives_a_container_s_life() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    assert_eq!(
        stores.docker_ok(&["version", "--format", "{{.Server.APIVersion}}"]),
        "1.43"
    );
    let id = stores.docker_ok(&[
        "create",
        "--name",
        "web",
        "--label",
        "app=web",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(id.len(), 64, "{id}");
    assert_eq!(
        stores.docker_ok(&[
            "ps",
            "-a",
            "--format",
            "{{.Names}} {{.State}} {{.Label \"app\"}}"
        ]),
        "web created web"
    );
    assert_eq!(stores.docker_ok(&["start", "web"]), "web");
    assert!(
        stores
            .docker_ok(&["ps", "--format", "{{.Names}} {{.Status}}"])
            .starts_with("web Up "),
    );
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.State.Running}} {{.Name}}", "web"]),
        "true /web"
    );
    // Shipyard's own CLI sees it: one store.
    let ours = stores.shipyard(&["inspect", "--shipyard", "web"]);
    assert!(ours.status.success(), "{ours:?}");
    let ours: serde_json::Value = serde_json::from_slice(&ours.stdout).unwrap();
    assert_eq!(ours[0]["id"], id.as_str(), "{ours}");
    assert_eq!(stores.docker_ok(&["stop", "-t", "1", "web"]), "web");
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.State.Status}}", "web"]),
        "exited"
    );
    // SIGTERM ended it: 128 + 15.
    assert_eq!(stores.docker_ok(&["wait", "web"]), "143");
    assert_eq!(stores.docker_ok(&["rm", "web"]), "web");
    assert_eq!(stores.docker_ok(&["ps", "-a", "-q"]), "");

    let id = stores.docker_ok(&["run", "-d", "--name", "bg", &stores.image, "sleep", "300"]);
    assert_eq!(id.len(), 64, "{id}");
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.State.Running}}", "bg"]),
        "true"
    );
    assert_eq!(stores.docker_ok(&["kill", "bg"]), "bg");
    assert_eq!(stores.docker_ok(&["wait", "bg"]), "137");
    let id = stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "forced",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(id.len(), 64);
    assert_eq!(
        stores.docker_ok(&["rm", "-f", "forced", "bg"]),
        "forced\nbg"
    );
    assert_eq!(stores.docker_ok(&["ps", "-a", "-q"]), "");
}

/// The answers a client acts on, exactly: 201 and the ID; 304 for a start
/// with nothing to do and a stop likewise; 409 for a name taken and for a
/// kill of a stopped container; 404 for an image that isn't stored (Docker's
/// words: the client pulls it) and for a container there isn't.
#[test]
fn statuses_are_docker_s() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let body = json!({"Image": stores.image, "Cmd": ["sleep", "300"], "Labels": {"app": "web"}});
    let created = api::post(&socket, "/v1.43/containers/create?name=web", Some(&body)).unwrap();
    assert_eq!(created.status, 201, "{}", created.text());
    let id = created.json()["Id"].as_str().unwrap().to_owned();
    assert_eq!(created.json()["Warnings"], json!([]));
    let taken = api::post(&socket, "/containers/create?name=web", Some(&body)).unwrap();
    assert_eq!(taken.status, 409, "{}", taken.text());
    let missing = json!({"Image": "nowhere.invalid/nothing:1"});
    let unstored = api::post(&socket, "/containers/create", Some(&missing)).unwrap();
    assert_eq!(unstored.status, 404);
    assert_eq!(
        unstored.json()["message"],
        "No such image: nowhere.invalid/nothing:1"
    );
    assert_eq!(
        api::get(&socket, "/containers/nothing/json")
            .unwrap()
            .status,
        404
    );

    let killed_stopped = api::post(&socket, "/containers/web/kill", None).unwrap();
    assert_eq!(killed_stopped.status, 409, "{}", killed_stopped.text());
    assert_eq!(
        api::post(&socket, "/containers/web/stop", None)
            .unwrap()
            .status,
        304
    );
    assert_eq!(
        api::post(&socket, "/containers/web/start", None)
            .unwrap()
            .status,
        204
    );
    assert_eq!(
        api::post(&socket, "/containers/web/start", None)
            .unwrap()
            .status,
        304
    );
    let inspected = api::get(&socket, &format!("/containers/{id}/json")).unwrap();
    assert_eq!(inspected.status, 200);
    assert_eq!(inspected.json()["State"]["Running"], true);
    assert_eq!(inspected.json()["Config"]["Labels"]["app"], "web");
    let listed = api::get(
        &socket,
        "/containers/json?filters=%7B%22label%22%3A%5B%22app%3Dweb%22%5D%7D",
    )
    .unwrap();
    assert_eq!(
        listed.json().as_array().unwrap().len(),
        1,
        "{}",
        listed.text()
    );
    let other = api::get(
        &socket,
        "/containers/json?filters=%7B%22label%22%3A%5B%22app%3Ddb%22%5D%7D",
    )
    .unwrap();
    assert_eq!(other.json(), json!([]));
    assert_eq!(
        api::post(&socket, "/containers/web/stop?t=1", None)
            .unwrap()
            .status,
        204
    );
    let waited = api::post(&socket, "/containers/web/wait", None).unwrap();
    assert_eq!(waited.json(), json!({"StatusCode": 143}));
    let removed = api::request(&socket, "DELETE", "/containers/web", None).unwrap();
    assert_eq!(removed.status, 204);
    assert_eq!(
        api::get(&socket, "/containers/web/json").unwrap().status,
        404
    );
}

/// A wait for the next exit waits past the end the container already had:
/// its first run exits 3, its second (held until the test lets it go)
/// exits 4, and only a wait for the next end answers 4.
#[test]
fn a_wait_for_the_next_exit_waits_for_a_new_one() {
    use std::io::{Read as _, Write as _};
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let script = "if [ -e /ran ]; then while [ ! -e /go ]; do sleep 0.1; done; exit 4; fi; \
                  touch /ran; exit 3";
    let body = json!({"Image": stores.image, "Cmd": ["sh", "-c", script]});
    let created = api::post(&socket, "/containers/create?name=once", Some(&body)).unwrap();
    assert_eq!(created.status, 201, "{}", created.text());
    assert_eq!(
        api::post(&socket, "/containers/once/start", None)
            .unwrap()
            .status,
        204
    );
    let first = api::post(&socket, "/containers/once/wait", None).unwrap();
    assert_eq!(first.json(), json!({"StatusCode": 3}));
    assert_eq!(
        api::post(&socket, "/containers/once/start", None)
            .unwrap()
            .status,
        204
    );
    // The wait's headers come at once (Docker's CLI starts the container
    // only once it has them): its first look is behind it then.
    let mut waiter = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    waiter
        .write_all(
            b"POST /containers/once/wait?condition=next-exit HTTP/1.1\r\nHost: d\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    waiter
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        waiter
            .read_exact(&mut byte)
            .expect("the wait's headers, before the end");
        head.push(byte[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    let go = stores.shipyard(&["exec", "once", "touch", "/go"]);
    assert!(go.status.success(), "{go:?}");
    waiter
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut rest = Vec::new();
    waiter.read_to_end(&mut rest).unwrap();
    let rest = String::from_utf8_lossy(&rest);
    assert!(rest.contains(r#"{"StatusCode":4}"#), "{rest}");
}

/// systemd's way: the listening socket as descriptor 3, with `LISTEN_PID`
/// naming the service and `LISTEN_FDS=1`. It serves that socket, binds none
/// of its own, and leaves the socket when it ends (it's systemd's).
#[test]
fn it_serves_the_socket_systemd_passes() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    let Some(stores) = Stores::new() else { return };
    let dir = stores.scratch.0.join("systemd");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("api.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let fd = listener.as_raw_fd();
    // `$$` is the shell's PID, and `exec` keeps it: the service's.
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "LISTEN_PID=$$ LISTEN_FDS=1 exec \"$0\" system service",
            SHIPYARD.path(),
        ])
        .envs(
            stores
                .command(&[])
                .get_envs()
                .filter_map(|(key, value)| Some((key, value?))),
        )
        .env("SHIPYARD_API_IDLE", "1")
        .stdout(Stdio::null());
    // SAFETY: `dup2` and `fcntl` are async-signal-safe, and nothing else
    // runs here.
    unsafe {
        command.pre_exec(move || {
            // The listener may be 3 already: `dup2(3, 3)` does nothing, its
            // close-on-exec flag kept, so the service would get no socket.
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
    let mut child = command.spawn().unwrap();
    drop(listener);
    let ping = api::get(&path, "/_ping").unwrap();
    assert_eq!(ping.status, 200);
    assert!(!stores.socket().exists(), "it bound no socket of its own");
    let started = Instant::now();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(path.exists(), "systemd's socket is left");
}

/// Idle for its idle time, the service ends, and removes its socket; an
/// open connection holds it up.
#[test]
fn the_service_ends_once_idle() {
    let Some(stores) = Stores::new() else { return };
    let mut service = stores.serve(1);
    eventually(Duration::from_secs(10), "the service ends", || {
        service.0.try_wait().unwrap().is_some()
    });
    assert!(service.0.try_wait().unwrap().unwrap().success());
    assert!(!stores.socket().exists(), "its socket is removed");
    // A second service binds it again: nothing was left holding it.
    let _again = stores.serve(60);
    assert_eq!(api::get(&stores.socket(), "/_ping").unwrap().status, 200);
}

/// systemd's stop (SIGTERM): the service ends at once when nothing is under
/// way, its socket removed, and the containers it started run on.
#[test]
fn a_stop_signal_ends_the_service() {
    let Some(stores) = Stores::new() else { return };
    let mut service = stores.serve(60);
    let socket = stores.socket();
    let body = json!({"Image": stores.image, "Cmd": ["sleep", "300"]});
    assert_eq!(
        api::post(&socket, "/containers/create?name=runs-on", Some(&body))
            .unwrap()
            .status,
        201
    );
    assert_eq!(
        api::post(&socket, "/containers/runs-on/start", None)
            .unwrap()
            .status,
        204
    );
    let pid = nix::unistd::Pid::from_raw(i32::try_from(service.0.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    eventually(Duration::from_secs(10), "the service ends", || {
        service.0.try_wait().unwrap().is_some()
    });
    assert!(service.0.try_wait().unwrap().unwrap().success());
    assert!(!socket.exists(), "its socket is removed");
    let inspected = stores.shipyard(&["inspect", "runs-on"]);
    assert!(inspected.status.success(), "{inspected:?}");
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspected[0]["State"]["Status"], "running", "it runs on");
    assert!(stores.shipyard(&["rm", "-f", "runs-on"]).status.success());
}

/// A second service on the same socket is refused, the first left serving.
#[test]
fn a_second_service_is_refused() {
    let Some(stores) = Stores::new() else { return };
    let _first = stores.serve(60);
    let second = stores
        .command(&["system", "service", "--socket"])
        .arg(stores.socket())
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(said.contains("is served already"), "{said}");
    assert_eq!(api::get(&stores.socket(), "/_ping").unwrap().status, 200);
}

/// Another user is closed on without an answer, even with the socket's
/// mode opened to everyone: the peer's UID is checked (`SO_PEERCRED`).
/// Root only (another user is needed); Docker's CLI is the client, as
/// root (answered: the control) and as nobody (refused).
#[test]
#[ignore = "needs root"]
fn another_user_is_refused() {
    use std::os::unix::process::CommandExt as _;
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    // Opened to everyone, so that only the peer check stands.
    for dir in [stores.scratch.0.as_path(), socket.parent().unwrap()] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    let version = ["version", "--format", "{{.Server.APIVersion}}"];
    assert_eq!(stores.docker_ok(&version), "1.43", "root is answered");
    let nobody = Command::new("docker")
        .args(version)
        .env("DOCKER_HOST", format!("unix://{}", socket.display()))
        .env("DOCKER_CONFIG", "/nonexistent")
        .uid(65534)
        .gid(65534)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!nobody.status.success(), "nobody was answered: {nobody:?}");
    assert!(
        !String::from_utf8_lossy(&nobody.stdout).contains("1.43"),
        "{nobody:?}"
    );
    // It reached the socket (no permission refused it): the service closed
    // on it, unanswered (Go's client says that one of three ways, by when
    // it sees the close: 11.6a2's root pass saw the third once).
    let said = String::from_utf8_lossy(&nobody.stderr);
    assert!(
        (said.contains("connection reset by peer")
            || said.contains("EOF")
            || said.contains("server closed idle connection"))
            && !said.contains("permission denied"),
        "{said}"
    );
    // Still serving its owner.
    assert_eq!(api::get(&socket, "/_ping").unwrap().status, 200);
}

/// `docker logs`: each stream on its own, `--tail`, `-t`; the raw answer
/// in Docker's frames, stdout's as 1 and stderr's as 2; a terminal
/// container's raw.
#[test]
fn logs_are_docker_s() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    // Its stderr line well before the others: `--tail 1` is the last line
    // of both streams, and the two are read through pipes of their own (no
    // order between them otherwise).
    let script = "echo oops >&2; sleep 1; echo one; echo two; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "talker",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    eventually(Duration::from_secs(10), "its output", || {
        stores.docker_ok(&["logs", "talker"]).contains("two")
    });
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(120);
    let logs = stores.docker(&["logs", "talker"]);
    assert_eq!(String::from_utf8_lossy(&logs.stdout), "one\ntwo\n");
    assert_eq!(String::from_utf8_lossy(&logs.stderr), "oops\n");
    assert_eq!(stores.docker_ok(&["logs", "--tail", "1", "talker"]), "two");
    // Times: all of them since a while ago, none up to then, none from a
    // time to come.
    let since = before.to_string();
    let later = (before + 3600).to_string();
    assert_eq!(
        stores.docker_ok(&["logs", "--since", &since, "talker"]),
        "one\ntwo"
    );
    assert_eq!(stores.docker_ok(&["logs", "--until", &since, "talker"]), "");
    assert_eq!(stores.docker_ok(&["logs", "--since", &later, "talker"]), "");
    let stamped = stores.docker_ok(&["logs", "-t", "--tail", "1", "talker"]);
    assert!(
        stamped.ends_with(" two") && stamped.starts_with("20") && stamped.contains('T'),
        "{stamped}"
    );
    let raw = api::get(&socket, "/containers/talker/logs?stdout=1&stderr=1").unwrap();
    assert_eq!(raw.status, 200);
    assert_eq!(
        raw.header("Content-Type"),
        Some("application/vnd.docker.multiplexed-stream")
    );
    let got = frames(&raw.body);
    let stream = |kind: u8| -> Vec<u8> {
        got.iter()
            .filter(|(k, _)| *k == kind)
            .flat_map(|(_, payload)| payload.clone())
            .collect()
    };
    assert_eq!(stream(1), b"one\ntwo\n");
    assert_eq!(stream(2), b"oops\n");
    let neither = api::get(&socket, "/containers/talker/logs").unwrap();
    assert_eq!(neither.status, 400);
    assert_eq!(
        api::get(&socket, "/containers/nothing/logs?stdout=1")
            .unwrap()
            .status,
        404
    );

    stores.docker_ok(&[
        "run",
        "-d",
        "-t",
        "--name",
        "tty",
        &stores.image,
        "sh",
        "-c",
        "echo hi; sleep 300",
    ]);
    eventually(Duration::from_secs(10), "its output", || {
        api::get(&socket, "/containers/tty/logs?stdout=1")
            .unwrap()
            .text()
            .contains("hi")
    });
    let raw = api::get(&socket, "/containers/tty/logs?stdout=1").unwrap();
    assert_eq!(
        raw.header("Content-Type"),
        Some("application/vnd.docker.raw-stream")
    );
    assert!(raw.text().starts_with("hi"), "{:?}", raw.text());
    stores.docker_ok(&["rm", "-f", "talker", "tty"]);
}

/// `docker logs -f` follows, and ends with its container.
#[test]
fn logs_follow_until_the_container_ends() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    // The second line comes well after the follow began.
    let script = "echo first; sleep 3; echo second; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "follow",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    let (mut follow, lines) = stores.docker_lines(&["logs", "-f", "follow"]);
    let next = || lines.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(next(), "first");
    assert_eq!(next(), "second", "followed as it was written");
    assert!(follow.0.try_wait().unwrap().is_none(), "still following");
    stores.docker_ok(&["stop", "-t", "1", "follow"]);
    eventually(Duration::from_secs(10), "the follow ends with it", || {
        follow.0.try_wait().unwrap().is_some()
    });
    stores.docker_ok(&["rm", "follow"]);
}

/// A timing budget (H7) for the API's bulk streams: 20,000 lines of logs
/// and a 64 MiB file's archive come whole and well inside their bounds
/// (11.7: a 50 ms wait whenever the channel filled held `logs` to some 320
/// lines a second, ~61 s for these, and `docker cp` to ~10 MiB/s).
#[test]
fn bulk_streams_keep_their_budget() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let script = "i=0; while [ $i -lt 20000 ]; do echo line$i; i=$((i+1)); done; \
                  head -c 67108864 /dev/zero >/big; echo done >&2; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "--network",
        "none",
        "--name",
        "bulk",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    eventually(Duration::from_secs(60), "its output and its file", || {
        stores.docker(&["logs", "bulk"]).stderr == b"done\n"
    });
    let began = std::time::Instant::now();
    let logs = api::get(&socket, "/containers/bulk/logs?stdout=1").unwrap();
    let took = began.elapsed();
    assert_eq!(logs.status, 200);
    let lines = frames(&logs.body);
    assert_eq!(lines.len(), 20_000);
    assert_eq!(
        lines.last().map(|(_, line)| line.as_slice()),
        Some(&b"line19999\n"[..])
    );
    assert!(took < Duration::from_secs(10), "20,000 lines took {took:?}");

    let began = std::time::Instant::now();
    let archive = api::get(&socket, "/containers/bulk/archive?path=/big").unwrap();
    let took = began.elapsed();
    assert_eq!(archive.status, 200);
    assert!(
        archive.body.len() > 64 << 20,
        "{} bytes",
        archive.body.len()
    );
    assert!(
        took < Duration::from_secs(10),
        "a 64 MiB archive took {took:?}"
    );
    stores.docker_ok(&["rm", "-f", "bulk"]);
}

/// The service's stop ends a follow it serves (it would hold the stop up
/// for good), and the stop is prompt.
#[test]
fn a_stop_ends_a_follow() {
    use std::io::Write as _;
    let Some(stores) = Stores::new() else { return };
    let mut service = stores.serve(60);
    let script = "echo ready; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "quiet",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    let mut follow = std::os::unix::net::UnixStream::connect(stores.socket()).unwrap();
    follow
        .write_all(b"GET /containers/quiet/logs?stdout=1&follow=1 HTTP/1.1\r\nHost: d\r\n\r\n")
        .unwrap();
    // Its first line read: the follow's reader is under way.
    follow
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut read = Vec::new();
    let mut byte = [0_u8; 1];
    while !read.windows(6).any(|window| window == b"ready\n") {
        std::io::Read::read_exact(&mut follow, &mut byte).expect("its first line");
        read.push(byte[0]);
    }
    assert!(read.starts_with(b"HTTP/1.1 200"), "{read:?}");
    let pid = nix::unistd::Pid::from_raw(i32::try_from(service.0.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    eventually(Duration::from_secs(10), "the service ends", || {
        service.0.try_wait().unwrap().is_some()
    });
    drop(follow);
    assert!(stores.shipyard(&["rm", "-f", "quiet"]).status.success());
}

/// `docker events`: between two times, with Docker's shape; filtered (by
/// its labels too); and followed as containers start. A filter Shipyard
/// can't read is refused.
#[test]
fn events_are_docker_s() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 1;
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "evented",
        "--label",
        "app=web",
        &stores.image,
        "true",
    ]);
    eventually(Duration::from_secs(10), "it ends", || {
        stores.docker_ok(&["inspect", "-f", "{{.State.Status}}", "evented"]) == "exited"
    });
    let until = since + 120;
    let window = [
        "events".to_owned(),
        "--since".to_owned(),
        since.to_string(),
        "--until".to_owned(),
        until
            .min(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 1,
            )
            .to_string(),
        "--format".to_owned(),
        "{{.Type}} {{.Action}} {{.Actor.Attributes.name}}".to_owned(),
    ];
    let window: Vec<&str> = window.iter().map(String::as_str).collect();
    let seen = stores.docker_ok(&window);
    for action in ["create", "start", "die"] {
        assert!(
            seen.contains(&format!("container {action} evented")),
            "{seen}"
        );
    }
    // Its own: the stores' probe ran a container in this window too.
    let mut filtered = window.clone();
    filtered.extend(["--filter", "event=die", "--filter", "container=evented"]);
    assert_eq!(stores.docker_ok(&filtered), "container die evented");

    let (_follow, lines) = stores.docker_lines(&[
        "events",
        "--filter",
        "container=evented",
        "--filter",
        "event=start",
        "--format",
        "{{.Action}} {{.Actor.Attributes.name}}",
    ]);
    // Followed: a start after it began.
    eventually(Duration::from_secs(10), "the start is followed", || {
        stores.docker_ok(&["start", "evented"]);
        lines
            .recv_timeout(Duration::from_millis(500))
            .is_ok_and(|line| line == "start evented")
    });
    // By its labels (step 11.4a1): a container's events carry them.
    let by_label = |label: &str| {
        let until = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1;
        stores.docker_ok(&[
            "events",
            "--since",
            &since.to_string(),
            "--until",
            &until.to_string(),
            "--filter",
            label,
            "--format",
            "{{.Action}}",
        ])
    };
    assert!(
        by_label("label=app=web")
            .lines()
            .any(|action| action == "start")
    );
    assert_eq!(by_label("label=app=db"), "", "another value matches none");
    let refused = api::get(
        &stores.socket(),
        "/events?filters=%7B%22daemon%22%3A%5B%22x%22%5D%7D&until=1",
    )
    .unwrap();
    assert_eq!(refused.status, 400, "a filter not read: {}", refused.text());
    stores.docker_ok(&["rm", "-f", "evented"]);
}

/// `docker run` attached: the output, each stream on its own, the exit
/// code, `--rm`; input through `-i`; a command's first line at its start
/// never missed (the attach is let in before the start gate opens).
#[test]
fn run_attached_is_docker_s() {
    use std::io::Write as _;
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let ran = stores.docker(&[
        "run",
        "--rm",
        &stores.image,
        "sh",
        "-c",
        "echo out; echo err >&2; exit 3",
    ]);
    assert_eq!(ran.status.code(), Some(3), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&ran.stderr), "err\n");
    eventually(Duration::from_secs(10), "--rm removes it", || {
        stores.docker_ok(&["ps", "-a", "-q"]).is_empty()
    });
    // Its first line, at once, every time.
    for _ in 0..5 {
        assert_eq!(
            stores.docker_ok(&["run", "--rm", &stores.image, "sh", "-c", "echo first"]),
            "first"
        );
    }
    // Its input's end ends `cat` (`StdinOnce`): bounded, a hang fails it.
    let mut cat = stores
        .docker_command(&["run", "--rm", "-i", &stores.image, "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    cat.stdin
        .take()
        .unwrap()
        .write_all(b"piped through\n")
        .unwrap();
    let (done, catted) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(cat.wait_with_output());
    });
    let catted = catted
        .recv_timeout(Duration::from_secs(60))
        .expect("`cat` ends at its input's end")
        .unwrap();
    assert!(catted.status.success(), "{catted:?}");
    assert_eq!(String::from_utf8_lossy(&catted.stdout), "piped through\n");
}

/// `docker start -a` of a created container: its output from its first
/// line; `docker attach` to a running one sees what it writes next, and
/// ends with it.
#[test]
fn start_attached_and_attach_are_docker_s() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&[
        "create",
        "--name",
        "made",
        &stores.image,
        "sh",
        "-c",
        "echo made then started",
    ]);
    assert_eq!(
        stores.docker_ok(&["start", "-a", "made"]),
        "made then started"
    );
    // Ticks until it's told to go: a tick read through the attach says it's in.
    let script = "while [ ! -e /tmp/go ]; do echo tick; sleep 0.1; done; echo later; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "attached",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    let (mut attached, lines) = stores.docker_lines(&["attach", "--no-stdin", "attached"]);
    assert_eq!(lines.recv_timeout(Duration::from_secs(20)).unwrap(), "tick");
    assert!(
        stores
            .shipyard(&["exec", "attached", "touch", "/tmp/go"])
            .status
            .success()
    );
    let after: Vec<String> =
        std::iter::from_fn(|| lines.recv_timeout(Duration::from_secs(20)).ok())
            .skip_while(|line| line == "tick")
            .take(1)
            .collect();
    assert_eq!(after, ["later"]);
    stores.docker_ok(&["stop", "-t", "1", "attached"]);
    eventually(Duration::from_secs(10), "the attach ends with it", || {
        attached.0.try_wait().unwrap().is_some()
    });
    // An ended container: `start -a` attaches first, then starts it.
    assert_eq!(
        stores.docker_ok(&["start", "-a", "made"]),
        "made then started",
        "again"
    );
    stores.docker_ok(&["rm", "made", "attached"]);
}

/// The detach keys typed through `docker attach` on a terminal (`script`
/// gives it one; Docker's keys detach terminal attaches) end the attach
/// and leave the container running.
#[test]
fn detach_keys_detach() {
    use std::io::{BufRead as _, Write as _};
    if !reference_tool("docker") || !reference_tool("script") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let ticks = "while true; do echo tick; sleep 0.2; done";
    stores.docker_ok(&[
        "run",
        "-d",
        "-i",
        "-t",
        "--name",
        "kept",
        &stores.image,
        "sh",
        "-c",
        ticks,
    ]);
    let command = format!(
        "DOCKER_HOST=unix://{} DOCKER_CONFIG={} docker attach --detach-keys ctrl-x kept",
        stores.socket().display(),
        stores.scratch.0.join("docker").display(),
    );
    let mut attach = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = attach.stdin.take().unwrap();
    // A tick read through the attach: it's in.
    let mut out = std::io::BufReader::new(attach.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("tick") {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "the attach's output");
    }
    input.write_all(b"\x18").unwrap();
    let mut attach = Running(attach);
    eventually(Duration::from_secs(10), "the keys detach", || {
        attach.0.try_wait().unwrap().is_some()
    });
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.State.Running}}", "kept"]),
        "true",
        "still running"
    );
    drop(input);
    stores.docker_ok(&["rm", "-f", "kept"]);
}

/// `docker run -it` on a terminal (`script` gives it one): the container
/// has a terminal of its own; and a resize through the API reaches it.
#[test]
fn terminals_are_docker_s() {
    if !reference_tool("docker") || !reference_tool("script") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let docker_host = format!("unix://{}", stores.socket().display());
    let command = format!(
        "DOCKER_HOST={docker_host} DOCKER_CONFIG={} docker run --rm -it {} sh -c 'tty; exit 4'",
        stores.scratch.0.join("docker").display(),
        stores.image
    );
    let ran = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ran.status.code(), Some(4), "{ran:?}");
    let said = String::from_utf8_lossy(&ran.stdout);
    assert!(
        said.contains("/dev/pts/"),
        "a terminal of its own: {said:?}"
    );

    let script = "while [ ! -e /tmp/go ]; do sleep 0.1; done; stty size; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "-t",
        "--name",
        "sized",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    let resized = api::post(&stores.socket(), "/containers/sized/resize?h=33&w=77", None).unwrap();
    assert_eq!(resized.status, 200, "{}", resized.text());
    assert!(
        stores
            .shipyard(&["exec", "sized", "touch", "/tmp/go"])
            .status
            .success()
    );
    eventually(Duration::from_secs(10), "its new size", || {
        stores.docker_ok(&["logs", "sized"]).contains("33 77")
    });
    // Without a terminal: nothing to do, as Docker's.
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "plain",
        &stores.image,
        "sleep",
        "300",
    ]);
    let plain = api::post(&stores.socket(), "/containers/plain/resize?h=33&w=77", None).unwrap();
    assert_eq!(plain.status, 200);
    let missing = api::post(&stores.socket(), "/containers/sized/resize", None).unwrap();
    assert_eq!(missing.status, 400);
    stores.docker_ok(&["rm", "-f", "sized", "plain"]);
}

/// An attach without `Upgrade` is refused (Docker's clients all ask for
/// it), before anything else.
#[test]
fn an_attach_without_upgrade_is_refused() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let body = json!({"Image": stores.image, "Cmd": ["sh", "-c", "exit 0"]});
    assert_eq!(
        api::post(&socket, "/containers/create?name=over", Some(&body))
            .unwrap()
            .status,
        201
    );
    let plain = api::post(&socket, "/containers/over/attach?stream=1&stdout=1", None).unwrap();
    assert_eq!(plain.status, 400, "{}", plain.text());
    assert!(
        plain.json()["message"]
            .as_str()
            .unwrap()
            .contains("Upgrade: tcp")
    );
}

/// `docker exec`: each stream on its own and the exit code; `-u`, `-w`; input
/// with `-i`; a terminal with `-it`; `-d`; refused for a container not
/// running.
#[test]
fn exec_is_docker_s() {
    use std::io::Write as _;
    if !reference_tool("docker") || !reference_tool("script") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&["run", "-d", "--name", "box", &stores.image, "sleep", "300"]);
    let ran = stores.docker(&["exec", "box", "sh", "-c", "echo out; echo err >&2; exit 3"]);
    assert_eq!(ran.status.code(), Some(3), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&ran.stderr), "err\n");

    assert_eq!(
        stores.docker_ok(&[
            "exec",
            "-u",
            "1000",
            "box",
            "stat",
            "-L",
            "-c",
            "%u",
            "/proc/self"
        ]),
        "1000"
    );
    assert_eq!(
        stores.docker_ok(&["exec", "-w", "/tmp", "box", "sh", "-c", "pwd"]),
        "/tmp"
    );
    let mut cat = stores
        .docker_command(&["exec", "-i", "box", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    cat.stdin.take().unwrap().write_all(b"piped\n").unwrap();
    let catted = cat.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&catted.stdout),
        "piped\n",
        "{catted:?}"
    );

    let command = format!(
        "DOCKER_HOST=unix://{} DOCKER_CONFIG={} docker exec -it box sh -c 'tty; exit 5'",
        stores.socket().display(),
        stores.scratch.0.join("docker").display(),
    );
    let ran = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ran.status.code(), Some(5), "{ran:?}");
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains("/dev/pts/"),
        "{ran:?}"
    );

    // A process it leaves in the background doesn't hold its end up.
    let began = Instant::now();
    let left = stores.docker(&["exec", "box", "sh", "-c", "sleep 60 & exit 7"]);
    assert_eq!(left.status.code(), Some(7), "{left:?}");
    assert!(
        began.elapsed() < Duration::from_secs(30),
        "{:?}",
        began.elapsed()
    );

    stores.docker_ok(&["exec", "-d", "box", "sh", "-c", "touch /tmp/detached"]);
    eventually(Duration::from_secs(10), "the detached exec ran", || {
        stores
            .docker(&["exec", "box", "sh", "-c", "test -e /tmp/detached"])
            .status
            .success()
    });

    stores.docker_ok(&["stop", "-t", "1", "box"]);
    let refused = stores.docker(&["exec", "box", "sh", "-c", "exit 0"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is not running"),
        "{refused:?}"
    );
    stores.docker_ok(&["rm", "box"]);
}

/// An exec's instance through the raw API: made (201), inspected before
/// its start (not running, no code), its start without `Upgrade` refused,
/// an unknown one 404.
#[test]
fn exec_instances_are_docker_s() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    stores.shipyard(&[
        "run",
        "-d",
        "--network",
        "none",
        "--name",
        "box",
        &stores.image,
        "sleep",
        "300",
    ]);
    let made = api::post(
        &socket,
        "/containers/box/exec",
        Some(&json!({"Cmd": ["sh", "-c", "exit 0"], "AttachStdout": true})),
    )
    .unwrap();
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 64);
    let inspected = api::get(&socket, &format!("/exec/{id}/json"))
        .unwrap()
        .json();
    assert_eq!(
        (inspected["Running"].clone(), inspected["ExitCode"].clone()),
        (json!(false), json!(null))
    );
    let start = api::post(
        &socket,
        &format!("/exec/{id}/start"),
        Some(&json!({"Detach": false})),
    )
    .unwrap();
    assert_eq!(start.status, 400, "{}", start.text());
    assert_eq!(api::get(&socket, "/exec/nothing/json").unwrap().status, 404);
    let privileged = api::post(
        &socket,
        "/containers/box/exec",
        Some(&json!({"Cmd": ["sh"], "Privileged": true})),
    )
    .unwrap();
    assert_eq!(privileged.status, 400);
    // Its container stopped since its create: a start is Docker's 409, not
    // an exec that fails with a code of its own (a detached start of one
    // made the same way works while it's up).
    let make = || {
        let made = api::post(
            &socket,
            "/containers/box/exec",
            Some(&json!({"Cmd": ["true"]})),
        )
        .unwrap();
        made.json()["Id"].as_str().unwrap().to_owned()
    };
    let start = |id: &str| {
        api::post(
            &socket,
            &format!("/exec/{id}/start"),
            Some(&json!({"Detach": true})),
        )
        .unwrap()
    };
    let (up, later) = (make(), make());
    assert_eq!(start(&up).status, 200);
    assert!(
        stores
            .shipyard(&["stop", "-t", "0", "box"])
            .status
            .success()
    );
    let refused = start(&later);
    assert_eq!(refused.status, 409, "{}", refused.text());
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// An exec's terminal through the raw API: sized as `ConsoleSize` asks at
/// its start, resized by `POST /exec/{id}/resize`; a resize of one without
/// a terminal refused (409); a client that hangs up ends its exec.
#[test]
fn exec_terminals_are_sized() {
    use std::io::Read as _;
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    assert!(
        stores
            .shipyard(&[
                "run",
                "-d",
                "--network",
                "none",
                "--name",
                "box",
                &stores.image,
                "sleep",
                "300",
            ])
            .status
            .success()
    );
    let make = |body: serde_json::Value| {
        let made = api::post(&socket, "/containers/box/exec", Some(&body)).unwrap();
        assert_eq!(made.status, 201, "{}", made.text());
        made.json()["Id"].as_str().unwrap().to_owned()
    };
    let script = "trap 'stty size' WINCH; stty size; while :; do sleep 0.1; done";
    let sized = make(json!({"Cmd": ["sh", "-c", script], "Tty": true, "AttachStdout": true}));
    let (status, mut session) = api::upgrade(
        &socket,
        &format!("/exec/{sized}/start"),
        &json!({"Detach": false, "Tty": true, "ConsoleSize": [33, 77]}),
    )
    .unwrap();
    assert_eq!(status, 101);
    session
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut said = Vec::new();
    let mut until = |wanted: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buffer = [0_u8; 1024];
        while !String::from_utf8_lossy(&said).contains(wanted) {
            assert!(
                Instant::now() < deadline,
                "{wanted:?} in {:?}",
                String::from_utf8_lossy(&said)
            );
            if let Ok(read) = session.read(&mut buffer) {
                said.extend_from_slice(&buffer[..read]);
            }
        }
    };
    until("33 77");
    let resized = api::post(&socket, &format!("/exec/{sized}/resize?h=40&w=100"), None).unwrap();
    assert_eq!(resized.status, 200, "{}", resized.text());
    until("40 100");
    // Its terminal on the host is raw: the container's `\r\n` passes as it
    // is, not made `\r\r\n`.
    let said = String::from_utf8_lossy(&said).into_owned();
    assert!(said.contains("33 77\r\n"), "{said:?}");
    assert!(!said.contains("\r\r"), "{said:?}");

    let plain = make(json!({"Cmd": ["sleep", "300"]}));
    let started = api::post(
        &socket,
        &format!("/exec/{plain}/start"),
        Some(&json!({"Detach": true})),
    )
    .unwrap();
    assert_eq!(started.status, 200, "{}", started.text());
    let refused = api::post(&socket, &format!("/exec/{plain}/resize?h=40&w=100"), None).unwrap();
    assert_eq!(refused.status, 409, "{}", refused.text());

    // Its client gone, the exec is hung up.
    let running = |id: &str| {
        api::get(&socket, &format!("/exec/{id}/json"))
            .unwrap()
            .json()["Running"]
            .clone()
    };
    assert_eq!(running(&sized), json!(true));
    drop(session);
    eventually(Duration::from_secs(30), "the exec hung up", || {
        running(&sized) == json!(false)
    });
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// F117: a detached exec (`docker exec -d`) holds the service up a while
/// at most: a long one runs on after the service's idle exit. Its `Pid` is
/// its command's, as the host sees it; with `Tty`, it has a terminal, as
/// Docker gives it one.
#[test]
fn a_detached_exec_lets_the_service_go() {
    let Some(stores) = Stores::new() else { return };
    let mut service = stores.serve(2);
    let socket = stores.socket();
    assert!(
        stores
            .shipyard(&[
                "run",
                "-d",
                "--network",
                "none",
                "--name",
                "box",
                &stores.image,
                "sleep",
                "300",
            ])
            .status
            .success()
    );
    let script = "if test -t 1; then echo yes > /tmp/tty; else echo no > /tmp/tty; fi; \
                  exec sleep 1000";
    let made = api::post(
        &socket,
        "/containers/box/exec",
        Some(&json!({"Cmd": ["sh", "-c", script], "Tty": true})),
    )
    .unwrap();
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_owned();
    let started = api::post(
        &socket,
        &format!("/exec/{id}/start"),
        Some(&json!({"Detach": true})),
    )
    .unwrap();
    assert_eq!(started.status, 200, "{}", started.text());
    // Its command's PID: the `sleep` it became, on the host.
    let mut pid = 0;
    eventually(Duration::from_secs(10), "its command's PID", || {
        let shown = api::get(&socket, &format!("/exec/{id}/json"))
            .unwrap()
            .json();
        pid = shown["Pid"].as_u64().unwrap_or(0);
        pid != 0
            && std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|line| line.starts_with(b"sleep\0"))
    });
    let tty = stores.shipyard(&["exec", "box", "cat", "/tmp/tty"]);
    assert_eq!(String::from_utf8_lossy(&tty.stdout), "yes\n", "{tty:?}");
    // The service ends idle while its exec runs on.
    eventually(Duration::from_secs(40), "the service's idle exit", || {
        service.0.try_wait().unwrap().is_some()
    });
    assert!(
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|line| line.starts_with(b"sleep")),
        "its command ran on"
    );
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// F117: an exec's detach keys (Docker's ctrl-p,ctrl-q, or its own
/// `DetachKeys`) end its session, typed into its terminal: what came before
/// them reaches it, its stream ends, and the exec runs on, holding the
/// service up until it ends (its terminal is the service's: the service's
/// exit would hang it up, 11.6a3's Murphy review).
#[test]
fn an_exec_s_detach_keys_end_its_session_only() {
    use std::io::{Read as _, Write as _};
    let Some(stores) = Stores::new() else { return };
    assert!(
        stores
            .shipyard(&[
                "run",
                "-d",
                "--network",
                "none",
                "--name",
                "box",
                &stores.image,
                "sleep",
                "300",
            ])
            .status
            .success()
    );
    for (keys, typed) in [(None, b"\x10\x11".as_slice()), (Some("ctrl-x,y"), b"\x18y")] {
        let mut service = stores.serve(2);
        let socket = stores.socket();
        let mut body = json!({
            "Cmd": ["sh", "-c", "read line; echo \"$line\" > /tmp/typed; exec sleep 300"],
            "Tty": true, "AttachStdin": true, "AttachStdout": true
        });
        if let Some(keys) = keys {
            body["DetachKeys"] = json!(keys);
        }
        let made = api::post(&socket, "/containers/box/exec", Some(&body)).unwrap();
        assert_eq!(made.status, 201, "{}", made.text());
        let id = made.json()["Id"].as_str().unwrap().to_owned();
        let (status, mut session) = api::upgrade(
            &socket,
            &format!("/exec/{id}/start"),
            &json!({"Detach": false}),
        )
        .unwrap();
        assert_eq!(status, 101);
        session.write_all(b"hello\r").unwrap();
        // Its command, the `sleep` it became once it read the line.
        let mut pid = 0;
        eventually(Duration::from_secs(10), "its command's PID", || {
            let shown = api::get(&socket, &format!("/exec/{id}/json"))
                .unwrap()
                .json();
            pid = shown["Pid"].as_u64().unwrap_or(0);
            pid != 0
                && std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|line| line.starts_with(b"sleep\0"))
        });
        let shown = api::get(&socket, &format!("/exec/{id}/json"))
            .unwrap()
            .json();
        assert_eq!(shown["DetachKeys"], json!(keys.unwrap_or("")));
        let read = stores.shipyard(&["exec", "box", "cat", "/tmp/typed"]);
        assert!(read.stdout.starts_with(b"hello"), "{read:?}");
        // The control: its session holds the service up.
        assert!(
            service.0.try_wait().unwrap().is_none(),
            "held while attached"
        );
        session.write_all(typed).unwrap();
        // Its session ends: the stream's end, not a hang-up of the exec.
        session
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut rest = Vec::new();
        let ended = session.read_to_end(&mut rest);
        assert!(ended.is_ok(), "{keys:?}: {ended:?}");
        // Past the service's idle time (2 s), it's still up, and the
        // command too: the detached exec holds it.
        std::thread::sleep(Duration::from_secs(4));
        assert!(service.0.try_wait().unwrap().is_none(), "{keys:?}: held");
        assert!(
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|line| line.starts_with(b"sleep\0")),
            "{keys:?}: its command ran on"
        );
        // Its command ended, the service goes idle and exits.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        );
        eventually(Duration::from_secs(30), "the service's idle exit", || {
            service.0.try_wait().unwrap().is_some()
        });
        let _ = stores.shipyard(&["exec", "box", "rm", "-f", "/tmp/typed"]);
    }
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// A service that bound its own socket (`shipyard compose`'s, the guest's)
/// ends idle with the socket's name gone and its lock let go first (F149):
/// at its last look at what's queued, a client finds no socket, and a
/// service of its own binds the same place at once (never refused until the
/// old one has gone). The control: it answered while it served.
#[test]
fn an_idle_service_s_socket_goes_before_it_does() {
    let Some(stores) = Stores::new() else { return };
    let socket = stores.socket();
    let pause = stores.scratch.0.join("idle-end");
    let service_at = |pause: Option<&std::path::Path>| {
        let mut command = stores.command(&["system", "service", "--socket"]);
        command
            .arg(&socket)
            .env("SHIPYARD_API_IDLE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(pause) = pause {
            command.env("SHIPYARD_TEST_API_IDLE_PAUSE", pause);
        }
        Running(command.spawn().unwrap())
    };
    let mut old = service_at(Some(&pause));
    api::wait_for(&socket, LISTENING);
    assert_eq!(
        api::get(&socket, "/_ping").unwrap().status,
        200,
        "the control"
    );
    eventually(Duration::from_secs(30), "the idle end", || pause.exists());
    let refused = std::os::unix::net::UnixStream::connect(&socket).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::NotFound, "{refused}");
    assert!(
        old.0.try_wait().unwrap().is_none(),
        "still there, at its pause"
    );
    let mut next = service_at(None);
    api::wait_for(&socket, LISTENING);
    assert_eq!(
        api::get(&socket, "/_ping").unwrap().status,
        200,
        "its successor"
    );
    std::fs::remove_file(&pause).unwrap();
    eventually(Duration::from_secs(30), "the old one's end", || {
        old.0.try_wait().unwrap().is_some()
    });
    eventually(Duration::from_secs(30), "its successor's idle end", || {
        next.0.try_wait().unwrap().is_some()
    });
}

/// An exec's input reaches its command as it comes, without a terminal
/// too: a line answered while the client's input is still open, never held
/// until it ends (`std::io::copy` from the client's socket to the command's
/// pipe was a `splice` that waited: 12.5a's probe).
#[test]
fn an_exec_s_input_reaches_it_as_it_comes() {
    use std::io::{Read as _, Write as _};
    let Some(stores) = Stores::new() else { return };
    assert!(
        stores
            .shipyard(&[
                "run",
                "-d",
                "--network",
                "none",
                "--name",
                "box",
                &stores.image,
                "sleep",
                "300",
            ])
            .status
            .success()
    );
    let _service = stores.serve(60);
    let socket = stores.socket();
    let body = json!({
        "Cmd": ["sh", "-c", "read line; echo \"got $line\"; sleep 30"],
        "AttachStdin": true, "AttachStdout": true
    });
    let made = api::post(&socket, "/containers/box/exec", Some(&body)).unwrap();
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_owned();
    let (status, mut session) = api::upgrade(
        &socket,
        &format!("/exec/{id}/start"),
        &json!({"Detach": false}),
    )
    .unwrap();
    assert_eq!(status, 101);
    session.write_all(b"hello\n").unwrap();
    session
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut out = Vec::new();
    let mut buffer = [0_u8; 256];
    while !String::from_utf8_lossy(&out).contains("got hello") {
        let read = session.read(&mut buffer);
        assert!(
            matches!(read, Ok(1..)),
            "{read:?}, the input still open: {:?}",
            String::from_utf8_lossy(&out)
        );
        out.extend_from_slice(&buffer[..read.unwrap()]);
    }
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// A finished exec's session ends both ways (F148): the client reads its
/// stream's end, and what it writes after is refused (the service's end
/// shut for reading too: its input thread, holding a copy of the client,
/// gone), never taken by a thread that waits for the client to close. The
/// control: its input reached the command while it ran.
#[test]
fn a_finished_exec_s_session_ends_both_ways() {
    use std::io::{Read as _, Write as _};
    let Some(stores) = Stores::new() else { return };
    assert!(
        stores
            .shipyard(&[
                "run",
                "-d",
                "--network",
                "none",
                "--name",
                "box",
                &stores.image,
                "sleep",
                "300",
            ])
            .status
            .success()
    );
    let _service = stores.serve(60);
    let socket = stores.socket();
    let body = json!({
        "Cmd": ["sh", "-c", "read line; echo \"got $line\""],
        "Tty": true, "AttachStdin": true, "AttachStdout": true
    });
    let made = api::post(&socket, "/containers/box/exec", Some(&body)).unwrap();
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_owned();
    let (status, mut session) = api::upgrade(
        &socket,
        &format!("/exec/{id}/start"),
        &json!({"Detach": false}),
    )
    .unwrap();
    assert_eq!(status, 101);
    session.write_all(b"hello\r").unwrap();
    session
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut out = Vec::new();
    let read = session.read_to_end(&mut out);
    assert!(
        read.is_ok(),
        "{read:?}: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        String::from_utf8_lossy(&out).contains("got hello"),
        "the control: {out:?}"
    );
    eventually(
        Duration::from_secs(10),
        "a write after the end refused",
        || session.write_all(b"more\n").is_err(),
    );
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// An exec's command gets its container's descriptor limit (the runtime's
/// snapshot of it), never the one the service raised for itself (F151's
/// premise, checked: the container here was started with this test's own
/// soft limit). The control: the service's own soft limit is its hard one.
#[test]
fn an_exec_gets_its_container_s_descriptor_limit() {
    use std::io::Read as _;
    let Some(stores) = Stores::new() else { return };
    assert!(
        stores
            .shipyard(&[
                "run",
                "-d",
                "--network",
                "none",
                "--name",
                "box",
                &stores.image,
                "sleep",
                "300",
            ])
            .status
            .success()
    );
    let (_, hard) =
        nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE).unwrap();
    if hard <= 1024 {
        eprintln!("skipping: the hard descriptor limit is 1024 here");
        return;
    }
    let service = stores.serve_limited(60, (1024, hard));
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", service.0.id())).unwrap();
    let open_files = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .unwrap();
    assert_eq!(
        open_files.split_whitespace().nth(3),
        Some(hard.to_string().as_str()),
        "the control: the service raised its own: {open_files}"
    );
    let socket = stores.socket();
    let body = json!({"Cmd": ["sh", "-c", "ulimit -n"], "AttachStdout": true});
    let made = api::post(&socket, "/containers/box/exec", Some(&body)).unwrap();
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_owned();
    let (status, mut session) = api::upgrade(
        &socket,
        &format!("/exec/{id}/start"),
        &json!({"Detach": false}),
    )
    .unwrap();
    assert_eq!(status, 101);
    let mut out = Vec::new();
    session.read_to_end(&mut out).unwrap();
    let (soft, _) =
        nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE).unwrap();
    let said = String::from_utf8_lossy(&out);
    assert!(
        said.contains(&format!("{soft}\n")),
        "the container's: {said:?}"
    );
    assert!(
        !said.contains(&format!("{hard}\n")),
        "not the service's: {said:?}"
    );
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// `docker exec -e KEY=VALUE`: the value reaches the command, and no
/// process but Docker's own CLI has it on its argv (the service's child
/// names the key only).
#[test]
fn an_exec_s_values_are_on_no_argv() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&["run", "-d", "--name", "box", &stores.image, "sleep", "300"]);
    let (_held, lines) = stores.docker_lines(&[
        "exec",
        "-e",
        "TOKEN=s3cret",
        "box",
        "sh",
        "-c",
        "echo $TOKEN; sleep 30",
    ]);
    assert_eq!(
        lines.recv_timeout(Duration::from_secs(20)).unwrap(),
        "s3cret"
    );
    let cmdlines: Vec<String> = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| std::fs::read(entry.ok()?.path().join("cmdline")).ok())
        .map(|cmdline| String::from_utf8_lossy(&cmdline).replace('\0', " "))
        .collect();
    // The control: the service's child is there to be read, by its key.
    assert!(
        cmdlines
            .iter()
            .any(|cmdline| cmdline.contains(" exec --env=TOKEN --pid-file=")),
        "the exec's child: {cmdlines:?}"
    );
    let leaked: Vec<&String> = cmdlines
        .iter()
        .filter(|cmdline| cmdline.contains("s3cret") && !cmdline.starts_with("docker "))
        .collect();
    assert!(leaked.is_empty(), "on an argv: {leaked:?}");

    stores.docker_ok(&["rm", "-f", "box"]);
}

/// Images through Docker's CLI: a pull (its progress, up to date the
/// second time), the list, inspect (the ID a container runs), a tag,
/// removal by tag and by ID (refused while in use, or while the ID names
/// two references without `-f`), and a removed image gone.
#[test]
fn images_are_docker_s() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve_here(60);
    let image = &stores.image;
    let pulled = stores.docker_ok(&["pull", image]);
    assert!(pulled.contains("Pulling from library/testimg"), "{pulled}");
    assert!(pulled.contains("Digest: sha256:"), "{pulled}");
    // `Stores::new` ran it, so the store had it: up to date.
    assert!(
        pulled.contains("Status: Image is up to date for"),
        "{pulled}"
    );
    // Its lines as they come, one a layer.
    let raw = api::post(
        &stores.socket(),
        &format!("/images/create?fromImage={image}&tag=latest"),
        None,
    )
    .unwrap();
    assert_eq!(raw.status, 200);
    let lines: Vec<serde_json::Value> = raw
        .text()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        lines.iter().any(|line| line["status"] == "Already exists"
            && line["id"].as_str().is_some_and(|id| id.len() == 12)),
        "{lines:?}"
    );
    let id = stores.docker_ok(&["image", "inspect", "-f", "{{.Id}}", image]);
    assert!(id.starts_with("sha256:") && id.len() == 71, "{id}");
    let listed = stores.docker_ok(&[
        "images",
        "--no-trunc",
        "--format",
        "{{.Repository}}:{{.Tag}} {{.ID}}",
    ]);
    assert!(listed.contains(&format!("{image}:latest {id}")), "{listed}");

    stores.docker_ok(&["create", "--name", "user", image]);
    assert_eq!(
        stores.docker_ok(&["inspect", "-f", "{{.Image}}", "user"]),
        id
    );
    stores.docker_ok(&["tag", image, "tagged:v1"]);
    stores.docker_ok(&["tag", image, &format!("{image}:v2")]);
    assert_eq!(
        stores.docker_ok(&["image", "inspect", "-f", "{{.Id}}", "tagged:v1"]),
        id
    );
    // By its repository's digest, as `RepoDigests` names it.
    let digested = stores.docker_ok(&["image", "inspect", "-f", "{{index .RepoDigests 0}}", image]);
    assert_eq!(
        stores.docker_ok(&["image", "inspect", "-f", "{{.Id}}", &digested]),
        id
    );
    let short = &id["sha256:".len().."sha256:".len() + 12];
    // Its ID names references in two repositories: refused without -f.
    let refused = stores.docker(&["rmi", short]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is referenced in several repositories"),
        "{refused:?}"
    );
    let untagged = stores.docker_ok(&["rmi", "tagged:v1"]);
    assert!(untagged.contains("Untagged: tagged:v1"), "{untagged}");
    // One repository's tags now: refused only because a container uses it.
    let in_use = stores.docker(&["rmi", short]);
    assert!(!in_use.status.success());
    assert!(
        String::from_utf8_lossy(&in_use.stderr).contains("is used by container"),
        "{in_use:?}"
    );
    let raw = api::request(
        &stores.socket(),
        "DELETE",
        &format!("/images/{short}"),
        None,
    )
    .unwrap();
    assert_eq!(raw.status, 409, "{}", raw.text());
    stores.docker_ok(&["rm", "user"]);
    // Shipyard's own `rmi`, by ID: every reference, its blobs.
    let removed = stores.shipyard(&["rmi", short]);
    assert!(removed.status.success(), "{removed:?}");
    let removed = String::from_utf8_lossy(&removed.stdout);
    for tag in ["latest", "v2"] {
        assert!(
            removed.contains(&format!("Untagged: {image}:{tag}")),
            "{removed}"
        );
    }
    assert!(removed.contains(&format!("Deleted: {id}")), "{removed}");
    let gone = stores.docker(&["image", "inspect", image]);
    assert!(!gone.status.success());
    assert!(
        String::from_utf8_lossy(&gone.stderr).contains("No such image"),
        "{gone:?}"
    );
}

/// A private registry: a pull without credentials refused; `docker login`
/// refused with a wrong password, then signed in (F118); with the client's
/// own config, as that login wrote it, it pulls, and the password is in no
/// file of the service's stores.
#[test]
fn a_pull_takes_the_client_s_credentials() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve_here(60);
    // `tester:s3cret`, base64.
    let auth = "dGVzdGVyOnMzY3JldA==";
    let (_private, image) = fixture_requiring(&format!("Basic {auth}"));
    let refused = stores.docker(&["pull", &image]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("the registry asks for a username and password"),
        "{refused:?}"
    );

    // `docker login` (`POST /auth`, F118): the password checked against
    // the registry, refused when wrong; the client keeps the right one in
    // its config, as Docker's CLI does.
    let registry = image.split('/').next().unwrap();
    let login = |password: &str| {
        use std::io::Write as _;
        let mut login = stores
            .docker_command(&["login", "-u", "tester", "--password-stdin", registry])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        login
            .stdin
            .take()
            .unwrap()
            .write_all(password.as_bytes())
            .unwrap();
        login.wait_with_output().unwrap()
    };
    let wrong = login("wrong");
    assert!(!wrong.status.success(), "{wrong:?}");
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("unauthorized:"),
        "{wrong:?}"
    );
    let right = login("s3cret");
    assert!(right.status.success(), "{right:?}");
    assert!(
        String::from_utf8_lossy(&right.stdout).contains("Login Succeeded"),
        "{right:?}"
    );
    // (Docker's CLI shows no layer lines when its output isn't a terminal.)
    let pulled = stores.docker_ok(&["pull", &image]);
    assert!(
        pulled.contains("Status: Downloaded newer image for"),
        "{pulled}"
    );

    // Every file of the stores and the service's log that holds the
    // password; the client's own config must be the only one (the control:
    // the walk does find it there).
    let mut leaked = Vec::new();
    let mut dirs = vec![stores.scratch.0.clone()];
    assert!(stores.scratch.0.join("service.log").is_file());
    while let Some(dir) = dirs.pop() {
        // A directory rootless runs leave to subordinate IDs can't be read:
        // the control below shows the walk reads what it should.
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(path),
                Ok(kind) if kind.is_file() => {
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    let text = String::from_utf8_lossy(&bytes);
                    if text.contains("s3cret") || text.contains(auth) {
                        leaked.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        leaked,
        [stores.scratch.0.join("docker").join("config.json")],
        "the password's only place is the client's config"
    );
}

/// The image endpoints' refusals, through the raw API: an import from a
/// URL or with no name (step 12.2b), another platform, a pull without `fromImage`, a bad `X-Registry-Auth`, a filter
/// not read, an unknown image (404, Docker's words).
#[test]
fn image_refusals_are_docker_s() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    for path in [
        "/images/create?fromSrc=-",
        "/images/create?fromSrc=https%3A%2F%2Fexample.com%2Fx.tar&repo=a",
        "/images/create?fromImage=alpine&platform=windows/amd64",
        "/images/create?tag=1",
    ] {
        let refused = api::post(&socket, path, None).unwrap();
        assert_eq!(refused.status, 400, "{path}: {}", refused.text());
    }
    let bad = api::request_with(
        &socket,
        "POST",
        "/images/create?fromImage=alpine",
        &[("X-Registry-Auth", "not base64!")],
    )
    .unwrap();
    assert_eq!(bad.status, 400, "{}", bad.text());
    let filtered = api::get(
        &socket,
        "/images/json?filters=%7B%22before%22%3A%5B%22x%22%5D%7D",
    )
    .unwrap();
    assert_eq!(filtered.status, 400, "{}", filtered.text());
    let missing = api::get(&socket, "/images/nothing:here/json").unwrap();
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["message"], "No such image: nothing:here");
    let listed = api::get(&socket, "/images/json").unwrap();
    assert_eq!(listed.status, 200);
    assert_eq!(
        listed.json().as_array().unwrap().len(),
        1,
        "{}",
        listed.text()
    );
}

/// Volumes through Docker's CLI and the raw API: made with labels (again:
/// the same), listed by label and dangling, inspected as Docker's; one
/// made without a name; a run's use of a labelled one; refused removal
/// while a container mounts it; Docker's 404 after.
#[test]
fn volumes_are_docker_s() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    assert_eq!(
        stores.docker_ok(&["volume", "create", "--label", "tier=db", "data"]),
        "data"
    );
    // Again: the same volume, as Docker's `local` driver answers.
    assert_eq!(stores.docker_ok(&["volume", "create", "data"]), "data");
    assert_eq!(
        stores.docker_ok(&[
            "volume",
            "ls",
            "--filter",
            "label=tier=db",
            "--format",
            "{{.Name}}"
        ]),
        "data"
    );
    assert_eq!(
        stores.docker_ok(&[
            "volume",
            "inspect",
            "-f",
            "{{.Labels.tier}} {{.Driver}} {{.Scope}}",
            "data"
        ]),
        "db local local"
    );
    let mountpoint = stores.docker_ok(&["volume", "inspect", "-f", "{{.Mountpoint}}", "data"]);
    assert!(mountpoint.ends_with("/volumes/data/_data"), "{mountpoint}");
    assert!(
        !stores
            .docker_ok(&["volume", "inspect", "-f", "{{.CreatedAt}}", "data"])
            .is_empty()
    );
    // Without a name: a new one, anonymous as Docker marks it.
    let anonymous = stores.docker_ok(&["volume", "create"]);
    assert_eq!(anonymous.len(), 64, "{anonymous}");
    assert!(anonymous.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(
        stores.docker_ok(&["volume", "inspect", "-f", "{{json .Labels}}", &anonymous]),
        r#"{"com.docker.volume.anonymous":""}"#
    );

    // A run uses the labelled volume as any other.
    let ran = stores.shipyard(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--volume",
        "data:/d",
        &stores.image,
        "sh",
        "-c",
        "echo kept > /d/file",
    ]);
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(
        std::fs::read_to_string(format!("{mountpoint}/file")).unwrap(),
        "kept\n"
    );

    stores.docker_ok(&[
        "create",
        "--network",
        "none",
        "--name",
        "user",
        "-v",
        "data:/d",
        &stores.image,
    ]);
    let in_use = stores.docker(&["volume", "rm", "data"]);
    assert!(!in_use.status.success());
    assert!(
        String::from_utf8_lossy(&in_use.stderr).contains("is in use by"),
        "{in_use:?}"
    );
    let raw = api::request(&socket, "DELETE", "/volumes/data", None).unwrap();
    assert_eq!(raw.status, 409, "{}", raw.text());
    let dangling = stores.docker_ok(&["volume", "ls", "-q", "--filter", "dangling=true"]);
    assert_eq!(dangling, anonymous);
    stores.docker_ok(&["rm", "user"]);
    stores.docker_ok(&["volume", "rm", "data"]);
    let gone = api::get(&socket, "/volumes/data").unwrap();
    assert_eq!(gone.status, 404);
    assert_eq!(gone.json()["message"], "get data: no such volume");
    assert_eq!(
        api::request(&socket, "DELETE", "/volumes/data?force=1", None)
            .unwrap()
            .status,
        204
    );
}

/// A volume prune takes only anonymous volumes unless asked for all, and
/// never one a container mounts; it says the bytes it frees. What Shipyard
/// can't make is refused.
#[test]
fn a_volume_prune_takes_what_it_s_asked_to() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let create = |body: serde_json::Value| {
        let made = api::post(&socket, "/volumes/create", Some(&body)).unwrap();
        assert_eq!(made.status, 201, "{}", made.text());
        made.json()
    };
    let prune = |path: &str| {
        let pruned = api::post(&socket, path, None).unwrap();
        assert_eq!(pruned.status, 200, "{}", pruned.text());
        pruned.json()
    };
    create(json!({"Name": "kept"}));
    let anonymous = create(json!({}));
    create(json!({"Name": "spare", "Labels": {"tier": "cache"}}));
    let other = create(json!({"Name": "other"}));
    let user = stores.shipyard(&[
        "create",
        "--network",
        "none",
        "--name",
        "user",
        "--volume",
        "kept:/d",
        &stores.image,
    ]);
    assert!(user.status.success(), "{user:?}");
    std::fs::write(
        format!("{}/blob", other["Mountpoint"].as_str().unwrap()),
        vec![7_u8; 5000],
    )
    .unwrap();

    // Without `all`: only the anonymous one.
    assert_eq!(
        prune("/volumes/prune")["VolumesDeleted"],
        json!([anonymous["Name"]])
    );
    // By a label, with `all`.
    let by_label = prune(
        "/volumes/prune?filters=%7B%22all%22%3A%5B%22true%22%5D%2C%22label%22%3A%5B%22tier%3Dcache%22%5D%7D",
    );
    assert_eq!(by_label["VolumesDeleted"], json!(["spare"]));
    // A client of API 1.41 asks for every unused volume, as Docker reads it.
    let old = prune("/v1.41/volumes/prune");
    assert_eq!(old["VolumesDeleted"], json!(["other"]), "{old}");
    assert!(old["SpaceReclaimed"].as_u64().unwrap() >= 5000, "{old}");
    assert_eq!(
        api::get(&socket, "/volumes/kept").unwrap().status,
        200,
        "mounted: kept"
    );

    for refused in [
        json!({"Name": "x", "Driver": "nfs"}),
        json!({"Name": "x", "DriverOpts": {"type": "nfs"}}),
        json!({"Name": "../x"}),
    ] {
        let answer = api::post(&socket, "/volumes/create", Some(&refused)).unwrap();
        assert_eq!(answer.status, 400, "{refused}: {}", answer.text());
    }
    let filtered = api::get(
        &socket,
        "/volumes?filters=%7B%22scope%22%3A%5B%22x%22%5D%7D",
    )
    .unwrap();
    assert_eq!(filtered.status, 400);
}

/// Networks through Docker's CLI (root's: bridges are the host's): made
/// with labels and a subnet, listed with the built-ins, inspected as
/// Docker's, by the start of its ID; a name taken and an option Shipyard
/// can't honour refused; a built-in kept.
#[test]
#[ignore = "needs root"]
fn networks_are_docker_s() {
    if !reference_tool("docker") || !is_root() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    // First, before anything makes the default network: its name is taken
    // all the same (made otherwise, it would change every default run).
    let default = api::post(
        &stores.socket(),
        "/networks/create",
        Some(&json!({"Name": "bridge", "Internal": true})),
    )
    .unwrap();
    assert_eq!(default.status, 409, "{}", default.text());
    assert_eq!(
        default.json()["message"],
        "network with name bridge already exists"
    );
    let id = stores.docker_ok(&[
        "network",
        "create",
        "--label",
        "tier=web",
        "--subnet",
        "10.250.1.0/24",
        "net",
    ]);
    assert_eq!(id.len(), 64, "{id}");
    let names = stores.docker_ok(&["network", "ls", "--format", "{{.Name}}"]);
    for name in ["bridge", "host", "none", "net"] {
        assert!(names.lines().any(|line| line == name), "{name} in {names}");
    }
    assert_eq!(
        stores.docker_ok(&["network", "ls", "-q", "--filter", "label=tier=web"]),
        &id[..12]
    );
    assert_eq!(
        stores.docker_ok(&[
            "network",
            "inspect",
            "-f",
            "{{.Id}} {{(index .IPAM.Config 0).Subnet}} {{.Labels.tier}} {{.Driver}}",
            &id[..8],
        ]),
        format!("{id} 10.250.1.0/24 web bridge")
    );
    let taken = stores.docker(&["network", "create", "net"]);
    assert!(
        String::from_utf8_lossy(&taken.stderr).contains("network with name net already exists"),
        "{taken:?}"
    );
    let option = stores.docker(&[
        "network",
        "create",
        "-o",
        "com.docker.network.bridge.name=br9",
        "odd",
    ]);
    assert!(
        String::from_utf8_lossy(&option.stderr).contains("isn't one Shipyard's networks honour"),
        "{option:?}"
    );
    let builtin = stores.docker(&["network", "rm", "host"]);
    assert!(
        String::from_utf8_lossy(&builtin.stderr).contains("pre-defined network"),
        "{builtin:?}"
    );
    let socket = stores.socket();
    for predefined in ["host", "bridge"] {
        let path = format!("/networks/{predefined}");
        let refused = api::request(&socket, "DELETE", &path, None).unwrap();
        assert_eq!(refused.status, 403, "{predefined}: {}", refused.text());
    }
    // A network whose labels this Shipyard can't read: listed without
    // them (the others with theirs), refused on its own.
    std::fs::write(
        stores
            .scratch
            .0
            .join("networks")
            .join("net")
            .join("labels.json"),
        r#"{"version":2}"#,
    )
    .unwrap();
    stores.docker_ok(&["network", "create", "--label", "a=b", "fine"]);
    let listed = stores.docker_ok(&["network", "ls", "--format", "{{.Name}} {{.Labels}}"]);
    assert!(listed.lines().any(|line| line == "net "), "{listed}");
    assert!(listed.lines().any(|line| line == "fine a=b"), "{listed}");
    assert_eq!(api::get(&socket, "/networks/net").unwrap().status, 500);
}

/// F120: `dangling` networks, Docker's rule: no container up on it, and
/// never a built-in; `false` the rest. A record that can't be read may
/// name one: then none is dangling, nor removed.
#[test]
#[ignore = "needs root"]
fn dangling_networks_are_docker_s() {
    if !reference_tool("docker") || !is_root() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&["network", "create", "--subnet", "10.250.7.0/24", "net"]);
    stores.docker_ok(&[
        "run",
        "-d",
        "--network",
        "net",
        "--name",
        "c",
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.docker_ok(&["network", "create", "--subnet", "10.250.8.0/24", "spare"]);
    let dangling = |value: &str| {
        let filter = format!("dangling={value}");
        let listed = stores.docker_ok(&["network", "ls", "-f", &filter, "--format", "{{.Name}}"]);
        let mut names: Vec<String> = listed.lines().map(str::to_owned).collect();
        names.sort();
        names
    };
    assert_eq!(dangling("true"), ["spare"]);
    let kept = dangling("false");
    for name in ["bridge", "host", "net"] {
        assert!(kept.iter().any(|kept| kept == name), "{name} in {kept:?}");
    }
    assert!(!kept.iter().any(|kept| kept == "spare"), "{kept:?}");
    // A record that can't be read may name it: never dangling, never
    // removed, until it can be (11.6a2's Murphy review); the control after.
    let broken = stores.scratch.0.join("containers").join("f".repeat(64));
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("record.json"), "not a record").unwrap();
    let unsure = stores.docker(&["network", "ls", "-f", "dangling=true", "-q"]);
    assert!(!unsure.status.success(), "{unsure:?}");
    let kept_back = stores.docker(&["network", "rm", "spare"]);
    assert!(!kept_back.status.success(), "{kept_back:?}");
    std::fs::remove_dir_all(&broken).unwrap();
    stores.docker_ok(&["network", "rm", "spare"]);
    stores.docker_ok(&["rm", "-f", "c"]);
    stores.docker_ok(&["network", "rm", "net"]);
}

/// A container on a network Docker's API made shows its ID; one connected
/// with an alias and an address is on it at that address, then gone after
/// a disconnect; a network in use is kept, then removed; a prune takes the
/// unused, never the default.
#[test]
#[ignore = "needs root"]
fn containers_join_and_leave_networks() {
    if !reference_tool("docker") || !is_root() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let id = stores.docker_ok(&["network", "create", "--subnet", "10.250.1.0/24", "net"]);
    stores.docker_ok(&[
        "run",
        "-d",
        "--network",
        "net",
        "--name",
        "c",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(
        stores.docker_ok(&[
            "inspect",
            "-f",
            "{{.NetworkSettings.Networks.net.NetworkID}}",
            "c"
        ]),
        id
    );
    stores.docker_ok(&["network", "create", "--subnet", "10.250.2.0/24", "other"]);
    stores.docker_ok(&[
        "network",
        "connect",
        "--alias",
        "db",
        "--ip",
        "10.250.2.9",
        "other",
        "c",
    ]);
    let aliases = stores.docker_ok(&[
        "inspect",
        "-f",
        "{{.NetworkSettings.Networks.other.Aliases}}",
        "c",
    ]);
    assert!(aliases.contains("db"), "{aliases}");
    // The list shows both, from every network's leases read once (11.7):
    // `ps`'s networks, and each one's address as `inspect` has it.
    let networks = stores.docker_ok(&["ps", "--filter", "name=c", "--format", "{{.Networks}}"]);
    assert!(
        networks.contains("net") && networks.contains("other"),
        "{networks}"
    );
    let listed = api::get(&stores.socket(), "/containers/json").unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&listed.body).unwrap();
    assert_eq!(
        listed[0]["NetworkSettings"]["Networks"]["other"]["IPAddress"], "10.250.2.9",
        "{listed}"
    );
    assert_eq!(
        stores.docker_ok(&[
            "network",
            "inspect",
            "-f",
            "{{range .Containers}}{{.Name}} {{.IPv4Address}}{{end}}",
            "other",
        ]),
        "c 10.250.2.9/24"
    );
    stores.docker_ok(&["network", "disconnect", "other", "c"]);
    assert_eq!(
        stores.docker_ok(&["network", "inspect", "-f", "{{len .Containers}}", "other"]),
        "0"
    );
    let in_use = api::request(&stores.socket(), "DELETE", "/networks/net", None).unwrap();
    assert_eq!(in_use.status, 409, "{}", in_use.text());
    stores.docker_ok(&["rm", "-f", "c"]);
    stores.docker_ok(&["network", "rm", "net"]);
    let pruned = stores.docker_ok(&["network", "prune", "-f"]);
    assert!(pruned.contains("other"), "{pruned}");
    let names = stores.docker_ok(&["network", "ls", "--format", "{{.Name}}"]);
    assert!(
        !names.lines().any(|line| line == "other" || line == "net"),
        "{names}"
    );
    assert!(
        names.lines().any(|line| line == "bridge"),
        "the default kept: {names}"
    );
}

/// Rootless, there are no bridge networks: the built-ins are listed (and
/// `pasta`, the rootless default), and making one is refused as needing
/// root (403).
#[test]
fn rootless_networks_are_the_built_ins() {
    if is_root() {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let listed = api::get(&socket, "/networks").unwrap();
    assert_eq!(listed.status, 200, "{}", listed.text());
    let names: Vec<String> = listed
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|network| network["Name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["host", "none", "pasta"]);
    let made = api::post(&socket, "/networks/create", Some(&json!({"Name": "net"}))).unwrap();
    assert_eq!(made.status, 403, "{}", made.text());
    assert!(
        made.json()["message"]
            .as_str()
            .unwrap()
            .contains("bridge networks need root")
    );
    let none = api::get(&socket, "/networks/none").unwrap();
    assert_eq!(none.json()["Driver"], "null");
    for (method, path, body) in [
        ("DELETE", "/networks/bridge", None),
        (
            "POST",
            "/networks/net/connect",
            Some(json!({"Container": "c"})),
        ),
    ] {
        let body = body.map(|body| serde_json::to_vec(&body).unwrap());
        let refused = api::request(&socket, method, path, body.as_deref()).unwrap();
        assert_eq!(refused.status, 403, "{method} {path}: {}", refused.text());
    }
    // None to prune (Docker's `system prune` asks): nothing deleted.
    let pruned = api::post(&socket, "/networks/prune", None).unwrap();
    assert_eq!(pruned.status, 200, "{}", pruned.text());
    assert_eq!(pruned.json(), json!({"NetworksDeleted": null}));
}

/// `docker cp` through the API, both ways: a file and a tree out, a file
/// in; a symlink copied as one; from a stopped container; a missing path
/// refused (Docker's 404).
#[test]
fn cp_is_docker_s() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let made = stores.shipyard(&[
        "run",
        "-d",
        "--network",
        "none",
        "--name",
        "box",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert!(made.status.success(), "{made:?}");
    let host = stores.scratch.0.join("host");
    std::fs::create_dir(&host).unwrap();
    std::fs::write(host.join("in.txt"), "into the box\n").unwrap();
    stores.docker_ok(&[
        "cp",
        &host.join("in.txt").display().to_string(),
        "box:/tmp/",
    ]);
    assert_eq!(
        stores.docker_ok(&["exec", "box", "cat", "/tmp/in.txt"]),
        "into the box"
    );

    stores.docker_ok(&[
        "exec",
        "box",
        "sh",
        "-c",
        "mkdir -p /srv/d/sub && echo deep > /srv/d/sub/f",
    ]);
    // A symlink, copied in as one (the image has no `ln`), then out.
    std::os::unix::fs::symlink("/tmp/in.txt", host.join("made-link")).unwrap();
    stores.docker_ok(&[
        "cp",
        &host.join("made-link").display().to_string(),
        "box:/srv/link",
    ]);
    let tree = host.join("tree");
    stores.docker_ok(&["cp", "box:/srv/d", &tree.display().to_string()]);
    assert_eq!(
        std::fs::read_to_string(tree.join("sub/f")).unwrap(),
        "deep\n"
    );
    let link = host.join("link");
    stores.docker_ok(&["cp", "box:/srv/link", &link.display().to_string()]);
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::path::Path::new("/tmp/in.txt")
    );
    // Its stat: a symlink (Go's bit), what it leads to.
    let head = api::request(
        &stores.socket(),
        "HEAD",
        "/containers/box/archive?path=/srv/link",
        None,
    )
    .unwrap();
    assert!(head.body.is_empty());
    let stat = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(head.header("x-docker-container-path-stat").unwrap())
            .unwrap()
    };
    let stat: serde_json::Value = serde_json::from_slice(&stat).unwrap();
    assert_eq!(stat["linkTarget"], "/tmp/in.txt", "{stat}");
    assert_eq!(
        stat["mode"].as_u64().unwrap() & (1 << 27),
        1 << 27,
        "{stat}"
    );

    let missing = stores.docker(&["cp", "box:/nothing", &host.display().to_string()]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("Could not find the file /nothing"),
        "{missing:?}"
    );

    stores.docker_ok(&["stop", "-t", "1", "box"]);
    let out = host.join("out.txt");
    stores.docker_ok(&["cp", "box:/tmp/in.txt", &out.display().to_string()]);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "into the box\n");
    stores.docker_ok(&["rm", "box"]);
}

/// The archive endpoints through the raw API: HEAD's stat header (a
/// directory, Go's mode); a gzip tar put; xz refused; a put into a file
/// refused; a missing path's 404; a read-only root's 403.
#[test]
fn archives_are_docker_s() {
    use base64::Engine as _;
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    for (name, flag) in [("box", None), ("ro", Some("--read-only"))] {
        let mut args = vec!["run", "-d", "--network", "none", "--name", name];
        args.extend(flag);
        args.extend([stores.image.as_str(), "sleep", "300"]);
        let made = stores.shipyard(&args);
        assert!(made.status.success(), "{made:?}");
    }
    let head = api::request(&socket, "HEAD", "/containers/box/archive?path=/tmp", None).unwrap();
    assert_eq!(head.status, 200);
    let stat = base64::engine::general_purpose::STANDARD
        .decode(head.header("x-docker-container-path-stat").unwrap())
        .unwrap();
    let stat: serde_json::Value = serde_json::from_slice(&stat).unwrap();
    assert_eq!(stat["name"], "tmp");
    assert_eq!(
        stat["mode"].as_u64().unwrap() & (1 << 31),
        1 << 31,
        "{stat}"
    );

    let dir = stores.scratch.0.join("archive");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("zipped.txt"), "through gzip\n").unwrap();
    let gzipped = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "tar -cf - -C {} zipped.txt | gzip -c",
            dir.display()
        ))
        .output()
        .unwrap();
    assert!(gzipped.status.success(), "{gzipped:?}");
    let put = |path: &str, body: &[u8]| {
        api::request(&socket, "PUT", &format!("/containers/{path}"), Some(body)).unwrap()
    };
    let answer = put("box/archive?path=/tmp", &gzipped.stdout);
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert!(
        stores
            .shipyard(&[
                "exec",
                "box",
                "sh",
                "-c",
                "test \"$(cat /tmp/zipped.txt)\" = 'through gzip'"
            ])
            .status
            .success()
    );
    let xz = put("box/archive?path=/tmp", b"\xfd7zXZ\x00rest");
    assert_eq!(xz.status, 400, "{}", xz.text());
    let into_file = put("box/archive?path=/tmp/zipped.txt", &gzipped.stdout);
    assert_eq!(into_file.status, 400, "{}", into_file.text());
    let read_only = put("ro/archive?path=/", &gzipped.stdout);
    assert_eq!(read_only.status, 403, "{}", read_only.text());
    let missing = api::get(&socket, "/containers/box/archive?path=/nothing").unwrap();
    assert_eq!(missing.status, 404);
    assert_eq!(
        missing.json()["message"],
        "Could not find the file /nothing in container box"
    );
    // No container: its own 404, never the file's.
    let ghost = api::get(&socket, "/containers/ghost/archive?path=/").unwrap();
    assert_eq!(ghost.status, 404);
    assert!(
        !ghost.json()["message"]
            .as_str()
            .unwrap()
            .contains("Could not find the file"),
        "{}",
        ghost.text()
    );
    // `copyUIDGID`: the archive's owner kept (where the container has the
    // ID: root's, here); without it, the container's root's.
    if is_root() {
        let owned = |name: &str| {
            let mut archive = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_uid(4321);
            header.set_gid(4321);
            header.set_cksum();
            archive.append_data(&mut header, name, &b"ok"[..]).unwrap();
            archive.into_inner().unwrap()
        };
        let kept = put("box/archive?path=/tmp&copyUIDGID=1", &owned("kept"));
        assert_eq!(kept.status, 200, "{}", kept.text());
        let rooted = put("box/archive?path=/tmp", &owned("rooted"));
        assert_eq!(rooted.status, 200, "{}", rooted.text());
        let owner = |name: &str| {
            let said =
                stores.shipyard(&["exec", "box", "stat", "-c", "%u", &format!("/tmp/{name}")]);
            String::from_utf8_lossy(&said.stdout).trim().to_owned()
        };
        assert_eq!(owner("kept"), "4321");
        assert_eq!(owner("rooted"), "0");
    }
    assert!(stores.shipyard(&["rm", "-f", "box", "ro"]).status.success());
}

/// F118: a tag's time is the image's `LastTagTime`.
#[test]
fn a_tag_s_time_is_its_last_tag_time() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&["tag", &stores.image, "tagged:v1"]);
    let tag_time = stores.docker_ok(&[
        "image",
        "inspect",
        "-f",
        "{{.Metadata.LastTagTime}}",
        "tagged:v1",
    ]);
    assert!(!tag_time.starts_with("0001-01-01"), "{tag_time}");
}

/// F121: a link's `linkTarget` is its chain followed in the container
/// (Docker's `FollowSymlinkInScope`): a link to a link, the file at its
/// end; a link in a directory on the way, followed too. A PUT to a
/// container that isn't there is 404, whatever its body.
#[test]
fn a_link_s_target_is_followed_in_the_container() {
    use base64::Engine as _;
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let made = stores.shipyard(&[
        "run",
        "-d",
        "--network",
        "none",
        "--name",
        "box",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert!(made.status.success(), "{made:?}");
    let put = |path: &str, body: &[u8]| {
        api::request(&socket, "PUT", &format!("/containers/{path}"), Some(body)).unwrap()
    };
    // Its container first: a missing one is 404, whatever the body.
    let xz_ghost = put("ghost/archive?path=/tmp", b"\xfd7zXZ\x00rest");
    assert_eq!(xz_ghost.status, 404, "{}", xz_ghost.text());
    // The links, put as an archive's (the image has no `ln`): `first` to
    // `second` to `real`, and `up` to `/tmp` itself.
    let mut archive = tar::Builder::new(Vec::new());
    let mut file = tar::Header::new_gnu();
    file.set_size(3);
    file.set_mode(0o644);
    file.set_cksum();
    archive
        .append_data(&mut file, "real", &b"hi\n"[..])
        .unwrap();
    for (name, target) in [("second", "real"), ("first", "/tmp/second"), ("up", "/tmp")] {
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        archive.append_link(&mut link, name, target).unwrap();
    }
    let linked = put("box/archive?path=/tmp", &archive.into_inner().unwrap());
    assert_eq!(linked.status, 200, "{}", linked.text());
    let link_target = |path: &str| {
        let head = api::request(
            &socket,
            "HEAD",
            &format!("/containers/box/archive?path={path}"),
            None,
        )
        .unwrap();
        assert_eq!(head.status, 200, "{path}");
        let stat = base64::engine::general_purpose::STANDARD
            .decode(head.header("x-docker-container-path-stat").unwrap())
            .unwrap();
        let stat: serde_json::Value = serde_json::from_slice(&stat).unwrap();
        stat["linkTarget"].as_str().unwrap().to_owned()
    };
    assert_eq!(link_target("/tmp/first"), "/tmp/real");
    assert_eq!(link_target("/tmp/up/first"), "/tmp/real");
    assert_eq!(link_target("/tmp/real"), "", "not a link");
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// F121: inspect's `ResolvConfPath`, `HostnamePath` and `HostsPath`
/// name the container's `/etc` files on the host while it runs (the files
/// its `/etc` shows), and are empty once it has ended (they go with it).
#[test]
fn inspect_names_the_etc_files_while_it_runs() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&["run", "-d", "--name", "etc", &stores.image, "sleep", "300"]);
    let paths = || {
        let shown = api::get(&stores.socket(), "/containers/etc/json")
            .unwrap()
            .json();
        ["ResolvConfPath", "HostnamePath", "HostsPath"]
            .map(|key| shown[key].as_str().unwrap().to_owned())
    };
    let [resolv, hostname, hosts] = paths();
    assert!(resolv.ends_with("/resolv.conf"), "{resolv}");
    assert!(hostname.ends_with("/hostname"), "{hostname}");
    assert!(hosts.ends_with("/hosts"), "{hosts}");
    let inside = stores.docker_ok(&["exec", "etc", "cat", "/etc/hostname"]);
    assert_eq!(
        std::fs::read_to_string(&hostname).unwrap().trim(),
        inside.trim()
    );
    stores.docker_ok(&["stop", "-t", "0", "etc"]);
    assert_eq!(paths(), ["", "", ""].map(str::to_owned), "after its end");
    stores.docker_ok(&["rm", "etc"]);
}

/// Shipyard's own views under `/shipyard/…` (a container's: its serial;
/// a network's: root's, refused rootless), and the inspect keys read
/// besides the record: a started container's writable area and its log
/// config.
#[test]
fn shipyard_s_views_and_inspect_s_layers() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    let made = stores.shipyard(&[
        "run",
        "-d",
        "--network",
        "none",
        "--name",
        "box",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert!(made.status.success(), "{made:?}");
    let own = api::get(&socket, "/shipyard/containers/box").unwrap();
    assert_eq!(own.status, 200, "{}", own.text());
    let own = own.json();
    assert_eq!(own["name"], "box");
    assert!(
        own["serial"]
            .as_str()
            .is_some_and(|serial| !serial.is_empty()),
        "{own}"
    );
    assert_eq!(
        api::get(&socket, "/shipyard/containers/ghost")
            .unwrap()
            .status,
        404
    );
    let network = api::get(&socket, "/shipyard/networks/bridge").unwrap();
    if is_root() {
        // Made by the listing, as a run makes it.
        api::get(&socket, "/networks").unwrap();
        let network = api::get(&socket, "/shipyard/networks/bridge").unwrap();
        assert_eq!(network.status, 200, "{}", network.text());
        assert_eq!(network.json()["Name"], "bridge");
        assert_eq!(network.json()["version"], 1);
    } else {
        assert_eq!(network.status, 403, "{}", network.text());
    }

    let inspected = api::get(&socket, "/containers/box/json").unwrap().json();
    let data = &inspected["GraphDriver"]["Data"];
    for key in ["UpperDir", "WorkDir", "MergedDir"] {
        let place = data[key].as_str().unwrap();
        assert!(std::path::Path::new(place).is_dir(), "{key}: {place}");
    }
    let lower = data["LowerDir"].as_str().unwrap();
    assert!(
        lower
            .split(':')
            .all(|layer| std::path::Path::new(layer).is_dir()),
        "{lower}"
    );
    assert_eq!(inspected["HostConfig"]["LogConfig"]["Type"], "json-file");
    assert!(stores.shipyard(&["rm", "-f", "box"]).status.success());
}

/// F62 through the API: Docker's CLI sends `PidsLimit: 0` without
/// `--pids-limit`, which leaves the default; `--pids-limit -1` asks none.
#[test]
fn docker_run_gets_the_default_process_limit() {
    if !reference_tool("docker") {
        return;
    }
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let pids_max = |name: &str| -> Option<String> {
        let pid = stores.docker_ok(&["inspect", "-f", "{{.State.Pid}}", name]);
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/pids.max"))
            .ok()
            .map(|text| text.trim().to_owned())
    };
    for (name, extra) in [("limited", None), ("unlimited", Some("-1"))] {
        let mut args = vec!["run", "-d", "--name", name, "--network", "none"];
        if let Some(limit) = extra {
            args.extend(["--pids-limit", limit]);
        }
        args.extend([stores.image.as_str(), "sleep", "300"]);
        stores.docker_ok(&args);
    }
    // Where the engine's `Tree::offers` finds `pids`: root's tree, or a
    // tester's delegation.
    let base = if is_root() {
        Some(std::path::PathBuf::from("/sys/fs/cgroup"))
    } else {
        shipyard_test_support::delegated_cgroup()
    };
    let offered = base.is_some_and(|base| {
        std::fs::read_to_string(base.join("cgroup.controllers"))
            .is_ok_and(|text| text.split_whitespace().any(|name| name == "pids"))
    });
    if offered {
        assert_eq!(pids_max("limited").as_deref(), Some("2048"));
        assert_eq!(pids_max("unlimited").as_deref(), Some("max"));
    } else {
        let limit = pids_max("limited");
        assert!(matches!(limit.as_deref(), None | Some("max")), "{limit:?}");
    }
    stores.docker_ok(&["rm", "-f", "limited", "unlimited"]);
}

/// F114: the service raises its own descriptor limit (started as systemd
/// starts it, 1024 of 4096 here) for its many creates and connections, and
/// its containers keep the limit it was started with, as the CLI's do.
#[test]
fn the_service_raises_its_own_descriptor_limit_only() {
    let Some(stores) = Stores::new() else { return };
    let service = stores.serve_limited(60, (1024, 4096));
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", service.0.id())).unwrap();
    let open_files = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .unwrap()
        .split_whitespace()
        .nth(3)
        .unwrap()
        .to_owned();
    assert_eq!(open_files, "4096", "{limits}");
    let ran = stores.docker_ok(&["run", "--rm", &stores.image, "sh", "-c", "ulimit -n"]);
    assert_eq!(ran.trim(), "1024");
    // An exec's command too: it gets the container's limits (the monitor's
    // snapshot), not those of the service's `shipyard exec`.
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "limited",
        &stores.image,
        "sleep",
        "60",
    ]);
    let execed = stores.docker_ok(&["exec", "limited", "sh", "-c", "ulimit -n"]);
    assert_eq!(execed.trim(), "1024");
    stores.docker_ok(&["rm", "-f", "limited"]);
}

/// F116: `StdinOnce` is kept with the container, not in the service's
/// memory: made by one service that then ends, started by the next, its
/// input still ends at its first client's end (`cat` ends with its input,
/// as `docker run -i` makes it).
#[test]
fn stdin_once_outlives_the_service_that_made_it() {
    use std::io::Write as _;
    let Some(stores) = Stores::new() else { return };
    let mut first = stores.serve(1);
    let body = json!({
        "Image": stores.image, "Cmd": ["cat"],
        "OpenStdin": true, "StdinOnce": true, "AttachStdin": true
    });
    assert_eq!(
        api::post(
            &stores.socket(),
            "/containers/create?name=once",
            Some(&body)
        )
        .unwrap()
        .status,
        201
    );
    eventually(Duration::from_secs(10), "the first service ends", || {
        first.0.try_wait().unwrap().is_some()
    });
    let _second = stores.serve(60);
    let mut started = stores
        .docker_command(&["start", "-a", "-i", "once"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    started.stdin.take().unwrap().write_all(b"typed\n").unwrap();
    let began = std::time::Instant::now();
    let ended = loop {
        if let Some(status) = started.try_wait().unwrap() {
            break status;
        }
        if began.elapsed() > Duration::from_secs(30) {
            let _ = started.kill();
            panic!("its input never ended: the container runs on");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(ended.success(), "{ended:?}");
    let inspected = api::get(&stores.socket(), "/containers/once/json")
        .unwrap()
        .json();
    assert_eq!(inspected["Config"]["StdinOnce"], true);
    assert_eq!(inspected["State"]["ExitCode"], 0);
}

/// The map's data (step 14.1a): `/shipyard/links` has every container, the
/// links compose declares (its `depends_on` label, as compose writes it),
/// refused once the target stops; and, as root, the connections between
/// containers, counted once from the client's side, the control first (no
/// connection, no observed link), a container sharing another's network
/// namespace said and never counted twice; a published port's connections
/// are its host clients' alone. Rootless, observed links are said
/// unavailable, the rest stands. `/shipyard/containers/{id}/sockets`
/// names a connection's peer.
#[test]
fn the_map_s_links_and_sockets_are_served() {
    let Some(stores) = Stores::new() else { return };
    let service = stores.serve(60);
    let socket = stores.socket();
    let labels = |service: &str| {
        vec![
            "--label".to_owned(),
            "com.docker.compose.project=p".to_owned(),
            "--label".to_owned(),
            format!("com.docker.compose.service={service}"),
        ]
    };
    let run = |name: &str, extra: &[String]| {
        let mut args = vec!["run", "-d", "--name", name];
        args.extend(extra.iter().map(String::as_str));
        args.extend([stores.image.as_str(), "sleep", "300"]);
        stores.docker_ok(&args)
    };
    let b = run(
        "b",
        &[labels("b"), vec!["-p".to_owned(), "18081:8081".to_owned()]].concat(),
    );
    let mut with_depends = labels("a");
    with_depends.extend([
        "--label".to_owned(),
        "com.docker.compose.depends_on=b:service_started:false".to_owned(),
    ]);
    let a = run("a", &with_depends);
    let links = || {
        let got = api::get(&socket, "/shipyard/links").unwrap();
        assert_eq!(got.status, 200, "{}", got.text());
        got.json()
    };
    let of_kind = |answer: &serde_json::Value, kind: &str| -> Vec<serde_json::Value> {
        answer["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|link| link["kind"] == kind)
            .cloned()
            .collect()
    };
    let first = links();
    assert_eq!(first["version"], 1);
    let declared = of_kind(&first, "declared");
    assert_eq!(declared.len(), 1, "{first}");
    assert_eq!(declared[0]["from"], a.as_str());
    assert_eq!(declared[0]["to"], b.as_str());
    assert_eq!(declared[0]["refused"], false);
    // The control: nothing connected, nothing observed.
    assert!(of_kind(&first, "observed").is_empty(), "{first}");
    if is_root() {
        assert_eq!(first["observed"]["available"], true, "{first}");
        // The containers' host is the service's network namespace.
        let host = std::fs::File::open(format!("/proc/{}/ns/net", service.0.id())).unwrap();
        observed_between(&stores, (&a, &b), &host, &links, &of_kind);
    } else {
        assert_eq!(first["observed"]["available"], false, "{first}");
        assert_eq!(first["observed"]["cause"], "rootless", "{first}");
        // A container's own sockets are still its own to read.
        let sockets = api::get(&socket, "/shipyard/containers/a/sockets").unwrap();
        assert_eq!(sockets.status, 200, "{}", sockets.text());
        assert_eq!(sockets.json()["version"], 1);
    }
    stores.docker_ok(&["stop", "-t", "0", "b"]);
    eventually(Duration::from_secs(10), "the declared link refused", || {
        of_kind(&links(), "declared")[0]["refused"] == true
    });
}

/// An inspected container's first network's `key` (`IPAddress`,
/// `Gateway`), as an address.
fn network_address(inspected: &serde_json::Value, key: &str) -> std::net::IpAddr {
    inspected["NetworkSettings"]["Networks"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()[key]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// The root half of [`the_map_s_links_and_sockets_are_served`]: `a`
/// connects to a listener in `b`'s network namespace (each socket made by
/// a thread joined to its container's), a third container joins `b`'s, and
/// a client in `host` (the service's namespace) connects through `b`'s
/// published port.
fn observed_between(
    stores: &Stores,
    (a, b): (&str, &str),
    host: &std::fs::File,
    links: &dyn Fn() -> serde_json::Value,
    of_kind: &dyn Fn(&serde_json::Value, &str) -> Vec<serde_json::Value>,
) {
    use shipyard_test_support::net::in_netns;
    let inspect = |name: &str| {
        api::get(&stores.socket(), &format!("/containers/{name}/json"))
            .unwrap()
            .json()
    };
    let netns = |name: &str| {
        let pid = inspect(name)["State"]["Pid"].as_i64().unwrap();
        std::fs::File::open(format!("/proc/{pid}/ns/net")).unwrap()
    };
    let b_ip = network_address(&inspect("b"), "IPAddress");
    let gateway = network_address(&inspect("a"), "Gateway");
    // An idle third container: the control that a link needs a connection.
    stores.docker_ok(&["run", "-d", "--name", "c", &stores.image, "sleep", "300"]);
    let listener = in_netns(&netns("b"), || {
        std::net::TcpListener::bind("0.0.0.0:8080").unwrap()
    });
    let published = in_netns(&netns("b"), || {
        std::net::TcpListener::bind("0.0.0.0:8081").unwrap()
    });
    let client = in_netns(&netns("a"), || {
        std::net::TcpStream::connect((b_ip, 8080)).unwrap()
    });
    let (_accepted, _) = listener.accept().unwrap();
    // Through the port b publishes on the host, by the network's gateway:
    // the host's NAT masquerades it on its way to b.
    let through_host = in_netns(&netns("a"), || {
        std::net::TcpStream::connect((gateway, 18081)).unwrap()
    });
    let (_accepted_through, _) = published.accept().unwrap();
    // A container in b's network namespace: one table, read once.
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "y",
        "--network",
        "container:b",
        &stores.image,
        "sleep",
        "300",
    ]);
    let mut seen = serde_json::Value::Null;
    eventually(Duration::from_secs(10), "the connections observed", || {
        let began = Instant::now();
        seen = links();
        // H7: one read of a handful of containers, well within a poll.
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        of_kind(&seen, "observed").len() >= 2
            && seen["containers"].as_array().unwrap().iter().any(|node| {
                node["shares_network_with"]
                    .as_array()
                    .is_some_and(|ids| ids.len() == 2)
            })
    });
    let observed = of_kind(&seen, "observed");
    assert_eq!(
        observed.len(),
        2,
        "each counted once, from the client: {seen}"
    );
    let direct = observed.iter().find(|link| link["port"] == 8080).unwrap();
    assert_eq!(
        (&direct["from"], &direct["to"]),
        (&json!(a), &json!(b)),
        "{seen}"
    );
    assert_eq!(direct["connections"], 1);
    assert_eq!(direct["via_host_port"], false);
    let through = observed.iter().find(|link| link["port"] == 8081).unwrap();
    assert_eq!(
        (&through["from"], &through["to"]),
        (&json!(a), &json!(b)),
        "{seen}"
    );
    assert_eq!(through["via_host_port"], true, "{seen}");
    host_client_counted(b, &seen, (host, gateway), &published, links);
    let c = inspect("c")["Id"].as_str().unwrap().to_owned();
    assert!(
        observed
            .iter()
            .all(|link| link["from"] != c.as_str() && link["to"] != c.as_str()),
        "the idle container has no link: {seen}"
    );
    let sockets = api::get(&stores.socket(), "/shipyard/containers/a/sockets")
        .unwrap()
        .json();
    assert_eq!(sockets["version"], 1);
    assert!(
        sockets["sockets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|sock| sock["state"] == "ESTAB" && sock["peer"] == "b"),
        "{sockets}"
    );
    drop((client, through_host));
    stores.docker_ok(&["rm", "-f", "y", "c"]);
}

/// The inspector's and the Volumes section's data (step 14.1b): a running
/// container's isolation as the kernel has it (its own namespaces, its
/// cgroup, its command the init's child, rootless its user mapping), a
/// container not running refused (the control: the running one answers);
/// every volume's size and top level, a long one's listing capped.
#[test]
fn a_container_s_isolation_and_the_volumes_are_served() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let socket = stores.socket();
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "iso",
        "-v",
        "kept:/data",
        &stores.image,
        "sh",
        "-c",
        "head -c 5000 /dev/zero > /data/five; mkdir /data/dir; exec sleep 300",
    ]);
    let isolation = api::get(&socket, "/shipyard/containers/iso/isolation").unwrap();
    assert_eq!(isolation.status, 200, "{}", isolation.text());
    let isolation = isolation.json();
    assert_eq!(isolation["version"], 1);
    let own = |kind: &str| {
        isolation["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ns| ns["kind"] == kind)
            .unwrap()["own"]
            .clone()
    };
    for kind in ["mnt", "pid", "uts", "ipc", "net"] {
        assert_eq!(own(kind), true, "{kind}: {isolation}");
    }
    assert!(
        isolation["cgroup"]
            .as_str()
            .is_some_and(|path| path.starts_with('/')),
        "{isolation}"
    );
    eventually(
        Duration::from_secs(10),
        "its command is the init's child",
        || {
            let now = api::get(&socket, "/shipyard/containers/iso/isolation")
                .unwrap()
                .json();
            now["command"]["args"] == json!(["sleep", "300"])
                && now["command"]["pid"] != now["init_pid"]
        },
    );
    if is_root() {
        assert_eq!(
            own("user"),
            false,
            "root's has the host's users: {isolation}"
        );
    } else {
        assert_eq!(isolation["user"]["inside"], 0, "{isolation}");
        assert_ne!(isolation["user"]["outside"], 0, "{isolation}");
    }
    stores.docker_ok(&["create", "--name", "idle", &stores.image, "sleep", "300"]);
    let refused = api::get(&socket, "/shipyard/containers/idle/isolation").unwrap();
    assert_eq!(refused.status, 409, "{}", refused.text());
    joined_namespaces_are_said(&stores);
    volumes_are_served(&stores);
}

/// A container that joins `iso`'s network (`--network container:`) has
/// the same `net` namespace ID, and its own `uts` (the control).
fn joined_namespaces_are_said(stores: &Stores) {
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "joiner",
        "--network",
        "container:iso",
        &stores.image,
        "sleep",
        "300",
    ]);
    let id = |name: &str, kind: &str| {
        let got = api::get(
            &stores.socket(),
            &format!("/shipyard/containers/{name}/isolation"),
        )
        .unwrap()
        .json();
        got["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ns| ns["kind"] == kind)
            .unwrap()["id"]
            .clone()
    };
    assert!(id("iso", "net").is_u64());
    assert_eq!(id("joiner", "net"), id("iso", "net"));
    assert_ne!(id("joiner", "uts"), id("iso", "uts"));
}

/// The second half of [`a_container_s_isolation_and_the_volumes_are_served`]:
/// `kept` (its container wrote 5,000 bytes and a directory) and a volume
/// of 250 files, through `/shipyard/volumes`.
fn volumes_are_served(stores: &Stores) {
    let socket = stores.socket();
    // The volumes: kept's two entries and its bytes; many's listing capped.
    stores.docker_ok(&[
        "run",
        "--rm",
        "-v",
        "many:/v",
        &stores.image,
        "sh",
        "-c",
        "i=0; while [ $i -lt 250 ]; do : > /v/f$i; i=$((i+1)); done",
    ]);
    let mut answer = serde_json::Value::Null;
    let volume = |answer: &serde_json::Value, name: &str| {
        answer["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|volume| volume["name"] == name)
            .cloned()
    };
    // Shared for 5 s: a read before the files were written may be kept.
    eventually(Duration::from_secs(15), "the volumes read", || {
        answer = api::get(&socket, "/shipyard/volumes").unwrap().json();
        volume(&answer, "kept").is_some_and(|kept| kept["size"].as_u64() >= Some(5000))
            && volume(&answer, "many").is_some()
    });
    assert_eq!(answer["version"], 1);
    let kept = volume(&answer, "kept").unwrap();
    assert_eq!(kept["complete"], true, "{kept}");
    let files: Vec<(&str, &str)> = kept["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["name"].as_str().unwrap(),
                file["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(files, [("dir", "directory"), ("five", "file")], "{kept}");
    let many = volume(&answer, "many").unwrap();
    assert_eq!(many["files"].as_array().unwrap().len(), 200, "{many}");
    assert_eq!(many["more"], 50, "{many}");
}

/// A published port's own count (step 14.3), `seen` before the host's
/// client: `a`'s connection through it is a container's, never the host's
/// (the control); a client in `host` (the containers' host here: the
/// service's network namespace), through the port on `gateway` (one of its
/// own addresses: the `output` chain's DNAT), is.
fn host_client_counted(
    b: &str,
    seen: &serde_json::Value,
    (host, gateway): (&std::fs::File, std::net::IpAddr),
    published: &std::net::TcpListener,
    links: &dyn Fn() -> serde_json::Value,
) {
    let entry = |answer: &serde_json::Value| {
        answer["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["container"] == b && entry["container_port"] == 8081)
            .unwrap()["connections"]
            .clone()
    };
    assert_eq!(entry(seen), 0, "a container's through the host: {seen}");
    let from_host = shipyard_test_support::net::in_netns(host, || {
        std::net::TcpStream::connect((gateway, 18081)).unwrap()
    });
    let (_accepted, _) = published.accept().unwrap();
    eventually(Duration::from_secs(10), "the host's client counted", || {
        entry(&links()) == 1
    });
    drop(from_host);
}

/// The rail's facts, as the desktop client reads them: `info`, the
/// networks (a compose network by its short name, root's: rootless has no
/// shared networks, its list the built-ins alone) and the volumes' count.
fn rail_facts(
    stores: &Stores,
    runtime: &tokio::runtime::Runtime,
    client: &shipyard_client::api::Client,
) {
    let info = runtime.block_on(client.info()).unwrap();
    assert!(info.kernel_version.is_some_and(|kernel| !kernel.is_empty()));
    let images = usize::try_from(info.images.unwrap()).unwrap();
    assert!(images >= 1, "the fixture image");
    stores.docker_ok(&["volume", "create", "counted"]);
    // A compose network by its short name (root's: rootless has no shared
    // networks, its list the built-ins alone).
    if is_root() {
        stores.docker_ok(&[
            "network",
            "create",
            "--label",
            "com.docker.compose.network=front",
            "p_front",
        ]);
    }
    let networks = runtime.block_on(client.networks()).unwrap();
    if is_root() {
        let front = networks
            .iter()
            .find(|network| network.name == "p_front")
            .unwrap();
        assert_eq!(front.short, "front", "compose's own name for it");
        assert!(front.subnet.is_some(), "{front:?}");
    }
    assert!(!networks.is_empty(), "the built-ins");
    assert_eq!(runtime.block_on(client.volume_count()).unwrap(), 1);
}

/// The live numbers (step 14.8a2), with `listed` and `made` running: the
/// host's memory and CPU time; with a cgroup to read (root, or delegated),
/// each running container's memory and its own monitor's, never a
/// container that only exists (`idle`, created) or has stopped (`ended`,
/// its monitor's cgroup left behind); the host's CPU time moves on.
fn usage_facts(
    stores: &Stores,
    runtime: &tokio::runtime::Runtime,
    client: &shipyard_client::api::Client,
) {
    stores.docker_ok(&["create", "--name", "idle", &stores.image, "sleep", "300"]);
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "ended",
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.docker_ok(&["stop", "-t", "0", "ended"]);
    let id = |name: &str| {
        api::get(&stores.socket(), &format!("/containers/{name}/json"))
            .unwrap()
            .json()["Id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = runtime
        .block_on(shipyard_client::usage::usage(client))
        .unwrap();
    assert!(first.memory.total > 0, "{first:?}");
    assert!(
        first.memory.available > 0 && first.memory.available <= first.memory.total,
        "{first:?}"
    );
    assert!(first.cpu.cpus >= 1 && first.cpu.time > 0, "{first:?}");
    let ids: Vec<&str> = first.containers.iter().map(|one| one.id.as_str()).collect();
    for absent in ["idle", "ended"] {
        assert!(!ids.contains(&id(absent).as_str()), "{absent}: {ids:?}");
    }
    if is_root() || shipyard_test_support::delegated_cgroup().is_some() {
        let listed = first
            .containers
            .iter()
            .find(|one| one.id == id("listed"))
            .unwrap_or_else(|| panic!("listed, running: {first:?}"));
        assert!(listed.memory > 0 && listed.cpu_time > 0, "{listed:?}");
        assert!(
            listed.monitor_memory.is_some_and(|memory| memory > 0),
            "its own monitor: {listed:?}"
        );
        assert!(ids.contains(&id("made").as_str()), "{ids:?}");
    }
    // Once the API's shared second is past: a read of its own, the host's
    // time on.
    eventually(
        Duration::from_secs(10),
        "the host's CPU time moves on",
        || {
            runtime
                .block_on(shipyard_client::usage::usage(client))
                .unwrap()
                .cpu
                .time
                > first.cpu.time
        },
    );
}

/// The inspector's facts (step 14.4a) of `made`, running: its inspect,
/// its image's layers, and a stats sample (its memory in use, under its
/// limit; rootless, with a delegated cgroup); a stats stream asked of no
/// container ends at once.
fn inspector_facts(
    stores: &Stores,
    runtime: &tokio::runtime::Runtime,
    client: &shipyard_client::api::Client,
) {
    let inspected = runtime.block_on(client.inspect("made")).unwrap();
    assert_eq!(inspected["State"]["OOMKilled"], false, "{inspected}");
    let image = runtime.block_on(client.image(&stores.image)).unwrap();
    assert!(
        image["RootFS"]["Layers"]
            .as_array()
            .is_some_and(|layers| !layers.is_empty()),
        "{image}"
    );
    // The first sample, or none within 5 s.
    let (sender, mut samples) = tokio::sync::mpsc::unbounded_channel();
    let first = runtime.block_on(async {
        let follow = shipyard_client::stats::follow(client, "made", move |sample| {
            let _ = sender.send(sample);
        });
        tokio::select! {
            sample = samples.recv() => sample,
            _ = follow => None,
            () = tokio::time::sleep(Duration::from_secs(5)) => None,
        }
    });
    // Rootless, a container's numbers are its delegated cgroup's (as the
    // stats tests above: none without one).
    if is_root() || shipyard_test_support::delegated_cgroup().is_some() {
        let sample = first.unwrap();
        assert!(sample.mem > 0 && sample.mem <= sample.limit, "{sample:?}");
    }
    // No such container: the API's refusal, not a stream that ended.
    let gone = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            shipyard_client::stats::follow(client, "no-such", |_| {}),
        )
        .await
    });
    assert!(
        matches!(&gone, Ok(Err(error)) if error.kind() == shipyard_client::ErrorKind::Api),
        "{gone:?}"
    );
}

/// A fixture whose command keeps running, as a run by spec needs (the
/// sheet gives no command: the image's own runs).
const SLEEPS: &str = r#"{"Cmd":["sleep","300"]}"#;

/// The inspector's actions and the Run sheet's run (step 14.4b), through
/// the client: a run by spec (its memory and its port set, nothing else),
/// paused, resumed, stopped, restarted, removed while it runs. Rootless, a
/// memory limit needs a delegated cgroup: without one, none of it runs.
#[test]
fn the_desktop_client_acts_on_containers() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::with_config(SLEEPS) else {
        return;
    };
    let _service = stores.serve(60);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    let spec = shipyard_client::run::RunSpec {
        image: stores.image.clone(),
        name: Some("ran".to_owned()),
        memory_mib: 256,
        // Not 18081: rootless, the links test's `b` publishes that on the
        // same loopback, beside this one.
        port: Some("18091:80".to_owned()),
    };
    let id = runtime
        .block_on(shipyard_client::run::run(&client, &spec, |_| {}))
        .unwrap();
    let inspect = || {
        api::get(&stores.socket(), "/containers/ran/json")
            .unwrap()
            .json()
    };
    let state = |key: &str| inspect()["State"][key].clone();
    let inspected = inspect();
    assert!(
        id.starts_with(inspected["Id"].as_str().unwrap()),
        "{id} {inspected}"
    );
    assert_eq!(
        inspected["HostConfig"]["Memory"], 268_435_456,
        "{inspected}"
    );
    assert_eq!(
        inspected["HostConfig"]["PortBindings"]["80/tcp"][0]["HostPort"], "18091",
        "{inspected}"
    );
    assert_eq!(state("Running"), true);
    // Its port held: a second run's start fails, and leaves nothing behind
    // (the name free for a run again).
    let clash = shipyard_client::run::RunSpec {
        name: Some("clash".to_owned()),
        ..spec.clone()
    };
    let refused = runtime.block_on(shipyard_client::run::run(&client, &clash, |_| {}));
    assert!(refused.is_err(), "{refused:?}");
    let left = api::get(&stores.socket(), "/containers/clash/json").unwrap();
    assert_eq!(left.status, 404, "{}", left.json());
    runtime.block_on(client.pause("ran")).unwrap();
    assert_eq!(state("Paused"), true);
    runtime.block_on(client.unpause("ran")).unwrap();
    assert_eq!(state("Paused"), false);
    runtime.block_on(client.stop("ran")).unwrap();
    assert_eq!(state("Running"), false);
    runtime.block_on(client.restart("ran")).unwrap();
    assert_eq!(state("Running"), true);
    // Running: a plain remove refused, a forced one stops it first.
    let refused = runtime.block_on(client.remove("ran", false));
    assert!(
        matches!(&refused, Err(error) if error.kind() == shipyard_client::ErrorKind::Api),
        "{refused:?}"
    );
    runtime.block_on(client.remove("ran", true)).unwrap();
    let gone = api::get(&stores.socket(), "/containers/ran/json").unwrap();
    assert_eq!(gone.status, 404);
}

/// The console (step 14.5), through the client: a container's logs (its
/// last lines, then a new one as it comes), and a shell in it on a
/// terminal: it answers, takes a resize, outlives a flood, and ends.
#[test]
fn the_desktop_client_shows_the_console() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let script = "echo one; echo two; echo three; sleep 1; echo four; sleep 300";
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "said",
        &stores.image,
        "sh",
        "-c",
        script,
    ]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    // Followed: the first three, then the fourth as it comes.
    let (sender, mut said) = tokio::sync::mpsc::unbounded_channel();
    let followed = runtime.block_on(async {
        let follow = shipyard_client::console::logs(&client, "said", true, move |line| {
            let _ = sender.send(line);
        });
        let read = async {
            let mut lines = Vec::new();
            while lines.len() < 4 {
                lines.push(said.recv().await.unwrap());
            }
            lines
        };
        tokio::select! {
            lines = read => lines,
            _ = follow => Vec::new(),
            () = tokio::time::sleep(Duration::from_secs(20)) => Vec::new(),
        }
    });
    assert_eq!(followed, ["one", "two", "three", "four"]);
    // Not followed: its lines so far, and the read ends.
    let mut read = Vec::new();
    runtime
        .block_on(shipyard_client::console::logs(
            &client,
            "said",
            false,
            |line| {
                read.push(line);
            },
        ))
        .unwrap();
    assert_eq!(read, ["one", "two", "three", "four"]);
    console_shell(&runtime, &client);
    dropped_shell(&stores, &runtime, &client);
}

/// A shell dropped while a command runs in it: the API hangs it up, and
/// its exec ends (nothing left running to hold the service awake).
fn dropped_shell(
    stores: &Stores,
    runtime: &tokio::runtime::Runtime,
    client: &shipyard_client::api::Client,
) {
    use shipyard_client::console::Shell;
    let exec = runtime.block_on(async {
        let (mut shell, output) = Shell::open(client, "said", 80, 24).await.unwrap();
        shell.write(b"sleep 300\n").await.unwrap();
        let exec = shell.exec_id().to_owned();
        drop((shell, output));
        exec
    });
    let path = format!("/exec/{exec}/json");
    shipyard_test_support::eventually(
        Duration::from_secs(30),
        "the dropped shell's exec to end",
        || api::get(&stores.socket(), &path).unwrap().json()["Running"] == false,
    );
}

/// A shell in `said`: it answers, takes a resize, outlives a flood, ends.
fn console_shell(runtime: &tokio::runtime::Runtime, client: &shipyard_client::api::Client) {
    use futures_util::StreamExt as _;
    use shipyard_client::console::Shell;
    runtime.block_on(async {
        let (mut shell, mut output) = Shell::open(client, "said", 80, 24).await.unwrap();
        let mut seen = String::new();
        let mut until = async |output: &mut shipyard_client::console::ShellOutput, wanted: &str| {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while !seen.contains(wanted) {
                let next = tokio::time::timeout_at(deadline, output.next()).await;
                let Ok(Some(Ok(bytes))) = next else {
                    panic!(
                        "no {wanted:?} in {:?}",
                        seen.chars().rev().take(300).collect::<String>()
                    );
                };
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.len() > 1 << 16 {
                    seen.drain(..seen.len() - (1 << 15));
                }
            }
        };
        shell.write(b"echo $((6*7))\n").await.unwrap();
        until(&mut output, "42").await;
        shell.resize(101, 31).await.unwrap();
        shell.write(b"stty size\n").await.unwrap();
        until(&mut output, "31 101").await;
        shell
            .write(b"dd if=/dev/zero bs=1M count=10 2>/dev/null; echo; echo flood-$((1+1))\n")
            .await
            .unwrap();
        until(&mut output, "flood-2").await;
        shell.write(b"echo after-$((3*3))\n").await.unwrap();
        until(&mut output, "after-9").await;
        // Docker's detach keys typed: no detach (the shell's are none's).
        shell.write(b"\x10\x11\n").await.unwrap();
        shell.write(b"echo $((5*5))\n").await.unwrap();
        until(&mut output, "25").await;
        shell.write(b"exit\n").await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(Ok(_)) = output.next().await {}
        })
        .await;
        assert!(ended.is_ok(), "the shell didn't end");
    });
}

/// The Images section (step 14.6a), through the client: two images of one
/// base read with their layers (the base's one digest in both: stored
/// once, its size known), an image a container uses refused (never `-f`),
/// and removed once unused, tag by tag.
#[test]
fn the_desktop_client_reads_the_images() {
    use shipyard_test_support::registry::{LayerEntry, built_layer, fixture_layers, layer_tar};
    // One base layer, built once (a tar of the same tree built twice can
    // differ by its times), in both images.
    let base = layer_tar(shipyard_test_support::rootfs::RootFs::minimal().path());
    let Some(stores) = shipyard_test_support::stores::Stores::serving(
        SHIPYARD,
        fixture_layers(vec![base.clone()]),
    )
    .map(Stores::wrapping) else {
        return;
    };
    let layer = built_layer(&[LayerEntry::File {
        path: "added",
        owner: (0, 0),
        mode: 0o644,
        content: b"one more layer",
    }]);
    let (_registry, derived) = fixture_layers(vec![base, layer]);
    let pulled = stores.shipyard(&["pull", &derived]);
    assert!(
        pulled.status.success(),
        "{}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let _service = stores.serve(60);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    let images = runtime
        .block_on(shipyard_client::images::images(&client))
        .unwrap();
    let of = |name: &str| {
        images
            .iter()
            .find(|image| {
                image
                    .tags
                    .iter()
                    .any(|tag| tag == name || *tag == format!("{name}:latest"))
            })
            .unwrap_or_else(|| panic!("no {name} in {images:?}"))
            .clone()
    };
    let (base, derived_image) = (of(&stores.image), of(&derived));
    assert_eq!(base.layers.len(), 1, "{base:?}");
    assert_eq!(derived_image.layers.len(), 2, "{derived_image:?}");
    assert_eq!(
        base.layers[0].diff, derived_image.layers[0].diff,
        "the base's layer, once"
    );
    assert!(
        derived_image.layers.iter().all(|layer| layer.size > 0),
        "{derived_image:?}"
    );
    // In use: its tag's removal refused (never forced: the engine's check).
    stores.docker_ok(&["create", "--name", "holds", &derived, "sh"]);
    let refused = runtime.block_on(shipyard_client::images::remove(&client, &derived, false));
    assert!(
        matches!(&refused, Err(error) if error.kind() == shipyard_client::ErrorKind::Api),
        "{refused:?}"
    );
    // Unused, under two tags in two repositories: each tag in turn, as the
    // page removes it (by its ID unforced, the engine would refuse the two
    // repositories).
    stores.docker_ok(&["rm", "holds"]);
    stores.docker_ok(&["tag", &derived, "second:1"]);
    for reference in [derived.as_str(), "second:1", &stores.image] {
        runtime
            .block_on(shipyard_client::images::remove(&client, reference, false))
            .unwrap();
    }
    let left = runtime
        .block_on(shipyard_client::images::images(&client))
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
}

/// `shipyard system service --ensure` (step 14.7, what the desktop app runs
/// on Linux): with nothing answering, it starts the service in the
/// background and returns once it answers; with one answering, it starts
/// none; a socket in a directory others may write is refused, nothing
/// started there. The service it started ends once idle.
#[test]
fn system_service_ensure_starts_one_service() {
    let Some(stores) = Stores::new() else { return };
    let socket = stores.socket();
    // The services serving `socket`: each `system service --socket SOCKET`.
    let services = |socket: &std::path::Path| {
        let wanted = socket.as_os_str().as_encoded_bytes();
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| std::fs::read(entry.ok()?.path().join("cmdline")).ok())
            .filter(|line| {
                let args: Vec<&[u8]> = line.split(|byte| *byte == 0).collect();
                args.contains(&b"service".as_slice())
                    && !args.contains(&b"--ensure".as_slice())
                    && args.contains(&wanted)
            })
            .count()
    };
    let ensure = |socket: &std::path::Path| {
        stores
            .command(&["system", "service", "--ensure", "--socket"])
            .arg(socket)
            .env("SHIPYARD_API_IDLE", "3")
            .output()
            .unwrap()
    };
    // The control: nothing serves it yet.
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_err());
    let first = ensure(&socket);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stdout.is_empty());
    assert_eq!(api::get(&socket, "/_ping").unwrap().status, 200);
    assert_eq!(services(&socket), 1);
    let second = ensure(&socket);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(services(&socket), 1, "a second service started");
    // A directory anyone may write: refused, nothing there.
    let shared = stores.scratch.0.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let refused = ensure(&shared.join("api.sock"));
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("isn't yours alone"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!shared.join("api.sock").exists());
    eventually(
        Duration::from_secs(30),
        "the service ends once idle",
        || services(&socket) == 0,
    );
}

/// The Volumes section (step 14.6b), through the client: a volume's size,
/// files and place read; its deletion refused while a stopped container
/// names it (never forced), done once it's gone; a name that would be
/// another path refused before any request.
#[test]
fn the_desktop_client_reads_the_volumes() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&["volume", "create", "kept"]);
    let image = stores.image.clone();
    stores.docker_ok(&[
        "run",
        "--rm",
        "--network",
        "none",
        "-v",
        "kept:/data",
        &image,
        "sh",
        "-c",
        "echo hello > /data/note",
    ]);
    stores.docker_ok(&[
        "create",
        "--name",
        "holds",
        "--network",
        "none",
        "-v",
        "kept:/data",
        &image,
        "sh",
    ]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    let read = || {
        runtime
            .block_on(shipyard_client::volumes::volumes(&client))
            .unwrap()
    };
    let contents = read();
    let kept = contents["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|volume| volume["name"] == "kept")
        .unwrap_or_else(|| panic!("no kept in {contents}"))
        .clone();
    assert!(kept["size"].as_u64().unwrap() >= 6, "{kept}");
    assert_eq!(kept["files"][0]["name"], "note", "{kept}");
    assert!(
        kept["mountpoint"]
            .as_str()
            .unwrap()
            .ends_with("/kept/_data"),
        "{kept}"
    );
    let refused = runtime.block_on(shipyard_client::volumes::remove(&client, "kept"));
    assert!(
        matches!(&refused, Err(error) if error.kind() == shipyard_client::ErrorKind::Api),
        "{refused:?}"
    );
    let bad = runtime.block_on(shipyard_client::volumes::remove(&client, ".."));
    assert!(
        matches!(&bad, Err(shipyard_client::ClientError::BadReference(_))),
        "{bad:?}"
    );
    stores.docker_ok(&["rm", "holds"]);
    runtime
        .block_on(shipyard_client::volumes::remove(&client, "kept"))
        .unwrap();
    let left = read();
    assert!(
        !left["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|volume| volume["name"] == "kept"),
        "{left}"
    );
}

/// The Networks section (step 14.6b), through the client: as root, a bridge
/// network's gateway, bridge and each container's veth (Shipyard's own view,
/// its version known); rootless, no bridge, and the view said absent (403),
/// never a failure.
#[test]
fn the_desktop_client_reads_the_networks() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    let image = stores.image.clone();
    if is_root() {
        stores.docker_ok(&["network", "create", "--subnet", "10.250.12.0/24", "viewed"]);
        stores.docker_ok(&[
            "run",
            "-d",
            "--name",
            "tap",
            "--network",
            "viewed",
            &image,
            "sleep",
            "60",
        ]);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    let networks = runtime.block_on(client.networks()).unwrap();
    if !is_root() {
        assert!(
            networks.iter().all(|network| network.driver != "bridge"),
            "{networks:?}"
        );
        let view = runtime
            .block_on(shipyard_client::networks::view(&client, "pasta"))
            .unwrap();
        assert!(view.is_none(), "{view:?}");
        return;
    }
    let viewed = networks
        .iter()
        .find(|network| network.name == "viewed")
        .unwrap_or_else(|| panic!("{networks:?}"));
    assert_eq!(
        (viewed.driver.as_str(), viewed.gateway.as_deref()),
        ("bridge", Some("10.250.12.1"))
    );
    // Read into the client's own type: every key the engine writes found.
    let view = runtime
        .block_on(shipyard_client::networks::view(&client, "viewed"))
        .unwrap()
        .expect("root's view");
    assert!(view.bridge.starts_with("shy"), "{view:?}");
    assert_eq!(view.gateway, "10.250.12.1");
    // A network's DNS server starts with its first container.
    assert!(view.dns_running, "{view:?}");
    let tap = stores.docker_ok(&["inspect", "-f", "{{.Id}}", "tap"]);
    let attached = &view.endpoints[0];
    assert_eq!(attached.container.as_deref(), Some(tap.trim()), "{view:?}");
    assert!(attached.host_veth.starts_with("shy"), "{view:?}");
    stores.docker_ok(&["rm", "-f", "tap"]);
}

/// The Run sheet's pull (step 14.4b): an image the store lacks is pulled
/// from its registry, each line handed on, then run. Rootless alone: the
/// fixture registry is on the tests' own loopback, and as root a container
/// there would take a subnet of the shared namespace's default network.
#[test]
fn the_desktop_client_pulls_what_it_runs() {
    if is_root() || shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::with_config(SLEEPS) else {
        return;
    };
    let _service = stores.serve_here(60);
    stores.docker_ok(&["rmi", &stores.image]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    let spec = shipyard_client::run::RunSpec {
        image: stores.image.clone(),
        name: Some("pulled".to_owned()),
        memory_mib: 128,
        port: None,
    };
    let mut lines = Vec::new();
    runtime
        .block_on(shipyard_client::run::run(&client, &spec, |line| {
            lines.push(line);
        }))
        .unwrap();
    assert!(!lines.is_empty(), "no pull said");
    let inspected = api::get(&stores.socket(), "/containers/pulled/json")
        .unwrap()
        .json();
    assert_eq!(inspected["State"]["Running"], true, "{inspected} {lines:?}");
    runtime.block_on(client.remove("pulled", true)).unwrap();
}

/// The desktop app's client (step 14.2) against a real service: it
/// connects (its version negotiated down to the API's), lists a container
/// it ran, reads the links at a version it knows, the system's info,
/// counts and networks (a compose network by its short name), starts a
/// created container (14.3), and gets the event of a container started
/// while it follows.
#[test]
fn the_desktop_client_reads_the_api() {
    let Some(stores) = Stores::new() else { return };
    let _service = stores.serve(60);
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "listed",
        &stores.image,
        "sleep",
        "300",
    ]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = shipyard_client::place::Endpoint::Unix(stores.socket());
    let client = runtime
        .block_on(shipyard_client::api::Client::connect(&endpoint))
        .unwrap();
    let listed = runtime.block_on(client.containers()).unwrap();
    assert!(
        listed.iter().any(|container| container
            .names
            .iter()
            .flatten()
            .any(|name| name == "/listed")),
        "{listed:?}"
    );
    let links = runtime.block_on(client.links()).unwrap();
    assert_eq!(links["version"], 1);
    rail_facts(&stores, &runtime, &client);
    // The map's one action (step 14.3): a created container started.
    stores.docker_ok(&["create", "--name", "made", &stores.image, "sleep", "300"]);
    runtime.block_on(client.start("made")).unwrap();
    let inspected = api::get(&stores.socket(), "/containers/made/json")
        .unwrap()
        .json();
    assert_eq!(inspected["State"]["Running"], true, "{inspected}");
    inspector_facts(&stores, &runtime, &client);
    usage_facts(&stores, &runtime, &client);
    // An event for a container started as the client follows: asked from
    // a moment before, so one that comes before the stream is up isn't lost.
    let since = shipyard_sys::time::unix_now() - 1;
    let (seen, heard) = std::sync::mpsc::channel();
    let follower = client.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = runtime.block_on(shipyard_client::events::follow(
            &follower,
            Some(since),
            |event| {
                let _ = seen.send(event);
            },
        ));
    });
    stores.docker_ok(&[
        "run",
        "-d",
        "--name",
        "heard",
        &stores.image,
        "sleep",
        "300",
    ]);
    let began = Instant::now();
    loop {
        let event = heard
            .recv_timeout(Duration::from_secs(10).saturating_sub(began.elapsed()))
            .expect("no event for the container started");
        let named = event
            .actor
            .as_ref()
            .and_then(|actor| actor.attributes.as_ref())
            .and_then(|attributes| attributes.get("name"))
            .is_some_and(|name| name == "heard");
        if named && event.action.as_deref() == Some("start") {
            break;
        }
    }
}
