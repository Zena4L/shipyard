//! Networks for `shipyard run` and `shipyard network`: the store of networks
//! (`/var/lib/shipyard/networks`), a run's place on one (its lease and veth
//! pair), and the `/etc` files each run gets.
//!
//! Everything here that changes the host's networking needs root: a bridge
//! and veth pairs are the host's devices. A rootless container gets a
//! network of its own through pasta instead (`shipyard_network::pasta`).

use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use shipyard_network::files::{DnsSettings, HostEntry};

/// A run's ID: 12 lowercase hex characters, random. A container's hostname,
/// unless `--hostname` says otherwise (as Docker names them), and the name
/// of what the run makes (its lease, its veth pair, its directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunId(String);

impl RunId {
    /// A new one, from the kernel's random numbers.
    pub(crate) fn new() -> Result<RunId> {
        let mut bytes = [0u8; 6];
        File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut bytes))
            .context("reading /dev/urandom for the run's ID")?;
        Ok(RunId(bytes.iter().fold(String::new(), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })))
    }

    /// The ID.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The host's end of the run's veth pair on its `n`th network (0
    /// first): `shy` and 8 characters of the ID; on the others, 9 and `n`
    /// (an interface name is at most 15 bytes: up to `n` = 99; the lengths
    /// differ, so the first network's names are never the others').
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "networks are Linux's")
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
pub(crate) enum NetworkMode {
    /// The host's.
    Host,
    /// One of its own, with only a loopback interface.
    None,
    /// On bridge networks, in order (`bridge`: the default one): an
    /// interface on each.
    Bridges(Vec<Joined>),
    /// One of its own, connected through pasta (rootless's default).
    Pasta,
}

/// A bridge network a container joins, and its settings there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Joined {
    /// The network.
    pub(crate) name: String,
    /// Its address there (`--ip`, or the long form's `ip=`).
    pub(crate) ip: Option<std::net::IpAddr>,
    /// Its IPv6 address there (`--ip6`, or the long form's `ip6=`).
    pub(crate) ip6: Option<std::net::Ipv6Addr>,
    /// Its hardware address there.
    pub(crate) mac: Option<[u8; 6]>,
    /// Its other names there (`--network-alias`, and the long form's).
    pub(crate) aliases: Vec<String>,
}

impl NetworkMode {
    /// The bridge network `name` alone, with no settings of its own.
    pub(crate) fn bridge(name: &str) -> NetworkMode {
        NetworkMode::Bridges(vec![Joined {
            name: name.to_owned(),
            ..Joined::default()
        }])
    }

    /// Whether the container has a network namespace of its own.
    pub(crate) fn own_namespace(&self) -> bool {
        !matches!(self, NetworkMode::Host)
    }
}

/// A run's network, as the flags ask for it: checked, but nothing made yet.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
pub(crate) struct NetworkPlan {
    pub(crate) mode: NetworkMode,
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
}

/// The directory networks are kept in: `SHIPYARD_NETWORK_DIR` if set (for
/// tests), else `/var/lib/shipyard/networks`.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn networks_dir() -> PathBuf {
    std::env::var_os("SHIPYARD_NETWORK_DIR")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || PathBuf::from("/var/lib/shipyard/networks"),
            PathBuf::from,
        )
}

/// Whether this process is the host's root: user 0 in the initial user
/// namespace (every ID mapped to itself). Root in a user namespace of its
/// own (`unshare -U -r`) isn't: the host's devices, firewall and
/// `/var/lib` aren't its own.
#[cfg(target_os = "linux")]
pub(crate) fn host_root() -> bool {
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
pub(crate) fn host_root() -> bool {
    false
}

/// Whether the default bridge network can be had here: the host's root,
/// with the right to change the network (`CAP_NET_ADMIN`: root in a
/// container started without it has none) and the kernel's settings
/// writable (a container's `/proc/sys` is often read-only). Where it can't,
/// the default is the host's network, as before.
#[cfg(target_os = "linux")]
pub(crate) fn can_network() -> bool {
    host_root() && why_no_network().is_none()
}

/// What keeps the host's root from the default bridge network, if
/// anything: no `CAP_NET_ADMIN`, or a read-only `/proc/sys`.
#[cfg(target_os = "linux")]
pub(crate) fn why_no_network() -> Option<&'static str> {
    /// `CAP_NET_ADMIN`'s bit.
    const CAP_NET_ADMIN: u32 = 12;
    let net_admin = fs::read_to_string("/proc/self/status").is_ok_and(|status| {
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
pub(crate) fn can_network() -> bool {
    false
}

/// Why a bridge network can't be had without the host's root.
pub(crate) const NETWORKS_NEED_ROOT: &str = "bridge networks need root for now: a bridge and \
     its veth pairs are the host's devices (rootless containers get a network of their own \
     through pasta, the default, or --network pasta)";

/// The hint where the default network couldn't be had.
pub(crate) const HOST_INSTEAD: &str = "--network host runs it on the host's network instead";

/// The networks, for the host's root.
#[cfg(target_os = "linux")]
pub(crate) fn networks() -> Result<shipyard_network::store::Networks> {
    if !host_root() {
        bail!("{NETWORKS_NEED_ROOT}");
    }
    Ok(shipyard_network::store::Networks::open(&networks_dir())?)
}

/// Off Linux: no networks.
#[cfg(not(target_os = "linux"))]
pub(crate) fn networks() -> Result<shipyard_network::store::Networks> {
    off_linux()
}

/// Networks are bridges and veth pairs: Linux's.
#[cfg(not(target_os = "linux"))]
fn off_linux<T>() -> Result<T> {
    bail!(
        "networks need Linux bridges, which aren't available on {} yet",
        std::env::consts::OS
    )
}

/// The host's routes' destinations (IPv4 and IPv6, all but the default
/// routes): the addresses a new network mustn't take.
#[cfg(target_os = "linux")]
fn host_routes() -> Result<Vec<shipyard_network::IpNet>> {
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
fn open_network(
    networks: &shipyard_network::store::Networks,
    name: &str,
) -> Result<shipyard_network::store::NetworkConfig> {
    use shipyard_network::store::{DEFAULT_NETWORK, NetworkSpec, StoreError};
    if let Some(config) = networks.get(name)? {
        return Ok(config);
    }
    if name != DEFAULT_NETWORK {
        bail!("there's no network named {name} (make it with `shipyard network create {name}`)");
    }
    let spec = NetworkSpec {
        name: name.to_owned(),
        ..NetworkSpec::default()
    };
    match networks.create(&spec, &host_routes()?) {
        Ok(config) => Ok(config),
        // Made meanwhile, by another run.
        Err(StoreError::Exists(_)) => networks
            .get(name)?
            .ok_or_else(|| anyhow::anyhow!("the network {name} went as it was made")),
        Err(error) => Err(error.into()),
    }
}

/// Makes Shipyard's firewall table current, then puts `config`'s network
/// in its sets. A missing table (after a reboot, or `nft flush ruleset`) or
/// an older one is made whole again, from every network and every live
/// run's published ports, under the store's lock, so two runs never build
/// it at once.
#[cfg(target_os = "linux")]
fn ensure_firewall(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    config: &shipyard_network::store::NetworkConfig,
) -> Result<()> {
    use shipyard_network::nft::{NetworkRules, Ruleset};
    if nft.ruleset()? != Ruleset::Current {
        let _all = networks.lock_all()?;
        if nft.ruleset()? != Ruleset::Current {
            let rules: Vec<NetworkRules> =
                networks.list()?.iter().map(NetworkRules::from).collect();
            let ports = networks.live_port_rules()?;
            nft.install(&rules, &ports, &networks.routing_closed())?;
        }
    }
    nft.add_network(&NetworkRules::from(config))?;
    Ok(())
}

/// Makes Shipyard's bridge table current (the module `nft::spoofing`:
/// each container sends only as itself), as [`ensure_firewall`] does the
/// other: whether it's there. A kernel without bridge filtering refuses
/// it; then containers can still send as one another, as before it, and
/// the run goes on, warned.
#[cfg(target_os = "linux")]
fn ensure_bridge_table(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
) -> bool {
    use shipyard_network::nft::Ruleset;
    let unguarded = |why: &str| {
        crate::warning(&format!(
            "containers on a network can send as one another here: {why}"
        ));
        false
    };
    match nft.bridge_ruleset() {
        Ok(Ruleset::Current) => return true,
        Ok(_) => {}
        Err(error) => return unguarded(&format!("reading shipyard's bridge table: {error}")),
    }
    let _all = match networks.lock_all() {
        Ok(lock) => lock,
        Err(error) => return unguarded(&error.to_string()),
    };
    match nft.bridge_ruleset() {
        Ok(Ruleset::Current) => return true,
        Ok(_) => {}
        Err(error) => return unguarded(&format!("reading shipyard's bridge table: {error}")),
    }
    if let Err(error) = nft.install_bridge_table(&[]) {
        // Only the kernel's refusal of the table itself says it has none.
        let no_filtering = matches!(
            error.errno(),
            Some(nix::errno::Errno::EAFNOSUPPORT | nix::errno::Errno::EOPNOTSUPP)
        );
        return unguarded(&if no_filtering {
            format!("the kernel has no bridge filtering ({error})")
        } else {
            format!("making shipyard's bridge table: {error}")
        });
    }
    // Every live container's port, read only now that the table is there: a
    // run guarding its port meanwhile is among them, or guards it after.
    match networks.live_port_guards() {
        Ok(guards) => {
            for guard in guards {
                if let Err(error) = nft.guard(&guard) {
                    crate::warning(&format!(
                        "the container on the port {} can send as another: {error}",
                        guard.port
                    ));
                }
            }
        }
        Err(error) => {
            crate::warning(&format!(
                "running containers can send as one another until they're run again: {error}"
            ));
        }
    }
    true
}

/// Why `-p` can't go on `config`'s network: an internal one, or an IPv6
/// host address on a network without IPv6.
#[cfg(target_os = "linux")]
fn check_ports(
    config: &shipyard_network::store::NetworkConfig,
    ports: &[shipyard_network::ports::PortRequest],
) -> Result<()> {
    let name = &config.name;
    if config.internal && !ports.is_empty() {
        bail!(
            "--publish/--publish-all: the network {name} is internal: nothing on it can be \
             reached from outside"
        );
    }
    if config.subnet6.is_none()
        && let Some(ip) = ports
            .iter()
            .find_map(|port| port.host_ip.filter(std::net::IpAddr::is_ipv6))
    {
        bail!(
            "--publish: {ip} is an IPv6 address, and the network {name} has no IPv6 (make one \
             with `shipyard network create --ipv6`)"
        );
    }
    Ok(())
}

/// Turns forwarding on for each of `config`'s families where the host
/// doesn't route: the containers' way out. The host still won't route
/// between its own interfaces (those it has now) for anyone: routing is
/// closed first, so that it's never an open router, even if the run goes no
/// further.
#[cfg(target_os = "linux")]
fn open_routing(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    config: &shipyard_network::store::NetworkConfig,
) -> Result<()> {
    use shipyard_network::{Family, attach};
    let families: &[Family] = if config.subnet6.is_some() {
        &[Family::V4, Family::V6]
    } else {
        &[Family::V4]
    };
    for &family in families {
        if attach::forwarding(family)? != Some(false) {
            continue;
        }
        let interfaces = attach::host_interfaces()?;
        networks.close_routing(family, &interfaces)?;
        nft.close_routing(family, &interfaces)?;
        let kept = attach::turn_forwarding_on(family)?;
        if family == Family::V4 {
            // That turned it on for every interface: internal networks'
            // bridges don't forward, whatever the rules.
            for internal in networks.list()?.iter().filter(|other| other.internal) {
                match attach::stop_forwarding(&internal.bridge) {
                    // Not made since the last reboot: done when it is.
                    Err(attach::AttachError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound => {}
                    other => other?,
                }
            }
        }
        tracing::info!("turned on {family} forwarding for containers' way out, and only for them");
        if !kept.is_empty() {
            tracing::info!(
                "kept taking router advertisements while forwarding (accept_ra=2) on: {}",
                kept.join(", ")
            );
        }
    }
    Ok(())
}

/// Sweeps `network`'s leases of runs that are gone, and what they left:
/// their published ports, their veth pair (the kernel deletes one with its
/// namespace, but not at once), and the connections the kernel remembers for
/// their address. What can't be cleaned up is a warning: the address is
/// free all the same.
#[cfg(target_os = "linux")]
fn sweep_gone(
    network: &shipyard_network::store::LockedNetwork,
    nft: &mut shipyard_network::nft::Nftables,
) -> Result<()> {
    for gone in network.sweep()? {
        release_lease(
            network.config(),
            &gone,
            nft,
            true,
            "a container that's gone",
        );
    }
    Ok(())
}

/// Publishes `ports`. The ports maps are every network's: a key can be
/// taken by a run that's gone (swept on another network, or a firewall
/// rebuilt from its lease as it was released); then it's reclaimed. A key
/// taken by a live run is an error (it can't be: the host port is held).
#[cfg(target_os = "linux")]
fn publish(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    ports: &[shipyard_network::nft::PortRule],
) -> Result<()> {
    match nft.publish(ports) {
        Err(error) if error.errno() == Some(nix::errno::Errno::EEXIST) => {}
        other => return Ok(other?),
    }
    let live = networks.live_port_rules()?;
    let mut stale = Vec::new();
    let mut todo = Vec::new();
    for (port, now) in ports.iter().zip(nft.published(ports)?) {
        match now {
            // Published already (the rest of the request failed).
            Some(now) if now == *port => {}
            Some(now) if live.contains(&now) => {
                bail!(
                    "port {} is published already, by another container",
                    port.published
                );
            }
            Some(now) => {
                stale.push(now);
                todo.push(*port);
            }
            None => todo.push(*port),
        }
    }
    nft.unpublish(&stale)?;
    Ok(nft.publish(&todo)?)
}

/// Undoes what a run's `lease` on `config`'s network made, in order: its
/// published ports (so no new connection reaches it), its veth pair (if
/// `connected`, and only as a port of the network's bridge: a device of its
/// name elsewhere is another run's), and the connections the kernel
/// remembers for its address. `whose` names the run in warnings.
#[cfg(target_os = "linux")]
fn release_lease(
    config: &shipyard_network::store::NetworkConfig,
    lease: &shipyard_network::store::Lease,
    nft: &mut shipyard_network::nft::Nftables,
    connected: bool,
    whose: &str,
) {
    let ports = lease.port_rules();
    if !ports.is_empty()
        && let Err(error) = nft.unpublish(&ports)
    {
        crate::warning(&format!("unpublishing the ports of {whose}: {error}"));
    }
    // Only where the bridge table is (a kernel without bridge filtering
    // has none).
    if matches!(
        nft.bridge_ruleset(),
        Ok(shipyard_network::nft::Ruleset::Current | shipyard_network::nft::Ruleset::Outdated)
    ) && let Err(error) = nft.unguard(&lease.port_guard())
    {
        crate::warning(&format!("forgetting the port of {whose}: {error}"));
    }
    if connected && let Err(error) = shipyard_network::attach::disconnect(config, lease) {
        crate::warning(&format!(
            "removing the veth {} of {whose}: {error}",
            lease.host_veth
        ));
    }
    for ip in lease.addresses() {
        if let Err(error) = shipyard_network::conntrack::flush_address(ip) {
            crate::warning(&format!("forgetting the connections of {whose}: {error}"));
        }
    }
}

/// Warns about other programs' firewall chains that would drop the
/// containers' traffic on `config`'s network: what the host forwards, for a
/// network with a way out ([`other_firewalls`]); what comes in to the host,
/// for one with a DNS server (`dns`: [`closed_dns`]).
#[cfg(target_os = "linux")]
fn warn_about_other_firewalls(
    nft: &mut shipyard_network::nft::Nftables,
    config: &shipyard_network::store::NetworkConfig,
    dns: bool,
) {
    if config.internal && !dns {
        return;
    }
    if let Ok(chains) = nft.chains() {
        let mut warnings = Vec::new();
        if !config.internal {
            let uplink = default_route_interface();
            warnings.extend(other_firewalls(&chains, &config.bridge, uplink.as_deref()));
        }
        if dns {
            warnings.extend(closed_dns(&chains, config));
        }
        for warning in warnings {
            crate::warning(&warning);
        }
    }
}

/// The interface of the host's IPv4 default route: its way out, if it has
/// one.
#[cfg(target_os = "linux")]
fn default_route_interface() -> Option<String> {
    let mut rtnetlink = shipyard_network::route::Rtnetlink::new().ok()?;
    let index = rtnetlink
        .routes(shipyard_network::Family::V4)
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

/// A warning for each of other programs' chains in `chains` that drop what
/// comes in to the host by default (ufw's `INPUT`, firewalld's): they'd
/// drop the queries `config`'s containers send its DNS server, on the
/// gateway. Each names the chain and gives the fix.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn closed_dns(
    chains: &[shipyard_network::nft::Chain],
    config: &shipyard_network::store::NetworkConfig,
) -> Vec<String> {
    let bridge = &config.bridge;
    chains
        .iter()
        // The server is on IPv4: an IPv6 chain (ufw makes one of each)
        // drops nothing it gets.
        .filter(|chain| {
            chain.drops_input()
                && !chain.is_shipyards()
                && matches!(chain.family_name(), "ip" | "inet")
        })
        .map(|chain| {
            let at = format!("{} {} {}", chain.family_name(), chain.table, chain.name);
            let fix = if chain.family_name() == "ip" && chain.table == "filter" {
                format!(
                    "iptables -I {} -i {bridge} -p udp --dport 53 -j ACCEPT; iptables -I {} -i \
                     {bridge} -p tcp --dport 53 -j ACCEPT",
                    chain.name, chain.name
                )
            } else {
                format!("nft insert rule {at} iifname {bridge} meta l4proto {{ udp, tcp }} th dport 53 accept")
            };
            format!(
                "the firewall's chain {at} drops what comes in to the host by default, so \
                 containers on {bridge} may get no answer from the network's DNS server (on \
                 {}:53); to let them through: {fix}",
                config.gateway
            )
        })
        .collect()
}

/// A warning for each of other programs' chains in `chains` that drop what
/// the host forwards by default (Docker's or ufw's `FORWARD`, firewalld's):
/// they'd drop the containers' traffic on `bridge` too, after Shipyard's
/// rules let it through. Each names the chain and gives the fix: Docker's
/// own way (its `DOCKER-USER` chain) where it's Docker's table.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn other_firewalls(
    chains: &[shipyard_network::nft::Chain],
    bridge: &str,
    uplink: Option<&str>,
) -> Vec<String> {
    // Out to the world only (the host's default route's interface), not into
    // another program's bridges (Docker's), whose containers its own rules
    // keep apart.
    let out = uplink.map_or_else(String::new, |uplink| format!(" -o {uplink}"));
    let nft_out = uplink.map_or_else(String::new, |uplink| format!(" oifname {uplink}"));
    chains
        .iter()
        .filter(|chain| chain.drops_forwarded() && !chain.is_shipyards())
        .map(|chain| {
            let at = format!("{} {} {}", chain.family_name(), chain.table, chain.name);
            let docker = chains.iter().any(|other| {
                other.family == chain.family
                    && other.table == chain.table
                    && other.name == "DOCKER-USER"
            });
            let fix = if docker {
                format!(
                    "iptables -I DOCKER-USER -i {bridge}{out} -j ACCEPT; iptables -I DOCKER-USER \
                     -o {bridge} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT"
                )
            } else {
                format!(
                    "nft insert rule {at} iifname {bridge}{nft_out} accept; nft insert rule {at} \
                     oifname {bridge} ct state related,established accept"
                )
            };
            format!(
                "the firewall's chain {at} drops forwarded traffic by default, so containers on \
                 {bridge} may have no way out; to let them through: {fix}"
            )
        })
        .collect()
}

/// A run's place on a network: its lease (its address, held while the run
/// lives), the host ports it publishes (held too), and once it's connected,
/// its veth pair. Released when the run ends (on drop).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct Attachment {
    config: shipyard_network::store::NetworkConfig,
    lease: Option<shipyard_network::store::HeldLease>,
    /// Which of the container's default routes go through it.
    routes: shipyard_network::attach::DefaultRoutes,
    /// Whether its veth pair was made: only then is the device of its name
    /// this run's to delete (another run's could have the same name).
    connected: std::cell::Cell<bool>,
    /// The host ports it publishes, held: no other program can take them.
    _held: Vec<shipyard_network::ports::HeldPort>,
}

#[cfg(target_os = "linux")]
impl Attachment {
    /// Takes an address on `joined`, the `n`th of `plan`'s networks (0
    /// first: its interface is `eth{n}`), publishing `ports` (the network
    /// with the IPv4 default route has them all; `routes` says which go
    /// through this one): the host's ports are held first; then, under the
    /// network's lock (its alone: a run never holds two networks' locks,
    /// so two can't wait for each other), the leases of runs that are
    /// gone are swept (with what they left), the network is made ready
    /// (its bridge, the firewall), the address and ports are taken, and
    /// the network's DNS server is started if it has one and it isn't
    /// running (a server that can't start fails the run).
    fn take(
        plan: &NetworkPlan,
        joined: &Joined,
        n: usize,
        routes: shipyard_network::attach::DefaultRoutes,
        ports: &[shipyard_network::ports::PortRequest],
    ) -> Result<Attachment> {
        use shipyard_network::store::LeaseRequest;
        use shipyard_network::{attach, nft::Nftables};
        let name = &joined.name;
        let networks = networks()?;
        let config = open_network(&networks, name)?;
        check_ports(&config, ports)?;
        let held = shipyard_network::ports::hold(ports)?;
        let network = networks.lock(name)?;
        let mut nft = Nftables::new()?;
        sweep_gone(&network, &mut nft)?;
        let config = network.config().clone();
        // Again: it may have been made again since it was looked at.
        check_ports(&config, ports)?;
        attach::ensure_bridge(&config).with_context(|| format!("the network {name}"))?;
        ensure_firewall(&networks, &mut nft, &config)
            .with_context(|| format!("the network {name}'s firewall rules"))?;
        let guarded = ensure_bridge_table(&networks, &mut nft);
        if !config.internal {
            open_routing(&networks, &mut nft, &config)?;
        }
        let lease = network.allocate(&LeaseRequest {
            run: plan.run.as_str().to_owned(),
            hostname: plan.hostname.clone(),
            host_veth: plan.run.host_veth(n),
            ip: joined.ip,
            ip6: joined.ip6,
            mac: joined.mac,
            ports: held.iter().map(|port| port.published).collect(),
            name: plan.name.clone(),
            aliases: joined.aliases.clone(),
            interface: shipyard_network::store::interface_name(n),
        })?;
        // From here, dropping it undoes it all.
        let attachment = Attachment {
            config,
            lease: Some(lease),
            routes,
            connected: std::cell::Cell::new(false),
            _held: held,
        };
        if let Some(lease) = &attachment.lease {
            if !lease.lease().ports.is_empty() {
                attach::allow_local_clients(&attachment.config)?;
            }
            publish(&networks, &mut nft, &lease.lease().port_rules())?;
            announce(ports, &lease.lease().ports);
            // Before its veth exists: from its first frame, it sends only as
            // itself.
            if guarded && let Err(error) = nft.guard(&lease.lease().port_guard()) {
                crate::warning(&format!(
                    "the container can send as another on the network {name}: {error}"
                ));
            }
        }
        let has_dns = attachment.config.has_dns();
        if has_dns {
            // Under the network's lock, with the lease held: see `ensure`.
            crate::dns_helper::ensure(&networks, &attachment.config)?;
        }
        warn_about_other_firewalls(&mut nft, &attachment.config, has_dns);
        drop(network);
        Ok(attachment)
    }

    /// The container's addresses: its IPv4 one, and its IPv6 one if the
    /// network has IPv6.
    pub(crate) fn addresses(&self) -> Vec<std::net::IpAddr> {
        self.lease
            .as_ref()
            .map_or_else(Vec::new, |lease| lease.lease().addresses().collect())
    }

    /// Connects the container whose network namespace is `netns`: the
    /// runtime's network hook.
    fn connect(&self, netns: std::os::fd::BorrowedFd<'_>) -> Result<(), String> {
        let Some(lease) = &self.lease else {
            return Err("the run's lease was given back already".to_owned());
        };
        shipyard_network::attach::connect(&self.config, lease.lease(), netns, self.routes)
            .map_err(|error| {
                format!(
                    "connecting the container to the network {}: {error}",
                    self.config.name
                )
            })?;
        self.connected.set(true);
        Ok(())
    }
}

/// Takes `plan`'s bridge networks, `joined`, in order (see
/// [`Attachment::take`]). The container's default route, and the ports it
/// publishes (`ports`), go through [`route_network`]. If one can't be
/// taken, those taken already are given back.
#[cfg(target_os = "linux")]
fn take_all(
    plan: &NetworkPlan,
    joined: &[Joined],
    ports: &[shipyard_network::ports::PortRequest],
) -> Result<Vec<Attachment>> {
    let store = networks()?;
    let configs: Vec<Option<shipyard_network::store::NetworkConfig>> = joined
        .iter()
        .map(|network| store.get(&network.name).ok().flatten())
        .collect();
    let internal: Vec<bool> = configs
        .iter()
        .map(|config| config.as_ref().is_some_and(|config| config.internal))
        .collect();
    let ipv6: Vec<bool> = configs
        .iter()
        .map(|config| {
            config
                .as_ref()
                .is_some_and(|config| config.subnet6.is_some())
        })
        .collect();
    let route = route_network(&internal);
    let route6 = route_network6(&internal, &ipv6);
    joined
        .iter()
        .enumerate()
        .map(|(n, network)| {
            let routes = shipyard_network::attach::DefaultRoutes {
                ipv4: n == route,
                ipv6: route6 == Some(n),
            };
            let ports = if routes.ipv4 { ports } else { &[] };
            Attachment::take(plan, network, n, routes, ports)
        })
        .collect()
}

/// Which of a container's networks (whether each is `internal`, in order)
/// its default route goes through: the first that isn't internal (an
/// internal network leads nowhere), else the first.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn route_network(internal: &[bool]) -> usize {
    internal.iter().position(|internal| !internal).unwrap_or(0)
}

/// Which network its IPv6 default route goes through, as
/// [`route_network`] chooses, among those with IPv6 (`ipv6`): none if
/// none has.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn route_network6(internal: &[bool], ipv6: &[bool]) -> Option<usize> {
    let with_ipv6 = || (0..ipv6.len()).filter(|&n| ipv6[n]);
    with_ipv6()
        .find(|&n| !internal.get(n).copied().unwrap_or(false))
        .or_else(|| with_ipv6().next())
}

/// The resolver for a container on `networks` (in order): the DNS server
/// of the first that has one and a way out, else of the first that has one
/// (as Docker's, the default network has none; a server forwards for a
/// container with a way out elsewhere); `None` for the host's resolvers.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn resolver(networks: &[&shipyard_network::store::NetworkConfig]) -> Option<std::net::IpAddr> {
    let with_dns = || networks.iter().filter(|config| config.has_dns());
    with_dns()
        .find(|config| !config.internal)
        .or_else(|| with_dns().next())
        .map(|config| config.gateway)
}

/// When the run ends: what its lease made is undone ([`release_lease`]),
/// then its address is given back, then its host ports.
#[cfg(target_os = "linux")]
impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            match shipyard_network::nft::Nftables::new() {
                Ok(mut nft) => {
                    release_lease(
                        &self.config,
                        lease.lease(),
                        &mut nft,
                        self.connected.get(),
                        "the container",
                    );
                }
                Err(error) => {
                    crate::warning(&format!("cleaning up the container's network: {error}"));
                }
            }
            lease.release();
        }
    }
}

/// Prints the ports of `published` whose host port the kernel chose (in
/// `requests`, those without one): the user has no other way to know them.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn announce(
    requests: &[shipyard_network::ports::PortRequest],
    published: &[shipyard_network::ports::Published],
) {
    for (request, published) in requests.iter().zip(published) {
        if request.host_port.is_none() {
            crate::notice(&format!("published {published}"));
        }
    }
}

/// The ports a run publishes: `-p`'s (`ports`), then with `-P`
/// (`publish_all`), the image's `exposed` ports that `-p` doesn't already
/// publish, on free host ports, as Docker's. One that can't be read is
/// skipped, with a warning.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn port_requests(
    ports: &[shipyard_network::ports::PortRequest],
    publish_all: bool,
    exposed: &[String],
) -> Vec<shipyard_network::ports::PortRequest> {
    let mut requests = ports.to_vec();
    if publish_all {
        for key in exposed {
            match shipyard_network::ports::parse_exposed(key) {
                Some(request)
                    if !ports.iter().any(|given| {
                        given.container_port == request.container_port
                            && given.protocol == request.protocol
                    }) =>
                {
                    requests.push(request);
                }
                Some(_) => {}
                None => crate::warning(&format!(
                    "--publish-all: the image's exposed port {key:?} can't be read"
                )),
            }
        }
    }
    requests
}

/// What a pasta run starts once its container exists.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PastaPlan {
    /// pasta itself, and what it can do.
    program: PathBuf,
    features: shipyard_network::pasta::Features,
    /// The host ports it publishes, held (free ones chosen, and taken ones
    /// refused, before anything was made) until just before the container
    /// is started ([`RunNetwork::release_ports`]): not until pasta binds
    /// them, since the container's first process, forked before pasta
    /// starts, holds its own copies of them until it execs.
    held: std::cell::RefCell<Vec<shipyard_network::ports::HeldPort>>,
    /// What it publishes.
    ports: Vec<shipyard_network::ports::Published>,
    /// The ports as asked (to tell the kernel-chosen ones).
    requests: Vec<shipyard_network::ports::PortRequest>,
    /// Where its files go (the run's directory).
    dir: PathBuf,
    /// The network wasn't asked for: a failure says how to do without.
    defaulted: bool,
    /// pasta, once started (by the hook): killed when the run ends.
    running: std::cell::RefCell<Option<shipyard_network::pasta::Pasta>>,
}

#[cfg(target_os = "linux")]
impl PastaPlan {
    /// Starts pasta for the container whose init is `pid` (the hook).
    fn start(&self, pid: libc::pid_t) -> Result<(), String> {
        let pasta = shipyard_network::pasta::Pasta::start(
            &self.program,
            self.features,
            pid,
            &self.ports,
            &self.dir,
        )
        .map_err(|error| {
            if self.defaulted {
                crate::hint(HOST_INSTEAD);
            }
            error.to_string()
        })?;
        *self.running.borrow_mut() = Some(pasta);
        announce(&self.requests, &self.ports);
        Ok(())
    }
}

/// How a run's container is connected.
#[derive(Debug)]
enum Connection {
    /// It isn't (the host's network, or none at all).
    Nothing,
    /// On bridge networks, in order.
    #[cfg(target_os = "linux")]
    Bridges(Vec<Attachment>),
    /// Through pasta.
    #[cfg(target_os = "linux")]
    Pasta(PastaPlan),
}

/// A run's network, set up before its container starts: how it's
/// connected, and its directory (the `/etc` files, pasta's files). Dropped
/// when the run ends, in that order (fields drop in order): the directory
/// goes last.
#[derive(Debug)]
pub(crate) struct RunNetwork {
    connection: Connection,
    dir: Option<RunDir>,
}

impl RunNetwork {
    /// Sets up what `plan` asks for (nothing, without one: an unisolated
    /// run), with `exposed` (the image's ports) for `-P`.
    pub(crate) fn set_up(plan: Option<&NetworkPlan>, exposed: &[String]) -> Result<RunNetwork> {
        let Some(plan) = plan else {
            return Ok(RunNetwork {
                connection: Connection::Nothing,
                dir: None,
            });
        };
        let requests = port_requests(&plan.ports, plan.publish_all, exposed);
        if !plan.etc_files && plan.mode.own_namespace() {
            warn_about_host_resolvers();
        }
        #[cfg(target_os = "linux")]
        {
            // A bridge network's address is taken now (each network's lock
            // is held briefly); the container is connected once its
            // namespace exists.
            if let NetworkMode::Bridges(joined) = &plan.mode {
                let attachments = take_all(plan, joined, &requests)?;
                let addresses = attachments
                    .iter()
                    .find(|attachment| attachment.routes.ipv4)
                    .map(Attachment::addresses)
                    .unwrap_or_default();
                let dir = plan
                    .etc_files
                    .then(|| {
                        let configs: Vec<_> = attachments
                            .iter()
                            .map(|attachment| &attachment.config)
                            .collect();
                        RunDir::make(plan, &addresses, resolver(&configs))
                    })
                    .transpose()?;
                return Ok(RunNetwork {
                    connection: Connection::Bridges(attachments),
                    dir,
                });
            }
            if plan.mode == NetworkMode::Pasta {
                let (program, features) = plan
                    .pasta
                    .clone()
                    .context("no pasta was found for the run")?;
                // pasta publishes on both families, as the firewall does.
                let held = shipyard_network::ports::hold(&requests)?;
                let ports = held.iter().map(|held| held.published).collect();
                // pasta answers DNS itself, for the host's resolver.
                let resolver = std::net::IpAddr::V4(shipyard_network::pasta::DNS_ADDRESS);
                let dir = RunDir::make(plan, &[], Some(resolver))?;
                let pasta = PastaPlan {
                    program,
                    features,
                    held: std::cell::RefCell::new(held),
                    ports,
                    requests,
                    dir: dir.path.clone(),
                    defaulted: plan.defaulted,
                    running: std::cell::RefCell::new(None),
                };
                return Ok(RunNetwork {
                    connection: Connection::Pasta(pasta),
                    dir: Some(dir),
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = requests;
        let dir = plan
            .etc_files
            .then(|| RunDir::make(plan, &[], None))
            .transpose()?;
        Ok(RunNetwork {
            connection: Connection::Nothing,
            dir,
        })
    }

    /// Lets go of the host ports pasta will publish: call it just before
    /// the container is started (see `PastaPlan::held`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::unused_self, reason = "networks are Linux's")
    )]
    pub(crate) fn release_ports(&self) {
        #[cfg(target_os = "linux")]
        if let Connection::Pasta(pasta) = &self.connection {
            pasta.held.borrow_mut().clear();
        }
    }

    /// The run's `/etc` files, if it gets them.
    pub(crate) fn etc(&self) -> Option<&RunDir> {
        self.dir.as_ref()
    }

    /// The runtime's network hook: it connects a container on a bridge
    /// network, or starts its pasta.
    pub(crate) fn hook(
        &self,
    ) -> Option<impl FnMut(shipyard_runtime::process::NetworkTarget<'_>) -> Result<(), String> + '_>
    {
        let connected = !matches!(self.connection, Connection::Nothing);
        connected.then_some(
            move |target: shipyard_runtime::process::NetworkTarget<'_>| -> Result<(), String> {
                match &self.connection {
                    #[cfg(target_os = "linux")]
                    Connection::Bridges(attachments) => attachments
                        .iter()
                        .try_for_each(|attachment| attachment.connect(target.netns)),
                    #[cfg(target_os = "linux")]
                    Connection::Pasta(pasta) => pasta.start(target.pid),
                    Connection::Nothing => {
                        let _ = target;
                        Ok(())
                    }
                }
            },
        )
    }
}

/// Warns that a container with a network of its own but no root of its own
/// sees the host's `/etc/resolv.conf`, if its resolvers are only the host's
/// own (`127.0.0.53`, say): they're unreachable from the container's
/// network, and its DNS lookups will fail.
fn warn_about_host_resolvers() {
    let host = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let servers: Vec<std::net::IpAddr> = host
        .lines()
        .filter_map(|line| line.strip_prefix("nameserver"))
        .filter_map(|server| server.trim().parse().ok())
        .collect();
    if !servers.is_empty() && servers.iter().all(std::net::IpAddr::is_loopback) {
        crate::warning(
            "the container sees the host's /etc/resolv.conf, whose resolvers are the host's own \
             (on its loopback), which a network of its own can't reach: its DNS lookups will \
             fail. Give it a root of its own (--image or --rootfs, then it gets resolvers it can \
             reach) or the host's network (--network host)",
        );
    }
}

/// A run's directory, `<runtime dir>/runs/<id>`, with the `/etc` files bound
/// into its container (and pasta's files, for a pasta run). Held (an
/// exclusive `flock` on its file [`RUN_LOCK`], and on it) while the run lives; removed
/// when it ends, or by the next run if it was killed.
#[derive(Debug)]
pub(crate) struct RunDir {
    path: PathBuf,
    /// Held while the run lives (see [`sweep_runs`]): its [`RUN_LOCK`],
    /// and the directory itself, which an older Shipyard's sweep probes.
    lock: Option<[nix::fcntl::Flock<File>; 2]>,
}

/// The file in a run's directory whose lock says the run lives: the run's
/// own (0600), so no other user can open it, and hold it to keep a dead
/// run's directory from being swept. (The directory is 0755.)
const RUN_LOCK: &str = "lock";

/// The files a run gets, by name: each goes in the container's `/etc`.
pub(crate) const ETC_FILES: [&str; 3] = ["hosts", "hostname", "resolv.conf"];

impl RunDir {
    /// Makes the run's directory with its `/etc` files, as `plan` says, for
    /// a container with `addresses` (on a bridge network), whose resolver is
    /// `resolver` (its network's DNS server, or pasta's) unless `--dns`
    /// names others. Stale directories of runs that are gone are swept
    /// first.
    pub(crate) fn make(
        plan: &NetworkPlan,
        addresses: &[std::net::IpAddr],
        resolver: Option<std::net::IpAddr>,
    ) -> Result<RunDir> {
        use shipyard_network::files;
        let mut failed = None;
        let runs = runs_dirs()
            .into_iter()
            .find(|runs| match make_runs_dir(runs) {
                Ok(()) => true,
                Err(error) => {
                    failed.get_or_insert(error);
                    false
                }
            })
            .ok_or_else(|| {
                failed.unwrap_or_else(|| anyhow::anyhow!("nowhere to keep the run's files"))
            })?;
        // Sweeping, and making a directory then locking it, under one lock:
        // a sweep never finds a new run's directory before its lock.
        let sweeping = lock_runs(&runs)?;
        sweep_runs(&runs);
        let path = runs.join(plan.run.as_str());
        // Ours alone while it's filled (whatever the umask): nobody can put
        // a link in it for its files to be written through.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("making {}", path.display()))?;
        // Removed on drop from here on, even if what follows fails.
        let mut dir = RunDir { path, lock: None };
        dir.lock = Some([lock_run(&dir.path, true)?, lock_dir(&dir.path)?]);
        drop(sweeping);
        let own = plan.mode.own_namespace();
        // With the host's network, its files as they are (its resolvers work
        // from there); with one of its own, the real upstreams behind a
        // local stub.
        let resolv_source: &[&str] = if own {
            &files::HOST_RESOLV_CONF
        } else {
            &["/etc/resolv.conf"]
        };
        let host_resolv = resolv_source
            .iter()
            .find_map(|path| fs::read_to_string(path).ok())
            .unwrap_or_default();
        let host_hosts = if own {
            None
        } else {
            Some(fs::read_to_string("/etc/hosts").unwrap_or_default())
        };
        let mut dns = plan.dns.clone();
        if let Some(resolver) = resolver
            && dns.servers.is_empty()
        {
            dns.servers = vec![resolver];
        }
        let contents = [
            files::hosts(
                host_hosts.as_deref(),
                &plan.hostname,
                addresses,
                &plan.hosts,
            ),
            files::hostname(&plan.hostname),
            files::resolv_conf(&host_resolv, own, &dns),
        ];
        for (name, text) in ETC_FILES.iter().zip(contents) {
            let file = dir.path.join(name);
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&file)
                .and_then(|mut out| {
                    use std::io::Write as _;
                    out.write_all(text.as_bytes())
                })
                .with_context(|| format!("writing {}", file.display()))?;
            // Readable by the container's root even when it isn't the
            // host's (`--uidmap`): its binds are made with its credentials.
            set_mode(&file, 0o644)?;
        }
        set_mode(&dir.path, 0o755)?;
        Ok(dir)
    }

    /// Where its file `name` is.
    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        // Removed while it's still locked (the lock goes after, with the
        // fields), so no sweep can take a directory half gone.
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Where run directories can go, in order of preference: the runtime
/// state's (`/run/shipyard`, or `$XDG_RUNTIME_DIR/shipyard` rootless); the
/// image store's; a directory of this user's in the temporary one
/// ([`make_runs_dir`] checks it's theirs alone). The first that can be made
/// is used: a rootless caller's `HOME` may be someone else's (`sudo -u`).
fn runs_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Root in a user namespace of its own (`unshare -U -r`) isn't the
    // host's: `/run/shipyard` and `/var/lib/shipyard` aren't its own. Its
    // runtime directory is, if it has one.
    if nix::unistd::geteuid().is_root() && !host_root() {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
            dirs.push(PathBuf::from(runtime).join("shipyard/runs"));
        }
    } else {
        if let Ok(root) = shipyard_runtime::container::default_root() {
            dirs.push(root.join("runs"));
        }
        if let Ok(store) = crate::image::store_dir() {
            dirs.push(store.join("runs"));
        }
    }
    dirs.push(
        std::env::temp_dir()
            .join(format!("shipyard-{}", nix::unistd::geteuid()))
            .join("runs"),
    );
    dirs
}

/// Makes `runs` if it's missing. As root, it and its parent (the runtime
/// state's directory) are made passable by anyone (`--x`): a container whose
/// root isn't the host's (`--uidmap`) reaches its `/etc` files through them.
/// Neither can be listed or written by others. Rootless, a container's root
/// is the user, who owns them: they stay private (the image store's, say).
fn make_runs_dir(runs: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let me = nix::unistd::geteuid();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(if me.is_root() { 0o711 } else { 0o700 })
        .create(runs)
        .with_context(|| format!("making {}", runs.display()))?;
    for dir in [runs.parent(), Some(runs)].into_iter().flatten() {
        let metadata =
            fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
        if !metadata.is_dir() || metadata.uid() != me.as_raw() {
            bail!(
                "{} must be a directory of this user's, not a symbolic link",
                dir.display()
            );
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 {
            bail!("{}: others can write to it", dir.display());
        }
        if me.is_root() && mode & 0o011 != 0o011 {
            set_mode(dir, mode | 0o011)?;
        }
    }
    Ok(())
}

/// The lock on `runs` itself (its file [`RUNS_LOCK`]), held to sweep it or
/// add to it; each holder is brief, so it's waited for.
fn lock_runs(runs: &Path) -> Result<nix::fcntl::Flock<File>> {
    let path = runs.join(RUNS_LOCK);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    flock(file, &path, nix::fcntl::FlockArg::LockExclusive)
}

/// The file in `runs` [`lock_runs`] locks: not a run's directory.
const RUNS_LOCK: &str = "lock";

/// Removes the run directories whose runs are gone (their lock is free).
/// Called with [`lock_runs`] held.
fn sweep_runs(runs: &Path) {
    let Ok(entries) = fs::read_dir(runs) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        // One without the file was made by an older Shipyard, which locked
        // the directory itself: probed that way, so an upgrade never sweeps
        // a live run's files.
        let lock = if path.join(RUN_LOCK).symlink_metadata().is_ok() {
            lock_run(&path, false)
        } else {
            lock_dir(&path)
        };
        if let Ok(_stale) = lock {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// An exclusive `flock` on the run directory `dir`'s [`RUN_LOCK`] file,
/// without waiting; the file is made (the run's alone) if `make`.
fn lock_run(dir: &Path, make: bool) -> Result<nix::fcntl::Flock<File>> {
    let path = dir.join(RUN_LOCK);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(make)
        .create_new(make)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    flock(file, &path, nix::fcntl::FlockArg::LockExclusiveNonblock)
}

/// An exclusive `flock` on the directory `path`, without waiting: how an
/// older Shipyard held its runs' directories, and probes them still.
fn lock_dir(path: &Path) -> Result<nix::fcntl::Flock<File>> {
    let dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    flock(dir, path, nix::fcntl::FlockArg::LockExclusiveNonblock)
}

/// `file` (open at `path`) locked as `how` says.
fn flock(file: File, path: &Path, how: nix::fcntl::FlockArg) -> Result<nix::fcntl::Flock<File>> {
    nix::fcntl::Flock::lock(file, how)
        .map_err(|(_, errno)| anyhow::anyhow!("locking {}: {}", path.display(), errno.desc()))
}

/// Sets `path`'s permission bits.
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting {}'s mode", path.display()))
}

/// `--network`'s modes: names no network can have.
pub(crate) const RESERVED: [&str; 3] = ["host", "none", "pasta"];

/// What `shipyard network create NAME` asks for: `subnets` and `gateways`
/// are at most one of each family (an IPv6 one means IPv6, as `ipv6`
/// does).
pub(crate) fn spec(
    name: &str,
    subnets: &[shipyard_network::IpNet],
    gateways: &[std::net::IpAddr],
    ipv6: bool,
    internal: bool,
) -> Result<shipyard_network::store::NetworkSpec> {
    /// The one of `of` (at most one: of one family, from `--what`).
    fn one<T>(what: &str, mut of: Vec<T>) -> Result<Option<T>> {
        if of.len() > 1 {
            bail!("--{what} is given twice for one family: once for IPv4, once for IPv6");
        }
        Ok(of.pop())
    }
    let subnet = one(
        "subnet",
        subnets
            .iter()
            .copied()
            .filter(shipyard_network::IpNet::is_ipv4)
            .collect(),
    )?;
    let subnet6 = one(
        "subnet",
        subnets
            .iter()
            .copied()
            .filter(|net| !net.is_ipv4())
            .collect(),
    )?;
    let gateway = one(
        "gateway",
        gateways
            .iter()
            .copied()
            .filter(std::net::IpAddr::is_ipv4)
            .collect(),
    )?;
    let gateway6 = one(
        "gateway",
        gateways
            .iter()
            .filter_map(|ip| match ip {
                std::net::IpAddr::V6(ip) => Some(*ip),
                std::net::IpAddr::V4(_) => None,
            })
            .collect(),
    )?;
    Ok(shipyard_network::store::NetworkSpec {
        name: name.to_owned(),
        subnet,
        gateway,
        internal,
        ipv6: ipv6 || subnet6.is_some() || gateway6.is_some(),
        subnet6,
        gateway6,
    })
}

/// `shipyard network create`: the network named `spec.name`, with its bridge
/// made at once.
#[cfg(target_os = "linux")]
pub(crate) fn create(
    spec: &shipyard_network::store::NetworkSpec,
) -> Result<shipyard_network::store::NetworkConfig> {
    if RESERVED.contains(&spec.name.as_str()) {
        bail!(
            "{} is a --network mode, not a name a network can have",
            spec.name
        );
    }
    if spec.ipv6 && !Path::new("/proc/sys/net/ipv6").exists() {
        bail!("--ipv6: this host's kernel has IPv6 turned off (ipv6.disable=1)");
    }
    let networks = networks()?;
    let config = networks.create(spec, &host_routes()?)?;
    if let Err(error) = shipyard_network::attach::ensure_bridge(&config) {
        // A network that can't have its bridge isn't one.
        if let Ok(locked) = networks.lock(&config.name) {
            let _ = networks.remove(locked);
        }
        return Err(anyhow::Error::from(error).context(format!("the network {}", config.name)));
    }
    Ok(config)
}

/// Off Linux: no networks.
#[cfg(not(target_os = "linux"))]
pub(crate) fn create(
    _spec: &shipyard_network::store::NetworkSpec,
) -> Result<shipyard_network::store::NetworkConfig> {
    off_linux()
}

/// A network, and its live leases: the containers on it now.
pub(crate) struct Listed {
    pub(crate) config: shipyard_network::store::NetworkConfig,
    pub(crate) leases: Vec<shipyard_network::store::Lease>,
    /// Whether its DNS server is running.
    pub(crate) dns_running: bool,
}

/// `shipyard network ls` and `inspect`: the networks named (all, for
/// none), with their live leases.
pub(crate) fn list(names: &[String]) -> Result<Vec<Listed>> {
    use shipyard_network::store::StoreError;
    let networks = networks()?;
    let configs = if names.is_empty() {
        networks.list()?
    } else {
        names
            .iter()
            .map(|name| {
                networks
                    .get(name)?
                    .ok_or_else(|| StoreError::NotFound(name.clone()).into())
            })
            .collect::<Result<_>>()?
    };
    configs
        .into_iter()
        .map(|config| {
            let leases = networks.live_leases(&config.name)?;
            let dns_running = networks.dns_running(&config.name);
            Ok(Listed {
                config,
                leases,
                dns_running,
            })
        })
        .collect()
}

/// `shipyard network rm`: the network named `name`, if no container uses it
/// (never the default one), with its bridge.
pub(crate) fn remove(name: &str) -> Result<()> {
    use shipyard_network::store::DEFAULT_NETWORK;
    if name == DEFAULT_NETWORK {
        bail!("the default network {DEFAULT_NETWORK} can't be removed");
    }
    let networks = networks()?;
    let locked = networks.lock(name)?;
    // A container that's gone doesn't keep it; the network's rules and
    // bridge go with it.
    teardown(&networks, &locked)?;
    networks.remove(locked)?;
    Ok(())
}

/// Takes the network `locked` apart, if no container uses it: what gone
/// containers left, its firewall rules, its bridge. (Its directory is the
/// caller's to remove.)
#[cfg(target_os = "linux")]
fn teardown(
    networks: &shipyard_network::store::Networks,
    locked: &shipyard_network::store::LockedNetwork,
) -> Result<()> {
    let name = &locked.config().name;
    let mut nft = shipyard_network::nft::Nftables::new()?;
    sweep_gone(locked, &mut nft).with_context(|| format!("the network {name}"))?;
    // Its own message names the network.
    locked.ensure_unused()?;
    {
        // Not while the firewall is rebuilt from every network (which
        // would put this one back).
        let _all = networks.lock_all()?;
        nft.remove_network(&shipyard_network::nft::NetworkRules::from(locked.config()))
            .with_context(|| format!("the network {name}'s firewall rules"))?;
    }
    shipyard_network::attach::delete_bridge(locked.config())
        .with_context(|| format!("the network {name}'s bridge"))?;
    Ok(())
}

/// Off Linux: there are no networks to take apart.
#[cfg(not(target_os = "linux"))]
fn teardown(
    _networks: &shipyard_network::store::Networks,
    locked: &shipyard_network::store::LockedNetwork,
) -> Result<()> {
    Ok(locked.ensure_unused()?)
}

/// What `shipyard network prune` did.
pub(crate) struct Pruned {
    pub(crate) removed: Vec<String>,
    pub(crate) in_use: Vec<String>,
    pub(crate) failed: Vec<anyhow::Error>,
}

/// `shipyard network prune`: every network but the default one that no
/// container uses.
pub(crate) fn prune() -> Result<Pruned> {
    use shipyard_network::store::{DEFAULT_NETWORK, StoreError};
    let mut pruned = Pruned {
        removed: Vec::new(),
        in_use: Vec::new(),
        failed: Vec::new(),
    };
    for config in networks()?.list()? {
        if config.name == DEFAULT_NETWORK {
            continue;
        }
        match remove(&config.name) {
            Ok(()) => pruned.removed.push(config.name),
            Err(error)
                if matches!(
                    error.downcast_ref::<StoreError>(),
                    Some(StoreError::InUse { .. })
                ) =>
            {
                pruned.in_use.push(config.name);
            }
            Err(error) => pruned
                .failed
                .push(error.context(format!("the network {}", config.name))),
        }
    }
    Ok(pruned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use shipyard_network::nft::Chain;

    /// A run's lock is a file only it can open: a lock on its directory
    /// (anyone's) no longer keeps a dead run's files from the sweep. An older
    /// Shipyard's directory, without the file, is still probed by its own
    /// lock.
    #[test]
    fn the_sweep_goes_by_each_runs_own_lock_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let runs = std::env::temp_dir().join(format!("shipyard-runs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&runs);
        let dir = |name: &str| {
            let path = runs.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        };
        let live = dir("live");
        let _live = lock_run(&live, true).unwrap();
        let mode = fs::metadata(live.join(RUN_LOCK))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        let dead = dir("dead");
        drop(lock_run(&dead, true).unwrap());
        let _squatter = lock_dir(&dead).unwrap();
        let old_live = dir("old-live");
        let _old_live = lock_dir(&old_live).unwrap();
        let old_dead = dir("old-dead");
        let _sweeping = lock_runs(&runs).unwrap();

        sweep_runs(&runs);
        assert!(live.exists());
        assert!(!dead.exists());
        assert!(old_live.exists());
        assert!(!old_dead.exists());
        assert!(
            runs.join(RUNS_LOCK).exists(),
            "the runs' own lock is no run"
        );
        fs::remove_dir_all(&runs).unwrap();
    }

    #[test]
    fn ipv6s_route_goes_through_a_network_with_ipv6() {
        // The first with IPv6 and a way out...
        assert_eq!(
            route_network6(&[false, true, false], &[false, true, true]),
            Some(2)
        );
        // ...else the first with IPv6...
        assert_eq!(route_network6(&[false, true], &[false, true]), Some(1));
        // ...and none without.
        assert_eq!(route_network6(&[false, false], &[false, false]), None);
        // IPv4's own, beside: the first network with a way out.
        assert_eq!(route_network(&[false, true, false]), 0);
    }

    #[test]
    fn publish_all_adds_the_exposed_ports_p_doesnt_publish() {
        use shipyard_network::ports::{Protocol, parse_publish};
        let given = parse_publish("8080:80").unwrap();
        let exposed = ["80/tcp", "80/udp", "443", "nonsense"].map(String::from);
        let ports = |requests: Vec<shipyard_network::ports::PortRequest>| -> Vec<_> {
            requests
                .iter()
                .map(|request| (request.host_port, request.container_port, request.protocol))
                .collect()
        };
        // Without -P, -p's alone.
        assert_eq!(
            ports(port_requests(&given, false, &exposed)),
            [(Some(8080), 80, Protocol::Tcp)]
        );
        // With it, the rest on free ports: not 80/tcp again (-p has it),
        // and a key that can't be read skipped.
        assert_eq!(
            ports(port_requests(&given, true, &exposed)),
            [
                (Some(8080), 80, Protocol::Tcp),
                (None, 80, Protocol::Udp),
                (None, 443, Protocol::Tcp),
            ]
        );
    }

    fn chain(family: u8, table: &str, name: &str, policy: Option<u32>) -> Chain {
        Chain {
            family,
            table: table.into(),
            name: name.into(),
            kind: Some("filter".into()),
            hook: policy.map(|_| 2),
            policy,
        }
    }

    fn config(name: &str, internal: bool, gateway: &str) -> shipyard_network::store::NetworkConfig {
        shipyard_network::store::NetworkConfig {
            name: name.into(),
            subnet: "10.0.0.0/8".parse().unwrap(),
            gateway: gateway.parse().unwrap(),
            bridge: shipyard_network::store::bridge_name(name),
            internal,
            created: 0,
            subnet6: None,
            gateway6: None,
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

    #[test]
    fn the_route_goes_through_the_first_network_with_a_way_out() {
        assert_eq!(route_network(&[false, false]), 0);
        assert_eq!(route_network(&[true, false, false]), 1);
        assert_eq!(route_network(&[true, true]), 0);
    }

    #[test]
    fn the_resolver_is_the_first_server_with_a_way_out() {
        let bridge = config("bridge", false, "10.88.0.1");
        let quiet = config("quiet", true, "10.89.0.1");
        let out = config("out", false, "10.90.0.1");
        let ip = |text: &str| Some(text.parse().unwrap());
        assert_eq!(resolver(&[&quiet, &out]), ip("10.90.0.1"));
        // The default network has no server; an internal one's forwards for
        // a container with a way out elsewhere.
        assert_eq!(resolver(&[&bridge, &quiet]), ip("10.89.0.1"));
        assert_eq!(resolver(&[&bridge]), None);
    }

    #[test]
    fn another_firewalls_input_drop_is_named_for_ipv4_only() {
        let input = |family: u8, table: &str, name: &str| Chain {
            hook: Some(1),
            ..chain(family, table, name, Some(0))
        };
        let config = shipyard_network::store::NetworkConfig {
            name: "app".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            gateway: "10.89.0.1".parse().unwrap(),
            bridge: "shy-x".into(),
            internal: false,
            created: 0,
            subnet6: None,
            gateway6: None,
        };
        // ufw's: one for each family. The server is on IPv4.
        let warnings = closed_dns(
            &[
                input(2, "filter", "INPUT"),
                input(10, "filter", "INPUT"),
                input(1, "shipyard", "input"),
                // A forward chain isn't one that drops what comes in.
                chain(1, "firewalld", "filter_FORWARD", Some(0)),
            ],
            &config,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("iptables -I INPUT -i shy-x -p udp --dport 53 -j ACCEPT"),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("10.89.0.1:53"), "{}", warnings[0]);
        let nft = closed_dns(&[input(1, "firewalld", "filter_INPUT")], &config);
        assert!(
            nft[0].contains("nft insert rule inet firewalld filter_INPUT iifname shy-x"),
            "{}",
            nft[0]
        );
    }

    #[test]
    fn another_firewalls_forward_drop_is_named_with_its_fix() {
        // Ours, and an accepting one, are fine.
        assert!(
            other_firewalls(
                &[
                    chain(1, "shipyard", "forward", Some(0)),
                    chain(1, "filter", "forward", Some(1)),
                ],
                "shipyard0",
                None
            )
            .is_empty()
        );
        // Docker's: its own way out.
        let docker = other_firewalls(
            &[
                chain(2, "filter", "FORWARD", Some(0)),
                chain(2, "filter", "DOCKER-USER", None),
            ],
            "shipyard0",
            Some("eth0"),
        );
        assert_eq!(docker.len(), 1);
        assert!(docker[0].contains("ip filter FORWARD"), "{}", docker[0]);
        assert!(
            // Out to the world, not into Docker's bridges.
            docker[0].contains("iptables -I DOCKER-USER -i shipyard0 -o eth0 -j ACCEPT"),
            "{}",
            docker[0]
        );
        // Anyone else's: nft's.
        let firewalld = other_firewalls(
            &[chain(1, "firewalld", "filter_FORWARD", Some(0))],
            "shy-x",
            None,
        );
        assert!(
            firewalld[0]
                .contains("nft insert rule inet firewalld filter_FORWARD iifname shy-x accept"),
            "{}",
            firewalld[0]
        );
    }
}
