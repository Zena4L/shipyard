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
///
/// Never waits (F196): a connect that would, a live service's backlog full,
/// is an answer (`EAGAIN`), as one taken is; nothing at the path or nothing
/// listening (`ENOENT`, `ECONNREFUSED`) isn't.
#[must_use]
pub fn answers(socket: &Path) -> bool {
    use nix::sys::socket::{self as sock, AddressFamily, SockFlag, SockType, UnixAddr};
    let Ok(address) = UnixAddr::new(socket) else {
        return false;
    };
    // Non-blocking and close-on-exec from its making on Linux: a process
    // another thread spawns meanwhile never holds it.
    #[cfg(target_os = "linux")]
    let flags = SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = SockFlag::empty();
    let Ok(fd) = sock::socket(AddressFamily::Unix, SockType::Stream, flags, None) else {
        return false;
    };
    #[cfg(not(target_os = "linux"))]
    {
        use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
        if fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).is_err()
            || fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).is_err()
        {
            return false;
        }
    }
    matches!(
        sock::connect(std::os::fd::AsRawFd::as_raw_fd(&fd), &address),
        Ok(()) | Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINPROGRESS)
    )
}

/// How long a launcher ([`ensure_launched`]) and the service it started
/// have, together, before the service is started as the caller's child.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(5);

/// [`ensure`], tried first through `launcher`: a command that starts the
/// service elsewhere (`systemd-run --user`, F198) and exits once it has. It
/// has [`LAUNCH_WAIT`] to exit with success and its service to answer;
/// past that, or once it fails, the service is started by [`ensure`] with
/// `service` (a launched one that answers late is refused the socket, and
/// ends), and the log says why.
///
/// Returns as [`ensure`] does: `None` for a launched service too (it's no
/// child of this process).
///
/// # Errors
///
/// As [`ensure`].
pub fn ensure_launched(
    socket: &Path,
    launcher: &dyn Fn() -> Command,
    service: &dyn Fn() -> Command,
) -> Result<Option<Child>> {
    if answers(socket) {
        return Ok(None);
    }
    let log = socket.parent().map(|dir| dir.join(SERVICE_LOG));
    match launched(socket, launcher, log.as_deref(), LAUNCH_WAIT) {
        Ok(()) => Ok(None),
        Err(why) => {
            if let Some(mut file) = log.as_deref().and_then(open_log) {
                use std::io::Write as _;
                let _ = writeln!(
                    file,
                    "shipyard: the service wasn't launched ({why}): started as its caller's child"
                );
            }
            ensure(socket, service)
        }
    }
}

/// `launcher` run, its output to `log`, and waited for: `Ok` once it has
/// exited with success and something answers on `socket`, within `wait`;
/// else why not (it's killed past `wait`).
fn launched(
    socket: &Path,
    launcher: &dyn Fn() -> Command,
    log: Option<&Path>,
    wait: Duration,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + wait;
    let mut child = spawn(launcher, log).map_err(|error| error.to_string())?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(format!("it ended: {status}")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("it didn't end within {} s", wait.as_secs()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    while !answers(socket) {
        if Instant::now() >= deadline {
            return Err(format!(
                "nothing answered within {} s of its start",
                wait.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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

    /// A live service whose backlog is full answers, at once (F196: a
    /// blocking connect waited until the app's bound killed it); nothing
    /// listening, or nothing there, doesn't.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_backlog_answers_without_waiting() {
        use nix::sys::socket::{
            self as sock, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
        };
        use std::os::fd::AsRawFd as _;
        let dir = scratch("backlog");
        let socket = dir.join("api.sock");
        let address = UnixAddr::new(&socket).unwrap();
        let unix = |flags| sock::socket(AddressFamily::Unix, SockType::Stream, flags, None);
        let listener = unix(SockFlag::SOCK_CLOEXEC).unwrap();
        sock::bind(listener.as_raw_fd(), &address).unwrap();
        sock::listen(&listener, Backlog::new(0).unwrap()).unwrap();
        // Connections queued until one would wait.
        let mut queued = Vec::new();
        loop {
            let client = unix(SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC).unwrap();
            match sock::connect(client.as_raw_fd(), &address) {
                Ok(()) => queued.push(client),
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(other) => panic!("connecting: {other}"),
            }
            assert!(queued.len() < 1000, "the backlog never filled");
        }
        let (tell, told) = std::sync::mpsc::channel();
        let asked = socket.clone();
        std::thread::spawn(move || tell.send(answers(&asked)));
        assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(true));
        // Closed, nothing listens: waited for, as a process another test
        // spawns holds a copy of the listener from its fork to its exec.
        drop(listener);
        let closed = Instant::now();
        while answers(&socket) {
            assert!(closed.elapsed() < Duration::from_secs(5), "still answers");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!answers(&dir.join("none")));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A launcher that exits with success and whose service answers: no
    /// child started here (F198), none of the caller's (the command given
    /// would fail the test if it were). One that fails, or never ends, is
    /// given up on and the service started as the caller's child, the log
    /// saying why.
    #[test]
    fn a_launched_service_is_waited_for_and_a_failed_launch_falls_back() {
        let dir = scratch("launched");
        let socket = dir.join("api.sock");
        let path = socket.clone();
        let binder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::os::unix::net::UnixListener::bind(&path).unwrap()
        });
        let started = ensure_launched(&socket, &|| Command::new("true"), &|| {
            panic!("started as the caller's child")
        });
        assert!(matches!(started, Ok(None)), "{started:?}");
        drop(binder.join().unwrap());
        let _ = std::fs::remove_file(&socket);

        let fallen_back = |launcher: &dyn Fn() -> Command| {
            let path = socket.clone();
            let binder = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                std::os::unix::net::UnixListener::bind(&path).unwrap()
            });
            let started = ensure_launched(&socket, launcher, &|| {
                let mut command = Command::new("sleep");
                command.arg("10");
                command
            });
            let mut child = started.unwrap().expect("no child started");
            drop(binder.join().unwrap());
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&socket);
        };
        fallen_back(&|| Command::new("false"));
        let log = std::fs::read_to_string(dir.join(SERVICE_LOG)).unwrap();
        assert!(log.contains("wasn't launched (it ended"), "{log}");
        // One that never ends: given up on within its bound.
        let begun = Instant::now();
        assert!(
            launched(
                &socket,
                &|| {
                    let mut command = Command::new("sleep");
                    command.arg("10");
                    command
                },
                None,
                Duration::from_millis(300)
            )
            .unwrap_err()
            .contains("didn't end")
        );
        assert!(begun.elapsed() < Duration::from_secs(5));
        // One that ends well but whose service never answers.
        let refused = launched(
            &socket,
            &|| Command::new("true"),
            None,
            Duration::from_millis(300),
        );
        assert!(refused.unwrap_err().contains("nothing answered"));
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
