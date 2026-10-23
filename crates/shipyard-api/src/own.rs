//! Shipyard's own views (step 11.2c3b): what `shipyard inspect --shipyard`
//! and `shipyard network inspect` show, under `/shipyard/…`, for a client
//! that wants what Docker's shapes leave out (a container's serial, its
//! stored config, its network lost; a network's bridge, its DNS server,
//! its isolation).

use std::sync::Arc;

use hyper::{Response, StatusCode};

use crate::Shared;
use crate::errors::ApiError;
use crate::reply::{self, Body};

type Reply = Result<Response<Body>, ApiError>;

/// `GET /shipyard/containers/{id}`: Shipyard's view of a container.
///
/// # Errors
///
/// No such container (404); as the engine's.
pub(crate) async fn container(shared: &Arc<Shared>, reference: String) -> Reply {
    let (view, _) = shared
        .call(move |engine| shipyard_engine::containers::inspect(engine, &reference))
        .await?;
    Ok(reply::json(StatusCode::OK, &view))
}

/// `GET /shipyard/networks/{name}`: Shipyard's view of a network, with the
/// containers on it. Rootless: 403 (bridges are root's).
///
/// # Errors
///
/// No such network (404); as the engine's.
pub(crate) async fn network(shared: &Arc<Shared>, name: String) -> Reply {
    let (mut views, _) = shared
        .call(move |engine| shipyard_engine::network::inspect(engine, &[name]))
        .await?;
    let view = views
        .pop()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such network"))?;
    Ok(reply::json(StatusCode::OK, &view))
}
