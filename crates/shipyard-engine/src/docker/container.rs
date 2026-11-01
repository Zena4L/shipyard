//! A container as `GET /containers/{id}/json` (and `docker inspect`)
//! shows it.

use std::collections::BTreeMap;

use serde::Serialize;

use super::ZERO_TIME;
use crate::Engine;
use crate::containers::{self, Record};
use crate::error::Result;

mod config;
mod network;

pub(super) use config::mounts;
pub use config::{
    ContainerConfig, HealthConfig, HostConfig, LogConfig, MountPoint, PortBinding, RestartPolicy,
};
use config::{container_config, host_config, tmpfs_mount};
pub use network::{EndpointIpamConfig, EndpointSettings, NetworkSettings};
pub(super) use network::{LiveLease, endpoints, live_leases_by_container, network_mode};
use network::{live_leases, network_settings};

/// A container, Docker's `ContainerInspectResponse`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct ContainerInspect {
    /// Its full ID.
    pub id: String,
    /// When it was made (RFC 3339).
    pub created: String,
    /// The program it runs: its entrypoint's, else its command's, first
    /// word, its image's where it gave none (F101).
    pub path: String,
    /// That program's arguments.
    pub args: Vec<String>,
    /// Where it is in its life.
    pub state: ContainerState,
    /// The image it runs: its manifest's digest, as the store has it.
    pub image: String,
    /// Its last start's log directory (empty if it hasn't started).
    pub log_path: String,
    /// Its `/etc/resolv.conf` on the host, while it's up (empty for a start
    /// without files of its own, and after its end: its files go with it,
    /// F121).
    pub resolv_conf_path: String,
    /// Its `/etc/hostname` on the host, likewise.
    pub hostname_path: String,
    /// Its `/etc/hosts` on the host, likewise.
    pub hosts_path: String,
    /// Its name, as Docker writes it (`/web`).
    pub name: String,
    /// The restarts its policy made since its user last started it.
    pub restart_count: u32,
    /// The storage driver (Shipyard's layers are overlayfs').
    pub driver: &'static str,
    /// Where its layers are: its writable area's places, and its image's
    /// layers below (none before its first start).
    pub graph_driver: GraphDriver,
    /// Always `linux`.
    pub platform: &'static str,
    /// Its volumes and binds.
    pub mounts: Vec<MountPoint>,
    /// How it runs, as Docker's `Config`.
    pub config: ContainerConfig,
    /// How the host runs it, as Docker's `HostConfig`.
    pub host_config: HostConfig,
    /// Its networks and published ports.
    pub network_settings: NetworkSettings,
}

/// A container's layers, as Docker's `GraphDriverData` says them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct GraphDriver {
    /// Always `overlay`.
    pub name: &'static str,
    /// `LowerDir` (its image's layers, the top one first, `:`-joined, as
    /// overlay takes them), `UpperDir`, `WorkDir`, `MergedDir`; `null`
    /// before its writable area is made (its first start).
    pub data: Option<BTreeMap<String, String>>,
}

impl GraphDriver {
    /// The storage driver's name: Shipyard's layers are overlayfs'.
    const OVERLAY: &'static str = "overlay";
}

/// What a container's inspect reads besides its record: its writable
/// area's places, its image's config (its command, variables, ports:
/// F101). Gathered apart (it reads the image store), none of it needed to
/// show the rest.
#[derive(Debug, Clone, Default)]
pub(super) struct Extras {
    /// [`GraphDriver::data`].
    pub(super) layers: Option<BTreeMap<String, String>>,
    /// The ports it exposes (`80/tcp`): its image's, then its own
    /// (`--expose`).
    pub(super) image_ports: Vec<String>,
    /// Its image's runtime config, read by its digest (none: gone, or
    /// unreadable; the view shows what it was given).
    pub(super) image: Option<shipyard_image::RuntimeConfig>,
    /// Its tmpfs mounts (`--tmpfs`, `--mount type=tmpfs`).
    pub(super) tmpfs: Vec<crate::run::TmpfsMount>,
    /// The container whose network it joins (`--network container:`).
    pub(super) network_from: Option<containers::ContainerId>,
    /// The container whose IPC namespace it joins (`--ipc container:`).
    pub(super) ipc_from: Option<containers::ContainerId>,
    /// Its PID namespace, as Docker says it (`--pid`).
    pub(super) pid_mode: String,
    /// Its input closed once its first client's ends (`StdinOnce`).
    pub(super) stdin_once: bool,
    /// Where its `/etc` files are, while it's up ([`containers::etc_dir`]).
    pub(super) etc_dir: Option<std::path::PathBuf>,
}

/// [`Extras`] of `record`, its state `state`: what can be read of them (an
/// image store that can't be read, an image gone: none of that part).
fn extras(engine: &Engine, record: &Record, state: &containers::ContainerState) -> Extras {
    let etc_dir = containers::etc_dir(engine, record, state);
    // Its own (`--expose`, its tmpfs mounts), whatever the store says.
    // One that can't be read (its starts are refused) is said: what it
    // was made with can't be shown.
    let created = containers::created(engine, &record.id).unwrap_or_else(|error| {
        engine.warn(
            crate::report::WarningKind::Runtime,
            format!(
                "{error:#}: its exposed ports, tmpfs mounts and the namespaces it may join can't \
                 be shown"
            ),
        );
        containers::Created::default()
    });
    let pid_mode = match (&created.pid_from, created.pid_host) {
        (Some(other), _) => format!("container:{}", other.as_str()),
        (None, true) => "host".to_owned(),
        (None, false) => String::new(),
    };
    let (asked, tmpfs, network_from, ipc_from, stdin_once) = (
        created.exposed,
        created.tmpfs,
        created.network_from,
        created.ipc_from,
        created.stdin_once,
    );
    let Ok(store) = crate::image::open_store(engine) else {
        return Extras {
            image_ports: asked,
            tmpfs,
            network_from,
            ipc_from,
            pid_mode,
            stdin_once,
            etc_dir,
            ..Extras::default()
        };
    };
    let image = record
        .config
        .image
        .as_deref()
        .zip(record.image_id.as_deref())
        .and_then(|(reference, id)| crate::image::runtime_config(&store, reference, id));
    let ports = image.as_ref().map_or(&[][..], |image| &image.exposed_ports);
    Extras {
        layers: layers(&store, record),
        image_ports: containers::exposed_ports(ports, &asked),
        image,
        tmpfs,
        network_from,
        ipc_from,
        pid_mode,
        stdin_once,
        etc_dir,
    }
}

/// [`GraphDriver::data`] of `record`'s own area: read alone, without its
/// lock (none before its first start, or while it's made or removed).
fn layers(store: &shipyard_image::Store, record: &Record) -> Option<BTreeMap<String, String>> {
    let layers = shipyard_image::owned_area_layers(store, record.id.as_str()).ok()??;
    let area_name = shipyard_image::owned_area_name(record.id.as_str());
    let places = shipyard_image::AreaPlaces::of(store, &area_name);
    let shown = |path: std::path::PathBuf| path.display().to_string();
    Some(BTreeMap::from([
        (
            "LowerDir".to_owned(),
            lower_dir(&shipyard_image::layer_directories(store, &layers)),
        ),
        ("UpperDir".to_owned(), shown(places.upper)),
        ("WorkDir".to_owned(), shown(places.work)),
        ("MergedDir".to_owned(), shown(places.merged)),
    ]))
}

/// Overlay's `lowerdir` of `directories` (base first): the top one first,
/// `:`-joined.
fn lower_dir(directories: &[std::path::PathBuf]) -> String {
    let top_first: Vec<String> = directories
        .iter()
        .rev()
        .map(|dir| dir.display().to_string())
        .collect();
    top_first.join(":")
}

/// Docker's `ContainerState`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Docker's shape: one boolean per state"
)]
pub struct ContainerState {
    /// `created`, `running`, `paused`, `restarting` or `exited`.
    pub status: &'static str,
    /// Up: running, paused, stopping or waiting to restart (as Docker
    /// says of a restarting container too).
    pub running: bool,
    /// Its processes frozen.
    pub paused: bool,
    /// Waiting for its restart policy to start it again.
    pub restarting: bool,
    /// The out-of-memory killer ended it.
    #[serde(rename = "OOMKilled")]
    pub oom_killed: bool,
    /// Never: Shipyard has no state Docker calls dead.
    pub dead: bool,
    /// Its init's PID on the host, while it's up (0 otherwise).
    pub pid: i32,
    /// How it last ended (128 + N for signal N; 0 if it hasn't).
    pub exit_code: i32,
    /// Why it couldn't be started, Docker's field (always empty here: a
    /// start that fails says so to its caller).
    pub error: String,
    /// When it last started, or Docker's zero time.
    pub started_at: String,
    /// When it last ended, or Docker's zero time.
    pub finished_at: String,
    /// What its health checks say, if it has them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
}

/// Docker's `Health`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Health {
    /// `starting`, `healthy` or `unhealthy`.
    pub status: crate::health::Status,
    /// How many checks failed in a row.
    pub failing_streak: u32,
    /// The latest results, oldest first.
    pub log: Vec<HealthcheckResult>,
}

/// One check's result, Docker's `HealthcheckResult`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct HealthcheckResult {
    /// When it began (RFC 3339).
    pub start: String,
    /// When it ended.
    pub end: String,
    /// Its exit code (-1: it ran out of time).
    pub exit_code: i32,
    /// The start of its output.
    pub output: String,
}

/// Its `/etc` file `name`'s path on the host, while it's up; else empty.
fn etc_file(extras: &Extras, name: &str) -> String {
    extras
        .etc_dir
        .as_ref()
        .map(|dir| dir.join(name).display().to_string())
        .unwrap_or_default()
}

/// The container `reference` names, as Docker's `inspect` shows it.
///
/// # Errors
///
/// As [`containers::find`] and [`containers::state()`].
pub fn inspect_container(engine: &Engine, reference: &str) -> Result<ContainerInspect> {
    let gathered = containers::gathered(engine, reference)?;
    let leases = live_leases(engine, &gathered.record);
    let extras = extras(engine, &gathered.record, &gathered.state);
    Ok(docker_view(gathered, &leases, &extras))
}

/// What `gathered` shows, with the networks' live leases (`networks`) and
/// what's read besides the record (`extras`).
fn docker_view(
    gathered: containers::Gathered,
    leases: &[LiveLease],
    extras: &Extras,
) -> ContainerInspect {
    let state = state(&gathered);
    let containers::Gathered {
        record, log_dir, ..
    } = gathered;
    let config = &record.config;
    // What a start runs (F101): its program, then its arguments.
    let process = Process::of(&record, extras.image.as_ref());
    let mut command = process.entrypoint.iter().chain(&process.cmd).cloned();
    let path = command.next().unwrap_or_default();
    let args = command.collect();
    ContainerInspect {
        id: record.id.as_str().to_owned(),
        created: super::time(record.created),
        path,
        args,
        image: record.image_id.clone().unwrap_or_default(),
        log_path: log_dir.unwrap_or_default(),
        resolv_conf_path: etc_file(extras, "resolv.conf"),
        hostname_path: etc_file(extras, "hostname"),
        hosts_path: etc_file(extras, "hosts"),
        name: format!("/{}", record.name.as_str()),
        restart_count: record.restart_count,
        driver: GraphDriver::OVERLAY,
        graph_driver: GraphDriver {
            name: GraphDriver::OVERLAY,
            data: extras.layers.clone(),
        },
        platform: "linux",
        mounts: mounts(config)
            .into_iter()
            .chain(extras.tmpfs.iter().map(tmpfs_mount))
            .collect(),
        config: container_config(&record, extras, process),
        host_config: HostConfig {
            tmpfs: (!extras.tmpfs.is_empty()).then(|| {
                extras
                    .tmpfs
                    .iter()
                    .map(|mount| {
                        (
                            mount.destination().display().to_string(),
                            mount.options_text(),
                        )
                    })
                    .collect()
            }),
            network_mode: network_mode(config, extras.network_from.as_ref()),
            ipc_mode: extras.ipc_from.as_ref().map_or_else(
                || "private".to_owned(),
                |other| format!("container:{}", other.as_str()),
            ),
            pid_mode: extras.pid_mode.clone(),
            ..host_config(&record)
        },
        network_settings: network_settings(&record, &state, leases, extras.network_from.as_ref()),
        state,
    }
}

/// Docker's state of what's gathered: a container stopping, or waiting to
/// restart, is still running for Docker (moby's `SetRestarting`).
fn state(gathered: &containers::Gathered) -> ContainerState {
    use containers::ContainerState as Shipyard;
    let record = &gathered.record;
    let (exit_code, oom, finished) = match &gathered.state {
        Shipyard::Restarting { code, finished, .. } => (*code, false, *finished),
        Shipyard::Exited {
            code,
            oom,
            finished,
        } => (*code, oom.is_some(), *finished),
        _ => (None, false, None),
    };
    let up = gathered.state.is_active();
    let time =
        |at: Option<std::time::SystemTime>| at.map_or_else(|| ZERO_TIME.to_owned(), super::time);
    ContainerState {
        status: gathered.state.docker_word(),
        running: up,
        paused: matches!(gathered.state, Shipyard::Paused),
        restarting: matches!(gathered.state, Shipyard::Restarting { .. }),
        oom_killed: oom,
        dead: false,
        pid: gathered.pid.filter(|_| up).unwrap_or(0),
        exit_code: exit_code
            .or_else(|| record.exit.as_ref().and_then(|exit| exit.code))
            .unwrap_or(0),
        error: String::new(),
        started_at: time(record.started),
        finished_at: time(finished.or(record.finished)),
        health: gathered.health.as_ref().map(|health| Health {
            status: health.status,
            failing_streak: health.failing_streak,
            log: health
                .log
                .iter()
                .map(|probe| HealthcheckResult {
                    start: probe.start.clone(),
                    end: probe.end.clone(),
                    exit_code: probe.exit_code,
                    output: probe.output.clone(),
                })
                .collect(),
        }),
    }
}

/// The process a container's starts run, as `inspect`'s `Config` shows
/// it (F101): its own settings, its image's where it gave none, by the
/// start's own rules (`RunRequest::entrypoint_and_cmd`, `shown_env`).
struct Process {
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    env: Vec<String>,
    workdir: String,
    user: String,
}

impl Process {
    /// `record`'s, with its image's `image` config (none: what it was
    /// given, as a record keeps it).
    fn of(record: &Record, image: Option<&shipyard_image::RuntimeConfig>) -> Self {
        let config = &record.config;
        let (entrypoint, cmd) = containers::shown_entrypoint_and_cmd(record, image);
        let env = record
            .to_request()
            .map_or_else(|_| config.env.clone(), |request| request.shown_env(image));
        let given_user = config.user.as_ref().map(|user| match &user.group {
            Some(group) => format!("{}:{group}", user.name),
            None => user.name.clone(),
        });
        Self {
            entrypoint,
            cmd,
            env,
            workdir: config
                .workdir
                .clone()
                .or_else(|| image.and_then(|image| image.working_dir.clone()))
                .unwrap_or_default(),
            user: given_user
                .or_else(|| image.and_then(|image| image.user.clone()))
                .unwrap_or_default(),
        }
    }
}

/// `list`, or `null` when it's empty: Go's empty slice, as Docker writes
/// its unset lists.
fn nil_if_empty<T>(list: Vec<T>) -> Option<Vec<T>> {
    (!list.is_empty()).then_some(list)
}

/// A port as Docker keys it: `80/tcp`.
fn port_key(port: u16, protocol: &str) -> String {
    format!("{port}/{protocol}")
}

#[cfg(test)]
mod tests;
