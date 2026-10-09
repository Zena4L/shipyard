//! Safe extraction of one layer into a directory.
//!
//! A layer is a tar archive (usually gzip-compressed) of the filesystem
//! changes it makes. Extracting one means writing files from an untrusted
//! stream, as root (or root in a user namespace), so this is the most
//! security-sensitive code in the image path. The rules:
//!
//! - **Nothing is reached by path.** Every write goes through
//!   [`DirWalker`]: one name at a time, relative to an
//!   open directory, with `O_NOFOLLOW`. A symlink in the layer (`etc -> /etc`)
//!   can't redirect a later write, deletion, link or attribute out of it: the
//!   walk refuses it. This is what keeps a hostile layer from deleting,
//!   creating or hard-linking host files (the tar-slip class, runc
//!   CVE-2021-30465).
//! - **The layer is verified as it's read.** The compressed bytes must hash
//!   to the layer's digest and the decompressed ones to its `diff_id` (the
//!   image config's record of the tar itself). A mismatch fails the layer;
//!   the caller discards the directory it was extracted into.
//! - **Untrusted sizes are bounded** before they're trusted: the decompressed
//!   bytes, the number of entries, each header extension, and the free space
//!   left on the store's filesystem.
//! - **Whiteouts are applied, never written.** `.wh.<name>` hides `<name>`
//!   in the layers below and `.wh..wh..opq` hides a whole directory's lower
//!   contents. Overlayfs reads these as a `0:0` character device and an
//!   `overlay.opaque` extended attribute, made at the end. A whiteout of
//!   something this same layer creates is dropped: by the OCI spec, a
//!   whiteout applies only to the layers below.
//! - **Metadata is the image's.** Owners, permission bits (including `/tmp`'s
//!   sticky 1777), times and file capabilities are applied. The mode is set
//!   after the owner, since `chown` clears setuid and setgid (TLPI 15.3), and
//!   directories get theirs last, deepest first, so a read-only directory
//!   doesn't block what's written into it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, BufReader, Read, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fchmod, fstat, futimens, mkdirat, utimensat};
use nix::sys::time::TimeSpec;
use nix::unistd::{Gid, Uid, fchown, fchownat, linkat, symlinkat};
use serde::{Deserialize, Serialize};

use crate::digest::{Digest, Verifier, VerifyError};
use crate::fsat::{self, DirWalker, WalkError};
use crate::store::LayerOwners;
use crate::tar_reader::{Entry, Kind, TarError, TarLimits, TarReader};

/// The OCI media types of an uncompressed layer.
const TAR_TYPES: [&str; 2] = [
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.nondistributable.v1.tar",
];
/// The media types of a gzip-compressed layer: OCI, and Docker's.
const GZIP_TYPES: [&str; 4] = [
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
    "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
];
/// The opaque-directory marker.
const OPAQUE_MARKER: &[u8] = b".wh..wh..opq";
/// The prefix of a whiteout, and of aufs's own metadata names.
const WHITEOUT_PREFIX: &[u8] = b".wh.";
const AUFS_PREFIX: &[u8] = b".wh..wh.";
/// A gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;
/// A filesystem's free space, and how much of it the store leaves alone.
#[derive(Debug, Clone, Copy)]
pub struct FreeSpace {
    /// Bytes an unprivileged writer may still use.
    pub available: u64,
    /// Bytes to leave free: 5 %, as filesystems reserve for root, but at
    /// least 1 GiB on a small disk and at most 10 GiB on a large one (5 % of
    /// 4 TB would stop every pull on a disk with 150 GB free).
    pub reserve: u64,
    /// Inodes (files, directories, links) an unprivileged writer may still
    /// make, and how many to leave free (5 %, at most a million); `None`
    /// where the filesystem has no fixed number (btrfs, say, says 0).
    pub files: Option<(u64, u64)>,
}

impl FreeSpace {
    /// The free space of the filesystem `fd` is on (`fstatvfs`).
    ///
    /// # Errors
    ///
    /// The errno of a failed `fstatvfs`.
    pub fn of(fd: impl std::os::fd::AsFd) -> nix::Result<Self> {
        let stat = nix::sys::statvfs::fstatvfs(fd)?;
        #[allow(
            clippy::useless_conversion,
            reason = "the statvfs field types differ between platforms"
        )]
        let (fragment, available, total) = (
            u64::from(stat.fragment_size()),
            u64::from(stat.blocks_available()),
            u64::from(stat.blocks()),
        );
        #[allow(
            clippy::useless_conversion,
            reason = "the statvfs field types differ between platforms"
        )]
        let (files_available, files) = (u64::from(stat.files_available()), u64::from(stat.files()));
        Ok(Self {
            available: available.saturating_mul(fragment),
            reserve: (total.saturating_mul(fragment) / 20).clamp(GIB, 10 * GIB),
            files: (files > 0).then(|| (files_available, (files / 20).min(MAX_FILES_RESERVE))),
        })
    }

    /// Whether this leaves enough free: `min_free` bytes (the reserve if
    /// `None`), and the inodes' reserve.
    fn check(self, min_free: Option<u64>) -> Result<(), UnpackError> {
        let reserve = min_free.unwrap_or(self.reserve);
        if self.available < reserve {
            return Err(UnpackError::NoSpace {
                available: self.available,
                reserve,
            });
        }
        if let Some((available, reserve)) = self.files
            && available < reserve
        {
            return Err(UnpackError::NoInodes { available, reserve });
        }
        Ok(())
    }
}

/// How often the free space is checked while writing, in bytes.
const SPACE_CHECK_EVERY: u64 = 64 * 1024 * 1024;
/// What an entry counts toward the next free-space check, whatever it
/// holds: a directory or an empty file writes no data, but takes an inode
/// and a block. So a layer of many of them is checked too: every 1024
/// entries at the most.
const ENTRY_COST: u64 = SPACE_CHECK_EVERY / 1024;
/// The most inodes (files) left free, when 5 % of a filesystem's are more.
const MAX_FILES_RESERVE: u64 = 1_000_000;

/// The layer to extract: what the bytes must hash to, and how they're
/// compressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerSpec {
    /// The digest of the (compressed) blob.
    pub digest: Digest,
    /// The layer's media type, which says how it's compressed.
    pub media_type: String,
    /// The digest of the uncompressed tar (`rootfs.diff_ids` in the config).
    pub diff_id: Digest,
}

/// ID ranges a process may own files as: `(first, count)` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRanges(pub Vec<(u32, u32)>);

impl IdRanges {
    /// Every ID (real root, outside any user namespace).
    #[must_use]
    pub fn all() -> Self {
        Self(vec![(0, u32::MAX)])
    }

    /// Whether `id` is in a range.
    #[must_use]
    pub fn contains(&self, id: u32) -> bool {
        self.0.iter().any(|&(first, count)| {
            id >= first && u64::from(id) < u64::from(first) + u64::from(count)
        })
    }
}

/// A user namespace's maps, applied by hand: an image's ID `X` becomes the
/// host ID the map gives `X` (real root can give a file to any host ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shift {
    /// The maps' identity ([`crate::store::map_id`]): the layers' name.
    id: u64,
    /// The user maps, `(container, host, size)`.
    uids: Vec<(u32, u32, u32)>,
    /// The group maps.
    gids: Vec<(u32, u32, u32)>,
    /// The host user and group the container's root is: what an ID the
    /// maps leave out (collapsed to 0) becomes.
    root: (u32, u32),
}

impl Shift {
    /// A shift by `uids` and `gids` (`(container, host, size)` each), if
    /// they map ID 0: an image's file owned by an ID they don't map is
    /// given to the container's root.
    #[must_use]
    pub fn new(uids: Vec<(u32, u32, u32)>, gids: Vec<(u32, u32, u32)>) -> Option<Self> {
        let root = (Self::host(&uids, 0)?, Self::host(&gids, 0)?);
        Some(Self {
            id: crate::store::map_id(&uids, &gids),
            uids,
            gids,
            root,
        })
    }

    /// The maps' identity: what names the layers shifted by them.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The user maps.
    #[must_use]
    pub fn uids(&self) -> &[(u32, u32, u32)] {
        &self.uids
    }

    /// The group maps.
    #[must_use]
    pub fn gids(&self) -> &[(u32, u32, u32)] {
        &self.gids
    }

    /// The host ID `id` is under `maps`, if they map it.
    fn host(maps: &[(u32, u32, u32)], id: u32) -> Option<u32> {
        maps.iter().find_map(|&(container, host, size)| {
            let offset = id.checked_sub(container)?;
            (offset < size).then(|| host.checked_add(offset)).flatten()
        })
    }
}

/// Whose files the extracted ones become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// The image's owners (`chown`), for root or root in a user namespace.
    /// An ID outside the ranges (a user namespace with too few IDs) becomes
    /// 0, and that file loses its setuid and setgid bits.
    Apply {
        /// The user IDs that can own files.
        uids: IdRanges,
        /// The group IDs that can own files.
        gids: IdRanges,
        /// How they're shifted on the way, where they aren't the image's own
        /// on the host or in the caller's range (real root, for a
        /// container with `--uidmap`).
        shift: Option<Shift>,
    },
    /// No `chown`: everything belongs to the caller, and setuid and setgid
    /// bits are dropped (a caller-owned setuid file means nothing good).
    Keep,
}

impl Ownership {
    /// Whose files a layer extracted this way has: part of its name in the
    /// store.
    #[must_use]
    pub fn owners(&self) -> LayerOwners {
        match self {
            Self::Apply { shift: None, .. } => LayerOwners::Image,
            Self::Apply {
                shift: Some(shift), ..
            } => LayerOwners::Mapped(shift.id()),
            Self::Keep => LayerOwners::Caller,
        }
    }
}

/// Limits on what a layer may make the extraction do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most decompressed bytes read.
    pub max_bytes: u64,
    /// The most entries, the longest paths, the largest header extensions.
    pub tar: TarLimits,
    /// The most bytes of bookkeeping kept in memory until the end (the
    /// paths the layer made, its whiteouts and opaque directories).
    pub max_retained: u64,
    /// The free space to leave on the filesystem; `None` for the default:
    /// 5 % of it, but at least 1 GiB and at most 10 GiB.
    pub min_free: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * GIB,
            tar: TarLimits::default(),
            max_retained: 256 * 1024 * 1024,
            min_free: None,
        }
    }
}

/// How to extract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpackOptions {
    /// Whose files they become.
    pub ownership: Ownership,
    /// The limits.
    pub limits: Limits,
    /// Whether to mark opaque directories with `trusted.overlay.opaque` too
    /// (real root only: what a mount without `userxattr` reads), besides
    /// `user.overlay.opaque` (what a rootless mount reads).
    pub trusted_xattrs: bool,
}

/// What an extraction did that someone may want to know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerReport {
    /// Entries extracted.
    pub entries: u64,
    /// Whiteouts made (deletions of lower files).
    pub whiteouts: u64,
    /// Directories made opaque.
    pub opaque_dirs: u64,
    /// Device nodes not created (a container's devices are its own).
    pub devices_skipped: u64,
    /// FIFOs not created (off Linux).
    pub fifos_skipped: u64,
    /// Files whose owner wasn't in the available IDs, given to root.
    pub ids_collapsed: u64,
    /// Overlay or `trusted.*` attributes refused: they'd steer the mount.
    pub xattrs_refused: u64,
    /// Other attributes not applied (unknown namespaces, or refused here).
    pub xattrs_not_applied: u64,
    /// aufs metadata entries (`.wh..wh.*`) ignored.
    pub aufs_ignored: u64,
    /// POSIX global headers ignored.
    pub global_headers_ignored: u64,
    /// Directories the layer implies (an entry below one it has no entry
    /// for), made with the lower layers' metadata there: the extraction is
    /// then the layer's on those lowers only (see `LayerOwners::name_on`).
    #[serde(default)]
    pub implied_dirs: u64,
}

impl LayerReport {
    /// What deserves a warning, as sentences.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut add = |count: u64, what: &str| {
            if count > 0 {
                warnings.push(format!("{count} {what}"));
            }
        };
        add(
            self.devices_skipped,
            "device node(s) in the image weren't created (a container's devices come from its own /dev)",
        );
        add(self.fifos_skipped, "FIFO(s) weren't created (Linux only)");
        add(
            self.ids_collapsed,
            "file(s) owned by IDs outside your subordinate ranges now belong to root \
             (add more IDs in /etc/subuid and /etc/subgid)",
        );
        add(
            self.xattrs_refused,
            "overlay or trusted.* attribute(s) in the image were refused",
        );
        add(
            self.xattrs_not_applied,
            "extended attribute(s) couldn't be applied",
        );
        add(self.aufs_ignored, "aufs metadata entr(ies) were ignored");
        add(
            self.global_headers_ignored,
            "tar global header(s) were ignored",
        );
        warnings
    }

    /// Adds another layer's counts.
    pub fn add(&mut self, other: &Self) {
        self.entries += other.entries;
        self.whiteouts += other.whiteouts;
        self.opaque_dirs += other.opaque_dirs;
        self.devices_skipped += other.devices_skipped;
        self.fifos_skipped += other.fifos_skipped;
        self.ids_collapsed += other.ids_collapsed;
        self.xattrs_refused += other.xattrs_refused;
        self.xattrs_not_applied += other.xattrs_not_applied;
        self.aufs_ignored += other.aufs_ignored;
        self.global_headers_ignored += other.global_headers_ignored;
        self.implied_dirs += other.implied_dirs;
    }
}

/// Why a layer couldn't be extracted.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    /// The archive is malformed, unsupported, or over a header limit.
    #[error(transparent)]
    Tar(#[from] TarError),
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        source: io::Error,
    },
    /// A symlink in the layer stands where a directory is needed: writing
    /// through it could leave the layer.
    #[error("the layer entry `{0}` would be reached through a symlink")]
    ThroughSymlink(String),
    /// A non-directory stands where a directory is needed.
    #[error("the layer entry `{0}` is under something that isn't a directory")]
    NotADirectory(String),
    /// A hard link to something this layer didn't create.
    #[error("the hard link `{link}` points to `{target}`, which isn't a file earlier in the layer")]
    HardlinkTarget {
        /// The link.
        link: String,
        /// What it points to.
        target: String,
    },
    /// A directory the layer implies, where a lower layer has a symlink.
    #[error(
        "the layer writes under `{0}`, a symlink in a layer below; it can't be stacked \
         with overlayfs (rebuild the image with the real path)"
    )]
    LowerSymlink(String),
    /// More bookkeeping than the limit (a layer of very many entries).
    #[error("the layer needs more than {limit} bytes of memory to extract (too many entries)")]
    TooMuchRetained {
        /// The limit.
        limit: u64,
    },
    /// More decompressed bytes than the limit.
    #[error("the layer is larger than the {limit}-byte limit")]
    TooLarge {
        /// The limit.
        limit: u64,
    },
    /// The filesystem would be left with too few free inodes.
    #[error(
        "only {available} inodes (files) free on the image store's filesystem, and \
         {reserve} must stay free"
    )]
    NoInodes {
        /// Inodes free.
        available: u64,
        /// Inodes that must stay free.
        reserve: u64,
    },
    /// The filesystem would be left with too little free space.
    #[error(
        "only {available} bytes free on the image store's filesystem, and {reserve} must stay \
         free"
    )]
    NoSpace {
        /// Bytes free.
        available: u64,
        /// Bytes that must stay free.
        reserve: u64,
    },
    /// The compressed bytes don't match the layer's digest.
    #[error("the layer's bytes don't match its digest: {0}")]
    Digest(VerifyError),
    /// The uncompressed tar doesn't match the config's `diff_id`.
    #[error("the layer's contents don't match the image config's diff_id: {0}")]
    DiffId(VerifyError),
    /// A layer media type Shipyard can't read.
    #[error("unsupported layer media type `{0}`")]
    MediaType(String),
    /// A whiteout or opaque directory couldn't be made: the image would be
    /// wrong without it.
    #[error("making the {what} at `{path}`: {source}{hint}")]
    OverlayMarker {
        /// A whiteout, or an opaque directory's mark.
        what: &'static str,
        /// Where.
        path: String,
        /// Why.
        source: io::Error,
        /// What to do about it.
        hint: &'static str,
    },
}

/// Extracts the layer read from `blob` into the directory `dest` (empty),
/// on top of `lowers` (the extracted layers below, base first: where a
/// directory the layer implies without an entry takes its metadata from).
///
/// # Errors
///
/// An [`UnpackError`]; `dest` is then partly written, for the caller to
/// discard.
pub fn unpack_layer(
    blob: impl Read,
    layer: &LayerSpec,
    dest: &OwnedFd,
    lowers: &[OwnedFd],
    options: &UnpackOptions,
) -> Result<LayerReport, UnpackError> {
    let decoder = Decoder::new(Hashing::new(blob), &layer.media_type)?;
    let bounded = Bounded::new(Hashing::new(decoder), options.limits.max_bytes);
    let mut tar = TarReader::new(bounded, options.limits.tar);
    let root = dest
        .try_clone()
        .map_err(|source| io_error("opening the layer", source))?;
    let mut unpacker = Unpacker::new(root, lowers, options);
    unpacker.check_space()?;
    while let Some(entry) = tar
        .next_entry()
        .map_err(|err| tar_error(err, &options.limits))?
    {
        unpacker.place(&entry, &mut tar)?;
    }
    unpacker.report.global_headers_ignored = tar.ignored_globals();

    // Verify both digests before anything below the tar is made from it.
    let mut bounded = tar.into_inner();
    io::copy(&mut bounded, &mut io::sink())
        .map_err(|err| stream_error(err, &options.limits, "reading the layer"))?;
    let (decoder, diff_id) = bounded.into_inner().finish();
    diff_id
        .verify(&layer.diff_id)
        .map_err(UnpackError::DiffId)?;
    let compressed = decoder
        .finish()
        .map_err(|err| io_error("reading the layer", err))?;
    let (_, digest) = compressed.finish();
    digest.verify(&layer.digest).map_err(UnpackError::Digest)?;

    unpacker.finish()
}

/// What this layer created at a path.
#[derive(Debug)]
enum Created {
    /// A directory, with the metadata it's to get at the end, and its
    /// identity (so a directory replaced since isn't the one changed).
    Directory { meta: Meta, identity: (u64, u64) },
    /// Anything else (a file, a link, a FIFO).
    Other,
}

/// The owner, mode and time an entry carries (its extended attributes are
/// applied at once, never kept).
#[derive(Debug, Clone, Copy)]
struct Meta {
    mode: u32,
    uid: u32,
    gid: u32,
    /// `None`: leave the time as it is.
    mtime: Option<(i64, u32)>,
}

impl Meta {
    fn of(entry: &Entry) -> Self {
        Self {
            mode: entry.mode,
            uid: entry.uid,
            gid: entry.gid,
            mtime: Some(entry.mtime),
        }
    }

    /// A directory the layer implies without an entry: root's, 0755, as
    /// containerd makes them.
    fn implied() -> Self {
        Self {
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: None,
        }
    }
}

/// The state of one extraction.
struct Unpacker<'a> {
    walker: DirWalker,
    /// What this layer made, by path: hard link sources, whiteout
    /// cancellation and the directories' deferred metadata come from here.
    created: BTreeMap<PathBuf, Created>,
    whiteouts: BTreeSet<PathBuf>,
    opaque: BTreeSet<PathBuf>,
    /// Bytes `created`, `whiteouts` and `opaque` hold, against
    /// [`Limits::max_retained`].
    retained: u64,
    lowers: &'a [OwnedFd],
    options: &'a UnpackOptions,
    report: LayerReport,
    /// Bytes written since the free space was last checked.
    unchecked: u64,
    buffer: Vec<u8>,
}

/// What one more recorded path costs in memory, besides its bytes (the
/// tree's node and the path's own allocation, roughly).
const RECORD_OVERHEAD: u64 = 64;

impl<'a> Unpacker<'a> {
    fn new(root: OwnedFd, lowers: &'a [OwnedFd], options: &'a UnpackOptions) -> Self {
        Self {
            walker: DirWalker::new(root),
            created: BTreeMap::new(),
            whiteouts: BTreeSet::new(),
            opaque: BTreeSet::new(),
            retained: 0,
            lowers,
            options,
            report: LayerReport::default(),
            unchecked: 0,
            buffer: vec![0; 64 * 1024],
        }
    }

    /// Counts `path` against the memory budget: a layer of millions of
    /// entries (or whiteouts, which cost no disk) can't exhaust memory.
    fn retain(&mut self, path: &Path) -> Result<(), UnpackError> {
        self.retained += path.as_os_str().len() as u64 + RECORD_OVERHEAD;
        if self.retained > self.options.limits.max_retained {
            return Err(UnpackError::TooMuchRetained {
                limit: self.options.limits.max_retained,
            });
        }
        Ok(())
    }

    /// Records what this layer made at `path`.
    fn record(&mut self, path: PathBuf, created: Created) -> Result<(), UnpackError> {
        self.retain(&path)?;
        self.created.insert(path, created);
        Ok(())
    }

    /// Places one entry; a file's data is read from `data`.
    fn place(&mut self, entry: &Entry, data: &mut impl Read) -> Result<(), UnpackError> {
        self.report.entries += 1;
        self.charge(ENTRY_COST)?;
        let Some(name) = entry.path.file_name() else {
            // The layer root itself (`./`).
            return self.place_root(entry);
        };
        let parent = entry.path.parent().unwrap_or(Path::new(""));
        let bytes = name.as_bytes();
        if bytes == OPAQUE_MARKER {
            self.retain(parent)?;
            self.opaque.insert(parent.to_owned());
            return Ok(());
        }
        if bytes.starts_with(AUFS_PREFIX) {
            self.report.aufs_ignored += 1;
            return Ok(());
        }
        if let Some(target) = bytes.strip_prefix(WHITEOUT_PREFIX) {
            if target.is_empty() || target == b"." || target == b".." {
                return Err(TarError::UnsafePath(entry.path.display().to_string()).into());
            }
            let path = parent.join(OsStr::from_bytes(target));
            self.retain(&path)?;
            self.whiteouts.insert(path);
            return Ok(());
        }
        match entry.kind {
            Kind::CharDevice | Kind::BlockDevice => {
                self.report.devices_skipped += 1;
                Ok(())
            }
            Kind::Directory => self.place_directory(entry, parent, name),
            Kind::File => self.place_file(entry, parent, name, data),
            Kind::Symlink => self.place_symlink(entry, parent, name),
            Kind::Hardlink => self.place_hardlink(entry, parent, name),
            Kind::Fifo => self.place_fifo(entry, parent, name),
        }
    }

    /// The layer root's own entry: its attributes now, the rest at the end.
    fn place_root(&mut self, entry: &Entry) -> Result<(), UnpackError> {
        if entry.kind != Kind::Directory {
            return Err(TarError::Malformed("the layer root isn't a directory".to_owned()).into());
        }
        let root = self
            .walker
            .root()
            .try_clone()
            .map_err(|source| io_error("opening the layer root", source))?;
        self.apply_xattrs(&root, &entry.xattrs);
        let identity = fsat::identity(&fstat(&root).map_err(failed("reading", Path::new("/")))?);
        self.record(
            PathBuf::new(),
            Created::Directory {
                meta: Meta::of(entry),
                identity,
            },
        )
    }

    fn place_directory(
        &mut self,
        entry: &Entry,
        parent: &Path,
        name: &OsStr,
    ) -> Result<(), UnpackError> {
        let path = &entry.path;
        let dir = self.enter(parent, true, path)?;
        let existing = fsat::stat_at(&dir, name).map_err(failed("reading", path))?;
        let is_dir = existing
            .as_ref()
            .is_some_and(|stat| fsat::kind(stat) == SFlag::S_IFDIR);
        if !is_dir {
            if existing.is_some() {
                self.remove(&dir, path, name)?;
            }
            mkdirat(&dir, name, Mode::from_bits_truncate(0o700))
                .map_err(failed("creating", path))?;
        }
        let made = fsat::open_dir_at(&dir, name).map_err(failed("opening", path))?;
        self.apply_xattrs(&made, &entry.xattrs);
        let identity = fsat::identity(&fstat(&made).map_err(failed("reading", path))?);
        // A directory listed again keeps what's below it; its owner, mode
        // and time come last (a read-only one would block what goes in).
        self.record(
            path.clone(),
            Created::Directory {
                meta: Meta::of(entry),
                identity,
            },
        )
    }

    fn place_file(
        &mut self,
        entry: &Entry,
        parent: &Path,
        name: &OsStr,
        data: &mut impl Read,
    ) -> Result<(), UnpackError> {
        let path = &entry.path;
        let dir = self.enter(parent, true, path)?;
        self.clear(&dir, path, name)?;
        let fd = openat(
            &dir,
            name,
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(failed("creating", path))?;
        let mut file = std::fs::File::from(fd);
        loop {
            let read = data
                .read(&mut self.buffer)
                .map_err(|err| stream_error(err, &self.options.limits, "reading the layer"))?;
            if read == 0 {
                break;
            }
            file.write_all(&self.buffer[..read])
                .map_err(|source| io_error(&format!("writing {}", path.display()), source))?;
            self.charge(read as u64)?;
        }
        let fd = OwnedFd::from(file);
        self.apply(&fd, Meta::of(entry), path)?;
        self.apply_xattrs(&fd, &entry.xattrs);
        self.record(path.clone(), Created::Other)
    }

    fn place_symlink(
        &mut self,
        entry: &Entry,
        parent: &Path,
        name: &OsStr,
    ) -> Result<(), UnpackError> {
        let path = &entry.path;
        let target = entry
            .link
            .as_deref()
            .ok_or_else(|| TarError::Malformed("a symlink without a target".to_owned()))?;
        let dir = self.enter(parent, true, path)?;
        self.clear(&dir, path, name)?;
        symlinkat(target, &dir, name).map_err(failed("creating", path))?;
        // The link's own owner and time (never its target's). No extended
        // attributes: `user.*` can't be set on a symlink, and nothing else
        // means anything on one.
        if let Some(((uid, gid), _)) = self.owner(entry.uid, entry.gid) {
            fchownat(
                &dir,
                name,
                Some(Uid::from_raw(uid)),
                Some(Gid::from_raw(gid)),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            )
            .map_err(failed("owning", path))?;
        }
        let time = timespec(entry.mtime);
        utimensat(
            &dir,
            name,
            &time,
            &time,
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
        .map_err(failed("timing", path))?;
        self.record(path.clone(), Created::Other)
    }

    fn place_hardlink(
        &mut self,
        entry: &Entry,
        parent: &Path,
        name: &OsStr,
    ) -> Result<(), UnpackError> {
        let path = &entry.path;
        let target = PathBuf::from(
            entry
                .link
                .clone()
                .ok_or_else(|| TarError::Malformed("a hard link without a target".to_owned()))?,
        );
        let refused = || UnpackError::HardlinkTarget {
            link: path.display().to_string(),
            target: target.display().to_string(),
        };
        // Only what this layer made, and not a directory: never something
        // reached through a symlink, a lower layer, or the host.
        if !matches!(self.created.get(&target), Some(Created::Other)) {
            return Err(refused());
        }
        if target == *path {
            return Ok(());
        }
        let source_name = target.file_name().ok_or_else(refused)?.to_owned();
        let source_dir = self.enter(target.parent().unwrap_or(Path::new("")), false, &target)?;
        let dir = self.enter(parent, true, path)?;
        self.clear(&dir, path, name)?;
        // Flags 0: a source that is a symlink is linked itself, not followed.
        linkat(
            &source_dir,
            source_name.as_os_str(),
            &dir,
            name,
            AtFlags::empty(),
        )
        .map_err(failed("linking", path))?;
        self.record(path.clone(), Created::Other)
    }

    #[cfg(target_os = "linux")]
    fn place_fifo(
        &mut self,
        entry: &Entry,
        parent: &Path,
        name: &OsStr,
    ) -> Result<(), UnpackError> {
        let path = &entry.path;
        let dir = self.enter(parent, true, path)?;
        self.clear(&dir, path, name)?;
        nix::unistd::mkfifoat(&dir, name, Mode::from_bits_truncate(0o600))
            .map_err(failed("creating", path))?;
        // Non-blocking, so opening it doesn't wait for a writer.
        let fd = openat(
            &dir,
            name,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(failed("opening", path))?;
        self.apply(&fd, Meta::of(entry), path)?;
        self.apply_xattrs(&fd, &entry.xattrs);
        self.record(path.clone(), Created::Other)
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unnecessary_wraps, reason = "the Linux version's signature")]
    fn place_fifo(&mut self, _: &Entry, _: &Path, _: &OsStr) -> Result<(), UnpackError> {
        self.report.fifos_skipped += 1;
        Ok(())
    }

    /// Opens the directory `dir` (for the entry at `path`), making what's
    /// missing (as implied directories) if `create`, and returns a
    /// descriptor for it.
    fn enter(&mut self, dir: &Path, create: bool, path: &Path) -> Result<OwnedFd, UnpackError> {
        let mut made = Vec::new();
        let opened = self
            .walker
            .enter(dir, create.then_some(&mut made))
            .map_err(|err| walk_error(err, path))?
            .try_clone()
            .map_err(|source| io_error("opening a directory", source))?;
        self.record_implied(made)?;
        Ok(opened)
    }

    /// Records directories made on the way, with the metadata of the nearest
    /// lower layer's directory there (what the image's builder had under
    /// it), or root's 0755.
    fn record_implied(&mut self, made: Vec<(PathBuf, (u64, u64))>) -> Result<(), UnpackError> {
        for (path, identity) in made {
            self.report.implied_dirs += 1;
            let meta = self.lower_meta(&path)?.unwrap_or_else(Meta::implied);
            self.record(path, Created::Directory { meta, identity })?;
        }
        Ok(())
    }

    /// The metadata of the directory at `path` in the nearest lower layer
    /// that has it. A symlink there (`lib -> usr/lib`) is refused: the
    /// image's builder wrote through it, and overlayfs would hide it.
    fn lower_meta(&self, path: &Path) -> Result<Option<Meta>, UnpackError> {
        let names: Vec<&OsStr> = path.iter().collect();
        for lower in self.lowers.iter().rev() {
            let mut dir = lower
                .try_clone()
                .map_err(|source| io_error("opening a lower layer", source))?;
            for (depth, name) in names.iter().enumerate() {
                let Some(stat) =
                    fsat::stat_at(&dir, name).map_err(failed("reading below", path))?
                else {
                    break;
                };
                let last = depth + 1 == names.len();
                match fsat::kind(&stat) {
                    SFlag::S_IFDIR if last => {
                        #[allow(
                            clippy::useless_conversion,
                            reason = "mode_t is u16 on macOS, u32 on Linux"
                        )]
                        return Ok(Some(Meta {
                            mode: u32::from(stat.st_mode) & 0o7777,
                            uid: stat.st_uid,
                            gid: stat.st_gid,
                            mtime: Some((
                                stat.st_mtime,
                                u32::try_from(stat.st_mtime_nsec).unwrap_or(0),
                            )),
                        }));
                    }
                    SFlag::S_IFDIR => {
                        dir =
                            fsat::open_dir_at(&dir, name).map_err(failed("reading below", path))?;
                    }
                    SFlag::S_IFLNK => {
                        let prefix: PathBuf = names[..=depth].iter().collect();
                        // Hidden if this layer has its own directory there.
                        if !last && self.created.contains_key(&prefix) {
                            return Ok(None);
                        }
                        return Err(UnpackError::LowerSymlink(prefix.display().to_string()));
                    }
                    // A file, or a whiteout: nothing below it shows through.
                    _ => return Ok(None),
                }
            }
        }
        Ok(None)
    }

    /// Makes room for a non-directory at `path`: whatever is there (a file,
    /// a symlink, a directory tree) goes, as tar's last entry wins.
    fn clear(&mut self, dir: &OwnedFd, path: &Path, name: &OsStr) -> Result<(), UnpackError> {
        if fsat::stat_at(dir, name)
            .map_err(failed("reading", path))?
            .is_some()
        {
            self.remove(dir, path, name)?;
        }
        Ok(())
    }

    /// Removes `path` (`name` in `dir`) and forgets everything recorded at
    /// and below it.
    fn remove(&mut self, dir: &OwnedFd, path: &Path, name: &OsStr) -> Result<(), UnpackError> {
        fsat::remove_tree_at(dir, name).map_err(failed("replacing", path))?;
        self.walker.forget(path);
        let below: Vec<PathBuf> = self
            .created
            .range(path.to_path_buf()..)
            .take_while(|(key, _)| key.starts_with(path))
            .map(|(key, _)| key.clone())
            .collect();
        for key in below {
            self.created.remove(&key);
        }
        Ok(())
    }

    /// Who an entry owned by `uid`:`gid` in the layer belongs to, and
    /// whether an ID had to give way to 0 (it loses setuid and setgid
    /// then); `None`: the caller (no `chown`).
    fn owner(&mut self, uid: u32, gid: u32) -> Option<((u32, u32), bool)> {
        match &self.options.ownership {
            Ownership::Keep => None,
            Ownership::Apply { uids, gids, shift } => {
                let (fits_uid, fits_gid) = (uids.contains(uid), gids.contains(gid));
                let collapsed = !(fits_uid && fits_gid);
                if collapsed {
                    self.report.ids_collapsed += 1;
                }
                let (uid, gid) = (
                    if fits_uid { uid } else { 0 },
                    if fits_gid { gid } else { 0 },
                );
                let owner = match shift {
                    // Mapped: `uids` and `gids` are the maps' container sides.
                    Some(shift) => (
                        Shift::host(&shift.uids, uid).unwrap_or(shift.root.0),
                        Shift::host(&shift.gids, gid).unwrap_or(shift.root.1),
                    ),
                    None => (uid, gid),
                };
                Some((owner, collapsed))
            }
        }
    }

    /// Applies `meta` to an open file or directory: owner, then mode (chown
    /// clears setuid), then times.
    fn apply(&mut self, fd: &OwnedFd, meta: Meta, path: &Path) -> Result<(), UnpackError> {
        let mut mode = meta.mode & 0o7777;
        match self.owner(meta.uid, meta.gid) {
            Some(((uid, gid), collapsed)) => {
                if collapsed {
                    mode &= !0o6000;
                }
                fchown(fd, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
                    .map_err(failed("owning", path))?;
            }
            None => mode &= !0o6000,
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "mode_t is u16 on macOS; the bits are 0o7777"
        )]
        fchmod(fd, Mode::from_bits_truncate(mode as nix::libc::mode_t))
            .map_err(failed("setting the mode of", path))?;
        if let Some(mtime) = meta.mtime {
            let time = timespec(mtime);
            futimens(fd, &time, &time).map_err(failed("timing", path))?;
        }
        Ok(())
    }

    /// Applies the extended attributes allowed in a layer. Overlay's own and
    /// `trusted.*` are refused: they'd make the mount redirect or skip
    /// files. Failures are counted, not fatal (a filesystem without `user.*`
    /// attributes can still hold the image).
    fn apply_xattrs(&mut self, fd: &OwnedFd, xattrs: &[(OsString, Vec<u8>)]) {
        for (name, value) in xattrs {
            let bytes = name.as_bytes();
            if bytes.starts_with(b"trusted.") || bytes.starts_with(b"user.overlay.") {
                self.report.xattrs_refused += 1;
            } else if bytes == b"security.selinux" {
                // Labels are the host's to give; every Fedora file has one.
            } else if bytes == b"security.capability" || bytes.starts_with(b"user.") {
                if !set_xattr(fd, name, value) {
                    self.report.xattrs_not_applied += 1;
                }
            } else {
                self.report.xattrs_not_applied += 1;
            }
        }
    }

    /// Counts `bytes` written toward the next free-space check, which
    /// comes once [`SPACE_CHECK_EVERY`] have been since the last.
    fn charge(&mut self, bytes: u64) -> Result<(), UnpackError> {
        self.unchecked += bytes;
        if self.unchecked >= SPACE_CHECK_EVERY {
            self.unchecked = 0;
            self.check_space()?;
        }
        Ok(())
    }

    /// Refuses to go on if the filesystem is nearly full: of bytes, or of
    /// inodes.
    fn check_space(&self) -> Result<(), UnpackError> {
        let space = FreeSpace::of(self.walker.root())
            .map_err(failed("checking the free space under", Path::new("/")))?;
        space.check(self.options.limits.min_free)
    }

    /// After the last entry: the whiteouts and opaque directories, then the
    /// directories' metadata, deepest first.
    fn finish(mut self) -> Result<LayerReport, UnpackError> {
        // A whiteout applies only to the layers below: one of something this
        // layer made is dropped, and a directory it remade hides what's
        // below it instead (it would merge with the lower one otherwise).
        let whiteouts = std::mem::take(&mut self.whiteouts);
        for path in whiteouts {
            match self.created.get(&path) {
                Some(Created::Directory { .. }) => {
                    self.opaque.insert(path);
                }
                Some(Created::Other) => {}
                None => self.make_whiteout(&path)?,
            }
        }
        let opaque = std::mem::take(&mut self.opaque);
        for path in &opaque {
            self.make_opaque(path)?;
        }

        // Children before parents: in reverse order, `a/b` comes before `a`.
        let directories: Vec<(PathBuf, Meta, (u64, u64))> = self
            .created
            .iter()
            .rev()
            .filter_map(|(path, created)| match created {
                Created::Directory { meta, identity } => Some((path.clone(), *meta, *identity)),
                Created::Other => None,
            })
            .collect();
        for (path, meta, identity) in directories {
            let fd = self.enter(&path, false, &path)?;
            let now = fsat::identity(&fstat(&fd).map_err(failed("reading", &path))?);
            if now == identity {
                self.apply(&fd, meta, &path)?;
            }
        }
        Ok(self.report)
    }

    /// A whiteout for `path`: a `0:0` character device, which overlayfs
    /// reads as "deleted" whether mounted by root or rootless.
    #[cfg(target_os = "linux")]
    fn make_whiteout(&mut self, path: &Path) -> Result<(), UnpackError> {
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            return Ok(());
        };
        let name = name.to_owned();
        let dir = self.enter(parent, true, path)?;
        nix::sys::stat::mknodat(&dir, name.as_os_str(), SFlag::S_IFCHR, Mode::empty(), 0).map_err(
            |errno| UnpackError::OverlayMarker {
                what: "whiteout",
                path: path.display().to_string(),
                source: io::Error::from(errno),
                hint: if errno == Errno::EPERM {
                    " (the image store's filesystem refuses whiteouts: overlayfs does, and \
                     without root, Linux before 5.8; put the store elsewhere with \
                     SHIPYARD_IMAGE_STORE)"
                } else {
                    ""
                },
            },
        )?;
        self.report.whiteouts += 1;
        Ok(())
    }

    /// Off Linux nothing mounts the store: a whiteout is only counted.
    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unnecessary_wraps, reason = "the Linux version's signature")]
    fn make_whiteout(&mut self, _: &Path) -> Result<(), UnpackError> {
        self.report.whiteouts += 1;
        Ok(())
    }

    /// Marks the directory at `path` opaque, made if the layer only implied
    /// it: `user.overlay.opaque`, read by a rootless mount (`userxattr`), and
    /// for real root `trusted.overlay.opaque`, read by its own mounts.
    /// Without the one its mounts read, deleted files would show through:
    /// that one is required. (Real root's `user.*` one only serves a mount
    /// in a user namespace, `--uidmap`: on a filesystem without `user.*`
    /// attributes it's counted, not fatal.)
    #[cfg(target_os = "linux")]
    fn make_opaque(&mut self, path: &Path) -> Result<(), UnpackError> {
        let dir = self.enter(path, true, path)?;
        let marked = |name: &str| fsat::set_xattr(&dir, OsStr::new(name), b"y");
        let marker_error = |errno: Errno| UnpackError::OverlayMarker {
            what: "opaque directory's mark",
            path: path.display().to_string(),
            source: io::Error::from(errno),
            hint: if errno == Errno::EOPNOTSUPP {
                " (the image store's filesystem has no extended attributes; put the store \
                 elsewhere with SHIPYARD_IMAGE_STORE)"
            } else {
                ""
            },
        };
        if self.options.trusted_xattrs {
            marked("trusted.overlay.opaque").map_err(marker_error)?;
            if marked("user.overlay.opaque").is_err() {
                self.report.xattrs_not_applied += 1;
            }
        } else {
            marked("user.overlay.opaque").map_err(marker_error)?;
        }
        self.report.opaque_dirs += 1;
        Ok(())
    }

    /// Off Linux nothing mounts the store: an opaque directory is only
    /// counted.
    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unnecessary_wraps, reason = "the Linux version's signature")]
    fn make_opaque(&mut self, _: &Path) -> Result<(), UnpackError> {
        self.report.opaque_dirs += 1;
        Ok(())
    }
}

/// Sets one extended attribute; whether it took.
#[cfg(target_os = "linux")]
fn set_xattr(fd: &OwnedFd, name: &OsStr, value: &[u8]) -> bool {
    fsat::set_xattr(fd, name, value).is_ok()
}

/// Off Linux, extended attributes aren't applied (the store isn't mounted).
#[cfg(not(target_os = "linux"))]
fn set_xattr(_: &OwnedFd, _: &OsStr, _: &[u8]) -> bool {
    false
}

/// A tar time as a `timespec`.
fn timespec((seconds, nanos): (i64, u32)) -> TimeSpec {
    TimeSpec::new(seconds, i64::from(nanos))
}

/// A failed walk, as an [`UnpackError`] naming the entry.
fn walk_error(err: WalkError, path: &Path) -> UnpackError {
    let shown = path.display().to_string();
    match err {
        WalkError::Symlink => UnpackError::ThroughSymlink(shown),
        WalkError::NotADirectory => UnpackError::NotADirectory(shown),
        WalkError::Missing => failed("reaching", path)(Errno::ENOENT),
        WalkError::Sys(errno) => failed("reaching", path)(errno),
    }
}

/// An [`UnpackError::Io`] for `action` on the entry at `path`, from an
/// errno.
fn failed<'p>(action: &'static str, path: &'p Path) -> impl FnOnce(Errno) -> UnpackError + 'p {
    move |errno| {
        io_error(
            &format!("{action} {}", path.display()),
            io::Error::from(errno),
        )
    }
}

fn io_error(context: &str, source: io::Error) -> UnpackError {
    UnpackError::Io {
        context: context.to_owned(),
        source,
    }
}

/// A stream error: the size limit if that's what it was.
fn stream_error(err: io::Error, limits: &Limits, context: &str) -> UnpackError {
    if err
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<LimitExceeded>)
    {
        UnpackError::TooLarge {
            limit: limits.max_bytes,
        }
    } else {
        io_error(context, err)
    }
}

/// A tar error: the size limit if that's what its I/O error was.
fn tar_error(err: TarError, limits: &Limits) -> UnpackError {
    match err {
        TarError::Io(err) => stream_error(err, limits, "reading the layer"),
        other => other.into(),
    }
}

/// How a layer's tar is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Not at all: a plain tar.
    None,
    /// With gzip.
    Gzip,
}

/// How a layer of `media_type` is compressed, if it's a layer Shipyard
/// extracts at all: a pull checks every layer this way before downloading.
///
/// # Errors
///
/// [`UnpackError::MediaType`] for zstd (not yet) or anything else.
pub fn layer_compression(media_type: &str) -> Result<Compression, UnpackError> {
    if TAR_TYPES.contains(&media_type) {
        Ok(Compression::None)
    } else if GZIP_TYPES.contains(&media_type) {
        Ok(Compression::Gzip)
    } else if media_type.ends_with("+zstd") {
        Err(UnpackError::MediaType(format!(
            "{media_type} (zstd layers aren't supported yet)"
        )))
    } else {
        Err(UnpackError::MediaType(media_type.to_owned()))
    }
}

/// Hashes what's read through it.
struct Hashing<R> {
    inner: R,
    verifier: Verifier,
}

impl<R: Read> Hashing<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            verifier: Verifier::new(),
        }
    }

    /// The stream, and the digest of everything read from it.
    fn finish(self) -> (R, Verifier) {
        (self.inner, self.verifier)
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.verifier.update(&buf[..read]);
        Ok(read)
    }
}

/// The decompression a media type calls for.
enum Decoder<R: Read> {
    Plain(R),
    Gzip(Box<flate2::bufread::MultiGzDecoder<BufReader<R>>>),
}

impl<R: Read> Decoder<R> {
    fn new(inner: R, media_type: &str) -> Result<Self, UnpackError> {
        Ok(if layer_compression(media_type)? == Compression::Gzip {
            // Every gzip member, not only the first (parallel compressors
            // write several).
            Self::Gzip(Box::new(flate2::bufread::MultiGzDecoder::new(
                BufReader::new(inner),
            )))
        } else {
            Self::Plain(inner)
        })
    }

    /// The compressed stream, read to its end (so its digest covers every
    /// byte, a gzip trailer included).
    fn finish(self) -> io::Result<R> {
        match self {
            Self::Plain(mut inner) => {
                io::copy(&mut inner, &mut io::sink())?;
                Ok(inner)
            }
            Self::Gzip(decoder) => {
                let mut buffered = decoder.into_inner();
                io::copy(&mut buffered, &mut io::sink())?;
                Ok(buffered.into_inner())
            }
        }
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(inner) => inner.read(buf),
            Self::Gzip(decoder) => decoder.read(buf),
        }
    }
}

/// The marker of a stream over its byte limit.
#[derive(Debug)]
struct LimitExceeded;

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the layer is over its size limit")
    }
}

impl std::error::Error for LimitExceeded {}

/// Fails a stream once more than `limit` bytes have come through it.
struct Bounded<R> {
    inner: R,
    read: u64,
    limit: u64,
}

impl<R: Read> Bounded<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            read: 0,
            limit,
        }
    }

    fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.read = self.read.saturating_add(read as u64);
        if self.read > self.limit {
            return Err(io::Error::other(LimitExceeded));
        }
        Ok(read)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
pub(crate) mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use super::*;

    #[test]
    fn a_shift_maps_each_id_to_its_host_id() {
        let maps = [(0, 100_000, 1000), (1000, 300_000, 10)];
        assert_eq!(Shift::host(&maps, 0), Some(100_000));
        assert_eq!(Shift::host(&maps, 999), Some(100_999));
        assert_eq!(Shift::host(&maps, 1005), Some(300_005));
        assert_eq!(Shift::host(&maps, 1010), None);
        assert_eq!(Shift::host(&[(0, u32::MAX, 2)], 1), None, "no overflow");
        let shift = Shift::new(maps.to_vec(), maps.to_vec()).unwrap();
        assert_eq!(shift.root, (100_000, 100_000));
        assert_eq!(shift.id(), crate::store::map_id(&maps, &maps));
        assert!(
            Shift::new(vec![(1, 100_000, 10)], maps.to_vec()).is_none(),
            "root must be mapped"
        );
        assert!(Shift::new(maps.to_vec(), vec![(1, 100_000, 10)]).is_none());
    }

    /// A scratch directory, removed on drop.
    pub(crate) struct Scratch(pub(crate) PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Self {
            static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("shipyard-unpack-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fsat::open_dir(self.0.parent().unwrap())
                .and_then(|parent| fsat::remove_tree_at(&parent, self.0.file_name().unwrap()));
        }
    }

    /// Builds a layer archive entry by entry.
    #[derive(Default)]
    pub(crate) struct Layer {
        bytes: Vec<u8>,
    }

    impl Layer {
        fn append(&mut self, mut header: tar::Header, data: &[u8]) -> &mut Self {
            header.set_cksum();
            self.bytes.extend_from_slice(header.as_bytes());
            self.bytes.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            self.bytes.resize(self.bytes.len() + pad, 0);
            self
        }

        /// A header with a raw name (anything, `..` included).
        fn header(name: &[u8], kind: tar::EntryType, size: usize, mode: u32) -> tar::Header {
            let mut header = tar::Header::new_ustar();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_entry_type(kind);
            header.set_size(size as u64);
            header.set_mode(mode);
            header.set_mtime(1_700_000_000);
            header
        }

        pub(crate) fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
            self.file_mode(name.as_bytes(), data, 0o644)
        }

        fn file_mode(&mut self, name: &[u8], data: &[u8], mode: u32) -> &mut Self {
            let header = Self::header(name, tar::EntryType::Regular, data.len(), mode);
            self.append(header, data)
        }

        #[cfg(target_os = "linux")]
        fn file_owned(&mut self, name: &str, mode: u32, owner: (u64, u64)) -> &mut Self {
            let mut header = Self::header(name.as_bytes(), tar::EntryType::Regular, 0, mode);
            header.set_uid(owner.0);
            header.set_gid(owner.1);
            self.append(header, b"")
        }

        pub(crate) fn dir(&mut self, name: &str, mode: u32) -> &mut Self {
            let header = Self::header(name.as_bytes(), tar::EntryType::Directory, 0, mode);
            self.append(header, b"")
        }

        fn link(&mut self, name: &str, kind: tar::EntryType, target: &str) -> &mut Self {
            let mut header = Self::header(name.as_bytes(), kind, 0, 0o777);
            header.as_old_mut().linkname[..target.len()].copy_from_slice(target.as_bytes());
            self.append(header, b"")
        }

        pub(crate) fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
            self.link(name, tar::EntryType::Symlink, target)
        }

        fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
            self.link(name, tar::EntryType::Link, target)
        }

        /// A PAX header for the next entry.
        fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
            let mut data = Vec::new();
            for (key, value) in records {
                let body = [key.as_bytes(), b"=", value, b"\n"].concat();
                let mut length = body.len() + 2;
                while length.to_string().len() + 1 + body.len() != length {
                    length += 1;
                }
                data.extend_from_slice(format!("{length} ").as_bytes());
                data.extend_from_slice(&body);
            }
            let header = Self::header(b"pax", tar::EntryType::XHeader, data.len(), 0o644);
            self.append(header, &data)
        }

        /// The archive, ended.
        fn tar(&self) -> Vec<u8> {
            let mut bytes = self.bytes.clone();
            bytes.extend_from_slice(&[0u8; 1024]);
            bytes
        }

        /// The gzip-compressed blob and its spec.
        pub(crate) fn blob(&self) -> (Vec<u8>, LayerSpec) {
            let tar = self.tar();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&tar).unwrap();
            let blob = gz.finish().unwrap();
            let spec = LayerSpec {
                digest: Digest::sha256(&blob),
                media_type: GZIP_TYPES[0].to_owned(),
                diff_id: Digest::sha256(&tar),
            };
            (blob, spec)
        }
    }

    pub(crate) fn keep() -> UnpackOptions {
        UnpackOptions {
            ownership: Ownership::Keep,
            limits: Limits {
                min_free: Some(0),
                ..Limits::default()
            },
            trusted_xattrs: false,
        }
    }

    /// Extracts `layer` into a fresh directory under `scratch`, on `lowers`.
    fn extract_with(
        scratch: &Scratch,
        layer: &Layer,
        lowers: &[&Path],
        options: &UnpackOptions,
    ) -> (PathBuf, Result<LayerReport, UnpackError>) {
        let (blob, spec) = layer.blob();
        extract_blob(scratch, &blob, &spec, lowers, options)
    }

    fn extract_blob(
        scratch: &Scratch,
        blob: &[u8],
        spec: &LayerSpec,
        lowers: &[&Path],
        options: &UnpackOptions,
    ) -> (PathBuf, Result<LayerReport, UnpackError>) {
        static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = scratch.0.join(format!("layer{n}"));
        std::fs::create_dir(&dest).unwrap();
        let fd = fsat::open_dir(&dest).unwrap();
        let lowers: Vec<OwnedFd> = lowers
            .iter()
            .map(|path| fsat::open_dir(path).unwrap())
            .collect();
        let result = unpack_layer(blob, spec, &fd, &lowers, options);
        (dest, result)
    }

    fn extract(scratch: &Scratch, layer: &Layer) -> (PathBuf, Result<LayerReport, UnpackError>) {
        extract_with(scratch, layer, &[], &keep())
    }

    /// A canary outside every layer: a file that must stay as it is.
    fn canary(scratch: &Scratch) -> PathBuf {
        let outside = scratch.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("canary"), "alive").unwrap();
        outside
    }

    fn assert_alive(outside: &Path) {
        assert_eq!(
            std::fs::read_to_string(outside.join("canary")).unwrap(),
            "alive",
            "the canary outside the layer was touched"
        );
        assert_eq!(
            std::fs::read_dir(outside).unwrap().count(),
            1,
            "something was made outside the layer"
        );
    }

    #[test]
    fn files_directories_links_and_modes_are_extracted() {
        let scratch = Scratch::new("basic");
        let mut layer = Layer::default();
        layer
            .dir("tmp", 0o1777)
            .dir("bin", 0o755)
            .file_mode(b"bin/tool", b"#!/bin/sh\n", 0o755)
            .symlink("bin/alias", "tool")
            .hardlink("bin/again", "bin/tool");
        let (dest, result) = extract(&scratch, &layer);
        let report = result.unwrap();
        assert_eq!(report.entries, 5);
        let mode = |path: &str| std::fs::symlink_metadata(dest.join(path)).unwrap().mode() & 0o7777;
        assert_eq!(
            mode("tmp"),
            0o1777,
            "/tmp keeps its sticky, world-writable mode"
        );
        assert_eq!(mode("bin/tool"), 0o755);
        assert_eq!(
            std::fs::read_link(dest.join("bin/alias")).unwrap(),
            Path::new("tool")
        );
        let ino = |path: &str| std::fs::metadata(dest.join(path)).unwrap().ino();
        assert_eq!(ino("bin/tool"), ino("bin/again"), "a hard link");
        assert_eq!(
            std::fs::metadata(dest.join("bin/tool")).unwrap().mtime(),
            1_700_000_000
        );
    }

    #[test]
    fn nothing_is_written_through_a_symlink() {
        let scratch = Scratch::new("through");
        let outside = canary(&scratch);
        let mut layer = Layer::default();
        layer
            .symlink("etc", outside.to_str().unwrap())
            .file("etc/canary", b"overwritten");
        let (_, result) = extract(&scratch, &layer);
        assert!(
            matches!(result, Err(UnpackError::ThroughSymlink(_))),
            "{result:?}"
        );
        assert_alive(&outside);
    }

    #[test]
    fn a_whiteout_or_opaque_marker_never_reaches_through_a_symlink() {
        for marker in ["etc/.wh.canary", "etc/.wh..wh..opq"] {
            let scratch = Scratch::new("whiteout");
            let outside = canary(&scratch);
            let mut layer = Layer::default();
            layer
                .symlink("etc", outside.to_str().unwrap())
                .file(marker, b"");
            let (_, result) = extract(&scratch, &layer);
            if cfg!(target_os = "linux") {
                assert!(
                    matches!(result, Err(UnpackError::ThroughSymlink(_))),
                    "{result:?}"
                );
            }
            assert_alive(&outside);
        }
    }

    #[test]
    fn a_hard_link_can_only_reach_what_the_layer_made() {
        let scratch = Scratch::new("hardlink");
        let outside = canary(&scratch);
        for target in ["h/canary", "elsewhere"] {
            let mut layer = Layer::default();
            layer
                .symlink("h", outside.to_str().unwrap())
                .hardlink("stolen", target);
            let (dest, result) = extract(&scratch, &layer);
            assert!(
                matches!(result, Err(UnpackError::HardlinkTarget { .. })),
                "{result:?}"
            );
            assert!(!dest.join("stolen").exists());
        }
        assert_alive(&outside);
    }

    #[test]
    fn climbing_paths_are_refused_and_absolute_ones_kept_inside() {
        let scratch = Scratch::new("paths");
        let outside = canary(&scratch);
        let mut layer = Layer::default();
        layer.file_mode(b"../outside/canary", b"x", 0o644);
        let (_, result) = extract(&scratch, &layer);
        assert!(
            matches!(result, Err(UnpackError::Tar(TarError::UnsafePath(_)))),
            "{result:?}"
        );
        assert_alive(&outside);

        let mut layer = Layer::default();
        layer.file_mode(b"/abs/file", b"x", 0o644);
        let (dest, result) = extract(&scratch, &layer);
        result.unwrap();
        assert!(dest.join("abs/file").is_file());
    }

    #[test]
    fn later_entries_replace_earlier_ones() {
        let scratch = Scratch::new("replace");
        let mut layer = Layer::default();
        layer
            .file("a", b"first")
            .file("a", b"second")
            .file("b", b"file")
            .dir("b", 0o755)
            .file("b/inside", b"")
            .dir("c", 0o700)
            .file("c/inside", b"")
            .file("c", b"file now")
            .symlink("d", "elsewhere")
            .file("d", b"file, not link");
        let (dest, result) = extract(&scratch, &layer);
        result.unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a")).unwrap(), "second");
        assert!(dest.join("b/inside").is_file());
        assert_eq!(std::fs::read_to_string(dest.join("c")).unwrap(), "file now");
        assert!(
            !std::fs::symlink_metadata(dest.join("d"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn a_file_in_the_middle_of_a_path_is_refused() {
        let scratch = Scratch::new("middle");
        let mut layer = Layer::default();
        layer.file("a", b"").file("a/b", b"");
        let (_, result) = extract(&scratch, &layer);
        assert!(
            matches!(result, Err(UnpackError::NotADirectory(_))),
            "{result:?}"
        );
    }

    #[test]
    fn a_replaced_directory_takes_its_last_metadata() {
        let scratch = Scratch::new("redir");
        let mut layer = Layer::default();
        layer
            .dir("d", 0o700)
            .file("d/x", b"")
            .file("d", b"")
            .dir("d", 0o751);
        let (dest, result) = extract(&scratch, &layer);
        result.unwrap();
        assert_eq!(
            std::fs::metadata(dest.join("d")).unwrap().mode() & 0o7777,
            0o751
        );
        assert!(!dest.join("d/x").exists());
    }

    #[test]
    fn setuid_is_dropped_when_owners_are_not_applied() {
        let scratch = Scratch::new("setuid");
        let mut layer = Layer::default();
        layer.file_mode(b"su", b"", 0o4755);
        let (dest, result) = extract(&scratch, &layer);
        result.unwrap();
        assert_eq!(
            std::fs::metadata(dest.join("su")).unwrap().mode() & 0o7777,
            0o755
        );
    }

    #[test]
    fn long_names_read_and_non_utf8_ones_too() {
        let scratch = Scratch::new("names");
        let long = format!("{}/file", "d".repeat(150));
        let mut layer = Layer::default();
        layer
            .pax(&[("path", long.as_bytes())])
            .file("short", b"long");
        // macOS's filesystem refuses names that aren't UTF-8 outright.
        if cfg!(target_os = "linux") {
            layer.file_mode(b"caf\xe9", b"latin-1", 0o644);
        }
        let (dest, result) = extract(&scratch, &layer);
        result.unwrap();
        assert_eq!(std::fs::read_to_string(dest.join(&long)).unwrap(), "long");
        if cfg!(target_os = "linux") {
            assert!(dest.join(OsStr::from_bytes(b"caf\xe9")).is_file());
        }
    }

    #[test]
    fn limits_hold() {
        let scratch = Scratch::new("limits");
        let mut layer = Layer::default();
        layer.file("big", &vec![0u8; 64 * 1024]);
        let mut options = keep();
        options.limits.max_bytes = 16 * 1024;
        let (_, result) = extract_with(&scratch, &layer, &[], &options);
        assert!(
            matches!(result, Err(UnpackError::TooLarge { .. })),
            "{result:?}"
        );

        let mut options = keep();
        options.limits.min_free = Some(u64::MAX);
        let (_, result) = extract_with(&scratch, &layer, &[], &options);
        assert!(
            matches!(result, Err(UnpackError::NoSpace { .. })),
            "{result:?}"
        );
    }

    /// What a layer decompresses to after its tar's end (the two zero
    /// blocks) is read, to check the `diff_id` against the whole stream, but
    /// never past the decompressed-size limit: a small blob that inflates to
    /// far more after the end is refused there. (Within the limit, the same
    /// kind of tail is fine: the control.)
    #[test]
    fn what_follows_the_archives_end_is_bounded_too() {
        let scratch = Scratch::new("tail");
        let mut layer = Layer::default();
        layer.file("f", b"f");
        let with_tail = |tail: usize| {
            let mut tar = layer.tar();
            tar.resize(tar.len() + tail, 0);
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            gz.write_all(&tar).unwrap();
            let blob = gz.finish().unwrap();
            let spec = LayerSpec {
                digest: Digest::sha256(&blob),
                media_type: GZIP_TYPES[0].to_owned(),
                diff_id: Digest::sha256(&tar),
            };
            (blob, spec)
        };
        let mut options = keep();
        options.limits.max_bytes = 1024 * 1024;
        // 16 MiB of zeros after the end: a blob of a few KiB.
        let (blob, spec) = with_tail(16 * 1024 * 1024);
        assert!(blob.len() < 64 * 1024, "{}", blob.len());
        let (_, result) = extract_blob(&scratch, &blob, &spec, &[], &options);
        assert!(
            matches!(result, Err(UnpackError::TooLarge { .. })),
            "{result:?}"
        );
        let (blob, spec) = with_tail(64 * 1024);
        let (dest, result) = extract_blob(&scratch, &blob, &spec, &[], &options);
        result.unwrap();
        assert!(dest.join("f").is_file());
    }

    /// Directories and empty files write no data, but each takes an inode:
    /// they count toward the free-space checks too, so a layer of nothing
    /// else stops short of a filesystem's last inodes (here a tmpfs of
    /// 20000), with the error that says so, not at the end of them. A
    /// smaller one fits there (the control). As root: the tmpfs is mounted
    /// in a mount namespace of this test's thread.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root"]
    fn a_layer_of_empty_entries_stops_short_of_the_last_inodes() {
        use nix::mount::{MntFlags, MsFlags, mount, umount2};
        std::thread::spawn(|| {
            nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS).unwrap();
            mount(
                None::<&str>,
                "/",
                None::<&str>,
                MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                None::<&str>,
            )
            .unwrap();
            let scratch = Scratch::new("inodes");
            std::fs::create_dir_all(&scratch.0).unwrap();
            mount(
                Some("tmpfs"),
                &scratch.0,
                Some("tmpfs"),
                MsFlags::empty(),
                Some("nr_inodes=20000"),
            )
            .unwrap();
            let empties = |count: usize| {
                let mut layer = Layer::default();
                for index in 0..count {
                    if index % 2 == 0 {
                        layer.dir(&format!("d{index}"), 0o755);
                    } else {
                        layer.file(&format!("f{index}"), b"");
                    }
                }
                layer
            };
            let (stopped, result) = extract_with(&scratch, &empties(30_000), &[], &keep());
            assert!(
                matches!(result, Err(UnpackError::NoInodes { .. })),
                "{result:?}"
            );
            // (The caller removes a failed layer's directory: its temporary.)
            std::fs::remove_dir_all(stopped).unwrap();
            let (_, result) = extract_with(&scratch, &empties(5_000), &[], &keep());
            assert_eq!(result.unwrap().entries, 5_000);
            umount2(&scratch.0, MntFlags::MNT_DETACH).unwrap();
        })
        .join()
        .unwrap();
    }

    /// Inodes have a reserve of their own, where the filesystem has a fixed
    /// number of them; where it has none (btrfs says 0), nothing is refused
    /// for them.
    #[test]
    fn free_inodes_are_checked_where_there_are_a_number() {
        let space = |files| FreeSpace {
            available: 100 * GIB,
            reserve: GIB,
            files,
        };
        assert!(matches!(
            space(Some((10, 50))).check(None),
            Err(UnpackError::NoInodes {
                available: 10,
                reserve: 50
            })
        ));
        assert!(space(Some((50, 50))).check(None).is_ok());
        assert!(space(None).check(None).is_ok());
        assert!(matches!(
            space(None).check(Some(200 * GIB)),
            Err(UnpackError::NoSpace { .. })
        ));
    }

    /// The inodes' reserve is 5 % of them, at most a million, and none is
    /// claimed where the filesystem reports no inodes.
    #[cfg(unix)]
    #[test]
    fn a_filesystems_inodes_are_read() {
        let scratch = Scratch::new("inodes");
        std::fs::create_dir_all(&scratch.0).unwrap();
        let dir = std::fs::File::open(&scratch.0).unwrap();
        let space = FreeSpace::of(&dir).unwrap();
        if let Some((available, reserve)) = space.files {
            assert!(available > 0);
            assert!(reserve <= MAX_FILES_RESERVE, "{reserve}");
        }
    }

    #[test]
    fn whiteouts_count_against_the_memory_budget() {
        let scratch = Scratch::new("budget");
        let mut layer = Layer::default();
        for n in 0..100 {
            layer.file(&format!(".wh.gone-{n}"), b"");
        }
        let mut options = keep();
        options.limits.max_retained = 1024;
        let (_, result) = extract_with(&scratch, &layer, &[], &options);
        assert!(
            matches!(result, Err(UnpackError::TooMuchRetained { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn the_digests_are_verified() {
        let scratch = Scratch::new("digests");
        let mut layer = Layer::default();
        layer.file("f", b"content");
        let (blob, spec) = layer.blob();
        let mut wrong = spec.clone();
        wrong.diff_id = Digest::sha256(b"something else");
        let (_, result) = extract_blob(&scratch, &blob, &wrong, &[], &keep());
        assert!(matches!(result, Err(UnpackError::DiffId(_))), "{result:?}");
        let mut wrong = spec.clone();
        wrong.digest = Digest::sha256(b"something else");
        let (_, result) = extract_blob(&scratch, &blob, &wrong, &[], &keep());
        assert!(matches!(result, Err(UnpackError::Digest(_))), "{result:?}");
    }

    #[test]
    fn several_gzip_members_are_all_read() {
        let scratch = Scratch::new("members");
        let mut layer = Layer::default();
        layer.file("one", b"1").file("two", b"2");
        let tar = layer.tar();
        let half = 1024;
        let mut blob = Vec::new();
        for part in [&tar[..half], &tar[half..]] {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(part).unwrap();
            blob.extend(gz.finish().unwrap());
        }
        let spec = LayerSpec {
            digest: Digest::sha256(&blob),
            media_type: GZIP_TYPES[2].to_owned(),
            diff_id: Digest::sha256(&tar),
        };
        let (dest, result) = extract_blob(&scratch, &blob, &spec, &[], &keep());
        result.unwrap();
        assert!(dest.join("two").is_file(), "the second member was read");
    }

    #[test]
    fn media_types_are_checked() {
        let scratch = Scratch::new("media");
        let mut layer = Layer::default();
        layer.file("f", b"");
        let (blob, mut spec) = layer.blob();
        for media_type in ["application/vnd.oci.image.layer.v1.tar+zstd", "text/plain"] {
            spec.media_type = media_type.to_owned();
            let (_, result) = extract_blob(&scratch, &blob, &spec, &[], &keep());
            assert!(
                matches!(result, Err(UnpackError::MediaType(_))),
                "{result:?}"
            );
        }
        // Uncompressed: the tar itself.
        let tar = layer.tar();
        let spec = LayerSpec {
            digest: Digest::sha256(&tar),
            media_type: TAR_TYPES[0].to_owned(),
            diff_id: Digest::sha256(&tar),
        };
        let (_, result) = extract_blob(&scratch, &tar, &spec, &[], &keep());
        result.unwrap();
    }

    #[test]
    fn whiteouts_of_the_layers_own_files_are_dropped() {
        let scratch = Scratch::new("own");
        let mut layer = Layer::default();
        layer
            .file(".wh.gone", b"")
            .file("gone", b"recreated")
            .file(".wh.dir", b"")
            .dir("dir", 0o755)
            .file(".wh.below", b"")
            .file(".wh..wh.plnk", b"");
        let (dest, result) = extract(&scratch, &layer);
        let report = result.unwrap();
        assert_eq!(
            report.whiteouts, 1,
            "only `below` is a lower file's whiteout"
        );
        assert_eq!(
            report.opaque_dirs, 1,
            "a recreated directory hides the lower one"
        );
        assert_eq!(report.aufs_ignored, 1);
        assert_eq!(
            std::fs::read_to_string(dest.join("gone")).unwrap(),
            "recreated"
        );
        assert!(!dest.join(".wh.gone").exists(), "never written literally");
    }

    #[test]
    fn overlay_attributes_in_a_layer_are_refused() {
        let scratch = Scratch::new("xattrs");
        let mut layer = Layer::default();
        layer
            .pax(&[
                ("SCHILY.xattr.trusted.overlay.redirect", b"/etc".as_slice()),
                ("SCHILY.xattr.user.overlay.metacopy", b"".as_slice()),
            ])
            .file("f", b"");
        let (_, result) = extract(&scratch, &layer);
        assert_eq!(result.unwrap().xattrs_refused, 2);
    }

    #[test]
    fn an_implied_directory_takes_the_lower_layers_metadata() {
        let scratch = Scratch::new("implied");
        let mut lower = Layer::default();
        lower.dir("usr", 0o750).symlink("lib", "usr/lib");
        let (lower_dir, result) = extract(&scratch, &lower);
        result.unwrap();

        let mut upper = Layer::default();
        upper.file("usr/new", b"");
        let (dest, result) = extract_with(&scratch, &upper, &[&lower_dir], &keep());
        result.unwrap();
        assert_eq!(
            std::fs::metadata(dest.join("usr")).unwrap().mode() & 0o7777,
            0o750
        );

        let mut upper = Layer::default();
        upper.file("lib/libc.so", b"");
        let (_, result) = extract_with(&scratch, &upper, &[&lower_dir], &keep());
        assert!(
            matches!(result, Err(UnpackError::LowerSymlink(_))),
            "{result:?}"
        );
    }

    #[test]
    fn sparse_files_and_bad_sizes_are_refused() {
        let scratch = Scratch::new("sparse");
        let mut layer = Layer::default();
        layer.append(
            Layer::header(b"sparse", tar::EntryType::GNUSparse, 0, 0o644),
            b"",
        );
        let (_, result) = extract(&scratch, &layer);
        assert!(
            matches!(result, Err(UnpackError::Tar(TarError::Unsupported(_)))),
            "{result:?}"
        );
    }

    /// An extended attribute's value (`getxattr(2)`), without following a
    /// symlink.
    #[cfg(target_os = "linux")]
    fn xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = std::ffi::CString::new(name).unwrap();
        let mut value = vec![0u8; 256];
        // SAFETY: both strings are NUL-terminated and live past the call;
        // `value` is a live buffer of the length given.
        let length = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        let length = usize::try_from(length).ok()?;
        value.truncate(length);
        Some(value)
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "needs root"]
    fn as_root_owners_whiteouts_opaque_markers_and_capabilities_are_made() {
        use std::os::unix::fs::FileTypeExt as _;
        // A file capability: CAP_NET_RAW, permitted and effective (v2).
        let capability: &[u8] = &[
            1, 0, 0, 2, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let scratch = Scratch::new("root");
        let mut layer = Layer::default();
        layer
            .file_owned("su", 0o4755, (1000, 1000))
            .pax(&[("SCHILY.xattr.security.capability", capability)])
            .file("ping", b"")
            .dir("opaque", 0o755)
            .file("opaque/.wh..wh..opq", b"")
            .file(".wh.gone", b"");
        let options = UnpackOptions {
            ownership: Ownership::Apply {
                uids: IdRanges::all(),
                gids: IdRanges::all(),
                shift: None,
            },
            trusted_xattrs: true,
            ..keep()
        };
        let (dest, result) = extract_with(&scratch, &layer, &[], &options);
        let report = result.unwrap();
        let su = std::fs::metadata(dest.join("su")).unwrap();
        assert_eq!((su.uid(), su.gid()), (1000, 1000));
        assert_eq!(su.mode() & 0o7777, 0o4755, "setuid kept, after the chown");
        assert_eq!(
            xattr(&dest.join("ping"), "security.capability").as_deref(),
            Some(capability)
        );
        let gone = std::fs::symlink_metadata(dest.join("gone")).unwrap();
        assert!(
            gone.file_type().is_char_device() && gone.rdev() == 0,
            "a 0:0 whiteout"
        );
        for name in ["trusted.overlay.opaque", "user.overlay.opaque"] {
            assert_eq!(
                xattr(&dest.join("opaque"), name).as_deref(),
                Some(&b"y"[..]),
                "{name}"
            );
        }
        assert_eq!((report.whiteouts, report.opaque_dirs), (1, 1));
        assert!(!dest.join(".wh.gone").exists() && !dest.join("opaque/.wh..wh..opq").exists());
    }

    /// An owner the ranges don't cover (a user namespace with too few IDs)
    /// gives way to 0, and its file loses setuid and setgid: a setuid
    /// program of root's it was never meant to be.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "needs root"]
    fn an_owner_outside_the_ranges_becomes_root_without_setuid() {
        let scratch = Scratch::new("collapse");
        let mut layer = Layer::default();
        layer.file_owned("inside", 0o4755, (1000, 1000)).file_owned(
            "outside",
            0o6755,
            (70000, 1000),
        );
        let options = UnpackOptions {
            ownership: Ownership::Apply {
                uids: IdRanges(vec![(0, 65536)]),
                gids: IdRanges(vec![(0, 65536)]),
                shift: None,
            },
            ..keep()
        };
        let (dest, result) = extract_with(&scratch, &layer, &[], &options);
        let report = result.unwrap();
        let inside = std::fs::metadata(dest.join("inside")).unwrap();
        assert_eq!((inside.uid(), inside.mode() & 0o7777), (1000, 0o4755));
        let outside = std::fs::metadata(dest.join("outside")).unwrap();
        assert_eq!((outside.uid(), outside.gid()), (0, 1000));
        assert_eq!(outside.mode() & 0o7777, 0o755, "setuid and setgid dropped");
        assert_eq!(report.ids_collapsed, 1);
    }

    proptest::proptest! {
        /// Whatever a layer of files, directories, links and whiteouts does,
        /// nothing outside it changes.
        #[test]
        fn nothing_escapes(ops in proptest::collection::vec((0u8..6, 0usize..4, 0usize..4), 1..24)) {
            let scratch = Scratch::new("prop");
            let outside = canary(&scratch);
            let target = outside.to_str().unwrap().to_owned();
            let names = ["a", "b", "a/b", "b/a"];
            let mut layer = Layer::default();
            for (op, first, second) in ops {
                let (name, other) = (names[first], names[second]);
                match op {
                    0 => { layer.file(name, b"x"); }
                    1 => { layer.dir(name, 0o755); }
                    2 => { layer.symlink(name, &target); }
                    3 => { layer.hardlink(name, &format!("{other}/canary")); }
                    4 => { layer.file(&format!("{name}/.wh.canary"), b""); }
                    _ => { layer.file(&format!("{name}/canary"), b"overwritten"); }
                }
            }
            let _ = extract(&scratch, &layer);
            assert_alive(&outside);
        }
    }
}
