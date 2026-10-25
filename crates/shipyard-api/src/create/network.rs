//! A create body's network: `HostConfig.NetworkMode` and the endpoints its
//! `NetworkingConfig` names, as the run's `--network` and their settings.

use shipyard_engine::network::{Joined, NetworkMode};

use crate::errors::ApiError;

use super::body::{CreateBody, EndpointSettings, HostConfig};
use super::{field, parsed_all};

/// The networks: `NetworkMode` (`default` or none: the engine's default),
/// with its settings from `NetworkingConfig`'s one endpoint (API 1.43
/// takes one at create, as Docker's does).
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
    let mut endpoints: Vec<(&String, &EndpointSettings)> =
        endpoints.into_iter().flatten().collect();
    if endpoints.len() > 1 {
        return Err(field(
            "NetworkingConfig",
            "a container joins one network at create: connect it to the others after",
        ));
    }
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
            Some((name, _)) if name.as_str() != "default" => name.as_str(),
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
    shipyard_network::store::validate_name(name)
        .map_err(|why| field("NetworkMode", &why.to_string()))?;
    let settings = match endpoints.pop() {
        Some((endpoint, settings)) if endpoint == name => Some(settings),
        Some((endpoint, _)) => {
            return Err(field(
                "NetworkingConfig",
                &format!("its endpoint {endpoint:?} isn't the NetworkMode's network {name:?}"),
            ));
        }
        None => None,
    };
    let mut joined = Joined {
        name: name.to_owned(),
        ..Joined::default()
    };
    if let Some(settings) = settings {
        endpoint(settings, &mut joined)?;
    }
    if joined.mac.is_none()
        && let Some(mac) = body.mac_address.as_deref().filter(|mac| !mac.is_empty())
    {
        joined.mac =
            Some(shipyard_network::store::parse_mac(mac).map_err(|why| field("MacAddress", &why))?);
    }
    Ok(vec![NetworkMode::Bridges(vec![joined])])
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
