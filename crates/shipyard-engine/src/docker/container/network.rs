//! A container's `NetworkSettings` as Docker's inspect and list show them: its
//! network mode, and an endpoint per network it's on, from the leases its
//! running start holds.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::Engine;
use crate::containers::{self, Config, Network, Record};

use super::ContainerState;
use super::config::PortBinding;
use super::{nil_if_empty, port_key};

/// Docker's `NetworkSettings`, the part Shipyard fills.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct NetworkSettings {
    /// Each port it exposes, `80/tcp`, with where it's published now
    /// (`null`: not published, or not up).
    pub ports: BTreeMap<String, Option<Vec<PortBinding>>>,
    /// Its bridge networks, by name, with its addresses there now.
    pub networks: BTreeMap<String, EndpointSettings>,
}

/// A container on one network, Docker's `EndpointSettings`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct EndpointSettings {
    /// Its other names there (`null`: none).
    pub aliases: Option<Vec<String>>,
    /// The network's gateway (empty while it isn't up).
    pub gateway: String,
    /// Its IPv4 address there (empty while it isn't up).
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    /// Its IPv4 prefix length.
    #[serde(rename = "IPPrefixLen")]
    pub ip_prefix_len: u8,
    /// Its IPv6 address there.
    #[serde(rename = "GlobalIPv6Address")]
    pub global_ipv6_address: String,
    /// Its IPv6 prefix length.
    #[serde(rename = "GlobalIPv6PrefixLen")]
    pub global_ipv6_prefix_len: u8,
    /// The network's IPv6 gateway.
    #[serde(rename = "IPv6Gateway")]
    pub ipv6_gateway: String,
    /// Its hardware address there.
    pub mac_address: String,
    /// The network's ID ([`network_id`](crate::docker::network_id)), while it's on it.
    #[serde(rename = "NetworkID")]
    pub network_id: String,
    /// Its endpoint's ID ([`endpoint_id`](crate::docker::endpoint_id)), while it's on it.
    #[serde(rename = "EndpointID")]
    pub endpoint_id: String,
    /// The addresses it asked for there (`--ip`, `--ip6`), or `null`.
    #[serde(rename = "IPAMConfig")]
    pub ipam_config: Option<EndpointIpamConfig>,
}

/// The addresses a container asked for on a network (Docker's
/// `EndpointIPAMConfig`).
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct EndpointIpamConfig {
    /// Its IPv4 address there, or empty.
    #[serde(rename = "IPv4Address")]
    pub ipv4_address: String,
    /// Its IPv6 address there, or empty.
    #[serde(rename = "IPv6Address")]
    pub ipv6_address: String,
}

/// Its network mode, Docker's word: its first network's, or
/// `container:ID` for one that joins `network_from`'s.
pub(in crate::docker) fn network_mode(
    config: &Config,
    network_from: Option<&containers::ContainerId>,
) -> String {
    if let Some(other) = network_from {
        return format!("container:{}", other.as_str());
    }
    match config.network.first() {
        Some(Network::Host) => "host".to_owned(),
        Some(Network::None) => "none".to_owned(),
        Some(Network::Pasta) => "pasta".to_owned(),
        Some(Network::Bridges(bridges)) => bridges
            .first()
            .map_or_else(|| "bridge".to_owned(), |bridge| bridge.name.clone()),
        // Not given: the one its start gets ([`crate::network::defaulted`]).
        None => match crate::network::defaulted() {
            crate::network::Defaulted::Bridge => "bridge",
            crate::network::Defaulted::Host => "host",
            crate::network::Defaulted::Pasta => "pasta",
        }
        .to_owned(),
    }
}

/// The networks `record`'s container has a live lease on, each with it:
/// read without making the store, taking a lock, or failing the view for
/// a network that can't be read (left out). Root's networks only (a
/// rootless container has none).
pub(super) fn live_leases(engine: &Engine, record: &Record) -> Vec<LiveLease> {
    let Some(networks) = existing_networks(engine) else {
        return Vec::new();
    };
    let found = networks.leases_of(record.id.as_str());
    with_ids(&networks, found, &mut BTreeMap::new())
}

/// [`live_leases`] of every container at once, by its ID: for a listing,
/// which reads each network once (11.7), not once a container.
pub(in crate::docker) fn live_leases_by_container(
    engine: &Engine,
) -> BTreeMap<String, Vec<LiveLease>> {
    let Some(networks) = existing_networks(engine) else {
        return BTreeMap::new();
    };
    let mut ids = BTreeMap::new();
    networks
        .live_leases_by_container()
        .into_iter()
        .map(|(container, found)| (container, with_ids(&networks, found, &mut ids)))
        .collect()
}

/// The networks, read without making the store (a rootless engine has
/// none).
fn existing_networks(engine: &Engine) -> Option<shipyard_network::store::Networks> {
    engine
        .paths()
        .networks()
        .ok()
        .and_then(shipyard_network::store::Networks::existing)
}

/// `found`'s leases with their networks' IDs, each network's read once
/// (`ids`, by its name).
fn with_ids(
    networks: &shipyard_network::store::Networks,
    found: Vec<(
        shipyard_network::store::NetworkConfig,
        shipyard_network::store::Lease,
    )>,
    ids: &mut BTreeMap<String, String>,
) -> Vec<LiveLease> {
    found
        .into_iter()
        .map(|(config, lease)| {
            let network_id = ids
                .entry(config.name.clone())
                .or_insert_with(|| {
                    // Labels that can't be read: the ID derived, as a
                    // network's of before.
                    let meta = networks.meta(&config.name).unwrap_or_default();
                    crate::docker::network_id(&config, &meta)
                })
                .clone();
            LiveLease {
                config,
                lease,
                network_id,
            }
        })
        .collect()
}

/// A container's live lease on a network, with the network's ID.
pub(in crate::docker) struct LiveLease {
    /// The network.
    pub(super) config: shipyard_network::store::NetworkConfig,
    /// The lease.
    pub(super) lease: shipyard_network::store::Lease,
    /// The network's ID.
    pub(super) network_id: String,
}

/// Docker's `NetworkSettings`: the ports it exposes, published where its
/// start bound them while it's up; each network it joins, with its live
/// addresses (from its `leases`) while it has them.
pub(super) fn network_settings(
    record: &Record,
    state: &ContainerState,
    leases: &[LiveLease],
    network_from: Option<&containers::ContainerId>,
) -> NetworkSettings {
    endpoints(record, state.running, leases, network_from)
}

/// What `record`'s container exposes and joins: as Docker, no ports until
/// it's `up`, then each it exposes with where it's published (`None`:
/// exposed, not published); each network it joins, with its live addresses
/// (from `leases`) while it has them. One that joins another's network
/// (`network_from`) has neither, as Docker shows it: they're the other's.
pub(in crate::docker) fn endpoints(
    record: &Record,
    up: bool,
    leases: &[LiveLease],
    network_from: Option<&containers::ContainerId>,
) -> NetworkSettings {
    if network_from.is_some() {
        return NetworkSettings {
            ports: BTreeMap::new(),
            networks: BTreeMap::new(),
        };
    }
    let mut ports: BTreeMap<String, Option<Vec<PortBinding>>> = BTreeMap::new();
    if up {
        ports.extend(
            record
                .config
                .publish
                .iter()
                .map(|port| (port_key(port.container_port, &port.protocol), None)),
        );
        for bound in &record.ports {
            ports
                .entry(port_key(bound.container_port, &bound.protocol))
                .or_insert(None)
                .get_or_insert_with(Vec::new)
                .push(PortBinding {
                    host_ip: bound.host_ip.to_string(),
                    host_port: bound.host_port.to_string(),
                });
        }
    }
    let mut endpoints: BTreeMap<String, EndpointSettings> = BTreeMap::new();
    // Not on a bridge network it names: its mode's own entry (`host`,
    // `none`, or the default network it gets), as Docker shows it. Pasta
    // isn't one of Docker's networks: none.
    let mode = network_mode(&record.config, None);
    if !matches!(record.config.network.first(), Some(Network::Bridges(_))) && mode != "pasta" {
        endpoints.insert(mode, EndpointSettings::default());
    }
    for network in &record.config.network {
        if let Network::Bridges(bridges) = network {
            for bridge in bridges {
                let asked =
                    (bridge.ip.is_some() || bridge.ip6.is_some()).then(|| EndpointIpamConfig {
                        ipv4_address: bridge.ip.map(|ip| ip.to_string()).unwrap_or_default(),
                        ipv6_address: bridge.ip6.map(|ip| ip.to_string()).unwrap_or_default(),
                    });
                endpoints.insert(
                    bridge.name.clone(),
                    EndpointSettings {
                        aliases: nil_if_empty(bridge.aliases.clone()),
                        ipam_config: asked,
                        ..EndpointSettings::default()
                    },
                );
            }
        }
    }
    for LiveLease {
        config,
        lease,
        network_id,
    } in leases
    {
        let endpoint = endpoints.entry(config.name.clone()).or_default();
        endpoint.gateway = config.gateway.to_string();
        endpoint.ip_address = lease.ip.to_string();
        endpoint.ip_prefix_len = config.subnet.prefix();
        endpoint.global_ipv6_address = lease.ip6.map(|ip| ip.to_string()).unwrap_or_default();
        endpoint.global_ipv6_prefix_len = config.subnet6.map_or(0, |subnet| subnet.prefix());
        endpoint.ipv6_gateway = config.gateway6.map(|ip| ip.to_string()).unwrap_or_default();
        endpoint.mac_address = shipyard_network::store::format_mac(lease.mac);
        endpoint.aliases = nil_if_empty(lease.aliases.clone());
        endpoint.network_id.clone_from(network_id);
        endpoint.endpoint_id = crate::docker::endpoint_id(network_id, record.id.as_str());
    }
    NetworkSettings {
        ports,
        networks: endpoints,
    }
}
