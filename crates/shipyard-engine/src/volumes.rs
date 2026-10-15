//! Named volumes: the glue over [`shipyard_storage::Volumes`].
//!
//! Volumes live under the same store directory as images (so one
//! `SHIPYARD_IMAGE_STORE` or the default location holds everything), in a
//! `volumes/` subtree. These back `shipyard volume create/ls/inspect/
//! rm/prune` and `system df`.

use std::path::Path;

use shipyard_storage::{Volume, Volumes};

use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result};
use crate::image::Usage;

/// Opens the volume store under the shared store directory.
///
/// # Errors
///
/// If the store can't be opened or the volume directory made.
pub fn open(engine: &Engine) -> Result<Volumes> {
    Ok(Volumes::open(crate::image::open_store(engine)?.root())?)
}

/// `shipyard volume create NAME`.
///
/// # Errors
///
/// Returns an error if the name is invalid ([`ErrorKind::InvalidRequest`]),
/// the volume exists ([`ErrorKind::Exists`]), or the store can't be written.
pub fn create(engine: &Engine, name: &str) -> Result<Volume> {
    let volume = open(engine)?.create(name)?;
    crate::events::emit_about(engine, crate::events::Kind::Volume, "create", name);
    Ok(volume)
}

/// `shipyard volume ls`.
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub fn list(engine: &Engine) -> Result<Vec<Volume>> {
    Ok(open(engine)?.list()?)
}

/// `shipyard volume inspect NAME`.
///
/// # Errors
///
/// Returns an error if the volume doesn't exist ([`ErrorKind::NotFound`]).
pub fn inspect(engine: &Engine, name: &str) -> Result<Volume> {
    Ok(open(engine)?.inspect(name)?)
}

/// How much the volumes take (for `shipyard system df`): reclaimable, the
/// ones `prune` would remove (no container uses them, running or not).
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub fn usage(engine: &Engine) -> Result<Usage> {
    let volumes = open(engine)?;
    let dir = shipyard_image::fsat::open_dir(volumes.root()).context("opening the volume store")?;
    // What `prune` would remove: none a container (running or not) names,
    // nor one in use. Records that can't be read: nothing counted as
    // reclaimable, and the figure said to be incomplete.
    let records = crate::containers::list(engine).ok();
    let mut usage = Usage {
        complete: records.is_some(),
        ..Usage::default()
    };
    for volume in volumes.list()? {
        let taken = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(&volume.name));
        usage.count += 1;
        usage.bytes += taken.bytes;
        usage.complete &= taken.complete;
        let named = records
            .as_deref()
            .is_none_or(|records| !users_of(records, &volume.name).is_empty());
        if !named && !volumes.in_use(&volume.name)? {
            usage.reclaimable += taken.bytes;
        }
    }
    Ok(usage)
}

/// Every container's record, to tell which mount a volume
/// ([`users_of`]). None without a place for records (no container can
/// have one).
///
/// # Errors
///
/// The records can't be listed: a container they'd name can't be ruled
/// out, so the volume `what` names is kept.
fn records(engine: &Engine, what: &str) -> Result<Vec<crate::containers::Record>> {
    match crate::containers::list(engine) {
        Ok(records) => Ok(records),
        Err(error) if error.kind() == ErrorKind::Environment => Ok(Vec::new()),
        Err(error) => Err(error.context(format!("{what} kept: the containers can't be listed"))),
    }
}

/// The containers of `records` that mount the volume `name`, whatever
/// their state (Docker's: a stopped container keeps its volumes), by name,
/// sorted.
fn users_of(records: &[crate::containers::Record], name: &str) -> Vec<String> {
    let mut users: Vec<String> = records
        .iter()
        .filter(|record| {
            record
                .config
                .volume
                .iter()
                .any(|spec| spec.split(':').next() == Some(name))
        })
        .map(|record| record.name.to_string())
        .collect();
    users.sort();
    users
}

/// The volume `name` refused as in use by the containers `users` (whose
/// records mount it).
fn in_use_by(name: &str, users: &[String]) -> Error {
    Error::new(
        ErrorKind::InUse,
        format!(
            "the volume {name} is in use by {}",
            shipyard_network::store::users_text(users)
        ),
    )
}

/// `shipyard volume rm NAME`: refused while a container's record mounts
/// it (running or not, as Docker's), naming them, or a run with no record
/// holds it (the store's own message).
///
/// # Errors
///
/// Returns an error if the volume doesn't exist ([`ErrorKind::NotFound`]),
/// is in use ([`ErrorKind::InUse`]), or can't be removed.
pub fn remove(engine: &Engine, name: &str) -> Result<()> {
    let volumes = open(engine)?;
    let users = users_of(&records(engine, &format!("the volume {name} is"))?, name);
    if !users.is_empty() {
        // Only an existing volume is in use: one that isn't is not found.
        volumes.inspect(name)?;
        return Err(in_use_by(name, &users));
    }
    let trash = volumes.trash(name)?;
    // Gone once moved aside: what's left to delete is no volume.
    crate::events::emit_about(engine, crate::events::Kind::Volume, "destroy", name);
    delete(engine, &[trash])
}

/// What `volume prune` did.
#[derive(Debug)]
pub struct PruneReport {
    /// The volumes removed.
    pub removed: Vec<String>,
    /// The volumes left, in use.
    pub in_use: Vec<String>,
    /// What couldn't be removed, and why.
    pub failed: Vec<Error>,
}

/// `shipyard volume prune`: removes every volume no container uses (none
/// whose record mounts it, running or not, and no run that holds it), and
/// what removals cut short left, going on past one that fails.
///
/// # Errors
///
/// Returns an error if the volume store can't be read, or the containers'
/// records listed.
pub fn prune(engine: &Engine) -> Result<PruneReport> {
    let volumes = open(engine)?;
    let records = records(engine, "every volume is")?;
    let mut in_use = Vec::new();
    let mut trashed = Vec::new();
    let mut failed = Vec::new();
    for volume in volumes.list()? {
        if !users_of(&records, &volume.name).is_empty() {
            in_use.push(volume.name);
            continue;
        }
        match volumes.trash(&volume.name) {
            Ok(trash) => trashed.push((volume.name, trash)),
            Err(shipyard_storage::VolumeError::InUse(name)) => in_use.push(name),
            // Removed by someone else meanwhile.
            Err(shipyard_storage::VolumeError::NotFound(_)) => {}
            Err(err) => {
                failed.push(Error::from(err).context(format!("the volume {}", volume.name)));
            }
        }
    }
    // What removals cut short left, deleted too.
    let trash_dir = volumes.trash_dir();
    let leftovers: Vec<String> = std::fs::read_dir(&trash_dir)
        .with_context(|| format!("reading {}", trash_dir.display()))?
        .filter_map(|entry| entry.ok()?.file_name().to_str().map(str::to_owned))
        .filter(|name| !trashed.iter().any(|(_, trash)| trash == name))
        .collect();
    let trash: Vec<String> = trashed
        .iter()
        .map(|(_, trash)| trash.clone())
        .chain(leftovers)
        .collect();
    if let Err(err) = delete(engine, &trash) {
        failed.push(err);
    }
    for (name, _) in &trashed {
        crate::events::emit_about(engine, crate::events::Kind::Volume, "destroy", name);
    }
    crate::events::emit_pruned(engine, crate::events::Kind::Volume, trashed.len());
    Ok(PruneReport {
        removed: trashed.into_iter().map(|(name, _)| name).collect(),
        in_use,
        failed,
    })
}

/// Deletes volumes moved aside, under the store's shared lock: in this
/// process first (most files are the caller's); what's left, files a
/// rootless container wrote as its other users (owned by subordinate IDs),
/// through the storage helper.
fn delete(engine: &Engine, trash: &[String]) -> Result<()> {
    if trash.is_empty() {
        return Ok(());
    }
    let store = crate::image::open_store(engine)?;
    let held = store.lock(shipyard_image::LockMode::Shared, shipyard_image::USE_WAIT)?;
    let in_process = remove_trash(store.root(), trash);
    #[cfg(target_os = "linux")]
    if let Err(err) = &in_process {
        return crate::image::through_helper(&store, &held, err, || {
            crate::image::Request::RemoveVolumes(trash.to_vec())
        });
    }
    let _ = held;
    in_process
}

/// Removes the volume trash `names` under the store at `store_root`, never
/// following a symlink (`remove_tree_at`), going on past one that fails.
///
/// # Errors
///
/// A name that can't be trash, or what removing one ran into (the first).
pub(crate) fn remove_trash(store_root: &Path, names: &[String]) -> Result<()> {
    let trash_dir = Volumes::open(store_root)?.trash_dir();
    let dir = shipyard_image::fsat::open_dir(&trash_dir)
        .with_context(|| format!("opening {}", trash_dir.display()))?;
    let mut first = None;
    for name in names {
        let removed = if shipyard_storage::is_trash(name) {
            shipyard_image::fsat::remove_tree_at(&dir, std::ffi::OsStr::new(name))
                .with_context(|| format!("removing {}", trash_dir.join(name).display()))
        } else {
            Err(Error::new(
                ErrorKind::Storage,
                format!("{name} can't be a volume moved aside"),
            ))
        };
        if let Err(err) = removed {
            first.get_or_insert(err);
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;
    use crate::testing::engine_on_a_new_store;

    #[test]
    fn volume_errors_have_kinds() {
        let (engine, collect, parent) = engine_on_a_new_store("volumes");
        create(&engine, "data").unwrap();
        let exists = create(&engine, "data").unwrap_err();
        assert_eq!(exists.kind(), ErrorKind::Exists, "{exists:#}");
        let missing = inspect(&engine, "nothing").unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound, "{missing:#}");
        let invalid = create(&engine, "../escape").unwrap_err();
        assert_eq!(invalid.kind(), ErrorKind::InvalidRequest, "{invalid:#}");
        remove(&engine, "data").unwrap();
        assert!(list(&engine).unwrap().is_empty());
        assert!(collect.events().is_empty(), "{:?}", collect.events());
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A volume a container's record mounts is kept while the record is,
    /// whatever its state (a container never started here): `rm` refuses
    /// it naming the container, `prune` leaves it; once the container is
    /// removed, `rm` removes it (Docker's).
    #[test]
    fn a_volume_a_record_mounts_is_kept() {
        let parent = std::env::temp_dir().join(format!(
            "shipyard-engine-volume-records-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&parent).unwrap();
        let engine = Engine::new(
            crate::Paths::from_env()
                .with_image_store(parent.join("store"))
                .with_containers(parent.join("containers")),
            std::sync::Arc::new(crate::report::Collect::default()),
        );
        create(&engine, "data").unwrap();
        create(&engine, "spare").unwrap();
        let request = crate::run::RunRequest {
            command: vec!["true".into()],
            volume: vec!["data:/data:ro".to_owned()],
            ..crate::run::RunRequest::default()
        };
        let record = crate::containers::create_record(
            &engine,
            crate::containers::NewContainer {
                name: Some(crate::containers::Name::parse("web").unwrap()),
                config: crate::containers::Config::from_request(&request).unwrap(),
                labels: crate::containers::Labels::default(),
                image_id: None,
                auto_remove: false,
            },
        )
        .unwrap();
        let refused = remove(&engine, "data").unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InUse, "{refused:#}");
        assert!(format!("{refused}").contains("web"), "{refused:#}");
        // One that isn't there is still not found.
        let missing = remove(&engine, "nothing").unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound, "{missing:#}");
        let pruned = prune(&engine).unwrap();
        assert_eq!(pruned.in_use, ["data"]);
        assert_eq!(pruned.removed, ["spare"]);
        assert!(pruned.failed.is_empty(), "{:?}", pruned.failed);
        crate::containers::remove_record(&engine, &record.id).unwrap();
        remove(&engine, "data").unwrap();
        assert!(list(&engine).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&parent);
    }
}
