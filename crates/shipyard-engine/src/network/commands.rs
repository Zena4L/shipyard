//! `shipyard network create`, `ls`, `inspect`, `rm` and `prune`: what they
//! do, as data the CLI prints.

#[cfg(target_os = "linux")]
use std::path::Path;

use super::networks;
#[cfg(not(target_os = "linux"))]
use super::off_linux;
#[cfg(target_os = "linux")]
use super::{RESERVED, host_routes};
use crate::Engine;
#[cfg(target_os = "linux")]
use crate::error::Context as _;
use crate::error::{Error, Result, bail};

/// What `shipyard network create NAME` asks for: `subnets` and `gateways`
/// are at most one of each family (an IPv6 one means IPv6, as `ipv6`
/// does).
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest) if a
/// family's subnet or gateway is given twice.
pub fn spec(
    name: &str,
    subnets: &[shipyard_network::IpNet],
    gateways: &[std::net::IpAddr],
    options: SpecOptions,
) -> Result<shipyard_network::store::NetworkSpec> {
    /// The one of `of` (at most one: of one family, from `--what`).
    fn one<T>(what: &str, mut of: Vec<T>) -> Result<Option<T>> {
        if of.len() > 1 {
            bail!(
                InvalidRequest,
                "--{what} is given twice for one family: once for IPv4, once for IPv6"
            );
        }
        Ok(of.pop())
    }
    if options.isolate_host && name == shipyard_network::store::DEFAULT_NETWORK {
        // It has no DNS server: isolated, its containers would reach no
        // resolver on the host.
        bail!(
            InvalidRequest,
            "--isolate-host: the default network {name} can't be isolated (make one of your own)"
        );
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
        internal: options.internal,
        ipv6: options.ipv6 || subnet6.is_some() || gateway6.is_some(),
        subnet6,
        gateway6,
        isolate_host: options.isolate_host,
        conntrack_limit: options.conntrack_limit,
    })
}

/// A network's options (`network create`'s flags), for [`spec`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is one of `network create`'s flags, on or off independently"
)]
pub struct SpecOptions {
    /// IPv6 too (`--ipv6`).
    pub ipv6: bool,
    /// No way out (`--internal`).
    pub internal: bool,
    /// Its containers kept from the host (`--isolate-host`).
    pub isolate_host: bool,
    /// Its containers' connections limited (`--conntrack-limit`).
    pub conntrack_limit: bool,
}

/// `shipyard network create`: the network named `spec.name`, with its bridge
/// made at once.
///
/// # Errors
///
/// If the name is taken or reserved, the kernel has no IPv6 when it's
/// asked for, or the bridge can't be made.
#[cfg(target_os = "linux")]
pub fn create(
    engine: &Engine,
    spec: &shipyard_network::store::NetworkSpec,
) -> Result<shipyard_network::store::NetworkConfig> {
    if RESERVED.contains(&spec.name.as_str()) {
        bail!(
            InvalidRequest,
            "{} is a --network mode, not a name a network can have",
            spec.name
        );
    }
    if spec.ipv6 && !Path::new("/proc/sys/net/ipv6").exists() {
        bail!(
            Unsupported,
            "--ipv6: this host's kernel has IPv6 turned off (ipv6.disable=1)"
        );
    }
    let networks = networks(engine)?;
    let config = networks.create(spec, &host_routes()?)?;
    if let Err(error) = shipyard_network::attach::ensure_bridge(&config) {
        // A network that can't have its bridge isn't one.
        if let Ok(locked) = networks.lock(&config.name) {
            let _ = networks.remove(locked);
        }
        return Err(Error::from(error).context(format!("the network {}", config.name)));
    }
    crate::events::emit_about(engine, crate::events::Kind::Network, "create", &config.name);
    Ok(config)
}

/// Off Linux: no networks.
///
/// # Errors
///
/// Always [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
#[cfg(not(target_os = "linux"))]
pub fn create(
    engine: &Engine,
    spec: &shipyard_network::store::NetworkSpec,
) -> Result<shipyard_network::store::NetworkConfig> {
    let _ = (engine, spec);
    off_linux()
}

/// A network, and its live leases: the containers on it now.
#[derive(Debug)]
pub struct Listed {
    /// The network.
    pub config: shipyard_network::store::NetworkConfig,
    /// Its live leases.
    pub leases: Vec<shipyard_network::store::Lease>,
    /// Whether its DNS server is running.
    pub dns_running: bool,
}

/// `shipyard network ls` and `inspect`: the networks named (all, for
/// none), with their live leases.
///
/// # Errors
///
/// If one named doesn't exist, or the store can't be read.
pub fn list(engine: &Engine, names: &[String]) -> Result<Vec<Listed>> {
    use shipyard_network::store::StoreError;
    let networks = networks(engine)?;
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

/// A network as `shipyard network inspect` shows it (its JSON keys are
/// Docker-like, and kept: scripts read them).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Inspected {
    /// Its name.
    pub name: String,
    /// Its IPv4 subnet.
    pub subnet: shipyard_network::ipnet::IpNet,
    /// Its IPv4 gateway.
    pub gateway: std::net::IpAddr,
    /// Its IPv6 subnet, if it has IPv6.
    pub subnet6: Option<shipyard_network::ipnet::IpNet>,
    /// Its IPv6 gateway, if it has IPv6.
    pub gateway6: Option<std::net::Ipv6Addr>,
    /// Its bridge's name.
    pub bridge: String,
    /// Whether it has no way out.
    pub internal: bool,
    /// Whether its containers are kept from the host (`--isolate-host`).
    pub isolate_host: bool,
    /// The most connections each of its containers may have open, if
    /// they're limited (`--conntrack-limit`).
    pub conntrack_limit: Option<u32>,
    /// When it was made (seconds since the epoch).
    pub created: u64,
    /// Whether its DNS server is running.
    #[serde(rename = "DNSRunning")]
    pub dns_running: bool,
    /// The containers on it now.
    pub containers: Vec<Attached>,
}

/// A container on a network, as `network inspect` shows it: one live
/// lease.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Attached {
    /// The container's ID (`ps`'s, in full): what the other commands take.
    /// A run with no record (`--no-image`, say) has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Its run's ID (each start's own).
    pub run: String,
    /// The container's name.
    pub name: Option<String>,
    /// Its other names on the network.
    pub aliases: Vec<String>,
    /// Its hostname.
    pub hostname: String,
    /// Its interface in the container.
    pub interface: String,
    /// Its IPv4 address.
    #[serde(rename = "IPAddress")]
    pub ip_address: std::net::IpAddr,
    /// Its IPv6 address, on a network with IPv6.
    #[serde(rename = "IPv6Address")]
    pub ipv6_address: Option<std::net::Ipv6Addr>,
    /// Its hardware address (`02:42:…`).
    pub mac_address: String,
    /// The host's end of its veth pair.
    pub host_veth: String,
    /// The host ports it publishes.
    pub ports: Vec<shipyard_network::ports::Published>,
}

impl From<Listed> for Inspected {
    fn from(listed: Listed) -> Self {
        let Listed {
            config,
            leases,
            dns_running,
        } = listed;
        Self {
            name: config.name,
            subnet: config.subnet,
            gateway: config.gateway,
            subnet6: config.subnet6,
            gateway6: config.gateway6,
            bridge: config.bridge,
            internal: config.internal,
            isolate_host: config.isolate_host,
            conntrack_limit: config
                .conntrack_limit
                .then_some(shipyard_network::nft::CONNTRACK_LIMIT),
            created: config.created,
            dns_running,
            containers: leases
                .into_iter()
                .map(|lease| Attached {
                    mac_address: shipyard_network::store::format_mac(lease.mac),
                    container: lease.container,
                    run: lease.run,
                    name: lease.name,
                    aliases: lease.aliases,
                    hostname: lease.hostname,
                    interface: lease.interface,
                    ip_address: lease.ip,
                    ipv6_address: lease.ip6,
                    host_veth: lease.host_veth,
                    ports: lease.ports,
                })
                .collect(),
        }
    }
}

/// `shipyard network inspect`: the networks named (all, for none), with
/// the containers on them now.
///
/// # Errors
///
/// As [`list`].
pub fn inspect(engine: &Engine, names: &[String]) -> Result<Vec<Inspected>> {
    Ok(list(engine, names)?
        .into_iter()
        .map(Inspected::from)
        .collect())
}

/// `shipyard network rm`: the network named `name`, if no container uses it
/// (never the default one), with its bridge. A container uses it if its
/// record joins it, running or not (a stopped one would start on it again),
/// or a run holds a lease on it.
///
/// # Errors
///
/// If it's the default one, doesn't exist, is in use
/// ([`ErrorKind::InUse`](crate::ErrorKind::InUse)), or can't be taken apart.
pub fn remove(engine: &Engine, name: &str) -> Result<()> {
    use shipyard_network::store::DEFAULT_NETWORK;
    if name == DEFAULT_NETWORK {
        bail!(
            InvalidRequest,
            "the default network {DEFAULT_NETWORK} can't be removed"
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        off_linux()
    }
    #[cfg(target_os = "linux")]
    {
        let networks = networks(engine)?;
        let locked = networks.lock(name)?;
        ensure_no_record_joins(engine, name)?;
        // A container that's gone doesn't keep it; the network's rules and
        // bridge go with it.
        teardown(engine, &networks, &locked)?;
        networks.remove(locked)?;
        crate::events::emit_about(engine, crate::events::Kind::Network, "destroy", name);
        Ok(())
    }
}

/// [`ErrorKind::InUse`](crate::ErrorKind::InUse) naming the containers
/// whose records join the network `name`, whatever their state. Records
/// that can't be read refuse it too: a container they'd name can't be ruled
/// out.
#[cfg(target_os = "linux")]
fn ensure_no_record_joins(engine: &Engine, name: &str) -> Result<()> {
    use shipyard_network::store::DEFAULT_NETWORK;
    let records = crate::containers::list(engine)
        .with_context(|| format!("the network {name} is kept: its containers can't be listed"))?;
    let mut users: Vec<String> = records
        .iter()
        .filter(|record| record.config.joins(name, DEFAULT_NETWORK))
        .map(|record| record.name.to_string())
        .collect();
    if users.is_empty() {
        return Ok(());
    }
    users.sort();
    bail!(
        InUse,
        "the network {name} is in use by {}",
        shipyard_network::store::users_text(&users)
    );
}

/// Takes the network `locked` apart, if no container uses it: what gone
/// containers left, its firewall rules, its bridge. (Its directory is the
/// caller's to remove.)
#[cfg(target_os = "linux")]
fn teardown(
    engine: &Engine,
    networks: &shipyard_network::store::Networks,
    locked: &shipyard_network::store::LockedNetwork,
) -> Result<()> {
    let name = &locked.config().name;
    let mut nft = shipyard_network::nft::Nftables::new()?;
    super::attach::sweep_gone(locked, &mut nft, engine.reporter().as_ref())
        .with_context(|| format!("the network {name}"))?;
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
    // Its DNS server's cgroup (the server ends as its bridge goes; what's
    // left of it, if anything, with the cgroup).
    if let Err(why) = super::dns::remove_cgroup(&networks.dir().join(name)) {
        engine.warn(
            crate::report::WarningKind::LeftForLater,
            format!("the network {name}'s DNS server's cgroup is left: {why}"),
        );
    }
    Ok(())
}

/// What `shipyard network prune` did.
#[derive(Debug)]
pub struct Pruned {
    /// The networks removed.
    pub removed: Vec<String>,
    /// The networks left, in use.
    pub in_use: Vec<String>,
    /// What couldn't be removed, and why.
    pub failed: Vec<Error>,
}

/// `shipyard network prune`: every network but the default one that no
/// container uses.
///
/// # Errors
///
/// If the networks can't be listed (one that can't be removed is in
/// [`Pruned::failed`]).
pub fn prune(engine: &Engine) -> Result<Pruned> {
    use shipyard_network::store::DEFAULT_NETWORK;
    let mut pruned = Pruned {
        removed: Vec::new(),
        in_use: Vec::new(),
        failed: Vec::new(),
    };
    for config in networks(engine)?.list()? {
        if config.name == DEFAULT_NETWORK {
            continue;
        }
        match remove(engine, &config.name) {
            Ok(()) => pruned.removed.push(config.name),
            // A lease or a record: both are the in-use kind.
            Err(error) if error.kind() == crate::ErrorKind::InUse => {
                pruned.in_use.push(config.name);
            }
            Err(error) => pruned
                .failed
                .push(error.context(format!("the network {}", config.name))),
        }
    }
    crate::events::emit_pruned(engine, crate::events::Kind::Network, pruned.removed.len());
    Ok(pruned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::testing::engine_on_a_new_store;

    #[test]
    fn network_errors_have_kinds() {
        let (engine, _, parent) = engine_on_a_new_store("networks");
        let engine = Engine::new(
            engine
                .paths()
                .clone()
                .with_networks(parent.join("networks")),
            std::sync::Arc::clone(engine.reporter()),
        );
        let twice = spec(
            "twice",
            &[
                "10.98.0.0/24".parse().unwrap(),
                "10.99.0.0/24".parse().unwrap(),
            ],
            &[],
            SpecOptions::default(),
        )
        .unwrap_err();
        assert_eq!(twice.kind(), ErrorKind::InvalidRequest, "{twice:#}");
        let isolated = |name| {
            spec(
                name,
                &[],
                &[],
                SpecOptions {
                    isolate_host: true,
                    ..SpecOptions::default()
                },
            )
        };
        let default = isolated("bridge").unwrap_err();
        assert_eq!(default.kind(), ErrorKind::InvalidRequest, "{default:#}");
        assert!(isolated("kept").unwrap().isolate_host);
        let default = remove(&engine, "bridge").unwrap_err();
        assert_eq!(default.kind(), ErrorKind::InvalidRequest, "{default:#}");
        // Who may have networks at all: the host's root, on Linux.
        let refused = list(&engine, &[]).map(|_| ()).err();
        if cfg!(not(target_os = "linux")) {
            assert_eq!(
                refused.map(|error| error.kind()),
                Some(ErrorKind::Unsupported)
            );
        } else if !super::super::host_root() {
            assert_eq!(
                refused.map(|error| error.kind()),
                Some(ErrorKind::NeedsRoot)
            );
        } else {
            assert!(refused.is_none(), "{refused:?}");
            let missing = remove(&engine, "nosuch").unwrap_err();
            assert_eq!(missing.kind(), ErrorKind::NotFound, "{missing:#}");
        }
        let _ = std::fs::remove_dir_all(&parent);
    }
}
