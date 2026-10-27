//! The guest's published ports on the Mac's loopback (step 10.3c1,
//! D10.3-10): the VM's host asks the guest's agent which ports its
//! containers publish (`__guest-ports`, every [`POLL`], and at once as a
//! command ends), listens on the Mac's addresses for them
//! ([`crate::ports::wanted`]), and joins each connection to its port in the
//! guest through the agent (`__guest-forward IP PORT`, then raw bytes). What
//! it forwards, and what it can't listen on, is in `ports.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::forward::{
    self, AGENT_PORT, Connection, FORWARD_REQUEST, FORWARD_UDP_REQUEST, MAX_PORTS_ANSWER,
    PORTS_REQUEST, PortsAnswer, Protocol, PublishedPort, Refusal, Request,
};

use crate::host::{ACCEPT_REST, log};
use crate::idle::{Clients, Decision, Idle};
use crate::machine::Machine;
use crate::ports::{Place, PortsFile, Unbound, wanted};

/// How often the guest is asked for its ports.
const POLL: Duration = Duration::from_secs(2);

/// How long after a boot the poll keeps its quick pace, whatever it hears.
const SETTLING: Duration = Duration::from_secs(30);

/// The longest the asks for the guest's ports are put off while nothing
/// runs in it and its answer doesn't change (F106: each is a process in the
/// guest). A command's or a `docker` call's end asks at once.
const QUIET_POLL: Duration = Duration::from_secs(10);

/// The least time between two asks for the guest's ports, however often
/// a connection's end wakes the poller.
const MIN_POLL: Duration = Duration::from_millis(250);

/// How often while it doesn't answer (an older guest, one restarting its
/// agent): its last answer kept meanwhile. And how often a port another
/// program on the Mac has is tried again: each try connects to that
/// program to see that it listens.
const POLL_SLOWLY: Duration = Duration::from_secs(30);

/// How long the guest's agent may take to answer (a poll, a forward's
/// welcome).
const ANSWER_WAIT: Duration = Duration::from_secs(15);

/// How long a forwarded connection one side has ended waits for the
/// other's end before it's cut (`shipyard_sys::relay::both_ways`).
const LINGER: Duration = Duration::from_secs(60);

/// The most forwarded connections at once: the forwarded ports' share of
/// the host's one budget (`serving::PORTS_SHARE`; the sockets' connections
/// count against it too). Each costs two threads and five descriptors on
/// the Mac (the host's limit is 10,240, macOS's threads per process 6,144).
pub(crate) const MAX_CONNECTIONS: usize = crate::serving::PORTS_SHARE;

/// The most at once to one of the Mac's addresses.
pub(crate) const MAX_PER_ADDRESS: usize = 256;

/// How long a listener waits for a connection before it looks whether it
/// should stop (it's woken at once when told to).
const ACCEPT_TICK: Duration = Duration::from_secs(1);

/// What the host forwards: its listeners, what it can't listen on, and the
/// connections' slots.
pub(crate) struct Forwarding {
    machine: Arc<Machine>,
    file: PathBuf,
    /// `comes-back.json`, and the count last written to it.
    comes_back: (PathBuf, Mutex<Option<u32>>),
    idle_limit: Duration,
    state: Mutex<State>,
    slots: Arc<Mutex<Slots>>,
    said: Mutex<BTreeSet<String>>,
    /// Done at the guest's next answer, once (a new disk kept: 13.8c).
    at_answer: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[derive(Default)]
struct State {
    listening: BTreeMap<Place, Listening>,
    /// Each with why, and when it was last tried.
    unbound: BTreeMap<Place, (Unbound, Instant)>,
    udp: u32,
    written: Option<PortsFile>,
    stopped: bool,
}

/// One of the Mac's addresses listened on: what it forwards to, its
/// listener's thread and how it's told to stop, and the connections it
/// took (shut down then).
struct Listening {
    guest: PublishedPort,
    stop: Arc<AtomicBool>,
    wake: std::io::PipeWriter,
    thread: Option<std::thread::JoinHandle<()>>,
    open: Arc<Mutex<BTreeMap<u64, TcpStream>>>,
}

impl Listening {
    /// Stops it: its listener closed (its thread woken, and waited for: the
    /// port is free when this returns), its connections shut down.
    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.write_all(b"x");
        if let Ok(open) = self.open.lock() {
            for stream in open.values() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Forwarding {
    /// Starts forwarding the guest's ports: a thread that asks for them,
    /// at once and every [`POLL`], and whenever `wake` says (a command
    /// ended), until [`Forwarding::stop`].
    pub(crate) fn start(
        machine: Arc<Machine>,
        (file, comes_back): (PathBuf, PathBuf),
        wake: Receiver<()>,
        (idle, budget): ((Arc<Clients>, Idle), Arc<crate::serving::Budget>),
    ) -> Arc<Self> {
        // A killed host's file says nothing true. (What comes back is kept:
        // it outlives the VM.)
        let _ = std::fs::remove_file(&file);
        let forwarding = Arc::new(Self {
            machine,
            file,
            comes_back: (comes_back, Mutex::new(None)),
            idle_limit: idle.1.limit(),
            state: Mutex::new(State::default()),
            slots: Arc::new(Mutex::new(Slots::of(budget))),
            said: Mutex::new(BTreeSet::new()),
            at_answer: Mutex::new(None),
        });
        let polled = Arc::clone(&forwarding);
        let poller = std::thread::Builder::new().spawn(move || polled.poll(&wake, idle));
        if let Err(error) = poller {
            log(&format!("no thread to forward ports: {error}"));
        }
        forwarding
    }

    /// The guest asked once more, now, and what would come back kept (the
    /// host's stop: a container removed since the last poll mustn't boot the
    /// VM at the next login). Whether it answered: a guest that's ready.
    pub(crate) fn last_look(&self) -> bool {
        match ask(&self.machine) {
            Ok(answer) => {
                self.answered();
                self.keep_comes_back(&answer);
                true
            }
            Err(_) => false,
        }
    }

    /// `then` done at the guest's next answer (a poll's, or the last look's):
    /// it answers only once it's ready.
    pub(crate) fn at_next_answer(&self, then: impl FnOnce() + Send + 'static) {
        if let Ok(mut at_answer) = self.at_answer.lock() {
            *at_answer = Some(Box::new(then));
        }
    }

    /// The guest answered: what waited for it done.
    fn answered(&self) {
        let then = self.at_answer.lock().ok().and_then(|mut at| at.take());
        if let Some(then) = then {
            then();
        }
    }

    /// What `answer` says would come back, written to `comes-back.json` if
    /// it changed; an answer older than v3 says nothing of it, and changes
    /// nothing.
    fn keep_comes_back(&self, answer: &PortsAnswer) {
        let Ok(mut last) = self.comes_back.1.lock() else {
            return;
        };
        let Some(count) = count_to_keep(answer, *last) else {
            return;
        };
        match crate::comes_back::ComesBack::of(count).write(&self.comes_back.0) {
            Ok(()) => *last = Some(count),
            Err(error) => log(&format!("what would come back isn't kept: {error}")),
        }
    }

    /// Stops every listener and connection, and removes `ports.json`.
    pub(crate) fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            for (_, listening) in std::mem::take(&mut state.listening) {
                listening.stop();
            }
        }
        let _ = std::fs::remove_file(&self.file);
    }

    /// Asks for the guest's ports until the host stops, follows them, and
    /// sees whether the VM is idle (then stops the host: the main thread's
    /// ordered stop, asked with a SIGTERM to this process).
    fn poll(self: &Arc<Self>, wake: &Receiver<()>, (clients, mut idle): (Arc<Clients>, Idle)) {
        let mut failing: Option<String> = None;
        // Unchanged answers in a row while nothing runs: the asks are put
        // off (`quiet_wait`).
        let (mut last, mut quiet) = (None::<PortsAnswer>, 0_u32);
        let began = Instant::now();
        loop {
            let asked_at = Instant::now();
            let asked = ask(&self.machine);
            let active = asked.as_ref().ok().map(|answer| answer.active);
            match asked {
                Ok(answer) => {
                    self.answered();
                    if failing.take().is_some() {
                        log("the guest answers for its ports again");
                    }
                    self.keep_comes_back(&answer);
                    self.follow(&answer);
                    let same = last.as_ref() == Some(&answer);
                    // (Not in the VM's first half-minute: the containers a
                    // boot brings back start after its first answers.)
                    let settled = began.elapsed() > SETTLING;
                    quiet = if same && answer.active == 0 && settled {
                        quiet.saturating_add(1)
                    } else {
                        0
                    };
                    last = Some(answer);
                }
                Err(why) => {
                    if failing.as_deref() != Some(why.as_str()) {
                        log(&format!(
                            "the guest's ports can't be had ({why}): its last ones kept, asked \
                             again every {} s",
                            POLL_SLOWLY.as_secs()
                        ));
                    }
                    failing = Some(why);
                    // (Answered again, the poll starts at its quick pace.)
                    (last, quiet) = (None, 0);
                }
            }
            match idle.after(&clients, active, Instant::now()) {
                Decision::Wait => {}
                Decision::KeptUp(count) => log(&format!(
                    "kept up: {count} active (idle for {} s otherwise)",
                    idle.limit().as_secs()
                )),
                Decision::NoAnswer => log(&format!(
                    "not stopped as idle: the guest hasn't answered for {} s (what it runs \
                     isn't known)",
                    idle.limit().as_secs()
                )),
                Decision::Stop => {
                    log(&format!("idle for {} s: stopping", idle.limit().as_secs()));
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::this(),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                    return;
                }
            }
            // (Never past the idle limit: the idle stop is decided here, and
            // a VM idle that long is to be stopped then.)
            let wait = if failing.is_some() {
                POLL_SLOWLY
            } else {
                quiet_wait(quiet).min(idle.limit().max(MIN_POLL))
            };
            match wake.recv_timeout(wait) {
                // A connection's end: asked again at once, at the quick pace.
                Ok(()) => {
                    quiet = 0;
                    while wake.try_recv().is_ok() {}
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Woken by a burst of short connections (a script's `docker`
            // calls), it still asks the guest no more than so often.
            if let Some(rest) = MIN_POLL.checked_sub(asked_at.elapsed()) {
                std::thread::sleep(rest);
            }
            if self.state.lock().map_or(true, |state| state.stopped) {
                return;
            }
        }
    }

    /// Listens for what `answer` publishes, and only that.
    fn follow(self: &Arc<Self>, answer: &PortsAnswer) {
        let wanted = wanted(&answer.ports, answer.version >= forward::PORTS_LIST_UDP);
        if wanted.left_out > 0 {
            self.say_once(format!(
                "the guest publishes more than {} ports: {} not forwarded",
                crate::ports::MAX_PORTS,
                wanted.left_out
            ));
        }
        for clash in &wanted.clashes {
            self.say_once(format!(
                "port {} on {} isn't forwarded: another container's port {0} has its address on \
                 this Mac",
                clash.port, clash.ip
            ));
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.stopped {
            return;
        }
        let gone: Vec<Place> = state
            .listening
            .iter()
            .filter(|(place, listening)| wanted.addresses.get(place) != Some(&listening.guest))
            .map(|(place, _)| *place)
            .collect();
        for place in gone {
            if let Some(listening) = state.listening.remove(&place) {
                listening.stop();
                log(&format!("{}/{}: no longer forwarded", place.1, place.0));
            }
        }
        state
            .unbound
            .retain(|place, _| wanted.addresses.contains_key(place));
        for (&place, &guest) in &wanted.addresses {
            let tried_lately = state
                .unbound
                .get(&place)
                .is_some_and(|(why, at)| *why == Unbound::InUse && at.elapsed() < POLL_SLOWLY);
            if state.listening.contains_key(&place) || tried_lately {
                continue;
            }
            let (protocol, address) = place;
            match self.listen(place, guest) {
                Ok(listening) => {
                    log(&format!(
                        "{address}/{protocol}: forwarded to the guest's {}",
                        guest.port
                    ));
                    state.unbound.remove(&place);
                    state.listening.insert(place, listening);
                }
                Err(why) => {
                    if state.unbound.get(&place).map(|(said, _)| said) != Some(&why) {
                        log(&format!("{address}/{protocol}: {why} (tried again)"));
                    }
                    state.unbound.insert(place, (why, Instant::now()));
                }
            }
        }
        state.udp = answer.udp;
        self.write_file(&mut state);
    }

    /// `ports.json` written if what it says changed.
    fn write_file(&self, state: &mut State) {
        let forwarded: Vec<Place> = state.listening.keys().copied().collect();
        let unbound: Vec<(Place, Unbound)> = state
            .unbound
            .iter()
            .map(|(place, (why, _))| (*place, why.clone()))
            .collect();
        let file = PortsFile::new(&forwarded, &unbound, state.udp, self.idle_limit);
        if state.written.as_ref() != Some(&file) {
            match file.write(&self.file) {
                Ok(()) => state.written = Some(file),
                Err(error) => log(&format!("{error}")),
            }
        }
    }

    /// A listener on `place` for `guest`, on a thread of its own: TCP's
    /// connections, or UDP's datagrams ([`crate::udp`]).
    fn listen(self: &Arc<Self>, place: Place, guest: PublishedPort) -> Result<Listening, Unbound> {
        let unbound = |error: std::io::Error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                Unbound::InUse
            } else {
                Unbound::Other(error.to_string())
            }
        };
        let (protocol, address) = place;
        if protocol == Protocol::Udp {
            let socket = bind_udp(address).map_err(unbound)?;
            return self.listen_udp(socket, address, guest);
        }
        let listener = bind_alone(address).map_err(unbound)?;
        let (woken, wake) = std::io::pipe().map_err(|e| Unbound::Other(e.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(BTreeMap::new()));
        let (stopped, taken) = (Arc::clone(&stop), Arc::clone(&open));
        let forwarding = Arc::clone(self);
        let thread = std::thread::Builder::new()
            .spawn(move || {
                let served = Served {
                    address,
                    guest,
                    stop: &stopped,
                    open: &taken,
                };
                forwarding.serve(&listener, &woken, &served);
            })
            .map_err(|error| Unbound::Other(format!("no thread to listen: {error}")))?;
        Ok(Listening {
            guest,
            stop,
            wake,
            thread: Some(thread),
            open,
        })
    }

    /// [`Forwarding::listen`]'s for UDP: `socket`'s datagrams relayed to
    /// `guest`'s port over one stream, every Mac peer's ([`crate::udp`]), on
    /// a thread of its own.
    fn listen_udp(
        self: &Arc<Self>,
        socket: UdpSocket,
        address: SocketAddr,
        guest: PublishedPort,
    ) -> Result<Listening, Unbound> {
        let (woken, wake) = std::io::pipe().map_err(|e| Unbound::Other(e.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let relayed: Arc<dyn crate::udp::Guest> = Arc::new(UdpGuest {
            forwarding: Arc::clone(self),
            address,
            guest,
        });
        let thread = std::thread::Builder::new()
            .spawn(move || crate::udp::relay(&socket, &woken, &stopped, &relayed))
            .map_err(|error| Unbound::Other(format!("no thread to listen: {error}")))?;
        Ok(Listening {
            guest,
            stop,
            wake,
            thread: Some(thread),
            open: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Takes `listener`'s connections until told to stop (`woken` then).
    fn serve(
        self: &Arc<Self>,
        listener: &TcpListener,
        woken: &std::io::PipeReader,
        served: &Served<'_>,
    ) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let address = served.address;
        let tick =
            nix::poll::PollTimeout::try_from(ACCEPT_TICK).unwrap_or(nix::poll::PollTimeout::NONE);
        loop {
            let mut fds = [
                nix::poll::PollFd::new(listener.as_fd(), nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(woken.as_fd(), nix::poll::PollFlags::POLLIN),
            ];
            let polled = nix::poll::poll(&mut fds, tick);
            // Told to stop: no more, even of a connection already waiting.
            if served.stop.load(Ordering::SeqCst) {
                return;
            }
            if !matches!(polled, Ok(1..)) {
                continue;
            }
            let tcp = match listener.accept() {
                Ok((tcp, _)) => tcp,
                Err(error) => {
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) {
                        self.say_once(format!("{address}: accepting: {error}"));
                        std::thread::sleep(ACCEPT_REST);
                    }
                    continue;
                }
            };
            // On macOS an accepted socket has its listener's O_NONBLOCK.
            if tcp.set_nonblocking(false).is_err() {
                continue;
            }
            if !self.slots.lock().is_ok_and(|mut slots| slots.take(address)) {
                self.say_once(format!(
                    "{address}: too many connections at once ({MAX_PER_ADDRESS} to one address, \
                     {MAX_CONNECTIONS} in all): new ones closed"
                ));
                continue;
            }
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let held = Held {
                address,
                id,
                slots: Arc::clone(&self.slots),
                open: Arc::clone(served.open),
            };
            // Known to its listener (shut down when it stops), or not kept.
            let Ok(copy) = tcp.try_clone() else {
                continue;
            };
            if let Ok(mut open) = served.open.lock() {
                open.insert(id, copy);
            }
            let (machine, forwarding, guest) =
                (Arc::clone(&self.machine), Arc::clone(self), served.guest);
            let joined = std::thread::Builder::new().spawn(move || {
                let _held = held;
                if let Err(why) = forward_one(&machine, guest, tcp) {
                    forwarding.say_once(format!("{address}: a connection not forwarded: {why}"));
                }
            });
            if let Err(error) = joined {
                self.say_once(format!("{address}: no thread for a connection: {error}"));
            }
        }
    }

    /// Logs `line` the first time it's said (a flood of the same failure
    /// is one line), up to a bound of lines remembered.
    fn say_once(&self, line: String) {
        let Ok(mut said) = self.said.lock() else {
            return;
        };
        if said.len() > 256 {
            said.clear();
        }
        if !said.contains(&line) {
            log(&line);
            said.insert(line);
        }
    }
}

/// What a listener's thread serves.
struct Served<'a> {
    address: SocketAddr,
    guest: PublishedPort,
    stop: &'a AtomicBool,
    open: &'a Arc<Mutex<BTreeMap<u64, TcpStream>>>,
}

/// What a UDP relay ([`crate::udp`]) asks of the host: the stream to
/// `guest`'s port (one for every Mac peer of `address`: no slot each).
struct UdpGuest {
    forwarding: Arc<Forwarding>,
    address: SocketAddr,
    guest: PublishedPort,
}

impl crate::udp::Guest for UdpGuest {
    fn join(&self) -> Result<(UnixStream, Vec<u8>), String> {
        let (ip, port) = (self.guest.ip.to_string(), self.guest.port.to_string());
        joined(&self.forwarding.machine, &[FORWARD_UDP_REQUEST, &ip, &port])
            .map_err(|refusal| refusal.to_string())
    }

    fn say(&self, line: String) {
        self.forwarding
            .say_once(format!("{}/udp: {line}", self.address));
    }
}

/// A connection's slot and its place in its listener's open ones, given
/// back when it ends (dropped).
struct Held {
    address: SocketAddr,
    id: u64,
    slots: Arc<Mutex<Slots>>,
    open: Arc<Mutex<BTreeMap<u64, TcpStream>>>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Ok(mut open) = self.open.lock() {
            open.remove(&self.id);
        }
        if let Ok(mut slots) = self.slots.lock() {
            slots.give_back(self.address);
        }
    }
}

/// The forwarded connections at once: at most [`MAX_CONNECTIONS`] (the
/// host's budget's share for them), and [`MAX_PER_ADDRESS`] to one address.
#[derive(Debug)]
pub(crate) struct Slots {
    budget: Arc<crate::serving::Budget>,
    per: BTreeMap<SocketAddr, usize>,
}

impl Slots {
    /// Slots drawn on the host's `budget`.
    pub(crate) fn of(budget: Arc<crate::serving::Budget>) -> Self {
        Self {
            budget,
            per: BTreeMap::new(),
        }
    }

    /// A slot for a connection to `address`, if one's free (its address's,
    /// and the budget's: taken together, or neither).
    pub(crate) fn take(&mut self, address: SocketAddr) -> bool {
        let here = self.per.get(&address).copied().unwrap_or(0);
        if here >= MAX_PER_ADDRESS || !self.budget.take_port() {
            return false;
        }
        self.per.insert(address, here + 1);
        true
    }

    /// `address`'s slot given back (to the budget too).
    pub(crate) fn give_back(&mut self, address: SocketAddr) {
        if let Some(here) = self.per.get_mut(&address) {
            *here = here.saturating_sub(1);
            if *here == 0 {
                self.per.remove(&address);
            }
            self.budget.give_back();
        }
    }
}

/// A listener on `address` alone, never in another program's way: bound
/// first without `SO_REUSEADDR` (which std sets), so that another
/// program's listener on the port, even on every address, makes it fail
/// (`EADDRINUSE`) rather than take that program's connections to
/// `address` (BSD lets a more specific bind beside a wildcard one with
/// it). Refused so, it's bound again with `SO_REUSEADDR` only if a
/// connection to it is refused (nothing listens there; any other outcome,
/// a timeout too, is a listener's): then what holds the port is an old
/// connection's `TIME_WAIT` (half a minute after a port forwarded before
/// closed), which shadows nobody. Non-blocking, close-on-exec; IPv6 for
/// IPv6 only.
pub(crate) fn bind_alone(address: SocketAddr) -> std::io::Result<TcpListener> {
    match bind(address, false) {
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            let listened = TcpStream::connect_timeout(&address, Duration::from_millis(500));
            if !listened.is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused) {
                return Err(error);
            }
            bind(address, true)
        }
        bound => bound,
    }
}

/// A non-blocking, close-on-exec listener on `address`, with
/// `SO_REUSEADDR` if `reuse`.
fn bind(address: SocketAddr, reuse: bool) -> std::io::Result<TcpListener> {
    use nix::sys::socket::{
        AddressFamily, Backlog, SockFlag, SockType, SockaddrStorage, listen, setsockopt, socket,
        sockopt,
    };
    let family = if address.is_ipv4() {
        AddressFamily::Inet
    } else {
        AddressFamily::Inet6
    };
    let fd: OwnedFd = socket(family, SockType::Stream, SockFlag::empty(), None)?;
    nix::fcntl::fcntl(
        &fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )?;
    if address.is_ipv6() {
        setsockopt(&fd, sockopt::Ipv6V6Only, &true)?;
    }
    if reuse {
        setsockopt(&fd, sockopt::ReuseAddr, &true)?;
    }
    nix::sys::socket::bind(fd.as_raw_fd(), &SockaddrStorage::from(address))?;
    listen(&fd, Backlog::new(128).unwrap_or(Backlog::MAXCONN))?;
    let listener = TcpListener::from(fd);
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// A UDP socket on `address` alone: std binds it without `SO_REUSEADDR`,
/// so another program's socket on its port (on it, or on every address)
/// makes it fail (`EADDRINUSE`) rather than share its datagrams; IPv6's
/// for IPv6 only (`::1` is). Non-blocking, close-on-exec.
fn bind_udp(address: SocketAddr) -> std::io::Result<UdpSocket> {
    // Tried again, three times at growing waits: `vm status`'s and
    // `doctor`'s look for a program on every address of the port
    // (`ports::held_on_every_address`) holds the wildcard for an instant,
    // which refuses this bind meanwhile (two CLIs' looks one after the
    // other, too).
    let mut bound = UdpSocket::bind(address);
    for wait in [20, 50, 120] {
        match &bound {
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                std::thread::sleep(Duration::from_millis(wait));
                bound = UdpSocket::bind(address);
            }
            _ => break,
        }
    }
    let socket = bound?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// A request that isn't a command (`args`) to the guest's agent, over
/// vsock (tried twice: its backlog may be full in a burst), its answer to
/// be read within [`ANSWER_WAIT`].
pub(crate) fn request(machine: &Machine, args: &[&str]) -> Result<Connection<UnixStream>, String> {
    request_from(machine, args).map(|(connection, _)| connection)
}

/// [`request`], with the connection's port on this side (what the guest
/// sees as its peer's: [`Machine::connect_from`]).
fn request_from(machine: &Machine, args: &[&str]) -> Result<(Connection<UnixStream>, u32), String> {
    let limit = Duration::from_secs(10);
    let (stream, port) = machine
        .connect_from(AGENT_PORT, limit)
        .or_else(|_| {
            std::thread::sleep(Duration::from_millis(100));
            machine.connect_from(AGENT_PORT, limit)
        })
        .map_err(|e| e.to_string())?;
    let mut stream = UnixStream::from(stream);
    stream
        .set_read_timeout(Some(ANSWER_WAIT))
        .map_err(|e| e.to_string())?;
    let hello = forward::hello(&Request::internal(args)).map_err(|e| e.to_string())?;
    forward::send(&mut stream, &hello).map_err(|e| e.to_string())?;
    Ok((Connection::new(stream), port))
}

/// Asks the guest's agent its boot ID (F90): what the host's
/// [`Liveness`](crate::liveness::Liveness) hears.
pub(crate) fn ask_boot(machine: &Machine) -> crate::liveness::Heard {
    use crate::liveness::Heard;
    let Ok(mut connection) = request(machine, &[forward::BOOT_REQUEST]) else {
        return Heard::Nothing;
    };
    heard(output(&mut connection, forward::MAX_BOOT_ANSWER))
}

/// What a boot ID's ask heard, from its answer: the ID; a refusal or a
/// guest that can't read its ID ([`forward::BOOT_UNREADABLE`]) is alive; an
/// older guest's failure (it doesn't know the ask) is unknown; nothing else
/// is an answer.
fn heard(answer: Result<Vec<u8>, Unanswered>) -> crate::liveness::Heard {
    use crate::liveness::Heard;
    match answer {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(id) if !id.trim().is_empty() => Heard::Id(id.trim().to_owned()),
            _ => Heard::Alive,
        },
        Err(Unanswered::Refused(_) | Unanswered::Failed(forward::BOOT_UNREADABLE)) => Heard::Alive,
        Err(Unanswered::Failed(_)) => Heard::Unknown,
        Err(Unanswered::Broken(_)) => Heard::Nothing,
    }
}

/// What `answer` says would come back, if it says it (v3) and it isn't
/// `last`, the count kept already.
fn count_to_keep(answer: &PortsAnswer, last: Option<u32>) -> Option<u32> {
    answer
        .comes_back
        .filter(|_| answer.version >= forward::PORTS_COMES_BACK)
        .filter(|count| last != Some(*count))
}

/// How long the poll waits after `quiet` unchanged answers in a row with
/// nothing running: [`POLL`], doubled each time, at most [`QUIET_POLL`].
fn quiet_wait(quiet: u32) -> Duration {
    POLL.saturating_mul(2_u32.saturating_pow(quiet.min(8)))
        .min(QUIET_POLL)
}

/// Asks the guest's agent what its containers publish.
fn ask(machine: &Machine) -> Result<PortsAnswer, String> {
    // Asked at this build's version: without it the guest answers v1.
    let version = forward::PORTS_VERSION.to_string();
    let mut connection = request(machine, &[PORTS_REQUEST, &version])?;
    let answer =
        output(&mut connection, MAX_PORTS_ANSWER).map_err(|refusal| refusal.to_string())?;
    PortsAnswer::decode(&answer).map_err(|e| e.to_string())
}

/// What an internal request (`__guest-ports`, `__guest-doctor`) answers on
/// `connection`, as the agent runs it: its stdout, up to `cap`, once it has
/// ended well. A refusal is its reason (`Refusal::Stopping` from a host
/// that's stopping); an end other than 0 is [`Unanswered::Failed`] (an older
/// guest runs a request it doesn't know as an unknown command, which fails);
/// any other failure is [`Unanswered::Broken`].
pub(crate) fn output<S: std::io::Read + std::io::Write>(
    connection: &mut Connection<S>,
    cap: usize,
) -> Result<Vec<u8>, Unanswered> {
    let other = Unanswered::Broken;
    let mut answer = Vec::new();
    loop {
        match connection.next_frame().map_err(|e| other(e.to_string()))? {
            // (An internal request gets no session token; skipped all the
            // same.)
            Some(Frame::Welcome { .. } | Frame::Stderr(_) | Frame::Session(_)) => {}
            Some(Frame::Stdout(bytes)) if answer.len() + bytes.len() <= cap => {
                answer.extend_from_slice(&bytes);
            }
            Some(Frame::Stdout(_)) => return Err(other("an answer too long".to_owned())),
            Some(Frame::Exited(0)) => return Ok(answer),
            Some(Frame::Exited(code)) => return Err(Unanswered::Failed(code)),
            Some(frame) if let Some(refusal) = Refusal::of(&frame) => {
                return Err(Unanswered::Refused(refusal));
            }
            Some(_) => return Err(other("not an answer to the request".to_owned())),
            None => return Err(other("the guest closed the connection".to_owned())),
        }
    }
}

/// Why a request to the agent got no answer: refused (the peer said no,
/// and why), failed (it ended with a code other than 0), or broken (the
/// wire: a closed connection, an answer too long, no answer in time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// The host or the guest said no.
    Refused(Refusal),
    /// No answer came, or none it could be.
    Broken(String),
    /// The request ran and failed with this code: a guest older than this
    /// Shipyard that doesn't know it, run as a command.
    Failed(i32),
}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(f),
            Self::Broken(why) => f.write_str(why),
            Self::Failed(code) => write!(
                f,
                "its answer ended with {code}: a guest older than this shipyard? \
                 scripts/guest/build.sh makes it this build's"
            ),
        }
    }
}

/// One connection, `tcp`, joined to `guest`'s port through the agent: the
/// forward asked, its welcome waited for, then the bytes both ways. (An
/// older guest would welcome it as a command: never asked, since it can't
/// answer for its ports and nothing is listened on.)
fn forward_one(machine: &Machine, guest: PublishedPort, mut tcp: TcpStream) -> Result<(), String> {
    let (ip, port) = (guest.ip.to_string(), guest.port.to_string());
    let (agent, unread) =
        joined(machine, &[FORWARD_REQUEST, &ip, &port]).map_err(|refusal| refusal.to_string())?;
    tcp.write_all(&unread).map_err(|e| e.to_string())?;
    shipyard_sys::relay::both_ways(tcp.into(), agent.into(), LINGER).map_err(|e| e.to_string())
}

/// A connection the agent is asked for (`args`: a port's, the API's), its
/// welcome waited for: the stream to relay, and the bytes already read past
/// the welcome. A refusal is its reason. A command's welcome (no input
/// taken: an older guest, which runs the request as an unknown command)
/// is refused, never relayed as the connection's bytes.
pub(crate) fn joined(
    machine: &Machine,
    args: &[&str],
) -> Result<(UnixStream, Vec<u8>), Unanswered> {
    joined_from(machine, args, false).map(|(agent, unread, _)| (agent, unread))
}

/// [`joined`], with what names the connection to the guest ([`hang_up`]):
/// its session's token, if `with_session` and the guest gave one after
/// its welcome (F155, version 6), else the connection's port on this side
/// (an older guest's way: a port can be another connection's by then).
pub(crate) fn joined_from(
    machine: &Machine,
    args: &[&str],
    with_session: bool,
) -> Result<(UnixStream, Vec<u8>, String), Unanswered> {
    let other = Unanswered::Broken;
    let (mut connection, port) = request_from(machine, args).map_err(other)?;
    let version = match connection.next_frame().map_err(|e| other(e.to_string()))? {
        Some(Frame::Welcome {
            stdin: true,
            version,
            ..
        }) => version,
        Some(Frame::Welcome { .. }) => {
            return Err(other(
                "the guest is older than this shipyard: scripts/guest/build.sh makes it this \
                 build's"
                    .to_owned(),
            ));
        }
        Some(frame) if let Some(refusal) = Refusal::of(&frame) => {
            return Err(Unanswered::Refused(refusal));
        }
        _ => return Err(other("the guest didn't take it".to_owned())),
    };
    let mut name = port.to_string();
    if with_session && version >= shipyard_runtime::attach::SESSION_VERSION {
        match connection.next_frame().map_err(|e| other(e.to_string()))? {
            Some(Frame::Session(0)) => {}
            Some(Frame::Session(token)) => name = forward::session_token_text(token),
            _ => return Err(other("the guest gave no session token".to_owned())),
        }
    }
    let (agent, unread) = connection.into_parts();
    agent
        .set_read_timeout(None)
        .map_err(|e| other(e.to_string()))?;
    Ok((agent, unread, name))
}

/// Tells the guest's agent that the client of its API connection `name`
/// names ([`joined_from`]) has gone (11.7): Virtualization.framework
/// passes this side's close on as a half-close only, which a client that
/// has merely finished sending also is, so the guest can't tell. Not
/// waited for (an older guest runs it as an unknown command, and fails).
pub(crate) fn hang_up(machine: &Machine, name: &str) {
    let _ = request(machine, &[forward::API_HANG_UP, name]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boot ID's answer: the ID; a refusal, or a guest that can't read its
    /// ID, alive; an older guest's failure unknown (never taken for a hang:
    /// it would stop every older guest's VM); a broken answer nothing.
    #[test]
    fn a_boot_answer_is_heard_for_what_it_is() {
        use crate::liveness::Heard;
        assert_eq!(
            heard(Ok(b"abc-123\n".to_vec())),
            Heard::Id("abc-123".to_owned())
        );
        assert_eq!(heard(Ok(Vec::new())), Heard::Alive);
        assert_eq!(
            heard(Err(Unanswered::Refused(Refusal::NotReady))),
            Heard::Alive
        );
        assert_eq!(
            heard(Err(Unanswered::Failed(forward::BOOT_UNREADABLE))),
            Heard::Alive
        );
        assert_eq!(heard(Err(Unanswered::Failed(2))), Heard::Unknown);
        assert_eq!(
            heard(Err(Unanswered::Broken("closed".to_owned()))),
            Heard::Nothing
        );
    }

    /// A v3 answer's count is kept when it changed; an older answer's (a v2
    /// guest's, read with none) never changes it.
    #[test]
    fn an_older_answer_keeps_the_count() {
        let mut answer = PortsAnswer::new();
        answer.comes_back = Some(2);
        assert_eq!(count_to_keep(&answer, None), Some(2));
        assert_eq!(count_to_keep(&answer, Some(2)), None, "unchanged");
        assert_eq!(count_to_keep(&answer, Some(1)), Some(2));
        let mut older = answer.clone();
        older.version = forward::PORTS_LIST_UDP;
        assert_eq!(
            count_to_keep(&older, Some(3)),
            None,
            "a v2 answer says nothing"
        );
        older.comes_back = None;
        assert_eq!(count_to_keep(&older, Some(3)), None);
    }

    /// A quiet poll waits twice as long each unchanged round, up to its
    /// bound; none quiet, the quick pace.
    #[test]
    fn a_quiet_poll_backs_off_to_its_bound() {
        assert_eq!(quiet_wait(0), POLL);
        assert_eq!(quiet_wait(1), POLL * 2);
        assert_eq!(quiet_wait(2), QUIET_POLL.min(POLL * 4));
        assert_eq!(quiet_wait(u32::MAX), QUIET_POLL);
        assert!(
            QUIET_POLL < Duration::from_secs(30),
            "well under the idle stop's granularity"
        );
    }

    /// No more than so many connections at once, in all and to one
    /// address; a slot given back is free again; a refusal leaves nothing.
    #[test]
    fn connections_are_bounded() {
        let one: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let budget = Arc::new(crate::serving::Budget::default());
        let mut slots = Slots::of(Arc::clone(&budget));
        for _ in 0..MAX_PER_ADDRESS {
            assert!(slots.take(one));
        }
        assert!(!slots.take(one), "past the bound for one address");
        assert_eq!(budget.open(), MAX_PER_ADDRESS, "a refusal takes nothing");
        slots.give_back(one);
        assert!(slots.take(one), "a slot given back");
        let mut taken = MAX_PER_ADDRESS;
        let mut port = 1000_u16;
        while taken < MAX_CONNECTIONS {
            let other = SocketAddr::new(one.ip(), port);
            while taken < MAX_CONNECTIONS && slots.take(other) {
                taken += 1;
            }
            port += 1;
        }
        let refused: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(!slots.take(refused), "past the bound in all");
        assert!(!slots.per.contains_key(&refused));
    }

    /// Another program's listener on the port, even on every address, makes
    /// a loopback bind fail as in use, never shadows it; a free port binds.
    #[test]
    fn a_port_in_use_on_any_address_is_busy() {
        let theirs = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = theirs.local_addr().unwrap().port();
        let refused = bind_alone(SocketAddr::from(([127, 0, 0, 1], port)));
        assert_eq!(
            refused.map(drop).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        let free = bind_alone("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(free.local_addr().unwrap().ip().is_loopback());
    }

    /// A UDP port another program holds on every address is in use, never
    /// shared (no `SO_REUSEADDR` retry, as TCP's for a `TIME_WAIT`); free, it
    /// binds.
    #[test]
    fn a_udp_port_held_on_every_address_is_busy() {
        let other = UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = other.local_addr().unwrap().port();
        let loopback = SocketAddr::from(([127, 0, 0, 1], port));
        let refused = bind_udp(loopback).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::AddrInUse);
        drop(other);
        assert!(bind_udp(loopback).is_ok(), "the control: free, it binds");
    }

    /// A port held only by an old connection's `TIME_WAIT` (the listening
    /// side closed it first: a forwarded port's, after its container went)
    /// is bound again at once, not "in use", though a bind without
    /// `SO_REUSEADDR` is refused.
    #[test]
    fn a_port_in_time_wait_is_bound_again() {
        let first = bind_alone("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = first.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        first.set_nonblocking(false).unwrap();
        let (accepted, _) = first.accept().unwrap();
        // This side first: its end goes to TIME_WAIT.
        drop(accepted);
        drop(client);
        drop(first);
        assert_eq!(
            bind(address, false).map(drop).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse,
            "TIME_WAIT holds the port"
        );
        let again = bind_alone(address).expect("bound again over TIME_WAIT");
        assert_eq!(again.local_addr().unwrap(), address);
    }

    /// A listener told to stop lets its port go before `stop` returns (the
    /// next bind of it never finds it in use), and takes no connection
    /// after.
    #[test]
    fn a_stopped_listener_frees_its_port_at_once() {
        let listener = bind_alone("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let (woken, wake) = std::io::pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let tick = nix::poll::PollTimeout::try_from(Duration::from_secs(60)).unwrap();
            let mut fds = [
                nix::poll::PollFd::new(listener.as_fd(), nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(woken.as_fd(), nix::poll::PollFlags::POLLIN),
            ];
            let _ = nix::poll::poll(&mut fds, tick);
            assert!(stopped.load(Ordering::SeqCst));
        });
        let listening = Listening {
            guest: PublishedPort {
                ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
                port: address.port(),
                protocol: Protocol::Tcp,
            },
            stop,
            wake,
            thread: Some(thread),
            open: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let began = Instant::now();
        listening.stop();
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "woken, not ticked"
        );
        bind(address, false).expect("the port is free once stop returns");
    }
}
