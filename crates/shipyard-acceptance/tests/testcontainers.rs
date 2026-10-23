//! Testcontainers for Rust against Shipyard's Docker API (stage 11, step
//! 11.4b1, D25): `shipyard system service` on a socket of the test's own,
//! `DOCKER_HOST` pointing at it, and Testcontainers' everyday calls: an
//! image pulled if missing, a container started with a wait for its
//! output, a file copied in before its start, `exec`, its logs, and its
//! stop and removal. Root's runs in a network namespace of its own, on a
//! network Testcontainers makes.
//!
//! Linux only; a host where an image can't run says why and returns. The
//! `shipyard` binary is the one cargo built beside this test (or
//! `SHIPYARD_TEST_BIN_DIR`'s): `cargo test --workspace` builds it first,
//! `cargo test -p shipyard-acceptance` alone doesn't (build `-p
//! shipyard-cli` first, or a stale one runs).
//!
//! **One test in this file, and only one:** `DOCKER_HOST` is set in the
//! process, and Testcontainers keeps one client for the process.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use shipyard_test_support::registry::{Scratch, Serving, fixture, image_runs_here};
use shipyard_test_support::{eventually, is_root};
use testcontainers::core::wait::ExitWaitStrategy;
use testcontainers::core::{
    CmdWaitFor, ExecCommand, Healthcheck, IntoContainerPort as _, Mount, WaitFor,
};
use testcontainers::runners::AsyncRunner as _;
use testcontainers::{GenericImage, ImageExt as _};

/// The `shipyard` binary cargo built beside this test's own (in
/// `target/debug/`), or `SHIPYARD_TEST_BIN_DIR`'s.
fn shipyard_binary() -> PathBuf {
    if let Some(dir) = std::env::var_os(shipyard_test_support::BIN_DIR_VAR) {
        return PathBuf::from(dir).join("shipyard");
    }
    let test = std::env::current_exe().unwrap();
    // target/debug/deps/<this test> → target/debug/shipyard
    let built = test.parent().unwrap().parent().unwrap().join("shipyard");
    assert!(
        built.exists(),
        "{} isn't built: build -p shipyard-cli first (cargo test --workspace does)",
        built.display()
    );
    built
}

/// One test's stores, socket and fixture image, and the API's service.
struct Api {
    scratch: Scratch,
    image: String,
    _registry: Serving,
}

impl Api {
    /// Its stores, if an image runs here (else it says why).
    fn new() -> Option<Self> {
        let (registry, image) = fixture();
        let scratch = Scratch::new();
        for dir in ["containers", "networks", "xdg", "sock"] {
            std::fs::create_dir_all(scratch.0.join(dir)).unwrap();
        }
        for dir in ["containers", "xdg", "sock"] {
            std::fs::set_permissions(scratch.0.join(dir), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let api = Self {
            scratch,
            image,
            _registry: registry,
        };
        // The image in the store: root's runs where the registry can't be
        // reached (a network namespace of its own).
        let probe = api
            .command(&[
                "run",
                "--rm",
                "--network",
                "none",
                &api.image,
                "sh",
                "-c",
                "exit 0",
            ])
            .output()
            .unwrap();
        image_runs_here(&probe).then_some(api)
    }

    /// The API's socket.
    fn socket(&self) -> PathBuf {
        self.scratch.0.join("sock/api.sock")
    }

    /// `shipyard ARGS` on these stores.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(shipyard_binary());
        command
            .args(args)
            .env("SHIPYARD_IMAGE_STORE", self.scratch.store())
            .env("SHIPYARD_CONTAINER_DIR", self.scratch.0.join("containers"))
            .env("SHIPYARD_NETWORK_DIR", self.scratch.0.join("networks"))
            .env("SHIPYARD_API_SOCKET", self.socket());
        if !is_root() {
            command.env("XDG_RUNTIME_DIR", self.scratch.0.join("xdg"));
        }
        command
    }

    /// The API's service, serving its socket until it's dropped.
    fn serve(&self) -> Service {
        let child = self
            .command(&[
                "system",
                "service",
                "--socket",
                self.socket().to_str().unwrap(),
            ])
            .env("SHIPYARD_API_IDLE", "600")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        eventually(Duration::from_secs(20), "the API's socket", || {
            std::os::unix::net::UnixStream::connect(self.socket()).is_ok()
        });
        Service(child)
    }
}

/// The API's service, killed when dropped.
struct Service(Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An image reference's name and tag, as Testcontainers takes them: the
/// tag is what follows the last `:` only if no `/` follows it (else that
/// `:` is a registry's port, and the tag is `latest`).
fn name_and_tag(reference: &str) -> (&str, &str) {
    match reference.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, tag),
        _ => (reference, "latest"),
    }
}

/// Testcontainers' everyday calls, against `api`'s service: a container
/// started (a file copied in first, its readiness from its output, on
/// `network` if given), `exec`, its logs, then its stop and removal.
async fn everyday(api: &Api, network: Option<&str>) {
    let (name, tag) = name_and_tag(&api.image);
    let image = GenericImage::new(name, tag).with_wait_for(WaitFor::message_on_stdout("ready"));
    let mut request = image
        .with_cmd(["sh", "-c", "echo ready; cat /given; sleep 300"])
        .with_copy_to("/given", b"from the test\n".to_vec());
    if let Some(network) = network {
        request = request.with_network(network);
    }
    let container = request.start().await.unwrap();
    let mut ran = container
        .exec(ExecCommand::new(["sh", "-c", "cat /given; exit 0"]))
        .await
        .unwrap();
    let said = ran.stdout_to_vec().await.unwrap();
    assert_eq!(said, b"from the test\n");
    let logs = container.stdout_to_vec().await.unwrap();
    let logs = String::from_utf8_lossy(&logs);
    assert!(
        logs.contains("ready") && logs.contains("from the test"),
        "{logs}"
    );
    let id = container.id().to_owned();
    let listed = || {
        let listed = api.command(&["ps", "-aq", "--no-trunc"]).output().unwrap();
        assert!(listed.status.success(), "{listed:?}");
        String::from_utf8(listed.stdout).unwrap()
    };
    assert!(listed().contains(&id), "there first");
    container.stop().await.unwrap();
    container.rm().await.unwrap();
    assert!(!listed().contains(&id), "removed");
}

/// The rest of what Testcontainers' users reach for: a health check
/// waited for, a message on stderr, variables, a label and a name, a
/// volume and a tmpfs, a port published and its host port read, `exec`'s
/// exit code, pause and unpause (where a cgroup of its own can be made), a
/// file copied out, a stop with a timeout
/// and the exit code it leaves.
async fn the_rest(api: &Api) {
    let (name, tag) = name_and_tag(&api.image);
    let image = GenericImage::new(name, tag)
        .with_exposed_port(80.tcp())
        .with_wait_for(WaitFor::message_on_stderr("up"))
        .with_wait_for(WaitFor::healthcheck());
    let container = image
        .with_cmd([
            "sh",
            "-c",
            "echo up >&2; echo kept > /v/x; touch /tmp/ok; sleep 300",
        ])
        .with_health_check(
            Healthcheck::cmd_shell("test -f /tmp/ok")
                .with_interval(Duration::from_millis(500))
                .with_retries(20),
        )
        .with_env_var("GREETING", "hello")
        .with_label("probe", "yes")
        .with_container_name("tc-named")
        .with_mount(Mount::volume_mount("tcvol", "/v"))
        .with_mount(Mount::tmpfs_mount("/scratch"))
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(80).await.unwrap();
    assert_ne!(port, 0);
    let mut greeting = container
        .exec(ExecCommand::new([
            "sh",
            "-c",
            "echo $GREETING; cat /proc/mounts",
        ]))
        .await
        .unwrap();
    let said = String::from_utf8(greeting.stdout_to_vec().await.unwrap()).unwrap();
    assert!(said.starts_with("hello\n"), "{said}");
    assert!(said.contains(" /scratch tmpfs "), "the tmpfs: {said}");
    let failed = container
        .exec(ExecCommand::new(["sh", "-c", "exit 3"]).with_cmd_ready_condition(CmdWaitFor::exit()))
        .await
        .unwrap();
    assert_eq!(failed.exit_code().await.unwrap(), Some(3));
    let labelled = api
        .command(&[
            "inspect",
            "-f",
            "{{index .Config.Labels \"probe\"}}",
            "tc-named",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&labelled.stdout).trim(),
        "yes",
        "{labelled:?}"
    );
    // Freezing needs a cgroup of its own: root's, or a delegated one.
    if is_root() || shipyard_test_support::delegated_cgroup().is_some() {
        container.pause().await.unwrap();
        container.unpause().await.unwrap();
    }
    assert!(container.is_running().await.unwrap());
    let copied = container
        .copy_file_from("/v/x", Vec::<u8>::new())
        .await
        .unwrap();
    assert_eq!(copied, b"kept\n");
    container.stop_with_timeout(Some(1)).await.unwrap();
    assert!(!container.is_running().await.unwrap());
    assert!(container.exit_code().await.unwrap().is_some());
    container.rm().await.unwrap();
}

/// A container waited for until it exits, with the code it's asked to
/// end with (Testcontainers' `WaitFor::exit`).
async fn exits(api: &Api) {
    let (name, tag) = name_and_tag(&api.image);
    let image = GenericImage::new(name, tag)
        .with_wait_for(WaitFor::exit(ExitWaitStrategy::new().with_exit_code(4)));
    let container = image
        .with_cmd(["sh", "-c", "exit 4"])
        .start()
        .await
        .unwrap();
    assert_eq!(container.exit_code().await.unwrap(), Some(4));
    container.rm().await.unwrap();
}

/// The `DOCKER_HOST` Testcontainers reads, once: this binary's one test
/// sets it before Testcontainers first looks.
fn point_docker_host_at(api: &Api) {
    // SAFETY: set before any thread of this test reads the environment
    // (Testcontainers reads it at its first call, below), and this binary
    // has one test.
    unsafe {
        std::env::set_var("DOCKER_HOST", format!("unix://{}", api.socket().display()));
    }
}

/// Testcontainers' everyday calls against Shipyard: as root on a network
/// of its own (in a network namespace of its own), else with no network
/// (rootless has no shared networks).
#[test]
fn testcontainers_runs_against_shipyard() {
    let Some(api) = Api::new() else {
        return;
    };
    point_docker_host_at(&api);
    // Rootless reaches the registry: the image taken out of the store, so
    // Testcontainers pulls it. Root's runs where it can't (its own network
    // namespace), so it keeps it.
    if !is_root() {
        let removed = api.command(&["rmi", &api.image]).output().unwrap();
        assert!(removed.status.success(), "{removed:?}");
    }
    let run = move || {
        let _service = api.serve();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                everyday(&api, is_root().then_some("tcnet")).await;
                the_rest(&api).await;
                exits(&api).await;
            });
    };
    if is_root() {
        shipyard_test_support::in_new_netns(run);
    } else {
        run();
    }
}
