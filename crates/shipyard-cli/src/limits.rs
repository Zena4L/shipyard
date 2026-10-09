//! `shipyard run`'s resource limits: Docker's flags, as cgroup v2 takes
//! them ([`Resources`]). Each converts the way Docker does, through the
//! runtime's `resources` helpers, so `--memory 512m --cpus 1.5` means here
//! what it means to `docker run`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use shipyard_runtime::resources::{self, CpuMax, IoLimit, Limit, Resources};

/// The limit flags. One group, so `--bundle` (which has its own
/// `linux.resources`) and `--no-isolation` (no cgroup) conflict with all of
/// them at once.
#[derive(Debug, Default, Args)]
#[group(id = "limits", multiple = true)]
pub(crate) struct LimitArgs {
    /// The most memory the container may use (such as 512m or 2g); past it,
    /// the kernel's OOM killer stops a process (Linux; rootless where a cgroup
    /// subtree is delegated).
    #[arg(short, long, value_name = "SIZE", value_parser = parse_size)]
    memory: Option<u64>,

    /// Memory kept for the container when the host runs short (cgroup v2's
    /// memory.low).
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    memory_reservation: Option<u64>,

    /// Memory plus swap (needs --memory, except -1): -1 for unlimited swap. Without it,
    /// the container may swap as much as --memory, as with Docker.
    #[arg(long, value_name = "SIZE", value_parser = parse_swap_total, allow_hyphen_values = true)]
    memory_swap: Option<i64>,

    /// How many CPUs' worth of time the container may use (such as 1.5).
    #[arg(long, value_name = "NUMBER", conflicts_with_all = ["cpu_period", "cpu_quota"])]
    cpus: Option<String>,

    /// The CPU period, in microseconds (1000 to 1000000; default 100000).
    #[arg(long, value_name = "MICROSECONDS")]
    cpu_period: Option<u64>,

    /// CPU time per period, in microseconds (-1 or 0 for no limit).
    #[arg(long, value_name = "MICROSECONDS", allow_hyphen_values = true)]
    cpu_quota: Option<i64>,

    /// The container's CPU share against others, in Docker's units (1024 is
    /// the default share).
    #[arg(short = 'c', long, value_name = "SHARES", alias = "cpu-share")]
    cpu_shares: Option<u64>,

    /// The CPUs the container may run on (such as 0-3 or 0,2).
    #[arg(long, value_name = "LIST")]
    cpuset_cpus: Option<String>,

    /// The memory nodes the container may use (such as 0 or 0-1).
    #[arg(long, value_name = "LIST")]
    cpuset_mems: Option<String>,

    /// The most processes (and threads) in the container, its init included
    /// (-1 or 0 for no limit).
    #[arg(long, value_name = "NUMBER", allow_hyphen_values = true)]
    pids_limit: Option<i64>,

    /// The container's disk-time share, in Docker's units (10 to 1000).
    #[arg(long, value_name = "WEIGHT")]
    blkio_weight: Option<u16>,

    /// Limits reads from a block device, in bytes per second
    /// (/dev/sda:10mb). Repeatable.
    #[arg(long, value_name = "DEVICE:RATE", value_parser = parse_device_size)]
    device_read_bps: Vec<(PathBuf, u64)>,

    /// Limits writes to a block device, in bytes per second. Repeatable.
    #[arg(long, value_name = "DEVICE:RATE", value_parser = parse_device_size)]
    device_write_bps: Vec<(PathBuf, u64)>,

    /// Limits reads from a block device, in operations per second
    /// (/dev/sda:1000). Repeatable.
    #[arg(long, value_name = "DEVICE:RATE", value_parser = parse_device_count)]
    device_read_iops: Vec<(PathBuf, u64)>,

    /// Limits writes to a block device, in operations per second. Repeatable.
    #[arg(long, value_name = "DEVICE:RATE", value_parser = parse_device_count)]
    device_write_iops: Vec<(PathBuf, u64)>,
}

impl LimitArgs {
    /// The flags' first name, if any limit is given: for refusing them where
    /// they can't work.
    pub(crate) fn first_given(&self) -> Option<&'static str> {
        let resources_asked = [
            (self.memory.is_some(), "--memory"),
            (self.memory_reservation.is_some(), "--memory-reservation"),
            (self.memory_swap.is_some(), "--memory-swap"),
            (self.cpus.is_some(), "--cpus"),
            (self.cpu_period.is_some(), "--cpu-period"),
            (self.cpu_quota.is_some(), "--cpu-quota"),
            (self.cpu_shares.is_some(), "--cpu-shares"),
            (self.cpuset_cpus.is_some(), "--cpuset-cpus"),
            (self.cpuset_mems.is_some(), "--cpuset-mems"),
            (self.pids_limit.is_some(), "--pids-limit"),
            (self.blkio_weight.is_some(), "--blkio-weight"),
            (!self.device_read_bps.is_empty(), "--device-read-bps"),
            (!self.device_write_bps.is_empty(), "--device-write-bps"),
            (!self.device_read_iops.is_empty(), "--device-read-iops"),
            (!self.device_write_iops.is_empty(), "--device-write-iops"),
        ];
        resources_asked
            .into_iter()
            .find_map(|(given, flag)| given.then_some(flag))
    }

    /// The limits for a new container, as the kernel takes them, checked;
    /// with Docker's default swap (as much as memory, without
    /// `--memory-swap`).
    ///
    /// # Errors
    ///
    /// A value the kernel wouldn't take, flags that contradict each other, or
    /// a device that isn't a block device.
    pub(crate) fn resources(&self) -> Result<Resources> {
        self.limits(true, None)
    }

    /// The limits given, and nothing else, for a running container (runc's
    /// `update`): `--memory` alone changes memory only, not swap.
    /// `--memory-swap` without `--memory` counts from `memory_now`, the
    /// container's memory limit.
    ///
    /// # Errors
    ///
    /// As [`LimitArgs::resources`].
    pub(crate) fn resources_as_given(&self, memory_now: Option<u64>) -> Result<Resources> {
        let mut limits = self.limits(false, memory_now)?;
        // Only what was given: the memory it counted from stays as it is.
        if !self.memory_given() {
            limits.memory_max = None;
        }
        Ok(limits)
    }

    /// Whether `--memory-swap` is given without `--memory` (for an update,
    /// it then needs the container's own memory limit).
    pub(crate) fn swap_without_memory(&self) -> bool {
        self.memory_swap.is_some_and(|total| total != 0) && !self.memory_given()
    }

    /// Whether `--memory` asks for a limit (0 is Docker's "not set").
    fn memory_given(&self) -> bool {
        self.memory.is_some_and(|bytes| bytes > 0)
    }

    fn limits(&self, docker_swap: bool, memory_now: Option<u64>) -> Result<Resources> {
        // Docker's: 0 is "not set" for each of these (as for --cpu-shares,
        // --pids-limit and --blkio-weight below).
        let memory = self.memory.filter(|&bytes| bytes > 0);
        let memory_swap = self.memory_swap.filter(|&total| total != 0);
        let mut limits = Resources::default();
        limits.memory_max = memory;
        limits.memory_low = self.memory_reservation.filter(|&bytes| bytes > 0);
        limits.cpu_weight = self.cpu_shares.and_then(resources::weight_from_shares);
        limits.cpuset_cpus.clone_from(&self.cpuset_cpus);
        limits.cpuset_mems.clone_from(&self.cpuset_mems);
        limits.pids_max = self.pids_limit.and_then(resources::positive);
        (limits.swap_max, limits.swap_is_default) = match (memory_swap, memory.or(memory_now)) {
            (Some(total), memory) => (Some(resources::swap_from_total(total, memory)?), false),
            // Docker's rule: as much swap as memory.
            (None, Some(memory)) if docker_swap => (Some(Limit::Value(memory)), true),
            (None, _) => (None, false),
        };
        limits.cpu_max = self.cpu_max()?;
        if let Some(weight) = self.blkio_weight {
            limits.io_weight = resources::io_weight_from_blkio(weight)?;
        }
        self.limit_io(&mut limits)?;
        limits.validate()?;
        Ok(limits)
    }

    /// `cpu.max`: from `--cpus`, or `--cpu-quota` and `--cpu-period`.
    fn cpu_max(&self) -> Result<Option<CpuMax>> {
        if let Some(cpus) = self.cpus.as_deref().filter(|cpus| !is_zero(cpus)) {
            return Ok(Some(resources::cpus(cpus)?));
        }
        Ok(CpuMax::from_parts(self.cpu_quota, self.cpu_period))
    }

    /// `io.max`: one line per device, with every limit given for it.
    fn limit_io(&self, limits: &mut Resources) -> Result<()> {
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
            // 0 is "not set", as for every flag (and as the OCI reader takes
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
fn block_device(path: &Path) -> Result<(u32, u32)> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let metadata = std::fs::metadata(path).with_context(|| path.display().to_string())?;
    if !metadata.file_type().is_block_device() {
        bail!("{} isn't a block device (such as /dev/sda)", path.display());
    }
    let device = metadata.rdev();
    #[cfg(target_os = "linux")]
    let numbers = (libc::major(device), libc::minor(device));
    #[cfg(not(target_os = "linux"))]
    let numbers = {
        let too_large = || anyhow::anyhow!("{}'s device number is too large", path.display());
        let device = libc::dev_t::try_from(device).map_err(|_| too_large())?;
        (
            u32::try_from(libc::major(device)).map_err(|_| too_large())?,
            u32::try_from(libc::minor(device)).map_err(|_| too_large())?,
        )
    };
    Ok(numbers)
}

fn parse_size(text: &str) -> Result<u64, String> {
    resources::parse_size(text).map_err(|err| err.to_string())
}

/// `--memory-swap`: -1, or a size.
fn parse_swap_total(text: &str) -> Result<i64, String> {
    if text.trim() == "-1" {
        return Ok(-1);
    }
    let size = parse_size(text)?;
    i64::try_from(size).map_err(|_| format!("{text:?} is too large"))
}

/// `DEVICE:RATE`, the rate a size (`10mb`). The last `:` splits them.
fn parse_device_size(text: &str) -> Result<(PathBuf, u64), String> {
    let (path, rate) = split_device(text)?;
    Ok((path, parse_size(rate)?))
}

/// `DEVICE:RATE`, the rate a plain number.
fn parse_device_count(text: &str) -> Result<(PathBuf, u64), String> {
    let (path, rate) = split_device(text)?;
    let rate = rate
        .parse()
        .map_err(|_| format!("{rate:?} isn't a number of operations per second"))?;
    Ok((path, rate))
}

fn split_device(text: &str) -> Result<(PathBuf, &str), String> {
    match text.rsplit_once(':') {
        Some((path, rate)) if !path.is_empty() && !rate.is_empty() => {
            Ok((PathBuf::from(path), rate))
        }
        _ => Err(format!(
            "{text:?} isn't DEVICE:RATE (such as /dev/sda:10mb)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        limits: LimitArgs,
    }

    fn resources(args: &[&str]) -> Result<Resources> {
        let cli =
            Cli::try_parse_from([&["shipyard"], args].concat()).map_err(anyhow::Error::from)?;
        cli.limits.resources()
    }

    #[test]
    fn memory_gets_as_much_swap_as_docker_gives() {
        let limits = resources(&["--memory", "512m"]).unwrap();
        assert_eq!(limits.memory_max, Some(512 << 20));
        assert_eq!(limits.swap_max, Some(Limit::Value(512 << 20)));
        assert!(limits.swap_is_default);
        // Memory plus swap equal to memory: no swap.
        let none = resources(&["-m", "512m", "--memory-swap", "512m"]).unwrap();
        assert_eq!(none.swap_max, Some(Limit::Value(0)));
        assert!(!none.swap_is_default);
        let unlimited = resources(&["-m", "512m", "--memory-swap", "-1"]).unwrap();
        assert_eq!(unlimited.swap_max, Some(Limit::Max));
        assert!(
            resources(&["--memory-swap", "1g"]).is_err(),
            "needs --memory"
        );
        assert_eq!(
            resources(&["--memory-swap", "-1"]).unwrap().swap_max,
            Some(Limit::Max),
            "unlimited needs none"
        );
        assert!(resources(&["-m", "1g", "--memory-swap", "512m"]).is_err());
        assert!(resources(&["-m", "1m"]).is_err(), "below 6 MiB");
    }

    /// A rate of 0 is "not set", as for every flag: nothing goes to
    /// `io.max` (which would refuse it), and no device is even looked at.
    #[test]
    fn a_zero_device_rate_is_no_limit() {
        let limits = resources(&["--device-read-bps", "/no/such/device:0"]).unwrap();
        assert!(limits.is_empty());
    }

    #[test]
    fn cpu_flags_become_cpu_max_and_weight() {
        let cpus = resources(&["--cpus", "1.5", "-c", "512"]).unwrap();
        assert_eq!(
            cpus.cpu_max,
            Some(CpuMax {
                quota: Some(150_000),
                period: Some(100_000)
            })
        );
        assert_eq!(cpus.cpu_weight, resources::weight_from_shares(512));
        let quota = resources(&["--cpu-quota", "50000", "--cpu-period", "200000"]).unwrap();
        assert_eq!(
            quota.cpu_max,
            Some(CpuMax {
                quota: Some(50_000),
                period: Some(200_000)
            })
        );
        for none in ["-1", "0"] {
            let no_quota = resources(&["--cpu-quota", none]).unwrap();
            assert_eq!(no_quota.cpu_max, None, "{none}: as Docker reads it");
        }
        assert!(resources(&["--cpus", "1", "--cpu-quota", "5000"]).is_err());
        assert!(resources(&["--cpu-period", "10"]).is_err(), "below 1 ms");
    }

    #[test]
    fn pids_and_cpusets_and_weights() {
        let limits = resources(&[
            "--pids-limit",
            "64",
            "--cpuset-cpus",
            "0-1",
            "--cpuset-mems",
            "0",
            "--blkio-weight",
            "500",
        ])
        .unwrap();
        assert_eq!(limits.pids_max, Some(64));
        assert_eq!(limits.cpuset_cpus.as_deref(), Some("0-1"));
        assert_eq!(limits.cpuset_mems.as_deref(), Some("0"));
        assert_eq!(
            limits.io_weight,
            resources::io_weight_from_blkio(500).unwrap()
        );
        for unlimited in ["-1", "0"] {
            assert_eq!(
                resources(&["--pids-limit", unlimited]).unwrap().pids_max,
                None
            );
        }
        assert!(resources(&["--cpuset-cpus", "3-1"]).is_err());
        assert!(resources(&["--blkio-weight", "5"]).is_err());
    }

    #[test]
    fn device_limits_need_a_block_device() {
        let err = resources(&["--device-read-bps", "/dev/null:1mb"]).unwrap_err();
        assert!(err.to_string().contains("isn't a block device"), "{err}");
        assert!(resources(&["--device-read-bps", "/dev/null"]).is_err());
        assert!(resources(&["--device-read-iops", "/dev/null:fast"]).is_err());
    }

    #[test]
    fn zero_is_not_set_as_docker_reads_it() {
        for zero in [
            &["--memory", "0"][..],
            &["--memory-reservation", "0"],
            &["--memory", "0", "--memory-swap", "0"],
            &["--cpus", "0"],
            &["--cpus", "0.0"],
            &["--cpu-period", "0"],
            &["--cpu-quota", "0", "--cpu-period", "0"],
            &["--cpu-shares", "0"],
            &["--pids-limit", "0"],
            &["--blkio-weight", "0"],
        ] {
            assert!(resources(zero).unwrap().is_empty(), "{zero:?}");
        }
        // --memory-swap 0 with a memory limit: Docker's default swap, as if
        // it weren't given.
        let limits = resources(&["--memory", "64m", "--memory-swap", "0"]).unwrap();
        assert!(limits.swap_is_default);
        assert!(resources(&["--cpus", "0.001"]).is_err(), "small, not zero");
    }

    #[test]
    fn an_update_changes_only_what_it_names() {
        let cli =
            Cli::try_parse_from(["shipyard", "--memory", "128m", "--cpu-share", "512"]).unwrap();
        let limits = cli.limits.resources_as_given(None).unwrap();
        assert_eq!(limits.memory_max, Some(128 << 20));
        assert_eq!(limits.swap_max, None, "no Docker default swap");
        assert_eq!(
            limits.cpu_weight,
            resources::weight_from_shares(512),
            "runc's spelling"
        );
    }

    #[test]
    fn an_update_of_swap_alone_counts_from_the_memory_limit() {
        let cli = Cli::try_parse_from(["shipyard", "--memory-swap", "96m"]).unwrap();
        assert!(cli.limits.swap_without_memory());
        let limits = cli.limits.resources_as_given(Some(64 << 20)).unwrap();
        assert_eq!(limits.swap_max, Some(Limit::Value(32 << 20)));
        assert_eq!(limits.memory_max, None, "the memory limit stays as it is");
        assert!(
            cli.limits.resources_as_given(None).is_err(),
            "no memory limit to count from"
        );
    }

    #[test]
    fn nothing_given_is_no_limits() {
        let cli = Cli::try_parse_from(["shipyard"]).unwrap();
        assert_eq!(cli.limits.first_given(), None);
        assert!(cli.limits.resources().unwrap().is_empty());
        let cli = Cli::try_parse_from(["shipyard", "--pids-limit", "5"]).unwrap();
        assert_eq!(cli.limits.first_given(), Some("--pids-limit"));
    }
}
