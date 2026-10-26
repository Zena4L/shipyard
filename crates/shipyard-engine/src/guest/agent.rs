//! The macOS guest's agent (step 10.3a): it listens on vsock
//! ([`forward::AGENT_PORT`]) for the Mac, and runs each connection's
//! command, `shipyard ARGS` as the Mac CLI was given, sending back what it
//! writes and how it ended. Only the host (CID 2) is let in: a process in
//! the guest reaching the agent would be root in the guest.
//!
//! The protocol is the attach protocol's frames: the client's
//! [`Frame::HelloRun`] with a [`Request`]; the agent's [`Frame::Welcome`],
//! the output ([`Frame::Stdout`], [`Frame::Stderr`]) and [`Frame::Exited`].
//! What the client types comes as [`Frame::Stdin`] and [`Frame::StdinEnd`].

use std::io::Write as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept4, bind, getpeername, listen,
    socket,
};
use nix::unistd::Pid;
use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::forward::{self, Connection, Request};

/// The host's CID: the only peer the agent serves.
const HOST_CID: u32 = 2;

/// Any CID: the agent listens on the guest's own.
const ANY_CID: u32 = u32::MAX;

/// The command every request runs.
const SHIPYARD: &str = "/usr/bin/shipyard";

/// The guest's environment for a command: its own, not the Mac's.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// How long `accept` rests after a failure (out of descriptors, of
/// memory): it would otherwise be ready again at once, forever.
const ACCEPT_REST: Duration = Duration::from_millis(100);

/// One connection's stream: what's written to it from several threads.
type Stream = Arc<Mutex<std::fs::File>>;

/// The agent's run: never returns.
pub(super) fn run() -> ! {
    raise_descriptor_limit();
    let code = match serve() {
        Ok(()) => 0,
        Err(error) => {
            say(&error);
            125
        }
    };
    std::process::exit(code)
}

/// The most descriptors the agent may hold: about four per forwarded
/// connection and per command, many at once (a browser's).
const DESCRIPTORS: u64 = 65_536;

/// Raises the agent's descriptor limit to [`DESCRIPTORS`] (it's root): its
/// soft limit would be 1,024.
fn raise_descriptor_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let wanted = match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) if soft >= DESCRIPTORS => (soft, hard),
        Ok((_, hard)) => (DESCRIPTORS, hard.max(DESCRIPTORS)),
        Err(_) => return,
    };
    if let Err(error) = setrlimit(Resource::RLIMIT_NOFILE, wanted.0, wanted.1) {
        say(&format!("raising the descriptor limit: {error}"));
    }
}

/// Says `line` on the agent's stderr (the console).
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "shipyard-agent: {line}");
}

/// Accepts the host's connections, each on a thread of its own.
fn serve() -> Result<(), String> {
    let listener = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|e| format!("making a vsock socket: {e}"))?;
    bind(
        listener.as_raw_fd(),
        &VsockAddr::new(ANY_CID, forward::AGENT_PORT),
    )
    .map_err(|e| format!("binding vsock port {}: {e}", forward::AGENT_PORT))?;
    // Room for a burst: forwarded connections, the host's polls, commands.
    listen(&listener, Backlog::new(128).unwrap_or(Backlog::MAXCONN))
        .map_err(|e| format!("listening: {e}"))?;
    loop {
        let fd = match accept4(listener.as_raw_fd(), SockFlag::SOCK_CLOEXEC) {
            Ok(fd) => fd,
            Err(error) => {
                say(&format!("accepting: {error}"));
                std::thread::sleep(ACCEPT_REST);
                continue;
            }
        };
        // SAFETY: `accept4` just returned this descriptor, owned by no one
        // else.
        let connection = unsafe { OwnedFd::from_raw_fd(fd) };
        let peer = getpeername::<VsockAddr>(connection.as_raw_fd());
        let cid = peer.as_ref().map(VsockAddr::cid);
        if !from_host(cid.ok()) {
            say(&format!(
                "refused a connection from CID {cid:?}: only the host is served"
            ));
            continue;
        }
        // The host's port: what names its API connection to a hang-up.
        let port = peer.map_or(0, |peer| peer.port());
        let spawned = std::thread::Builder::new().spawn(move || {
            if let Err(error) = handle(std::fs::File::from(connection), port) {
                say(&error);
            }
        });
        if let Err(error) = spawned {
            say(&format!("a connection not served: {error}"));
        }
    }
}

/// Whether a connection from `cid` is the host's.
pub(super) fn from_host(cid: Option<u32>) -> bool {
    cid == Some(HOST_CID)
}

/// One connection (from the host's `port`): its request run, its output
/// sent, its end said.
fn handle(stream: std::fs::File, port: u32) -> Result<(), String> {
    let writer: Stream = Arc::new(Mutex::new(
        stream
            .try_clone()
            .map_err(|e| format!("a connection: {e}"))?,
    ));
    let mut connection = Connection::new(stream);
    // The version both speak, as a monitor agrees it: the older one's (a
    // newer client is answered at this guest's, never refused for it).
    let (agreed, request) = match connection.next_frame().map_err(|e| e.to_string())? {
        Some(Frame::HelloRun { version, request }) => {
            match shipyard_runtime::attach::negotiate(version) {
                Some(agreed) => (agreed, request),
                None => return refuse(&writer, &format!("a hello of version {version}")),
            }
        }
        Some(_) => return refuse(&writer, "a run's hello was expected"),
        None => return Ok(()),
    };
    let request = match Request::decode(&request) {
        Ok(request) => request,
        Err(error) => return refuse(&writer, &error.to_string()),
    };
    if request.args == [forward::STOP_REQUEST] {
        // PID 1 told first: the host may have gone already (it doesn't wait
        // for the answer), and a failed answer mustn't keep the stop back.
        let told = kill(Pid::from_raw(1), Signal::SIGTERM);
        let _ = send(&writer, &Frame::Exited(i32::from(told.is_err())));
        return told.map_err(|e| format!("telling the guest's init to stop: {e}"));
    }
    if !std::path::Path::new(super::READY).exists() {
        // Said by its code to a client that reads one (F99).
        return send(&writer, &forward::Refusal::NotReady.frame(agreed));
    }
    match connection_asked(&request.args) {
        Some(Asked::Port(ip, port)) => {
            return forward_port((&writer, agreed), connection, ip, port);
        }
        Some(Asked::Api) => return forward_api((&writer, agreed), connection, port),
        Some(Asked::HangUp(theirs)) => {
            hang_up_api(theirs);
            return Ok(());
        }
        None => {}
    }
    if let Some(theirs) = request.time {
        follow_clock(theirs);
    }
    if !std::path::Path::new(&request.cwd).is_dir() {
        return refuse(
            &writer,
            &format!(
                "{} isn't in the guest: is it under your home, which the VM shares?",
                request.cwd
            ),
        );
    }
    relay(&request, (&writer, agreed), connection)
}

/// Runs the request's command, on a terminal of its own if the caller has
/// one (step 10.3b2, D10.3-7), else on pipes; relays it; says its end. One
/// that can't start is refused, saying why; one whose relay fails is
/// killed (never left unwaited for: this process outlives it).
fn relay(
    request: &Request,
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
) -> Result<(), String> {
    let pty = match request
        .terminal
        .map(shipyard_runtime::terminal::host_pty)
        .transpose()
    {
        Ok(pty) => pty,
        Err(error) => return refuse(writer, &format!("no terminal for the command: {error}")),
    };
    let mut child = match spawn(request, pty.as_ref().map(|(_, slave)| slave)) {
        Ok(child) => child,
        Err(why) => return refuse(writer, &why),
    };
    let relayed = relay_running(&mut child, request, pty, (writer, agreed), connection);
    if relayed.is_err() {
        // (Once reaped, neither does anything.)
        let _ = child.kill();
        let _ = child.wait();
    }
    relayed
}

/// [`relay`] once its command (`child`, on `pty` if it has one) runs.
fn relay_running(
    child: &mut Child,
    request: &Request,
    pty: Option<(OwnedFd, OwnedFd)>,
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
) -> Result<(), String> {
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: pty.is_some(),
            stdin: request.input,
        },
    )?;
    let (input, outputs) = if let Some((master, slave)) = pty {
        // Only the command holds its side now: its end is the master's end
        // of output (EIO).
        drop(slave);
        let reader = master
            .try_clone()
            .map_err(|e| format!("the command's terminal: {e}"))?;
        let output = pump(std::fs::File::from(reader), Frame::Stdout, writer)?;
        (Input::Terminal(std::fs::File::from(master)), vec![output])
    } else {
        let input = Input::Pipe(child.stdin.take());
        (input, pump_outputs(child, writer)?)
    };
    let group = i32::try_from(child.id()).ok().map(Pid::from_raw);
    let ended = Arc::new(AtomicBool::new(false));
    pump_input(connection, input, group, Arc::clone(&ended));
    let status = child
        .wait()
        .map_err(|e| format!("waiting for {SHIPYARD}: {e}"))?;
    // From here its group's number may be another's: no signal for it.
    ended.store(true, Ordering::SeqCst);
    // Every output frame before the end.
    for output in outputs {
        let _ = output.join();
    }
    send(writer, &Frame::Exited(exit_code(status)))
}

/// How long the guest's ports may take to be read.
const PORTS_LIMIT: Duration = Duration::from_secs(10);

/// How long a forward's check may use the last ports read: a burst of
/// connections (a browser's, a load test's) reads them once, not once
/// each.
const PORTS_FRESH: Duration = Duration::from_secs(1);

/// The last ports read, and when: one reader at a time (the others wait
/// for its answer).
static LAST_PORTS: Mutex<Option<(std::time::Instant, forward::PortsAnswer)>> = Mutex::new(None);

/// Whether a running container publishes `wanted`: by the last answer if
/// it was read since this asked (yes or no: callers waiting for one read
/// share it, F110), or if it's fresh and says so; else by one read again
/// ([`read_published`]: a port published within the last second isn't
/// refused).
fn is_published(wanted: forward::PublishedPort) -> Result<bool, String> {
    let asked = std::time::Instant::now();
    let mut last = LAST_PORTS
        .lock()
        .map_err(|_| "the ports' lock, poisoned".to_owned())?;
    if let Some((at, answer)) = last.as_ref() {
        let has = answer.ports.contains(&wanted);
        if answers(*at, asked, std::time::Instant::now(), has) {
            return Ok(has);
        }
    }
    // Stamped with when the read began, not ended: a caller that asked
    // while it ran may have asked after what it saw (a port published
    // since), and is answered by it only if it asked before.
    let reading = std::time::Instant::now();
    let answer = read_published()?;
    let published = answer.ports.contains(&wanted);
    *last = Some((reading, answer));
    Ok(published)
}

/// Whether the last ports answer, whose read began at `read`, answers a
/// caller that asked at `asked` (`now`, `has`: whether it has the port):
/// read since it asked, either way; or fresh ([`PORTS_FRESH`]) and saying
/// yes.
fn answers(
    read: std::time::Instant,
    asked: std::time::Instant,
    now: std::time::Instant,
    has: bool,
) -> bool {
    read >= asked || (has && now.saturating_duration_since(read) < PORTS_FRESH)
}

/// What the guest's containers publish, from `__guest-ports` run as a
/// process of its own (a panic or a hang there costs this answer, never
/// the agent and its sessions), within [`PORTS_LIMIT`].
fn read_published() -> Result<forward::PortsAnswer, String> {
    let mut child = guest_command(&[forward::PORTS_REQUEST], "/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("running {SHIPYARD}: {e}"))?;
    let read = read_within(child.stdout.take(), PORTS_LIMIT);
    if read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("waiting for it: {e}"))?;
    let bytes = read?;
    if !status.success() {
        return Err(format!("it ended with {}", exit_code(status)));
    }
    forward::PortsAnswer::decode(&bytes).map_err(|e| e.to_string())
}

/// All `out` gives, up to [`forward::MAX_PORTS_ANSWER`], if it ends within
/// `limit` (read on a thread of its own, so a child that hangs is given up
/// on).
fn read_within(out: Option<std::process::ChildStdout>, limit: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let Some(out) = out else {
        return Err("no output to read".to_owned());
    };
    let (sender, received) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .spawn(move || {
            let mut bytes = Vec::new();
            let cap = u64::try_from(forward::MAX_PORTS_ANSWER).unwrap_or(u64::MAX);
            let read = out.take(cap).read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send(read);
        })
        .map_err(|e| format!("a thread for its answer: {e}"))?;
    match received.recv_timeout(limit) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(format!("its answer can't be read: {error}")),
        Err(_) => Err(format!("no answer within {} s", limit.as_secs())),
    }
}

/// `shipyard ARGS` in `cwd`, with the guest's environment only: its `PATH`
/// and root's home.
fn guest_command(args: &[&str], cwd: &str) -> Command {
    let mut command = Command::new(SHIPYARD);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", "/root");
    command
}

/// How long a forwarded connection waits for its port in the guest.
const FORWARD_CONNECT: Duration = Duration::from_secs(10);

/// [`forward::FORWARD_REQUEST`] `IP PORT`: a connection to the port a
/// running container publishes on `IP` (never the guest's own services),
/// through the guest's own address for it ([`dialled`]: `output`'s DNAT
/// takes it to the container); then the bytes both ways, raw, until both
/// have ended.
fn forward_port(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    ip: &str,
    port: &str,
) -> Result<(), String> {
    let (Ok(ip), Ok(port)) = (ip.parse::<std::net::IpAddr>(), port.parse::<u16>()) else {
        return refuse(writer, &format!("{ip} {port} isn't an address and a port"));
    };
    let wanted = forward::PublishedPort { ip, port };
    let published = match is_published(wanted) {
        Ok(published) => published,
        Err(why) => return refuse(writer, &format!("the guest's ports: {why}")),
    };
    if port == 0 || !published {
        return refuse(
            writer,
            &format!("no running container publishes port {port} on {ip}"),
        );
    }
    let address = std::net::SocketAddr::new(dialled(ip), port);
    let tcp = match std::net::TcpStream::connect_timeout(&address, FORWARD_CONNECT) {
        Ok(tcp) => tcp,
        Err(error) => return refuse(writer, &format!("{address} in the guest: {error}")),
    };
    keep_alive(&tcp);
    let lingers = shipyard_sys::relay::Lingers {
        after_one: LINGER,
        after_other: LINGER,
    };
    welcome_and_relay((writer, agreed), connection, tcp.into(), lingers)
        .map_err(|e| format!("the forwarded connection: {e}"))
}

/// A connection the host asks for instead of a command: to a published
/// port, or to the guest's Docker API.
#[derive(Debug, PartialEq, Eq)]
enum Asked<'a> {
    /// [`forward::FORWARD_REQUEST`] `IP PORT`.
    Port(&'a str, &'a str),
    /// [`forward::API_REQUEST`].
    Api,
    /// [`forward::API_HANG_UP`] `PORT`: the client of the API connection
    /// from the host's `PORT` has gone.
    HangUp(&'a str),
}

/// Which connection `args` ask for, if they ask for one.
fn connection_asked(args: &[String]) -> Option<Asked<'_>> {
    match args {
        [kind, ip, port] if kind == forward::FORWARD_REQUEST => Some(Asked::Port(ip, port)),
        [kind] if kind == forward::API_REQUEST => Some(Asked::Api),
        [kind, port] if kind == forward::API_HANG_UP => Some(Asked::HangUp(port)),
        _ => None,
    }
}

/// Welcomes the host's connection, then relays its bytes (those read
/// past the request first) and `peer`'s both ways, raw, until both have
/// ended (`lingers`: `one` the host's side).
fn welcome_and_relay(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    peer: OwnedFd,
    lingers: shipyard_sys::relay::Lingers,
) -> Result<(), String> {
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: false,
            stdin: true,
        },
    )?;
    let (vsock, unread) = connection.into_parts();
    let mut peer = std::fs::File::from(peer);
    peer.write_all(&unread).map_err(|e| e.to_string())?;
    shipyard_sys::relay::both_ways_lingering(vsock.into(), peer.into(), lingers)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The guest's Docker API socket: root's (`Paths::api_socket`: the
/// runtime's root, [`shipyard_runtime::container::DEFAULT_ROOT`]), given to
/// the service it starts too, so the two agree whatever the agent's own
/// environment.
fn api_socket() -> std::path::PathBuf {
    std::path::Path::new(shipyard_runtime::container::DEFAULT_ROOT).join("api.sock")
}

/// [`forward::API_REQUEST`]: a connection to the guest's Docker API (step
/// 11.5b1, the Mac's `docker`), its service started if none answers (it
/// ends once idle; a thread here reaps the one started); then the bytes
/// both ways, raw, as [`forward_port`]'s ([`forward::API_LINGERS`]).
fn forward_api(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    port: u32,
) -> Result<(), String> {
    let api = match connect_api() {
        Ok(api) => api,
        Err(why) => return refuse(writer, &format!("the guest's Docker API: {why}")),
    };
    // Kept by the host's port while it's relayed: a hang-up names it.
    let _kept = api
        .try_clone()
        .ok()
        .map(|api| KeptApiConnection::keep(port, api));
    welcome_and_relay(
        (writer, agreed),
        connection,
        api.into(),
        forward::API_LINGERS,
    )
    .map_err(|e| format!("the Docker API's connection: {e}"))
}

/// The API connections being relayed, by the host's port, each with an
/// identity of its own: what a hang-up ([`forward::API_HANG_UP`]) reaches.
static API_CONNECTIONS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<u32, (u64, std::os::unix::net::UnixStream)>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// One of [`API_CONNECTIONS`], let go when dropped (its relay's end): only
/// while it's still the one kept by its port (a new connection that took
/// the port since is its own).
struct KeptApiConnection {
    port: u32,
    identity: u64,
}

impl KeptApiConnection {
    fn keep(port: u32, api: std::os::unix::net::UnixStream) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let identity = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        API_CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(port, (identity, api));
        Self { port, identity }
    }
}

impl Drop for KeptApiConnection {
    fn drop(&mut self) {
        let mut kept = API_CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if kept
            .get(&self.port)
            .is_some_and(|(identity, _)| *identity == self.identity)
        {
            kept.remove(&self.port);
        }
    }
}

/// The API connection from the host's `port` shut both ways: the service
/// sees its client hang up (`POLLHUP`), as a client's on Linux is seen,
/// and its relay here ends. One not kept (ended already): nothing.
fn hang_up_api(port: &str) {
    let Ok(port) = port.parse::<u32>() else {
        return;
    };
    if let Some((_, api)) = API_CONNECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&port)
    {
        let _ = api.shutdown(std::net::Shutdown::Both);
    }
}

/// A connection to the guest's Docker API, its service started first if
/// none answers; tried twice, as an idle service may let go of its socket
/// between the check and the connection.
fn connect_api() -> Result<std::os::unix::net::UnixStream, String> {
    let socket = api_socket();
    let service = || {
        let mut command = guest_command(&["system", "service", "--socket"], "/");
        command.arg(&socket);
        command
    };
    let mut tried = 0;
    loop {
        match crate::api_service::ensure(&socket, &service) {
            Ok(None) => {}
            Ok(Some(mut started)) => {
                std::thread::spawn(move || started.wait());
            }
            Err(error) => return Err(error.to_string()),
        }
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(api) => return Ok(api),
            Err(error) if tried > 0 => return Err(error.to_string()),
            Err(_) => tried += 1,
        }
    }
}

/// How long a forwarded connection one side has ended waits for the
/// other's end before it's cut (`shipyard_sys::relay::both_ways`).
const LINGER: Duration = Duration::from_secs(60);

/// The address a port published on `ip` is reached at from the guest
/// itself: its loopback's for every address (DNAT'd in `output`), else
/// that address.
fn dialled(ip: std::net::IpAddr) -> std::net::IpAddr {
    if ip.is_unspecified() {
        std::net::Ipv4Addr::LOCALHOST.into()
    } else {
        ip
    }
}

/// Keepalive on a forwarded connection: once its container's network is
/// gone (removed: nothing answers, nothing resets), it ends within about
/// 90 s rather than holding the Mac's end for good.
fn keep_alive(tcp: &std::net::TcpStream) {
    use nix::sys::socket::{setsockopt, sockopt};
    let _ = setsockopt(tcp, sockopt::KeepAlive, &true);
    let _ = setsockopt(tcp, sockopt::TcpKeepIdle, &60);
    let _ = setsockopt(tcp, sockopt::TcpKeepInterval, &10);
    let _ = setsockopt(tcp, sockopt::TcpKeepCount, &3);
}

/// Where what the client types goes: the command's stdin pipe (none
/// without input), or its terminal's master.
enum Input {
    Pipe(Option<ChildStdin>),
    Terminal(std::fs::File),
}

/// How far behind the Mac's the guest's clock may be before it's set to
/// it: the Mac slept, the guest with it, and its clock stood still.
const CLOCK_BEHIND: u64 = 2;

/// How far ahead it may be before it's set back: further, since a request
/// may have waited (two CLIs' clocks never pull it back and forth).
const CLOCK_AHEAD: u64 = 30;

/// Sets the guest's clock to the Mac's (`theirs`, seconds since the epoch)
/// if they're far enough apart ([`skewed`]: TLS to a registry fails on a
/// clock that's behind).
fn follow_clock(theirs: u64) {
    let ours = shipyard_sys::time::unix_seconds();
    if !skewed(ours, theirs) {
        return;
    }
    let to = nix::sys::time::TimeSpec::from_duration(Duration::from_secs(theirs));
    match nix::time::clock_settime(nix::time::ClockId::CLOCK_REALTIME, to) {
        Ok(()) => say(&format!(
            "the clock was {ours}, the Mac's {theirs}: set to the Mac's"
        )),
        Err(error) => say(&format!("setting the clock to the Mac's: {error}")),
    }
}

/// Whether the guest's clock (`ours`) is far enough from the Mac's
/// (`theirs`, seconds) to be set: [`CLOCK_BEHIND`] behind, or
/// [`CLOCK_AHEAD`] ahead.
pub(super) fn skewed(ours: u64, theirs: u64) -> bool {
    if ours < theirs {
        theirs - ours > CLOCK_BEHIND
    } else {
        ours - theirs > CLOCK_AHEAD
    }
}

/// Runs the request's command: `shipyard ARGS`, in its directory, with the
/// guest's environment, the caller's few variables and those it named. On
/// `terminal` (a pseudo-terminal's slave), as a session leader whose
/// controlling terminal it is; else in a process group of its own (to be
/// hung up on), its stdin empty without input.
fn spawn(request: &Request, terminal: Option<&OwnedFd>) -> Result<Child, String> {
    let env = request.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    let args: Vec<&str> = request.args.iter().map(String::as_str).collect();
    let mut command = guest_command(&args, &request.cwd);
    // The caller's named variables in one of their own, for its `-e KEY`
    // to find: never `shipyard`'s own environment ([`forward::CALLER_ENV`]).
    let named: Vec<(String, String)> = request
        .named_env
        .iter()
        .filter(|(name, _)| !forward::reserved(name))
        .cloned()
        .collect();
    command
        .envs(forward::passed_env(env))
        .env(forward::CALLER_ENV, forward::caller_env_value(&named));
    // A credential handed to a push or pull (step 12.4, D12.4-2): on a pipe
    // at its descriptor, read whole and closed by the command first thing;
    // never in an environment (no child of the command inherits it). Held
    // here until the command is spawned.
    let handed = request
        .registry_auth
        .as_ref()
        .map(|auth| handed_pipe(&auth.0))
        .transpose()?;
    if let Some(read) = &handed {
        let from = read.as_raw_fd();
        command.env(
            forward::REGISTRY_AUTH_FD_ENV,
            forward::REGISTRY_AUTH_FD.to_string(),
        );
        // SAFETY: between fork and exec, `dup2` and `fcntl` only:
        // async-signal-safe, no allocation, no lock.
        unsafe {
            command.pre_exec(move || at_descriptor(from, forward::REGISTRY_AUTH_FD));
        }
    }
    if let Some(slave) = terminal {
        let end = |slave: &OwnedFd| {
            slave
                .try_clone()
                .map(Stdio::from)
                .map_err(|e| format!("the command's terminal: {e}"))
        };
        command
            .stdin(end(slave)?)
            .stdout(end(slave)?)
            .stderr(end(slave)?);
        // SAFETY: between fork and exec, two system calls only:
        // async-signal-safe, no allocation, no lock.
        unsafe {
            command.pre_exec(shipyard_runtime::terminal::take_controlling_terminal);
        }
    } else {
        command
            .stdin(if request.input {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
    }
    command
        .spawn()
        .map_err(|e| format!("running {SHIPYARD}: {e}"))
}

/// The most a handed credential may be: well under a pipe's buffer, so
/// writing it whole before the command runs can't block.
const MAX_HANDED: usize = 16 * 1024;

/// A pipe holding `auth` whole, its write end closed: its read end.
fn handed_pipe(auth: &str) -> Result<OwnedFd, String> {
    if auth.len() > MAX_HANDED {
        return Err(format!("the handed credential is over {MAX_HANDED} bytes"));
    }
    let (read, mut write) =
        std::io::pipe().map_err(|e| format!("the handed credential's pipe: {e}"))?;
    write
        .write_all(auth.as_bytes())
        .map_err(|e| format!("the handed credential's pipe: {e}"))?;
    drop(write);
    Ok(OwnedFd::from(read))
}

/// After `fork`: descriptor `from` placed at `to`, open across `exec`.
/// `dup2(fd, fd)` does nothing, close-on-exec included, so the flag is
/// cleared either way (AGENTS.md's gotcha).
fn at_descriptor(from: std::os::fd::RawFd, to: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: plain system calls on numbers; async-signal-safe.
    unsafe {
        if from != to && libc::dup2(from, to) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(to, libc::F_SETFD, 0) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Sends what the command writes on its pipes, as it comes: a thread for
/// its stdout, one for its stderr.
fn pump_outputs(
    child: &mut Child,
    writer: &Stream,
) -> Result<Vec<std::thread::JoinHandle<()>>, String> {
    let mut threads = Vec::new();
    if let Some(out) = child.stdout.take() {
        threads.push(pump(out, Frame::Stdout, writer)?);
    }
    if let Some(err) = child.stderr.take() {
        threads.push(pump(err, Frame::Stderr, writer)?);
    }
    Ok(threads)
}

/// Sends what `source` gives, each piece as a frame `framed` makes (its
/// stdout's or its stderr's), on a thread done at its end (a pipe's end of
/// file, a terminal master's EIO).
fn pump(
    mut source: impl std::io::Read + Send + 'static,
    framed: fn(Vec<u8>) -> Frame,
    writer: &Stream,
) -> Result<std::thread::JoinHandle<()>, String> {
    let writer = Arc::clone(writer);
    std::thread::Builder::new()
        .spawn(move || {
            let mut buffer = vec![0; 32 * 1024];
            while let Ok(read @ 1..) = source.read(&mut buffer) {
                if send(&writer, &framed(buffer[..read].to_vec())).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| format!("a thread for the command's output: {e}"))
}

/// What the client sends, on a thread of its own: its typing to the
/// command, its window's size to the command's terminal, its signals to the
/// command's group (on a terminal, its foreground group: what a key would
/// signal); the client gone, the command hung up on (as a terminal's close
/// would). Nothing is signalled once the command has `ended`.
fn pump_input(
    mut connection: Connection<std::fs::File>,
    mut input: Input,
    group: Option<Pid>,
    ended: Arc<AtomicBool>,
) {
    let pumped = std::thread::Builder::new().spawn(move || {
        let send_signal = |input: &Input, signal| {
            if ended.load(Ordering::SeqCst) {
                return;
            }
            let foreground = match input {
                Input::Terminal(master) => nix::unistd::tcgetpgrp(master).ok(),
                Input::Pipe(_) => None,
            };
            if let Some(target) = signal_target(foreground, group) {
                let _ = killpg(target, signal);
            }
        };
        loop {
            match connection.next_frame() {
                Ok(Some(Frame::Stdin(bytes))) => match &mut input {
                    Input::Pipe(stdin) => {
                        if let Some(pipe) = stdin.as_mut()
                            && pipe.write_all(&bytes).is_err()
                        {
                            *stdin = None;
                        }
                    }
                    Input::Terminal(master) => {
                        let _ = master.write_all(&bytes);
                    }
                },
                Ok(Some(Frame::StdinEnd)) => {
                    if let Input::Pipe(stdin) = &mut input {
                        *stdin = None;
                    }
                }
                Ok(Some(Frame::Resize { rows, cols })) => {
                    if let Input::Terminal(master) = &input {
                        let size = shipyard_runtime::terminal::Size::new(rows, cols);
                        let _ =
                            shipyard_runtime::terminal::set_window_size(master.as_raw_fd(), size);
                    }
                }
                Ok(Some(Frame::Signal(number))) => {
                    if let Some(forwarded) = forwarded(number) {
                        send_signal(&input, forwarded);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    send_signal(&input, Signal::SIGHUP);
                    return;
                }
            }
        }
    });
    if let Err(error) = pumped {
        say(&format!("no thread for the client's input: {error}"));
    }
}

/// Whose group a client's signal goes to: the command's terminal's
/// `foreground` group, else the command's own `group`. A terminal whose
/// session has ended has none, and says 0, which `killpg` would take for
/// this process's own group (and PID 1's, were they one): never a target,
/// nor is 1.
fn signal_target(foreground: Option<Pid>, group: Option<Pid>) -> Option<Pid> {
    foreground.filter(|pgrp| pgrp.as_raw() > 1).or(group)
}

/// The signal a client's `Signal` frame asks for, if it's one a client may
/// pass on ([`shipyard_runtime::process::is_forwardable`]: never SIGKILL
/// or SIGSTOP, never a fault's).
pub(super) fn forwarded(number: u8) -> Option<Signal> {
    let number = i32::from(number);
    if !shipyard_runtime::process::is_forwardable(number) {
        return None;
    }
    Signal::try_from(number).ok()
}

/// A command's end as an exit code: its own, or 128 plus the signal that
/// ended it (as a shell says it).
pub(super) fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(i32::from(shipyard_runtime::process::EXIT_ENGINE_ERROR))
}

/// Sends `frame` to the client.
fn send(writer: &Stream, frame: &Frame) -> Result<(), String> {
    let mut writer = writer
        .lock()
        .map_err(|_| "a writer's lock, poisoned".to_owned())?;
    forward::send(&mut *writer, frame).map_err(|e| format!("writing to the connection: {e}"))
}

/// Refuses the client, saying why.
fn refuse(writer: &Stream, why: &str) -> Result<(), String> {
    send(writer, &Frame::Refused(why.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ports answer read after a caller asked answers it, yes or no
    /// (waiters share one read, F110); an older one, only while fresh
    /// and saying yes.
    #[test]
    fn a_ports_answer_since_the_ask_is_shared() {
        use std::time::{Duration, Instant};
        let asked = Instant::now();
        let later = asked + Duration::from_millis(5);
        assert!(answers(later, asked, later, false), "read since: a no");
        assert!(answers(later, asked, later, true), "read since: a yes");
        let before = asked.checked_sub(Duration::from_millis(5)).unwrap();
        assert!(answers(before, asked, asked, true), "fresh, a yes");
        assert!(!answers(before, asked, asked, false), "read before: a no");
        let stale = asked + PORTS_FRESH + Duration::from_millis(10);
        assert!(!answers(before, asked, stale, true), "stale");
    }

    /// A signal goes to the terminal's foreground group, else the command's
    /// own; a terminal with no session left (0), or 1, is never a target.
    #[test]
    fn a_signal_never_goes_to_this_group_or_init() {
        let (job, command) = (Pid::from_raw(40), Some(Pid::from_raw(30)));
        assert_eq!(signal_target(Some(job), command), Some(job));
        assert_eq!(signal_target(None, command), command);
        assert_eq!(signal_target(Some(Pid::from_raw(0)), command), command);
        assert_eq!(signal_target(Some(Pid::from_raw(1)), command), command);
        assert_eq!(signal_target(Some(Pid::from_raw(0)), None), None);
    }

    /// A port published on every address is reached at the guest's
    /// loopback (DNAT'd in `output`); one on an address of its own, there.
    #[test]
    fn a_published_port_is_dialled_where_its_dnat_is() {
        let ip = |text: &str| text.parse::<std::net::IpAddr>().unwrap();
        assert_eq!(dialled(ip("0.0.0.0")), ip("127.0.0.1"));
        assert_eq!(dialled(ip("::")), ip("127.0.0.1"));
        assert_eq!(dialled(ip("10.88.0.1")), ip("10.88.0.1"));
        assert_eq!(dialled(ip("127.0.0.1")), ip("127.0.0.1"));
    }

    /// A port's or the API's connection is asked by its exact arguments;
    /// anything else is a command (never a connection by a near miss).
    #[test]
    fn a_connection_is_asked_by_its_args_alone() {
        let args = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        let port = args(&[forward::FORWARD_REQUEST, "10.0.0.2", "80"]);
        assert_eq!(connection_asked(&port), Some(Asked::Port("10.0.0.2", "80")));
        assert_eq!(
            connection_asked(&args(&[forward::API_REQUEST])),
            Some(Asked::Api)
        );
        assert_eq!(
            connection_asked(&args(&[forward::API_HANG_UP, "1234"])),
            Some(Asked::HangUp("1234"))
        );
        for command in [
            args(&[forward::API_REQUEST, "x"]),
            args(&[forward::API_HANG_UP]),
            args(&[forward::FORWARD_REQUEST, "10.0.0.2"]),
            args(&["ps"]),
            args(&[]),
        ] {
            assert_eq!(connection_asked(&command), None, "{command:?}");
        }
    }

    /// Only the host is served: the guest's own CID (a process of its),
    /// loopback's, any other, or one that can't be read, isn't.
    #[test]
    fn only_the_host_is_served() {
        assert!(from_host(Some(2)));
        for cid in [Some(0), Some(1), Some(3), Some(u32::MAX), None] {
            assert!(!from_host(cid), "{cid:?}");
        }
    }

    /// A client may pass on an interrupt, a stop request, a hang-up; not a
    /// kill, a stop, a fault, or a number that isn't a signal.
    #[test]
    fn only_forwardable_signals_pass() {
        for (number, signal) in [
            (2, Signal::SIGINT),
            (15, Signal::SIGTERM),
            (1, Signal::SIGHUP),
        ] {
            assert_eq!(forwarded(number), Some(signal), "{number}");
        }
        for number in [0, 9, 19, 11, 17, 200] {
            assert_eq!(forwarded(number), None, "{number}");
        }
    }

    /// The clock follows the Mac's only past the skew allowed, either way.
    #[test]
    fn a_clock_is_set_only_when_it_drifted() {
        assert!(!skewed(1_000, 1_002), "behind, a little");
        assert!(skewed(1_000, 1_003), "behind");
        assert!(!skewed(1_030, 1_000), "ahead, by a wait's worth");
        assert!(skewed(1_031, 1_000), "ahead");
    }

    /// An exit code is the command's, or 128 plus the signal that ended it.
    #[test]
    fn a_signal_is_128_plus_its_number() {
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(9)), 137);
    }
}
