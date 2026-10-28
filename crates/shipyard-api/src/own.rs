//! Shipyard's own views (step 11.2c3b): what `shipyard inspect --shipyard`
//! and `shipyard network inspect` show, under `/shipyard/…`, for a client
//! that wants what Docker's shapes leave out (a container's serial, its
//! stored config, its network lost; a network's bridge, its DNS server,
//! its isolation), and what the desktop UI's map is drawn from (step 14.1a:
//! a container's sockets, every container's links), and its inspector and
//! Volumes section (step 14.1b: a container's isolation, every volume's size
//! and files); a network's view says its version (step 14.6b); what
//! everything uses now, the UI's live numbers (step 14.8a2).

use std::sync::Arc;

use hyper::{Response, StatusCode};

use crate::Shared;
use crate::errors::ApiError;
use crate::reply::{self, Body};
use crate::routes::Own;

type Reply = Result<Response<Body>, ApiError>;

/// What `asked` asks of Shipyard's own views.
///
/// # Errors
///
/// As each one's.
pub(crate) async fn answer(shared: &Arc<Shared>, asked: Own) -> Reply {
    match asked {
        Own::Container(id) => container(shared, id).await,
        Own::Network(name) => network(shared, name).await,
        Own::Sockets(id) => sockets(shared, id).await,
        Own::Links => links(shared).await,
        Own::Isolation(id) => isolation(shared, id).await,
        Own::Volumes => volumes(shared).await,
        Own::Usage => usage(shared).await,
    }
}

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

/// The version of `/shipyard/networks/{name}`'s answer (step 14.6b: the
/// engine's view with this beside it, so `network inspect --shipyard`'s own
/// output is unchanged).
pub(crate) const NETWORK_VERSION: u32 = 1;

/// A view with its answer's version beside its own keys.
#[derive(serde::Serialize)]
struct Versioned<'a, T> {
    version: u32,
    #[serde(flatten)]
    view: &'a T,
}

/// `GET /shipyard/networks/{name}`: Shipyard's view of a network, with the
/// containers on it, its version beside it. Rootless: 403 (bridges are
/// root's).
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
    Ok(reply::json(
        StatusCode::OK,
        &Versioned {
            version: NETWORK_VERSION,
            view: &view,
        },
    ))
}

/// `GET /shipyard/containers/{id}/sockets`: the container's sockets, each
/// peer named where it's a container (`port --sockets`'s view, with its
/// version).
///
/// # Errors
///
/// No such container (404), not running (409); as the engine's.
pub(crate) async fn sockets(shared: &Arc<Shared>, reference: String) -> Reply {
    let (found, _) = shared
        .call(move |engine| shipyard_engine::containers::sockets(engine, &reference))
        .await?;
    Ok(reply::json(StatusCode::OK, &found))
}

/// The latest answer of a read shared among its askers, and when it was
/// read.
pub(crate) type Latest = tokio::sync::Mutex<Option<(std::time::Instant, Arc<serde_json::Value>)>>;

/// How long a links answer is shared with the pollers that ask after it.
const LINKS_SHARED_FOR: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a volumes answer is shared (each read walks every volume).
const VOLUMES_SHARED_FOR: std::time::Duration = std::time::Duration::from_secs(5);

/// `GET /shipyard/links`: every container, its links (declared and
/// observed) and its published ports. Shared for [`LINKS_SHARED_FOR`]
/// (each read costs every container's socket tables): see [`shared_read`].
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn links(shared: &Arc<Shared>) -> Reply {
    shared_read(
        shared,
        |shared| &shared.links,
        LINKS_SHARED_FOR,
        "the links",
        shipyard_engine::containers::links::links,
    )
    .await
}

/// `GET /shipyard/volumes`: every volume's size and top level, each walk
/// bounded, the whole read within 3 s (`volumes::contents`). Shared for [`VOLUMES_SHARED_FOR`]: see
/// [`shared_read`].
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn volumes(shared: &Arc<Shared>) -> Reply {
    shared_read(
        shared,
        |shared| &shared.volumes,
        VOLUMES_SHARED_FOR,
        "the volumes",
        shipyard_engine::volumes::contents,
    )
    .await
}

/// How long a usage answer is shared (the UI polls it every 2 s).
const USAGE_SHARED_FOR: std::time::Duration = std::time::Duration::from_secs(1);

/// `GET /shipyard/usage`: the host's memory and CPU time, each running
/// container's CPU time and memory, the monitors' memory: counters a
/// reader divides between two reads. Shared for [`USAGE_SHARED_FOR`]: see
/// [`shared_read`].
///
/// # Errors
///
/// As the engine's.
pub(crate) async fn usage(shared: &Arc<Shared>) -> Reply {
    shared_read(
        shared,
        |shared| &shared.usage,
        USAGE_SHARED_FOR,
        "the usage",
        shipyard_engine::containers::usage::usage,
    )
    .await
}

/// `GET /shipyard/containers/{id}/isolation`: a running container's
/// namespaces, cgroup, command and user mapping, as the kernel has them.
///
/// # Errors
///
/// No such container (404), not running (409); as the engine's.
pub(crate) async fn isolation(shared: &Arc<Shared>, reference: String) -> Reply {
    let (found, _) = shared
        .call(move |engine| shipyard_engine::containers::isolation::isolation(engine, &reference))
        .await?;
    Ok(reply::json(StatusCode::OK, &found))
}

/// A read one at a time, its answer kept in `latest`: an asker that comes
/// while one is under way, or within `shared_for` of the last, gets that
/// one's. The read and its keeping run in a task of their own: an asker
/// that hangs up never cancels them, so the next one finds the answer.
async fn shared_read<T: serde::Serialize + Send + 'static>(
    shared: &Arc<Shared>,
    latest: fn(&Shared) -> &Latest,
    shared_for: std::time::Duration,
    what: &'static str,
    read: fn(&shipyard_engine::Engine) -> shipyard_engine::Result<T>,
) -> Reply {
    let shared = Arc::clone(shared);
    let answer = tokio::spawn(async move {
        let mut kept = latest(&shared).lock().await;
        if let Some((at, answer)) = kept.as_ref()
            && at.elapsed() < shared_for
        {
            return Ok(Arc::clone(answer));
        }
        let (found, _) = shared.call(read).await?;
        let answer = Arc::new(
            serde_json::to_value(&found).map_err(|error| ApiError::internal(error.to_string()))?,
        );
        *kept = Some((std::time::Instant::now(), Arc::clone(&answer)));
        Ok::<_, ApiError>(answer)
    })
    .await
    .map_err(|error| ApiError::internal(format!("reading {what}: {error}")))??;
    Ok(reply::json(StatusCode::OK, answer.as_ref()))
}
