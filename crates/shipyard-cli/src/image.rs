//! Running a container from an image: the thin engine glue between
//! `shipyard-image` (pull, store, unpack) and the runtime.
//!
//! `prepare` turns an image reference into the pieces a run needs: it pulls
//! the image if the store doesn't have it, extracts its layers into
//! directories, and makes the run's own writable area (`containers/<name>`,
//! [`shipyard_image::ContainerArea`]), held for the run's whole life and
//! removed when it ends. It returns an `Overlay` (the layer directories) and
//! the image's runtime config. (It and its types are Linux-only, since only
//! Linux mounts the overlay.)
//!
//! Processes share the store through its lock (`shipyard_image::Store::lock`):
//! `prepare` holds it shared, so garbage collection can't take what it's
//! adding. `rmi` and `prune` hold it exclusive only while they decide and
//! move things aside to trash (quick renames, keeping what live areas use),
//! then shared while they delete the trash, so runs can start meanwhile.
//!
//! The overlay is **not** mounted here. The runtime mounts it inside the
//! container's own user and mount namespaces (`Root::overlay`), where the
//! kernel permits an unprivileged (rootless) overlay mount — mounting it in
//! the engine beforehand would fail rootless with `EPERM`. So this module
//! only prepares directories; the mount is the init's job.
//!
//! There's no engine crate yet (stage 9 introduces one around on-disk state),
//! so this orchestration lives in the CLI.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
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
use crate::storage_helper::{self, Outcome, Request};

/// When a run pulls its image (`--pull`), as Docker's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PullPolicy {
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
pub(crate) struct Prepared {
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
/// has ended, before the CLI exits: dying from the container's signal runs
/// no destructors. Dropping it removes it too, for an error on the way. (A
/// run killed outright leaves it unlocked: the next run, or `prune`, sweeps
/// it.)
#[cfg(target_os = "linux")]
pub(crate) struct RunArea {
    store: Store,
    area: Option<shipyard_image::ContainerArea>,
}

#[cfg(target_os = "linux")]
impl RunArea {
    /// Removes the area, now the container has ended.
    pub(crate) fn remove(mut self) {
        self.clean_up();
    }

    /// Removes the area while still holding its lock, under the store's
    /// shared lock (so a garbage collection can't take it half-way). A
    /// failure is a warning: the area is left for a later sweep.
    fn clean_up(&mut self) {
        let Some(area) = self.area.take() else {
            return;
        };
        let removed = self
            .store
            .lock(LockMode::Shared, USE_WAIT)
            .map_err(anyhow::Error::from)
            .and_then(|shared| {
                let trash = shipyard_image::trash_areas(&self.store, &[area.name().to_owned()])?;
                delete(&self.store, &shared, &Trash::Areas(trash))
            });
        if let Err(err) = removed {
            crate::warning(&format!(
                "couldn't remove the container's writable layer ({}): {err:#}; the next run \
                 or `shipyard image prune` will",
                area.name()
            ));
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for RunArea {
    fn drop(&mut self) {
        self.clean_up();
    }
}

/// The store directory. An explicit `SHIPYARD_IMAGE_STORE` wins (useful where
/// the default is on an overlay filesystem, which can't hold an overlay
/// upperdir); otherwise `/var/lib/shipyard/images` for root, or
/// `$XDG_DATA_HOME`/`~/.local/share/shipyard/images` for anyone else — the
/// location stage 9 formalises. Shared with the volume store.
pub(crate) fn store_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SHIPYARD_IMAGE_STORE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if nix::unistd::geteuid().is_root() {
        return Ok(PathBuf::from("/var/lib/shipyard/images"));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(data).join("shipyard/images"));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .context("neither XDG_DATA_HOME nor HOME is set, so there's nowhere to keep images")?;
    Ok(PathBuf::from(home).join(".local/share/shipyard/images"))
}

/// Opens the store, made private if it's new. Every command that touches
/// the store directory (volumes too) opens it this way first, so nothing
/// makes its root with looser permissions.
///
/// # Errors
///
/// If the store can't be made, or it's reachable by other users.
pub(crate) fn open_store() -> Result<Store> {
    Store::open(store_dir()?).context("opening the image store")
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
pub(crate) fn prepare(
    reference: &str,
    policy: PullPolicy,
    maps: Option<&IdMaps>,
) -> Result<Prepared> {
    let store = open_store()?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    // Areas runs killed outright left (best effort: `prune` tries again).
    if let Err(err) = sweep_stale(&store, &shared) {
        tracing::debug!("couldn't sweep stale container layers: {err:#}");
    }
    let image = ImageReference::parse(reference)?;
    let Ready {
        pulled,
        extracted: (directories, names),
    } = ready(&store, &shared, &image, policy, false, maps)?;

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
        },
    })
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
    store: &Store,
    held: &StoreLock,
    image: &ImageReference,
    policy: PullPolicy,
    quiet: bool,
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "macOS keeps the blobs: nothing to extract")
    )]
    maps: Option<&shipyard_runtime::isolation::IdMaps>,
) -> Result<Ready> {
    let pulled = obtain(store, image, policy, quiet)?;
    #[cfg(target_os = "linux")]
    let extracted = {
        let layers = shipyard_image::layer_specs(&pulled.manifest, &pulled.config)
            .with_context(|| format!("reading the layers of {image}"))?;
        extract(store, held, &layers, maps)
            .with_context(|| format!("extracting the layers of {image}"))?
    };
    #[cfg(not(target_os = "linux"))]
    let _ = held;
    record_image(store, &pulled)
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
    store: &Store,
    image: &ImageReference,
    policy: PullPolicy,
    quiet: bool,
) -> Result<Pulled> {
    let platform = Platform::host();
    if policy != PullPolicy::Always {
        match shipyard_image::from_store(image, &platform, store) {
            Ok(Some(found)) => return Ok(found),
            Ok(None) => {}
            Err(err) if policy == PullPolicy::Never => {
                return Err(err).with_context(|| format!("reading {image} from the image store"));
            }
            Err(err) => crate::warning(&format!(
                "reading {image} from the image store: {err}; pulling it again"
            )),
        }
    }
    if policy == PullPolicy::Never {
        anyhow::bail!(
            "{image} isn't in the image store (pull it first, or leave out --pull=never)"
        );
    }
    // An explicit pull also checks what the store has, and repairs it.
    let options = shipyard_image::PullOptions {
        recheck: policy == PullPolicy::Always,
    };
    download(store, image, &platform, options, quiet)
}

/// Pulls `image` into the store, signing in with Docker's credentials if
/// the registry asks, telling stderr of each layer unless `quiet`.
fn download(
    store: &Store,
    image: &ImageReference,
    platform: &Platform,
    options: shipyard_image::PullOptions,
    quiet: bool,
) -> Result<Pulled> {
    use std::io::Write as _;
    if !quiet {
        let _ = writeln!(std::io::stderr().lock(), "Pulling {image}");
    }
    let progress = |layer: &Descriptor, fetched: bool| {
        if !quiet {
            let short: String = layer.digest.hex().chars().take(12).collect();
            let what = if fetched {
                format!("downloaded ({})", crate::human_bytes(layer.size))
            } else {
                "already here".to_owned()
            };
            // Not `eprintln!`: it panics if stderr is a closed pipe.
            let _ = writeln!(std::io::stderr().lock(), "{short}: {what}");
        }
    };
    let credentials = shipyard_image::Credentials::docker();
    shipyard_image::pull(image, platform, store, credentials, options, &progress)
        .with_context(|| format!("pulling {image}"))
}

/// `shipyard pull`: downloads `reference` into the store (and, on Linux,
/// extracts it, so the first run starts at once).
///
/// # Errors
///
/// If the image can't be pulled, extracted or recorded.
pub(crate) fn pull_command(reference: &str, quiet: bool) -> Result<String> {
    let store = open_store()?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let image = ImageReference::parse(reference)?;
    let ready = ready(&store, &shared, &image, PullPolicy::Always, quiet, None)?;
    drop(shared);
    Ok(ready.pulled.manifest_digest.to_string())
}

/// Removes the areas of runs that ended without removing them (whose lock
/// is free). Only those: under the shared lock, another process may be
/// making or removing an area of its own meanwhile.
#[cfg(target_os = "linux")]
fn sweep_stale(store: &Store, held: &StoreLock) -> Result<()> {
    let stale: Vec<String> = shipyard_image::areas(store)?
        .into_iter()
        .filter_map(|area| match area {
            shipyard_image::Area::Stale { name } => Some(name),
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
    /// namespace (`storage_helper`), mapped with these, with the image's
    /// owners shifted into the caller's range.
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
        anyhow::anyhow!("--uidmap and --gidmap must map the container's root (ID 0), for an image")
    })
}

/// Gives `paths` to the host user and group the container's root is under
/// `maps`.
#[cfg(target_os = "linux")]
fn give_to_container_root(paths: &[PathBuf], maps: &IdMaps) -> Result<()> {
    let (Some(uid), Some(gid)) = (maps.host_uid(0), maps.host_gid(0)) else {
        anyhow::bail!("--uidmap must map the container's root (ID 0), for an image");
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
    store: &Store,
    held: &StoreLock,
    layers: &[LayerSpec],
    maps: Option<&IdMaps>,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    use shipyard_image::{Limits, Ownership, UnpackOptions};
    let named = |owners: LayerOwners| {
        let names = layers
            .iter()
            .map(|layer| owners.name(&layer.digest))
            .collect();
        (
            shipyard_image::layer_directories(store, layers, owners),
            names,
        )
    };
    let extractor = Extractor::for_caller(maps)?;
    let wanted = named(extractor.owners());
    if wanted.0.iter().all(|dir| dir.is_dir()) {
        // All there already: nothing to start.
        return Ok(wanted);
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
        crate::warning(&format!(
            "the image's files will all belong to you, not to its users: {why}"
        ));
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
                uids: storage_helper::ranges(&maps.uids),
                gids: storage_helper::ranges(&maps.gids),
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
        crate::warning(&warning);
    }
    Ok(named(owners))
}

/// Runs `request` in the storage helper, on this store, under `held`.
#[cfg(target_os = "linux")]
fn helper(store: &Store, held: &StoreLock, maps: &IdMaps, request: &Request) -> Result<Outcome> {
    let dir = shipyard_image::fsat::open_dir(store.root()).context("opening the image store")?;
    storage_helper::run(&dir, held, maps, request)
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
            shipyard_image::remove_layers(store, names).map_err(anyhow::Error::from)
        }
        Trash::Areas(names) if !names.is_empty() => {
            shipyard_image::remove_areas(store, names).map_err(anyhow::Error::from)
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
    err: &anyhow::Error,
    request: impl FnOnce() -> Request,
) -> Result<()> {
    let Extractor::Helper(maps) = Extractor::for_caller(None)? else {
        return Err(anyhow::anyhow!("{err:#}"));
    };
    tracing::debug!("deleting through the storage helper: {err:#}");
    match helper(store, held, &maps, &request())? {
        Outcome::Done(_) => Ok(()),
        Outcome::Refused(why) => Err(anyhow::anyhow!(
            "{err:#} (and no user namespace to delete the rest from: {why})"
        )),
    }
}

/// Records a pulled image in the store's reference index.
fn record_image(store: &Store, pulled: &Pulled) -> Result<()> {
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
        crate::warning(&format!(
            "the store index was corrupt; it was moved to {} and a new one started (images \
             pulled before aren't listed, and nothing is removed until it's deleted)",
            aside.display()
        ));
    }
    Ok(())
}

/// Lists the images in the store (for `shipyard images`).
///
/// # Errors
///
/// Returns an error if the store index can't be read.
pub(crate) fn list_images() -> Result<Vec<shipyard_image::ListedImage>> {
    let index = Index::open(open_store()?.root())?;
    Ok(index.images())
}

/// Removes an image reference and garbage-collects what nothing needs any
/// more (for `shipyard rmi`). An image a running container uses is refused,
/// unless `force` (or another reference keeps the same image): then it's
/// untagged, and the layers the container stacks stay until a later prune.
/// Returns the blob digests removed.
///
/// # Errors
///
/// Returns an error if no image matches, it's in use, the store is busy, or
/// it can't be updated.
pub(crate) fn remove_image(reference: &str, force: bool) -> Result<Vec<String>> {
    collect(|index, held, live, remove_layers| {
        let key = index_key(index, reference);
        let reference = key.as_str();
        if !force
            && let Some(record) = index.get(reference)
            && live.iter().any(|area| area.image == record.manifest)
            && !index.images().iter().any(|other| {
                other.reference != reference && other.record.manifest == record.manifest
            })
        {
            anyhow::bail!(
                "the image {reference} is in use by a running container (--force untags it anyway)"
            );
        }
        Ok(index.remove(reference, held, &in_use(live), remove_layers)?)
    })
}

/// Removes every blob and layer nothing needs, and what interrupted work
/// left (for `shipyard image prune`).
///
/// # Errors
///
/// Returns an error if the store is busy or can't be updated.
pub(crate) fn prune_images() -> Result<Vec<String>> {
    collect(|index, held, live, remove_layers| {
        Ok(index.prune(held, &in_use(live), remove_layers)?)
    })
}

/// A garbage collection: under the store's lock, exclusive, it sweeps the
/// areas (stale ones and leftovers to trash; the live ones' layers are
/// kept) and lets `decide` update the index, its layers moved to trash;
/// then, the lock shared again, deletes the trash.
fn collect(
    decide: impl FnOnce(
        &mut Index,
        &StoreLock,
        &[LiveArea],
        &mut shipyard_image::index::LayerRemover<'_>,
    ) -> Result<Vec<String>>,
) -> Result<Vec<String>> {
    let store = open_store()?;
    let mut lock = store.lock(LockMode::Exclusive, GC_WAIT)?;
    let mut live = Vec::new();
    let mut gone = Vec::new();
    for area in shipyard_image::areas(&store)? {
        match area {
            shipyard_image::Area::Live(area) => live.push(area),
            shipyard_image::Area::Stale { name } | shipyard_image::Area::Leftover { name } => {
                gone.push(name);
            }
        }
    }
    // One area that can't be moved stops nothing else: it's left for later.
    let area_trash = shipyard_image::trash_areas(&store, &gone).unwrap_or_else(|err| {
        crate::warning(&format!("couldn't remove stale container layers: {err}"));
        Vec::new()
    });
    let mut index = Index::open(store.root())?;
    let mut layer_trash = Vec::new();
    let removed = decide(&mut index, &lock, &live, &mut |names| {
        layer_trash.extend(shipyard_image::trash_layers(&store, names)?);
        Ok(())
    })?;

    // The slow part, while runs carry on.
    lock.downgrade().context("sharing the image store's lock")?;
    for trash in [Trash::Layers(layer_trash), Trash::Areas(area_trash)] {
        if let Err(err) = delete(&store, &lock, &trash) {
            crate::warning(&format!(
                "couldn't delete everything removed ({err:#}); the next prune will"
            ));
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

/// The layer directories live areas stack.
fn in_use(live: &[LiveArea]) -> BTreeSet<String> {
    live.iter()
        .flat_map(|area| area.layers.iter().cloned())
        .collect()
}

/// How much the store's images and containers take (for `shipyard system
/// df`).
pub(crate) struct StoreUsage {
    /// How many images the store has.
    pub images: u64,
    /// Their blobs and extracted layers.
    pub disk: shipyard_image::DiskUsage,
    /// The running containers' writable layers.
    pub containers: Usage,
}

/// What a kind of thing in the store takes.
#[derive(Debug, Default)]
pub(crate) struct Usage {
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
pub(crate) fn disk_usage() -> Result<StoreUsage> {
    let store = open_store()?;
    let areas = shipyard_image::areas(&store)?;
    let in_use: BTreeSet<String> = areas
        .iter()
        .filter_map(|area| match area {
            shipyard_image::Area::Live(live) => Some(live.layers.iter().cloned()),
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
        let (name, running) = match area {
            shipyard_image::Area::Live(live) => (live.name, true),
            shipyard_image::Area::Stale { name } | shipyard_image::Area::Leftover { name } => {
                (name, false)
            }
        };
        let taken = shipyard_image::fsat::tree_usage(&dir, std::ffi::OsStr::new(&name));
        containers.complete &= taken.complete;
        containers.bytes += taken.bytes;
        if running {
            containers.count += 1;
        } else {
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
}
