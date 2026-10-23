//! The VM's host process, `__shipyard-vm` (step 10.3a): it holds the VM for
//! as long as it runs. It takes the VM directory's lock first (one host per
//! user; liveness is the lock), boots the guest, waits for its agent, then
//! serves its socket: each connection, from the same user only, is joined
//! to the agent over vsock, bytes copied both ways. SIGTERM, SIGINT or
//! SIGHUP stop it in order (the guest asked to stop its containers and
//! power off), and a guest that stops by itself ends it.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
use shipyard_runtime::attach::forward::{self, AGENT_PORT, STOP_REQUEST};

use shipyard_runtime::attach::Frame;

use crate::forwarding::Forwarding;
use crate::idle::{Clients, Idle};
use crate::machine::{Machine, Settings, State};
use crate::{Guest, VmError, VmPaths};

/// The data disk's size: a sparse file, so only what the guest writes
/// takes room on the Mac.
const DISK_SIZE: u64 = 64 << 30;

/// The guest's memory (stage 13 makes it give back what it doesn't use).
const MEMORY: u64 = 4 << 30;

/// The console log's size past which it's moved aside at the next start.
const CONSOLE_KEPT: u64 = 1 << 20;

/// How long the guest has to boot to its agent (the client waits longer:
/// `client::START_LIMIT`).
pub(crate) const BOOT_LIMIT: Duration = Duration::from_secs(90);

/// How long `accept` rests after a failure (out of descriptors): it would
/// otherwise be ready again at once, forever.
pub(crate) const ACCEPT_REST: Duration = Duration::from_millis(100);

/// How long the guest has to stop its containers and power off.
const STOP_LIMIT: Duration = Duration::from_secs(90);

/// The host's run: never returns.
pub(crate) fn run() -> ! {
    let code = match serve() {
        Ok(()) => 0,
        Err(error) => {
            log(&format!("{error}"));
            1
        }
    };
    std::process::exit(code)
}

/// A line in the host's log (its stderr: `host.log`, as the CLI starts it).
pub(crate) fn log(line: &str) {
    let _ = writeln!(std::io::stderr(), "shipyard-vm: {line}");
}

fn serve() -> Result<(), VmError> {
    // launchd's socket first, if launchd started this host (step 11.5b2a:
    // the check-in makes it): every failure after is said to the clients
    // waiting on it, or launchd would start the host again and again for
    // them while they hang.
    let launchd = crate::launchd::activated_socket()?.map(Arc::new);
    let served = served_from(launchd.clone());
    if let (Err(error), Some(docker)) = (&served, &launchd) {
        refuse_waiting(docker, &error.to_string());
    }
    served
}

/// [`serve`], once launchd's socket (if any) is had.
fn served_from(launchd: Option<Arc<UnixListener>>) -> Result<(), VmError> {
    let paths = VmPaths::from_env()?;
    let uid = nix::unistd::getuid().as_raw();
    paths.prepare(uid)?;
    if let Some(docker) = &launchd
        && !asked(&paths, docker, std::time::SystemTime::now())
    {
        // launchd's own run at load: the socket now made (its check-in),
        // nothing to serve until a connection comes.
        log("launchd's socket made: nothing asked, so nothing started");
        return Ok(());
    }
    let Some(_lock) = take_lock(&paths)? else {
        log("another host holds the VM: nothing to do");
        return match launchd {
            Some(_) => Err(VmError::Host(
                "another host runs this VM (one started without launchd)".to_owned(),
            )),
            None => Ok(()),
        };
    };
    // The CLI's mark taken (again: one written as this host started), so
    // none outlives this host to boot the VM at the next login.
    let _ = std::fs::remove_file(paths.start_asked());
    // Docker's socket: launchd's, left to it at the end, or the host's
    // own, bound before the boot (a client waits in its backlog) and
    // removed at the end.
    // Its own only without launchd's agent: one installed holds it (this
    // host started without it, `launchctl print` failing for a moment,
    // would otherwise take the agent's socket away for good: 11.6b's
    // review). Then it isn't served here: its clients go to launchd's.
    let own = launchd.is_none() && !crate::launchd::installed(&paths);
    let docker = match launchd {
        Some(docker) => {
            log("Docker's socket is launchd's");
            Some(docker)
        }
        None if !own => {
            log("Docker's socket is launchd's agent's, which isn't running: left to it");
            None
        }
        None => Some(Arc::new(crate::serving::bind_private(
            &paths.docker_socket(),
        )?)),
    };
    let served = boot_and_serve(&paths, uid, docker.as_ref(), own);
    if own && let Some(docker) = &docker {
        if let Err(error) = &served {
            refuse_waiting(docker, &error.to_string());
        }
        let _ = std::fs::remove_file(paths.docker_socket());
    }
    let _ = std::fs::remove_file(paths.start_asked());
    served
}

/// How old a CLI's start mark may be and still count: its start's own
/// bound. An older one (its CLI killed, its kick refused) asks nothing.
const MARK_FRESH: Duration = Duration::from_secs(150);

/// Whether a host launchd started (at `now`) was asked for: by the CLI (a
/// fresh mark, taken here) or by a connection waiting on `docker`
/// (launchd's start for it: queued already, seen within a second).
/// Neither: launchd ran it as it loaded its agent, to make the socket.
fn asked(paths: &VmPaths, docker: &UnixListener, now: std::time::SystemTime) -> bool {
    use std::os::fd::AsFd as _;
    let mark = paths.start_asked();
    let fresh = std::fs::metadata(&mark)
        .and_then(|meta| meta.modified())
        .is_ok_and(|made| now.duration_since(made).unwrap_or_default() <= MARK_FRESH);
    if std::fs::remove_file(&mark).is_ok() && fresh {
        return true;
    }
    let mut waiting = [nix::poll::PollFd::new(
        docker.as_fd(),
        nix::poll::PollFlags::POLLIN,
    )];
    nix::poll::poll(&mut waiting, nix::poll::PollTimeout::from(1000_u16)).is_ok_and(|n| n > 0)
}

/// Answers every client of the user's waiting on `docker` with Docker's
/// error, `why` (none waited for, none written to for more than a moment):
/// a host that won't serve them lets them go.
fn refuse_waiting(docker: &UnixListener, why: &str) {
    if docker.set_nonblocking(true).is_err() {
        return;
    }
    let uid = nix::unistd::getuid().as_raw();
    while let Ok((client, _)) = docker.accept() {
        if !crate::peer::peer_uid(&client).is_some_and(|peer| crate::same_user(peer, uid)) {
            continue;
        }
        let _ = client.set_nonblocking(false);
        let _ = client.set_write_timeout(Some(Duration::from_secs(1)));
        crate::api::refuse(client, why);
    }
}

/// Boots the guest and serves both sockets until the host is asked to
/// stop, or the guest stops by itself. `own_docker`: the host made
/// Docker's socket (else it's launchd's, never removed here); `docker`
/// none: it isn't served here (launchd's agent, not running, holds it).
fn boot_and_serve(
    paths: &VmPaths,
    uid: u32,
    docker: Option<&Arc<UnixListener>>,
    own_docker: bool,
) -> Result<(), VmError> {
    // The signals that stop it, blocked in every thread from here (the
    // main thread waits for them).
    let stops = stop_signals();
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&stops), None)
        .map_err(|e| VmError::Host(format!("blocking signals: {e}")))?;
    raise_descriptor_limit();
    let guest = Guest::find(&paths.guest)?;
    guest.verify()?;
    log(&format!(
        "booting the guest (Alpine {}, kernel {}, shipyard {})",
        guest.manifest.alpine, guest.manifest.kernel.version, guest.manifest.shipyard.revision
    ));
    let disk = make_disk(paths)?;
    let machine = Arc::new(Machine::new(settings(paths, &guest, &disk)?)?);
    machine.start(Duration::from_secs(30))?;
    wait_for_agent(&machine, paths)?;
    // From here the guest runs: a failure stops it in order (its
    // containers, its disk), never by the host's end alone.
    let listener = match keep_disk(paths, &disk)
        .and_then(|()| crate::serving::bind_private(&paths.socket()))
    {
        Ok(listener) => listener,
        Err(error) => {
            log(&format!("{error}: stopping the guest"));
            stop_guest(&machine);
            return Err(error);
        }
    };
    log("the guest is up");
    // A command's end wakes the ports' poller: a `run -d -p` just made
    // is forwarded at once.
    let (ended, wake) = std::sync::mpsc::channel();
    let clients = Arc::new(Clients::new());
    let idle = Idle::new(idle_limit());
    let forwarding = Forwarding::start(
        Arc::clone(&machine),
        paths.ports(),
        wake,
        (Arc::clone(&clients), idle),
    );
    let stopping = Arc::new(AtomicBool::new(false));
    // (launchd's Docker socket is its own: never removed here.)
    let sockets: Vec<PathBuf> = std::iter::once(paths.socket())
        .chain(own_docker.then(|| paths.docker_socket()))
        .collect();
    watch(&machine, &stopping, sockets, &forwarding);
    let serving = Arc::new(crate::serving::Serving {
        machine: Arc::clone(&machine),
        uid,
        clients: Arc::clone(&clients),
        ended,
    });
    if let Some(docker) = docker {
        let (theirs, api) = (Arc::clone(&serving), Arc::clone(docker));
        std::thread::spawn(move || {
            crate::serving::accept(&api, &crate::api::SERVED, &theirs, !own_docker);
        });
    }
    std::thread::spawn(move || crate::serving::accept(&listener, &CLI_SERVED, &serving, false));
    let signal = stops
        .wait()
        .map_err(|e| VmError::Host(format!("waiting: {e}")))?;
    // From here the stop is this thread's: the watcher leaves it be, and
    // a client still to be taken is refused (its CLI starts the VM again
    // once it's down), as at an idle stop.
    stopping.store(true, Ordering::SeqCst);
    clients.stop();
    log(&format!("{signal:?}: stopping the guest"));
    // (Tests only, in a debug build: the window in which a client is
    // refused, held open for a command to be sent into.)
    if cfg!(debug_assertions)
        && let Some(hold) = std::env::var("SHIPYARD_VM_TEST_STOP_HOLD")
            .ok()
            .and_then(|ms| ms.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(hold));
    }
    let _ = std::fs::remove_file(paths.socket());
    forwarding.stop();
    stop_guest(&machine);
    Ok(())
}

/// Watches for a guest that stops by itself (it powered off, or failed):
/// then nothing more can be served, and the host ends. A stop the host
/// asked for is its main thread's to see through.
fn watch(
    machine: &Arc<Machine>,
    stopping: &Arc<AtomicBool>,
    sockets: Vec<PathBuf>,
    forwarding: &Arc<Forwarding>,
) {
    let machine = Arc::clone(machine);
    let stopping = Arc::clone(stopping);
    let forwarding = Arc::clone(forwarding);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            if machine.state() != State::Up {
                break;
            }
        }
        for socket in &sockets {
            let _ = std::fs::remove_file(socket);
        }
        forwarding.stop();
        log("the guest stopped by itself (its console log says why)");
        std::process::exit(1);
    });
}

/// The soft descriptor limit raised to the hard one: every client holds
/// several (its socket, the agent's, their copies), as does every
/// forwarded connection, and macOS's soft limit is 256. The limit had is
/// logged: a hard one lower than 10,240 leaves less room.
fn raise_descriptor_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    if let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) {
        // (macOS refuses past OPEN_MAX for an unlimited hard limit; a soft
        // limit already higher is kept, never lowered.)
        let wanted = soft.max(hard.min(10_240));
        if wanted > soft {
            let _ = setrlimit(Resource::RLIMIT_NOFILE, wanted, hard);
        }
    }
    if let Ok((soft, _)) = getrlimit(Resource::RLIMIT_NOFILE) {
        log(&format!("descriptors: up to {soft}"));
    }
}

/// The idle limit (`SHIPYARD_VM_IDLE`, D10.3-11), a wrong one said.
fn idle_limit() -> Duration {
    let setting = std::env::var("SHIPYARD_VM_IDLE").ok();
    crate::idle::idle_limit(setting.as_deref()).unwrap_or_else(|default| {
        log(&format!(
            "SHIPYARD_VM_IDLE={setting:?} isn't whole seconds (at least 1): {} s it is",
            default.as_secs()
        ));
        default
    })
}

/// SIGTERM (`vm stop`, a logout, the idle stop), SIGINT and SIGHUP.
fn stop_signals() -> SigSet {
    let mut set = SigSet::empty();
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        set.add(signal);
    }
    set
}

/// The VM directory's lock, taken without waiting (`None`: another host
/// has it), with this process's PID written in it for `vm stop`.
fn take_lock(paths: &VmPaths) -> Result<Option<std::fs::File>, VmError> {
    let path = paths.lock();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| VmError::io(&path, e))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(VmError::io(&path, e)),
    }
    file.set_len(0).map_err(|e| VmError::io(&path, e))?;
    writeln!(file, "{}", std::process::id()).map_err(|e| VmError::io(&path, e))?;
    Ok(Some(file))
}

/// The data disk to boot with: the kept one, else a new one, made empty
/// and sparse as `data.img.new` (a new one left by a first boot cut short
/// is made again: its guest's format may be half done).
fn make_disk(paths: &VmPaths) -> Result<PathBuf, VmError> {
    let kept = paths.disk();
    if kept.exists() {
        // (One made before backups were left out of is, now.)
        exclude_from_backups(&kept);
        return Ok(kept);
    }
    let new = paths.new_disk();
    let _ = std::fs::remove_file(&new);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&new)
        .map_err(|e| VmError::io(&new, e))?;
    file.set_len(DISK_SIZE).map_err(|e| VmError::io(&new, e))?;
    exclude_from_backups(&new);
    Ok(new)
}

/// `disk` left out of Time Machine's backups (as `tmutil addexclusion`
/// marks it, an attribute that follows the file through its rename): a
/// changed disk image is copied whole at each backup, many GiB an hour.
/// Best effort: said in the log if it can't be.
fn exclude_from_backups(disk: &std::path::Path) {
    let excluded = std::process::Command::new("/usr/bin/tmutil")
        .arg("addexclusion")
        .arg(disk)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !excluded.is_ok_and(|status| status.success()) {
        log(&format!(
            "{} isn't left out of Time Machine's backups (tmutil addexclusion failed)",
            disk.display()
        ));
    }
}

/// A new disk kept once a guest has booted with it (formatted, and
/// mounted: its agent runs only after).
fn keep_disk(paths: &VmPaths, disk: &std::path::Path) -> Result<(), VmError> {
    if disk == paths.new_disk() {
        std::fs::rename(disk, paths.disk()).map_err(|e| VmError::io(disk, e))?;
    }
    Ok(())
}

/// What the VM is made with: the guest, the disk, the home, the console.
fn settings(paths: &VmPaths, guest: &Guest, disk: &std::path::Path) -> Result<Settings, VmError> {
    // Canonical: the CLI compares canonical paths with it, and the guest
    // mounts it at this path.
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| {
            VmError::Place("HOME isn't an absolute path: nothing to share".to_owned())
        })?;
    let home = std::fs::canonicalize(&home).map_err(|e| VmError::io(&home, e))?;
    // The VM's own files, if under the home: canonical, as the home is.
    let own: Vec<PathBuf> = [&paths.dir, &paths.guest]
        .into_iter()
        .filter_map(|place| std::fs::canonicalize(place).ok())
        .collect();
    let own: Vec<&std::path::Path> = own.iter().map(PathBuf::as_path).collect();
    let cmdline = cmdline(&home.to_string_lossy(), &own)?;
    let console = console_log(paths)?;
    let cpus = diagnosis_override("SHIPYARD_VM_CPUS").map_or_else(
        || std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4)),
        |cpus| usize::try_from(cpus).unwrap_or(1).clamp(1, 16),
    );
    let memory = diagnosis_override("SHIPYARD_VM_MEMORY_MIB")
        .map_or(MEMORY, |mib| mib.clamp(1024, 64 * 1024) << 20);
    let mac = crate::network::NetworkFile::kept_or_made(&paths.network())?;
    // Said, so a diagnosis override left in a shell is seen in the log.
    let overridden = ["SHIPYARD_VM_CPUS", "SHIPYARD_VM_MEMORY_MIB"]
        .into_iter()
        .filter(|name| diagnosis_override(name).is_some())
        .collect::<Vec<_>>();
    log(&format!(
        "{cpus} CPUs, {} MiB, network card {mac}{}",
        memory >> 20,
        if overridden.is_empty() {
            String::new()
        } else {
            format!(" (overridden: {})", overridden.join(", "))
        }
    ));
    Ok(Settings {
        kernel: guest.kernel.clone(),
        initramfs: guest.initramfs.clone(),
        cmdline,
        disk: disk.to_owned(),
        home,
        console,
        mac,
        cpus,
        memory,
    })
}

/// A number the guest's settings are overridden with, for diagnosing the
/// guest (`SHIPYARD_VM_CPUS`, `SHIPYARD_VM_MEMORY_MIB`: F132's harness,
/// `guest-crash-rate.sh`); never needed otherwise. One that isn't a number
/// is ignored.
fn diagnosis_override(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

/// The kernel's command line: the console, the home's path (where the
/// guest mounts it), and the places in it the guest hides from what runs
/// in it (`hidden`, those under the home: the VM's own files). A home with
/// a space, or a character the command line can't carry, is refused, and
/// so is `/` (the guest's own files would be the Mac's).
pub(crate) fn cmdline(home: &str, hidden: &[&std::path::Path]) -> Result<String, VmError> {
    if home.is_empty() || !home.chars().all(|c| c.is_ascii_graphic()) {
        return Err(VmError::Place(format!(
            "the home {home:?} has a character the guest's command line can't carry"
        )));
    }
    if home == "/" {
        return Err(VmError::Place(
            "the home is /: sharing it would lay the Mac's files over the guest's".to_owned(),
        ));
    }
    let under: Vec<&str> = hidden
        .iter()
        .filter(|place| place.starts_with(home))
        .filter_map(|place| place.to_str())
        .collect();
    let mut line = format!("console=hvc0 {}{home}", forward::HOME_KEY);
    if !under.is_empty() {
        line = format!(
            "{line} {}{}",
            forward::HIDE_KEY,
            forward::hidden_value(&under)
        );
    }
    Ok(line)
}

/// The console log, opened to append (moved aside first once past
/// [`CONSOLE_KEPT`]).
fn console_log(paths: &VmPaths) -> Result<std::fs::File, VmError> {
    let path = paths.console_log();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CONSOLE_KEPT) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| VmError::io(&path, e))
}

/// Waits for the guest's agent to take a connection.
fn wait_for_agent(machine: &Machine, paths: &VmPaths) -> Result<(), VmError> {
    let deadline = Instant::now() + BOOT_LIMIT;
    loop {
        if machine.connect(AGENT_PORT, Duration::from_secs(2)).is_ok() {
            // (The agent sees this probe end at once, and goes on.)
            return Ok(());
        }
        if machine.state() != State::Up || Instant::now() > deadline {
            machine.force_stop(Duration::from_secs(10));
            return Err(VmError::Machine(format!(
                "the guest didn't come up within {BOOT_LIMIT:?}: its console log says why ({})",
                paths.console_log().display()
            )));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The CLI's connections (`vm.sock`): each joined to the guest's agent,
/// refused as the forwarding wire says.
const CLI_SERVED: crate::serving::Served = crate::serving::Served {
    what: "a client",
    refuse: |client, refusal| {
        // Said by its code to a client that reads one (F99): its hello read
        // first, for its version (a moment's wait at most; an older one's
        // words if it says none).
        let _ = client.set_read_timeout(Some(Duration::from_secs(1)));
        let mut connection = forward::Connection::new(client);
        let version = match connection.next_frame() {
            Ok(Some(Frame::HelloRun { version, .. })) => version,
            _ => 0,
        };
        let (mut client, _) = connection.into_parts();
        let _ = forward::send(&mut client, &refusal.frame(version));
    },
    serve: |machine, client| match machine.connect(AGENT_PORT, Duration::from_secs(10)) {
        Ok(agent) => join(client, agent),
        Err(error) => log(&format!("a client not served: {error}")),
    },
};

/// Copies bytes both ways between `client` and the agent, each direction's
/// end passed on as a half-close (the other side's reads end), until both
/// have ended (or, one ended, the other has lingered too long).
fn join(client: UnixStream, agent: OwnedFd) {
    if let Err(error) = shipyard_sys::relay::both_ways(client.into(), agent, CLIENT_LINGER) {
        log(&format!("a client not served: {error}"));
    }
}

/// How long a client whose command's answer has ended may keep its side
/// open: a CLI ends at once; one that doesn't is cut.
const CLIENT_LINGER: Duration = Duration::from_secs(60);

/// The guest stopped in order: asked through its agent (its containers
/// stopped, then a power-off), forced past [`STOP_LIMIT`].
fn stop_guest(machine: &Machine) {
    let asked = crate::forwarding::request(machine, &[STOP_REQUEST]);
    if let Err(error) = asked {
        log(&format!("{error}: stopped at once"));
        machine.force_stop(Duration::from_secs(10));
        return;
    }
    let deadline = Instant::now() + STOP_LIMIT;
    while machine.state() == State::Up && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    if machine.state() == State::Up {
        log(&format!(
            "the guest still runs after {STOP_LIMIT:?}: stopped at once"
        ));
        machine.force_stop(Duration::from_secs(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal goes as the client reads it: a version 5 client gets
    /// its code, a version 4 one the words, one that says no hello (within
    /// the second) the words too.
    #[test]
    fn a_refusal_is_said_as_the_client_reads_it() {
        use shipyard_runtime::attach::{DECLINE_VERSION, Decline};
        let request = forward::Request::internal(&["ps"]).encode().unwrap();
        for version in [Some(DECLINE_VERSION), Some(DECLINE_VERSION - 1), None] {
            let (mut client, server) = UnixStream::pair().unwrap();
            if let Some(version) = version {
                let hello = Frame::HelloRun {
                    version,
                    request: request.clone(),
                };
                forward::send(&mut client, &hello).unwrap();
            }
            (CLI_SERVED.refuse)(server, &forward::Refusal::Stopping);
            let said = forward::Connection::new(client).next_frame().unwrap();
            let expected = if version == Some(DECLINE_VERSION) {
                Frame::Declined {
                    reason: Decline::Stopping,
                    why: forward::STOPPING.to_owned(),
                }
            } else {
                Frame::Refused(forward::STOPPING.to_owned())
            };
            assert_eq!(said, Some(expected), "{version:?}");
        }
    }

    /// launchd's start of the host is asked for by a fresh mark of the
    /// CLI's (taken), or a connection waiting; a stale mark (its CLI
    /// killed, before a reboot) asks nothing and is taken all the same,
    /// and no mark with nothing waiting is launchd's run at load.
    #[test]
    fn a_launchd_start_is_asked_by_a_fresh_mark_or_a_connection() {
        let dir = std::env::temp_dir().join(format!("shipyard-asked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = VmPaths::at(dir.clone(), dir.join("guest")).unwrap();
        let docker = UnixListener::bind(dir.join("d.sock")).unwrap();
        let now = std::time::SystemTime::now();
        assert!(!asked(&paths, &docker, now), "nothing asked");
        std::fs::write(paths.start_asked(), b"").unwrap();
        assert!(asked(&paths, &docker, now), "a fresh mark");
        assert!(!paths.start_asked().exists(), "the mark taken");
        std::fs::write(paths.start_asked(), b"").unwrap();
        let later = now + MARK_FRESH + Duration::from_secs(60);
        assert!(!asked(&paths, &docker, later), "a stale mark");
        assert!(!paths.start_asked().exists(), "a stale mark taken too");
        let _waiting = UnixStream::connect(dir.join("d.sock")).unwrap();
        assert!(asked(&paths, &docker, now), "a connection waiting");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The kernel's command line carries the home; a home it can't carry
    /// (a space, a control character) is refused.
    #[test]
    fn the_command_line_carries_the_home() {
        assert_eq!(
            cmdline("/Users/me", &[]).unwrap_or_default(),
            "console=hvc0 shipyard.home=/Users/me"
        );
        for home in ["/Users/my name", "/Users/me\n", "", "/"] {
            assert!(
                matches!(cmdline(home, &[]), Err(VmError::Place(_))),
                "{home:?}"
            );
        }
        // The VM's own files are hidden if under the home, not elsewhere.
        let vm = std::path::Path::new("/Users/me/Library/Application Support/Shipyard/vm");
        let line = cmdline("/Users/me", &[vm, std::path::Path::new("/tmp/vm")]).unwrap();
        assert_eq!(
            forward::hidden_of(&line),
            ["/Users/me/Library/Application Support/Shipyard/vm"]
        );
    }
}
