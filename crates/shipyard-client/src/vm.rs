//! The Mac's VM, as the app sees it: its phase, asked of its host's own
//! socket (`host.sock`), which never starts it (a connection to launchd's
//! `docker.sock` would, with `vm install`); and its start, by the
//! `shipyard` binary's own `vm start` (never this app re-executed: the VM's
//! host is `shipyard`'s), its lines handed on.

use std::path::Path;
use std::time::Duration;

use crate::settings::Settings;
use crate::{ClientError, Result};

/// How long a start may take: `shipyard vm start`'s own bound.
#[cfg(target_os = "macos")]
pub const START_WAIT: Duration = shipyard_vm::client::START_MOST;
/// How long a start may take (the Mac's bound; no VM elsewhere).
#[cfg(not(target_os = "macos"))]
pub const START_WAIT: Duration = Duration::from_mins(20);

/// How long `codesign` may take to say a signature.
const CODESIGN_WAIT: Duration = Duration::from_secs(10);

/// The most of a failed start's lines kept to say it.
const KEPT_LINES: usize = 20;

/// The entitlement a `shipyard` that runs the VM is signed with.
pub const VIRTUALIZATION: &str = "com.apple.security.virtualization";

/// Where the VM stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum VmState {
    /// No host runs it.
    Stopped,
    /// Its host runs, in this phase (`preparing`, `booting`, `checking`,
    /// `up`, `stopping`; `unanswered` when its lock is held but its
    /// `host.sock` didn't answer: one older than 0.5, or busy).
    Running {
        /// The phase's word.
        phase: &'static str,
        /// Its API serves (`up`): `docker.sock` may be connected to.
        serving: bool,
    },
}

/// The VM's state for `settings`.
///
/// # Errors
///
/// Off the Mac ([`ClientError::Unsupported`]); the VM's places can't be
/// found.
pub fn state(settings: &Settings) -> Result<VmState> {
    #[cfg(target_os = "macos")]
    {
        let paths = crate::place::vm_paths(settings)?;
        Ok(match shipyard_vm::client::phase(&paths) {
            Some(answer) => VmState::Running {
                phase: answer.phase.word(),
                serving: answer.phase == shipyard_vm::phase::HostPhase::Up,
            },
            // No answer isn't a stop: the lock says whether a host runs.
            None => match shipyard_vm::client::status(&paths) {
                Ok(shipyard_vm::client::Status::Stopped) => VmState::Stopped,
                Ok(shipyard_vm::client::Status::Running(_)) | Err(_) => VmState::Running {
                    phase: "unanswered",
                    serving: false,
                },
            },
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = settings;
        Err(ClientError::Unsupported("Shipyard's VM is the Mac's"))
    }
}

/// How long [`wait_serving`] waits after a start: the host says it serves
/// once its watch sees the guest ready, up to a second after `vm start`
/// returns (step 13.8c).
pub const SERVING_WAIT: Duration = Duration::from_secs(30);

/// Waits until the VM serves its API, asking its host (never `docker.sock`)
/// every quarter second, within `within`.
///
/// # Errors
///
/// [`ClientError::VmStart`] if it stopped meanwhile; [`ClientError::Timeout`]
/// past `within`; the state's own.
pub async fn wait_serving(settings: &Settings, within: Duration) -> Result<()> {
    let asked = async {
        loop {
            let settings = settings.clone();
            let now = tokio::task::spawn_blocking(move || state(&settings))
                .await
                .map_err(std::io::Error::other)??;
            match now {
                VmState::Running { serving: true, .. } => return Ok(()),
                VmState::Stopped => {
                    return Err(ClientError::VmStart(
                        "the VM stopped before it served".to_owned(),
                    ));
                }
                VmState::Running { .. } => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    };
    tokio::time::timeout(within, asked)
        .await
        .map_err(|_| ClientError::Timeout {
            what: "the VM's API".to_owned(),
            seconds: within.as_secs(),
        })?
}

/// Whether `shipyard`'s signature carries [`VIRTUALIZATION`] (an unsigned
/// build can't start the VM: said with how to sign it).
///
/// # Errors
///
/// [`ClientError::Shipyard`] if it doesn't, or `codesign` can't tell.
pub async fn check_signed(shipyard: &Path) -> Result<()> {
    let output = tokio::time::timeout(
        CODESIGN_WAIT,
        tokio::process::Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(shipyard)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ClientError::Shipyard("codesign didn't answer".to_owned()))??;
    if String::from_utf8_lossy(&output.stdout).contains(VIRTUALIZATION) {
        Ok(())
    } else {
        Err(ClientError::Shipyard(format!(
            "{} isn't signed to run the VM (no {VIRTUALIZATION} entitlement): sign it with \
             scripts/macos/sign.sh",
            shipyard.display()
        )))
    }
}

/// Starts the VM with `shipyard vm start` (its VM directory the settings'),
/// each line it says handed to `on_line`, within [`START_WAIT`].
///
/// # Errors
///
/// [`ClientError::VmStart`] with its last lines if it failed;
/// [`ClientError::Timeout`] past its bound.
pub async fn start(
    shipyard: &Path,
    settings: &Settings,
    mut on_line: impl FnMut(String),
) -> Result<()> {
    use tokio::io::AsyncBufReadExt as _;
    let mut command = tokio::process::Command::new(shipyard);
    command
        .args(["vm", "start"])
        .stdin(std::process::Stdio::null())
        // Its progress is on stderr; stdout is never read, so never a pipe
        // that could fill and hold the start.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        // An app quit mid-start leaves the boot to finish, then to the
        // VM's idle stop (D20 allows it): never a VM killed half-booted.
        .kill_on_drop(false);
    if let Some(dir) = &settings.vm_dir {
        command.env("SHIPYARD_VM_DIR", dir);
    }
    let mut child = command.spawn()?;
    let mut lines = std::collections::VecDeque::new();
    // Read as bytes, each line said lossily: a line that isn't UTF-8 is
    // never an end of the start.
    let mut stderr = tokio::io::BufReader::new(
        child
            .stderr
            .take()
            .ok_or_else(|| ClientError::VmStart("its output can't be read".to_owned()))?,
    )
    .split(b'\n');
    let ran = tokio::time::timeout(START_WAIT, async {
        while let Some(line) = stderr.next_segment().await? {
            let line = String::from_utf8_lossy(&line).into_owned();
            on_line(line.clone());
            lines.push_back(line);
            if lines.len() > KEPT_LINES {
                lines.pop_front();
            }
        }
        child.wait().await
    })
    .await
    .map_err(|_| ClientError::Timeout {
        what: "the VM's start".to_owned(),
        seconds: START_WAIT.as_secs(),
    })??;
    if ran.success() {
        Ok(())
    } else {
        Err(ClientError::VmStart(Vec::from(lines).join("\n")))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A fake `shipyard` at a scratch path, running `body`.
    fn fake(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("shipyard-client-vm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("shipyard");
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// `vm start` run with the settings' VM directory, each line it says
    /// handed on as it comes (one not UTF-8 said lossily, never an end); a
    /// failure carries its last lines.
    #[test]
    fn a_start_hands_on_its_lines_and_its_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let settings = Settings {
            vm_dir: Some("/somewhere/vm".into()),
            ..Settings::default()
        };
        let started = fake(
            "ok",
            r#"[ "$1 $2" = "vm start" ] || exit 9
echo booting >&2
printf 'caf\351\n' >&2
echo "dir $SHIPYARD_VM_DIR" >&2
echo "on stdout""#,
        );
        let mut lines = Vec::new();
        runtime
            .block_on(start(&started, &settings, |line| lines.push(line)))
            .unwrap();
        assert_eq!(lines, ["booting", "caf\u{fffd}", "dir /somewhere/vm"]);
        let failed = fake("fails", "echo 'not enough memory' >&2; exit 1");
        let error = runtime
            .block_on(start(&failed, &settings, |_| {}))
            .unwrap_err();
        assert!(
            matches!(&error, ClientError::VmStart(said) if said == "not enough memory"),
            "{error:?}"
        );
        for program in [started, failed] {
            std::fs::remove_dir_all(program.parent().unwrap()).unwrap();
        }
    }
}
