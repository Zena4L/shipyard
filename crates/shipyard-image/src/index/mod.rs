//! The store's reference index: which images are present, what blobs each
//! holds, and reference-counted garbage collection.
//!
//! The content-addressed store ([`Store`](crate::Store)) keeps blobs by
//! digest and deduplicates them, but it doesn't know which blobs belong to
//! which image, or whether a blob is still needed. The index does: it records,
//! for each image reference, the digests it depends on (its manifest, config,
//! and layers; a layer's extracted directories are named by its digest).
//! From that:
//!
//! - [`Index::images`] lists what's present;
//! - [`Index::remove`] drops a reference and deletes the blobs and layer
//!   directories no *other* reference still needs (reference-counted GC);
//! - [`Index::prune`] deletes everything no reference needs at all;
//! - [`Index::disk_usage`] totals the blobs' bytes, and how many no image
//!   needs.
//!
//! The index is a single JSON file in the store, written atomically (a temp
//! file synced and renamed into place, then the directory synced), so a
//! crash never leaves it half-written. Adding to it ([`Index::record`])
//! takes `index.lock`, so runs recording at once don't lose each other's
//! images. Removing from it, and collecting garbage, is for a caller that
//! holds the store's lock exclusively ([`crate::Store::lock`]): nothing is
//! being added meanwhile, and nothing is half-written, so temporaries can
//! go too.
//!
//! A corrupt index is never taken as empty by garbage collection: that
//! would make every image unreferenced, and delete them all. Recording
//! moves it aside (`index.json.corrupt-<time>`) and starts again; garbage
//! collection refuses while such a file is there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::LockMode;

mod gc;
mod pins;
#[cfg(test)]
mod tests;

/// The index file's name within the store root.
const INDEX_FILE: &str = "index.json";
/// The lock file `record`'s read-modify-write takes.
const INDEX_LOCK: &str = "index.lock";

/// The most of the index read: a few hundred bytes an image, so room for
/// far more images than a store holds.
const INDEX_MAX: u64 = 64 * 1024 * 1024;
/// How long recording waits for another process's record.
const INDEX_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// The prefix a corrupt index is moved aside to.
const CORRUPT_PREFIX: &str = "index.json.corrupt-";
/// A container's pin: an index entry under this prefix, its container
/// store's identity and the container's ID (`container:<store>:<id>`) keeps
/// the image it was made from (its manifest, config and layers),
/// whatever happens to its tag (since 9.4a). Garbage collection keeps what
/// any entry names; listings, lookups and removals by reference leave pins
/// out: only [`Index::pin`] and [`Index::unpin`] touch them.
const PIN_PREFIX: &str = "container:";

/// What one image reference depends on in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRecord {
    /// The manifest blob's digest (as `sha256:...`).
    pub manifest: String,
    /// The config blob's digest.
    pub config: String,
    /// The layer blob digests, base first.
    pub layers: Vec<String>,
}

impl ImageRecord {
    /// Every blob digest this image references (manifest, config, layers).
    fn blob_digests(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.manifest)
            .chain(std::iter::once(&self.config))
            .chain(&self.layers)
    }
}

/// A listed image: its reference and its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedImage {
    /// The reference (`alpine:3.20`, `ghcr.io/owner/app:v1`).
    pub reference: String,
    /// What it depends on.
    pub record: ImageRecord,
}

/// Disk usage of the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Total bytes of all blobs on disk.
    pub blob_bytes: u64,
    /// Number of distinct blobs.
    pub blob_count: u64,
    /// The bytes of blobs no image references.
    pub blob_reclaimable: u64,
    /// Number of extracted layer directories.
    pub layer_count: u64,
    /// Their bytes.
    pub layer_bytes: u64,
    /// The bytes of those no image references and no running container
    /// stacks (and what removals cut short left).
    pub layer_reclaimable: u64,
    /// Whether every layer could be read (rootless, an image's other
    /// users' directories may not be): `false`, they take more.
    pub complete: bool,
}

/// Why an index operation failed.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// Reading or writing the index, a blob, or a layer directory failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The index file held invalid JSON.
    #[error("the store index {path} is corrupt: {source}")]
    Corrupt {
        /// The index path.
        path: String,
        /// The parse error.
        source: serde_json::Error,
    },
    /// Unused layer directories couldn't be removed.
    #[error("removing unused layers: {0}")]
    RemoveLayers(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A corrupt index was moved aside; nothing is collected until it's
    /// looked at.
    #[error(
        "the store index was found corrupt and moved to {0}; nothing is removed while it's \
         there (check it, then delete it)"
    )]
    CorruptMovedAside(String),
    /// Another process held the index's lock past the wait.
    #[error("the image store is busy; try again")]
    Busy,
    /// No image matched the reference given to [`Index::remove`].
    #[error("no such image: {0}")]
    NoSuchImage(String),
}

/// Removes layer directories by name (see [`crate::remove_layers`]): what
/// garbage collection calls, since removing some takes the user namespace
/// their files were made in.
pub type LayerRemover<'a> =
    dyn FnMut(&[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + 'a;

/// The store's reference index, loaded from (and saved to) `root/index.json`.
#[derive(Debug)]
pub struct Index {
    root: PathBuf,
    images: BTreeMap<String, ImageRecord>,
}

impl Index {
    /// Opens the index under `root`, creating an empty one if none exists.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError`] if the index file exists but can't be read or is
    /// corrupt.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, IndexError> {
        let root = root.into();
        let path = root.join(INDEX_FILE);
        let corrupt = |source| IndexError::Corrupt {
            path: path.display().to_string(),
            source,
        };
        let images = match shipyard_sys::fs::read_bounded(&path, INDEX_MAX) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(corrupt)?,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            // No index Shipyard wrote (too large, a link, not a file): one
            // to move aside, as one that doesn't parse.
            Err(e)
                if shipyard_sys::fs::refusal(&e).is_some()
                    || e.raw_os_error() == Some(libc::ELOOP) =>
            {
                return Err(corrupt(serde_json::Error::io(e)));
            }
            Err(source) => {
                return Err(IndexError::Io {
                    context: format!("reading {}", path.display()),
                    source,
                });
            }
        };
        Ok(Self { root, images })
    }

    /// Records (or replaces) the image at `reference` in the index under
    /// `root`: the index is read again under its lock, so a record another
    /// process just made isn't lost. A corrupt index is moved aside first;
    /// where to is returned, for a warning.
    ///
    /// # Errors
    ///
    /// [`IndexError::Busy`] if the lock is held too long;
    /// [`IndexError::Io`] if the index can't be read or written.
    pub fn record(
        root: &Path,
        reference: &str,
        record: ImageRecord,
    ) -> Result<Option<PathBuf>, IndexError> {
        let _lock = lock(root)?;
        let (mut index, moved) = match Self::open(root) {
            // Already there, the same: nothing to write (nor sync, on every run).
            Ok(index) if index.images.get(reference) == Some(&record) => return Ok(None),
            Ok(index) => (index, None),
            Err(IndexError::Corrupt { .. }) => {
                let seconds = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs());
                // The PID too: two in the same second don't overwrite one
                // another.
                let aside = root.join(format!("{CORRUPT_PREFIX}{seconds}-{}", std::process::id()));
                std::fs::rename(root.join(INDEX_FILE), &aside).map_err(|source| {
                    IndexError::Io {
                        context: format!("moving the corrupt index to {}", aside.display()),
                        source,
                    }
                })?;
                (
                    Self {
                        root: root.to_owned(),
                        images: BTreeMap::new(),
                    },
                    Some(aside),
                )
            }
            Err(err) => return Err(err),
        };
        index.images.insert(reference.to_owned(), record);
        index.save()?;
        Ok(moved)
    }

    /// The record of `reference`, if the index has one (a container's pin
    /// is never one: it isn't a reference).
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<&ImageRecord> {
        if reference.starts_with(PIN_PREFIX) {
            return None;
        }
        self.images.get(reference)
    }

    /// The images present, by reference (containers' pins left out).
    #[must_use]
    pub fn images(&self) -> Vec<ListedImage> {
        self.images
            .iter()
            .filter(|(reference, _)| !reference.starts_with(PIN_PREFIX))
            .map(|(reference, record)| ListedImage {
                reference: reference.clone(),
                record: record.clone(),
            })
            .collect()
    }

    /// Removes the entry `reference` from the index under `root`, under its
    /// lock, collecting nothing. Whether it was there.
    ///
    /// # Errors
    ///
    /// [`IndexError::Busy`] if the lock is held too long;
    /// [`IndexError::Io`] or [`IndexError::Corrupt`] if the index can't be
    /// read or written.
    fn forget(root: &Path, reference: &str) -> Result<bool, IndexError> {
        let _lock = lock(root)?;
        let mut index = Self::open(root)?;
        if index.images.remove(reference).is_none() {
            return Ok(false);
        }
        index.save()?;
        Ok(true)
    }

    /// Writes the index atomically and durably: a temporary file of its own
    /// (made exclusively), synced, renamed into place, and the directory
    /// synced so the rename itself survives a crash.
    fn save(&self) -> Result<(), IndexError> {
        use shipyard_sys::fs::Step;
        let io = |context: String| move |source| IndexError::Io { context, source };
        let path = self.root.join(INDEX_FILE);
        let json = serde_json::to_vec_pretty(&self.images).map_err(|source| {
            io("serializing the store index".to_owned())(std::io::Error::other(source))
        })?;
        shipyard_sys::fs::write_atomically(&path, &json, &shipyard_sys::fs::Write::new(0o666))
            .map_err(|error| {
                let context = match error.step {
                    Step::Open | Step::Write => format!(
                        "writing {}",
                        error
                            .temporary
                            .as_ref()
                            .map_or_else(|| path.clone(), |temporary| self.root.join(temporary))
                            .display()
                    ),
                    Step::Rename => format!("placing {}", path.display()),
                    Step::SyncDirectory => format!("syncing {}", self.root.display()),
                };
                io(context)(error.source)
            })
    }
}

/// Takes the index's lock (exclusive, short: a read-modify-write).
fn lock(root: &Path) -> Result<nix::fcntl::Flock<std::os::fd::OwnedFd>, IndexError> {
    let failed = |errno: nix::errno::Errno| match errno {
        nix::errno::Errno::EWOULDBLOCK => IndexError::Busy,
        errno => IndexError::Io {
            context: format!("locking {}", root.join(INDEX_LOCK).display()),
            source: errno.into(),
        },
    };
    let dir = crate::fsat::open_dir(root).map_err(failed)?;
    crate::fsat::lock_file(&dir, INDEX_LOCK, LockMode::Exclusive, INDEX_WAIT).map_err(failed)
}
