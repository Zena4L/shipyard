//! A container as `GET /containers/json` (and `docker ps`) lists it, and
//! the status words Docker writes there (`Up 3 minutes (healthy)`).

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use super::container::{EndpointSettings, MountPoint};
use super::filters::{ContainerFilters, Facts};
use crate::Engine;
use crate::containers::{self, ContainerState, Summary};
use crate::error::{Error, ErrorKind, Result};

/// A container, Docker's `ContainerSummary`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct ContainerSummary {
    /// Its full ID.
    pub id: String,
    /// Its name, as Docker writes it (`/web`).
    pub names: Vec<String>,
    /// The image, as given.
    pub image: String,
    /// The image's digest every start runs.
    #[serde(rename = "ImageID")]
    pub image_id: String,
    /// The command it runs, as one line: its own, its image's where it
    /// gave none (F101).
    pub command: String,
    /// When it was made, in seconds since the epoch.
    pub created: i64,
    /// Its ports: those published while it's up, with where.
    pub ports: Vec<SummaryPort>,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// `created`, `running`, `paused`, `restarting` or `exited`.
    pub state: &'static str,
    /// Its status in Docker's words: `Up 3 minutes (healthy)`,
    /// `Exited (0) 2 hours ago`.
    pub status: String,
    /// Its network mode.
    pub host_config: SummaryHostConfig,
    /// Its networks, by name.
    pub network_settings: SummaryNetworks,
    /// Its binds and volumes.
    pub mounts: Vec<MountPoint>,
    /// Its health: `starting`, `healthy`, `unhealthy`, or `none` (Docker's
    /// `Health.Status`, served from API 1.52 on: not by this one, 1.43).
    #[serde(skip)]
    pub health: &'static str,
}

/// One of a summary's ports, Docker's `Port`.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct SummaryPort {
    /// The host address, if it's published.
    #[serde(rename = "IP", skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// The container's port.
    #[serde(rename = "PrivatePort")]
    pub private_port: u16,
    /// The host's port, if it's published.
    #[serde(rename = "PublicPort", skip_serializing_if = "Option::is_none")]
    pub public_port: Option<u16>,
    /// `tcp` or `udp`.
    #[serde(rename = "Type")]
    pub kind: String,
}

/// A summary's `HostConfig`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct SummaryHostConfig {
    /// `bridge` (or the network's name), `host`, `none` or `pasta`.
    pub network_mode: String,
}

/// A summary's `NetworkSettings`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct SummaryNetworks {
    /// Its networks, by name, with its addresses there while it's up.
    pub networks: BTreeMap<String, EndpointSettings>,
}

/// The containers, as Docker lists them: only those up unless `all`,
/// newest first (Docker's order).
///
/// # Errors
///
/// As [`containers::summaries`].
pub fn list_containers(engine: &Engine, all: bool) -> Result<Vec<ContainerSummary>> {
    list_containers_where(engine, all, &ContainerFilters::default())
}

/// The containers `filters` pass, as [`list_containers`] lists them: a
/// `status` or `exited` asked for looks at every one, as `all` does
/// (Docker's); `before` and `since` name a container (as [`containers::find`]
/// does: its ID, its name, or a start of its ID no other has), and keep
/// those made before or after it; `ancestor` matches an image by the name
/// it was given, or by the image the store has under that name.
///
/// # Errors
///
/// As [`containers::summaries`]; a container `before` or `since` names
/// that isn't there ([`ErrorKind::NotFound`]), or a start of more than one
/// ID ([`ErrorKind::InvalidRequest`]).
pub fn list_containers_where(
    engine: &Engine,
    all: bool,
    filters: &ContainerFilters,
) -> Result<Vec<ContainerSummary>> {
    let now = SystemTime::now();
    let all = all || filters.wants_all();
    let bounded = filters.bounds().next().is_some();
    // Newest first; every one only if `before` or `since` may name one not
    // up (a list of those up reads no more than it shows).
    let mut listed = containers::summaries(engine, all || bounded)?;
    listed.reverse();
    let mut kept = vec![true; listed.len()];
    for (key, reference) in filters.bounds() {
        let at = named_in(&listed, reference)?;
        for (index, keep) in kept.iter_mut().enumerate() {
            // Newest first: `since` keeps what's before it, `before` what's
            // after.
            *keep &= if key == "since" {
                index < at
            } else {
                index > at
            };
        }
    }
    let ancestors: Vec<String> = filters
        .ancestors()
        .flat_map(|name| crate::image::manifests_named(engine, name))
        .collect();
    // Every network's leases read once for the whole list (11.7).
    let leases_of = super::container::live_leases_by_container(engine);
    Ok(listed
        .iter()
        .zip(kept)
        .filter(|(summary, keep)| *keep && (all || summary.state.is_active()))
        .filter_map(|(summary, _)| {
            let leases = leases_of
                .get(summary.record.id.as_str())
                .map_or(&[][..], Vec::as_slice);
            // Another's network, if it joins one (a create that can't be
            // read: none shown, its starts say why).
            let network_from = crate::containers::network_from(engine, &summary.record.id);
            let listed = summarized(summary, leases, network_from.as_ref(), now);
            let facts = Facts {
                exit_code: match summary.state {
                    ContainerState::Exited { code, .. } => code,
                    _ => None,
                },
                ancestors: &ancestors,
            };
            filters.pass(&listed, facts).then_some(listed)
        })
        .collect())
}

/// Where in `listed` the container `reference` is, as [`containers::find`]
/// finds one: its full ID, its name (with Docker's `/` or not), or a start
/// of its ID no other has.
fn named_in(listed: &[Summary], reference: &str) -> Result<usize> {
    let not_found = || {
        Error::new(
            ErrorKind::NotFound,
            format!("No such container: {reference}"),
        )
    };
    let name = reference.strip_prefix('/').unwrap_or(reference);
    let exact = listed
        .iter()
        .position(|summary| summary.record.id.as_str() == reference)
        .or_else(|| {
            listed
                .iter()
                .position(|summary| summary.record.name.as_str() == name)
        });
    if let Some(at) = exact {
        return Ok(at);
    }
    if reference.is_empty() || !reference.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(not_found());
    }
    let prefix = reference.to_ascii_lowercase();
    let mut matches = listed
        .iter()
        .enumerate()
        .filter(|(_, summary)| summary.record.id.as_str().starts_with(&prefix))
        .map(|(at, _)| at);
    match (matches.next(), matches.next()) {
        (Some(at), None) => Ok(at),
        (Some(_), Some(_)) => Err(Error::new(
            ErrorKind::InvalidRequest,
            format!("{reference} is the start of more than one container's ID"),
        )),
        (None, _) => Err(not_found()),
    }
}

/// A container's health as Docker words it.
fn health_word(summary: &Summary) -> &'static str {
    match summary.health.as_ref().map(|health| health.status) {
        None => "none",
        Some(crate::health::Status::Starting) => "starting",
        Some(crate::health::Status::Healthy) => "healthy",
        Some(crate::health::Status::Unhealthy) => "unhealthy",
    }
}

/// `summary`, with its `leases`, as Docker lists it at `now`.
fn summarized(
    summary: &Summary,
    leases: &[super::container::LiveLease],
    network_from: Option<&crate::containers::ContainerId>,
    now: SystemTime,
) -> ContainerSummary {
    let record = &summary.record;
    let config = &record.config;
    let up = matches!(
        summary.state,
        ContainerState::Running { .. } | ContainerState::Paused
    );
    let mut ports: Vec<SummaryPort> = Vec::new();
    if up {
        ports.extend(record.ports.iter().map(|bound| SummaryPort {
            ip: Some(bound.host_ip.to_string()),
            private_port: bound.container_port,
            public_port: Some(bound.host_port),
            kind: bound.protocol.clone(),
        }));
    }
    let created = record
        .created
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let networks = super::container::endpoints(record, up, leases, network_from).networks;
    ContainerSummary {
        id: record.id.as_str().to_owned(),
        names: vec![format!("/{}", record.name.as_str())],
        image: config.image.clone().unwrap_or_default(),
        image_id: record.image_id.clone().unwrap_or_default(),
        command: summary.command().join(" "),
        created,
        ports,
        labels: record.labels.as_map().clone(),
        state: summary.state.docker_word(),
        status: status_text(summary, now),
        host_config: SummaryHostConfig {
            network_mode: super::container::network_mode(config, network_from),
        },
        network_settings: SummaryNetworks { networks },
        mounts: super::container::mounts(config),
        health: health_word(summary),
    }
}

/// A container's status as Docker words it at `now`: its state, then its
/// health while it runs (`(healthy)`, `(health: starting)`: Docker's says
/// none paused or ended, though inspect keeps the last), then
/// ` (no network)` if this start lost its network (Shipyard's own words).
#[must_use]
pub fn status_text(summary: &Summary, now: SystemTime) -> String {
    let state = match summary.state {
        // Docker's: `Up 3 minutes (Paused)`, since its start.
        ContainerState::Paused => match summary.record.started {
            Some(since) => format!("Up {} (Paused)", ago(since, now)),
            None => "Up (Paused)".to_owned(),
        },
        ref state => status(state, now),
    };
    let running = matches!(summary.state, ContainerState::Running { .. });
    let health = summary
        .health
        .as_ref()
        .filter(|_| running)
        .map(|health| health.status);
    with_network(with_health(state, health), summary.network_lost.is_some())
}

/// A state as Docker says it: `Up 3 minutes`, `Exited (0) 2 hours ago`.
fn status(state: &ContainerState, now: SystemTime) -> String {
    match state {
        ContainerState::Running { since: Some(since) } => format!("Up {}", ago(*since, now)),
        ContainerState::Running { since: None } => "Up".to_owned(),
        ContainerState::Paused => "Paused".to_owned(),
        ContainerState::Stopping => "Stopping".to_owned(),
        // Docker's: how it last ended, and when.
        ContainerState::Exited { code, finished, .. } => ended("Exited", *code, *finished, now),
        ContainerState::Restarting { code, finished, .. } => {
            ended("Restarting", *code, *finished, now)
        }
        ContainerState::Created => "Created".to_owned(),
    }
}

/// `status` with what the health checks say, as Docker: `Up 3 minutes
/// (healthy)`, `(unhealthy)`, `(health: starting)`.
fn with_health(status: String, health: Option<crate::health::Status>) -> String {
    use crate::health::Status;
    match health {
        None => status,
        Some(Status::Starting) => format!("{status} (health: starting)"),
        Some(said) => format!("{status} ({said})"),
    }
}

/// `status` with ` (no network)` if this start lost its network (its pasta
/// died while it ran).
fn with_network(status: String, lost: bool) -> String {
    if lost {
        format!("{status} (no network)")
    } else {
        status
    }
}

/// `WORD (CODE) N ago`: `?` for a code not known, no time for none.
fn ended(word: &str, code: Option<i32>, finished: Option<SystemTime>, now: SystemTime) -> String {
    let code = code.map_or_else(|| "?".to_owned(), |code| code.to_string());
    match finished {
        Some(finished) => format!("{word} ({code}) {} ago", ago(finished, now)),
        None => format!("{word} ({code})"),
    }
}

/// How long ago `then` was, as Docker says it: `Less than a second`,
/// `5 seconds`, `About a minute`, `3 minutes`, `About an hour`, `2 hours`,
/// `4 days`, `2 weeks`, `3 months`, `2 years`.
#[must_use]
pub fn ago(then: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    let plural =
        |count: u64, unit: &str| format!("{count} {unit}{}", if count == 1 { "" } else { "s" });
    match seconds {
        0 => "Less than a second".to_owned(),
        1..60 => plural(seconds, "second"),
        60..120 => "About a minute".to_owned(),
        120..3600 => plural(seconds / 60, "minute"),
        3600..7200 => "About an hour".to_owned(),
        7200..172_800 => plural(seconds / 3600, "hour"),
        172_800..1_209_600 => plural(seconds / 86_400, "day"),
        1_209_600..5_184_000 => plural(seconds / 604_800, "week"),
        5_184_000..63_072_000 => plural(seconds / 2_592_000, "month"),
        _ => plural(seconds / 31_536_000, "year"),
    }
}

#[cfg(test)]
mod tests;
