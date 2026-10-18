//! What a container's record keeps of how to run it: the request as asked
//! ([`RunRequest`](crate::run::RunRequest)), in a form of its own (`Config`), so that the record's
//! format doesn't change when a type inside the engine or the runtime
//! does. Each start makes the request from it again, and resolves it as a
//! foreground run does (the image, the networks, the `/etc` files): what
//! can't outlive a reboot is never stored.
//!
//! Values are written as their names or as plain numbers (a signal as
//! `SIGTERM`, a capability as `NET_ADMIN`, an rlimit as its kind and two
//! numbers, a device's access as `rwm`), in types of this module's own,
//! none of another crate's: a change there can't change a record. The
//! format is pinned by a test that reads a literal record.
//!
//! This serde form is the record's (inside version 3) and, until stage 11
//! gives `inspect` Docker's form, `inspect`'s JSON too: a change to it is a
//! new record version (`record.rs`'s upgrade rule).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr};

use serde::{Deserialize, Serialize};

use crate::error::{Result, bail};

/// How to run a container, as it was asked: [`RunRequest`](crate::run::RunRequest)'s fields, but
/// the caller's environment, which is read when the container is made
/// (a bare `--env KEY` is stored with the value it had then), and the
/// name, which is the record's ([`Record::name`](super::Record::name)).
/// Made by [`from_request`](Self::from_request) only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a mirror of the flags, which are booleans"
)]
pub struct Config {
    /// `-i`.
    pub interactive: bool,
    /// `-t`.
    pub tty: bool,
    /// `KEY=VALUE`s, in order.
    pub env: Vec<String>,
    /// `--workdir`.
    pub workdir: Option<String>,
    /// `--user`.
    pub user: Option<User>,
    /// `--group-add`.
    pub group_add: Vec<String>,
    /// `--ulimit`.
    pub ulimit: Vec<Ulimit>,
    /// `--oom-score-adj`.
    pub oom_score_adj: Option<i32>,
    /// `--device`.
    pub device: Vec<Device>,
    /// `--device-cgroup-rule`.
    pub device_cgroup_rule: Vec<DeviceCgroupRule>,
    /// `--stop-signal`, as its name (`SIGTERM`), or a number.
    pub stop_signal: String,
    /// `--stop-timeout`, in seconds.
    pub stop_timeout: u64,
    /// `--restart`. Records before version 3 have none (`no`): the one
    /// field a version may lack, as the config is the same type in every
    /// version.
    #[serde(default)]
    pub restart: RestartPolicy,
    /// `--health-cmd` and the rest: none, no checks. Added before version 3
    /// was released (9.6b), as `restart`.
    #[serde(default)]
    pub health: Option<crate::health::HealthCheck>,
    /// `--no-isolation`.
    pub no_isolation: bool,
    /// `--hostname`.
    pub hostname: Option<String>,
    /// `--domainname`.
    pub domainname: Option<String>,
    /// `--sysctl`.
    pub sysctl: Vec<Sysctl>,
    /// `--network`, in order.
    pub network: Vec<Network>,
    /// `--network-alias`.
    pub network_alias: Vec<String>,
    /// `--ip`.
    pub ip: Option<IpAddr>,
    /// `--ip6`.
    pub ip6: Option<Ipv6Addr>,
    /// `--mac-address`, as `aa:bb:cc:dd:ee:ff`.
    pub mac_address: Option<String>,
    /// `--add-host`.
    pub add_host: Vec<Host>,
    /// `--dns`.
    pub dns: Vec<IpAddr>,
    /// `--dns-search`.
    pub dns_search: Vec<String>,
    /// `--dns-option`.
    pub dns_option: Vec<String>,
    /// `--publish`.
    pub publish: Vec<Port>,
    /// `--publish-all`.
    pub publish_all: bool,
    /// `--rootfs`.
    pub rootfs: Option<String>,
    /// The image (`run IMAGE`).
    pub image: Option<String>,
    /// `--entrypoint`.
    pub entrypoint: Option<String>,
    /// `--pull`: `missing`, `always` or `never`.
    pub pull: String,
    /// `--read-only`.
    pub read_only: bool,
    /// `--cap-add`: names, or `ALL`.
    pub cap_add: Vec<String>,
    /// `--cap-drop`.
    pub cap_drop: Vec<String>,
    /// `--security-opt`.
    pub security_opt: Vec<Security>,
    /// `--uidmap`.
    pub uidmap: Vec<IdRange>,
    /// `--gidmap`.
    pub gidmap: Vec<IdRange>,
    /// `--bundle`.
    pub bundle: Option<String>,
    /// The cgroup limits (`--memory`, `--cpus`, …).
    pub resources: CgroupLimits,
    /// `--volume`, as given (`SOURCE:DEST[:ro]`).
    pub volume: Vec<String>,
    /// The command and its arguments.
    pub command: Vec<String>,
}

/// What happens when a container ends (`--restart`), Docker's policies:
/// written and read in their words (`no`, `on-failure`, `on-failure:3`,
/// `always`, `unless-stopped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub enum RestartPolicy {
    /// Never started again by its policy.
    #[default]
    No,
    /// Started again when it ends with a code other than 0, at most `max`
    /// times (none: without end) since its user last started it.
    OnFailure {
        /// The most restarts.
        max: Option<u32>,
    },
    /// Started again however it ends; once its user stops it, only when
    /// the host starts again.
    Always,
    /// As `Always`, but never once its user has stopped it.
    UnlessStopped,
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::No => f.write_str("no"),
            Self::OnFailure { max: None } => f.write_str("on-failure"),
            Self::OnFailure { max: Some(max) } => write!(f, "on-failure:{max}"),
            Self::Always => f.write_str("always"),
            Self::UnlessStopped => f.write_str("unless-stopped"),
        }
    }
}

impl std::str::FromStr for RestartPolicy {
    type Err = crate::Error;

    fn from_str(text: &str) -> Result<Self> {
        Ok(match text {
            "no" => Self::No,
            "always" => Self::Always,
            "unless-stopped" => Self::UnlessStopped,
            "on-failure" => Self::OnFailure { max: None },
            _ => match text.strip_prefix("on-failure:").map(str::parse::<u32>) {
                Some(Ok(max)) if max > 0 => Self::OnFailure { max: Some(max) },
                _ => bail!(
                    InvalidRequest,
                    "restart policy {text:?}: give no, always, unless-stopped, on-failure or \
                     on-failure:N (N at least 1)"
                ),
            },
        })
    }
}

impl TryFrom<String> for RestartPolicy {
    type Error = crate::Error;

    fn try_from(text: String) -> Result<Self> {
        text.parse()
    }
}

impl From<RestartPolicy> for String {
    fn from(policy: RestartPolicy) -> Self {
        policy.to_string()
    }
}

/// An rlimit: its kind's name (`nofile`), and the soft and hard values
/// (`None`: unlimited).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Ulimit {
    /// The resource (`nofile`, `nproc`, …).
    pub kind: String,
    /// The soft limit.
    pub soft: Option<u64>,
    /// The hard limit.
    pub hard: Option<u64>,
}

/// A host device given to the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Device {
    /// The host's device (or a directory of them).
    pub host: String,
    /// Where it goes in the container, if not at the host's path.
    pub container: Option<String>,
    /// What the container may do with it: `r`, `w` and `m`.
    pub access: String,
}

/// `--user`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct User {
    /// The user: a name or a number.
    pub name: String,
    /// The group, if given: a name or a number.
    pub group: Option<String>,
}

/// A kernel setting the container owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Sysctl {
    /// Its name (`net.ipv4.ip_forward`).
    pub key: String,
    /// Its value.
    pub value: String,
}

/// A kind of device the container may use (`--device-cgroup-rule`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DeviceCgroupRule {
    /// Allowed, or denied.
    pub allow: bool,
    /// `c` (character), `b` (block) or `a` (both).
    pub kind: String,
    /// The major number (`None`: any).
    pub major: Option<u32>,
    /// The minor number (`None`: any).
    pub minor: Option<u32>,
    /// What it allows or denies: `r`, `w` and `m`.
    pub access: String,
}

/// A range of a user namespace's map: `size` IDs from `container` inside
/// are `host` onwards outside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IdRange {
    /// The first ID inside.
    pub container: u32,
    /// The host ID it is.
    pub host: u32,
    /// How many.
    pub size: u32,
}

/// The cgroup limits, as the cgroup files take them (`None`: the kernel's
/// default, no limit).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CgroupLimits {
    /// `memory.max`, in bytes.
    pub memory_max: Option<u64>,
    /// `memory.low`, in bytes.
    pub memory_low: Option<u64>,
    /// `memory.swap.max`, in bytes (`max`: no limit).
    pub swap_max: Option<Amount>,
    /// Whether `swap_max` is Docker's default, not asked for.
    pub swap_is_default: bool,
    /// `cpu.max`'s quota in microseconds (`None`: no limit).
    pub cpu_quota: Option<u64>,
    /// `cpu.max`'s period in microseconds (`None`: the cgroup's own).
    pub cpu_period: Option<u64>,
    /// Whether `cpu.max` is written at all.
    pub cpu_max: bool,
    /// `cpu.weight`.
    pub cpu_weight: Option<u16>,
    /// `cpuset.cpus`.
    pub cpuset_cpus: Option<String>,
    /// `cpuset.mems`.
    pub cpuset_mems: Option<String>,
    /// `pids.max`.
    pub pids_max: Option<u64>,
    /// `io.weight`.
    pub io_weight: Option<u16>,
    /// `io.max`, per device.
    pub io_max: Vec<IoLimit>,
    /// Other cgroup files, written as given.
    pub unified: BTreeMap<String, String>,
}

/// An amount that can also be no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Amount {
    /// No limit.
    Max,
    /// This much.
    Value(u64),
}

/// One device's `io.max`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IoLimit {
    /// The block device's major number.
    pub major: u32,
    /// Its minor number.
    pub minor: u32,
    /// Bytes read per second.
    pub rbps: Option<u64>,
    /// Bytes written per second.
    pub wbps: Option<u64>,
    /// Reads per second.
    pub riops: Option<u64>,
    /// Writes per second.
    pub wiops: Option<u64>,
}

/// A network asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Network {
    /// The host's.
    Host,
    /// One of its own, only a loopback.
    None,
    /// One of its own, through pasta.
    Pasta,
    /// Bridge networks, in order.
    Bridges(Vec<Bridge>),
}

/// A bridge network joined, and the container's settings there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Bridge {
    /// The network.
    pub name: String,
    /// Its address there.
    pub ip: Option<IpAddr>,
    /// Its IPv6 address there.
    pub ip6: Option<Ipv6Addr>,
    /// Its hardware address there, as `aa:bb:cc:dd:ee:ff`.
    pub mac: Option<String>,
    /// Its other names there.
    pub aliases: Vec<String>,
}

impl Bridge {
    /// The network `name`, joined with no settings of its own.
    #[must_use]
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ip: None,
            ip6: None,
            mac: None,
            aliases: Vec::new(),
        }
    }
}

/// An `/etc/hosts` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Host {
    /// The name.
    pub name: String,
    /// Its address.
    pub ip: IpAddr,
}

/// A port published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Port {
    /// The host address (`None`: every one).
    pub host_ip: Option<IpAddr>,
    /// The host port (`None`: a free one).
    pub host_port: Option<u16>,
    /// The container's port.
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
}

/// A security option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Security {
    /// `no-new-privileges`, on or off.
    NoNewPrivileges(bool),
    /// `seccomp=unconfined`.
    SeccompUnconfined,
    /// `seccomp=FILE`.
    SeccompProfile(String),
}

mod bridges;
mod convert;
