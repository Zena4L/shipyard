//! Volumes as Docker's Engine API shapes them (step 11.2c2a): `GET
//! /volumes`' and `GET /volumes/{name}`'s `Volume`, checked against Docker
//! 29.8's. Every volume is `local`'s: a directory of the store's.

use std::collections::BTreeMap;

use serde::Serialize;

use super::filters::VolumeFilters;
use crate::Engine;
use crate::error::Result;

/// The label Docker gives a volume made without a name (a create's, or a
/// container's `-v /path`): its prune takes those without `all`.
pub const ANONYMOUS_VOLUME: &str = "com.docker.volume.anonymous";

/// A volume (Docker's `Volume`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Volume {
    /// When it was made (RFC 3339, whole seconds).
    pub created_at: String,
    /// Always `local`.
    pub driver: String,
    /// Its labels, or `null`.
    pub labels: Option<BTreeMap<String, String>>,
    /// Where its data is on the host.
    pub mountpoint: String,
    /// Its name.
    pub name: String,
    /// Its driver's options: always `null`.
    pub options: Option<()>,
    /// Always `local`.
    pub scope: String,
}

/// `GET /volumes`' answer (Docker's `VolumeListResponse`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct VolumeList {
    /// The volumes.
    pub volumes: Vec<Volume>,
    /// What couldn't be read, said: `null` when nothing.
    pub warnings: Option<Vec<String>>,
}

/// `GET /volumes`: every volume, by name. One whose labels or time can't be
/// read is shown without them, and said in `Warnings`, never the list
/// refused; one removed while it's listed is left out.
///
/// # Errors
///
/// The store can't be read.
pub fn list_volumes(engine: &Engine) -> Result<VolumeList> {
    let store = crate::volumes::open(engine)?;
    let mut warnings = Vec::new();
    let volumes = store
        .list()?
        .into_iter()
        .filter_map(|volume| shown(&store, volume, &mut warnings))
        .collect();
    Ok(VolumeList {
        volumes,
        warnings: Some(warnings).filter(|warnings| !warnings.is_empty()),
    })
}

/// `GET /volumes/{name}`.
///
/// # Errors
///
/// No such volume ([`crate::ErrorKind::NotFound`]); its labels can't be
/// read ([`crate::ErrorKind::Storage`]).
pub fn inspect_volume(engine: &Engine, name: &str) -> Result<Volume> {
    let store = crate::volumes::open(engine)?;
    let volume = store.inspect(name)?;
    let labels = store.labels(name)?;
    // A making's time that can't be read (a newer Shipyard's file, a damaged
    // one) is said, never a failed inspect: the directory's stands in, as a
    // listing does.
    let created = store.created(name).or_else(|error| match error {
        shipyard_storage::VolumeError::Created { .. } => {
            engine.warn(
                crate::report::WarningKind::RecordUnreadable,
                error.to_string(),
            );
            Ok(volume_dir_time(&volume))
        }
        error => Err(error),
    })?;
    Ok(of(volume, labels, created))
}

/// The time of the directory holding `volume`'s data (its making, but for
/// what was made beside it since).
fn volume_dir_time(volume: &shipyard_storage::Volume) -> std::time::SystemTime {
    volume
        .data
        .parent()
        .and_then(|dir| std::fs::metadata(dir).ok())
        .and_then(|metadata| metadata.modified().ok())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// `volume` as Docker shows it, what can't be read of it said in
/// `warnings`; `None` if it's gone meanwhile.
fn shown(
    store: &shipyard_storage::Volumes,
    volume: shipyard_storage::Volume,
    warnings: &mut Vec<String>,
) -> Option<Volume> {
    let created = match store.created(&volume.name) {
        Ok(created) => created,
        Err(shipyard_storage::VolumeError::NotFound(_)) => return None,
        Err(error) => {
            warnings.push(error.to_string());
            std::time::UNIX_EPOCH
        }
    };
    let labels = store.labels(&volume.name).unwrap_or_else(|error| {
        warnings.push(error.to_string());
        BTreeMap::new()
    });
    Some(of(volume, labels, created))
}

/// Docker's shape of `volume`.
fn of(
    volume: shipyard_storage::Volume,
    labels: BTreeMap<String, String>,
    created: std::time::SystemTime,
) -> Volume {
    Volume {
        created_at: whole_seconds(created),
        driver: "local".to_owned(),
        labels: Some(labels).filter(|labels| !labels.is_empty()),
        mountpoint: volume.data.display().to_string(),
        name: volume.name,
        options: None,
        scope: "local".to_owned(),
    }
}

/// `at` in RFC 3339, to the second, as Docker shows a volume's.
fn whole_seconds(at: std::time::SystemTime) -> String {
    let seconds = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(std::time::UNIX_EPOCH, |since| {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(since.as_secs())
        });
    super::time(seconds)
}

/// `GET /volumes` and `volume ls`: the volumes `filters` pass, by name.
/// Which volumes containers use is read only for `dangling`.
///
/// # Errors
///
/// As [`list_volumes`]; the containers' records, for `dangling`.
pub fn list_volumes_where(engine: &Engine, filters: &VolumeFilters) -> Result<VolumeList> {
    let mut listed = list_volumes(engine)?;
    let mounted = if filters.wants_mounted() {
        crate::volumes::in_use(engine)?
    } else {
        std::collections::BTreeSet::new()
    };
    listed
        .volumes
        .retain(|volume| filters.pass(volume, mounted.contains(&volume.name)));
    listed.volumes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A volume has Docker 29.8's keys, its empty parts `null` as Docker's
    /// are, its time to the second.
    #[test]
    fn a_volume_is_shown_as_docker_shows_it() {
        let volume = shipyard_storage::Volume {
            name: "data".to_owned(),
            data: "/store/volumes/data/_data".into(),
        };
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(1_760_000_000_123_456_789);
        let shown = serde_json::to_value(of(volume.clone(), BTreeMap::new(), at)).unwrap();
        assert_eq!(
            shown,
            serde_json::json!({
                "CreatedAt": "2025-10-09T08:53:20Z", "Driver": "local", "Labels": null,
                "Mountpoint": "/store/volumes/data/_data", "Name": "data", "Options": null,
                "Scope": "local"
            })
        );
        let labelled = of(
            volume,
            BTreeMap::from([("a".to_owned(), "b".to_owned())]),
            at,
        );
        assert_eq!(
            serde_json::to_value(labelled).unwrap()["Labels"],
            serde_json::json!({"a": "b"})
        );
        let empty = VolumeList {
            volumes: Vec::new(),
            warnings: None,
        };
        assert_eq!(
            serde_json::to_value(empty).unwrap(),
            serde_json::json!({"Volumes": [], "Warnings": null})
        );
    }
}
