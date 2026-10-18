//! A container's [`Config`] made from the request asked ([`RunRequest`]),
//! and the request a start runs made from it again: each value in a form
//! of this module's own.
#![cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "a start makes its request again, and starts are Linux's"
    )
)]

use std::path::PathBuf;

use shipyard_network::files::HostEntry;
use shipyard_network::ports::{PortRequest, Protocol};
use shipyard_runtime::devices::{Access, DeviceKind, DeviceRule};
use shipyard_runtime::isolation::IdMap;
use shipyard_runtime::process::{Rlimit, RlimitKind, RlimitValue, Signal};
use shipyard_runtime::resources::{CpuMax, IoMax, Limit, Resources};
use shipyard_runtime::security::Capability;

use super::super::names::Name;
use super::{
    Amount, Bridge, CgroupLimits, Config, Device, DeviceCgroupRule, Host, IdRange, IoLimit,
    Network, Port, Security, Sysctl, Ulimit, User,
};
use crate::error::{Result, bail};
use crate::image::PullPolicy;
use crate::network::{Joined, NetworkMode};
use crate::run::{CapabilityArg, DeviceArg, Limits, RunRequest, RunStreams, SecurityOpt};

impl Config {
    /// The config to keep of `request`. Its bare `--env KEY`s take their
    /// values from its caller's environment now (one that's unset is left
    /// out, as a run leaves it out); its `name` is the record's, not kept
    /// here. Its limits are kept, and which flag asked for the first of
    /// them isn't: what a record keeps is run where limits work.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a
    /// value a record can't keep (an argument, a variable or a path that
    /// isn't UTF-8: JSON has no bytes), a bare `--env` name a run would
    /// refuse (empty, or with spaces), or limits that couldn't be worked
    /// out.
    #[allow(
        clippy::too_many_lines,
        reason = "a field-by-field mirror of the request, kept in one place"
    )]
    pub fn from_request(request: &RunRequest) -> Result<Self> {
        // Every field named, none `..`: a field added to the request must
        // be kept here too (or said not to be).
        let RunRequest {
            streams: RunStreams { interactive, tty },
            env,
            caller_env,
            workdir,
            user,
            group_add,
            ulimit,
            oom_score_adj,
            device,
            device_cgroup_rule,
            stop_signal,
            stop_timeout,
            restart,
            health,
            no_isolation,
            hostname,
            domainname,
            sysctl,
            network,
            // The record's own name (and its name on its networks).
            name: _,
            network_alias,
            ip,
            ip6,
            mac_address,
            add_host,
            dns,
            dns_search,
            dns_option,
            publish,
            publish_all,
            rootfs,
            image,
            entrypoint,
            pull,
            read_only,
            cap_add,
            cap_drop,
            security_opt,
            uidmap,
            gidmap,
            bundle,
            limits,
            volume,
            command,
            // A cache of what the default network is, worked out from the
            // rest: nothing asked.
            default_mode: _,
            detached: _,
            container: _,
        } = request;
        // A check with nothing to run is refused at once, not at each start.
        if let Some(check) = health {
            check.command()?;
        }
        let resources = match &limits.resources {
            Ok(resources) => resources.clone(),
            Err(why) => bail!(InvalidRequest, "{why}"),
        };
        Ok(Self {
            interactive: *interactive,
            tty: *tty,
            env: resolved_env(env, caller_env)?,
            workdir: workdir.as_deref().map(path_text).transpose()?,
            user: user.as_ref().map(|(name, group)| User {
                name: name.clone(),
                group: group.clone(),
            }),
            group_add: group_add.clone(),
            ulimit: ulimit.iter().map(Ulimit::from).collect(),
            oom_score_adj: *oom_score_adj,
            device: device.iter().map(Device::from_arg).collect::<Result<_>>()?,
            device_cgroup_rule: device_cgroup_rule
                .iter()
                .map(DeviceCgroupRule::from)
                .collect(),
            stop_signal: signal_text(*stop_signal),
            stop_timeout: *stop_timeout,
            restart: *restart,
            health: health.clone(),
            no_isolation: *no_isolation,
            hostname: hostname.clone(),
            domainname: domainname.clone(),
            sysctl: sysctl
                .iter()
                .map(|(key, value)| Sysctl {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            network: network.iter().map(Network::from).collect(),
            network_alias: network_alias.clone(),
            ip: *ip,
            ip6: *ip6,
            mac_address: mac_address.map(mac_text),
            add_host: add_host
                .iter()
                .map(|entry| Host {
                    name: entry.name.clone(),
                    ip: entry.ip,
                })
                .collect(),
            dns: dns.clone(),
            dns_search: dns_search.clone(),
            dns_option: dns_option.clone(),
            publish: publish.iter().map(Port::from).collect(),
            publish_all: *publish_all,
            rootfs: rootfs.as_deref().map(path_text).transpose()?,
            image: image.clone(),
            entrypoint: entrypoint.as_deref().map(utf8).transpose()?,
            pull: pull_text(pull.unwrap_or(PullPolicy::Missing)).to_owned(),
            read_only: *read_only,
            cap_add: cap_add.iter().copied().map(capability_text).collect(),
            cap_drop: cap_drop.iter().copied().map(capability_text).collect(),
            security_opt: security_opt
                .iter()
                .map(Security::from_opt)
                .collect::<Result<_>>()?,
            uidmap: uidmap.iter().map(IdRange::from).collect(),
            gidmap: gidmap.iter().map(IdRange::from).collect(),
            bundle: bundle.as_deref().map(path_text).transpose()?,
            resources: CgroupLimits::from(&resources),
            volume: volume.clone(),
            command: command.iter().map(|arg| utf8(arg)).collect::<Result<_>>()?,
        })
    }

    /// The request this config asks for, for the container named `name`:
    /// what a start runs.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Storage`](crate::ErrorKind::Storage): a value in the
    /// record this Shipyard can't take (a signal, a capability, a pull
    /// policy or an rlimit it doesn't know, or a mangled address).
    pub(crate) fn to_request(&self, name: &Name) -> Result<RunRequest> {
        let capabilities = |names: &[String]| {
            names
                .iter()
                .map(|name| parse_capability(name))
                .collect::<Result<_>>()
        };
        // Every field named, none `..`, as `from_request`.
        Ok(RunRequest {
            default_mode: std::cell::OnceCell::new(),
            detached: false,
            container: None,
            streams: RunStreams {
                interactive: self.interactive,
                tty: self.tty,
            },
            env: self.env.iter().map(Into::into).collect(),
            caller_env: Vec::new(),
            workdir: self.workdir.as_ref().map(PathBuf::from),
            user: self
                .user
                .as_ref()
                .map(|user| (user.name.clone(), user.group.clone())),
            group_add: self.group_add.clone(),
            ulimit: self
                .ulimit
                .iter()
                .map(Ulimit::to_rlimit)
                .collect::<Result<_>>()?,
            oom_score_adj: self.oom_score_adj,
            device: self
                .device
                .iter()
                .map(Device::to_arg)
                .collect::<Result<_>>()?,
            device_cgroup_rule: self
                .device_cgroup_rule
                .iter()
                .map(DeviceCgroupRule::to_rule)
                .collect::<Result<_>>()?,
            stop_signal: stored(self.stop_signal.parse(), "stop signal", &self.stop_signal)?,
            stop_timeout: self.stop_timeout,
            restart: self.restart,
            health: self.health.clone(),
            no_isolation: self.no_isolation,
            hostname: self.hostname.clone(),
            domainname: self.domainname.clone(),
            sysctl: self
                .sysctl
                .iter()
                .map(|sysctl| (sysctl.key.clone(), sysctl.value.clone()))
                .collect(),
            network: self
                .network
                .iter()
                .map(Network::to_mode)
                .collect::<Result<_>>()?,
            name: Some(name.to_string()),
            network_alias: self.network_alias.clone(),
            ip: self.ip,
            ip6: self.ip6,
            mac_address: self.mac_address.as_deref().map(parse_mac).transpose()?,
            add_host: self
                .add_host
                .iter()
                .map(|host| HostEntry {
                    name: host.name.clone(),
                    ip: host.ip,
                })
                .collect(),
            dns: self.dns.clone(),
            dns_search: self.dns_search.clone(),
            dns_option: self.dns_option.clone(),
            publish: self
                .publish
                .iter()
                .map(Port::to_request)
                .collect::<Result<_>>()?,
            publish_all: self.publish_all,
            rootfs: self.rootfs.as_ref().map(PathBuf::from),
            image: self.image.clone(),
            entrypoint: self.entrypoint.as_ref().map(Into::into),
            // `missing` stands for "not given": only an image's is read.
            pull: if self.image.is_some() {
                Some(parse_pull(&self.pull)?)
            } else {
                None
            },
            read_only: self.read_only,
            cap_add: capabilities(&self.cap_add)?,
            cap_drop: capabilities(&self.cap_drop)?,
            security_opt: self.security_opt.iter().map(Security::to_opt).collect(),
            uidmap: self.uidmap.iter().map(IdRange::to_map).collect(),
            gidmap: self.gidmap.iter().map(IdRange::to_map).collect(),
            bundle: self.bundle.as_ref().map(PathBuf::from),
            // What's refused off Linux names the first limit flag; a record
            // is made where the limits work, and needs no name for them.
            limits: Limits {
                first_given: None,
                resources: Ok(self.resources.to_resources()),
            },
            volume: self.volume.clone(),
            command: self.command.iter().map(Into::into).collect(),
        })
    }
}

/// `env`, its bare `KEY`s given their values from `caller`.
fn resolved_env(
    env: &[std::ffi::OsString],
    caller: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<Vec<String>> {
    let mut resolved = Vec::new();
    for entry in env {
        let entry = utf8(entry)?;
        if entry.contains('=') {
            resolved.push(entry);
            continue;
        }
        // As a run refuses them (`add_env`): a typo like `TOKEN secret`
        // would put the secret in the "name", which isn't said.
        if entry.is_empty() {
            bail!(InvalidRequest, "invalid --env: the variable name is empty");
        }
        if entry.bytes().any(|b| b.is_ascii_whitespace()) {
            bail!(
                InvalidRequest,
                "invalid --env: names can't contain spaces (did you mean KEY=VALUE?)"
            );
        }
        if let Some((_, value)) = caller.iter().find(|(key, _)| *key == *entry) {
            resolved.push(format!("{entry}={}", utf8(value)?));
        }
    }
    Ok(resolved)
}

fn utf8(text: &std::ffi::OsStr) -> Result<String> {
    match text.to_str() {
        Some(text) => Ok(text.to_owned()),
        None => bail!(
            InvalidRequest,
            "{text:?} isn't UTF-8, which a container's record can't keep"
        ),
    }
}

fn path_text(path: &std::path::Path) -> Result<String> {
    utf8(path.as_os_str())
}

/// A value from the record, or the error for one this Shipyard can't take.
fn stored<T, E: std::fmt::Display>(
    parsed: std::result::Result<T, E>,
    what: &str,
    text: &str,
) -> Result<T> {
    match parsed {
        Ok(value) => Ok(value),
        Err(err) => bail!(
            Storage,
            "the container's record has a {what} this Shipyard can't take ({text:?}: {err})"
        ),
    }
}

fn signal_text(signal: Signal) -> String {
    signal
        .name()
        .map_or_else(|| signal.as_raw().to_string(), std::borrow::Cow::into_owned)
}

fn mac_text(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let bytes: Option<Vec<u8>> = text
        .split(':')
        .map(|byte| {
            (byte.len() == 2)
                .then(|| u8::from_str_radix(byte, 16).ok())
                .flatten()
        })
        .collect();
    match bytes.and_then(|bytes| <[u8; 6]>::try_from(bytes).ok()) {
        Some(mac) => Ok(mac),
        None => stored(Err("not six hex bytes"), "hardware address", text),
    }
}

fn pull_text(pull: PullPolicy) -> &'static str {
    match pull {
        PullPolicy::Missing => "missing",
        PullPolicy::Always => "always",
        PullPolicy::Never => "never",
    }
}

fn parse_pull(text: &str) -> Result<PullPolicy> {
    match text {
        "missing" => Ok(PullPolicy::Missing),
        "always" => Ok(PullPolicy::Always),
        "never" => Ok(PullPolicy::Never),
        _ => stored(Err("not missing, always or never"), "pull policy", text),
    }
}

fn capability_text(capability: CapabilityArg) -> String {
    match capability {
        CapabilityArg::All => "ALL".to_owned(),
        CapabilityArg::One(capability) => capability.to_string(),
    }
}

fn parse_capability(text: &str) -> Result<CapabilityArg> {
    if text == "ALL" {
        return Ok(CapabilityArg::All);
    }
    stored(text.parse::<Capability>(), "capability", text).map(CapabilityArg::One)
}

impl From<&Rlimit> for Ulimit {
    fn from(limit: &Rlimit) -> Self {
        let value = |value| match value {
            RlimitValue::Finite(value) => Some(value),
            RlimitValue::Unlimited => None,
        };
        Self {
            kind: limit.kind.to_string(),
            soft: value(limit.soft),
            hard: value(limit.hard),
        }
    }
}

impl Ulimit {
    fn to_rlimit(&self) -> Result<Rlimit> {
        let value = |value: Option<u64>| value.map_or(RlimitValue::Unlimited, RlimitValue::Finite);
        let kind: RlimitKind = stored(self.kind.parse(), "rlimit", &self.kind)?;
        stored(
            Rlimit::new(kind, value(self.soft), value(self.hard)),
            "rlimit",
            &self.kind,
        )
    }
}

impl Device {
    fn from_arg(device: &DeviceArg) -> Result<Self> {
        Ok(Self {
            host: path_text(&device.host)?,
            container: device.container.as_deref().map(path_text).transpose()?,
            access: device.access.to_string(),
        })
    }

    fn to_arg(&self) -> Result<DeviceArg> {
        Ok(DeviceArg {
            host: PathBuf::from(&self.host),
            container: self.container.as_ref().map(PathBuf::from),
            access: stored(Access::parse(&self.access), "device access", &self.access)?,
        })
    }
}

impl From<&DeviceRule> for DeviceCgroupRule {
    fn from(rule: &DeviceRule) -> Self {
        Self {
            allow: rule.allow,
            kind: rule.kind.map_or('a', DeviceKind::letter).to_string(),
            major: rule.major,
            minor: rule.minor,
            access: rule.access.to_string(),
        }
    }
}

impl DeviceCgroupRule {
    fn to_rule(&self) -> Result<DeviceRule> {
        let kind = match self.kind.as_str() {
            "a" => None,
            "c" => Some(DeviceKind::Char),
            "b" => Some(DeviceKind::Block),
            _ => return stored(Err("not c, b or a"), "device kind", &self.kind),
        };
        let access = stored(Access::parse(&self.access), "device access", &self.access)?;
        Ok(DeviceRule::new(
            self.allow, kind, self.major, self.minor, access,
        ))
    }
}

impl From<&IdMap> for IdRange {
    fn from(map: &IdMap) -> Self {
        Self {
            container: map.container,
            host: map.host,
            size: map.size,
        }
    }
}

impl IdRange {
    fn to_map(&self) -> IdMap {
        IdMap {
            container: self.container,
            host: self.host,
            size: self.size,
        }
    }
}

impl From<&Resources> for CgroupLimits {
    fn from(resources: &Resources) -> Self {
        let amount = |limit: Limit| match limit {
            Limit::Max => Amount::Max,
            Limit::Value(value) => Amount::Value(value),
        };
        Self {
            memory_max: resources.memory_max,
            memory_low: resources.memory_low,
            swap_max: resources.swap_max.map(amount),
            swap_is_default: resources.swap_is_default,
            cpu_quota: resources.cpu_max.and_then(|cpu| cpu.quota),
            cpu_period: resources.cpu_max.and_then(|cpu| cpu.period),
            cpu_max: resources.cpu_max.is_some(),
            cpu_weight: resources.cpu_weight,
            cpuset_cpus: resources.cpuset_cpus.clone(),
            cpuset_mems: resources.cpuset_mems.clone(),
            pids_max: resources.pids_max,
            io_weight: resources.io_weight,
            io_max: resources
                .io_max
                .iter()
                .map(|io| IoLimit {
                    major: io.major,
                    minor: io.minor,
                    rbps: io.rbps,
                    wbps: io.wbps,
                    riops: io.riops,
                    wiops: io.wiops,
                })
                .collect(),
            unified: resources.unified.clone(),
        }
    }
}

impl CgroupLimits {
    fn to_resources(&self) -> Resources {
        let mut resources = Resources::default();
        resources.memory_max = self.memory_max;
        resources.memory_low = self.memory_low;
        resources.swap_max = self.swap_max.map(|amount| match amount {
            Amount::Max => Limit::Max,
            Amount::Value(value) => Limit::Value(value),
        });
        resources.swap_is_default = self.swap_is_default;
        resources.cpu_max = self.cpu_max.then_some(CpuMax {
            quota: self.cpu_quota,
            period: self.cpu_period,
        });
        resources.cpu_weight = self.cpu_weight;
        resources.cpuset_cpus.clone_from(&self.cpuset_cpus);
        resources.cpuset_mems.clone_from(&self.cpuset_mems);
        resources.pids_max = self.pids_max;
        resources.io_weight = self.io_weight;
        resources.io_max = self
            .io_max
            .iter()
            .map(|io| {
                let mut limit = IoMax::device(io.major, io.minor);
                limit.rbps = io.rbps;
                limit.wbps = io.wbps;
                limit.riops = io.riops;
                limit.wiops = io.wiops;
                limit
            })
            .collect();
        resources.unified.clone_from(&self.unified);
        resources
    }
}

impl From<&NetworkMode> for Network {
    fn from(mode: &NetworkMode) -> Self {
        match mode {
            NetworkMode::Host => Self::Host,
            NetworkMode::None => Self::None,
            NetworkMode::Pasta => Self::Pasta,
            NetworkMode::Bridges(joined) => Self::Bridges(
                joined
                    .iter()
                    .map(|joined| Bridge {
                        name: joined.name.clone(),
                        ip: joined.ip,
                        ip6: joined.ip6,
                        mac: joined.mac.map(mac_text),
                        aliases: joined.aliases.clone(),
                    })
                    .collect(),
            ),
        }
    }
}

impl Network {
    fn to_mode(&self) -> Result<NetworkMode> {
        Ok(match self {
            Self::Host => NetworkMode::Host,
            Self::None => NetworkMode::None,
            Self::Pasta => NetworkMode::Pasta,
            Self::Bridges(bridges) => NetworkMode::Bridges(
                bridges
                    .iter()
                    .map(|bridge| {
                        Ok(Joined {
                            name: bridge.name.clone(),
                            ip: bridge.ip,
                            ip6: bridge.ip6,
                            mac: bridge.mac.as_deref().map(parse_mac).transpose()?,
                            aliases: bridge.aliases.clone(),
                        })
                    })
                    .collect::<Result<_>>()?,
            ),
        })
    }
}

impl From<&PortRequest> for Port {
    fn from(port: &PortRequest) -> Self {
        Self {
            host_ip: port.host_ip,
            host_port: port.host_port,
            container_port: port.container_port,
            protocol: port.protocol.name().to_owned(),
        }
    }
}

impl Port {
    fn to_request(&self) -> Result<PortRequest> {
        let protocol = match self.protocol.as_str() {
            "tcp" => Protocol::Tcp,
            "udp" => Protocol::Udp,
            _ => return stored(Err("not tcp or udp"), "port protocol", &self.protocol),
        };
        Ok(PortRequest {
            host_ip: self.host_ip,
            host_port: self.host_port,
            container_port: self.container_port,
            protocol,
        })
    }
}

impl Security {
    fn from_opt(opt: &SecurityOpt) -> Result<Self> {
        Ok(match opt {
            SecurityOpt::NoNewPrivileges(on) => Self::NoNewPrivileges(*on),
            SecurityOpt::SeccompUnconfined => Self::SeccompUnconfined,
            SecurityOpt::SeccompProfile(path) => Self::SeccompProfile(path_text(path)?),
        })
    }

    fn to_opt(&self) -> SecurityOpt {
        match self {
            Self::NoNewPrivileges(on) => SecurityOpt::NoNewPrivileges(*on),
            Self::SeccompUnconfined => SecurityOpt::SeccompUnconfined,
            Self::SeccompProfile(path) => SecurityOpt::SeccompProfile(PathBuf::from(path)),
        }
    }
}
