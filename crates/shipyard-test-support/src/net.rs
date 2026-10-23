//! Networks for the end-to-end tests (Linux, root): a stand-in for the
//! internet, and ways to listen and connect from inside any network
//! namespace, a container's included.
//!
//! A test runs in a network namespace of its own ([`crate::in_new_netns`]):
//! that's "the host". [`Upstream`] joins it to a second namespace by a veth
//! pair, as an uplink to the world: `192.0.2.1` on the host's side, `192.0.2.2`
//! on the far side (TEST-NET-1, never routed for real). A thread can join any
//! namespace (`setns` changes only its own), so "a server in the container"
//! is a listener on a thread joined to the container's namespace: no program
//! of ours has to be in its root filesystem.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::sched::{CloneFlags, setns, unshare};

use crate::{Run, Shipyard};
use shipyard_network::IpNet;
use shipyard_network::dns::message::{self, Name, RData, Received, Response, rcode};
use shipyard_network::route::{LinkChange, Rtnetlink};

/// How long anything here waits.
const WAIT: Duration = Duration::from_secs(10);

/// The calling thread's network namespace.
#[must_use]
pub fn this_netns() -> File {
    File::open("/proc/thread-self/ns/net").expect("this thread's network namespace")
}

/// Runs `f` on a thread joined to the network namespace `netns`, and
/// returns what it returns.
pub fn in_netns<T: Send>(netns: &File, f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                setns(netns.as_fd(), CloneFlags::CLONE_NEWNET)
                    .expect("joining a network namespace");
                f()
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// The network namespace of the container `shipyard` (the CLI's process ID)
/// runs, once it's connected to its network: a child of the CLI's in
/// another namespace than its own (pasta, another child, stays in the
/// host's), with an interface up besides `lo` (`eth0` on a bridge; with an
/// older pasta, the host's interface's name) and a default route (an IPv6
/// one too, if it has an IPv6 address).
#[must_use]
pub fn container_netns(shipyard: libc::pid_t) -> File {
    container_netns_on(shipyard, 1)
}

/// [`container_netns`], once the container is connected to all of its
/// `networks` (that many interfaces up besides `lo`).
#[must_use]
pub fn container_netns_on(shipyard: libc::pid_t, networks: usize) -> File {
    container_init(shipyard, networks).1
}

/// The process ID of the container `shipyard` runs (its init), and its
/// network namespace, once it's connected to all of its `networks` (see
/// [`container_netns_on`]).
#[must_use]
pub fn container_init(shipyard: libc::pid_t, networks: usize) -> (libc::pid_t, File) {
    use std::os::unix::fs::MetadataExt as _;
    let deadline = Instant::now() + WAIT;
    let own = std::fs::metadata(format!("/proc/{shipyard}/ns/net"))
        .expect("shipyard's network namespace")
        .ino();
    loop {
        let children =
            std::fs::read_to_string(format!("/proc/{shipyard}/task/{shipyard}/children"))
                .unwrap_or_default();
        for child in children.split_whitespace() {
            let Ok(netns) = File::open(format!("/proc/{child}/ns/net")) else {
                continue;
            };
            if netns.metadata().is_ok_and(|ns| ns.ino() == own) {
                continue;
            }
            let connected = Rtnetlink::in_namespace(netns.as_fd()).is_ok_and(|mut inside| {
                let up = inside.links().map_or(0, |links| {
                    links
                        .iter()
                        .filter(|link| link.index != 1 && link.is_up())
                        .count()
                });
                let has_default = |inside: &mut Rtnetlink, family| {
                    inside.routes(family).is_ok_and(|routes| {
                        routes.iter().any(|route| route.destination.prefix() == 0)
                    })
                };
                // With an IPv6 address of its own (not link-local), its IPv6
                // default route comes last.
                let ipv6 = inside.addresses(None).is_ok_and(|addresses| {
                    addresses.iter().any(|address| match address.net.addr() {
                        std::net::IpAddr::V6(ip) => {
                            !ip.is_loopback() && !ip.is_unicast_link_local()
                        }
                        std::net::IpAddr::V4(_) => false,
                    })
                });
                up >= networks
                    && has_default(&mut inside, shipyard_network::Family::V4)
                    && (!ipv6 || has_default(&mut inside, shipyard_network::Family::V6))
            });
            if connected && let Ok(pid) = child.parse() {
                return (pid, netns);
            }
        }
        assert!(
            Instant::now() < deadline,
            "the container of shipyard {shipyard} never got its network"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A listener on `port` in `netns`, answering every connection with
/// `reply` until it's dropped: listening when this returns. It records
/// where each client came from.
#[derive(Debug)]
pub struct Server {
    peers: std::sync::mpsc::Receiver<SocketAddr>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Listens on `port` (every address, IPv4 and IPv6) in `netns`.
    #[must_use]
    pub fn start(netns: &File, port: u16, reply: &'static str) -> Server {
        use std::sync::atomic::Ordering;
        let netns = netns.try_clone().expect("a namespace descriptor");
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (peer_tx, peers) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            setns(netns.as_fd(), CloneFlags::CLONE_NEWNET).expect("joining a network namespace");
            // IPv6's every address takes IPv4's too (`IPV6_V6ONLY` off, the
            // kernel's default); a namespace without IPv6 has IPv4's.
            let listener = TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, port))
                .or_else(|_| TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)))
                .expect("listening");
            listener
                .set_nonblocking(true)
                .expect("a nonblocking listener");
            let _ = ready_tx.send(());
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, from)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.write_all(reply.as_bytes());
                        // An IPv4 client, as IPv4 (not `::ffff:a.b.c.d`).
                        let from = SocketAddr::new(from.ip().to_canonical(), from.port());
                        let _ = peer_tx.send(from);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        ready.recv_timeout(WAIT).expect("the server started");
        Server {
            peers,
            stop,
            thread: Some(thread),
        }
    }

    /// Where its next client came from.
    #[must_use]
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peers.recv_timeout(WAIT).ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Connects to `address` from `netns` and reads what it sends until it
/// closes: `None` if it can't be reached (within a second or two).
#[must_use]
pub fn fetch_from(netns: &File, address: SocketAddr) -> Option<String> {
    in_netns(netns, || fetch(address))
}

/// How long a connection may take to be made: a dropped packet makes it
/// time out, which is what a test of something unreachable waits for.
const CONNECT_WAIT: Duration = Duration::from_secs(2);

/// Connects to `address` from this thread's namespace and reads what it
/// sends until it closes.
#[must_use]
pub fn fetch(address: SocketAddr) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_WAIT).ok()?;
    stream.set_read_timeout(Some(WAIT)).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    Some(text)
}

/// A second network namespace, joined to the calling thread's (the test's
/// "host") by a veth pair: the world outside.
#[derive(Debug)]
pub struct Upstream {
    netns: File,
}

impl Upstream {
    /// The host's address on the link.
    pub const HOST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    /// The upstream side's.
    pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    /// The host's IPv6 address on the link (documentation's prefix,
    /// `2001:db8::/32`, never routed for real).
    pub const HOST6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
    /// The upstream side's.
    pub const ADDRESS6: std::net::Ipv6Addr =
        std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);

    /// Makes it, from the calling thread's namespace: `up0` there, `eth0`
    /// in the new one, both up, with their addresses ([`HOST`](Self::HOST)
    /// and [`ADDRESS`](Self::ADDRESS)); and the host's loopback up (a new
    /// namespace's is down: 127.0.0.1 isn't there).
    #[must_use]
    #[allow(
        clippy::new_without_default,
        reason = "it makes namespaces and links: never implicitly"
    )]
    pub fn new() -> Upstream {
        let upstream = Upstream::with("up0", Upstream::HOST.octets(), Upstream::ADDRESS.octets());
        // IPv6 too, on the same link (`/64`s, with no duplicate address
        // detection: usable at once).
        let net = |ip: std::net::Ipv6Addr| {
            IpNet::new(std::net::IpAddr::V6(ip), 64).expect("an IPv6 address")
        };
        let mut host_side = Rtnetlink::new().expect("rtnetlink");
        let link = host_side.link("up0").expect("up0").expect("up0 is there");
        host_side
            .add_address(link.index, net(Upstream::HOST6))
            .expect("the host's IPv6 address on the link");
        let mut far = Rtnetlink::in_namespace(upstream.netns.as_fd()).expect("rtnetlink upstream");
        let eth0 = far.link("eth0").expect("eth0").expect("eth0 is there");
        far.add_address(eth0.index, net(Upstream::ADDRESS6))
            .expect("the far IPv6 address");
        upstream
    }

    /// Another link like it: `link` on the host's side with `host`, the far
    /// side with `far_address` (both /24).
    #[must_use]
    pub fn with(link: &str, host: [u8; 4], far_address: [u8; 4]) -> Upstream {
        let netns = std::thread::spawn(|| {
            unshare(CloneFlags::CLONE_NEWNET).expect("an upstream network namespace");
            // Held open, the namespace outlives the thread.
            this_netns()
        })
        .join()
        .expect("making the upstream namespace");
        let net =
            |octets: [u8; 4]| IpNet::new(std::net::IpAddr::from(octets), 24).expect("an address");
        let mut host_side = Rtnetlink::new().expect("rtnetlink");
        host_side
            .change(1, LinkChange::Up(true))
            .expect("the host's lo up");
        host_side
            .create_veth(link, "eth0", Some(netns.as_fd()), None)
            .expect("the link");
        let ours = host_side.link(link).expect("the link").expect("it's there");
        host_side
            .add_address(ours.index, net(host))
            .expect("the host's address on the link");
        host_side
            .change(ours.index, LinkChange::Up(true))
            .expect("the link up");
        let mut far = Rtnetlink::in_namespace(netns.as_fd()).expect("rtnetlink upstream");
        far.change(1, LinkChange::Up(true)).expect("lo up");
        let eth0 = far.link("eth0").expect("eth0").expect("eth0 is there");
        far.add_address(eth0.index, net(far_address))
            .expect("the far address");
        far.change(eth0.index, LinkChange::Up(true))
            .expect("eth0 up");
        Upstream { netns }
    }

    /// Makes the far side the host's way to the world: a default route
    /// through it (pasta copies the host's routed interface into a
    /// container).
    pub fn route_default(&self) {
        let mut host_side = Rtnetlink::new().expect("rtnetlink");
        host_side
            .add_route(&shipyard_network::route::Route::default_via(
                std::net::IpAddr::V4(Upstream::ADDRESS),
            ))
            .expect("a default route upstream");
        host_side
            .add_route(&shipyard_network::route::Route::default_via(
                std::net::IpAddr::V6(Upstream::ADDRESS6),
            ))
            .expect("an IPv6 default route upstream");
    }

    /// Makes the host the far side's way to everywhere else: default
    /// routes (IPv4 and IPv6) through it, as a world routing traffic for
    /// the host's networks to it would have. For an [`Upstream::new`].
    pub fn route_back(&self) {
        let mut far = Rtnetlink::in_namespace(self.netns.as_fd()).expect("rtnetlink upstream");
        for gateway in [
            std::net::IpAddr::V4(Upstream::HOST),
            std::net::IpAddr::V6(Upstream::HOST6),
        ] {
            far.add_route(&shipyard_network::route::Route::default_via(gateway))
                .expect("a default route back through the host");
        }
    }

    /// Its network namespace.
    #[must_use]
    pub fn netns(&self) -> &File {
        &self.netns
    }
}

/// Asks the DNS server at `server`, port 53, over UDP, from the network
/// namespace `netns`, for `name`'s records of type `qtype` (as
/// `shipyard_network::dns::message::rtype`): its response, read. `None` if
/// none came.
#[must_use]
pub fn resolve(netns: &File, server: Ipv4Addr, name: &str, qtype: u16) -> Option<Response> {
    let query = message::query(0x5eed, &Name::from_text(name).expect("a name"), qtype);
    in_netns(netns, || {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.set_read_timeout(Some(WAIT)).ok()?;
        socket.connect((server, 53)).ok()?;
        socket.send(&query).ok()?;
        let mut buffer = [0u8; 4096];
        let length = socket.recv(&mut buffer).ok()?;
        message::read_response(buffer.get(..length)?).ok()
    })
}

/// [`resolve`] over TCP.
#[must_use]
pub fn resolve_tcp(netns: &File, server: Ipv4Addr, name: &str, qtype: u16) -> Option<Response> {
    let query = message::query(0x5eed, &Name::from_text(name).expect("a name"), qtype);
    in_netns(netns, || {
        let mut stream = TcpStream::connect_timeout(&SocketAddr::from((server, 53)), WAIT).ok()?;
        stream.set_read_timeout(Some(WAIT)).ok()?;
        stream
            .write_all(&message::frame(&query).expect("a short query"))
            .ok()?;
        let mut length = [0u8; 2];
        stream.read_exact(&mut length).ok()?;
        let mut response = vec![0u8; usize::from(u16::from_be_bytes(length))];
        stream.read_exact(&mut response).ok()?;
        message::read_response(&response).ok()
    })
}

/// The `A` records in `response`.
#[must_use]
pub fn addresses(response: &Response) -> Vec<Ipv4Addr> {
    response
        .answers
        .iter()
        .filter_map(|answer| match answer.data {
            RData::A(ip) => Some(ip),
            _ => None,
        })
        .collect()
}

/// A DNS server standing in for the world's (upstream's, say): it answers
/// every query (whatever its type) with one A record, `address`, on UDP
/// port 53 of every address of the network namespace it's started in.
/// Stops on drop.
#[derive(Debug)]
pub struct FakeResolver {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeResolver {
    /// Starts it in `netns`.
    #[must_use]
    pub fn start(netns: &File, address: Ipv4Addr) -> FakeResolver {
        use std::sync::atomic::{AtomicBool, Ordering};
        let socket = in_netns(netns, || UdpSocket::bind("0.0.0.0:53")).expect("port 53");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("a timeout");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopping = std::sync::Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while !stopping.load(Ordering::Relaxed) {
                let Ok((length, client)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                if let Some(Received::Query(query)) = buffer.get(..length).map(message::read_query)
                {
                    let answer =
                        message::response(&query, rcode::NOERROR, false, &[RData::A(address)]);
                    let _ = socket.send_to(&answer, client);
                }
            }
        });
        FakeResolver {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeResolver {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A scratch directory for a test's networks (`SHIPYARD_NETWORK_DIR`), so
/// none of its networks is the machine's. Removed on drop.
#[derive(Debug)]
pub struct NetworkDir {
    path: std::path::PathBuf,
    /// A container store of its own, beside it: a container's record names
    /// its networks by name, and another test's network of the same name
    /// mustn't count as this one's (`network rm` refuses a network a record
    /// joins).
    containers: std::path::PathBuf,
    shipyard: Shipyard,
}

impl NetworkDir {
    /// A new one, for running `shipyard`.
    #[must_use]
    pub fn new(shipyard: Shipyard) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "shipyard-test-networks-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("a scratch networks directory");
        let containers = path.with_extension("containers");
        NetworkDir {
            path,
            containers,
            shipyard,
        }
    }

    /// `shipyard ARGS…` with these networks. A network's DNS server stops
    /// a second after its last container (not half a minute), so none
    /// outlives its test for long.
    #[must_use]
    pub fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = self.shipyard.command(args);
        command.env("SHIPYARD_NETWORK_DIR", &self.path);
        command.env("SHIPYARD_CONTAINER_DIR", &self.containers);
        command.env("SHIPYARD_DNS_IDLE", "1");
        command
    }

    /// `shipyard run FLAGS… sleep 30` with these networks (their DNS
    /// servers forwarding to [`Upstream::ADDRESS`]), and its container's
    /// network namespace once it's connected to every network it joins
    /// (one per `--network`, or the default's).
    #[must_use]
    pub fn sleeping(&self, flags: &[&str]) -> (crate::Running, File) {
        self.sleeping_with(&[], flags)
    }

    /// [`sleeping`](Self::sleeping), with `env` set too.
    #[must_use]
    pub fn sleeping_with(&self, env: &[(&str, &str)], flags: &[&str]) -> (crate::Running, File) {
        let mut command =
            self.command(&[crate::run_words(flags), flags, &["sleep", "30"]].concat());
        command.env("SHIPYARD_DNS_UPSTREAM", Upstream::ADDRESS.to_string());
        command.envs(env.iter().copied());
        let running = crate::start_command(command);
        let joined = flags.iter().filter(|&&flag| flag == "--network").count();
        let netns = container_netns_on(running.pid(), joined.max(1));
        (running, netns)
    }

    /// `shipyard network create ARGS…`, which must work.
    pub fn create(&self, args: &[&str]) {
        let created = self.run(&[&["network", "create"], args].concat());
        assert_eq!(created.code, Some(0), "{}", created.stderr);
    }

    /// `network inspect --shipyard NAME`'s network, as JSON.
    #[must_use]
    pub fn inspect(&self, name: &str) -> serde_json::Value {
        // Shipyard's own view (`--shipyard`): leases, the bridge, the DNS
        // server; plain `network inspect` is Docker's (step 11.3c2).
        let inspect = self.run(&["network", "inspect", "--shipyard", name]);
        assert_eq!(inspect.code, Some(0), "{}", inspect.stderr);
        let mut json: serde_json::Value =
            serde_json::from_str(&inspect.stdout).expect("inspect's JSON");
        json[0].take()
    }

    /// The network `name`'s gateway: where its DNS server is.
    #[must_use]
    pub fn gateway(&self, name: &str) -> Ipv4Addr {
        self.inspect(name)["Gateway"]
            .as_str()
            .and_then(|gateway| gateway.parse().ok())
            .expect("a gateway")
    }

    /// The running container named `container` on `network`, as `network
    /// inspect` shows it.
    #[must_use]
    pub fn lease(&self, network: &str, container: &str) -> serde_json::Value {
        let mut found = self.inspect(network);
        let leases = found["Containers"].as_array_mut().expect("containers");
        let at = leases
            .iter()
            .position(|lease| lease["Name"] == container)
            .unwrap_or_else(|| panic!("no container named {container} on {network}"));
        leases.swap_remove(at)
    }

    /// Its address on `network`.
    #[must_use]
    pub fn address(&self, network: &str, container: &str) -> Ipv4Addr {
        self.lease(network, container)["IPAddress"]
            .as_str()
            .and_then(|ip| ip.parse().ok())
            .expect("an address")
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The process IDs of these networks' DNS servers running now (their
    /// watchdogs: see [`dns_workers`](Self::dns_workers)).
    #[must_use]
    pub fn dns_servers(&self) -> Vec<libc::pid_t> {
        let ours = self.dns_processes();
        ours.iter()
            .filter(|(_, parent)| !ours.iter().any(|(pid, _)| pid == parent))
            .map(|&(pid, _)| pid)
            .collect()
    }

    /// The workers of these networks' DNS servers: each server's (its
    /// watchdog's) child, which serves.
    #[must_use]
    pub fn dns_workers(&self) -> Vec<libc::pid_t> {
        let ours = self.dns_processes();
        ours.iter()
            .filter(|(_, parent)| ours.iter().any(|(pid, _)| pid == parent))
            .map(|&(pid, _)| pid)
            .collect()
    }

    /// Every process of these networks' DNS servers (watchdogs and
    /// workers), with its parent.
    fn dns_processes(&self) -> Vec<(libc::pid_t, libc::pid_t)> {
        let mut servers = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return servers;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|pid| pid.parse().ok()) else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let args: Vec<&[u8]> = cmdline.split(|&byte| byte == 0).collect();
            let ours = args.get(1) == Some(&&b"__shipyard-dns"[..])
                && args
                    .get(2)
                    .map(|dir| Path::new(std::ffi::OsStr::from_bytes(dir)))
                    == Some(self.path.as_path());
            // A zombie has no command line: it's gone already. (So has one
            // still exiting, its files maybe still open: `exit_mm` comes
            // before `exit_files`. Wait on what it held, not on this.)
            let parent = std::fs::read_to_string(entry.path().join("status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix("PPid:"))
                        .and_then(|parent| parent.trim().parse().ok())
                });
            if let (true, Some(parent)) = (ours, parent) {
                servers.push((pid, parent));
            }
        }
        servers
    }

    /// Waits up to `wait` for these networks to have `count` DNS servers
    /// running, and returns them.
    #[must_use]
    pub fn wait_for_dns_servers(&self, count: usize, wait: Duration) -> Vec<libc::pid_t> {
        let deadline = Instant::now() + wait;
        loop {
            let servers = self.dns_servers();
            if servers.len() == count || Instant::now() >= deadline {
                return servers;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// [`dns_workers`](Self::dns_workers) once there are `count` of them,
    /// or as they are after `wait`.
    #[must_use]
    pub fn wait_for_dns_workers(&self, count: usize, wait: Duration) -> Vec<libc::pid_t> {
        let deadline = Instant::now() + wait;
        loop {
            let workers = self.dns_workers();
            if workers.len() == count || Instant::now() >= deadline {
                return workers;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `shipyard ARGS…` with these networks, to its end.
    #[must_use]
    pub fn run(&self, args: &[&str]) -> Run {
        crate::run(self.command(args))
    }
}

impl Drop for NetworkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.containers);
    }
}

/// An unprivileged user for root tests to run rootless containers as:
/// `SHIPYARD_TEST_USER` (a name) if set, else the one `sudo` ran the tests
/// for (`SUDO_UID`), else `tester` (the test container's). `None` if there's
/// none.
#[derive(Debug, Clone)]
pub struct TestUser {
    /// Its user ID.
    pub uid: u32,
    /// Its primary group.
    pub gid: u32,
    /// Its home.
    pub home: std::path::PathBuf,
}

impl TestUser {
    /// The user to run rootless containers as, if there is one.
    #[must_use]
    pub fn find() -> Option<TestUser> {
        let by = |uid: Option<u32>, name: Option<&str>| {
            let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
            passwd.lines().find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                let [entry, _, user, group, _, home, ..] = fields.as_slice() else {
                    return None;
                };
                let user: u32 = user.parse().ok()?;
                let matches =
                    uid.is_some_and(|uid| uid == user) || name.is_some_and(|name| name == *entry);
                let gid = group.parse().ok()?;
                (matches && user != 0).then(|| TestUser {
                    uid: user,
                    gid,
                    home: std::path::PathBuf::from(home),
                })
            })
        };
        if let Ok(name) = std::env::var("SHIPYARD_TEST_USER") {
            let user = by(None, Some(&name));
            assert!(user.is_some(), "SHIPYARD_TEST_USER={name}: no such user");
            return user;
        }
        if let Some(uid) = std::env::var("SUDO_UID")
            .ok()
            .and_then(|uid| uid.parse().ok())
        {
            return by(Some(uid), None);
        }
        by(None, Some("tester"))
    }

    /// `command`, run as this user, with `runtime` (made, theirs, private)
    /// as its `XDG_RUNTIME_DIR`: rootless shipyard keeps its runs there.
    #[must_use]
    pub fn command(&self, command: std::process::Command, runtime: &Path) -> std::process::Command {
        use std::os::unix::process::CommandExt as _;
        let mut command = self.environment(command, runtime);
        command.uid(self.uid).gid(self.gid);
        command
    }

    /// `command` with this user's environment and `runtime` (made, theirs,
    /// private) as its `XDG_RUNTIME_DIR` (with a container store of theirs
    /// in it), but still run as the caller: for
    /// a test that has root's work to do in the child first, then drops to
    /// the user itself.
    #[must_use]
    pub fn environment(
        &self,
        mut command: std::process::Command,
        runtime: &Path,
    ) -> std::process::Command {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(runtime).expect("a runtime directory");
        std::os::unix::fs::chown(runtime, Some(self.uid), Some(self.gid))
            .expect("the runtime directory the user's");
        std::fs::set_permissions(runtime, std::fs::Permissions::from_mode(0o700))
            .expect("a private runtime directory");
        // Its containers' records too (a foreground run is a container): the
        // user's own store, private.
        let containers = runtime.join("containers");
        std::fs::create_dir_all(&containers).expect("a container store");
        std::os::unix::fs::chown(&containers, Some(self.uid), Some(self.gid))
            .expect("the container store the user's");
        std::fs::set_permissions(&containers, std::fs::Permissions::from_mode(0o700))
            .expect("a private container store");
        command
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", runtime)
            .env("SHIPYARD_CONTAINER_DIR", &containers)
            .env_remove("SUDO_UID")
            .env_remove("SUDO_GID")
            .env_remove("SUDO_USER");
        command
    }

    /// The cgroup a process of this user's can be put in for its rootless
    /// containers to have resource limits: a leaf of a subtree delegated to
    /// it (owned by it, as systemd's `Delegate=` or `test-linux.sh` gives),
    /// within a few levels of `/sys/fs/cgroup`. `None` if there's none.
    #[must_use]
    pub fn delegated_cgroup(&self) -> Option<std::path::PathBuf> {
        use std::os::unix::fs::MetadataExt as _;
        fn walk(dir: &Path, uid: u32, depth: u32) -> Option<std::path::PathBuf> {
            let children: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
                .ok()?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .collect();
            let owned = std::fs::metadata(dir).is_ok_and(|meta| meta.uid() == uid);
            if owned && children.is_empty() {
                return Some(dir.to_path_buf());
            }
            if depth == 0 {
                return None;
            }
            children
                .iter()
                .find_map(|child| walk(child, uid, depth - 1))
        }
        let found = walk(Path::new("/sys/fs/cgroup"), self.uid, 4)?;
        // One with controllers on for its children can hold no process
        // (cgroup v2's no-internal-processes rule: EBUSY), as a systemd
        // host's user slices: a leaf of the tests' own under it, the user's.
        let busy = std::fs::read_to_string(found.join("cgroup.subtree_control"))
            .is_ok_and(|controllers| !controllers.trim().is_empty());
        if !busy {
            return Some(found);
        }
        let leaf = found.join("shipyard-tests");
        let _ = std::fs::create_dir(&leaf);
        for file in [
            "",
            "cgroup.procs",
            "cgroup.subtree_control",
            "cgroup.threads",
        ] {
            std::os::unix::fs::chown(leaf.join(file), Some(self.uid), Some(self.gid)).ok()?;
        }
        Some(leaf)
    }
}
