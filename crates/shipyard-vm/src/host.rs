//! The VM's host process, `__shipyard-vm` (step 10.3a): it holds the VM for
//! as long as it runs. It takes the VM directory's lock first (one host per
//! user; liveness is the lock), boots the guest, waits for its agent, then
//! serves its socket: each connection, from the same user only, is joined
//! to the agent over vsock, bytes copied both ways. A stop asked on its own
//! socket (`host.sock`, `control.rs`: what `vm stop` does), or a SIGTERM,
//! SIGINT or SIGHUP, stops it in order (the guest asked to stop its
//! containers and power off), and a guest that stops by itself ends it.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shipyard_runtime::attach::forward::{self, AGENT_PORT, STOP_REQUEST};

use shipyard_runtime::attach::Frame;

use crate::control::{Phase, Stops};
use crate::forwarding::Forwarding;
use crate::idle::{Clients, Idle};
use crate::machine::{Machine, Settings, State};
use crate::phase::HostPhase;
use crate::{Guest, VmError, VmPaths};

/// The data disk's size: a sparse file, so only what the guest writes
/// takes room on the Mac.
const DISK_SIZE: u64 = 64 << 30;

/// The console log's size past which it's moved aside at the next start.
const CONSOLE_KEPT: u64 = 1 << 20;

/// The console log's size past which it's moved aside while the guest runs
/// (F90: a guest that logs without end would fill the Mac's disk).
const CONSOLE_MOST: u64 = 4 << 20;

/// How often the console log's size is looked at while the guest runs.
const CONSOLE_EVERY: Duration = Duration::from_secs(60);

/// How long the guest has to boot to its agent (the client waits longer:
/// `client::START_LIMIT`).
pub(crate) const BOOT_LIMIT: Duration = Duration::from_secs(90);

/// How long `accept` rests after a failure (out of descriptors): it would
/// otherwise be ready again at once, forever.
pub(crate) const ACCEPT_REST: Duration = Duration::from_millis(100);

/// How long the guest has to stop its containers and power off (the
/// bound a CLI waits by, too).
const STOP_LIMIT: Duration = crate::phase::STOP_BOUND;

/// The most the stop's last look at the guest's ports may take (its ask's
/// connect and answer waits).
const LAST_LOOK: Duration = Duration::from_secs(40);

/// How long a guest past its stop's bound has to end once forced.
const FORCED_STOP: Duration = Duration::from_secs(10);

/// The longest an ordered stop may take (13.6b1's review): asked as the VM
/// boots, it waits for the boot, then for a disk check no stop cuts, then
/// for its containers' stop, then the force. launchd's `ExitTimeOut` is
/// past it.
pub(crate) const LONGEST_STOP: Duration = Duration::from_secs(
    MACHINE_START.as_secs()
        + BOOT_LIMIT.as_secs()
        + REBOOTED_STOP_LIMIT.as_secs()
        + STOP_LIMIT.as_secs()
        + LAST_LOOK.as_secs()
        + FORCED_STOP.as_secs(),
);

/// How long the host has to prepare its guest (its disk, its settings)
/// before starting it.
const PREPARE_LIMIT: Duration = Duration::from_secs(60);

/// How long the machine has to start (before its guest's boot).
const MACHINE_START: Duration = Duration::from_secs(30);

/// How long a guest that rebooted in place has to stop: its init holds the
/// stop until its data disk's check (a repair, after a panic) is done, and
/// a repair cut short leaves the disk worse (13.2's Murphy review).
const REBOOTED_STOP_LIMIT: Duration = Duration::from_secs(600);

/// The host's run: never returns.
pub(crate) fn run() -> ! {
    let code = match serve() {
        Ok(Ended::Served) => 0,
        // (A CLI waiting for this start says it was stopped, never starts
        // another: 13.6b1's review.)
        Ok(Ended::StoppedBeforeServing) => crate::phase::STOPPED_BEFORE_SERVING,
        Err(error) => {
            log(&format!("{error}"));
            1
        }
    };
    std::process::exit(code)
}

/// How a host's run ended well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// It served until stopped, or had nothing to do.
    Served,
    /// A stop came before it served anyone (as it prepared or booted).
    StoppedBeforeServing,
}

/// A line in the host's log (its stderr: `host.log`, as the CLI starts it).
pub(crate) fn log(line: &str) {
    let _ = writeln!(std::io::stderr(), "shipyard-vm: {line}");
}

fn serve() -> Result<Ended, VmError> {
    // The stop signals first, before any other thread: none ends the host
    // before its stop runs (13.6b1).
    let stops = Stops::watched()?;
    // launchd's socket first, if launchd started this host (step 11.5b2a:
    // the check-in makes it): every failure after is said to the clients
    // waiting on it, or launchd would start the host again and again for
    // them while they hang.
    let launchd = crate::launchd::activated_socket()?.map(Arc::new);
    let served = served_from(launchd.clone(), &stops);
    if let (Err(error), Some(docker)) = (&served, &launchd) {
        refuse_waiting(docker, &error.to_string());
    }
    served
}

/// [`serve`], once launchd's socket (if any) is had.
fn served_from(launchd: Option<Arc<UnixListener>>, stops: &Arc<Stops>) -> Result<Ended, VmError> {
    let paths = VmPaths::from_env()?;
    let uid = nix::unistd::getuid().as_raw();
    paths.prepare(uid)?;
    if let Some(docker) = &launchd
        && !asked(
            &paths,
            docker,
            std::time::SystemTime::now(),
            crate::comes_back::at_login(
                std::env::var_os(crate::comes_back::AT_LOGIN_VAR).as_deref(),
            ),
        )
    {
        // launchd's own run at load: the socket now made (its check-in),
        // nothing to serve until a connection comes.
        log("launchd's socket made: nothing asked, so nothing started");
        return Ok(Ended::Served);
    }
    let Some(mut lock) = take_lock(&paths)? else {
        log("another host holds the VM: nothing to do");
        return match launchd {
            Some(_) => Err(VmError::Host(
                "another host runs this VM (one started without launchd)".to_owned(),
            )),
            None => Ok(Ended::Served),
        };
    };
    // Its own socket, then its PID: a CLI that finds a PID and no socket
    // has met an older host (13.6b1's review).
    let phase = Phase::preparing(PREPARE_LIMIT);
    let host_socket = crate::serving::bind_private(&paths.host_socket())?;
    crate::control::serve(host_socket, uid, Arc::clone(&phase), Arc::clone(stops));
    write_pid(&mut lock, &paths)?;
    crate::control::keep_log_small(&paths.host_log());
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
    let control = Control {
        stops: Arc::clone(stops),
        phase,
    };
    let served = boot_and_serve(&paths, uid, (docker.as_ref(), own), &control);
    if own && let Some(docker) = &docker {
        if let Err(error) = &served {
            refuse_waiting(docker, &error.to_string());
        }
        let _ = std::fs::remove_file(paths.docker_socket());
    }
    let _ = std::fs::remove_file(paths.start_asked());
    let _ = std::fs::remove_file(paths.host_socket());
    served
}

/// What the host's main thread steers by: the stops asked, its phase.
struct Control {
    stops: Arc<Stops>,
    phase: Arc<Phase>,
}

/// How long a guest in `phase` has to stop in order: one checking its disk,
/// as long as a repair may take (PID 1 holds the stop till it's done),
/// whoever asks (a user, the idle rule, the liveness watch).
fn stop_limit(phase: HostPhase) -> Duration {
    if phase == HostPhase::Checking {
        REBOOTED_STOP_LIMIT
    } else {
        STOP_LIMIT
    }
}

/// No client let in from here (a stop's): each is refused as stopping, and
/// `vm_socket`, given, removed, so a CLI starts the next VM rather than
/// wait on this one.
fn close_doors(clients: &Clients, vm_socket: Option<&std::path::Path>) {
    clients.stop();
    if let Some(socket) = vm_socket {
        let _ = std::fs::remove_file(socket);
    }
}

/// Why the liveness watch stops the guest, for its `verdict`.
fn why_stopped(verdict: &crate::liveness::Verdict) -> &'static str {
    if *verdict == crate::liveness::Verdict::Rebooted {
        "the guest rebooted in place (a kernel panic? its console log says)"
    } else {
        "the guest's agent hasn't answered six asks in a row (hung?)"
    }
}

/// How long the liveness watch's stop of a guest found `verdict` in
/// `phase` waits for it in order: a rebooted one may be checking its disk
/// again, a hung one may be still checking it (the agent answers late on a
/// Mac short of memory): neither is cut at [`STOP_LIMIT`].
fn watched_stop_limit(verdict: &crate::liveness::Verdict, phase: HostPhase) -> Duration {
    if *verdict == crate::liveness::Verdict::Rebooted {
        REBOOTED_STOP_LIMIT
    } else {
        stop_limit(phase)
    }
}

/// The guest stopped in order, within its phase's bound ([`stop_limit`]),
/// the host's phase said as stopping meanwhile (a CLI asking then waits by
/// it, and a late "ready" never moves it back).
fn stop_in_order(machine: &Machine, phase: &Phase) {
    let limit = stop_limit(phase.get());
    phase.set(HostPhase::Stopping, limit + FORCED_STOP);
    stop_guest(machine, limit);
}

/// How old a CLI's start mark may be and still count: its start's own
/// bound. An older one (its CLI killed, its kick refused) asks nothing.
const MARK_FRESH: Duration = Duration::from_secs(150);

/// Whether a host launchd started (at `now`) was asked for: by the CLI (a
/// fresh mark, taken here), by a connection waiting on `docker` (launchd's
/// start for it: queued already, seen within a second), or by the login
/// (`vm install --at-login`: its run at load, while containers would come
/// back, F106). Neither: launchd ran it as it loaded its agent, to make the
/// socket; a run at load `vm install` caused (its own fresh mark) never
/// boots the VM.
fn asked(
    paths: &VmPaths,
    docker: &UnixListener,
    now: std::time::SystemTime,
    at_login: bool,
) -> bool {
    use std::os::fd::AsFd as _;
    if taken_fresh(&paths.start_asked(), now) {
        return true;
    }
    let just_installed = taken_fresh(&paths.just_installed(), now);
    let mut waiting = [nix::poll::PollFd::new(
        docker.as_fd(),
        nix::poll::PollFlags::POLLIN,
    )];
    if nix::poll::poll(&mut waiting, nix::poll::PollTimeout::from(1000_u16)).is_ok_and(|n| n > 0) {
        return true;
    }
    if just_installed || !at_login {
        return false;
    }
    match crate::comes_back::ComesBack::read(&paths.comes_back()) {
        Ok(Some(back)) if back.containers > 0 => {
            log(&format!(
                "at login: {}: starting the VM",
                crate::comes_back::would_come_back(back.containers)
            ));
            true
        }
        Ok(_) => false,
        Err(error) => {
            log(&format!("at login: {error}: the VM isn't started"));
            false
        }
    }
}

/// Whether the mark at `path` was there, fresh as of `now` (within
/// [`MARK_FRESH`]): taken either way.
fn taken_fresh(path: &std::path::Path, now: std::time::SystemTime) -> bool {
    let fresh = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|made| now.duration_since(made).unwrap_or_default() <= MARK_FRESH);
    std::fs::remove_file(path).is_ok() && fresh
}

/// Answers every client of the user's waiting on `docker` with Docker's
/// error, `why` (none waited for, none written to for more than a moment):
/// a host that won't serve them lets them go.
fn refuse_waiting(docker: &UnixListener, why: &str) {
    use std::os::fd::AsFd as _;
    let uid = nix::unistd::getuid().as_raw();
    // Each one there now (a poll of 0 ms, then its accept): the listener's
    // own flags untouched, as launchd hands the same one to the next host
    // (F147).
    let waiting = || {
        let mut fds = [nix::poll::PollFd::new(
            docker.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO).is_ok_and(|n| n > 0)
    };
    while waiting()
        && let Ok(client) = crate::serving::accepted(docker)
    {
        if !crate::peer::peer_uid(&client).is_some_and(|peer| crate::same_user(peer, uid)) {
            continue;
        }
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
    (docker, own_docker): (Option<&Arc<UnixListener>>, bool),
    control: &Control,
) -> Result<Ended, VmError> {
    raise_descriptor_limit();
    // (The CLI checks first; a host launchd started hasn't been checked.)
    if let Some(why) = crate::doctor::this_macs_floor_refusal() {
        return Err(VmError::Unsupported(why));
    }
    let Some(Booted {
        machine,
        listener,
        new_disk,
    }) = boot(paths, control)?
    else {
        return Ok(Ended::StoppedBeforeServing);
    };
    // A command's end wakes the ports' poller: a `run -d -p` just made
    // is forwarded at once.
    let (ended, wake) = std::sync::mpsc::channel();
    let clients = Arc::new(Clients::new());
    let idle = Idle::new(idle_limit());
    let budget = Arc::new(crate::serving::Budget::default());
    let forwarding = Forwarding::start(
        Arc::clone(&machine),
        (paths.ports(), paths.comes_back()),
        wake,
        ((Arc::clone(&clients), idle), Arc::clone(&budget)),
    );
    // A new disk is kept at the guest's first answer too: the ready watch
    // may have moved on without one (another build's refusal), and the
    // liveness watch's stop asks for nothing.
    if let Some(new_disk) = new_disk.clone() {
        forwarding.at_next_answer(move || {
            if let Err(error) = new_disk.keep() {
                log(&format!(
                    "{error}: the new data disk is made again at the next start"
                ));
            }
        });
    }
    let stopping = Arc::new(AtomicBool::new(false));
    // (launchd's Docker socket is its own: never removed here.)
    // What an end of the host's own leaves behind otherwise.
    let leftovers: Vec<PathBuf> = [paths.socket(), paths.host_socket(), paths.booted()]
        .into_iter()
        .chain(own_docker.then(|| paths.docker_socket()))
        .collect();
    watch(&machine, &stopping, leftovers.clone(), &forwarding);
    keep_alive_checked(
        (&machine, &control.phase, &clients),
        (&stopping, paths.socket()),
        leftovers,
        &forwarding,
    );
    keep_console_small(paths.console_log(), &stopping);
    let serving = Arc::new(crate::serving::Serving {
        machine: Arc::clone(&machine),
        uid,
        clients: Arc::clone(&clients),
        ended,
        budget,
    });
    // A stop asked since the boot's last look: nothing let in only to be cut.
    if control.stops.asked().is_some() {
        clients.stop();
    }
    if let Some(docker) = docker {
        let (theirs, api) = (Arc::clone(&serving), Arc::clone(docker));
        std::thread::spawn(move || {
            crate::serving::accept(&api, &crate::api::SERVED, &theirs, !own_docker);
        });
    }
    std::thread::spawn(move || crate::serving::accept(&listener, &CLI_SERVED, &serving, false));
    let why = control.stops.wait();
    // From here the stop is this thread's: the watcher leaves it be, and
    // a client still to be taken is refused (its CLI starts the VM again
    // once it's down), as at an idle stop.
    if stopping.swap(true, Ordering::SeqCst) {
        // The liveness watch is stopping a guest that rebooted or hung, and
        // ends the host itself once it's down.
        loop {
            std::thread::park();
        }
    }
    clients.stop();
    log(&format!("{why}: stopping the guest"));
    // (Tests only, in a debug build: the window in which a client is
    // refused, held open for a command to be sent into.)
    if cfg!(debug_assertions)
        && let Some(hold) = std::env::var("SHIPYARD_VM_TEST_STOP_HOLD")
            .ok()
            .and_then(|ms| ms.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(hold));
    }
    close_doors(&clients, Some(&paths.socket()));
    // Said as stopping before the last look (its ask may wait on a wedged
    // agent: 13.6b2's review), the stop's bound counted from then.
    let limit = stop_limit(control.phase.get());
    control
        .phase
        .set(HostPhase::Stopping, LAST_LOOK + limit + FORCED_STOP);
    // The last look keeps a new disk not kept yet if the guest answers (a
    // command ran in the moment between its ready mark and the watch's
    // next ask); one that fails the ask (an older guest's) ran it, so it's
    // ready too.
    if !forwarding.last_look()
        && let Some(new_disk) = &new_disk
        && is_ready(&ready_answer(&machine))
        && let Err(error) = new_disk.keep()
    {
        log(&format!(
            "{error}: the new data disk is made again at the next start"
        ));
    }
    forwarding.stop();
    stop_guest(&machine, limit);
    let _ = std::fs::remove_file(paths.booted());
    Ok(Ended::Served)
}

/// What [`boot`] gives.
struct Booted {
    machine: Arc<Machine>,
    /// `vm.sock`, to serve.
    listener: UnixListener,
    /// The disk made at this start, if it was (kept once the guest is
    /// ready).
    new_disk: Option<Arc<NewDisk>>,
}

/// The guest booted to its agent, `vm.sock` bound, what booted kept: its
/// machine and the socket to serve; `None` if a stop came first (taken
/// before the guest starts, else once its agent answers, before a client
/// is let in: none admitted only to be cut).
fn boot(paths: &VmPaths, control: &Control) -> Result<Option<Booted>, VmError> {
    let guest = Guest::find(&paths.guest)?;
    guest.verify()?;
    log(&format!(
        "booting the guest (Alpine {}, kernel {}, shipyard {})",
        guest.manifest.alpine, guest.manifest.kernel.version, guest.manifest.shipyard.revision
    ));
    let disk = make_disk(paths)?;
    let new_disk = NewDisk::of(paths, &disk);
    let machine = Arc::new(Machine::new(settings(paths, &guest, &disk)?)?);
    if let Some(why) = control.stops.asked() {
        log(&format!("{why}: stopped before its guest started"));
        return Ok(None);
    }
    control
        .phase
        .set(HostPhase::Booting, MACHINE_START + BOOT_LIMIT);
    machine.start(MACHINE_START)?;
    wait_for_agent(&machine, paths)?;
    // From here the guest runs: a failure stops it in order (its
    // containers, its disk), never by the host's end alone. Until it's
    // ready, it's checking its disk: no stop cuts that short.
    control.phase.set(HostPhase::Checking, REBOOTED_STOP_LIMIT);
    let ready = {
        let (new_disk, stops) = (new_disk.clone(), Arc::clone(&control.stops));
        move || {
            if let Some(new_disk) = new_disk
                && let Err(error) = new_disk.keep()
            {
                log(&format!("{error}: stopping the guest"));
                stops.ask("the new data disk can't be kept");
            }
        }
    };
    let asked = Arc::clone(&machine);
    watch_ready(
        move || ready_answer(&asked),
        Arc::clone(&control.phase),
        ready,
        READY_ASK_EVERY,
    );
    if let Some(why) = control.stops.asked() {
        log(&format!(
            "{why}: stopping the guest, once its agent answers"
        ));
        stop_in_order(&machine, &control.phase);
        return Ok(None);
    }
    let listener = match crate::serving::bind_private(&paths.socket()) {
        Ok(listener) => listener,
        Err(error) => {
            log(&format!("{error}: stopping the guest"));
            stop_in_order(&machine, &control.phase);
            return Err(error);
        }
    };
    log("the guest is up");
    // What booted, for a CLI of another build to tell (F173b); stale once
    // this host is gone, and read only while it runs.
    let booted = crate::booted::Booted::of(std::process::id(), &guest.manifest.shipyard)
        .serving(&paths.guest, std::env::var_os("HOME").as_deref());
    // (The home as `settings` shares it: canonical, from the same `HOME`.)
    if let Err(error) = booted.write(&paths.booted()) {
        log(&format!("what booted isn't kept: {error}"));
    }
    Ok(Some(Booted {
        machine,
        listener,
        new_disk,
    }))
}

/// How often [`watch_ready`] asks.
const READY_ASK_EVERY: Duration = Duration::from_secs(1);

/// Whether `answer` (to [`ready_answer`]) says the guest is ready: it
/// answered, or ran the ask and failed it (an older guest's); the agent
/// runs nothing before its disk is formatted and mounted.
fn is_ready(answer: &Result<(), crate::forwarding::Unanswered>) -> bool {
    matches!(
        answer,
        Ok(()) | Err(crate::forwarding::Unanswered::Failed(_))
    )
}

/// The guest asked for its ports: its answer, or why there's none.
fn ready_answer(machine: &Machine) -> Result<(), crate::forwarding::Unanswered> {
    use crate::forwarding::Unanswered;
    let mut connection = crate::forwarding::request(machine, &[forward::PORTS_REQUEST])
        .map_err(Unanswered::Broken)?;
    crate::forwarding::output(&mut connection, forward::MAX_PORTS_ANSWER).map(drop)
}

/// The host's phase moved from checking to up once the guest no longer
/// refuses as not ready (its disk checked), `ask`ed every `every`; a phase
/// moved on meanwhile (a stop) is left be. `ready` is done once the guest
/// answers, or runs the ask and fails it (an older guest that doesn't know
/// it): the agent runs nothing before its disk is formatted and mounted
/// (PID 1 marks it ready only then). Never on a refusal: one that comes
/// before the agent's ready check (another build's, a hello it can't read)
/// says nothing of the disk.
fn watch_ready(
    ask: impl Fn() -> Result<(), crate::forwarding::Unanswered> + Send + 'static,
    phase: Arc<Phase>,
    ready: impl FnOnce() + Send + 'static,
    every: Duration,
) -> std::thread::JoinHandle<()> {
    use crate::forwarding::Unanswered;
    std::thread::spawn(move || {
        while phase.get() == HostPhase::Checking {
            let answer = ask();
            match answer {
                _ if is_ready(&answer) => {
                    ready();
                    phase.advance(HostPhase::Checking, HostPhase::Up, Duration::ZERO);
                    return;
                }
                // Still checking, or a failed ask: asked again.
                Err(Unanswered::Refused(forward::Refusal::NotReady) | Unanswered::Broken(_)) => {}
                // Up, as it serves no better later; but not ready (a new disk
                // is kept at its first answer for its ports, if one comes).
                Ok(()) | Err(_) => {
                    log(&format!(
                        "the guest's readiness unanswered ({}): a new disk is kept once it answers",
                        answer
                            .as_ref()
                            .err()
                            .map_or_else(String::new, ToString::to_string)
                    ));
                    phase.advance(HostPhase::Checking, HostPhase::Up, Duration::ZERO);
                    return;
                }
            }
            std::thread::sleep(every);
        }
    })
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
        end_host(
            &sockets,
            &forwarding,
            "the guest stopped by itself (its console log says why)",
        );
    });
}

/// Ends the host once its guest is down without its asking: nothing more
/// can be served.
fn end_host(sockets: &[PathBuf], forwarding: &Forwarding, why: &str) -> ! {
    for socket in sockets {
        let _ = std::fs::remove_file(socket);
    }
    forwarding.stop();
    log(why);
    std::process::exit(1);
}

/// Asks the guest its boot ID every [`liveness::EVERY`] (F90): a guest that
/// rebooted in place (a panic: `panic=5`) or stopped answering is stopped, and
/// the watch then ends the host as for a guest that stopped by itself (the
/// next command boots a fresh one). An older guest that doesn't know the ask
/// is left unasked, said once.
///
/// [`liveness::EVERY`]: crate::liveness::EVERY
fn keep_alive_checked(
    (machine, phase, clients): (&Arc<Machine>, &Arc<Phase>, &Arc<Clients>),
    (stopping, vm_socket): (&Arc<AtomicBool>, PathBuf),
    sockets: Vec<PathBuf>,
    forwarding: &Arc<Forwarding>,
) {
    use crate::liveness::{EVERY, Liveness, Verdict};
    let machine = Arc::clone(machine);
    let phase = Arc::clone(phase);
    let clients = Arc::clone(clients);
    let stopping = Arc::clone(stopping);
    let forwarding = Arc::clone(forwarding);
    std::thread::spawn(move || {
        let mut liveness = Liveness::default();
        loop {
            std::thread::sleep(EVERY);
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            let (why, limit) = match liveness.heard(crate::forwarding::ask_boot(&machine)) {
                Verdict::Up => continue,
                Verdict::Off => {
                    log(
                        "the guest doesn't say its boot ID (an older guest): not watched for a hang",
                    );
                    return;
                }
                verdict @ (Verdict::Rebooted | Verdict::Hung) => (
                    why_stopped(&verdict),
                    watched_stop_limit(&verdict, phase.get()),
                ),
            };
            // The stop is this thread's from here, or a user's already: never
            // both (a hard stop of ours mustn't cut an ordered one short).
            if stopping.swap(true, Ordering::SeqCst) {
                return;
            }
            log(&format!("{why}: stopping the VM"));
            // In order: a rebooted guest's init holds it until its disk's
            // check is done, as a hung one's does while the host still sees
            // it checking (stage 13's close: never a repair cut at 90 s). A
            // hung agent is waited for the whole bound: PID 1 holds its
            // port, so the stop's ask is taken even with no agent behind it.
            phase.set(HostPhase::Stopping, limit + FORCED_STOP);
            // As a user's stop does: no client let in to a guest being
            // stopped (refused, its CLI starts the next VM once this one's
            // down), the CLI's socket gone (the whole-stage review).
            close_doors(&clients, Some(&vm_socket));
            stop_guest(&machine, limit);
            end_host(&sockets, &forwarding, &format!("{why}: the VM is stopped"));
        }
    });
}

/// Keeps the console log under [`CONSOLE_MOST`] while the guest runs: past
/// it, the log is copied to `console.log.1` and emptied. The framework writes
/// it through a descriptor of its own opened to append, so its next write
/// lands at the start of the emptied file; a line it writes between the copy
/// and the emptying is lost.
fn keep_console_small(console: PathBuf, stopping: &Arc<AtomicBool>) {
    let stopping = Arc::clone(stopping);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(CONSOLE_EVERY);
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            if std::fs::metadata(&console).is_ok_and(|m| m.len() > CONSOLE_MOST)
                && let Err(error) = move_console_aside(&console)
            {
                log(&format!("the console log grew past its bound: {error}"));
            }
        }
    });
}

/// The console log at `console`: its last [`CONSOLE_MOST`] bytes (a guest
/// that logs fast may have written far more since the last look) copied to
/// `console.log.1`, then the log emptied in place.
fn move_console_aside(console: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _};
    let older = console.with_extension("log.1");
    let mut file = std::fs::File::open(console)?;
    let len = file.metadata()?.len();
    file.seek(std::io::SeekFrom::Start(len.saturating_sub(CONSOLE_MOST)))?;
    let mut bytes = Vec::new();
    file.take(CONSOLE_MOST).read_to_end(&mut bytes)?;
    let how = shipyard_sys::fs::Write::new(0o600);
    shipyard_sys::fs::write_atomically(&older, &bytes, &how).map_err(std::io::Error::from)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(console)?
        .set_len(0)
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

/// How many more times the VM's lock is tried, 20 ms apart, before it's
/// taken to be another host's.
const LOCK_TRIES: u32 = 10;

/// The VM directory's lock, taken without waiting on another host (`None`:
/// one has it; a moment's shared hold of a `vm status` is waited out); its
/// PID is written once the host's own socket is bound ([`write_pid`]).
fn take_lock(paths: &VmPaths) -> Result<Option<std::fs::File>, VmError> {
    let path = paths.lock();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| VmError::io(&path, e))?;
    // A `vm status` holds it shared for a moment (`client::status`'s
    // probe): tried again for 200 ms before another host is taken to run,
    // as a real one holds it for its whole life (the whole-stage review: a
    // host launchd started refused every `docker` client waiting for it).
    let mut tries = 0;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if tries < LOCK_TRIES => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(VmError::io(&path, e)),
        }
    }
    file.set_len(0).map_err(|e| VmError::io(&path, e))?;
    Ok(Some(file))
}

/// This process's PID written in its lock `file` (`vm status` says it; an
/// older `vm stop` signals it).
fn write_pid(file: &mut std::fs::File, paths: &VmPaths) -> Result<(), VmError> {
    writeln!(file, "{}", std::process::id()).map_err(|e| VmError::io(&paths.lock(), e))
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
    let excluded = crate::child::output_within(
        std::process::Command::new("/usr/bin/tmutil")
            .arg("addexclusion")
            .arg(disk),
        TMUTIL_LIMIT,
    );
    if !excluded.is_ok_and(|output| output.is_some_and(|output| output.status.success())) {
        log(&format!(
            "{} isn't left out of Time Machine's backups (tmutil addexclusion failed)",
            disk.display()
        ));
    }
}

/// How long `tmutil` may take (a start's preparation is bounded: 13.6b1).
const TMUTIL_LIMIT: Duration = Duration::from_secs(10);

/// A disk made at this start (`data.img.new`), kept as `data.img` once its
/// guest is ready, never before: PID 1 starts its agent before it formats
/// the disk (D10.4-4), so a host gone while it formats (killed, the Mac's
/// power lost) leaves `data.img.new`, which the next start makes anew,
/// never a half-made `data.img` every later boot refuses (13.8c).
struct NewDisk {
    new: PathBuf,
    kept: PathBuf,
    done: std::sync::Mutex<bool>,
}

impl NewDisk {
    /// `disk`, if it's the one made at this start.
    fn of(paths: &VmPaths, disk: &std::path::Path) -> Option<Arc<Self>> {
        (disk == paths.new_disk()).then(|| {
            Arc::new(Self {
                new: paths.new_disk(),
                kept: paths.disk(),
                done: std::sync::Mutex::new(false),
            })
        })
    }

    /// Renamed to the kept disk's name, once.
    fn keep(&self) -> Result<(), VmError> {
        let mut done = self
            .done
            .lock()
            .map_err(|_| VmError::io(&self.new, std::io::Error::other("a keeper panicked")))?;
        if !*done {
            std::fs::rename(&self.new, &self.kept).map_err(|e| VmError::io(&self.new, e))?;
            *done = true;
        }
        Ok(())
    }
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
    // Rosetta's share, where the Mac has Rosetta for Linux: the guest then
    // runs amd64 images through it (step 13.3), told by its command line.
    let rosetta = crate::machine::rosetta() == crate::machine::Rosetta::Installed;
    if !rosetta {
        log("this Mac has no Rosetta for Linux: amd64 images won't run in the VM");
    }
    // The places shared beyond the home (step 13.4): each a device of its
    // own, named on the command line in order.
    // Their names are text (`MAC_SHARES`' own): each device's tag is its
    // place in the line's list, so none may be left out of one alone.
    let others: Vec<PathBuf> = crate::shares::all(&home)
        .into_iter()
        .skip(1)
        .filter(|place| place.to_str().is_some())
        .collect();
    let named: Vec<&str> = others.iter().filter_map(|place| place.to_str()).collect();
    log(&format!(
        "shared with the guest: {}",
        std::iter::once(home.to_string_lossy().as_ref())
            .chain(named.iter().copied())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let cmdline = cmdline(&home.to_string_lossy(), &named, &own, rosetta)?;
    let console = console_log(paths)?;
    // The size: `vm set`'s (`vm.json`), else the default for this Mac;
    // a diagnosis override above both.
    // An unreadable `vm.json` (corrupt, or a newer Shipyard's) is said and
    // the default taken: the VM still starts (`vm set` writes a new one).
    let set = crate::settings::VmSettings::read(&paths.settings()).unwrap_or_else(|error| {
        log(&format!("{error}: the default size is used"));
        None
    });
    let this_mac = crate::settings::this_mac()?;
    let size = crate::settings::resolve(set.as_ref(), this_mac);
    if let Some(set) = &set
        && (set.memory_mib.is_some_and(|mib| mib != size.memory_mib.0)
            || set.cpus.is_some_and(|cpus| cpus != size.cpus.0))
    {
        log(
            "vm.json asks for more than this Mac can give (another Mac's?): kept within its bounds",
        );
    }
    let cpus = diagnosis_override("SHIPYARD_VM_CPUS").map_or_else(
        || usize::try_from(size.cpus.0).unwrap_or(1),
        |cpus| usize::try_from(cpus).unwrap_or(1).clamp(1, 16),
    );
    let memory = diagnosis_override("SHIPYARD_VM_MEMORY_MIB")
        .map_or(size.memory_mib.0 << 20, |mib| {
            mib.clamp(1024, 64 * 1024) << 20
        });
    let card = crate::network::NetworkFile::kept_or_made(&paths.network(), &paths.dir)?;
    if card.remade {
        log(
            "network.json was made for another VM directory (this one's a copy?): its card has \
             an address of its own now",
        );
    }
    let mac = card.mac;
    // Said, so a diagnosis override left in a shell is seen in the log, and
    // what `vm set` asked for is told from the default.
    let from = |name: &str, source: crate::settings::Source| {
        if diagnosis_override(name).is_some() {
            format!(" ({name})")
        } else if source == crate::settings::Source::Set {
            " (vm set)".to_owned()
        } else {
            String::new()
        }
    };
    log(&format!(
        "{cpus} CPUs{}, {} MiB{}, network card {mac}",
        from("SHIPYARD_VM_CPUS", size.cpus.1),
        memory >> 20,
        from("SHIPYARD_VM_MEMORY_MIB", size.memory_mib.1),
    ));
    Ok(Settings {
        kernel: guest.kernel.clone(),
        initramfs: guest.initramfs.clone(),
        cmdline,
        disk: disk.to_owned(),
        home,
        others,
        console,
        mac,
        cpus,
        memory,
        rosetta,
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
/// guest mounts it), the places shared beyond it (`others`, each mounted
/// at its path), and the places in any of them the guest hides from what
/// runs in it (`hidden`: the VM's own files). A home with a space, or a
/// character the command line can't carry, is refused, and so is `/` (the
/// guest's own files would be the Mac's). With `rosetta`, the word that
/// says Rosetta's share is given; it and the shares come before the
/// hidden places (the longest part: a line cut short loses them, not the
/// rest).
pub(crate) fn cmdline(
    home: &str,
    others: &[&str],
    hidden: &[&std::path::Path],
    rosetta: bool,
) -> Result<String, VmError> {
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
        .filter(|place| {
            std::iter::once(home)
                .chain(others.iter().copied())
                .any(|shared| place.starts_with(shared))
        })
        .filter_map(|place| place.to_str())
        .collect();
    // `panic=5`: a guest whose kernel panics reboots, which the host's
    // watch sees (another boot ID: F90), rather than hanging for good.
    let mut line = format!("console=hvc0 panic=5 {}{home}", forward::HOME_KEY);
    if rosetta {
        line = format!("{line} {}", forward::ROSETTA_WORD);
    }
    if !others.is_empty() {
        line = format!(
            "{line} {}{}",
            forward::SHARES_KEY,
            forward::places_value(others)
        );
    }
    if !under.is_empty() {
        line = format!(
            "{line} {}{}",
            forward::HIDE_KEY,
            forward::places_value(&under)
        );
    }
    // Past the kernel's bound the line is cut, and the hidden places with
    // it: refused, never booted without them.
    if line.len() >= COMMAND_LINE_SIZE {
        return Err(VmError::Place(format!(
            "the guest's command line would be {} bytes, past the kernel's {COMMAND_LINE_SIZE} \
             (the VM's directory or home has too long a path)",
            line.len()
        )));
    }
    Ok(line)
}

/// The most an arm64 kernel's command line holds (`COMMAND_LINE_SIZE`),
/// its last byte the end.
const COMMAND_LINE_SIZE: usize = 2048;

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

/// Waits for the guest's agent to answer: its boot ID asked (an answer of
/// any kind, a refusal or an older guest's failure, is an agent's). A
/// connection alone isn't one: the guest's PID 1 holds the agent's port
/// from its boot (F108), so the kernel takes a connection with no agent
/// behind it.
fn wait_for_agent(machine: &Machine, paths: &VmPaths) -> Result<(), VmError> {
    use crate::liveness::Heard;
    let deadline = Instant::now() + BOOT_LIMIT;
    loop {
        if machine.state() == State::Up
            && !matches!(crate::forwarding::ask_boot(machine), Heard::Nothing)
        {
            return Ok(());
        }
        if machine.state() != State::Up || Instant::now() > deadline {
            machine.force_stop(FORCED_STOP);
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
/// stopped, then a power-off), forced past `limit` (its phase's: a disk
/// check's is [`REBOOTED_STOP_LIMIT`], which bounds any `Checking` phase's
/// stop, not only a rebooted guest's).
fn stop_guest(machine: &Machine, limit: Duration) {
    let asked = crate::forwarding::request(machine, &[STOP_REQUEST]);
    if let Err(error) = asked {
        log(&format!("{error}: stopped at once"));
        machine.force_stop(FORCED_STOP);
        return;
    }
    let deadline = Instant::now() + limit;
    while machine.state() == State::Up && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    if machine.state() == State::Up {
        log(&format!(
            "the guest still runs after {limit:?}: stopped at once"
        ));
        machine.force_stop(FORCED_STOP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest asked as `answers` say, in turn (the last for good): how
    /// many asks it took, whether `ready` was done, and the phase after.
    fn watched(
        answers: Vec<Result<(), crate::forwarding::Unanswered>>,
    ) -> (usize, bool, HostPhase) {
        use std::sync::Mutex;
        let phase = Phase::preparing(Duration::ZERO);
        phase.set(HostPhase::Checking, Duration::ZERO);
        let left = Arc::new(Mutex::new(answers));
        let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let (counted, marked) = (Arc::clone(&asks), Arc::clone(&done));
        watch_ready(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let mut left = left.lock().unwrap();
                if left.len() > 1 {
                    left.remove(0)
                } else {
                    left[0].clone()
                }
            },
            Arc::clone(&phase),
            move || marked.store(true, Ordering::SeqCst),
            Duration::from_millis(1),
        )
        .join()
        .unwrap();
        (
            asks.load(Ordering::SeqCst),
            done.load(Ordering::SeqCst),
            phase.get(),
        )
    }

    /// A new disk is kept once its guest answers, never while it refuses as
    /// still checking (formatting its disk) or before an ask gets through;
    /// a refusal of another kind moves the phase on and keeps nothing
    /// (13.8c).
    #[test]
    fn a_new_disk_is_kept_only_once_ready() {
        use crate::forwarding::Unanswered;
        let not_ready = || Err(Unanswered::Refused(forward::Refusal::NotReady));
        let broken = || Err(Unanswered::Broken("no answer".to_owned()));
        assert_eq!(
            watched(vec![not_ready(), broken(), not_ready(), Ok(())]),
            (4, true, HostPhase::Up)
        );
        assert_eq!(
            watched(vec![not_ready(), Err(Unanswered::Failed(1))]),
            (2, true, HostPhase::Up),
            "an older guest's failure: it ran the ask, so it's ready"
        );
        assert_eq!(
            watched(vec![
                not_ready(),
                Err(Unanswered::Refused(forward::Refusal::Newer))
            ]),
            (2, false, HostPhase::Up),
            "another build's refusal: up, but nothing said of the disk"
        );
    }

    /// The disk made at this start is renamed once, however often it's
    /// kept; the one kept before isn't the new one.
    #[test]
    fn a_new_disk_is_renamed_once() {
        let dir = std::env::temp_dir().join(format!("shipyard-new-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = VmPaths::at(dir.clone(), dir.join("guest")).unwrap();
        assert!(NewDisk::of(&paths, &paths.disk()).is_none());
        std::fs::write(paths.new_disk(), b"disk").unwrap();
        let new = NewDisk::of(&paths, &paths.new_disk()).unwrap();
        new.keep().unwrap();
        new.keep().unwrap();
        assert_eq!(std::fs::read(paths.disk()).unwrap(), b"disk");
        assert!(!paths.new_disk().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A hung guest still checking its disk gets a repair's bound, one up
    /// the ordinary one; a rebooted one a repair's always.
    #[test]
    fn a_watched_stop_never_cuts_a_disk_check() {
        use crate::liveness::Verdict;
        assert_eq!(
            watched_stop_limit(&Verdict::Hung, HostPhase::Checking),
            REBOOTED_STOP_LIMIT
        );
        assert_eq!(
            watched_stop_limit(&Verdict::Hung, HostPhase::Up),
            STOP_LIMIT
        );
        for phase in [HostPhase::Checking, HostPhase::Up] {
            assert_eq!(
                watched_stop_limit(&Verdict::Rebooted, phase),
                REBOOTED_STOP_LIMIT
            );
        }
        assert!(REBOOTED_STOP_LIMIT > STOP_LIMIT, "the control");
    }

    /// Past its bound the console log is moved aside whole and emptied in
    /// place; the framework's descriptor, open to append, writes on at the
    /// emptied file's start.
    #[test]
    fn a_full_console_is_moved_aside_and_written_on() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("shipyard-console-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let console = dir.join("console.log");
        let mut framework = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&console)
            .unwrap();
        framework.write_all(b"before\n").unwrap();
        move_console_aside(&console).unwrap();
        assert_eq!(
            std::fs::read(dir.join("console.log.1")).unwrap(),
            b"before\n"
        );
        assert_eq!(std::fs::metadata(&console).unwrap().len(), 0);
        framework.write_all(b"after\n").unwrap();
        assert_eq!(std::fs::read(&console).unwrap(), b"after\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
        assert!(!asked(&paths, &docker, now, false), "nothing asked");
        std::fs::write(paths.start_asked(), b"").unwrap();
        assert!(asked(&paths, &docker, now, false), "a fresh mark");
        assert!(!paths.start_asked().exists(), "the mark taken");
        std::fs::write(paths.start_asked(), b"").unwrap();
        let later = now + MARK_FRESH + Duration::from_secs(60);
        assert!(!asked(&paths, &docker, later, false), "a stale mark");
        assert!(!paths.start_asked().exists(), "a stale mark taken too");
        // At login (`--at-login`): only while containers would come back,
        // never for `vm install`'s own run, never from an unreadable count.
        assert!(
            !asked(&paths, &docker, now, true),
            "nothing comes back: none known"
        );
        crate::comes_back::ComesBack::of(0)
            .write(&paths.comes_back())
            .unwrap();
        assert!(!asked(&paths, &docker, now, true), "nothing comes back");
        crate::comes_back::ComesBack::of(2)
            .write(&paths.comes_back())
            .unwrap();
        assert!(asked(&paths, &docker, now, true), "two come back");
        assert!(
            !asked(&paths, &docker, now, false),
            "not asked for at login"
        );
        std::fs::write(paths.just_installed(), b"").unwrap();
        assert!(!asked(&paths, &docker, now, true), "the install's own run");
        assert!(!paths.just_installed().exists(), "the install's mark taken");
        std::fs::write(paths.comes_back(), b"not json").unwrap();
        assert!(!asked(&paths, &docker, now, true), "an unreadable count");
        let _waiting = UnixStream::connect(dir.join("d.sock")).unwrap();
        assert!(asked(&paths, &docker, now, false), "a connection waiting");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The kernel's command line carries the home; a home it can't carry
    /// (a space, a control character) is refused.
    #[test]
    fn the_command_line_carries_the_home() {
        assert_eq!(
            cmdline("/Users/me", &[], &[], false).unwrap_or_default(),
            "console=hvc0 panic=5 shipyard.home=/Users/me"
        );
        let long = format!("/Users/{}", "m".repeat(COMMAND_LINE_SIZE));
        for home in ["/Users/my name", "/Users/me\n", "", "/", &long] {
            assert!(
                matches!(cmdline(home, &[], &[], false), Err(VmError::Place(_))),
                "{home:?}"
            );
        }
        // The VM's own files are hidden if under a share, not elsewhere.
        let vm = std::path::Path::new("/Users/me/Library/Application Support/Shipyard/vm");
        let test = std::path::Path::new("/private/var/folders/x/T/vm");
        let line = cmdline(
            "/Users/me",
            &["/Volumes", "/private/var/folders"],
            &[vm, test, std::path::Path::new("/opt/vm")],
            true,
        )
        .unwrap();
        let rosetta = line.find(forward::ROSETTA_WORD).unwrap();
        let shares = line.find(forward::SHARES_KEY).unwrap();
        let hide = line.find(forward::HIDE_KEY).unwrap();
        assert!(rosetta < hide && shares < hide, "{line}");
        assert_eq!(
            forward::places_of(&line, forward::SHARES_KEY),
            ["/Volumes", "/private/var/folders"]
        );
        assert_eq!(
            forward::places_of(&line, forward::HIDE_KEY),
            [vm.to_str().unwrap(), test.to_str().unwrap()]
        );
    }
}
