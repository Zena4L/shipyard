//! Docker's rows (docker/cli's `formatter` contexts): what `--format` reads
//! of each object a list prints, by Docker's field names, every value the
//! text Docker writes (`"3"`, `"true"`, `"N/A"`), so a template and `--format
//! json` give what Docker's give. Built from the engine's Docker shapes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use shipyard_engine::docker::{
    ContainerStats, ContainerSummary, ImageSummary, NetworkResource, Volume,
};

use super::template::Row;

/// How long a short ID is.
const SHORT_ID: usize = 12;

/// A container, Docker's `ContainerContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerRow {
    #[serde(rename = "ID")]
    id: String,
    names: String,
    image: String,
    command: String,
    created_at: String,
    running_for: String,
    ports: String,
    state: String,
    status: String,
    size: String,
    labels: String,
    mounts: String,
    local_volumes: String,
    networks: String,
    platform: Option<()>,
    health_status: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl ContainerRow {
    /// `summary` as Docker's `ps` shows it at `now`, cut short unless
    /// `full` (`--no-trunc`).
    pub(crate) fn new(summary: &ContainerSummary, now: SystemTime, full: bool) -> Self {
        let command = if full {
            summary.command.clone()
        } else {
            ellipsis(&summary.command, 20)
        };
        Self {
            id: short(&summary.id, full),
            names: summary
                .names
                .iter()
                .map(|name| name.trim_start_matches('/'))
                .collect::<Vec<_>>()
                .join(","),
            image: summary.image.clone(),
            command: go_quote(&command),
            created_at: go_time(unix(summary.created), 0),
            running_for: format!(
                "{} ago",
                shipyard_engine::docker::ago(unix_time(summary.created), now)
            ),
            ports: summary
                .ports
                .iter()
                .map(|port| {
                    port_text(
                        port.ip.as_deref().zip(port.public_port),
                        port.private_port,
                        &port.kind,
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
            state: summary.state.to_owned(),
            status: summary.status.clone(),
            // Not measured (Docker's without `--size`).
            size: "0B".to_owned(),
            labels: joined_labels(&summary.labels),
            mounts: summary
                .mounts
                .iter()
                .map(|mount| {
                    let name = if mount.name.is_empty() {
                        &mount.source
                    } else {
                        &mount.name
                    };
                    if full {
                        name.clone()
                    } else {
                        ellipsis(name, 15)
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            local_volumes: summary
                .mounts
                .iter()
                .filter(|mount| mount.kind == "volume")
                .count()
                .to_string(),
            networks: summary
                .network_settings
                .networks
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
            platform: None,
            health_status: summary.health.to_owned(),
            label_map: summary.labels.clone(),
        }
    }
}

impl Row for ContainerRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "CONTAINER ID"),
            ("Names", "NAMES"),
            ("Image", "IMAGE"),
            ("Command", "COMMAND"),
            ("CreatedAt", "CREATED AT"),
            ("RunningFor", "CREATED"),
            ("Ports", "PORTS"),
            ("State", "STATE"),
            ("Status", "STATUS"),
            ("Size", "SIZE"),
            ("Labels", "LABELS"),
            ("Mounts", "MOUNTS"),
            ("LocalVolumes", "LOCAL VOLUMES"),
            ("Networks", "NETWORKS"),
            ("Platform", "PLATFORM"),
            ("HealthStatus", "HEALTH STATUS"),
        ]
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// `ps`'s default table (Docker's).
pub(crate) const CONTAINERS_TABLE: &str = "table {{.ID}}\t{{.Image}}\t{{.Command}}\t\
     {{.RunningFor}}\t{{.Status}}\t{{.Ports}}\t{{.Names}}";

/// A port as `ps` shows it: `0.0.0.0:8080->80/tcp`, `[::]:8080->80/tcp`
/// where it's published (`host`: the address and port), `80/tcp` where
/// it isn't.
fn port_text(host: Option<(&str, u16)>, private: u16, kind: &str) -> String {
    match host {
        Some((ip, public)) if ip.contains(':') => format!("[{ip}]:{public}->{private}/{kind}"),
        Some((ip, public)) => format!("{ip}:{public}->{private}/{kind}"),
        None => format!("{private}/{kind}"),
    }
}

/// An image under one of its names, Docker's `ImageContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ImageRow {
    #[serde(rename = "ID")]
    id: String,
    repository: String,
    tag: String,
    digest: String,
    created_since: String,
    created_at: String,
    size: String,
    shared_size: String,
    unique_size: String,
    containers: String,
}

impl ImageRow {
    /// `image`'s rows, one per name (as Docker's: an image tagged twice is
    /// two rows; one known by its digest only, `<none>` for its tag), at
    /// `now`, its ID cut short unless `full`.
    pub(crate) fn rows(image: &ImageSummary, now: SystemTime, full: bool) -> Vec<Self> {
        Self::rows_of(
            &ImageParts {
                id: &image.id,
                repo_tags: &image.repo_tags,
                repo_digests: &image.repo_digests,
                created: image.created,
                size: image.size,
                shared_size: image.shared_size,
                containers: image.containers,
            },
            now,
            full,
        )
    }

    /// [`ImageRow::rows`] of an image's parts.
    fn rows_of(image: &ImageParts<'_>, now: SystemTime, full: bool) -> Vec<Self> {
        let digest_of = |repository: &str| {
            image
                .repo_digests
                .iter()
                .find_map(|named| {
                    let (name, digest) = named.split_once('@')?;
                    (name == repository).then(|| digest.to_owned())
                })
                .unwrap_or_else(|| "<none>".to_owned())
        };
        let mut names: Vec<(String, String)> = image
            .repo_tags
            .iter()
            .map(|tagged| {
                let (repository, tag) = tagged
                    .rsplit_once(':')
                    .filter(|(_, tag)| !tag.contains('/'))
                    .unwrap_or((tagged.as_str(), "<none>"));
                (repository.to_owned(), tag.to_owned())
            })
            .collect();
        if names.is_empty() {
            names = image
                .repo_digests
                .iter()
                .filter_map(|named| named.split_once('@'))
                .map(|(repository, _)| (repository.to_owned(), "<none>".to_owned()))
                .collect();
        }
        if names.is_empty() {
            names.push(("<none>".to_owned(), "<none>".to_owned()));
        }
        let id = if full {
            image.id.to_owned()
        } else {
            short(image.id.trim_start_matches("sha256:"), false)
        };
        let created_since = if image.created > 0 {
            format!(
                "{} ago",
                shipyard_engine::docker::ago(unix_time(image.created), now)
            )
        } else {
            "N/A".to_owned()
        };
        let not_known =
            |count: i64, shown: String| if count < 0 { "N/A".to_owned() } else { shown };
        names
            .into_iter()
            .map(|(repository, tag)| Self {
                id: id.clone(),
                digest: digest_of(&repository),
                repository,
                tag,
                created_since: created_since.clone(),
                created_at: go_time(unix(image.created), 0),
                size: human_size(image.size, 3),
                shared_size: not_known(
                    image.shared_size,
                    human_size(image.shared_size.unsigned_abs(), 3),
                ),
                unique_size: not_known(
                    image.shared_size,
                    human_size(
                        image.size.saturating_sub(image.shared_size.unsigned_abs()),
                        3,
                    ),
                ),
                containers: not_known(image.containers, image.containers.to_string()),
            })
            .collect()
    }
}

impl Row for ImageRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "IMAGE ID"),
            ("Repository", "REPOSITORY"),
            ("Tag", "TAG"),
            ("Digest", "DIGEST"),
            ("CreatedSince", "CREATED"),
            ("CreatedAt", "CREATED AT"),
            ("Size", "SIZE"),
            ("SharedSize", "SHARED SIZE"),
            ("UniqueSize", "UNIQUE SIZE"),
            ("Containers", "CONTAINERS"),
        ]
    }
}

/// What an image's rows are made of ([`ImageSummary`]'s).
struct ImageParts<'a> {
    id: &'a str,
    repo_tags: &'a [String],
    repo_digests: &'a [String],
    created: i64,
    size: u64,
    shared_size: i64,
    containers: i64,
}

/// `images`' default table (Docker's classic one).
pub(crate) const IMAGES_TABLE: &str = "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t\
     {{if .CreatedSince}}{{.CreatedSince}}{{else}}N/A{{end}}\t{{.Size}}";

/// `images --digests`' default table (Docker's).
pub(crate) const IMAGES_DIGESTS_TABLE: &str = "table {{.Repository}}\t{{.Tag}}\t{{.Digest}}\t\
     {{.ID}}\t{{if .CreatedSince}}{{.CreatedSince}}{{else}}N/A{{end}}\t{{.Size}}";

/// A volume, Docker's `volumeContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct VolumeRow {
    name: String,
    driver: String,
    scope: String,
    mountpoint: String,
    labels: String,
    links: String,
    size: String,
    group: String,
    availability: String,
    status: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl From<&Volume> for VolumeRow {
    fn from(volume: &Volume) -> Self {
        let labels = volume.labels.clone().unwrap_or_default();
        let not_known = || "N/A".to_owned();
        Self {
            name: volume.name.clone(),
            driver: volume.driver.clone(),
            scope: volume.scope.clone(),
            mountpoint: volume.mountpoint.clone(),
            labels: joined_labels(&labels),
            links: not_known(),
            size: not_known(),
            group: not_known(),
            availability: not_known(),
            status: not_known(),
            label_map: labels,
        }
    }
}

impl Row for VolumeRow {
    fn id(&self) -> &str {
        &self.name
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("Name", "VOLUME NAME"),
            ("Driver", "DRIVER"),
            ("Scope", "SCOPE"),
            ("Mountpoint", "MOUNTPOINT"),
            ("Labels", "LABELS"),
            ("Links", "LINKS"),
            ("Size", "SIZE"),
            ("Group", "GROUP"),
            ("Availability", "AVAILABILITY"),
            ("Status", "STATUS"),
        ]
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// `volume ls`' default table (Docker's).
pub(crate) const VOLUMES_TABLE: &str = "table {{.Driver}}\t{{.Name}}";

/// A network, Docker's `networkContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct NetworkRow {
    #[serde(rename = "ID")]
    id: String,
    name: String,
    driver: String,
    scope: String,
    #[serde(rename = "IPv4")]
    ipv4: String,
    #[serde(rename = "IPv6")]
    ipv6: String,
    internal: String,
    labels: String,
    created_at: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl NetworkRow {
    /// `network` as Docker's `network ls` shows it, its ID cut short unless
    /// `full`.
    pub(crate) fn new(network: &NetworkResource, full: bool) -> Self {
        let created_at = shipyard_runtime::logs::time::Timestamp::parse(&network.created)
            .map(|stamp| go_time(stamp.secs(), stamp.nanos()))
            .unwrap_or_default();
        Self {
            id: short(&network.id, full),
            name: network.name.clone(),
            driver: network.driver.clone(),
            scope: network.scope.clone(),
            ipv4: network.enable_ipv4.to_string(),
            ipv6: network.enable_ipv6.to_string(),
            internal: network.internal.to_string(),
            labels: joined_labels(&network.labels),
            created_at,
            label_map: network.labels.clone(),
        }
    }
}

impl Row for NetworkRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "NETWORK ID"),
            ("Name", "NAME"),
            ("Driver", "DRIVER"),
            ("Scope", "SCOPE"),
            ("IPv4", "IPV4"),
            ("IPv6", "IPV6"),
            ("Internal", "INTERNAL"),
            ("Labels", "LABELS"),
            ("CreatedAt", "CREATED AT"),
        ]
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// `network ls`' default table (Docker's).
pub(crate) const NETWORKS_TABLE: &str = "table {{.ID}}\t{{.Name}}\t{{.Driver}}\t{{.Scope}}";

/// A kind of thing `system df` counts, Docker's `diskUsage…Context`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DfRow {
    #[serde(rename = "Type")]
    kind: &'static str,
    total_count: String,
    active: String,
    size: String,
    reclaimable: String,
}

impl DfRow {
    /// `usage` of `kind` (`Images`, `Containers`, `Local Volumes`), its
    /// sizes Docker's (`20.35GB`), the reclaimable part with its share of
    /// the whole (`15.94GB (78%)`). What couldn't all be read is at least
    /// what's said: `≥`.
    pub(crate) fn new(kind: &'static str, usage: &shipyard_engine::image::Usage) -> Self {
        let at_least = if usage.complete { "" } else { "≥ " };
        let reclaimable = match usage
            .reclaimable
            .saturating_mul(100)
            .checked_div(usage.bytes)
        {
            Some(share) => format!("{at_least}{} ({share}%)", human_size(usage.reclaimable, 4)),
            None => format!("{at_least}{}", human_size(usage.reclaimable, 4)),
        };
        Self {
            kind,
            total_count: usage.total.to_string(),
            active: usage.active.to_string(),
            size: format!("{at_least}{}", human_size(usage.bytes, 4)),
            reclaimable,
        }
    }
}

impl Row for DfRow {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("Type", "TYPE"),
            ("TotalCount", "TOTAL"),
            ("Active", "ACTIVE"),
            ("Size", "SIZE"),
            ("Reclaimable", "RECLAIMABLE"),
        ]
    }

    fn id(&self) -> &str {
        self.kind
    }
}

/// `system df`'s default table (Docker's).
pub(crate) const DF_TABLE: &str =
    "table {{.Type}}\t{{.TotalCount}}\t{{.Active}}\t{{.Size}}\t{{.Reclaimable}}";

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

/// An ID, its first 12 characters unless `full`.
fn short(id: &str, full: bool) -> String {
    if full {
        id.to_owned()
    } else {
        id.chars().take(SHORT_ID).collect()
    }
}

/// Labels as Docker lists them: `KEY=VALUE`, sorted, joined by commas.
fn joined_labels(labels: &BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `text` cut to `width` characters, its last one an ellipsis, if it's
/// longer (Docker's `Ellipsis`).
fn ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `text` as Go's `strconv.Quote` writes it: in double quotes, with `"`,
/// `\` and what isn't printable escaped.
fn go_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            c if u32::from(c) < 0x20 || c == '\u{7f}' => {
                let _ = write!(quoted, "\\x{:02x}", u32::from(c));
            }
            c if crate::output::acts_on_terminal(c) => {
                let _ = write!(quoted, "\\u{:04x}", u32::from(c));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Seconds since the epoch, as a count (none before it).
fn unix(seconds: i64) -> i64 {
    seconds.max(0)
}

/// Seconds since the epoch as a time.
fn unix_time(seconds: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
}

/// A time as Go's `Time.String` writes it, in UTC: `2026-10-20 13:54:44
/// +0000 UTC`, a fraction only if there's one.
fn go_time(seconds: i64, nanos: u32) -> String {
    let Some(stamp) = shipyard_runtime::logs::time::Timestamp::from_unix(seconds, nanos) else {
        return String::new();
    };
    // `YYYY-MM-DDTHH:MM:SS.NNNNNNNNNZ`
    let text = stamp.to_string();
    let (date, rest) = text.split_once('T').unwrap_or((&text, ""));
    let rest = rest.trim_end_matches('Z');
    let (time, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{date} {time} +0000 UTC")
    } else {
        format!("{date} {time}.{fraction} +0000 UTC")
    }
}

/// A byte count as Docker's `HumanSizeWithPrecision` writes it: decimal
/// units (`kB`, `MB`…), `precision` significant digits (Go's `%g`).
pub(crate) fn human_size(bytes: u64, precision: usize) -> String {
    go_units(
        bytes,
        1000.0,
        &["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"],
        precision,
    )
}

/// A byte count as Docker's `BytesSize` writes it: binary units (`KiB`,
/// `GiB`), 4 significant digits (`11.73GiB`).
pub(crate) fn binary_size(bytes: u64) -> String {
    go_units(
        bytes,
        1024.0,
        &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"],
        4,
    )
}

/// `bytes` in the largest of `units` (each `base` times the last) under
/// which it's at least 1, to `precision` significant digits: go-units'
/// `CustomSize`.
fn go_units(bytes: u64, base: f64, units: &[&str], precision: usize) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display only, to a few significant digits"
    )]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= base && unit + 1 < units.len() {
        size /= base;
        unit += 1;
    }
    format!(
        "{}{}",
        go_g(size, precision),
        units.get(unit).unwrap_or(&"")
    )
}

/// `value` as Go's `%.{precision}g` writes it: that many significant
/// digits, no trailing zeros, an exponent only when it's large or small.
fn go_g(value: f64, precision: usize) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let precision = precision.max(1);
    // Rounded to its significant digits first: that decides the exponent.
    let scientific = format!("{value:.*e}", precision - 1);
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trimmed = |text: &str| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text.to_owned()
        }
    };
    if exponent < -4 || exponent >= i32::try_from(precision).unwrap_or(i32::MAX) {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trimmed(mantissa), exponent.unsigned_abs());
    }
    let decimals =
        usize::try_from(i32::try_from(precision).unwrap_or(0) - 1 - exponent).unwrap_or(0);
    trimmed(&format!("{value:.decimals$}"))
}

#[cfg(test)]
mod tests;
