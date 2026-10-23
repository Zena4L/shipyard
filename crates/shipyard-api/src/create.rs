//! `POST /containers/create`'s body, Docker's `ContainerCreateRequest`
//! (its config, `HostConfig` and `NetworkingConfig`), as the engine's
//! [`RunRequest`]. Every value is parsed by the parser the CLI's flag for it
//! uses. What Shipyard can't do yet is refused (400) with what to use
//! instead; fields it doesn't know are ignored, as Docker ignores them.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use shipyard_engine::containers::RestartPolicy;
use shipyard_engine::health::HealthCheck;
use shipyard_engine::network::{Joined, NetworkMode};
use shipyard_engine::run::{CapabilityArg, DeviceArg, Limits, RunRequest, RunStreams, SecurityOpt};
use shipyard_runtime::devices::{Access, DeviceRule};
use shipyard_runtime::process::Rlimit;
use shipyard_runtime::process::Signal;
use shipyard_runtime::resources::DockerLimits;

use crate::errors::ApiError;

mod body;

pub(crate) use body::{CreateBody, HostConfig, Words};
use body::{EndpointSettings, PortBinding};

/// A create, as the engine takes it.
#[derive(Debug)]
pub(crate) struct Create {
    /// The run asked for.
    pub(crate) request: RunRequest,
    /// Removed once it ends (`AutoRemove`, `--rm`).
    pub(crate) auto_remove: bool,
}

/// The create `body` asks for, named `name` (the query's, its leading `/`
/// taken off as Docker takes it off).
///
/// # Errors
///
/// A value that isn't one, or something Shipyard can't do (400).
pub(crate) fn request(body: &CreateBody, name: Option<&str>) -> Result<Create, ApiError> {
    let host = body.host_config.clone().unwrap_or_default();
    refuse_what_is_not_supported(&host)?;
    let mut request = RunRequest::default();
    request.name = name
        .map(|name| name.strip_prefix('/').unwrap_or(name))
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    request.image = Some(
        body.image
            .clone()
            .filter(|image| !image.is_empty())
            .ok_or_else(|| ApiError::bad("an Image is needed"))?,
    );
    the_command(body, &mut request);
    the_process(body, &mut request)?;
    the_host_config(&host, &mut request)?;
    the_anonymous_volumes(body, &mut request)?;
    request.network = the_network(body, &host)?;
    // Its own, or another container's (compose's `ipc: service:`).
    request.ipc =
        shipyard_engine::run::IpcMode::parse(host.ipc_mode.as_deref().unwrap_or_default())
            .map_err(|why| field("IpcMode", &why))?;
    request.limits = the_limits(&host)?;
    request.health = body.healthcheck.as_ref().map(health_check).transpose()?;
    Ok(Create {
        request,
        auto_remove: host.auto_remove.unwrap_or(false),
    })
}

/// What Shipyard can't do yet, refused with what to use instead: a
/// container made without it would not be the one asked for.
fn refuse_what_is_not_supported(host: &HostConfig) -> Result<(), ApiError> {
    let given = |list: &Option<Vec<String>>| list.as_ref().is_some_and(|list| !list.is_empty());
    let mode = |mode: &Option<String>, allowed: &[&str]| {
        mode.as_deref()
            .filter(|mode| !mode.is_empty() && !allowed.contains(mode))
            .map(str::to_owned)
    };
    let refusals = [
        (
            host.privileged == Some(true),
            "Privileged isn't supported: give the capabilities (CapAdd) and devices (Devices) \
             it needs"
                .to_owned(),
        ),
        (
            host.init == Some(true),
            "Init isn't supported yet: run the command without it".to_owned(),
        ),
        (
            given(&host.volumes_from),
            "VolumesFrom isn't supported: name the volumes (Binds)".to_owned(),
        ),
        (
            given(&host.links),
            "Links aren't supported: put the containers on one network, where they find each \
             other by name"
                .to_owned(),
        ),
        (
            host.volume_driver
                .as_deref()
                .is_some_and(|driver| !driver.is_empty()),
            "VolumeDriver isn't supported: Shipyard's volumes are local".to_owned(),
        ),
        (
            host.runtime
                .as_deref()
                .is_some_and(|runtime| !runtime.is_empty()),
            "Runtime isn't supported: Shipyard runs containers with its own".to_owned(),
        ),
        (
            host.log_config
                .as_ref()
                .and_then(|log| log.kind.as_deref())
                .is_some_and(|kind| !["", "json-file", "local"].contains(&kind)),
            "LogConfig's Type isn't supported: Shipyard keeps the logs itself (json-file, \
             local, or none given)"
                .to_owned(),
        ),
    ];
    if let Some((_, why)) = refusals.into_iter().find(|(refused, _)| *refused) {
        return Err(ApiError::bad(why));
    }
    if let Some(field) = unsupported_settings(host) {
        return Err(ApiError::bad(format!(
            "{field} isn't supported yet: create the container without it"
        )));
    }
    let namespaces = [
        ("PidMode", mode(&host.pid_mode, &[])),
        ("UTSMode", mode(&host.uts_mode, &[])),
        ("UsernsMode", mode(&host.userns_mode, &[])),
        ("CgroupnsMode", mode(&host.cgroupns_mode, &["private"])),
    ];
    if let Some((field, Some(value))) = namespaces.into_iter().find(|(_, value)| value.is_some()) {
        return Err(ApiError::bad(format!(
            "{field} {value:?} isn't supported: a container has namespaces of its own"
        )));
    }
    Ok(())
}

/// The first of Docker's settings Shipyard doesn't do yet that's given
/// (not at Docker's default: what its CLI sends when the flag isn't used).
fn unsupported_settings(host: &HostConfig) -> Option<&'static str> {
    let nonzero = |value: Option<i64>| value.is_some_and(|value| value != 0);
    let listed = |list: Option<&Vec<serde_json::Value>>| list.is_some_and(|list| !list.is_empty());
    let given = [
        ("ShmSize", nonzero(host.shm_size)),
        (
            "DeviceRequests (--gpus)",
            listed(host.device_requests.as_ref()),
        ),
        (
            "StorageOpt",
            host.storage_opt.as_ref().is_some_and(|opt| !opt.is_empty()),
        ),
        (
            "CgroupParent",
            host.cgroup_parent
                .as_deref()
                .is_some_and(|parent| !parent.is_empty()),
        ),
        ("OomKillDisable", host.oom_kill_disable == Some(true)),
        (
            "MemorySwappiness",
            host.memory_swappiness
                .is_some_and(|swappiness| swappiness != -1),
        ),
        // Null is Docker's default; a list (even an empty one) changes them.
        ("MaskedPaths", host.masked_paths.is_some()),
        ("ReadonlyPaths", host.readonly_paths.is_some()),
        (
            "BlkioWeightDevice",
            listed(host.blkio_weight_device.as_ref()),
        ),
        ("CpuRealtimePeriod", nonzero(host.cpu_realtime_period)),
        ("CpuRealtimeRuntime", nonzero(host.cpu_realtime_runtime)),
    ];
    given
        .into_iter()
        .find_map(|(field, given)| given.then_some(field))
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

/// The entrypoint and the command: Docker's entrypoint is a list, the
/// engine's one program, so its other words go before `Cmd`. An empty
/// entrypoint (`[]` or `[""]`) is none, the image's left out.
fn the_command(body: &CreateBody, request: &mut RunRequest) {
    let mut command: Vec<OsString> = Vec::new();
    if let Some(entrypoint) = &body.entrypoint {
        let mut words = entrypoint.0.iter();
        request.entrypoint = Some(words.next().map_or_else(OsString::new, OsString::from));
        command.extend(words.map(OsString::from));
    }
    if let Some(cmd) = &body.cmd {
        command.extend(cmd.0.iter().map(OsString::from));
    }
    request.command = command;
}

/// The process: its environment, user, directory, names, stop, streams.
fn the_process(body: &CreateBody, request: &mut RunRequest) -> Result<(), ApiError> {
    let nonempty = |text: &Option<String>| text.clone().filter(|text| !text.is_empty());
    request.env = body.env.iter().flatten().map(OsString::from).collect();
    request.user = nonempty(&body.user)
        .map(|user| shipyard_engine::run::parse_user(&user).map_err(|why| field("User", &why)))
        .transpose()?;
    request.workdir = nonempty(&body.working_dir).map(PathBuf::from);
    request.hostname = nonempty(&body.hostname);
    request.domainname = nonempty(&body.domainname);
    request.labels = body.labels.clone().unwrap_or_default();
    // Docker's `{"80/tcp": {}}`: the keys, checked by the engine.
    request.expose = body
        .exposed_ports
        .as_ref()
        .map(|ports| ports.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(signal) = nonempty(&body.stop_signal) {
        request.stop_signal = signal
            .parse::<Signal>()
            .map_err(|why| field("StopSignal", &why.to_string()))?;
    }
    if let Some(timeout) = body.stop_timeout {
        request.stop_timeout = u64::try_from(timeout)
            .map_err(|_| field("StopTimeout", &format!("{timeout} is negative")))?;
    }
    request.streams = RunStreams {
        interactive: body.open_stdin.unwrap_or(false),
        tty: body.tty.unwrap_or(false),
        // Its input ends at its first attached client's end (`StdinOnce`,
        // as `docker run -i` creates it), at each start: kept with the
        // container (its `created.json`, F116). With a terminal, its input
        // never ends: closing it is a hang-up.
        once: body.open_stdin == Some(true)
            && body.stdin_once == Some(true)
            && body.tty != Some(true),
    };
    Ok(())
}

/// One of `HostConfig.Mounts`, as `--mount` reads it (the engine's
/// [`MountRequest`](shipyard_engine::run::MountRequest), its checks
/// shared): what Shipyard can't do refused (a volume's driver, labels or
/// subpath, a tmpfs's own `Options`, and what `MountRequest` refuses);
/// with a bind's source to make, if it asks for one (`CreateMountpoint`).
fn mounted(
    mount: &body::Mount,
) -> Result<(shipyard_engine::run::DockerMount, Option<PathBuf>), ApiError> {
    let refuse = |what: &str| Err(field("Mounts", &format!("{what} isn't supported")));
    if let Some(volume) = &mount.volume_options
        && (volume
            .labels
            .as_ref()
            .is_some_and(|labels| !labels.is_empty())
            || volume
                .driver_config
                .as_ref()
                .is_some_and(|driver| !driver.is_null())
            || volume
                .subpath
                .as_deref()
                .is_some_and(|subpath| !subpath.is_empty()))
    {
        return refuse("a volume's labels, driver or subpath");
    }
    let mut asked = shipyard_engine::run::MountRequest::new(
        mount
            .kind
            .as_deref()
            .unwrap_or("volume")
            .to_ascii_lowercase(),
        mount.target.clone().unwrap_or_default(),
    );
    asked.source.clone_from(&mount.source);
    asked.read_only = mount.read_only.unwrap_or(false);
    if let Some(bind) = &mount.bind_options {
        asked.propagation.clone_from(&bind.propagation);
        asked.non_recursive = bind.non_recursive.unwrap_or(false);
        asked.create_mountpoint = bind.create_mountpoint.unwrap_or(false);
    }
    asked.no_copy = mount
        .volume_options
        .as_ref()
        .and_then(|volume| volume.no_copy);
    if let Some(tmpfs) = &mount.tmpfs_options {
        if tmpfs
            .options
            .as_ref()
            .is_some_and(|options| !options.is_empty())
        {
            return refuse("a tmpfs's own options (TmpfsOptions.Options)");
        }
        asked.tmpfs_size = tmpfs
            .size_bytes
            .filter(|&size| size != 0)
            .map(|size| size.to_string());
        asked.tmpfs_mode = tmpfs.mode.filter(|&mode| mode != 0).map(go_mode);
    }
    let resolved = asked
        .resolve()
        .map_err(|error| field("Mounts", &error.to_string()))?;
    Ok((resolved, asked.source_to_make().map(PathBuf::from)))
}

/// Go's `os.FileMode` as an octal `mode`: its permission bits, and its
/// set-user-ID, set-group-ID and sticky bits (Go's own, high up) where
/// `chmod(2)` has them; its other bits (the type) left out, as the
/// kernel's `&07777` leaves them in Docker's daemon.
fn go_mode(mode: u32) -> String {
    const SETUID: u32 = 1 << 23;
    const SETGID: u32 = 1 << 22;
    const STICKY: u32 = 1 << 20;
    let mut octal = mode & 0o7777;
    for (go, unix) in [(SETUID, 0o4000), (SETGID, 0o2000), (STICKY, 0o1000)] {
        if mode & go != 0 {
            octal |= unix;
        }
    }
    format!("{octal:o}")
}

/// `Volumes` (Docker's `-v /path`): each place no bind, mount or tmpfs
/// covers is an anonymous volume, made at the create (`/path` among the
/// request's volumes).
fn the_anonymous_volumes(body: &CreateBody, request: &mut RunRequest) -> Result<(), ApiError> {
    for path in body.volumes.iter().flatten().map(|(path, _)| path) {
        if !path.starts_with('/') || path.contains(':') {
            return Err(field(
                "Volumes",
                &format!("{path:?} isn't an absolute path without ':'"),
            ));
        }
        if !shipyard_engine::run::covers(
            &request.volume,
            &request.tmpfs,
            std::path::Path::new(path),
        ) {
            request.volume.push(path.clone());
        }
    }
    Ok(())
}

/// The host config's settings, but for the network and the limits.
fn the_host_config(host: &HostConfig, request: &mut RunRequest) -> Result<(), ApiError> {
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

/// Each of `values` parsed by `parse`, the first that isn't refused with
/// the field's name.
fn parsed_all<T, E: std::fmt::Display>(
    name: &str,
    values: Option<&Vec<String>>,
    parse: impl Fn(&str) -> Result<T, E>,
) -> Result<Vec<T>, ApiError> {
    values
        .into_iter()
        .flatten()
        .map(|text| parse(text).map_err(|why| field(name, &why.to_string())))
        .collect()
}

/// A field's value refused, saying which.
fn field(name: &str, why: &str) -> ApiError {
    ApiError::bad(format!("{name}: {why}"))
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

/// The networks: `NetworkMode` (`default` or none: the engine's default),
/// with its settings from `NetworkingConfig`'s one endpoint (API 1.43
/// takes one at create, as Docker's does).
fn the_network(body: &CreateBody, host: &HostConfig) -> Result<Vec<NetworkMode>, ApiError> {
    if body.network_disabled == Some(true) {
        return Ok(vec![NetworkMode::None]);
    }
    let endpoints = body
        .networking_config
        .as_ref()
        .and_then(|config| config.endpoints_config.as_ref());
    let mut endpoints: Vec<(&String, &EndpointSettings)> =
        endpoints.into_iter().flatten().collect();
    if endpoints.len() > 1 {
        return Err(field(
            "NetworkingConfig",
            "a container joins one network at create: connect it to the others after",
        ));
    }
    let mode = host.network_mode.as_deref().unwrap_or_default();
    let name = match mode {
        "host" => return only_mode(NetworkMode::Host, &endpoints),
        "none" => return only_mode(NetworkMode::None, &endpoints),
        "pasta" => return only_mode(NetworkMode::Pasta, &endpoints),
        // The endpoint names the network (`--network NAME`, an older
        // client's way); one named `default` (Docker's CLI since API 1.44
        // sends the mode's own name) is the default network, a name Docker
        // keeps for it.
        "" | "default" => match endpoints.first() {
            Some((name, _)) if name.as_str() != "default" => name.as_str(),
            // The default network takes none of an endpoint's settings
            // (Docker's: `--ip`, an alias, a hardware address need a
            // network of the user's): refused, never dropped.
            Some((_, settings)) if has_settings(settings)? => {
                return Err(field(
                    "NetworkingConfig",
                    "an address, alias or hardware address needs a network of your own \
                     (shipyard network create, then --network)",
                ));
            }
            _ if body
                .mac_address
                .as_deref()
                .is_some_and(|mac| !mac.is_empty()) =>
            {
                return Err(field(
                    "MacAddress",
                    "a hardware address needs a bridge network",
                ));
            }
            _ => return Ok(Vec::new()),
        },
        // Another container's (compose's `network_mode: service:`), as it
        // is: what can't go with it is the engine's to refuse.
        container if let Some(parsed) = NetworkMode::parse_container(container) => {
            if body
                .mac_address
                .as_deref()
                .is_some_and(|mac| !mac.is_empty())
            {
                return Err(field(
                    "MacAddress",
                    "a container that joins another's network has that one's addresses",
                ));
            }
            return match parsed {
                Ok(mode) => only_mode(mode, &endpoints),
                Err(why) => Err(field("NetworkMode", why)),
            };
        }
        name => name,
    };
    shipyard_network::store::validate_name(name)
        .map_err(|why| field("NetworkMode", &why.to_string()))?;
    let settings = match endpoints.pop() {
        Some((endpoint, settings)) if endpoint == name => Some(settings),
        Some((endpoint, _)) => {
            return Err(field(
                "NetworkingConfig",
                &format!("its endpoint {endpoint:?} isn't the NetworkMode's network {name:?}"),
            ));
        }
        None => None,
    };
    let mut joined = Joined {
        name: name.to_owned(),
        ..Joined::default()
    };
    if let Some(settings) = settings {
        endpoint(settings, &mut joined)?;
    }
    if joined.mac.is_none()
        && let Some(mac) = body.mac_address.as_deref().filter(|mac| !mac.is_empty())
    {
        joined.mac =
            Some(shipyard_network::store::parse_mac(mac).map_err(|why| field("MacAddress", &why))?);
    }
    Ok(vec![NetworkMode::Bridges(vec![joined])])
}

/// A network mode that takes no endpoint settings.
fn only_mode(
    mode: NetworkMode,
    endpoints: &[(&String, &EndpointSettings)],
) -> Result<Vec<NetworkMode>, ApiError> {
    if let Some((name, _)) = endpoints.first() {
        return Err(field(
            "NetworkingConfig",
            &format!("the endpoint {name:?} needs a bridge network: this mode takes none"),
        ));
    }
    Ok(vec![mode])
}

/// Whether `settings` set anything an endpoint takes (an address, a
/// hardware address, an alias).
fn has_settings(settings: &EndpointSettings) -> Result<bool, ApiError> {
    let mut joined = Joined::default();
    endpoint(settings, &mut joined)?;
    Ok(joined.ip.is_some()
        || joined.ip6.is_some()
        || joined.mac.is_some()
        || !joined.aliases.is_empty())
}

/// An endpoint's settings: its addresses, hardware address and aliases.
fn endpoint(settings: &EndpointSettings, joined: &mut Joined) -> Result<(), ApiError> {
    let ipam = settings.ipam_config.clone().unwrap_or_default();
    if let Some(ip) = ipam.ipv4_address.filter(|ip| !ip.is_empty()) {
        joined.ip = Some(
            ip.parse()
                .map_err(|_| field("IPv4Address", &format!("{ip:?} isn't an IP address")))?,
        );
    }
    if let Some(ip) = ipam.ipv6_address.filter(|ip| !ip.is_empty()) {
        joined.ip6 = Some(
            ip.parse()
                .map_err(|_| field("IPv6Address", &format!("{ip:?} isn't an IPv6 address")))?,
        );
    }
    if let Some(mac) = settings
        .mac_address
        .as_deref()
        .filter(|mac| !mac.is_empty())
    {
        joined.mac =
            Some(shipyard_network::store::parse_mac(mac).map_err(|why| field("MacAddress", &why))?);
    }
    joined.aliases = parsed_all(
        "Aliases",
        settings.aliases.as_ref(),
        shipyard_network::dns::parse_name,
    )?;
    Ok(())
}

/// The resource limits, by the CLI's own mapping (Docker's 0 is "not
/// set").
fn the_limits(host: &HostConfig) -> Result<Limits, ApiError> {
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

/// Docker's `Healthcheck`: its test (none: the image's; `["NONE"]`: no
/// check), its durations in nanoseconds (0: the default), its retries.
fn health_check(check: &body::Healthcheck) -> Result<HealthCheck, ApiError> {
    let millis = |name: &str, nanos: Option<i64>| {
        let nanos = u64::try_from(nanos.unwrap_or(0))
            .map_err(|_| field(name, "a duration can't be negative"))?;
        shipyard_engine::health::flag_millis(name, Some(Duration::from_nanos(nanos)))
            .map_err(|why| field("Healthcheck", &why.to_string()))
    };
    Ok(HealthCheck {
        test: check.test.clone().unwrap_or_default(),
        interval_ms: millis("Interval", check.interval)?,
        timeout_ms: millis("Timeout", check.timeout)?,
        start_period_ms: millis("StartPeriod", check.start_period)?,
        start_interval_ms: millis("StartInterval", check.start_interval)?,
        retries: u32::try_from(check.retries.unwrap_or(0))
            .map_err(|_| field("Retries", "it can't be negative"))?,
        shell: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
