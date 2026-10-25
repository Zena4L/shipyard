//! A load's or an import's staging: a directory of its own under the
//! store's `blobs/` (`.load-<pid>-<n>-<nanos>`), made as `….new` and renamed
//! once its `flock` is held, where blobs are written checked against their
//! digests before any reaches the store (`Staging::promote`, under the
//! store's shared lock); one a killed load left, swept when its lock is free
//! (`sweep_staging`: by the next load and the store's collection).

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::ArchiveError;
use crate::digest::{Digest, Verifier};
use crate::store::Store;
use crate::unpack::FreeSpace;

impl Staging {
    /// Moves the staged blobs `digests` into `store` (its shared lock the
    /// caller's): [`Archive::promote`](super::load::Archive::promote)'s, and an import's.
    pub(crate) fn promote(&self, store: &Store, digests: &[Digest]) -> Result<(), ArchiveError> {
        for digest in digests {
            if store.contains(digest) {
                continue;
            }
            let from = self.path(digest);
            let to = store.path(digest);
            fs::rename(&from, &to).map_err(|source| {
                ArchiveError::io(format!("placing the blob {digest} in the store"), source)
            })?;
        }
        let dir = store.root().join("blobs").join("sha256");
        File::open(&dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|source| ArchiveError::io(format!("syncing {}", dir.display()), source))
    }
}

/// A load's blobs before they're the store's: a directory of its own in the
/// store's `blobs/` (`.load-<pid>-<n>-<nanos>`), held by a lock in it for
/// as long as the load runs. Removed when dropped; one a killed load left is
/// swept, once its lock is free, by the next load or the store's collection.
#[derive(Debug)]
pub(crate) struct Staging {
    dir: PathBuf,
    _lock: nix::fcntl::Flock<File>,
    /// The staged blobs' digests (`sha256:<hex>`).
    staged: BTreeSet<String>,
}

/// The prefix of a staging directory's name.
pub const STAGING_PREFIX: &str = ".load-";

/// The lock file in a staging directory.
pub const STAGING_LOCK: &str = ".lock";

/// The suffix of a staging directory still being made: its lock not yet
/// held, so the sweep can't tell a live one from a dead one by it.
pub const STAGING_NEW: &str = ".new";

/// How old a staging directory still being made must be for the sweep to
/// take it for one a load was killed making (it takes a live one a moment).
pub const STAGING_NEW_STALE: std::time::Duration = std::time::Duration::from_secs(600);

impl Staging {
    /// A new staging directory in `store`'s `blobs/`: made under a name
    /// of its own (`….new`) until its lock is held, then renamed, so the
    /// sweep never sees it unlocked under its final name.
    pub(crate) fn new(store: &Store) -> Result<Self, ArchiveError> {
        use nix::fcntl::{Flock, FlockArg};
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let blobs = store.root().join("blobs");
        // What killed loads left (their locks free), gone first: else it
        // waits for the next `rmi` or prune, its bytes counted nowhere. Safe
        // without the store's lock: a live load's is always held.
        sweep_staging(&blobs);
        // The time too: a PID used again never meets an old name.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let dir = blobs.join(format!(
            "{STAGING_PREFIX}{}-{}-{nanos}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut new = dir.clone().into_os_string();
        new.push(STAGING_NEW);
        let new = PathBuf::from(new);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&new)
            .map_err(|source| ArchiveError::io(format!("making {}", new.display()), source))?;
        let made = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(new.join(STAGING_LOCK))
            .map_err(|source| ArchiveError::io("making the load's lock", source))
            .and_then(|lock| {
                Flock::lock(lock, FlockArg::LockExclusiveNonblock)
                    .map_err(|(_, errno)| ArchiveError::io("locking the load", errno.into()))
            })
            .and_then(|lock| {
                fs::rename(&new, &dir)
                    .map(|()| lock)
                    .map_err(|source| ArchiveError::io(format!("making {}", dir.display()), source))
            });
        let lock = match made {
            Ok(lock) => lock,
            Err(error) => {
                let _ = fs::remove_dir_all(&new);
                return Err(error);
            }
        };
        Ok(Self {
            dir,
            _lock: lock,
            staged: BTreeSet::new(),
        })
    }

    /// Where a staged blob is.
    fn path(&self, digest: &Digest) -> PathBuf {
        self.dir.join(digest.hex())
    }

    /// Whether `digest` is staged.
    pub(super) fn has(&self, digest: &Digest) -> bool {
        self.staged.contains(&digest.to_string())
    }

    /// A staged blob's bytes, at most `limit`.
    pub(super) fn read(&self, digest: &Digest, limit: u64) -> Result<Vec<u8>, ArchiveError> {
        shipyard_sys::fs::read_bounded(&self.path(digest), limit)
            .map_err(|source| ArchiveError::io(format!("reading the blob {digest}"), source))
    }

    /// Stages `size` bytes of `from` as the blob `<hex>`: checked against
    /// it. One staged already is skipped (its bytes are its digest's).
    pub(super) fn stage_named(
        &mut self,
        hex: &str,
        from: &mut impl Read,
        size: u64,
    ) -> Result<Digest, ArchiveError> {
        let digest = Digest::parse(&format!("sha256:{hex}"))?;
        if self.has(&digest) {
            return Ok(digest);
        }
        let got = self.stage(from, size)?;
        if got != digest {
            return Err(ArchiveError::Malformed(format!(
                "its blob {digest} holds the bytes of {got}"
            )));
        }
        Ok(digest)
    }

    /// Stages `size` bytes of `from` under the digest they have: written to
    /// a temporary file (`.<n>.tmp`), hashing them; synced and read-only (as
    /// the store's blobs are), then named by their digest. (Not the store's
    /// `BlobWriter`: its digest is known before, its file in the store.) The
    /// free space checked first, from the size the tar's header says.
    pub(super) fn stage(
        &mut self,
        from: &mut impl Read,
        size: u64,
    ) -> Result<Digest, ArchiveError> {
        let space = File::open(&self.dir)
            .map_err(|source| ArchiveError::io("opening the load's directory", source))?;
        let free = FreeSpace::of(&space)
            .map_err(|errno| ArchiveError::io("checking the free space", errno.into()))?;
        if free.available < size.saturating_add(free.reserve) {
            return Err(ArchiveError::NoSpace {
                available: free.available.saturating_sub(size),
                reserve: free.reserve,
            });
        }
        let mut writer = self.writer()?;
        let mut left = size;
        let mut buffer = vec![0u8; 64 * 1024];
        while left > 0 {
            let want = usize::try_from(left.min(buffer.len() as u64)).unwrap_or(buffer.len());
            let read = from.read(&mut buffer[..want]).map_err(ArchiveError::Read)?;
            if read == 0 {
                return Err(ArchiveError::Cut);
            }
            writer
                .write_all(&buffer[..read])
                .map_err(|source| writer.failed(source))?;
            left -= read as u64;
        }
        writer.commit()
    }

    /// A blob to stage as it's written, its size not known before (an
    /// import's layer): [`StageWriter::commit`] names it by its digest.
    pub(crate) fn writer(&mut self) -> Result<StageWriter<'_>, ArchiveError> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let temporary = self
            .dir
            .join(format!(".{}.tmp", COUNT.fetch_add(1, Ordering::Relaxed)));
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| ArchiveError::io("staging a blob", source))?;
        Ok(StageWriter {
            staging: self,
            temporary,
            file,
            verifier: Verifier::new(),
            written: 0,
            checked_at: 0,
            short: None,
        })
    }

    /// A staged blob's size.
    pub(crate) fn size(&self, digest: &Digest) -> Result<u64, ArchiveError> {
        fs::metadata(self.path(digest))
            .map(|meta| meta.len())
            .map_err(|source| ArchiveError::io(format!("reading the blob {digest}"), source))
    }

    /// Whether a staged blob starts with `magic`.
    pub(crate) fn starts_with(&self, digest: &Digest, magic: &[u8]) -> Result<bool, ArchiveError> {
        let mut head = vec![0u8; magic.len()];
        let read = File::open(self.path(digest))
            .and_then(|mut file| file.read(&mut head))
            .map_err(|source| ArchiveError::io(format!("reading the blob {digest}"), source))?;
        Ok(read == magic.len() && head == magic)
    }

    /// Stages `bytes` (a manifest made for a legacy image).
    pub(crate) fn stage_bytes(&mut self, bytes: &[u8]) -> Result<Digest, ArchiveError> {
        self.stage(&mut &bytes[..], bytes.len() as u64)
    }
}

/// A blob being staged ([`Staging::writer`]): hashed as it's written, the
/// store's free-space reserve checked every [`StageWriter::CHECK_EVERY`]
/// bytes.
pub(crate) struct StageWriter<'s> {
    staging: &'s mut Staging,
    temporary: PathBuf,
    file: File,
    verifier: Verifier,
    written: u64,
    checked_at: u64,
    /// The free space, and the reserve, when a check found it short.
    short: Option<(u64, u64)>,
}

impl StageWriter<'_> {
    /// How often the free space is checked, in bytes written.
    const CHECK_EVERY: u64 = 64 * 1024 * 1024;

    /// The free space and the reserve, if a check found the store full.
    pub(crate) fn short(&self) -> Option<(u64, u64)> {
        self.short
    }

    /// A write's failure as the archive's: the store full (as its check
    /// found it), or the write's own.
    pub(crate) fn failed(&self, source: std::io::Error) -> ArchiveError {
        match self.short {
            Some((available, reserve)) => ArchiveError::NoSpace { available, reserve },
            None => ArchiveError::io("staging a blob", source),
        }
    }

    /// Synced, read-only (as the store's blobs are), and named by its
    /// digest: that digest.
    pub(crate) fn commit(mut self) -> Result<Digest, ArchiveError> {
        self.file
            .sync_all()
            .and_then(|()| {
                self.file
                    .set_permissions(fs::Permissions::from_mode(crate::store::BLOB_MODE))
            })
            .map_err(|source| ArchiveError::io("staging a blob", source))?;
        let digest = std::mem::take(&mut self.verifier).finish();
        fs::rename(&self.temporary, self.staging.path(&digest))
            .map_err(|source| ArchiveError::io("staging a blob", source))?;
        self.staging.staged.insert(digest.to_string());
        Ok(digest)
    }
}

impl std::io::Write for StageWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written >= self.checked_at {
            let free = FreeSpace::of(&self.file).map_err(std::io::Error::from)?;
            // Room for what's written until the next check, not only this.
            let until_next = Self::CHECK_EVERY.saturating_add(buf.len() as u64);
            if free.available < free.reserve.saturating_add(until_next) {
                self.short = Some((free.available, free.reserve));
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
            self.checked_at = self.written.saturating_add(Self::CHECK_EVERY);
        }
        let written = self.file.write(buf)?;
        self.verifier.update(&buf[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for StageWriter<'_> {
    fn drop(&mut self) {
        // Committed, it's gone from here already; else a part never named.
        let _ = fs::remove_file(&self.temporary);
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Removes the staging directories in `blobs` whose lock is free (a load
/// killed before its end): the store's collection's, under its exclusive
/// lock. One a live load holds is left, as is one still being made
/// (`….new`) unless it's past [`STAGING_NEW_STALE`] (a load killed making
/// it).
pub(crate) fn sweep_staging(blobs: &Path) {
    use nix::fcntl::{Flock, FlockArg};
    let Ok(entries) = fs::read_dir(blobs) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(STAGING_PREFIX) {
            continue;
        }
        let dir = entry.path();
        if name.to_string_lossy().ends_with(STAGING_NEW) {
            let stale = entry
                .metadata()
                .and_then(|made| made.modified())
                .is_ok_and(|made| made.elapsed().is_ok_and(|age| age > STAGING_NEW_STALE));
            if stale {
                let _ = fs::remove_dir_all(&dir);
            }
            continue;
        }
        let held = File::open(dir.join(STAGING_LOCK))
            .ok()
            .map(|lock| Flock::lock(lock, FlockArg::LockExclusiveNonblock).is_err());
        // No lock file (killed before it was made), or one no one holds.
        if held != Some(true) {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
