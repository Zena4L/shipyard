//! Networks (step 11.2c2b): `GET /networks` lists (the built-ins too:
//! D11.2c2-3), `GET /networks/{id}` inspects, `POST /networks/create`
//! makes one (its labels and options kept beside it; options honoured or
//! refused: D11.2c2b-1), `DELETE /networks/{id}` removes, `POST
//! /networks/{id}/connect` and `…/disconnect` join and leave a container,
//! `POST /networks/prune` removes the unused. A network is named by its
//! name, its ID, or the start of its ID, as Docker's are.
//!
//! Bridge networks are root's: rootless, the built-ins are listed and
//! inspected, and anything that would change a network is the engine's 403
//! (it needs root).

use std::collections::BTreeMap;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use shipyard_engine::docker::{Found, NetworkCreate};

use crate::Shared;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body};
use shipyard_engine::docker::filters::NetworkFilters;

type Reply = Result<Response<Body>, ApiError>;

/// `POST /networks/create`'s body (Docker's `NetworkCreateRequest`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct CreateBody {
    name: String,
    driver: Option<String>,
    scope: Option<String>,
    internal: Option<bool>,
    attachable: Option<bool>,
    ingress: Option<bool>,
    config_only: Option<bool>,
    config_from: Option<ConfigFromBody>,
    #[serde(rename = "IPAM")]
    ipam: Option<IpamBody>,
    #[serde(rename = "EnableIPv4")]
    enable_ipv4: Option<bool>,
    #[serde(rename = "EnableIPv6")]
    enable_ipv6: Option<bool>,
    options: Option<BTreeMap<String, String>>,
    labels: Option<BTreeMap<String, String>>,
}

/// Docker's `ConfigReference`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ConfigFromBody {
    network: String,
}

/// Docker's `IPAM`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct IpamBody {
    driver: Option<String>,
    config: Option<Vec<IpamConfigBody>>,
    options: Option<BTreeMap<String, String>>,
}

/// One of Docker's `IPAMConfig`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct IpamConfigBody {
    subnet: Option<String>,
    #[serde(rename = "IPRange")]
    ip_range: Option<String>,
    gateway: Option<String>,
    auxiliary_addresses: Option<BTreeMap<String, String>>,
}

/// `…/connect`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ConnectBody {
    container: String,
    endpoint_config: Option<EndpointBody>,
}

/// Docker's `EndpointSettings`, as a connect gives it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct EndpointBody {
    aliases: Option<Vec<String>>,
    #[serde(rename = "IPAMConfig")]
    ipam_config: Option<EndpointIpamBody>,
    links: Option<Vec<String>>,
    mac_address: Option<String>,
    driver_opts: Option<BTreeMap<String, String>>,
}

/// Docker's `EndpointIPAMConfig`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EndpointIpamBody {
    #[serde(rename = "IPv4Address")]
    ipv4: Option<String>,
    #[serde(rename = "IPv6Address")]
    ipv6: Option<String>,
}

/// `…/disconnect`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct DisconnectBody {
    container: String,
    force: bool,
}

/// `GET /networks` (`filters`: `name`, `id`, `label`, `driver`, `type`).
///
/// # Errors
///
/// A filter that can't be read (400); the store's errors.
pub(crate) async fn list(shared: &Arc<Shared>, query: &Query) -> Reply {
    let filters = NetworkFilters::parse(crate::filters::given(query.get("filters"))?)?;
    let (listed, _) = shared
        .call(move |engine| shipyard_engine::docker::list_networks_where(engine, &filters))
        .await?;
    Ok(reply::json(StatusCode::OK, &listed))
}

/// `GET /networks/{id}`: 404 `network NAME not found`, Docker's words.
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn inspect(shared: &Arc<Shared>, name: String) -> Reply {
    let (network, _) = shared
        .call(move |engine| shipyard_engine::docker::inspect_network(engine, &name))
        .await?;
    Ok(reply::json(StatusCode::OK, &network))
}

/// `POST /networks/create`: 201 `{"Id", "Warning"}`. What Shipyard's
/// networks can't be is refused (400): another driver or scope, an
/// attachable, ingress or config-only network, another IPAM driver or its
/// options, an IP range, auxiliary addresses, no IPv4.
///
/// # Errors
///
/// As above; a name taken (409); not root (403).
pub(crate) async fn create(shared: &Arc<Shared>, body: Incoming) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Created {
        id: String,
        warning: String,
    }
    let body: CreateBody = crate::read_json(body).await?;
    let request = network_create(body)?;
    let (id, _) = shared
        .call(move |engine| {
            shipyard_engine::docker::create_network(engine, &request).map_err(|error| {
                // Docker's words for a name taken.
                if error.kind() == shipyard_engine::ErrorKind::Exists {
                    shipyard_engine::Error::new(
                        shipyard_engine::ErrorKind::Exists,
                        format!("network with name {} already exists", request.name),
                    )
                } else {
                    error
                }
            })
        })
        .await?;
    Ok(reply::json(
        StatusCode::CREATED,
        &Created {
            id,
            warning: String::new(),
        },
    ))
}

/// Docker's create body as the engine's request, what Shipyard can't make
/// refused.
///
/// # Errors
///
/// What's refused, and subnets or gateways that aren't addresses (400).
pub(crate) fn network_create(body: CreateBody) -> Result<NetworkCreate, ApiError> {
    let refuse = |what: &str| {
        Err(ApiError::bad(format!(
            "{what} isn't supported: a network of Shipyard's is a local bridge"
        )))
    };
    let set = |text: &Option<String>, default: &str| {
        text.as_deref()
            .is_some_and(|text| !text.is_empty() && text != default)
    };
    // Docker keeps `default` for the default network: a container's
    // create takes an endpoint so named as it, so no network has the name.
    if body.name == "default" {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!(
                "{} is a pre-defined network and cannot be created",
                body.name
            ),
        ));
    }
    shipyard_engine::docker::check_network_driver(body.driver.as_deref())?;
    if set(&body.scope, "local") {
        return refuse("a scope other than local");
    }
    if body.attachable == Some(true) || body.ingress == Some(true) || body.config_only == Some(true)
    {
        return refuse("an attachable, ingress or config-only network");
    }
    if body
        .config_from
        .as_ref()
        .is_some_and(|from| !from.network.is_empty())
    {
        return refuse("a network made from another's config");
    }
    if body.enable_ipv4 == Some(false) {
        return refuse("a network without IPv4");
    }
    let ipam = body.ipam.unwrap_or_default();
    if set(&ipam.driver, "default") {
        return refuse("an IPAM driver other than default");
    }
    if ipam
        .options
        .as_ref()
        .is_some_and(|options| !options.is_empty())
    {
        return refuse("IPAM options");
    }
    let mut subnets = Vec::new();
    let mut gateways = Vec::new();
    for config in ipam.config.unwrap_or_default() {
        if config
            .ip_range
            .as_deref()
            .is_some_and(|range| !range.is_empty())
        {
            return refuse("an IP range");
        }
        if config
            .auxiliary_addresses
            .as_ref()
            .is_some_and(|addresses| !addresses.is_empty())
        {
            return refuse("auxiliary addresses");
        }
        if let Some(subnet) = config.subnet.filter(|subnet| !subnet.is_empty()) {
            subnets.push(
                subnet
                    .parse()
                    .map_err(|_| ApiError::bad(format!("invalid subnet {subnet:?}")))?,
            );
        }
        if let Some(gateway) = config.gateway.filter(|gateway| !gateway.is_empty()) {
            gateways.push(
                gateway
                    .parse()
                    .map_err(|_| ApiError::bad(format!("invalid gateway {gateway:?}")))?,
            );
        }
    }
    Ok(NetworkCreate {
        name: body.name,
        internal: body.internal.unwrap_or(false),
        ipv6: body.enable_ipv6.unwrap_or(false),
        subnets,
        gateways,
        labels: body.labels.unwrap_or_default(),
        options: body.options.unwrap_or_default(),
    })
}

/// `DELETE /networks/{id}`: 204. A pre-defined one (the default `bridge`,
/// a built-in) is refused (403, Docker's words), and so is one a container
/// uses (409).
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn remove(shared: &Arc<Shared>, name: String) -> Reply {
    let name = custom(shared, name, "removed").await?;
    shared
        .call(move |engine| shipyard_engine::network::remove(engine, &name))
        .await?;
    Ok(reply::empty(StatusCode::NO_CONTENT))
}

/// `POST /networks/{id}/connect` (`Container`, `EndpointConfig`: its
/// `Aliases`, `IPAMConfig`'s addresses; `Links`, `MacAddress` and
/// `DriverOpts` refused): 200.
///
/// # Errors
///
/// As the engine's; what's refused (400).
pub(crate) async fn connect(shared: &Arc<Shared>, name: String, body: Incoming) -> Reply {
    let body: ConnectBody = crate::read_json(body).await?;
    let endpoint = body.endpoint_config.unwrap_or_default();
    if endpoint
        .links
        .as_ref()
        .is_some_and(|links| !links.is_empty())
    {
        return Err(ApiError::bad(
            "links aren't supported: containers on one network find each other by name",
        ));
    }
    if endpoint
        .mac_address
        .as_deref()
        .is_some_and(|mac| !mac.is_empty())
    {
        return Err(ApiError::bad(
            "a MAC address isn't supported: it's derived from the container's address",
        ));
    }
    if endpoint
        .driver_opts
        .as_ref()
        .is_some_and(|options| !options.is_empty())
    {
        return Err(ApiError::bad("endpoint driver options aren't supported"));
    }
    let addresses = endpoint.ipam_config.unwrap_or_default();
    let ip = parsed(addresses.ipv4, "IPv4Address")?;
    let ip6 = parsed(addresses.ipv6, "IPv6Address")?;
    let aliases = endpoint.aliases.unwrap_or_default();
    let name = joinable(shared, name).await?;
    shared
        .call(move |engine| {
            let mut request = shipyard_engine::run::ConnectRequest::new(&name, &body.container);
            request.ip = ip;
            request.ip6 = ip6;
            request.aliases = aliases;
            shipyard_engine::run::connect(engine, &request)
        })
        .await?;
    Ok(reply::empty(StatusCode::OK))
}

/// `POST /networks/{id}/disconnect` (`Container`, `Force`): 200.
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn disconnect(shared: &Arc<Shared>, name: String, body: Incoming) -> Reply {
    let body: DisconnectBody = crate::read_json(body).await?;
    let name = joinable(shared, name).await?;
    shared
        .call(move |engine| {
            shipyard_engine::run::disconnect(engine, &name, &body.container, body.force)
        })
        .await?;
    Ok(reply::empty(StatusCode::OK))
}

/// `POST /networks/prune` (`filters`: `until`, `label`, `label!`): every
/// network no container uses but the default one, Docker's
/// `NetworksDeleted` (`null` for none). Rootless: none to prune (`null`):
/// there are no networks of Shipyard's, and Docker's `system prune` asks.
///
/// # Errors
///
/// A filter that can't be read (400).
pub(crate) async fn prune(shared: &Arc<Shared>, query: &Query) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Pruned {
        networks_deleted: Option<Vec<String>>,
    }
    let filters = shipyard_engine::docker::filters::ContainerPruneFilters::parse(
        crate::filters::given(query.get("filters"))?,
        shipyard_engine::docker::unix_now(),
    )?;
    let (report, _) = shared
        .call(move |engine| {
            shipyard_engine::network::prune_where_any(engine, &|labels, created| {
                filters.wants_made(created, labels)
            })
        })
        .await?;
    let removed = report.map(|report| {
        // What failed isn't Docker's answer's: logged.
        for failed in &report.failed {
            tracing::warn!("a network prune couldn't remove: {failed}");
        }
        report.removed
    });
    Ok(reply::json(
        StatusCode::OK,
        &Pruned {
            networks_deleted: removed.filter(|removed| !removed.is_empty()),
        },
    ))
}

/// The name of the network `name_or_id` names, if it isn't a pre-defined
/// one (the default `bridge`, a built-in): those are refused as `what`
/// (Docker's 403).
///
/// # Errors
///
/// A pre-defined one (403); as [`shipyard_engine::docker::find_network`].
async fn custom(shared: &Arc<Shared>, name_or_id: String, what: &str) -> Result<String, ApiError> {
    let (found, _) = shared
        .call(move |engine| shipyard_engine::docker::find_network(engine, &name_or_id))
        .await?;
    let name = found.name().to_owned();
    if shipyard_engine::docker::is_predefined(&name) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("{name} is a pre-defined network and cannot be {what}"),
        ));
    }
    Ok(name)
}

/// The name of the network `name_or_id` names, for a container to join or
/// leave: any but a built-in (`host`, `none`, `pasta`: a container has one
/// of those only from its making, Docker's 403).
///
/// # Errors
///
/// A built-in (403); as [`shipyard_engine::docker::find_network`].
async fn joinable(shared: &Arc<Shared>, name_or_id: String) -> Result<String, ApiError> {
    let (found, _) = shared
        .call(move |engine| shipyard_engine::docker::find_network(engine, &name_or_id))
        .await?;
    match found {
        Found::Builtin(name) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("{name} is a pre-defined network: a container is on it only from its making"),
        )),
        Found::Network(config) => Ok(config.name),
    }
}

/// `text` as an address, if given.
fn parsed<T: std::str::FromStr>(text: Option<String>, what: &str) -> Result<Option<T>, ApiError> {
    match text.filter(|text| !text.is_empty()) {
        None => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| ApiError::bad(format!("invalid {what} {text:?}"))),
    }
}

#[cfg(test)]
mod tests;
