//! Volumes (step 11.2c2a): `GET /volumes` lists, `POST /volumes/create`
//! makes one (with its labels, kept in its `labels.json`), `GET
//! /volumes/{name}` inspects, `DELETE /volumes/{name}` removes, `POST
//! /volumes/prune` removes the unused (only anonymous ones unless `all`, as
//! Docker's API since 1.42). Every volume is the `local` driver's: a
//! directory of the store's.

use std::collections::BTreeMap;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::Shared;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body};
use shipyard_engine::docker::filters::{PruneFilters, VolumeFilters};

type Reply = Result<Response<Body>, ApiError>;

/// `POST /volumes/create`'s body (Docker's `VolumeCreateOptions`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct CreateBody {
    name: Option<String>,
    driver: Option<String>,
    driver_opts: Option<BTreeMap<String, String>>,
    labels: Option<BTreeMap<String, String>>,
}

/// `GET /volumes` (`filters`: `name`, `label`, `dangling`, `driver`).
///
/// # Errors
///
/// A filter that can't be read (400); the store's errors.
pub(crate) async fn list(shared: &Arc<Shared>, query: &Query) -> Reply {
    let filters = VolumeFilters::parse(crate::filters::given(query.get("filters"))?)?;
    let (listed, _) = shared
        .call(move |engine| shipyard_engine::docker::list_volumes_where(engine, &filters))
        .await?;
    Ok(reply::json(StatusCode::OK, &listed))
}

/// `POST /volumes/create` (`Name`, none for a new 64-hex one; `Driver`,
/// `local` or none; `Labels`): 201 and the volume, an existing one as it
/// is. Driver options are refused: what they'd mount (NFS, a device)
/// Shipyard doesn't.
///
/// # Errors
///
/// A body that isn't Docker's (400), another driver or driver options
/// (400), a name that isn't one (400); the store's errors.
pub(crate) async fn create(shared: &Arc<Shared>, body: Incoming) -> Reply {
    let body: CreateBody = crate::read_json(body).await?;
    shipyard_engine::volumes::check_driver(
        body.driver.as_deref(),
        body.driver_opts
            .as_ref()
            .is_some_and(|options| !options.is_empty()),
    )?;
    let labels = body.labels.unwrap_or_default();
    let name = body.name;
    let (volume, _) = shared
        .call(move |engine| {
            let made = shipyard_engine::volumes::create_with(engine, name.as_deref(), &labels)?;
            shipyard_engine::docker::inspect_volume(engine, &made.name)
        })
        .await?;
    Ok(reply::json(StatusCode::CREATED, &volume))
}

/// `GET /volumes/{name}`: 404 `get NAME: no such volume`, Docker's words.
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn inspect(shared: &Arc<Shared>, name: String) -> Reply {
    let shown = name.clone();
    let (volume, _) = shared
        .call(move |engine| shipyard_engine::docker::inspect_volume(engine, &name))
        .await
        .map_err(|error| or_no_such(error, &shown))?;
    Ok(reply::json(StatusCode::OK, &volume))
}

/// `DELETE /volumes/{name}` (`force`: one that isn't there is no error):
/// 204; in use, 409.
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn remove(shared: &Arc<Shared>, name: String, query: &Query) -> Reply {
    let force = query.flag("force");
    let shown = name.clone();
    let removed = shared
        .call(move |engine| shipyard_engine::volumes::remove(engine, &name))
        .await;
    match removed {
        Ok(_) => Ok(reply::empty(StatusCode::NO_CONTENT)),
        Err(error) if error.status == StatusCode::NOT_FOUND && force => {
            Ok(reply::empty(StatusCode::NO_CONTENT))
        }
        Err(error) => Err(or_no_such(error, &shown)),
    }
}

/// `POST /volumes/prune` (`filters`: `all`, `label`, `label!`): what's
/// unused and wanted removed; Docker's `VolumesDeleted` and
/// `SpaceReclaimed`. A client of an API older than 1.42 (`version`) asks
/// for every unused volume, as Docker's daemon reads it.
///
/// # Errors
///
/// A filter that can't be read (400); the store's errors.
pub(crate) async fn prune(shared: &Arc<Shared>, query: &Query, version: (u32, u32)) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Pruned {
        volumes_deleted: Vec<String>,
        space_reclaimed: u64,
    }
    let mut filters = PruneFilters::parse(crate::filters::given(query.get("filters"))?)?;
    if version < ALL_BY_DEFAULT_BEFORE {
        filters.all = true;
    }
    // Its warnings aren't Docker's answer's: what failed is logged.
    let (report, _) = shared
        .call(move |engine| {
            shipyard_engine::volumes::prune_where(engine, &|_, labels| filters.wants(labels))
        })
        .await?;
    for failed in &report.failed {
        tracing::warn!("a volume prune couldn't remove: {failed}");
    }
    Ok(reply::json(
        StatusCode::OK,
        &Pruned {
            volumes_deleted: report.removed,
            space_reclaimed: report.bytes,
        },
    ))
}

/// The API version from which a prune takes only anonymous volumes
/// without `all` (Docker 23's).
const ALL_BY_DEFAULT_BEFORE: (u32, u32) = (1, 42);

/// `error`, as Docker words it if it's the volume `name` not found.
fn or_no_such(error: ApiError, name: &str) -> ApiError {
    if error.status == StatusCode::NOT_FOUND {
        ApiError::new(StatusCode::NOT_FOUND, format!("get {name}: no such volume"))
    } else {
        error
    }
}
