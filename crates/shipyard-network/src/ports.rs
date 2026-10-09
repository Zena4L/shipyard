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
//! A port on every address is published (and held) on IPv4 and IPv6; one
//! on an address, on that address's family. An IPv6 address is written in
//! brackets, as in a URL: `[2001:db8::1]:8080:80`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::OwnedFd;

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
/// would otherwise get the IPv6 clients). Dropping it frees the port.
#[derive(Debug)]
pub struct HeldPort {
    /// What's published.
    pub published: Published,
    _sockets: Vec<OwnedFd>,
}

/// Holds the host's port for each of `requests`, in order: the ports asked
/// for, or free ones the kernel picks. Either all are held, or none.
pub fn hold(requests: &[PortRequest]) -> Result<Vec<HeldPort>, PortError> {
    requests.iter().map(hold_one).collect()
}

/// Holds one port: on its address (in that address's family); or, on every
/// address, on IPv4 and on IPv6 (if the host has IPv6), the same port.
fn hold_one(request: &PortRequest) -> Result<HeldPort, PortError> {
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
        bind_socket(
            request.protocol,
            AddressFamily::Inet6,
            &SockaddrIn6::from(std::net::SocketAddrV6::new(ip, port, 0, 0)),
        )
    };
    let (first, host_port) = match address {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            let socket = bind_socket(
                request.protocol,
                AddressFamily::Inet,
                &SockaddrIn::new(a, b, c, d, asked),
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
        _sockets: sockets,
    })
}

/// A socket of `protocol` in `family`, bound to `address`, never listening.
/// Close-on-exec: never a container's. No `SO_REUSEADDR`: a port another
/// socket has (or a closed connection still waits on, `TIME_WAIT`) is
/// refused, as for any program. An IPv6 one is IPv6 only.
fn bind_socket(
    protocol: Protocol,
    family: nix::sys::socket::AddressFamily,
    address: &dyn nix::sys::socket::SockaddrLike,
) -> Result<OwnedFd, nix::errno::Errno> {
    use nix::sys::socket::{SockFlag, SockProtocol, SockType, bind, setsockopt, socket, sockopt};
    use std::os::fd::AsRawFd as _;
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
    bind(socket.as_raw_fd(), address)?;
    Ok(socket)
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
        let held = hold(&[PortRequest {
            host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            host_port: None,
            container_port: 80,
            protocol: Protocol::Tcp,
        }])
        .unwrap();
        let port = held[0].published.host_port;
        assert_ne!(port, 0);
        let again = hold(&[PortRequest {
            host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            host_port: Some(port),
            container_port: 80,
            protocol: Protocol::Tcp,
        }]);
        assert!(
            matches!(again, Err(PortError::InUse { port: p, .. }) if p == port),
            "{again:?}"
        );
        // Free again once dropped.
        drop(held);
        hold(&[PortRequest {
            host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            host_port: Some(port),
            container_port: 80,
            protocol: Protocol::Tcp,
        }])
        .unwrap();
    }

    #[test]
    fn a_port_on_every_address_is_held_on_ipv6_too() {
        let held = hold(&[PortRequest {
            host_ip: None,
            host_port: None,
            container_port: 80,
            protocol: Protocol::Tcp,
        }])
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
