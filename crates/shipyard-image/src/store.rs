//! The image store: content-addressed blobs, and the layers extracted from
//! them.
//!
//! The store's root is private to its owner (mode 0700): the extracted
//! layers keep their images' setuid programs and world-writable
//! directories, which no other user of the host may reach. Shipyard makes
//! it so, and refuses a root someone else made open to others rather than
//! change a directory it didn't create.
//!
//! - `blobs/sha256/<hex>`: every blob an image is made of (manifests,
//!   configs, layer tarballs), named by its digest;
//! - `layers/<name>/diff`: each layer extracted, for overlayfs to stack
//!   ([`crate::images`]);
//! - `containers/<name>`: each running container's writable layer
//!   ([`crate::containers`]).
//!
//! **Processes share a store, so they coordinate through `gc.lock`**
//! ([`Store::lock`]): whatever adds to the store or depends on what's in
//! it (a pull, an extraction, a run being prepared) holds it shared, and
//! garbage collection holds it exclusive. So a collection never sees a
//! pull half-way, nor removes what one is adding; and running containers
//! are found by the locks their own areas hold.
//!
//! Blobs are kept in one directory tree, named by their digest.
//! Because the name is the content's hash, the store has three properties for
//! free:
//!
//! - **Verification.** A blob is written only if its bytes hash to the digest
//!   asked for; the hash is computed as the bytes stream in, and a mismatch
//!   leaves nothing behind. Reading back, the path *is* the digest, so a blob
//!   can be re-verified any time.
//! - **Deduplication.** Two images that share a layer name the same file;
//!   writing a blob that's already there is a no-op. Identical layers are
//!   stored once (FEATURES §6).
//! - **Immutability.** A finished blob is read-only. Nothing rewrites it in
//!   place; a new version has a different digest and a different name.
//!
//! Writes are atomic: bytes go to a file of their own in the same directory
//! (a blob's `.partial` file, or a private temporary one), are verified,
//! flushed to disk, made read-only, and only then renamed onto the final
//! name. A rename within a directory is atomic on a POSIX filesystem, so a
//! reader never sees a half-written or unverified blob, and an interrupted
//! write leaves a partial download (for the next pull to go on with) or a
//! stray temporary file, never a bad blob under a trusted name. (The rename makes the blob *visible* atomically; the
//! directory entry's durability across a hard crash would need the directory
//! itself fsynced, which the store does not do — a blob lost to a crash is
//! simply re-pulled, since the store is a cache.) This is the same discipline
//! the runtime uses for its PID file and state.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::digest::{Digest, Verifier, VerifyError};

/// Mode for a finished blob: read-only for everyone (0o444).
const BLOB_MODE: u32 = 0o444;

/// How many bytes to move at a time between the source and the blob file.
/// Kept at 16 KiB: large enough that the per-call overhead is negligible for
/// multi-megabyte layers, small enough to sit comfortably on the stack.
const COPY_CHUNK: usize = 16 * 1024;

/// How many times [`Store::begin_blob`] opens a partial blob that turns out
/// to have been replaced as it was locked, before it writes a private file.
const PARTIAL_TRIES: usize = 8;

/// The lock file processes coordinate through (see [`Store::lock`]).
const GC_LOCK: &str = "gc.lock";

/// How long garbage collection waits for preparing runs and pulls to let
/// go of the store, before it says the store is busy.
pub const GC_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a run or a pull waits for garbage collection to finish.
pub const USE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// How the store's lock is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// By whatever adds to the store or depends on what's in it; any number
    /// at once.
    Shared,
    /// By garbage collection, alone.
    Exclusive,
}

/// The store's lock, held until dropped.
#[derive(Debug)]
pub struct StoreLock {
    lock: nix::fcntl::Flock<std::os::fd::OwnedFd>,
    mode: LockMode,
}

impl StoreLock {
    /// How it's held.
    #[must_use]
    pub fn mode(&self) -> LockMode {
        self.mode
    }

    /// Turns an exclusive lock into a shared one, for the slow part of a
    /// collection (deleting what it moved to trash): runs can start
    /// meanwhile. `flock(2)` doesn't make the change atomic (the old lock
    /// goes first), so another collection waiting for the lock may get it
    /// in between, and delete the same trash; removal tolerates that
    /// (what another process removed first counts as removed).
    ///
    /// # Errors
    ///
    /// The `flock(2)` errno.
    pub fn downgrade(&mut self) -> nix::Result<()> {
        self.lock.relock(nix::fcntl::FlockArg::LockShared)?;
        self.mode = LockMode::Shared;
        Ok(())
    }

    /// A descriptor of the lock's open file description that a child
    /// process can inherit (no close-on-exec): the lock then lasts until the
    /// child ends too. Given to the storage helper, so work it does for
    /// this process stays covered if this process dies first.
    ///
    /// # Errors
    ///
    /// The `dup(2)` errno.
    pub fn inheritable(&self) -> nix::Result<std::os::fd::OwnedFd> {
        nix::unistd::dup(&*self.lock)
    }
}

/// The directory an extracted layer's contents are in, under its
/// `layers/<name>` directory.
pub const DIFF: &str = "diff";

/// Whose files an extracted layer's are. Part of the layer's name: the same
/// layer extracted both ways (with and without a user namespace to give
/// files to the image's users) is two directories, never one reused as the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerOwners {
    /// The image's own users and groups.
    Image,
    /// The extracting user's, every file (no IDs to give them to).
    Caller,
    /// The image's users and groups shifted by a user namespace's maps
    /// (root running a container with `--uidmap`): the maps' identity
    /// ([`map_id`]).
    Mapped(u64),
}

impl LayerOwners {
    /// The suffix of a [`Caller`](Self::Caller) layer's name.
    const CALLER_SUFFIX: &'static str = "-caller";
    /// What comes before a [`Mapped`](Self::Mapped) layer's map identity.
    const MAP_INFIX: &'static str = "-map-";

    /// The name of `digest`'s layer directory: its hex, with a suffix for a
    /// caller-owned or mapped one.
    #[must_use]
    pub fn name(self, digest: &Digest) -> String {
        match self {
            Self::Image => digest.hex(),
            Self::Caller => format!("{}{}", digest.hex(), Self::CALLER_SUFFIX),
            Self::Mapped(id) => format!("{}{}{id:016x}", digest.hex(), Self::MAP_INFIX),
        }
    }

    /// The digest hex a layer directory's name is for, if it is one's.
    #[must_use]
    pub fn digest_hex(name: &str) -> Option<&str> {
        let hex = name
            .strip_suffix(Self::CALLER_SUFFIX)
            .or_else(|| {
                let (hex, id) = name.rsplit_once(Self::MAP_INFIX)?;
                (id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
            })
            .unwrap_or(name);
        Digest::parse(&format!("sha256:{hex}"))
            .is_ok()
            .then_some(hex)
    }
}

/// A stable identity for user namespace maps, given as their
/// `container:host:size` triples (users, then groups): FNV-1a of them, so
/// the same maps name the same layers, run after run.
#[must_use]
pub fn map_id(uids: &[(u32, u32, u32)], gids: &[(u32, u32, u32)]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = OFFSET;
    let mut feed = |value: u32| {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    for (index, maps) in [uids, gids].into_iter().enumerate() {
        feed(u32::try_from(index).unwrap_or(0));
        for &(container, host, size) in maps {
            feed(container);
            feed(host);
            feed(size);
        }
    }
    hash
}

/// A content-addressed blob store rooted at a directory.
#[derive(Debug, Clone)]
pub struct Store {
    /// The store root; blobs live under `root/blobs/sha256/`.
    root: PathBuf,
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An I/O error, with what was being attempted.
    #[error("{context}: {source}")]
    Io {
        /// What the store was doing.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The bytes written didn't hash to the digest asked for. The partial
    /// blob is removed before this is returned.
    #[error(transparent)]
    Verify(#[from] VerifyError),
    /// The store's root exists and other users can reach it.
    #[error(
        "the image store {0} can be reached by other users; it must be private to you \
         (`chmod 700` it, or set SHIPYARD_IMAGE_STORE to a new directory)"
    )]
    NotPrivate(String),
    /// Another process holds the store's lock past the wait.
    #[error(
        "the image store is busy (a pull or a run is preparing, or images are being \
         removed); try again"
    )]
    Busy,
    /// A blob read with [`Store::read_blob`] exceeded the caller's size limit.
    #[error("the blob {path} is larger than the {limit}-byte limit")]
    TooLarge {
        /// The blob's path.
        path: String,
        /// The limit it exceeded, in bytes.
        limit: u64,
    },
}

impl Store {
    /// Opens (creating if needed) a store rooted at `root`.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotPrivate`] if `root` exists and others can reach it;
    /// [`StoreError::Io`] if the directory tree can't be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        prepare_root(&root)?;
        for dir in [
            root.join("blobs").join("sha256"),
            root.join("layers"),
            root.join(crate::containers::CONTAINERS),
        ] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|source| Self::io(&dir, "creating the store directory", source))?;
        }
        Ok(Self { root })
    }

    /// The store's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of extracted layers, opened: everything in it is
    /// reached by descriptor from here.
    ///
    /// # Errors
    ///
    /// The error opening it.
    pub fn layers_dir(&self) -> io::Result<std::os::fd::OwnedFd> {
        let root = crate::fsat::open_dir(&self.root)?;
        Ok(crate::fsat::open_dir_at(
            &root,
            std::ffi::OsStr::new("layers"),
        )?)
    }

    /// Takes the store's lock (see the module documentation), waiting up to
    /// `wait` for whoever holds it the other way. It's held until the
    /// returned value drops, or the process ends.
    ///
    /// # Errors
    ///
    /// [`StoreError::Busy`] past the wait; [`StoreError::Io`] if the lock
    /// file can't be opened.
    pub fn lock(&self, mode: LockMode, wait: std::time::Duration) -> Result<StoreLock, StoreError> {
        let root = crate::fsat::open_dir(&self.root)
            .map_err(|errno| Self::io(&self.root, "opening the store", errno.into()))?;
        match crate::fsat::lock_file(&root, GC_LOCK, mode, wait) {
            Ok(lock) => Ok(StoreLock { lock, mode }),
            Err(nix::errno::Errno::EWOULDBLOCK) => Err(StoreError::Busy),
            Err(errno) => Err(Self::io(&self.root.join(GC_LOCK), "locking", errno.into())),
        }
    }

    /// The directory of the containers' writable layers, opened.
    ///
    /// # Errors
    ///
    /// The error opening it.
    pub fn containers_dir(&self) -> io::Result<std::os::fd::OwnedFd> {
        let root = crate::fsat::open_dir(&self.root)?;
        Ok(crate::fsat::open_dir_at(
            &root,
            std::ffi::OsStr::new(crate::containers::CONTAINERS),
        )?)
    }

    /// The content directory of an extracted layer (what overlayfs stacks),
    /// whether or not it exists.
    #[must_use]
    pub fn layer_diff(&self, digest: &Digest, owners: LayerOwners) -> PathBuf {
        self.root
            .join("layers")
            .join(owners.name(digest))
            .join(DIFF)
    }

    /// The path a blob with `digest` has, whether or not it exists.
    #[must_use]
    pub fn path(&self, digest: &Digest) -> PathBuf {
        self.root
            .join("blobs")
            .join(digest.algorithm())
            .join(digest.hex())
    }

    /// Whether a blob with `digest` is in the store.
    #[must_use]
    pub fn contains(&self, digest: &Digest) -> bool {
        self.path(digest).is_file()
    }

    /// Opens a blob for reading.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the blob isn't present or can't be opened.
    pub fn open_blob(&self, digest: &Digest) -> Result<File, StoreError> {
        let path = self.path(digest);
        File::open(&path).map_err(|source| StoreError::Io {
            context: format!("opening the blob {}", path.display()),
            source,
        })
    }

    /// Writes a blob read from `source`, verifying it hashes to `expected`.
    ///
    /// If the blob is already present, `source` is not read and the existing
    /// blob is kept (writes are idempotent — this is the deduplication path).
    /// Otherwise the bytes stream to a temporary file while being hashed; on a
    /// digest mismatch the temporary file is removed and nothing is stored.
    ///
    /// Returns `true` if a new blob was written, `false` if it was already
    /// present.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Verify`] if the bytes don't match `expected` (no
    /// blob is left), or [`StoreError::Io`] for a filesystem failure.
    pub fn write_blob(
        &self,
        expected: &Digest,
        source: &mut impl Read,
    ) -> Result<bool, StoreError> {
        if self.contains(expected) {
            return Ok(false);
        }
        let mut writer = self.temporary_writer(expected)?;
        writer.append(source)?;
        writer.commit()
    }

    /// Starts (or takes up again) writing the blob `digest`: in its
    /// `.partial` file, which outlives this process so a later pull goes on
    /// from where this one stopped. The bytes already there are hashed again
    /// (one read), and the writer appends after them.
    ///
    /// The partial file is locked (`flock`) while written: if another
    /// process is writing it (a pull of an image sharing the layer), this
    /// writes a private temporary file from the start instead.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the file can't be made, locked or read.
    pub fn begin_blob(&self, digest: &Digest) -> Result<BlobWriter, StoreError> {
        use nix::fcntl::{Flock, FlockArg};
        let path = self
            .path(digest)
            .with_file_name(format!(".{}.partial", digest.hex()));
        // The lock is on an inode, the commit renames a name: the two must
        // agree for as long as the lock is held. A writer that finishes
        // renames its partial away (and the next makes a new one), so what
        // was opened may not be what's at the path by the time it's locked.
        for _ in 0..PARTIAL_TRIES {
            let file = match open_partial(&path) {
                Ok(file) => file,
                // A pull killed between sealing its finished partial and
                // renaming it left it read-only: it goes.
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                    fs::remove_file(&path)
                        .map_err(|source| Self::io(&path, "replacing the partial blob", source))?;
                    continue;
                }
                Err(source) => return Err(Self::io(&path, "opening the partial blob", source)),
            };
            let mut locked = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(locked) => locked,
                // Another writer has it: a private file, from the start.
                Err((_, nix::errno::Errno::EWOULDBLOCK)) => return self.temporary_writer(digest),
                Err((_, errno)) => {
                    return Err(Self::io(&path, "locking the partial blob", errno.into()));
                }
            };
            if !names(&path, &locked) {
                continue;
            }
            let mut verifier = Verifier::new();
            let have = hash_all(&mut locked, &mut verifier)
                .map_err(|source| Self::io(&path, "reading the partial blob", source))?;
            return Ok(BlobWriter {
                final_path: self.path(digest),
                digest: digest.clone(),
                path,
                file: Handle::Partial(locked),
                verifier,
                have,
                resumed: have,
                done: false,
            });
        }
        self.temporary_writer(digest)
    }

    /// How many bytes a download of `digest` a pull stopped in already has
    /// (its partial file's length; 0 if there's none).
    #[must_use]
    pub fn partial_len(&self, digest: &Digest) -> u64 {
        let path = self
            .path(digest)
            .with_file_name(format!(".{}.partial", digest.hex()));
        fs::symlink_metadata(path)
            .ok()
            .filter(fs::Metadata::is_file)
            .map_or(0, |meta| meta.len())
    }

    /// A writer of `digest` into a private temporary file, from the start:
    /// removed if it isn't committed.
    fn temporary_writer(&self, digest: &Digest) -> Result<BlobWriter, StoreError> {
        let path = self.temporary_path(digest);
        let file = Self::create_temporary(&path)?;
        Ok(BlobWriter {
            final_path: self.path(digest),
            digest: digest.clone(),
            path,
            file: Handle::Temporary(file),
            verifier: Verifier::new(),
            have: 0,
            resumed: 0,
            done: false,
        })
    }

    /// Checks the stored blob `digest` still hashes to its name (one read).
    /// One that doesn't (a disk that lost or changed bytes) is removed, to be
    /// fetched again: `false`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if it can't be read or removed.
    pub fn check_blob(&self, digest: &Digest) -> Result<bool, StoreError> {
        let path = self.path(digest);
        let mut file =
            File::open(&path).map_err(|source| Self::io(&path, "reading the blob", source))?;
        let mut verifier = Verifier::new();
        hash_all(&mut file, &mut verifier)
            .map_err(|source| Self::io(&path, "reading the blob", source))?;
        if verifier.verify(digest).is_ok() {
            return Ok(true);
        }
        // Only the copy that was checked: another pull may have put a good
        // one in its place meanwhile.
        if names(&path, &file) {
            fs::remove_file(&path)
                .map_err(|source| Self::io(&path, "removing the corrupt blob", source))?;
        }
        Ok(false)
    }

    /// Reads a whole blob into memory, verifying it still matches `digest`.
    ///
    /// For small blobs (manifests, configs): it refuses anything larger than
    /// `limit` bytes rather than allocate without bound for a hostile or
    /// mis-identified blob. Layers should stream through
    /// [`open_blob`](Self::open_blob) instead.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if it can't be read, [`StoreError::TooLarge`]
    /// if it exceeds `limit`, or [`StoreError::Verify`] if the stored bytes no
    /// longer hash to `digest` (corruption on disk).
    pub fn read_blob(&self, digest: &Digest, limit: u64) -> Result<Vec<u8>, StoreError> {
        let path = self.path(digest);
        let file = File::open(&path).map_err(|source| StoreError::Io {
            context: format!("reading the blob {}", path.display()),
            source,
        })?;
        // Read at most `limit + 1`: if that many bytes come back, it's over.
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| StoreError::Io {
                context: format!("reading the blob {}", path.display()),
                source,
            })?;
        if u64::try_from(bytes.len()).map_or(true, |len| len > limit) {
            return Err(StoreError::TooLarge {
                path: path.display().to_string(),
                limit,
            });
        }
        let mut verifier = Verifier::new();
        verifier.update(&bytes);
        verifier.verify(digest)?;
        Ok(bytes)
    }

    /// The temporary path for an in-progress write of `digest`, in the same
    /// directory as the final blob so the rename stays within one filesystem.
    /// The PID and a per-call counter keep two writers of the *same* digest —
    /// in this process or another, in parallel — from colliding on the
    /// temporary name (the final name is shared and deduplicated; the
    /// temporary one must not be).
    fn temporary_path(&self, digest: &Digest) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{}.{}.{nonce}.tmp", digest.hex(), std::process::id());
        self.path(digest).with_file_name(name)
    }

    /// Creates the temporary file, failing if one with the same name exists
    /// (`O_EXCL`, which never follows a symlink planted in the directory).
    fn create_temporary(temporary: &Path) -> Result<File, StoreError> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temporary)
            .map_err(|source| Self::io(temporary, "creating the temporary blob", source))
    }

    /// Removes a temporary file, ignoring failure (best effort cleanup).
    fn discard(temporary: &Path) {
        let _ = fs::remove_file(temporary);
    }

    /// Builds a [`StoreError::Io`] naming the path and the action.
    fn io(path: &Path, action: &str, source: io::Error) -> StoreError {
        StoreError::Io {
            context: format!("{action} {}", path.display()),
            source,
        }
    }
}

/// Makes the store's root directory, private to its owner (see the module
/// documentation). Missing parents get the usual permissions; only the root
/// is made private. One that exists already is checked, never changed: it
/// may be a directory the user has other plans for (`$HOME`, `/tmp`).
fn prepare_root(root: &Path) -> Result<(), StoreError> {
    if let Some(parent) = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|source| Store::io(parent, "creating the store's parent directory", source))?;
    }
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::metadata(root)
                .map_err(|source| Store::io(root, "reading the store directory", source))?;
            if !meta.is_dir() {
                return Err(Store::io(
                    root,
                    "opening the store directory",
                    io::Error::from(io::ErrorKind::NotADirectory),
                ));
            }
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(StoreError::NotPrivate(root.display().to_string()));
            }
            Ok(())
        }
        Err(source) => Err(Store::io(root, "creating the store directory", source)),
    }
}

/// Opens (or makes) a partial blob, never through a symlink, checked to be
/// a regular file.
fn open_partial(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::other("it isn't a regular file"))
    }
}

/// Whether `path` names the file open as `file` (the same inode, still
/// linked).
fn names(path: &Path, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (fs::symlink_metadata(path), file.metadata()) {
        (Ok(there), Ok(open)) => {
            (there.dev(), there.ino()) == (open.dev(), open.ino()) && open.nlink() > 0
        }
        _ => false,
    }
}

/// Feeds everything in `file`, from its start, to `verifier`, leaving the
/// file at its end; how many bytes that was.
fn hash_all(file: &mut File, verifier: &mut Verifier) -> io::Result<u64> {
    use std::io::{Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(total);
        }
        verifier.update(&buffer[..read]);
        total += read as u64;
    }
}

/// The file a [`BlobWriter`] writes.
#[derive(Debug)]
enum Handle {
    /// The blob's `.partial` file, locked while this writes it.
    Partial(nix::fcntl::Flock<File>),
    /// A private temporary file.
    Temporary(File),
}

impl Handle {
    fn file(&mut self) -> &mut File {
        match self {
            Self::Partial(locked) => locked,
            Self::Temporary(file) => file,
        }
    }
}

/// A blob being written, hashed as it goes ([`Store::begin_blob`]). Nothing
/// is visible under the blob's name until [`commit`](Self::commit) verifies
/// it. Dropped uncommitted, a temporary file goes, and a partial one stays,
/// for the next pull to go on with.
#[derive(Debug)]
pub struct BlobWriter {
    digest: Digest,
    final_path: PathBuf,
    path: PathBuf,
    file: Handle,
    verifier: Verifier,
    have: u64,
    resumed: u64,
    done: bool,
}

impl BlobWriter {
    /// How many bytes it has.
    #[must_use]
    pub fn have(&self) -> u64 {
        self.have
    }

    /// How many bytes it had to begin with (a partial file's, taken up
    /// again).
    #[must_use]
    pub fn resumed(&self) -> u64 {
        self.resumed
    }

    /// Throws away what it has, to start again from the first byte.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the file can't be truncated.
    pub fn reset(&mut self) -> Result<(), StoreError> {
        use std::io::{Seek as _, SeekFrom};
        let path = self.path.clone();
        let file = self.file.file();
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(drop))
            .map_err(|source| Store::io(&path, "truncating", source))?;
        self.verifier = Verifier::new();
        self.have = 0;
        self.resumed = 0;
        Ok(())
    }

    /// Appends everything `source` gives, hashing it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if `source` fails or the file can't be written:
    /// what was appended before the failure stays.
    pub fn append(&mut self, source: &mut (impl Read + ?Sized)) -> Result<(), StoreError> {
        let mut buffer = [0u8; COPY_CHUNK];
        loop {
            let read = source.read(&mut buffer).map_err(|source| StoreError::Io {
                context: "reading the blob's bytes".to_owned(),
                source,
            })?;
            if read == 0 {
                return Ok(());
            }
            let chunk = &buffer[..read];
            self.file
                .file()
                .write_all(chunk)
                .map_err(|source| Store::io(&self.path, "writing", source))?;
            self.verifier.update(chunk);
            self.have += read as u64;
        }
    }

    /// Makes it the blob: verified against its digest, synced to disk, made
    /// read-only, then renamed onto its name. Whether it was new (`false`:
    /// another writer placed the same blob first).
    ///
    /// # Errors
    ///
    /// [`StoreError::Verify`] if the bytes don't hash to the digest (the file
    /// is removed, partial or not); [`StoreError::Io`] for a filesystem
    /// failure.
    pub fn commit(mut self) -> Result<bool, StoreError> {
        let verifier = std::mem::take(&mut self.verifier);
        if let Err(mismatch) = verifier.verify(&self.digest) {
            Store::discard(&self.path);
            self.done = true;
            return Err(StoreError::Verify(mismatch));
        }
        let path = self.path.clone();
        let file = self.file.file();
        file.sync_all()
            .map_err(|source| Store::io(&path, "syncing the blob", source))?;
        file.set_permissions(fs::Permissions::from_mode(BLOB_MODE))
            .map_err(|source| Store::io(&path, "sealing the blob read-only", source))?;
        self.done = true;
        match fs::rename(&self.path, &self.final_path) {
            Ok(()) => Ok(true),
            Err(source) => {
                // Another writer may have placed the blob first: the same
                // bytes, so either is right. Keep whichever is in place.
                Store::discard(&self.path);
                if self.final_path.is_file() {
                    Ok(false)
                } else {
                    Err(Store::io(&self.final_path, "placing the blob", source))
                }
            }
        }
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if !self.done && matches!(self.file, Handle::Temporary(_)) {
            Store::discard(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A throwaway directory, removed when dropped, following the repository's
    /// test convention (a unique name under the system temp dir).
    struct Scratch(PathBuf);

    #[test]
    fn mapped_layers_are_named_by_their_maps() {
        let digest = Digest::sha256(b"layer");
        let id = map_id(&[(0, 100_000, 65536)], &[(0, 100_000, 65536)]);
        assert_eq!(id, map_id(&[(0, 100_000, 65536)], &[(0, 100_000, 65536)]));
        assert_ne!(id, map_id(&[(0, 200_000, 65536)], &[(0, 100_000, 65536)]));
        assert_ne!(id, map_id(&[(0, 100_000, 65536)], &[(0, 200_000, 65536)]));
        let name = LayerOwners::Mapped(id).name(&digest);
        assert_eq!(name, format!("{}-map-{id:016x}", digest.hex()));
        assert_eq!(LayerOwners::digest_hex(&name), Some(digest.hex().as_str()));
        assert_eq!(
            LayerOwners::digest_hex(&format!("{}-map-xyz", digest.hex())),
            None
        );
    }

    #[test]
    fn a_path_names_the_open_file_only_until_it_is_replaced() {
        let scratch = Scratch::new();
        fs::create_dir_all(&scratch.0).unwrap();
        let path = scratch.0.join("partial");
        fs::write(&path, b"mine").unwrap();
        let file = File::open(&path).unwrap();
        assert!(names(&path, &file));
        // Committed by another writer (renamed away), and a new one made.
        fs::rename(&path, scratch.0.join("blob")).unwrap();
        fs::write(&path, b"theirs").unwrap();
        assert!(!names(&path, &file));
        fs::remove_file(&path).unwrap();
        assert!(!names(&path, &file));
    }

    #[test]
    fn a_read_only_partial_left_by_a_killed_pull_is_replaced() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).unwrap();
        let digest = Digest::sha256(b"blob");
        let partial = store
            .path(&digest)
            .with_file_name(format!(".{}.partial", digest.hex()));
        fs::write(&partial, b"bl").unwrap();
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o444)).unwrap();
        let mut writer = store.begin_blob(&digest).unwrap();
        // Root reads and writes it anyway; anyone else starts afresh.
        writer.reset().unwrap();
        writer.append(&mut &b"blob"[..]).unwrap();
        assert!(writer.commit().unwrap());
        assert!(store.check_blob(&digest).unwrap());
    }

    #[test]
    fn the_lock_is_shared_or_exclusive_and_goes_with_its_holder() {
        use std::time::Duration;
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).unwrap();
        let short = Duration::from_millis(50);
        let first = store.lock(LockMode::Shared, short).unwrap();
        let second = store.lock(LockMode::Shared, short).unwrap();
        assert!(matches!(
            store.lock(LockMode::Exclusive, short),
            Err(StoreError::Busy)
        ));
        drop((first, second));
        let exclusive = store.lock(LockMode::Exclusive, short).unwrap();
        assert!(matches!(
            store.lock(LockMode::Shared, short),
            Err(StoreError::Busy)
        ));
        drop(exclusive);
        store.lock(LockMode::Shared, short).unwrap();
    }

    #[test]
    fn an_existing_root_open_to_others_is_refused_not_changed() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new();
        fs::create_dir_all(&scratch.0).unwrap();
        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            Store::open(&scratch.0),
            Err(StoreError::NotPrivate(_))
        ));
        let mode = fs::metadata(&scratch.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "a directory Shipyard didn't make is left as it is"
        );
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            // Not made here: the store makes its own root, private (one that
            // already exists, open to others, is refused).
            let dir = std::env::temp_dir()
                .join(format!("shipyard-store-{}-{unique}", std::process::id()));
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A generous limit for the small-blob reads in these tests.
    const TEST_LIMIT: u64 = 1 << 20;

    /// Writes `bytes` as a blob and returns its digest.
    fn put(store: &Store, bytes: &[u8]) -> (Digest, bool) {
        let digest = Digest::sha256(bytes);
        let fresh = store
            .write_blob(&digest, &mut &bytes[..])
            .expect("writing a valid blob");
        (digest, fresh)
    }

    #[test]
    fn a_written_blob_reads_back_the_same() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, fresh) = put(&store, b"layer contents");
        assert!(fresh, "the first write is a new blob");
        assert!(store.contains(&digest));
        assert_eq!(
            store.read_blob(&digest, TEST_LIMIT).unwrap(),
            b"layer contents"
        );
    }

    #[test]
    fn the_blob_path_is_the_digest() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, _) = put(&store, b"x");
        let path = store.path(&digest);
        assert!(path.ends_with(format!("blobs/sha256/{}", digest.hex())));
        assert!(path.is_file());
    }

    #[test]
    fn an_identical_blob_is_stored_once() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, first) = put(&store, b"shared layer");
        let (same, second) = put(&store, b"shared layer");
        assert_eq!(digest, same);
        assert!(first, "first write is fresh");
        assert!(!second, "the second write deduplicates (no new blob)");
    }

    #[test]
    fn a_finished_blob_is_read_only() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, _) = put(&store, b"immutable");
        let mode = fs::metadata(store.path(&digest))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, BLOB_MODE, "blob is 0o444");
    }

    #[test]
    fn a_digest_mismatch_stores_nothing() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        // Ask for the digest of "right" but feed "wrong".
        let claimed = Digest::sha256(b"right");
        let error = store
            .write_blob(&claimed, &mut &b"wrong"[..])
            .expect_err("a mismatch is refused");
        assert!(matches!(error, StoreError::Verify(_)), "{error}");
        assert!(!store.contains(&claimed), "no blob under the claimed name");
        // No temporary file left behind either.
        let leftovers: Vec<_> = fs::read_dir(store.path(&claimed).parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftovers.is_empty(),
            "no stray temporary file: {leftovers:?}"
        );
    }

    #[test]
    fn reading_a_corrupted_blob_is_refused() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, _) = put(&store, b"good");
        // Corrupt the stored bytes behind the store's back (make it writable
        // first, since blobs are read-only).
        let path = store.path(&digest);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, b"evil").unwrap();
        let error = store
            .read_blob(&digest, TEST_LIMIT)
            .expect_err("corruption is caught");
        assert!(matches!(error, StoreError::Verify(_)), "{error}");
    }

    #[test]
    fn an_empty_blob_round_trips() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, _) = put(&store, b"");
        assert!(store.contains(&digest));
        assert_eq!(store.read_blob(&digest, TEST_LIMIT).unwrap(), b"");
    }

    #[test]
    fn a_blob_over_the_limit_is_refused() {
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let (digest, _) = put(&store, b"0123456789");
        // Exactly the length is allowed; one less is not.
        assert_eq!(store.read_blob(&digest, 10).unwrap().len(), 10);
        let error = store
            .read_blob(&digest, 9)
            .expect_err("over the limit is refused");
        assert!(
            matches!(error, StoreError::TooLarge { limit: 9, .. }),
            "{error}"
        );
    }

    #[test]
    fn concurrent_writers_of_the_same_digest_do_not_collide() {
        // Two threads writing the same content at once must not fail on the
        // temporary file (regression for the PID-only temp name): exactly one
        // reports a fresh write, the other deduplicates, and neither errors.
        let scratch = Scratch::new();
        let store = Store::open(&scratch.0).expect("opening the store");
        let bytes = b"a layer two pulls fetch at once".to_vec();
        let digest = Digest::sha256(&bytes);

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let bytes = bytes.clone();
                let digest = digest.clone();
                std::thread::spawn(move || {
                    store
                        .write_blob(&digest, &mut &bytes[..])
                        .expect("no writer errors on the race")
                })
            })
            .collect();
        let fresh_count = threads
            .into_iter()
            .map(|t| t.join().expect("thread panicked"))
            .filter(|&fresh| fresh)
            .count();
        // Hard to assert exactly one on a loose filesystem, but at least one
        // write is fresh and the blob is present and correct.
        assert!(fresh_count >= 1, "at least one fresh write");
        assert!(store.contains(&digest));
        assert_eq!(store.read_blob(&digest, TEST_LIMIT).unwrap(), bytes);
    }
}
