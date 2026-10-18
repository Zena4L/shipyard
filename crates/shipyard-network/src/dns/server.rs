//! The network's DNS server (Linux): sockets on the gateway's port 53,
//! bound to the network's bridge, a thread for UDP and one for TCP, and the
//! main thread watching for the moment to stop.
//!
//! It's started as root (port 53 is privileged, and only root may bind a
//! socket to a device), binds ([`bind`]), then gives root up for good
//! ([`drop_privileges`]): it answers queries from containers, so a mistake
//! in it mustn't be root's. What it needs afterwards was opened before:
//! the leases directory, the network's lock file, its own lock.
//!
//! **Liveness.** The server holds the network's `dns.lock` for its whole
//! life (the run that started it passed it on locked); a run that finds it
//! held knows a server is there. The server stops once the network has had
//! no live container for a while ([`Setup::idle`]): under the network's
//! lock, which runs hold while they join, it checks again, then lets its
//! own lock go *before* the network's, so a run that comes next finds no
//! server, and starts one. A new server whose address the old one still
//! holds for a moment waits for it ([`BIND_WAIT`]). It also stops when its
//! bridge is gone (`shipyard network rm`).

use std::collections::HashMap;
use std::fs::File;
use std::io::Read as _;
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, bind as bind_socket, listen,
    setsockopt, socket, sockopt,
};
use nix::unistd::{Gid, Uid};
use thiserror::Error;

use super::answer::{self, Known, Names, Reply, Zone};
use super::message::{self, Query, Received, rcode};
use crate::ipnet::IpNet;
use crate::store::{LeaseDir, NetworkConfig, NetworksDir};

/// The user and group the server runs as once its sockets are bound:
/// `nobody`'s, the kernel's overflow ID; the group is the one the networks'
/// files are shared with.
pub const NOBODY: u32 = crate::store::SERVERS_GROUP;

/// How long a network may have no container before its server stops.
pub const IDLE: Duration = Duration::from_secs(30);

/// How long binding waits for an address a server that's stopping still
/// holds.
pub const BIND_WAIT: Duration = Duration::from_secs(3);

/// The DNS port.
const PORT: u16 = 53;

/// How often the server checks on its network.
const TICK: Duration = Duration::from_secs(1);

/// The most queries forwarded at once, and for one container; one past
/// that is answered SERVFAIL.
const MAX_FORWARDS: Places = Places::new(64, 16);

/// The most TCP connections at once, and from one container; one past
/// that is closed. Each lives [`CONNECTION_LIFETIME`] at most, so its
/// place always comes back (F58).
const MAX_CONNECTIONS: Places = Places::new(64, 8);

/// The most tasks (threads) a server has at once: its main thread, the UDP
/// and TCP threads, and one per forwarded query and TCP connection. Its
/// cgroup's `pids.max` must allow at least this, or queries fail.
pub const MOST_TASKS: usize = 3 + MAX_FORWARDS.total + MAX_CONNECTIONS.total;

/// How long an upstream resolver has to answer.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// How long forwarding a query may take, whatever the upstreams do.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(4);

/// How long a TCP client may take to send a query, whole: from its first
/// byte (or the last answer) to its last.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a TCP connection is read from, from when it's served, whatever
/// it sends (F58): a query read by then is still answered (forwarded within
/// [`FORWARD_TIMEOUT`], written within [`CLIENT_TIMEOUT`]), then it's
/// closed, and a resolver opens another (RFC 7766 lets a server close an
/// idle or busy connection). Without it, a client sending a query every few
/// seconds kept its place for good.
const CONNECTION_LIFETIME: Duration = Duration::from_secs(30);

/// The most upstream resolvers asked (as resolv.conf's `MAXNS`).
const MAX_UPSTREAMS: usize = 3;

/// Where the host's resolvers are named.
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// What can stop the server from starting.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// The network's bridge isn't there.
    #[error("the bridge {bridge} isn't there: {}", errno.desc())]
    NoBridge {
        /// Its name.
        bridge: String,
        /// Why.
        errno: Errno,
    },
    /// A socket couldn't be made or bound.
    #[error("binding {address} on {bridge}{hint}: {}", errno.desc())]
    Bind {
        /// The address.
        address: SocketAddr,
        /// The bridge.
        bridge: String,
        /// What to look at, for an address in use.
        hint: &'static str,
        /// Why.
        errno: Errno,
    },
    /// Root couldn't be given up.
    #[error("giving up root: {0}")]
    Privileges(String),
    /// The server's threads couldn't be started.
    #[error("starting the server's threads: {0}")]
    Threads(io::Error),
}

/// The server's sockets, bound.
#[derive(Debug)]
pub struct Sockets {
    udp: UdpSocket,
    tcp: TcpListener,
    /// The bridge's index when they were bound: a bridge made again under
    /// the same name is another one, which the sockets aren't bound to.
    bridge_index: u32,
}

impl Sockets {
    /// The index of the bridge they're bound to.
    #[must_use]
    pub fn bridge_index(&self) -> u32 {
        self.bridge_index
    }
}

/// Binds the server's sockets for `config`'s network: its gateway's port
/// 53, UDP and TCP, each bound to its bridge (`SO_BINDTODEVICE`), so only
/// its containers' queries reach them. An address still held by a server
/// that's stopping is waited for, up to [`BIND_WAIT`].
pub fn bind(config: &NetworkConfig) -> Result<Sockets, ServerError> {
    let bridge = config.bridge.clone();
    let bridge_index =
        nix::net::if_::if_nametoindex(bridge.as_str()).map_err(|errno| ServerError::NoBridge {
            bridge: bridge.clone(),
            errno,
        })?;
    let IpAddr::V4(gateway) = config.gateway else {
        return Err(ServerError::Bind {
            address: SocketAddr::new(config.gateway, PORT),
            bridge,
            hint: " (the server listens on the network's IPv4 gateway)",
            errno: Errno::EAFNOSUPPORT,
        });
    };
    let address = std::net::SocketAddrV4::new(gateway, PORT);
    let deadline = Instant::now() + BIND_WAIT;
    loop {
        match bind_both(address, &bridge) {
            Ok((udp, tcp)) => {
                return Ok(Sockets {
                    udp,
                    tcp,
                    bridge_index,
                });
            }
            Err(Errno::EADDRINUSE) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(errno) => {
                return Err(ServerError::Bind {
                    address: SocketAddr::V4(address),
                    bridge,
                    hint: if errno == Errno::EADDRINUSE {
                        " (is another DNS server listening on every address, port 53?)"
                    } else {
                        ""
                    },
                    errno,
                });
            }
        }
    }
}

/// The UDP and TCP sockets on `address`, bound to `bridge`.
fn bind_both(
    address: std::net::SocketAddrV4,
    bridge: &str,
) -> Result<(UdpSocket, TcpListener), Errno> {
    let address = SockaddrIn::from(address);
    let device = std::ffi::OsString::from(bridge);
    let udp = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    setsockopt(&udp, sockopt::BindToDevice, &device)?;
    bind_socket(std::os::fd::AsRawFd::as_raw_fd(&udp), &address)?;
    let tcp = socket(
        AddressFamily::Inet,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    // A TCP port a stopped server left connections on (`TIME_WAIT`) can be
    // bound again; one that's listening still can't.
    setsockopt(&tcp, sockopt::ReuseAddr, &true)?;
    setsockopt(&tcp, sockopt::BindToDevice, &device)?;
    bind_socket(std::os::fd::AsRawFd::as_raw_fd(&tcp), &address)?;
    listen(&tcp, Backlog::new(64)?)?;
    Ok((UdpSocket::from(udp), TcpListener::from(tcp)))
}

/// Gives root up for good: no supplementary groups, `nobody`'s user and
/// group IDs (real, effective and saved, so there's no way back), and
/// `no_new_privs`, so no program it could run gains any. Changing every
/// user ID from 0 clears the capabilities; that's checked.
pub fn drop_privileges() -> Result<(), ServerError> {
    let failed = |what: &str, errno: Errno| ServerError::Privileges(format!("{what}: {errno}"));
    // It holds nothing open by its path from here on.
    nix::unistd::chdir("/").map_err(|errno| failed("changing to /", errno))?;
    nix::unistd::setgroups(&[]).map_err(|errno| failed("dropping the groups", errno))?;
    let group = Gid::from_raw(NOBODY);
    nix::unistd::setresgid(group, group, group)
        .map_err(|errno| failed("becoming the group nobody", errno))?;
    let user = Uid::from_raw(NOBODY);
    nix::unistd::setresuid(user, user, user)
        .map_err(|errno| failed("becoming the user nobody", errno))?;
    nix::sys::prctl::set_no_new_privs().map_err(|errno| failed("no_new_privs", errno))?;
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| ServerError::Privileges(format!("reading its status: {error}")))?;
    let kept = status.lines().any(|line| {
        ["CapPrm:", "CapEff:", "CapAmb:"].iter().any(|set| {
            line.strip_prefix(set)
                .is_some_and(|caps| !caps.trim().trim_start_matches('0').is_empty())
        })
    });
    if kept {
        return Err(ServerError::Privileges(
            "capabilities were kept (securebits?)".to_owned(),
        ));
    }
    Ok(())
}

/// Where the server forwards what isn't its to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstreams {
    /// The host's resolvers (`/etc/resolv.conf`, read again when it
    /// changes). The server is on the host's network: even a resolver on
    /// its loopback (systemd-resolved's `127.0.0.53`) is reachable.
    Host,
    /// These (for tests).
    Fixed(Vec<SocketAddr>),
}

/// What the server works from, opened before it gave root up.
#[derive(Debug)]
pub struct Setup {
    /// The network's addresses, and whether it's internal.
    pub zone: Zone,
    /// The network's gateway: never an upstream (it's the server itself).
    pub gateway: IpAddr,
    /// Its bridge's name.
    pub bridge: String,
    /// The network's name.
    pub name: String,
    /// Its leases.
    pub leases: LeaseDir,
    /// Every network's directory: an asker's other networks are read
    /// through it.
    pub networks: NetworksDir,
    /// Its lock file (taken to stop).
    pub network_lock: File,
    /// The server's own lock (`dns.lock`), held already.
    pub dns_lock: File,
    /// How long the network may have no container before the server stops.
    pub idle: Duration,
    /// Where to forward.
    pub upstreams: Upstreams,
}

/// Why the server stopped.
#[derive(Debug)]
pub enum Ending {
    /// Its network had no container for [`Setup::idle`]. The network's
    /// lock is held: the process must exit with it, never letting it go
    /// first.
    Idle(Flock<File>),
    /// Its bridge is gone: the network was removed.
    BridgeGone,
}

/// What the threads share.
#[derive(Debug)]
struct Shared {
    zone: Zone,
    gateway: IpAddr,
    name: String,
    leases: LeaseDir,
    networks: NetworksDir,
    upstreams: RwLock<Vec<SocketAddr>>,
    /// Queries being forwarded.
    forwards: Taken,
    /// TCP connections open.
    connections: Taken,
    /// What each asker's run is on besides this network, found lately.
    elsewhere: Mutex<HashMap<String, (Instant, Elsewhere)>>,
    /// The network's live leases, as last read ([`Shared::own`]).
    own: Mutex<Option<Own>>,
}

/// The network's live leases as read at `read`, when the leases directory
/// was at `version`.
#[derive(Debug)]
struct Own {
    version: crate::store::LeasesVersion,
    read: Instant,
    leases: Arc<Vec<crate::store::Lease>>,
}

/// What an asker is on besides the server's network: the names of those
/// networks that have names, and whether any has a way out.
#[derive(Debug, Clone, Default)]
struct Elsewhere {
    /// Each network's subnets, and its names.
    networks: Vec<(Vec<IpNet>, Names)>,
    way_out: bool,
}

/// How long what an asker is on elsewhere is remembered: finding it reads
/// every network's leases, too much for every query (a container that
/// asks a lot mustn't slow everyone's), and a second's lag is fine for
/// other networks' containers.
const ELSEWHERE_FOR: Duration = Duration::from_secs(1);
/// How long the network's live leases are kept, if no lease was made or
/// removed meanwhile: reading them takes a file and a lock test per lease,
/// too much for every query (a container that asks a lot mustn't slow
/// everyone's). A new lease (once written: a read that met one being
/// written isn't kept) or a removed one is seen at once; a container killed
/// a moment ago, whose lease is still there until swept, may still be
/// answered for this long.
const OWN_FOR: Duration = Duration::from_secs(1);

/// The most askers' runs remembered at once.
const ELSEWHERE_MAX: usize = 256;

impl Shared {
    /// Whether `client` may be answered ([`from_network`]).
    fn answers_to(&self, client: IpAddr) -> bool {
        from_network(self.zone, self.gateway, client)
    }

    /// The names known to `client` now, and whether its other queries are
    /// forwarded. First the network's names, its live leases', read afresh
    /// (a container killed a moment ago answers to nothing); then, in their
    /// names' order, those of each other network with names that the
    /// client's container is on too (a live lease of its run's there), with
    /// their addresses on that network, which it reaches through its
    /// interface there. Other names are forwarded unless the network is
    /// internal and so are all the client's others: a container with a way
    /// out elsewhere may look the world's names up (as Docker's resolver).
    fn known_to(&self, client: IpAddr) -> (Known, bool) {
        let own = self.own();
        let asker = own
            .iter()
            .find(|lease| lease.ip == client.to_canonical())
            .map(|lease| lease.run.clone());
        let subnets = std::iter::once(self.zone.subnet).chain(self.zone.subnet6);
        let mut known = Known::new(subnets, Names::of(&own));
        let Some(run) = asker else {
            return (known, !self.zone.internal);
        };
        let elsewhere = self.elsewhere(&run);
        for (subnets, names) in elsewhere.networks {
            known.also(subnets, names);
        }
        (known, !self.zone.internal || elsewhere.way_out)
    }

    /// The network's live leases: as last read, if no lease was made or
    /// removed since and that was within [`OWN_FOR`]; read afresh
    /// otherwise.
    fn own(&self) -> Arc<Vec<crate::store::Lease>> {
        let version = self.leases.version();
        let mut own = self.own.lock().unwrap_or_else(PoisonError::into_inner);
        if let (Some(version), Some(last)) = (version, own.as_ref())
            && last.version == version
            && last.read.elapsed() < OWN_FOR
        {
            return Arc::clone(&last.leases);
        }
        let read = self.leases.read_live().unwrap_or_default();
        let leases = Arc::new(read.leases);
        *own = version.filter(|_| read.settled).map(|version| Own {
            version,
            read: Instant::now(),
            leases: Arc::clone(&leases),
        });
        leases
    }

    /// Who `client`'s forwarding places count against ([`forwarder`]).
    fn forwarder(&self, client: IpAddr) -> IpAddr {
        forwarder(self.zone, &self.own(), client)
    }

    /// What the run `run` is on besides this network ([`Elsewhere`]), as
    /// found within [`ELSEWHERE_FOR`].
    fn elsewhere(&self, run: &str) -> Elsewhere {
        let mut remembered = self
            .elsewhere
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((found, elsewhere)) = remembered.get(run)
            && found.elapsed() < ELSEWHERE_FOR
        {
            return elsewhere.clone();
        }
        let mut elsewhere = Elsewhere::default();
        for name in self.networks.names().unwrap_or_default() {
            if name == self.name {
                continue;
            }
            let Ok(leases) = self.networks.leases(&name).and_then(|dir| dir.live()) else {
                continue;
            };
            if !leases.iter().any(|lease| lease.run == run) {
                continue;
            }
            let Some(config) = self.networks.config(&name) else {
                continue;
            };
            elsewhere.way_out |= !config.internal;
            // The default network has no names (as Docker's).
            if config.has_dns() {
                let subnets = std::iter::once(config.subnet)
                    .chain(config.subnet6)
                    .collect();
                elsewhere.networks.push((subnets, Names::of(&leases)));
            }
        }
        if remembered.len() >= ELSEWHERE_MAX {
            remembered.clear();
        }
        remembered.insert(run.to_owned(), (Instant::now(), elsewhere.clone()));
        elsewhere
    }

    /// The upstream resolvers, now.
    fn upstreams(&self) -> Vec<SocketAddr> {
        self.upstreams
            .read()
            .map(|upstreams| upstreams.clone())
            .unwrap_or_default()
    }
}

/// Whether `client` is one of the addresses of `zone`'s containers (not its
/// gateway, `gateway`, nor its network or broadcast address): the server
/// answers nothing else. Its sockets only see what comes in on its bridge,
/// but a container can write any source address: an answer to one that
/// isn't the network's would go where the container said, sent by the host
/// (to another network, or the world).
fn from_network(zone: Zone, gateway: IpAddr, client: IpAddr) -> bool {
    let client = client.to_canonical();
    let IpAddr::V4(v4) = client else {
        return false;
    };
    zone.subnet.contains(client)
        && client != gateway
        && client != zone.subnet.network().addr()
        && Some(v4) != zone.subnet.broadcast()
}

/// Who the forwarding places of a query from `client` count against: the
/// client, where it's the address of one of `leases` (the network's live
/// ones); otherwise the network, all such clients together. A container can
/// write any source address it likes (where the bridge table can't stop it):
/// a hundred made up mustn't take every place from the containers.
fn forwarder(zone: Zone, leases: &[crate::store::Lease], client: IpAddr) -> IpAddr {
    let client = client.to_canonical();
    if leases.iter().any(|lease| lease.ip == client) {
        client
    } else {
        zone.subnet.network().addr()
    }
}

/// Serves until it's time to stop: returns why. The caller exits then (with
/// the network's lock, for [`Ending::Idle`]).
pub fn serve(setup: Setup, sockets: Sockets) -> Result<Ending, ServerError> {
    let Setup {
        zone,
        gateway,
        bridge,
        name,
        leases,
        networks,
        network_lock,
        dns_lock,
        idle,
        upstreams,
    } = setup;
    // Its own lock is held already, on an open file it may share (a
    // watchdog's): kept as a plain file, never a `Flock`, whose drop (on a
    // failure, say) would let go of it for every holder. Only an idle end
    // lets go of it, on purpose.
    let mut resolv = ResolvConf::default();
    let initial = match &upstreams {
        Upstreams::Fixed(fixed) => fixed.clone(),
        Upstreams::Host => resolv.read(gateway).unwrap_or_default(),
    };
    let shared = Arc::new(Shared {
        zone,
        gateway,
        name,
        leases,
        networks,
        upstreams: RwLock::new(initial),
        forwards: Taken::default(),
        connections: Taken::default(),
        elsewhere: Mutex::default(),
        own: Mutex::default(),
    });
    let udp = Arc::new(sockets.udp);
    let tcp = sockets.tcp;
    {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("udp".into())
            .spawn(move || serve_udp(&udp, &shared))
            .map_err(ServerError::Threads)?;
    }
    {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tcp".into())
            .spawn(move || serve_tcp(&tcp, &shared))
            .map_err(ServerError::Threads)?;
    }

    let mut network_lock = network_lock;
    let mut idle_since: Option<Instant> = None;
    loop {
        std::thread::sleep(TICK);
        if nix::net::if_::if_nametoindex(bridge.as_str()).ok() != Some(sockets.bridge_index) {
            return Ok(Ending::BridgeGone);
        }
        if upstreams == Upstreams::Host
            && let Some(read) = resolv.read(gateway)
            && let Ok(mut upstreams) = shared.upstreams.write()
        {
            *upstreams = read;
        }
        // Not idle while it has a container, or can't tell.
        if !shared.leases.live().is_ok_and(|leases| leases.is_empty()) {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() < idle {
            continue;
        }
        // Under the network's lock, no run is joining: if there's still no
        // container, there won't be one before this server is gone.
        match Flock::lock(network_lock, FlockArg::LockExclusiveNonblock) {
            Ok(held) if shared.leases.live().is_ok_and(|leases| leases.is_empty()) => {
                // Its own lock first (for every holder of its open file): a
                // run waiting for the network's finds no server, and
                // starts one.
                // SAFETY: `flock` on a descriptor this owns, open.
                unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&dns_lock), libc::LOCK_UN) };
                return Ok(Ending::Idle(held));
            }
            Ok(held) => match held.unlock() {
                Ok(file) => network_lock = file,
                // It can't be let go of (it can't happen): stop, and the
                // network's lock goes with the process.
                Err((held, _)) => return Ok(Ending::Idle(held)),
            },
            Err((file, _)) => network_lock = file,
        }
        idle_since = None;
    }
}

/// Writes `index`, the bridge's, in the server's lock file (`lock`, open):
/// a run that finds a server there knows which bridge it serves. A bridge
/// made again (deleted by hand) is another, which it's about to leave.
pub fn record_bridge(lock: &File, index: u32) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    lock.set_len(0)?;
    lock.write_all_at(index.to_string().as_bytes(), 0)
}

/// The bridge a running server serves, as it wrote in its lock file
/// (`lock`, open): `None` if it hasn't yet.
#[must_use]
pub fn served_bridge(lock: &File) -> Option<u32> {
    use std::os::unix::fs::FileExt as _;
    let mut buffer = [0u8; 16];
    let read = lock.read_at(&mut buffer, 0).ok()?;
    std::str::from_utf8(buffer.get(..read)?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The host's resolv.conf, read again only when it changes.
#[derive(Debug, Default)]
struct ResolvConf {
    /// When it was last read (its modification time and size).
    seen: Option<(SystemTime, u64)>,
}

impl ResolvConf {
    /// Its resolvers, if it changed since it was last read (the first
    /// time, it has): at most [`MAX_UPSTREAMS`], never `gateway` (the
    /// server itself) or an address that can't be one.
    fn read(&mut self, gateway: IpAddr) -> Option<Vec<SocketAddr>> {
        let metadata = std::fs::metadata(RESOLV_CONF).ok();
        let now = metadata.and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
        if now.is_some() && now == self.seen {
            return None;
        }
        self.seen = now;
        let text = std::fs::read_to_string(RESOLV_CONF).unwrap_or_default();
        Some(upstreams_from(&text, gateway))
    }
}

/// The resolvers `text` (a resolv.conf) names, as [`ResolvConf::read`] takes
/// them.
fn upstreams_from(text: &str, gateway: IpAddr) -> Vec<SocketAddr> {
    crate::files::nameservers(text)
        .into_iter()
        .filter(|ip| *ip != gateway && !ip.is_unspecified())
        .take(MAX_UPSTREAMS)
        .map(|ip| SocketAddr::new(ip, PORT))
        .collect()
}

/// What to do with a message received.
enum Response {
    /// Send this.
    Now(Vec<u8>),
    /// Forward the query (it's the message as received), and pass the
    /// answer on.
    Forward(Query),
    /// Nothing.
    Nothing,
}

/// The response to `message`, from `client`.
fn respond(message: &[u8], shared: &Shared, client: IpAddr) -> Response {
    match message::read_query(message) {
        Received::Ignored => Response::Nothing,
        Received::Unanswerable { header, rcode } => Response::Now(message::refusal(header, rcode)),
        Received::Query(query) => {
            let (known, forwards) = shared.known_to(client);
            match answer::answer(&query, &known, forwards) {
                Reply::Answer(bytes) => Response::Now(bytes),
                Reply::Forward => Response::Forward(query),
            }
        }
    }
}

/// "The server failed", for `query`.
fn servfail(query: &Query) -> Vec<u8> {
    message::response(query, rcode::SERVFAIL, false, &[])
}

/// How many places there are for work that takes a while (a forwarded
/// query, a TCP connection): in all, and for any one client, so that no
/// container can take them all from the others.
#[derive(Debug, Clone, Copy)]
struct Places {
    total: usize,
    per_client: usize,
}

impl Places {
    const fn new(total: usize, per_client: usize) -> Places {
        Places { total, per_client }
    }
}

/// The places taken, by client.
#[derive(Debug, Default)]
struct Taken(Mutex<HashMap<IpAddr, usize>>);

impl Taken {
    /// Takes one of `places` for `client`, if one is free.
    fn take(&self, places: Places, client: IpAddr) -> bool {
        let mut taken = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let total: usize = taken.values().sum();
        let theirs = taken.entry(client).or_insert(0);
        if total >= places.total || *theirs >= places.per_client {
            return false;
        }
        *theirs += 1;
        true
    }

    /// Gives back a place `client` took.
    fn give_back(&self, client: IpAddr) {
        let mut taken = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(theirs) = taken.get_mut(&client) {
            *theirs = theirs.saturating_sub(1);
            if *theirs == 0 {
                taken.remove(&client);
            }
        }
    }
}

/// The UDP thread: each query from the network is answered at once, or
/// forwarded on a thread of its own (up to [`MAX_FORWARDS`] at once).
fn serve_udp(socket: &Arc<UdpSocket>, shared: &Arc<Shared>) {
    let mut buffer = vec![0u8; usize::from(u16::MAX)];
    loop {
        let Ok((length, client)) = socket.recv_from(&mut buffer) else {
            // Not one that goes on happening (the socket is fine), but never
            // a busy loop if it did.
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if !shared.answers_to(client.ip()) {
            continue;
        }
        let message = buffer.get(..length).unwrap_or_default();
        let query = match respond(message, shared, client.ip()) {
            Response::Now(bytes) => {
                let _ = socket.send_to(&bytes, client);
                continue;
            }
            Response::Nothing => continue,
            Response::Forward(query) => query,
        };
        let forwarder = shared.forwarder(client.ip());
        if !shared.forwards.take(MAX_FORWARDS, forwarder) {
            let _ = socket.send_to(&servfail(&query), client);
            continue;
        }
        let forwarded = message.to_vec();
        // Said at once if no thread can forward it (the cgroup's tasks all
        // taken, say), rather than left to time out.
        let unforwarded = servfail(&query);
        let (to_client, state) = (Arc::clone(socket), Arc::clone(shared));
        let spawned = std::thread::Builder::new().spawn(move || {
            let reply =
                forward_udp(&forwarded, &state.upstreams()).unwrap_or_else(|| servfail(&query));
            let _ = to_client.send_to(&reply, client);
            state.forwards.give_back(forwarder);
        });
        if spawned.is_err() {
            shared.forwards.give_back(forwarder);
            let _ = socket.send_to(&unforwarded, client);
        }
    }
}

/// The TCP thread: each connection from the network on a thread of its own
/// (up to [`MAX_CONNECTIONS`] at once), its places counted against its
/// client as forwarding places are ([`forwarder`]: a live lease's address,
/// else the network's, all made-up sources together).
fn serve_tcp(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let Ok(client) = stream.peer_addr().map(|peer| peer.ip()) else {
            continue;
        };
        if !shared.answers_to(client) {
            continue;
        }
        let key = shared.forwarder(client);
        if !shared.connections.take(MAX_CONNECTIONS, key) {
            continue;
        }
        let state = Arc::clone(shared);
        let spawned = std::thread::Builder::new().spawn(move || {
            serve_connection(stream, &state, client);
            state.connections.give_back(key);
        });
        if spawned.is_err() {
            shared.connections.give_back(key);
        }
    }
}

/// Answers the queries on a TCP connection from `client`, for
/// [`CONNECTION_LIFETIME`] at most ([`serve_frames`]).
fn serve_connection(stream: TcpStream, shared: &Shared, client: IpAddr) {
    serve_frames(stream, CONNECTION_LIFETIME, |message| {
        match respond(message, shared, client) {
            Response::Now(bytes) => Some(bytes),
            Response::Forward(query) => {
                Some(forward_tcp(message, &shared.upstreams()).unwrap_or_else(|| servfail(&query)))
            }
            Response::Nothing => None,
        }
    });
}

/// Answers the queries on a TCP connection, one after the other, each with
/// what `answer` says (`None`: the connection closed), until the client
/// closes it, takes longer than [`CLIENT_TIMEOUT`] to send one, or the
/// connection has lived `lifetime` (a query being read then isn't waited
/// for; one read before is answered).
fn serve_frames(
    mut stream: TcpStream,
    lifetime: Duration,
    mut answer: impl FnMut(&[u8]) -> Option<Vec<u8>>,
) {
    let end = Instant::now() + lifetime;
    if stream.set_write_timeout(Some(CLIENT_TIMEOUT)).is_err() {
        return;
    }
    while let Some(message) = read_frame(&mut stream, end.min(Instant::now() + CLIENT_TIMEOUT)) {
        let Some(reply) = answer(&message) else {
            return;
        };
        let Ok(framed) = message::frame(&reply) else {
            return;
        };
        if stream.write_all(&framed).is_err() {
            return;
        }
    }
}

/// A message framed for TCP, read whole from `stream` by `deadline`
/// (`None` at its end, on an error, or if it takes longer: a byte at a time
/// doesn't keep a connection forever).
fn read_frame(stream: &mut TcpStream, deadline: Instant) -> Option<Vec<u8>> {
    let mut length = [0u8; 2];
    read_by(stream, &mut length, deadline)?;
    let mut message = vec![0u8; usize::from(u16::from_be_bytes(length))];
    read_by(stream, &mut message, deadline)?;
    Some(message)
}

/// Fills `buffer` from `stream` by `deadline`.
fn read_by(stream: &mut TcpStream, buffer: &mut [u8], deadline: Instant) -> Option<()> {
    let mut filled = 0;
    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(rest) {
            Ok(0) => return None,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(())
}

/// `query` sent to each of `upstreams` over UDP in turn, until one answers
/// (within [`FORWARD_TIMEOUT`] in all): its answer, as it is.
fn forward_udp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; usize::from(u16::MAX)];
    let overall = Instant::now() + FORWARD_TIMEOUT;
    for upstream in upstreams {
        let any = match upstream {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let Ok(socket) = UdpSocket::bind(any) else {
            continue;
        };
        if socket.connect(upstream).is_err() || socket.send(query).is_err() {
            continue;
        }
        let deadline = (Instant::now() + UPSTREAM_TIMEOUT).min(overall);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || socket.set_read_timeout(Some(left)).is_err() {
                break;
            }
            // A refusal (nothing on the port) is an error too: the next one.
            let Ok(length) = socket.recv(&mut buffer) else {
                break;
            };
            let reply = buffer.get(..length).unwrap_or_default();
            if message::answers(reply, query) {
                return Some(reply.to_vec());
            }
        }
    }
    None
}

/// `query` sent to each of `upstreams` over TCP in turn, until one answers
/// (within [`FORWARD_TIMEOUT`] in all).
fn forward_tcp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    let framed = message::frame(query).ok()?;
    let overall = Instant::now() + FORWARD_TIMEOUT;
    for upstream in upstreams {
        let wait = UPSTREAM_TIMEOUT.min(overall.saturating_duration_since(Instant::now()));
        if wait.is_zero() {
            break;
        }
        let Ok(mut stream) = TcpStream::connect_timeout(upstream, wait) else {
            continue;
        };
        if stream.set_write_timeout(Some(wait)).is_err() || stream.write_all(&framed).is_err() {
            continue;
        }
        if let Some(reply) = read_frame(&mut stream, Instant::now() + wait)
            && message::answers(&reply, query)
        {
            return Some(reply);
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;
    use crate::dns::message::RData;

    #[test]
    fn upstreams_are_the_hosts_resolvers_but_the_server() {
        let text = "nameserver 127.0.0.53\nnameserver 10.89.0.1\nnameserver 0.0.0.0\n\
                    nameserver 1.1.1.1\nnameserver ::1\nnameserver 9.9.9.9\n";
        assert_eq!(
            upstreams_from(text, "10.89.0.1".parse().unwrap()),
            ["127.0.0.53:53", "1.1.1.1:53", "[::1]:53"].map(|addr| addr.parse().unwrap())
        );
    }

    #[test]
    fn places_run_out_in_all_and_for_one_client() {
        let places = Places::new(3, 2);
        let taken = Taken::default();
        let (a, b): (IpAddr, IpAddr) = ("10.89.0.2".parse().unwrap(), "10.89.0.3".parse().unwrap());
        assert!(taken.take(places, a));
        assert!(taken.take(places, a));
        // Not a third for one client...
        assert!(!taken.take(places, a));
        // ...but for another, up to the total.
        assert!(taken.take(places, b));
        assert!(!taken.take(places, b));
        taken.give_back(a);
        assert!(taken.take(places, b));
        assert!(!taken.take(places, a));
    }

    #[test]
    fn only_the_networks_containers_are_answered() {
        let zone = Zone {
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: false,
        };
        let gateway = "10.89.0.1".parse().unwrap();
        for ok in ["10.89.0.2", "10.89.0.254", "::ffff:10.89.0.9"] {
            assert!(from_network(zone, gateway, ok.parse().unwrap()), "{ok}");
        }
        for not in [
            "10.89.0.1",
            "10.89.0.0",
            "10.89.0.255",
            "10.89.1.2",
            "192.0.2.1",
            "::1",
        ] {
            assert!(!from_network(zone, gateway, not.parse().unwrap()), "{not}");
        }
    }

    /// A query's forwarding places count against its client where that's a
    /// live lease's address; against the network, all together, for any
    /// other (made-up) source: those can't take every place.
    #[test]
    fn made_up_sources_share_their_forwarding_places() {
        let zone = Zone {
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: false,
        };
        let lease: crate::store::Lease = serde_json::from_value(serde_json::json!({
            "ip": "10.89.0.2", "mac": [2, 66, 10, 89, 0, 2], "run": "r",
            "hostname": "h", "host_veth": "shyr"
        }))
        .unwrap();
        let leased: IpAddr = "10.89.0.2".parse().unwrap();
        let network: IpAddr = "10.89.0.0".parse().unwrap();
        assert_eq!(
            forwarder(zone, std::slice::from_ref(&lease), leased),
            leased
        );
        let taken = Taken::default();
        let mut granted = 0;
        for last in 3..=200_u8 {
            let made_up = IpAddr::from([10, 89, 0, last]);
            let key = forwarder(zone, std::slice::from_ref(&lease), made_up);
            assert_eq!(key, network);
            granted += usize::from(taken.take(MAX_FORWARDS, key));
        }
        assert_eq!(granted, MAX_FORWARDS.per_client);
        // The leased client still has its own.
        assert!(taken.take(MAX_FORWARDS, leased));
        // TCP connections' places too (`serve_tcp` keys them alike).
        let connections = Taken::default();
        let mut granted = 0;
        for last in 3..=200_u8 {
            let made_up = IpAddr::from([10, 89, 0, last]);
            let key = forwarder(zone, std::slice::from_ref(&lease), made_up);
            granted += usize::from(connections.take(MAX_CONNECTIONS, key));
        }
        assert_eq!(granted, MAX_CONNECTIONS.per_client);
        assert!(connections.take(MAX_CONNECTIONS, leased));
    }

    /// A query for example.com, with the ID 7.
    fn example() -> (Vec<u8>, Query) {
        let name = message::Name::from_text("example.com").unwrap();
        let bytes = message::query(7, &name, message::rtype::A);
        let Received::Query(query) = message::read_query(&bytes) else {
            panic!("not a query");
        };
        (bytes, query)
    }

    const WORLD: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

    #[test]
    fn queries_go_to_the_first_upstream_that_answers_over_udp() {
        let (bytes, query) = example();
        let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = upstream.local_addr().unwrap();
        let answering = std::thread::spawn(move || {
            let mut buffer = [0u8; 512];
            let (_, client) = upstream.recv_from(&mut buffer).unwrap();
            // Another query's answer first: not this one's.
            let mut other = query.clone();
            other.header.id = 8;
            let wrong = message::response(&other, rcode::NOERROR, false, &[]);
            upstream.send_to(&wrong, client).unwrap();
            let right = message::response(&query, rcode::NOERROR, false, &[RData::A(WORLD)]);
            upstream.send_to(&right, client).unwrap();
        });
        // Nothing on the first: refused at once, and on to the next.
        let closed = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let reply = forward_udp(&bytes, &[closed, address]).unwrap();
        answering.join().unwrap();
        let read = message::read_response(&reply).unwrap();
        assert_eq!(read.header.id, 7);
        assert_eq!(read.answers[0].data, RData::A(WORLD));
        assert_eq!(forward_udp(&bytes, &[]), None);
        assert_eq!(forward_udp(&bytes, &[closed]), None);
    }

    #[test]
    fn queries_go_to_the_first_upstream_that_answers_over_tcp() {
        let (bytes, query) = example();
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = upstream.local_addr().unwrap();
        let answering = std::thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            assert_eq!(
                read_frame(&mut stream, Instant::now() + Duration::from_secs(5)).unwrap(),
                example().0
            );
            let right = message::response(&query, rcode::NOERROR, false, &[RData::A(WORLD)]);
            stream.write_all(&message::frame(&right).unwrap()).unwrap();
        });
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let reply = forward_tcp(&bytes, &[closed, address]).unwrap();
        answering.join().unwrap();
        assert_eq!(
            message::read_response(&reply).unwrap().answers[0].data,
            RData::A(WORLD)
        );
        assert_eq!(forward_tcp(&bytes, &[closed]), None);
    }

    #[test]
    fn a_tcp_frame_must_come_whole_and_in_time() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let within = |time: Duration| Instant::now() + time;
        client.write_all(b"\x00\x03abc\x00\x05ab").unwrap();
        assert_eq!(
            read_frame(&mut server, within(Duration::from_secs(5))).unwrap(),
            b"abc"
        );
        // Two bytes of five, then nothing: given up on at the deadline,
        // however long the connection stays open.
        let started = Instant::now();
        assert_eq!(
            read_frame(&mut server, within(Duration::from_millis(300))),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        // Closed early.
        client.write_all(b"\x00\x09ab").unwrap();
        drop(client);
        assert_eq!(
            read_frame(&mut server, within(Duration::from_secs(5))),
            None
        );
    }

    /// A connection is closed once it has lived its lifetime, however
    /// often its client asks (F58): queries every 100 ms, each answered
    /// while it lives (the control), then the end, within the lifetime and
    /// one query's read of it.
    #[test]
    fn a_connection_is_closed_at_its_lifetime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let lifetime = Duration::from_millis(500);
        // Before the server's own clock starts: never after its end.
        let started = Instant::now();
        let serving = std::thread::spawn(move || {
            serve_frames(server, lifetime, |message| Some(message.to_vec()));
        });
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut answered = 0;
        loop {
            assert!(
                started.elapsed() < lifetime + Duration::from_secs(2),
                "still open after {:?}",
                started.elapsed()
            );
            if client.write_all(b"\x00\x02hi").is_err() {
                break;
            }
            let mut reply = [0u8; 4];
            match client.read_exact(&mut reply) {
                Ok(()) => {
                    assert_eq!(&reply, b"\x00\x02hi");
                    answered += 1;
                }
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        serving.join().unwrap();
        assert!(answered >= 2, "answered while it lived: {answered}");
        assert!(
            started.elapsed() >= lifetime,
            "closed before its lifetime: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_server_records_its_bridge_in_its_lock() {
        let path = std::env::temp_dir().join(format!("shipyard-dns-lock-{}", std::process::id()));
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        assert_eq!(served_bridge(&lock), None);
        record_bridge(&lock, 123_456).unwrap();
        record_bridge(&lock, 7).unwrap();
        assert_eq!(served_bridge(&lock), Some(7));
        std::fs::remove_file(&path).unwrap();
    }
}
