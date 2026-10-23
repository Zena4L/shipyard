//! Docker's resource limits, as `docker run` and Docker's Engine API take
//! them, made [`Resources`] the way Docker makes them (step 11.2: one
//! mapping for `shipyard run`'s flags, `shipyard-runtime update`'s and the
//! API's `HostConfig`): 0 is "not set" for each, as much swap as memory
//! unless asked otherwise, CPU shares as a weight, `--cpus` as `cpu.max`.

use std::path::{Path, PathBuf};

use super::{CpuMax, IoLimit, Limit, ResourceError, Resources};

/// Docker's limits, as given: each `None` (or empty) when not given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DockerLimits {
    /// `--memory` / `Memory`, in bytes.
    pub memory: Option<u64>,
    /// `--memory-reservation` / `MemoryReservation`, in bytes.
    pub memory_reservation: Option<u64>,
    /// `--memory-swap` / `MemorySwap`: memory plus swap, in bytes; -1 is
    /// no limit.
    pub memory_swap: Option<i64>,
    /// `--cpus`, a decimal (`1.5`); the API's `NanoCpus` as one
    /// ([`DockerLimits::nano_cpus`]).
    pub cpus: Option<String>,
    /// `--cpu-period` / `CpuPeriod`, in microseconds.
    pub cpu_period: Option<u64>,
    /// `--cpu-quota` / `CpuQuota`, in microseconds (-1 or 0: no limit).
    pub cpu_quota: Option<i64>,
    /// `--cpu-shares` / `CpuShares`, Docker's units.
    pub cpu_shares: Option<u64>,
    /// `--cpuset-cpus` / `CpusetCpus`.
    pub cpuset_cpus: Option<String>,
    /// `--cpuset-mems` / `CpusetMems`.
    pub cpuset_mems: Option<String>,
    /// `--pids-limit` / `PidsLimit` (-1 or 0: no limit).
    pub pids_limit: Option<i64>,
    /// `--blkio-weight` / `BlkioWeight` (10 to 1000).
    pub blkio_weight: Option<u16>,
    /// `--device-read-bps`: each device's bytes per second.
    pub device_read_bps: Vec<(PathBuf, u64)>,
    /// `--device-write-bps`.
    pub device_write_bps: Vec<(PathBuf, u64)>,
    /// `--device-read-iops`: each device's operations per second.
    pub device_read_iops: Vec<(PathBuf, u64)>,
    /// `--device-write-iops`.
    pub device_write_iops: Vec<(PathBuf, u64)>,
}

impl DockerLimits {
    /// The API's `NanoCpus` (CPUs times 10⁹) as `--cpus` writes it: a
    /// decimal (`1500000000` is `1.5`); 0 or less is none.
    #[must_use]
    pub fn nano_cpus(nano: i64) -> Option<String> {
        let nano = u64::try_from(nano).ok().filter(|&nano| nano > 0)?;
        let (whole, part) = (nano / 1_000_000_000, nano % 1_000_000_000);
        Some(if part == 0 {
            whole.to_string()
        } else {
            let part = format!("{part:09}");
            format!("{whole}.{}", part.trim_end_matches('0'))
        })
    }

    /// The limits for a new container, as the kernel takes them, checked;
    /// with Docker's default swap (as much as memory, without a swap total).
    ///
    /// # Errors
    ///
    /// A value the kernel wouldn't take, limits that contradict each
    /// other, or a device that isn't a block device.
    pub fn resources(&self) -> Result<Resources, ResourceError> {
        self.limits(true, None)
    }

    /// The limits given, and nothing else, for a running container (runc's
    /// `update`): a memory limit alone changes memory only, not swap; a
    /// swap total without one counts from `memory_now`, the container's.
    ///
    /// # Errors
    ///
    /// As [`DockerLimits::resources`].
    pub fn resources_as_given(&self, memory_now: Option<u64>) -> Result<Resources, ResourceError> {
        let mut limits = self.limits(false, memory_now)?;
        // Only what was given: the memory it counted from stays as it is.
        if !self.memory_given() {
            limits.memory_max = None;
        }
        Ok(limits)
    }

    /// Whether these limits ask anything: Docker's 0 (and an empty list)
    /// is "not set" for each, as its CLI and API send for every limit not
    /// given.
    #[must_use]
    pub fn asks_anything(&self) -> bool {
        let set = |value: Option<u64>| value.is_some_and(|value| value != 0);
        let signed = |value: Option<i64>| value.is_some_and(|value| value != 0);
        let text = |value: &Option<String>| value.as_deref().is_some_and(|text| !text.is_empty());
        set(self.memory)
            || set(self.memory_reservation)
            || signed(self.memory_swap)
            || self.cpus.as_deref().is_some_and(|cpus| {
                // `0` (and `0.0`) is "not set"; anything else asks, even what
                // isn't a number (it's refused, not ignored).
                !cpus.is_empty() && !cpus.parse::<f64>().is_ok_and(|cpus| cpus == 0.0)
            })
            || set(self.cpu_period)
            || signed(self.cpu_quota)
            || set(self.cpu_shares)
            || text(&self.cpuset_cpus)
            || text(&self.cpuset_mems)
            || signed(self.pids_limit)
            || self.blkio_weight.is_some_and(|weight| weight != 0)
            || !self.device_read_bps.is_empty()
            || !self.device_write_bps.is_empty()
            || !self.device_read_iops.is_empty()
            || !self.device_write_iops.is_empty()
    }

    /// Whether a swap total is given without a memory limit (an update
    /// then needs the container's own memory limit).
    #[must_use]
    pub fn swap_without_memory(&self) -> bool {
        self.memory_swap.is_some_and(|total| total != 0) && !self.memory_given()
    }

    /// Whether a memory limit is asked for (0 is Docker's "not set").
    fn memory_given(&self) -> bool {
        self.memory.is_some_and(|bytes| bytes > 0)
    }

    fn limits(
        &self,
        docker_swap: bool,
        memory_now: Option<u64>,
    ) -> Result<Resources, ResourceError> {
        // Docker's: 0 is "not set" for each of these (as for the shares,
        // the pids limit and the blkio weight below).
        let memory = self.memory.filter(|&bytes| bytes > 0);
        let memory_swap = self.memory_swap.filter(|&total| total != 0);
        let (swap_max, swap_is_default) = match (memory_swap, memory.or(memory_now)) {
            (Some(total), memory) => (Some(super::swap_from_total(total, memory)?), false),
            // Docker's rule: as much swap as memory.
            (None, Some(memory)) if docker_swap => (Some(Limit::Value(memory)), true),
            (None, _) => (None, false),
        };
        let io_weight = match self.blkio_weight {
            Some(weight) => super::io_weight_from_blkio(weight)?,
            None => None,
        };
        let mut limits = Resources {
            memory_max: memory,
            memory_low: self.memory_reservation.filter(|&bytes| bytes > 0),
            swap_max,
            swap_is_default,
            cpu_weight: self.cpu_shares.and_then(super::weight_from_shares),
            cpu_max: self.cpu_max()?,
            cpuset_cpus: self.cpuset_cpus.clone(),
            cpuset_mems: self.cpuset_mems.clone(),
            pids_max: self.pids_limit.and_then(super::positive),
            io_weight,
            ..Resources::default()
        };
        self.limit_io(&mut limits)?;
        limits.validate()?;
        Ok(limits)
    }

    /// `cpu.max`: from the CPUs, or the quota and period.
    fn cpu_max(&self) -> Result<Option<CpuMax>, ResourceError> {
        if let Some(cpus) = self.cpus.as_deref().filter(|cpus| !is_zero(cpus)) {
            return Ok(Some(super::cpus(cpus)?));
        }
        Ok(CpuMax::from_parts(self.cpu_quota, self.cpu_period))
    }

    /// `io.max`: one line per device, with every limit given for it.
    fn limit_io(&self, limits: &mut Resources) -> Result<(), ResourceError> {
        // (The first constructor's type is spelled out; the others follow.)
        let flags = [
            (
                self.device_read_bps.as_slice(),
                IoLimit::ReadBps as fn(u64) -> IoLimit,
            ),
            (&self.device_write_bps, IoLimit::WriteBps),
            (&self.device_read_iops, IoLimit::ReadIops),
            (&self.device_write_iops, IoLimit::WriteIops),
        ];
        for (given, limit) in flags {
            // 0 is "not set", as for every limit (and as the OCI reader takes
            // it): `io.max` would refuse it.
            for (path, rate) in given.iter().filter(|(_, rate)| *rate > 0) {
                let (major, minor) = block_device(path)?;
                limits.limit_io(major, minor, limit(*rate));
            }
        }
        Ok(())
    }
}

/// Whether `text` is a decimal zero (`0`, `0.0`, `.0`): Docker's "no limit"
/// for `--cpus`.
fn is_zero(text: &str) -> bool {
    let text = text.trim();
    text.contains('0')
        && text.chars().all(|c| c == '0' || c == '.')
        && text.matches('.').count() <= 1
}

/// The major and minor numbers of the block device at `path`.
fn block_device(path: &Path) -> Result<(u32, u32), ResourceError> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let refused = |why: String| ResourceError::NotABlockDevice {
        path: path.to_owned(),
        why,
    };
    let metadata = std::fs::metadata(path).map_err(|error| refused(error.to_string()))?;
    if !metadata.file_type().is_block_device() {
        return Err(refused("another kind of file".to_owned()));
    }
    let device = metadata.rdev();
    #[cfg(target_os = "linux")]
    let numbers = (libc::major(device), libc::minor(device));
    #[cfg(not(target_os = "linux"))]
    let numbers = {
        let too_large = || refused("its device number is too large".to_owned());
        let device = libc::dev_t::try_from(device).map_err(|_| too_large())?;
        (
            u32::try_from(libc::major(device)).map_err(|_| too_large())?,
            u32::try_from(libc::minor(device)).map_err(|_| too_large())?,
        )
    };
    Ok(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NanoCpus` as `--cpus` writes it.
    #[test]
    fn nano_cpus_are_a_decimal() {
        assert_eq!(
            DockerLimits::nano_cpus(1_500_000_000).as_deref(),
            Some("1.5")
        );
        assert_eq!(DockerLimits::nano_cpus(2_000_000_000).as_deref(), Some("2"));
        assert_eq!(DockerLimits::nano_cpus(10_000_000).as_deref(), Some("0.01"));
        assert_eq!(DockerLimits::nano_cpus(0), None);
        assert_eq!(DockerLimits::nano_cpus(-1), None);
    }

    /// The API's create with `NanoCpus` gives the `cpu.max` `--cpus` does.
    #[test]
    fn nano_cpus_limit_as_cpus_do() {
        let from_nano = DockerLimits {
            cpus: DockerLimits::nano_cpus(1_500_000_000),
            ..DockerLimits::default()
        };
        let from_flag = DockerLimits {
            cpus: Some("1.5".to_owned()),
            ..DockerLimits::default()
        };
        assert_eq!(from_nano.resources(), from_flag.resources());
        assert!(from_nano.resources().unwrap().cpu_max.is_some());
    }
}
