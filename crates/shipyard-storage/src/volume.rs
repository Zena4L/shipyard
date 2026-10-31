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
//! volume's lock file (`volumes/.locks/<name>`) for as long as it lives
//! ([`Volumes::hold`]); removing takes it exclusive, and gives up if anyone
//! holds it. A lock goes with its process, however that ends, so a killed
//! run leaves nothing to clean up. A volume is removed by renaming it aside
//! (`.trash-…`) under the lock, then deleting that: what a rootless
//! container wrote as its other users needs the storage helper, which the
//! caller runs. Its lock files go with it, unlinked under its lock (F150:
//! they piled up, two a volume ever made). A lock is on an open file, not a
//! name, so whoever opened one before it went can still lock it, and would
//! hold a lock that guards nothing: every lock taken here is checked, once
//! held, to still be the file at its path (`still_linked`), and taken again
//! on the new one if not.
//!
//! A volume made with labels (Docker's API, step 11.2c2a) keeps them in
//! `volumes/<name>/labels.json` (versioned from its first write, H2),
//! written once, before its data directory: a volume is there when its data
//! directory is, so a half-made one is never listed. One without the file
//! has none (every volume made before).

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

/// The subdirectory holding a volume's actual data, as Docker names it, so a
/// little metadata could sit beside it later without clashing.
const DATA_DIR: &str = "_data";

/// A volume's labels, beside its data directory.
const LABELS_FILE: &str = "labels.json";
/// The newest `labels.json` this Shipyard reads and writes.
const LABELS_VERSION: u32 = 1;
/// The most a `labels.json` holds: written no bigger, read no further.
const LABELS_MAX: u64 = 64 * 1024;
/// How long a create waits for the volume's lock (a run making it, a
/// removal moving it aside: moments).
const CREATE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// When a volume was made, beside its data directory (F146: its
/// directory's own time moves with what's made and removed beside `_data`,
/// a first fill's `filling` marker among it).
const CREATED_FILE: &str = "created.json";
/// The newest `created.json` this Shipyard reads and writes.
const CREATED_VERSION: u32 = 1;
/// The most a `created.json` holds.
const CREATED_MAX: u64 = 1024;

/// `created.json`: its version, and the making's time since the epoch.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatedFile {
    version: u32,
    seconds: u64,
    nanos: u32,
}

/// `labels.json`: its version, and the labels.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelsFile {
    version: u32,
    labels: BTreeMap<String, String>,
}

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
    /// Its `labels.json` isn't one this Shipyard reads: a newer version,
    /// or not one at all.
    #[error("the labels of the volume {name}: {source}")]
    Labels {
        /// The volume.
        name: String,
        /// Why.
        #[source]
        source: shipyard_sys::versioned::Error,
    },
    /// Its `created.json` isn't one this Shipyard reads: a newer version,
    /// or not one at all.
    #[error("when the volume {name} was made: {source}")]
    Created {
        /// The volume.
        name: String,
        /// Why.
        #[source]
        source: shipyard_sys::versioned::Error,
    },
    /// Labels too many to keep (their file would be over 64 KiB).
    #[error("the labels of the volume {0} are too many: 64 KiB at most")]
    TooManyLabels(String),
    /// Its lock was held past the wait (a removal, a run making it, another
    /// fill).
    #[error("the volume {0} is busy: try again")]
    Busy(String),
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
        for dir in [
            root.join(LOCKS_DIR),
            root.join(FILL_LOCKS_DIR),
            root.join(TRASH_DIR),
        ] {
            std::fs::create_dir_all(&dir).map_err(|source| VolumeError::Io {
                context: format!("creating the volume store {}", dir.display()),
                source,
            })?;
        }
        // The caller's alone, whatever the directory above it is: a volume
        // may hold what an image put there (a setuid-root file, say: the
        // copy-up keeps the image's owners and modes), which no other user
        // may reach (11.6b's security review). An older one is narrowed.
        let private = <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700);
        std::fs::set_permissions(&root, private).map_err(|source| VolumeError::Io {
            context: format!("making the volume store {} private", root.display()),
            source,
        })?;
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
        let _ = self.write_created(name);
        Ok(Volume {
            name: name.to_owned(),
            data,
        })
    }

    /// Creates a volume named `name` with `labels` (Docker's API): under its
    /// lock, exclusive, so a run making it at once (shared) waits or finds
    /// it made; its `labels.json` first (if it has labels), then its data
    /// directory. A failure leaves nothing of it.
    ///
    /// # Errors
    ///
    /// [`VolumeError::InvalidName`], [`VolumeError::Exists`] (or held by a
    /// run: then it exists too), or [`VolumeError::Io`].
    pub fn create_with(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Volume, VolumeError> {
        validate_name(name)?;
        let dir = self.root.join(name);
        let data = dir.join(DATA_DIR);
        // Made already: said at once, never after waiting out a run that
        // holds it (shared, for its whole life: 11.6b's runtime review;
        // Docker's answers with the volume as it is).
        if data.is_dir() {
            return Err(VolumeError::Exists(name.to_owned()));
        }
        let deadline = std::time::Instant::now() + CREATE_WAIT;
        let _lock = loop {
            let lock = shipyard_sys::lock::lock_until(
                self.lock_file(name)?,
                shipyard_sys::lock::Mode::Exclusive,
                deadline,
                std::time::Duration::from_millis(20),
            )
            .map_err(|errno| match errno {
                // Held by a run that made it meanwhile: it exists.
                nix::errno::Errno::EWOULDBLOCK if data.is_dir() => {
                    VolumeError::Exists(name.to_owned())
                }
                nix::errno::Errno::EWOULDBLOCK => VolumeError::Busy(name.to_owned()),
                errno => Self::locking(name, errno),
            })?;
            if self.still_its_lock(name, &lock)? {
                break lock;
            }
        };
        if data.is_dir() {
            return Err(VolumeError::Exists(name.to_owned()));
        }
        let creating = |source| VolumeError::Io {
            context: format!("creating the volume {name}"),
            source,
        };
        // A directory without data is what a create cut short left: taken.
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(creating(error)),
        }
        let made = self
            .write_labels(name, labels)
            .and_then(|()| self.write_created(name))
            .and_then(|()| {
                std::fs::create_dir(&data).map_err(|error| {
                    // Made meanwhile by what takes no lock (`create`): it's there.
                    if error.kind() == std::io::ErrorKind::AlreadyExists && data.is_dir() {
                        VolumeError::Exists(name.to_owned())
                    } else {
                        creating(error)
                    }
                })
            });
        if let Err(error) = made {
            let _ = std::fs::remove_file(dir.join(LABELS_FILE));
            let _ = std::fs::remove_file(dir.join(CREATED_FILE));
            let _ = std::fs::remove_dir(&dir);
            return Err(error);
        }
        Ok(Volume {
            name: name.to_owned(),
            data,
        })
    }

    /// Writes `labels` as the volume `name`'s `labels.json` (none: no file,
    /// any left by a create cut short removed).
    fn write_labels(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), VolumeError> {
        let path = self.root.join(name).join(LABELS_FILE);
        let io = |source| VolumeError::Io {
            context: format!("writing {}", path.display()),
            source,
        };
        if labels.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(io(error)),
                _ => Ok(()),
            };
        }
        let file = LabelsFile {
            version: LABELS_VERSION,
            labels: labels.clone(),
        };
        let json = serde_json::to_vec(&file).map_err(|error| io(std::io::Error::other(error)))?;
        if u64::try_from(json.len()).is_ok_and(|len| len > LABELS_MAX) {
            return Err(VolumeError::TooManyLabels(name.to_owned()));
        }
        shipyard_sys::fs::write_atomically(&path, &json, &shipyard_sys::fs::Write::new(0o644))
            .map_err(|error| io(error.source))
    }

    /// The labels of the volume `name`: none if it has no `labels.json`
    /// (one made by a run, or before labels were kept).
    ///
    /// # Errors
    ///
    /// [`VolumeError::Labels`] if the file is of a newer version or isn't
    /// one; [`VolumeError::Io`] if it can't be read (never through a
    /// symlink, never past 64 KiB).
    pub fn labels(&self, name: &str) -> Result<BTreeMap<String, String>, VolumeError> {
        validate_name(name)?;
        let path = self.root.join(name).join(LABELS_FILE);
        match shipyard_sys::versioned::read_file::<LabelsFile>(&path, LABELS_MAX, LABELS_VERSION) {
            Ok(file) => Ok(file.map(|file| file.labels).unwrap_or_default()),
            Err(shipyard_sys::versioned::FileError::Io(source)) => Err(VolumeError::Io {
                context: format!("reading {}", path.display()),
                source,
            }),
            Err(shipyard_sys::versioned::FileError::Versioned(source)) => {
                Err(VolumeError::Labels {
                    name: name.to_owned(),
                    source,
                })
            }
        }
    }

    /// Writes now as the volume `name`'s making, in its `created.json`.
    fn write_created(&self, name: &str) -> Result<(), VolumeError> {
        let path = self.root.join(name).join(CREATED_FILE);
        let io = |source| VolumeError::Io {
            context: format!("writing {}", path.display()),
            source,
        };
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let file = CreatedFile {
            version: CREATED_VERSION,
            seconds: since.as_secs(),
            nanos: since.subsec_nanos(),
        };
        let json = serde_json::to_vec(&file).map_err(|error| io(std::io::Error::other(error)))?;
        shipyard_sys::fs::write_atomically(&path, &json, &shipyard_sys::fs::Write::new(0o644))
            .map_err(|error| io(error.source))
    }

    /// When the volume `name` was made: its `created.json`'s time; one
    /// made before it was kept (or whose making was cut before it was
    /// written), its directory's last change, which a first fill moves.
    ///
    /// # Errors
    ///
    /// [`VolumeError::NotFound`]; [`VolumeError::Created`] for a
    /// `created.json` of a newer version, or that isn't one;
    /// [`VolumeError::Io`].
    pub fn created(&self, name: &str) -> Result<std::time::SystemTime, VolumeError> {
        self.inspect(name)?;
        let dir = self.root.join(name);
        let path = dir.join(CREATED_FILE);
        match shipyard_sys::versioned::read_file::<CreatedFile>(&path, CREATED_MAX, CREATED_VERSION)
        {
            Ok(Some(file)) => {
                return Ok(std::time::UNIX_EPOCH
                    + std::time::Duration::new(file.seconds, file.nanos.min(999_999_999)));
            }
            Ok(None) => {}
            Err(shipyard_sys::versioned::FileError::Io(source)) => {
                return Err(VolumeError::Io {
                    context: format!("reading {}", path.display()),
                    source,
                });
            }
            Err(shipyard_sys::versioned::FileError::Versioned(source)) => {
                return Err(VolumeError::Created {
                    name: name.to_owned(),
                    source,
                });
            }
        }
        std::fs::metadata(&dir)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| VolumeError::Io {
                context: format!("reading {}", dir.display()),
                source,
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
                Ok(()) => {
                    // Made here: a `labels.json` there is what a create cut
                    // short left (a create holds the lock alone), not this
                    // volume's.
                    let _ = std::fs::remove_file(dir.join(LABELS_FILE));
                    // Best effort: a time that can't be kept (a full disk)
                    // fails no run; its directory's stands in.
                    let _ = self.write_created(name);
                    true
                }
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
        // Let go even if what follows fails.
        let lock = shipyard_sys::lock::HeldFile::new(self.held_shared(name)?);
        let (volume, created) = self.get_or_create(name)?;
        Ok(VolumeHold {
            volume,
            lock,
            created,
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
        // Let go even if what follows fails.
        let lock = shipyard_sys::lock::HeldFile::new(self.held_shared(name)?);
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
                Ok(()) => {
                    // Its lock files go with it, still under its lock (no
                    // fill runs: one needs it held). Whoever opened one
                    // before finds it unlinked once locked, and opens the
                    // path again (`still_its_lock`).
                    let _ = std::fs::remove_file(self.fill_lock_path(name));
                    let _ = std::fs::remove_file(self.lock_path(name));
                    return Ok(trash);
                }
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

    /// Volume `name`'s fill lock, taken (waiting up to `wait`, unless
    /// `give_up` says to stop first): one fill from an image at a time
    /// (step 11.4a2b), apart from the volume's own lock, which runs hold
    /// shared and a fill needs held. A fill that ended before it was done
    /// left its marker (`<name>/filling`), which [`Filling::unfinished`]
    /// reads.
    ///
    /// # Errors
    ///
    /// [`VolumeError::Busy`] if another fill holds it past `wait`, or
    /// `give_up` said to stop; [`VolumeError::InvalidName`] or
    /// [`VolumeError::Io`].
    pub fn lock_fill(
        &self,
        name: &str,
        wait: std::time::Duration,
        give_up: &dyn Fn() -> bool,
    ) -> Result<Filling, VolumeError> {
        validate_name(name)?;
        let path = self.fill_lock_path(name);
        let open = || open_lock(&path);
        let mut file = open()?;
        let deadline = std::time::Instant::now() + wait;
        let lock = loop {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) if Self::held_is_at(name, &lock, &path)? => break lock,
                // Removed with its volume while this opened it: the path's.
                Ok(_) => file = open()?,
                Err((unlocked, nix::errno::Errno::EWOULDBLOCK)) => {
                    if give_up() || std::time::Instant::now() >= deadline {
                        return Err(VolumeError::Busy(name.to_owned()));
                    }
                    file = unlocked;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err((_, errno)) => return Err(Self::locking(name, errno)),
            }
        };
        Ok(Filling {
            _lock: lock,
            marker: self.root.join(name).join(FILLING),
        })
    }

    /// Who left volume `name`'s fill unfinished, if anyone did: the
    /// container its marker names (read without its lock: a look only).
    #[must_use]
    pub fn unfinished_fill(&self, name: &str) -> Option<String> {
        validate_name(name).ok()?;
        read_marker(&self.root.join(name).join(FILLING))
    }

    /// The lock of volume `name`, taken exclusive: `None` if a run holds it.
    /// (A moment's wait, for another process only looking, as `in_use`
    /// does.)
    fn exclusive(&self, name: &str) -> Result<Option<Flock<File>>, VolumeError> {
        let mut file = self.lock_file(name)?;
        for _ in 0..LOCK_TRIES {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) if self.still_its_lock(name, &lock)? => return Ok(Some(lock)),
                // Removed with its volume while this opened it: the path's.
                Ok(_) => file = self.lock_file(name)?,
                Err((unlocked, nix::errno::Errno::EWOULDBLOCK)) => {
                    file = unlocked;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err((_, errno)) => return Err(Self::locking(name, errno)),
            }
        }
        Ok(None)
    }

    /// Volume `name`'s lock, held shared (a run's hold: waited for without
    /// a deadline, as a removal under way ends at once), on the file at its
    /// path.
    fn held_shared(&self, name: &str) -> Result<File, VolumeError> {
        self.held_shared_from(name, self.lock_file(name)?)
    }

    /// [`held_shared`](Self::held_shared), from `first`, the lock file as
    /// it was opened (perhaps before a removal took it away).
    fn held_shared_from(&self, name: &str, first: File) -> Result<File, VolumeError> {
        let mut lock = first;
        loop {
            lock.lock_shared()
                .map_err(|source| Self::locking(name, source))?;
            if self.still_its_lock(name, &lock)? {
                return Ok(lock);
            }
            // Removed with its volume while this waited: the new file's.
            lock = self.lock_file(name)?;
        }
    }

    /// Whether `held`, locked, is still volume `name`'s lock file.
    fn still_its_lock(&self, name: &str, held: &File) -> Result<bool, VolumeError> {
        Self::held_is_at(name, held, &self.lock_path(name))
    }

    /// Whether `held`, locked, is still the file at `path` (see the
    /// module's doc).
    fn held_is_at(name: &str, held: &File, path: &Path) -> Result<bool, VolumeError> {
        shipyard_sys::lock::still_linked(held, path).map_err(|source| Self::locking(name, source))
    }

    /// Where volume `name`'s lock file is (in `.locks/`, apart from
    /// anything a volume can be named).
    fn lock_path(&self, name: &str) -> PathBuf {
        self.root.join(LOCKS_DIR).join(name)
    }

    /// Where volume `name`'s fill lock is.
    fn fill_lock_path(&self, name: &str) -> PathBuf {
        self.root.join(FILL_LOCKS_DIR).join(name)
    }

    /// The lock file of volume `name` ([`lock_path`](Self::lock_path)),
    /// made if needed: never through a symlink, and closed in any program
    /// this one runs.
    fn lock_file(&self, name: &str) -> Result<File, VolumeError> {
        open_lock(&self.lock_path(name))
    }

    fn locking(name: &str, source: impl Into<std::io::Error>) -> VolumeError {
        VolumeError::Io {
            context: format!("locking the volume {name}"),
            source: source.into(),
        }
    }
}

/// The lock file at `path`, made if needed: never through a symlink, and
/// closed in any program this one runs.
fn open_lock(path: &Path) -> Result<File, VolumeError> {
    shipyard_sys::fs::open_lock_file(path).map_err(|source| VolumeError::Io {
        context: format!("opening {}", path.display()),
        source,
    })
}

/// The errno values `rename` gives for a target that exists.
mod libc_errno {
    pub(super) const ENOTEMPTY: i32 = nix::errno::Errno::ENOTEMPTY as i32;
    pub(super) const EEXIST: i32 = nix::errno::Errno::EEXIST as i32;
}

/// The directory of the volumes' lock files.
const LOCKS_DIR: &str = ".locks";
/// The directory of the volumes' fill locks (apart from `.locks`: a
/// volume's name may end in anything a suffix would be).
const FILL_LOCKS_DIR: &str = ".fill-locks";
/// The marker of a fill under way, in a volume's directory beside `_data`.
const FILLING: &str = "filling";
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

/// A volume's fill, its lock held ([`Volumes::lock_fill`]): dropped, the
/// next fill may go.
#[derive(Debug)]
pub struct Filling {
    _lock: Flock<File>,
    marker: PathBuf,
}

impl Filling {
    /// Who began a fill that ended before it was done, if one did: the
    /// container its marker names (what's in the volume is part of that
    /// container's fill; `Some("")` for a marker that names none).
    #[must_use]
    pub fn unfinished(&self) -> Option<String> {
        read_marker(&self.marker)
    }

    /// Marks the fill under way for container `owner`, before anything is
    /// copied (written whole, its directory synced): a fill cut short
    /// (killed, failed, the power lost) leaves the mark, naming whose it
    /// was.
    ///
    /// # Errors
    ///
    /// [`VolumeError::Io`]: it can't be written.
    pub fn begin(&self, owner: &str) -> Result<(), VolumeError> {
        // Whole or not at all, its directory synced: a power loss after
        // the copy began never loses it.
        shipyard_sys::fs::write_atomically(
            &self.marker,
            owner.as_bytes(),
            &shipyard_sys::fs::Write::new(0o600),
        )
        .map_err(|error| VolumeError::Io {
            context: format!("marking {}", self.marker.display()),
            source: error.source,
        })
    }

    /// Marks the fill done (the mark removed, its directory synced).
    ///
    /// # Errors
    ///
    /// [`VolumeError::Io`]: the mark can't be removed.
    pub fn done(self) -> Result<(), VolumeError> {
        let io = |source| VolumeError::Io {
            context: format!("removing {}", self.marker.display()),
            source,
        };
        match std::fs::remove_file(&self.marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io(source)),
        }
        match self.marker.parent() {
            Some(dir) => File::open(dir).and_then(|dir| dir.sync_all()).map_err(io),
            None => Ok(()),
        }
    }
}

/// What a fill's marker at `path` says (its owner), if it's there: read
/// with a bound, never through a link.
fn read_marker(path: &Path) -> Option<String> {
    let bytes = shipyard_sys::fs::read_bounded(path, 256).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A volume a run holds (see [`Volumes::hold`]): released when dropped
/// (or, once [given up](Self::give_up), when every copy of its
/// [`hold`](Self::hold) is closed).
#[derive(Debug)]
pub struct VolumeHold {
    volume: Volume,
    /// Its lock file, locked (`flock`, shared), to hand over.
    lock: shipyard_sys::lock::HeldFile,
    /// Whether holding it made it.
    created: bool,
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
        self.lock.hold()
    }

    /// Gives the volume up to whoever holds a copy of its
    /// [`hold`](Self::hold): this value's copy is closed, never unlocked, so
    /// the volume stays held while any copy is open.
    pub fn give_up(mut self) {
        self.lock.give_up();
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

    /// The volume store is the caller's alone (0700), one made wider before
    /// narrowed: a volume may hold an image's setuid-root file.
    #[test]
    fn the_volume_store_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = std::env::temp_dir().join(format!("volumes-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("volumes")).unwrap();
        std::fs::set_permissions(
            scratch.join("volumes"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        Volumes::open(&scratch).unwrap();
        let mode = std::fs::metadata(scratch.join("volumes"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::remove_dir_all(&scratch).unwrap();
    }
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

    /// A volume's making is its own time: what's made and removed beside
    /// its data later (a first fill's marker) never moves it, by a run's
    /// making or Docker's API's alike; one made before the time was kept
    /// reads its directory's (the previous version: none), and a newer
    /// `created.json` is refused, never guessed at.
    #[test]
    fn a_volume_s_making_stays_its_own() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let moved = |name: &str| {
            std::fs::File::open(scratch.0.join("volumes").join(name))
                .unwrap()
                .set_modified(later)
                .unwrap();
        };
        drop(volumes.hold("by-run").unwrap());
        volumes.create_with("by-api", &BTreeMap::new()).unwrap();
        for name in ["by-run", "by-api"] {
            let made = volumes.created(name).unwrap();
            moved(name);
            assert_eq!(volumes.created(name).unwrap(), made, "{name}");
            assert!(made < later, "{name}");
        }
        std::fs::remove_file(scratch.0.join("volumes").join("by-run").join(CREATED_FILE)).unwrap();
        // (The removal itself moved the directory's time: set again.)
        moved("by-run");
        assert_eq!(volumes.created("by-run").unwrap(), later, "the directory's");
        std::fs::write(
            scratch.0.join("volumes").join("by-api").join(CREATED_FILE),
            br#"{"version":2,"seconds":1,"nanos":0}"#,
        )
        .unwrap();
        assert!(matches!(
            volumes.created("by-api"),
            Err(VolumeError::Created { .. })
        ));
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

    /// A volume made with labels keeps them in its `labels.json`, read
    /// back as written; one without (made by a run, or before 11.2c2a)
    /// has none; its creation time is its making's.
    #[test]
    fn a_volume_keeps_its_labels() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let labels = BTreeMap::from([
            ("com.docker.compose.project".to_owned(), "app".to_owned()),
            ("empty".to_owned(), String::new()),
        ]);
        let before = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        let made = volumes.create_with("data", &labels).unwrap();
        assert!(made.data.is_dir());
        assert_eq!(volumes.labels("data").unwrap(), labels);
        assert_eq!(volumes.list().unwrap(), vec![made]);
        assert!(volumes.created("data").unwrap() >= before);
        let written =
            std::fs::read_to_string(volumes.root().join("data").join(LABELS_FILE)).unwrap();
        assert!(written.starts_with(r#"{"version":1,"#), "{written}");

        volumes.create("plain").unwrap();
        assert!(volumes.labels("plain").unwrap().is_empty());
        assert!(!volumes.root().join("plain").join(LABELS_FILE).exists());
        volumes.create_with("bare", &BTreeMap::new()).unwrap();
        assert!(!volumes.root().join("bare").join(LABELS_FILE).exists());
        assert!(matches!(
            volumes.create_with("data", &labels),
            Err(VolumeError::Exists(_))
        ));
    }

    /// A `labels.json` of a newer version, or not one at all, or a symlink,
    /// is refused, never guessed at.
    #[test]
    fn labels_that_cant_be_read_are_refused() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        volumes.create("data").unwrap();
        let path = volumes.root().join("data").join(LABELS_FILE);
        for bad in [
            r#"{"version":2,"labels":{}}"#,
            "not json",
            r#"{"labels":{}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                matches!(volumes.labels("data"), Err(VolumeError::Labels { .. })),
                "{bad}"
            );
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::write(
            volumes.root().join("elsewhere"),
            r#"{"version":1,"labels":{"a":"b"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(volumes.root().join("elsewhere"), &path).unwrap();
        assert!(matches!(
            volumes.labels("data"),
            Err(VolumeError::Io { .. })
        ));
    }

    /// Labels whose file would be over 64 KiB are refused, and nothing
    /// is made.
    #[test]
    fn too_many_labels_are_refused() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let labels = BTreeMap::from([("big".to_owned(), "x".repeat(70 * 1024))]);
        assert!(matches!(
            volumes.create_with("data", &labels),
            Err(VolumeError::TooManyLabels(_))
        ));
        assert!(volumes.list().unwrap().is_empty());
        assert!(!volumes.root().join("data").exists());
    }

    /// A create of a volume a run holds (shared, for its whole life) says
    /// it exists at once, never after the create's wait (11.6b's runtime
    /// review: `docker volume create` again, compose's `external`).
    #[test]
    fn a_held_volume_exists_at_once() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let _held = volumes.hold("data").unwrap();
        let began = std::time::Instant::now();
        assert!(matches!(
            volumes.create_with("data", &BTreeMap::new()),
            Err(VolumeError::Exists(_))
        ));
        assert!(began.elapsed() < CREATE_WAIT / 2, "{:?}", began.elapsed());
    }

    /// A run that makes a volume where a create was cut short (its labels
    /// written, its data not) makes it without those labels.
    #[test]
    fn a_run_s_volume_never_takes_a_cut_short_create_s_labels() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        std::fs::create_dir(volumes.root().join("data")).unwrap();
        std::fs::write(
            volumes.root().join("data").join(LABELS_FILE),
            r#"{"version":1,"labels":{"com.docker.volume.anonymous":""}}"#,
        )
        .unwrap();
        let hold = volumes.hold("data").unwrap();
        assert!(hold.created());
        assert!(volumes.labels("data").unwrap().is_empty());
    }

    /// A create that can't finish leaves nothing: no volume listed, no
    /// labels left to a volume made later by that name.
    #[test]
    fn a_create_cut_short_leaves_nothing() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        // Its data directory can't be made: a file is in its place.
        std::fs::create_dir(volumes.root().join("data")).unwrap();
        std::fs::write(volumes.root().join("data").join(DATA_DIR), "").unwrap();
        let labels = BTreeMap::from([("a".to_owned(), "b".to_owned())]);
        assert!(matches!(
            volumes.create_with("data", &labels),
            Err(VolumeError::Io { .. })
        ));
        assert!(!volumes.root().join("data").join(LABELS_FILE).exists());
        assert!(volumes.list().unwrap().is_empty());
        // A directory a cut-short create left (no data) is taken.
        std::fs::remove_file(volumes.root().join("data").join(DATA_DIR)).unwrap();
        std::fs::write(volumes.root().join("data").join(LABELS_FILE), "stale").unwrap();
        volumes.create_with("data", &BTreeMap::new()).unwrap();
        assert!(volumes.labels("data").unwrap().is_empty());
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

    /// F150: a removed volume's lock files go with it (they piled up, two
    /// a volume ever made); the control: they're there while it is.
    #[test]
    fn a_removed_volume_leaves_no_lock_files() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        let held = volumes.hold("gone").unwrap();
        drop(
            volumes
                .lock_fill("gone", std::time::Duration::ZERO, &|| false)
                .unwrap(),
        );
        drop(held);
        let locks = volumes.root().join(".locks").join("gone");
        let fill = volumes.root().join(".fill-locks").join("gone");
        assert!(locks.exists() && fill.exists(), "the control");
        volumes.trash("gone").unwrap();
        assert!(!locks.exists(), "its lock file");
        assert!(!fill.exists(), "its fill lock");
    }

    /// F150's race: a run that opened a volume's lock file before a removal
    /// took it away, and locks it after (as a blocked `hold` does), holds
    /// the new file at the path, so the volume made again under that name
    /// is held: removing it is refused. (Holding the old one, the removal
    /// would find the new file free and take the volume from the run.)
    #[test]
    fn a_hold_on_a_removed_lock_file_holds_the_new_one() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        volumes.create("again").unwrap();
        let opened_before = volumes.lock_file("again").unwrap();
        volumes.trash("again").unwrap();
        volumes.create("again").unwrap();
        let held = volumes.held_shared_from("again", opened_before).unwrap();
        assert!(
            matches!(volumes.trash("again"), Err(VolumeError::InUse(_))),
            "held, so never removed"
        );
        drop(held);
        volumes.trash("again").unwrap();
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

    /// One fill of a volume at a time: a second is `Busy` past its wait, or
    /// at once if it's asked to give up; free again once the first ends.
    /// The marker names its fill's owner until it's done; none before.
    #[test]
    fn one_fill_at_a_time_and_its_marker_names_its_owner() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        volumes.create("v").unwrap();
        let never = || false;
        let first = volumes
            .lock_fill("v", std::time::Duration::from_secs(1), &never)
            .unwrap();
        let waited = std::time::Instant::now();
        let second = volumes.lock_fill("v", std::time::Duration::from_millis(200), &never);
        assert!(matches!(second, Err(VolumeError::Busy(_))), "{second:?}");
        assert!(waited.elapsed() >= std::time::Duration::from_millis(200));
        let asked = volumes.lock_fill("v", std::time::Duration::from_secs(60), &|| true);
        assert!(
            matches!(asked, Err(VolumeError::Busy(_))),
            "given up: {asked:?}"
        );
        assert_eq!(first.unfinished(), None);
        assert_eq!(volumes.unfinished_fill("v"), None);
        first.begin("c1").unwrap();
        assert_eq!(first.unfinished().as_deref(), Some("c1"));
        assert_eq!(volumes.unfinished_fill("v").as_deref(), Some("c1"));
        first.done().unwrap();
        assert_eq!(volumes.unfinished_fill("v"), None, "done: unmarked");
        let again = volumes
            .lock_fill("v", std::time::Duration::from_secs(1), &never)
            .unwrap();
        // Begun, then dropped unfinished (a fill cut short): the mark stays.
        again.begin("c2").unwrap();
        drop(again);
        assert_eq!(volumes.unfinished_fill("v").as_deref(), Some("c2"));
    }

    /// A marker that's a link is never read through it.
    #[test]
    fn a_fill_s_marker_is_never_read_through_a_link() {
        let scratch = Scratch::new();
        let volumes = Volumes::open(&scratch.0).unwrap();
        volumes.create("v").unwrap();
        let elsewhere = scratch.0.join("elsewhere");
        std::fs::write(&elsewhere, "c9").unwrap();
        std::os::unix::fs::symlink(&elsewhere, volumes.root().join("v").join(FILLING)).unwrap();
        assert_eq!(volumes.unfinished_fill("v"), None);
    }
}
