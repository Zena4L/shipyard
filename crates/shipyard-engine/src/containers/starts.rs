//! What a container's starts leave: their log directories, health,
//! network-lost markers, and the previous start kept aside.

use std::fs;
use std::path::PathBuf;

use super::store::logs_dir;
#[cfg(target_os = "linux")]
use super::store::record_dir;
#[cfg(target_os = "linux")]
use super::{ContainerId, Exit};
use super::{ContainerState, Record};
use crate::Engine;
#[cfg(target_os = "linux")]
use crate::error::{ErrorKind, Result};

/// What a container's last start left (its start, end, code and boot),
/// kept beside its record while a new start is in flight (stage 9, step
/// 9.9c, R2): a file of its own, never a record field (an older binary
/// refuses a record with a field it doesn't know).
#[cfg(target_os = "linux")]
const PREVIOUS_START: &str = "previous-start.json";

/// What container `record`'s health checks say of its current start, if
/// it has checks and is running or paused (`state`), and its companion has said anything
/// yet (its start's [`health::FILE`](crate::health::FILE)).
#[must_use]
pub fn health(
    engine: &Engine,
    record: &Record,
    state: &ContainerState,
) -> Option<crate::health::Health> {
    // Running or paused only: a container restarting (or stopping) has
    // no checks under way, and the last start's would be out of date.
    let up = matches!(
        state,
        ContainerState::Running { .. } | ContainerState::Paused
    );
    if record.config.health.is_none() || !up {
        return None;
    }
    let logs = logs_dir(engine, &record.id).ok()?;
    crate::health::Health::read(&logs.join(last_logs_name(&logs)?))
}

/// Whether container `record`'s current start lost its network (its pasta
/// died while it ran: `network::lost`), if it's
/// running or paused.
#[must_use]
pub fn network_lost(
    engine: &Engine,
    record: &Record,
    state: &ContainerState,
) -> Option<crate::network::lost::NetworkLost> {
    if !matches!(
        state,
        ContainerState::Running { .. } | ContainerState::Paused
    ) {
        return None;
    }
    last_start_lost(engine, record)
}

/// Whether container `record`'s last start lost its network, whatever its
/// state (a foreground run asks once its container has ended).
#[must_use]
pub fn last_start_lost(
    engine: &Engine,
    record: &Record,
) -> Option<crate::network::lost::NetworkLost> {
    let logs = logs_dir(engine, &record.id).ok()?;
    crate::network::lost::NetworkLost::read(&logs.join(last_logs_name(&logs)?))
}

/// The name of the last start's log directory in `logs` (see
/// [`logs_dir`]): the highest there, if any.
pub(crate) fn last_logs_name(logs: &std::path::Path) -> Option<String> {
    let (last, _) = starts(logs).ok()?.pop()?;
    Some(format!("{last:010}"))
}

/// The name of the next start's log directory in `logs` (see
/// [`logs_dir`]): one past the highest there.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "detached containers are Linux's")
)]
pub(crate) fn next_logs_name(logs: &std::path::Path) -> String {
    // Past every number there, whatever it names (a file in the way too).
    let last = fs::read_dir(logs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{:010}", last.saturating_add(1))
}

/// The start directories in `logs` (see [`logs_dir`]), oldest first, with
/// their numbers: directories (never through a link) named by digits alone;
/// anything else there is left out. None if there's no `logs` yet.
///
/// # Errors
///
/// `logs` can't be listed.
pub(crate) fn starts(logs: &std::path::Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let entries = match fs::read_dir(logs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut numbered = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let (Ok(number), true) = (name.parse::<u64>(), entry.file_type()?.is_dir()) else {
            continue;
        };
        numbered.push((number, entry.path()));
    }
    numbered.sort();
    Ok(numbered)
}

/// What a container's record says of its last start: kept while a new
/// start, which overwrites it, is in flight ([`keep_previous_start`]), and
/// put back if that start doesn't happen, even if what made it was killed.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviousStart {
    /// The file's version: 2 (1 kept only `started`, `finished`, `exit` and
    /// `boot_id`; read still, F55).
    version: u32,
    /// When it started (none: never).
    started: Option<std::time::SystemTime>,
    /// When it ended.
    finished: Option<std::time::SystemTime>,
    /// How it ended.
    exit: Option<Exit>,
    /// The boot it started in.
    boot_id: Option<String>,
    /// The rest a start rewrites before the runtime starts it (none in a v1
    /// file: those fields are left as the record has them).
    #[serde(default)]
    rewritten: Option<Rewritten>,
}

/// What else a start rewrites in the record before the runtime starts it
/// (`run::start`'s `connect_and_start`), kept so a start that doesn't
/// happen leaves none of it (F55): a user's stop, the restart count and
/// backoff, the next start, the ports.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Rewritten {
    /// Whether its user had stopped it.
    stopped_by_user: bool,
    /// Its restarts by its policy so far.
    restart_count: u32,
    /// The policy's backoff before its next restart.
    restart_delay_ms: u64,
    /// When the policy's waiter was to start it next.
    next_start: Option<std::time::SystemTime>,
    /// The host ports its last start published.
    ports: Vec<crate::containers::BoundPort>,
}

#[cfg(target_os = "linux")]
impl PreviousStart {
    /// The version written.
    const VERSION: u32 = 2;

    /// The versions read: v1's four fields, and v2's.
    const READ: [u32; 2] = [1, 2];

    /// The most of the file read: the record's own bound, as the ports it
    /// keeps can be as many as the record's.
    const MAX_FILE: u64 = crate::containers::record::MAX_SIZE;

    /// What `record` says of its last start.
    pub(crate) fn of(record: &Record) -> Self {
        Self {
            version: Self::VERSION,
            started: record.started,
            finished: record.finished,
            exit: record.exit.clone(),
            boot_id: record.boot_id.clone(),
            rewritten: Some(Rewritten {
                stopped_by_user: record.stopped_by_user,
                restart_count: record.restart_count,
                restart_delay_ms: record.restart_delay_ms,
                next_start: record.next_start,
                ports: record.ports.clone(),
            }),
        }
    }

    /// `record` saying it again.
    pub(crate) fn restore(self, record: &mut Record) {
        record.started = self.started;
        record.finished = self.finished;
        record.exit = self.exit;
        record.boot_id = self.boot_id;
        if let Some(rewritten) = self.rewritten {
            // A start clears the user's stop: one set now came since (a
            // `stop` of the dead start), and wins, with no next start.
            if record.stopped_by_user {
                record.next_start = None;
            } else {
                record.stopped_by_user = rewritten.stopped_by_user;
                record.next_start = rewritten.next_start;
            }
            record.restart_count = rewritten.restart_count;
            record.restart_delay_ms = rewritten.restart_delay_ms;
            record.ports = rewritten.ports;
        }
    }
}

/// What a start that never ran leaves in `record` (F64): what its last
/// start left, put back from `previous` (kept aside before it: an earlier
/// end, whole), or, with none kept, never started.
#[cfg(target_os = "linux")]
pub(crate) fn settle_unstarted(record: &mut Record, previous: Option<PreviousStart>) {
    match previous {
        Some(previous) => previous.restore(record),
        None => record.mark_never_started(),
    }
}

/// Keeps `previous` (what container `id`'s last start left) beside its
/// record, before a new start overwrites it there: written whole.
///
/// # Errors
///
/// [`ErrorKind::Storage`]: it couldn't be written.
#[cfg(target_os = "linux")]
pub(crate) fn keep_previous_start(
    engine: &Engine,
    id: &ContainerId,
    previous: &PreviousStart,
) -> Result<()> {
    let json = serde_json::to_vec(previous)?;
    crate::fs::write_atomically(&record_dir(engine, id)?, PREVIOUS_START, &json, 0o600)
        .map_err(|err| err.io_as(ErrorKind::Storage))
}

/// What [`keep_previous_start`] kept for container `id`, if it's there and
/// readable (v1 or v2; one of another version is passed over).
#[cfg(target_os = "linux")]
pub(crate) fn previous_start(engine: &Engine, id: &ContainerId) -> Option<PreviousStart> {
    let path = record_dir(engine, id).ok()?.join(PREVIOUS_START);
    let bytes = shipyard_sys::fs::read_bounded(&path, PreviousStart::MAX_FILE).ok()?;
    serde_json::from_slice::<PreviousStart>(&bytes)
        .ok()
        .filter(|previous| PreviousStart::READ.contains(&previous.version))
}

/// Removes what [`keep_previous_start`] kept for container `id`: its start
/// happened, or what it overwrote was put back. Gone already is removed.
#[cfg(target_os = "linux")]
pub(crate) fn forget_previous_start(engine: &Engine, id: &ContainerId) {
    if let Ok(dir) = record_dir(engine, id) {
        let _ = std::fs::remove_file(dir.join(PREVIOUS_START));
    }
}
