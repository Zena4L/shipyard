use std::collections::BTreeMap;

use super::*;

/// Docker's CLI's `stats` row over a sample and the one before: its CPU
/// share times the CPUs, memory less `inactive_file`, every interface's
/// and device's bytes summed, as Docker 29.8 printed them (2026-10-20's
/// capture: `1.125MiB / 64MiB`, `648B / 84B`, `2.29MB / 0B`).
#[test]
fn a_stats_row_is_docker_s() {
    use shipyard_engine::docker::{BlkioEntry, NetworkStats};
    let mut stats = ContainerStats::default();
    stats.id = "0123456789abcdef0123".to_owned();
    stats.name = "/web".to_owned();
    stats.precpu_stats.cpu_usage.total_usage = 1_000_000;
    stats.precpu_stats.system_cpu_usage = 100_000_000;
    stats.cpu_stats.cpu_usage.total_usage = 6_000_000;
    stats.cpu_stats.system_cpu_usage = 300_000_000;
    stats.cpu_stats.online_cpus = 4;
    stats.memory_stats.usage = 1_179_648 + 4_096;
    stats.memory_stats.limit = 64 << 20;
    stats
        .memory_stats
        .stats
        .insert("inactive_file".to_owned(), 4_096);
    let interface = |rx, tx| {
        let mut net = NetworkStats::default();
        net.rx_bytes = rx;
        net.tx_bytes = tx;
        net
    };
    stats.networks = Some(BTreeMap::from([
        ("eth0".to_owned(), interface(600, 80)),
        ("eth1".to_owned(), interface(48, 4)),
    ]));
    let entry = |op: &str, value| {
        let mut entry = BlkioEntry::default();
        entry.op = op.to_owned();
        entry.value = value;
        entry
    };
    stats.blkio_stats.io_service_bytes_recursive = Some(vec![
        entry("read", 2_000_000),
        entry("read", 290_000),
        entry("write", 0),
    ]);
    stats.pids_stats.current = 3;
    let row = serde_json::to_value(StatsRow::new(&stats, "web", false)).unwrap();
    assert_eq!(
        row,
        serde_json::json!({
            "BlockIO": "2.29MB / 0B",
            "CPUPerc": "10.00%",
            "Container": "web",
            "ID": "0123456789ab",
            "MemPerc": "1.76%",
            "MemUsage": "1.125MiB / 64MiB",
            "Name": "web",
            "NetIO": "648B / 84B",
            "PIDs": "3",
        })
    );
    // A counter gone back (a restart's new cgroup): no share, no panic.
    stats.precpu_stats = stats.cpu_stats.clone();
    stats.cpu_stats.cpu_usage.total_usage = 0;
    let row = StatsRow::new(&stats, "web", true);
    assert_eq!(row.cpu_perc, "0.00%");
    assert_eq!(row.id, "0123456789abcdef0123");
}

/// Memory in use less the cache nothing uses, but never below nothing; no
/// limit's share is 0.
#[test]
fn memory_is_docker_s() {
    let mut stats = ContainerStats::default();
    stats.memory_stats.usage = 100;
    stats
        .memory_stats
        .stats
        .insert("inactive_file".to_owned(), 150);
    assert_eq!(memory_used(&stats), 100);
    stats
        .memory_stats
        .stats
        .insert("inactive_file".to_owned(), 30);
    assert_eq!(memory_used(&stats), 70);
    assert!(percent(70, 0).abs() < f64::EPSILON);
}

/// Docker's CPU share needs both the container's and the host's CPU time to
/// have grown; it's the container's share times the host's CPUs.
#[test]
fn the_cpu_share_is_docker_s() {
    let sample = |cpu: (u64, u64), system: (u64, u64), cpus: u32| {
        let mut stats = ContainerStats::default();
        stats.precpu_stats.cpu_usage.total_usage = cpu.0;
        stats.cpu_stats.cpu_usage.total_usage = cpu.1;
        stats.precpu_stats.system_cpu_usage = system.0;
        stats.cpu_stats.system_cpu_usage = system.1;
        stats.cpu_stats.online_cpus = cpus;
        cpu_percent(&stats)
    };
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(close(sample((0, 50), (0, 100), 2), 100.0));
    assert!(
        close(sample((0, 50), (100, 100), 2), 0.0),
        "the host's didn't grow"
    );
    assert!(
        close(sample((50, 50), (0, 100), 2), 0.0),
        "the container's didn't"
    );
    assert!(close(sample((0, 50), (0, 100), 0), 0.0), "no CPUs known");
}
