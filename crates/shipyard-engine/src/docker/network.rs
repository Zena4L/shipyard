//! Networks as Docker's Engine API shapes them (step 11.2c2b): `GET
//! /networks`' and `GET /networks/{id}`'s `NetworkResource`, checked
//! against Docker 29.8's at 1.43; and a create from Docker's terms.
//!
//! A network's ID is made at its making (random, kept in its
//! `labels.json`): the same each time it's asked for, new for a network
//! made again under the name. One made before has none kept: its ID is
//! derived from its name and the second it was made (a sha256). Docker always lists `host` and
//! `none` (D11.2c2-3); with networks of its own (root), the default
//! `bridge` too, made first as a run makes it. Rootless, there are no
//! bridge networks: `host`, `none` and `pasta` are listed, and anything
//! that would change a network is refused as needing root.
//!
//! Docker's driver options are honoured or refused, never ignored
//! (D11.2c2b-1): its bridge's at their defaults pass (kept, and shown
//! back), Shipyard's own turn its features on, anything else is refused.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use shipyard_network::store::{DEFAULT_NETWORK, NetworkConfig, NetworkMeta};

use super::filters::NetworkFilters;
use crate::Engine;
use crate::error::{ErrorKind, Result, bail};

/// Docker's bridge options Shipyard's bridges have, and their only value.
const DOCKER_DEFAULTS: [(&str, &str); 4] = [
    ("com.docker.network.bridge.enable_icc", "true"),
    ("com.docker.network.bridge.enable_ip_masquerade", "true"),
    ("com.docker.network.bridge.host_binding_ipv4", "0.0.0.0"),
    ("com.docker.network.driver.mtu", "1500"),
];
/// Shipyard's own option: its containers kept from the host.
pub const ISOLATE_HOST: &str = "com.shipyard.network.isolate-host";
/// Shipyard's own option: its containers' connections limited.
pub const CONNTRACK_LIMIT: &str = "com.shipyard.network.conntrack-limit";

/// A network (Docker's `NetworkResource`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Docker's shape: its flags, each on or off"
)]
pub struct NetworkResource {
    /// Its name.
    pub name: String,
    /// Its ID: 64 hex characters.
    pub id: String,
    /// When it was made (RFC 3339), Docker's zero time for a built-in.
    pub created: String,
    /// Always `local`.
    pub scope: String,
    /// `bridge`, or a built-in's (`host`, `null`, `pasta`).
    pub driver: String,
    /// Whether it has IPv4 (a bridge network always has).
    #[serde(rename = "EnableIPv4")]
    pub enable_ipv4: bool,
    /// Whether it has IPv6.
    #[serde(rename = "EnableIPv6")]
    pub enable_ipv6: bool,
    /// Its addresses.
    #[serde(rename = "IPAM")]
    pub ipam: Ipam,
    /// Whether it has no way out.
    pub internal: bool,
    /// Always false.
    pub attachable: bool,
    /// Always false.
    pub ingress: bool,
    /// Always none.
    pub config_from: ConfigFrom,
    /// Always false.
    pub config_only: bool,
    /// Its containers, by ID: in an inspect only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containers: Option<BTreeMap<String, NetworkContainer>>,
    /// Its driver options, as given (Docker's defaults, Shipyard's own).
    pub options: BTreeMap<String, String>,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
}

/// A network's addresses (Docker's `IPAM`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Ipam {
    /// Always `default`.
    pub driver: String,
    /// Always `null`.
    pub options: Option<()>,
    /// One per family, or `null` for a built-in.
    pub config: Option<Vec<IpamConfig>>,
}

/// One of a network's subnets.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct IpamConfig {
    /// The subnet.
    pub subnet: String,
    /// Its gateway.
    pub gateway: String,
}

/// Where a network's config comes from: always none.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct ConfigFrom {
    /// Always empty.
    pub network: String,
}

/// A container on a network (Docker's `EndpointResource`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct NetworkContainer {
    /// The container's name.
    pub name: String,
    /// Its endpoint's ID ([`endpoint_id`]).
    #[serde(rename = "EndpointID")]
    pub endpoint_id: String,
    /// Its hardware address.
    pub mac_address: String,
    /// Its IPv4 address, with the prefix (`10.89.0.2/24`).
    #[serde(rename = "IPv4Address")]
    pub ipv4_address: String,
    /// Its IPv6 address, with the prefix, or empty.
    #[serde(rename = "IPv6Address")]
    pub ipv6_address: String,
}

/// A network to make, in Docker's terms (`POST /networks/create`), its
/// fields Shipyard can't take already refused by the caller.
#[derive(Debug, Clone, Default)]
pub struct NetworkCreate {
    /// Its name.
    pub name: String,
    /// No way out.
    pub internal: bool,
    /// IPv6 too.
    pub ipv6: bool,
    /// Its subnets: at most one a family.
    pub subnets: Vec<shipyard_network::IpNet>,
    /// Their gateways: at most one a family.
    pub gateways: Vec<IpAddr>,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// Its driver options (checked here).
    pub options: BTreeMap<String, String>,
}

/// The ID of the network `config`: the one its `meta` keeps, else the
/// sha256 of its name and the second it was made (a network made before
/// IDs were kept).
#[must_use]
pub fn network_id(config: &NetworkConfig, meta: &NetworkMeta) -> String {
    meta.id
        .clone()
        .unwrap_or_else(|| hashed(&[config.name.as_bytes(), &config.created.to_be_bytes()]))
}

/// Whether `name` is one of Docker's pre-defined networks: the default
/// `bridge` and the built-ins (`host`, `none`; `pasta`, rootless).
#[must_use]
pub fn is_predefined(name: &str) -> bool {
    name == DEFAULT_NETWORK || BUILTINS_ROOTLESS.contains(&name)
}

/// The ID of the built-in network `name` (`host`, `none`, `pasta`).
fn builtin_id(name: &str) -> String {
    hashed(&[b"shipyard built-in network", name.as_bytes()])
}

/// A container's endpoint on a network: the sha256 of the two IDs.
#[must_use]
pub fn endpoint_id(network_id: &str, container: &str) -> String {
    hashed(&[network_id.as_bytes(), container.as_bytes()])
}

/// The sha256 of `parts`, each ended by a NUL, in hex.
fn hashed(parts: &[&[u8]]) -> String {
    let mut sha = Sha256::new();
    for part in parts {
        sha.update(part);
        sha.update([0]);
    }
    shipyard_sys::hex::lower(&sha.finalize())
}

/// What a network's options turn on: Shipyard's own features.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Honoured {
    /// Its containers kept from the host ([`ISOLATE_HOST`]).
    pub isolate_host: bool,
    /// Its containers' connections limited ([`CONNTRACK_LIMIT`]).
    pub conntrack_limit: bool,
}

/// Refuses a network driver Shipyard's networks aren't (the CLI's and
/// the API's create alike): any but `bridge` (none, or empty, is it).
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`].
pub fn check_driver(driver: Option<&str>) -> Result<()> {
    if let Some(driver) = driver.filter(|driver| !driver.is_empty() && *driver != "bridge") {
        bail!(
            InvalidRequest,
            "the network driver {driver} isn't supported: a network of Shipyard's is a local bridge"
        );
    }
    Ok(())
}

/// What `options` turn on: Shipyard's own (isolate the host, limit
/// connections; `true`, `1`, or `false`, `0`, empty). Docker's bridge
/// options pass at their defaults.
///
/// # Errors
///
/// An option Shipyard can't honour, at another value or unknown
/// ([`ErrorKind::InvalidRequest`]), saying so.
pub fn check_options(options: &BTreeMap<String, String>) -> Result<Honoured> {
    let flag = |key: &str, value: &str| match value {
        "true" | "1" => Ok(true),
        "false" | "0" | "" => Ok(false),
        _ => bail!(
            InvalidRequest,
            "the option {key} is true or false, not {value:?}"
        ),
    };
    let mut honoured = Honoured::default();
    for (key, value) in options {
        match key.as_str() {
            ISOLATE_HOST => honoured.isolate_host = flag(key, value)?,
            CONNTRACK_LIMIT => honoured.conntrack_limit = flag(key, value)?,
            _ => match DOCKER_DEFAULTS.iter().find(|(known, _)| known == key) {
                Some((_, wanted)) if value == wanted => {}
                Some((_, wanted)) => bail!(
                    InvalidRequest,
                    "the option {key} can only be {wanted} on Shipyard's networks, not {value:?}"
                ),
                None => bail!(
                    InvalidRequest,
                    "the option {key} isn't one Shipyard's networks honour (their bridge's \
                     name, MTU and the rest are fixed; {ISOLATE_HOST} and {CONNTRACK_LIMIT} are \
                     its own)"
                ),
            },
        }
    }
    Ok(honoured)
}

/// `POST /networks/create`: the network `request` asks for, with its
/// labels and options kept beside it. Its ID.
///
/// # Errors
///
/// An option refused, more than one subnet or gateway a family, a name or
/// subnet that can't be, `default` ([`ErrorKind::InvalidRequest`]); a name taken
/// ([`ErrorKind::Exists`]); not the host's root ([`ErrorKind::NeedsRoot`]).
pub fn create_network(engine: &Engine, request: &NetworkCreate) -> Result<String> {
    // Docker keeps `default` for the default network (a create's endpoint
    // so named is it): no network has the name, made by the CLI either
    // (the API says it first, as Docker's 403: 11.6b's clean-code review).
    if request.name == "default" {
        crate::error::bail!(
            InvalidRequest,
            "default is a pre-defined network and cannot be created"
        );
    }
    let honoured = check_options(&request.options)?;
    let spec = crate::network::spec(
        &request.name,
        &request.subnets,
        &request.gateways,
        crate::network::SpecOptions {
            ipv6: request.ipv6,
            internal: request.internal,
            isolate_host: honoured.isolate_host,
            conntrack_limit: honoured.conntrack_limit,
        },
    )?;
    let meta = NetworkMeta {
        labels: request.labels.clone(),
        options: request.options.clone(),
        ..NetworkMeta::default()
    };
    let (config, meta) = crate::network::create_with(engine, &spec, meta)?;
    Ok(network_id(&config, &meta))
}

/// `GET /networks`: the built-ins and every network, by name, without
/// their containers. Where runs get the default `bridge` (root, able to
/// make bridges), it's made first if it isn't there, as a run makes it.
/// Rootless (no networks of its own): the built-ins only. A network whose
/// labels can't be read is listed without them (and logged), never the
/// list refused.
///
/// # Errors
///
/// The store can't be read.
pub fn list_networks(engine: &Engine) -> Result<Vec<NetworkResource>> {
    let Some(networks) = store(engine)? else {
        return Ok(BUILTINS_ROOTLESS.iter().map(|name| builtin(name)).collect());
    };
    let mut listed: Vec<NetworkResource> = BUILTINS.iter().map(|name| builtin(name)).collect();
    for config in networks.list()? {
        let meta = networks.meta(&config.name).unwrap_or_else(|error| {
            tracing::warn!(
                "listing the network {} without its labels: {error}",
                config.name
            );
            NetworkMeta::default()
        });
        listed.push(resource(&config, meta, None));
    }
    listed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
}

/// The network store, the default network made first where runs get it
/// (best effort: a failure is logged, and the networks there are still
/// served); `None` rootless, or off Linux (no networks of its own). A
/// listing's ([`list_networks`]): a lookup takes the store as it is
/// ([`store_as_is`]), so a look at a name never makes a network.
///
/// # Errors
///
/// The store can't be opened.
fn store(engine: &Engine) -> Result<Option<shipyard_network::store::Networks>> {
    let Some(networks) = store_as_is(engine)? else {
        return Ok(None);
    };
    if matches!(
        crate::network::defaulted(),
        crate::network::Defaulted::Bridge
    ) && let Err(error) = crate::network::ensure_default(engine)
    {
        tracing::warn!("making the default network {DEFAULT_NETWORK}: {error}");
    }
    Ok(Some(networks))
}

/// The network store as it is: `None` rootless, or off Linux.
///
/// # Errors
///
/// The store can't be opened.
fn store_as_is(engine: &Engine) -> Result<Option<shipyard_network::store::Networks>> {
    match crate::network::networks(engine) {
        Ok(networks) => Ok(Some(networks)),
        Err(error) if matches!(error.kind(), ErrorKind::NeedsRoot | ErrorKind::Unsupported) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// `GET /networks/{id}`: the network `name_or_id` names, with its
/// containers.
///
/// # Errors
///
/// None does ([`ErrorKind::NotFound`]), or more than one does by the start
/// of its ID ([`ErrorKind::InvalidRequest`]).
pub fn inspect_network(engine: &Engine, name_or_id: &str) -> Result<NetworkResource> {
    match find_network(engine, name_or_id)? {
        Found::Builtin(name) => {
            let mut shown = builtin(name);
            shown.containers = Some(BTreeMap::new());
            Ok(shown)
        }
        Found::Network(config) => {
            let networks = crate::network::networks(engine)?;
            let meta = networks.meta(&config.name)?;
            let leases = networks.live_leases(&config.name)?;
            let id = network_id(&config, &meta);
            let containers = leases
                .into_iter()
                .map(|lease| {
                    let container = lease.container.clone().unwrap_or_else(|| lease.run.clone());
                    let shown = NetworkContainer {
                        name: lease.name.clone().unwrap_or_default(),
                        endpoint_id: endpoint_id(&id, &container),
                        mac_address: shipyard_network::store::format_mac(lease.mac),
                        ipv4_address: format!("{}/{}", lease.ip, config.subnet.prefix()),
                        ipv6_address: lease
                            .ip6
                            .zip(config.subnet6)
                            .map(|(ip, subnet)| format!("{ip}/{}", subnet.prefix()))
                            .unwrap_or_default(),
                    };
                    (container, shown)
                })
                .collect();
            Ok(resource(&config, meta, Some(containers)))
        }
    }
}

/// What a name or ID names.
#[derive(Debug)]
pub enum Found {
    /// A built-in: `host`, `none`, `pasta`.
    Builtin(&'static str),
    /// A network of the store's.
    Network(NetworkConfig),
}

impl Found {
    /// Its name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Builtin(name) => name,
            Self::Network(config) => &config.name,
        }
    }
}

/// The network `name_or_id` names, as Docker looks one up: its whole ID,
/// then its name, then the start of its ID (one network's only).
///
/// # Errors
///
/// None ([`ErrorKind::NotFound`]); rootless, one that isn't a built-in
/// ([`ErrorKind::NeedsRoot`]: bridges are root's); the start of more than
/// one network's ID ([`ErrorKind::InvalidRequest`]).
pub fn find_network(engine: &Engine, name_or_id: &str) -> Result<Found> {
    let networks = store_as_is(engine)?;
    let builtins: &[&'static str] = if networks.is_some() {
        &BUILTINS
    } else {
        &BUILTINS_ROOTLESS
    };
    let configs: Vec<(NetworkConfig, String)> = match &networks {
        Some(networks) => networks
            .list()?
            .into_iter()
            .map(|config| {
                let meta = networks.meta(&config.name).unwrap_or_default();
                let id = network_id(&config, &meta);
                (config, id)
            })
            .collect(),
        None => Vec::new(),
    };
    let named = |wanted: &dyn Fn(&str, &str) -> bool| -> Vec<Found> {
        builtins
            .iter()
            .filter(|name| wanted(name, &builtin_id(name)))
            .map(|name| Found::Builtin(name))
            .chain(
                configs
                    .iter()
                    .filter(|(config, id)| wanted(&config.name, id))
                    .map(|(config, _)| Found::Network(config.clone())),
            )
            .collect()
    };
    for found in [
        named(&|_, id| id == name_or_id),
        named(&|name, _| name == name_or_id),
    ] {
        if let Some(found) = found.into_iter().next() {
            return Ok(found);
        }
    }
    let mut by_start =
        if name_or_id.is_empty() || !name_or_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            Vec::new()
        } else {
            named(&|_, id| id.starts_with(name_or_id))
        };
    if by_start.len() > 1 {
        bail!(
            InvalidRequest,
            "network {name_or_id} is ambiguous ({} matches found based on ID prefix)",
            by_start.len()
        );
    }
    match by_start.pop() {
        Some(found) => Ok(found),
        None if networks.is_none() => bail!(NeedsRoot, "{}", crate::network::NETWORKS_NEED_ROOT),
        None => bail!(NotFound, "network {name_or_id} not found"),
    }
}

/// The built-ins listed with networks of the store's (root's).
const BUILTINS: [&str; 2] = ["host", "none"];
/// The built-ins listed rootless (no networks of its own): every
/// `--network` mode, pasta's among them.
const BUILTINS_ROOTLESS: [&str; 3] = crate::network::RESERVED;

/// The built-in network `name`, as Docker shows its own.
fn builtin(name: &str) -> NetworkResource {
    let driver = match name {
        "none" => "null",
        other => other,
    };
    NetworkResource {
        name: name.to_owned(),
        id: builtin_id(name),
        created: super::ZERO_TIME.to_owned(),
        scope: "local".to_owned(),
        driver: driver.to_owned(),
        enable_ipv4: false,
        enable_ipv6: false,
        ipam: Ipam {
            driver: "default".to_owned(),
            options: None,
            config: None,
        },
        internal: false,
        attachable: false,
        ingress: false,
        config_from: ConfigFrom::default(),
        config_only: false,
        containers: None,
        options: BTreeMap::new(),
        labels: BTreeMap::new(),
    }
}

/// The network `config`, as Docker shows it, with `meta` and the
/// `containers` given (an inspect's).
fn resource(
    config: &NetworkConfig,
    meta: NetworkMeta,
    containers: Option<BTreeMap<String, NetworkContainer>>,
) -> NetworkResource {
    let mut subnets = vec![IpamConfig {
        subnet: config.subnet.to_string(),
        gateway: config.gateway.to_string(),
    }];
    if let (Some(subnet), Some(gateway)) = (config.subnet6, config.gateway6) {
        subnets.push(IpamConfig {
            subnet: subnet.to_string(),
            gateway: gateway.to_string(),
        });
    }
    let id = network_id(config, &meta);
    let mut options = meta.options;
    if config.name == DEFAULT_NETWORK {
        options.insert(
            "com.docker.network.bridge.default_bridge".to_owned(),
            "true".to_owned(),
        );
    }
    if config.isolate_host {
        options.insert(ISOLATE_HOST.to_owned(), "true".to_owned());
    }
    if config.conntrack_limit {
        options.insert(CONNTRACK_LIMIT.to_owned(), "true".to_owned());
    }
    let created = std::time::UNIX_EPOCH + std::time::Duration::from_secs(config.created);
    NetworkResource {
        name: config.name.clone(),
        id,
        created: super::time(created),
        scope: "local".to_owned(),
        driver: "bridge".to_owned(),
        enable_ipv4: true,
        enable_ipv6: config.subnet6.is_some(),
        ipam: Ipam {
            driver: "default".to_owned(),
            options: None,
            config: Some(subnets),
        },
        internal: config.internal,
        attachable: false,
        ingress: false,
        config_from: ConfigFrom::default(),
        config_only: false,
        containers,
        options,
        labels: meta.labels,
    }
}

/// `GET /networks` and `network ls`: the networks `filters` pass, by name.
///
/// # Errors
///
/// As [`list_networks`].
pub fn list_networks_where(
    engine: &Engine,
    filters: &NetworkFilters,
) -> Result<Vec<NetworkResource>> {
    let mut networks = list_networks(engine)?;
    // Read only if asked (every container's record and state).
    let in_use = if filters.wants_in_use() {
        in_use(engine)?
    } else {
        std::collections::BTreeSet::new()
    };
    networks.retain(|network| filters.pass(network, in_use.contains(&network.name)));
    Ok(networks)
}

/// The networks a container that's up joins: none where there are no
/// networks of Shipyard's (off Linux).
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        clippy::unnecessary_wraps,
        reason = "it fails only where there are networks: Linux"
    )
)]
fn in_use(engine: &Engine) -> Result<std::collections::BTreeSet<String>> {
    #[cfg(target_os = "linux")]
    return crate::network::networks_in_use(engine);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        Ok(std::collections::BTreeSet::new())
    }
}

#[cfg(test)]
mod tests;
