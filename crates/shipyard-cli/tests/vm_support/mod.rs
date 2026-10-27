//! The test VM of the macOS VM's end-to-end tests (`tests/vm.rs`,
//! `tests/vm_terminal.rs`, `tests/vm_docker.rs`): a VM directory of the test's own, the guest
//! built by `scripts/guest/build.sh`, the binary signed first (cargo's
//! runner signs the test binary, not the `shipyard` it runs).

#![allow(dead_code, reason = "each test binary uses part of it")]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The binary under test.
pub(crate) const SHIPYARD: &str = env!("CARGO_BIN_EXE_shipyard");

/// The binary signed once for a file's tests: two signings of one file at
/// once could step on each other.
static SIGNED: std::sync::Once = std::sync::Once::new();

/// How many tests' VMs may run at once: each is a 4 GiB VM booting and
/// pulling images; a dozen at once (cargo's threads) wedged a guest, and
/// four at once crashed one boot in ~40 where one alone never did (F132:
/// 6/227 against 0/157, `guest-crash-rate.sh`).
const AT_ONCE: usize = 2;

/// Which of the [`AT_ONCE`] slots run a VM now, and a wait for one to end.
/// Each slot keeps its network card's address from test to test
/// (`target/vm-test-cards/`): a new one for each test's VM would take
/// another of the Mac's NAT leases (253, an hour each) at every run.
static RUNNING: (std::sync::Mutex<[bool; AT_ONCE]>, std::sync::Condvar) = (
    std::sync::Mutex::new([false; AT_ONCE]),
    std::sync::Condvar::new(),
);

/// A VM directory of the test's own (short: a socket's path must fit), its
/// VM stopped and the directory removed at the end; one of [`AT_ONCE`].
pub(crate) struct Vm {
    /// The VM's directory.
    pub(crate) dir: PathBuf,
    guest: PathBuf,
    /// Its slot, of [`RUNNING`].
    slot: usize,
    /// Its slot's kept `network.json`.
    card: PathBuf,
}

impl Vm {
    /// The test's VM, if VM tests run here (`SHIPYARD_TEST_VM=required`;
    /// else it says why), its directory in `/tmp`.
    pub(crate) fn new(name: &str) -> Option<Self> {
        Self::at(name, "/tmp/shipyard-vm-test-")
    }

    /// [`Vm::new`], its directory in the home (as a user's VM directory
    /// is: `~/Library/Application Support/Shipyard/vm`), for a test of what
    /// a bind of the home sees of it. Short, for its socket's path. Those a
    /// killed test left (its process gone: no `Drop` ran) are removed
    /// first: unlike `/tmp`, the home is never swept.
    pub(crate) fn new_in_home(name: &str) -> Option<Self> {
        let home = std::env::var("HOME").expect("HOME, for a VM directory in the home");
        sweep_left_in(Path::new(&home));
        Self::at(name, &format!("{home}/.shipyard-vm-test-"))
    }

    /// The test's VM, its directory `PREFIX` + `name` + this process's ID.
    fn at(name: &str, prefix: &str) -> Option<Self> {
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
        let dir = PathBuf::from(format!("{prefix}{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (slots, freed) = &RUNNING;
        let mut busy = freed
            .wait_while(slots.lock().unwrap(), |busy| busy.iter().all(|b| *b))
            .unwrap();
        let slot = busy.iter().position(|b| !*b).unwrap();
        busy[slot] = true;
        drop(busy);
        let cards = repo.join("target/vm-test-cards");
        std::fs::create_dir_all(&cards).unwrap();
        let card = cards.join(format!("slot-{slot}.json"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if card.is_file() {
            std::fs::copy(&card, dir.join("network.json")).unwrap();
        }
        // Each test VM the size they had before 13.2 (4 GiB, 4 CPUs), not a
        // quarter of this Mac's each: four at once are what F132's crashes
        // came with, and the timings were set at that size. (`vm set` in a
        // test changes it.)
        std::fs::write(
            dir.join("vm.json"),
            r#"{"version":1,"memory_mib":4096,"cpus":4}"#,
        )
        .unwrap();
        Some(Self {
            dir,
            guest,
            slot,
            card,
        })
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
        bounded(self.command(args), COMMAND_LIMIT)
    }

    /// The guest's files it boots.
    pub(crate) fn guest(&self) -> &Path {
        &self.guest
    }

    /// Docker's API on the Mac, as this VM's host serves it.
    pub(crate) fn docker_socket(&self) -> PathBuf {
        self.dir.join("docker.sock")
    }

    /// The guest's console so far.
    pub(crate) fn console(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.dir.join("console.log")).unwrap_or_default())
            .into_owned()
    }
}

/// Removes the test VM directories in `home` whose test process is gone
/// (`.shipyard-vm-test-NAME-PID`).
fn sweep_left_in(home: &Path) {
    let Ok(entries) = std::fs::read_dir(home) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(".shipyard-vm-test-")
            .and_then(|rest| rest.rsplit_once('-'))
            .and_then(|(_, pid)| pid.parse::<i32>().ok())
        else {
            continue;
        };
        let gone = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
            == Err(nix::errno::Errno::ESRCH);
        if gone {
            // Its VM's host may outlive it: stopped first, its disk let go.
            let _ = Command::new(SHIPYARD)
                .args(["vm", "stop"])
                .env_remove("SHIPYARD_VM")
                .env("SHIPYARD_VM_DIR", entry.path())
                .stdin(Stdio::null())
                .output();
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let stopped = self
            .command(&["vm", "stop"])
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        // The slot's address kept for its next VM, unless this one may
        // still run: two VMs with one address would clash on the NAT. Kept
        // as a v1 file (the address alone): a v2 one names this directory,
        // and the next test's new directory would make a new address
        // (F138's rule), a NAT lease spent per test (AGENTS.md §10).
        let kept = self.dir.join("network.json");
        if stopped && kept.is_file() {
            let mac = std::fs::read(&kept)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|file| file.get("mac")?.as_str().map(str::to_owned));
            if let Some(mac) = mac {
                let card = serde_json::json!({ "version": 1, "mac": mac });
                let _ = std::fs::write(&self.card, card.to_string());
            }
        } else if !stopped {
            let _ = std::fs::remove_file(&self.card);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        let (slots, freed) = &RUNNING;
        if let Ok(mut busy) = slots.lock() {
            busy[self.slot] = false;
        }
        freed.notify_one();
    }
}

/// A scratch directory of the test's own, removed at the end.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    /// In the temporary directory.
    pub(crate) fn temp(name: &str) -> Self {
        Self::at(&std::env::temp_dir(), name)
    }

    /// Under the home, which the VM shares.
    pub(crate) fn in_home(name: &str) -> Self {
        Self::at(Path::new(&std::env::var("HOME").unwrap()), name)
    }

    /// Under `/tmp`, which the VM shares too (step 13.4), by the path a
    /// user names (`/tmp`, a link to `/private/tmp` on a Mac).
    pub(crate) fn in_tmp(name: &str) -> Self {
        Self::at(Path::new("/tmp"), name)
    }

    /// Its own, though tests of one process run at once: the process's
    /// ID and a count.
    fn at(base: &Path, name: &str) -> Self {
        static MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = MADE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = base.join(format!(".shipyard-test-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `bytes` as text (what isn't, replaced).
pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Docker's CLI on `PATH`; `None` (said) if it isn't there, a failure
/// where the VM tests are required (`needed` says for what).
pub(crate) fn docker_cli(needed: &str) -> Option<PathBuf> {
    let found = shipyard_sys::path::find_program("docker", std::env::var_os("PATH").as_deref());
    if found.is_none() {
        assert!(
            std::env::var("SHIPYARD_TEST_VM").as_deref() != Ok("required"),
            "{needed} need Docker's CLI on PATH"
        );
        eprintln!("no docker on PATH: skipped");
    }
    found
}

/// How long a VM test's command may take: past the slowest pull and build
/// seen, and a cold boot (F132: a guest that died fails its test, never
/// holds the suite).
pub(crate) const COMMAND_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// `command`'s output, its stdin nothing, killed past `limit` (a hung
/// relay fails its test instead of holding the suite for good).
pub(crate) fn bounded(mut command: Command, limit: std::time::Duration) -> Output {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning");
    finish_within(child, limit, &format!("{command:?}"))
}

/// A command's output within [`COMMAND_LIMIT`], as `bounded`'s, for a
/// command built in place (its stdin as it was set).
pub(crate) trait BoundedOutput {
    /// Its output; killed past the limit, the test failing.
    fn bounded_output(&mut self) -> Output;
}

impl BoundedOutput for Command {
    fn bounded_output(&mut self) -> Output {
        let child = self
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning");
        finish_within(child, COMMAND_LIMIT, &format!("{self:?}"))
    }
}

/// What a running `child` (`what`, for its failure) said, once it ends
/// within `limit`; killed past it, the test failing then (F132: the VM
/// tests' every wait is bounded). Its piped stdout and stderr are read as it
/// runs.
pub(crate) fn finish_within(
    mut child: std::process::Child,
    limit: std::time::Duration,
    what: &str,
) -> Output {
    use std::io::Read as _;
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().expect("waiting") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} still ran after {limit:?}: killed");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}
