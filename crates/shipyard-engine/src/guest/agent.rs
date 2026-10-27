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

use nix::sys::signal::{Signal, kill};
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

/// How long a client has to say its hello.
const HELLO_WAIT: Duration = Duration::from_secs(10);

/// Sets `stream`'s read timeout (`SO_RCVTIMEO`): `None` waits for ever. A
/// failure leaves it as it was.
fn set_read_wait(stream: &std::fs::File, wait: Option<Duration>) {
    let wait = wait.map_or(nix::sys::time::TimeVal::new(0, 0), |wait| {
        nix::sys::time::TimeVal::new(i64::try_from(wait.as_secs()).unwrap_or(i64::MAX), 0)
    });
    let _ = nix::sys::socket::setsockopt(stream, nix::sys::socket::sockopt::ReceiveTimeout, &wait);
}

/// One connection's stream: what's written to it from several threads.
type Stream = Arc<Mutex<std::fs::File>>;

/// The agent's run: never returns.
pub(super) fn run() -> ! {
    let _ = STARTED.set(std::time::Instant::now());
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

/// The agent's listening vsock socket, bound to [`forward::AGENT_PORT`]
/// (close-on-exec): what PID 1 makes first thing and keeps (F108).
///
/// # Errors
///
/// The socket's, the bind's (the port taken) or the listen's.
pub(super) fn bound_listener() -> Result<OwnedFd, String> {
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
    Ok(listener)
}

/// Whether descriptor `fd` is a vsock socket on [`forward::AGENT_PORT`]:
/// the agent's (one that isn't open, or isn't a socket, isn't).
pub(super) fn is_agent_socket(fd: std::os::fd::RawFd) -> bool {
    nix::sys::socket::getsockname::<VsockAddr>(fd)
        .is_ok_and(|address| address.port() == forward::AGENT_PORT)
}

/// The listener PID 1 handed this agent at [`forward::AGENT_FD`] (a vsock
/// socket on [`forward::AGENT_PORT`]), taken over close-on-exec. None (an
/// agent PID 1 didn't start, or a hand-over gone wrong) is an error, said:
/// the port is PID 1's, and an agent that bound another would never be
/// reached.
fn listener() -> Result<OwnedFd, String> {
    if is_agent_socket(forward::AGENT_FD) {
        // SAFETY: PID 1 left it for this process alone, and nothing here has
        // opened anything yet that could be it.
        return unsafe {
            shipyard_sys::fd::take_inherited(forward::AGENT_FD, shipyard_sys::fd::OnExec::Close)
        }
        .map_err(|e| format!("taking the agent's socket: {e}"));
    }
    Err(format!(
        "no listening socket at descriptor {} (the agent is started by the guest's init)",
        forward::AGENT_FD
    ))
}

/// Accepts the host's connections, each on a thread of its own.
fn serve() -> Result<(), String> {
    let listener = listener()?;
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
    // The hello within a bound (F109): a client that connects and says
    // nothing doesn't hold a thread for good. Cleared once it's read, on
    // every path: a forwarded port's quiet stream is never cut by it.
    set_read_wait(&stream, Some(HELLO_WAIT));
    let mut connection = Connection::new(stream);
    // The version both speak, as a monitor agrees it: the older one's (a
    // newer client is answered at this guest's, never refused for it).
    let hello = connection.next_frame().map_err(|e| e.to_string());
    if let Ok(stream) = writer.lock() {
        set_read_wait(&stream, None);
    }
    let (agreed, request) = match hello? {
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
        // Another build's request: said so, for the Mac to stop this VM
        // and start its own (F167).
        Err(shipyard_runtime::attach::forward::RequestError::Newer(_)) => {
            return send(&writer, &forward::Refusal::Newer.frame(agreed));
        }
        Err(error) => return refuse(&writer, &error.to_string()),
    };
    if request.args == [forward::STOP_REQUEST] {
        // PID 1 told first: the host may have gone already (it doesn't wait
        // for the answer), and a failed answer mustn't keep the stop back.
        let told = kill(Pid::from_raw(1), Signal::SIGTERM);
        let _ = send(&writer, &Frame::Exited(i32::from(told.is_err())));
        return told.map_err(|e| format!("telling the guest's init to stop: {e}"));
    }
    if request.args == [forward::BOOT_REQUEST] {
        // Answered before the guest is ready: a long check of the data disk
        // mustn't look like a hang to the host's watch.
        return answer_boot(&writer, agreed);
    }
    if !std::path::Path::new(super::READY).exists() {
        // Said by its code to a client that reads one (F99).
        return send(&writer, &forward::Refusal::NotReady.frame(agreed));
    }
    match connection_asked(&request.args) {
        Some(Asked::Port(ip, port)) => {
            return forward_port((&writer, agreed), connection, ip, port);
        }
        Some(Asked::Udp(ip, port)) => {
            return forward_udp((&writer, agreed), connection, ip, port);
        }
        Some(Asked::Control(token)) => return control((&writer, agreed), connection, token),
        Some(Asked::Api) => return forward_api((&writer, agreed), connection, port),
        Some(Asked::HangUp(theirs)) => {
            hang_up_api(theirs);
            return Ok(());
        }
        None => {}
    }
    // A request that waited in the backlog while the agent restarted (its
    // client may have given up since) isn't run, and its old stamp never
    // sets the clock back (F108).
    if let Some(theirs) = request.time {
        let since_start = STARTED
            .get()
            .map_or(Duration::ZERO, std::time::Instant::elapsed);
        let ours = shipyard_sys::time::unix_seconds();
        if waited_too_long(ours, theirs, since_start) {
            return refuse(
                &writer,
                "this command waited too long for the VM (its agent was restarting): run it again",
            );
        }
        if follows(ours, theirs, since_start) {
            follow_clock(theirs);
        }
    }
    if let Some(why) = not_a_directory_here(&request.cwd) {
        return refuse(&writer, &why);
    }
    relay(&request, (&writer, agreed), connection)
}

/// Why `cwd` (a forwarded command's directory) can't be the command's, if
/// it can't: not in the guest, or in a folder macOS hasn't let the VM read
/// (named with the permission, F140).
fn not_a_directory_here(cwd: &str) -> Option<String> {
    match std::fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => None,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Some(
            super::shares::Shared::here()
                .ok()
                .flatten()
                .and_then(|shared| {
                    super::shares::privacy_hint(shared.home(), std::path::Path::new(cwd))
                })
                .unwrap_or_else(|| format!("{cwd}: {error}")),
        ),
        _ => Some(format!(
            "{cwd} isn't in the guest: is it in a place the VM shares (your home, /Volumes, \
             /tmp)?"
        )),
    }
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
    let group = i32::try_from(child.id()).ok().map(Pid::from_raw);
    let ended = Arc::new(AtomicBool::new(false));
    let master = pty.as_ref().and_then(|(master, _)| master.try_clone().ok());
    let controlled = Arc::new(super::control::Controlled::new(
        group,
        master,
        Arc::clone(&ended),
    ));
    // A user's command gets a control connection of its own (F94): its
    // token said to a client that reads one, 0 if none could be made (the
    // client then stays in-band); the host's own asks never get one. Said
    // right after the welcome, before anything pumps the command's output:
    // the client reads it as the next frame.
    let session =
        agreed >= shipyard_runtime::attach::SESSION_VERSION && !forward::is_internal(&request.args);
    let registered = session
        .then(|| super::control::Registered::new(Arc::clone(&controlled)))
        .flatten();
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: pty.is_some(),
            stdin: request.input,
        },
    )?;
    if session {
        let token = registered
            .as_ref()
            .map_or(0, super::control::Registered::token);
        send(writer, &Frame::Session(token))?;
    }
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
    pump_input(connection, input, Arc::clone(&controlled));
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
    let version = forward::PORTS_VERSION.to_string();
    let mut child = guest_command(&[forward::PORTS_REQUEST, &version], "/")
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

/// `shipyard ARGS` in `cwd`, with the guest's environment only: its `PATH`,
/// root's home and its own temporary place (its `/tmp` is the Mac's).
fn guest_command(args: &[&str], cwd: &str) -> Command {
    let mut command = Command::new(SHIPYARD);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", "/root")
        .env("TMPDIR", super::GUEST_TMP);
    command
}

/// How long a forwarded connection waits for its port in the guest.
const FORWARD_CONNECT: Duration = Duration::from_secs(10);

/// Whether a TCP connection was ever dialled through the guest's own
/// address (said once: a pasta network's port, or a lease unread).
static DNAT_SAID: AtomicBool = AtomicBool::new(false);

/// [`forward::FORWARD_REQUEST`]'s and [`forward::FORWARD_UDP_REQUEST`]'s
/// `IP PORT`, as the port a running container publishes with `protocol`,
/// and an engine to find its container with: `None` once refused (said to
/// `writer`).
///
/// # Errors
///
/// The refusal's write.
fn published_or_refuse(
    writer: &Stream,
    (ip, port): (&str, &str),
    protocol: forward::Protocol,
) -> Result<Option<(forward::PublishedPort, crate::Engine)>, String> {
    let (Ok(ip), Ok(port)) = (ip.parse::<std::net::IpAddr>(), port.parse::<u16>()) else {
        refuse(writer, &format!("{ip} {port} isn't an address and a port"))?;
        return Ok(None);
    };
    let wanted = forward::PublishedPort { ip, port, protocol };
    let published = match is_published(wanted) {
        Ok(published) => published,
        Err(why) => {
            refuse(writer, &format!("the guest's ports: {why}"))?;
            return Ok(None);
        }
    };
    if port == 0 || !published {
        refuse(
            writer,
            &format!("no running container publishes {protocol} port {port} on {ip}"),
        )?;
        return Ok(None);
    }
    let engine = crate::Engine::new(crate::Paths::from_env(), Arc::new(crate::report::Quiet));
    Ok(Some((wanted, engine)))
}

/// [`forward::FORWARD_REQUEST`] `IP PORT`: a connection to the port a
/// running container publishes on `IP` (never the guest's own services),
/// dialled at the container's own address ([`super::forwarded::find_target`],
/// F97); then the bytes both ways, raw, until both have ended.
fn forward_port(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    ip: &str,
    port: &str,
) -> Result<(), String> {
    let Some((wanted, engine)) = published_or_refuse(writer, (ip, port), forward::Protocol::Tcp)?
    else {
        return Ok(());
    };
    // The container itself; one no lease explains (a pasta network's),
    // through the guest's DNAT, said once.
    let address = super::forwarded::find_target(&engine, &wanted).unwrap_or_else(|| {
        if !DNAT_SAID.swap(true, Ordering::SeqCst) {
            say(&format!(
                "port {} on {}: no container's address for it, dialled through the guest's own \
                 address",
                wanted.port, wanted.ip
            ));
        }
        std::net::SocketAddr::new(dialled(wanted.ip), wanted.port)
    });
    let tcp = match std::net::TcpStream::connect_timeout(&address, FORWARD_CONNECT) {
        Ok(tcp) => tcp,
        Err(error) => return refuse(writer, &format!("{address} in the guest: {error}")),
    };
    keep_alive(&tcp);
    let lingers = shipyard_sys::relay::Lingers {
        after_one: LINGER,
        after_other: LINGER,
    };
    welcome_and_relay(
        (writer, agreed),
        connection,
        tcp.into(),
        (lingers, Named::NotNamed),
    )
    .map_err(|e| format!("the forwarded connection: {e}"))
}

/// [`forward::CONTROL_REQUEST`] `TOKEN`: the control of the command `token`
/// names ([`super::control`]), welcomed, then served until it ends. One no
/// command runs under (ended, or never given) is refused.
fn control(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    token: &str,
) -> Result<(), String> {
    let Some(controlled) = forward::session_token_of(token).and_then(super::control::find) else {
        return refuse(writer, "no command runs under that token");
    };
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: false,
            stdin: true,
        },
    )?;
    super::control::serve(connection, &controlled);
    Ok(())
}

/// [`forward::FORWARD_UDP_REQUEST`] `IP PORT`: the Mac's datagrams to a UDP
/// port a running container publishes on `IP`, relayed to the container
/// itself ([`super::forwarded::relay_udp`]: its address looked up again as
/// it goes). One no lease explains is refused: the guest's own address
/// would take its datagrams into the socket the port's holder keeps, never
/// answering.
fn forward_udp(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    ip: &str,
    port: &str,
) -> Result<(), String> {
    let Some((wanted, engine)) = published_or_refuse(writer, (ip, port), forward::Protocol::Udp)?
    else {
        return Ok(());
    };
    if super::forwarded::find_target(&engine, &wanted).is_none() {
        return refuse(
            writer,
            &format!(
                "UDP port {} on {}: no container's address for it",
                wanted.port, wanted.ip
            ),
        );
    }
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: false,
            stdin: true,
        },
    )?;
    let (vsock, unread) = connection.into_parts();
    super::forwarded::relay_udp(&vsock, &unread, || {
        super::forwarded::find_target(&engine, &wanted)
    })
    .map_err(|e| format!("the forwarded UDP port: {e}"))
}

/// A connection the host asks for instead of a command: to a published
/// port, or to the guest's Docker API.
#[derive(Debug, PartialEq, Eq)]
enum Asked<'a> {
    /// [`forward::FORWARD_REQUEST`] `IP PORT`.
    Port(&'a str, &'a str),
    /// [`forward::FORWARD_UDP_REQUEST`] `IP PORT`.
    Udp(&'a str, &'a str),
    /// [`forward::CONTROL_REQUEST`] `TOKEN`.
    Control(&'a str),
    /// [`forward::API_REQUEST`].
    Api,
    /// [`forward::API_HANG_UP`] `NAME`: the client of the API connection
    /// `NAME` names (its token, or from an older host the host's port) has
    /// gone.
    HangUp(&'a str),
}

/// Which connection `args` ask for, if they ask for one.
fn connection_asked(args: &[String]) -> Option<Asked<'_>> {
    match args {
        [kind, ip, port] if kind == forward::FORWARD_REQUEST => Some(Asked::Port(ip, port)),
        [kind, ip, port] if kind == forward::FORWARD_UDP_REQUEST => Some(Asked::Udp(ip, port)),
        [kind, token] if kind == forward::CONTROL_REQUEST => Some(Asked::Control(token)),
        [kind] if kind == forward::API_REQUEST => Some(Asked::Api),
        [kind, name] if kind == forward::API_HANG_UP => Some(Asked::HangUp(name)),
        _ => None,
    }
}

/// Whether a relayed connection's welcome is followed by a token, to a
/// client of version 6 or later ([`Frame::Session`]).
#[derive(Debug, Clone, Copy)]
enum Named {
    /// None: a port's connection, which nothing hangs up.
    NotNamed,
    /// An API connection's, named by its token for a hang-up (`None`: none
    /// could be made, said as 0, so its client names it by its port).
    ByToken(Option<u128>),
}

/// Welcomes the host's connection, then relays its bytes (those read
/// past the request first) and `peer`'s both ways, raw, until both have
/// ended (`lingers`: `one` the host's side).
fn welcome_and_relay(
    (writer, agreed): (&Stream, u32),
    connection: Connection<std::fs::File>,
    peer: OwnedFd,
    (lingers, named): (shipyard_sys::relay::Lingers, Named),
) -> Result<(), String> {
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: false,
            stdin: true,
        },
    )?;
    if let Named::ByToken(token) = named
        && agreed >= shipyard_runtime::attach::SESSION_VERSION
    {
        send(writer, &Frame::Session(token.unwrap_or(0)))?;
    }
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
    // Kept by the host's port, and by a token of its own, while it's
    // relayed: a hang-up names it by either (F155: a port can be another's
    // by then, a token never is).
    let token = super::control::new_token();
    let _kept = KeptApiConnection::keep(port, token, &api);
    welcome_and_relay(
        (writer, agreed),
        connection,
        api.into(),
        (forward::API_LINGERS, Named::ByToken(token)),
    )
    .map_err(|e| format!("the Docker API's connection: {e}"))
}

/// The API connections being relayed, each under its names (the host's
/// port, in decimal; its token, 32 hex digits), with an identity of its
/// own: what a hang-up ([`forward::API_HANG_UP`]) reaches.
static API_CONNECTIONS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, (u64, std::os::unix::net::UnixStream)>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// One of [`API_CONNECTIONS`], let go when dropped (its relay's end): each
/// name only while it's still this one's (a new connection that took the
/// port since is its own).
struct KeptApiConnection {
    names: Vec<String>,
    identity: u64,
}

impl KeptApiConnection {
    fn keep(port: u32, token: Option<u128>, api: &std::os::unix::net::UnixStream) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let identity = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let names: Vec<String> = std::iter::once(port.to_string())
            .chain(token.map(forward::session_token_text))
            .collect();
        let mut kept = API_CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in &names {
            if let Ok(copy) = api.try_clone() {
                kept.insert(name.clone(), (identity, copy));
            }
        }
        Self { names, identity }
    }
}

impl Drop for KeptApiConnection {
    fn drop(&mut self) {
        let mut kept = API_CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in &self.names {
            if kept
                .get(name)
                .is_some_and(|(identity, _)| *identity == self.identity)
            {
                kept.remove(name);
            }
        }
    }
}

/// The API connection `name` names (the host's port from an older host,
/// else its token) shut both ways: the service sees its client hang up
/// (`POLLHUP`), as a client's on Linux is seen, and its relay here ends.
/// One not kept (ended already): nothing.
fn hang_up_api(name: &str) {
    if let Some((_, api)) = API_CONNECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
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

/// The guest's boot ID ([`forward::BOOT_REQUEST`]): `Welcome`, the ID, then
/// `Exited` 0 ([`forward::BOOT_UNREADABLE`] if the kernel won't say it).
fn answer_boot(writer: &Stream, agreed: u32) -> Result<(), String> {
    send(
        writer,
        &Frame::Welcome {
            version: agreed,
            terminal: false,
            stdin: false,
        },
    )?;
    match std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(id) => {
            send(writer, &Frame::Stdout(id.trim().as_bytes().to_vec()))?;
            send(writer, &Frame::Exited(0))
        }
        Err(error) => {
            send(
                writer,
                &Frame::Stderr(format!("the boot ID: {error}").into_bytes()),
            )?;
            send(writer, &Frame::Exited(forward::BOOT_UNREADABLE))
        }
    }
}

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

/// How long a request may have waited for the agent before it's run: past
/// this, its client has likely given up (F108's backlog).
const STALE_REQUEST: u64 = 60;

/// When this agent started: requests can have waited for it only in its
/// first [`STALE_REQUEST`] seconds (PID 1's backlog, drained at once).
static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Whether a request stamped `theirs` (the Mac's clock) waited too long for
/// the agent: past [`STALE_REQUEST`] by the guest's clock (`ours`), in the
/// agent's first [`STALE_REQUEST`] (`since_start`). Later, nothing can have
/// waited for it (it's up, taking each at once): an old stamp is a guest
/// whose clock ran ahead, set right by the request instead.
fn waited_too_long(ours: u64, theirs: u64, since_start: Duration) -> bool {
    early(since_start) && ours.saturating_sub(theirs) > STALE_REQUEST
}

/// Whether the guest's clock follows a request stamped `theirs`: always
/// forward (the Mac slept, the guest's clock stood still), and back only
/// past an agent's first [`STALE_REQUEST`] (`since_start`), when an old
/// stamp is the guest's clock ahead, not a request that waited for the
/// agent (whose stamp would move the clock backwards by its wait).
fn follows(ours: u64, theirs: u64, since_start: Duration) -> bool {
    theirs >= ours || !early(since_start)
}

/// Whether an agent `since_start` old is in its first [`STALE_REQUEST`]:
/// when what waited for it in PID 1's backlog comes.
fn early(since_start: Duration) -> bool {
    since_start < Duration::from_secs(STALE_REQUEST)
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
pub(super) fn at_descriptor(
    from: std::os::fd::RawFd,
    to: std::os::fd::RawFd,
) -> std::io::Result<()> {
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
/// command (`controlled`: on a terminal, its foreground group, what a key
/// would signal); the client gone, the command hung up on (as a terminal's
/// close would). Nothing is signalled once the command has ended. (A client
/// with a control connection sends its signals there: they never wait
/// behind its input here.)
fn pump_input(
    mut connection: Connection<std::fs::File>,
    mut input: Input,
    controlled: Arc<super::control::Controlled>,
) {
    let pumped = std::thread::Builder::new().spawn(move || {
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
                Ok(Some(frame @ (Frame::Resize { .. } | Frame::Signal(_)))) => {
                    controlled.apply(&frame);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    controlled.signal(Signal::SIGHUP);
                    return;
                }
            }
        }
    });
    if let Err(error) = pumped {
        say(&format!("no thread for the client's input: {error}"));
    }
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

    /// In an agent's first minute, a request that waited past a minute for it
    /// isn't run (each of a backlog, the second too: the first never set the
    /// clock back); one just sent, or stamped ahead (the Mac's clock ahead),
    /// is. Later, an old stamp is the guest's clock ahead: served (its clock
    /// set right).
    #[test]
    fn a_request_that_waited_too_long_is_refused() {
        let early = Duration::from_secs(1);
        assert!(!waited_too_long(1_000, 1_000, early));
        assert!(!waited_too_long(1_060, 1_000, early));
        assert!(waited_too_long(1_100, 1_000, early));
        assert!(
            waited_too_long(1_100, 1_005, early),
            "the next of the backlog"
        );
        assert!(
            !waited_too_long(1_000, 5_000, early),
            "the Mac's clock ahead"
        );
        assert!(!waited_too_long(1_100, 1_000, Duration::from_secs(61)));
        // The clock: forward always; back (a 45 s wait's stamp) only past the
        // agent's first minute.
        assert!(follows(1_000, 1_500, early));
        assert!(!follows(1_045, 1_000, early));
        assert!(follows(1_045, 1_000, Duration::from_secs(61)));
    }

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

    /// Every request of the host's own is internal (no session token: the
    /// ports poll, `doctor`, the boot ID read their answers whole), and a
    /// user's command never is.
    #[test]
    fn the_hosts_own_requests_are_internal() {
        for own in [
            forward::STOP_REQUEST,
            forward::BOOT_REQUEST,
            forward::PORTS_REQUEST,
            forward::DOCTOR_REQUEST,
            forward::FORWARD_REQUEST,
            forward::FORWARD_UDP_REQUEST,
            forward::CONTROL_REQUEST,
            forward::API_REQUEST,
            forward::API_HANG_UP,
        ] {
            assert!(
                forward::is_internal(&Request::internal(&[own]).args),
                "{own}"
            );
        }
        for command in [&["run", "-i", "x"][..], &["ps"], &[], &["guest-ports"]] {
            assert!(
                !forward::is_internal(&Request::internal(command).args),
                "{command:?}"
            );
        }
    }

    /// An API connection is kept under its port and its token, either of
    /// which hangs it up; one that took the port since keeps it when the
    /// first's relay ends, and both names go with their own connection.
    #[test]
    fn an_api_connection_is_named_by_its_port_and_its_token() {
        use std::io::Read as _;
        let (first, mut first_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (second, mut second_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let named = |name: &str| API_CONNECTIONS.lock().unwrap().contains_key(name);
        // (Ports past any real vsock one's: no test beside this shares them.)
        let (port, token) = (4_000_000_001, 0x5eed_u128);
        let token_name = forward::session_token_text(token);
        let kept_first = KeptApiConnection::keep(port, Some(token), &first);
        assert!(named(&port.to_string()) && named(&token_name));
        hang_up_api(&token_name);
        first_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(
            first_peer.read(&mut [0_u8; 8]).unwrap(),
            0,
            "shut by its token"
        );
        let kept_second = KeptApiConnection::keep(port, None, &second);
        drop(kept_first);
        assert!(named(&port.to_string()), "the second's port kept");
        assert!(!named(&token_name), "the first's token gone");
        hang_up_api(&port.to_string());
        second_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(
            second_peer.read(&mut [0_u8; 8]).unwrap(),
            0,
            "shut by its port"
        );
        drop(kept_second);
        assert!(!named(&port.to_string()));
    }

    /// A client that says nothing is given up on within its wait; with none
    /// set again, a read waits for what comes.
    #[test]
    fn a_silent_client_is_given_up_on() {
        use std::io::Read as _;
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut ours = std::fs::File::from(std::os::fd::OwnedFd::from(ours));
        set_read_wait(&ours, Some(Duration::from_secs(1)));
        let began = std::time::Instant::now();
        let error = ours.read(&mut [0_u8; 8]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(began.elapsed() < Duration::from_secs(5));
        set_read_wait(&ours, None);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            std::io::Write::write_all(&mut theirs, b"hi").unwrap();
        });
        let mut got = [0_u8; 2];
        ours.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi", "no wait left set");
        writer.join().unwrap();
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
