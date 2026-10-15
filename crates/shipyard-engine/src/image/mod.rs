//! Images: the glue between `shipyard-image` (pull, store, unpack) and the
//! runtime.
//!
//! `prepare` turns an image reference into the pieces a run needs: it
//! pulls the image if the store doesn't have it, extracts its layers into
//! directories, and makes the run's own writable area (`containers/run-…`,
//! [`shipyard_image::ContainerArea`]), held for the run's whole life and
//! removed when it ends. It returns an `Overlay` (the layer directories) and
//! the image's runtime config. (It and its types are Linux-only, since only
//! Linux mounts the overlay.)
//!
//! A container (9.4a) is prepared differently: `make_ready` at its making
//! (pulled, extracted, recorded), its image `pin`ned (an index entry that
//! keeps its files while the container exists); then at each start
//! `prepare_owned`, by the digest it was made from, from the store only,
//! with the container's own writable area (`ctr-<id>`): made at its first
//! start, taken again by every later one, never removed by a sweep or a
//! prune while its container exists (only by `remove_owned_area`, its
//! removal).
//!
//! Processes share the store through its lock (`shipyard_image::Store::lock`):
//! `prepare` holds it shared, so garbage collection can't take what it's
//! adding. [`remove`] and [`prune`] hold it exclusive only while they decide
//! and move things aside to trash (quick renames, keeping what live areas
//! use), then shared while they delete the trash, so runs can start
//! meanwhile.
//!
//! The overlay is **not** mounted here. The runtime mounts it inside the
//! container's own user and mount namespaces (`Root::overlay`), where the
//! kernel permits an unprivileged (rootless) overlay mount — mounting it in
//! the engine beforehand would fail rootless with `EPERM`. So this module
//! only prepares directories; the mount is the init's job.

mod helper;

use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::Arc;

use shipyard_image::index::Index;
use shipyard_image::{
    Descriptor, GC_WAIT, ImageReference, LiveArea, LockMode, Platform, Pulled, Store, StoreLock,
    USE_WAIT,
};
#[cfg(target_os = "linux")]
use shipyard_image::{LayerOwners, LayerSpec, RuntimeConfig};
#[cfg(target_os = "linux")]
use shipyard_runtime::isolation::{IdMap, IdMaps, Overlay};

#[cfg(target_os = "linux")]
pub(crate) use helper::Request;
pub use helper::entry_point as storage_helper_entry_point;
#[cfg(target_os = "linux")]
use helper::{Outcome, ranges};

use crate::Engine;
#[cfg(target_os = "linux")]
use crate::error::ErrorKind;
use crate::error::{Context as _, Error, Result, bail};
#[cfg(target_os = "linux")]
use crate::report::Reporter;
use crate::report::{Event, Progress, WarningKind};

/// When a run pulls its image (`--pull`), as Docker's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPolicy {
    /// Only if the store doesn't have it whole.
    Missing,
    /// Every time (a tag may have moved).
    Always,
    /// Never: the store's, or nothing.
    Never,
}

/// A prepared image, ready to run: the overlay layout, the image's baked-in
/// runtime defaults, and the run's writable area.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct Prepared {
    /// The merged mount point — the container's root filesystem path. The
    /// runtime mounts the overlay here, then binds and pivots into it.
    pub root: PathBuf,
    /// The overlay to mount as the root (layer dirs + upper + work).
    pub overlay: Overlay,
    /// The image's baked-in runtime defaults.
    pub config: RuntimeConfig,
    /// The run's writable area: keep it until the container ends.
    pub area: RunArea,
}

/// A run's writable area, alive (locked) while this value is, and removed
/// when it goes. The run calls [`remove`](Self::remove) once the container
/// has ended, before the process exits: dying from the container's signal
/// runs no destructors. Dropping it removes it too, for an error on the way.
/// (A run killed outright leaves it unlocked: the next run, or `prune`,
/// sweeps it.) A detached container's is given up to its monitor instead
/// (`hold`, `give_up`), and removed by its `__cleanup`.
///
/// A container's own area (9.4a, `ctr-<id>`) is never removed this way:
/// dropped or ended, it's only unlocked (its files are the container's,
/// kept until `rm`); only `remove_owned_area` removes it.
#[cfg(target_os = "linux")]
pub struct RunArea {
    store: Store,
    area: Option<shipyard_image::ContainerArea>,
    /// A container's own: unlocked, never removed, as it goes.
    owned: bool,
    reporter: Arc<dyn Reporter>,
}

#[cfg(target_os = "linux")]
impl RunArea {
    /// Removes the area, now the container has ended.
    pub fn remove(mut self) {
        self.clean_up();
    }

    /// Whether it's a container's own (`ctr-<id>`): kept as it goes.
    #[must_use]
    pub(crate) fn is_owned(&self) -> bool {
        self.owned
    }

    /// The area's name in the image store's `containers/` (none once given
    /// up).
    #[must_use]
    pub(crate) fn name(&self) -> Option<&str> {
        self.area.as_ref().map(shipyard_image::ContainerArea::name)
    }

    /// The area's locked directory, for a container's monitor to keep it
    /// alive once this process has gone (none once given up).
    #[must_use]
    pub(crate) fn hold(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.area.as_ref().map(shipyard_image::ContainerArea::hold)
    }

    /// Gives the area up to whoever holds a copy of [`hold`](Self::hold):
    /// this process's copy is closed (never unlocked), and nothing is
    /// removed (the holder does that).
    pub(crate) fn give_up(mut self) {
        if let Some(area) = self.area.take() {
            area.give_up();
        }
    }

    /// Removes the area while still holding its lock, under the store's
    /// shared lock (so a garbage collection can't take it half-way). A
    /// failure is a warning: the area is left for a later sweep. A
    /// container's own is only unlocked (dropped).
    fn clean_up(&mut self) {
        let Some(area) = self.area.take() else {
            return;
        };
        if self.owned {
            return;
        }
        let removed = self
            .store
            .lock(LockMode::Shared, USE_WAIT)
            .map_err(Error::from)
            .and_then(|shared| {
                let trash = shipyard_image::trash_areas(&self.store, &[area.name().to_owned()])?;
                delete(&self.store, &shared, &Trash::Areas(trash))
            });
        if let Err(err) = removed {
            crate::report::warn(
                self.reporter.as_ref(),
                WarningKind::LeftForLater,
                format!(
                    "couldn't remove the container's writable layer ({}): {err:#}; the next run \
                     or `shipyard image prune` will",
                    area.name()
                ),
            );
        }
    }
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for RunArea {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunArea")
            .field("store", &self.store.root())
            .field(
                "area",
                &self.area.as_ref().map(shipyard_image::ContainerArea::name),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl Drop for RunArea {
    fn drop(&mut self) {
        self.clean_up();
    }
}

/// Opens the store, made private if it's new. Every operation that touches
/// the store directory (volumes too) opens it this way first, so nothing
/// makes its root with looser permissions.
///
/// # Errors
///
/// If the store can't be made, or it's reachable by other users.
pub fn open_store(engine: &Engine) -> Result<Store> {
    Store::open(engine.paths().image_store()?).context("opening the image store")
}

/// Prepares `reference` to run: find it in the store, or pull it (as
/// `policy` says), extract its layers, and make the run's own writable area,
/// all under the store's shared lock.
///
/// # Errors
///
/// Returns an error if the image can't be found or pulled, or a layer can't
/// be extracted.
#[cfg(target_os = "linux")]
pub fn prepare(
    engine: &Engine,
    reference: &str,
    policy: PullPolicy,
    maps: Option<&IdMaps>,
) -> Result<Prepared> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    // Areas runs killed outright left (best effort: `prune` tries again).
    if let Err(err) = sweep_stale(engine, &store, &shared) {
        tracing::debug!("couldn't sweep stale container layers: {err:#}");
    }
    let image = ImageReference::parse(reference)?;
    let Ready {
        pulled,
        extracted: (directories, names),
    } = ready(engine, &store, &shared, &image, policy, maps)?;

    // The run's own writable area, listing the layers it stacks, so garbage
    // collection keeps them while it runs.
    let area = shipyard_image::ContainerArea::create(
        &store,
        &shared,
        &pulled.manifest_digest.to_string(),
        &names,
    )
    .context("making the container's writable layer")?;
    drop(shared);
    if let Some(maps) = maps {
        // The container's root, mapped, writes the writable layer (and
        // overlay's root directory takes the upper directory's owner).
        give_to_container_root(&[area.upper(), area.work()], maps)?;
    }

    Ok(Prepared {
        root: area.merged(),
        overlay: Overlay::new(directories, area.upper(), area.work()),
        config: pulled.config.config,
        area: RunArea {
            store,
            area: Some(area),
            owned: false,
            reporter: Arc::clone(engine.reporter()),
        },
    })
}

/// Makes `reference` ready for a container to be made from it (`create`):
/// found or pulled (as `policy` says), its layers extracted (with `maps`,
/// as its starts will want them), and recorded in the index, as `pull`
/// does. No writable area: the container's own is made at its first start.
/// Returns its manifest's digest (what the container runs) and its runtime
/// config (what's settled from it at create: its health check).
///
/// # Errors
///
/// As [`prepare`].
#[cfg(target_os = "linux")]
pub(crate) fn make_ready(
    engine: &Engine,
    reference: &str,
    policy: PullPolicy,
    maps: Option<&IdMaps>,
) -> Result<(String, shipyard_image::RuntimeConfig)> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let image = ImageReference::parse(reference)?;
    let ready = ready(engine, &store, &shared, &image, policy, maps)?;
    Ok((
        ready.pulled.manifest_digest.to_string(),
        ready.pulled.config.config,
    ))
}

/// How long a start waits for its container's own area's lock: a listing's
/// probe holds it for an instant; a monitor, for the container's life.
#[cfg(target_os = "linux")]
const OWNED_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Prepares container `id`'s image to start: `reference` as the container
/// was made with, by the manifest digest it was made from (`image_id`),
/// from the store only (a container's pin keeps it there), recording no
/// reference in the index; its layers extracted (again, if a collection
/// took them); and the container's own writable area (`ctr-<id>`, its
/// container store `owner`), taken if it exists: then its overlay stacks
/// the layers the area lists, the same every start.
///
/// # Errors
///
/// The image isn't in the store any more ([`ErrorKind::NotFound`]), a
/// layer can't be extracted, a layer the area stacks is gone, or the area
/// is in use ([`ErrorKind::InUse`]: the container is running).
#[cfg(target_os = "linux")]
pub(crate) fn prepare_owned(
    engine: &Engine,
    id: &str,
    owner: &str,
    reference: &str,
    image_id: &str,
    maps: Option<&IdMaps>,
) -> Result<Prepared> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    if let Err(err) = sweep_stale(engine, &store, &shared) {
        tracing::debug!("couldn't sweep stale container layers: {err:#}");
    }
    let mut image = ImageReference::parse(reference)?;
    image.digest = Some(
        shipyard_image::Digest::parse(image_id)
            .map_err(|err| Error::new(crate::ErrorKind::Storage, format!("{image_id}: {err}")))?,
    );
    let Some(pulled) = shipyard_image::from_store(&image, &Platform::host(), &store)
        .with_context(|| format!("reading {reference} ({image_id}) from the image store"))?
    else {
        bail!(
            NotFound,
            "the image the container was made from ({reference}, {image_id}) isn't in the image \
             store any more"
        );
    };
    let layers = shipyard_image::layer_specs(&pulled.manifest, &pulled.config)
        .with_context(|| format!("reading the layers of {reference}"))?;
    let (directories, names) = extract(engine, &store, &shared, &layers, maps)
        .with_context(|| format!("extracting the layers of {reference}"))?;
    let area = shipyard_image::ContainerArea::create_or_take_owned(
        &store, &shared, id, owner, image_id, &names, OWNED_WAIT,
    )
    .map_err(|err| match err {
        shipyard_image::AreaError::InUse(_) => Error::new(crate::ErrorKind::InUse, err.to_string()),
        other => Error::from(other).context("the container's writable layer"),
    })?;
    // Its stack as it was made: the same layers under the same upper layer.
    let directories = if area.layers() == names.as_slice() {
        directories
    } else {
        let layers_dir = store.root().join("layers");
        area.layers()
            .iter()
            .map(|name| {
                let dir = layers_dir.join(name);
                if dir.is_dir() {
                    Ok(dir)
                } else {
                    Err(Error::new(
                        crate::ErrorKind::Storage,
                        format!(
                            "the container's writable layer stacks the layer {name}, which isn't in \
                             the image store any more"
                        ),
                    ))
                }
            })
            .collect::<Result<Vec<_>>>()?
    };
    drop(shared);
    if let Some(maps) = maps {
        give_to_container_root(&[area.upper(), area.work()], maps)?;
    }
    Ok(Prepared {
        root: area.merged(),
        overlay: Overlay::new(directories, area.upper(), area.work()),
        config: pulled.config.config,
        area: RunArea {
            store,
            area: Some(area),
            owned: true,
            reporter: Arc::clone(engine.reporter()),
        },
    })
}

/// Pins the image a container `id` is made from (`reference`, by its
/// manifest `digest` if it's known): an index entry under its container
/// store's identity and its ID ([`Index::pin`]) that keeps the image's
/// files while the container exists. Again is nothing new (the same entry).
/// Returns the image's manifest digest (the record's `image_id`).
///
/// # Errors
///
/// The image isn't in the store, or the index can't be written.
#[cfg(target_os = "linux")]
pub(crate) fn pin(
    engine: &Engine,
    id: &str,
    reference: &str,
    digest: Option<&str>,
) -> Result<String> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let owner = crate::containers::require_store_id(engine)?;
    let mut image = ImageReference::parse(reference)?;
    if let Some(digest) = digest {
        image.digest = Some(
            shipyard_image::Digest::parse(digest)
                .map_err(|err| Error::new(ErrorKind::Storage, format!("{digest}: {err}")))?,
        );
    }
    let Some(pulled) = shipyard_image::from_store(&image, &Platform::host(), &store)
        .with_context(|| format!("reading {reference} from the image store"))?
    else {
        bail!(NotFound, "{reference} isn't in the image store");
    };
    let record = shipyard_image::ImageRecord {
        manifest: pulled.manifest_digest.to_string(),
        config: pulled.manifest.config.digest.to_string(),
        layers: pulled
            .manifest
            .layers
            .iter()
            .map(|layer| layer.digest.to_string())
            .collect(),
    };
    let digest = record.manifest.clone();
    Index::pin(store.root(), &owner, id, record)?;
    drop(shared);
    Ok(digest)
}

/// Lets go of container `id`'s pin (the next collection takes what only it
/// kept). Whether it had one.
///
/// # Errors
///
/// The index can't be read or written.
#[cfg(target_os = "linux")]
pub(crate) fn unpin(engine: &Engine, id: &str) -> Result<bool> {
    let store = open_store(engine)?;
    // Under the store's lock, as every index writer: a collection (which
    // holds it exclusively, and saves what it read) never interleaves.
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let Some(owner) = crate::containers::store_id(engine, false)? else {
        return Ok(false);
    };
    let unpinned = Index::unpin(store.root(), &owner, id)?;
    drop(shared);
    Ok(unpinned)
}

/// Removes container `id`'s own writable area, if it has one, under the
/// store's shared lock: `rm`'s, once nothing holds it (else
/// [`ErrorKind::InUse`]).
///
/// # Errors
///
/// It's held, or it couldn't be removed.
#[cfg(target_os = "linux")]
pub(crate) fn remove_owned_area(engine: &Engine, id: &str) -> Result<()> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    // Taken (a listing's probe may hold it for an instant), then moved
    // aside while held: no sweep sees it half gone.
    let Some(area) = shipyard_image::ContainerArea::take_owned(&store, id, OWNED_WAIT).map_err(
        |err| match err {
            shipyard_image::AreaError::InUse(_) => {
                Error::new(ErrorKind::InUse, "the container's writable layer is in use")
            }
            other => Error::from(other),
        },
    )?
    else {
        return Ok(());
    };
    let trash = shipyard_image::trash_areas(&store, &[area.name().to_owned()])?;
    let deleted = delete(&store, &shared, &Trash::Areas(trash));
    drop(area);
    deleted
}

/// An image ready to run: found or pulled, extracted (on Linux), and
/// recorded.
struct Ready {
    pulled: Pulled,
    /// Its layers' directories, lowest first, and their names in the store.
    #[cfg(target_os = "linux")]
    extracted: (Vec<std::path::PathBuf>, Vec<String>),
}

/// Makes `image` ready, under the store's shared lock `held`: found in the
/// store or pulled (as `policy` says), its layers extracted (Linux: macOS
/// keeps the blobs for the VM), and recorded in the store's index, so
/// `images`, `rmi` and `prune` know it's present and what it depends on.
fn ready(
    engine: &Engine,
    store: &Store,
    held: &StoreLock,
    image: &ImageReference,
    policy: PullPolicy,
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "macOS keeps the blobs: nothing to extract")
    )]
    maps: Option<&shipyard_runtime::isolation::IdMaps>,
) -> Result<Ready> {
    let pulled = obtain(engine, store, image, policy)?;
    #[cfg(target_os = "linux")]
    let extracted = {
        let layers = shipyard_image::layer_specs(&pulled.manifest, &pulled.config)
            .with_context(|| format!("reading the layers of {image}"))?;
        extract(engine, store, held, &layers, maps)
            .with_context(|| format!("extracting the layers of {image}"))?
    };
    #[cfg(not(target_os = "linux"))]
    let _ = held;
    record_image(engine, store, &pulled)
        .with_context(|| format!("recording {image} in the store index"))?;
    Ok(Ready {
        pulled,
        #[cfg(target_os = "linux")]
        extracted,
    })
}

/// The image `image` names: the store's if `policy` lets it be and the
/// store has it whole (no request at all), else pulled. A stored copy that
/// can't be read is pulled again, unless `policy` forbids it.
fn obtain(
    engine: &Engine,
    store: &Store,
    image: &ImageReference,
    policy: PullPolicy,
) -> Result<Pulled> {
    let platform = Platform::host();
    if policy != PullPolicy::Always {
        match shipyard_image::from_store(image, &platform, store) {
            Ok(Some(found)) => return Ok(found),
            Ok(None) => {}
            Err(err) if policy == PullPolicy::Never => {
                return Err(err).with_context(|| format!("reading {image} from the image store"));
            }
            Err(err) => engine.warn(
                WarningKind::StoredImageUnreadable,
                format!("reading {image} from the image store: {err}; pulling it again"),
            ),
        }
    }
    if policy == PullPolicy::Never {
        bail!(
            NotFound,
            "{image} isn't in the image store (pull it first, or leave out --pull=never)"
        );
    }
    // An explicit pull also checks what the store has, and repairs it.
    let options = shipyard_image::PullOptions {
        recheck: policy == PullPolicy::Always,
    };
    download(engine, store, image, &platform, options)
}

/// Pulls `image` into the store, signing in with Docker's credentials if
/// the registry asks, reporting each layer's progress.
fn download(
    engine: &Engine,
    store: &Store,
    image: &ImageReference,
    platform: &Platform,
    options: shipyard_image::PullOptions,
) -> Result<Pulled> {
    engine.report(Event::Progress(Progress::Pulling {
        image: image.to_string(),
    }));
    let progress = |layer: &Descriptor, fetched: bool| {
        engine.report(Event::Progress(Progress::Layer {
            short_digest: layer.digest.hex().chars().take(12).collect(),
            downloaded: fetched.then_some(layer.size),
        }));
    };
    let credentials = shipyard_image::Credentials::docker();
    let pulled = shipyard_image::pull(image, platform, store, credentials, options, &progress)
        .with_context(|| format!("pulling {image}"))?;
    crate::events::emit_about(
        engine,
        crate::events::Kind::Image,
        "pull",
        &image.to_string(),
    );
    Ok(pulled)
}

/// `shipyard pull`: downloads `reference` into the store (and, on Linux,
/// extracts it, so the first run starts at once). Returns its manifest's
/// digest.
///
/// # Errors
///
/// If the image can't be pulled, extracted or recorded.
pub fn pull(engine: &Engine, reference: &str) -> Result<String> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let image = ImageReference::parse(reference)?;
    let ready = ready(engine, &store, &shared, &image, PullPolicy::Always, None)?;
    drop(shared);
    Ok(ready.pulled.manifest_digest.to_string())
}

/// Removes a detached container's writable area `name`, as a foreground
/// run removes its own ([`RunArea`]'s clean-up): under the store's shared
/// lock, while `hold` (its locked directory, as its monitor kept it) still
/// holds it, so no sweep takes it half way; the hold goes after. Removed by
/// its name, whatever else holds it: it's this container's.
///
/// # Errors
///
/// The store can't be opened or locked, or the area couldn't be removed
/// (then it's left for the next run's sweep, or `prune`, once the hold is
/// closed).
#[cfg(target_os = "linux")]
pub(crate) fn release_area(engine: &Engine, name: &str, hold: std::os::fd::OwnedFd) -> Result<()> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    // Only the area held: what's held is told by a label (a container's
    // monitor's), and a label alone never says which directory to delete.
    let held = nix::sys::stat::fstat(&hold).ok();
    let there = store.containers_dir().ok().and_then(|dir| {
        nix::sys::stat::fstatat(&dir, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).ok()
    });
    let same = matches!((held, there), (Some(held), Some(there))
        if held.st_dev == there.st_dev && held.st_ino == there.st_ino);
    if !same {
        crate::error::bail!(
            InvalidRequest,
            "the writable area held isn't {name}: it's left for a later sweep"
        );
    }
    let trash = shipyard_image::trash_areas(&store, &[name.to_owned()])?;
    let deleted = delete(&store, &shared, &Trash::Areas(trash));
    drop(hold);
    deleted
}

/// Removes the areas of runs that ended without removing them (whose lock
/// is free), and the areas of this engine's containers that are gone
/// ([`orphaned`]). Only those: under the shared lock, another process may
/// be making or removing an area of its own meanwhile.
#[cfg(target_os = "linux")]
fn sweep_stale(engine: &Engine, store: &Store, held: &StoreLock) -> Result<()> {
    let areas = shipyard_image::areas(store)?;
    let mine = crate::containers::store_id(engine, false).ok().flatten();
    let stale: Vec<String> = areas
        .into_iter()
        .filter_map(|area| match area {
            shipyard_image::Area::Stale { name } => Some(name),
            shipyard_image::Area::Owned(owned) if orphaned(engine, mine.as_deref(), &owned) => {
                Some(owned.name)
            }
            _ => None,
        })
        .collect();
    if !stale.is_empty() {
        let trash = shipyard_image::trash_areas(store, &stale)?;
        delete(store, held, &Trash::Areas(trash))?;
    }
    Ok(())
}

/// How this caller extracts layers, and removes them.
#[cfg(target_os = "linux")]
enum Extractor {
    /// Root: in this process, with the image's owners, as far as its user
    /// namespace's maps go (all IDs on the host, fewer nested in another
    /// engine's container, where `trusted.*` attributes can't be set).
    Root(IdMaps),
    /// Anyone else with subordinate IDs: in the storage helper's user
    /// namespace (`helper`), mapped with these, with the image's owners
    /// shifted into the caller's range.
    Helper(IdMaps),
    /// No IDs to give files to: in this process, every file the caller's
    /// (root inside the container all the same); why.
    Caller(String),
    /// Root, for a container with `--uidmap`: in this process, the image's
    /// owners shifted by those maps (root gives files to any host ID); a
    /// copy of its own (`LayerOwners::Mapped`).
    Mapped(shipyard_image::Shift),
}

#[cfg(target_os = "linux")]
impl Extractor {
    /// How this caller extracts: with `maps` (`--uidmap`, root only),
    /// shifted by them, in this process.
    fn for_caller(maps: Option<&IdMaps>) -> Result<Self> {
        if nix::unistd::geteuid().is_root() {
            if let Some(maps) = maps {
                return Ok(Self::Mapped(shift(maps)?));
            }
            return Ok(Self::Root(shipyard_runtime::process::current_id_maps()?));
        }
        let rootless = shipyard_runtime::process::rootless_id_maps();
        Ok(match rootless.shortfall {
            Some(why) => Self::Caller(why),
            None => Self::Helper(rootless.maps),
        })
    }

    fn owners(&self) -> LayerOwners {
        match self {
            Self::Root(_) | Self::Helper(_) => LayerOwners::Image,
            Self::Caller(_) => LayerOwners::Caller,
            Self::Mapped(shift) => LayerOwners::Mapped(shift.id()),
        }
    }
}

/// The shift `maps` make (`--uidmap`): refused if they don't map the
/// container's root (ID 0), before anything is extracted.
#[cfg(target_os = "linux")]
fn shift(maps: &IdMaps) -> Result<shipyard_image::Shift> {
    let triples = |maps: &[IdMap]| -> Vec<(u32, u32, u32)> {
        maps.iter()
            .map(|map| (map.container, map.host, map.size))
            .collect()
    };
    shipyard_image::Shift::new(triples(&maps.uids), triples(&maps.gids)).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidRequest,
            "--uidmap and --gidmap must map the container's root (ID 0), for an image",
        )
    })
}

/// Gives `paths` to the host user and group the container's root is under
/// `maps`.
#[cfg(target_os = "linux")]
fn give_to_container_root(paths: &[PathBuf], maps: &IdMaps) -> Result<()> {
    let (Some(uid), Some(gid)) = (maps.host_uid(0), maps.host_gid(0)) else {
        bail!(
            InvalidRequest,
            "--uidmap must map the container's root (ID 0), for an image"
        );
    };
    for path in paths {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))
            .with_context(|| format!("giving {} to the container's root", path.display()))?;
    }
    Ok(())
}

/// Extracts `layers` into the store as this caller can (see [`Extractor`]),
/// holding the store's lock (`held`, shared), and returns their content
/// directories and their directories' names in `layers/` (base first).
#[cfg(target_os = "linux")]
fn extract(
    engine: &Engine,
    store: &Store,
    held: &StoreLock,
    layers: &[LayerSpec],
    maps: Option<&IdMaps>,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    use shipyard_image::{Limits, Ownership, UnpackOptions};
    // The layers' directories and names, if all are extracted (each
    // layer's name depends on its entries: `placed_layers`).
    let placed = |owners: LayerOwners| -> Result<Option<(Vec<PathBuf>, Vec<String>)>> {
        Ok(shipyard_image::placed_layers(store, layers, owners)?
            .map(|names| (shipyard_image::layer_directories(store, &names), names)))
    };
    let extractor = Extractor::for_caller(maps)?;
    if let Some(placed) = placed(extractor.owners())? {
        // All there already: nothing to start.
        return Ok(placed);
    }
    let in_process = |ownership, trusted_xattrs| {
        let options = UnpackOptions {
            ownership,
            limits: Limits::default(),
            trusted_xattrs,
        };
        shipyard_image::extract_layers(store, layers, &options).map(|done| done.report)
    };
    let caller_owned = |why: &str| {
        engine.warn(
            WarningKind::OwnersCollapsed,
            format!("the image's files will all belong to you, not to its users: {why}"),
        );
        in_process(Ownership::Keep, false).map(|report| (report, LayerOwners::Caller))
    };
    let (report, owners) = match extractor {
        Extractor::Root(maps) => {
            let host = maps.uids
                == [IdMap {
                    container: 0,
                    host: 0,
                    size: u32::MAX,
                }];
            let ownership = Ownership::Apply {
                uids: ranges(&maps.uids),
                gids: ranges(&maps.gids),
                shift: None,
            };
            (in_process(ownership, host)?, LayerOwners::Image)
        }
        Extractor::Helper(maps) => {
            match helper(store, held, &maps, &Request::Extract(layers.to_vec()))? {
                Outcome::Done(report) => (report, LayerOwners::Image),
                Outcome::Refused(why) => caller_owned(&why)?,
            }
        }
        Extractor::Mapped(shift) => {
            let id = shift.id();
            let ranges = |maps: &[(u32, u32, u32)]| {
                shipyard_image::IdRanges(
                    maps.iter().map(|&(first, _, size)| (first, size)).collect(),
                )
            };
            let ownership = Ownership::Apply {
                uids: ranges(shift.uids()),
                gids: ranges(shift.gids()),
                shift: Some(shift),
            };
            // `user.overlay.*` marks: the overlay is mounted with `userxattr`.
            (in_process(ownership, false)?, LayerOwners::Mapped(id))
        }
        Extractor::Caller(why) => caller_owned(&why)?,
    };
    for warning in report.warnings() {
        engine.warn(WarningKind::UnpackSkipped, warning);
    }
    placed(owners)?.ok_or_else(|| {
        Error::new(
            ErrorKind::Image,
            "the image's layers aren't in the store after extracting them",
        )
    })
}

/// Runs `request` in the storage helper, on this store, under `held`.
#[cfg(target_os = "linux")]
fn helper(store: &Store, held: &StoreLock, maps: &IdMaps, request: &Request) -> Result<Outcome> {
    let dir = shipyard_image::fsat::open_dir(store.root()).context("opening the image store")?;
    helper::run(&dir, held, maps, request).map_err(|error| error.io_as(ErrorKind::Storage))
}

/// Trash to delete: names moved aside in `layers/` or `containers/`.
enum Trash {
    Layers(Vec<String>),
    Areas(Vec<String>),
}

/// Deletes trash, holding the store's lock (`held`). In this process first:
/// most files are the caller's. What's left (files owned by subordinate IDs:
/// an image's other users, what a rootless container wrote as them) goes
/// through the storage helper, where this caller's files are made through
/// it.
fn delete(store: &Store, held: &StoreLock, trash: &Trash) -> Result<()> {
    let in_process = match trash {
        Trash::Layers(names) if !names.is_empty() => {
            shipyard_image::remove_layers(store, names).map_err(Error::from)
        }
        Trash::Areas(names) if !names.is_empty() => {
            shipyard_image::remove_areas(store, names).map_err(Error::from)
        }
        _ => return Ok(()),
    };
    #[cfg(target_os = "linux")]
    if let Err(err) = &in_process {
        return through_helper(store, held, err, || match trash {
            Trash::Layers(names) => Request::RemoveLayers(names.clone()),
            Trash::Areas(names) => Request::RemoveAreas(names.clone()),
        });
    }
    let _ = held;
    in_process
}

/// What deleting in this process left (`err`), deleted through the storage
/// helper, if the caller is rootless: the files are an image's other users'
/// (or what a container wrote as them). As root, `err` stands.
#[cfg(target_os = "linux")]
pub(crate) fn through_helper(
    store: &Store,
    held: &StoreLock,
    err: &Error,
    request: impl FnOnce() -> Request,
) -> Result<()> {
    let Extractor::Helper(maps) = Extractor::for_caller(None)? else {
        return Err(Error::new(err.kind(), format!("{err:#}")));
    };
    tracing::debug!("deleting through the storage helper: {err:#}");
    match helper(store, held, &maps, &request())? {
        Outcome::Done(_) => Ok(()),
        Outcome::Refused(why) => Err(Error::new(
            err.kind(),
            format!("{err:#} (and no user namespace to delete the rest from: {why})"),
        )),
    }
}

/// Records a pulled image in the store's reference index.
fn record_image(engine: &Engine, store: &Store, pulled: &Pulled) -> Result<()> {
    let record = shipyard_image::ImageRecord {
        manifest: pulled.manifest_digest.to_string(),
        config: pulled.manifest.config.digest.to_string(),
        layers: pulled
            .manifest
            .layers
            .iter()
            .map(|layer| layer.digest.to_string())
            .collect(),
    };
    if let Some(aside) = Index::record(store.root(), &pulled.reference.to_string(), record)? {
        engine.warn(
            WarningKind::IndexReset,
            format!(
                "the store index was corrupt; it was moved to {} and a new one started (images \
                 pulled before aren't listed, and nothing is removed until it's deleted)",
                aside.display()
            ),
        );
    }
    Ok(())
}

/// Lists the images in the store (for `shipyard images`).
///
/// # Errors
///
/// Returns an error if the store index can't be read.
pub fn list(engine: &Engine) -> Result<Vec<shipyard_image::ListedImage>> {
    let index = Index::open(open_store(engine)?.root())?;
    Ok(index.images())
}

/// Removes an image reference and garbage-collects what nothing needs any
/// more (for `shipyard rmi`). An image a container was made from (running
/// or not: its pin), or a foreground run uses, is refused, unless `force`
/// (or another reference keeps the same image): then it's untagged; a
/// container's pin keeps what it needs, a run's layers stay until a later
/// prune.
/// Returns the blob digests removed.
///
/// # Errors
///
/// Returns an error if no image matches ([`crate::ErrorKind::NotFound`]),
/// it's in use ([`crate::ErrorKind::InUse`]), the store is busy
/// ([`crate::ErrorKind::Busy`]), or it can't be updated.
pub fn remove(engine: &Engine, reference: &str, force: bool) -> Result<Vec<String>> {
    // The reference untagged, and its image (its manifest's digest).
    let mut untagged = (String::new(), None);
    let removed = collect(engine, |index, held, live, in_use, remove_layers| {
        let key = index_key(index, reference);
        let reference = key.as_str();
        let only_reference = |manifest: &str| {
            !index
                .images()
                .iter()
                .any(|other| other.reference != reference && other.record.manifest == manifest)
        };
        if !force && let Some(record) = index.get(reference) {
            // A container made from it (running or not) pins it.
            let pinned = index.pinned_by(&record.manifest);
            if !pinned.is_empty() && only_reference(&record.manifest) {
                let named: Vec<String> = pinned
                    .iter()
                    .map(|id| {
                        id.chars()
                            .take(crate::containers::ContainerId::SHORT)
                            .collect()
                    })
                    .collect();
                bail!(
                    InUse,
                    "the image {reference} is used by container {} (--force untags it anyway: \
                     the containers keep what they need)",
                    named.join(", ")
                );
            }
            if live.iter().any(|area| area.image == record.manifest)
                && only_reference(&record.manifest)
            {
                bail!(
                    InUse,
                    "the image {reference} is in use by a running container (--force untags it \
                     anyway)"
                );
            }
        }
        let manifest = index.get(reference).map(|record| record.manifest.clone());
        let removed = index.remove(reference, held, in_use, remove_layers)?;
        untagged = (reference.to_owned(), manifest);
        Ok(removed)
    })?;
    let (reference, manifest) = untagged;
    crate::events::emit_about(engine, crate::events::Kind::Image, "untag", &reference);
    // Deleted: the image itself, not only its name (no other reference
    // keeps it). Its layers and config aren't images.
    if let Some(manifest) = manifest.filter(|manifest| removed.contains(manifest)) {
        crate::events::emit_about(engine, crate::events::Kind::Image, "delete", &manifest);
    }
    Ok(removed)
}

/// Removes every blob and layer nothing needs, and what interrupted work
/// left (for `shipyard image prune`).
///
/// # Errors
///
/// Returns an error if the store is busy or can't be updated.
pub fn prune(engine: &Engine) -> Result<Vec<String>> {
    let removed = collect(engine, |index, held, _, in_use, remove_layers| {
        Ok(index.prune(held, in_use, remove_layers)?)
    })?;
    crate::events::emit_pruned(engine, crate::events::Kind::Image, removed.len());
    Ok(removed)
}

/// A garbage collection: under the store's lock, exclusive, it sweeps the
/// areas (stale ones, leftovers, and gone containers' own to trash; the
/// live ones' layers and containers' own are kept) and lets `decide`
/// update the index (given the running areas, and every layer an area
/// stacks), its layers moved to trash; then, the lock shared again,
/// deletes the trash.
fn collect(
    engine: &Engine,
    decide: impl FnOnce(
        &mut Index,
        &StoreLock,
        &[LiveArea],
        &BTreeSet<String>,
        &mut shipyard_image::index::LayerRemover<'_>,
    ) -> Result<Vec<String>>,
) -> Result<Vec<String>> {
    let store = open_store(engine)?;
    let mut lock = store.lock(LockMode::Exclusive, GC_WAIT)?;
    let mine = crate::containers::store_id(engine, false).ok().flatten();
    let mut live = Vec::new();
    let mut owned = Vec::new();
    let mut gone = Vec::new();
    for area in shipyard_image::areas(&store)? {
        match area {
            shipyard_image::Area::Live(area) => live.push(area),
            shipyard_image::Area::Owned(area) if orphaned(engine, mine.as_deref(), &area) => {
                gone.push(area.name);
            }
            shipyard_image::Area::Owned(area) => owned.push(area),
            shipyard_image::Area::Stale { name } | shipyard_image::Area::Leftover { name } => {
                gone.push(name);
            }
        }
    }
    let in_use = in_use(&live, &owned);
    // This store's pins whose containers are gone (a removal cut short):
    // what only they kept is collected.
    if let Some(mine) = &mine {
        for (owner, id) in Index::open(store.root())?.pins() {
            if &owner == mine && !crate::containers::exists(engine, &id) {
                Index::unpin(store.root(), &owner, &id)?;
            }
        }
    }
    // One area that can't be moved stops nothing else: it's left for later.
    let area_trash = shipyard_image::trash_areas(&store, &gone).unwrap_or_else(|err| {
        engine.warn(
            WarningKind::LeftForLater,
            format!("couldn't remove stale container layers: {err}"),
        );
        Vec::new()
    });
    let mut index = Index::open(store.root())?;
    let mut layer_trash = Vec::new();
    let removed = decide(&mut index, &lock, &live, &in_use, &mut |names| {
        layer_trash.extend(shipyard_image::trash_layers(&store, names)?);
        Ok(())
    })?;

    // The slow part, while runs carry on.
    lock.downgrade().context("sharing the image store's lock")?;
    for trash in [Trash::Layers(layer_trash), Trash::Areas(area_trash)] {
        if let Err(err) = delete(&store, &lock, &trash) {
            engine.warn(
                WarningKind::LeftForLater,
                format!("couldn't delete everything removed ({err:#}); the next prune will"),
            );
        }
    }
    Ok(removed)
}

/// The index's name for what the user typed: its canonical form
/// (`alpine` is `docker.io/library/alpine:latest`) if the index has that,
/// else what they typed (as the index may name it, or for the error that
/// names it).
fn index_key(index: &Index, typed: &str) -> String {
    ImageReference::parse(typed)
        .map(|reference| reference.to_string())
        .ok()
        .filter(|canonical| index.get(canonical).is_some())
        .unwrap_or_else(|| typed.to_owned())
}

/// Whether a container's own area is left by a container that's gone: it
/// belongs to this engine's container store (`mine`: an area of another
/// store sharing this image store is never this one's to judge), its
/// container has no directory there (decided by the directory, never by a
/// listing that skips a record it can't read), and nothing holds it.
fn orphaned(engine: &Engine, mine: Option<&str>, area: &shipyard_image::OwnedArea) -> bool {
    !area.held
        && mine.is_some_and(|mine| area.owner.as_deref() == Some(mine))
        && !crate::containers::exists(engine, &area.container)
}

/// The layer directories live areas, and containers' own (running or not),
/// stack.
fn in_use(live: &[LiveArea], owned: &[shipyard_image::OwnedArea]) -> BTreeSet<String> {
    live.iter()
        .flat_map(|area| area.layers.iter().cloned())
        .chain(owned.iter().flat_map(|area| area.layers.iter().cloned()))
        .collect()
}

/// How much the store's images and containers take (for `shipyard system
/// df`).
#[derive(Debug)]
pub struct StoreUsage {
    /// How many images the store has.
    pub images: u64,
    /// Their blobs and extracted layers.
    pub disk: shipyard_image::DiskUsage,
    /// The containers' writable layers: running ones (`count`), and kept
    /// ones of stopped containers (in `bytes`, not reclaimable: `rm`
    /// reclaims them, not a prune).
    pub containers: Usage,
}

/// What a kind of thing in the store takes.
#[derive(Debug, Default)]
pub struct Usage {
    /// How many there are (in use, for containers).
    pub count: u64,
    /// Their bytes.
    pub bytes: u64,
    /// The bytes nothing uses now.
    pub reclaimable: u64,
    /// Whether all of it could be read: `false`, it takes more.
    pub complete: bool,
}

/// The store's disk usage (for `shipyard system df`).
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub fn disk_usage(engine: &Engine) -> Result<StoreUsage> {
    let store = open_store(engine)?;
    let areas = shipyard_image::areas(&store)?;
    let in_use: BTreeSet<String> = areas
        .iter()
        .filter_map(|area| match area {
            shipyard_image::Area::Live(live) => Some(live.layers.iter().cloned()),
            shipyard_image::Area::Owned(owned) => Some(owned.layers.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect();
    let index = Index::open(store.root())?;
    let disk = index.disk_usage(&in_use)?;
    let dir = store
        .containers_dir()
        .context("opening the containers' layers")?;
    let mut containers = Usage {
        complete: true,
        ..Usage::default()
    };
    for area in areas {
        // Running, kept (a stopped container's, which `rm` reclaims, not a
        // prune), or nobody's.
        let (name, running, kept) = match area {
            shipyard_image::Area::Live(live) => (live.name, true, true),
            shipyard_image::Area::Owned(owned) => (owned.name, owned.held, true),
            shipyard_image::Area::Stale { name } | shipyard_image::Area::Leftover { name } => {
                (name, false, false)
            }
        };
        let taken = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(&name));
        containers.complete &= taken.complete;
        containers.bytes += taken.bytes;
        if running {
            containers.count += 1;
        }
        if !kept {
            containers.reclaimable += taken.bytes;
        }
    }
    Ok(StoreUsage {
        images: u64::try_from(index.images().len()).unwrap_or(u64::MAX),
        disk,
        containers,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::testing::engine_on_a_new_store;

    #[test]
    fn what_the_user_types_finds_the_canonical_name() {
        let root = std::env::temp_dir().join(format!("shipyard-key-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let record = || shipyard_image::ImageRecord {
            manifest: "sha256:aa".to_owned(),
            config: "sha256:bb".to_owned(),
            layers: Vec::new(),
        };
        let canonical = "docker.io/library/alpine:latest";
        Index::record(&root, canonical, record()).unwrap();
        Index::record(&root, "Not A Reference", record()).unwrap();
        let index = Index::open(&root).unwrap();
        for typed in ["alpine", "alpine:latest", "docker.io/alpine", canonical] {
            assert_eq!(index_key(&index, typed), canonical, "{typed}");
        }
        assert_eq!(index_key(&index, "Not A Reference"), "Not A Reference");
        assert_eq!(
            index_key(&index, "busybox"),
            "busybox",
            "not there: as typed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn errors_have_kinds() {
        let (engine, _, parent) = engine_on_a_new_store("kinds");
        let missing = remove(&engine, "alpine", false).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound, "{missing:#}");
        let bad = pull(&engine, "").unwrap_err();
        assert_eq!(bad.kind(), ErrorKind::InvalidRequest, "{bad:#}");
        assert!(list(&engine).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn never_pulling_an_image_the_store_lacks_is_not_found() {
        let (engine, collect, parent) = engine_on_a_new_store("never");
        let Err(error) = prepare(&engine, "alpine", PullPolicy::Never, None) else {
            panic!("an empty store has no alpine");
        };
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error:#}");
        assert!(
            collect.events().is_empty(),
            "nothing pulled: {:?}",
            collect.events()
        );
        let _ = std::fs::remove_dir_all(&parent);
    }
}
