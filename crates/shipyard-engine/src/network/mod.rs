//! Networks for `shipyard run` and `shipyard network`: the store of networks
//! (`/var/lib/shipyard/networks`), a run's place on one (its lease and veth
//! pair), and the `/etc` files each run gets.
//!
//! Everything here that changes the host's networking needs root: a bridge
//! and veth pairs are the host's devices. A rootless container gets a
//! network of its own through pasta instead (`shipyard_network::pasta`).
//!
//! A network's DNS server is this binary, re-executed: a binary that runs
//! containers on networks calls [`dns_entry_point`] first thing in `main`.

mod attach;
mod commands;
#[cfg(target_os = "linux")]
mod dns;
mod dns_server;
mod firewall;
mod live;
pub(crate) mod lost;
#[cfg(target_os = "linux")]
mod pasta;
mod run;
mod rundir;

use std::path::PathBuf;

use shipyard_network::files::{DnsSettings, HostEntry};

#[cfg(target_os = "linux")]
pub(crate) use attach::release_lease_hold;
#[cfg(target_os = "linux")]
pub(crate) use attach::resolver;
#[cfg(target_os = "linux")]
pub(crate) use commands::networks_in_use;
pub use commands::{
    Attached, NetworkInspected, Pruned, SpecOptions, create, create_with, inspect, prune,
    prune_where, prune_where_any, remove, spec,
};
pub use dns_server::entry_point as dns_entry_point;
pub(crate) use dns_server::{IDLE_ENV as DNS_IDLE_ENV, UPSTREAM_ENV as DNS_UPSTREAM_ENV};
#[cfg(target_os = "linux")]
pub(crate) use firewall::describe_conflict;
#[cfg(target_os = "linux")]
pub(crate) use live::{
    Joining, check_bridge, connect_live, container_addresses, container_leases, disconnect_live,
    next_interface, way_out,
};
pub use lost::{NetworkLost, pasta_lost};
#[cfg(target_os = "linux")]
pub(crate) use run::DetachedNetwork;
pub(crate) use run::RunNetwork;
pub(crate) use rundir::ETC_FILES;
pub use rundir::RunDir;

use crate::Engine;
use crate::error::{Context as _, Result, bail};

/// A run's ID: 12 lowercase hex characters, random. A container's hostname,
/// unless `--hostname` says otherwise (as Docker names them), and the name
/// of what the run makes (its lease, its veth pair, its directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunId(String);

impl RunId {
    /// A new one, from the kernel's random numbers.
    pub(crate) fn new() -> Result<RunId> {
        let bytes = crate::random::bytes::<6>().context("the run's ID")?;
        Ok(RunId(shipyard_sys::hex::lower(&bytes)))
    }

    /// The ID.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// A run's ID as a lease keeps it (`Lease::run`), if it's one: 12
    /// lowercase hex digits, as [`new`](Self::new) makes them (a run that
    /// joins another network later takes it from its leases: its veths'
    /// names come from it).
    #[must_use]
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "joining a network later is Linux's")
    )]
    pub(crate) fn from_lease(text: &str) -> Option<RunId> {
        (text.len() == 12 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| RunId(text.to_owned()))
    }

    /// The host's end of the run's veth pair on its `n`th network (0
    /// first): `shy` and 8 characters of the ID; on the others, 9 and `n`
    /// (an interface name is at most 15 bytes: up to `n` = 99; the lengths
    /// differ, so the first network's names are never the others').
    #[must_use]
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "veths are Linux's")
    )]
    pub(crate) fn host_veth(&self, n: usize) -> String {
        if n == 0 {
            format!(
                "{}{}",
                shipyard_network::store::DEVICE_PREFIX,
                self.0.get(..8).unwrap_or(&self.0)
            )
        } else {
            format!(
                "{}{}{n}",
                shipyard_network::store::DEVICE_PREFIX,
                self.0.get(..9).unwrap_or(&self.0)
            )
        }
    }
}

/// What network a container gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    /// The host's.
    Host,
    /// One of its own, with only a loopback interface.
    None,
    /// On bridge networks, in order (`bridge`: the default one): an
    /// interface on each.
    Bridges(Vec<Joined>),
    /// One of its own, connected through pasta (rootless's default).
    Pasta,
    /// Another container's (`--network container:NAME`, step 11.4b2b1):
    /// as given, then by the other's ID once a create has found it. Joined
    /// at each start, as it is: nothing of this container's is made in it.
    Container(String),
}

/// A bridge network a container joins, and its settings there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Joined {
    /// The network.
    pub name: String,
    /// Its address there (`--ip`, or the long form's `ip=`).
    pub ip: Option<std::net::IpAddr>,
    /// Its IPv6 address there (`--ip6`, or the long form's `ip6=`).
    pub ip6: Option<std::net::Ipv6Addr>,
    /// Its hardware address there.
    pub mac: Option<[u8; 6]>,
    /// Its other names there (`--network-alias`, and the long form's).
    pub aliases: Vec<String>,
}

impl NetworkMode {
    /// The bridge network `name` alone, with no settings of its own.
    #[must_use]
    pub fn bridge(name: &str) -> NetworkMode {
        NetworkMode::Bridges(vec![Joined {
            name: name.to_owned(),
            ..Joined::default()
        }])
    }

    /// Whether the container has a network namespace of its own (not the
    /// host's, nor another container's).
    #[must_use]
    pub fn own_namespace(&self) -> bool {
        !matches!(self, NetworkMode::Host | NetworkMode::Container(_))
    }

    /// The container whose network it joins, if it's another's.
    #[must_use]
    pub fn joined_container(&self) -> Option<&str> {
        match self {
            NetworkMode::Container(container) => Some(container),
            _ => None,
        }
    }

    /// Docker's `container:NAME` (the CLI's `--network`, the API's
    /// `NetworkMode`), if `text` is one: none if it isn't, a refusal if it
    /// names nothing.
    ///
    /// # Errors
    ///
    /// `container:` with no name after it.
    #[must_use]
    pub fn parse_container(text: &str) -> Option<Result<NetworkMode, &'static str>> {
        let name = text.strip_prefix("container:")?;
        Some(if name.is_empty() {
            Err("container: needs the container's name or ID")
        } else {
            Ok(NetworkMode::Container(name.to_owned()))
        })
    }
}

/// Another container's init, as a start joins its namespaces: by PID and
/// start time (a reused PID is refused), with its user namespace's maps
/// where that's joined too (this container isn't the host's root, and it
/// isn't this process's own).
#[derive(Debug, Clone)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "namespaces are joined on Linux")
)]
pub(crate) struct JoinedProcess {
    /// Its PID.
    pub(crate) pid: u32,
    /// Its start time.
    pub(crate) start_time: u64,
    /// Its user namespace's maps, read from it.
    pub(crate) maps: Option<shipyard_runtime::isolation::IdMaps>,
}

/// Another container's network, as a start joins it: its init (by PID and
/// start time), its user namespace's maps where this one must join that
/// too, and what its `/etc` files are made of (D11.4b2b-2).
#[derive(Debug, Clone)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are joined on Linux")
)]
pub(crate) struct JoinedNetwork {
    /// Its init, whose network namespace is joined.
    pub(crate) init: JoinedProcess,
    /// Its hostname.
    pub(crate) hostname: String,
    /// Its addresses on its first network the host routes.
    pub(crate) addresses: Vec<std::net::IpAddr>,
    /// Its DNS server.
    pub(crate) resolver: Option<std::net::IpAddr>,
    /// Its `--add-host`.
    pub(crate) hosts: Vec<HostEntry>,
    /// Its `--dns*`.
    pub(crate) dns: DnsSettings,
    /// Whether its network is a namespace of its own (not the host's).
    pub(crate) own: bool,
    /// Whether it's pasta's (its run files go where pasta's do).
    pub(crate) pasta: bool,
}

/// A run's network, as the flags ask for it: checked, but nothing made yet.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are set up on Linux")
)]
pub(crate) struct NetworkPlan {
    /// The network it gets.
    pub(crate) mode: NetworkMode,
    /// The run it's for.
    pub(crate) run: RunId,
    /// The container's hostname (`--hostname`, else the run's ID).
    pub(crate) hostname: String,
    /// Whether it gets the `/etc` files (it has a root filesystem of its
    /// own to bind them into).
    pub(crate) etc_files: bool,
    /// `--add-host`.
    pub(crate) hosts: Vec<HostEntry>,
    /// `--dns`, `--dns-search`, `--dns-option`.
    pub(crate) dns: DnsSettings,
    /// `-p`.
    pub(crate) ports: Vec<shipyard_network::ports::PortRequest>,
    /// `-P`: the image's `ExposedPorts` too, on free host ports.
    pub(crate) publish_all: bool,
    /// pasta and what it can do, for the `Pasta` mode (found on `PATH`, and
    /// asked, when the run was checked).
    pub(crate) pasta: Option<(PathBuf, shipyard_network::pasta::Features)>,
    /// The mode is the default, not one asked for.
    pub(crate) defaulted: bool,
    /// `--name`: the container's name on its networks.
    pub(crate) name: Option<String>,
    /// A detached container's ID, written into its leases.
    pub(crate) container: Option<crate::containers::ContainerId>,
    /// Another container's network it joins, found for this start.
    pub(crate) joined: Option<JoinedNetwork>,
}

impl NetworkPlan {
    /// Whether it's in a network namespace that isn't the host's (its own,
    /// or another container's that is), as its `/etc` files are made:
    /// another container's as that one's are.
    pub(crate) fn in_a_network_namespace(&self) -> bool {
        self.joined
            .as_ref()
            .map_or_else(|| self.mode.own_namespace(), |joined| joined.own)
    }
}

/// Whether this process is the host's root: user 0 in the initial user
/// namespace (every ID mapped to itself). Root in a user namespace of its
/// own (`unshare -U -r`) isn't: the host's devices, firewall and
/// `/var/lib` aren't its own.
#[cfg(target_os = "linux")]
#[must_use]
pub fn host_root() -> bool {
    nix::unistd::geteuid().is_root()
        && shipyard_runtime::process::current_id_maps().is_ok_and(|maps| {
            let whole = shipyard_runtime::isolation::IdMap {
                container: 0,
                host: 0,
                size: u32::MAX,
            };
            maps.uids == [whole]
        })
}

/// Off Linux: never (there are no networks).
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn host_root() -> bool {
    false
}

/// The network a container gets when none is asked for, as a run picks it
/// and a view says it (one decision for both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Defaulted {
    /// The default bridge network: root, where networks can be made.
    Bridge,
    /// The host's: off Linux, or a root that can't make a bridge.
    Host,
    /// One of its own through pasta: rootless.
    Pasta,
}

/// [`Defaulted`] here, now.
pub(crate) fn defaulted() -> Defaulted {
    if !cfg!(target_os = "linux") {
        Defaulted::Host
    } else if can_network() {
        Defaulted::Bridge
    } else if host_root() {
        Defaulted::Host
    } else {
        Defaulted::Pasta
    }
}

/// Whether the default bridge network can be had here: the host's root,
/// with the right to change the network (`CAP_NET_ADMIN`: root in a
/// container started without it has none) and the kernel's settings
/// writable (a container's `/proc/sys` is often read-only). Where it can't,
/// the default is the host's network, as before.
#[cfg(target_os = "linux")]
#[must_use]
pub fn can_network() -> bool {
    host_root() && why_no_network().is_none()
}

/// What keeps the host's root from the default bridge network, if
/// anything: no `CAP_NET_ADMIN`, or a read-only `/proc/sys`.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn why_no_network() -> Option<&'static str> {
    /// `CAP_NET_ADMIN`'s bit.
    const CAP_NET_ADMIN: u32 = 12;
    let net_admin = std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .and_then(|caps| u64::from_str_radix(caps.trim(), 16).ok())
            .is_some_and(|caps| caps & (1 << CAP_NET_ADMIN) != 0)
    });
    if !net_admin {
        return Some("shipyard has no CAP_NET_ADMIN");
    }
    let writable = nix::unistd::access(
        "/proc/sys/net/ipv4/ip_forward",
        nix::unistd::AccessFlags::W_OK,
    )
    .is_ok();
    (!writable).then_some("/proc/sys is read-only here")
}

/// Off Linux: never.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn can_network() -> bool {
    false
}

/// The interface of the host's default route for `family`: its way out,
/// if it has one (in the main routing table: a VPN routing by policy goes
/// unseen).
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn default_route_interface(family: shipyard_network::Family) -> Option<String> {
    let mut rtnetlink = shipyard_network::route::Rtnetlink::new().ok()?;
    let index = rtnetlink
        .routes(family)
        .ok()?
        .into_iter()
        .find(|route| route.destination.prefix() == 0)?
        .index?;
    rtnetlink
        .links()
        .ok()?
        .into_iter()
        .find(|link| link.index == index)
        .map(|link| link.name)
}

/// Whether bridged traffic meets the IP hooks here (`br_netfilter`, which
/// Docker loads): then a chain dropping what's forwarded keeps containers on
/// one bridge from each other too.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn bridged_traffic_filtered() -> bool {
    std::fs::read_to_string("/proc/sys/net/bridge/bridge-nf-call-iptables")
        .is_ok_and(|value| value.trim() == "1")
}

/// Why a bridge network can't be had without the host's root.
pub(crate) const NETWORKS_NEED_ROOT: &str = "bridge networks need root for now: a bridge and \
     its veth pairs are the host's devices (rootless containers get a network of their own \
     through pasta, the default, or --network pasta)";

/// The hint where the default network couldn't be had.
pub(crate) const HOST_INSTEAD: &str = "--network host runs it on the host's network instead";

/// The networks, for the host's root.
///
/// # Errors
///
/// [`ErrorKind::NeedsRoot`](crate::ErrorKind::NeedsRoot) if this isn't the
/// host's root, or the store can't be opened.
#[cfg(target_os = "linux")]
pub(crate) fn networks(engine: &Engine) -> Result<shipyard_network::store::Networks> {
    if !host_root() {
        bail!(NeedsRoot, "{NETWORKS_NEED_ROOT}");
    }
    Ok(shipyard_network::store::Networks::open(
        engine.paths().networks()?,
    )?)
}

/// Off Linux: no networks.
///
/// # Errors
///
/// Always [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
#[cfg(not(target_os = "linux"))]
pub(crate) fn networks(engine: &Engine) -> Result<shipyard_network::store::Networks> {
    let _ = engine;
    off_linux()
}

/// Networks are bridges and veth pairs: Linux's.
#[cfg(not(target_os = "linux"))]
pub(super) fn off_linux<T>() -> Result<T> {
    bail!(
        Unsupported,
        "networks need Linux bridges, which aren't available on {} yet",
        std::env::consts::OS
    )
}

/// The host's routes' destinations (IPv4 and IPv6, all but the default
/// routes): the addresses a new network mustn't take.
#[cfg(target_os = "linux")]
pub(super) fn host_routes() -> Result<Vec<shipyard_network::IpNet>> {
    use shipyard_network::Family;
    use shipyard_network::route::Rtnetlink;
    let mut rtnetlink = Rtnetlink::new()?;
    let mut routes = rtnetlink.routes(Family::V4)?;
    // A kernel without IPv6 has no IPv6 routes to list.
    routes.extend(rtnetlink.routes(Family::V6).unwrap_or_default());
    Ok(routes
        .into_iter()
        .map(|route| route.destination)
        .filter(|destination| destination.prefix() > 0)
        .collect())
}

/// The network `name`, made first if it's the default one (`bridge`) and
/// isn't there yet.
#[cfg(target_os = "linux")]
pub(super) fn open_network(
    engine: &crate::Engine,
    networks: &shipyard_network::store::Networks,
    name: &str,
) -> Result<shipyard_network::store::NetworkConfig> {
    use shipyard_network::store::{DEFAULT_NETWORK, NetworkSpec, StoreError};
    if let Some(config) = networks.get(name)? {
        return Ok(config);
    }
    if name != DEFAULT_NETWORK {
        bail!(
            NotFound,
            "there's no network named {name} (make it with `shipyard network create {name}`)"
        );
    }
    let spec = NetworkSpec {
        name: name.to_owned(),
        ..NetworkSpec::default()
    };
    let meta = shipyard_network::store::NetworkMeta {
        id: Some(crate::docker::new_id()?),
        ..shipyard_network::store::NetworkMeta::default()
    };
    match networks.create_with(&spec, &host_routes()?, &meta) {
        Ok(config) => {
            crate::events::emit_about(engine, crate::events::Kind::Network, "create", name);
            Ok(config)
        }
        // Made meanwhile, by another run.
        Err(StoreError::Exists(_)) => networks.get(name)?.ok_or_else(|| {
            crate::Error::new(
                crate::ErrorKind::Network,
                format!("the network {name} went as it was made"),
            )
        }),
        Err(error) => Err(error.into()),
    }
}

/// The default network (`bridge`), made if it isn't there yet, as a run
/// makes it: what Docker always lists.
///
/// # Errors
///
/// Not the host's root ([`ErrorKind::NeedsRoot`](crate::ErrorKind::NeedsRoot)),
/// or it can't be made.
#[cfg(target_os = "linux")]
pub(crate) fn ensure_default(engine: &crate::Engine) -> Result<()> {
    let networks = networks(engine)?;
    open_network(engine, &networks, shipyard_network::store::DEFAULT_NETWORK).map(|_| ())
}

/// Off Linux: no networks.
///
/// # Errors
///
/// Always [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
#[cfg(not(target_os = "linux"))]
pub(crate) fn ensure_default(engine: &crate::Engine) -> Result<()> {
    let _ = engine;
    off_linux()
}

/// `--network`'s modes: names no network can have.
pub const RESERVED: [&str; 3] = ["host", "none", "pasta"];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn a_run_id_reads_back_from_a_lease() {
        let made = RunId::new().unwrap();
        assert_eq!(RunId::from_lease(made.as_str()), Some(made));
        for not in [
            "",
            "a1b2c3",
            "A1B2C3D4E5F6",
            "a1b2c3d4e5f6a",
            "a1b2c3d4e5fg",
        ] {
            assert_eq!(RunId::from_lease(not), None, "{not}");
        }
    }

    #[test]
    fn host_veth_names_differ_by_network_and_fit() {
        let run = RunId("a1b2c3d4e5f6".into());
        assert_eq!(run.host_veth(0), "shya1b2c3d4");
        assert_eq!(run.host_veth(1), "shya1b2c3d4e1");
        assert_eq!(run.host_veth(99), "shya1b2c3d4e99");
        assert!(run.host_veth(MAX_INTERFACE).len() <= 15);
    }

    /// The highest network index a run has (see `run::MAX_NETWORKS`).
    const MAX_INTERFACE: usize = 99;
}
