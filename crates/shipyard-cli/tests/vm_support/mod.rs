//! The test VM of the macOS VM's end-to-end tests (`tests/vm.rs`,
//! `tests/vm_terminal.rs`): a VM directory of the test's own, the guest
//! built by `scripts/guest/build.sh`, the binary signed first (cargo's
//! runner signs the test binary, not the `shipyard` it runs).

#![allow(dead_code, reason = "each test binary uses part of it")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The binary under test.
pub(crate) const SHIPYARD: &str = env!("CARGO_BIN_EXE_shipyard");

/// The binary signed once for a file's tests: two signings of one file at
/// once could step on each other.
static SIGNED: std::sync::Once = std::sync::Once::new();

/// How many tests' VMs may run at once: each is a 4 GiB VM booting and
/// pulling images; a dozen at once (cargo's threads) wedged a guest.
const AT_ONCE: usize = 4;

/// The VMs running now, and a wait for one to end.
static RUNNING: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// A VM directory of the test's own (short: a socket's path must fit), its
/// VM stopped and the directory removed at the end; one of [`AT_ONCE`].
pub(crate) struct Vm {
    /// The VM's directory.
    pub(crate) dir: PathBuf,
    guest: PathBuf,
}

impl Vm {
    /// The test's VM, if VM tests run here (`SHIPYARD_TEST_VM=required`;
    /// else it says why).
    pub(crate) fn new(name: &str) -> Option<Self> {
        let required = std::env::var("SHIPYARD_TEST_VM").as_deref() == Ok("required");
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let guest = std::env::var_os("SHIPYARD_GUEST_DIR")
            .map_or_else(|| repo.join("target/guest"), PathBuf::from);
        if !required {
            eprintln!("VM tests need SHIPYARD_TEST_VM=required: skipped");
            return None;
        }
        assert!(
            guest.join("manifest.json").is_file(),
            "no guest at {} (scripts/guest/build.sh)",
            guest.display()
        );
        SIGNED.call_once(|| {
            let signed = Command::new(repo.join("scripts/macos/sign.sh"))
                .arg(SHIPYARD)
                .status()
                .unwrap();
            assert!(signed.success(), "signing {SHIPYARD}");
        });
        let dir = PathBuf::from(format!(
            "/tmp/shipyard-vm-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let (count, freed) = &RUNNING;
        let mut running = freed
            .wait_while(count.lock().unwrap(), |running| *running >= AT_ONCE)
            .unwrap();
        *running += 1;
        Some(Self { dir, guest })
    }

    /// What a `shipyard` needs to reach this VM: its places, and the VM on
    /// (cargo's runs turn it off, `.cargo/config.toml`). Each variable is
    /// set, or with `None` removed.
    pub(crate) fn env(&self) -> Vec<(String, Option<OsString>)> {
        vec![
            ("SHIPYARD_VM".to_owned(), None),
            ("SHIPYARD_VM_DIR".to_owned(), Some(self.dir.clone().into())),
            (
                "SHIPYARD_GUEST_DIR".to_owned(),
                Some(self.guest.clone().into()),
            ),
        ]
    }

    /// `shipyard ARGS`, to this VM.
    pub(crate) fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(SHIPYARD);
        command.args(args);
        for (name, value) in self.env() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
    }

    /// `shipyard ARGS`, with nothing on its stdin.
    pub(crate) fn shipyard(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .output()
            .expect("running shipyard")
    }

    /// The guest's console so far.
    pub(crate) fn console(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.dir.join("console.log")).unwrap_or_default())
            .into_owned()
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.shipyard(&["vm", "stop"]);
        let _ = std::fs::remove_dir_all(&self.dir);
        let (count, freed) = &RUNNING;
        if let Ok(mut running) = count.lock() {
            *running = running.saturating_sub(1);
        }
        freed.notify_one();
    }
}
