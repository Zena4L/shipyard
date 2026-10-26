//! Pushing an image (step 12.4): its stored blobs and manifest sent as they
//! are, so the registry's copy has the same digests, and its ID is the
//! same.
//!
//! [`open`] opens every blob under the store's lock (held by the caller),
//! so a prune meanwhile can't take one; the upload then reads the open
//! descriptors, by offset, with no lock held ([`push`]). Each distinct blob
//! is sent once: a `HEAD` first (one the registry has is skipped), then
//! the upload, the config and layers by up to [`WORKERS`] at once; the
//! manifest last, so the tag never names an image the registry doesn't
//! have whole.
//!
//! An image pulled through a multi-platform index keeps only its own
//! platform's manifest: pushed, it's that manifest alone. Pushed back to a
//! tag that still names the very index it came through, it would replace
//! that multi-platform image with one platform's: refused
//! ([`PushError::WouldFlatten`]).

use std::fs::File;

use crate::digest::Digest;
use crate::manifest::{ImageManifest, media_type};
use crate::registry::{Registry, RegistryError};
use crate::store::{Store, StoreError};

/// The most blobs uploaded at once.
pub const WORKERS: usize = 3;

/// An image to push: its manifest, and the index it was pulled through,
/// if one.
#[derive(Debug, Clone)]
pub struct ToPush {
    /// Its manifest's digest: the image's ID.
    pub manifest_digest: Digest,
    /// The index it came through, if one.
    pub index_digest: Option<Digest>,
    /// Its manifest.
    pub manifest: ImageManifest,
}

/// An image whose blobs are open, to push.
#[derive(Debug)]
pub struct Opened {
    manifest_digest: Digest,
    index_digest: Option<Digest>,
    manifest_bytes: Vec<u8>,
    media_type: String,
    /// Each distinct blob once: the config, then the layers.
    blobs: Vec<OpenBlob>,
}

/// One blob of a push, open.
#[derive(Debug)]
struct OpenBlob {
    digest: Digest,
    size: u64,
    file: File,
}

/// What happened to one blob of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushEvent {
    /// Sent.
    Pushed {
        /// Its digest.
        digest: Digest,
        /// Its size.
        size: u64,
    },
    /// The registry had it.
    Exists {
        /// Its digest.
        digest: Digest,
    },
}

/// What a push did: the manifest's digest and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// Its digest (the image's ID).
    pub digest: Digest,
    /// Its size in bytes.
    pub size: u64,
}

/// Why a push failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PushError {
    /// The registry failed, refused or denied it.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// A blob couldn't be read from the store.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The tag names the multi-platform index this image was pulled
    /// through: a push would make it one platform's.
    #[error(
        "{name} names the multi-platform image {index} this one came from: pushing would replace \
         it with this platform's alone; push to another tag"
    )]
    WouldFlatten {
        /// The name pushed to.
        name: String,
        /// The index it names.
        index: Digest,
    },
    /// The store is missing one of the image's own blobs.
    #[error(
        "the image store has no blob {0} (the image's own): remove the image and pull it again"
    )]
    Missing(Digest),
}

/// Opens the manifest and every blob of `image` in `store`, which the
/// caller holds locked (shared), so none is collected meanwhile.
///
/// # Errors
///
/// A blob missing ([`PushError::Missing`]) or unreadable.
pub fn open(store: &Store, image: &ToPush) -> Result<Opened, PushError> {
    let present = |digest: &Digest| {
        if store.contains(digest) {
            Ok(())
        } else {
            Err(PushError::Missing(digest.clone()))
        }
    };
    present(&image.manifest_digest)?;
    let manifest_bytes = store.read_blob(&image.manifest_digest, crate::MANIFEST_SIZE_LIMIT)?;
    let mut blobs: Vec<OpenBlob> = Vec::new();
    for blob in std::iter::once(&image.manifest.config).chain(&image.manifest.layers) {
        if blobs.iter().any(|had| had.digest == blob.digest) {
            continue;
        }
        let (file, size) = store
            .open_sized(&blob.digest)?
            .ok_or_else(|| PushError::Missing(blob.digest.clone()))?;
        blobs.push(OpenBlob {
            digest: blob.digest.clone(),
            size,
            file,
        });
    }
    Ok(Opened {
        manifest_digest: image.manifest_digest.clone(),
        index_digest: image.index_digest.clone(),
        media_type: media_type::of_manifest(&manifest_bytes),
        manifest_bytes,
        blobs,
    })
}

/// Pushes `opened` to `repository` on `registry` as `tag`: the blobs the
/// registry lacks, then the manifest. `progress` hears of each blob.
///
/// # Errors
///
/// The registry's ([`PushError::Registry`]), after its retries; the tag
/// naming the index the image came from ([`PushError::WouldFlatten`]).
pub fn push(
    opened: &Opened,
    registry: &Registry,
    repository: &str,
    tag: &str,
    progress: &(dyn Fn(&PushEvent) + Sync),
) -> Result<Pushed, PushError> {
    if let Some(index) = &opened.index_digest
        && registry.manifest_digest(repository, tag)?.as_ref() == Some(index)
    {
        return Err(PushError::WouldFlatten {
            name: format!("{repository}:{tag}"),
            index: index.clone(),
        });
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(opened.blobs.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(blob) = opened.blobs.get(at) else {
                        return;
                    };
                    if failed.lock().is_ok_and(|failed| failed.is_some()) {
                        return;
                    }
                    let sent = send(registry, repository, blob, progress);
                    if let Err(err) = sent {
                        if let Ok(mut failed) = failed.lock() {
                            failed.get_or_insert(err);
                        }
                        return;
                    }
                }
            });
        }
    });
    if let Some(err) = failed.into_inner().ok().flatten() {
        return Err(err.into());
    }
    registry.put_manifest(
        repository,
        tag,
        &opened.media_type,
        &opened.manifest_bytes,
        &opened.manifest_digest,
    )?;
    Ok(Pushed {
        digest: opened.manifest_digest.clone(),
        size: opened.manifest_bytes.len() as u64,
    })
}

/// One blob: skipped if the registry has it, else uploaded.
fn send(
    registry: &Registry,
    repository: &str,
    blob: &OpenBlob,
    progress: &(dyn Fn(&PushEvent) + Sync),
) -> Result<(), RegistryError> {
    if registry.blob_exists(repository, &blob.digest)? {
        progress(&PushEvent::Exists {
            digest: blob.digest.clone(),
        });
        return Ok(());
    }
    registry.upload_blob(repository, &blob.digest, blob.size, &blob.file)?;
    progress(&PushEvent::Pushed {
        digest: blob.digest.clone(),
        size: blob.size,
    });
    Ok(())
}
