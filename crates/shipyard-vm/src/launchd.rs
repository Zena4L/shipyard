//! The VM under launchd (step 11.5b2a, D11.5b-2, D11.5b2-1): a launch agent
//! of the user's own holds Docker's socket (`docker.sock`) while nothing
//! runs, and starts the VM's host at its first connection, as systemd's
//! socket starts the API on a Linux host. `shipyard vm install` writes and
//! loads it, `uninstall` takes it away; once it's loaded, the CLI starts
//! the host through it too, so the one host always holds launchd's socket.

use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::Digest as _;

use crate::{HOST_ARG, VmError, VmPaths};

/// The agent's label for the user's VM.
pub const LABEL: &str = "dev.shipyard.vm";

/// The name of the agent's socket, in its `Sockets` and as the host asks
/// launchd for it.
const SOCKET_NAME: &std::ffi::CStr = c"docker";

/// launchd's own tool.
const LAUNCHCTL: &str = "/bin/launchctl";

/// How long launchd lets the agent's host stop before it kills it (at
/// `uninstall`, at logout): its guest's orderly stop (90 s, then 10 s to
/// force it) and a margin. launchd's own default is 20 s.
const EXIT_TIMEOUT: u64 = 120;

/// How long `install` waits for launchd's socket.
const SOCKET_WAIT: Duration = Duration::from_secs(10);

/// How long `install` and `uninstall` wait for launchd to let the agent go
/// (its host stopping its guest in order): past [`EXIT_TIMEOUT`].
const UNLOAD_WAIT: Duration = Duration::from_secs(EXIT_TIMEOUT + 10);

/// The agent's label for `paths`: [`LABEL`] for the default VM directory,
/// else one of its own (`LABEL.` and 12 hex of the directory's canonical
/// path: a second VM, a test's), so two never share one and one directory
/// named two ways (`/tmp`, `/private/tmp`) has one.
#[must_use]
pub fn label(paths: &VmPaths, home: Option<&Path>) -> String {
    let canonical = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    let dir = canonical(&paths.dir);
    if home.is_some_and(|home| canonical(&VmPaths::default_dir(home)) == dir) {
        return LABEL.to_owned();
    }
    let digest = sha2::Sha256::digest(dir.as_os_str().as_encoded_bytes());
    format!("{LABEL}.{}", shipyard_sys::hex::lower(&digest[..6]))
}

/// [`label`] with the user's home.
fn label_of(paths: &VmPaths) -> String {
    label(paths, home().as_deref())
}

/// Where the user's launch agents are: `SHIPYARD_VM_AGENTS_DIR` (tests),
/// else `~/Library/LaunchAgents`.
fn agents_dir() -> Result<PathBuf, VmError> {
    if let Some(dir) = std::env::var_os("SHIPYARD_VM_AGENTS_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    home()
        .map(|home| home.join("Library/LaunchAgents"))
        .ok_or_else(|| VmError::Place("HOME isn't set: no LaunchAgents to install in".to_owned()))
}

/// The agent's property list for `paths`.
fn plist_file(paths: &VmPaths) -> Result<PathBuf, VmError> {
    Ok(agents_dir()?.join(format!("{}.plist", label_of(paths))))
}

/// The user's home, as the VM's places take it.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// This binary, as it was run (a link kept as a link: a package manager's
/// `bin/shipyard` outlives its versioned target). What the agent runs, and
/// what `doctor` compares it with.
pub(crate) fn this_binary() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

/// The folder of `home`'s that macOS guards (its privacy permissions:
/// Desktop, Documents, Downloads, iCloud Drive) holding `path`, if one
/// does. A program there, or a file it reads there, waits for the user's
/// permission when launchd starts it (it isn't the terminal's, which has
/// it): the agent's host would hang before its first line.
#[must_use]
pub fn protected(home: Option<&Path>, path: &Path) -> Option<&'static str> {
    let home = home?;
    let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let path = canonical(path);
    [
        ("Desktop", "Desktop"),
        ("Documents", "Documents"),
        ("Downloads", "Downloads"),
        ("Library/Mobile Documents", "iCloud Drive"),
    ]
    .into_iter()
    .find(|(dir, _)| path.starts_with(canonical(&home.join(dir))))
    .map(|(_, name)| name)
}

/// The agent: what it runs (`program`, the host's argument), its socket
/// (only the user's: 0600), the variables it's started with (the VM's
/// places always: the host's must be the CLI's), its log, how long its
/// stop may take.
#[must_use]
pub fn agent(
    label: &str,
    program: &Path,
    paths: &VmPaths,
    env: &[(String, String)],
) -> plist::Value {
    use plist::{Dictionary, Value};
    let text = |path: &Path| Value::String(path.to_string_lossy().into_owned());
    let mut socket = Dictionary::new();
    socket.insert("SockPathName".to_owned(), text(&paths.docker_socket()));
    socket.insert("SockPathMode".to_owned(), Value::Integer(0o600.into()));
    let mut sockets = Dictionary::new();
    sockets.insert(
        SOCKET_NAME.to_string_lossy().into_owned(),
        Value::Dictionary(socket),
    );
    let mut variables = Dictionary::new();
    variables.insert("SHIPYARD_VM_DIR".to_owned(), text(&paths.dir));
    variables.insert("SHIPYARD_GUEST_DIR".to_owned(), text(&paths.guest));
    for (name, value) in env {
        variables.insert(name.clone(), Value::String(value.clone()));
    }
    let mut agent = Dictionary::new();
    agent.insert("Label".to_owned(), Value::String(label.to_owned()));
    agent.insert(
        "ProgramArguments".to_owned(),
        Value::Array(vec![text(program), Value::String(HOST_ARG.to_owned())]),
    );
    agent.insert("Sockets".to_owned(), Value::Dictionary(sockets));
    agent.insert(
        "EnvironmentVariables".to_owned(),
        Value::Dictionary(variables),
    );
    agent.insert("WorkingDirectory".to_owned(), Value::String("/".to_owned()));
    agent.insert("StandardOutPath".to_owned(), text(&paths.host_log()));
    agent.insert("StandardErrorPath".to_owned(), text(&paths.host_log()));
    // Started only by a connection, or the CLI's kickstart: never at login
    // nor again once it ends (daemonless, D20).
    agent.insert("RunAtLoad".to_owned(), Value::Boolean(false));
    agent.insert("KeepAlive".to_owned(), Value::Boolean(false));
    agent.insert(
        "ExitTimeOut".to_owned(),
        Value::Integer(EXIT_TIMEOUT.into()),
    );
    agent.insert(
        "ProcessType".to_owned(),
        Value::String("Interactive".to_owned()),
    );
    Value::Dictionary(agent)
}

/// `gui/UID/LABEL`: the agent in the user's login session.
fn target(label: &str) -> String {
    format!("{}/{label}", domain())
}

/// `gui/UID`: the user's login session.
fn domain() -> String {
    format!("gui/{}", nix::unistd::getuid().as_raw())
}

/// What `launchctl` did: whether it worked, what it printed, what it said.
struct Said {
    worked: bool,
    out: String,
    err: String,
}

/// `launchctl ARGS`, its output kept.
fn launchctl(args: &[&str]) -> Result<Said, VmError> {
    let output = Command::new(LAUNCHCTL)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| VmError::io(Path::new(LAUNCHCTL), e))?;
    Ok(Said {
        worked: output.status.success(),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Whether launchd has the agent `label` loaded.
fn has(label: &str) -> bool {
    launchctl(&["print", &target(label)]).is_ok_and(|said| said.worked)
}

/// Whether the agent of the VM at `paths` is installed (its plist is
/// there), loaded or not.
pub(crate) fn installed(paths: &VmPaths) -> bool {
    plist_file(paths).is_ok_and(|file| file.is_file())
}

/// Whether the agent for `paths` is loaded in the user's session.
#[must_use]
pub fn loaded(paths: &VmPaths) -> bool {
    has(&label_of(paths))
}

/// The agent's job as launchd says it (`launchctl print`): whether it
/// runs, how often it has been started, how it last ended. `None` if it
/// isn't loaded, or launchd says it otherwise (its words aren't an
/// interface: the CLI only waits longer then).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Job {
    /// Its host runs now.
    pub(crate) running: bool,
    /// How many times launchd has started it.
    pub(crate) runs: u64,
    /// Its last exit code, once it has ended.
    pub(crate) last_exit: Option<i64>,
}

/// [`Job`] for `paths`.
pub(crate) fn job(paths: &VmPaths) -> Option<Job> {
    let said = launchctl(&["print", &target(&label_of(paths))]).ok()?;
    said.worked.then(|| job_from(&said.out)).flatten()
}

/// [`Job`] from `launchctl print`'s text.
fn job_from(printed: &str) -> Option<Job> {
    let field = |name: &str| {
        printed
            .lines()
            .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix(" = "))
    };
    Some(Job {
        running: field("state")? == "running",
        runs: field("runs")?.parse().ok()?,
        last_exit: field("last exit code").and_then(|code| code.parse().ok()),
    })
}

/// What `shipyard vm install` did: the agent's label and its file.
#[derive(Debug)]
pub struct Installed {
    /// The agent's label.
    pub label: String,
    /// Its property list.
    pub plist: PathBuf,
}

/// Writes the agent for `paths` and loads it (one loaded before taken out
/// first, and waited for: installing again updates it, a moved binary's
/// included). Refused while the VM runs: its host holds `docker.sock`
/// itself, where launchd would make its own. An agent that makes no
/// socket is taken out again.
///
/// # Errors
///
/// [`VmError::Host`] while the VM runs, or launchd's refusal;
/// [`VmError::Io`] and [`VmError::Place`] for the files.
pub fn install(paths: &VmPaths) -> Result<Installed, VmError> {
    if crate::client::status(paths)? != crate::client::Status::Stopped {
        return Err(VmError::Host(
            "the VM runs: stop it first (shipyard vm stop), then install".to_owned(),
        ));
    }
    paths.prepare(nix::unistd::getuid().as_raw())?;
    let program = this_binary().map_err(|e| VmError::Host(format!("this binary: {e}")))?;
    for (what, path) in [("shipyard", &program), ("the guest's files", &paths.guest)] {
        if let Some(folder) = protected(home().as_deref(), path) {
            return Err(VmError::Place(format!(
                "{what} ({}) is in your {folder} folder, which macOS lets launchd's \
                 programs read only with your permission: put it elsewhere (as \
                 /usr/local/bin), then install",
                path.display()
            )));
        }
    }
    let env: Vec<(String, String)> = std::env::var("SHIPYARD_VM_IDLE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| ("SHIPYARD_VM_IDLE".to_owned(), value))
        .into_iter()
        .collect();
    let label = label_of(paths);
    let file = plist_file(paths)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| VmError::io(dir, e))?;
    }
    let mut bytes = Vec::new();
    agent(&label, &program, paths, &env)
        .to_writer_xml(&mut bytes)
        .map_err(|e| VmError::io(&file, std::io::Error::other(e)))?;
    let how = shipyard_sys::fs::Write::new(0o644).durability(shipyard_sys::fs::Durability::Synced);
    shipyard_sys::fs::write_atomically(&file, &bytes, &how)
        .map_err(|e| VmError::io(&file, std::io::Error::from(e)))?;
    unload(&label)?;
    bootstrap(&file)?;
    // launchd makes the socket when its run at load checks in: a `docker`
    // right after this finds it there.
    let socket = paths.docker_socket();
    if !until(SOCKET_WAIT, || socket_there(&socket)) {
        let _ = unload(&label);
        let _ = std::fs::remove_file(&file);
        return Err(VmError::Host(format!(
            "launchd loaded {label} but made no socket at {} within {} s, so it's taken out \
             again: {}",
            socket.display(),
            SOCKET_WAIT.as_secs(),
            crate::client::last_lines(&paths.host_log(), 3)
        )));
    }
    Ok(Installed { label, plist: file })
}

/// `launchctl bootstrap` of `file`, tried again once a moment after a
/// refusal (launchd may still be letting an agent of that label go).
fn bootstrap(file: &Path) -> Result<(), VmError> {
    let file = file.to_string_lossy();
    let mut said = launchctl(&["bootstrap", &domain(), &file])?;
    if !said.worked {
        std::thread::sleep(Duration::from_secs(1));
        said = launchctl(&["bootstrap", &domain(), &file])?;
    }
    if said.worked {
        Ok(())
    } else {
        Err(VmError::Host(format!(
            "launchctl bootstrap {file}: {}",
            said.err
        )))
    }
}

/// The agent `label` unloaded, and waited for until launchd has let it go
/// (a host it started stops its guest first). `false` if it wasn't loaded.
fn unload(label: &str) -> Result<bool, VmError> {
    if !has(label) {
        return Ok(false);
    }
    let said = launchctl(&["bootout", &target(label)])?;
    if !until(UNLOAD_WAIT, || !has(label)) {
        return Err(VmError::Host(format!(
            "launchd still has {label} after {} s ({})",
            UNLOAD_WAIT.as_secs(),
            said.err
        )));
    }
    Ok(true)
}

/// What `shipyard vm uninstall` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uninstalled {
    /// There was an agent to take out.
    pub was_installed: bool,
    /// The VM ran, and was stopped (in order) as its agent went.
    pub stopped_the_vm: bool,
}

/// Takes the agent for `paths` away: unloaded (a host it started is asked
/// to stop, as `vm stop` does, and waited for), its file, its socket and
/// the CLI's start mark removed.
///
/// # Errors
///
/// [`VmError::Host`] if launchd doesn't let it go; [`VmError::Io`] and
/// [`VmError::Place`] for the files.
pub fn uninstall(paths: &VmPaths) -> Result<Uninstalled, VmError> {
    let running = crate::client::status(paths)? != crate::client::Status::Stopped;
    let unloaded = unload(&label_of(paths))?;
    let file = plist_file(paths)?;
    let removed = match std::fs::remove_file(&file) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(VmError::io(&file, error)),
    };
    let _ = std::fs::remove_file(paths.docker_socket());
    let _ = std::fs::remove_file(paths.start_asked());
    Ok(Uninstalled {
        was_installed: unloaded || removed,
        stopped_the_vm: unloaded && running,
    })
}

/// Whether `done` came true within `limit`, asked every 100 ms.
fn until(limit: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a socket is at `path`.
pub(crate) fn socket_there(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

/// Starts the agent's host now (the CLI's start, once it's loaded): no
/// second one if it runs already.
pub(crate) fn kickstart(paths: &VmPaths) -> Result<(), VmError> {
    let label = label_of(paths);
    let said = launchctl(&["kickstart", &target(&label)])?;
    if said.worked {
        Ok(())
    } else {
        Err(VmError::Host(format!(
            "launchctl kickstart {label}: {}",
            said.err
        )))
    }
}

/// Asks the agent's host to stop (SIGTERM: its guest stopped in order).
pub(crate) fn terminate(paths: &VmPaths) {
    let _ = launchctl(&["kill", "SIGTERM", &target(&label_of(paths))]);
}

/// The program the agent for `paths` runs, as its property list says, if
/// it's loaded.
#[must_use]
pub fn program(paths: &VmPaths) -> Option<PathBuf> {
    if !loaded(paths) {
        return None;
    }
    let agent = plist::Value::from_file(plist_file(paths).ok()?).ok()?;
    let program = agent
        .as_dictionary()?
        .get("ProgramArguments")?
        .as_array()?
        .first()?
        .as_string()?;
    Some(PathBuf::from(program))
}

unsafe extern "C" {
    /// launchd's `launch_activate_socket(3)`: the descriptors of the
    /// agent's socket `name`, in an array the caller frees. Neither std,
    /// nix, rustix nor libc binds it.
    fn launch_activate_socket(
        name: *const libc::c_char,
        fds: *mut *mut libc::c_int,
        count: *mut libc::size_t,
    ) -> libc::c_int;
}

/// Docker's socket as launchd holds it, if launchd started this process
/// as the agent: `None` if it didn't (`ESRCH`: not launchd's job; `ENOENT`:
/// no such socket in its agent).
///
/// # Errors
///
/// [`VmError::Host`] for any other refusal, or not exactly one socket.
pub(crate) fn activated_socket() -> Result<Option<UnixListener>, VmError> {
    let name = SOCKET_NAME;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut count: libc::size_t = 0;
    // SAFETY: `name` is a C string that lives through the call; `fds` and
    // `count` are valid places for launchd to write its array and its
    // length.
    let error = unsafe { launch_activate_socket(name.as_ptr(), &raw mut fds, &raw mut count) };
    socket_from(error, fds, count)
}

/// [`activated_socket`]'s answer from what launchd gave: its error, or
/// its array (taken over, then freed).
fn socket_from(
    error: libc::c_int,
    fds: *mut libc::c_int,
    count: libc::size_t,
) -> Result<Option<UnixListener>, VmError> {
    match error {
        0 => {}
        libc::ESRCH | libc::ENOENT => return Ok(None),
        other => {
            return Err(VmError::Host(format!(
                "launchd's socket: {}",
                std::io::Error::from_raw_os_error(other)
            )));
        }
    }
    let owned: Vec<OwnedFd> = (0..count)
        // SAFETY: launchd gave `count` descriptors in `fds`, each this
        // process's to own from now (taken once, here).
        .map(|i| unsafe { OwnedFd::from_raw_fd(*fds.add(i)) })
        .collect();
    // SAFETY: the array launchd allocated with `malloc`, freed once.
    unsafe { libc::free(fds.cast()) };
    let mut owned = owned.into_iter();
    match (owned.next(), owned.next()) {
        (Some(socket), None) => Ok(Some(UnixListener::from(socket))),
        _ => Err(VmError::Host(format!(
            "launchd's socket: {count} descriptors where one was expected"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn paths(dir: &str) -> VmPaths {
        VmPaths::at(PathBuf::from(dir), PathBuf::from("/g")).unwrap()
    }

    /// The default directory's agent is `dev.shipyard.vm`; another
    /// directory's has a label of its own, the same each time.
    #[test]
    fn each_vm_directory_has_its_own_label() {
        let home = Path::new("/Users/someone");
        let default = VmPaths::at(VmPaths::default_dir(home), PathBuf::from("/g")).unwrap();
        assert_eq!(label(&default, Some(home)), LABEL);
        let other = label(&paths("/tmp/vm-a"), Some(home));
        assert!(other.starts_with("dev.shipyard.vm."), "{other}");
        assert_eq!(other.len(), LABEL.len() + 1 + 12);
        assert_eq!(label(&paths("/tmp/vm-a"), Some(home)), other);
        assert_ne!(label(&paths("/tmp/vm-b"), Some(home)), other);
        assert_ne!(label(&default, None), LABEL);
        // One directory named two ways (the temporary one is under
        // `/var`, a link to `/private/var`) has one label.
        let temp = std::env::temp_dir();
        let canonical = std::fs::canonicalize(&temp).unwrap();
        assert_ne!(
            temp, canonical,
            "a temporary directory named through a link"
        );
        let named = |dir: PathBuf| VmPaths::at(dir, PathBuf::from("/g")).unwrap();
        assert_eq!(
            label(&named(temp), Some(home)),
            label(&named(canonical), Some(home))
        );
    }

    /// The agent runs this program as the host, holds `docker.sock` as the
    /// user's alone, is started with the VM's variables, logs to the
    /// host's log, and is started by nothing but a connection or the CLI.
    #[test]
    fn the_agent_says_what_runs_and_what_it_holds() {
        let paths = paths("/tmp/vm-agent");
        let env = vec![("SHIPYARD_VM_DIR".to_owned(), "/tmp/vm-agent".to_owned())];
        let agent = agent(
            "dev.shipyard.vm.x",
            Path::new("/bin/shipyard"),
            &paths,
            &env,
        );
        let mut bytes = Vec::new();
        agent.to_writer_xml(&mut bytes).unwrap();
        let read = plist::Value::from_reader_xml(bytes.as_slice()).unwrap();
        let read = read.as_dictionary().unwrap();
        assert_eq!(read["Label"].as_string(), Some("dev.shipyard.vm.x"));
        let args: Vec<_> = read["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_string().unwrap())
            .collect();
        assert_eq!(args, ["/bin/shipyard", HOST_ARG]);
        let socket = read["Sockets"].as_dictionary().unwrap()["docker"]
            .as_dictionary()
            .unwrap();
        assert_eq!(
            socket["SockPathName"].as_string(),
            Some("/tmp/vm-agent/docker.sock")
        );
        assert_eq!(socket["SockPathMode"].as_signed_integer(), Some(0o600));
        assert_eq!(
            read["EnvironmentVariables"].as_dictionary().unwrap()["SHIPYARD_VM_DIR"].as_string(),
            Some("/tmp/vm-agent")
        );
        assert_eq!(
            read["StandardErrorPath"].as_string(),
            Some("/tmp/vm-agent/host.log")
        );
        assert_eq!(read["RunAtLoad"].as_boolean(), Some(false));
        assert_eq!(read["KeepAlive"].as_boolean(), Some(false));
        // Its stop is the host's whole orderly stop (90 s, then 10 s).
        assert!(read["ExitTimeOut"].as_unsigned_integer().unwrap() > 100);
        // The VM's places always, the guest's too: the host's are the CLI's.
        let variables = read["EnvironmentVariables"].as_dictionary().unwrap();
        assert_eq!(variables["SHIPYARD_GUEST_DIR"].as_string(), Some("/g"));
    }

    /// A program or the guest's files in a folder macOS guards (or below
    /// it) is said with the folder's name; one elsewhere, or without a
    /// home, isn't.
    #[test]
    fn guarded_folders_are_named() {
        let home = Path::new("/Users/someone");
        let at = |path: &str| protected(Some(home), Path::new(path));
        assert_eq!(
            at("/Users/someone/Desktop/shipyard/target/debug/shipyard"),
            Some("Desktop")
        );
        assert_eq!(at("/Users/someone/Documents/x"), Some("Documents"));
        assert_eq!(at("/Users/someone/Downloads/shipyard"), Some("Downloads"));
        assert_eq!(
            at("/Users/someone/Library/Mobile Documents/com~apple~CloudDocs/s"),
            Some("iCloud Drive")
        );
        assert_eq!(at("/Users/someone/DesktopX/shipyard"), None);
        assert_eq!(at("/usr/local/bin/shipyard"), None);
        assert_eq!(protected(None, Path::new("/Users/someone/Desktop/s")), None);
    }

    /// The job as `launchctl print` says it: running, its runs, its last
    /// exit (none before its first end); text it doesn't recognise is none.
    #[test]
    fn launchds_job_is_read() {
        let printed = "gui/501/dev.shipyard.vm = {\n\tstate = not running\n\truns = 3\n\
                       \tlast exit code = 1\n}\n";
        assert_eq!(
            job_from(printed),
            Some(Job {
                running: false,
                runs: 3,
                last_exit: Some(1)
            })
        );
        let first = "\tstate = running\n\truns = 1\n\tlast exit code = (never exited)\n";
        assert_eq!(
            job_from(first),
            Some(Job {
                running: true,
                runs: 1,
                last_exit: None
            })
        );
        assert_eq!(job_from("something else"), None);
    }

    /// launchd's "not mine" (`ESRCH`, not its job; `ENOENT`, no such
    /// socket) is none: the host binds its own. Another error is said;
    /// one socket is taken, two refused.
    #[test]
    fn launchds_answer_is_read() {
        let none = std::ptr::null_mut();
        assert!(matches!(socket_from(libc::ESRCH, none, 0), Ok(None)));
        assert!(matches!(socket_from(libc::ENOENT, none, 0), Ok(None)));
        assert!(matches!(
            socket_from(libc::EPERM, none, 0),
            Err(VmError::Host(_))
        ));
        let held = |n: usize| {
            // SAFETY: an array of `n` descriptors made as launchd makes it.
            let fds = unsafe { libc::malloc(n * size_of::<libc::c_int>()) }.cast::<libc::c_int>();
            for i in 0..n {
                let (socket, _) = std::os::unix::net::UnixStream::pair().unwrap();
                // SAFETY: inside the array just made.
                unsafe { *fds.add(i) = std::os::fd::IntoRawFd::into_raw_fd(socket) };
            }
            fds
        };
        assert!(matches!(socket_from(0, held(1), 1), Ok(Some(_))));
        assert!(matches!(socket_from(0, held(2), 2), Err(VmError::Host(_))));
    }
}
