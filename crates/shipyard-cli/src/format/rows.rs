//! Docker's rows (docker/cli's `formatter` contexts): what `--format` reads
//! of each object a list prints, by Docker's field names, every value the
//! text Docker writes (`"3"`, `"true"`, `"N/A"`), so a template and `--format
//! json` give what Docker's give. Built from the engine's Docker shapes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

mod containers;
mod images;
mod resources;
mod stats;
mod units;

pub(crate) use containers::{CONTAINERS_TABLE, ContainerRow};
pub(crate) use images::{HISTORY_TABLE, HistoryRow, IMAGES_DIGESTS_TABLE, IMAGES_TABLE, ImageRow};
pub(crate) use resources::{DF_TABLE, DfRow, NETWORKS_TABLE, NetworkRow, VOLUMES_TABLE, VolumeRow};
pub(crate) use stats::{STATS_TABLE, StatsRow};
pub(crate) use units::{binary_size, human_size};

/// How long a short ID is.
const SHORT_ID: usize = 12;

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

#[cfg(test)]
mod tests;
