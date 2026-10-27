//! What `ps` and `inspect` show of containers: their records with their
//! states, as data (the CLI prints it, an API sends it).

use std::collections::BTreeMap;

use serde::Serialize;

use super::config::StoredConfig;
use super::record::{BoundPort, Record};
use super::state::ContainerState;
use crate::Engine;
use crate::error::Result;
use crate::events::rfc3339;
use crate::report::WarningKind;
use shipyard_runtime::attach::forward::{PortsAnswer, Protocol, PublishedPort};

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
    /// The command it runs: its entrypoint, then its arguments, its
    /// image's where it gave none (F101: [`shown_command`]).
    pub(crate) command: Vec<String>,
}

/// The entrypoint and the command `record` runs, apart, as a start
/// resolves them (the start's own rule, `RunRequest::entrypoint_and_cmd`)
/// with its image's `image` config: its own settings, its image's where it
/// gave none. A record whose request can't be made back shows what it was
/// given (an empty entrypoint, `--entrypoint ""`, is none).
pub(crate) fn shown_entrypoint_and_cmd(
    record: &Record,
    image: Option<&shipyard_image::RuntimeConfig>,
) -> (Vec<String>, Vec<String>) {
    let words = |list: Vec<std::ffi::OsString>| -> Vec<String> {
        list.iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    };
    record.to_request().map_or_else(
        |_| {
            let config = &record.config;
            let entrypoint = config
                .entrypoint
                .iter()
                .filter(|entrypoint| !entrypoint.is_empty())
                .cloned()
                .collect();
            (entrypoint, config.command.clone())
        },
        |request| {
            let (entrypoint, cmd) = request.entrypoint_and_cmd(image);
            (words(entrypoint), words(cmd))
        },
    )
}

/// The command `record` runs, as one list: its entrypoint, then its
/// arguments ([`shown_entrypoint_and_cmd`]).
pub(crate) fn shown_command(
    record: &Record,
    image: Option<&shipyard_image::RuntimeConfig>,
) -> Vec<String> {
    let (entrypoint, cmd) = shown_entrypoint_and_cmd(record, image);
    entrypoint.into_iter().chain(cmd).collect()
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
    // Each image read once, however many containers share it.
    let mut images = crate::image::ImageConfigs::new(engine);
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
        if all || state.is_active() {
            let health = super::health(engine, &record, &state);
            let network_lost = super::network_lost(engine, &record, &state);
            let command = shown_command(
                &record,
                images.get(record.config.image.as_deref(), record.image_id.as_deref()),
            );
            found.push(Summary {
                record,
                state,
                health,
                network_lost,
                command,
            });
        }
    }
    Ok(found)
}

impl Summary {
    /// The command it runs ([`Summary::command`]'s field, F101).
    #[must_use]
    pub fn command(&self) -> Vec<String> {
        self.command.clone()
    }
}

/// A container as Shipyard's own view shows it (its JSON is Shipyard's,
/// documented in `docs/user/containers.md`; `inspect` shows Docker's).
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
    /// How it runs, as it was made: its record's form.
    pub config: StoredConfig,
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
    super::require(engine, &record, super::Needs::Attachable, "see its sockets")?;
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

/// What both views of one container (Shipyard's own, [`inspect`], and
/// Docker's, [`crate::docker::inspect_container`]) are made from: its
/// record, its state now, and what its last start left (read once).
#[derive(Debug)]
pub(crate) struct Gathered {
    /// Its record.
    pub(crate) record: Record,
    /// Its state now.
    pub(crate) state: ContainerState,
    /// Its init's PID, while it's up.
    pub(crate) pid: Option<i32>,
    /// Its last start's log directory, if it has started.
    pub(crate) log_dir: Option<String>,
    /// What its health checks say, if it has them and is up.
    pub(crate) health: Option<crate::health::Health>,
    /// When and how this start lost its network, if it did, while it's up.
    pub(crate) network_lost: Option<crate::network::lost::NetworkLost>,
}

/// The container `reference` names, gathered for a view.
///
/// # Errors
///
/// As [`super::find`] and [`super::state()`].
pub(crate) fn gathered(engine: &Engine, reference: &str) -> Result<Gathered> {
    let record = super::find(engine, reference)?;
    let state = super::state(engine, &record)?;
    let log_dir = super::logs_dir(engine, &record.id).ok().and_then(|logs| {
        let last = super::last_logs_name(&logs)?;
        Some(logs.join(last).display().to_string())
    });
    let pid = state
        .is_active()
        .then(|| init_pid(engine, &record))
        .flatten();
    let health = super::health(engine, &record, &state);
    let network_lost = super::network_lost(engine, &record, &state);
    Ok(Gathered {
        record,
        state,
        pid,
        log_dir,
        health,
        network_lost,
    })
}

/// The container `reference` names (as [`super::find`] finds it), as
/// Shipyard's own view shows it (`docker inspect`'s is
/// [`crate::docker::inspect_container`]).
///
/// # Errors
///
/// As [`super::find`] and [`super::state()`].
pub fn inspect(engine: &Engine, reference: &str) -> Result<Inspected> {
    let Gathered {
        record,
        state,
        pid,
        log_dir,
        health,
        network_lost,
    } = gathered(engine, reference)?;
    let (exit_code, oom, finished) = match &state {
        ContainerState::Exited {
            code,
            oom,
            finished,
        } => (*code, oom.clone(), *finished),
        ContainerState::Restarting { code, finished, .. } => (*code, None, *finished),
        _ => (None, None, None),
    };
    let state = InspectedState {
        status: state.word(),
        running: state.is_active(),
        pid,
        exit_code,
        oom,
        started_at: record.started.map(rfc3339),
        finished_at: finished.or(record.finished).map(rfc3339),
        health,
        network_lost,
    };
    Ok(inspected(record, state, log_dir))
}

/// What `inspect` shows of `record`, in `state`, its last start's logs in
/// `log_dir`.
pub(super) fn inspected(
    record: Record,
    state: InspectedState,
    log_dir: Option<String>,
) -> Inspected {
    let Record {
        id,
        serial,
        name,
        created,
        config,
        labels,
        image_id,
        auto_remove,
        ports,
        restart_count,
        ..
    } = record;
    Inspected {
        id: id.as_str().to_owned(),
        serial: serial.to_string(),
        name: name.as_str().to_owned(),
        created: rfc3339(created),
        image: config.image.clone(),
        image_id,
        state,
        ports,
        labels: labels.as_map().clone(),
        auto_remove,
        restart_count,
        log_dir,
        config: StoredConfig::from(&config),
    }
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

/// What the macOS guest's VM host asks (step 10.3c1): how many containers
/// are active (running, paused, stopping, waiting to restart: what keeps
/// the VM from being idle), and the ports those running or paused
/// publish, TCP and UDP, once each. Read without taking any
/// lock; a container whose state can't be read is left out.
///
/// # Errors
///
/// As [`super::list`].
pub fn published(engine: &Engine) -> Result<PortsAnswer> {
    let mut answer = PortsAnswer::new();
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_mut, reason = "counted in the guest alone")
    )]
    let mut comes_back = 0_u32;
    for record in super::list(engine)? {
        if let Ok(state) = super::state(engine, &record) {
            add_published(&mut answer, &state, &record.ports);
            // What a restart of the guest would start (a Mac's login start
            // goes by it: 13.6b2).
            #[cfg(target_os = "linux")]
            {
                let up = matches!(
                    state,
                    ContainerState::Running { .. }
                        | ContainerState::Paused
                        | ContainerState::Stopping
                );
                if crate::run::comes_back(&record, up) {
                    comes_back = comes_back.saturating_add(1);
                }
            }
        }
    }
    answer.comes_back = Some(comes_back);
    answer.ports.sort_unstable();
    answer.ports.dedup();
    Ok(answer)
}

/// Adds a container in `state`, publishing `ports`, to `answer`.
fn add_published(answer: &mut PortsAnswer, state: &ContainerState, ports: &[BoundPort]) {
    if state.is_active() {
        answer.active = answer.active.saturating_add(1);
    }
    if !matches!(
        state,
        ContainerState::Running { .. } | ContainerState::Paused
    ) {
        return;
    }
    for port in ports {
        let protocol = match port.protocol.as_str() {
            "tcp" => Protocol::Tcp,
            "udp" => Protocol::Udp,
            _ => continue,
        };
        answer.ports.push(PublishedPort {
            ip: port.host_ip,
            port: port.host_port,
            protocol,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// Every active container counts; only a running or paused one's ports
    /// are published, each with its protocol (a stopped one's record keeps
    /// its last ports).
    #[test]
    fn only_up_containers_publish_and_active_ones_count() {
        let any = std::net::Ipv4Addr::UNSPECIFIED.into();
        let ports = [
            BoundPort::new(any, 8080, 80, "tcp"),
            BoundPort::new(any, 5353, 53, "udp"),
        ];
        let mut answer = PortsAnswer::new();
        add_published(
            &mut answer,
            &ContainerState::Running { since: None },
            &ports,
        );
        add_published(&mut answer, &ContainerState::Paused, &ports);
        add_published(&mut answer, &ContainerState::Stopping, &ports);
        let restarting = ContainerState::Restarting {
            code: None,
            finished: None,
            attempt: 1,
            at: None,
        };
        add_published(&mut answer, &restarting, &ports);
        let exited = ContainerState::Exited {
            code: Some(0),
            oom: None,
            finished: None,
        };
        add_published(&mut answer, &exited, &ports);
        add_published(&mut answer, &ContainerState::Created, &ports);
        assert_eq!(answer.active, 4);
        assert_eq!(answer.udp, 0, "a v2 answer lists them");
        let tcp = PublishedPort {
            ip: any,
            port: 8080,
            protocol: Protocol::Tcp,
        };
        let udp = PublishedPort {
            ip: any,
            port: 5353,
            protocol: Protocol::Udp,
        };
        assert_eq!(answer.ports, [tcp, udp, tcp, udp]);
    }
}
