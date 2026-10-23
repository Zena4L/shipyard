//! Docker's `ContainerCreateRequest` as it comes (API 1.43's names): every
//! field optional, `null` as good as absent, the fields Shipyard doesn't
//! read left out (serde skips them, as Docker skips the ones it doesn't
//! know).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};

/// The request's body: a container's config, with its `HostConfig` and
/// `NetworkingConfig`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct CreateBody {
    pub(crate) hostname: Option<String>,
    pub(crate) domainname: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) tty: Option<bool>,
    pub(crate) open_stdin: Option<bool>,
    pub(crate) stdin_once: Option<bool>,
    pub(crate) env: Option<Vec<String>>,
    pub(crate) cmd: Option<Words>,
    pub(crate) healthcheck: Option<Healthcheck>,
    pub(crate) entrypoint: Option<Words>,
    pub(crate) image: Option<String>,
    pub(crate) labels: Option<BTreeMap<String, String>>,
    pub(crate) exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
    pub(crate) volumes: Option<BTreeMap<String, serde_json::Value>>,
    pub(crate) working_dir: Option<String>,
    pub(crate) network_disabled: Option<bool>,
    pub(crate) mac_address: Option<String>,
    pub(crate) stop_signal: Option<String>,
    pub(crate) stop_timeout: Option<i64>,
    pub(crate) host_config: Option<HostConfig>,
    pub(crate) networking_config: Option<NetworkingConfig>,
}

/// A list of words that may come as one string (Docker's `StrSlice`:
/// `"sh"` is `["sh"]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Words(pub(crate) Vec<String>);

impl<'de> Deserialize<'de> for Words {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            One(String),
            Many(Vec<String>),
        }
        Ok(Self(match Either::deserialize(deserializer)? {
            Either::One(word) => vec![word],
            Either::Many(words) => words,
        }))
    }
}

/// Docker's `HealthConfig`: durations in nanoseconds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct Healthcheck {
    pub(crate) test: Option<Vec<String>>,
    pub(crate) interval: Option<i64>,
    pub(crate) timeout: Option<i64>,
    pub(crate) start_period: Option<i64>,
    pub(crate) start_interval: Option<i64>,
    pub(crate) retries: Option<i64>,
}

/// Docker's `HostConfig`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct HostConfig {
    pub(crate) binds: Option<Vec<String>>,
    pub(crate) network_mode: Option<String>,
    pub(crate) port_bindings: Option<BTreeMap<String, Option<Vec<PortBinding>>>>,
    pub(crate) restart_policy: Option<RestartPolicy>,
    pub(crate) auto_remove: Option<bool>,
    pub(crate) volume_driver: Option<String>,
    pub(crate) volumes_from: Option<Vec<String>>,
    pub(crate) mounts: Option<Vec<Mount>>,
    pub(crate) cap_add: Option<Vec<String>>,
    pub(crate) cap_drop: Option<Vec<String>>,
    pub(crate) cgroupns_mode: Option<String>,
    pub(crate) dns: Option<Vec<String>>,
    pub(crate) dns_options: Option<Vec<String>>,
    pub(crate) dns_search: Option<Vec<String>>,
    pub(crate) extra_hosts: Option<Vec<String>>,
    pub(crate) group_add: Option<Vec<String>>,
    pub(crate) ipc_mode: Option<String>,
    pub(crate) links: Option<Vec<String>>,
    pub(crate) oom_score_adj: Option<i32>,
    pub(crate) pid_mode: Option<String>,
    pub(crate) privileged: Option<bool>,
    pub(crate) publish_all_ports: Option<bool>,
    pub(crate) readonly_rootfs: Option<bool>,
    pub(crate) security_opt: Option<Vec<String>>,
    pub(crate) tmpfs: Option<BTreeMap<String, String>>,
    #[serde(rename = "UTSMode")]
    pub(crate) uts_mode: Option<String>,
    pub(crate) userns_mode: Option<String>,
    pub(crate) sysctls: Option<BTreeMap<String, String>>,
    pub(crate) runtime: Option<String>,
    pub(crate) init: Option<bool>,
    pub(crate) log_config: Option<LogConfig>,
    pub(crate) ulimits: Option<Vec<Ulimit>>,
    pub(crate) devices: Option<Vec<Device>>,
    pub(crate) device_cgroup_rules: Option<Vec<String>>,
    pub(crate) memory: Option<i64>,
    pub(crate) memory_reservation: Option<i64>,
    pub(crate) memory_swap: Option<i64>,
    pub(crate) nano_cpus: Option<i64>,
    pub(crate) cpu_period: Option<i64>,
    pub(crate) cpu_quota: Option<i64>,
    pub(crate) cpu_shares: Option<i64>,
    pub(crate) cpuset_cpus: Option<String>,
    pub(crate) cpuset_mems: Option<String>,
    pub(crate) pids_limit: Option<i64>,
    pub(crate) blkio_weight: Option<u16>,
    pub(crate) blkio_device_read_bps: Option<Vec<ThrottleDevice>>,
    pub(crate) blkio_device_write_bps: Option<Vec<ThrottleDevice>>,
    #[serde(rename = "BlkioDeviceReadIOps")]
    pub(crate) blkio_device_read_iops: Option<Vec<ThrottleDevice>>,
    #[serde(rename = "BlkioDeviceWriteIOps")]
    pub(crate) blkio_device_write_iops: Option<Vec<ThrottleDevice>>,
    // Known to Docker, not done by Shipyard: refused unless at Docker's
    // defaults (`create::unsupported_settings`).
    pub(crate) shm_size: Option<i64>,
    pub(crate) device_requests: Option<Vec<serde_json::Value>>,
    pub(crate) storage_opt: Option<BTreeMap<String, String>>,
    pub(crate) cgroup_parent: Option<String>,
    pub(crate) oom_kill_disable: Option<bool>,
    pub(crate) memory_swappiness: Option<i64>,
    pub(crate) masked_paths: Option<Vec<String>>,
    pub(crate) readonly_paths: Option<Vec<String>>,
    pub(crate) blkio_weight_device: Option<Vec<serde_json::Value>>,
    pub(crate) cpu_realtime_period: Option<i64>,
    pub(crate) cpu_realtime_runtime: Option<i64>,
}

/// One of `PortBindings`' bindings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct PortBinding {
    pub(crate) host_ip: Option<String>,
    pub(crate) host_port: Option<String>,
}

/// `RestartPolicy`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct RestartPolicy {
    pub(crate) name: Option<String>,
    pub(crate) maximum_retry_count: Option<i64>,
}

/// `LogConfig`: only its driver is read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct LogConfig {
    #[serde(rename = "Type")]
    pub(crate) kind: Option<String>,
}

/// One of `Ulimits`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct Ulimit {
    pub(crate) name: String,
    pub(crate) soft: i64,
    pub(crate) hard: i64,
}

/// One of `Devices`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct Device {
    pub(crate) path_on_host: Option<String>,
    pub(crate) path_in_container: Option<String>,
    pub(crate) cgroup_permissions: Option<String>,
}

/// One of the `BlkioDevice…` limits.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct ThrottleDevice {
    pub(crate) path: Option<String>,
    pub(crate) rate: u64,
}

/// `NetworkingConfig`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct NetworkingConfig {
    pub(crate) endpoints_config: Option<BTreeMap<String, EndpointSettings>>,
}

/// One of `EndpointsConfig`'s endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct EndpointSettings {
    #[serde(rename = "IPAMConfig")]
    pub(crate) ipam_config: Option<IpamConfig>,
    pub(crate) aliases: Option<Vec<String>>,
    pub(crate) mac_address: Option<String>,
}

/// An endpoint's addresses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct IpamConfig {
    #[serde(rename = "IPv4Address")]
    pub(crate) ipv4_address: Option<String>,
    #[serde(rename = "IPv6Address")]
    pub(crate) ipv6_address: Option<String>,
}

/// One of Docker's `HostConfig.Mounts`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct Mount {
    #[serde(rename = "Type")]
    pub(crate) kind: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) read_only: Option<bool>,
    pub(crate) bind_options: Option<BindOptions>,
    pub(crate) volume_options: Option<VolumeOptions>,
    pub(crate) tmpfs_options: Option<TmpfsOptions>,
}

/// A bind mount's options.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct BindOptions {
    pub(crate) propagation: Option<String>,
    pub(crate) non_recursive: Option<bool>,
    pub(crate) create_mountpoint: Option<bool>,
}

/// A volume mount's options.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct VolumeOptions {
    pub(crate) no_copy: Option<bool>,
    pub(crate) labels: Option<BTreeMap<String, String>>,
    pub(crate) driver_config: Option<serde_json::Value>,
    pub(crate) subpath: Option<String>,
}

/// A tmpfs mount's options: its size in bytes, its mode (Go's
/// `os.FileMode`, a number: `1023` is `01777`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct TmpfsOptions {
    pub(crate) size_bytes: Option<i64>,
    pub(crate) mode: Option<u32>,
    pub(crate) options: Option<Vec<Vec<String>>>,
}
