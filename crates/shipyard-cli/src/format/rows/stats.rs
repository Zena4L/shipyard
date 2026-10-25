//! `stats`' rows: a container's usage as Docker's `--format` sees it, its
//! percentages worked out as Docker's CLI works them out.

use serde::Serialize;
use shipyard_engine::docker::ContainerStats;

use crate::format::template::Row;

use super::short;
use super::units::{binary_size, human_size};

/// What a container uses, Docker's `statsContext`: Docker's CLI's own
/// computations over a sample and the one before it (its `precpu_stats`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatsRow {
    #[serde(rename = "BlockIO")]
    block_io: String,
    #[serde(rename = "CPUPerc")]
    cpu_perc: String,
    #[serde(rename = "Container")]
    container: String,
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "MemPerc")]
    mem_perc: String,
    #[serde(rename = "MemUsage")]
    mem_usage: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "NetIO")]
    net_io: String,
    #[serde(rename = "PIDs")]
    pids: String,
}

impl StatsRow {
    /// `stats` as Docker's `stats` shows it, the container named as it was
    /// asked for (`container`), its ID cut short unless `full`.
    pub(crate) fn new(stats: &ContainerStats, container: &str, full: bool) -> Self {
        let used = memory_used(stats);
        let limit = stats.memory_stats.limit;
        let (rx, tx) = stats
            .networks
            .iter()
            .flatten()
            .fold((0, 0), |(rx, tx), (_, net)| {
                (rx + net.rx_bytes, tx + net.tx_bytes)
            });
        let (read, written) = stats
            .blkio_stats
            .io_service_bytes_recursive
            .iter()
            .flatten()
            .fold((0, 0), |(read, written), entry| {
                match entry.op.to_ascii_lowercase().as_str() {
                    "read" => (read + entry.value, written),
                    "write" => (read, written + entry.value),
                    _ => (read, written),
                }
            });
        Self {
            block_io: format!("{} / {}", human_size(read, 3), human_size(written, 3)),
            cpu_perc: format!("{:.2}%", cpu_percent(stats)),
            container: container.to_owned(),
            id: short(&stats.id, full),
            mem_perc: format!("{:.2}%", percent(used, limit)),
            mem_usage: format!("{} / {}", binary_size(used), binary_size(limit)),
            name: stats
                .name
                .strip_prefix('/')
                .unwrap_or(&stats.name)
                .to_owned(),
            net_io: format!("{} / {}", human_size(rx, 3), human_size(tx, 3)),
            pids: stats.pids_stats.current.to_string(),
        }
    }
}

impl Row for StatsRow {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("BlockIO", "BLOCK I/O"),
            ("CPUPerc", "CPU %"),
            ("Container", "CONTAINER"),
            ("ID", "CONTAINER ID"),
            ("MemPerc", "MEM %"),
            ("MemUsage", "MEM USAGE / LIMIT"),
            ("Name", "NAME"),
            ("NetIO", "NET I/O"),
            ("PIDs", "PIDS"),
        ]
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// `stats`' default table (Docker's).
pub(crate) const STATS_TABLE: &str = "table {{.ID}}\t{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}\t{{.NetIO}}\t{{.BlockIO}}\t{{.PIDs}}";

/// Docker's CLI's `calculateCPUPercentUnix`: the container's CPU time
/// between the two samples over the host's, times its CPUs; 0 when either
/// didn't grow.
fn cpu_percent(stats: &ContainerStats) -> f64 {
    let (now, before) = (&stats.cpu_stats, &stats.precpu_stats);
    let cpu = now
        .cpu_usage
        .total_usage
        .checked_sub(before.cpu_usage.total_usage);
    let system = now.system_cpu_usage.checked_sub(before.system_cpu_usage);
    match (cpu, system) {
        (Some(cpu), Some(system)) if cpu > 0 && system > 0 => {
            percent(cpu, system) * f64::from(now.online_cpus)
        }
        _ => 0.0,
    }
}

/// Docker's CLI's `calculateMemUsageUnixNoCache`: the memory in use less
/// the page cache nothing is using (`inactive_file`, cgroup v2's;
/// `total_inactive_file`, v1's), which the kernel takes back first.
fn memory_used(stats: &ContainerStats) -> u64 {
    let memory = &stats.memory_stats;
    let inactive = memory
        .stats
        .get("inactive_file")
        .or_else(|| memory.stats.get("total_inactive_file"))
        .copied()
        .unwrap_or(0);
    if inactive < memory.usage {
        memory.usage - inactive
    } else {
        memory.usage
    }
}

/// `part` as a percentage of `whole` (0 of nothing).
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a percentage shown to two decimals"
    )]
    let share = part as f64 / whole as f64;
    share * 100.0
}

#[cfg(test)]
mod tests;
