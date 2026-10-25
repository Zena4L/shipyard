//! One test's stores and fixture image, for tests that run the `shipyard`
//! binary (step 12.5b1, F145: copied into each test file before). A test
//! file wraps it in a type of its own (`Deref` to it) for what only it does:
//! its service, Docker's CLI, a compose project.

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};

use crate::registry::{Scratch, Serving, fixture, image_runs_here};
use crate::{Shipyard, is_root};

/// A scratch directory with the stores in it (images, containers,
/// networks; a runtime directory of its own when rootless), the fixture
/// image its registry serves, and the binary run on them.
#[derive(Debug)]
pub struct Stores {
    /// The scratch directory (removed when dropped).
    pub scratch: Scratch,
    /// The fixture image's reference.
    pub image: String,
    binary: Shipyard,
    _registry: Serving,
}

impl Stores {
    /// Stores for `binary` and the fixture image, if an image runs here
    /// (else it says why).
    #[must_use]
    pub fn new(binary: Shipyard) -> Option<Self> {
        Self::serving(binary, fixture())
    }

    /// Stores for `binary` and the image `(registry, image)` serves, if an
    /// image runs here (else it says why): the image pulled by a first run.
    ///
    /// # Panics
    ///
    /// The scratch directories can't be made.
    #[must_use]
    pub fn serving(binary: Shipyard, (registry, image): (Serving, String)) -> Option<Self> {
        let scratch = Scratch::new();
        for dir in ["containers", "networks", "xdg"] {
            std::fs::create_dir_all(scratch.0.join(dir)).unwrap();
        }
        for dir in ["containers", "xdg"] {
            std::fs::set_permissions(scratch.0.join(dir), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let stores = Self {
            scratch,
            image,
            binary,
            _registry: registry,
        };
        let probe = stores.shipyard(&[
            "run",
            "--rm",
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            "exit 0",
        ]);
        image_runs_here(&probe).then_some(stores)
    }

    /// `shipyard ARGS`, not yet run, on these stores.
    #[must_use]
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.binary.path());
        command
            .args(args)
            .env("SHIPYARD_IMAGE_STORE", self.scratch.store())
            .env("SHIPYARD_CONTAINER_DIR", self.scratch.0.join("containers"))
            .env("SHIPYARD_NETWORK_DIR", self.scratch.0.join("networks"));
        if !is_root() {
            command.env("XDG_RUNTIME_DIR", self.scratch.0.join("xdg"));
        }
        command
    }

    /// `shipyard ARGS`, run on these stores (its output, for a caller that
    /// looks).
    ///
    /// # Panics
    ///
    /// It can't be run.
    #[allow(
        clippy::must_use_candidate,
        reason = "a test may run it for its effect alone (a removal at its end)"
    )]
    pub fn shipyard(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running shipyard")
    }

    /// `shipyard ARGS`, which must succeed: its stdout.
    ///
    /// # Panics
    ///
    /// It failed, or its stdout isn't text.
    #[allow(
        clippy::must_use_candidate,
        reason = "a test may run it for its effect alone, its success checked"
    )]
    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.shipyard(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}
