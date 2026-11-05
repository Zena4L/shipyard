//! What a start joined of other containers, and whether it still is
//! (F136(a)). A container that joins another's network or IPC namespace
//! holds that namespace, not the container: when the other starts again
//! (its restart policy, a crash and restart), it does so in a new one, and
//! the joiner stays, "Up", in the old one, empty. As its pasta's end is
//! (`network/lost.rs`), it's said: `ps` (` (no network)`, ` (no shared
//! IPC)`), Shipyard's inspect, and an event when the other starts again.
//!
//! Each start that joins writes [`FILE`] in its log directory: the run it
//! joined (the other's init, by PID and start time), versioned (H2). The
//! views compare it, without a lock, with the other's init now.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::store::logs_dir;
use super::{ContainerState, Record};
use crate::Engine;

/// The file that says what a start joined, in its log directory.
pub(crate) const FILE: &str = "joined";
/// The file's version: what's written.
const VERSION: u32 = 1;
/// The most of the file read: a few dozen bytes.
const MAX_FILE: u64 = 4 * 1024;

/// A run of another container a start joined: its ID, and its init's PID
/// and start time (together, that run alone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JoinedRun {
    /// The other container's ID.
    pub(crate) id: String,
    /// Its init's PID.
    pub(crate) pid: u32,
    /// Its init's start time.
    pub(crate) start_time: u64,
}

/// The runs a start joined the network and IPC namespace of. (A PID
/// namespace's joiner ends with it: nothing to say.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JoinedRuns {
    /// Its network's.
    pub(crate) network: Option<JoinedRun>,
    /// Its IPC namespace's.
    pub(crate) ipc: Option<JoinedRun>,
}

/// Version 1 of the file, as it's written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network: Option<JoinedRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ipc: Option<JoinedRun>,
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "starts are Linux's")
)]
impl JoinedRuns {
    /// Whether it joined nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.network.is_none() && self.ipc.is_none()
    }

    /// As the file keeps it.
    fn to_json(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&V1 {
            v: VERSION,
            network: self.network.clone(),
            ipc: self.ipc.clone(),
        })
    }

    /// What `bytes` say, if they're a file of a version this reads.
    fn from_json(bytes: &[u8]) -> Option<Self> {
        let read: V1 = serde_json::from_slice(bytes).ok()?;
        (read.v == VERSION).then_some(Self {
            network: read.network,
            ipc: read.ipc,
        })
    }

    /// Kept in the start's log directory `dir`, if it joined anything.
    ///
    /// # Errors
    ///
    /// It couldn't be written.
    pub(crate) fn write(&self, dir: &Path) -> crate::error::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        // Placed is enough: a marker a power cut takes is said nothing of.
        let json = self
            .to_json()
            .map_err(|error| crate::Error::from(std::io::Error::other(error)))?;
        let _ = crate::fs::write_atomically(dir, FILE, &json, 0o600)?.unsynced();
        Ok(())
    }

    /// The file in the start's log directory `dir`, if there's one this
    /// reads.
    fn read(dir: &Path) -> Option<Self> {
        Self::from_json(&shipyard_sys::fs::read_bounded(&dir.join(FILE), MAX_FILE).ok()?)
    }
}

/// What a running container has lost of what its start joined: the
/// container whose namespace it was, by name, for each.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct JoinsLost {
    /// The container whose network it joined, which has started again (or
    /// ended) since: this one keeps the old, empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The same, for its IPC namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipc: Option<String>,
}

impl JoinsLost {
    /// Whether it has lost nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.network.is_none() && self.ipc.is_none()
    }
}

/// What container `record`, in `state`, has lost of what its last start
/// joined: nothing unless it's running or paused, or its start joined
/// nothing.
#[must_use]
pub fn joins_lost(engine: &Engine, record: &Record, state: &ContainerState) -> JoinsLost {
    if !matches!(
        state,
        ContainerState::Running { .. } | ContainerState::Paused
    ) {
        return JoinsLost::default();
    }
    let Some(runs) = last_start_runs(engine, record) else {
        return JoinsLost::default();
    };
    let lost = |run: Option<JoinedRun>| run.filter(|run| !still_running(engine, run));
    JoinsLost {
        network: lost(runs.network).map(|run| name_of(engine, &run.id)),
        ipc: lost(runs.ipc).map(|run| name_of(engine, &run.id)),
    }
}

/// What container `record`'s last start joined, if it says.
fn last_start_runs(engine: &Engine, record: &Record) -> Option<JoinedRuns> {
    let logs = logs_dir(engine, &record.id).ok()?;
    JoinedRuns::read(&logs.join(super::last_logs_name(&logs)?))
}

/// Whether the run `run` names is its container's still, and alive (read
/// without a lock: a view never waits on a command). What can't be read
/// is taken as still: a view never says a loss it can't see.
fn still_running(engine: &Engine, run: &JoinedRun) -> bool {
    use shipyard_runtime::container::{is_process, peek_init};
    let Ok(root) = engine.paths.runtime_root() else {
        return true;
    };
    match peek_init(root, &run.id) {
        // Its state kept by a killed clean-up names a dead init: lost too.
        Ok(Some((pid, start_time))) => {
            u32::try_from(pid) == Ok(run.pid)
                && start_time == run.start_time
                && is_process(pid, start_time)
        }
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Container `id`'s name, or its short ID once it's gone.
#[cfg(target_os = "linux")]
fn name_of(engine: &Engine, id: &str) -> String {
    super::ContainerId::parse(id)
        .ok()
        .and_then(|id| super::read_one(engine, &id).ok())
        .map_or_else(
            || id.get(..12).unwrap_or(id).to_owned(),
            |record| record.name.as_str().to_owned(),
        )
}

/// Off Linux: no runtime root, so nothing is ever lost and this is never
/// reached (`still_running` says so first).
#[cfg(not(target_os = "linux"))]
fn name_of(_engine: &Engine, id: &str) -> String {
    id.to_owned()
}

/// At container `record`'s start: each container up whose last start
/// joined another run of its network or IPC namespace is told, by an event
/// (`network_lost`, `ipc_lost`), that it keeps the old one.
#[cfg(target_os = "linux")]
pub(crate) fn tell_joiners(engine: &Engine, record: &Record) {
    let Ok(all) = super::list(engine) else {
        return;
    };
    // Its joiners are named in their `created.json`: only theirs are read
    // further (a start's cost grows with them, not with the store).
    let joins_it = |other: &Record| {
        super::created(engine, &other.id).is_ok_and(|created| {
            created.network_from.as_ref() == Some(&record.id)
                || created.ipc_from.as_ref() == Some(&record.id)
        })
    };
    for joiner in all
        .iter()
        .filter(|other| other.id != record.id && joins_it(other))
    {
        let Some(runs) = last_start_runs(engine, joiner) else {
            continue;
        };
        if !super::state(engine, joiner).is_ok_and(|state| state.is_up()) {
            continue;
        }
        for (run, action, what) in [
            (runs.network, "network_lost", "network"),
            (runs.ipc, "ipc_lost", "IPC namespace"),
        ] {
            if run.is_some_and(|run| run.id == record.id.as_str() && !still_running(engine, &run)) {
                crate::events::emit(
                    engine,
                    &crate::events::container(joiner, action).with(
                        "how",
                        format!(
                            "container {} started again: this one keeps its old {what}",
                            record.name
                        ),
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// Version 1, as written, read back; a newer version, another shape
    /// and nothing at all are refused (nothing said, never a guess: H2).
    #[test]
    fn the_file_s_versions() {
        let runs = JoinedRuns {
            network: Some(JoinedRun {
                id: "a".repeat(64),
                pid: 42,
                start_time: 7,
            }),
            ipc: None,
        };
        let pinned = format!(
            r#"{{"v":1,"network":{{"id":"{}","pid":42,"start_time":7}}}}"#,
            "a".repeat(64)
        );
        assert_eq!(String::from_utf8(runs.to_json().unwrap()).unwrap(), pinned);
        assert_eq!(JoinedRuns::from_json(pinned.as_bytes()), Some(runs));
        assert_eq!(
            JoinedRuns::from_json(pinned.replace(r#""v":1"#, r#""v":2"#).as_bytes()),
            None
        );
        assert_eq!(JoinedRuns::from_json(b"[1,2]"), None);
        assert_eq!(JoinedRuns::from_json(b""), None);
        assert!(JoinedRuns::default().is_empty());
    }
}
