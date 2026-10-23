//! A container as `GET /containers/{id}/json` (and `docker inspect`)
//! shows it.

use std::collections::BTreeMap;

use serde::Serialize;

use super::ZERO_TIME;
use crate::Engine;
use crate::containers::{self, Config, Network, Record};
use crate::error::Result;

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

/// How a container's logs are kept (Docker's `LogConfig`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct LogConfig {
    /// Always `json-file`: the driver Docker's clients know (`logs` works).
    /// The files at `LogPath` are Shipyard's own records, not Docker's
    /// json-file lines.
    #[serde(rename = "Type")]
    pub kind: &'static str,
    /// Its settings: none of a user's (Shipyard's defaults).
    pub config: BTreeMap<String, String>,
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

/// A mount, Docker's `MountPoint`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct MountPoint {
    /// `volume` or `bind`.
    #[serde(rename = "Type")]
    pub kind: &'static str,
    /// The volume's name (empty for a bind).
    pub name: String,
    /// The host path (a bind's), or empty.
    pub source: String,
    /// Where it is in the container.
    pub destination: String,
    /// `ro` or empty.
    pub mode: String,
    /// Writable.
    #[serde(rename = "RW")]
    pub rw: bool,
}

/// Docker's container `Config`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Docker's shape: the streams' flags"
)]
pub struct ContainerConfig {
    /// Its hostname (Docker's default: the ID's first 12 characters).
    pub hostname: String,
    /// Its domain name.
    pub domainname: String,
    /// Its user (`name[:group]`), or empty.
    pub user: String,
    /// Stdin attached (`-i`).
    pub attach_stdin: bool,
    /// Stdout attached.
    pub attach_stdout: bool,
    /// Stderr attached.
    pub attach_stderr: bool,
    /// On a terminal (`-t`).
    pub tty: bool,
    /// Stdin kept open (`-i`).
    pub open_stdin: bool,
    /// Stdin closed after one attach.
    pub stdin_once: bool,
    /// Its environment, `KEY=VALUE`s.
    pub env: Vec<String>,
    /// Its command (its image's where it gave none, and none if its own
    /// entrypoint replaced the image's), or `null` for none.
    pub cmd: Option<Vec<String>>,
    /// The image, as given.
    pub image: String,
    /// Its working directory, or empty.
    pub working_dir: String,
    /// Its entrypoint (its image's where it gave none), or `null` for
    /// none.
    pub entrypoint: Option<Vec<String>>,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// The ports it publishes, `80/tcp` with an empty object each.
    pub exposed_ports: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    /// Its health check, if it has one of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,
    /// The signal that stops it.
    pub stop_signal: String,
    /// How long it's given to stop, in seconds.
    pub stop_timeout: u64,
}

/// Docker's `HealthConfig`: times in nanoseconds.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct HealthConfig {
    /// `["CMD-SHELL", …]`, `["CMD", …]` or `["NONE"]`.
    pub test: Vec<String>,
    /// Between two checks.
    pub interval: u64,
    /// The longest a check may take.
    pub timeout: u64,
    /// After the start, how long failures don't count.
    pub start_period: u64,
    /// Between two checks during the start period.
    pub start_interval: u64,
    /// Failures in a row that make it unhealthy.
    pub retries: u32,
}

/// Docker's `HostConfig`, the part Shipyard fills.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct HostConfig {
    /// Its binds and volumes, as given (`SOURCE:DEST[:ro]`).
    pub binds: Option<Vec<String>>,
    /// Its tmpfs mounts: where, and their options as given (left out
    /// without any, as Docker's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmpfs: Option<BTreeMap<String, String>>,
    /// Its logs: Shipyard's own, Docker's `json-file`'s files.
    pub log_config: LogConfig,
    /// Its published ports: `80/tcp` to where on the host.
    pub port_bindings: BTreeMap<String, Vec<PortBinding>>,
    /// Its restart policy.
    pub restart_policy: RestartPolicy,
    /// Removed once it ends (`--rm`).
    pub auto_remove: bool,
    /// `bridge` (or the network's name), `host`, `none` or `pasta`.
    pub network_mode: String,
    /// `private`, or `container:ID` for another container's IPC namespace.
    pub ipc_mode: String,
    /// Capabilities added.
    pub cap_add: Option<Vec<String>>,
    /// Capabilities dropped.
    pub cap_drop: Option<Vec<String>>,
    /// DNS servers.
    pub dns: Vec<String>,
    /// DNS options.
    pub dns_options: Vec<String>,
    /// DNS search domains.
    pub dns_search: Vec<String>,
    /// `/etc/hosts` entries, `name:ip`.
    pub extra_hosts: Option<Vec<String>>,
    /// Groups added.
    pub group_add: Option<Vec<String>>,
    /// Its OOM score adjustment.
    pub oom_score_adj: i32,
    /// Its root filesystem read-only.
    pub readonly_rootfs: bool,
    /// `no-new-privileges`, `seccomp=…`.
    pub security_opt: Option<Vec<String>>,
    /// Never (Shipyard has no `--privileged`).
    pub privileged: bool,
    /// Memory limit in bytes (0: none).
    pub memory: u64,
    /// Memory reservation in bytes (0: none).
    pub memory_reservation: u64,
    /// CPU period in microseconds (0: the kernel's).
    pub cpu_period: u64,
    /// CPU quota in microseconds (0: none).
    pub cpu_quota: u64,
    /// CPUs it may use.
    pub cpuset_cpus: String,
    /// Memory nodes it may use.
    pub cpuset_mems: String,
    /// Most processes (`null`: none).
    pub pids_limit: Option<u64>,
    /// Its rlimits.
    pub ulimits: Vec<serde_json::Value>,
    /// Its kernel settings.
    pub sysctls: BTreeMap<String, String>,
}

/// One host address a port is published on.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct PortBinding {
    /// The host address (empty: every one).
    pub host_ip: String,
    /// The host port (empty: a free one).
    pub host_port: String,
}

/// Docker's `RestartPolicy`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct RestartPolicy {
    /// `no`, `always`, `unless-stopped` or `on-failure`.
    pub name: &'static str,
    /// `on-failure`'s most restarts (0: no limit).
    pub maximum_retry_count: u32,
}

/// Docker's `NetworkSettings`, the part Shipyard fills.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct NetworkSettings {
    /// Each port it exposes, `80/tcp`, with where it's published now
    /// (`null`: not published, or not up).
    pub ports: BTreeMap<String, Option<Vec<PortBinding>>>,
    /// Its bridge networks, by name, with its addresses there now.
    pub networks: BTreeMap<String, EndpointSettings>,
}

/// A container on one network, Docker's `EndpointSettings`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct EndpointSettings {
    /// Its other names there (`null`: none).
    pub aliases: Option<Vec<String>>,
    /// The network's gateway (empty while it isn't up).
    pub gateway: String,
    /// Its IPv4 address there (empty while it isn't up).
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    /// Its IPv4 prefix length.
    #[serde(rename = "IPPrefixLen")]
    pub ip_prefix_len: u8,
    /// Its IPv6 address there.
    #[serde(rename = "GlobalIPv6Address")]
    pub global_ipv6_address: String,
    /// Its IPv6 prefix length.
    #[serde(rename = "GlobalIPv6PrefixLen")]
    pub global_ipv6_prefix_len: u8,
    /// The network's IPv6 gateway.
    #[serde(rename = "IPv6Gateway")]
    pub ipv6_gateway: String,
    /// Its hardware address there.
    pub mac_address: String,
    /// The network's ID ([`super::network_id`]), while it's on it.
    #[serde(rename = "NetworkID")]
    pub network_id: String,
    /// Its endpoint's ID ([`super::endpoint_id`]), while it's on it.
    #[serde(rename = "EndpointID")]
    pub endpoint_id: String,
    /// The addresses it asked for there (`--ip`, `--ip6`), or `null`.
    #[serde(rename = "IPAMConfig")]
    pub ipam_config: Option<EndpointIpamConfig>,
}

/// The addresses a container asked for on a network (Docker's
/// `EndpointIPAMConfig`).
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct EndpointIpamConfig {
    /// Its IPv4 address there, or empty.
    #[serde(rename = "IPv4Address")]
    pub ipv4_address: String,
    /// Its IPv6 address there, or empty.
    #[serde(rename = "IPv6Address")]
    pub ipv6_address: String,
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

/// Its mounts, from its `--volume`s, read by the rule a run reads them by
/// (`VolumeMount::parse`: a bare name is a named volume, an absolute path a
/// bind); one that isn't a mount (none is, once made) left out.
pub(super) fn mounts(config: &Config) -> Vec<MountPoint> {
    config
        .volume
        .iter()
        .filter_map(|given| shipyard_storage::volume::VolumeMount::parse(given).ok())
        .map(|mount| {
            let named = shipyard_storage::volume::is_named_volume(&mount.source);
            MountPoint {
                kind: if named { "volume" } else { "bind" },
                name: if named {
                    mount.source.clone()
                } else {
                    String::new()
                },
                source: if named { String::new() } else { mount.source },
                destination: mount.destination.display().to_string(),
                mode: if mount.read_only { "ro" } else { "" }.to_owned(),
                rw: !mount.read_only,
            }
        })
        .collect()
}

/// A tmpfs mount, as Docker's `Mounts` shows one: no source, writable
/// unless `ro`.
fn tmpfs_mount(mount: &crate::run::TmpfsMount) -> MountPoint {
    MountPoint {
        kind: "tmpfs",
        name: String::new(),
        source: String::new(),
        destination: mount.destination().display().to_string(),
        mode: String::new(),
        rw: !mount.read_only(),
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

/// Docker's `Config` of `record`'s.
fn container_config(record: &Record, extras: &Extras, process: Process) -> ContainerConfig {
    let config = &record.config;
    ContainerConfig {
        hostname: config
            .hostname
            .clone()
            .unwrap_or_else(|| record.id.short().to_owned()),
        domainname: config.domainname.clone().unwrap_or_default(),
        user: process.user,
        attach_stdin: config.interactive,
        attach_stdout: true,
        attach_stderr: true,
        tty: config.tty,
        open_stdin: config.interactive,
        stdin_once: extras.stdin_once,
        env: process.env,
        cmd: (!process.cmd.is_empty()).then_some(process.cmd),
        image: config.image.clone().unwrap_or_default(),
        working_dir: process.workdir,
        entrypoint: (!process.entrypoint.is_empty()).then_some(process.entrypoint),
        labels: record.labels.as_map().clone(),
        // Its image's own, and those it publishes.
        exposed_ports: extras
            .image_ports
            .iter()
            .cloned()
            .chain(
                config
                    .publish
                    .iter()
                    .map(|port| port_key(port.container_port, &port.protocol)),
            )
            .map(|port| (port, serde_json::Map::new()))
            .collect(),
        healthcheck: config.health.as_ref().map(|check| HealthConfig {
            test: check.test.clone(),
            interval: check.interval_ms.saturating_mul(1_000_000),
            timeout: check.timeout_ms.saturating_mul(1_000_000),
            start_period: check.start_period_ms.saturating_mul(1_000_000),
            start_interval: check.start_interval_ms.saturating_mul(1_000_000),
            retries: check.retries,
        }),
        stop_signal: config.stop_signal.clone(),
        stop_timeout: config.stop_timeout,
    }
}

/// Docker's `HostConfig` of `record`'s.
fn host_config(record: &Record) -> HostConfig {
    let config = &record.config;
    let limits = &config.resources;
    let restart = match config.restart {
        containers::RestartPolicy::No => ("no", 0),
        containers::RestartPolicy::Always => ("always", 0),
        containers::RestartPolicy::UnlessStopped => ("unless-stopped", 0),
        containers::RestartPolicy::OnFailure { max } => ("on-failure", max.unwrap_or(0)),
    };
    HostConfig {
        binds: nil_if_empty(config.volume.clone()),
        // Beside the record (`created.json`): the view adds them.
        tmpfs: None,
        log_config: LogConfig {
            kind: "json-file",
            config: BTreeMap::new(),
        },
        port_bindings: port_bindings(config),
        restart_policy: RestartPolicy {
            name: restart.0,
            maximum_retry_count: restart.1,
        },
        auto_remove: record.auto_remove,
        network_mode: network_mode(config, None),
        ipc_mode: "private".to_owned(),
        cap_add: nil_if_empty(config.cap_add.clone()),
        cap_drop: nil_if_empty(config.cap_drop.clone()),
        // (Docker writes these three as lists, empty or not.)
        dns: config.dns.iter().map(ToString::to_string).collect(),
        dns_options: config.dns_option.clone(),
        dns_search: config.dns_search.clone(),
        extra_hosts: nil_if_empty(
            config
                .add_host
                .iter()
                .map(|host| format!("{}:{}", host.name, host.ip))
                .collect(),
        ),
        group_add: nil_if_empty(config.group_add.clone()),
        oom_score_adj: config.oom_score_adj.unwrap_or(0),
        readonly_rootfs: config.read_only,
        security_opt: nil_if_empty(config.security_opt.iter().map(security_word).collect()),
        privileged: false,
        memory: limits.memory_max.unwrap_or(0),
        memory_reservation: limits.memory_low.unwrap_or(0),
        cpu_period: limits.cpu_period.unwrap_or(0),
        cpu_quota: limits.cpu_quota.unwrap_or(0),
        cpuset_cpus: limits.cpuset_cpus.clone().unwrap_or_default(),
        cpuset_mems: limits.cpuset_mems.clone().unwrap_or_default(),
        pids_limit: limits.pids_max,
        ulimits: config
            .ulimit
            .iter()
            .map(|ulimit| {
                serde_json::json!({
                    "Name": ulimit.kind,
                    "Soft": ulimit.soft.map_or(-1, |soft| i64::try_from(soft).unwrap_or(i64::MAX)),
                    "Hard": ulimit.hard.map_or(-1, |hard| i64::try_from(hard).unwrap_or(i64::MAX)),
                })
            })
            .collect(),
        sysctls: config
            .sysctl
            .iter()
            .map(|sysctl| (sysctl.key.clone(), sysctl.value.clone()))
            .collect(),
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

/// A `--security-opt` as Docker writes it.
fn security_word(option: &containers::Security) -> String {
    match option {
        containers::Security::NoNewPrivileges(true) => "no-new-privileges".to_owned(),
        containers::Security::NoNewPrivileges(false) => "no-new-privileges=false".to_owned(),
        containers::Security::SeccompUnconfined => "seccomp=unconfined".to_owned(),
        containers::Security::SeccompProfile(path) => format!("seccomp={path}"),
    }
}

/// Its `--publish`es, as Docker's `PortBindings`.
fn port_bindings(config: &Config) -> BTreeMap<String, Vec<PortBinding>> {
    let mut bindings: BTreeMap<String, Vec<PortBinding>> = BTreeMap::new();
    for port in &config.publish {
        bindings
            .entry(port_key(port.container_port, &port.protocol))
            .or_default()
            .push(PortBinding {
                host_ip: port.host_ip.map(|ip| ip.to_string()).unwrap_or_default(),
                host_port: port
                    .host_port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            });
    }
    bindings
}

/// Its network mode, Docker's word: its first network's, or
/// `container:ID` for one that joins `network_from`'s.
pub(super) fn network_mode(
    config: &Config,
    network_from: Option<&containers::ContainerId>,
) -> String {
    if let Some(other) = network_from {
        return format!("container:{}", other.as_str());
    }
    match config.network.first() {
        Some(Network::Host) => "host".to_owned(),
        Some(Network::None) => "none".to_owned(),
        Some(Network::Pasta) => "pasta".to_owned(),
        Some(Network::Bridges(bridges)) => bridges
            .first()
            .map_or_else(|| "bridge".to_owned(), |bridge| bridge.name.clone()),
        // Not given: the one its start gets ([`crate::network::defaulted`]).
        None => match crate::network::defaulted() {
            crate::network::Defaulted::Bridge => "bridge",
            crate::network::Defaulted::Host => "host",
            crate::network::Defaulted::Pasta => "pasta",
        }
        .to_owned(),
    }
}

/// The networks `record`'s container has a live lease on, each with it:
/// read without making the store, taking a lock, or failing the view for
/// a network that can't be read (left out). Root's networks only (a
/// rootless container has none).
pub(super) fn live_leases(engine: &Engine, record: &Record) -> Vec<LiveLease> {
    let Some(networks) = existing_networks(engine) else {
        return Vec::new();
    };
    let found = networks.leases_of(record.id.as_str());
    with_ids(&networks, found, &mut BTreeMap::new())
}

/// [`live_leases`] of every container at once, by its ID: for a listing,
/// which reads each network once (11.7), not once a container.
pub(super) fn live_leases_by_container(engine: &Engine) -> BTreeMap<String, Vec<LiveLease>> {
    let Some(networks) = existing_networks(engine) else {
        return BTreeMap::new();
    };
    let mut ids = BTreeMap::new();
    networks
        .live_leases_by_container()
        .into_iter()
        .map(|(container, found)| (container, with_ids(&networks, found, &mut ids)))
        .collect()
}

/// The networks, read without making the store (a rootless engine has
/// none).
fn existing_networks(engine: &Engine) -> Option<shipyard_network::store::Networks> {
    engine
        .paths()
        .networks()
        .ok()
        .and_then(shipyard_network::store::Networks::existing)
}

/// `found`'s leases with their networks' IDs, each network's read once
/// (`ids`, by its name).
fn with_ids(
    networks: &shipyard_network::store::Networks,
    found: Vec<(
        shipyard_network::store::NetworkConfig,
        shipyard_network::store::Lease,
    )>,
    ids: &mut BTreeMap<String, String>,
) -> Vec<LiveLease> {
    found
        .into_iter()
        .map(|(config, lease)| {
            let network_id = ids
                .entry(config.name.clone())
                .or_insert_with(|| {
                    // Labels that can't be read: the ID derived, as a
                    // network's of before.
                    let meta = networks.meta(&config.name).unwrap_or_default();
                    super::network_id(&config, &meta)
                })
                .clone();
            LiveLease {
                config,
                lease,
                network_id,
            }
        })
        .collect()
}

/// A container's live lease on a network, with the network's ID.
pub(super) struct LiveLease {
    /// The network.
    pub(super) config: shipyard_network::store::NetworkConfig,
    /// The lease.
    pub(super) lease: shipyard_network::store::Lease,
    /// The network's ID.
    pub(super) network_id: String,
}

/// Docker's `NetworkSettings`: the ports it exposes, published where its
/// start bound them while it's up; each network it joins, with its live
/// addresses (from its `leases`) while it has them.
fn network_settings(
    record: &Record,
    state: &ContainerState,
    leases: &[LiveLease],
    network_from: Option<&containers::ContainerId>,
) -> NetworkSettings {
    endpoints(record, state.running, leases, network_from)
}

/// What `record`'s container exposes and joins: as Docker, no ports until
/// it's `up`, then each it exposes with where it's published (`None`:
/// exposed, not published); each network it joins, with its live addresses
/// (from `leases`) while it has them. One that joins another's network
/// (`network_from`) has neither, as Docker shows it: they're the other's.
pub(super) fn endpoints(
    record: &Record,
    up: bool,
    leases: &[LiveLease],
    network_from: Option<&containers::ContainerId>,
) -> NetworkSettings {
    if network_from.is_some() {
        return NetworkSettings {
            ports: BTreeMap::new(),
            networks: BTreeMap::new(),
        };
    }
    let mut ports: BTreeMap<String, Option<Vec<PortBinding>>> = BTreeMap::new();
    if up {
        ports.extend(
            record
                .config
                .publish
                .iter()
                .map(|port| (port_key(port.container_port, &port.protocol), None)),
        );
        for bound in &record.ports {
            ports
                .entry(port_key(bound.container_port, &bound.protocol))
                .or_insert(None)
                .get_or_insert_with(Vec::new)
                .push(PortBinding {
                    host_ip: bound.host_ip.to_string(),
                    host_port: bound.host_port.to_string(),
                });
        }
    }
    let mut endpoints: BTreeMap<String, EndpointSettings> = BTreeMap::new();
    // Not on a bridge network it names: its mode's own entry (`host`,
    // `none`, or the default network it gets), as Docker shows it. Pasta
    // isn't one of Docker's networks: none.
    let mode = network_mode(&record.config, None);
    if !matches!(record.config.network.first(), Some(Network::Bridges(_))) && mode != "pasta" {
        endpoints.insert(mode, EndpointSettings::default());
    }
    for network in &record.config.network {
        if let Network::Bridges(bridges) = network {
            for bridge in bridges {
                let asked =
                    (bridge.ip.is_some() || bridge.ip6.is_some()).then(|| EndpointIpamConfig {
                        ipv4_address: bridge.ip.map(|ip| ip.to_string()).unwrap_or_default(),
                        ipv6_address: bridge.ip6.map(|ip| ip.to_string()).unwrap_or_default(),
                    });
                endpoints.insert(
                    bridge.name.clone(),
                    EndpointSettings {
                        aliases: nil_if_empty(bridge.aliases.clone()),
                        ipam_config: asked,
                        ..EndpointSettings::default()
                    },
                );
            }
        }
    }
    for LiveLease {
        config,
        lease,
        network_id,
    } in leases
    {
        let endpoint = endpoints.entry(config.name.clone()).or_default();
        endpoint.gateway = config.gateway.to_string();
        endpoint.ip_address = lease.ip.to_string();
        endpoint.ip_prefix_len = config.subnet.prefix();
        endpoint.global_ipv6_address = lease.ip6.map(|ip| ip.to_string()).unwrap_or_default();
        endpoint.global_ipv6_prefix_len = config.subnet6.map_or(0, |subnet| subnet.prefix());
        endpoint.ipv6_gateway = config.gateway6.map(|ip| ip.to_string()).unwrap_or_default();
        endpoint.mac_address = shipyard_network::store::format_mac(lease.mac);
        endpoint.aliases = nil_if_empty(lease.aliases.clone());
        endpoint.network_id.clone_from(network_id);
        endpoint.endpoint_id = super::endpoint_id(network_id, record.id.as_str());
    }
    NetworkSettings {
        ports,
        networks: endpoints,
    }
}

#[cfg(test)]
mod tests;
