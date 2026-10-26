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

/// The log of a service started here, beside its socket (F149).
pub const SERVICE_LOG: &str = "service.log";

/// Past this, a service's log is moved aside at the next start
/// (`service.log.1`), and a new one begun.
const LOG_MAX: u64 = 4 * 1024 * 1024;

/// Whether something serves `socket` (systemd's socket answers at once).
#[must_use]
pub fn answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Makes sure something serves `socket`: if nothing answers, `service`
/// (the command that serves it, as `shipyard system service --socket
/// SOCKET`) is started, in a process group of its own, its output to
/// [`SERVICE_LOG`] beside the socket (nowhere if that can't be had), and
/// waited for, up to [`SERVICE_WAIT`]. One that ends first is
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
    let log = socket.parent().map(|dir| dir.join(SERVICE_LOG));
    let mut child = spawn(service, log.as_deref())?;
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
            child = spawn(service, log.as_deref())?;
        }
        if Instant::now() >= deadline {
            // Not left behind: a long-lived caller (the guest's agent)
            // would keep it as a zombie.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new(
                ErrorKind::Busy,
                format!(
                    "shipyard system service didn't serve {} within {} s{}: its log, {}, may \
                     say why",
                    socket.display(),
                    SERVICE_WAIT.as_secs(),
                    ended.map_or_else(String::new, |status| format!(" (it ended: {status})")),
                    log.as_deref().map_or_else(
                        || "beside its socket".to_owned(),
                        |log| log.display().to_string()
                    )
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(Some(child))
}

/// `service`, started in a group of its own (a Ctrl-C to its caller isn't
/// its), its input nowhere, its output to `log` (nowhere without one).
fn spawn(service: &dyn Fn() -> Command, log: Option<&Path>) -> Result<Child> {
    use std::os::unix::process::CommandExt as _;
    let opened = log.and_then(open_log);
    let output = |copy: Option<&std::fs::File>| {
        copy.and_then(|file| file.try_clone().ok())
            .map_or_else(Stdio::null, Stdio::from)
    };
    service()
        .stdin(Stdio::null())
        .stdout(output(opened.as_ref()))
        .stderr(output(opened.as_ref()))
        .process_group(0)
        .spawn()
        .map_err(|e| Error::wrap(ErrorKind::Io, e).context("starting shipyard system service"))
}

/// The service's log at `path`, to append to: its directory made the
/// caller's alone (as the socket's is), the file `0600`, never through a
/// link; one past [`LOG_MAX`] moved aside first to `service.log.1` (the one
/// before it gone), so a service still writing to it keeps its words on
/// disk. `None` if any of it fails: a log is no reason not to serve.
fn open_log(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = path.parent()?;
    if !matches!(shipyard_sys::fs::private_dir(dir), Ok(None)) {
        return None;
    }
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()
    };
    let file = open()?;
    if file.metadata().ok()?.len() <= LOG_MAX {
        return Some(file);
    }
    let mut older = path.as_os_str().to_owned();
    older.push(".1");
    std::fs::rename(path, older).ok()?;
    open()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A service's words go to its log beside the socket, appended start
    /// after start; a log past its bound is moved aside at the next start
    /// (`service.log.1`, the one before it gone), never growing for good
    /// (F149).
    #[test]
    fn a_service_s_words_are_logged_and_the_log_is_bounded() {
        let dir = scratch("log").join("sock");
        let log = dir.join(SERVICE_LOG);
        let said = |words: &str| {
            let words = words.to_owned();
            let mut child = spawn(
                &move || {
                    let mut command = Command::new("sh");
                    command.args(["-c", &format!("echo {words} >&2")]);
                    command
                },
                Some(&log),
            )
            .unwrap();
            child.wait().unwrap();
        };
        said("first");
        said("second");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "first\nsecond\n");
        let big = vec![b'x'; usize::try_from(LOG_MAX).unwrap() + 10];
        std::fs::write(&log, &big).unwrap();
        said("after");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "after\n");
        assert_eq!(std::fs::read(dir.join("service.log.1")).unwrap(), big);
    }

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
