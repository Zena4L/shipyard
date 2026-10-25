//! Removing images and collecting garbage (blobs and layer directories no
//! image or running container needs), and the store's disk usage.

use std::collections::BTreeSet;
use std::path::Path;

use super::{
    CORRUPT_PREFIX, DiskUsage, INDEX_FILE, ImageRecord, Index, IndexError, LayerRemover, PIN_PREFIX,
};
use crate::digest::Digest;
use crate::store::{LayerOwners, LockMode, StoreLock};

impl Index {
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
        self.remove_all(&[reference.to_owned()], held, in_use, remove_layers)
    }

    /// [`remove`](Self::remove) for several references at once: all of
    /// them or none (one that isn't there: none is), the index saved once,
    /// the garbage collected once.
    ///
    /// # Errors
    ///
    /// As [`remove`](Self::remove).
    pub fn remove_all(
        &mut self,
        references: &[String],
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
        // A container's pin is never removed by reference (`rmi`): only by
        // its container's removal.
        if let Some(missing) = references.iter().find(|reference| {
            reference.starts_with(PIN_PREFIX) || !self.images.contains_key(reference.as_str())
        }) {
            return Err(IndexError::NoSuchImage(missing.clone()));
        }
        for reference in references {
            self.images.remove(reference);
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

        // A load's staging directory (`blobs/.load-<pid>-<n>`) a killed load
        // left: its lock free.
        crate::archive::staging::sweep_staging(&self.root.join("blobs"));

        // Extracted layer directories (`layers/<hex>`, and its `-caller`,
        // `-map-` and `-on-` forms) no image or running container needs, and
        // leftovers. Anything else
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
}

/// Whether the layer directory `name` is one nothing needs: what a removal
/// cut short left, or a layer no image references (`referenced`, by digest
/// hex) and no running container stacks (`in_use`). Garbage collection
/// removes these; `system df` counts them reclaimable.
pub(super) fn layer_unneeded(
    name: &str,
    in_use: &BTreeSet<String>,
    referenced: &BTreeSet<String>,
) -> bool {
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
