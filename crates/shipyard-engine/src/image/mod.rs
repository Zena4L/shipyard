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
    Descriptor, GC_WAIT, LiveArea, LockMode, Platform, Pulled, Store, StoreLock, USE_WAIT,
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
/// A registry credential, as a client hands one in ([`GivenCredential`]).
pub use shipyard_image::Credential;
/// An image reference (`alpine:3.20`), as the API parses and shows one.
pub use shipyard_image::{ImageReference, ReferenceError};

use crate::Engine;
#[cfg(target_os = "linux")]
use crate::error::ErrorKind;
use crate::error::{Context as _, Error, Result, bail};
#[cfg(target_os = "linux")]
use crate::report::Reporter;
use crate::report::{Message, Progress, WarningKind};

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
pub(crate) struct RunArea {
    store: Store,
    area: Option<shipyard_image::ContainerArea>,
    /// A container's own: unlocked, never removed, as it goes.
    owned: bool,
    reporter: Arc<dyn Reporter>,
}

#[cfg(target_os = "linux")]
impl RunArea {
    /// Removes the area, now the container has ended.
    pub(crate) fn remove(mut self) {
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
pub(crate) fn open_store(engine: &Engine) -> Result<Store> {
    Store::open(engine.paths().image_store()?).context("opening the image store")
}

/// The runtime config (its command, variables, ports…) of the image a
/// container was made from: `reference` as it was given, pinned to its
/// manifest digest `image_id`, read from `store` as a start reads it (no
/// tag looked up: one moved or removed since doesn't change it). `None` if
/// it's gone or can't be read: a view shows what it can without it.
pub(crate) fn runtime_config(
    store: &Store,
    reference: &str,
    image_id: &str,
) -> Option<shipyard_image::RuntimeConfig> {
    let mut image = ImageReference::parse(reference).ok()?;
    image.digest = Some(shipyard_image::Digest::parse(image_id).ok()?);
    let pulled = shipyard_image::from_store(&image, &Platform::host(), store).ok()??;
    Some(pulled.config.config)
}

/// Images' runtime configs, each read once ([`runtime_config`]): for a
/// listing whose containers share images. The store is opened at the
/// first container with an image (a listing never makes it).
pub(crate) struct ImageConfigs<'a> {
    engine: &'a Engine,
    /// Opened (or found missing) at the first container with an image.
    store: std::cell::OnceCell<Option<Store>>,
    read: std::collections::HashMap<String, Option<shipyard_image::RuntimeConfig>>,
}

impl<'a> ImageConfigs<'a> {
    /// None read yet, from `engine`'s store.
    pub(crate) fn new(engine: &'a Engine) -> Self {
        Self {
            engine,
            store: std::cell::OnceCell::new(),
            read: std::collections::HashMap::new(),
        }
    }

    /// The runtime config of the image `reference` (as given) pinned to
    /// `image_id`, if any.
    pub(crate) fn get(
        &mut self,
        reference: Option<&str>,
        image_id: Option<&str>,
    ) -> Option<&shipyard_image::RuntimeConfig> {
        let (reference, image_id) = (reference?, image_id?);
        let engine = self.engine;
        let store = self
            .store
            .get_or_init(|| {
                // Only one that's there: a view never makes the store.
                let root = engine.paths().image_store().ok()?;
                root.exists().then(|| open_store(engine).ok()).flatten()
            })
            .as_ref()?;
        self.read
            .entry(image_id.to_owned())
            .or_insert_with(|| runtime_config(store, reference, image_id))
            .as_ref()
    }
}

/// How many images (manifests, however many names each has) the store
/// has: none if there's no store (a look never makes one).
///
/// # Errors
///
/// The store or its index can't be read.
pub fn count_images(engine: &Engine) -> Result<usize> {
    if !engine
        .paths()
        .image_store()
        .is_ok_and(std::path::Path::exists)
    {
        return Ok(0);
    }
    let store = open_store(engine)?;
    let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let index = Index::open(store.root())?;
    let manifests: BTreeSet<String> = index
        .images()
        .into_iter()
        .map(|image| image.record.manifest)
        .collect();
    Ok(manifests.len())
}

/// The manifest digests `name` (a reference or an ID) names in the store,
/// for `ps -f ancestor=`: none if it names none, if it can't be read, or if
/// there's no store (a listing never makes one).
pub(crate) fn manifests_named(engine: &Engine, name: &str) -> Vec<String> {
    let found = || -> Result<Vec<String>> {
        let store = open_store(engine)?;
        let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
        let index = Index::open(store.root())?;
        let (images, _) = named(&index, name)?;
        Ok(images
            .into_iter()
            .map(|image| image.record.manifest)
            .collect())
    };
    let there = engine
        .paths()
        .image_store()
        .is_ok_and(std::path::Path::exists);
    if !there {
        return Vec::new();
    }
    found().unwrap_or_else(|error| {
        tracing::debug!("ancestor {name}: {error:#}");
        Vec::new()
    })
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
    } = ready(engine, &store, &shared, &image, Fetch::docker(policy), maps)?;

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
    let ready = ready(engine, &store, &shared, &image, Fetch::docker(policy), maps)?;
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
    // A pin keeps the image's blobs: its index too, if it has one.
    let record = shipyard_image::ImageRecord::of(&pulled, None);
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

/// How an image is fetched: when it's pulled, and with whose credentials.
struct Fetch {
    policy: PullPolicy,
    credentials: shipyard_image::Credentials,
}

impl Fetch {
    /// As `policy` says, with Docker's credentials (its config file).
    fn docker(policy: PullPolicy) -> Self {
        Self {
            policy,
            credentials: shipyard_image::Credentials::docker(),
        }
    }
}

/// Makes `image` ready, under the store's shared lock `held`: found in the
/// store or pulled (as `fetch` says), its layers extracted (Linux: macOS
/// keeps the blobs for the VM), and recorded in the store's index, so
/// `images`, `rmi` and `prune` know it's present and what it depends on.
fn ready(
    engine: &Engine,
    store: &Store,
    held: &StoreLock,
    image: &ImageReference,
    fetch: Fetch,
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "macOS keeps the blobs: nothing to extract")
    )]
    maps: Option<&shipyard_runtime::isolation::IdMaps>,
) -> Result<Ready> {
    let pulled = obtain(engine, store, image, fetch)?;
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

/// The image `image` names: the store's if `fetch`'s policy lets it be
/// and the store has it whole (no request at all), else pulled. A stored
/// copy that can't be read is pulled again, unless the policy forbids it.
fn obtain(engine: &Engine, store: &Store, image: &ImageReference, fetch: Fetch) -> Result<Pulled> {
    let Fetch {
        policy,
        credentials,
    } = fetch;
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
    download(engine, store, image, &platform, options, credentials)
}

/// Pulls `image` into the store, signing in with `credentials` if the
/// registry asks, reporting each layer's progress.
fn download(
    engine: &Engine,
    store: &Store,
    image: &ImageReference,
    platform: &Platform,
    options: shipyard_image::PullOptions,
    credentials: shipyard_image::Credentials,
) -> Result<Pulled> {
    engine.report(Message::Progress(Progress::Pulling {
        image: image.to_string(),
    }));
    let progress = |layer: &Descriptor, fetched: bool| {
        engine.report(Message::Progress(Progress::Layer {
            short_digest: layer.digest.hex().chars().take(12).collect(),
            downloaded: fetched.then_some(layer.size),
        }));
    };
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
    Ok(pull_with(engine, reference, None)?.digest)
}

/// What [`pull_with`] pulled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledImage {
    /// What Docker's `Digest:` says: the multi-platform index the
    /// reference named, if it named one, else its manifest's digest (F118).
    pub digest: String,
    /// Whether the reference names another image than before (or none
    /// before): Docker's "Downloaded newer image", else "up to date".
    pub newer: bool,
}

/// A credential a client handed in for a pull (Docker's `X-Registry-Auth`),
/// and the registry it's for, if it names one.
#[derive(Debug, Clone)]
pub struct GivenCredential {
    /// The credential: never logged, kept or shown.
    pub credential: Credential,
    /// The registry it's for (`docker.io`, `https://index.docker.io/v1/`,
    /// `ghcr.io`); `None`: any.
    pub registry: Option<String>,
}

/// What a sign-in came to ([`login`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// The registry took the credential (or asked for none).
    Succeeded,
    /// The registry refused it, or took none of its kind: why, in words
    /// that never hold the secret.
    Refused(String),
}

/// Signs in to the registry `given` names (Docker Hub if it names none)
/// with its credential, as `docker login` asks Docker's daemon to (`POST
/// /auth`, F118): checked against the registry's `/v2/`, never kept,
/// logged or shown. The client keeps it, as Docker's CLI does.
///
/// # Errors
///
/// [`crate::ErrorKind::Image`]: the registry couldn't be
/// reached, or answered otherwise.
pub fn login(given: GivenCredential) -> Result<Login> {
    use shipyard_image::RegistryError;
    match shipyard_image::login(given.registry.as_deref(), given.credential) {
        Ok(()) => Ok(Login::Succeeded),
        // Docker's words for it (`unauthorized: …`): never "sign in again".
        Err(RegistryError::Refused { registry, .. }) => Ok(Login::Refused(format!(
            "unauthorized: {registry} refused the username and password or token"
        ))),
        Err(error @ RegistryError::Auth { .. }) => {
            Ok(Login::Refused(format!("unauthorized: {error}")))
        }
        Err(error) => Err(crate::Error::new(
            crate::ErrorKind::Image,
            format!("signing in: {error}"),
        )),
    }
}

/// [`pull`], signing in with `given` instead of Docker's config file when
/// a client handed one in (an anonymous pull for any other registry).
///
/// # Errors
///
/// As [`pull`].
pub fn pull_with(
    engine: &Engine,
    reference: &str,
    given: Option<GivenCredential>,
) -> Result<PulledImage> {
    let store = open_store(engine)?;
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let image = ImageReference::parse(reference)?;
    // What the reference named before (an index that can't be read: none).
    let before = Index::open(store.root()).ok().and_then(|index| {
        index
            .get(&image.to_string())
            .map(|record| record.manifest.clone())
    });
    let fetch = match given {
        Some(given) => Fetch {
            policy: PullPolicy::Always,
            credentials: shipyard_image::Credentials::given(
                given.credential,
                given.registry.as_deref(),
            ),
        },
        None => Fetch::docker(PullPolicy::Always),
    };
    let ready = ready(engine, &store, &shared, &image, fetch, None)?;
    drop(shared);
    let manifest = ready.pulled.manifest_digest.to_string();
    // Docker's `Digest:`: the index the reference named, else its manifest.
    let digest = ready
        .pulled
        .index_digest
        .as_ref()
        .map_or_else(|| manifest.clone(), ToString::to_string);
    Ok(PulledImage {
        newer: before.as_ref() != Some(&manifest),
        digest,
    })
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

/// Records a pulled image in the store's reference index: the index it
/// came through, and when its reference was pointed at it (now, unless it
/// already named this image: a run of a stored one leaves its time).
fn record_image(engine: &Engine, store: &Store, pulled: &Pulled) -> Result<()> {
    let key = pulled.reference.to_string();
    let manifest = pulled.manifest_digest.to_string();
    let kept = Index::open(store.root())
        .ok()
        .and_then(|index| index.get(&key).cloned())
        .filter(|before| before.manifest == manifest)
        .and_then(|before| before.tagged_at);
    let record = shipyard_image::ImageRecord::of(
        pulled,
        Some(kept.unwrap_or_else(std::time::SystemTime::now)),
    );
    if let Some(aside) = Index::record(store.root(), &key, record)? {
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

/// Tags the image `source` names (a reference, or an ID: [`remove`]'s
/// rule) as `target` too (`repo[:tag]`, `latest` if none), as `docker tag`
/// does: a reference the index records, the same image's. Returns the
/// target as the index names it.
///
/// # Errors
///
/// `source` names no image ([`crate::ErrorKind::NotFound`]), or an
/// ambiguous ID; `target` isn't a reference, or has a digest
/// ([`crate::ErrorKind::InvalidRequest`]); the store can't be updated.
pub fn tag(engine: &Engine, source: &str, target: &str) -> Result<String> {
    let target = ImageReference::parse(target)?;
    if target.digest.is_some() {
        bail!(
            InvalidRequest,
            "a tag can't have a digest: {target} (name it repository[:tag])"
        );
    }
    let store = open_store(engine)?;
    // Shared: no collection removes the image's blobs meanwhile.
    let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let index = Index::open(store.root())?;
    let (found, _) = named(&index, source)?;
    let Some(image) = found.into_iter().next() else {
        bail!(NotFound, "no such image: {source}");
    };
    let key = target.to_string();
    // Its time now; no index of its own (a registry knows this name by
    // none yet: Docker's adds a repository's digest at a pull or a push).
    let record = shipyard_image::ImageRecord {
        index_digest: None,
        tagged_at: Some(std::time::SystemTime::now()),
        ..image.record
    };
    if let Some(aside) = Index::record(store.root(), &key, record)? {
        engine.warn(
            WarningKind::IndexReset,
            format!(
                "the store index was corrupt; it was moved to {} and a new one started",
                aside.display()
            ),
        );
    }
    crate::events::emit_about(engine, crate::events::Kind::Image, "tag", &key);
    Ok(key)
}

/// Whether the store has the image `reference` names whole, for this
/// host's platform, as a run would take it without pulling (the API's
/// create answers "No such image" otherwise, and its client pulls).
///
/// # Errors
///
/// The reference isn't one ([`crate::ErrorKind::InvalidRequest`]), or the
/// store can't be opened or read: never "not stored" (a client told so
/// would pull it again).
pub fn stored(engine: &Engine, reference: &str) -> Result<bool> {
    let image = ImageReference::parse(reference)
        .map_err(|error| crate::Error::new(crate::ErrorKind::InvalidRequest, error.to_string()))?;
    let store = open_store(engine)?;
    let found = shipyard_image::from_store(&image, &Platform::host(), &store)
        .with_context(|| format!("reading {reference} from the image store"))?;
    Ok(found.is_some())
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

/// What [`remove`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removed {
    /// The references untagged, as the index names them.
    pub untagged: Vec<String>,
    /// Whether the name given was an ID (else a reference, the one
    /// untagged).
    pub by_id: bool,
    /// The blob digests deleted (an image's own config and manifest
    /// among them when no reference keeps it any more), sorted.
    pub deleted: Vec<String>,
}

/// Removes an image and garbage-collects what nothing needs any more (for
/// `shipyard rmi` and Docker's `DELETE /images/{name}`). `name` is a
/// reference, else an ID ([`Index::by_id`]: its config or manifest digest,
/// or a unique prefix), which removes every reference of that image; one
/// with more than one is refused unless `force`, as Docker does. An image
/// a container was made from (running or not: its pin), or a foreground
/// run uses, is refused, unless `force` (or another reference keeps the
/// same image): then it's untagged; a container's pin keeps what it needs,
/// a run's layers stay until a later prune.
///
/// # Errors
///
/// Returns an error if no image matches ([`crate::ErrorKind::NotFound`]),
/// an ID's prefix matches more than one ([`crate::ErrorKind::InvalidRequest`]),
/// it's in use or has several references without `force`
/// ([`crate::ErrorKind::InUse`]), the store is busy
/// ([`crate::ErrorKind::Busy`]), or it can't be updated.
pub fn remove(engine: &Engine, name: &str, force: bool) -> Result<Removed> {
    // The references untagged, and the image's manifest's digest.
    let mut untagged = (Vec::new(), None);
    let mut by_id = false;
    let deleted = collect(engine, |index, held, live, in_use, remove_layers| {
        let (found, id) = named(index, name)?;
        let targets: Vec<String> = found.into_iter().map(|image| image.reference).collect();
        by_id = id;
        let manifest = targets
            .first()
            .and_then(|reference| index.get(reference))
            .map(|record| record.manifest.clone());
        if !force && let Some(manifest) = &manifest {
            refuse_in_use(index, name, &targets, manifest, live)?;
        }
        // All of them, or none: one save, one collection.
        let deleted = index.remove_all(&targets, held, in_use, remove_layers)?;
        untagged = (targets, manifest);
        Ok(deleted)
    })?;
    let (untagged, manifest) = untagged;
    for reference in &untagged {
        crate::events::emit_about(engine, crate::events::Kind::Image, "untag", reference);
    }
    // Deleted: the image itself, not only its name (no other reference
    // keeps it). Its layers and config aren't images.
    if let Some(manifest) = manifest.filter(|manifest| deleted.contains(manifest)) {
        crate::events::emit_about(engine, crate::events::Kind::Image, "delete", &manifest);
    }
    let mut deleted = deleted;
    deleted.sort();
    deleted.dedup();
    Ok(Removed {
        untagged,
        by_id,
        deleted,
    })
}

/// Removes every reference of the images `manifests` names that nothing
/// uses, decided again under the store's lock: no container made from it
/// (its record, read under the lock, so one placed before is seen and one
/// placed after fails at its pin; or its pin, which an unreadable record
/// still has) and no run (its area). What it removed: the references
/// untagged, the blobs deleted.
///
/// # Errors
///
/// The store is busy ([`crate::ErrorKind::Busy`]), or can't be read or
/// updated; the containers' records can't be listed.
pub(crate) fn remove_unused(
    engine: &Engine,
    manifests: &BTreeSet<String>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut untagged = Vec::new();
    let mut images = BTreeSet::new();
    let deleted = collect(engine, |index, held, live, in_use, remove_layers| {
        // What a container's record names: its image's manifest, or (a
        // record older than `image_id`) the one its reference names now.
        let named: BTreeSet<String> = crate::containers::list(engine)?
            .iter()
            .filter_map(|record| {
                record.image_id.clone().or_else(|| {
                    let reference = record.config.image.as_deref()?;
                    let canonical = ImageReference::parse(reference).ok()?.to_string();
                    Some(index.get(&canonical)?.manifest.clone())
                })
            })
            .collect();
        let unused = |manifest: &String| {
            manifests.contains(manifest)
                && !named.contains(manifest)
                && index.pinned_by(manifest).is_empty()
                && !live.iter().any(|area| &area.image == manifest)
        };
        let targets: Vec<String> = index
            .images()
            .into_iter()
            .filter(|image| unused(&image.record.manifest))
            .map(|image| {
                images.insert(image.record.manifest);
                image.reference
            })
            .collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let deleted = index.remove_all(&targets, held, in_use, remove_layers)?;
        untagged = targets;
        Ok(deleted)
    })?;
    for reference in &untagged {
        crate::events::emit_about(engine, crate::events::Kind::Image, "untag", reference);
    }
    for manifest in images.iter().filter(|manifest| deleted.contains(*manifest)) {
        crate::events::emit_about(engine, crate::events::Kind::Image, "delete", manifest);
    }
    let mut deleted = deleted;
    deleted.sort();
    deleted.dedup();
    Ok((untagged, deleted))
}

/// The bytes container `id`'s own writable layer holds (what could be
/// read of it; 0 without one), for a prune's reclaimed space.
pub(crate) fn owned_area_bytes(engine: &Engine, id: &str) -> u64 {
    // No store: no layer (and a measure never makes one).
    if !engine
        .paths()
        .image_store()
        .is_ok_and(std::path::Path::exists)
    {
        return 0;
    }
    let Ok(store) = open_store(engine) else {
        return 0;
    };
    let name = shipyard_image::owned_area_name(id);
    let upper = shipyard_image::AreaPlaces::of(&store, &name).upper;
    let (Some(parent), Some(leaf)) = (upper.parent(), upper.file_name()) else {
        return 0;
    };
    shipyard_image::fsat::open_dir(parent)
        .map_or(0, |dir| shipyard_image::fsat::tree_usage(&dir, leaf).bytes)
}

/// What `name` names in `index`: the reference it is (canonical, if the
/// index has that), else every reference of the image it's the ID of
/// ([`Index::by_id`]); and whether it was an ID.
///
/// # Errors
///
/// Neither ([`crate::ErrorKind::NotFound`]); an ID's prefix of more than
/// one image ([`crate::ErrorKind::InvalidRequest`]); an index that can't be
/// read.
pub(crate) fn named(index: &Index, name: &str) -> Result<(Vec<shipyard_image::ListedImage>, bool)> {
    let key = index_key(index, name);
    if let Some(record) = index.get(&key) {
        let found = shipyard_image::ListedImage {
            reference: key,
            record: record.clone(),
        };
        return Ok((vec![found], false));
    }
    // `repo@sha256:…` (an image's `RepoDigests`): that repository's
    // references to that manifest.
    if let Ok(reference) = ImageReference::parse(name)
        && let Some(digest) = &reference.digest
    {
        let digest = digest.to_string();
        let found: Vec<_> = index
            .images()
            .into_iter()
            .filter(|image| {
                image.record.manifest == digest
                    && ImageReference::parse(&image.reference).is_ok_and(|listed| {
                        listed.registry == reference.registry
                            && listed.repository == reference.repository
                    })
            })
            .collect();
        if found.is_empty() {
            bail!(NotFound, "no such image: {name}");
        }
        return Ok((found, true));
    }
    match index.by_id(name) {
        Ok(images) => Ok((images, true)),
        // Neither: named as typed, as the index's own refusal has it.
        Err(shipyard_image::IndexError::NoSuchImage(_)) => bail!(NotFound, "no such image: {name}"),
        Err(error) => Err(error.into()),
    }
}

/// Refuses to remove `targets` (what `name` names) of the image whose
/// manifest is `manifest`, without `--force`: references in more than one
/// repository at once (an ID's: Docker's rule), or the image in use while
/// no other reference keeps it.
fn refuse_in_use(
    index: &Index,
    name: &str,
    targets: &[String],
    manifest: &str,
    live: &[LiveArea],
) -> Result<()> {
    let repositories: BTreeSet<String> = targets
        .iter()
        .map(|target| {
            ImageReference::parse(target).map_or_else(
                |_| target.clone(),
                |parsed| format!("{}/{}", parsed.registry, parsed.repository),
            )
        })
        .collect();
    if repositories.len() > 1 {
        bail!(
            InUse,
            "the image {name} is referenced in several repositories ({}): --force removes them \
             all",
            targets.join(", ")
        );
    }
    let kept_elsewhere = index
        .images()
        .iter()
        .any(|other| !targets.contains(&other.reference) && other.record.manifest == manifest);
    if kept_elsewhere {
        return Ok(());
    }
    // A container made from it (running or not) pins it.
    let pinned = index.pinned_by(manifest);
    if !pinned.is_empty() {
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
            "the image {name} is used by container {} (--force untags it anyway: the containers \
             keep what they need)",
            named.join(", ")
        );
    }
    if live.iter().any(|area| area.image == manifest) {
        bail!(
            InUse,
            "the image {name} is in use by a running container (--force untags it anyway)"
        );
    }
    Ok(())
}

/// Removes every blob and layer nothing needs, and what interrupted work
/// left (for `shipyard image prune`).
///
/// # Errors
///
/// Returns an error if the store is busy or can't be updated.
pub fn prune(engine: &Engine) -> Result<Vec<String>> {
    let removed = collect_unreferenced(engine)?;
    crate::events::emit_pruned(engine, crate::events::Kind::Image, removed.len());
    Ok(removed)
}

/// [`prune`]'s collection, unsaid: for a prune that says the whole of
/// what it did once.
pub(crate) fn collect_unreferenced(engine: &Engine) -> Result<Vec<String>> {
    collect(engine, |index, held, _, in_use, remove_layers| {
        Ok(index.prune(held, in_use, remove_layers)?)
    })
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
    /// The images (their manifests): those a container's layers use are
    /// active; their blobs and extracted layers.
    pub images: Usage,
    /// The containers: those running are active; their writable layers,
    /// kept ones of stopped containers in `bytes`, not reclaimable (`rm`
    /// reclaims them, not a prune).
    pub containers: Usage,
}

/// What a kind of thing in the store takes.
#[derive(Debug, Default)]
pub struct Usage {
    /// How many there are.
    pub total: u64,
    /// How many are in use (Docker's `ACTIVE`).
    pub active: u64,
    /// Their bytes.
    pub bytes: u64,
    /// The bytes nothing uses now.
    pub reclaimable: u64,
    /// Whether all of it could be read: `false`, it takes more.
    pub complete: bool,
}

/// `n` as a count of things.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
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
            containers.active += 1;
        }
        if !kept {
            containers.reclaimable += taken.bytes;
        }
    }
    // Every container's record, not only those with a layer (one never
    // started has none). Unreadable: at least those running, said to be
    // more (`complete` false).
    let records = crate::containers::list(engine).ok();
    if let Some(records) = &records {
        containers.total = count(records.len()).max(containers.active);
    } else {
        containers.total = containers.active;
        containers.complete = false;
    }
    // An image is active when a container's record runs it (its manifest
    // digest, what every start runs): never by layers a base shares.
    let used: BTreeSet<&str> = records
        .iter()
        .flatten()
        .filter_map(|record| record.image_id.as_deref())
        .collect();
    let manifests: BTreeSet<String> = index
        .images()
        .into_iter()
        .map(|image| image.record.manifest)
        .collect();
    let images = Usage {
        total: count(manifests.len()),
        active: count(
            manifests
                .iter()
                .filter(|manifest| used.contains(manifest.as_str()))
                .count(),
        ),
        bytes: disk.blob_bytes + disk.layer_bytes,
        reclaimable: disk.blob_reclaimable + disk.layer_reclaimable,
        complete: disk.complete,
    };
    Ok(StoreUsage { images, containers })
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
            index_digest: None,
            tagged_at: None,
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
