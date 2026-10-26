//! `POST /containers/create`'s body, Docker's `ContainerCreateRequest`
//! (its config, `HostConfig` and `NetworkingConfig`), as the engine's
//! [`RunRequest`]. Every value is parsed by the parser the CLI's flag for it
//! uses. What Shipyard can't do yet is refused (400) with what to use
//! instead; fields it doesn't know are ignored, as Docker ignores them.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use shipyard_engine::health::HealthCheck;
use shipyard_engine::run::{RunRequest, RunStreams};
use shipyard_runtime::process::Signal;

use crate::errors::ApiError;

mod body;

pub(crate) use body::{CreateBody, HostConfig, Words};

mod host;
mod mounts;
mod network;

pub(crate) use host::{docker_limits, restart_policy};
use host::{the_host_config, the_limits};
use mounts::the_anonymous_volumes;
use network::the_network;

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

#[cfg(test)]
mod tests;
