//! The container endpoints of step 11.2a: list, inspect, create, start,
//! stop, restart, kill, remove and wait. Each is the engine's call (the
//! CLI's), answered as Docker answers it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hyper::StatusCode;
use hyper::body::Incoming;
use serde::Serialize;
use shipyard_engine::docker::filters::ContainerFilters;
use shipyard_engine::docker::{self, ContainerSummary, End};
use shipyard_engine::run::StopOptions;
use shipyard_engine::{ErrorKind, image, run};
use shipyard_runtime::process::Signal;

use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body};
use crate::{Shared, create};

type Reply = Result<hyper::Response<Body>, ApiError>;

/// How often a `wait` looks again.
const WAIT_POLL: Duration = Duration::from_millis(100);

/// `GET /containers/json`: Docker's summaries, newest first (`all`, else
/// the running ones; `limit`; `filters`).
pub(crate) async fn list(shared: &Arc<Shared>, query: &Query) -> Reply {
    let all = query.flag("all");
    let limit: Option<i64> = query.parsed("limit")?;
    let filters = ContainerFilters::parse(crate::filters::given(query.get("filters"))?)?;
    let (mut summaries, _) = shared
        .call(move |engine| {
            docker::list_containers_where(engine, all || limit.is_some_and(|n| n > 0), &filters)
        })
        .await?;
    if let Some(limit) = limit
        .and_then(|limit| usize::try_from(limit).ok())
        .filter(|&n| n > 0)
    {
        summaries.truncate(limit);
    }
    Ok(reply::json::<Vec<ContainerSummary>>(
        StatusCode::OK,
        &summaries,
    ))
}

/// `GET /containers/{id}/json`.
pub(crate) async fn inspect(shared: &Arc<Shared>, id: String) -> Reply {
    let (inspected, _) = shared
        .call(move |engine| docker::inspect_container(engine, &id))
        .await?;
    Ok(reply::json(StatusCode::OK, &inspected))
}

/// `GET /containers/{id}/top`: its processes in `ps -ef`'s columns, as
/// Docker's `ContainerTopResponse`; `ps_args` other than `-ef` (Docker's
/// daemon's default) are refused, 400: Shipyard reads /proc itself.
pub(crate) async fn top(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    if let Some(asked) = query
        .get("ps_args")
        .filter(|asked| !matches!(asked.trim(), "" | "-ef"))
    {
        return Err(ApiError::bad(format!(
            "ps options aren't supported ({asked:?}): Shipyard shows `ps -ef`'s columns"
        )));
    }
    let (top, _) = shared.call(move |engine| docker::top(engine, &id)).await?;
    Ok(reply::json(StatusCode::OK, &top))
}

/// `POST /containers/prune`: every stopped container `filters` takes
/// (`until`, `label`, `label!`) removed; Docker's `ContainersDeleted`
/// (`null` for none) and `SpaceReclaimed`. What couldn't be removed is
/// logged, as Docker's daemon does.
pub(crate) async fn prune(shared: &Arc<Shared>, query: &Query) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Pruned {
        containers_deleted: Option<Vec<String>>,
        space_reclaimed: u64,
    }
    let filters = shipyard_engine::docker::filters::ContainerPruneFilters::parse(
        crate::filters::given(query.get("filters"))?,
        shipyard_engine::docker::unix_now(),
    )?;
    let (pruned, _) = shared
        .call(move |engine| shipyard_engine::prune::containers(engine, &filters))
        .await?;
    for failed in &pruned.failed {
        tracing::warn!("a container prune couldn't remove: {failed}");
    }
    let deleted: Vec<String> = pruned
        .removed
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    Ok(reply::json(
        StatusCode::OK,
        &Pruned {
            containers_deleted: (!deleted.is_empty()).then_some(deleted),
            space_reclaimed: pruned.bytes,
        },
    ))
}

/// `POST /containers/create`: 201 with its ID and the warnings; an image
/// that isn't stored is Docker's `No such image` (404: the client pulls it,
/// then asks again), never pulled here.
pub(crate) async fn create(shared: &Arc<Shared>, query: &Query, body: Incoming) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Created {
        id: String,
        warnings: Vec<String>,
    }
    let body: create::CreateBody = crate::read_json(body).await?;
    let mut create = create::request(&body, query.get("name"))?;
    create.request.pull = Some(image::PullPolicy::Never);
    // `?platform=` (Docker's CLI sends its `--platform`): it must run here,
    // and an image stored for another is "No such image", so the client
    // pulls the one asked for (step 13.3).
    create.request.platform = match query.get("platform").filter(|asked| !asked.is_empty()) {
        None => None,
        Some(asked) => Some(
            image::platform_here(asked, "run").map_err(|error| ApiError::bad(error.to_string()))?,
        ),
    };
    let reference = create.request.image.clone().unwrap_or_default();
    let platform = create.request.platform.clone();
    let (made, warnings) = shared
        .call(move |engine| {
            if !image::stored(engine, &reference, platform.as_ref())? {
                return Ok(Err(reference));
            }
            run::create(engine, create.request, create.auto_remove).map(Ok)
        })
        .await?;
    let record = made.map_err(|reference| {
        ApiError::new(StatusCode::NOT_FOUND, format!("No such image: {reference}"))
    })?;
    Ok(reply::json(
        StatusCode::CREATED,
        &Created {
            id: record.id.as_str().to_owned(),
            warnings,
        },
    ))
}

/// `POST /containers/{id}/start`: 304 if it's running already. Attaches
/// waiting for it are let in before its start gate opens
/// ([`crate::attach::hand_over`]): nothing it writes first is missed. Its
/// input ends at its first client's end if it was made so (`StdinOnce`,
/// kept with the container: F116). A
/// start that fails removes a container made to be removed (`--rm`), as
/// Docker's does: its client waits for that.
pub(crate) async fn start(shared: &Arc<Shared>, id: String) -> Reply {
    let held = Arc::clone(shared);
    let (started, _) = shared
        .call(move |engine| {
            let standing = docker::standing(engine, &id)?;
            if standing.started() {
                return Ok(false);
            }
            let id = standing.id.as_str();
            let root = engine.paths().runtime_root()?.to_owned();
            // (Its input closed once, if it was made so: its start reads it.)
            let started = run::start_with(engine, id, |id| {
                crate::attach::hand_over(&held.pending, &root, id.as_str());
            });
            if let Err(error) = started {
                if standing.auto_remove {
                    // As `--rm` does: its anonymous volumes too.
                    let _ = run::remove_with(engine, id, run::RemoveOptions::new(true, true));
                }
                return Err(error);
            }
            Ok(true)
        })
        .await?;
    Ok(reply::empty(if started {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_MODIFIED
    }))
}

/// `POST /containers/{id}/stop` (`t`, `signal`): 304 if it isn't running.
/// Its wait for the end is given up once its client goes, or the service
/// stops (F123): the stop goes on in the container's monitor (SIGKILL at
/// its timeout), as Docker's daemon finishes it, and the service can end.
pub(crate) async fn stop(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let options = stop_options(query)?;
    // Set as this request's future is dropped: hyper drops it when its
    // client goes, while the engine's call goes on on its own thread.
    let client_gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _goes = SetOnDrop(Arc::clone(&client_gone));
    let service = Arc::clone(shared);
    let (stopped, _) = shared
        .call(move |engine| {
            if !docker::standing(engine, &id)?.running() {
                return Ok(false);
            }
            let gone = || {
                client_gone.load(std::sync::atomic::Ordering::Acquire) || service.idle.is_stopping()
            };
            run::stop_until(engine, &id, options, &gone).map(|_| true)
        })
        .await?;
    Ok(reply::empty(if stopped {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_MODIFIED
    }))
}

/// `POST /containers/{id}/restart` (`t`, `signal`).
pub(crate) async fn restart(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let options = stop_options(query)?;
    shared
        .call(move |engine| run::restart(engine, &id, options))
        .await?;
    Ok(reply::empty(StatusCode::NO_CONTENT))
}

/// `POST /containers/{id}/kill` (`signal`, SIGKILL if none): 409 if it
/// isn't running, as Docker's.
pub(crate) async fn kill(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let signal = signal(query)?.unwrap_or(Signal::KILL);
    shared
        .call(move |engine| run::kill(engine, &id, signal))
        .await?;
    Ok(reply::empty(StatusCode::NO_CONTENT))
}

/// `POST /containers/{id}/pause` (`freeze`) and `/unpause`: 204; 409 for
/// one that isn't running, or is (or isn't) paused already, as Docker's.
pub(crate) async fn pause(shared: &Arc<Shared>, id: String, freeze: bool) -> Reply {
    shared
        .call(move |engine| {
            if freeze {
                run::pause(engine, &id).map(drop)
            } else {
                run::unpause(engine, &id).map(drop)
            }
        })
        .await?;
    Ok(reply::empty(StatusCode::NO_CONTENT))
}

/// `POST /containers/{id}/rename?name=NEW`: 204; 404 for no such
/// container, 409 for a name taken, as Docker's. `docker rename`, and
/// compose's recreate (it renames the new container to the old's name once
/// the old one is gone: 11.6b's API review).
pub(crate) async fn rename(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let name = query
        .get("name")
        .map(|name| name.trim_start_matches('/'))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::bad("a rename needs a name"))?
        .to_owned();
    shared
        .call(move |engine| run::rename(engine, &id, &name).map(drop))
        .await?;
    Ok(reply::empty(StatusCode::NO_CONTENT))
}

/// `POST /containers/{id}/update`: Docker's `UpdateConfig`, its resource
/// fields (`Memory`, `NanoCpus`, `PidsLimit`, …, named as a create's
/// `HostConfig` names them) and its `RestartPolicy`, changed only when it
/// names one (Docker's CLI sends `{"Name": ""}` without `--restart`); 200
/// with what was skipped, as Docker's `Warnings`. Asking nothing is 200
/// too, as Docker's (the container still has to be there).
pub(crate) async fn update(shared: &Arc<Shared>, id: String, body: Incoming) -> Reply {
    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Updated {
        warnings: Vec<String>,
    }
    let asked: create::HostConfig = crate::read_json(body).await?;
    let request = update_request(&asked)?;
    let ((), warnings) = shared
        .call(move |engine| {
            if request == run::UpdateRequest::default() {
                return docker::inspect_container(engine, &id).map(drop);
            }
            run::update(engine, &id, &request).map(drop)
        })
        .await?;
    Ok(reply::json(StatusCode::OK, &Updated { warnings }))
}

/// What Docker's update body `asked` asks the engine for.
///
/// # Errors
///
/// A value that isn't one, or a setting an update can't change here
/// (400: what Shipyard can't do is refused, never ignored).
pub(crate) fn update_request(asked: &create::HostConfig) -> Result<run::UpdateRequest, ApiError> {
    let unsupported = [
        (
            "Ulimits",
            asked.ulimits.as_ref().is_some_and(|list| !list.is_empty()),
        ),
        (
            "Devices",
            asked.devices.as_ref().is_some_and(|list| !list.is_empty()),
        ),
        (
            "DeviceCgroupRules",
            asked
                .device_cgroup_rules
                .as_ref()
                .is_some_and(|list| !list.is_empty()),
        ),
        ("OomKillDisable", asked.oom_kill_disable == Some(true)),
        (
            "MemorySwappiness",
            asked.memory_swappiness.is_some_and(|value| value != -1),
        ),
        (
            "BlkioWeightDevice",
            asked
                .blkio_weight_device
                .as_ref()
                .is_some_and(|list| !list.is_empty()),
        ),
        (
            "CpuRealtimePeriod",
            asked.cpu_realtime_period.is_some_and(|value| value != 0),
        ),
        (
            "CpuRealtimeRuntime",
            asked.cpu_realtime_runtime.is_some_and(|value| value != 0),
        ),
    ];
    if let Some((name, _)) = unsupported.iter().find(|(_, set)| *set) {
        return Err(ApiError::bad(format!(
            "{name} can't be updated here: give it when the container is made, or not at all"
        )));
    }
    let mut request = run::UpdateRequest::default();
    let named = asked
        .restart_policy
        .as_ref()
        .is_some_and(|policy| policy.name.as_deref().is_some_and(|name| !name.is_empty()));
    if named {
        request.restart = Some(create::restart_policy(asked)?);
    }
    Ok(request.with_limits(create::docker_limits(asked)?))
}

/// `DELETE /containers/{id}` (`force`; `v`: its anonymous volumes too;
/// `link` is refused).
pub(crate) async fn remove(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    if query.flag("link") {
        return Err(ApiError::bad(
            "links aren't supported: there's none to remove",
        ));
    }
    let options = run::RemoveOptions::new(query.flag("force"), query.flag("v"));
    shared
        .call(move |engine| run::remove_with(engine, &id, options))
        .await?;
    Ok(reply::empty(StatusCode::NO_CONTENT))
}

/// The options of `stop` and `restart`: `t` in seconds (-1: as long as it
/// takes), `signal`.
fn stop_options(query: &Query) -> Result<StopOptions, ApiError> {
    let timeout = query.parsed::<i64>("t")?.map(StopOptions::timeout_of);
    Ok(StopOptions {
        signal: signal(query)?,
        timeout,
    })
}

/// The query's `signal`, if it gives one: a name (`SIGTERM`, `TERM`) or a
/// number.
fn signal(query: &Query) -> Result<Option<Signal>, ApiError> {
    query
        .get("signal")
        .filter(|signal| !signal.is_empty())
        .map(|signal| {
            signal
                .parse::<Signal>()
                .map_err(|why| ApiError::bad(format!("signal: {why}")))
        })
        .transpose()
}

/// What `wait` waits for (its `condition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Condition {
    /// It isn't running (now, or once it ends): the default.
    NotRunning,
    /// Its next end.
    NextExit,
    /// It's removed.
    Removed,
}

/// `POST /containers/{id}/wait` (`condition`): its exit code once the
/// condition holds. Answered at once (200, the headers sent), its body
/// written when the condition holds, as Docker's daemon does: Docker's CLI
/// waits for the headers before it starts the container (`docker run`), so
/// an answer held back until the end would never come. A container that
/// isn't there is refused first (404). It's looked for every [`WAIT_POLL`]
/// without a lock, until the condition holds or the client has gone.
pub(crate) async fn wait(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let condition = match query.get("condition").unwrap_or_default() {
        "" | "not-running" => Condition::NotRunning,
        "next-exit" => Condition::NextExit,
        "removed" => Condition::Removed,
        other => {
            return Err(ApiError::bad(format!(
                "invalid condition: {other:?} (not-running, next-exit or removed)"
            )));
        }
    };
    // A die the journal says is one since the wait began (a second's
    // margin for the clocks' granularity).
    let began = SystemTime::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    // The first look finds it by its reference (the engine's answer if
    // it's not there); the others by its ID, so a container made under its
    // name meanwhile isn't taken for it.
    let first = shared.standing(id).await?;
    let (answer, body) = reply::channel();
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let answered = watch_until(&shared, first, condition, began, || answer.is_closed()).await;
        if let Some(answered) = answered {
            let bytes = serde_json::to_vec(&answered).unwrap_or_default();
            let _ = answer.send(bytes.into()).await;
        }
    });
    Ok(reply::channelled(StatusCode::OK, "application/json", body))
}

/// Looks at the container `first` describes until `condition` holds: its
/// answer; `None` once `gone` says its client has gone. A look that fails
/// is answered as Docker answers a wait's failure (its `Error`).
async fn watch_until(
    shared: &Arc<Shared>,
    first: docker::Standing,
    condition: Condition,
    began: SystemTime,
    gone: impl Fn() -> bool,
) -> Option<WaitAnswer> {
    let id = first.id.clone();
    let mut watch = Watch::new(condition);
    let mut standing = Some(first);
    loop {
        match watch.look(standing.as_ref()) {
            Next::Answer(answer) => return Some(answer),
            Next::GoneUnseen { ran } => {
                let id = id.clone();
                let read = shared
                    .call(move |engine| docker::last_end(engine, &id, began))
                    .await;
                return Some(match read {
                    Ok((
                        docker::LastEnd {
                            code: Some(code), ..
                        },
                        _,
                    )) => WaitAnswer::code(Some(code)),
                    // Never seen running, and no start in the journal: it
                    // never ran (a start that failed, then `--rm`), so it has
                    // no end to say, as Docker answers it (F139). One that
                    // ran and whose end is lost keeps the error.
                    Ok((last, _)) if !ran && !last.started => WaitAnswer::code(None),
                    Ok(_) => WaitAnswer::unseen(),
                    Err(error) => WaitAnswer::failed(error.message),
                });
            }
            Next::Again => {}
        }
        tokio::time::sleep(WAIT_POLL).await;
        if gone() {
            return None;
        }
        let id = id.clone();
        let looked = shared
            .call(move |engine| match docker::standing(engine, id.as_str()) {
                Ok(standing) => Ok(Some(standing)),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            })
            .await;
        standing = match looked {
            Ok((standing, _)) => standing,
            Err(error) => return Some(WaitAnswer::failed(error.message)),
        };
    }
}

/// Sets its flag when it's dropped: an async request's way to tell the
/// blocking work it started that the request is over.
struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// `wait`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaitAnswer {
    #[serde(rename = "StatusCode")]
    status_code: i64,
    #[serde(rename = "Error", skip_serializing_if = "Option::is_none")]
    error: Option<BTreeMap<&'static str, String>>,
}

impl WaitAnswer {
    fn code(code: Option<i32>) -> Self {
        Self {
            status_code: i64::from(code.unwrap_or(0)),
            error: None,
        }
    }

    /// Gone, its end neither seen nor in the journal: said, never made up.
    fn unseen() -> Self {
        Self::failed("it was removed before its end could be read".to_owned())
    }

    /// The wait failed after its answer had begun (`why`), as Docker says
    /// so: in the body's `Error`.
    fn failed(why: String) -> Self {
        Self {
            status_code: -1,
            error: Some(BTreeMap::from([("Message", why)])),
        }
    }
}

/// What a wait does after a look.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// Answers.
    Answer(WaitAnswer),
    /// The container is gone, its end unseen: the journal is asked;
    /// `ran` says whether it was ever seen running or ended.
    GoneUnseen { ran: bool },
    /// Looks again.
    Again,
}

/// A wait's progress: its condition, the end it began with, the last end
/// it saw.
#[derive(Debug)]
struct Watch {
    condition: Condition,
    /// Whether it has looked once.
    looked: bool,
    /// The end the container had when the wait began.
    first: Option<End>,
    /// The last end seen.
    seen: Option<End>,
    /// Whether it was ever seen running or ended.
    ran: bool,
}

impl Watch {
    fn new(condition: Condition) -> Self {
        Self {
            condition,
            looked: false,
            first: None,
            seen: None,
            ran: false,
        }
    }

    /// One look at `standing` (`None`: it's gone). An end its restart
    /// policy follows is an end (moby wakes its waiters at a restart), but
    /// never the one it had when the wait began.
    fn look(&mut self, standing: Option<&docker::Standing>) -> Next {
        let Some(standing) = standing else {
            // Gone: the end last seen is its last.
            return match self.seen {
                Some(end) => Next::Answer(WaitAnswer::code(end.code)),
                None => Next::GoneUnseen { ran: self.ran },
            };
        };
        let ended = standing.ended();
        self.ran |= standing.running() || ended.is_some();
        if !self.looked {
            self.looked = true;
            self.first = ended;
        }
        if ended.is_some() {
            self.seen = ended;
        }
        let new_end = ended.is_some() && ended != self.first;
        let holds = match self.condition {
            Condition::NotRunning => !standing.running() || (standing.restarting() && new_end),
            Condition::NextExit => (!standing.running() || standing.restarting()) && new_end,
            Condition::Removed => false,
        };
        if holds {
            Next::Answer(WaitAnswer::code(ended.and_then(|end| end.code)))
        } else {
            Next::Again
        }
    }
}

#[cfg(test)]
mod tests;
