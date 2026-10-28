//! Filesystem work by directory descriptor, never by path string.
//!
//! A path like `layer/etc/passwd` is resolved by the kernel one component at
//! a time, and any of them may be a symlink it follows (`path_resolution(7)`).
//! In a directory whose contents came from an untrusted layer, `etc` may be a
//! symlink to the host's `/etc`, and a write "inside the layer" lands on the
//! host. So nothing here takes a path below a trusted root: every step opens
//! one name relative to an already-open directory (`openat(2)` and the other
//! `*at()` calls, TLPI 18.11), with `O_NOFOLLOW`, so a symlink is an error
//! where a directory is expected and is never walked through.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, unlinkat};

use crate::store::LockMode;

/// Opens directory `name` in `dir`, refusing a symlink (`ELOOP`) or
/// anything that isn't a directory (`ENOTDIR`).
///
/// # Errors
///
/// The `openat(2)` errno.
pub fn open_dir_at(dir: &impl AsFd, name: &OsStr) -> nix::Result<OwnedFd> {
    openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

/// Opens a trusted directory by path (following symlinks: a store under a
/// symlinked `~/.local/share` is fine). Everything below it is then reached
/// with the `*at()` calls.
///
/// # Errors
///
/// The `open(2)` errno.
pub fn open_dir(path: &Path) -> nix::Result<OwnedFd> {
    nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

/// What `name` in `dir` is, without following a symlink; `None` if it
/// doesn't exist.
///
/// # Errors
///
/// The `fstatat(2)` errno, other than `ENOENT`.
pub fn stat_at(dir: &impl AsFd, name: &OsStr) -> nix::Result<Option<FileStat>> {
    match fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::ENOENT) => Ok(None),
        Err(errno) => Err(errno),
    }
}

/// The file type bits of a stat.
#[must_use]
pub fn kind(stat: &FileStat) -> SFlag {
    SFlag::from_bits_truncate(stat.st_mode & SFlag::S_IFMT.bits())
}

/// A walk down a tree from a root directory descriptor, keeping the open
/// directories of the last path walked: entries of a tar archive come
/// depth-first, so most share their parent with the one before and cost no
/// system call at all. Holds at most one descriptor per level of the deepest
/// path (bounded by the caller's path limits).
#[derive(Debug)]
pub struct DirWalker {
    root: OwnedFd,
    /// The open directories below the root, with their names.
    stack: Vec<(OsString, OwnedFd)>,
}

/// Why a walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// A component is a symlink: never walked through.
    Symlink,
    /// A component is something other than a directory.
    NotADirectory,
    /// A component doesn't exist (and the walk doesn't create).
    Missing,
    /// Any other failure.
    Sys(Errno),
}

impl DirWalker {
    /// A walker from `root`.
    #[must_use]
    pub fn new(root: OwnedFd) -> Self {
        Self {
            root,
            stack: Vec::new(),
        }
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &OwnedFd {
        &self.root
    }

    /// Opens the directory at `path` (relative to the root, normal
    /// components only), reusing what's open of it. With `made`, missing
    /// components are created with `mkdirat` (mode 0700, for the caller to
    /// set properly), and each new one's relative path and identity is
    /// pushed to it; once one is made, all below it are new too. Without,
    /// a missing component is [`WalkError::Missing`].
    ///
    /// # Errors
    ///
    /// A [`WalkError`]: a symlink or a non-directory on the way, a missing
    /// component when not creating, or another failure.
    pub fn enter(
        &mut self,
        path: &Path,
        mut made: Option<&mut Vec<(PathBuf, (u64, u64))>>,
    ) -> Result<&OwnedFd, WalkError> {
        let names: Vec<&OsStr> = path.iter().collect();
        // Keep the open prefix this path shares with the last one.
        let shared = self
            .stack
            .iter()
            .zip(&names)
            .take_while(|((open, _), name)| open.as_os_str() == **name)
            .count();
        self.stack.truncate(shared);
        for (depth, name) in names.iter().enumerate().skip(shared) {
            let parent = self.stack.last().map_or(&self.root, |(_, fd)| fd);
            let opened = match (open_dir_at(parent, name), made.as_deref_mut()) {
                (Ok(fd), _) => fd,
                (Err(Errno::ENOENT), Some(made)) => {
                    match mkdirat(parent, *name, Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(errno) => return Err(WalkError::Sys(errno)),
                    }
                    let fd =
                        open_dir_at(parent, name).map_err(|errno| classify(parent, name, errno))?;
                    let stat = fstat(&fd).map_err(WalkError::Sys)?;
                    made.push((names[..=depth].iter().collect(), identity(&stat)));
                    fd
                }
                (Err(errno), _) => return Err(classify(parent, name, errno)),
            };
            self.stack.push(((*name).to_owned(), opened));
        }
        Ok(self.stack.last().map_or(&self.root, |(_, fd)| fd))
    }

    /// Forgets the open directories at and below `path`, after it was
    /// removed or replaced.
    pub fn forget(&mut self, path: &Path) {
        let depth = path.iter().count();
        if depth == 0 {
            self.stack.clear();
            return;
        }
        let matches = self
            .stack
            .iter()
            .zip(path.iter())
            .take_while(|((open, _), name)| open.as_os_str() == *name)
            .count();
        if matches == depth {
            self.stack.truncate(depth - 1);
        }
    }
}

/// Turns an `openat` errno for `name` in `dir` into what it means on a
/// walk. A symlink opened with `O_NOFOLLOW | O_DIRECTORY` fails with `ELOOP`
/// on Linux but `ENOTDIR` on macOS, so `ENOTDIR` is checked.
fn classify(dir: &OwnedFd, name: &OsStr, errno: Errno) -> WalkError {
    match errno {
        Errno::ELOOP | Errno::EMLINK => WalkError::Symlink,
        Errno::ENOTDIR => match stat_at(dir, name) {
            Ok(Some(stat)) if kind(&stat) == SFlag::S_IFLNK => WalkError::Symlink,
            _ => WalkError::NotADirectory,
        },
        Errno::ENOENT => WalkError::Missing,
        other => WalkError::Sys(other),
    }
}

/// Removes `name` from `dir`: a file, a symlink (never followed), or a whole
/// directory tree, emptied one level at a time by descriptor.
///
/// The walk holds two descriptors at a time, whatever the depth: it goes
/// down by `openat(O_NOFOLLOW)` and back up by `..`, checking at each step up
/// that `..` is the directory it came from (by device and inode), so a tree
/// rearranged under it can't lead it out. Without the right to read or
/// search a directory (a layer's `0555` or `0000` ones, removed by their
/// owner without `CAP_DAC_OVERRIDE`), it first gives the owner `rwx`.
///
/// # Errors
///
/// The errno of the step that failed.
pub fn remove_tree_at(dir: &OwnedFd, name: &OsStr) -> nix::Result<()> {
    let Some(stat) = stat_at(dir, name)? else {
        return Ok(());
    };
    if kind(&stat) != SFlag::S_IFDIR {
        return unlink_or_fix(dir, name);
    }
    // The path from `dir` down to the current directory: each level's name
    // and identity, to check the way back up. What's gone meanwhile (another
    // removal of the same tree got there first) is taken as removed.
    let mut trail: Vec<(OsString, (u64, u64))> = Vec::new();
    let mut current = match open_for_removal(dir, name) {
        Err(Errno::ENOENT) => return Ok(()),
        opened => opened?,
    };
    trail.push((name.to_owned(), identity(&fstat(&current)?)));
    loop {
        // Remove what's here; the first subdirectory found is descended into.
        let mut descended = false;
        for child in list(&current)? {
            let Some(stat) = stat_at(&current, &child)? else {
                continue;
            };
            if kind(&stat) == SFlag::S_IFDIR {
                let below = match open_for_removal(&current, &child) {
                    Err(Errno::ENOENT) => continue,
                    opened => opened?,
                };
                trail.push((child, identity(&fstat(&below)?)));
                current = below;
                descended = true;
                break;
            }
            unlink_or_fix(&current, &child)?;
        }
        if descended {
            continue;
        }
        // Empty: go back up and remove it.
        let Some((empty, _)) = trail.pop() else {
            return Ok(());
        };
        let parent = match trail.last() {
            Some((_, expected)) => {
                let parent = open_dir_at(&current, OsStr::new(".."))?;
                if identity(&fstat(&parent)?) != *expected {
                    // Moved under us: stop rather than remove the wrong thing.
                    return Err(Errno::ESTALE);
                }
                parent
            }
            None => dir.try_clone().map_err(|_| Errno::EBADF)?,
        };
        match unlinkat(&parent, empty.as_os_str(), UnlinkatFlags::RemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => {}
            Err(Errno::EACCES) => {
                fchmod(&parent, Mode::from_bits_truncate(0o700))?;
                unlinkat(&parent, empty.as_os_str(), UnlinkatFlags::RemoveDir)?;
            }
            Err(errno) => return Err(errno),
        }
        if trail.is_empty() {
            return Ok(());
        }
        current = parent;
    }
}

/// Opens a directory to empty it, giving its owner `rwx` first if it can't
/// be read, or after, if it can be read but not searched or written (listing
/// it goes through its `.`, which takes search; removing what's in it takes
/// write).
fn open_for_removal(parent: &OwnedFd, name: &OsStr) -> nix::Result<OwnedFd> {
    let dir = match open_dir_at(parent, name) {
        Err(Errno::EACCES) => {
            make_searchable(parent, name)?;
            open_dir_at(parent, name)?
        }
        other => other?,
    };
    if fstat(&dir)?.st_mode & 0o700 != 0o700 {
        fchmod(&dir, Mode::from_bits_truncate(0o700))?;
    }
    Ok(dir)
}

/// Gives the owner of directory `name` in `parent` `rwx`, never through a
/// symlink: the directory is first opened as a path only (`O_PATH`, which
/// needs no permission on it, with `O_NOFOLLOW`), and the mode changed
/// through `/proc/self/fd`, which reaches exactly that directory.
#[cfg(target_os = "linux")]
fn make_searchable(parent: &OwnedFd, name: &OsStr) -> nix::Result<()> {
    use std::os::fd::AsRawFd as _;
    let dir = openat(
        parent,
        name,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    nix::sys::stat::fchmodat(
        nix::fcntl::AT_FDCWD,
        format!("/proc/self/fd/{}", dir.as_raw_fd()).as_str(),
        Mode::from_bits_truncate(0o700),
        nix::sys::stat::FchmodatFlags::FollowSymlink,
    )
}

/// Elsewhere there's no `O_PATH`: the name is checked to be a directory
/// first. (Only the store's owner can change it in between, and nothing
/// off Linux mounts the store.)
#[cfg(not(target_os = "linux"))]
fn make_searchable(parent: &OwnedFd, name: &OsStr) -> nix::Result<()> {
    match stat_at(parent, name)? {
        Some(stat) if kind(&stat) == SFlag::S_IFDIR => nix::sys::stat::fchmodat(
            parent,
            name,
            Mode::from_bits_truncate(0o700),
            nix::sys::stat::FchmodatFlags::NoFollowSymlink,
        ),
        _ => Err(Errno::ENOTDIR),
    }
}

/// Unlinks a non-directory, giving the directory's owner write access first
/// if that's what's missing.
fn unlink_or_fix(dir: &OwnedFd, name: &OsStr) -> nix::Result<()> {
    match unlinkat(dir, name, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(Errno::EACCES) => {
            fchmod(dir, Mode::from_bits_truncate(0o700))?;
            unlinkat(dir, name, UnlinkatFlags::NoRemoveDir)
        }
        Err(errno) => Err(errno),
    }
}

/// The names in a directory, but `.` and `..`, from a fresh open of it (so
/// its read position starts at the beginning).
///
/// # Errors
///
/// The errno of opening or reading it.
pub(crate) fn list(dir: &OwnedFd) -> nix::Result<Vec<OsString>> {
    let mut names = Vec::new();
    let _: std::ops::ControlFlow<()> = for_each_name(dir, |name| {
        names.push(name.to_owned());
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(names)
}

/// How much a tree takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeUsage {
    /// The bytes of its files (a file linked more than once counted once).
    pub bytes: u64,
    /// Whether all of it was walked: `false`, there's more (a directory
    /// that couldn't be read, past a [`Budget`], or a mount inside).
    pub complete: bool,
}

/// How much `name` in `dir` takes: a file's size, or a directory's files'
/// (walked by descriptor, never through a symlink). A directory that can't
/// be read (another user's) is left out, and the usage marked incomplete.
#[must_use]
pub fn tree_usage(dir: &OwnedFd, name: &OsStr) -> TreeUsage {
    tree_usage_within(dir, name, &Budget::UNLIMITED)
}

/// How far a [`tree_usage_within`] may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// The most entries looked at (each a `fstatat`).
    pub entries: u64,
    /// The most levels below `name` walked (each holds a descriptor while
    /// its subtree is walked).
    pub depth: u32,
    /// The most directories waiting their turn (each a name held).
    pub waiting: usize,
    /// Whether to stay on `name`'s own mount (one inside it, a btrfs
    /// subvolume too, is neither walked nor counted).
    pub one_filesystem: bool,
    /// When to stop, whatever is left.
    pub until: Option<std::time::Instant>,
}

impl Budget {
    /// No bound, every mount: [`tree_usage`]'s.
    pub const UNLIMITED: Self = Self {
        entries: u64::MAX,
        depth: u32::MAX,
        waiting: usize::MAX,
        one_filesystem: false,
        until: None,
    };
}

/// [`tree_usage`] within `budget`: past it, what's left is left out and the
/// usage marked incomplete (a tree someone else fills, a container's
/// volume, can't hold the walker, its memory or its descriptors).
#[must_use]
pub fn tree_usage_within(dir: &OwnedFd, name: &OsStr, budget: &Budget) -> TreeUsage {
    let mut walk = TreeWalk {
        usage: TreeUsage {
            bytes: 0,
            complete: true,
        },
        linked: std::collections::HashSet::new(),
        pending: Vec::new(),
        budget: *budget,
        seen: 0,
        mount: None,
    };
    let Ok(dir) = dir.try_clone() else {
        walk.usage.complete = false;
        return walk.usage;
    };
    if walk.visit(&std::rc::Rc::new(dir), name, 0).is_break() {
        return walk.usage;
    }
    // A directory is opened when its turn comes, its parent kept open
    // meanwhile: the descriptors open grow with the tree's depth, and the
    // names waiting with its width, up to the budget's.
    while let Some((parent, name, depth)) = walk.pending.pop() {
        let Ok(directory) = open_dir_at(&*parent, &name) else {
            walk.usage.complete = false;
            continue;
        };
        drop(parent);
        if !walk.on_its_mount(&directory) {
            continue;
        }
        let directory = std::rc::Rc::new(directory);
        let read = for_each_name(&directory, |child| walk.visit(&directory, child, depth + 1));
        match read {
            Ok(std::ops::ControlFlow::Break(())) => return walk.usage,
            Ok(std::ops::ControlFlow::Continue(())) => {}
            Err(_) => walk.usage.complete = false,
        }
    }
    walk.usage
}

/// A [`tree_usage_within`] under way.
struct TreeWalk {
    usage: TreeUsage,
    /// The files seen with more than one link.
    linked: std::collections::HashSet<(u64, u64)>,
    /// The directories still to walk: each by its parent, name and depth.
    pending: Vec<(std::rc::Rc<OwnedFd>, OsString, u32)>,
    budget: Budget,
    /// The entries looked at.
    seen: u64,
    /// The first directory's mount, where the walk stays if asked to.
    mount: Option<u64>,
}

impl TreeWalk {
    /// Counts `name` in `parent` (`depth` levels below the first), or
    /// leaves it to walk later; past the budget's entries or time, the
    /// walk's end.
    fn visit(
        &mut self,
        parent: &std::rc::Rc<OwnedFd>,
        name: &OsStr,
        depth: u32,
    ) -> std::ops::ControlFlow<()> {
        let late = self.seen.is_multiple_of(1024)
            && self
                .budget
                .until
                .is_some_and(|until| std::time::Instant::now() >= until);
        if self.seen >= self.budget.entries || late {
            self.usage.complete = false;
            return std::ops::ControlFlow::Break(());
        }
        self.seen += 1;
        match stat_at(&**parent, name) {
            Ok(Some(stat)) if kind(&stat) == SFlag::S_IFDIR => {
                if depth >= self.budget.depth || self.pending.len() >= self.budget.waiting {
                    self.usage.complete = false;
                } else {
                    self.pending.push((parent.clone(), name.to_owned(), depth));
                }
            }
            Ok(Some(stat)) => {
                if stat.st_nlink <= 1 || self.linked.insert(identity(&stat)) {
                    let size = u64::try_from(stat.st_size).unwrap_or(0);
                    self.usage.bytes = self.usage.bytes.saturating_add(size);
                }
            }
            // Gone meanwhile.
            Ok(None) => {}
            Err(_) => self.usage.complete = false,
        }
        std::ops::ControlFlow::Continue(())
    }

    /// Whether `directory` is on the walk's first mount (always, unless
    /// asked to stay on one): a mount inside is said, never walked.
    fn on_its_mount(&mut self, directory: &OwnedFd) -> bool {
        if !self.budget.one_filesystem {
            return true;
        }
        let Some(mount) = mount_of(directory) else {
            self.usage.complete = false;
            return false;
        };
        if *self.mount.get_or_insert(mount) == mount {
            true
        } else {
            self.usage.complete = false;
            false
        }
    }
}

/// The mount `directory` is on: its mount ID on Linux (a btrfs subvolume
/// has a device of its own but is no mount: `statx`'s `STX_MNT_ID` tells),
/// its device elsewhere.
fn mount_of(directory: &OwnedFd) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        shipyard_sys::fs::mount_id(directory).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        fstat(directory).ok().map(|stat| identity(&stat).0)
    }
}

/// Each name in `directory` but `.` and `..`, handed to `each` as it's read
/// (a fresh open, so from the beginning), until `each` breaks.
///
/// # Errors
///
/// The errno of opening or reading it.
fn for_each_name(
    directory: &OwnedFd,
    mut each: impl FnMut(&OsStr) -> std::ops::ControlFlow<()>,
) -> nix::Result<std::ops::ControlFlow<()>> {
    let fresh = open_dir_at(directory, OsStr::new("."))?;
    let mut read = nix::dir::Dir::from_fd(fresh)?;
    for entry in read.iter() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if each(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name)).is_break() {
            return Ok(std::ops::ControlFlow::Break(()));
        }
    }
    Ok(std::ops::ControlFlow::Continue(()))
}

/// What an entry of a [`listing`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link (never followed).
    Link,
    /// Anything else (a FIFO, a socket, a device).
    Other,
}

/// One entry of a directory, as a [`listing`] says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its name, as the directory holds it.
    pub name: OsString,
    /// What it is (`Other` too for one that couldn't be looked at).
    pub kind: EntryKind,
    /// Its own size (a directory's own, not its tree's).
    pub size: u64,
}

/// The entries of directory `name` in `dir`, by name, at most `max`, the
/// rest counted; opened without following a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The first `max` entries, by name (of those read).
    pub entries: Vec<Listed>,
    /// How many more there are (of those read).
    pub more: usize,
    /// Whether every name was read: `false`, the directory holds more than
    /// `scan` names, and both are of the first `scan` read.
    pub complete: bool,
}

/// Lists directory `name` in `dir`, reading at most `scan` of its names
/// (a directory of millions costs `scan` names' time, `max` names' memory):
/// see [`Listing`].
///
/// # Errors
///
/// The errno of opening or reading it (a link or a non-directory refused).
pub fn listing(dir: &impl AsFd, name: &OsStr, max: usize, scan: usize) -> nix::Result<Listing> {
    let directory = open_dir_at(dir, name)?;
    // The first `max` names in order, kept as they're read (the largest out
    // when another comes).
    let mut first: std::collections::BinaryHeap<OsString> = std::collections::BinaryHeap::new();
    let mut more = 0;
    let mut read = 0;
    let complete = for_each_name(&directory, |name| {
        if read >= scan {
            return std::ops::ControlFlow::Break(());
        }
        read += 1;
        first.push(name.to_owned());
        if first.len() > max {
            first.pop();
            more += 1;
        }
        std::ops::ControlFlow::Continue(())
    })?
    .is_continue();
    let entries = first
        .into_sorted_vec()
        .into_iter()
        .map(|name| {
            let stat = stat_at(&directory, &name).ok().flatten();
            let kind = match stat.as_ref().map(kind) {
                Some(SFlag::S_IFREG) => EntryKind::File,
                Some(SFlag::S_IFDIR) => EntryKind::Directory,
                Some(SFlag::S_IFLNK) => EntryKind::Link,
                _ => EntryKind::Other,
            };
            Listed {
                name,
                kind,
                size: stat.map_or(0, |stat| u64::try_from(stat.st_size).unwrap_or(0)),
            }
        })
        .collect();
    Ok(Listing {
        entries,
        more,
        complete,
    })
}

/// A file's identity: its device and inode.
#[must_use]
pub fn identity(stat: &FileStat) -> (u64, u64) {
    #[allow(
        clippy::cast_sign_loss,
        clippy::unnecessary_cast,
        reason = "dev_t is i32 on macOS and u64 on Linux; device numbers aren't negative"
    )]
    (stat.st_dev as u64, stat.st_ino as u64)
}

/// Takes an `flock(2)` on the lock file `name` in `dir` (made if missing,
/// mode 0600), shared or exclusive, waiting up to `wait` for whoever holds
/// it the other way. The lock belongs to the open file description: it's
/// released when the returned value drops, or when every process holding
/// a descriptor of it has ended, however they ended, so a lock can't
/// outlive its holders. Opened read-only (`flock` needs no more, and a lock
/// file another user made stays usable) and close-on-exec (nothing this
/// process execs inherits it by accident).
///
/// # Errors
///
/// `EWOULDBLOCK` if it's still held at the deadline; another errno if the
/// file can't be opened or locked.
pub fn lock_file(
    dir: &OwnedFd,
    name: &str,
    mode: LockMode,
    wait: std::time::Duration,
) -> nix::Result<nix::fcntl::Flock<OwnedFd>> {
    let fd = shipyard_sys::fs::open_lock_file_at(dir.as_fd(), name)?;
    lock(fd, mode, wait)
}

/// Takes an `flock(2)` on `fd`, as [`lock_file`] does.
fn lock(
    fd: OwnedFd,
    mode: LockMode,
    wait: std::time::Duration,
) -> nix::Result<nix::fcntl::Flock<OwnedFd>> {
    use shipyard_sys::lock::Mode;
    let mode = match mode {
        LockMode::Exclusive => Mode::Exclusive,
        LockMode::Shared => Mode::Shared,
    };
    let deadline = std::time::Instant::now() + wait;
    shipyard_sys::lock::lock_until(fd, mode, deadline, std::time::Duration::from_millis(20))
}

/// Renames `name` in `dir` to a fresh `.trash-…` name, which it returns;
/// `None` if there's no such name. Removing something big then can't stop
/// half-way under its own name: what's left is trash, for later.
///
/// # Errors
///
/// The `rename(2)` errno.
pub(crate) fn move_to_trash(dir: &OwnedFd, name: &str) -> nix::Result<Option<OsString>> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let trash = OsString::from(format!(".trash-{name}-{}-{nonce}", std::process::id()));
        // `renameat2(RENAME_NOREPLACE)` isn't everywhere: a stale trash name
        // of the same PID is checked for first (only this process makes
        // names with its PID).
        if stat_at(dir, &trash)?.is_some() {
            continue;
        }
        return match nix::fcntl::renameat(dir, name, dir, trash.as_os_str()) {
            Ok(()) => Ok(Some(trash)),
            Err(Errno::ENOENT) => Ok(None),
            Err(errno) => Err(errno),
        };
    }
}

/// Whether `name` is something a process left behind in a store
/// directory: trash from a removal cut short, or a temporary from work
/// cut short (`.trash-…`, `.tmp-…`, `.new-…`). A single name.
#[must_use]
pub(crate) fn is_leftover(name: &str) -> bool {
    [".trash-", ".tmp-", ".new-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        && !name.contains('/')
}

/// Sets an extended attribute on an open file (`fsetxattr(2)`; `nix` has no
/// xattr calls).
///
/// # Errors
///
/// The errno: `ENOTSUP` where the filesystem has no such namespace, `EPERM`
/// where the caller may not set it.
#[cfg(target_os = "linux")]
pub fn set_xattr(fd: &impl AsFd, name: &OsStr, value: &[u8]) -> nix::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| Errno::EINVAL)?;
    // SAFETY: the descriptor is open for the call's duration; `name` is a
    // NUL-terminated string and `value`/`len` describe a live byte slice.
    let result = unsafe {
        libc::fsetxattr(
            fd.as_fd().as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    Errno::result(result).map(drop)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A scratch directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("shipyard-fsat-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn fd(&self) -> OwnedFd {
            open_dir(&self.0).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_walk_makes_missing_directories_and_refuses_symlinks() {
        let scratch = Scratch::new("walk");
        let outside = Scratch::new("walk-outside");
        std::os::unix::fs::symlink(&outside.0, scratch.0.join("link")).unwrap();
        std::fs::write(scratch.0.join("file"), "").unwrap();
        let mut walker = DirWalker::new(scratch.fd());
        let mut made = Vec::new();
        walker.enter(Path::new("a/b"), Some(&mut made)).unwrap();
        let made: Vec<PathBuf> = made.into_iter().map(|(path, _)| path).collect();
        assert_eq!(made, [PathBuf::from("a"), PathBuf::from("a/b")]);
        assert!(scratch.0.join("a/b").is_dir());
        assert_eq!(
            walker
                .enter(Path::new("link/x"), Some(&mut Vec::new()))
                .err(),
            Some(WalkError::Symlink)
        );
        assert!(!outside.0.join("x").exists(), "made through the symlink");
        assert_eq!(
            walker
                .enter(Path::new("file/x"), Some(&mut Vec::new()))
                .err(),
            Some(WalkError::NotADirectory)
        );
        assert_eq!(
            walker.enter(Path::new("c"), None).err(),
            Some(WalkError::Missing)
        );
    }

    #[test]
    fn a_trees_usage_counts_each_file_once_and_follows_nothing() {
        let scratch = Scratch::new("usage");
        let tree = scratch.0.join("tree");
        std::fs::create_dir_all(tree.join("a/b/c")).unwrap();
        std::fs::write(tree.join("a/one"), [0u8; 100]).unwrap();
        std::fs::write(tree.join("a/b/c/two"), [0u8; 20]).unwrap();
        // A second link to `one` counts nothing more.
        std::fs::hard_link(tree.join("a/one"), tree.join("a/b/again")).unwrap();
        // A symlink out of the tree is not followed (only its own size).
        let outside = scratch.0.join("outside");
        std::fs::write(&outside, [0u8; 5000]).unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("out")).unwrap();
        let link_size = std::fs::symlink_metadata(tree.join("out")).unwrap().len();
        // Many directories side by side: walked one at a time.
        for i in 0..300 {
            std::fs::create_dir(tree.join(format!("wide{i}"))).unwrap();
        }
        let usage = tree_usage(&scratch.fd(), OsStr::new("tree"));
        assert!(usage.complete);
        assert_eq!(usage.bytes, 120 + link_size);
        // A file on its own is its size; nothing at all, nothing.
        assert_eq!(tree_usage(&scratch.fd(), OsStr::new("outside")).bytes, 5000);
        assert_eq!(tree_usage(&scratch.fd(), OsStr::new("none")).bytes, 0);
    }

    /// A walk within a budget stops past its entries or its depth and says
    /// so; within it, the same as an unbounded one.
    #[test]
    fn a_walk_within_a_budget_stops_and_says_so() {
        let scratch = Scratch::new("budget");
        let tree = scratch.0.join("tree");
        // 300 levels deep, a 10-byte file at the bottom; 50 files of 1 byte.
        let mut deep = tree.clone();
        for _ in 0..300 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("bottom"), [0u8; 10]).unwrap();
        for i in 0..50 {
            std::fs::write(tree.join(format!("f{i}")), [0u8; 1]).unwrap();
        }
        let within = |entries, depth, waiting, until| {
            tree_usage_within(
                &scratch.fd(),
                OsStr::new("tree"),
                &Budget {
                    entries,
                    depth,
                    waiting,
                    one_filesystem: true,
                    until,
                },
            )
        };
        let all = within(10_000, 1000, 1000, None);
        assert_eq!(
            all,
            TreeUsage {
                bytes: 60,
                complete: true
            }
        );
        let shallow = within(10_000, 256, 1000, None);
        assert!(!shallow.complete, "past its depth");
        assert_eq!(shallow.bytes, 50, "the bottom file left out");
        // 20 entries: `tree` itself, then at most 19 files of a byte.
        let few = within(20, 1000, 1000, None);
        assert!(!few.complete, "past its entries");
        assert!(few.bytes <= 19, "{few:?}");
        let late = within(10_000, 1000, 1000, Some(std::time::Instant::now()));
        assert_eq!(
            late,
            TreeUsage {
                bytes: 0,
                complete: false
            },
            "past its time"
        );
        // No directory may wait: the first level's walked, nothing below.
        let narrow = within(10_000, 1000, 0, None);
        assert!(!narrow.complete, "past its waiting");
    }

    /// A mount inside a tree walked on its own filesystem is neither walked
    /// nor counted, and the usage said incomplete; walked across, it is
    /// (the control).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root"]
    fn a_walk_on_one_filesystem_never_crosses_a_mount() {
        use nix::mount::{MntFlags, MsFlags, mount, umount2};
        let scratch = Scratch::new("mount");
        let inside = scratch.0.join("tree/m");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(scratch.0.join("tree/own"), [0u8; 3]).unwrap();
        mount(
            Some("tmpfs"),
            &inside,
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )
        .unwrap();
        std::fs::write(inside.join("other"), [0u8; 1000]).unwrap();
        let walk = |one_filesystem| {
            tree_usage_within(
                &scratch.fd(),
                OsStr::new("tree"),
                &Budget {
                    one_filesystem,
                    ..Budget::UNLIMITED
                },
            )
        };
        let (on_one, across) = (walk(true), walk(false));
        umount2(&inside, MntFlags::MNT_DETACH).unwrap();
        assert_eq!(
            on_one,
            TreeUsage {
                bytes: 3,
                complete: false
            }
        );
        assert_eq!(
            across,
            TreeUsage {
                bytes: 1003,
                complete: true
            }
        );
    }

    /// A directory's listing: its entries by name, each what it is (a link
    /// never followed), at most `max`, the rest counted; a link refused as
    /// the directory itself.
    #[test]
    fn a_listing_is_bounded_and_follows_nothing() {
        let scratch = Scratch::new("listing");
        let outside = Scratch::new("listing-outside");
        std::fs::write(outside.0.join("secret"), "x").unwrap();
        let dir = scratch.0.join("vol");
        std::fs::create_dir_all(dir.join("b-dir")).unwrap();
        std::fs::write(dir.join("a-file"), [0u8; 7]).unwrap();
        std::os::unix::fs::symlink(&outside.0, dir.join("c-link")).unwrap();
        std::fs::write(dir.join("d-more"), "").unwrap();
        let listed = listing(&scratch.fd(), OsStr::new("vol"), 3, 100).unwrap();
        let seen: Vec<(&OsStr, EntryKind)> = listed
            .entries
            .iter()
            .map(|entry| (entry.name.as_os_str(), entry.kind))
            .collect();
        assert_eq!(
            seen,
            [
                (OsStr::new("a-file"), EntryKind::File),
                (OsStr::new("b-dir"), EntryKind::Directory),
                (OsStr::new("c-link"), EntryKind::Link),
            ]
        );
        assert_eq!(listed.entries[0].size, 7);
        assert_eq!(listed.more, 1);
        assert!(listed.complete);
        // Two names read of four: what's said is of those two.
        let scanned = listing(&scratch.fd(), OsStr::new("vol"), 3, 2).unwrap();
        assert_eq!(
            (scanned.entries.len(), scanned.more, scanned.complete),
            (2, 0, false)
        );
        std::os::unix::fs::symlink(&outside.0, scratch.0.join("linked")).unwrap();
        // Refused (Linux says `ELOOP`, macOS `ENOTDIR`), never listed.
        assert!(matches!(
            listing(&scratch.fd(), OsStr::new("linked"), 10, 100),
            Err(Errno::ELOOP | Errno::ENOTDIR)
        ));
    }

    #[test]
    fn a_tree_is_removed_without_following_symlinks() {
        let scratch = Scratch::new("remove");
        let outside = Scratch::new("remove-outside");
        std::fs::write(outside.0.join("canary"), "alive").unwrap();
        let tree = scratch.0.join("tree");
        std::fs::create_dir_all(tree.join("a/b/c")).unwrap();
        std::fs::write(tree.join("a/b/c/file"), "").unwrap();
        std::fs::write(tree.join("a/file"), "").unwrap();
        std::os::unix::fs::symlink(&outside.0, tree.join("a/escape")).unwrap();
        // A directory its owner can't read or write.
        std::fs::create_dir(tree.join("locked")).unwrap();
        std::fs::write(tree.join("locked/inside"), "").unwrap();
        std::fs::set_permissions(tree.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        // Ones it can read but not search, or not write (the fuzzer's: a
        // copied archive's `tmp` with mode 3651).
        for (name, mode) in [("unsearchable", 0o3651), ("readonly", 0o555)] {
            std::fs::create_dir(tree.join(name)).unwrap();
            std::fs::write(tree.join(name).join("inside"), "").unwrap();
            std::fs::set_permissions(tree.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        remove_tree_at(&scratch.fd(), OsStr::new("tree")).unwrap();
        assert!(!tree.exists());
        assert_eq!(
            std::fs::read_to_string(outside.0.join("canary")).unwrap(),
            "alive"
        );
    }

    #[test]
    fn a_deep_tree_is_removed_with_few_descriptors() {
        let scratch = Scratch::new("deep");
        let mut path = scratch.0.join("deep");
        for _ in 0..300 {
            path.push("d");
        }
        std::fs::create_dir_all(&path).unwrap();
        remove_tree_at(&scratch.fd(), OsStr::new("deep")).unwrap();
        assert!(!scratch.0.join("deep").exists());
    }

    #[test]
    fn removing_a_missing_name_or_a_file_works() {
        let scratch = Scratch::new("missing");
        remove_tree_at(&scratch.fd(), OsStr::new("nothing")).unwrap();
        std::fs::write(scratch.0.join("file"), "").unwrap();
        remove_tree_at(&scratch.fd(), OsStr::new("file")).unwrap();
        assert!(!scratch.0.join("file").exists());
    }
}
