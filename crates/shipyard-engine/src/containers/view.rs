//! What `ps` and `inspect` show of containers: their records with their
//! states, as data (the CLI prints it, an API sends it).

use std::collections::BTreeMap;

use serde::Serialize;

use super::config::Config;
use super::record::{BoundPort, Record};
use super::state::ContainerState;
use crate::Engine;
use crate::error::Result;
use crate::events::rfc3339;
use crate::report::WarningKind;

/// A container as `ps` lists it: its record and its state.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Summary {
    /// Its record.
    pub record: Record,
    /// Its state, now.
    pub state: ContainerState,
    /// What its health checks say, if it has them and is up.
    pub health: Option<crate::health::Health>,
    /// When and how this start lost its network, if it did (its pasta
    /// died), while it's up.
    pub network_lost: Option<crate::network::lost::NetworkLost>,
}

/// The containers, oldest first, each with its state; only the ones up
/// (running, paused, stopping, restarting) unless `all`. Read without taking any
/// lock: a container whose state can't be read is reported and left out.
///
/// # Errors
///
/// As [`super::list`].
pub fn summaries(engine: &Engine, all: bool) -> Result<Vec<Summary>> {
    let mut found = Vec::new();
    for record in super::list(engine)? {
        let state = match super::state(engine, &record) {
            Ok(state) => state,
            Err(error) => {
                engine.warn(
                    WarningKind::RecordUnreadable,
                    format!("container {} is left out: {error:#}", record.name),
                );
                continue;
            }
        };
        if all || state.is_up() {
            let health = super::health(engine, &record, &state);
            let network_lost = super::network_lost(engine, &record, &state);
            found.push(Summary {
                record,
                state,
                health,
                network_lost,
            });
        }
    }
    Ok(found)
}

/// A container as `ps --format json` lists it.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Listed {
    /// Its full ID.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its serial.
    pub serial: String,
    /// The image it was made from, as given.
    pub image: Option<String>,
    /// The command it was given (its entrypoint, then its command; empty:
    /// the image's own).
    pub command: Vec<String>,
    /// When it was made (RFC 3339, UTC).
    pub created: String,
    /// `created`, `running`, `paused`, `stopping`, `restarting` or
    /// `exited`.
    pub state: &'static str,
    /// What its health checks say (`starting`, `healthy`, `unhealthy`), if
    /// it has them and is up.
    pub health: Option<crate::health::Status>,
    /// Whether this start lost its network (its pasta died).
    pub network_lost: bool,
    /// The host ports its last start published.
    pub ports: Vec<BoundPort>,
}

impl From<&Summary> for Listed {
    fn from(summary: &Summary) -> Self {
        let record = &summary.record;
        Self {
            id: record.id.as_str().to_owned(),
            name: record.name.as_str().to_owned(),
            serial: record.serial.to_string(),
            image: record.config.image.clone(),
            command: summary.command(),
            created: rfc3339(record.created),
            state: summary.state.word(),
            health: summary.health.as_ref().map(|health| health.status),
            network_lost: summary.network_lost.is_some(),
            ports: record.ports.clone(),
        }
    }
}

impl Summary {
    /// The command it was given: its entrypoint (unless `""`), then its
    /// command; empty when it's the image's own.
    #[must_use]
    pub fn command(&self) -> Vec<String> {
        let config = &self.record.config;
        config
            .entrypoint
            .iter()
            .filter(|entrypoint| !entrypoint.is_empty())
            .chain(&config.command)
            .cloned()
            .collect()
    }
}

/// A container as `inspect` shows it (its JSON is Shipyard's own,
/// documented in `docs/user/containers.md`).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Inspected {
    /// Its full ID.
    pub id: String,
    /// Its serial (`SHPU 304812 2`).
    pub serial: String,
    /// Its name.
    pub name: String,
    /// When it was made (RFC 3339, UTC).
    pub created: String,
    /// The image it was made from, as given.
    pub image: Option<String>,
    /// That image's manifest digest: what every start runs.
    pub image_id: Option<String>,
    /// Where it is in its life.
    pub state: InspectedState,
    /// The host ports its last start published.
    pub ports: Vec<BoundPort>,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// Removed once it ends (`--rm`).
    pub auto_remove: bool,
    /// The restarts its policy (`config.restart`) made since its user last
    /// started it.
    pub restart_count: u32,
    /// Its last start's log directory, if it has started.
    pub log_dir: Option<String>,
    /// How it runs, as it was made.
    pub config: Config,
}

/// `inspect`'s `state`.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct InspectedState {
    /// `created`, `running`, `paused`, `stopping`, `restarting` or `exited`.
    pub status: &'static str,
    /// Whether it's up (running, paused, stopping or restarting).
    pub running: bool,
    /// Its init's PID (on the host), while it runs.
    pub pid: Option<i32>,
    /// How it last ended: its exit code (128 + N for signal N).
    pub exit_code: Option<i32>,
    /// What the out-of-memory killer did, if it ended it.
    pub oom: Option<String>,
    /// When it last started (RFC 3339, UTC).
    pub started_at: Option<String>,
    /// When it last ended.
    pub finished_at: Option<String>,
    /// What its health checks say, with their latest results, if it has
    /// them and is up.
    pub health: Option<crate::health::Health>,
    /// When and how this start lost its network (its pasta died), if it
    /// did, while it's up.
    pub network_lost: Option<crate::network::lost::NetworkLost>,
}

/// A container's published ports, as `port` shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ports {
    /// The container's name.
    pub name: String,
    /// The host ports it publishes now, in the order its start bound them
    /// (none unless it's running or paused).
    pub ports: Vec<BoundPort>,
}

/// The host ports the container `reference` names (as [`super::find`]
/// finds it) publishes now: those to its port `wanted` (its number and
/// protocol, `tcp` or `udp`), or all of them. A container that isn't
/// running or paused publishes none.
///
/// # Errors
///
/// As [`super::find`] and [`super::state()`].
pub fn ports(engine: &Engine, reference: &str, wanted: Option<(u16, &str)>) -> Result<Ports> {
    let record = super::find(engine, reference)?;
    let up = matches!(
        super::state(engine, &record)?,
        ContainerState::Running { .. } | ContainerState::Paused
    );
    let ports = if up {
        record
            .ports
            .iter()
            .filter(|port| {
                wanted.is_none_or(|(number, protocol)| {
                    port.container_port == number && port.protocol == protocol
                })
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    Ok(Ports {
        name: record.name.as_str().to_owned(),
        ports,
    })
}

/// A socket of a container's, as `port --sockets` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Sock {
    /// `tcp` or `udp`.
    pub protocol: &'static str,
    /// Its own address and port.
    pub local: std::net::SocketAddr,
    /// The other side's, if it's connected.
    pub remote: Option<std::net::SocketAddr>,
    /// Its state, as `ss` says it (`LISTEN`, `ESTAB`, `UNCONN`, …).
    pub state: &'static str,
    /// The container at the other side, by its name, where one of the
    /// networks' leases has that address (root's networks).
    pub peer: Option<String>,
}

/// A container's sockets, as `port --sockets` shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sockets {
    /// The container's name.
    pub name: String,
    /// Its sockets: those it listens on first, then by their own port.
    pub sockets: Vec<Sock>,
}

/// The sockets of the container `reference` names, read from its network
/// namespace's tables through its init (`/proc/PID/net`), each peer named
/// where it's a container on one of the networks.
///
/// # Errors
///
/// As [`super::find`]; [`ErrorKind::NotRunning`](crate::ErrorKind::NotRunning)
/// if it isn't up (running, paused or created), or ended as its tables were
/// read; [`ErrorKind::Environment`](crate::ErrorKind::Environment) if its
/// `/proc` entry is hidden from this user (`hidepid`);
/// [`ErrorKind::Io`](crate::ErrorKind::Io) if its tables can't be read
/// otherwise; [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) off
/// Linux.
pub fn sockets(engine: &Engine, reference: &str) -> Result<Sockets> {
    let record = super::find(engine, reference)?;
    super::require(engine, &record, super::Needs::Up, "see its sockets")?;
    #[cfg(target_os = "linux")]
    {
        use shipyard_network::observe::ObserveError;
        let ended = || {
            crate::error::Error::new(
                crate::error::ErrorKind::NotRunning,
                format!("container {} has ended: it has no sockets", record.name),
            )
        };
        let (pid, started) = engine
            .paths
            .runtime_root()
            .ok()
            .and_then(|root| {
                shipyard_runtime::container::peek_init(root, record.id.as_str())
                    .ok()
                    .flatten()
            })
            .ok_or_else(ended)?;
        let read = shipyard_network::observe::sockets(pid).map_err(|error| {
            let kind = match error {
                ObserveError::Gone(_) => return ended(),
                // hidepid: whose /proc is it is the host's to say.
                ObserveError::Hidden(_) => crate::error::ErrorKind::Environment,
                _ => crate::error::ErrorKind::Io,
            };
            crate::error::Error::new(
                kind,
                format!("container {}'s sockets: {error}", record.name),
            )
        })?;
        // The read held the process at `pid` (a pidfd), but that may have
        // been another one's, its PID reused after the init ended: still
        // the init, after the read, or the read was of nothing of ours.
        if !shipyard_runtime::container::is_process(pid, started) {
            return Err(ended());
        }
        let names: BTreeMap<String, String> = super::list(engine)
            .unwrap_or_default()
            .into_iter()
            .map(|other| (other.id.as_str().to_owned(), other.name.as_str().to_owned()))
            .collect();
        let peers: BTreeMap<std::net::IpAddr, String> = crate::network::container_addresses(engine)
            .into_iter()
            .filter_map(|(ip, id)| Some((ip, names.get(&id)?.clone())))
            .collect();
        let mut sockets: Vec<(bool, Sock)> = read
            .iter()
            .map(|socket| {
                let remote = (!socket.remote.ip().is_unspecified()).then_some(socket.remote);
                let sock = Sock {
                    protocol: socket.protocol.as_str(),
                    local: socket.local,
                    remote,
                    state: socket.state_word(),
                    peer: remote.and_then(|remote| peers.get(&remote.ip().to_canonical()).cloned()),
                };
                (socket.is_listening(), sock)
            })
            .collect();
        sockets.sort_by(|(a_listens, a), (b_listens, b)| {
            b_listens
                .cmp(a_listens)
                .then(a.local.port().cmp(&b.local.port()))
                .then(a.protocol.cmp(b.protocol))
                .then(a.local.cmp(&b.local))
                .then(a.remote.cmp(&b.remote))
        });
        Ok(Sockets {
            name: record.name.as_str().to_owned(),
            sockets: sockets.into_iter().map(|(_, sock)| sock).collect(),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        crate::error::bail!(Unsupported, "a container's sockets are read on Linux")
    }
}

/// The container `reference` names (as [`super::find`] finds it), as
/// `inspect` shows it.
///
/// # Errors
///
/// As [`super::find`] and [`super::state()`].
pub fn inspect(engine: &Engine, reference: &str) -> Result<Inspected> {
    let record = super::find(engine, reference)?;
    let state = super::state(engine, &record)?;
    let (exit_code, oom, finished) = match &state {
        ContainerState::Exited {
            code,
            oom,
            finished,
        } => (*code, oom.clone(), *finished),
        ContainerState::Restarting { code, finished, .. } => (*code, None, *finished),
        _ => (None, None, None),
    };
    let log_dir = super::logs_dir(engine, &record.id).ok().and_then(|logs| {
        let last = super::last_logs_name(&logs)?;
        Some(logs.join(last).display().to_string())
    });
    Ok(Inspected {
        id: record.id.as_str().to_owned(),
        serial: record.serial.to_string(),
        name: record.name.as_str().to_owned(),
        created: rfc3339(record.created),
        image: record.config.image.clone(),
        image_id: record.image_id.clone(),
        state: InspectedState {
            status: state.word(),
            running: state.is_up(),
            pid: state.is_up().then(|| init_pid(engine, &record)).flatten(),
            exit_code,
            oom,
            started_at: record.started.map(rfc3339),
            finished_at: finished.or(record.finished).map(rfc3339),
            health: super::health(engine, &record, &state),
            network_lost: super::network_lost(engine, &record, &state),
        },
        ports: record.ports.clone(),
        labels: record.labels.as_map().clone(),
        auto_remove: record.auto_remove,
        restart_count: record.restart_count,
        log_dir,
        config: record.config,
    })
}

/// Container `record`'s init's PID, from its runtime state, read without
/// its lock (`inspect` never waits behind another command).
fn init_pid(engine: &Engine, record: &Record) -> Option<i32> {
    let root = engine.paths.runtime_root().ok()?;
    shipyard_runtime::container::peek_init(root, record.id.as_str())
        .ok()
        .flatten()
        .map(|(pid, _)| pid)
}
