//! Named volumes: the glue over [`shipyard_storage::Volumes`].
//!
//! Volumes live under the same store directory as images (so one
//! `SHIPYARD_IMAGE_STORE` or the default location holds everything), in a
//! `volumes/` subtree. These back `shipyard volume create/ls/inspect/
//! rm/prune` and `system df`.

mod listing;

pub use listing::{VOLUMES_VERSION, VolumeContent, VolumeContents, VolumeFile, contents};

use std::collections::{BTreeMap, BTreeSet};
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

/// Refuses what a volume of Shipyard's can't be (the CLI's and the API's
/// create alike): a driver other than `local` (none, or empty, is it), or
/// driver options (`type=nfs` and the like would mount something else).
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`], saying which.
pub fn check_driver(driver: Option<&str>, has_options: bool) -> Result<()> {
    if let Some(driver) = driver.filter(|driver| !driver.is_empty() && *driver != "local") {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            format!("the volume driver {driver} isn't supported: only local"),
        ));
    }
    if has_options {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "volume driver options aren't supported: a volume is a directory of the store's",
        ));
    }
    Ok(())
}

/// Marks `labels` anonymous, as Docker marks a volume it made unnamed
/// (`com.docker.volume.anonymous`, empty): its prune takes those.
pub(crate) fn mark_anonymous(labels: &mut BTreeMap<String, String>) {
    labels.insert(crate::docker::ANONYMOUS_VOLUME.to_owned(), String::new());
}

/// `POST /volumes/create` (Docker's): the volume `name` (none: a new
/// 64-hex one, as Docker names it) with `labels`. One that exists is
/// returned as it is, its labels unchanged, as Docker's `local` driver
/// does.
///
/// # Errors
///
/// Returns an error if the name is invalid ([`ErrorKind::InvalidRequest`]),
/// or the store can't be written.
pub fn create_with(
    engine: &Engine,
    name: Option<&str>,
    labels: &BTreeMap<String, String>,
) -> Result<Volume> {
    // Without a name: a new one, and anonymous, as Docker marks it (its
    // prune takes those without `all`).
    let mut labels = labels.clone();
    let name = if let Some(name) = name.filter(|name| !name.is_empty()) {
        name.to_owned()
    } else {
        mark_anonymous(&mut labels);
        crate::docker::new_id()?
    };
    let volumes = open(engine)?;
    match volumes.create_with(&name, &labels) {
        Ok(volume) => {
            crate::events::emit_about(engine, crate::events::Kind::Volume, "create", &name);
            Ok(volume)
        }
        Err(shipyard_storage::VolumeError::Exists(_)) => Ok(volumes.inspect(&name)?),
        Err(error) => Err(error.into()),
    }
}

/// Makes the new anonymous volume `name` for a create (never one that's
/// there: [`ErrorKind::Exists`]), and
/// holds it (shared) until the container's record names it: a prune
/// meanwhile only tries its lock, so never takes it.
///
/// # Errors
///
/// It's there already, or it couldn't be made or held.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a create is Linux's")
)]
pub(crate) fn create_anonymous(
    engine: &Engine,
    name: &str,
) -> Result<shipyard_storage::VolumeHold> {
    let mut labels = BTreeMap::new();
    mark_anonymous(&mut labels);
    let volumes = open(engine)?;
    volumes.create_with(name, &labels)?;
    crate::events::emit_about(engine, crate::events::Kind::Volume, "create", name);
    Ok(volumes.hold_existing(name)?)
}

/// The labels of the volume `name` (none for one made without).
///
/// # Errors
///
/// Its labels can't be read (a newer version: [`ErrorKind::Storage`]).
pub fn labels(engine: &Engine, name: &str) -> Result<BTreeMap<String, String>> {
    Ok(open(engine)?.labels(name)?)
}

/// The volumes some container's record mounts (running or not, as `rm`
/// refuses them): what isn't dangling. Read without the store's lock: a
/// listing's answer, stale once given (`rm` and `prune` decide under it).
///
/// # Errors
///
/// The records can't be listed.
pub fn in_use(engine: &Engine) -> Result<BTreeSet<String>> {
    let records = crate::containers::list(engine)?;
    Ok(records
        .iter()
        .flat_map(|record| {
            record
                .config
                .volume
                .iter()
                .filter_map(|spec| mounted_volume(spec))
        })
        .map(str::to_owned)
        .collect())
}

/// The named volume a `--volume` spec mounts (`NAME:/path[:ro]`), if it
/// mounts one (not a host path).
fn mounted_volume(spec: &str) -> Option<&str> {
    spec.split(':')
        .next()
        .filter(|source| shipyard_storage::volume::is_named_volume(source))
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
        usage.total += 1;
        usage.bytes += taken.bytes;
        usage.complete &= taken.complete;
        let named = records
            .as_deref()
            .is_none_or(|records| !users_of(records, &volume.name).is_empty());
        if !named && !volumes.in_use(&volume.name)? {
            usage.reclaimable += taken.bytes;
        } else {
            usage.active += 1;
        }
    }
    Ok(usage)
}

/// The containers of `records` that mount the volume `name`, whatever
/// their state (Docker's: a stopped container keeps its volumes), by name,
/// sorted.
pub(crate) fn users_of(records: &[crate::containers::Record], name: &str) -> Vec<String> {
    let mut users: Vec<String> = records
        .iter()
        .filter(|record| {
            record
                .config
                .volume
                .iter()
                .any(|spec| mounted_volume(spec) == Some(name))
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
    // The check and the move aside under the container store's lock: a
    // container made meanwhile, naming it, can't come between (F56).
    let trash =
        crate::containers::with_records_locked(engine, &format!("the volume {name}"), |records| {
            let users = users_of(records, name);
            if !users.is_empty() {
                // Only an existing volume is in use: one that isn't is not
                // found.
                volumes.inspect(name)?;
                return Err(in_use_by(name, &users));
            }
            Ok(volumes.trash(name)?)
        })?;
    // Gone once moved aside: what's left to delete is no volume. Deleted
    // with the store's lock let go (the image store's lock comes first).
    crate::events::emit_about(engine, crate::events::Kind::Volume, "destroy", name);
    delete(engine, &[trash])
}

/// What `volume prune` did.
#[derive(Debug)]
pub struct PruneReport {
    /// The volumes removed.
    pub removed: Vec<String>,
    /// The bytes they held (what could be read of them).
    pub bytes: u64,
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
    prune_where(engine, &|_, _| true)
}

/// [`prune`], of the volumes `wanted` takes (by name and labels: Docker's
/// prune takes only anonymous ones unless asked for all). A volume whose
/// labels can't be read is never taken.
///
/// # Errors
///
/// As [`prune`].
pub fn prune_where(
    engine: &Engine,
    wanted: &dyn Fn(&str, &BTreeMap<String, String>) -> bool,
) -> Result<PruneReport> {
    let volumes = open(engine)?;
    let mut in_use = Vec::new();
    let mut trashed = Vec::new();
    let mut failed = Vec::new();
    // Each check and move aside under the container store's lock (F56).
    crate::containers::with_records_locked(engine, "every volume", |records| {
        for volume in volumes.list()? {
            // Labels that can't be read: never taken, and said.
            match volumes.labels(&volume.name) {
                Ok(labels) if wanted(&volume.name, &labels) => {}
                Ok(_) => continue,
                Err(err) => {
                    failed.push(Error::from(err).context(format!("the volume {}", volume.name)));
                    continue;
                }
            }
            if !users_of(records, &volume.name).is_empty() {
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
        Ok(())
    })?;
    // What they held, measured where they were moved: after the lock, so
    // no walk of a big volume holds the container store up.
    let trash_dir = volumes.trash_dir();
    let bytes = match shipyard_image::fsat::open_dir(&trash_dir) {
        Ok(dir) => trashed.iter().fold(0_u64, |bytes, (_, trash)| {
            let taken = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(trash));
            bytes.saturating_add(taken.bytes)
        }),
        Err(_) => 0,
    };
    // What removals cut short left, deleted too.
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
        bytes,
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
        let engine = Engine::new(
            engine
                .paths()
                .clone()
                .with_containers(parent.join("containers")),
            std::sync::Arc::clone(engine.reporter()),
        );
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
        let (engine, parent) = engine_with_records("volume-records");
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
                created: crate::containers::Created::default(),
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

    /// A container store whose place isn't known (a relative
    /// `SHIPYARD_CONTAINER_DIR`, no home) might hold records naming any
    /// volume: `rm` and `prune` keep them all, saying so; with its place
    /// known (and no record there), the same volume goes.
    #[test]
    fn volumes_are_kept_when_the_container_store_isnt_found() {
        let (engine, parent) = engine_with_records("volume-store-unknown");
        create(&engine, "data").unwrap();
        let lost = Engine::new(
            engine
                .paths()
                .clone()
                .without_containers("SHIPYARD_CONTAINER_DIR must be absolute"),
            std::sync::Arc::clone(engine.reporter()),
        );
        let refused = remove(&lost, "data").unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Environment, "{refused:#}");
        let refused = prune(&lost).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Environment, "{refused:#}");
        assert_eq!(list(&engine).unwrap().len(), 1, "kept");
        remove(&engine, "data").unwrap();
        assert!(list(&engine).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// An engine on a store of its own, its records in `parent/containers`.
    fn engine_with_records(name: &str) -> (Engine, std::path::PathBuf) {
        let parent = std::env::temp_dir().join(format!(
            "shipyard-engine-{name}-{}-{}",
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
        (engine, parent)
    }

    /// A record that can't be read might name any volume (F56): `rm` of
    /// one no readable record names is refused, and `prune` removes
    /// nothing; once it can be read (here, gone), both go ahead (the
    /// control).
    #[test]
    fn an_unreadable_record_keeps_the_volumes() {
        let (engine, parent) = engine_with_records("volume-unreadable");
        create(&engine, "spare").unwrap();
        crate::containers::create_record(
            &engine,
            crate::containers::NewContainer {
                name: Some(crate::containers::Name::parse("other").unwrap()),
                config: crate::containers::Config::from_request(&crate::run::RunRequest {
                    command: vec!["true".into()],
                    ..crate::run::RunRequest::default()
                })
                .unwrap(),
                labels: crate::containers::Labels::default(),
                image_id: None,
                auto_remove: false,
                created: crate::containers::Created::default(),
            },
        )
        .unwrap();
        // Another container's record, torn: unknown what it mounts.
        let torn = parent.join("containers").join("a".repeat(64));
        std::fs::create_dir(&torn).unwrap();
        std::fs::write(torn.join("record.json"), "{ not json").unwrap();
        let refused = remove(&engine, "spare").unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Storage, "{refused:#}");
        assert_eq!(prune(&engine).unwrap_err().kind(), ErrorKind::Storage);
        assert_eq!(list(&engine).unwrap().len(), 1, "kept");
        std::fs::remove_dir_all(&torn).unwrap();
        assert_eq!(prune(&engine).unwrap().removed, ["spare"]);
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// `prune` decides under the container store's lock (F56): held by
    /// another (a `create` publishing a record), a `prune` waits for it,
    /// and goes ahead once it's let go, having waited; never past it.
    #[test]
    fn prune_waits_for_the_container_store() {
        let (engine, parent) = engine_with_records("volume-store-lock");
        create(&engine, "spare").unwrap();
        std::fs::create_dir_all(parent.join("containers")).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join("containers").join(".lock"))
            .unwrap();
        lock.lock().unwrap();
        let held = std::time::Duration::from_millis(300);
        // One clock for both, started before the prune's thread: started in
        // it, it would start late and measure a held lock as a moment short.
        let letting_go = std::time::Instant::now();
        let pruning = std::thread::spawn(move || {
            let pruned = prune(&engine);
            (pruned, letting_go.elapsed())
        });
        while letting_go.elapsed() < held {
            std::thread::yield_now();
        }
        drop(lock);
        let (pruned, took) = pruning.join().unwrap();
        assert_eq!(pruned.unwrap().removed, ["spare"]);
        assert!(took >= held, "it went ahead past the lock: {took:?}");
        let _ = std::fs::remove_dir_all(&parent);
    }
}
