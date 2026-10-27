//! A container's network, lost (stage 9, step 9.6c1): its pasta died while
//! it ran, and it runs on with only its loopback, as Podman's does.
//!
//! pasta's launcher sees it (`shipyard_network::pasta`, following the
//! container's init) and calls this binary's hook, [`pasta_lost`], once,
//! before it ends: the hook writes [`FILE`] in the start's log directory
//! (atomically; versioned, H2) and says it in the events journal
//! (`network_lost`). `ps` and `inspect` read the file while that start runs.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file that says a start lost its network, in its log directory.
pub(crate) const FILE: &str = "network-lost";
/// The file's version: what's written.
pub(crate) const VERSION: u32 = 1;

/// The most of the file read: a few dozen bytes.
const MAX_FILE: u64 = 64 * 1024;

/// When, and how, a start lost its network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkLost {
    /// When (RFC 3339).
    pub time: String,
    /// How pasta ended: `pasta ended (code 1)`, `pasta was killed
    /// (SIGKILL)`.
    pub how: String,
}

/// Version 1 of the file, as it's written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    v: u32,
    time: String,
    how: String,
}

impl NetworkLost {
    /// As the file keeps it.
    #[must_use]
    pub(crate) fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&V1 {
            v: VERSION,
            time: self.time.clone(),
            how: self.how.clone(),
        })
        .unwrap_or_default()
    }

    /// What `bytes` say, if they're a file of a version this reads.
    #[must_use]
    pub(crate) fn from_json(bytes: &[u8]) -> Option<Self> {
        let read: V1 = serde_json::from_slice(bytes).ok()?;
        (read.v == VERSION).then_some(Self {
            time: read.time,
            how: read.how,
        })
    }

    /// The file in the start's log directory `dir`, if there's one this
    /// reads.
    #[must_use]
    pub(crate) fn read(dir: &Path) -> Option<Self> {
        Self::from_json(&shipyard_sys::fs::read_bounded(&dir.join(FILE), MAX_FILE).ok()?)
    }
}

/// The hook pasta's launcher calls (give it to
/// `shipyard_network::pasta::entry_point`): a detached container's pasta
/// died while its init ran. Writes `network-lost` in the start's log directory,
/// and says `network_lost` in the events journal of the store the
/// directory is in (`<containers>/<id>/logs/<N>`; `SHIPYARD_EVENTS_DIR`
/// honoured, as everywhere). Never fails the launcher: what can't be done
/// is left undone.
pub fn pasta_lost(lost: &shipyard_network::pasta::Lost) {
    let said = NetworkLost {
        time: crate::events::rfc3339(std::time::SystemTime::now()),
        how: format!("pasta {}", lost.how),
    };
    // Placed is enough: a marker a power cut takes is the boot's to settle.
    let written = crate::fs::write_atomically(&lost.start_dir, FILE, &said.to_json(), 0o600)
        .map(crate::fs::Placed::unsynced);
    #[cfg(target_os = "linux")]
    if written.is_ok() {
        say_it(lost, &said);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = written;
}

/// The `network_lost` event, for the container whose start's log directory
/// `lost` names.
#[cfg(target_os = "linux")]
fn say_it(lost: &shipyard_network::pasta::Lost, said: &NetworkLost) {
    let Some(record_dir) = lost.start_dir.parent().and_then(Path::parent) else {
        return;
    };
    let (Some(containers), Some(id)) = (
        record_dir.parent(),
        record_dir.file_name().and_then(|id| id.to_str()),
    ) else {
        return;
    };
    // In the launcher, in the container's user namespace (rootless: as its
    // root): only the containers' store (given) and the journal (beside
    // it, or `SHIPYARD_EVENTS_DIR`) are this caller's; this engine's other
    // places (its image store, runtime root) would be root's.
    let engine = crate::Engine::new(
        crate::Paths::from_env().with_containers(containers),
        std::sync::Arc::new(crate::report::Quiet),
    );
    let Ok(id) = crate::containers::ContainerId::parse(id) else {
        return;
    };
    // By its ID: one record read, not the store listed.
    if let Ok(record) = crate::containers::read_one(&engine, &id) {
        crate::events::emit(
            &engine,
            &crate::events::container(&record, "network_lost").with("how", &said.how),
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A pinned version 1 file reads as written; another version doesn't.
    #[test]
    fn a_lost_file_reads_back_and_version_1_is_pinned() {
        let lost = NetworkLost {
            time: "2026-10-14T09:00:00.000000000Z".to_owned(),
            how: "pasta was killed (SIGKILL)".to_owned(),
        };
        let pinned =
            r#"{"v":1,"time":"2026-10-14T09:00:00.000000000Z","how":"pasta was killed (SIGKILL)"}"#;
        assert_eq!(String::from_utf8(lost.to_json()).unwrap(), pinned);
        assert_eq!(NetworkLost::from_json(pinned.as_bytes()), Some(lost));
        assert_eq!(
            NetworkLost::from_json(pinned.replace(r#""v":1"#, r#""v":2"#).as_bytes()),
            None
        );
    }
}
