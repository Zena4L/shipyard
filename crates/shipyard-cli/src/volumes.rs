//! Named volume management for the CLI: the glue over
//! [`shipyard_storage::Volumes`].
//!
//! Volumes live under the same store directory as images (so one
//! `SHIPYARD_IMAGE_STORE` or the default location holds everything), in a
//! `volumes/` subtree. These helpers back `shipyard volume create/ls/inspect/
//! rm/prune` and `system df`.

use std::path::Path;

use anyhow::{Context as _, Result};
use shipyard_storage::{Volume, Volumes};

/// Opens the volume store under the shared store directory.
///
/// # Errors
///
/// If the store can't be opened or the volume directory made.
pub(crate) fn volumes() -> Result<Volumes> {
    Ok(Volumes::open(crate::image::open_store()?.root())?)
}

/// `shipyard volume create NAME`.
///
/// # Errors
///
/// Returns an error if the name is invalid, the volume exists, or the store
/// can't be written.
pub(crate) fn create(name: &str) -> Result<Volume> {
    Ok(volumes()?.create(name)?)
}

/// `shipyard volume ls`.
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub(crate) fn list() -> Result<Vec<Volume>> {
    Ok(volumes()?.list()?)
}

/// `shipyard volume inspect NAME`.
///
/// # Errors
///
/// Returns an error if the volume doesn't exist.
pub(crate) fn inspect(name: &str) -> Result<Volume> {
    Ok(volumes()?.inspect(name)?)
}

/// How much the volumes take (for `shipyard system df`): reclaimable, the
/// ones no running container uses.
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub(crate) fn usage() -> Result<crate::image::Usage> {
    let volumes = volumes()?;
    let dir = shipyard_image::fsat::open_dir(volumes.root()).context("opening the volume store")?;
    let mut usage = crate::image::Usage {
        complete: true,
        ..crate::image::Usage::default()
    };
    for volume in volumes.list()? {
        let taken = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(&volume.name));
        usage.count += 1;
        usage.bytes += taken.bytes;
        usage.complete &= taken.complete;
        if !volumes.in_use(&volume.name)? {
            usage.reclaimable += taken.bytes;
        }
    }
    Ok(usage)
}

/// `shipyard volume rm NAME`: refused while a running container uses it.
///
/// # Errors
///
/// Returns an error if the volume doesn't exist, is in use, or can't be
/// removed.
pub(crate) fn remove(name: &str) -> Result<()> {
    let trash = volumes()?.trash(name)?;
    delete(&[trash])
}

/// What `volume prune` did.
pub(crate) struct PruneReport {
    /// The volumes removed.
    pub removed: Vec<String>,
    /// The volumes left, in use.
    pub in_use: Vec<String>,
    /// What couldn't be removed, and why.
    pub failed: Vec<anyhow::Error>,
}

/// `shipyard volume prune`: removes every volume no running container uses
/// (and what removals cut short left), going on past one that fails.
///
/// # Errors
///
/// Returns an error if the volume store can't be read.
pub(crate) fn prune() -> Result<PruneReport> {
    let pruned = volumes()?.trash_unused()?;
    let mut failed: Vec<anyhow::Error> = pruned
        .failed
        .into_iter()
        .map(|(name, err)| anyhow::Error::from(err).context(format!("the volume {name}")))
        .collect();
    let trash: Vec<String> = pruned
        .trashed
        .iter()
        .map(|(_, trash)| trash.clone())
        .chain(pruned.leftovers)
        .collect();
    if let Err(err) = delete(&trash) {
        failed.push(err);
    }
    Ok(PruneReport {
        removed: pruned.trashed.into_iter().map(|(name, _)| name).collect(),
        in_use: pruned.in_use,
        failed,
    })
}

/// Deletes volumes moved aside, under the store's shared lock: in this
/// process first (most files are the caller's); what's left, files a
/// rootless container wrote as its other users (owned by subordinate IDs),
/// through the storage helper.
fn delete(trash: &[String]) -> Result<()> {
    if trash.is_empty() {
        return Ok(());
    }
    let store = crate::image::open_store()?;
    let held = store.lock(shipyard_image::LockMode::Shared, shipyard_image::USE_WAIT)?;
    let in_process = remove_trash(store.root(), trash);
    #[cfg(target_os = "linux")]
    if let Err(err) = &in_process {
        return crate::image::through_helper(&store, &held, err, || {
            crate::storage_helper::Request::RemoveVolumes(trash.to_vec())
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
            Err(anyhow::anyhow!("{name} can't be a volume moved aside"))
        };
        if let Err(err) = removed {
            first.get_or_insert(err);
        }
    }
    first.map_or(Ok(()), Err)
}
