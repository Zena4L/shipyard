//! What a container has used, from its cgroup's files, in the shape runc's
//! `events` prints (its `types.Stats`, the fields cgroup v2 has), so engines
//! that read runc's read Shipyard's.
//!
//! Pure: [`Stats::read`] takes a function that reads one of the cgroup's
//! files, so it's tested anywhere. A file that's missing (its controller
//! isn't handed down) leaves its part zero.

use std::collections::BTreeMap;

use serde::Serialize;

use super::parse_keyed;

/// A cgroup's usage, as runc's `events` data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Stats {
    /// CPU time and throttling.
    pub cpu: Cpu,
    /// Memory and swap.
    pub memory: Memory,
    /// Processes and threads.
    pub pids: Pids,
    /// Block I/O.
    pub blkio: Blkio,
}

/// CPU time (`cpu.stat`), in nanoseconds, as runc gives it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Cpu {
    /// Time used.
    pub usage: CpuUsage,
    /// Time held back by `cpu.max`.
    pub throttling: Throttling,
}

/// Time used, in nanoseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CpuUsage {
    /// In all.
    pub total: u64,
    /// In the kernel.
    pub kernel: u64,
    /// In user space.
    pub user: u64,
}

/// `cpu.max` at work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Throttling {
    /// Periods with something runnable.
    pub periods: u64,
    /// Periods it was held back in.
    pub throttled_periods: u64,
    /// How long it was held back, in nanoseconds.
    pub throttled_time: u64,
}

/// Memory, in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Memory {
    /// The page cache (`memory.stat`'s `file`).
    pub cache: u64,
    /// Memory.
    pub usage: MemoryEntry,
    /// Swap.
    pub swap: MemoryEntry,
    /// `memory.stat`, every line.
    pub raw: BTreeMap<String, u64>,
}

/// One kind of memory: use and limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct MemoryEntry {
    /// The limit; `u64::MAX` for none, as runc says it.
    pub limit: u64,
    /// In use now.
    pub usage: u64,
    /// The most ever in use (`memory.peak`).
    pub max: u64,
    /// How often the limit was hit (`memory.events`' `max`).
    pub failcnt: u64,
}

/// Processes and threads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Pids {
    /// How many now.
    pub current: u64,
    /// The limit; 0 for none, as runc says it.
    pub limit: u64,
}

/// Block I/O, per device and operation (`io.stat`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Blkio {
    /// Bytes read and written.
    pub io_service_bytes_recursive: Vec<BlkioEntry>,
    /// Operations: reads and writes.
    pub io_serviced_recursive: Vec<BlkioEntry>,
}

/// One device's count of one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct BlkioEntry {
    /// The device's major number.
    pub major: u32,
    /// Its minor number.
    pub minor: u32,
    /// `Read` or `Write`.
    pub op: &'static str,
    /// The count.
    pub value: u64,
}

impl Stats {
    /// The stats of a cgroup whose file `name` `read` returns (`None` if
    /// it's missing).
    #[must_use]
    pub fn read(read: impl Fn(&str) -> Option<String>) -> Self {
        let keyed = |file: &str| {
            read(file)
                .map(|text| parse_keyed(&text))
                .unwrap_or_default()
        };
        let number = |file: &str| read(file).and_then(|text| text.trim().parse::<u64>().ok());
        let cpu = keyed("cpu.stat");
        let micros = |key: &str| cpu.get(key).copied().unwrap_or(0).saturating_mul(1_000);
        let memory_stat = keyed("memory.stat");
        let (io_bytes, io_ops) = parse_io_stat(&read("io.stat").unwrap_or_default());
        Self {
            cpu: Cpu {
                usage: CpuUsage {
                    total: micros("usage_usec"),
                    kernel: micros("system_usec"),
                    user: micros("user_usec"),
                },
                throttling: Throttling {
                    periods: cpu.get("nr_periods").copied().unwrap_or(0),
                    throttled_periods: cpu.get("nr_throttled").copied().unwrap_or(0),
                    throttled_time: micros("throttled_usec"),
                },
            },
            memory: Memory {
                cache: memory_stat.get("file").copied().unwrap_or(0),
                usage: MemoryEntry {
                    limit: limit(read("memory.max")),
                    usage: number("memory.current").unwrap_or(0),
                    max: number("memory.peak").unwrap_or(0),
                    failcnt: keyed("memory.events").get("max").copied().unwrap_or(0),
                },
                swap: MemoryEntry {
                    limit: limit(read("memory.swap.max")),
                    usage: number("memory.swap.current").unwrap_or(0),
                    max: number("memory.swap.peak").unwrap_or(0),
                    failcnt: keyed("memory.swap.events").get("max").copied().unwrap_or(0),
                },
                raw: memory_stat,
            },
            pids: Pids {
                current: number("pids.current").unwrap_or(0),
                // runc's "no limit" here is 0.
                limit: number("pids.max").unwrap_or(0),
            },
            blkio: Blkio {
                io_service_bytes_recursive: io_bytes,
                io_serviced_recursive: io_ops,
            },
        }
    }
}

/// A limit file (`memory.max`): its number, or `u64::MAX` for `max` or a
/// missing file, as runc says "no limit".
fn limit(text: Option<String>) -> u64 {
    text.and_then(|text| text.trim().parse().ok())
        .unwrap_or(u64::MAX)
}

/// `io.stat`: one line per device, `MAJ:MIN rbytes=… wbytes=… rios=… wios=…
/// dbytes=… dios=…`. Returns the bytes and the operations, as runc's
/// `Read`/`Write` entries; discards and lines that don't fit are left out.
#[must_use]
pub fn parse_io_stat(text: &str) -> (Vec<BlkioEntry>, Vec<BlkioEntry>) {
    let mut bytes = Vec::new();
    let mut ops = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some((major, minor)) = fields.next().and_then(|device| {
            let (major, minor) = device.split_once(':')?;
            Some((major.parse().ok()?, minor.parse().ok()?))
        }) else {
            continue;
        };
        for (key, value) in fields.filter_map(|field| field.split_once('=')) {
            let Ok(value) = value.parse() else {
                continue;
            };
            let (list, op) = match key {
                "rbytes" => (&mut bytes, "Read"),
                "wbytes" => (&mut bytes, "Write"),
                "rios" => (&mut ops, "Read"),
                "wios" => (&mut ops, "Write"),
                _ => continue,
            };
            list.push(BlkioEntry {
                major,
                minor,
                op,
                value,
            });
        }
    }
    (bytes, ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup's files, as a v2 kernel writes them.
    fn file(name: &str) -> Option<String> {
        Some(
            match name {
                "cpu.stat" => {
                    "usage_usec 1500\nuser_usec 1000\nsystem_usec 500\nnr_periods 10\n\
                     nr_throttled 3\nthrottled_usec 20\n"
                }
                "memory.max" => "67108864\n",
                "memory.current" => "4096\n",
                "memory.peak" => "8192\n",
                "memory.events" => "low 0\nhigh 0\nmax 2\noom 1\noom_kill 1\n",
                "memory.swap.current" => "0\n",
                "memory.stat" => "anon 1024\nfile 2048\n",
                "pids.current" => "3\n",
                "memory.swap.max" | "pids.max" => "max\n",
                "io.stat" => "8:0 rbytes=512 wbytes=1024 rios=1 wios=2 dbytes=0 dios=0\n",
                _ => return None,
            }
            .to_owned(),
        )
    }

    #[test]
    fn stats_read_as_runc_reports_them() {
        let stats = Stats::read(file);
        assert_eq!(stats.cpu.usage.total, 1_500_000, "nanoseconds");
        assert_eq!(
            (stats.cpu.usage.user, stats.cpu.usage.kernel),
            (1_000_000, 500_000)
        );
        assert_eq!(stats.cpu.throttling.throttled_periods, 3);
        assert_eq!(stats.cpu.throttling.throttled_time, 20_000);
        assert_eq!(stats.memory.usage.limit, 67_108_864);
        assert_eq!(
            (stats.memory.usage.usage, stats.memory.usage.max),
            (4096, 8192)
        );
        assert_eq!(stats.memory.usage.failcnt, 2);
        assert_eq!(stats.memory.swap.limit, u64::MAX, "max: no limit");
        assert_eq!(stats.memory.cache, 2048);
        assert_eq!((stats.pids.current, stats.pids.limit), (3, 0));
        assert_eq!(stats.blkio.io_service_bytes_recursive.len(), 2);
        assert_eq!(stats.blkio.io_serviced_recursive[1].value, 2);
    }

    #[test]
    fn the_json_has_runcs_field_names() {
        let json = serde_json::to_value(Stats::read(file)).unwrap();
        assert_eq!(json["cpu"]["usage"]["total"], 1_500_000);
        assert_eq!(json["cpu"]["throttling"]["throttledPeriods"], 3);
        assert_eq!(json["memory"]["usage"]["failcnt"], 2);
        assert_eq!(json["memory"]["raw"]["anon"], 1024);
        assert_eq!(json["pids"]["current"], 3);
        assert_eq!(json["blkio"]["ioServiceBytesRecursive"][0]["op"], "Read");
        assert_eq!(json["blkio"]["ioServicedRecursive"][0]["major"], 8);
    }

    #[test]
    fn missing_files_leave_their_part_empty() {
        let stats = Stats::read(|_| None);
        assert_eq!(stats.cpu.usage.total, 0);
        assert_eq!(stats.memory.usage.limit, u64::MAX);
        assert!(stats.blkio.io_service_bytes_recursive.is_empty());
    }

    #[test]
    fn io_stat_lines_that_dont_fit_are_skipped() {
        let (bytes, ops) = parse_io_stat("garbage\n8:x rbytes=1\n259:0 rbytes=7 dbytes=9 wios=x\n");
        assert_eq!(bytes.len(), 1);
        assert_eq!((bytes[0].major, bytes[0].value), (259, 7));
        assert!(ops.is_empty());
    }
}
