//! Named volumes: persistent directories a container mounts, outliving it.
//!
//! A named volume is a directory the engine owns, under a volumes root
//! (`volumes/<name>/_data`), that a container mounts at a path inside it. Its
//! contents persist across container runs and removals — the point of a
//! volume versus the container's own writable layer, which goes when the
//! container does. This mirrors Docker's `local` volume driver and its
//! `/var/lib/docker/volumes/<name>/_data` layout.
//!
//! Volumes are plain directories, so this is portable (no kernel feature);
//! mounting one into a container is a bind mount the runtime already does.
//! This module owns the lifecycle: create, list, inspect, remove, prune.
//!
//! **A volume in use is never removed.** A run holds a shared `flock` on the
//! volume's lock file (`volumes/.<name>.lock`) for as long as it lives
//! ([`Volumes::hold`]); removing takes it exclusive, and gives up if anyone
//! holds it. A lock goes with its process, however that ends, so a killed
//! run leaves nothing to clean up. Lock files are never removed (a name's
//! lock is the same file across the volume's lives), so a lock always names
//! what it guards. A volume is removed by renaming it aside (`.trash-…`)
//! under the lock, then deleting that: what a rootless container wrote as
//! its other users needs the storage helper, which the caller runs.

use std::fs::File;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

/// The subdirectory holding a volume's actual data, as Docker names it, so a
/// little metadata could sit beside it later without clashing.
const DATA_DIR: &str = "_data";

/// Why a volume operation failed.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// An I/O error, with what was being attempted.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A volume name wasn't valid (empty, or with a path separator).
    #[error("`{0}` is not a valid volume name (letters, digits, `_`, `.`, `-`; no slashes)")]
    InvalidName(String),
    /// No volume with the given name exists.
    #[error("no such volume: {0}")]
    NotFound(String),
    /// A volume with the name already exists (on a `create` that must be new).
    #[error("the volume {0} already exists")]
    Exists(String),
    /// A running container uses the volume.
    #[error("the volume {0} is in use by a running container")]
    InUse(String),
}

/// A named volume: its name and the host directory of its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The volume's name.
    pub name: String,
    /// The host path of its data directory (what a container binds).
    pub data: PathBuf,
}

/// The store of named volumes under a root directory.
#[derive(Debug, Clone)]
pub struct Volumes {
    root: PathBuf,
}

impl Volumes {
    /// Opens (creating if needed) the volume store under `root`. Volumes live
    /// in `root/volumes/<name>/_data`.
    ///
    /// # Errors
    ///
    /// [`VolumeError::Io`] if the directory can't be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, VolumeError> {
        let root = root.into().join("volumes");
        for dir in [root.join(LOCKS_DIR), root.join(TRASH_DIR)] {
            std::fs::create_dir_all(&dir).map_err(|source| VolumeError::Io {
                context: format!("creating the volume store {}", dir.display()),
                source,
            })?;
        }
        Ok(Self { root })
    }

    /// Creates a volume named `name`, failing if it already exists.
    ///
    /// # Errors
    ///
    /// [`VolumeError::InvalidName`], [`VolumeError::Exists`], or
    /// [`VolumeError::Io`].
    pub fn create(&self, name: &str) -> Result<Volume, VolumeError> {
        validate_name(name)?;
        let dir = self.root.join(name);
        if dir.exists() {
            return Err(VolumeError::Exists(name.to_owned()));
        }
        let data = dir.join(DATA_DIR);
        std::fs::create_dir_all(&data).map_err(|source| VolumeError::Io {
            context: format!("creating the volume {name}"),
            source,
        })?;
        Ok(Volume {
            name: name.to_owned(),
            data,
        })
    }

    /// Returns a volume, creating it if it doesn't exist (the behaviour `run
    /// --volume NAME:...` wants: a named volume is made on first use). Only
    /// under its lock: see [`hold`](Self::hold).
    /// Whether this call made it is the second value: of two runs making
    /// the same volume at once, one did (its data directory's `mkdir`
    /// succeeded).
    fn get_or_create(&self, name: &str) -> Result<(Volume, bool), VolumeError> {
        validate_name(name)?;
        let dir = self.root.join(name);
        let data = dir.join(DATA_DIR);
        let creating = |source| VolumeError::Io {
            context: format!("creating the volume {name}"),
            source,
        };
        let made = if data.is_dir() {
            false
        } else {
            std::fs::create_dir_all(&dir).map_err(creating)?;
            match std::fs::create_dir(&data) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(creating(error)),
            }
        };
        let volume = Volume {
            name: name.to_owned(),
            data,
        };
        Ok((volume, made))
    }

    /// Looks up a volume by name.
    ///
    /// # Errors
    ///
    /// [`VolumeError::NotFound`] if it doesn't exist.
    pub fn inspect(&self, name: &str) -> Result<Volume, VolumeError> {
        validate_name(name)?;
        let data = self.root.join(name).join(DATA_DIR);
        if data.is_dir() {
            Ok(Volume {
                name: name.to_owned(),
                data,
            })
        } else {
            Err(VolumeError::NotFound(name.to_owned()))
        }
    }

    /// Lists all volumes, by name.
    ///
    /// # Errors
    ///
    /// [`VolumeError::Io`] if the store can't be read.
    pub fn list(&self) -> Result<Vec<Volume>, VolumeError> {
        let mut volumes = Vec::new();
        let entries = std::fs::read_dir(&self.root).map_err(|source| VolumeError::Io {
            context: format!("reading the volume store {}", self.root.display()),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| VolumeError::Io {
                context: "reading a volume entry".to_owned(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip dotfiles and anything without the data directory.
            if name.starts_with('.') {
                continue;
            }
            let data = entry.path().join(DATA_DIR);
            if data.is_dir() {
                volumes.push(Volume { name, data });
            }
        }
        volumes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(volumes)
    }

    /// The directory the volumes are in (each `<name>/_data`).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where volumes moved aside wait to be deleted (each by a name
    /// [`trash`](Self::trash) gives).
    #[must_use]
    pub fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }

    /// The data directory volume `name` has (or would have), made or not.
    ///
    /// # Errors
    ///
    /// [`VolumeError::InvalidName`].
    pub fn data_path(&self, name: &str) -> Result<PathBuf, VolumeError> {
        validate_name(name)?;
        Ok(self.root.join(name).join(DATA_DIR))
    }

    /// The volume `name`, made if it doesn't exist, held for a run: while
    /// the returned value lives, nothing removes it. Waits for a removal
    /// under way to finish moving it aside (then makes it again, empty).
    ///
    /// # Errors
    ///
    /// [`VolumeError::InvalidName`] or [`VolumeError::Io`].
    pub fn hold(&self, name: &str) -> Result<VolumeHold, VolumeError> {
        validate_name(name)?;
        let lock = self.lock_file(name)?;
        lock.lock_shared()
            .map_err(|source| Self::locking(name, source))?;
        let (volume, created) = self.get_or_create(name)?;
        Ok(VolumeHold {
            volume,
            lock,
            created,
            given_up: false,
        })
    }

    /// The volume `name`, held as [`hold`](Self::hold) holds it, only if
    /// it exists: never made (a copy from a stopped container's volume that
    /// was removed since finds none, rather than a new empty one).
    ///
    /// # Errors
    ///
    /// [`VolumeError::NotFound`] if it doesn't exist (or is being removed);
    /// [`VolumeError::InvalidName`] or [`VolumeError::Io`].
    pub fn hold_existing(&self, name: &str) -> Result<VolumeHold, VolumeError> {
        validate_name(name)?;
        let lock = self.lock_file(name)?;
        lock.lock_shared()
            .map_err(|source| Self::locking(name, source))?;
        let data = self.root.join(name).join(DATA_DIR);
        if !data.is_dir() {
            return Err(VolumeError::NotFound(name.to_owned()));
        }
        Ok(VolumeHold {
            volume: Volume {
                name: name.to_owned(),
                data,
            },
            lock,
            created: false,
            given_up: false,
        })
    }

    /// Moves the volume `name` aside for removal, unless a run holds it:
    /// what's to delete then, by its name in [`trash_dir`](Self::trash_dir).
    ///
    /// # Errors
    ///
    /// [`VolumeError::InUse`] if a run holds it, [`VolumeError::NotFound`]
    /// if there's no such volume, or [`VolumeError::Io`].
    pub fn trash(&self, name: &str) -> Result<String, VolumeError> {
        validate_name(name)?;
        let dir = self.root.join(name);
        // Checked first, so a mistyped name leaves no lock file behind; and
        // again under the lock.
        if !dir.join(DATA_DIR).is_dir() {
            return Err(VolumeError::NotFound(name.to_owned()));
        }
        let _lock = self
            .exclusive(name)?
            .ok_or_else(|| VolumeError::InUse(name.to_owned()))?;
        if !dir.join(DATA_DIR).is_dir() {
            return Err(VolumeError::NotFound(name.to_owned()));
        }
        for _ in 0..TRASH_TRIES {
            let trash = trash_name(name);
            match std::fs::rename(&dir, self.trash_dir().join(&trash)) {
                Ok(()) => return Ok(trash),
                // A leftover by that name (a reused PID): another name.
                Err(err)
                    if matches!(
                        err.raw_os_error(),
                        Some(libc_errno::ENOTEMPTY | libc_errno::EEXIST)
                    ) => {}
                Err(source) => {
                    return Err(VolumeError::Io {
                        context: format!("moving the volume {name} aside"),
                        source,
                    });
                }
            }
        }
        Err(VolumeError::Io {
            context: format!("moving the volume {name} aside"),
            source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
        })
    }

    /// Whether a run holds the volume `name` now.
    ///
    /// # Errors
    ///
    /// [`VolumeError::InvalidName`] or [`VolumeError::Io`].
    pub fn in_use(&self, name: &str) -> Result<bool, VolumeError> {
        validate_name(name)?;
        Ok(self.exclusive(name)?.is_none())
    }

    /// Moves aside every volume no run holds: the names and what to delete,
    /// the names left (in use), those that couldn't be moved (and why), and
    /// what earlier removals left in the trash (to delete as well).
    ///
    /// # Errors
    ///
    /// [`VolumeError::Io`] if the store can't be read.
    pub fn trash_unused(&self) -> Result<Pruned, VolumeError> {
        let mut pruned = Pruned::default();
        for volume in self.list()? {
            match self.trash(&volume.name) {
                Ok(trash) => pruned.trashed.push((volume.name, trash)),
                Err(VolumeError::InUse(name)) => pruned.in_use.push(name),
                // Removed by someone else meanwhile.
                Err(VolumeError::NotFound(_)) => {}
                Err(err) => pruned.failed.push((volume.name, err)),
            }
        }
        let trash_dir = self.trash_dir();
        let entries = std::fs::read_dir(&trash_dir).map_err(|source| VolumeError::Io {
            context: format!("reading {}", trash_dir.display()),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| VolumeError::Io {
                context: format!("reading {}", trash_dir.display()),
                source,
            })?;
            if let Some(name) = entry.file_name().to_str()
                && !pruned.trashed.iter().any(|(_, trash)| trash == name)
            {
                pruned.leftovers.push(name.to_owned());
            }
        }
        Ok(pruned)
    }

    /// The lock of volume `name`, taken exclusive: `None` if a run holds it.
    /// (A moment's wait, for another process only looking, as `in_use`
    /// does.)
    fn exclusive(&self, name: &str) -> Result<Option<Flock<File>>, VolumeError> {
        let mut file = self.lock_file(name)?;
        for _ in 0..LOCK_TRIES {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => return Ok(Some(lock)),
                Err((unlocked, nix::errno::Errno::EWOULDBLOCK)) => {
                    file = unlocked;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err((_, errno)) => return Err(Self::locking(name, errno)),
            }
        }
        Ok(None)
    }

    /// The lock file of volume `name` (in `.locks/`, apart from anything a
    /// volume can be named), made if needed: never through a symlink, and
    /// closed in any program this one runs.
    fn lock_file(&self, name: &str) -> Result<File, VolumeError> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = self.root.join(LOCKS_DIR).join(name);
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(lock_open_flags())
            .open(&path)
            .map_err(|source| VolumeError::Io {
                context: format!("opening {}", path.display()),
                source,
            })
    }

    fn locking(name: &str, source: impl Into<std::io::Error>) -> VolumeError {
        VolumeError::Io {
            context: format!("locking the volume {name}"),
            source: source.into(),
        }
    }
}

/// The errno values `rename` gives for a target that exists.
mod libc_errno {
    pub(super) const ENOTEMPTY: i32 = nix::errno::Errno::ENOTEMPTY as i32;
    pub(super) const EEXIST: i32 = nix::errno::Errno::EEXIST as i32;
}

/// `O_NOFOLLOW | O_CLOEXEC`, for lock files.
fn lock_open_flags() -> i32 {
    (nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_CLOEXEC).bits()
}

/// The directory of the volumes' lock files.
const LOCKS_DIR: &str = ".locks";
/// The directory of the volumes moved aside.
const TRASH_DIR: &str = ".trash";
/// How many names a volume moved aside is tried under.
const TRASH_TRIES: usize = 8;
/// How many times a lock another process only looks at is tried.
const LOCK_TRIES: usize = 10;

/// A name for volume `name` in the trash: its own, and unique (the
/// process, the time, a count).
fn trash_name(name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{name}-{}-{nanos:x}-{count}", std::process::id())
}

/// Whether `name` can name something in the trash: one plain path
/// component.
#[must_use]
pub fn is_trash(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// A volume a run holds (see [`Volumes::hold`]): released when dropped
/// (or, once [given up](Self::give_up), when every copy of its
/// [`hold`](Self::hold) is closed).
#[derive(Debug)]
pub struct VolumeHold {
    volume: Volume,
    /// Its lock file, locked (`flock`, shared): unlocked when this value goes
    /// (another thread's child, forked meanwhile, may hold a copy until it
    /// execs), unless it was given up: then this copy is only closed.
    lock: File,
    /// Whether holding it made it.
    created: bool,
    given_up: bool,
}

impl Drop for VolumeHold {
    fn drop(&mut self) {
        if !self.given_up {
            let _ = self.lock.unlock();
        }
    }
}

impl VolumeHold {
    /// The volume held.
    #[must_use]
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Whether holding it made it: a run's first use of a name.
    #[must_use]
    pub fn created(&self) -> bool {
        self.created
    }

    /// The locked descriptor, for a process that keeps the volume held (a
    /// container's monitor), once this value is [given up](Self::give_up).
    #[must_use]
    pub fn hold(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.lock)
    }

    /// Gives the volume up to whoever holds a copy of its
    /// [`hold`](Self::hold): this value's copy is closed, never unlocked, so
    /// the volume stays held while any copy is open.
    pub fn give_up(mut self) {
        self.given_up = true;
    }
}

/// What [`Volumes::trash_unused`] did.
#[derive(Debug, Default)]
pub struct Pruned {
    /// The volumes moved aside: each name, and its trash name.
    pub trashed: Vec<(String, String)>,
    /// The volumes left, in use.
    pub in_use: Vec<String>,
    /// Trash an earlier removal left (cut short), to delete too.
    pub leftovers: Vec<String>,
    /// The volumes that couldn't be moved aside, and why.
    pub failed: Vec<(String, VolumeError)>,
}

/// Validates a volume name: non-empty, and only the characters Docker allows
/// (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`), so it's a single safe path component.
fn validate_name(name: &str) -> Result<(), VolumeError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(VolumeError::InvalidName(name.to_owned()))
    }
}

/// Whether `source` names a volume (a bare name: no `/`) rather than a host
/// path. This is how `--volume SOURCE:DEST` tells the two apart, as Docker
/// does.
#[must_use]
pub fn is_named_volume(source: &str) -> bool {
    validate_name(source).is_ok()
}

/// A parsed `--volume SOURCE:DEST[:ro]` specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    /// The source: a named volume or a host path.
    pub source: String,
    /// The mount point inside the container (absolute).
    pub destination: PathBuf,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl VolumeMount {
    /// Parses `SOURCE:DEST` or `SOURCE:DEST:ro` / `:rw`.
    ///
    /// # Errors
    ///
    /// Returns a message if the form is wrong or the destination isn't an
    /// absolute path.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let parts: Vec<&str> = spec.split(':').collect();
        let (source, destination, mode) = match parts.as_slice() {
            [source, destination] => (*source, *destination, None),
            [source, destination, mode] => (*source, *destination, Some(*mode)),
            _ => {
                return Err(format!("a volume is SOURCE:DEST[:ro|rw], not `{spec}`"));
            }
        };
        if source.is_empty() {
            return Err(format!("the volume source is empty in `{spec}`"));
        }
        if !is_named_volume(source) && !Path::new(source).is_absolute() {
            return Err(format!(
                "`{source}` is neither a volume name nor an absolute host path"
            ));
        }
        let destination = PathBuf::from(destination);
        if !destination.is_absolute() {
            return Err(format!(
                "the volume mount point `{}` must be an absolute path",
                destination.display()
            ));
        }
        let read_only = match mode {
            None | Some("rw") => false,
            Some("ro") => true,
            Some(other) => return Err(format!("a volume mode is ro or rw, not `{other}`")),
        };
        Ok(Self {
            source: source.to_owned(),
            destination,
            read_only,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("shipyard-vol-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A hold dropped (not given up) frees the volume at once, even while a
    /// stray copy of its lock's descriptor is open (another thread's child,
    /// between its fork and its exec): it's unlocked, not only closed.
    #[test]
    fn a_dropped_hold_frees_the_volume_despite_a_stray_copy() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let hold = volumes.hold("data").unwrap();
        let stray = hold.hold().try_clone_to_owned().unwrap();
        drop(hold);
        assert!(volumes.trash("data").is_ok(), "free despite the copy");
        drop(stray);
    }

    /// A volume handed over (a copy of its hold, as a container's monitor
    /// keeps) stays held once its holder has gone: it can't be removed until
    /// the copy is closed. Dropping the holder only closes, never unlocks.
    #[test]
    fn a_volume_handed_over_stays_held() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let hold = volumes.hold("data").unwrap();
        let copy = hold.hold().try_clone_to_owned().unwrap();
        hold.give_up();
        assert!(
            matches!(volumes.trash("data"), Err(VolumeError::InUse(_))),
            "held by the copy"
        );
        drop(copy);
        assert!(
            volumes.trash("data").is_ok(),
            "free once the copy is closed"
        );
    }

    #[test]
    fn a_hold_says_whether_it_made_the_volume() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        assert!(volumes.hold("new").unwrap().created());
        assert!(!volumes.hold("new").unwrap().created());
    }

    #[test]
    fn a_created_volume_can_be_inspected_and_listed() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let created = volumes.create("data").unwrap();
        assert!(created.data.is_dir());
        assert_eq!(volumes.inspect("data").unwrap(), created);
        assert_eq!(volumes.list().unwrap(), vec![created]);
    }

    #[test]
    fn creating_an_existing_volume_is_an_error_but_get_or_create_is_idempotent() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        volumes.create("v").unwrap();
        assert!(matches!(volumes.create("v"), Err(VolumeError::Exists(_))));
        // get_or_create returns the same one twice.
        let (a, made) = volumes.get_or_create("v").unwrap();
        assert!(!made, "it existed");
        let (b, made) = volumes.get_or_create("v").unwrap();
        assert!(!made);
        assert_eq!(a, b);
    }

    #[test]
    fn a_held_volume_is_never_moved_aside() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let held = volumes.hold("busy").unwrap();
        assert!(held.volume().data.is_dir(), "made on first use");
        std::fs::write(held.volume().data.join("f"), "kept").unwrap();
        volumes.create("idle").unwrap();
        assert!(matches!(volumes.trash("busy"), Err(VolumeError::InUse(_))));
        assert!(volumes.in_use("busy").unwrap() && !volumes.in_use("idle").unwrap());
        // What a removal cut short left in the trash.
        std::fs::create_dir(volumes.trash_dir().join("old-1-0")).unwrap();
        let pruned = volumes.trash_unused().unwrap();
        assert_eq!(pruned.in_use, ["busy"]);
        assert_eq!(pruned.trashed.len(), 1);
        assert_eq!(pruned.trashed[0].0, "idle");
        assert!(is_trash(&pruned.trashed[0].1));
        assert_eq!(pruned.leftovers, ["old-1-0"]);
        assert_eq!(volumes.list().unwrap(), [held.volume().clone()]);
        assert_eq!(
            std::fs::read(held.volume().data.join("f")).unwrap(),
            b"kept"
        );
        drop(held);
        let trash = volumes.trash("busy").unwrap();
        assert!(volumes.trash_dir().join(trash).join("_data").is_dir());
        assert!(volumes.list().unwrap().is_empty());
    }

    /// A volume can be named anything a trash or lock name could look like:
    /// they live apart.
    #[test]
    fn no_volume_name_meets_a_lock_or_the_trash() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let held = volumes.hold("trash-foo").unwrap();
        volumes.create("locks").unwrap();
        let pruned = volumes.trash_unused().unwrap();
        assert_eq!(pruned.in_use, ["trash-foo"]);
        assert!(pruned.leftovers.is_empty(), "{:?}", pruned.leftovers);
        assert!(matches!(
            volumes.trash("trash-foo"),
            Err(VolumeError::InUse(_))
        ));
        drop(held);
    }

    #[test]
    fn a_mistyped_name_leaves_nothing_behind() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        assert!(matches!(
            volumes.trash("typo"),
            Err(VolumeError::NotFound(_))
        ));
        assert_eq!(
            std::fs::read_dir(volumes.root().join(".locks"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_missing_volume_is_not_found() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        assert!(matches!(
            volumes.inspect("ghost"),
            Err(VolumeError::NotFound(_))
        ));
        assert!(matches!(
            volumes.trash("ghost"),
            Err(VolumeError::NotFound(_))
        ));
    }

    #[test]
    fn bad_names_are_refused() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        for bad in ["", "../escape", "a/b", ".hidden", "-leading"] {
            assert!(
                matches!(volumes.create(bad), Err(VolumeError::InvalidName(_))),
                "{bad:?} should be invalid"
            );
        }
    }

    #[test]
    fn volume_specs_parse() {
        let v = VolumeMount::parse("data:/var/lib/data").unwrap();
        assert_eq!(v.source, "data");
        assert_eq!(v.destination, Path::new("/var/lib/data"));
        assert!(!v.read_only);

        let ro = VolumeMount::parse("/host/path:/mnt:ro").unwrap();
        assert_eq!(ro.source, "/host/path");
        assert!(ro.read_only);

        assert!(
            VolumeMount::parse("data:relative").is_err(),
            "dest must be absolute"
        );
        assert!(VolumeMount::parse("onlyone").is_err());
        assert!(
            VolumeMount::parse("./data:/mnt").is_err(),
            "a host path is absolute"
        );
        assert!(VolumeMount::parse("a:/b:bogus").is_err());
    }

    #[test]
    fn named_volumes_versus_host_paths() {
        assert!(is_named_volume("data"));
        assert!(is_named_volume("my-vol_1.2"));
        assert!(!is_named_volume("/host/path"));
        assert!(!is_named_volume("./rel"));
    }
}
