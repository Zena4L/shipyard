//! `shipyard compose` against Shipyard's API (stage 11, step 11.4a1, D27):
//! Docker's own `docker compose` (its CLI plugin, pinned in the test
//! image), started through `shipyard compose`, which starts the API's
//! service itself when nothing serves its socket (D11.4-2) and lets it end
//! once idle.
//!
//! Each test has stores of its own, a socket of its own
//! (`SHIPYARD_API_SOCKET`), and the fixture registry's image. Root's runs
//! in a network namespace of its own (the project's network is a bridge);
//! rootless Shipyard has no shared networks, so its project's services
//! have none of their own (`network_mode: none`). Linux only, and only
//! where `docker` is installed (`SHIPYARD_TEST_TOOLS=required` makes that
//! a failure).

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use shipyard_test_support::{Shipyard, eventually, is_root, reference_tool};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// One test's stores, socket, project directory and fixture image:
/// test-support's stores, with a compose project's place.
struct Project(shipyard_test_support::stores::Stores);

impl std::ops::Deref for Project {
    type Target = shipyard_test_support::stores::Stores;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Project {
    /// Its stores, if `docker compose` is here and an image runs (else it
    /// says why): the image in the store, so compose's `pull_policy:
    /// missing` finds it (root's project runs where the registry can't be
    /// reached).
    fn new() -> Option<Self> {
        if !reference_tool("docker") {
            return None;
        }
        // Docker's CLI without its compose plugin: as a missing tool.
        let plugin = Command::new("docker").args(["compose", "version"]).output();
        if !plugin.as_ref().is_ok_and(|output| output.status.success()) {
            assert!(
                std::env::var("SHIPYARD_TEST_TOOLS").as_deref() != Ok("required"),
                "docker compose is required here (SHIPYARD_TEST_TOOLS), but isn't installed"
            );
            return None;
        }
        let stores = shipyard_test_support::stores::Stores::new(SHIPYARD)?;
        for dir in ["sock", "docker", "project"] {
            std::fs::create_dir_all(stores.scratch.0.join(dir)).unwrap();
        }
        std::fs::set_permissions(
            stores.scratch.0.join("sock"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Some(Self(stores))
    }

    /// The API's socket for this test.
    fn socket(&self) -> PathBuf {
        self.scratch.0.join("sock/api.sock")
    }

    /// `shipyard ARGS` on these stores (`compose …` too), in the project's
    /// directory.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = self.0.command(args);
        command
            .current_dir(self.scratch.0.join("project"))
            .env("SHIPYARD_API_SOCKET", self.socket())
            .env("SHIPYARD_API_IDLE", "3")
            .env("DOCKER_CONFIG", self.scratch.0.join("docker"))
            .env("COMPOSE_PROJECT_NAME", "proj");
        command
    }

    /// `shipyard ARGS`, run.
    fn shipyard(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running shipyard")
    }

    /// `shipyard compose ARGS`, which must succeed: its stdout and stderr
    /// (compose says its progress on stderr).
    fn compose(&self, args: &[&str]) -> String {
        let mut all = vec!["compose"];
        all.extend_from_slice(args);
        let output = self.shipyard(&all);
        assert!(output.status.success(), "compose {args:?}: {output:?}");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// Writes the project's `compose.yaml`.
    fn write(&self, text: &str) {
        std::fs::write(self.scratch.0.join("project/compose.yaml"), text).unwrap();
    }

    /// Whether the API's service still serves this test's socket.
    fn served(&self) -> bool {
        std::os::unix::net::UnixStream::connect(self.socket()).is_ok()
    }
}

/// A project of three services through `shipyard compose`, as root: the
/// API's service started by it; `up -d` (a health-checked service awaited
/// by its dependent, a named volume, a port, labels), `ps`, `logs`,
/// `exec`, `run --rm`, an attached `up` whose exit code is its service's
/// (compose follows its containers' events by label), `stop`/`start`,
/// `restart`, `down -v` with a container of another profile still naming
/// the network (Docker's rule: removed); then the service, idle, ends.
#[test]
#[ignore = "needs root"]
fn compose_runs_a_project() {
    let Some(project) = Project::new() else {
        return;
    };
    project.write(&root_project(&project.image));
    shipyard_test_support::in_new_netns(move || {
        assert!(!project.served(), "nothing serves it yet");
        project.compose(&["up", "-d"]);
        assert!(project.served(), "shipyard compose started the service");
        let ps = project.compose(&["ps", "--format", "{{.Service}} {{.Status}} {{.Ports}}"]);
        assert!(
            ps.lines()
                .any(|line| line.starts_with("db Up") && line.contains("(healthy)")),
            "{ps}"
        );
        assert!(
            ps.lines()
                .any(|line| line.starts_with("web Up") && line.contains("18080->80/tcp")),
            "{ps}"
        );
        shares_its_namespaces(&project, "side", "web");
        has_its_files(&project, "side", "web");
        let labelled = project.shipyard(&[
            "inspect",
            "-f",
            "{{index .Config.Labels \"app\"}}",
            "proj-web-1",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&labelled.stdout).trim(),
            "probe",
            "{labelled:?}"
        );
        eventually(Duration::from_secs(10), "its output", || {
            project.compose(&["logs", "web"]).contains("web-up")
        });
        assert!(project.compose(&["top", "web"]).contains("sleep 300"));
        assert!(
            project
                .compose(&["exec", "-T", "web", "sh", "-c", "echo exec-ok"])
                .contains("exec-ok")
        );
        assert!(
            project
                .compose(&["run", "--rm", "web", "sh", "-c", "echo ran-once"])
                .contains("ran-once")
        );
        let attached = project.shipyard(&[
            "compose",
            "--profile",
            "once",
            "up",
            "--abort-on-container-exit",
            "--exit-code-from",
            "once",
            "once",
        ]);
        assert_eq!(attached.status.code(), Some(3), "{attached:?}");
        assert!(
            String::from_utf8_lossy(&attached.stdout).contains("once-done"),
            "{attached:?}"
        );
        project.compose(&["stop"]);
        project.compose(&["start"]);
        project.compose(&["restart", "web"]);
        down_leaves_nothing(&project);
        // Its socket goes with it (a look by connecting would be a client,
        // and keep it up).
        eventually(Duration::from_secs(20), "the service, idle, ends", || {
            !project.socket().exists()
        });
    });
}

/// Rootless, a project whose services have no network of their own (no
/// shared networks rootless): `up -d`, `ps`, `logs`, `exec`, a recreate,
/// `down -v`,
/// a named volume and an anonymous one made and removed, a bind's missing
/// source made (compose sends it as a `Binds` entry).
#[test]
fn compose_runs_a_rootless_project() {
    if is_root() {
        return;
    }
    let Some(project) = Project::new() else {
        return;
    };
    project.write(&format!(
        "services:
  app:
    image: {image}
    command: [\"sh\", \"-c\", \"echo app-up > /data/up; cat /data/up; sleep 300\"]
    network_mode: none
    volumes: [\"data:/data\", \"/cache\", \"./made:/made\"]
  side:
    image: {image}
    command: [\"sleep\", \"300\"]
    network_mode: \"service:app\"
    ipc: \"service:app\"
    pid: \"service:app\"
    depends_on: [app]
volumes:
  data: {{}}
",
        image = project.image
    ));
    let made = project.scratch.0.join("project/made");
    assert!(!made.exists());
    project.compose(&["up", "-d"]);
    assert!(
        made.is_dir(),
        "a short-syntax bind's missing source is made"
    );
    let ps = project.compose(&["ps", "--format", "{{.Service}} {{.State}}"]);
    assert!(ps.contains("app running"), "{ps}");
    shares_its_namespaces(&project, "side", "app");
    eventually(Duration::from_secs(10), "its output", || {
        project.compose(&["logs", "app"]).contains("app-up")
    });
    assert!(
        project
            .compose(&["exec", "-T", "app", "cat", "/data/up"])
            .contains("app-up")
    );
    // A recreate (a changed file, `--force-recreate`): compose makes the new
    // container under a name of its own, removes the old, then renames the
    // new to the old's (`POST /containers/{id}/rename`: 11.6b's API review).
    project.compose(&["up", "-d", "--force-recreate", "--no-deps", "side"]);
    let names = project.shipyard(&["ps", "-a", "--format", "{{.Names}}"]);
    let names = String::from_utf8_lossy(&names.stdout).into_owned();
    assert!(names.lines().any(|name| name == "proj-side-1"), "{names}");
    assert!(
        !names.lines().any(|name| name.ends_with("_proj-side-1")),
        "none left under its temporary name: {names}"
    );
    let ps = project.compose(&["ps", "--format", "{{.Service}} {{.State}}"]);
    assert!(ps.contains("side running"), "{ps}");
    // Its anonymous `/cache`, beside the named one.
    let volumes = || {
        let output = project.shipyard(&["volume", "ls", "-q"]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let anonymous = |listed: &str| {
        listed
            .lines()
            .filter(|name| shipyard_test_support::is_anonymous_volume(name))
            .count()
    };
    let before = volumes();
    assert!(before.contains("proj_data"), "{before}");
    assert_eq!(anonymous(&before), 1, "{before}");
    project.compose(&["down", "-v"]);
    let after = volumes();
    assert!(!after.contains("proj_data"), "{after}");
    assert_eq!(
        anonymous(&after),
        0,
        "down -v takes the anonymous one: {after}"
    );
}

/// The root project's file: `db` (healthy), `web` on the project's network
/// with a port and a label, `side` in `web`'s network, `once` (another
/// profile).
fn root_project(image: &str) -> String {
    format!(
        "services:
  db:
    image: {image}
    command: [\"sh\", \"-c\", \"echo db-up; sleep 300\"]
    volumes: [\"data:/data\"]
    healthcheck:
      test: [\"CMD\", \"sh\", \"-c\", \"exit 0\"]
      interval: 1s
      retries: 20
  web:
    image: {image}
    command: [\"sh\", \"-c\", \"echo web-up; sleep 300\"]
    depends_on:
      db:
        condition: service_healthy
    ports: [\"18080:80\"]
    labels:
      app: probe
  side:
    image: {image}
    command: [\"sleep\", \"300\"]
    network_mode: \"service:web\"
    ipc: \"service:web\"
    pid: \"service:web\"
    depends_on: [web]
  once:
    image: {image}
    command: [\"sh\", \"-c\", \"echo once-done; exit 3\"]
    profiles: [\"once\"]
volumes:
  data: {{}}
",
    )
}

/// `joiner` is in `joined`'s network, IPC and PID namespaces (`network_mode:`,
/// `ipc:` and `pid: service:`), and in a mount namespace of its own (the
/// control).
fn shares_its_namespaces(project: &Project, joiner: &str, joined: &str) {
    let links = |service: &str| {
        project.compose(&[
            "exec",
            "-T",
            service,
            "sh",
            "-c",
            "readlink /proc/self/ns/net; readlink /proc/self/ns/mnt; readlink /proc/self/ns/ipc; \
             readlink /proc/self/ns/pid",
        ])
    };
    let (joiner, joined) = (links(joiner), links(joined));
    assert_eq!(
        joiner.lines().next(),
        joined.lines().next(),
        "{joiner} / {joined}"
    );
    assert_ne!(
        joiner.lines().nth(1),
        joined.lines().nth(1),
        "{joiner} / {joined}"
    );
    assert_eq!(
        joiner.lines().nth(2),
        joined.lines().nth(2),
        "IPC: {joiner} / {joined}"
    );
    assert_eq!(
        joiner.lines().nth(3),
        joined.lines().nth(3),
        "PID: {joiner} / {joined}"
    );
    assert!(
        joiner
            .lines()
            .nth(3)
            .is_some_and(|line| line.starts_with("pid:[")),
        "{joiner}"
    );
    assert!(joiner.starts_with("net:["), "{joiner}");
}

/// `joiner`'s `/etc` files are made as `joined`'s (on a bridge network):
/// its resolvers, and the line that names it with its address.
fn has_its_files(project: &Project, joiner: &str, joined: &str) {
    let cat = |service: &str, file: &str| project.compose(&["exec", "-T", service, "cat", file]);
    assert_eq!(
        cat(joiner, "/etc/resolv.conf"),
        cat(joined, "/etc/resolv.conf")
    );
    let hostname = cat(joined, "/etc/hostname");
    let hostname = hostname.trim();
    let hosts = cat(joined, "/etc/hosts");
    let named = hosts
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(hostname) && !line.starts_with("127."))
        .unwrap_or_else(|| panic!("{hostname} with its address in {hosts}"));
    assert!(
        cat(joiner, "/etc/hosts").lines().any(|line| line == named),
        "{named}"
    );
    assert_eq!(cat(joiner, "/etc/hostname").trim(), hostname);
}

/// `down -v` of `project` with `once` (another profile) left stopped on its
/// network: the network and the volume go anyway (Docker's rule), `once`
/// stays until its profile's `down`. Each `ls` must succeed, and what goes
/// is seen there first.
fn down_leaves_nothing(project: &Project) {
    let listed = |args: &[&str]| {
        let output = project.shipyard(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let networks = || listed(&["network", "ls", "--format", "{{.Name}}"]);
    let volumes = || listed(&["volume", "ls", "-q"]);
    let containers = || listed(&["ps", "-a", "--format", "{{.Names}} {{.State}}"]);
    // Its control: the network, the volume, and `once` (another
    // profile) left stopped on the network.
    assert!(networks().contains("proj_default"), "{}", networks());
    assert!(volumes().contains("proj_data"), "{}", volumes());
    assert!(
        containers().contains("proj-once-1 exited"),
        "{}",
        containers()
    );
    // `down` without its profile leaves `once`: the network goes anyway.
    project.compose(&["down", "-v"]);
    assert!(!networks().contains("proj_default"), "{}", networks());
    assert!(!volumes().contains("proj_data"), "{}", volumes());
    assert!(containers().contains("proj-once-1"), "{}", containers());
    project.compose(&["--profile", "once", "down"]);
    assert_eq!(containers(), "", "every one gone");
}
