//! What a container may use (stage 5): its memory, CPU, processes and disk
//! bandwidth, as cgroup v2 controls them (`cgroups(7)`, the kernel's
//! `Documentation/admin-guide/cgroup-v2.rst`).
//!
//! [`Resources`] says it in the kernel's own terms, one field per cgroup
//! file it is written to. Front ends speak other units: Docker's flags
//! (`--memory 512m`, `--cpus 1.5`, `--cpu-shares 512`) and OCI's
//! `linux.resources` (memory plus swap, CPU shares, block I/O weights from
//! cgroup v1). The conversions here turn them into the kernel's, the way
//! Docker and runc do, so every front end means the same thing.
//!
//! Pure, and cross-platform: a Mac parses and checks the same values a Linux
//! host applies.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::cgroup::Controller;

/// The CPU period Docker uses for `--cpus`: 100 ms.
pub const CPU_PERIOD: u64 = 100_000;

/// What a container may use, as the cgroup v2 files take it. `None` (or an
/// empty list) leaves the kernel's default: no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Resources {
    /// `memory.max`: the most memory, in bytes. Past it, the kernel reclaims,
    /// then kills a process (`oom_kill`).
    pub memory_max: Option<u64>,
    /// `memory.low`: memory, in bytes, protected from reclaim while the host
    /// is short (Docker's `--memory-reservation`).
    pub memory_low: Option<u64>,
    /// `memory.swap.max`: the most swap, in bytes ([`Limit::Max`]: no limit).
    pub swap_max: Option<Limit>,
    /// Whether `swap_max` is Docker's default (as much swap as memory), not
    /// asked for: where the kernel doesn't account swap, it's skipped, with a
    /// warning, instead of refused.
    pub swap_is_default: bool,
    /// `cpu.max`: CPU time allowed per period.
    pub cpu_max: Option<CpuMax>,
    /// `cpu.weight`: a share of the CPU against siblings, 1 to 10000 (100 is
    /// the default).
    pub cpu_weight: Option<u16>,
    /// `cpuset.cpus`: the CPUs it may run on, as a list (`0-3,6`).
    pub cpuset_cpus: Option<String>,
    /// `cpuset.mems`: the memory nodes it may use, as a list.
    pub cpuset_mems: Option<String>,
    /// `pids.max`: the most processes and threads.
    pub pids_max: Option<u64>,
    /// `io.weight`: a share of disk time against siblings, 1 to 10000.
    pub io_weight: Option<u16>,
    /// `io.max`: bandwidth and operation limits, per device.
    pub io_max: Vec<IoMax>,
    /// Other files of the controllers, by name, written as given (OCI's
    /// `unified`): `memory.high`, say. Written last, so they win.
    pub unified: BTreeMap<String, String>,
}

/// A limit that can also be "none".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Limit {
    /// No limit (`max`).
    Max,
    /// This many.
    Value(u64),
}

/// `cpu.max`: `quota` microseconds of CPU time in every `period`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuMax {
    /// Microseconds per period; `None` is no limit (`max`).
    pub quota: Option<u64>,
    /// The period, in microseconds (1000 to 1,000,000); `None` leaves the
    /// cgroup's own (100,000 in a new one): only the quota is written.
    pub period: Option<u64>,
}

impl CpuMax {
    /// `cpu.max` from a quota and a period as runc takes them: a quota of 0
    /// or less is none (`max`), a period of 0 the cgroup's own; neither
    /// given is no `cpu.max` at all.
    #[must_use]
    pub fn from_parts(quota: Option<i64>, period: Option<u64>) -> Option<CpuMax> {
        let quota = quota.and_then(positive);
        let period = period.filter(|&period| period > 0);
        (quota.is_some() || period.is_some()).then_some(CpuMax { quota, period })
    }
}

/// One device's `io.max` line: the limits given, the others unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IoMax {
    /// The block device's major number.
    pub major: u32,
    /// Its minor number.
    pub minor: u32,
    /// Bytes read per second.
    pub rbps: Option<u64>,
    /// Bytes written per second.
    pub wbps: Option<u64>,
    /// Read operations per second.
    pub riops: Option<u64>,
    /// Write operations per second.
    pub wiops: Option<u64>,
}

impl IoMax {
    /// No limits yet, for device `major:minor`.
    #[must_use]
    pub fn device(major: u32, minor: u32) -> Self {
        Self {
            major,
            minor,
            ..Self::default()
        }
    }
}

/// One of a device's `io.max` limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoLimit {
    /// Bytes read per second.
    ReadBps(u64),
    /// Bytes written per second.
    WriteBps(u64),
    /// Read operations per second.
    ReadIops(u64),
    /// Write operations per second.
    WriteIops(u64),
}

/// A value the kernel would refuse, or that can't mean what it says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ResourceError {
    /// A memory amount that isn't a size (`512m`, `1g`, `1048576`).
    #[error("{0:?} isn't an amount of memory (such as 512m or 2g)")]
    BadSize(String),
    /// A number below the smallest the kernel takes.
    #[error("{what} must be at least {min} (got {value})")]
    TooSmall {
        /// The setting.
        what: &'static str,
        /// The value given.
        value: u64,
        /// The smallest allowed.
        min: u64,
    },
    /// A number out of the range the kernel takes.
    #[error("{what} must be between {min} and {max} (got {value})")]
    OutOfRange {
        /// The setting.
        what: &'static str,
        /// The value given.
        value: u64,
        /// The smallest allowed.
        min: u64,
        /// The largest allowed.
        max: u64,
    },
    /// A CPU or memory-node list the kernel wouldn't parse.
    #[error("{0:?} isn't a list of numbers and ranges (such as 0-3,6)")]
    BadList(String),
    /// A number of CPUs that isn't a positive number.
    #[error("{0:?} isn't a number of CPUs (such as 1.5)")]
    BadCpus(String),
    /// Fewer CPUs than the kernel's smallest quota gives.
    #[error("{0:?} CPUs is too little: at least 0.01 (the kernel's smallest quota, 1 ms in 100)")]
    TooFewCpus(String),
    /// A memory-plus-swap total that's negative but not -1 (no limit).
    #[error("memory plus swap of {0} means nothing: -1 is no limit, else a size")]
    BadSwapTotal(i64),
    /// Memory plus swap smaller than the memory alone.
    #[error(
        "memory plus swap ({total}) can't be less than the memory limit ({memory}): it's the \
         total of both"
    )]
    SwapBelowMemory {
        /// Memory plus swap.
        total: u64,
        /// The memory limit.
        memory: u64,
    },
    /// A reservation larger than the limit: it could never be met.
    #[error(
        "the memory reservation ({low} bytes) can't be more than the memory limit ({max} bytes)"
    )]
    ReservationAboveLimit {
        /// The reservation (`memory.low`).
        low: u64,
        /// The limit (`memory.max`).
        max: u64,
    },
    /// A swap limit without a memory limit to add it to.
    #[error("a limit on memory plus swap needs a memory limit too")]
    SwapWithoutMemory,
    /// An `io.max` line without a limit.
    #[error("device {0}:{1}'s I/O limit has no limit in it")]
    EmptyIoMax(u32, u32),
    /// A `unified` key that isn't a controller's file.
    #[error(
        "{0:?} isn't a controller's file (such as memory.high): a name without `/`, and not \
         one of the cgroup.* files, which move or kill processes rather than limit them"
    )]
    BadUnifiedKey(String),
    /// A device rate given for a path that isn't a block device.
    #[error("{path} isn't a block device (such as /dev/sda): {why}")]
    NotABlockDevice {
        /// The path given.
        path: std::path::PathBuf,
        /// Why.
        why: String,
    },
}

/// The smallest memory limit accepted: below a few MiB a container can't
/// even start (Docker refuses under 6 MiB too).
pub const MIN_MEMORY: u64 = 6 << 20;

impl Resources {
    /// Whether nothing is asked for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// These limits with `given`'s over them: each limit `given` sets wins,
    /// the rest stay (an `update` of what a container keeps). `cpu.max`'s
    /// quota and period each (a period given alone keeps the quota); a
    /// default swap (as much as memory) follows a new memory limit; a
    /// device's `io.max` given replaces that device's; other files are
    /// added.
    #[must_use]
    pub fn overlaid(&self, given: &Self) -> Self {
        fn take<T: Clone>(mine: &mut Option<T>, theirs: Option<&T>) {
            if let Some(theirs) = theirs {
                *mine = Some(theirs.clone());
            }
        }
        let mut limits = self.clone();
        take(&mut limits.memory_max, given.memory_max.as_ref());
        take(&mut limits.memory_low, given.memory_low.as_ref());
        take(&mut limits.pids_max, given.pids_max.as_ref());
        take(&mut limits.cpu_weight, given.cpu_weight.as_ref());
        take(&mut limits.io_weight, given.io_weight.as_ref());
        take(&mut limits.cpuset_cpus, given.cpuset_cpus.as_ref());
        take(&mut limits.cpuset_mems, given.cpuset_mems.as_ref());
        if given.swap_max.is_some() {
            limits.swap_max = given.swap_max;
            limits.swap_is_default = given.swap_is_default;
        } else if self.swap_is_default
            && let Some(memory) = given.memory_max
        {
            // Docker's default: as much swap as memory, the new memory.
            limits.swap_max = Some(Limit::Value(memory));
        }
        if let Some(cpu) = given.cpu_max {
            let (quota, period) = self
                .cpu_max
                .map_or((None, None), |kept| (kept.quota, kept.period));
            limits.cpu_max = Some(CpuMax {
                quota: cpu.quota.or(quota),
                period: cpu.period.or(period),
            });
        }
        for device in &given.io_max {
            limits
                .io_max
                .retain(|io| (io.major, io.minor) != (device.major, device.minor));
            limits.io_max.push(*device);
        }
        limits.unified.extend(
            given
                .unified
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        limits
    }

    /// Checks the values as the kernel would take them, so a mistake is
    /// reported before anything starts, naming the setting.
    ///
    /// # Errors
    ///
    /// The first value out of range, or a malformed list.
    pub fn validate(&self) -> Result<(), ResourceError> {
        for (what, value) in [
            ("the memory limit (bytes)", self.memory_max),
            ("the memory reservation (bytes)", self.memory_low),
        ] {
            if let Some(value) = value {
                at_least(what, value, MIN_MEMORY)?;
            }
        }
        if let (Some(low), Some(max)) = (self.memory_low, self.memory_max)
            && low > max
        {
            return Err(ResourceError::ReservationAboveLimit { low, max });
        }
        if let Some(cpu) = self.cpu_max {
            if let Some(period) = cpu.period {
                in_range("the CPU period (microseconds)", period, 1_000, 1_000_000)?;
            }
            if let Some(quota) = cpu.quota {
                at_least("the CPU quota (microseconds)", quota, 1_000)?;
            }
        }
        for (what, weight) in [
            ("the CPU weight", self.cpu_weight),
            ("the I/O weight", self.io_weight),
        ] {
            if let Some(weight) = weight {
                in_range(what, u64::from(weight), 1, 10_000)?;
            }
        }
        for list in [&self.cpuset_cpus, &self.cpuset_mems].into_iter().flatten() {
            check_list(list)?;
        }
        if let Some(pids) = self.pids_max {
            at_least("the process limit", pids, 1)?;
        }
        if let Some(io) = self.io_max.iter().find(|io| {
            [io.rbps, io.wbps, io.riops, io.wiops]
                .iter()
                .all(Option::is_none)
        }) {
            return Err(ResourceError::EmptyIoMax(io.major, io.minor));
        }
        if let Some(key) = self.unified.keys().find(|key| !is_controller_file(key)) {
            return Err(ResourceError::BadUnifiedKey(key.clone()));
        }
        Ok(())
    }

    /// Adds `limit` to device `major:minor`'s `io.max` line, making the line
    /// if it's the device's first: one line per device, every limit on it.
    pub fn limit_io(&mut self, major: u32, minor: u32, limit: IoLimit) {
        let index = self
            .io_max
            .iter()
            .position(|io| (io.major, io.minor) == (major, minor))
            .unwrap_or_else(|| {
                self.io_max.push(IoMax::device(major, minor));
                self.io_max.len() - 1
            });
        let Some(line) = self.io_max.get_mut(index) else {
            return;
        };
        match limit {
            IoLimit::ReadBps(rate) => line.rbps = Some(rate),
            IoLimit::WriteBps(rate) => line.wbps = Some(rate),
            IoLimit::ReadIops(rate) => line.riops = Some(rate),
            IoLimit::WriteIops(rate) => line.wiops = Some(rate),
        }
    }

    /// The cgroup files to write, and what, in order: the CPUs and memory
    /// nodes first (a cgroup with an empty cpuset can't run anything), then
    /// memory, CPU, processes and I/O, and the `unified` entries last.
    /// `io.max` comes once per device: the kernel takes one line per write.
    #[must_use]
    pub fn files(&self) -> Vec<(String, String)> {
        let mut files: Vec<(String, String)> = Vec::new();
        let mut add = |file: &str, value: String| files.push((file.to_owned(), value));
        if let Some(cpus) = &self.cpuset_cpus {
            add("cpuset.cpus", cpus.clone());
        }
        if let Some(mems) = &self.cpuset_mems {
            add("cpuset.mems", mems.clone());
        }
        if let Some(low) = self.memory_low {
            add("memory.low", low.to_string());
        }
        if let Some(max) = self.memory_max {
            add("memory.max", max.to_string());
        }
        if let Some(swap) = self.swap_max {
            add("memory.swap.max", swap.to_string());
        }
        if let Some(weight) = self.cpu_weight {
            add("cpu.weight", weight.to_string());
        }
        if let Some(CpuMax { quota, period }) = self.cpu_max {
            let quota = quota.map_or_else(|| "max".to_owned(), |quota| quota.to_string());
            // The quota alone keeps the cgroup's period (`update
            // --cpu-quota` keeps an earlier `--cpu-period`, as runc's).
            add(
                "cpu.max",
                match period {
                    Some(period) => format!("{quota} {period}"),
                    None => quota,
                },
            );
        }
        if let Some(pids) = self.pids_max {
            add("pids.max", pids.to_string());
        }
        if let Some(weight) = self.io_weight {
            add("io.weight", format!("default {weight}"));
        }
        for io in &self.io_max {
            add("io.max", io.to_string());
        }
        for (file, value) in &self.unified {
            add(file, value.clone());
        }
        files
    }

    /// The controllers its files belong to: what must be handed down to the
    /// container's cgroup. (A `unified` file of a controller Shipyard doesn't
    /// manage, `hugetlb.*` say, counts for none: its write says if it's
    /// missing.)
    #[must_use]
    pub fn controllers(&self) -> BTreeSet<Controller> {
        self.files()
            .iter()
            .filter_map(|(file, _)| Controller::from_name(file.split_once('.')?.0))
            .collect()
    }
}

/// Whether `name` is a controller's own file (`memory.high`): one path
/// component, `controller.file`, and not a `cgroup.*` core file.
fn is_controller_file(name: &str) -> bool {
    name.split_once('.').is_some_and(|(controller, file)| {
        !controller.is_empty()
            && !file.is_empty()
            && controller != "cgroup"
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
    })
}

impl std::fmt::Display for Limit {
    /// As the kernel's files say it: `max` or the number.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Max => f.write_str("max"),
            Self::Value(value) => write!(f, "{value}"),
        }
    }
}

impl std::fmt::Display for IoMax {
    /// An `io.max` line: `MAJOR:MINOR` and the limits given (`rbps=1048576`);
    /// the others are left as they are.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)?;
        for (key, value) in [
            ("rbps", self.rbps),
            ("wbps", self.wbps),
            ("riops", self.riops),
            ("wiops", self.wiops),
        ] {
            if let Some(value) = value {
                write!(f, " {key}={value}")?;
            }
        }
        Ok(())
    }
}

fn at_least(what: &'static str, value: u64, min: u64) -> Result<(), ResourceError> {
    if value >= min {
        Ok(())
    } else {
        Err(ResourceError::TooSmall { what, value, min })
    }
}

fn in_range(what: &'static str, value: u64, min: u64, max: u64) -> Result<(), ResourceError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ResourceError::OutOfRange {
            what,
            value,
            min,
            max,
        })
    }
}

/// A cpuset list: numbers and `a-b` ranges (`a <= b`), comma-separated.
fn check_list(list: &str) -> Result<(), ResourceError> {
    let bad = || ResourceError::BadList(list.to_owned());
    let number = |text: &str| -> Result<u32, ResourceError> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        text.parse().map_err(|_| bad())
    };
    for part in list.split(',') {
        match part.split_once('-') {
            Some((low, high)) if number(low)? <= number(high)? => {}
            Some(_) => return Err(bad()),
            None => {
                number(part)?;
            }
        }
    }
    Ok(())
}

/// Docker's memory sizes (`RAMInBytes`): a number, optionally with a
/// fraction, then an optional `k`, `m`, `g`, `t` or `p` (powers of 1024),
/// optionally followed by `i` and `b`, in any case: `512m`, `1.5g`,
/// `512MiB`, `1048576`, `64kb`.
///
/// # Errors
///
/// Anything else, or a size past `u64::MAX`.
pub fn parse_size(text: &str) -> Result<u64, ResourceError> {
    let bad = || ResourceError::BadSize(text.to_owned());
    let lower = text.trim().to_ascii_lowercase();
    let unit_start = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(unit_start);
    let unit = unit.trim_start();
    let unit = unit.strip_suffix('b').unwrap_or(unit);
    let unit = unit.strip_suffix('i').unwrap_or(unit);
    let shift = match unit {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        "p" => 50,
        _ => return Err(bad()),
    };
    let bytes = scale_decimal(number, 1u128 << shift).ok_or_else(bad)?;
    u64::try_from(bytes).map_err(|_| bad())
}

/// A decimal number (`1`, `1.5`, `.25`) times `unit`, exactly, rounded down:
/// "1.5" × 2^30 is 15 × 2^30 / 10. (Scaling the unit by ten per fraction
/// digit instead would lose bytes: 2^30 isn't a multiple of ten.) `None` if
/// it isn't such a number, or overflows 128 bits.
fn scale_decimal(text: &str, unit: u128) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = format!("{whole}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let numerator: u128 = digits.parse().ok()?;
    let denominator = 10u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    Some(numerator.checked_mul(unit)? / denominator)
}

/// `--cpus N` as `cpu.max`: `N` CPUs' worth of time in each 100 ms period,
/// as Docker sets it (`NanoCPUs`).
///
/// # Errors
///
/// Not a positive number, or less than 1% of a CPU (the kernel's 1 ms
/// smallest quota).
pub fn cpus(text: &str) -> Result<CpuMax, ResourceError> {
    let quota = scale_decimal(text.trim(), u128::from(CPU_PERIOD))
        .and_then(|quota| u64::try_from(quota).ok())
        .ok_or_else(|| ResourceError::BadCpus(text.to_owned()))?;
    if quota < 1_000 {
        return Err(ResourceError::TooFewCpus(text.to_owned()));
    }
    Ok(CpuMax {
        quota: Some(quota),
        period: Some(CPU_PERIOD),
    })
}

/// A count where zero or a negative number means "no limit", as Docker's
/// flags and runc read `--pids-limit`, `--cpu-quota`, `pids.limit` and
/// `cpu.quota`: the count if it's positive.
#[must_use]
pub fn positive(value: i64) -> Option<u64> {
    u64::try_from(value).ok().filter(|&value| value > 0)
}

/// cgroup v1's CPU shares (Docker's `--cpu-shares`, OCI's `cpu.shares`) as a
/// v2 `cpu.weight`, with runc's formula: the shares range 2–262144 mapped
/// linearly onto 1–10000 (1024 shares, v1's default, become 39). Zero means
/// "not set", as in Docker and runc: `None`, the kernel's default weight
/// (100), not the smallest.
#[must_use]
pub fn weight_from_shares(shares: u64) -> Option<u16> {
    if shares == 0 {
        return None;
    }
    let shares = shares.clamp(2, 262_144);
    let weight = 1 + (shares - 2) * 9_999 / 262_142;
    Some(u16::try_from(weight).unwrap_or(10_000))
}

/// cgroup v1's block I/O weight (Docker's `--blkio-weight`, OCI's
/// `blockIO.weight`, 10–1000) as a v2 `io.weight` (1–10000), with runc's
/// formula. Zero means "not set", as in Docker and runc: `None`.
///
/// # Errors
///
/// A weight outside 10–1000 (and not zero).
pub fn io_weight_from_blkio(weight: u16) -> Result<Option<u16>, ResourceError> {
    if weight == 0 {
        return Ok(None);
    }
    in_range("the block I/O weight", u64::from(weight), 10, 1_000)?;
    let converted = 1 + (u32::from(weight) - 10) * 9_999 / 990;
    Ok(Some(u16::try_from(converted).unwrap_or(10_000)))
}

/// Docker's `--memory-swap` (memory *plus* swap; `-1` for unlimited swap)
/// as `memory.swap.max` (swap alone), given the memory limit.
///
/// # Errors
///
/// A total without a memory limit, smaller than it, or negative but not -1.
pub fn swap_from_total(total: i64, memory: Option<u64>) -> Result<Limit, ResourceError> {
    // Unlimited needs no memory limit to count from (Docker's rule too).
    if total == -1 {
        return Ok(Limit::Max);
    }
    let memory = memory.ok_or(ResourceError::SwapWithoutMemory)?;
    let total = u64::try_from(total).map_err(|_| ResourceError::BadSwapTotal(total))?;
    total
        .checked_sub(memory)
        .map(Limit::Value)
        .ok_or(ResourceError::SwapBelowMemory { total, memory })
}

mod docker;

pub use docker::DockerLimits;

#[cfg(test)]
mod tests {
    use super::*;

    /// An update's limits over a container's: each one given wins (swap
    /// with its being Docker's default or not, a device's `io.max` by
    /// device), the rest stay.
    #[test]
    fn given_limits_win_over_kept_ones() {
        let kept = Resources {
            memory_max: Some(64 << 20),
            swap_max: Some(Limit::Value(64 << 20)),
            swap_is_default: true,
            pids_max: Some(2048),
            cpu_weight: Some(100),
            cpuset_cpus: Some("0".to_owned()),
            io_max: vec![IoMax {
                major: 8,
                minor: 0,
                rbps: Some(1),
                ..IoMax::default()
            }],
            ..Resources::default()
        };
        let given = Resources {
            memory_max: Some(128 << 20),
            swap_max: Some(Limit::Max),
            pids_max: Some(50),
            cpu_max: Some(CpuMax {
                quota: Some(50_000),
                period: Some(100_000),
            }),
            io_max: vec![IoMax {
                major: 8,
                minor: 0,
                wbps: Some(2),
                ..IoMax::default()
            }],
            ..Resources::default()
        };
        let both = kept.overlaid(&given);
        assert_eq!(both.memory_max, Some(128 << 20));
        assert_eq!(both.swap_max, Some(Limit::Max));
        assert!(!both.swap_is_default, "asked for, now");
        assert_eq!(both.pids_max, Some(50));
        assert_eq!(both.cpu_max, given.cpu_max);
        assert_eq!(both.cpu_weight, Some(100), "kept");
        assert_eq!(both.cpuset_cpus.as_deref(), Some("0"), "kept");
        assert_eq!(
            both.io_max, given.io_max,
            "the device's replaced, not added"
        );
        assert_eq!(kept.overlaid(&Resources::default()), kept, "nothing given");
        // A default swap follows memory; one asked for stays.
        let memory = Resources {
            memory_max: Some(256 << 20),
            ..Resources::default()
        };
        let followed = kept.overlaid(&memory);
        assert_eq!(followed.swap_max, Some(Limit::Value(256 << 20)));
        assert!(followed.swap_is_default);
        let asked = Resources {
            swap_is_default: false,
            ..kept.clone()
        };
        assert_eq!(
            asked.overlaid(&memory).swap_max,
            Some(Limit::Value(64 << 20)),
            "asked for: kept"
        );
        // `cpu.max`'s halves each: a period alone keeps the quota.
        let period = Resources {
            cpu_max: Some(CpuMax {
                quota: None,
                period: Some(50_000),
            }),
            ..Resources::default()
        };
        assert_eq!(
            both.overlaid(&period).cpu_max,
            Some(CpuMax {
                quota: Some(50_000),
                period: Some(50_000),
            })
        );
    }

    #[test]
    fn sizes_parse_as_docker_reads_them() {
        for (text, bytes) in [
            ("1048576", 1 << 20),
            ("512m", 512 << 20),
            ("512M", 512 << 20),
            ("512mb", 512 << 20),
            ("512MiB", 512 << 20),
            ("1g", 1 << 30),
            ("1.5g", 3 << 29),
            ("64k", 64 << 10),
            ("2 g", 2 << 30),
            ("0", 0),
        ] {
            assert_eq!(parse_size(text), Ok(bytes), "{text}");
        }
        for bad in [
            "",
            "m",
            "12x",
            "1.2.3g",
            "-5m",
            "18446744073709551616",
            "99999999999p",
        ] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cpus_become_a_quota_per_100ms() {
        assert_eq!(
            cpus("1.5").unwrap(),
            CpuMax {
                quota: Some(150_000),
                period: Some(100_000)
            }
        );
        assert_eq!(cpus("2").unwrap().quota, Some(200_000));
        assert_eq!(cpus(".5").unwrap().quota, Some(50_000));
        assert_eq!(cpus("0.01").unwrap().quota, Some(1_000));
        assert_eq!(
            cpus("0.001"),
            Err(ResourceError::TooFewCpus("0.001".into())),
            "below the kernel's 1 ms"
        );
        assert_eq!(
            cpus("1.000001").unwrap().quota,
            Some(100_000),
            "rounded down to a microsecond"
        );
        for bad in ["", ".", "-1", "1,5", "two"] {
            assert!(cpus(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shares_and_blkio_weights_map_as_runc_maps_them() {
        assert_eq!(weight_from_shares(1024), Some(39));
        assert_eq!(weight_from_shares(2), Some(1));
        assert_eq!(weight_from_shares(1), Some(1), "clamped");
        assert_eq!(weight_from_shares(262_144), Some(10_000));
        assert_eq!(weight_from_shares(1 << 40), Some(10_000), "clamped");
        // Zero is "not set" (the kernel's default, 100), never the smallest
        // weight: a bundle's `"shares": 0` must not starve the container.
        assert_eq!(weight_from_shares(0), None);
        assert_eq!(io_weight_from_blkio(0), Ok(None));
        assert_eq!(io_weight_from_blkio(10), Ok(Some(1)));
        assert_eq!(io_weight_from_blkio(1_000), Ok(Some(10_000)));
        assert_eq!(io_weight_from_blkio(500), Ok(Some(4_950)));
        assert!(io_weight_from_blkio(5).is_err());
        assert!(io_weight_from_blkio(1_001).is_err());
    }

    #[test]
    fn memory_plus_swap_becomes_swap_alone() {
        // The total is Docker's signed value; the memory limit is bytes.
        let (gib, gib_bytes): (i64, u64) = (1 << 30, 1 << 30);
        assert_eq!(
            swap_from_total(3 * gib, Some(gib_bytes)),
            Ok(Limit::Value(2 << 30))
        );
        assert_eq!(swap_from_total(-1, Some(gib_bytes)), Ok(Limit::Max));
        assert_eq!(
            swap_from_total(gib, Some(gib_bytes)),
            Ok(Limit::Value(0)),
            "no swap"
        );
        assert!(matches!(
            swap_from_total(gib, Some(2 << 30)),
            Err(ResourceError::SwapBelowMemory { .. })
        ));
        assert_eq!(
            swap_from_total(gib, None),
            Err(ResourceError::SwapWithoutMemory)
        );
        assert_eq!(
            swap_from_total(-1, None),
            Ok(Limit::Max),
            "Docker takes it alone"
        );
        assert_eq!(
            swap_from_total(-5, Some(gib_bytes)),
            Err(ResourceError::BadSwapTotal(-5)),
            "only -1 means no limit"
        );
    }

    #[test]
    fn values_are_checked_as_the_kernel_would() {
        let ok = Resources {
            memory_max: Some(512 << 20),
            cpu_max: Some(CpuMax {
                quota: Some(50_000),
                period: Some(100_000),
            }),
            cpu_weight: Some(100),
            cpuset_cpus: Some("0-3,6".into()),
            pids_max: Some(100),
            io_max: vec![IoMax {
                rbps: Some(1 << 20),
                ..IoMax::device(8, 0)
            }],
            ..Resources::default()
        };
        assert_eq!(ok.validate(), Ok(()));
        assert!(Resources::default().is_empty() && Resources::default().validate().is_ok());
        let bad = [
            Resources {
                memory_max: Some(1 << 20),
                ..Resources::default()
            },
            Resources {
                cpu_weight: Some(0),
                ..Resources::default()
            },
            Resources {
                cpu_weight: Some(10_001),
                ..Resources::default()
            },
            Resources {
                cpu_max: Some(CpuMax {
                    quota: Some(500),
                    period: Some(100_000),
                }),
                ..Resources::default()
            },
            Resources {
                cpu_max: Some(CpuMax {
                    quota: None,
                    period: Some(10),
                }),
                ..Resources::default()
            },
            Resources {
                cpuset_cpus: Some("3-1".into()),
                ..Resources::default()
            },
            Resources {
                cpuset_mems: Some("0,,1".into()),
                ..Resources::default()
            },
            Resources {
                pids_max: Some(0),
                ..Resources::default()
            },
            Resources {
                memory_max: Some(64 << 20),
                memory_low: Some(128 << 20),
                ..Resources::default()
            },
            Resources {
                io_max: vec![IoMax::device(8, 0)],
                ..Resources::default()
            },
        ];
        for resources in bad {
            assert!(resources.validate().is_err(), "{resources:?}");
        }
    }

    #[test]
    fn limits_become_the_kernels_files_in_order() {
        let mut limits = Resources {
            memory_max: Some(512 << 20),
            memory_low: Some(256 << 20),
            swap_max: Some(Limit::Max),
            cpu_max: Some(CpuMax {
                quota: None,
                period: Some(100_000),
            }),
            cpu_weight: Some(39),
            cpuset_cpus: Some("0-1".into()),
            cpuset_mems: Some("0".into()),
            pids_max: Some(64),
            io_weight: Some(500),
            unified: BTreeMap::from([("memory.high".into(), "400M".into())]),
            ..Resources::default()
        };
        limits.limit_io(8, 0, IoLimit::ReadBps(1 << 20));
        limits.limit_io(8, 16, IoLimit::WriteIops(100));
        limits.limit_io(8, 0, IoLimit::WriteBps(2 << 20));
        let files = limits.files();
        let pairs: Vec<(&str, &str)> = files
            .iter()
            .map(|(f, v)| (f.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("cpuset.cpus", "0-1"),
                ("cpuset.mems", "0"),
                ("memory.low", "268435456"),
                ("memory.max", "536870912"),
                ("memory.swap.max", "max"),
                ("cpu.weight", "39"),
                ("cpu.max", "max 100000"),
                ("pids.max", "64"),
                ("io.weight", "default 500"),
                // One line per device, both its limits on it.
                ("io.max", "8:0 rbps=1048576 wbps=2097152"),
                ("io.max", "8:16 wiops=100"),
                ("memory.high", "400M"),
            ]
        );
        assert_eq!(
            limits.controllers().into_iter().collect::<Vec<_>>(),
            Controller::ALL
        );
        assert!(Resources::default().files().is_empty());
        assert!(Resources::default().controllers().is_empty());
    }

    /// A quota with no period given writes the quota alone, so the cgroup
    /// keeps its own period (an `update --cpu-quota` after a run with
    /// `--cpu-period`, as runc's); with one, both.
    #[test]
    fn a_quota_alone_keeps_the_period() {
        let cpu_max = |period| Resources {
            cpu_max: Some(CpuMax {
                quota: Some(50_000),
                period,
            }),
            ..Resources::default()
        };
        assert_eq!(
            cpu_max(None).files(),
            [("cpu.max".to_owned(), "50000".to_owned())]
        );
        assert_eq!(
            cpu_max(Some(200_000)).files(),
            [("cpu.max".to_owned(), "50000 200000".to_owned())]
        );
    }

    #[test]
    fn zero_and_negative_counts_are_no_limit() {
        assert_eq!(positive(64), Some(64));
        for none in [0, -1, -5, i64::MIN] {
            assert_eq!(positive(none), None, "{none}");
        }
    }

    #[test]
    fn unified_keys_are_controller_files() {
        for good in [
            "memory.high",
            "cpu.max.burst",
            "hugetlb.2MB.max",
            "io.latency",
        ] {
            let limits = Resources {
                unified: BTreeMap::from([(good.into(), "1".into())]),
                ..Resources::default()
            };
            assert_eq!(limits.validate(), Ok(()), "{good}");
        }
        for bad in [
            "cgroup.procs",
            "cgroup.kill",
            "memory",
            "../memory.max",
            "child/memory.max",
            ".max",
            "memory.",
            "memory high",
        ] {
            let limits = Resources {
                unified: BTreeMap::from([(bad.into(), "1".into())]),
                ..Resources::default()
            };
            assert_eq!(
                limits.validate(),
                Err(ResourceError::BadUnifiedKey(bad.into())),
                "{bad}"
            );
        }
    }
}
