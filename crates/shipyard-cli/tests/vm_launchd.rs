//! End-to-end tests of the VM under launchd (step 11.5b2a): `shipyard vm
//! install` loads a launch agent that holds Docker's socket, and the VM
//! starts at its first connection; `shipyard compose` on the Mac.
//!
//! They run where the VM tests do (`SHIPYARD_TEST_VM=required`), with
//! Docker's CLI on `PATH`. Their agents are temporary jobs in the user's
//! login session (D11.5b2-2): each with a label of its own (its VM
//! directory's), its property list in a scratch directory (never
//! `~/Library/LaunchAgents`), taken out at the end; one a killed run left
//! is taken out at the next.

#![cfg(target_os = "macos")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

/// The most a command may take here (a pull, a cold boot).
const LIMIT: Duration = Duration::from_secs(180);

/// The prefix of the tests' agents' directories (in the temporary one).
const AGENTS: &str = "agents";

mod vm_support;
use vm_support::{Scratch, Vm, bounded, text};

/// Docker's CLI, for these tests.
fn docker_cli() -> Option<std::path::PathBuf> {
    vm_support::docker_cli("these tests")
}

/// Takes out the agents a killed run of these tests left: those whose
/// property list is in a tests' agents' directory (never a user's own) of
/// a run whose process is gone (one going on beside keeps its own).
fn sweep_left_agents() {
    let uid = nix::unistd::getuid().as_raw();
    let listed = Command::new("/bin/launchctl").arg("list").output().unwrap();
    let marker = format!(".shipyard-test-{AGENTS}-");
    for label in text(&listed.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|label| label.starts_with("dev.shipyard.vm."))
    {
        let target = format!("gui/{uid}/{label}");
        let printed = Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()
            .unwrap();
        let left = text(&printed.stdout).lines().any(|line| {
            line.trim()
                .strip_prefix("path = ")
                .and_then(|path| path.split(&marker).nth(1))
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<i32>().ok())
                .is_some_and(|pid| {
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
                })
        });
        if left {
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", &target])
                .status();
        }
    }
}

/// `shipyard` and the guest copied outside the folders macOS guards (the
/// tree is under the Desktop: a program launchd starts there waits for the
/// user's permission), signed as the original is.
struct Copies {
    dir: Scratch,
}

impl Copies {
    fn new(vm: &Vm) -> Self {
        let dir = Scratch::temp("launchd-bin");
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::copy(vm_support::SHIPYARD, dir.0.join("shipyard")).unwrap();
        let signed = Command::new(repo.join("scripts/macos/sign.sh"))
            .arg(dir.0.join("shipyard"))
            .output()
            .unwrap();
        assert!(signed.status.success(), "{signed:?}");
        let guest = dir.0.join("guest");
        let copied = Command::new("/bin/cp")
            .arg("-R")
            .arg(vm.guest())
            .arg(&guest)
            .status()
            .unwrap();
        assert!(copied.success());
        Self { dir }
    }

    /// `shipyard ARGS` (the copy) for `vm`, its guest the copy too, its
    /// agents in `agents`.
    fn command(&self, vm: &Vm, agents: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(self.dir.0.join("shipyard"));
        command.args(args);
        for (name, value) in vm.env() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        // From the home itself: a host launchd started reads the folders
        // macOS guards (this tree is under the Desktop) only with the user's
        // permission, and a command's directory must be in the guest.
        command
            .current_dir(std::env::var_os("HOME").unwrap())
            .env("SHIPYARD_GUEST_DIR", self.dir.0.join("guest"))
            .env("SHIPYARD_VM_AGENTS_DIR", agents);
        command
    }
}

/// A VM whose agent is installed, its property list in `agents`; taken
/// out at the end, whatever the test did.
struct Installed {
    vm: Vm,
    copies: Copies,
    agents: Scratch,
    /// The agent's label.
    label: String,
}

impl Installed {
    fn new(name: &str) -> Option<Self> {
        let vm = Vm::new(name)?;
        docker_cli()?;
        sweep_left_agents();
        let copies = Copies::new(&vm);
        let paths = shipyard_vm::VmPaths::at(vm.dir.clone(), PathBuf::from("/")).unwrap();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let label = shipyard_vm::launchd::label(&paths, home.as_deref());
        let installed = Self {
            vm,
            copies,
            agents: Scratch::temp(AGENTS),
            label,
        };
        let done = installed.shipyard(&["vm", "install"]);
        assert!(done.status.success(), "{done:?}");
        Some(installed)
    }

    /// `shipyard ARGS` for this VM and its agents' directory.
    fn command(&self, args: &[&str]) -> Command {
        self.copies.command(&self.vm, &self.agents.0, args)
    }

    fn shipyard(&self, args: &[&str]) -> Output {
        bounded(self.command(args), LIMIT)
    }

    /// `docker ARGS` against the VM's socket.
    fn docker(&self, args: &[&str]) -> Output {
        docker_at(&self.vm, args)
    }

    /// Whether launchd has the agent.
    fn loaded(&self) -> bool {
        let target = format!("gui/{}/{}", nix::unistd::getuid().as_raw(), self.label);
        Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()
            .unwrap()
            .status
            .success()
    }

    fn host_log(&self) -> String {
        std::fs::read_to_string(self.vm.dir.join("host.log")).unwrap_or_default()
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = bounded(self.command(&["vm", "uninstall"]), LIMIT);
    }
}

/// `docker ARGS` against `vm`'s socket.
fn docker_at(vm: &Vm, args: &[&str]) -> Output {
    let mut command = Command::new(docker_cli().unwrap());
    command
        .args(args)
        .env(
            "DOCKER_HOST",
            format!("unix://{}", vm.docker_socket().display()),
        )
        .env_remove("DOCKER_CONTEXT");
    bounded(command, LIMIT)
}

/// The milestone of 11.5b2a: launchd holds Docker's socket and nothing
/// runs (its run at load only makes the socket); `docker` starts the VM
/// (H7: a cold start within a bound); stopped, the socket stays launchd's
/// and the next `docker` starts it again; `shipyard ps` starts it through
/// launchd too; installing while it runs is refused; `uninstall` leaves no
/// job and no socket.
#[test]
fn launchd_holds_dockers_socket() {
    let Some(installed) = Installed::new("launchd") else {
        return;
    };
    let socket = installed.vm.docker_socket();
    assert!(socket.exists(), "no socket after install");
    let status = installed.shipyard(&["vm", "status"]);
    assert!(text(&status.stdout).starts_with("stopped"), "{status:?}");
    assert!(text(&status.stdout).contains("launchd's"), "{status:?}");
    // (launchd's run at load says so once its wait for a connection ends.)
    let began = Instant::now();
    while !installed.host_log().contains("nothing asked") {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{}",
            installed.host_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let began = Instant::now();
    let version = installed.docker(&["version", "--format", "{{.Server.Version}}"]);
    let took = began.elapsed();
    assert_eq!(
        text(&version.stdout).trim(),
        env!("CARGO_PKG_VERSION"),
        "{version:?}"
    );
    eprintln!("a cold `docker version` through launchd took {took:?}");
    // (launchd starts a job at most every 10 s: its run at load counts.)
    assert!(took < Duration::from_secs(30), "took {took:?}");
    assert!(text(&installed.shipyard(&["vm", "status"]).stdout).starts_with("running"));

    let again = installed.shipyard(&["vm", "install"]);
    assert!(
        !again.status.success(),
        "installed while running: {again:?}"
    );

    assert!(installed.shipyard(&["vm", "stop"]).status.success());
    assert!(socket.exists(), "the stop took launchd's socket");
    let ran = installed.docker(&["run", "--rm", IMAGE, "echo", "again"]);
    assert_eq!(text(&ran.stdout).trim(), "again", "{ran:?}");

    assert!(installed.shipyard(&["vm", "stop"]).status.success());
    // A second command while the host boots (its lock taken, no socket
    // yet): it neither marks nor kicks, so no start mark outlives the start
    // (one would boot the VM at the next login).
    let boots = installed.host_log().matches("booting the guest").count();
    let first = installed.command(&["ps"]);
    let first = std::thread::spawn(move || bounded(first, LIMIT));
    let began = Instant::now();
    while installed.host_log().matches("booting the guest").count() == boots {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "{}",
            installed.host_log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let ps_too = installed.shipyard(&["ps"]);
    let ps = first.join().unwrap();
    assert!(ps.status.success(), "{ps:?}");
    assert!(ps_too.status.success(), "{ps_too:?}");
    assert!(
        !installed.vm.dir.join("start-asked").exists(),
        "a start mark outlived the start"
    );
    let launched = installed
        .host_log()
        .matches("Docker's socket is launchd's")
        .count();
    assert!(
        launched >= 3,
        "the host wasn't launchd's each time:\n{}",
        installed.host_log()
    );

    let removed = installed.shipyard(&["vm", "uninstall"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(!installed.loaded(), "launchd still has the agent");
    assert!(!socket.exists(), "the socket outlived the agent");
}

/// `shipyard compose` on the Mac: Docker's compose against the VM's
/// socket, the VM started first; a project up, then down.
#[test]
fn shipyard_compose_runs_on_the_mac() {
    let Some(vm) = Vm::new("compose-mac") else {
        return;
    };
    if docker_cli().is_none() {
        return;
    }
    let project = Scratch::in_home("compose-mac");
    std::fs::write(
        project.0.join("compose.yaml"),
        format!("services:\n  app:\n    image: {IMAGE}\n    command: sleep 60\n"),
    )
    .unwrap();
    let in_project = |args: &[&str]| {
        let mut command = vm.command(args);
        command.current_dir(&project.0);
        bounded(command, LIMIT)
    };
    let up = in_project(&["compose", "up", "-d"]);
    assert!(up.status.success(), "{up:?}");
    let ps = in_project(&["compose", "ps", "-q"]);
    assert_eq!(text(&ps.stdout).lines().count(), 1, "{ps:?}");
    let down = in_project(&["compose", "down"]);
    assert!(down.status.success(), "{down:?}");
}

/// A host launchd can't boot (its guest gone) is said at once: the CLI
/// fails within a bound, not its whole start limit; `docker` gets Docker's
/// error, never a hang.
#[test]
fn a_host_launchd_cant_boot_is_said() {
    let Some(installed) = Installed::new("launchd-fails") else {
        return;
    };
    std::fs::remove_dir_all(installed.copies.dir.0.join("guest")).unwrap();
    let began = Instant::now();
    let ps = installed.shipyard(&["ps"]);
    let took = began.elapsed();
    assert!(!ps.status.success(), "{ps:?}");
    assert!(took < Duration::from_secs(60), "the failure took {took:?}");
    let version = installed.docker(&["version", "--format", "{{.Server.Version}}"]);
    assert!(!version.status.success(), "{version:?}");
}
