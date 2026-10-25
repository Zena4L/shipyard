//! Files Shipyard keeps its own state in: written whole or not at all,
//! read with a bound, and lock files, each opened so a link someone else
//! planted is refused rather than followed.
//!
//! - **Writes** go to a temporary file made with `O_EXCL` (a name already
//!   there, planted or left by a crash, is never written through), synced,
//!   then renamed over the target: `rename(2)` is atomic within a directory
//!   (TLPI §18.4), so a reader sees the old file or the new one. Syncing the
//!   directory afterwards makes the rename itself survive a crash (§13.3).
//! - **Reads** open with `O_NOFOLLOW` (a symlink is refused), `O_NONBLOCK` (a
//!   FIFO in a file's place can't block the reader), check what was actually
//!   opened with `fstat` (only a regular file is read), and refuse a file
//!   larger than its cap rather than read it whole or cut it short.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::errno::Errno;
use nix::fcntl::{OFlag, openat, renameat};
use nix::sys::stat::Mode;
use nix::unistd::{UnlinkatFlags, unlinkat};

/// What of a write survives the machine stopping (a power cut, a crash of
/// the kernel), beyond what any write gives: whole or not at all to every
/// reader, whatever process is killed when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// The file and then its directory synced: after a restart, the new
    /// file is there, whole.
    Synced,
    /// The file synced, its directory not: after a restart, the new file
    /// or the old one, whole.
    FileSynced,
    /// Nothing synced: for a file in a directory a restart empties anyway
    /// (`/run`, a tmpfs), where a sync would cost and keep nothing.
    Atomic,
}

/// What a [`Write`] calls on its temporary file before writing to it.
type Prepare<'a> = &'a dyn Fn(&File) -> io::Result<()>;

/// How [`write_atomically_at`] writes a file.
#[derive(Clone, Copy)]
pub struct Write<'a> {
    mode: u32,
    durability: Durability,
    prepare: Option<Prepare<'a>>,
}

impl<'a> Write<'a> {
    /// A file made with permissions `mode` (less the umask, as `open(2)`
    /// makes any file), synced, its directory synced.
    #[must_use]
    pub fn new(mode: u32) -> Self {
        Self {
            mode,
            durability: Durability::Synced,
            prepare: None,
        }
    }

    /// The same, with `durability`.
    #[must_use]
    pub fn durability(self, durability: Durability) -> Self {
        Self { durability, ..self }
    }

    /// The same, `prepare` called on the temporary file before anything is
    /// written to it (to set its exact permissions or its group, say).
    #[must_use]
    pub fn prepared_by(self, prepare: Prepare<'a>) -> Self {
        Self {
            prepare: Some(prepare),
            ..self
        }
    }
}

impl fmt::Debug for Write<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Write")
            .field("mode", &format_args!("{:o}", self.mode))
            .field("durability", &self.durability)
            .field("prepare", &self.prepare.is_some())
            .finish()
    }
}

/// The step of [`write_atomically_at`] that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Opening the directory, or making the temporary file.
    Open,
    /// Preparing, writing or syncing the temporary file.
    Write,
    /// Renaming it over the target.
    Rename,
    /// Syncing the directory after the rename: the new file is in place,
    /// but a crash could still bring back the old one.
    SyncDirectory,
}

/// A write that failed: at which step, and why.
#[derive(Debug)]
pub struct WriteError {
    /// The step that failed.
    pub step: Step,
    /// Why.
    pub source: io::Error,
    /// The temporary file's name in the directory, once one was chosen
    /// (chosen, not necessarily made: its `open` may be what failed).
    pub temporary: Option<OsString>,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<WriteError> for io::Error {
    fn from(error: WriteError) -> Self {
        error.source
    }
}

fn failed(step: Step) -> impl FnOnce(io::Error) -> WriteError {
    move |source| WriteError {
        step,
        source,
        temporary: None,
    }
}

/// How many temporary names a write tries before it gives up: one is taken
/// only if a crash left it behind (with this PID, after a reboot) or someone
/// planted it, and the next is a new name.
const TEMPORARY_TRIES: u32 = 16;

/// Writes `bytes` to `name` in the directory open at `dir`, whole or not
/// at all (see the module's doc). In a directory that was removed, the
/// temporary file can't be made: `ENOENT`, and nothing is made anywhere
/// else.
///
/// # Errors
///
/// The step that failed and its error. Before the rename, the temporary
/// file this write made is removed (never a file it didn't make) and the
/// old file is untouched.
pub fn write_atomically_at(
    dir: impl AsFd,
    name: impl AsRef<OsStr>,
    bytes: &[u8],
    how: &Write<'_>,
) -> Result<(), WriteError> {
    let dir = dir.as_fd();
    let name = name.as_ref();
    let mode = libc::mode_t::try_from(how.mode)
        .map(Mode::from_bits_truncate)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
        .map_err(failed(Step::Open))?;
    let (file, temporary) = make_temporary(dir, name, mode)?;
    let placed = fill(File::from(file), bytes, how)
        .map_err(failed(Step::Write))
        .and_then(|()| {
            renameat(dir, temporary.as_os_str(), dir, name)
                .map_err(|errno| failed(Step::Rename)(errno.into()))
        });
    if let Err(error) = placed {
        let _ = unlinkat(dir, temporary.as_os_str(), UnlinkatFlags::NoRemoveDir);
        #[cfg(test)]
        hooks::count(&hooks::TEMPORARIES_REMOVED);
        return Err(WriteError {
            temporary: Some(temporary),
            ..error
        });
    }
    if how.durability == Durability::Synced {
        sync_directory_fd(dir).map_err(failed(Step::SyncDirectory))?;
    }
    Ok(())
}

/// [`write_atomically_at`] for the file at `path`, in its directory (opened
/// the way `rename(2)` itself would reach it: links on the way followed).
/// A directory this user can write in but not read (a drop box, Linux) is
/// reached with `O_PATH`, which can't be synced: there [`Durability::Synced`]
/// syncs the file only ([`Durability::FileSynced`]).
///
/// # Errors
///
/// As [`write_atomically_at`]'s; [`Step::Open`] if the directory can't be
/// opened.
pub fn write_atomically(path: &Path, bytes: &[u8], how: &Write<'_>) -> Result<(), WriteError> {
    let (dir, name) = split(path).map_err(failed(Step::Open))?;
    let (dir, can_sync) = open_to_write_in(&dir).map_err(failed(Step::Open))?;
    if can_sync {
        return write_atomically_at(dir, name, bytes, how);
    }
    let how = Write {
        durability: match how.durability {
            Durability::Synced => Durability::FileSynced,
            other => other,
        },
        ..*how
    };
    write_atomically_at(dir, name, bytes, &how)
}

/// The directory at `dir`, opened to make, rename and remove files in, and
/// whether it can be synced through what's open. On Linux, one this user
/// may write in and search but not list (a drop box: mode 0733) is reached
/// with `O_PATH`, which needs no more than that, as a path would, but can't
/// be synced through.
fn open_to_write_in(dir: &Path) -> io::Result<(OwnedFd, bool)> {
    match open_directory(dir) {
        Ok(opened) => Ok((opened.into(), true)),
        #[cfg(target_os = "linux")]
        Err(error) if error.raw_os_error() == Some(libc::EACCES) => {
            let flags = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
            let opened = nix::fcntl::open(dir, flags, Mode::empty()).map_err(io::Error::from)?;
            Ok((opened, false))
        }
        Err(error) => Err(error),
    }
}

/// Makes a temporary file for `name` in `dir`, exclusively: a name already
/// there is never opened, the next one is tried instead.
fn make_temporary(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    mode: Mode,
) -> Result<(OwnedFd, OsString), WriteError> {
    let mut tries = 0;
    loop {
        let temporary = temporary_name(name).map_err(failed(Step::Open))?;
        let made = openat(
            dir,
            temporary.as_os_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            mode,
        );
        tries += 1;
        match made {
            Ok(file) => return Ok((file, temporary)),
            Err(Errno::EEXIST) if tries < TEMPORARY_TRIES => {}
            Err(errno) => {
                return Err(WriteError {
                    temporary: Some(temporary),
                    ..failed(Step::Open)(errno.into())
                });
            }
        }
    }
}

/// The temporary file's data written and synced.
fn fill(mut file: File, bytes: &[u8], how: &Write<'_>) -> io::Result<()> {
    if let Some(prepare) = how.prepare {
        prepare(&file)?;
    }
    file.write_all(bytes)?;
    if how.durability == Durability::Atomic {
        return Ok(());
    }
    #[cfg(test)]
    hooks::count(&hooks::FILES_SYNCED);
    file.sync_all()
}

/// `.NAME.PID.N.tmp`: unique to this process (the PID) and to this write
/// (the counter), hidden from a plain listing.
fn temporary_name(name: &OsStr) -> io::Result<OsString> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    if let Some(forced) = hooks::NEXT_TEMPORARY
        .take()
        .or_else(|| hooks::EVERY_TEMPORARY.with_borrow(Clone::clone))
    {
        return Ok(forced);
    }
    if name.is_empty() || name.as_bytes().contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a file name, not a path",
        ));
    }
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(temporary)
}

/// `path`'s directory (`.` for a bare name) and its last component.
fn split(path: &Path) -> io::Result<(PathBuf, &OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((dir, name))
}

fn open_directory(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(dir)
}

/// Syncs the directory at `dir`: what was made, renamed or removed in it is
/// on the disk once this returns. A file system that can't sync a directory
/// at all (`EINVAL`: 9p and some FUSE ones) has nothing more to give: that's
/// done too.
///
/// # Errors
///
/// It couldn't be opened or synced.
pub fn sync_directory(dir: &Path) -> io::Result<()> {
    sync_directory_fd(open_directory(dir)?.as_fd())
}

fn sync_directory_fd(dir: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(test)]
    hooks::count(&hooks::DIRECTORIES_SYNCED);
    match fsync(dir) {
        Err(Errno::EINVAL) => Ok(()),
        synced => synced.map_err(io::Error::from),
    }
}

#[cfg(not(test))]
fn fsync(fd: BorrowedFd<'_>) -> nix::Result<()> {
    nix::unistd::fsync(fd)
}

#[cfg(test)]
fn fsync(fd: BorrowedFd<'_>) -> nix::Result<()> {
    match hooks::FSYNC_FAILS.with(std::cell::Cell::get) {
        Some(errno) => Err(errno),
        None => nix::unistd::fsync(fd),
    }
}

const READ_FLAGS: libc::c_int = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// Reads the file `name` in the directory open at `dir`, whole, if it's a
/// regular file of at most `cap` bytes (see the module's doc).
///
/// # Errors
///
/// The `open(2)` error (`ENOENT`, `ELOOP` for a symlink…);
/// a [`Refusal`] (see [`refusal`]) if it isn't a regular file or is larger
/// than `cap`.
pub fn read_bounded_at(dir: impl AsFd, name: impl AsRef<OsStr>, cap: u64) -> io::Result<Vec<u8>> {
    let flags = OFlag::O_RDONLY | OFlag::from_bits_truncate(READ_FLAGS);
    let file = openat(dir, name.as_ref(), flags, Mode::empty()).map_err(io::Error::from)?;
    read_bounded_from(&File::from(file), cap)
}

/// [`read_bounded_at`] for the file at `path` (the directories on the way
/// may be links; the file itself may not).
///
/// # Errors
///
/// As [`read_bounded_at`]'s.
pub fn read_bounded(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    read_bounded_from(&open_for_reading(path)?, cap)
}

/// Opens the file at `path` the way [`read_bounded`] does (never through a
/// symlink at its name, never waiting on a FIFO), for a caller that needs
/// the open file itself too (its times, say), then reads it with
/// [`read_bounded_from`].
///
/// # Errors
///
/// The `open(2)` error.
pub fn open_for_reading(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(READ_FLAGS)
        .open(path)
}

/// Why a read refused a file it opened: told apart from any error the
/// system gave (an `EINVAL`, an `EFBIG`) by [`refusal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a regular file (a FIFO, a directory, a device).
    NotAFile,
    /// Larger than the bound it was read with.
    TooLarge {
        /// The bound, in bytes.
        cap: u64,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAFile => f.write_str("not a regular file"),
            Self::TooLarge { cap } => write!(f, "larger than {cap} bytes"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for io::Error {
    fn from(refusal: Refusal) -> Self {
        let kind = match refusal {
            Refusal::NotAFile => io::ErrorKind::InvalidInput,
            Refusal::TooLarge { .. } => io::ErrorKind::FileTooLarge,
        };
        io::Error::new(kind, refusal)
    }
}

/// The [`Refusal`] `error` is, if a bounded read refused the file (rather
/// than the system failing it).
#[must_use]
pub fn refusal(error: &io::Error) -> Option<Refusal> {
    error.get_ref()?.downcast_ref::<Refusal>().copied()
}

/// `bytes` as text, failed as `std::fs::read_to_string` fails text that
/// isn't UTF-8 ([`io::ErrorKind::InvalidData`]), for a bounded read that
/// replaces it.
///
/// # Errors
///
/// It isn't UTF-8.
pub fn text(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

/// Reads `file`, already open (to read it as the file it locked or
/// checked, not one put at its name since), with [`read_bounded_at`]'s
/// rules: a regular file of at most `cap` bytes, read from where it's at.
///
/// # Errors
///
/// A [`Refusal`] if it isn't a regular file or is larger than `cap`; a read
/// error.
pub fn read_bounded_from(file: &File, cap: u64) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Refusal::NotAFile.into());
    }
    // Too large already: refused unread. One that grows meanwhile is cut at
    // `cap + 1` bytes, and refused all the same.
    if metadata.len() > cap {
        return Err(Refusal::TooLarge { cap }.into());
    }
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > cap) {
        return Err(Refusal::TooLarge { cap }.into());
    }
    Ok(bytes)
}

/// Opens `path`, a lock file (made if it's missing, the caller's alone, a
/// symlink there refused), to `flock`. Read and write, as the engine's,
/// network's and volumes' lock files always were (the image store's are
/// opened read-only: [`open_lock_file_at`]).
///
/// # Errors
///
/// It couldn't be opened.
pub fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// Opens the lock file `name` in the directory open at `dir` (made if it's
/// missing, the caller's alone, a symlink there refused), read-only: `flock`
/// needs no more, and a lock file another user made stays usable. By
/// descriptor, as the image store reaches everything.
///
/// # Errors
///
/// The `openat(2)` errno.
pub fn open_lock_file_at(dir: impl AsFd, name: impl AsRef<OsStr>) -> nix::Result<OwnedFd> {
    let dir = dir.as_fd();
    // macOS can answer ENOENT to an `O_CREAT` open while another process
    // creates the same name at that moment: the name exists by the retry.
    let open = || {
        openat(
            dir,
            name.as_ref(),
            OFlag::O_RDONLY | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
    };
    match open() {
        Err(Errno::ENOENT) => open(),
        opened => opened,
    }
}

/// Sets `file`'s permissions to exactly `mode`, whatever the umask made
/// them (for [`Write::prepared_by`]).
///
/// # Errors
///
/// The `fchmod(2)` error.
pub fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

/// Why a directory isn't the caller's alone, if it isn't: not a directory
/// (or a symbolic link), another user's, or one others can write to (they
/// could plant files, or a symbolic link, where the caller reads, writes and
/// deletes). `None`: it's the caller's alone.
///
/// # Errors
///
/// It can't be looked at (it's missing, say).
pub fn unsafe_dir(dir: &Path) -> io::Result<Option<&'static str>> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(dir)?;
    Ok(if !metadata.is_dir() {
        Some("it isn't a directory (or it's a symbolic link)")
    } else if metadata.uid() != nix::unistd::geteuid().as_raw() {
        Some("it belongs to another user")
    } else if metadata.mode() & 0o022 != 0 {
        Some("others can write to it")
    } else {
        None
    })
}

/// Makes `dir` (and what's missing above it) if it's missing, the caller's
/// alone (0700), then [checks](unsafe_dir) it is: why it isn't, if it isn't.
/// The runtime's state root and the Docker API's socket directory are made
/// so.
///
/// # Errors
///
/// It couldn't be made or looked at.
pub fn private_dir(dir: &Path) -> io::Result<Option<&'static str>> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    unsafe_dir(dir)
}

/// The directory `below` names inside `dir`, each of its components a
/// directory opened without following a symlink (`O_NOFOLLOW`): a
/// component that is a symlink, or isn't a directory, is an error
/// (`ELOOP`, `ENOTDIR`), never a way out of `dir`. `below` is relative and
/// plain (no `..`, no `/` at its start). Open for reference only (`O_PATH`
/// on Linux): what a bind's source or a walk needs.
///
/// # Errors
///
/// `below` isn't plain ([`io::ErrorKind::InvalidInput`]); a component
/// that's missing, a symlink or not a directory.
pub fn open_below(dir: BorrowedFd<'_>, below: &Path) -> io::Result<OwnedFd> {
    walk_below(dir, below, None)
}

/// The ID of the mount `fd` is on (`statx`'s `STX_MNT_ID`, Linux 5.8):
/// what tells a mount point from its parent where `st_dev` can't (overlayfs
/// gives a file the device of the layer it's in, without `xino`).
///
/// # Errors
///
/// The `statx`'s; [`io::ErrorKind::Unsupported`] from a kernel that doesn't
/// give the mount's ID.
#[cfg(target_os = "linux")]
pub fn mount_id(fd: impl AsFd) -> io::Result<u64> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    let stat = statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this kernel doesn't give a mount's ID (statx's STX_MNT_ID, Linux 5.8)",
        ));
    }
    Ok(stat.stx_mnt_id)
}

/// [`open_below`], making each missing component (`mkdirat`, `mode`) on
/// the way: what a bind's missing source is made with, so no symlink
/// another put on its path is ever followed (11.7). One made meanwhile by
/// another is taken as it is, if it's a directory.
///
/// # Errors
///
/// As [`open_below`]'s; a component that can't be made.
pub fn make_below(dir: BorrowedFd<'_>, below: &Path, mode: u32) -> io::Result<OwnedFd> {
    walk_below(dir, below, Some(mode))
}

/// [`open_below`] and [`make_below`]'s walk: each component made first if
/// `make` gives its mode.
fn walk_below(dir: BorrowedFd<'_>, below: &Path, make: Option<u32>) -> io::Result<OwnedFd> {
    use std::path::Component;
    #[cfg(target_os = "linux")]
    let reference = OFlag::O_PATH;
    #[cfg(not(target_os = "linux"))]
    let reference = OFlag::O_RDONLY;
    let flags = reference | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut names = Vec::new();
    for component in below.components() {
        match component {
            Component::Normal(name) => names.push(name),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: not a plain relative path", below.display()),
                ));
            }
        }
    }
    // The permission bits alone, as `mode_t` is wherever this runs (16 bits
    // on macOS).
    let mode = make.map(|mode| {
        Mode::from_bits_truncate(libc::mode_t::try_from(mode & 0o7777).unwrap_or_default())
    });
    let mut at = nix::unistd::dup(dir)?;
    for name in names {
        if let Some(mode) = mode {
            match nix::sys::stat::mkdirat(&at, name, mode) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
        at = openat(&at, name, flags, Mode::empty())?;
    }
    Ok(at)
}

#[cfg(test)]
mod hooks {
    use std::cell::Cell;

    use nix::errno::Errno;

    thread_local! {
        pub(super) static DIRECTORIES_SYNCED: Cell<usize> = const { Cell::new(0) };
        pub(super) static FILES_SYNCED: Cell<usize> = const { Cell::new(0) };
        pub(super) static TEMPORARIES_REMOVED: Cell<usize> = const { Cell::new(0) };
        pub(super) static FSYNC_FAILS: Cell<Option<Errno>> = const { Cell::new(None) };
        pub(super) static NEXT_TEMPORARY: Cell<Option<std::ffi::OsString>> = const { Cell::new(None) };
        pub(super) static EVERY_TEMPORARY: std::cell::RefCell<Option<std::ffi::OsString>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn count(counter: &'static std::thread::LocalKey<Cell<usize>>) {
        counter.with(|count| count.set(count.get() + 1));
    }
}

#[cfg(test)]
mod tests;
