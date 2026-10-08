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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::store::{LayerOwners, LockMode, StoreLock};

/// The index file's name within the store root.
const INDEX_FILE: &str = "index.json";
/// The lock file `record`'s read-modify-write takes.
const INDEX_LOCK: &str = "index.lock";
/// How long recording waits for another process's record.
const INDEX_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// The prefix a corrupt index is moved aside to.
const CORRUPT_PREFIX: &str = "index.json.corrupt-";

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
        let images = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| IndexError::Corrupt {
                path: path.display().to_string(),
                source,
            })?,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
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

    /// The record of `reference`, if the index has one.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<&ImageRecord> {
        self.images.get(reference)
    }

    /// The images present, by reference.
    #[must_use]
    pub fn images(&self) -> Vec<ListedImage> {
        self.images
            .iter()
            .map(|(reference, record)| ListedImage {
                reference: reference.clone(),
                record: record.clone(),
            })
            .collect()
    }

    /// Removes `reference` and garbage-collects the blobs and layer
    /// directories nothing needs any more: no remaining image, and no running
    /// container (`in_use`, the layer directories live areas stack). Returns
    /// the blob digests removed. Layer directories go through
    /// `remove_layers` (with their names), since removing one may take the
    /// user namespace its files were made in.
    ///
    /// Hold the store's lock exclusively (see the module documentation).
    ///
    /// # Errors
    ///
    /// [`IndexError::NoSuchImage`] if nothing matches;
    /// [`IndexError::CorruptMovedAside`] while a corrupt index waits to be
    /// looked at; [`IndexError::Io`] on a filesystem failure.
    pub fn remove(
        &mut self,
        reference: &str,
        held: &StoreLock,
        in_use: &BTreeSet<String>,
        remove_layers: &mut LayerRemover<'_>,
    ) -> Result<Vec<String>, IndexError> {
        debug_assert_eq!(
            held.mode(),
            LockMode::Exclusive,
            "collection needs the lock alone"
        );
        self.refuse_after_corruption()?;
        if self.images.remove(reference).is_none() {
            return Err(IndexError::NoSuchImage(reference.to_owned()));
        }
        self.save()?;
        self.collect_garbage(in_use, remove_layers)
    }

    /// Removes every blob and layer directory nothing needs, as
    /// [`remove`](Self::remove) does after its image is gone.
    ///
    /// Hold the store's lock exclusively (see the module documentation).
    ///
    /// # Errors
    ///
    /// As [`remove`](Self::remove), but for a missing image.
    pub fn prune(
        &mut self,
        held: &StoreLock,
        in_use: &BTreeSet<String>,
        remove_layers: &mut LayerRemover<'_>,
    ) -> Result<Vec<String>, IndexError> {
        debug_assert_eq!(
            held.mode(),
            LockMode::Exclusive,
            "collection needs the lock alone"
        );
        self.refuse_after_corruption()?;
        self.collect_garbage(in_use, remove_layers)
    }

    /// Refuses garbage collection while a corrupt index waits to be looked
    /// at: the images it listed would all look unreferenced. (And clears
    /// the index's own temporaries a crash left.)
    fn refuse_after_corruption(&self) -> Result<(), IndexError> {
        for entry in read_dir(&self.root)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(CORRUPT_PREFIX) {
                return Err(IndexError::CorruptMovedAside(
                    entry.path().display().to_string(),
                ));
            }
            if name.starts_with(&format!(".{INDEX_FILE}.")) && is_temporary(&name) {
                remove_file(&entry.path())?;
            }
        }
        Ok(())
    }

    /// The set of blob digests any image still references.
    fn referenced_digests(&self) -> BTreeSet<&String> {
        self.images
            .values()
            .flat_map(ImageRecord::blob_digests)
            .collect()
    }

    /// The set of layer digests any image still references (the extracted
    /// directories are named by layer digest hex).
    fn referenced_layers(&self) -> BTreeSet<String> {
        self.images
            .values()
            .flat_map(|record| record.layers.iter())
            .filter_map(|digest| Digest::parse(digest).ok().map(|d| d.hex()))
            .collect()
    }

    /// Deletes unreferenced blobs and layer directories, and what writes and
    /// removals cut short left (under the exclusive lock, nothing is
    /// mid-way).
    fn collect_garbage(
        &self,
        in_use: &BTreeSet<String>,
        remove_layers: &mut LayerRemover<'_>,
    ) -> Result<Vec<String>, IndexError> {
        let referenced = self.referenced_digests();
        let mut removed = Vec::new();

        // Blobs: blobs/sha256/<hex>, temporaries (`.<hex>.<pid>.<n>.tmp`)
        // and partial downloads (`.<hex>.partial`).
        let blobs_dir = self.root.join("blobs").join("sha256");
        if blobs_dir.is_dir() {
            for entry in read_dir(&blobs_dir)? {
                let name = entry.file_name().to_string_lossy().into_owned();
                // A write cut short (`.<hex>.<pid>.<n>.tmp`) or a download
                // a pull stopped in (`.<hex>.partial`).
                if name.starts_with('.') && is_temporary(&name) {
                    remove_file(&entry.path())?;
                    continue;
                }
                let digest = format!("sha256:{name}");
                if Digest::parse(&digest).is_ok() && !referenced.contains(&digest) {
                    remove_file(&entry.path())?;
                    removed.push(digest);
                }
            }
        }

        // Extracted layer directories (`layers/<hex>`, `<hex>-caller`) no
        // image or running container needs, and leftovers. Anything else
        // there isn't the store's.
        let referenced_layers = self.referenced_layers();
        let layers_dir = self.root.join("layers");
        if layers_dir.is_dir() {
            let unreferenced: Vec<String> = read_dir(&layers_dir)?
                .into_iter()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| layer_unneeded(name, in_use, &referenced_layers))
                .collect();
            if !unreferenced.is_empty() {
                remove_layers(&unreferenced).map_err(IndexError::RemoveLayers)?;
            }
        }
        Ok(removed)
    }

    /// The store's disk usage: its blobs and extracted layers, and how much
    /// of each is reclaimable right now (no image references it; for a
    /// layer, no running container in `in_use` stacks it either).
    ///
    /// # Errors
    ///
    /// [`IndexError::Io`] if the blobs directory can't be read.
    pub fn disk_usage(&self, in_use: &BTreeSet<String>) -> Result<DiskUsage, IndexError> {
        let referenced = self.referenced_digests();
        let mut usage = DiskUsage {
            complete: true,
            ..DiskUsage::default()
        };
        let blobs_dir = self.root.join("blobs").join("sha256");
        if blobs_dir.is_dir() {
            for entry in read_dir(&blobs_dir)? {
                // Temporaries aren't images.
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let size = match entry.metadata() {
                    Ok(meta) => meta.len(),
                    // Removed meanwhile (an `rmi`, a prune).
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(IndexError::Io {
                            context: format!("statting {}", entry.path().display()),
                            source,
                        });
                    }
                };
                usage.blob_bytes += size;
                usage.blob_count += 1;
                let digest = format!("sha256:{}", entry.file_name().to_string_lossy());
                if !referenced.contains(&digest) {
                    usage.blob_reclaimable += size;
                }
            }
        }
        let layers_dir = self.root.join("layers");
        if layers_dir.is_dir() {
            let referenced_layers = self.referenced_layers();
            let dir = crate::fsat::open_dir(&layers_dir).map_err(|errno| IndexError::Io {
                context: format!("opening {}", layers_dir.display()),
                source: errno.into(),
            })?;
            for entry in read_dir(&layers_dir)? {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let taken = crate::fsat::tree_usage(&dir, std::ffi::OsStr::new(&name));
                usage.complete &= taken.complete;
                usage.layer_bytes += taken.bytes;
                if !crate::fsat::is_leftover(&name) {
                    usage.layer_count += 1;
                }
                if layer_unneeded(&name, in_use, &referenced_layers) {
                    usage.layer_reclaimable += taken.bytes;
                }
            }
        }
        Ok(usage)
    }

    /// Writes the index atomically and durably: a temporary file of its own
    /// (made exclusively), synced, renamed into place, and the directory
    /// synced so the rename itself survives a crash.
    fn save(&self) -> Result<(), IndexError> {
        use std::io::Write as _;
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let io = |context: String| move |source| IndexError::Io { context, source };
        let path = self.root.join(INDEX_FILE);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = self
            .root
            .join(format!(".{INDEX_FILE}.{}.{nonce}.tmp", std::process::id()));
        let json = serde_json::to_vec_pretty(&self.images).map_err(|source| {
            io("serializing the store index".to_owned())(std::io::Error::other(source))
        })?;
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .and_then(|mut file| {
                file.write_all(&json)?;
                file.sync_all()
            });
        if let Err(source) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(format!("writing {}", temporary.display()))(source));
        }
        std::fs::rename(&temporary, &path).map_err(io(format!("placing {}", path.display())))?;
        std::fs::File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .map_err(io(format!("syncing {}", self.root.display())))
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

/// Whether the layer directory `name` is one nothing needs: what a removal
/// cut short left, or a layer no image references (`referenced`, by digest
/// hex) and no running container stacks (`in_use`). Garbage collection
/// removes these; `system df` counts them reclaimable.
fn layer_unneeded(name: &str, in_use: &BTreeSet<String>, referenced: &BTreeSet<String>) -> bool {
    crate::fsat::is_leftover(name)
        || (!in_use.contains(name)
            && LayerOwners::digest_hex(name).is_some_and(|hex| !referenced.contains(hex)))
}

/// Whether `name` is one of the store's own temporaries (`….tmp`), or a
/// download a pull stopped in (`….partial`): always written so, lowercase.
/// Under the exclusive lock no pull is writing either.
fn is_temporary(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "tmp" || extension == "partial")
}

/// Reads a directory, collecting entries, naming it on failure.
fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>, IndexError> {
    std::fs::read_dir(dir)
        .map_err(|source| IndexError::Io {
            context: format!("reading {}", dir.display()),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| IndexError::Io {
            context: format!("reading an entry of {}", dir.display()),
            source,
        })
}

/// Removes a file, naming it on failure.
fn remove_file(path: &Path) -> Result<(), IndexError> {
    std::fs::remove_file(path).map_err(|source| IndexError::Io {
        context: format!("removing {}", path.display()),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("shipyard-index-{}-{n}", std::process::id()));
            std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
            std::fs::create_dir_all(dir.join("layers")).unwrap();
            // Private, as a store's root must be (for its lock).
            std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
            Self(dir)
        }

        /// Writes a fake blob with the given hex name and byte length.
        fn blob(&self, hex: &str, len: usize) {
            std::fs::write(self.0.join("blobs/sha256").join(hex), vec![0u8; len]).unwrap();
        }

        /// Creates a fake extracted-layer directory.
        fn layer_dir(&self, hex: &str) {
            std::fs::create_dir_all(self.0.join("layers").join(hex)).unwrap();
        }

        /// The store's lock, exclusive, as a collection holds it.
        fn exclusive(&self) -> StoreLock {
            crate::Store::open(&self.0)
                .unwrap()
                .lock(LockMode::Exclusive, std::time::Duration::ZERO)
                .unwrap()
        }

        /// Removes layer directories by name, as the CLI's remover does.
        fn remover(
            &self,
        ) -> impl FnMut(&[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + use<>
        {
            let layers = self.0.join("layers");
            move |names: &[String]| {
                for name in names {
                    std::fs::remove_dir_all(layers.join(name))?;
                }
                Ok(())
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A record from short fake hex digests.
    fn record(manifest: &str, config: &str, layers: &[&str]) -> ImageRecord {
        let d = |hex: &str| format!("sha256:{}", hex.repeat(64 / hex.len().max(1)));
        ImageRecord {
            manifest: d(manifest),
            config: d(config),
            layers: layers.iter().map(|h| d(h)).collect(),
        }
    }

    fn hex(short: &str) -> String {
        short.repeat(64 / short.len().max(1))
    }

    #[test]
    fn images_are_listed_after_being_put() {
        let scratch = Scratch::new();
        Index::record(&scratch.0, "alpine:3.20", record("a", "b", &["c"])).unwrap();
        let images = Index::open(&scratch.0).unwrap().images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference, "alpine:3.20");
    }

    #[test]
    fn removing_an_image_gcs_only_its_unshared_blobs() {
        let scratch = Scratch::new();
        // Two images sharing layer "c"; image1 also has unique "a1"/"b1".
        scratch.blob(&hex("a1"), 10);
        scratch.blob(&hex("b1"), 10);
        scratch.blob(&hex("a2"), 10);
        scratch.blob(&hex("b2"), 10);
        scratch.blob(&hex("c"), 100);
        scratch.layer_dir(&hex("c"));

        Index::record(&scratch.0, "img1", record("a1", "b1", &["c"])).unwrap();
        Index::record(&scratch.0, "img2", record("a2", "b2", &["c"])).unwrap();
        let mut index = Index::open(&scratch.0).unwrap();

        let removed = index
            .remove(
                "img1",
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover(),
            )
            .unwrap();
        // img1's manifest and config go; the shared layer "c" stays.
        assert!(removed.iter().any(|d| d.contains(&hex("a1"))));
        assert!(removed.iter().any(|d| d.contains(&hex("b1"))));
        assert!(
            !removed.iter().any(|d| d.contains(&hex("c"))),
            "shared layer kept"
        );
        assert!(scratch.0.join("blobs/sha256").join(hex("c")).exists());
        assert!(scratch.0.join("layers").join(hex("c")).exists());
        // img2's blobs survive.
        assert!(scratch.0.join("blobs/sha256").join(hex("a2")).exists());
    }

    #[test]
    fn removing_the_last_image_gcs_the_layer_dir_too() {
        let scratch = Scratch::new();
        scratch.blob(&hex("c"), 100);
        scratch.layer_dir(&hex("c"));
        Index::record(&scratch.0, "only", record("a", "b", &["c"])).unwrap();
        let mut index = Index::open(&scratch.0).unwrap();
        index
            .remove(
                "only",
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover(),
            )
            .unwrap();
        assert!(
            !scratch.0.join("layers").join(hex("c")).exists(),
            "layer dir gone"
        );
    }

    #[test]
    fn garbage_collection_leaves_what_isnt_the_stores() {
        let scratch = Scratch::new();
        scratch.layer_dir("lost+found");
        scratch.layer_dir(".tmp-in-progress");
        scratch.layer_dir(&format!("{}-caller", hex("d")));
        scratch.layer_dir(".trash-left-1-0");
        let mut index = Index::open(&scratch.0).unwrap();
        index
            .prune(
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover(),
            )
            .unwrap();
        let layers = scratch.0.join("layers");
        assert!(layers.join("lost+found").exists(), "not the store's");
        assert!(
            !layers.join(".tmp-in-progress").exists(),
            "under the exclusive lock, no extraction is in progress"
        );
        assert!(!layers.join(format!("{}-caller", hex("d"))).exists());
        assert!(!layers.join(".trash-left-1-0").exists());
    }

    #[test]
    fn removing_an_unknown_image_is_an_error() {
        let scratch = Scratch::new();
        let mut index = Index::open(&scratch.0).unwrap();
        assert!(matches!(
            index.remove(
                "ghost",
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover()
            ),
            Err(IndexError::NoSuchImage(_))
        ));
    }

    #[test]
    fn prune_reports_reclaimable_and_removes_orphans() {
        let scratch = Scratch::new();
        scratch.blob(&hex("a"), 10); // referenced
        scratch.blob(&hex("f"), 50); // orphan
        Index::record(&scratch.0, "img", record("a", "a", &["a"])).unwrap();
        let mut index = Index::open(&scratch.0).unwrap();

        let usage = index.disk_usage(&BTreeSet::new()).unwrap();
        assert_eq!(usage.blob_count, 2);
        assert_eq!(usage.blob_reclaimable, 50);

        index
            .prune(
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover(),
            )
            .unwrap();
        assert!(
            !scratch.0.join("blobs/sha256").join(hex("f")).exists(),
            "orphan pruned"
        );
        assert!(
            scratch.0.join("blobs/sha256").join(hex("a")).exists(),
            "referenced kept"
        );
    }

    #[test]
    fn the_index_survives_reopening() {
        let scratch = Scratch::new();
        Index::record(&scratch.0, "img", record("a", "b", &["c"])).unwrap();
        let index = Index::open(&scratch.0).unwrap();
        assert_eq!(index.images().len(), 1);
    }

    #[test]
    fn records_made_at_once_are_all_kept() {
        let scratch = Scratch::new();
        std::thread::scope(|threads| {
            for n in 0..8 {
                let root = &scratch.0;
                threads.spawn(move || {
                    Index::record(root, &format!("img{n}"), record("a", "b", &["c"])).unwrap();
                });
            }
        });
        assert_eq!(Index::open(&scratch.0).unwrap().images().len(), 8);
    }

    #[test]
    fn a_corrupt_index_is_moved_aside_and_stops_collection() {
        let scratch = Scratch::new();
        scratch.blob(&hex("a"), 10);
        std::fs::write(scratch.0.join(INDEX_FILE), "{ not json").unwrap();
        let moved = Index::record(&scratch.0, "img", record("b", "b", &["b"])).unwrap();
        assert!(moved.is_some_and(|path| path.exists()));
        let mut index = Index::open(&scratch.0).unwrap();
        assert!(matches!(
            index.prune(
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover()
            ),
            Err(IndexError::CorruptMovedAside(_))
        ));
        assert!(
            scratch.0.join("blobs/sha256").join(hex("a")).exists(),
            "nothing collected while the old index waits"
        );
    }

    #[test]
    fn layers_running_containers_use_are_kept() {
        let scratch = Scratch::new();
        scratch.layer_dir(&hex("c"));
        scratch.layer_dir(&format!("{}-caller", hex("d")));
        let mut index = Index::open(&scratch.0).unwrap();
        let in_use = BTreeSet::from([hex("c"), format!("{}-caller", hex("d"))]);
        index
            .prune(&scratch.exclusive(), &in_use, &mut scratch.remover())
            .unwrap();
        assert!(scratch.0.join("layers").join(hex("c")).exists());
        assert!(
            scratch
                .0
                .join("layers")
                .join(format!("{}-caller", hex("d")))
                .exists()
        );
    }

    #[test]
    fn temporaries_go_and_are_not_reported() {
        let scratch = Scratch::new();
        let temporary = format!(".{}.1.0.tmp", hex("e"));
        scratch.blob(&temporary, 5);
        let partial = format!(".{}.partial", hex("f"));
        scratch.blob(&partial, 5);
        let mut index = Index::open(&scratch.0).unwrap();
        let removed = index
            .prune(
                &scratch.exclusive(),
                &BTreeSet::new(),
                &mut scratch.remover(),
            )
            .unwrap();
        assert!(removed.is_empty(), "{removed:?}");
        assert!(!scratch.0.join("blobs/sha256").join(temporary).exists());
        assert!(!scratch.0.join("blobs/sha256").join(partial).exists());
    }
}
