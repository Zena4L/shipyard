//! The record's form of a [`Config`] (step 11.1, D10.1d-1): frozen types of
//! the record's own, the same in every version that has a config (1 to 3),
//! that no change to the engine's [`Config`] can alter. Each field is
//! written as it always was (its name, its order, its words), so a record
//! reads and writes byte for byte as 0.2.0's (`version_3_reads_as_written`).
//! A change here is a new record version (`record.rs`'s upgrade rule),
//! never a change to these types: a new version gets new ones beside them.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr};

use serde::{Deserialize, Serialize};

use super::{
    Amount, Bridge, CgroupLimits, Config, Device, DeviceCgroupRule, Host, IdRange, IoLimit,
    Network, Port, RestartPolicy, Security, Sysctl, Ulimit, User,
};
use crate::error::Result;
use crate::health::HealthCheck;

/// [`Config`] as a record keeps it (and Shipyard's own `inspect` view
/// shows it): its fields are the record's, read only through a [`Config`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a mirror of the flags, which are booleans"
)]
pub struct StoredConfig {
    interactive: bool,
    tty: bool,
    env: Vec<String>,
    workdir: Option<String>,
    user: Option<StoredUser>,
    group_add: Vec<String>,
    ulimit: Vec<StoredUlimit>,
    oom_score_adj: Option<i32>,
    device: Vec<StoredDevice>,
    device_cgroup_rule: Vec<StoredDeviceCgroupRule>,
    stop_signal: String,
    stop_timeout: u64,
    /// Records before version 3 have none (`no`).
    #[serde(default = "no_restart")]
    restart: String,
    /// Added before version 3 was released (9.6b), as `restart`.
    #[serde(default)]
    health: Option<StoredHealthCheck>,
    no_isolation: bool,
    hostname: Option<String>,
    domainname: Option<String>,
    sysctl: Vec<StoredSysctl>,
    network: Vec<StoredNetwork>,
    network_alias: Vec<String>,
    ip: Option<IpAddr>,
    ip6: Option<Ipv6Addr>,
    mac_address: Option<String>,
    add_host: Vec<StoredHost>,
    dns: Vec<IpAddr>,
    dns_search: Vec<String>,
    dns_option: Vec<String>,
    publish: Vec<StoredPort>,
    publish_all: bool,
    rootfs: Option<String>,
    image: Option<String>,
    entrypoint: Option<String>,
    pull: String,
    read_only: bool,
    cap_add: Vec<String>,
    cap_drop: Vec<String>,
    security_opt: Vec<StoredSecurity>,
    uidmap: Vec<StoredIdRange>,
    gidmap: Vec<StoredIdRange>,
    bundle: Option<String>,
    resources: StoredCgroupLimits,
    volume: Vec<String>,
    command: Vec<String>,
}

/// A record before version 3 has no restart policy: `no`.
fn no_restart() -> String {
    "no".to_owned()
}

/// A restart policy as a record writes it: its own words, never
/// [`RestartPolicy`]'s `Display` (a change there mustn't change a record).
fn restart_words(policy: RestartPolicy) -> String {
    match policy {
        RestartPolicy::No => "no".to_owned(),
        RestartPolicy::OnFailure { max: None } => "on-failure".to_owned(),
        RestartPolicy::OnFailure { max: Some(max) } => format!("on-failure:{max}"),
        RestartPolicy::Always => "always".to_owned(),
        RestartPolicy::UnlessStopped => "unless-stopped".to_owned(),
    }
}

/// A record's restart words read back: anything else is a damaged record
/// ([`ErrorKind::Storage`](crate::ErrorKind::Storage)), never a flag to fix.
fn restart_policy(words: &str) -> Result<RestartPolicy> {
    match words {
        "no" => Ok(RestartPolicy::No),
        "always" => Ok(RestartPolicy::Always),
        "unless-stopped" => Ok(RestartPolicy::UnlessStopped),
        "on-failure" => Ok(RestartPolicy::OnFailure { max: None }),
        _ => match words.strip_prefix("on-failure:").map(str::parse::<u32>) {
            Some(Ok(max)) if max > 0 => Ok(RestartPolicy::OnFailure { max: Some(max) }),
            _ => crate::error::bail!(
                Storage,
                "a container's record has a restart policy that isn't one: {words:?}"
            ),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredUser {
    name: String,
    group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredUlimit {
    kind: String,
    soft: Option<u64>,
    hard: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDevice {
    host: String,
    container: Option<String>,
    access: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDeviceCgroupRule {
    allow: bool,
    kind: String,
    major: Option<u32>,
    minor: Option<u32>,
    access: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHealthCheck {
    test: Vec<String>,
    interval_ms: u64,
    timeout_ms: u64,
    start_period_ms: u64,
    start_interval_ms: u64,
    retries: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    shell: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSysctl {
    key: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StoredNetwork {
    Host,
    None,
    Pasta,
    Bridges(Vec<StoredBridge>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBridge {
    name: String,
    ip: Option<IpAddr>,
    ip6: Option<Ipv6Addr>,
    mac: Option<String>,
    aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHost {
    name: String,
    ip: IpAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPort {
    host_ip: Option<IpAddr>,
    host_port: Option<u16>,
    container_port: u16,
    protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredSecurity {
    NoNewPrivileges(bool),
    SeccompUnconfined,
    SeccompProfile(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdRange {
    container: u32,
    host: u32,
    size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCgroupLimits {
    memory_max: Option<u64>,
    memory_low: Option<u64>,
    swap_max: Option<StoredAmount>,
    swap_is_default: bool,
    cpu_quota: Option<u64>,
    cpu_period: Option<u64>,
    cpu_max: bool,
    cpu_weight: Option<u16>,
    cpuset_cpus: Option<String>,
    cpuset_mems: Option<String>,
    pids_max: Option<u64>,
    io_weight: Option<u16>,
    io_max: Vec<StoredIoLimit>,
    unified: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StoredAmount {
    Max,
    Value(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIoLimit {
    major: u32,
    minor: u32,
    rbps: Option<u64>,
    wbps: Option<u64>,
    riops: Option<u64>,
    wiops: Option<u64>,
}

impl From<&Config> for StoredConfig {
    fn from(config: &Config) -> Self {
        // Every field named, none `..`: a field added to `Config` fails to
        // build here until the record says how it's kept.
        let Config {
            interactive,
            tty,
            env,
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
            resources,
            volume,
            command,
        } = config.clone();
        Self {
            interactive,
            tty,
            env,
            workdir,
            user: user.map(Into::into),
            group_add,
            ulimit: ulimit.into_iter().map(Into::into).collect(),
            oom_score_adj,
            device: device.into_iter().map(Into::into).collect(),
            device_cgroup_rule: device_cgroup_rule.into_iter().map(Into::into).collect(),
            stop_signal,
            stop_timeout,
            restart: restart_words(restart),
            health: health.map(StoredHealthCheck::from),
            no_isolation,
            hostname,
            domainname,
            sysctl: sysctl.into_iter().map(Into::into).collect(),
            network: network.into_iter().map(StoredNetwork::from).collect(),
            network_alias,
            ip,
            ip6,
            mac_address,
            add_host: add_host.into_iter().map(Into::into).collect(),
            dns,
            dns_search,
            dns_option,
            publish: publish.into_iter().map(Into::into).collect(),
            publish_all,
            rootfs,
            image,
            entrypoint,
            pull,
            read_only,
            cap_add,
            cap_drop,
            security_opt: security_opt.into_iter().map(StoredSecurity::from).collect(),
            uidmap: uidmap.into_iter().map(StoredIdRange::from).collect(),
            gidmap: gidmap.into_iter().map(StoredIdRange::from).collect(),
            bundle,
            resources: StoredCgroupLimits::from(resources),
            volume,
            command,
        }
    }
}

impl TryFrom<StoredConfig> for Config {
    type Error = crate::Error;

    /// The engine's config from a record's: a restart policy that isn't one
    /// is the one way it fails (as reading it always did).
    fn try_from(stored: StoredConfig) -> Result<Self> {
        let StoredConfig {
            interactive,
            tty,
            env,
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
            resources,
            volume,
            command,
        } = stored;
        Ok(Self {
            interactive,
            tty,
            env,
            workdir,
            user: user.map(Into::into),
            group_add,
            ulimit: ulimit.into_iter().map(Into::into).collect(),
            oom_score_adj,
            device: device.into_iter().map(Into::into).collect(),
            device_cgroup_rule: device_cgroup_rule.into_iter().map(Into::into).collect(),
            stop_signal,
            stop_timeout,
            restart: restart_policy(&restart)?,
            health: health.map(HealthCheck::from),
            no_isolation,
            hostname,
            domainname,
            sysctl: sysctl.into_iter().map(Into::into).collect(),
            network: network.into_iter().map(Network::from).collect(),
            network_alias,
            ip,
            ip6,
            mac_address,
            add_host: add_host.into_iter().map(Into::into).collect(),
            dns,
            dns_search,
            dns_option,
            publish: publish.into_iter().map(Into::into).collect(),
            publish_all,
            rootfs,
            image,
            entrypoint,
            pull,
            read_only,
            cap_add,
            cap_drop,
            security_opt: security_opt.into_iter().map(Security::from).collect(),
            uidmap: uidmap.into_iter().map(IdRange::from).collect(),
            gidmap: gidmap.into_iter().map(IdRange::from).collect(),
            bundle,
            resources: CgroupLimits::from(resources),
            volume,
            command,
        })
    }
}

impl From<User> for StoredUser {
    fn from(User { name, group }: User) -> Self {
        Self { name, group }
    }
}

impl From<StoredUser> for User {
    fn from(StoredUser { name, group }: StoredUser) -> Self {
        Self { name, group }
    }
}

impl From<Ulimit> for StoredUlimit {
    fn from(Ulimit { kind, soft, hard }: Ulimit) -> Self {
        Self { kind, soft, hard }
    }
}

impl From<StoredUlimit> for Ulimit {
    fn from(StoredUlimit { kind, soft, hard }: StoredUlimit) -> Self {
        Self { kind, soft, hard }
    }
}

impl From<Device> for StoredDevice {
    fn from(
        Device {
            host,
            container,
            access,
        }: Device,
    ) -> Self {
        Self {
            host,
            container,
            access,
        }
    }
}

impl From<StoredDevice> for Device {
    fn from(
        StoredDevice {
            host,
            container,
            access,
        }: StoredDevice,
    ) -> Self {
        Self {
            host,
            container,
            access,
        }
    }
}

impl From<DeviceCgroupRule> for StoredDeviceCgroupRule {
    fn from(
        DeviceCgroupRule {
            allow,
            kind,
            major,
            minor,
            access,
        }: DeviceCgroupRule,
    ) -> Self {
        Self {
            allow,
            kind,
            major,
            minor,
            access,
        }
    }
}

impl From<StoredDeviceCgroupRule> for DeviceCgroupRule {
    fn from(
        StoredDeviceCgroupRule {
            allow,
            kind,
            major,
            minor,
            access,
        }: StoredDeviceCgroupRule,
    ) -> Self {
        Self {
            allow,
            kind,
            major,
            minor,
            access,
        }
    }
}

impl From<Sysctl> for StoredSysctl {
    fn from(Sysctl { key, value }: Sysctl) -> Self {
        Self { key, value }
    }
}

impl From<StoredSysctl> for Sysctl {
    fn from(StoredSysctl { key, value }: StoredSysctl) -> Self {
        Self { key, value }
    }
}

impl From<Host> for StoredHost {
    fn from(Host { name, ip }: Host) -> Self {
        Self { name, ip }
    }
}

impl From<StoredHost> for Host {
    fn from(StoredHost { name, ip }: StoredHost) -> Self {
        Self { name, ip }
    }
}

impl From<Port> for StoredPort {
    fn from(
        Port {
            host_ip,
            host_port,
            container_port,
            protocol,
        }: Port,
    ) -> Self {
        Self {
            host_ip,
            host_port,
            container_port,
            protocol,
        }
    }
}

impl From<StoredPort> for Port {
    fn from(
        StoredPort {
            host_ip,
            host_port,
            container_port,
            protocol,
        }: StoredPort,
    ) -> Self {
        Self {
            host_ip,
            host_port,
            container_port,
            protocol,
        }
    }
}

impl From<HealthCheck> for StoredHealthCheck {
    fn from(check: HealthCheck) -> Self {
        let HealthCheck {
            test,
            interval_ms,
            timeout_ms,
            start_period_ms,
            start_interval_ms,
            retries,
            shell,
        } = check;
        Self {
            test,
            interval_ms,
            timeout_ms,
            start_period_ms,
            start_interval_ms,
            retries,
            shell,
        }
    }
}

impl From<StoredHealthCheck> for HealthCheck {
    fn from(check: StoredHealthCheck) -> Self {
        let StoredHealthCheck {
            test,
            interval_ms,
            timeout_ms,
            start_period_ms,
            start_interval_ms,
            retries,
            shell,
        } = check;
        Self {
            test,
            interval_ms,
            timeout_ms,
            start_period_ms,
            start_interval_ms,
            retries,
            shell,
        }
    }
}

impl From<Network> for StoredNetwork {
    fn from(network: Network) -> Self {
        match network {
            Network::Host => Self::Host,
            Network::None => Self::None,
            Network::Pasta => Self::Pasta,
            Network::Bridges(bridges) => Self::Bridges(
                bridges
                    .into_iter()
                    .map(
                        |Bridge {
                             name,
                             ip,
                             ip6,
                             mac,
                             aliases,
                         }| StoredBridge {
                            name,
                            ip,
                            ip6,
                            mac,
                            aliases,
                        },
                    )
                    .collect(),
            ),
        }
    }
}

impl From<StoredNetwork> for Network {
    fn from(network: StoredNetwork) -> Self {
        match network {
            StoredNetwork::Host => Self::Host,
            StoredNetwork::None => Self::None,
            StoredNetwork::Pasta => Self::Pasta,
            StoredNetwork::Bridges(bridges) => Self::Bridges(
                bridges
                    .into_iter()
                    .map(
                        |StoredBridge {
                             name,
                             ip,
                             ip6,
                             mac,
                             aliases,
                         }| Bridge {
                            name,
                            ip,
                            ip6,
                            mac,
                            aliases,
                        },
                    )
                    .collect(),
            ),
        }
    }
}

impl From<Security> for StoredSecurity {
    fn from(security: Security) -> Self {
        match security {
            Security::NoNewPrivileges(on) => Self::NoNewPrivileges(on),
            Security::SeccompUnconfined => Self::SeccompUnconfined,
            Security::SeccompProfile(path) => Self::SeccompProfile(path),
        }
    }
}

impl From<StoredSecurity> for Security {
    fn from(security: StoredSecurity) -> Self {
        match security {
            StoredSecurity::NoNewPrivileges(on) => Self::NoNewPrivileges(on),
            StoredSecurity::SeccompUnconfined => Self::SeccompUnconfined,
            StoredSecurity::SeccompProfile(path) => Self::SeccompProfile(path),
        }
    }
}

impl From<IdRange> for StoredIdRange {
    fn from(
        IdRange {
            container,
            host,
            size,
        }: IdRange,
    ) -> Self {
        Self {
            container,
            host,
            size,
        }
    }
}

impl From<StoredIdRange> for IdRange {
    fn from(
        StoredIdRange {
            container,
            host,
            size,
        }: StoredIdRange,
    ) -> Self {
        Self {
            container,
            host,
            size,
        }
    }
}

impl From<Amount> for StoredAmount {
    fn from(amount: Amount) -> Self {
        match amount {
            Amount::Max => Self::Max,
            Amount::Value(value) => Self::Value(value),
        }
    }
}

impl From<StoredAmount> for Amount {
    fn from(amount: StoredAmount) -> Self {
        match amount {
            StoredAmount::Max => Self::Max,
            StoredAmount::Value(value) => Self::Value(value),
        }
    }
}

impl From<IoLimit> for StoredIoLimit {
    fn from(
        IoLimit {
            major,
            minor,
            rbps,
            wbps,
            riops,
            wiops,
        }: IoLimit,
    ) -> Self {
        Self {
            major,
            minor,
            rbps,
            wbps,
            riops,
            wiops,
        }
    }
}

impl From<StoredIoLimit> for IoLimit {
    fn from(
        StoredIoLimit {
            major,
            minor,
            rbps,
            wbps,
            riops,
            wiops,
        }: StoredIoLimit,
    ) -> Self {
        Self {
            major,
            minor,
            rbps,
            wbps,
            riops,
            wiops,
        }
    }
}

impl From<CgroupLimits> for StoredCgroupLimits {
    fn from(limits: CgroupLimits) -> Self {
        let CgroupLimits {
            memory_max,
            memory_low,
            swap_max,
            swap_is_default,
            cpu_quota,
            cpu_period,
            cpu_max,
            cpu_weight,
            cpuset_cpus,
            cpuset_mems,
            pids_max,
            io_weight,
            io_max,
            unified,
        } = limits;
        Self {
            memory_max,
            memory_low,
            swap_max: swap_max.map(StoredAmount::from),
            swap_is_default,
            cpu_quota,
            cpu_period,
            cpu_max,
            cpu_weight,
            cpuset_cpus,
            cpuset_mems,
            pids_max,
            io_weight,
            io_max: io_max.into_iter().map(StoredIoLimit::from).collect(),
            unified,
        }
    }
}

impl From<StoredCgroupLimits> for CgroupLimits {
    fn from(limits: StoredCgroupLimits) -> Self {
        let StoredCgroupLimits {
            memory_max,
            memory_low,
            swap_max,
            swap_is_default,
            cpu_quota,
            cpu_period,
            cpu_max,
            cpu_weight,
            cpuset_cpus,
            cpuset_mems,
            pids_max,
            io_weight,
            io_max,
            unified,
        } = limits;
        Self {
            memory_max,
            memory_low,
            swap_max: swap_max.map(Amount::from),
            swap_is_default,
            cpu_quota,
            cpu_period,
            cpu_max,
            cpu_weight,
            cpuset_cpus,
            cpuset_mems,
            pids_max,
            io_weight,
            io_max: io_max.into_iter().map(IoLimit::from).collect(),
            unified,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record's restart words are its own: every policy written and
    /// read back; anything else a damaged record (`Storage`), never the
    /// flag's advice.
    #[test]
    fn restart_words_are_the_records_own() {
        for policy in [
            RestartPolicy::No,
            RestartPolicy::Always,
            RestartPolicy::UnlessStopped,
            RestartPolicy::OnFailure { max: None },
            RestartPolicy::OnFailure { max: Some(5) },
        ] {
            assert_eq!(restart_policy(&restart_words(policy)).unwrap(), policy);
        }
        assert_eq!(
            restart_words(RestartPolicy::OnFailure { max: Some(5) }),
            "on-failure:5"
        );
        for damaged in ["bogus", "on-failure:0", "on-failure:x", ""] {
            assert_eq!(
                restart_policy(damaged).unwrap_err().kind(),
                crate::ErrorKind::Storage,
                "{damaged:?}"
            );
        }
    }
}
