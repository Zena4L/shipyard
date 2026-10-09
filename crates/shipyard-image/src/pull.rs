//! Pulling an image: from a reference to every blob in the store, verified.
//!
//! 1. Fetch the top document the reference names. A pinned digest
//!    (`@sha256:…`) must be the digest of exactly the bytes served: the
//!    registry is trusted for nothing the digest can check.
//! 2. If it's an index (a multi-platform list), pick the entry for the
//!    platform, and fetch that manifest, held to the entry's digest and size.
//! 3. Store the index and the manifest, so a later run needs no registry.
//! 4. Fetch the config, held to its digest and size (an artifact's config is
//!    refused: it isn't a runnable image), and check the image's platform.
//! 5. Fetch every layer the store doesn't have, up to four at once, each
//!    held to its digest and size.
//!
//! What kind of document the top one is comes from its own JSON (its
//! `mediaType`, then its shape), and the `Content-Type` only breaks a tie:
//! proxies and static mirrors label things `application/json`.
//!
//! The layer workers are scoped threads, all joined before this returns:
//! the CLI forks the container afterwards, and must have no other thread.

use std::io::Read;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::credentials::Credentials;
use crate::digest::Digest;
use crate::index::Index as StoreIndex;
use crate::manifest::{Descriptor, ImageConfig, ImageManifest, Index, Platform, media_type};
use crate::reference::ImageReference;
use crate::registry::{BlobError, BlobSink, Registry, RegistryError};
use crate::store::{BlobWriter, Store, StoreError};
use crate::unpack::{FreeSpace, UnpackError};

/// The most layers downloaded at once.
const MAX_PARALLEL_LAYERS: usize = 4;

/// A pulled (or found) image: what it is, and where its pieces are.
#[derive(Debug, Clone)]
pub struct Pulled {
    /// The reference, as given (canonical).
    pub reference: ImageReference,
    /// The image's platform, from its config.
    pub platform: Platform,
    /// The image manifest (the platform's, if the reference named an index).
    pub manifest: ImageManifest,
    /// The image config.
    pub config: ImageConfig,
    /// The manifest's digest.
    pub manifest_digest: Digest,
}

/// Why a pull failed.
#[derive(Debug, thiserror::Error)]
pub enum PullError {
    /// A registry request failed.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// A document didn't parse.
    #[error(transparent)]
    Manifest(#[from] crate::manifest::ManifestError),
    /// A blob couldn't be stored, or didn't match its digest.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The store's index couldn't be read.
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
    /// The registry served something other than the pinned digest.
    #[error("the registry served {actual} for the pinned {expected}")]
    Pinned {
        /// The digest the reference pinned.
        expected: Digest,
        /// What the served bytes hash to.
        actual: Digest,
    },
    /// A manifest from an index isn't what the index described.
    #[error(
        "the index names the manifest {digest} ({size} bytes), but the registry served \
         {actual} ({actual_size} bytes)"
    )]
    IndexMismatch {
        /// What the index named.
        digest: Digest,
        /// The size the index gave.
        size: u64,
        /// What the served bytes hash to.
        actual: Digest,
        /// How many there were.
        actual_size: u64,
    },
    /// The image isn't a runnable image (an OCI artifact: a chart, a
    /// signature).
    #[error("this is an OCI artifact ({0}), not a runnable image")]
    Artifact(String),
    /// The config is larger than a config should be.
    #[error("the image config is {0} bytes, over the limit for one")]
    ConfigTooLarge(u64),
    /// The image has more layers than [`MAX_LAYERS`](crate::MAX_LAYERS).
    #[error("the image has {0} layers, more than the {limit} an image may have", limit = crate::MAX_LAYERS)]
    TooManyLayers(usize),
    /// A layer is in a form Shipyard can't extract.
    #[error(transparent)]
    Layer(#[from] UnpackError),
    /// The layers to download wouldn't fit.
    #[error(
        "the image needs {needed} bytes more, and the image store's filesystem has {available} \
         free, of which {reserve} must stay free"
    )]
    NoSpace {
        /// Bytes the missing layers take.
        needed: u64,
        /// Bytes free.
        available: u64,
        /// Bytes that must stay free.
        reserve: u64,
    },
    /// The image is for another platform.
    #[error("the image is for {image}, not {wanted}")]
    Platform {
        /// The platform asked for.
        wanted: String,
        /// The image's.
        image: String,
    },
}

/// How a pull goes.
#[derive(Debug, Clone, Copy, Default)]
pub struct PullOptions {
    /// Verify the blobs the store has before taking them as they are, and
    /// fetch again any that no longer match (one read of each): what an
    /// explicit pull does, to repair a store a disk damaged.
    pub recheck: bool,
}

/// Pulls `image` for `platform` into `store`, from its registry, signing in
/// with `credentials` if it asks. `downloaded` is told of each layer once
/// it's in the store (fetched, or there already: then with `false`), from
/// the download threads.
///
/// # Errors
///
/// Returns a [`PullError`] if any request fails, a document doesn't parse
/// or isn't what it should be, a blob fails verification, or the layers
/// wouldn't fit.
pub fn pull(
    image: &ImageReference,
    platform: &Platform,
    store: &Store,
    credentials: Credentials,
    options: PullOptions,
    downloaded: &(dyn Fn(&Descriptor, bool) + Sync),
) -> Result<Pulled, PullError> {
    let registry = Registry::new(image.host()).with_credentials(credentials, &image.registry);
    pull_with(image, platform, store, &registry, options, downloaded)
}

/// The body of [`pull`], taking an already-built [`Registry`] so tests can
/// point it at a local fixture server with [`Registry::with_base`].
///
/// # Errors
///
/// As [`pull`].
pub fn pull_with(
    image: &ImageReference,
    platform: &Platform,
    store: &Store,
    registry: &Registry,
    options: PullOptions,
    downloaded: &(dyn Fn(&Descriptor, bool) + Sync),
) -> Result<Pulled, PullError> {
    let limit = crate::MANIFEST_SIZE_LIMIT;
    let top = registry.get_manifest(&image.repository, &image.manifest_path(), limit)?;
    let top_digest = Digest::sha256(&top.bytes);
    if let Some(pinned) = &image.digest
        && *pinned != top_digest
    {
        return Err(PullError::Pinned {
            expected: pinned.clone(),
            actual: top_digest,
        });
    }

    let (manifest_bytes, manifest_digest) = if is_index(&top.bytes, top.media_type.as_deref()) {
        store.write_blob(&top_digest, &mut &top.bytes[..])?;
        let index = Index::parse(&top.bytes)?;
        let entry = index.select(platform)?;
        let fetched = registry.get_manifest(
            &image.repository,
            &entry.digest.to_string(),
            entry.size.min(limit),
        )?;
        let actual = Digest::sha256(&fetched.bytes);
        if actual != entry.digest || fetched.bytes.len() as u64 != entry.size {
            return Err(PullError::IndexMismatch {
                digest: entry.digest.clone(),
                size: entry.size,
                actual,
                actual_size: fetched.bytes.len() as u64,
            });
        }
        (fetched.bytes, actual)
    } else {
        (top.bytes, top_digest)
    };
    store.write_blob(&manifest_digest, &mut &manifest_bytes[..])?;
    let manifest = ImageManifest::parse(&manifest_bytes)?;
    check_manifest(&manifest)?;

    let fetch = Fetch {
        registry,
        repository: &image.repository,
        store,
        recheck: options.recheck,
    };
    fetch.blob(&manifest.config, &AtomicBool::new(false))?;
    let config = ImageConfig::parse(&store.read_blob(&manifest.config.digest, limit)?)?;
    let image_platform = check_platform(&config, platform)?;

    check_space(store, &manifest.layers)?;
    fetch.layers(&manifest.layers, downloaded)?;
    Ok(Pulled {
        reference: image.clone(),
        platform: image_platform,
        manifest,
        config,
        manifest_digest,
    })
}

/// The image `reference` names, from the store alone, if it has every
/// piece (and they verify): the manifest the store's index recorded for it
/// (or the pinned digest's), its config, and its layer blobs. `None` if
/// anything is missing: then it has to be pulled.
///
/// # Errors
///
/// A [`PullError`] if the index can't be read, a stored document is corrupt,
/// or the stored image is for another platform.
pub fn from_store(
    reference: &ImageReference,
    platform: &Platform,
    store: &Store,
) -> Result<Option<Pulled>, PullError> {
    let limit = crate::MANIFEST_SIZE_LIMIT;
    let index = StoreIndex::open(store.root())?;
    let recorded = index
        .get(&reference.to_string())
        .and_then(|record| Digest::parse(&record.manifest).ok());
    let Some(top_digest) = recorded.or_else(|| reference.digest.clone()) else {
        return Ok(None);
    };
    if !store.contains(&top_digest) {
        return Ok(None);
    }
    let top = store.read_blob(&top_digest, limit)?;
    // A pinned index: the platform's manifest, if that's stored too.
    let (manifest_bytes, manifest_digest) = if is_index(&top, None) {
        let digest = Index::parse(&top)?.select(platform)?.digest.clone();
        if !store.contains(&digest) {
            return Ok(None);
        }
        (store.read_blob(&digest, limit)?, digest)
    } else {
        (top, top_digest)
    };
    let manifest = ImageManifest::parse(&manifest_bytes)?;
    check_manifest(&manifest)?;
    let mut pieces = std::iter::once(&manifest.config).chain(&manifest.layers);
    if !pieces.all(|blob| store.contains(&blob.digest)) {
        return Ok(None);
    }
    let config = ImageConfig::parse(&store.read_blob(&manifest.config.digest, limit)?)?;
    let image_platform = check_platform(&config, platform)?;
    Ok(Some(Pulled {
        reference: reference.clone(),
        platform: image_platform,
        manifest,
        config,
        manifest_digest,
    }))
}

/// Whether a manifest document is an index: by its own `mediaType`, then by
/// its shape (`manifests`, or `config`/`layers`), and only then by the
/// `Content-Type` it came with.
fn is_index(bytes: &[u8], content_type: Option<&str>) -> bool {
    #[derive(serde::Deserialize)]
    struct Shape {
        #[serde(rename = "mediaType")]
        media_type: Option<String>,
        manifests: Option<serde::de::IgnoredAny>,
        config: Option<serde::de::IgnoredAny>,
    }
    match serde_json::from_slice::<Shape>(bytes) {
        Ok(Shape {
            media_type: Some(media),
            ..
        }) if media_type::is_index(&media) || media_type::is_manifest(&media) => {
            media_type::is_index(&media)
        }
        Ok(Shape {
            manifests: Some(_),
            config: None,
            ..
        }) => true,
        Ok(Shape {
            manifests: None,
            config: Some(_),
            ..
        }) => false,
        _ => content_type.is_some_and(media_type::is_index),
    }
}

/// Refuses what isn't a runnable image, before anything is downloaded: an
/// artifact's config, an oversized config, a layer in a form Shipyard can't
/// extract.
fn check_manifest(manifest: &ImageManifest) -> Result<(), PullError> {
    if !media_type::is_config(&manifest.config.media_type) {
        return Err(PullError::Artifact(manifest.config.media_type.clone()));
    }
    if manifest.config.size > crate::MANIFEST_SIZE_LIMIT {
        return Err(PullError::ConfigTooLarge(manifest.config.size));
    }
    if manifest.layers.len() > crate::MAX_LAYERS {
        return Err(PullError::TooManyLayers(manifest.layers.len()));
    }
    for layer in &manifest.layers {
        crate::unpack::layer_compression(&layer.media_type)?;
    }
    Ok(())
}

/// The image's platform, if it runs on `wanted`.
fn check_platform(config: &ImageConfig, wanted: &Platform) -> Result<Platform, PullError> {
    let image = config.platform();
    if image.runs_on(wanted) {
        Ok(image)
    } else {
        Err(PullError::Platform {
            wanted: wanted.to_string(),
            image: image.to_string(),
        })
    }
}

/// Where a pull's blobs come from and go to.
struct Fetch<'a> {
    registry: &'a Registry,
    repository: &'a str,
    store: &'a Store,
    /// Verify the blobs the store has (see [`PullOptions::recheck`]).
    recheck: bool,
}

impl Fetch<'_> {
    /// Fetches one blob into the store, held to its descriptor's digest and
    /// size, going on from a partial download a stopped pull left; whether
    /// it was fetched (`false`: the store had it). With `recheck`, a blob
    /// the store has is verified first, and fetched again if it no longer
    /// matches.
    ///
    /// Once `stop` is set (another layer failed), the transfer ends at its
    /// next read.
    fn blob(&self, descriptor: &Descriptor, stop: &AtomicBool) -> Result<bool, PullError> {
        let (store, digest) = (self.store, &descriptor.digest);
        if store.contains(digest) {
            if !self.recheck || store.check_blob(digest)? {
                return Ok(false);
            }
            tracing::warn!("the stored blob {digest} was corrupt: fetching it again");
        }
        let mut writer = store.begin_blob(digest)?;
        if writer.have() > 0 {
            tracing::debug!("{digest}: going on from byte {}", writer.have());
        }
        let mut restarted = false;
        loop {
            if writer.have() < descriptor.size {
                self.registry.read_blob(
                    self.repository,
                    digest,
                    descriptor.size,
                    &mut Fetching {
                        writer: &mut writer,
                        stop,
                    },
                )?;
            }
            let resumed = writer.resumed() > 0;
            match writer.commit() {
                Ok(_) => return Ok(true),
                // What an earlier pull left was bad (torn by a crash, say):
                // the partial is gone, start from the first byte, once.
                Err(StoreError::Verify(_)) if resumed && !restarted => {
                    tracing::warn!("the partial download of {digest} was corrupt: starting again");
                    restarted = true;
                    writer = store.begin_blob(digest)?;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Fetches every layer into the store, up to [`MAX_PARALLEL_LAYERS`] at
    /// once, telling `downloaded` of each. A failure stops the workers from
    /// starting more, and the transfers they're in; the first is returned.
    /// The workers are joined before this returns: no thread outlives the
    /// pull (a container is forked later).
    fn layers(
        &self,
        layers: &[Descriptor],
        downloaded: &(dyn Fn(&Descriptor, bool) + Sync),
    ) -> Result<(), PullError> {
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let first_error: OnceLock<PullError> = OnceLock::new();
        let workers = MAX_PARALLEL_LAYERS.min(layers.len());

        // A worker that panics takes the scope with it (`scope` panics too).
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    while !failed.load(Ordering::Relaxed) {
                        let Some(layer) = layers.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            return;
                        };
                        match self.blob(layer, &failed) {
                            Ok(fetched) => downloaded(layer, fetched),
                            Err(error) => {
                                let _ = first_error.set(error);
                                failed.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });
        first_error.into_inner().map_or(Ok(()), Err)
    }
}

/// A blob going into the store, as the registry client sees it: appends
/// stop once `stop` is set.
struct Fetching<'a> {
    writer: &'a mut BlobWriter,
    stop: &'a AtomicBool,
}

impl BlobSink for Fetching<'_> {
    type Error = StoreError;

    fn have(&self) -> u64 {
        self.writer.have()
    }

    fn reset(&mut self) -> Result<(), StoreError> {
        self.writer.reset()
    }

    fn append(&mut self, body: &mut dyn Read) -> Result<(), StoreError> {
        self.writer.append(&mut Stoppable {
            inner: body,
            stop: self.stop,
        })
    }
}

/// A reader that fails once `stop` is set.
struct Stoppable<'a, R> {
    inner: R,
    stop: &'a AtomicBool,
}

impl<R: Read> Read for Stoppable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("stopped: another layer failed"));
        }
        self.inner.read(buf)
    }
}

impl From<BlobError<StoreError>> for PullError {
    fn from(err: BlobError<StoreError>) -> Self {
        match err {
            BlobError::Registry(err) => Self::Registry(err),
            BlobError::Sink(err) => Self::Store(err),
        }
    }
}

/// Refuses a pull whose missing `layers` wouldn't fit in the store's
/// filesystem, past the free space it leaves alone: each is held to its
/// size, so their sum (less what partial downloads have already) is the
/// most the download writes. Advisory: two pulls at once each count the
/// same free space, and a full disk mid-way is an error that loses nothing.
fn check_space(store: &Store, layers: &[Descriptor]) -> Result<(), PullError> {
    let needed = layers
        .iter()
        .filter(|layer| !store.contains(&layer.digest))
        .map(|layer| layer.size.saturating_sub(store.partial_len(&layer.digest)))
        .fold(0u64, u64::saturating_add);
    if needed == 0 {
        return Ok(());
    }
    let root = std::fs::File::open(store.root()).map_err(|source| StoreError::Io {
        context: format!("opening {}", store.root().display()),
        source,
    })?;
    let space = FreeSpace::of(&root).map_err(|errno| StoreError::Io {
        context: format!("checking the free space under {}", store.root().display()),
        source: errno.into(),
    })?;
    if space.available < needed.saturating_add(space.reserve) {
        return Err(PullError::NoSpace {
            needed,
            available: space.available,
            reserve: space.reserve,
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_transfer_reads_no_more() {
        let stop = AtomicBool::new(false);
        let mut reader = Stoppable {
            inner: &b"abcdef"[..],
            stop: &stop,
        };
        let mut buf = [0u8; 3];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        stop.store(true, Ordering::Relaxed);
        assert!(reader.read(&mut buf).is_err());
    }

    #[test]
    fn the_body_says_what_a_document_is_before_the_content_type() {
        let index = br#"{"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
        let manifest = br#"{"schemaVersion":2,"config":{},"layers":[]}"#;
        assert!(is_index(index, Some("application/json")));
        assert!(is_index(br#"{"manifests":[]}"#, None));
        assert!(!is_index(manifest, Some(media_type::OCI_INDEX)));
        assert!(
            is_index(b"{}", Some(media_type::OCI_INDEX)),
            "a tie: the label"
        );
    }
}
