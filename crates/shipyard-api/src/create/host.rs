//! A create body's `HostConfig` as the run's flags: its ports, restart policy,
//! devices, security options and limits, each through the CLI's own parser.

use std::collections::BTreeMap;
use std::path::PathBuf;

use shipyard_engine::containers::RestartPolicy;
use shipyard_engine::run::{CapabilityArg, DeviceArg, Limits, RunRequest, SecurityOpt};
use shipyard_runtime::devices::{Access, DeviceRule};
use shipyard_runtime::process::Rlimit;
use shipyard_runtime::resources::DockerLimits;

use crate::errors::ApiError;

use super::body::{HostConfig, PortBinding};
use super::mounts::mounted;
use super::{body, field, parsed_all};

/// The host config's settings, but for the network and the limits.
pub(super) fn the_host_config(host: &HostConfig, request: &mut RunRequest) -> Result<(), ApiError> {
    request.volume = host.binds.clone().unwrap_or_default();
    // A bind's missing host path made, as Docker's daemon makes one for
    // `Binds` (compose's short syntax); the CLI's `-v` keeps refusing it.
    request.make_sources = request
        .volume
        .iter()
        .filter_map(|spec| shipyard_engine::run::bind_source(spec))
        .collect();
    for (path, options) in host.tmpfs.iter().flatten() {
        let options = options
            .split(',')
            .filter(|option| !option.is_empty())
            .map(str::to_owned);
        request.tmpfs.push(
            shipyard_engine::run::TmpfsMount::new(path, options)
                .map_err(|error| field("Tmpfs", &error.to_string()))?,
        );
    }
    for mount in host.mounts.iter().flatten() {
        let (resolved, to_make) = mounted(mount)?;
        request.make_sources.extend(to_make);
        match resolved {
            shipyard_engine::run::DockerMount::Volume(spec) => request.volume.push(spec),
            shipyard_engine::run::DockerMount::Tmpfs(tmpfs) => request.tmpfs.push(tmpfs),
        }
    }
    request.publish = published(host.port_bindings.as_ref())?;
    request.publish_all = host.publish_all_ports.unwrap_or(false);
    request.restart = restart_policy(host)?;
    request.read_only = host.readonly_rootfs.unwrap_or(false);
    request.cap_add = parsed_all("CapAdd", host.cap_add.as_ref(), str::parse::<CapabilityArg>)?;
    request.cap_drop = parsed_all(
        "CapDrop",
        host.cap_drop.as_ref(),
        str::parse::<CapabilityArg>,
    )?;
    request.security_opt = parsed_all("SecurityOpt", host.security_opt.as_ref(), security_opt)?;
    request.dns = parsed_all("Dns", host.dns.as_ref(), |text| {
        text.parse::<std::net::IpAddr>()
            .map_err(|_| format!("{text:?} isn't an IP address"))
    })?;
    request.dns_search = parsed_all(
        "DnsSearch",
        host.dns_search.as_ref(),
        shipyard_network::files::parse_resolv_word,
    )?;
    request.dns_option = parsed_all(
        "DnsOptions",
        host.dns_options.as_ref(),
        shipyard_network::files::parse_resolv_word,
    )?;
    request.add_host = parsed_all(
        "ExtraHosts",
        host.extra_hosts.as_ref(),
        shipyard_network::files::parse_host_entry,
    )?;
    request.group_add = parsed_all(
        "GroupAdd",
        host.group_add.as_ref(),
        shipyard_engine::run::parse_name_or_id,
    )?;
    // Docker's 0 is "not set".
    request.oom_score_adj = host.oom_score_adj.filter(|&score| score != 0);
    request.sysctl = host
        .sysctls
        .iter()
        .flatten()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    request.ulimit = host
        .ulimits
        .iter()
        .flatten()
        .map(|limit| {
            format!("{}={}:{}", limit.name, limit.soft, limit.hard)
                .parse::<Rlimit>()
                .map_err(|why| field("Ulimits", &why.to_string()))
        })
        .collect::<Result<_, _>>()?;
    request.device = host
        .devices
        .iter()
        .flatten()
        .map(device)
        .collect::<Result<_, _>>()?;
    request.device_cgroup_rule = parsed_all(
        "DeviceCgroupRules",
        host.device_cgroup_rules.as_ref(),
        |text| {
            text.parse::<DeviceRule>()
                .map_err(|why: shipyard_runtime::devices::DeviceError| why.to_string())
        },
    )?;
    Ok(())
}

/// A security option as Docker's client sends it, read by `--security-opt`'s
/// parser: a seccomp profile must be a file's absolute path (it's read where
/// the service runs); Docker's CLI sends the profile's JSON itself, which
/// isn't supported yet.
fn security_opt(text: &str) -> Result<SecurityOpt, String> {
    if let Some(profile) = text
        .strip_prefix("seccomp=")
        .or_else(|| text.strip_prefix("seccomp:"))
    {
        if profile.trim_start().starts_with('{') {
            return Err(
                "a seccomp profile given inline (Docker's CLI sends the file's JSON) isn't \
                 supported yet: give unconfined, or leave Shipyard's own"
                    .to_owned(),
            );
        }
        if profile != "unconfined" && !profile.starts_with('/') {
            return Err(format!(
                "seccomp={profile}: give the profile's absolute path (it's read where the \
                 service runs)"
            ));
        }
    }
    text.parse()
}

/// `PortBindings` (`{"80/tcp": [{"HostIp": "", "HostPort": "8080"}]}`) as
/// `-p` specs, read by `-p`'s parser: an empty host port is one the kernel
/// picks, an empty address all of them.
fn published(
    bindings: Option<&BTreeMap<String, Option<Vec<PortBinding>>>>,
) -> Result<Vec<shipyard_network::ports::PortRequest>, ApiError> {
    let mut requests = Vec::new();
    for (port, bindings) in bindings.into_iter().flatten() {
        let none = [PortBinding::default()];
        let bindings = match bindings {
            Some(list) if !list.is_empty() => list.as_slice(),
            _ => &none,
        };
        for binding in bindings {
            let ip = binding.host_ip.as_deref().unwrap_or_default();
            let host_port = binding.host_port.as_deref().unwrap_or_default();
            let ip = if ip.contains(':') {
                format!("[{ip}]")
            } else {
                ip.to_owned()
            };
            let spec = match (ip.is_empty(), host_port.is_empty()) {
                (true, true) => port.clone(),
                (true, false) => format!("{host_port}:{port}"),
                (false, _) => format!("{ip}:{host_port}:{port}"),
            };
            let parsed = shipyard_network::ports::parse_publish(&spec)
                .map_err(|why| field("PortBindings", &why.to_string()))?;
            requests.extend(parsed);
        }
    }
    Ok(requests)
}

/// `RestartPolicy`: its name, and the retries of `on-failure`.
pub(crate) fn restart_policy(host: &HostConfig) -> Result<RestartPolicy, ApiError> {
    let Some(policy) = &host.restart_policy else {
        return Ok(RestartPolicy::No);
    };
    let name = policy.name.as_deref().unwrap_or_default();
    let retries = policy.maximum_retry_count.unwrap_or(0);
    let text = match name {
        "" => "no".to_owned(),
        "on-failure" if retries > 0 => format!("on-failure:{retries}"),
        _ if retries != 0 => {
            return Err(field(
                "RestartPolicy",
                &format!("MaximumRetryCount is for on-failure, not {name:?}"),
            ));
        }
        other => other.to_owned(),
    };
    text.parse()
        .map_err(|why: shipyard_engine::Error| field("RestartPolicy", &why.to_string()))
}

/// One of `Devices`: Docker's `PathOnHost`, `PathInContainer` (the host's
/// path if empty) and `CgroupPermissions` (`rwm` if empty).
fn device(device: &body::Device) -> Result<DeviceArg, ApiError> {
    let host = device.path_on_host.as_deref().unwrap_or_default();
    if !host.starts_with('/') {
        return Err(field(
            "Devices",
            &format!("{host:?}: the host's device is an absolute path"),
        ));
    }
    let container = device
        .path_in_container
        .as_deref()
        .filter(|path| !path.is_empty() && *path != host);
    if let Some(container) = container.filter(|path| !path.starts_with('/')) {
        return Err(field(
            "Devices",
            &format!("{container:?}: the path in the container is absolute"),
        ));
    }
    let access = match device.cgroup_permissions.as_deref() {
        None | Some("") => Access::ALL,
        Some(perms) => Access::parse(perms).map_err(|why| field("Devices", &why.to_string()))?,
    };
    Ok(DeviceArg {
        host: PathBuf::from(host),
        container: container.map(PathBuf::from),
        access,
    })
}

/// The resource limits, by the CLI's own mapping (Docker's 0 is "not
/// set").
pub(super) fn the_limits(host: &HostConfig) -> Result<Limits, ApiError> {
    let limits = docker_limits(host)?;
    let given = limits != DockerLimits::default();
    Ok(Limits {
        first_given: given.then_some("HostConfig's resource limits"),
        resources: limits.resources().map_err(|why| why.to_string()),
        // Docker's CLI sends 0 without `--pids-limit`: unset, as null is
        // (the default); -1 asks for none.
        pids_given: host.pids_limit.is_some_and(|limit| limit != 0),
    })
}

/// `host`'s resource fields as Docker's limits (a create's, an update's:
/// one mapping), each 0 "not set".
///
/// # Errors
///
/// A negative size or count, or a device's rate without its path (400).
pub(crate) fn docker_limits(host: &HostConfig) -> Result<DockerLimits, ApiError> {
    let unsigned = |name: &str, value: Option<i64>| {
        value
            .filter(|&value| value != 0)
            .map(|value| {
                u64::try_from(value).map_err(|_| field(name, &format!("{value} is negative")))
            })
            .transpose()
    };
    let throttle = |name: &str, devices: &Option<Vec<body::ThrottleDevice>>| {
        devices
            .iter()
            .flatten()
            .map(|device| match &device.path {
                Some(path) if !path.is_empty() => Ok((PathBuf::from(path), device.rate)),
                _ => Err(field(name, "a device needs its Path")),
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let mut limits = DockerLimits::default();
    limits.memory = unsigned("Memory", host.memory)?;
    limits.memory_reservation = unsigned("MemoryReservation", host.memory_reservation)?;
    limits.memory_swap = host.memory_swap.filter(|&total| total != 0);
    limits.cpus = host.nano_cpus.and_then(DockerLimits::nano_cpus);
    limits.cpu_period = unsigned("CpuPeriod", host.cpu_period)?;
    limits.cpu_quota = host.cpu_quota.filter(|&quota| quota != 0);
    limits.cpu_shares = unsigned("CpuShares", host.cpu_shares)?;
    limits.cpuset_cpus = host.cpuset_cpus.clone().filter(|cpus| !cpus.is_empty());
    limits.cpuset_mems = host.cpuset_mems.clone().filter(|mems| !mems.is_empty());
    limits.pids_limit = host.pids_limit.filter(|&limit| limit != 0);
    limits.blkio_weight = host.blkio_weight.filter(|&weight| weight != 0);
    limits.device_read_bps = throttle("BlkioDeviceReadBps", &host.blkio_device_read_bps)?;
    limits.device_write_bps = throttle("BlkioDeviceWriteBps", &host.blkio_device_write_bps)?;
    limits.device_read_iops = throttle("BlkioDeviceReadIOps", &host.blkio_device_read_iops)?;
    limits.device_write_iops = throttle("BlkioDeviceWriteIOps", &host.blkio_device_write_iops)?;
    Ok(limits)
}
