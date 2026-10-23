//! Docker's API served on demand (D11.4-2, step 11.5b1): `shipyard system
//! service` started when nothing answers on its socket, as `shipyard
//! compose` does on a Linux host and the macOS guest's agent does for the
//! Mac's `docker`. It exits once idle (daemonless, D20); the next caller
//! starts it again.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, ErrorKind, Result};

/// How long a service started here has to answer.
pub const SERVICE_WAIT: Duration = Duration::from_secs(10);

/// Whether something serves `socket` (systemd's socket answers at once).
#[must_use]
pub fn answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Makes sure something serves `socket`: if nothing answers, `service`
/// (the command that serves it, as `shipyard system service --socket
/// SOCKET`) is started, in a process group of its own and its output
/// nowhere, and waited for, up to [`SERVICE_WAIT`]. One that ends first is
/// no failure while there's time: another caller may have started its own
/// a moment before (this one's then refused), or an idle one may be letting
/// go of the socket; it's started again then.
///
/// Returns the service started here, if one was (running: the caller
/// reaps it or leaves it to be), or `None` if one answered already. One
/// that ended here has been reaped (`try_wait`), and one still running at
/// the deadline is killed and reaped.
///
/// # Errors
///
/// [`ErrorKind::Io`] if it can't be started; [`ErrorKind::Busy`] if nothing
/// answered in time.
pub fn ensure(socket: &Path, service: &dyn Fn() -> Command) -> Result<Option<Child>> {
    if answers(socket) {
        return Ok(None);
    }
    let deadline = Instant::now() + SERVICE_WAIT;
    let mut child = spawn(service)?;
    let mut ended = None;
    while !answers(socket) {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| Error::wrap(ErrorKind::Io, e))?
        {
            ended = Some(status);
            std::thread::sleep(Duration::from_millis(200));
            if answers(socket) {
                break;
            }
            child = spawn(service)?;
        }
        if Instant::now() >= deadline {
            // Not left behind: a long-lived caller (the guest's agent)
            // would keep it as a zombie.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new(
                ErrorKind::Busy,
                format!(
                    "shipyard system service didn't serve {} within {} s{}: run it by hand to \
                     see why",
                    socket.display(),
                    SERVICE_WAIT.as_secs(),
                    ended.map_or_else(String::new, |status| format!(" (it ended: {status})"))
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(Some(child))
}

/// `service`, started in a group of its own (a Ctrl-C to its caller isn't
/// its), its standard files nowhere.
fn spawn(service: &dyn Fn() -> Command) -> Result<Child> {
    use std::os::unix::process::CommandExt as _;
    service()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| Error::wrap(ErrorKind::Io, e).context("starting shipyard system service"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-api-service-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A socket that answers already: nothing is started (the command
    /// given would fail the test if it were).
    #[test]
    fn a_socket_that_answers_starts_nothing() {
        let dir = scratch("answers");
        let socket = dir.join("api.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let started = ensure(&socket, &|| panic!("started a service"));
        assert!(matches!(started, Ok(None)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing answers: the command is started, and waited for until its
    /// socket answers (bound by a thread after a moment, as a service
    /// would: positive control, it didn't answer before).
    #[test]
    fn a_service_is_started_and_waited_for() {
        let dir = scratch("started");
        let socket = dir.join("api.sock");
        assert!(!answers(&socket));
        let path = socket.clone();
        let binder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::os::unix::net::UnixListener::bind(&path).unwrap()
        });
        let started = ensure(&socket, &|| {
            let mut command = Command::new("sleep");
            command.arg("5");
            command
        });
        let mut child = started.unwrap().unwrap();
        assert!(answers(&socket));
        drop(binder.join().unwrap());
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A service that never serves: refused as busy, with its end said.
    #[test]
    fn a_service_that_never_answers_is_busy() {
        let dir = scratch("never");
        let socket = dir.join("api.sock");
        let started = ensure(&socket, &|| Command::new("true"));
        assert_eq!(started.unwrap_err().kind(), ErrorKind::Busy);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
