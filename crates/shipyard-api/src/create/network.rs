//! A create body's network: `HostConfig.NetworkMode` and the endpoints its
//! `NetworkingConfig` names, as the run's `--network` and their settings.

use shipyard_engine::network::{Joined, NetworkMode};

use crate::errors::ApiError;

use super::body::{CreateBody, EndpointSettings, HostConfig};
use super::{field, parsed_all};

/// The networks: `NetworkMode` (`default` or none: the engine's default),
/// with its settings from `NetworkingConfig`'s endpoint of that name, then
/// every other endpoint (API 1.44: several at create, as `docker run
/// --network a --network b` sends them), in name order, each with its own
/// settings: the first is the container's `eth0`.
pub(super) fn the_network(
    body: &CreateBody,
    host: &HostConfig,
) -> Result<Vec<NetworkMode>, ApiError> {
    if body.network_disabled == Some(true) {
        return Ok(vec![NetworkMode::None]);
    }
    let endpoints = body
        .networking_config
        .as_ref()
        .and_then(|config| config.endpoints_config.as_ref());
    let endpoints: Vec<(&String, &EndpointSettings)> = endpoints.into_iter().flatten().collect();
    let mode = host.network_mode.as_deref().unwrap_or_default();
    let name = match mode {
        "host" => return only_mode(NetworkMode::Host, &endpoints),
        "none" => return only_mode(NetworkMode::None, &endpoints),
        "pasta" => return only_mode(NetworkMode::Pasta, &endpoints),
        // The endpoint names the network (`--network NAME`, an older
        // client's way); one named `default` (Docker's CLI since API 1.44
        // sends the mode's own name) is the default network, a name Docker
        // keeps for it.
        "" | "default" => match endpoints.first() {
            // Several: the first, in name order, is the container's first.
            Some((name, _)) if endpoints.len() > 1 || name.as_str() != "default" => name.as_str(),
            // The default network takes none of an endpoint's settings
            // (Docker's: `--ip`, an alias, a hardware address need a
            // network of the user's): refused, never dropped.
            Some((_, settings)) if has_settings(settings)? => {
                return Err(field(
                    "NetworkingConfig",
                    "an address, alias or hardware address needs a network of your own \
                     (shipyard network create, then --network)",
                ));
            }
            _ if body
                .mac_address
                .as_deref()
                .is_some_and(|mac| !mac.is_empty()) =>
            {
                return Err(field(
                    "MacAddress",
                    "a hardware address needs a bridge network",
                ));
            }
            _ => return Ok(Vec::new()),
        },
        // Another container's (compose's `network_mode: service:`), as it
        // is: what can't go with it is the engine's to refuse.
        container if let Some(parsed) = NetworkMode::parse_container(container) => {
            if body
                .mac_address
                .as_deref()
                .is_some_and(|mac| !mac.is_empty())
            {
                return Err(field(
                    "MacAddress",
                    "a container that joins another's network has that one's addresses",
                ));
            }
            return match parsed {
                Ok(mode) => only_mode(mode, &endpoints),
                Err(why) => Err(field("NetworkMode", why)),
            };
        }
        name => name,
    };
    Ok(vec![NetworkMode::Bridges(bridges(
        name, mode, &endpoints, body,
    )?)])
}

/// The bridge networks joined: `first`, then every other endpoint's, each
/// with its endpoint's settings; `mode` is what `NetworkMode` said, which a
/// bad name is blamed on when it's that one.
fn bridges(
    first: &str,
    mode: &str,
    endpoints: &[(&String, &EndpointSettings)],
    body: &CreateBody,
) -> Result<Vec<Joined>, ApiError> {
    let names = std::iter::once(first).chain(
        endpoints
            .iter()
            .map(|(endpoint, _)| endpoint.as_str())
            .filter(|endpoint| *endpoint != first),
    );
    let mut networks = Vec::new();
    for network in names {
        if network == "default" {
            // The engine's default network is joined alone, as `default`
            // names nothing else's (Docker's too: `docker run --network
            // default --network app` is refused).
            return Err(field(
                "NetworkingConfig",
                "the default network can't be joined with others: name the networks",
            ));
        }
        shipyard_network::store::validate_name(network).map_err(|why| {
            let given = if network == mode {
                "NetworkMode"
            } else {
                "NetworkingConfig"
            };
            field(given, &why.to_string())
        })?;
        let mut joined = Joined {
            name: network.to_owned(),
            ..Joined::default()
        };
        if let Some((_, settings)) = endpoints.iter().find(|(endpoint, _)| *endpoint == network) {
            endpoint(settings, &mut joined)?;
        }
        networks.push(joined);
    }
    // The container-wide hardware address (deprecated in API 1.44): the
    // first network's; another there refused, as Docker refuses it.
    if let Some(primary) = networks.first_mut()
        && let Some(mac) = body.mac_address.as_deref().filter(|mac| !mac.is_empty())
    {
        let mac =
            shipyard_network::store::parse_mac(mac).map_err(|why| field("MacAddress", &why))?;
        if primary.mac.is_some_and(|own| own != mac) {
            return Err(field(
                "MacAddress",
                "the first network's endpoint names another hardware address",
            ));
        }
        primary.mac = Some(mac);
    }
    Ok(networks)
}

/// A network mode that takes no endpoint settings.
fn only_mode(
    mode: NetworkMode,
    endpoints: &[(&String, &EndpointSettings)],
) -> Result<Vec<NetworkMode>, ApiError> {
    if let Some((name, _)) = endpoints.first() {
        return Err(field(
            "NetworkingConfig",
            &format!("the endpoint {name:?} needs a bridge network: this mode takes none"),
        ));
    }
    Ok(vec![mode])
}

/// Whether `settings` set anything an endpoint takes (an address, a
/// hardware address, an alias).
fn has_settings(settings: &EndpointSettings) -> Result<bool, ApiError> {
    let mut joined = Joined::default();
    endpoint(settings, &mut joined)?;
    Ok(joined.ip.is_some()
        || joined.ip6.is_some()
        || joined.mac.is_some()
        || !joined.aliases.is_empty())
}

/// An endpoint's settings: its addresses, hardware address and aliases.
fn endpoint(settings: &EndpointSettings, joined: &mut Joined) -> Result<(), ApiError> {
    let ipam = settings.ipam_config.clone().unwrap_or_default();
    if let Some(ip) = ipam.ipv4_address.filter(|ip| !ip.is_empty()) {
        joined.ip = Some(
            ip.parse()
                .map_err(|_| field("IPv4Address", &format!("{ip:?} isn't an IP address")))?,
        );
    }
    if let Some(ip) = ipam.ipv6_address.filter(|ip| !ip.is_empty()) {
        joined.ip6 = Some(
            ip.parse()
                .map_err(|_| field("IPv6Address", &format!("{ip:?} isn't an IPv6 address")))?,
        );
    }
    if let Some(mac) = settings
        .mac_address
        .as_deref()
        .filter(|mac| !mac.is_empty())
    {
        joined.mac =
            Some(shipyard_network::store::parse_mac(mac).map_err(|why| field("MacAddress", &why))?);
    }
    joined.aliases = parsed_all(
        "Aliases",
        settings.aliases.as_ref(),
        shipyard_network::dns::parse_name,
    )?;
    Ok(())
}
