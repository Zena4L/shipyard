//! What everything uses now (step 14.8a2): the desktop UI's rail (memory
//! in use), Manifest (each container's CPU and memory) and status bar (the
//! monitors' memory), in one read. Plain counters, never rates: the host's
//! memory (`/proc/meminfo`: the host's on Linux, the guest's on a Mac), its
//! CPU time and CPUs (`/proc/stat`), each running container's CPU time and
//! memory (its cgroup's) and its monitor's memory (the monitor's cgroup's,
//! by the container's ID: this store's monitors alone, never a stopped
//! container's leftover cgroup). A reader divides two reads for a CPU
//! share, as Docker's CLI does with `stats`. Each cgroup is read without
//! the runtime's lock on its container: a start, delete or exec under way
//! never holds the read up. A container whose cgroup can't be read
//! (rootless without delegation, or ended as it was read) is left out.

use serde::Serialize;

use crate::Engine;
use crate::error::Result;

/// The answer's version (`GET /shipyard/usage`).
pub const USAGE_VERSION: u32 = 1;

/// What everything uses now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Usage {
    /// [`USAGE_VERSION`].
    pub version: u32,
    /// The host's memory.
    pub memory: HostMemory,
    /// The host's CPU time.
    pub cpu: HostCpu,
    /// Each running or paused container's, by ID.
    pub containers: Vec<ContainerUsage>,
}

/// The host's memory, bytes (`/proc/meminfo`; 0: unread).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct HostMemory {
    /// `MemTotal`.
    pub total: u64,
    /// `MemAvailable`: what can be had without swapping (the kernel's
    /// estimate, more than `MemFree`: its caches can be dropped).
    pub available: u64,
}

/// The host's CPU time since boot (`/proc/stat`'s, as Docker's
/// `system_cpu_usage`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct HostCpu {
    /// In all its CPUs, nanoseconds.
    pub time: u64,
    /// Its CPUs online.
    pub cpus: u32,
}

/// What a container uses (its cgroup's).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ContainerUsage {
    /// Its ID.
    pub id: String,
    /// Its CPU time, nanoseconds.
    pub cpu_time: u64,
    /// Its memory, bytes, less what the kernel can drop at once
    /// (`inactive_file`), as Docker's CLI says it.
    pub memory: u64,
    /// Its monitor's memory, bytes (none: no cgroup of its own, a
    /// foreground run's or rootless without delegation).
    pub monitor_memory: Option<u64>,
}

/// The most of `/proc/meminfo` and `/proc/stat` read (a line a CPU in
/// the latter).
pub(crate) const PROC_MAX: u64 = 1 << 20;

/// `/proc/meminfo`'s `MemTotal` and `MemAvailable`, bytes (0 for one it
/// doesn't say).
pub(crate) fn meminfo(text: &str) -> HostMemory {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| {
                let kib = line.strip_prefix(name)?.strip_prefix(':')?;
                kib.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
            })
            .map_or(0, |kib| kib.saturating_mul(1024))
    };
    HostMemory {
        total: field("MemTotal"),
        available: field("MemAvailable"),
    }
}

/// What everything uses now: see [`Usage`].
///
/// # Errors
///
/// The container store can't be read; off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn usage(engine: &Engine) -> Result<Usage> {
    #[cfg(target_os = "linux")]
    return linux::usage(engine);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        Err(crate::error::linux_only("containers"))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{ContainerUsage, HostCpu, PROC_MAX, USAGE_VERSION, Usage, meminfo};
    use crate::Engine;
    use crate::error::Result;

    pub(super) fn usage(engine: &Engine) -> Result<Usage> {
        let root = engine.paths.runtime_root()?;
        let mut containers = Vec::new();
        for record in crate::containers::list(engine)? {
            // One whose state can't be read (removed since the list) is
            // left out, as one whose cgroup can't be: never the whole read.
            if !crate::containers::state(engine, &record).is_ok_and(|state| state.is_up()) {
                continue;
            }
            // Ended since its state was read, or no cgroup of its own: left out.
            let Ok(Some(cgroup)) =
                shipyard_runtime::container::peek_stats(root, record.id.as_str())
            else {
                continue;
            };
            let inactive = cgroup.memory.raw.get("inactive_file").copied().unwrap_or(0);
            containers.push(ContainerUsage {
                id: record.id.as_str().to_owned(),
                cpu_time: cgroup.cpu.usage.total,
                memory: cgroup.memory.usage.usage.saturating_sub(inactive),
                monitor_memory: shipyard_runtime::container::monitor_memory(record.id.as_str()),
            });
        }
        let (time, cpus) = crate::docker::proc::read("/proc/stat", PROC_MAX)
            .map_or((0, 0), |text| {
                crate::docker::stats::host_cpu(&text, crate::docker::proc::clock_ticks())
            });
        Ok(Usage {
            version: USAGE_VERSION,
            memory: crate::docker::proc::read("/proc/meminfo", PROC_MAX)
                .map(|text| meminfo(&text))
                .unwrap_or_default(),
            cpu: HostCpu { time, cpus },
            containers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's memory is `MemTotal` and `MemAvailable` (never `MemFree`,
    /// which leaves out the caches the kernel can drop), in bytes.
    #[test]
    fn the_host_s_memory_is_what_meminfo_says_available() {
        let text = "MemTotal:        4028776 kB\n\
                    MemFree:          123456 kB\n\
                    MemAvailable:    3000000 kB\n\
                    Buffers:           10000 kB\n";
        assert_eq!(
            meminfo(text),
            HostMemory {
                total: 4_028_776 * 1024,
                available: 3_000_000 * 1024,
            }
        );
        // A field it doesn't say (a kernel before 3.14 has no MemAvailable).
        assert_eq!(meminfo("MemTotal: 1 kB\n").available, 0);
    }

    /// The answer says its version and its keys, as a reader reads them.
    #[test]
    fn the_answer_says_its_version() {
        let usage = Usage {
            version: USAGE_VERSION,
            containers: vec![ContainerUsage {
                id: "c".to_owned(),
                cpu_time: 5,
                memory: 7,
                monitor_memory: None,
            }],
            ..Usage::default()
        };
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["containers"][0]["cpu_time"], 5);
        assert_eq!(json["memory"]["available"], 0);
        assert_eq!(
            json["containers"][0]["monitor_memory"],
            serde_json::Value::Null
        );
        assert_eq!(json["cpu"]["cpus"], 0);
    }
}
