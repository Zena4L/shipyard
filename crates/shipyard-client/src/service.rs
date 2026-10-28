//! Linux's API service started on demand (step 14.7, F190): when nothing
//! answers on the user's socket, `shipyard system service --ensure` (the
//! engine's own rule, as `shipyard compose` starts it) starts the service
//! in a process group of its own and returns once it answers. The service
//! runs on to its idle end (daemonless, D20): neither the app nor the
//! `--ensure` that started it holds it.

use std::path::Path;
use std::time::Duration;

use crate::{ClientError, Result};

/// How long `--ensure` may take: the engine waits 10 s for the service to
/// answer; this is past it.
pub const ENSURE_WAIT: Duration = Duration::from_secs(20);

/// The most of what `shipyard` said that's kept for the message.
const MAX_SAID: usize = 1024;

/// Makes sure Shipyard's service answers on `socket`, by `shipyard`.
///
/// # Errors
///
/// [`ClientError::ServiceStart`] with what it said if it failed;
/// [`ClientError::Timeout`] past [`ENSURE_WAIT`]; the system's if it
/// can't be run.
pub async fn ensure(shipyard: &Path, socket: &Path) -> Result<()> {
    ensure_within(shipyard, socket, ENSURE_WAIT).await
}

/// [`ensure`], bounded by `wait`.
async fn ensure_within(shipyard: &Path, socket: &Path, wait: Duration) -> Result<()> {
    let mut command = tokio::process::Command::new(shipyard);
    command
        .args(["system", "service", "--ensure", "--socket"])
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        // Past its bound, the `--ensure` is killed, never the service it
        // started: that isn't this process's child (the `--ensure`'s), and
        // holds none of these pipes (its output is its log's).
        .kill_on_drop(true);
    let output = tokio::time::timeout(wait, command.output())
        .await
        .map_err(|_| ClientError::Timeout {
            what: "starting Shipyard's service".to_owned(),
            seconds: wait.as_secs(),
        })??;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    // A `shipyard` from before 14.7 has no `--ensure`: its usage error says
    // so, in clap's words, which the window shouldn't.
    if output.status.code() == Some(2) && said.contains("'--ensure'") {
        return Err(ClientError::ServiceStart(format!(
            "the `shipyard` at {} is older than this app (it can't start its own service): \
             update it, or start the service with `shipyard system service`",
            shipyard.display()
        )));
    }
    Err(ClientError::ServiceStart(
        said.trim().chars().take(MAX_SAID).collect(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A fake `shipyard` at a scratch path, running `body` (its directory
    /// removed by the test).
    fn fake(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!(
            "shipyard-client-service-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shipyard");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// It asks `shipyard` for `--ensure` on the socket given, and says
    /// what a failure said.
    #[tokio::test]
    async fn it_runs_the_cli_s_ensure_and_says_its_failure() {
        let asked = std::env::temp_dir().join(format!("shipyard-ensure-{}", std::process::id()));
        let fine = fake("fine", &format!("echo \"$@\" > {}", asked.display()));
        ensure(&fine, Path::new("/run/user/1000/shipyard/api.sock"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&asked).unwrap().trim(),
            "system service --ensure --socket /run/user/1000/shipyard/api.sock"
        );
        let failing = fake("failing", "echo 'shipyard: not yours alone' >&2; exit 1");
        let failed = ensure(&failing, Path::new("/s")).await;
        assert!(
            matches!(&failed, Err(ClientError::ServiceStart(said)) if said == "shipyard: not yours alone"),
            "{failed:?}"
        );
        // The window heads it as unreachable: nothing answers still.
        assert_eq!(failed.unwrap_err().kind(), crate::ErrorKind::Unreachable);
        // An older `shipyard`, without `--ensure`: said as that.
        let older = fake(
            "older",
            "echo \"error: unexpected argument '--ensure' found\" >&2; exit 2",
        );
        let refused = ensure(&older, Path::new("/s")).await;
        assert!(
            matches!(&refused, Err(ClientError::ServiceStart(said)) if said.contains("is older than this app")),
            "{refused:?}"
        );
        // One that never ends is given up on at its bound.
        let stuck = fake("stuck", "exec sleep 30");
        let timed_out = ensure_within(&stuck, Path::new("/s"), Duration::from_millis(300)).await;
        assert!(
            matches!(&timed_out, Err(ClientError::Timeout { .. })),
            "{timed_out:?}"
        );
        let _ = std::fs::remove_file(&asked);
        for path in [fine, failing, older, stuck] {
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }
}
