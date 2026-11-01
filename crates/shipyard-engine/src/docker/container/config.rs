//! A container's `Config` and `HostConfig` as Docker's inspect shows them: its
//! process, its mounts, its limits, its restart policy and ports.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::containers::{self, Config, Record};

use super::network::network_mode;
use super::{Extras, Process, nil_if_empty, port_key};

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
    /// Empty for its own, `host`, or `container:ID` for another container's
    /// PID namespace (Docker's words).
    pub pid_mode: String,
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

/// Its mounts, from its `--volume`s, read by the rule a run reads them by
/// (`VolumeMount::parse`: a bare name is a named volume, an absolute path a
/// bind); one that isn't a mount (none is, once made) left out.
pub(in crate::docker) fn mounts(config: &Config) -> Vec<MountPoint> {
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
pub(super) fn tmpfs_mount(mount: &crate::run::TmpfsMount) -> MountPoint {
    MountPoint {
        kind: "tmpfs",
        name: String::new(),
        source: String::new(),
        destination: mount.destination().display().to_string(),
        mode: String::new(),
        rw: !mount.read_only(),
    }
}

/// Docker's `Config` of `record`'s.
pub(super) fn container_config(
    record: &Record,
    extras: &Extras,
    process: Process,
) -> ContainerConfig {
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
pub(super) fn host_config(record: &Record) -> HostConfig {
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
        pid_mode: String::new(),
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
