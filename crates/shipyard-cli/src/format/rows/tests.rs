use super::*;

/// Docker 29.8's sizes (2026-10-20's captures): `images` at 3 significant
/// digits, `system df` at 4.
#[test]
fn sizes_are_docker_s() {
    assert_eq!(human_size(138_200_000, 3), "138MB");
    assert_eq!(human_size(207_000_000, 3), "207MB");
    assert_eq!(human_size(4_096, 4), "4.096kB");
    assert_eq!(human_size(8_192, 4), "8.192kB");
    assert_eq!(human_size(20_350_000_000, 4), "20.35GB");
    assert_eq!(human_size(114_600_000_000, 4), "114.6GB");
    assert_eq!(human_size(4_100, 3), "4.1kB");
    assert_eq!(human_size(0, 3), "0B");
    assert_eq!(human_size(999, 3), "999B");
    assert_eq!(human_size(1_000, 3), "1kB");
    // Go's `%.3g` of 999.6 rounds to 4 digits: an exponent.
    assert_eq!(human_size(999_600, 3), "1e+03kB");
}

/// Go's `%g`: significant digits, no trailing zeros, an exponent beyond.
#[test]
fn go_g_is_go_s() {
    assert_eq!(go_g(1.0, 3), "1");
    assert_eq!(go_g(1.5, 3), "1.5");
    assert_eq!(go_g(12.345, 3), "12.3");
    assert_eq!(go_g(123.45, 3), "123");
    assert_eq!(go_g(0.000_012_3, 3), "1.23e-05");
    assert_eq!(go_g(0.001_23, 3), "0.00123");
}

/// Go's `Time.String` in UTC, the fraction only when there's one
/// (Docker's `CreatedAt`: `2026-10-20 13:54:44 +0000 UTC`).
#[test]
fn times_are_go_s() {
    assert_eq!(go_time(1_792_504_484, 0), "2026-10-20 13:54:44 +0000 UTC");
    assert_eq!(
        go_time(1_792_504_484, 32_406_375),
        "2026-10-20 13:54:44.032406375 +0000 UTC"
    );
    assert_eq!(
        go_time(1_792_504_484, 500_000_000),
        "2026-10-20 13:54:44.5 +0000 UTC"
    );
}

/// `ps`'s COMMAND: cut to 20 with an ellipsis (`"sh -c 'echo a long …"`),
/// then quoted as Go quotes it.
#[test]
fn commands_are_cut_and_quoted_as_docker_s() {
    let command = "sh -c 'echo a long command with words'";
    assert_eq!(go_quote(&ellipsis(command, 20)), "\"sh -c 'echo a long …\"");
    assert_eq!(go_quote("sleep 600"), "\"sleep 600\"");
    assert_eq!(
        go_quote("sh -c \"a\\b\"\n\u{1b}\u{202e}"),
        "\"sh -c \\\"a\\\\b\\\"\\n\\x1b\\u202e\""
    );
    assert_eq!(ellipsis("shipyard-fmt-vol", 15), "shipyard-fmt-v…");
    assert_eq!(ellipsis("short", 15), "short");
}

/// Ports as Docker's `ps` writes them (2026-10-20's capture: `0.0.0.0:18089->80/tcp,
/// [::]:18089->80/tcp`).
#[test]
fn ports_are_docker_s() {
    assert_eq!(
        port_text(Some(("0.0.0.0", 18089)), 80, "tcp"),
        "0.0.0.0:18089->80/tcp"
    );
    assert_eq!(
        port_text(Some(("::", 18089)), 80, "tcp"),
        "[::]:18089->80/tcp"
    );
    assert_eq!(port_text(None, 53, "udp"), "53/udp");
}

/// Every row's header names every field it has, so a table's header never
/// writes `<no value>`; and the fields are Docker 29.8's (its `--format
/// json`, 2026-10-20), no more, no fewer.
#[test]
fn every_field_is_docker_s_and_has_a_header() {
    fn names<R: Row>() -> Vec<&'static str> {
        let mut names: Vec<&str> = R::headers().iter().map(|&(name, _)| name).collect();
        names.sort_unstable();
        names
    }
    assert_eq!(
        names::<ContainerRow>(),
        [
            "Command",
            "CreatedAt",
            "HealthStatus",
            "ID",
            "Image",
            "Labels",
            "LocalVolumes",
            "Mounts",
            "Names",
            "Networks",
            "Platform",
            "Ports",
            "RunningFor",
            "Size",
            "State",
            "Status"
        ]
    );
    assert_eq!(
        names::<ImageRow>(),
        [
            "Containers",
            "CreatedAt",
            "CreatedSince",
            "Digest",
            "ID",
            "Repository",
            "SharedSize",
            "Size",
            "Tag",
            "UniqueSize"
        ]
    );
    assert_eq!(
        names::<VolumeRow>(),
        [
            "Availability",
            "Driver",
            "Group",
            "Labels",
            "Links",
            "Mountpoint",
            "Name",
            "Scope",
            "Size",
            "Status"
        ]
    );
    assert_eq!(
        names::<NetworkRow>(),
        [
            "CreatedAt",
            "Driver",
            "ID",
            "IPv4",
            "IPv6",
            "Internal",
            "Labels",
            "Name",
            "Scope"
        ]
    );
}

/// What each row writes (`--format json`) is its header's fields, no more,
/// no fewer: a field added to one and not the other fails here.
#[test]
fn each_row_writes_its_header_s_fields() {
    fn same<R: Row>(row: &R) {
        let written = serde_json::to_value(row).unwrap();
        let mut keys: Vec<&str> = written
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut names: Vec<&str> = R::headers().iter().map(|&(name, _)| name).collect();
        names.sort_unstable();
        assert_eq!(keys, names);
    }
    let text = String::new;
    same(&ContainerRow {
        id: text(),
        names: text(),
        image: text(),
        command: text(),
        created_at: text(),
        running_for: text(),
        ports: text(),
        state: text(),
        status: text(),
        size: text(),
        labels: text(),
        mounts: text(),
        local_volumes: text(),
        networks: text(),
        platform: None,
        health_status: text(),
        label_map: BTreeMap::new(),
    });
    same(&ImageRow {
        id: text(),
        repository: text(),
        tag: text(),
        digest: text(),
        created_since: text(),
        created_at: text(),
        size: text(),
        shared_size: text(),
        unique_size: text(),
        containers: text(),
    });
    same(&VolumeRow {
        name: text(),
        driver: text(),
        scope: text(),
        mountpoint: text(),
        labels: text(),
        links: text(),
        size: text(),
        group: text(),
        availability: text(),
        status: text(),
        label_map: BTreeMap::new(),
    });
    same(&NetworkRow {
        id: text(),
        name: text(),
        driver: text(),
        scope: text(),
        ipv4: text(),
        ipv6: text(),
        internal: text(),
        labels: text(),
        created_at: text(),
        label_map: BTreeMap::new(),
    });
}

/// An image of 3.62 MB, made at the epoch (`created` unknown), its shared
/// size and containers not counted.
fn parts<'a>(id: &'a str, repo_tags: &'a [String], repo_digests: &'a [String]) -> ImageParts<'a> {
    ImageParts {
        id,
        repo_tags,
        repo_digests,
        created: 0,
        size: 3_620_000,
        shared_size: -1,
        containers: -1,
    }
}

/// An image's rows as Docker's `images` makes them: one per name (tag),
/// its digest by repository; one known by its digest alone `<none>` for its
/// tag; one with no name `<none>` for both; sizes and counts not known
/// `N/A`; the ID cut short, or whole with `sha256:` (`--no-trunc`).
#[test]
fn an_image_is_a_row_per_name() {
    let id = format!("sha256:{}", "ab".repeat(32));
    let tags = ["alpine:3.20".to_owned(), "localhost:5000/app:v1".to_owned()];
    let digests = ["alpine@sha256:feed".to_owned()];
    let now = SystemTime::UNIX_EPOCH;
    let rows = ImageRow::rows_of(&parts(&id, &tags, &digests), now, false);
    let named: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|row| {
            (
                row.repository.as_str(),
                row.tag.as_str(),
                row.digest.as_str(),
            )
        })
        .collect();
    assert_eq!(
        named,
        [
            ("alpine", "3.20", "sha256:feed"),
            ("localhost:5000/app", "v1", "<none>")
        ]
    );
    let first = &rows[0];
    assert_eq!(first.id, "ab".repeat(6));
    assert_eq!(first.size, "3.62MB");
    assert_eq!(
        (
            first.shared_size.as_str(),
            first.unique_size.as_str(),
            first.containers.as_str(),
            first.created_since.as_str()
        ),
        ("N/A", "N/A", "N/A", "N/A")
    );
    let digest_only = ImageRow::rows_of(&parts(&id, &[], &digests), now, true);
    assert_eq!(
        (
            digest_only[0].repository.as_str(),
            digest_only[0].tag.as_str(),
            digest_only[0].id.as_str()
        ),
        ("alpine", "<none>", id.as_str())
    );
    let nameless = ImageRow::rows_of(&parts(&id, &[], &[]), now, false);
    assert_eq!(
        (nameless[0].repository.as_str(), nameless[0].tag.as_str()),
        ("<none>", "<none>")
    );
}

/// `system df`'s rows, Docker's: a size at 4 digits, the reclaimable part
/// with its share (`15.94GB (78%)`), none of nothing; `≥` where something
/// couldn't be read.
#[test]
fn disk_usage_rows_are_docker_s() {
    let usage = |bytes, reclaimable, complete| shipyard_engine::image::Usage {
        total: 27,
        active: 1,
        bytes,
        reclaimable,
        complete,
    };
    let row = DfRow::new("Images", &usage(20_350_000_000, 15_940_000_000, true));
    assert_eq!(
        (
            row.kind,
            row.total_count.as_str(),
            row.active.as_str(),
            row.size.as_str(),
            row.reclaimable.as_str()
        ),
        ("Images", "27", "1", "20.35GB", "15.94GB (78%)")
    );
    assert_eq!(
        DfRow::new("Local Volumes", &usage(0, 0, true)).reclaimable,
        "0B"
    );
    let partial = DfRow::new("Containers", &usage(4096, 0, false));
    assert_eq!(
        (partial.size.as_str(), partial.reclaimable.as_str()),
        ("≥ 4.096kB", "≥ 0B (0%)")
    );
}

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
