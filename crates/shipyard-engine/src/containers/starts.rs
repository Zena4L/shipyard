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

/// What a start kept aside of its last one ([`previous_start`]).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) enum Kept {
    /// Nothing: none was kept, or what's there can't be read.
    None,
    /// What the record said before the start.
    Start(PreviousStart),
    /// A newer Shipyard's file, of this version: never read as what it
    /// says (H2), but kept for that Shipyard.
    Newer(u32),
}

#[cfg(target_os = "linux")]
impl Kept {
    /// A newer Shipyard's file's version, if it's one.
    pub(crate) fn newer(&self) -> Option<u32> {
        match self {
            Self::Newer(version) => Some(*version),
            Self::None | Self::Start(_) => None,
        }
    }

    /// What it kept, if it's one this Shipyard reads.
    #[cfg(test)]
    pub(crate) fn start(self) -> Option<PreviousStart> {
        match self {
            Self::Start(previous) => Some(previous),
            Self::None | Self::Newer(_) => None,
        }
    }
}

/// What a start that never ran leaves in `record` (F64): what its last
/// start left, put back from `kept` (kept aside before it: an earlier end,
/// whole), or, with none kept, never started. A newer Shipyard's file is
/// never read as what it says (H2): never started, then, which is true of
/// this start (its last run's end, in that file, isn't put back).
#[cfg(target_os = "linux")]
pub(crate) fn settle_unstarted(record: &mut Record, kept: Kept) {
    match kept {
        Kept::Start(previous) => previous.restore(record),
        Kept::None | Kept::Newer(_) => record.mark_never_started(),
    }
}

/// Container `id`'s record settled for a start that never ran (F64): by
/// what its start kept aside ([`settle_unstarted`]), unless the record has
/// an end already (put back by the start's own undoing: left as it is,
/// F110); then what was kept let go of ([`done_with_previous_start`]).
/// One meaning for `take`'s reconciling and the recovery pass alike
/// (`__cleanup`'s, deciding inside its own rewrite, matches it).
///
/// # Errors
///
/// The record can't be read or written.
#[cfg(target_os = "linux")]
pub(crate) fn settle_never_ran(engine: &Engine, id: &ContainerId) -> Result<Record> {
    let kept = previous_start(engine, id);
    let newer = kept.newer();
    let record = super::update(engine, id, |record| {
        if record.finished.is_none() {
            settle_unstarted(record, kept);
        }
    })?;
    done_with_previous_start(engine, id, newer);
    Ok(record)
}

/// Lets go of what container `id`'s start kept aside, once it's been used
/// or is no start's to restore (`newer`: [`Kept::Newer`]'s version, if it
/// was a newer Shipyard's: then kept, for it, and said).
#[cfg(target_os = "linux")]
pub(crate) fn done_with_previous_start(engine: &Engine, id: &ContainerId, newer: Option<u32>) {
    match newer {
        None => forget_previous_start(engine, id),
        Some(version) => engine.warn(
            crate::report::WarningKind::LeftForLater,
            format!(
                "container {}'s previous-start.json is version {version}, newer than this \
                 Shipyard's ({}): not read (what it kept isn't put back), and left for the \
                 Shipyard that wrote it",
                id.short(),
                PreviousStart::VERSION
            ),
        ),
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
        .map(|placed| placed.said(engine))
}

/// What [`keep_previous_start`] kept for container `id`: v1 or v2 read; a
/// newer version known as one, never guessed at (H2); none, or one that
/// can't be read, is [`Kept::None`].
#[cfg(target_os = "linux")]
pub(crate) fn previous_start(engine: &Engine, id: &ContainerId) -> Kept {
    #[derive(serde::Deserialize)]
    struct Versioned {
        version: u32,
    }
    let Some(bytes) = record_dir(engine, id).ok().and_then(|dir| {
        shipyard_sys::fs::read_bounded(&dir.join(PREVIOUS_START), PreviousStart::MAX_FILE).ok()
    }) else {
        return Kept::None;
    };
    match serde_json::from_slice::<Versioned>(&bytes) {
        Ok(Versioned { version }) if version > PreviousStart::VERSION => Kept::Newer(version),
        _ => serde_json::from_slice::<PreviousStart>(&bytes)
            .ok()
            .filter(|previous| PreviousStart::READ.contains(&previous.version))
            .map_or(Kept::None, Kept::Start),
    }
}

/// Removes what [`keep_previous_start`] kept for container `id`: its start
/// happened, or what it overwrote was put back. Gone already is removed.
#[cfg(target_os = "linux")]
pub(crate) fn forget_previous_start(engine: &Engine, id: &ContainerId) {
    if let Ok(dir) = record_dir(engine, id) {
        let _ = std::fs::remove_file(dir.join(PREVIOUS_START));
    }
}
