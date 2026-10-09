//! A bundle's `linux.resources` and `linux.cgroupsPath`: the limits, as the
//! runtime spec writes them (cgroup v1's units, as runc reads them), turned
//! into [`Resources`]; and where the runtime puts the container's cgroup.
//!
//! As everywhere in bundles: what can't be applied is refused, never
//! dropped (a limit missing is a container less confined than asked).
//! cgroup v2 has no kernel memory limit, swappiness, OOM-killer switch,
//! real-time CPU budget, per-device weights, huge page or network classes,
//! so those are refused when asked for. Device rules (`devices`) are read
//! here too, and enforced by the container's device program.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{BundleError, Reader, asks_nothing};
use crate::cgroup::{RUN_PARENT, RUNTIME_PARENT};
use crate::devices::{Access, DeviceKind, DeviceRule};
use crate::resources::{self, CpuMax, IoLimit, Limit, Resources};

/// `linux.resources`. Fields the spec doesn't define are ignored.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ResourcesConfig {
    #[serde(default)]
    devices: Vec<DeviceRuleConfig>,
    memory: Option<MemoryConfig>,
    cpu: Option<CpuConfig>,
    pids: Option<PidsConfig>,
    #[serde(rename = "blockIO")]
    block_io: Option<BlockIoConfig>,
    #[serde(default)]
    hugepage_limits: Vec<Value>,
    network: Option<Value>,
    rdma: Option<Value>,
    #[serde(default)]
    unified: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MemoryConfig {
    limit: Option<i64>,
    reservation: Option<i64>,
    swap: Option<i64>,
    kernel: Option<i64>,
    #[serde(rename = "kernelTCP")]
    kernel_tcp: Option<i64>,
    /// A `Value`: the spec says unsigned, but engines write Go's -1 too.
    swappiness: Option<Value>,
    #[serde(rename = "disableOOMKiller")]
    disable_oom_killer: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CpuConfig {
    shares: Option<u64>,
    quota: Option<i64>,
    burst: Option<u64>,
    period: Option<u64>,
    realtime_runtime: Option<i64>,
    realtime_period: Option<u64>,
    cpus: Option<String>,
    mems: Option<String>,
    idle: Option<i64>,
}

#[derive(Deserialize, Default)]
struct PidsConfig {
    limit: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct BlockIoConfig {
    weight: Option<u16>,
    leaf_weight: Option<u16>,
    #[serde(default)]
    weight_device: Vec<Value>,
    #[serde(default)]
    throttle_read_bps_device: Vec<ThrottleConfig>,
    #[serde(default)]
    throttle_write_bps_device: Vec<ThrottleConfig>,
    #[serde(default, rename = "throttleReadIOPSDevice")]
    throttle_read_iops_device: Vec<ThrottleConfig>,
    #[serde(default, rename = "throttleWriteIOPSDevice")]
    throttle_write_iops_device: Vec<ThrottleConfig>,
}

/// One of `linux.resources.devices`: a rule, as cgroup v1's
/// `devices.allow`/`deny` took it.
#[derive(Deserialize)]
struct DeviceRuleConfig {
    allow: bool,
    /// `a` (both, the default), `c` or `b`.
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Missing or -1: any.
    major: Option<i64>,
    minor: Option<i64>,
    /// `rwm`'s letters; missing: all three.
    access: Option<String>,
}

/// One device's rate limit.
#[derive(Deserialize)]
struct ThrottleConfig {
    major: i64,
    minor: i64,
    rate: u64,
}

/// A device number in a rule: -1 or missing is any.
fn device_number(number: Option<i64>, field: &str) -> Result<Option<u32>, String> {
    match number {
        None | Some(-1) => Ok(None),
        Some(number) => u32::try_from(number)
            .map(Some)
            .map_err(|_| format!("{field} {number} isn't a device number")),
    }
}

/// One rule, as [`DeviceRule`].
fn device_rule(config: &DeviceRuleConfig) -> Result<DeviceRule, String> {
    let kind = match config.kind.as_deref() {
        None | Some("a") => None,
        Some("c") => Some(DeviceKind::Char),
        Some("b") => Some(DeviceKind::Block),
        Some(other) => return Err(format!("type {other:?} isn't a, c or b")),
    };
    let access = match config.access.as_deref() {
        None => Access::ALL,
        Some(text) => Access::parse(text).map_err(|err| err.to_string())?,
    };
    Ok(DeviceRule::new(
        config.allow,
        kind,
        device_number(config.major, "major")?,
        device_number(config.minor, "minor")?,
        access,
    ))
}

impl Reader<'_> {
    /// `linux.resources`, read once: the limits, as [`Resources`], checked,
    /// and the device rules (`devices`), in order (see `devices`).
    pub(super) fn resources(
        &mut self,
        value: Option<&Value>,
    ) -> Result<(Resources, Vec<DeviceRule>), BundleError> {
        let Some(value) = value.filter(|value| !asks_nothing(value)) else {
            return Ok((Resources::default(), Vec::new()));
        };
        let config: ResourcesConfig = serde_json::from_value(value.clone())
            .map_err(|err| self.invalid(format!("linux.resources: {err}")))?;
        let rules = config
            .devices
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                device_rule(rule)
                    .map_err(|why| self.invalid(format!("linux.resources.devices[{index}]: {why}")))
            })
            .collect::<Result<_, _>>()?;
        self.refuse_unsupported(&config);
        let mut limits = Resources::default();
        if let Some(memory) = &config.memory {
            self.memory(memory, &mut limits)?;
        }
        if let Some(cpu) = &config.cpu {
            self.cpu(cpu, &mut limits);
        }
        limits.pids_max = config
            .pids
            .and_then(|pids| pids.limit)
            .and_then(resources::positive);
        if let Some(block_io) = &config.block_io {
            self.block_io(block_io, &mut limits)?;
        }
        limits.unified = config.unified;
        limits
            .validate()
            .map_err(|err| self.invalid(format!("linux.resources: {err}")))?;
        Ok((limits, rules))
    }

    /// What cgroup v2 (or this version) can't apply: refused, by name.
    fn refuse_unsupported(&mut self, config: &ResourcesConfig) {
        if !config.hugepage_limits.is_empty() {
            self.refuse(
                "linux.resources.hugepageLimits",
                "huge page limits aren't supported",
            );
        }
        for (field, value) in [
            ("linux.resources.network", &config.network),
            ("linux.resources.rdma", &config.rdma),
        ] {
            if value.as_ref().is_some_and(|value| !asks_nothing(value)) {
                self.refuse(field, "cgroup v2 has no such controller here");
            }
        }
    }

    /// `memory`: the limit and reservation (-1 or 0: none), and swap as
    /// runc converts it (the spec's is memory plus swap; -1 unlimited).
    fn memory(&mut self, memory: &MemoryConfig, limits: &mut Resources) -> Result<(), BundleError> {
        for (field, value) in [
            ("linux.resources.memory.kernel", memory.kernel),
            ("linux.resources.memory.kernelTCP", memory.kernel_tcp),
        ] {
            // -1 is runc's "unlimited": nothing to apply.
            if value.is_some_and(|value| value > 0) {
                self.refuse(field, "cgroup v2 has no kernel memory limit");
            }
        }
        // -1 (or its unsigned form) is "the default": nothing asked.
        let swappiness_asked = memory.swappiness.as_ref().is_some_and(|value| {
            !value.is_null() && value.as_i64() != Some(-1) && value.as_u64() != Some(u64::MAX)
        });
        if swappiness_asked {
            self.refuse(
                "linux.resources.memory.swappiness",
                "cgroup v2 has no per-cgroup swappiness",
            );
        }
        if memory.disable_oom_killer == Some(true) {
            self.refuse(
                "linux.resources.memory.disableOOMKiller",
                "cgroup v2 can't turn the OOM killer off",
            );
        }
        limits.memory_max = self.bytes("memory.limit", memory.limit)?;
        limits.memory_low = self.bytes("memory.reservation", memory.reservation)?;
        limits.swap_max = match memory.swap {
            None | Some(0) => None,
            Some(-1) => Some(Limit::Max),
            Some(total) => Some(
                resources::swap_from_total(total, limits.memory_max)
                    .map_err(|err| self.invalid(format!("linux.resources.memory.swap: {err}")))?,
            ),
        };
        Ok(())
    }

    /// A byte count where -1 means "no limit" and 0 "not set": both `None`.
    fn bytes(&self, field: &str, value: Option<i64>) -> Result<Option<u64>, BundleError> {
        match value {
            None | Some(0 | -1) => Ok(None),
            Some(bytes) => u64::try_from(bytes).map(Some).map_err(|_| {
                self.invalid(format!(
                    "linux.resources.{field}: {bytes} is neither a size nor -1 (no limit)"
                ))
            }),
        }
    }

    /// `cpu`: shares as a weight, quota and period as `cpu.max`, the cpuset.
    fn cpu(&mut self, cpu: &CpuConfig, limits: &mut Resources) {
        for (field, asked, why) in [
            (
                "linux.resources.cpu.realtimeRuntime",
                cpu.realtime_runtime.is_some_and(|value| value != 0),
                "real-time CPU budgets aren't supported",
            ),
            (
                "linux.resources.cpu.realtimePeriod",
                cpu.realtime_period.is_some_and(|value| value != 0),
                "real-time CPU budgets aren't supported",
            ),
            (
                "linux.resources.cpu.burst",
                cpu.burst.is_some_and(|value| value != 0),
                "CPU bursts aren't supported yet",
            ),
            (
                "linux.resources.cpu.idle",
                cpu.idle.is_some_and(|value| value != 0),
                "idle scheduling isn't supported yet",
            ),
        ] {
            if asked {
                self.refuse(field, why);
            }
        }
        limits.cpu_weight = cpu.shares.and_then(resources::weight_from_shares);
        limits.cpu_max = CpuMax::from_parts(cpu.quota, cpu.period);
        limits.cpuset_cpus = cpu.cpus.clone().filter(|cpus| !cpus.is_empty());
        limits.cpuset_mems = cpu.mems.clone().filter(|mems| !mems.is_empty());
    }

    /// `blockIO`: the weight (v1's 10–1000, converted) and the per-device
    /// rate limits, one `io.max` line per device.
    fn block_io(
        &mut self,
        block_io: &BlockIoConfig,
        limits: &mut Resources,
    ) -> Result<(), BundleError> {
        if block_io.leaf_weight.is_some_and(|weight| weight != 0) {
            self.refuse(
                "linux.resources.blockIO.leafWeight",
                "cgroup v2 has no leaf weight",
            );
        }
        if !block_io.weight_device.is_empty() {
            self.refuse(
                "linux.resources.blockIO.weightDevice",
                "per-device weights aren't supported yet",
            );
        }
        if let Some(weight) = block_io.weight {
            limits.io_weight = resources::io_weight_from_blkio(weight)
                .map_err(|err| self.invalid(format!("linux.resources.blockIO.weight: {err}")))?;
        }
        // (The first constructor's type is spelled out; the others follow.)
        let throttles = [
            (
                block_io.throttle_read_bps_device.as_slice(),
                IoLimit::ReadBps as fn(u64) -> IoLimit,
            ),
            (&block_io.throttle_write_bps_device, IoLimit::WriteBps),
            (&block_io.throttle_read_iops_device, IoLimit::ReadIops),
            (&block_io.throttle_write_iops_device, IoLimit::WriteIops),
        ];
        for (devices, limit) in throttles {
            // A rate of 0 is "no limit", as runc writes nothing for it.
            for device in devices.iter().filter(|device| device.rate > 0) {
                let number = |value: i64| {
                    u32::try_from(value).map_err(|_| {
                        self.invalid(format!(
                            "linux.resources.blockIO: {value} isn't a device number"
                        ))
                    })
                };
                limits.limit_io(
                    number(device.major)?,
                    number(device.minor)?,
                    limit(device.rate),
                );
            }
        }
        Ok(())
    }

    /// `linux.cgroupsPath`, as a cgroup below the root: an absolute path is
    /// below `/sys/fs/cgroup`, a relative one below `shipyard/runtime/`.
    /// `None` if it's empty.
    pub(super) fn cgroups_path(
        &mut self,
        text: Option<&str>,
    ) -> Result<Option<PathBuf>, BundleError> {
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return Ok(None);
        };
        if is_systemd_form(text) {
            self.refuse(
                "linux.cgroupsPath",
                "systemd's \"slice:prefix:name\" form isn't supported: Shipyard manages the \
                 cgroup tree itself",
            );
            return Ok(None);
        }
        let path = Path::new(text);
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => relative.push(name),
                Component::RootDir => {}
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                    return Err(self.invalid(format!(
                        "linux.cgroupsPath {text:?}: `.` and `..` aren't allowed"
                    )));
                }
            }
        }
        if relative.as_os_str().is_empty() {
            return Err(self.invalid(format!(
                "linux.cgroupsPath {text:?}: that's the root of the cgroup tree"
            )));
        }
        if relative.starts_with(RUN_PARENT) {
            return Err(self.invalid(format!(
                "linux.cgroupsPath {text:?}: {RUN_PARENT} is shipyard run's (its cgroups come and \
                 go with its runs)"
            )));
        }
        Ok(Some(if path.is_absolute() {
            relative
        } else {
            Path::new(RUNTIME_PARENT).join(relative)
        }))
    }
}

/// systemd's form, `slice:prefix:name`: three parts, no `/`.
fn is_systemd_form(text: &str) -> bool {
    !text.contains('/') && text.split(':').count() == 3
}
