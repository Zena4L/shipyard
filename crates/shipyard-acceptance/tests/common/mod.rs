//! What the acceptance tests share (step 15.6a): the `shipyard` cargo built,
//! one test's stores and fixture image, and the API's service on a socket of
//! the test's own.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use shipyard_test_support::registry::{Scratch, Serving, fixture, image_runs_here};
use shipyard_test_support::{eventually, is_root};

/// The `shipyard` binary cargo built beside this test's own (in
/// `target/debug/`), or `SHIPYARD_TEST_BIN_DIR`'s.
pub(crate) fn shipyard_binary() -> PathBuf {
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
pub(crate) struct Api {
    /// The test's own directories.
    pub(crate) scratch: Scratch,
    /// The fixture image, in the store.
    pub(crate) image: String,
    _registry: Serving,
}

impl Api {
    /// Its stores, if an image runs here (else it says why).
    pub(crate) fn new() -> Option<Self> {
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
    pub(crate) fn socket(&self) -> PathBuf {
        self.scratch.0.join("sock/api.sock")
    }

    /// `shipyard ARGS` on these stores.
    pub(crate) fn command(&self, args: &[&str]) -> Command {
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
    pub(crate) fn serve(&self) -> Service {
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
pub(crate) struct Service(Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
