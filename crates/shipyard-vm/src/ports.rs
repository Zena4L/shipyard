//! The guest's published ports on the Mac (step 10.3c1, D10.3-10): which
//! addresses the VM's host listens on for them ([`wanted`]), and the file
//! it says what it forwards in ([`PortsFile`], for `vm status` and
//! `doctor`). The listening itself is the host's.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use serde::{Deserialize, Serialize};
use shipyard_runtime::attach::forward::PublishedPort;

use crate::VmError;

/// The most ports the Mac listens on for the guest: containers that
/// publish more, all told, don't get the Mac's descriptors for them all.
pub const MAX_PORTS: usize = 256;

/// Where the Mac listens for the guest's published ports, each address
/// with the port in the guest its connections go to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    /// The Mac's addresses (its loopback's only, D10.3-10), each with what
    /// it forwards to.
    pub addresses: BTreeMap<SocketAddr, PublishedPort>,
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
/// hasn't), on `127.0.0.1`; one on another IPv6 address on `::1`. One on
/// the guest's `::1` isn't reached even in the guest (it isn't DNAT'd):
/// left out. Port 0 is left out, and past [`MAX_PORTS`] ports, the rest.
#[must_use]
pub fn wanted(ports: &[PublishedPort]) -> Wanted {
    let mut sorted = ports.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let numbers: BTreeSet<u16> = sorted
        .iter()
        .map(|published| published.port)
        .filter(|&port| port != 0)
        .collect();
    let kept: BTreeSet<u16> = numbers.iter().copied().take(MAX_PORTS).collect();
    let mut wanted = Wanted {
        left_out: numbers.len() - kept.len(),
        ..Wanted::default()
    };
    for published in sorted {
        if !kept.contains(&published.port) {
            continue;
        }
        let v4 = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), published.port);
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), published.port);
        let on: &[SocketAddr] = match published.ip {
            ip if ip.is_unspecified() => &[v4, v6],
            IpAddr::V6(ip) if ip.is_loopback() => &[],
            IpAddr::V4(_) => &[v4],
            IpAddr::V6(_) => &[v6],
        };
        let mut clashed = false;
        for &address in on {
            match wanted.addresses.entry(address) {
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

/// `ports.json`'s format, written in it from the first (H2).
pub const PORTS_FILE_VERSION: u32 = 1;

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
    /// The Mac's addresses listened on for the guest.
    pub forwarded: Vec<SocketAddr>,
    /// Those wanted that can't be listened on, each with why; tried again.
    pub unbound: Vec<(SocketAddr, Unbound)>,
    /// How many UDP ports the guest publishes: not forwarded.
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
    /// `forwarded`, `unbound`, `udp` and the idle limit, at this version.
    #[must_use]
    pub fn new(
        forwarded: Vec<SocketAddr>,
        unbound: Vec<(SocketAddr, Unbound)>,
        udp: u32,
        idle: std::time::Duration,
    ) -> Self {
        Self {
            version: PORTS_FILE_VERSION,
            forwarded,
            unbound,
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
        let bytes = serde_json::to_vec(self).map_err(|e| VmError::io(path, e.into()))?;
        let how =
            shipyard_sys::fs::Write::new(0o600).durability(shipyard_sys::fs::Durability::Atomic);
        shipyard_sys::fs::write_atomically(path, &bytes, &how)
            .map_err(|e| VmError::io(path, std::io::Error::other(e.to_string())))
    }

    /// Reads it from `path`: `None` if there's none.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`]: unreadable, not this format, or a newer version.
    pub fn read(path: &Path) -> Result<Option<Self>, VmError> {
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let bytes = match shipyard_sys::fs::read_bounded(path, PORTS_FILE_CAP) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(VmError::io(path, error)),
        };
        let unreadable = |e: serde_json::Error| VmError::io(path, e.into());
        let versioned: Versioned = serde_json::from_slice(&bytes).map_err(unreadable)?;
        if versioned.version > PORTS_FILE_VERSION {
            return Err(VmError::io(
                path,
                std::io::Error::other(format!(
                    "version {}, newer than this shipyard's {PORTS_FILE_VERSION}",
                    versioned.version
                )),
            ));
        }
        serde_json::from_slice(&bytes).map(Some).map_err(unreadable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(ip: &str, port: u16) -> PublishedPort {
        PublishedPort {
            ip: ip.parse().unwrap(),
            port,
        }
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
        ];
        let wanted = wanted(&ports);
        let expected: BTreeMap<SocketAddr, PublishedPort> = [
            ("127.0.0.1:8080", published("0.0.0.0", 8080)),
            ("[::1]:8080", published("0.0.0.0", 8080)),
            ("127.0.0.1:5432", published("127.0.0.1", 5432)),
            ("127.0.0.1:9000", published("10.88.0.1", 9000)),
            ("[::1]:9001", published("fd00::1", 9001)),
        ]
        .into_iter()
        .map(|(mac, guest)| (mac.parse().unwrap(), guest))
        .collect();
        assert_eq!(wanted.addresses, expected);
        assert_eq!(wanted.left_out, 0);
        // `::` and `0.0.0.0` on one port: the same addresses, not a clash.
        assert_eq!(wanted.clashes, []);
        assert!(
            wanted
                .addresses
                .keys()
                .all(|address| address.ip().is_loopback())
        );
    }

    /// Two containers on one port, of two guest addresses, want one Mac
    /// address: the first in order has it, the other is a clash.
    #[test]
    fn a_clash_keeps_the_first() {
        let wanted = wanted(&[published("10.88.0.3", 80), published("10.88.0.2", 80)]);
        let mac: SocketAddr = "127.0.0.1:80".parse().unwrap();
        assert_eq!(wanted.addresses[&mac], published("10.88.0.2", 80));
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
        let wanted = wanted(&ports);
        assert_eq!(wanted.addresses.len(), MAX_PORTS);
        assert_eq!(wanted.left_out, 300 - MAX_PORTS);
        let highest = u16::try_from(MAX_PORTS).unwrap();
        let at = |port| SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        assert!(wanted.addresses.contains_key(&at(highest)));
        assert!(!wanted.addresses.contains_key(&at(highest + 1)));
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
            vec!["127.0.0.1:8080".parse().unwrap()],
            vec![
                ("[::1]:5432".parse().unwrap(), Unbound::InUse),
                (
                    "[::1]:6379".parse().unwrap(),
                    Unbound::Other("off".to_owned()),
                ),
            ],
            1,
            std::time::Duration::from_secs(300),
        );
        file.write(&path).unwrap();
        assert_eq!(PortsFile::read(&path).unwrap(), Some(file));
        std::fs::write(
            &path,
            r#"{"version":1,"forwarded":["127.0.0.1:80"],"unbound":[["[::1]:80","in_use"]],"udp":0,"idle_seconds":300}"#,
        )
        .unwrap();
        let v1 = PortsFile::read(&path).unwrap().unwrap();
        assert_eq!((v1.forwarded.len(), &v1.unbound[0].1), (1, &Unbound::InUse));
        std::fs::write(
            &path,
            r#"{"version":2,"forwarded":[],"unbound":[],"udp":0,"idle_seconds":300}"#,
        )
        .unwrap();
        assert!(PortsFile::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
