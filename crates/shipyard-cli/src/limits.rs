//! `shipyard run`'s resource limits: Docker's flags, as cgroup v2 takes
//! them ([`Resources`]). Each converts the way Docker does, through the
//! runtime's `resources` helpers, so `--memory 512m --cpus 1.5` means here
//! what it means to `docker run`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use shipyard_runtime::resources::{self, DockerLimits, Resources};

/// The limit flags. One group, so `--bundle` (which has its own
/// `linux.resources`) and `--no-isolation` (no cgroup) conflict with all of
/// them at once.
#[derive(Debug, Default, Args)]
#[group(id = "limits", multiple = true)]
pub struct LimitArgs {
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

/// The limits `update` changes, Docker's `update`'s: those a running
/// container's cgroup takes at once (no device rates: those are given when
/// it's made). Each not given stays as it is.
#[derive(Debug, Default, Args)]
#[group(id = "update_limits", multiple = true)]
pub struct UpdateLimitArgs {
    /// The most memory the container may use (such as 512m or 2g). Its
    /// swap, if it's the default (as much as its memory), follows.
    #[arg(short, long, value_name = "SIZE", value_parser = parse_size)]
    memory: Option<u64>,

    /// Memory kept for the container when the host runs short.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    memory_reservation: Option<u64>,

    /// Memory plus swap: counted from --memory, or the container's own
    /// memory limit without it; -1 for unlimited swap.
    #[arg(long, value_name = "SIZE", value_parser = parse_swap_total, allow_hyphen_values = true)]
    memory_swap: Option<i64>,

    /// How many CPUs' worth of time the container may use (such as 1.5).
    #[arg(long, value_name = "NUMBER", conflicts_with_all = ["cpu_period", "cpu_quota"])]
    cpus: Option<String>,

    /// The CPU period, in microseconds (1000 to 1000000); its quota stays.
    #[arg(long, value_name = "MICROSECONDS")]
    cpu_period: Option<u64>,

    /// CPU time per period, in microseconds; its period stays.
    #[arg(long, value_name = "MICROSECONDS")]
    cpu_quota: Option<u64>,

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
    /// (a number: a limit isn't lifted yet).
    #[arg(long, value_name = "NUMBER")]
    pids_limit: Option<u64>,

    /// The container's disk-time share, in Docker's units (10 to 1000).
    #[arg(long, value_name = "WEIGHT")]
    blkio_weight: Option<u16>,
}

impl UpdateLimitArgs {
    /// The flags as Docker's limits (the engine's one mapping of them).
    #[must_use]
    pub fn docker(&self) -> DockerLimits {
        let signed =
            |value: Option<u64>| value.map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        let mut limits = DockerLimits::default();
        limits.memory = self.memory;
        limits.memory_reservation = self.memory_reservation;
        limits.memory_swap = self.memory_swap;
        limits.cpus.clone_from(&self.cpus);
        limits.cpu_period = self.cpu_period;
        limits.cpu_quota = signed(self.cpu_quota);
        limits.cpu_shares = self.cpu_shares;
        limits.cpuset_cpus.clone_from(&self.cpuset_cpus);
        limits.cpuset_mems.clone_from(&self.cpuset_mems);
        limits.pids_limit = signed(self.pids_limit);
        limits.blkio_weight = self.blkio_weight;
        limits
    }
}

impl LimitArgs {
    /// Whether `--pids-limit` asks something: a limit, or `-1` (none).
    /// Not given, or `0` (Docker's "unset", what its CLI sends without the
    /// flag), the engine's default applies (F62).
    #[must_use]
    pub fn pids_given(&self) -> bool {
        self.pids_limit.is_some_and(|limit| limit != 0)
    }

    /// The flags' first name, if any limit is given: for refusing them where
    /// they can't work.
    #[must_use]
    pub fn first_given(&self) -> Option<&'static str> {
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
    pub fn resources(&self) -> Result<Resources> {
        Ok(self.docker().resources()?)
    }

    /// The limits given, and nothing else, for a running container (runc's
    /// `update`): `--memory` alone changes memory only, not swap.
    /// `--memory-swap` without `--memory` counts from `memory_now`, the
    /// container's memory limit.
    ///
    /// # Errors
    ///
    /// As [`LimitArgs::resources`].
    pub fn resources_as_given(&self, memory_now: Option<u64>) -> Result<Resources> {
        Ok(self.docker().resources_as_given(memory_now)?)
    }

    /// Whether `--memory-swap` is given without `--memory` (for an update,
    /// it then needs the container's own memory limit).
    #[must_use]
    pub fn swap_without_memory(&self) -> bool {
        self.docker().swap_without_memory()
    }

    /// The flags as Docker's limits, for the runtime's one mapping of them
    /// (the API's too: `DockerLimits`).
    fn docker(&self) -> DockerLimits {
        let mut limits = DockerLimits::default();
        limits.memory = self.memory;
        limits.memory_reservation = self.memory_reservation;
        limits.memory_swap = self.memory_swap;
        limits.cpus.clone_from(&self.cpus);
        limits.cpu_period = self.cpu_period;
        limits.cpu_quota = self.cpu_quota;
        limits.cpu_shares = self.cpu_shares;
        limits.cpuset_cpus.clone_from(&self.cpuset_cpus);
        limits.cpuset_mems.clone_from(&self.cpuset_mems);
        limits.pids_limit = self.pids_limit;
        limits.blkio_weight = self.blkio_weight;
        limits.device_read_bps.clone_from(&self.device_read_bps);
        limits.device_write_bps.clone_from(&self.device_write_bps);
        limits.device_read_iops.clone_from(&self.device_read_iops);
        limits.device_write_iops.clone_from(&self.device_write_iops);
        limits
    }
}

/// A size (`64k`, `10m`, `1g`), as `--memory` takes it.
pub fn parse_size(text: &str) -> Result<u64, String> {
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
    use shipyard_runtime::resources::{CpuMax, Limit};

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
