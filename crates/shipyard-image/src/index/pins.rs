//! Containers' pins: index entries under `container:<store>:<id>` that keep
//! the image a container was made from, whatever happens to its tag.

use std::path::{Path, PathBuf};

use super::{ImageRecord, Index, IndexError, PIN_PREFIX};

impl Index {
    /// Pins the image `record` for container `id` of container store
    /// `owner` (in the index under `root`): kept while the container
    /// exists.
    ///
    /// # Errors
    ///
    /// As [`record`](Self::record).
    pub fn pin(
        root: &Path,
        owner: &str,
        id: &str,
        record: ImageRecord,
    ) -> Result<Option<PathBuf>, IndexError> {
        Self::record(root, &format!("{PIN_PREFIX}{owner}:{id}"), record)
    }

    /// Lets go of container `id`'s pin (of container store `owner`),
    /// collecting nothing (the next collection takes what only it kept).
    /// Whether it had one.
    ///
    /// # Errors
    ///
    /// [`IndexError::Busy`] if the lock is held too long;
    /// [`IndexError::Io`] or [`IndexError::Corrupt`] if the index can't be
    /// read or written.
    pub fn unpin(root: &Path, owner: &str, id: &str) -> Result<bool, IndexError> {
        Self::forget(root, &format!("{PIN_PREFIX}{owner}:{id}"))
    }

    /// The containers that pin images: each one's store and ID.
    #[must_use]
    pub fn pins(&self) -> Vec<(String, String)> {
        self.images
            .keys()
            .filter_map(|reference| {
                let (owner, id) = reference.strip_prefix(PIN_PREFIX)?.split_once(':')?;
                Some((owner.to_owned(), id.to_owned()))
            })
            .collect()
    }

    /// The containers (their IDs) whose pins name the image whose manifest
    /// digest is `manifest`.
    #[must_use]
    pub fn pinned_by(&self, manifest: &str) -> Vec<String> {
        self.images
            .iter()
            .filter(|(_, record)| record.manifest == manifest)
            .filter_map(|(reference, _)| {
                let (_, id) = reference.strip_prefix(PIN_PREFIX)?.split_once(':')?;
                Some(id.to_owned())
            })
            .collect()
    }
}
