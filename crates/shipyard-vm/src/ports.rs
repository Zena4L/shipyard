//! The guest's published ports on the Mac (step 10.3c1, D10.3-10): which
//! addresses the VM's host listens on for them ([`wanted`]), and the file
//! it says what it forwards in ([`PortsFile`], for `vm status` and
//! `doctor`). The listening itself is the host's.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use serde::{Deserialize, Serialize};
use shipyard_runtime::attach::forward::{Protocol, PublishedPort};

use crate::VmError;

/// The most ports the Mac listens on for the guest: containers that
/// publish more, all told, don't get the Mac's descriptors for them all.
pub const MAX_PORTS: usize = 256;

/// One of the Mac's addresses listened on, for one protocol (TCP's port 53
/// and UDP's are two).
pub type Place = (Protocol, SocketAddr);

/// Where the Mac listens for the guest's published ports, each address
/// with the port in the guest its connections go to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    /// The Mac's addresses (its loopback's only, D10.3-10), each with what
    /// it forwards to.
    pub addresses: BTreeMap<Place, PublishedPort>,
    /// How many ports were left out past [`MAX_PORTS`].
    pub left_out: usize,
    /// Published ports whose Mac address another already has (two
    /// containers on one port of two guest addresses): the first kept.
    pub clashes: Vec<PublishedPort>,
}

/// Where the Mac listens for the guest's `ports` (D10.3-10: on its
/// loopback only, so nothing on its network reaches a container): one
/// published on every address on both `127.0.0.1` and `::1`; one on
/// `127.0.0.1`, or another IPv4 address of the guest's (which the Mac
/// hasn't), on `127.0.0.1`; one on another IPv6 address on `::1`; one on
/// the guest's `::1` on `::1` only from a guest that `dials_containers`
/// (its answer v2: step 13.5a), an older one can't reach it (it isn't
/// DNAT'd). Each protocol apart. Port 0 is left out, and past [`MAX_PORTS`]
/// ports (a protocol's port each), the rest.
#[must_use]
pub fn wanted(ports: &[PublishedPort], dials_containers: bool) -> Wanted {
    let mut sorted = ports.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let numbers: BTreeSet<(u16, Protocol)> = sorted
        .iter()
        .map(|published| (published.port, published.protocol))
        .filter(|&(port, _)| port != 0)
        .collect();
    let kept: BTreeSet<(u16, Protocol)> = numbers.iter().copied().take(MAX_PORTS).collect();
    let mut wanted = Wanted {
        left_out: numbers.len() - kept.len(),
        ..Wanted::default()
    };
    for published in sorted {
        if !kept.contains(&(published.port, published.protocol)) {
            continue;
        }
        let v4 = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), published.port);
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), published.port);
        let on: &[SocketAddr] = match published.ip {
            ip if ip.is_unspecified() => &[v4, v6],
            IpAddr::V6(ip) if ip.is_loopback() && !dials_containers => &[],
            IpAddr::V4(_) => &[v4],
            IpAddr::V6(_) => &[v6],
        };
        let mut clashed = false;
        for &address in on {
            match wanted.addresses.entry((published.protocol, address)) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(published);
                }
                // (Every address twice, `0.0.0.0` and `::`, is one.)
                std::collections::btree_map::Entry::Occupied(held) => {
                    clashed |= !(held.get().ip.is_unspecified() && published.ip.is_unspecified());
                }
            }
        }
        if clashed {
            wanted.clashes.push(published);
        }
    }
    wanted
}

/// `ports.json`'s format, written in it from the first (H2): v2 (step
/// 13.5a) adds the UDP ports forwarded and not.
pub const PORTS_FILE_VERSION: u32 = 2;

/// The most `ports.json` is read as.
const PORTS_FILE_CAP: u64 = 256 * 1024;

/// What the VM's host forwards, in its directory's `ports.json`: written
/// whole at each change, gone when the host ends (a host killed leaves it:
/// a reader checks the host runs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortsFile {
    /// [`PORTS_FILE_VERSION`] when written.
    pub version: u32,
    /// The Mac's addresses listened on for the guest's TCP ports.
    pub forwarded: Vec<SocketAddr>,
    /// Those wanted that can't be listened on, each with why; tried again.
    pub unbound: Vec<(SocketAddr, Unbound)>,
    /// The Mac's addresses listened on for its UDP ports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwarded_udp: Vec<SocketAddr>,
    /// Those wanted for UDP that can't be listened on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unbound_udp: Vec<(SocketAddr, Unbound)>,
    /// How many UDP ports an older guest publishes, which it doesn't list:
    /// not forwarded.
    pub udp: u32,
    /// The host's idle limit, in seconds (`SHIPYARD_VM_IDLE`'s, or the
    /// default): what `doctor` says of a running VM.
    pub idle_seconds: u64,
}

/// Why one of the Mac's addresses can't be listened on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Unbound {
    /// Another program on the Mac listens on its port (on it, or on every
    /// address): left to it.
    InUse,
    /// Another failure (the Mac's IPv6 loopback off), as it was said.
    Other(String),
}

impl std::fmt::Display for Unbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUse => f.write_str("in use on this Mac"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl PortsFile {
    /// `forwarded`, `unbound` (each of either protocol), an older guest's
    /// `udp` count and the idle limit, at this version.
    #[must_use]
    pub fn new(
        forwarded: &[Place],
        unbound: &[(Place, Unbound)],
        udp: u32,
        idle: std::time::Duration,
    ) -> Self {
        let of = |wanted: Protocol| -> Vec<SocketAddr> {
            forwarded
                .iter()
                .filter(|(protocol, _)| *protocol == wanted)
                .map(|(_, address)| *address)
                .collect()
        };
        let unbound_of = |wanted: Protocol| -> Vec<(SocketAddr, Unbound)> {
            unbound
                .iter()
                .filter(|((protocol, _), _)| *protocol == wanted)
                .map(|((_, address), why)| (*address, why.clone()))
                .collect()
        };
        Self {
            version: PORTS_FILE_VERSION,
            forwarded: of(Protocol::Tcp),
            unbound: unbound_of(Protocol::Tcp),
            forwarded_udp: of(Protocol::Udp),
            unbound_udp: unbound_of(Protocol::Udp),
            udp,
            idle_seconds: idle.as_secs(),
        }
    }

    /// Writes it at `path`, whole.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`].
    pub fn write(&self, path: &Path) -> Result<(), VmError> {
        // Not synced: the host writes it again at each change while it runs.
        crate::state_file::write(path, self, shipyard_sys::fs::Durability::Atomic)
    }

    /// Reads it from `path`: `None` if there's none.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`]: unreadable, not this format, or a newer version.
    pub fn read(path: &Path) -> Result<Option<Self>, VmError> {
        crate::state_file::read(path, PORTS_FILE_CAP, PORTS_FILE_VERSION, "")
    }
}

/// The forwarded ports of `file` another program on this Mac holds on
/// every address (F98: bound after the port was forwarded, with
/// `SO_REUSEADDR`, which BSD lets beside the VM's `127.0.0.1:P`; `localhost`
/// then reaches the container, never that program), each once, in order.
#[must_use]
pub fn shadowed(file: &PortsFile) -> Vec<(u16, Protocol)> {
    let ports: BTreeSet<(u16, Protocol)> = file
        .forwarded
        .iter()
        .map(|address| (address.port(), Protocol::Tcp))
        .chain(
            file.forwarded_udp
                .iter()
                .map(|address| (address.port(), Protocol::Udp)),
        )
        .collect();
    ports
        .into_iter()
        .filter(|&(port, protocol)| held_on_every_address(port, protocol))
        .collect()
}

/// Whether another program holds `port` on every address (`0.0.0.0`, or
/// `::` either way): a bind of each there with `SO_REUSEADDR`, never
/// listened on and closed at once, is refused (`EADDRINUSE`) only then (the
/// VM's own `127.0.0.1:P` and `[::1]:P` don't refuse it: checked on macOS
/// 27, step 13.5b; BSD's rules: Linux's would refuse it there, and this runs
/// on a Mac only). Any other failure (IPv6 off) says nothing.
#[must_use]
pub fn held_on_every_address(port: u16, protocol: Protocol) -> bool {
    use nix::sys::socket::{
        AddressFamily, SockFlag, SockType, SockaddrIn, SockaddrIn6, bind, setsockopt, socket,
        sockopt,
    };
    let kind = match protocol {
        Protocol::Tcp => SockType::Stream,
        Protocol::Udp => SockType::Datagram,
    };
    let refused = |family: AddressFamily| -> bool {
        // (macOS has no `SOCK_CLOEXEC`: closed before this returns.)
        let Ok(probe) = socket(family, kind, SockFlag::empty(), None) else {
            return false;
        };
        if setsockopt(&probe, sockopt::ReuseAddr, &true).is_err() {
            return false;
        }
        let bound = if family == AddressFamily::Inet6 {
            setsockopt(&probe, sockopt::Ipv6V6Only, &true).and_then(|()| {
                bind(
                    std::os::fd::AsRawFd::as_raw_fd(&probe),
                    &SockaddrIn6::from(std::net::SocketAddrV6::new(
                        Ipv6Addr::UNSPECIFIED,
                        port,
                        0,
                        0,
                    )),
                )
            })
        } else {
            bind(
                std::os::fd::AsRawFd::as_raw_fd(&probe),
                &SockaddrIn::from(std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
            )
        };
        bound == Err(nix::errno::Errno::EADDRINUSE)
    };
    refused(AddressFamily::Inet) || refused(AddressFamily::Inet6)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(ip: &str, port: u16) -> PublishedPort {
        PublishedPort {
            ip: ip.parse().unwrap(),
            port,
            protocol: Protocol::Tcp,
        }
    }

    fn udp(ip: &str, port: u16) -> PublishedPort {
        PublishedPort {
            protocol: Protocol::Udp,
            ..published(ip, port)
        }
    }

    fn tcp_at(address: &str) -> Place {
        (Protocol::Tcp, address.parse().unwrap())
    }

    /// Every address on the Mac is a loopback one: both for a port
    /// published everywhere (once, though the guest has it on 0.0.0.0 and
    /// ::), the one it names for loopback, the family's for a guest's own;
    /// each forwards to the port as the guest publishes it.
    #[test]
    fn ports_are_listened_on_the_macs_loopback_only() {
        let ports = [
            published("0.0.0.0", 8080),
            published("::", 8080),
            published("127.0.0.1", 5432),
            published("10.88.0.1", 9000),
            published("fd00::1", 9001),
            published("::1", 6379),
            published("0.0.0.0", 0),
            udp("0.0.0.0", 8080),
        ];
        // An older guest's `::1` isn't reached: left out.
        let older = wanted(&ports, false);
        assert!(!older.addresses.contains_key(&tcp_at("[::1]:6379")));
        let wanted = wanted(&ports, true);
        let expected: BTreeMap<Place, PublishedPort> = [
            (tcp_at("127.0.0.1:8080"), published("0.0.0.0", 8080)),
            (tcp_at("[::1]:8080"), published("0.0.0.0", 8080)),
            (tcp_at("127.0.0.1:5432"), published("127.0.0.1", 5432)),
            (tcp_at("127.0.0.1:9000"), published("10.88.0.1", 9000)),
            (tcp_at("[::1]:9001"), published("fd00::1", 9001)),
            // The guest's `::1`: dialled at the container, so forwarded.
            (tcp_at("[::1]:6379"), published("::1", 6379)),
            // UDP's 8080 is a port of its own beside TCP's.
            (
                (Protocol::Udp, "127.0.0.1:8080".parse().unwrap()),
                udp("0.0.0.0", 8080),
            ),
            (
                (Protocol::Udp, "[::1]:8080".parse().unwrap()),
                udp("0.0.0.0", 8080),
            ),
        ]
        .into_iter()
        .collect();
        assert_eq!(wanted.addresses, expected);
        assert_eq!(wanted.left_out, 0);
        // `::` and `0.0.0.0` on one port: the same addresses, not a clash.
        assert_eq!(wanted.clashes, []);
        assert!(
            wanted
                .addresses
                .keys()
                .all(|(_, address)| address.ip().is_loopback())
        );
    }

    /// Two containers on one port, of two guest addresses, want one Mac
    /// address: the first in order has it, the other is a clash.
    #[test]
    fn a_clash_keeps_the_first() {
        let wanted = wanted(
            &[published("10.88.0.3", 80), published("10.88.0.2", 80)],
            true,
        );
        assert_eq!(
            wanted.addresses[&tcp_at("127.0.0.1:80")],
            published("10.88.0.2", 80)
        );
        assert_eq!(wanted.clashes, [published("10.88.0.3", 80)]);
    }

    /// Past [`MAX_PORTS`] ports, the rest aren't listened on, and are
    /// counted: the lowest are kept, whatever their order.
    #[test]
    fn at_most_so_many_ports() {
        let ports: Vec<_> = (1..=300_u16)
            .rev()
            .map(|port| published("127.0.0.1", port))
            .collect();
        let wanted = wanted(&ports, true);
        assert_eq!(wanted.addresses.len(), MAX_PORTS);
        assert_eq!(wanted.left_out, 300 - MAX_PORTS);
        let highest = u16::try_from(MAX_PORTS).unwrap();
        let at = |port| {
            (
                Protocol::Tcp,
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            )
        };
        assert!(wanted.addresses.contains_key(&at(highest)));
        assert!(!wanted.addresses.contains_key(&at(highest + 1)));
    }

    /// A Mac program that holds a forwarded port on every address, with
    /// `SO_REUSEADDR`, is found, by IPv4 or IPv6 and for TCP and UDP; the
    /// VM's own loopback listener isn't taken for one (the control). BSD's
    /// bind rules: Linux refuses the probe beside the loopback listener (it
    /// asks both for `SO_REUSEADDR`), and the probe runs on a Mac only.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_program_on_every_address_is_found() {
        use std::net::{TcpListener, UdpSocket};
        // The VM's own: `127.0.0.1:P` alone, bound without `SO_REUSEADDR` as
        // its listeners are (std's would set it).
        let ours = {
            use nix::sys::socket::{
                AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, bind, listen, socket,
            };
            let socket = socket(
                AddressFamily::Inet,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            bind(
                std::os::fd::AsRawFd::as_raw_fd(&socket),
                &SockaddrIn::from(std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            )
            .unwrap();
            listen(&socket, Backlog::new(1).unwrap()).unwrap();
            TcpListener::from(socket)
        };
        let port = ours.local_addr().unwrap().port();
        assert!(!held_on_every_address(port, Protocol::Tcp), "the control");
        // std's listener sets `SO_REUSEADDR`, as most servers do.
        let theirs = TcpListener::bind(("0.0.0.0", port)).unwrap();
        assert!(held_on_every_address(port, Protocol::Tcp));
        let forwarded = PortsFile::new(
            &[(
                Protocol::Tcp,
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            )],
            &[],
            0,
            std::time::Duration::from_secs(300),
        );
        assert_eq!(shadowed(&forwarded), [(port, Protocol::Tcp)]);
        drop(theirs);
        assert!(!held_on_every_address(port, Protocol::Tcp));
        let udp_ours = UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_port = udp_ours.local_addr().unwrap().port();
        assert!(
            !held_on_every_address(udp_port, Protocol::Udp),
            "the control"
        );
        let udp_theirs = {
            use nix::sys::socket::{
                AddressFamily, SockFlag, SockType, SockaddrIn6, bind, setsockopt, socket, sockopt,
            };
            // On `::`, both families (a dual-stack server's way).
            let socket = socket(
                AddressFamily::Inet6,
                SockType::Datagram,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            setsockopt(&socket, sockopt::ReuseAddr, &true).unwrap();
            setsockopt(&socket, sockopt::Ipv6V6Only, &false).unwrap();
            bind(
                std::os::fd::AsRawFd::as_raw_fd(&socket),
                &SockaddrIn6::from(std::net::SocketAddrV6::new(
                    Ipv6Addr::UNSPECIFIED,
                    udp_port,
                    0,
                    0,
                )),
            )
            .unwrap();
            socket
        };
        assert!(held_on_every_address(udp_port, Protocol::Udp));
        drop(udp_theirs);
        let file = PortsFile::new(
            &[(
                Protocol::Tcp,
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            )],
            &[],
            0,
            std::time::Duration::from_secs(300),
        );
        assert!(shadowed(&file).is_empty());
        drop(ours);
        drop(udp_ours);
    }

    /// `ports.json` reads back as written; v1 as it was first written; a
    /// newer version is refused; none is `None`.
    #[test]
    fn the_ports_file_is_versioned() {
        let dir = std::env::temp_dir().join(format!("shipyard-ports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ports.json");
        assert_eq!(PortsFile::read(&path).unwrap(), None);
        let file = PortsFile::new(
            &[
                tcp_at("127.0.0.1:8080"),
                (Protocol::Udp, "127.0.0.1:53".parse().unwrap()),
            ],
            &[
                (tcp_at("[::1]:5432"), Unbound::InUse),
                (tcp_at("[::1]:6379"), Unbound::Other("off".to_owned())),
                ((Protocol::Udp, "[::1]:53".parse().unwrap()), Unbound::InUse),
            ],
            0,
            std::time::Duration::from_secs(300),
        );
        assert_eq!(file.forwarded, ["127.0.0.1:8080".parse().unwrap()]);
        assert_eq!(file.forwarded_udp, ["127.0.0.1:53".parse().unwrap()]);
        assert_eq!(file.unbound.len(), 2);
        assert_eq!(file.unbound_udp.len(), 1);
        file.write(&path).unwrap();
        assert_eq!(PortsFile::read(&path).unwrap(), Some(file));
        std::fs::write(
            &path,
            r#"{"version":1,"forwarded":["127.0.0.1:80"],"unbound":[["[::1]:80","in_use"]],"udp":0,"idle_seconds":300}"#,
        )
        .unwrap();
        let v1 = PortsFile::read(&path).unwrap().unwrap();
        assert_eq!((v1.forwarded.len(), &v1.unbound[0].1), (1, &Unbound::InUse));
        assert!(v1.forwarded_udp.is_empty());
        std::fs::write(
            &path,
            r#"{"version":3,"forwarded":[],"unbound":[],"udp":0,"idle_seconds":300}"#,
        )
        .unwrap();
        assert!(PortsFile::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
