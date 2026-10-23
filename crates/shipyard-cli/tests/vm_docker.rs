//! End-to-end tests of Docker's API on the Mac (step 11.5b1): Docker's own
//! CLI against the socket the VM's host serves (`docker.sock` in the VM's
//! directory), joined to the guest's API.
//!
//! They run where the VM tests do (`SHIPYARD_TEST_VM=required`), and need
//! Docker's CLI (`docker`, with its compose plugin) on `PATH` there; each
//! command is pointed at the VM's socket by `DOCKER_HOST`, with a Docker
//! configuration of its own: the user's contexts and settings are never
//! read or changed.

#![cfg(target_os = "macos")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

/// The most a `docker` command may take here (a pull included).
const DOCKER_LIMIT: Duration = Duration::from_secs(180);

mod vm_support;
use vm_support::{Scratch, Vm, bounded};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Docker's CLI, found on `PATH`; `None` (said) if it isn't there, a
/// failure where the VM tests are required.
fn docker_cli() -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("docker"))
        .find(|path| path.is_file());
    if found.is_none() {
        assert!(
            std::env::var("SHIPYARD_TEST_VM").as_deref() != Ok("required"),
            "the Mac's Docker API tests need Docker's CLI on PATH"
        );
        eprintln!("no docker on PATH: skipped");
    }
    found
}

/// Docker's CLI pointed at `vm`'s socket, with a configuration directory
/// of its own (`config`).
struct Docker {
    cli: PathBuf,
    host: String,
    config: Scratch,
}

impl Docker {
    fn new(vm: &Vm) -> Option<Self> {
        let cli = docker_cli()?;
        let config = Scratch::temp("docker-config");
        // The user's plugins (compose) found where Docker keeps them, read
        // only: the configuration itself is this test's.
        let home = std::env::var("HOME").unwrap();
        let plugins = serde_json::json!({
            "cliPluginsExtraDirs": [
                format!("{home}/.docker/cli-plugins"),
                "/usr/local/lib/docker/cli-plugins",
                "/opt/homebrew/lib/docker/cli-plugins",
            ]
        });
        std::fs::write(config.0.join("config.json"), plugins.to_string()).unwrap();
        Some(Self {
            cli,
            host: format!("unix://{}", vm.docker_socket().display()),
            config,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .args(args)
            .env("DOCKER_HOST", &self.host)
            .env("DOCKER_CONFIG", &self.config.0)
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH");
        command
    }

    /// `docker ARGS`, bounded (a pull included).
    fn run(&self, args: &[&str]) -> Output {
        bounded(self.command(args), DOCKER_LIMIT)
    }
}

/// The milestone of 11.5b1: Docker's CLI on the Mac uses Shipyard as a
/// daemon. The socket is the user's alone; `docker version` answers within
/// a bound (H7) and names Shipyard; a container runs with its output, its
/// input (a half-close passed on, its output after it kept), a bind of the
/// home, `exec`, a port on the Mac's loopback; the socket is gone once the
/// VM has stopped.
#[test]
fn dockers_cli_uses_shipyard_on_the_mac() {
    let Some(vm) = Vm::new("docker") else {
        return;
    };
    let Some(docker) = Docker::new(&vm) else {
        return;
    };
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    let socket = vm.docker_socket();
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");

    let began = Instant::now();
    let version = docker.run(&["version", "--format", "{{.Server.Version}}"]);
    let took = began.elapsed();
    assert!(version.status.success(), "{version:?}");
    assert_eq!(text(&version.stdout).trim(), env!("CARGO_PKG_VERSION"));
    eprintln!("`docker version` took {took:?}");
    assert!(
        took < Duration::from_secs(5),
        "`docker version` took {took:?}"
    );

    let hi = docker.run(&["run", "--rm", IMAGE, "echo", "hi"]);
    assert!(hi.status.success(), "{hi:?}");
    assert_eq!(text(&hi.stdout).trim_end(), "hi");

    // Input: its end passed on, the container's output after it kept (no
    // cut while the service still answers).
    let mut piped = docker
        .command(&[
            "run",
            "-i",
            "--rm",
            IMAGE,
            "sh",
            "-c",
            "cat; sleep 2; echo after",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    piped.stdin.take().unwrap().write_all(b"typed\n").unwrap();
    let piped = piped.wait_with_output().unwrap();
    assert!(piped.status.success(), "{piped:?}");
    assert_eq!(text(&piped.stdout), "typed\nafter\n");

    // A bind of the home, `exec`, a published port.
    let project = Scratch::in_home("docker-bind");
    std::fs::write(project.0.join("note"), "from the mac\n").unwrap();
    let bind = format!("{}:/w", project.0.display());
    let server = "while true; do printf 'HTTP/1.0 200 OK\\r\\n\\r\\nfrom the guest\\n' \
                  | nc -l -p 80; done";
    let ran = docker.run(&[
        "run",
        "-d",
        "--name",
        "web",
        "-v",
        &bind,
        "-p",
        "127.0.0.1:0:80",
        IMAGE,
        "sh",
        "-c",
        server,
    ]);
    assert!(ran.status.success(), "{ran:?}");
    let read = docker.run(&["exec", "web", "cat", "/w/note"]);
    assert_eq!(text(&read.stdout), "from the mac\n", "{read:?}");
    let port = docker.run(&["port", "web", "80"]);
    let address = text(&port.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(address.starts_with("127.0.0.1:"), "{port:?}");
    let began = Instant::now();
    loop {
        if fetch(&address).as_deref() == Some("from the guest\n") {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "{address} didn't answer; the host's log:\n{}",
            std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(docker.run(&["rm", "-f", "web"]).status.success());

    assert!(vm.shipyard(&["vm", "stop"]).status.success());
    assert!(!socket.exists(), "the socket outlived the VM");
}

/// `GET /` from `address`: the body, if it answered.
fn fetch(address: &str) -> Option<String> {
    let address: std::net::SocketAddr = address.parse().ok()?;
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok()?;
    answer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
}

/// An API connection is a client: `docker events` held open keeps the VM
/// up well past its idle limit; once it's gone, the VM stops by itself
/// (the positive control: the limit does stop it).
/// A Docker client killed during a quiet `exec` on the Mac hangs its exec
/// up, as on Linux (11.7: only a half-close reached the guest's service,
/// so the exec ran on and the host kept its relay). The control: one that
/// writes is gone too (its output's `EPIPE` ended it before), and the
/// container itself is untouched.
#[test]
fn a_killed_quiet_exec_is_hung_up() {
    let Some(vm) = Vm::new("docker-hang-up") else {
        return;
    };
    let Some(docker) = Docker::new(&vm) else {
        return;
    };
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    let up = docker.run(&["run", "-d", "--name", "quiet", IMAGE, "sleep", "600"]);
    assert!(up.status.success(), "{up:?}");
    let mut clients = Vec::new();
    for (name, script) in [
        ("quiet", "echo $$ >/tmp/quiet; exec sleep 600"),
        (
            "talk",
            "echo $$ >/tmp/talk; while :; do echo x; sleep 1; done",
        ),
    ] {
        let client = docker
            .command(&["exec", "-i", "quiet", "sh", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        clients.push((name, client));
    }
    let alive = |name: &str| {
        let looked = docker.run(&[
            "exec",
            "quiet",
            "sh",
            "-c",
            &format!("[ -s /tmp/{name} ] && kill -0 $(cat /tmp/{name}) && echo alive"),
        ]);
        text(&looked.stdout).trim() == "alive"
    };
    let began = Instant::now();
    while !(alive("quiet") && alive("talk")) {
        assert!(began.elapsed() < DOCKER_LIMIT, "the execs never ran");
        std::thread::sleep(Duration::from_millis(200));
    }
    for (_, client) in &mut clients {
        client.kill().unwrap();
        client.wait().unwrap();
    }
    let killed = Instant::now();
    while alive("quiet") || alive("talk") {
        assert!(
            killed.elapsed() < Duration::from_secs(20),
            "an exec ran on 20 s after its client was killed"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("hung up {:?} after the clients' kill", killed.elapsed());
    let still = docker.run(&["inspect", "-f", "{{.State.Running}}", "quiet"]);
    assert_eq!(text(&still.stdout).trim(), "true", "{still:?}");
    assert!(docker.run(&["rm", "-f", "quiet"]).status.success());
}

#[test]
fn an_api_connection_keeps_the_vm_up() {
    let Some(vm) = Vm::new("dockeridle") else {
        return;
    };
    let Some(docker) = Docker::new(&vm) else {
        return;
    };
    let started = vm
        .command(&["vm", "start"])
        .env("SHIPYARD_VM_IDLE", "3")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    let mut events = docker
        .command(&["events"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // Three limits and more, with the poll's 2 s: an idle VM would be down.
    std::thread::sleep(Duration::from_secs(12));
    assert!(
        text(&vm.shipyard(&["vm", "status"]).stdout).starts_with("running"),
        "stopped though a connection was open; the host's log:\n{}",
        std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
    );
    let _ = events.kill();
    let _ = events.wait();
    let began = Instant::now();
    while !text(&vm.shipyard(&["vm", "status"]).stdout).starts_with("stopped") {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "still running; the host's log:\n{}",
            std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Docker's own compose, on the Mac, against the socket: a project up, its
/// logs, down (its network made and removed through the API).
#[test]
fn docker_compose_runs_on_the_mac() {
    let Some(vm) = Vm::new("compose") else {
        return;
    };
    let Some(docker) = Docker::new(&vm) else {
        return;
    };
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    let project = Scratch::in_home("compose");
    std::fs::write(
        project.0.join("compose.yaml"),
        format!(
            "services:\n  app:\n    image: {IMAGE}\n    command: sh -c \"echo up from compose; \
             sleep 60\"\n"
        ),
    )
    .unwrap();
    let in_project = |args: &[&str]| {
        let mut command = docker.command(args);
        command.current_dir(&project.0);
        bounded(command, DOCKER_LIMIT)
    };
    let up = in_project(&["compose", "up", "-d"]);
    assert!(up.status.success(), "{up:?}");
    let began = Instant::now();
    loop {
        let logs = in_project(&["compose", "logs"]);
        if text(&logs.stdout).contains("up from compose") {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(30), "{logs:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let down = in_project(&["compose", "down"]);
    assert!(down.status.success(), "{down:?}");
    let left = docker.run(&["ps", "-aq"]);
    assert_eq!(text(&left.stdout), "", "{left:?}");
}
