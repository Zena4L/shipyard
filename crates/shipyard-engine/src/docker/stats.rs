//! `docker stats` (step 11.3d2): what a container uses, in Docker's
//! `ContainerStats` shape: its cgroup's numbers (the runtime's), the
//! host's CPU time for its share (`/proc/stat`), its network namespace's
//! counters (its init's `/proc/PID/net/dev`). Each sample carries the one
//! before as `precpu_stats`, for a CPU percentage between the two.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::Engine;
use crate::error::Result;

/// What a container uses, Docker's `ContainerStats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ContainerStats {
    /// When it was read (Docker's time).
    pub read: String,
    /// When the sample before was (Docker's zero time for none).
    pub preread: String,
    /// The container's ID.
    pub id: String,
    /// Its name, as Docker writes it (`/web`).
    pub name: String,
    /// `linux`.
    pub os_type: String,
    /// Always 0 (Windows').
    pub num_procs: u32,
    /// Its processes.
    pub pids_stats: PidsStats,
    /// Its CPU time, and the host's.
    pub cpu_stats: CpuStats,
    /// The sample before's (zeros for none).
    pub precpu_stats: CpuStats,
    /// Its memory.
    pub memory_stats: MemoryStats,
    /// Its block I/O.
    pub blkio_stats: BlkioStats,
    /// Its interfaces' counters, by name (`lo` left out), while it runs
    /// in a network of its own; left out otherwise, as Docker leaves it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub networks: Option<BTreeMap<String, NetworkStats>>,
}

/// Docker's `PidsStats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct PidsStats {
    /// Its processes and threads now.
    pub current: u64,
    /// Its limit (`u64::MAX`: none, as Docker writes it).
    pub limit: u64,
}

/// Docker's `CPUStats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CpuStats {
    /// Its CPU time.
    pub cpu_usage: CpuUsage,
    /// The host's CPU time since boot, in nanoseconds (all its CPUs').
    pub system_cpu_usage: u64,
    /// The host's CPUs online.
    pub online_cpus: u32,
    /// `cpu.max` at work.
    pub throttling_data: ThrottlingData,
}

/// Docker's `CPUUsage`, in nanoseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CpuUsage {
    /// In all.
    pub total_usage: u64,
    /// In the kernel.
    pub usage_in_kernelmode: u64,
    /// In user space.
    pub usage_in_usermode: u64,
}

/// Docker's `ThrottlingData`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ThrottlingData {
    /// Periods with something runnable.
    pub periods: u64,
    /// Periods it was held back in.
    pub throttled_periods: u64,
    /// How long it was held back, in nanoseconds.
    pub throttled_time: u64,
}

/// Docker's `MemoryStats` (cgroup v2's).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct MemoryStats {
    /// In use now, in bytes (the page cache too: a client takes
    /// `stats.inactive_file` off, as Docker's CLI does).
    pub usage: u64,
    /// Its limit, or the host's memory without one (Docker's).
    pub limit: u64,
    /// `memory.stat`, every line.
    pub stats: BTreeMap<String, u64>,
}

/// Docker's `BlkioStats`: what cgroup v2 has of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct BlkioStats {
    /// Bytes read and written, per device (`null`: none, as Docker's).
    pub io_service_bytes_recursive: Option<Vec<BlkioEntry>>,
    /// Operations, per device.
    pub io_serviced_recursive: Option<Vec<BlkioEntry>>,
    /// cgroup v1's, always `null` (as Docker's on v2).
    pub io_queue_recursive: Option<Vec<BlkioEntry>>,
    /// cgroup v1's, always `null`.
    pub io_service_time_recursive: Option<Vec<BlkioEntry>>,
    /// cgroup v1's, always `null`.
    pub io_wait_time_recursive: Option<Vec<BlkioEntry>>,
    /// cgroup v1's, always `null`.
    pub io_merged_recursive: Option<Vec<BlkioEntry>>,
    /// cgroup v1's, always `null`.
    pub io_time_recursive: Option<Vec<BlkioEntry>>,
    /// cgroup v1's, always `null`.
    pub sectors_recursive: Option<Vec<BlkioEntry>>,
}

/// One of Docker's `BlkioStatEntry`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct BlkioEntry {
    /// The device's major number.
    pub major: u32,
    /// Its minor number.
    pub minor: u32,
    /// `read` or `write`.
    pub op: String,
    /// How much.
    pub value: u64,
}

/// Docker's `NetworkStats`: an interface's counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NetworkStats {
    /// Bytes received.
    pub rx_bytes: u64,
    /// Packets received.
    pub rx_packets: u64,
    /// Receive errors.
    pub rx_errors: u64,
    /// Received packets dropped.
    pub rx_dropped: u64,
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Packets sent.
    pub tx_packets: u64,
    /// Send errors.
    pub tx_errors: u64,
    /// Sent packets dropped.
    pub tx_dropped: u64,
}

/// What the container `reference` names uses now, with `previous` (the
/// sample before, if any) as its `precpu_stats`. One that isn't running
/// gives Docker's zeros.
///
/// # Errors
///
/// No such container; its cgroup can't be read (one without a cgroup of
/// its own: rootless without delegation); off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn stats(
    engine: &Engine,
    reference: &str,
    previous: Option<&ContainerStats>,
) -> Result<ContainerStats> {
    #[cfg(target_os = "linux")]
    return linux::stats(engine, reference, previous);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, previous);
        Err(crate::error::linux_only("containers"))
    }
}

/// `/proc/stat`'s first line as the host's CPU time in nanoseconds (each
/// state's clock ticks, summed), and how many CPUs it lists.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "read by `stats`, which is Linux's (and the tests)"
    )
)]
pub(crate) fn host_cpu(text: &str, ticks: u64) -> (u64, u32) {
    let total: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("cpu "))
        .map_or(0, |line| {
            // user nice system idle iowait irq softirq, as Docker's
            // daemon counts them (`parts[1:8]`): not steal, nor guest and
            // guest_nice (in user and nice already).
            line.split_whitespace()
                .take(7)
                .filter_map(|ticks| ticks.parse::<u64>().ok())
                .sum()
        });
    let cpus = text
        .lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count();
    (
        total.saturating_mul(1_000_000_000 / ticks.max(1)),
        u32::try_from(cpus).unwrap_or(u32::MAX),
    )
}

/// `/proc/PID/net/dev`'s interfaces, `lo` left out.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "read by `stats`, which is Linux's (and the tests)"
    )
)]
pub(crate) fn net_dev(text: &str) -> BTreeMap<String, NetworkStats> {
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, counters) = line.split_once(':')?;
            let name = name.trim();
            if name == "lo" {
                return None;
            }
            let numbers: Vec<u64> = counters
                .split_whitespace()
                .map(|number| number.parse().ok())
                .collect::<Option<_>>()?;
            // Received: bytes packets errs drop fifo frame compressed
            // multicast; sent: bytes (8) packets errs drop …
            let at = |index: usize| numbers.get(index).copied();
            Some((
                name.to_owned(),
                NetworkStats {
                    rx_bytes: at(0)?,
                    rx_packets: at(1)?,
                    rx_errors: at(2)?,
                    rx_dropped: at(3)?,
                    tx_bytes: at(8)?,
                    tx_packets: at(9)?,
                    tx_errors: at(10)?,
                    tx_dropped: at(11)?,
                },
            ))
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        BlkioEntry, BlkioStats, ContainerStats, CpuStats, CpuUsage, MemoryStats, PidsStats,
        ThrottlingData, host_cpu, net_dev,
    };
    use crate::Engine;
    use crate::containers;
    use crate::error::Result;

    pub(super) fn stats(
        engine: &Engine,
        reference: &str,
        previous: Option<&ContainerStats>,
    ) -> Result<ContainerStats> {
        let record = containers::find(engine, reference)?;
        let mut sample = ContainerStats {
            read: crate::docker::time(std::time::SystemTime::now()),
            preread: previous.map_or_else(
                || crate::docker::ZERO_TIME.to_owned(),
                |before| before.read.clone(),
            ),
            id: record.id.as_str().to_owned(),
            name: format!("/{}", record.name),
            os_type: "linux".to_owned(),
            precpu_stats: previous
                .map(|before| before.cpu_stats.clone())
                .unwrap_or_default(),
            ..ContainerStats::default()
        };
        if !is_up(engine, &record)? {
            // Docker's: zeros for one that isn't running.
            return Ok(sample);
        }
        let root = engine.paths.runtime_root()?;
        let (cgroup, _oom) = match shipyard_runtime::container::stats(root, record.id.as_str()) {
            Ok(read) => read,
            // Ended since its state was read: Docker's zeros, never an
            // error for a stop.
            Err(_) if !is_up(engine, &record)? => return Ok(sample),
            Err(err) => return Err(err.into()),
        };
        let ticks = crate::docker::proc::clock_ticks();
        let (system, cpus) =
            std::fs::read_to_string("/proc/stat").map_or((0, 0), |text| host_cpu(&text, ticks));
        sample.cpu_stats = CpuStats {
            cpu_usage: CpuUsage {
                total_usage: cgroup.cpu.usage.total,
                usage_in_kernelmode: cgroup.cpu.usage.kernel,
                usage_in_usermode: cgroup.cpu.usage.user,
            },
            system_cpu_usage: system,
            online_cpus: cpus,
            throttling_data: ThrottlingData {
                periods: cgroup.cpu.throttling.periods,
                throttled_periods: cgroup.cpu.throttling.throttled_periods,
                throttled_time: cgroup.cpu.throttling.throttled_time,
            },
        };
        let limit = cgroup.memory.usage.limit;
        sample.memory_stats = MemoryStats {
            usage: cgroup.memory.usage.usage,
            limit: if limit == u64::MAX {
                crate::docker::system::mem_total()
            } else {
                limit
            },
            stats: cgroup.memory.raw.clone(),
        };
        sample.pids_stats = PidsStats {
            current: cgroup.pids.current,
            limit: if cgroup.pids.limit == 0 {
                u64::MAX
            } else {
                cgroup.pids.limit
            },
        };
        let entries = |list: &[shipyard_runtime::cgroup::BlkioEntry]| {
            (!list.is_empty()).then(|| {
                list.iter()
                    .map(|entry| BlkioEntry {
                        major: entry.major,
                        minor: entry.minor,
                        op: entry.op.to_lowercase(),
                        value: entry.value,
                    })
                    .collect()
            })
        };
        sample.blkio_stats = BlkioStats {
            io_service_bytes_recursive: entries(&cgroup.blkio.io_service_bytes_recursive),
            io_serviced_recursive: entries(&cgroup.blkio.io_serviced_recursive),
            ..BlkioStats::default()
        };
        // Another container's network is that one's to count (Docker shows
        // none for one that joins it).
        let joins_another = containers::network_from(engine, &record.id).is_some();
        sample.networks = shipyard_runtime::container::peek_init(root, record.id.as_str())
            .ok()
            .flatten()
            .filter(|_| !joins_another)
            .and_then(|(pid, started)| own_networks(pid, started));
        Ok(sample)
    }

    /// Whether `record`'s container is running or paused now.
    fn is_up(engine: &Engine, record: &containers::Record) -> Result<bool> {
        Ok(containers::state(engine, record)?.is_up())
    }

    /// The most of `/proc/PID/net/dev` read: a line an interface, and a
    /// container can make many.
    const NET_DEV_MAX: u64 = 1 << 20;

    /// The counters of the network namespace init `pid` (started at
    /// `started`) is in: none if it's this process's own (the host's
    /// network, `--network host`: Docker shows none), or if `pid` is no
    /// longer that init (it ended, and the PID was reused) once read.
    fn own_networks(
        pid: i32,
        started: u64,
    ) -> Option<std::collections::BTreeMap<String, super::NetworkStats>> {
        use std::os::unix::fs::MetadataExt as _;
        let namespace = |path: &str| std::fs::metadata(path).ok().map(|meta| meta.ino());
        let its = namespace(&format!("/proc/{pid}/ns/net"))?;
        if namespace("/proc/self/ns/net") == Some(its) {
            return None;
        }
        let text = crate::docker::proc::read(&format!("/proc/{pid}/net/dev"), NET_DEV_MAX)?;
        shipyard_runtime::container::is_process(pid, started).then(|| net_dev(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's CPU time from `/proc/stat` (each state's ticks, summed, in
    /// nanoseconds), and its CPUs.
    #[test]
    fn the_host_s_cpu_time_is_its_ticks() {
        let text = "cpu  100 0 50 1000 0 0 0 40 30 7\n\
                    cpu0 50 0 25 500 0 0 0 0 0 0\n\
                    cpu1 50 0 25 500 0 0 0 0 0 0\n\
                    intr 1 2 3\n\
                    btime 1792504484\n";
        assert_eq!(host_cpu(text, 100), (1150 * 10_000_000, 2));
    }

    /// An interface's counters, `lo` left out, a garbled line skipped.
    #[test]
    fn interfaces_are_read_from_net_dev() {
        let text = "Inter-|   Receive                                                |  Transmit\n \
                    face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
                    lo:     100       1    0    0    0     0          0         0      100       1    0    0    0     0       0          0\n  \
                    eth0:     648       6    1    2    0     0          0         0       84       2    3    4    0     0       0          0\n  \
                    bad: x y\n";
        let read = net_dev(text);
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(
            read["eth0"],
            NetworkStats {
                rx_bytes: 648,
                rx_packets: 6,
                rx_errors: 1,
                rx_dropped: 2,
                tx_bytes: 84,
                tx_packets: 2,
                tx_errors: 3,
                tx_dropped: 4,
            }
        );
    }

    /// The keys are Docker 29.8's `GET /containers/{id}/stats` (2026-10-20's
    /// capture), but `storage_stats` (Windows'); `networks` only while it
    /// runs in a network of its own; blkio's eight lists, cgroup v1's `null`.
    #[test]
    fn the_stats_are_docker_s() {
        let shown = serde_json::to_value(ContainerStats::default()).unwrap();
        let mut keys: Vec<&str> = shown
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "blkio_stats",
                "cpu_stats",
                "id",
                "memory_stats",
                "name",
                "num_procs",
                "os_type",
                "pids_stats",
                "precpu_stats",
                "preread",
                "read"
            ]
        );
        let cpu = serde_json::to_value(CpuStats::default()).unwrap();
        for key in [
            "cpu_usage",
            "system_cpu_usage",
            "online_cpus",
            "throttling_data",
        ] {
            assert!(cpu.get(key).is_some(), "{key}");
        }
        let blkio = serde_json::to_value(BlkioStats::default()).unwrap();
        assert_eq!(blkio.as_object().unwrap().len(), 8, "{blkio}");
        assert!(
            blkio
                .as_object()
                .unwrap()
                .values()
                .all(serde_json::Value::is_null)
        );
        let running = ContainerStats {
            networks: Some(BTreeMap::new()),
            ..ContainerStats::default()
        };
        assert_eq!(
            serde_json::to_value(&running).unwrap()["networks"],
            serde_json::json!({})
        );
    }
}
