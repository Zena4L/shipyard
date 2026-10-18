//! Published ports: `-p [ip:][hostPort:]containerPort[/tcp|udp]` and `-P`,
//! and holding the host's ports while a container uses them.
//!
//! A published port is a rule (a DNAT map element, see `nft`): traffic to
//! the host's port goes to the container's. The host's port is also *held*:
//! the run binds a socket to it, without `listen`, for its whole life. No
//! other program can then bind it (it would get EADDRINUSE, as it would for
//! any program's port), and two runs can't publish the same one. The socket
//! never sees a packet: the DNAT happens before the kernel looks for one.
//!
//! A TCP port only closed connections still wait on (`TIME_WAIT`, the
//! minute or so after the side that closed first, here pasta for a
//! rootless container, let go) is held all the same, with `SO_REUSEADDR`
//! for one call, under a lock (`after_time_wait`, Linux).
//!
//! A port on every address is published (and held) on IPv4 and IPv6; one
//! on an address, on that address's family. An IPv6 address is written in
//! brackets, as in a URL: `[2001:db8::1]:8080:80`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::OwnedFd;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The most ports one `-p` can publish.
const MAX_RANGE: u32 = 1024;

/// A transport protocol a port can be published for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

impl Protocol {
    /// Its IP protocol number (`IPPROTO_TCP`, `IPPROTO_UDP`).
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Protocol::Tcp => 6,
            Protocol::Udp => 17,
        }
    }

    /// Its name, as `-p` writes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

/// What can be wrong with a port to publish.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PortError {
    /// A `-p` that can't be read.
    #[error(
        "{spec:?}: {reason} (expected [IP:][HOST_PORT:]CONTAINER_PORT[/tcp|udp], like 8080:80, \
         127.0.0.1:8080:80/udp or [2001:db8::1]:8080:80)"
    )]
    Invalid {
        /// What was given.
        spec: String,
        /// What's wrong.
        reason: String,
    },
    /// The IPv6 loopback address, which nothing published can be on.
    #[error(
        "{0:?}: a port on [::1] can't reach a container (the kernel never routes the IPv6 loopback \
         address off the host's own interface; 127.0.0.1 can be published on)"
    )]
    Loopback6(String),
    /// The host's port is taken.
    #[error("port {port}/{} on {address} is already in use", protocol.name())]
    InUse {
        /// The host address.
        address: IpAddr,
        /// The port.
        port: u16,
        /// Its protocol.
        protocol: Protocol,
    },
    /// The host's port couldn't be held for another reason.
    #[error("holding port {port}/{} on {address}: {}", protocol.name(), errno.desc())]
    Hold {
        /// The host address.
        address: IpAddr,
        /// The port (0: any).
        port: u16,
        /// Its protocol.
        protocol: Protocol,
        /// Why.
        errno: nix::errno::Errno,
    },
}

/// One port to publish, as asked: `host_port` `None` takes a free one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRequest {
    /// The host address it's published on (`None`: all of them).
    pub host_ip: Option<IpAddr>,
    /// The host's port, if one is asked for.
    pub host_port: Option<u16>,
    /// The container's port.
    pub container_port: u16,
    /// Its protocol.
    pub protocol: Protocol,
}

/// One port published: the host's address and port, to the container's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Published {
    /// The host address (`None`: all of them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<IpAddr>,
    /// The host's port.
    pub host_port: u16,
    /// The container's port.
    pub container_port: u16,
    /// Its protocol.
    pub protocol: Protocol,
}

impl std::fmt::Display for Published {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.host_ip {
            Some(IpAddr::V6(ip)) => write!(f, "[{ip}]")?,
            Some(IpAddr::V4(ip)) => write!(f, "{ip}")?,
            None => write!(f, "{}", Ipv4Addr::UNSPECIFIED)?,
        }
        write!(
            f,
            ":{} -> {}/{}",
            self.host_port,
            self.container_port,
            self.protocol.name()
        )
    }
}

/// Reads one `-p`: the ports it asks for (a range gives several).
pub fn parse_publish(spec: &str) -> Result<Vec<PortRequest>, PortError> {
    let invalid = |reason: &str| PortError::Invalid {
        spec: spec.to_owned(),
        reason: reason.to_owned(),
    };
    let (ports, protocol) = match spec.rsplit_once('/') {
        Some((ports, "tcp")) => (ports, Protocol::Tcp),
        Some((ports, "udp")) => (ports, Protocol::Udp),
        Some(_) => return Err(invalid("the protocol must be tcp or udp")),
        None => (spec, Protocol::Tcp),
    };
    // An IPv6 address, in brackets: `[2001:db8::1]:8080:80`.
    let (bracketed, ports) = match ports.strip_prefix('[') {
        Some(rest) => {
            let (ip, rest) = rest
                .split_once("]:")
                .ok_or_else(|| invalid("an IPv6 address in brackets, then :PORTS"))?;
            let ip: Ipv6Addr = ip
                .parse()
                .map_err(|_| invalid("the host address isn't an IPv6 address"))?;
            if ip.is_loopback() {
                return Err(PortError::Loopback6(spec.to_owned()));
            }
            if ip.is_unicast_link_local() {
                return Err(invalid(
                    "a link-local address (fe80::…) is one link's alone: it can't be published on",
                ));
            }
            (Some(IpAddr::V6(ip)), rest)
        }
        None => (None, ports),
    };
    let parts: Vec<&str> = ports.split(':').collect();
    let (host_ip, host, container) = match (bracketed, parts.as_slice()) {
        (None, [container]) => (None, None, *container),
        (None, [host, container]) => (None, Some(*host), *container),
        (Some(ip), [host, container]) => {
            (Some(ip), (!host.is_empty()).then_some(*host), *container)
        }
        (None, [ip, host, container]) => {
            let ip: Ipv4Addr = ip.parse().map_err(|_| {
                invalid("the host address isn't an IPv4 address (an IPv6 one goes in brackets)")
            })?;
            let host = (!host.is_empty()).then_some(*host);
            (Some(IpAddr::V4(ip)), host, *container)
        }
        _ => return Err(invalid("too many parts")),
    };
    // `0.0.0.0` and `[::]` are every address, as no address is.
    let host_ip = host_ip.filter(|ip| !ip.is_unspecified());
    let container = parse_range(container).ok_or_else(|| invalid("a port is 1 to 65535"))?;
    let host = host
        .map(|host| parse_range(host).ok_or_else(|| invalid("a port is 1 to 65535")))
        .transpose()?;
    let count = u32::from(container.1 - container.0) + 1;
    if count > MAX_RANGE {
        return Err(invalid(&format!(
            "a range can have at most {MAX_RANGE} ports"
        )));
    }
    if let Some(host) = host
        && host.1 - host.0 != container.1 - container.0
    {
        return Err(invalid(
            "the host's and the container's ranges must be the same length",
        ));
    }
    Ok((0..=container.1 - container.0)
        .map(|step| PortRequest {
            host_ip,
            host_port: host.map(|host| host.0 + step),
            container_port: container.0 + step,
            protocol,
        })
        .collect())
}

/// `N` or `N-M` (N ≤ M), each 1 to 65535.
fn parse_range(text: &str) -> Option<(u16, u16)> {
    let port = |text: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|&port| port != 0 && text.bytes().all(|b| b.is_ascii_digit()))
    };
    match text.split_once('-') {
        Some((start, end)) => {
            let (start, end) = (port(start)?, port(end)?);
            (start <= end).then_some((start, end))
        }
        None => port(text).map(|port| (port, port)),
    }
}

/// An image's `ExposedPorts` key (`80/tcp`, `53/udp`, or `8080`, TCP) as a
/// port to publish on a free host port (`-P`); `None` for one that can't be
/// read.
#[must_use]
pub fn parse_exposed(key: &str) -> Option<PortRequest> {
    let (port, protocol) = match key.split_once('/') {
        Some((port, "tcp")) => (port, Protocol::Tcp),
        Some((port, "udp")) => (port, Protocol::Udp),
        Some(_) => return None,
        None => (key, Protocol::Tcp),
    };
    let (container_port, end) = parse_range(port)?;
    (container_port == end).then_some(PortRequest {
        host_ip: None,
        host_port: None,
        container_port,
        protocol,
    })
}

/// A host port held for a run: the sockets bound to it (IPv4, and for one
/// published on every address, IPv6 too: a program listening on `[::]:PORT`
/// would otherwise get the IPv6 clients). Dropping it frees the port, unless
/// a copy of its [`sockets`](Self::sockets) is still open.
#[derive(Debug)]
pub struct HeldPort {
    /// What's published.
    pub published: Published,
    sockets: Vec<OwnedFd>,
}

impl HeldPort {
    /// The bound sockets (one or two), for a process that keeps the port
    /// held (a container's monitor).
    pub fn sockets(&self) -> impl Iterator<Item = std::os::fd::BorrowedFd<'_>> {
        self.sockets.iter().map(std::os::fd::AsFd::as_fd)
    }

    /// Whether it's held on IPv6's every address (`[::]`) too: published on
    /// every address, on a host with IPv6.
    #[must_use]
    pub fn also_ipv6(&self) -> bool {
        self.published.host_ip.is_none() && self.sockets.len() > 1
    }
}

/// Holds the host's port for each of `requests`, in order: the ports asked
/// for, or free ones the kernel picks. Either all are held, or none.
/// `lock_dir`, the caller's own runtime directory, holds the lock a port
/// that only closed connections wait on is held under (Linux: see
/// `after_time_wait`); without one, it's held under an abstract socket
/// name, as before, which any user can take.
pub fn hold(requests: &[PortRequest], lock_dir: Option<&Path>) -> Result<Vec<HeldPort>, PortError> {
    requests
        .iter()
        .map(|request| hold_one(request, lock_dir))
        .collect()
}

/// Holds one port: on its address (in that address's family); or, on every
/// address, on IPv4 and on IPv6 (if the host has IPv6), the same port.
fn hold_one(request: &PortRequest, lock_dir: Option<&Path>) -> Result<HeldPort, PortError> {
    use nix::sys::socket::{AddressFamily, SockaddrIn, SockaddrIn6, getsockname};
    use std::os::fd::AsRawFd as _;
    let address = request.host_ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let asked = request.host_port.unwrap_or(0);
    let fail = |port, errno| match errno {
        nix::errno::Errno::EADDRINUSE => PortError::InUse {
            address,
            port,
            protocol: request.protocol,
        },
        errno => PortError::Hold {
            address,
            port,
            protocol: request.protocol,
            errno,
        },
    };
    let ipv6 = |ip: Ipv6Addr, port| {
        bind_port(
            request.protocol,
            AddressFamily::Inet6,
            &SockaddrIn6::from(std::net::SocketAddrV6::new(ip, port, 0, 0)),
            port,
            lock_dir,
        )
    };
    let (first, host_port) = match address {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            let socket = bind_port(
                request.protocol,
                AddressFamily::Inet,
                &SockaddrIn::new(a, b, c, d, asked),
                asked,
                lock_dir,
            )
            .map_err(|errno| fail(asked, errno))?;
            let port = getsockname::<SockaddrIn>(socket.as_raw_fd())
                .map_err(|errno| fail(asked, errno))?
                .port();
            (socket, port)
        }
        IpAddr::V6(ip) => {
            let socket = ipv6(ip, asked).map_err(|errno| fail(asked, errno))?;
            let port = getsockname::<SockaddrIn6>(socket.as_raw_fd())
                .map_err(|errno| fail(asked, errno))?
                .port();
            (socket, port)
        }
    };
    let mut sockets = vec![first];
    if request.host_ip.is_none() {
        match ipv6(Ipv6Addr::UNSPECIFIED, host_port) {
            Ok(socket) => sockets.push(socket),
            // No IPv6 here: nothing to hold.
            Err(nix::errno::Errno::EAFNOSUPPORT | nix::errno::Errno::EADDRNOTAVAIL) => {}
            Err(errno) => return Err(fail(host_port, errno)),
        }
    }
    Ok(HeldPort {
        published: Published {
            host_ip: request.host_ip,
            host_port,
            container_port: request.container_port,
            protocol: request.protocol,
        },
        sockets,
    })
}

/// [`bind_socket`], to `address` with its `port`; a TCP port that only
/// closed connections still wait on is held all the same (Linux:
/// [`after_time_wait`]).
fn bind_port(
    protocol: Protocol,
    family: nix::sys::socket::AddressFamily,
    address: &dyn nix::sys::socket::SockaddrLike,
    port: u16,
    lock_dir: Option<&Path>,
) -> Result<OwnedFd, nix::errno::Errno> {
    match bind_socket(protocol, family, address) {
        #[cfg(target_os = "linux")]
        Err(nix::errno::Errno::EADDRINUSE) if protocol == Protocol::Tcp && port != 0 => {
            after_time_wait::bind(family, address, port, lock_dir)
        }
        bound => {
            let _ = (port, lock_dir);
            bound
        }
    }
}

/// A socket of `protocol` in `family`, bound to `address`, never listening.
/// Close-on-exec: never a container's. No `SO_REUSEADDR`: a port another
/// socket has (or a closed connection still waits on, `TIME_WAIT`) is
/// refused, as for any program (but see [`bind_port`]). An IPv6 one is IPv6
/// only.
fn bind_socket(
    protocol: Protocol,
    family: nix::sys::socket::AddressFamily,
    address: &dyn nix::sys::socket::SockaddrLike,
) -> Result<OwnedFd, nix::errno::Errno> {
    use std::os::fd::AsRawFd as _;
    let socket = unbound_socket(protocol, family)?;
    nix::sys::socket::bind(socket.as_raw_fd(), address)?;
    Ok(socket)
}

/// A socket of `protocol` in `family`, not bound yet: close-on-exec, and
/// an IPv6 one IPv6 only.
fn unbound_socket(
    protocol: Protocol,
    family: nix::sys::socket::AddressFamily,
) -> Result<OwnedFd, nix::errno::Errno> {
    use nix::sys::socket::{SockFlag, SockProtocol, SockType, setsockopt, socket, sockopt};
    let (kind, protocol) = match protocol {
        Protocol::Tcp => (SockType::Stream, SockProtocol::Tcp),
        Protocol::Udp => (SockType::Datagram, SockProtocol::Udp),
    };
    // macOS has no SOCK_CLOEXEC: there it's set at once after.
    #[cfg(target_os = "linux")]
    let flags = SockFlag::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = SockFlag::empty();
    let socket = socket(family, kind, flags, protocol)?;
    #[cfg(not(target_os = "linux"))]
    nix::fcntl::fcntl(
        &socket,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )?;
    if family == nix::sys::socket::AddressFamily::Inet6 {
        setsockopt(&socket, sockopt::Ipv6V6Only, &true)?;
    }
    Ok(socket)
}

/// Holding a TCP port that only closed connections still wait on.
///
/// The side of a TCP connection that closes first keeps its address and
/// port in `TIME_WAIT` for a minute or so (twice the longest a segment can
/// live: TLPI §61.6.6), so that a stray segment of the old connection
/// can't be taken for a new one's. A rootless container's pasta is that
/// side for its published ports: once it served a client, a start again
/// would find its port "in use" for that minute. A plain `bind` refuses a
/// port in `TIME_WAIT`; one with `SO_REUSEADDR` is let through.
///
/// But `SO_REUSEADDR` also lets two sockets that both have it, neither
/// listening, bind one port: two runs could both hold it. So, Linux's rules
/// (checked by a probe, as root and not): a socket bound with it, and at
/// once without it again, holds the port as a plain one does (every later
/// `bind` is refused, with `SO_REUSEADDR` or not, listening or not). That
/// leaves the moment between the two calls, and what else is on the port:
///
/// 1. Under a lock every run of this user's Shipyard holding a port this
///    way takes: `port-reservations.lock` in its own runtime directory
///    (mode 0700: no other user can take it, F53), freed when its holder
///    ends, even killed. Without one (a rootless run with no
///    `XDG_RUNTIME_DIR`, a root whose `/run` isn't writable), the abstract
///    socket name `@shipyard/port-reservations`, as before: any user can
///    take that one. Two users' Shipyards, or a v0.2.0 restart waiter
///    beside this one, don't share a lock: each still passes step 3, but
///    two asking for one port at the same instant could both pass step 2's
///    moment and both hold it (Linux shares `SO_REUSEADDR` across users).
/// 2. Bound with `SO_REUSEADDR`, which is cleared at once.
/// 3. Kept only if every socket the kernel's tables list on the port
///    (`/proc/net/tcp` and `tcp6`) is in `TIME_WAIT`: a listener, a
///    connection still open, or another program's listener from that
///    moment (outside the lock) means the port is let go, and in use. (The
///    tables don't list a socket only bound, never listening. One bound
///    with `SO_REUSEADDR` can be beside the port held, but once it's held
///    it can't `listen`, nor can any other: probed. The port is this run's
///    to serve on.)
///
/// The lock not had within `LOCK_WAIT` (a stuck holder, or another
/// process holding the name) is a port in use, as before.
#[cfg(target_os = "linux")]
mod after_time_wait {
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::sys::socket::{AddressFamily, SockaddrLike, setsockopt, sockopt};

    use crate::observe::{self, State, Table};

    /// The lock's file, in the caller's runtime directory.
    const LOCK_FILE: &str = "port-reservations.lock";

    /// The lock's name without a runtime directory, in the abstract
    /// namespace (no file: any user can bind it).
    pub(super) const LOCK_NAME: &[u8] = b"shipyard/port-reservations";

    /// How long the lock is waited for.
    pub(super) const LOCK_WAIT: Duration = Duration::from_secs(2);

    /// `address` (with its `port`) held, if only closed connections wait
    /// on it; else `EADDRINUSE`.
    pub(super) fn bind(
        family: AddressFamily,
        address: &dyn SockaddrLike,
        port: u16,
        lock_dir: Option<&std::path::Path>,
    ) -> Result<OwnedFd, Errno> {
        let Some(_lock) = Lock::take(lock_dir, LOCK_WAIT) else {
            return Err(Errno::EADDRINUSE);
        };
        let socket = super::unbound_socket(super::Protocol::Tcp, family)?;
        setsockopt(&socket, sockopt::ReuseAddr, &true)?;
        nix::sys::socket::bind(socket.as_raw_fd(), address)?;
        // Held as a plain socket holds it, from here on.
        setsockopt(&socket, sockopt::ReuseAddr, &false)?;
        // Anything else on the port (a listener, a connection still open,
        // one that came meanwhile from outside the lock): let go.
        if !only_time_wait(port) {
            return Err(Errno::EADDRINUSE);
        }
        Ok(socket)
    }

    /// Whether every TCP socket the tables list on `port` (any address,
    /// either family) is in `TIME_WAIT` (this process's held ones, only
    /// bound, aren't listed). A table that can't be read says no.
    fn only_time_wait(port: u16) -> bool {
        [
            ("/proc/net/tcp", Table::Tcp),
            ("/proc/net/tcp6", Table::Tcp6),
        ]
        .into_iter()
        .all(|(path, table)| {
            let Ok(file) = std::fs::File::open(path) else {
                // No IPv6 here: no table, no sockets.
                return table == Table::Tcp6 && !std::path::Path::new(path).exists();
            };
            observe::read(std::io::BufReader::new(file), table, |socket| {
                socket.local.port() == port && socket.state != State::TimeWait
            })
            .is_ok_and(|others| others.is_empty())
        })
    }

    /// The lock, held while this lives: [`LOCK_FILE`] in the caller's
    /// runtime directory, or [`LOCK_NAME`] without one (held by a socket
    /// bound to it).
    pub(super) struct Lock {
        _file: Option<nix::fcntl::Flock<std::fs::File>>,
        _name: Option<OwnedFd>,
    }

    impl Lock {
        /// The lock in `dir` (made, the caller's alone, if it's missing, as
        /// the runtime makes it), or the abstract name if there's none or
        /// it can't be made or opened, waited for up to `wait`: `None` if
        /// it isn't had by then.
        pub(super) fn take(dir: Option<&std::path::Path>, wait: Duration) -> Option<Lock> {
            let deadline = Instant::now() + wait;
            match dir.and_then(lock_file) {
                Some(file) => shipyard_sys::lock::lock_until(
                    file,
                    shipyard_sys::lock::Mode::Exclusive,
                    deadline,
                    Duration::from_millis(5),
                )
                .ok()
                .map(|file| Lock {
                    _file: Some(file),
                    _name: None,
                }),
                None => abstract_name(deadline).map(|name| Lock {
                    _file: None,
                    _name: Some(name),
                }),
            }
        }
    }

    /// [`LOCK_FILE`] in `dir`, opened (`dir` made if it's missing).
    fn lock_file(dir: &std::path::Path) -> Option<std::fs::File> {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .ok()?;
        shipyard_sys::fs::open_lock_file(&dir.join(LOCK_FILE)).ok()
    }

    /// A socket bound to [`LOCK_NAME`], tried until `deadline`.
    fn abstract_name(deadline: Instant) -> Option<OwnedFd> {
        use nix::sys::socket::{SockFlag, SockType, UnixAddr, socket};
        let name = UnixAddr::new_abstract(LOCK_NAME).ok()?;
        loop {
            let socket = socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC,
                None,
            )
            .ok()?;
            match nix::sys::socket::bind(socket.as_raw_fd(), &name) {
                Ok(()) => return Some(socket),
                Err(Errno::EADDRINUSE) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn one(spec: &str) -> PortRequest {
        let requests = parse_publish(spec).unwrap();
        assert_eq!(requests.len(), 1, "{spec}");
        requests[0]
    }

    #[test]
    fn the_forms_of_p() {
        assert_eq!(
            one("80"),
            PortRequest {
                host_ip: None,
                host_port: None,
                container_port: 80,
                protocol: Protocol::Tcp
            }
        );
        assert_eq!(one("8080:80").host_port, Some(8080));
        assert_eq!(one("8080:80/udp").protocol, Protocol::Udp);
        let local = one("127.0.0.1:8080:80");
        assert_eq!(local.host_ip, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let v6 = one("[2001:db8::1]:8080:80/udp");
        assert_eq!(v6.host_ip, Some("2001:db8::1".parse().unwrap()));
        assert_eq!(v6.host_port, Some(8080));
        assert_eq!(v6.protocol, Protocol::Udp);
        // Every address, as no address is.
        assert_eq!(one("[::]::80").host_port, None);
        assert_eq!(one("[::]::80").host_ip, None);
        assert_eq!(one("0.0.0.0:8080:80").host_ip, None);
        assert_eq!(local.host_port, Some(8080));
        // An address, any port.
        assert_eq!(one("127.0.0.1::80").host_port, None);
        let range = parse_publish("8000-8002:80-82").unwrap();
        assert_eq!(
            range
                .iter()
                .map(|r| (r.host_port, r.container_port))
                .collect::<Vec<_>>(),
            [(Some(8000), 80), (Some(8001), 81), (Some(8002), 82)]
        );
        assert_eq!(parse_publish("80-81").unwrap().len(), 2);
    }

    #[test]
    fn what_p_refuses() {
        for bad in [
            "",
            "0",
            "65536",
            "x",
            "80/sctp",
            "8000-8001:80",
            "82-80",
            "1:2:3:4:5",
            "a.b:1:2",
            "-1",
            "+80",
            "1-2000",
        ] {
            assert!(
                matches!(parse_publish(bad), Err(PortError::Invalid { .. })),
                "{bad:?}: {:?}",
                parse_publish(bad)
            );
        }
        // IPv6 without its brackets, or with nothing after them.
        for bad in [
            "::1:8080:80",
            "2001:db8::1:8080:80",
            "[2001:db8::1]",
            "[x]:80:80",
            "[::]:1:2:3",
            "[fe80::1]:8080:80",
            "[fe80::1%eth0]:8080:80",
        ] {
            assert!(
                matches!(parse_publish(bad), Err(PortError::Invalid { .. })),
                "{bad}: {:?}",
                parse_publish(bad)
            );
        }
        assert!(matches!(
            parse_publish("[::1]:8080:80"),
            Err(PortError::Loopback6(_))
        ));
    }

    #[test]
    fn exposed_ports() {
        assert_eq!(parse_exposed("80/tcp").unwrap().container_port, 80);
        assert_eq!(parse_exposed("53/udp").unwrap().protocol, Protocol::Udp);
        assert_eq!(parse_exposed("8080").unwrap().protocol, Protocol::Tcp);
        assert_eq!(parse_exposed("80/sctp"), None);
        assert_eq!(parse_exposed("x/tcp"), None);
    }

    #[test]
    fn a_held_port_is_taken_and_a_free_one_is_chosen() {
        let held = hold(
            &[PortRequest {
                host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                host_port: None,
                container_port: 80,
                protocol: Protocol::Tcp,
            }],
            None,
        )
        .unwrap();
        let port = held[0].published.host_port;
        assert_ne!(port, 0);
        let again = hold(
            &[PortRequest {
                host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                host_port: Some(port),
                container_port: 80,
                protocol: Protocol::Tcp,
            }],
            None,
        );
        assert!(
            matches!(again, Err(PortError::InUse { port: p, .. }) if p == port),
            "{again:?}"
        );
        // Free again once dropped: once every copy is closed. A test beside
        // this one that spawns a process holds a copy of every descriptor
        // here between its `fork` and its `exec` (close-on-exec closes them
        // only then), so it's waited for, with a bound.
        drop(held);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match hold(
                &[PortRequest {
                    host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    host_port: Some(port),
                    container_port: 80,
                    protocol: Protocol::Tcp,
                }],
                None,
            ) {
                Ok(_) => break,
                Err(PortError::InUse { .. }) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("not free again: {error:?}"),
            }
        }
    }

    #[test]
    fn a_port_on_every_address_is_held_on_ipv6_too() {
        let held = hold(
            &[PortRequest {
                host_ip: None,
                host_port: None,
                container_port: 80,
                protocol: Protocol::Tcp,
            }],
            None,
        )
        .unwrap();
        let port = held[0].published.host_port;
        // Where the host has IPv6 at all: an IPv6-only socket (which the
        // IPv4 one alone wouldn't stop) can't have it.
        if std::net::TcpListener::bind("[::1]:0").is_ok() {
            let any = std::net::SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, port, 0, 0);
            let error = bind_socket(
                Protocol::Tcp,
                nix::sys::socket::AddressFamily::Inet6,
                &nix::sys::socket::SockaddrIn6::from(any),
            )
            .unwrap_err();
            assert_eq!(error, nix::errno::Errno::EADDRINUSE);
        }
    }

    #[cfg(target_os = "linux")]
    /// A port whose server side closed first: that side waits on it.
    fn time_wait_port() -> u16 {
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (served, _) = listener.accept().unwrap();
        drop(served);
        std::thread::sleep(Duration::from_millis(50));
        drop(client);
        drop(listener);
        std::thread::sleep(Duration::from_millis(50));
        port
    }

    #[cfg(target_os = "linux")]
    /// `port` held on every address, TCP, the lock in `locks`.
    fn hold_tcp(port: u16, locks: Option<&Path>) -> Result<Vec<HeldPort>, PortError> {
        hold(
            &[PortRequest {
                host_ip: None,
                host_port: Some(port),
                container_port: 80,
                protocol: Protocol::Tcp,
            }],
            locks,
        )
    }

    #[cfg(target_os = "linux")]
    /// Whether `held` failed as `port` in use.
    fn in_use(held: &Result<Vec<HeldPort>, PortError>, port: u16) -> bool {
        matches!(held, Err(PortError::InUse { port: p, .. }) if *p == port)
    }

    #[cfg(target_os = "linux")]
    /// A runtime directory of the test's own, for the reservation lock.
    fn locks_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-ports-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A TCP port only a closed connection waits on (`TIME_WAIT`) is held
    /// (F42), then as any held port: refused to a second hold, and to a
    /// program binding with `SO_REUSEADDR` (std's listeners do). One that
    /// another socket is on too, or whose lock another process holds, is
    /// still in use. (One test: the lock is held in its last part.)
    #[cfg(target_os = "linux")]
    #[test]
    fn a_port_closed_connections_wait_on_is_held() {
        use nix::sys::socket::{AddressFamily, SockaddrIn, setsockopt, sockopt};
        use std::os::fd::AsRawFd as _;
        use std::time::Duration;
        let locks = locks_dir();
        let time_wait = time_wait_port;
        let on_every_address = |port| hold_tcp(port, Some(&locks));
        let port = time_wait();
        assert_eq!(
            bind_socket(
                Protocol::Tcp,
                AddressFamily::Inet,
                &SockaddrIn::new(0, 0, 0, 0, port)
            )
            .unwrap_err(),
            nix::errno::Errno::EADDRINUSE,
            "a plain bind isn't refused: the test shows nothing"
        );
        let held = on_every_address(port).unwrap();
        let again = on_every_address(port);
        assert!(in_use(&again, port), "{again:?}");
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_err(),
            "held, but not from SO_REUSEADDR"
        );
        drop(held);

        // Another program's socket on it, bound with SO_REUSEADDR, never
        // listening (the kernel's tables don't list one): held beside it,
        // which then can't listen, nor can any other.
        let port = time_wait();
        let other = unbound_socket(Protocol::Tcp, AddressFamily::Inet).unwrap();
        setsockopt(&other, sockopt::ReuseAddr, &true).unwrap();
        nix::sys::socket::bind(other.as_raw_fd(), &SockaddrIn::new(0, 0, 0, 0, port)).unwrap();
        let held = on_every_address(port).unwrap();
        let backlog = nix::sys::socket::Backlog::new(1).unwrap();
        assert_eq!(
            nix::sys::socket::listen(&other, backlog).unwrap_err(),
            nix::errno::Errno::EADDRINUSE
        );
        drop((held, other));

        // A connection still open on it (its listener gone): in use.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _served = listener.accept().unwrap();
        drop(listener);
        let open = on_every_address(port);
        assert!(in_use(&open, port), "{open:?}");

        // The lock held by another: in use, as before.
        let port = time_wait();
        let lock = after_time_wait::Lock::take(Some(&locks), Duration::ZERO).unwrap();
        let locked = on_every_address(port);
        assert!(in_use(&locked, port), "{locked:?}");
        drop(lock);
        on_every_address(port).unwrap();

        std::fs::remove_dir_all(&locks).unwrap();
    }

    /// The reservation lock is the caller's own (F53): the abstract name it
    /// once was, held by anyone (another user could), is no longer in the
    /// way. Without a runtime directory it's that name, as before: held by
    /// another, the port is in use; free, it's held. (The lock file held by
    /// another run is in the way: the test above.)
    /// A socket bound to the reservation lock's abstract name: held as any
    /// local user could hold it.
    #[cfg(target_os = "linux")]
    fn squat() -> OwnedFd {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, socket};
        use std::os::fd::AsRawFd as _;
        let squatter = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .unwrap();
        let name = UnixAddr::new_abstract(after_time_wait::LOCK_NAME).unwrap();
        nix::sys::socket::bind(squatter.as_raw_fd(), &name).unwrap();
        squatter
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_reservation_lock_is_the_callers_own() {
        let locks = locks_dir();
        // The abstract name the lock once was, held by anyone (another user
        // could): no longer in the way (F53). Positive control above: the
        // lock file held is.
        let port = time_wait_port();
        let squatter = squat();
        let started = std::time::Instant::now();
        hold_tcp(port, Some(&locks)).unwrap();
        assert!(
            started.elapsed() < after_time_wait::LOCK_WAIT,
            "waited for the abstract name"
        );
        drop(squatter);

        // No runtime directory: the abstract name, as before. Held by
        // another, the port is in use; free, it's held.
        let port = time_wait_port();
        let squatter = squat();
        let without = hold_tcp(port, None);
        assert!(in_use(&without, port), "{without:?}");
        drop(squatter);
        hold_tcp(port, None).unwrap();
        std::fs::remove_dir_all(&locks).unwrap();
    }

    #[test]
    fn published_ports_are_written_as_docker_shows_them() {
        let published = Published {
            host_ip: None,
            host_port: 8080,
            container_port: 80,
            protocol: Protocol::Tcp,
        };
        assert_eq!(published.to_string(), "0.0.0.0:8080 -> 80/tcp");
        let on_v6 = Published {
            host_ip: Some("2001:db8::1".parse().unwrap()),
            ..published
        };
        assert_eq!(on_v6.to_string(), "[2001:db8::1]:8080 -> 80/tcp");
        let json = serde_json::to_string(&published).unwrap();
        assert_eq!(
            json,
            r#"{"host_port":8080,"container_port":80,"protocol":"tcp"}"#
        );
        assert_eq!(serde_json::from_str::<Published>(&json).unwrap(), published);
    }

    proptest! {
        #[test]
        fn parsing_never_panics(spec in "\\PC{0,40}") {
            let _ = parse_publish(&spec);
            let _ = parse_exposed(&spec);
        }

        #[test]
        fn a_range_is_its_length(start in 1u16..60000, len in 0u16..100) {
            let end = start + len;
            let requests = parse_publish(&format!("{start}-{end}:{start}-{end}")).unwrap();
            prop_assert_eq!(requests.len(), usize::from(len) + 1);
        }
    }
}
