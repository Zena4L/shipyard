//! Each running container's writable layer: `containers/<name>` in the
//! store.
//!
//! An image's layers are shared and read-only; what a container writes goes
//! to its own area, the overlay's upper layer (with overlayfs's work
//! directory, and the mount point). An area is made per run and removed
//! when the run ends.
//!
//! **An area is alive while its directory is locked.** The process running
//! the container holds an `flock(2)` on the area's directory for the
//! container's whole life; the kernel drops it when that process ends,
//! however it ends (SIGKILL included). So whether an area is in use is a
//! try-lock away, never a guess from a PID (which the system reuses), and an
//! area whose lock is free is stale: left by a run that couldn't clean up.
//! Garbage collection keeps the layers that live areas list, and removes the
//! stale areas.
//!
//! **An area appears whole.** It's made under a temporary `.new-…` name,
//! locked, filled in, and only then renamed to `run-…`: a process sweeping
//! stale areas never sees one that isn't locked yet. Removing one renames it
//! to `.trash-…` first, as for layers.
//!
//! (On NFS, Linux emulates `flock` with per-process POSIX locks, which this
//! can't rely on; overlayfs refuses an upper layer on NFS anyway.)

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};

use crate::fsat;
use crate::store::{LockMode, Store, StoreLock};

/// The directory of the areas, under the store's root.
pub const CONTAINERS: &str = "containers";

/// The file that names an area's image (its manifest digest).
const IMAGE: &str = "image";
/// The file that lists the layer directories an area's overlay stacks.
const LAYERS: &str = "layers";
/// The most an area's `image` and `layers` files are read (they list at
/// most a few hundred layer names).
const LIST_LIMIT: u64 = 1024 * 1024;

/// Why an area couldn't be made, listed or removed.
#[derive(Debug, thiserror::Error)]
pub enum AreaError {
    /// A system call failed.
    #[error("{action} {name}: {source}")]
    Io {
        /// What was being done.
        action: &'static str,
        /// The area (or directory) it was done to.
        name: String,
        /// The error.
        source: std::io::Error,
    },
    /// A name given to [`trash_areas`] isn't an area's.
    #[error("`{0}` isn't the name of a container's writable layer")]
    NotAnArea(String),
}

/// An [`AreaError::Io`] for `action` on `name`, from an errno or an I/O
/// error.
fn failed<E: Into<std::io::Error>>(
    action: &'static str,
    name: impl std::fmt::Display,
) -> impl FnOnce(E) -> AreaError {
    move |err| AreaError::Io {
        action,
        name: name.to_string(),
        source: err.into(),
    }
}

/// The directory of the areas, opened.
fn containers(store: &Store) -> Result<OwnedFd, AreaError> {
    store
        .containers_dir()
        .map_err(failed("opening", CONTAINERS))
}

/// A container's writable area, alive (locked) while this value is.
#[derive(Debug)]
pub struct ContainerArea {
    name: String,
    path: PathBuf,
    _lock: Flock<OwnedFd>,
}

impl ContainerArea {
    /// Makes a new area for a container of the image whose manifest digest
    /// is `image`, stacking the layer directories `layers` (their names in
    /// `layers/`): locked, then given its name. `held` is the store's lock,
    /// which garbage collection can't take meanwhile.
    ///
    /// # Errors
    ///
    /// An [`AreaError`] if a step fails.
    pub fn create(
        store: &Store,
        held: &StoreLock,
        image: &str,
        layers: &[String],
    ) -> Result<Self, AreaError> {
        debug_assert_eq!(held.mode(), LockMode::Shared, "made while runs prepare");
        let containers = containers(store)?;
        let (temporary, name) = new_names(&containers)?;
        let made = (|| {
            let dir = fsat::open_dir_at(&containers, temporary.as_ref())
                .map_err(failed("opening", &temporary))?;
            // Never contended: the name is new. Taken before anything can
            // see the area as `run-…`.
            let lock = Flock::lock(
                dir.try_clone().map_err(failed("opening", &temporary))?,
                FlockArg::LockExclusiveNonblock,
            )
            .map_err(|(_, errno)| failed("locking", &temporary)(errno))?;
            write_file(&dir, IMAGE, image.as_bytes())?;
            write_file(&dir, LAYERS, layers.join("\n").as_bytes())?;
            for (sub, mode) in [("upper", 0o755), ("work", 0o700), ("merged", 0o755)] {
                mkdirat(&dir, sub, Mode::from_bits_truncate(mode))
                    .map_err(failed("creating", format!("{name}/{sub}")))?;
            }
            nix::fcntl::renameat(&containers, temporary.as_str(), &containers, name.as_str())
                .map_err(failed("naming", &name))?;
            Ok(lock)
        })();
        match made {
            Ok(lock) => Ok(Self {
                path: store.root().join(CONTAINERS).join(&name),
                name,
                _lock: lock,
            }),
            Err(err) => {
                // Only this process knows the temporary name: nothing else
                // removes it while it lives.
                let _ = fsat::remove_tree_at(&containers, OsStr::new(&temporary));
                Err(err)
            }
        }
    }

    /// The area's name, in `containers/`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The overlay's upper (writable) layer.
    #[must_use]
    pub fn upper(&self) -> PathBuf {
        self.path.join("upper")
    }

    /// The overlay's work directory.
    #[must_use]
    pub fn work(&self) -> PathBuf {
        self.path.join("work")
    }

    /// Where the overlay is mounted: the container's root filesystem.
    #[must_use]
    pub fn merged(&self) -> PathBuf {
        self.path.join("merged")
    }
}

/// A fresh temporary name and the name it will have, made exclusively
/// (`mkdir` fails if the name exists): no area is ever reused.
fn new_names(containers: &OwnedFd) -> Result<(String, String), AreaError> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        // The time makes a name unlikely to have been used by an earlier
        // process with the same PID; the counter makes this loop end.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("run-{}-{nanos:x}-{nonce:x}", std::process::id());
        let temporary = format!(".new-{name}");
        if fsat::stat_at(containers, OsStr::new(&name))
            .map_err(failed("reading", &name))?
            .is_some()
        {
            continue;
        }
        match mkdirat(
            containers,
            temporary.as_str(),
            Mode::from_bits_truncate(0o700),
        ) {
            Ok(()) => return Ok((temporary, name)),
            Err(Errno::EEXIST) => {}
            Err(errno) => return Err(failed("creating", &temporary)(errno)),
        }
    }
}

/// Writes a small file in a new area.
fn write_file(dir: &OwnedFd, name: &str, contents: &[u8]) -> Result<(), AreaError> {
    let fd = openat(
        dir,
        name,
        OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(failed("creating", name))?;
    std::fs::File::from(fd)
        .write_all(contents)
        .map_err(failed("writing", name))
}

/// What a running container's area says it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveArea {
    /// Its name, in `containers/`.
    pub name: String,
    /// Its image's manifest digest.
    pub image: String,
    /// The layer directories its overlay stacks.
    pub layers: Vec<String>,
}

/// An area found in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Area {
    /// Locked: its container is running.
    Live(LiveArea),
    /// Unlocked: left by a run that ended without removing it.
    Stale {
        /// Its name.
        name: String,
    },
    /// What a removal or a creation cut short left (`.trash-…`, `.new-…`):
    /// for garbage collection, which runs when nothing is mid-way.
    Leftover {
        /// Its name.
        name: String,
    },
}

/// The areas in the store, each found live or stale by trying its lock. An
/// area being removed meanwhile (its files going) is left out.
///
/// # Errors
///
/// An [`AreaError`] if the directory can't be read, or a live area's lists
/// can't be (then nothing can be collected safely).
pub fn areas(store: &Store) -> Result<Vec<Area>, AreaError> {
    let containers = containers(store)?;
    let mut found = Vec::new();
    for name in fsat::list(&containers).map_err(failed("reading", CONTAINERS))? {
        let Some(name) = name.to_str().map(str::to_owned) else {
            continue;
        };
        if fsat::is_leftover(&name) {
            found.push(Area::Leftover { name });
            continue;
        }
        if !is_area_name(&name) {
            continue;
        }
        let dir = match fsat::open_dir_at(&containers, OsStr::new(&name)) {
            Ok(dir) => dir,
            // Removed meanwhile (its run ended), or not a directory.
            Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => continue,
            Err(errno) => return Err(failed("opening", &name)(errno)),
        };
        let probe = dir.try_clone().map_err(failed("opening", &name))?;
        match Flock::lock(probe, FlockArg::LockExclusiveNonblock) {
            // Free: nobody runs it. (The lock goes again at once.)
            Ok(_) => found.push(Area::Stale { name }),
            Err((_, Errno::EWOULDBLOCK)) => {
                // Its run is removing it, if its lists are gone.
                let (Some(image), Some(layers)) = (
                    read_list(&dir, IMAGE, &name)?,
                    read_list(&dir, LAYERS, &name)?,
                ) else {
                    continue;
                };
                found.push(Area::Live(LiveArea {
                    name,
                    image,
                    layers: layers.lines().map(str::to_owned).collect(),
                }));
            }
            Err((_, errno)) => return Err(failed("locking", &name)(errno)),
        }
    }
    Ok(found)
}

/// Reads one of an area's small files; `None` if it's gone.
fn read_list(dir: &OwnedFd, file: &str, area: &str) -> Result<Option<String>, AreaError> {
    let fd = match openat(
        dir,
        file,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(None),
        Err(errno) => return Err(failed("reading", format!("{area}/{file}"))(errno)),
    };
    let mut text = String::new();
    std::fs::File::from(fd)
        .take(LIST_LIMIT)
        .read_to_string(&mut text)
        .map_err(failed("reading", format!("{area}/{file}")))?;
    Ok(Some(text))
}

/// Moves areas aside to trash, by name (`run-…`), and returns the trash
/// names, for [`remove_areas`] to delete; leftovers are returned as they
/// are. A name that isn't one is refused, so nothing else in the store is
/// reached through here.
///
/// # Errors
///
/// [`AreaError::NotAnArea`] for a bad name; [`AreaError::Io`] if one can't
/// be renamed.
pub fn trash_areas(store: &Store, names: &[String]) -> Result<Vec<String>, AreaError> {
    let containers = containers(store)?;
    let mut trash = Vec::with_capacity(names.len());
    for name in names {
        if fsat::is_leftover(name) {
            trash.push(name.clone());
        } else if !is_area_name(name) {
            return Err(AreaError::NotAnArea(name.clone()));
        } else if let Some(moved) =
            fsat::move_to_trash(&containers, name).map_err(failed("moving aside", name))?
        {
            trash.push(moved.to_string_lossy().into_owned());
        }
    }
    Ok(trash)
}

/// Removes areas by name: what [`trash_areas`] moved aside (deleted as it
/// is), or an area (moved aside first, then deleted).
///
/// # Errors
///
/// As [`trash_areas`]; and [`AreaError::Io`] if one can't be deleted
/// (files a rootless container wrote as its other users need the user
/// namespace they belong to). What was moved aside stays trash.
pub fn remove_areas(store: &Store, names: &[String]) -> Result<(), AreaError> {
    let containers = containers(store)?;
    for name in trash_areas(store, names)? {
        fsat::remove_tree_at(&containers, OsStr::new(&name)).map_err(failed("removing", &name))?;
    }
    Ok(())
}

/// Whether `name` is an area's (`run-<pid>-<hex>-<hex>`): a single name,
/// nothing that climbs.
fn is_area_name(name: &str) -> bool {
    name.strip_prefix("run-").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use super::*;
    use crate::unpack::tests::Scratch;

    fn store(scratch: &Scratch) -> Store {
        Store::open(scratch.0.join("store")).unwrap()
    }

    fn shared(store: &Store) -> StoreLock {
        store
            .lock(LockMode::Shared, std::time::Duration::ZERO)
            .unwrap()
    }

    #[test]
    fn an_area_is_live_while_held_and_stale_after() {
        let scratch = Scratch::new("areas");
        let store = store(&scratch);
        let layers = vec!["aa".to_owned(), "bb-caller".to_owned()];
        let first = ContainerArea::create(&store, &shared(&store), "sha256:img", &layers).unwrap();
        let second = ContainerArea::create(&store, &shared(&store), "sha256:img", &layers).unwrap();
        assert_ne!(first.name(), second.name(), "never reused");
        assert!(first.upper().is_dir() && first.work().is_dir() && first.merged().is_dir());

        let live = areas(&store).unwrap();
        assert!(live.contains(&Area::Live(LiveArea {
            name: first.name().to_owned(),
            image: "sha256:img".to_owned(),
            layers: layers.clone(),
        })));
        assert!(first.name().starts_with("run-"));
        let name = second.name().to_owned();
        drop(second);
        assert!(areas(&store).unwrap().contains(&Area::Stale { name }));
    }

    #[test]
    fn areas_are_removed_by_name_only() {
        let scratch = Scratch::new("remove-areas");
        let store = store(&scratch);
        let area = ContainerArea::create(&store, &shared(&store), "sha256:img", &[]).unwrap();
        let name = area.name().to_owned();
        drop(area);
        for bad in ["..", "layers", "run-../x", "run-", "../run-1"] {
            assert!(matches!(
                remove_areas(&store, &[bad.to_owned()]),
                Err(AreaError::NotAnArea(_))
            ));
        }
        // Moved aside first: the trash is what's deleted.
        let trash = trash_areas(&store, &[name]).unwrap();
        assert!(trash[0].starts_with(".trash-run-"), "{trash:?}");
        assert!(matches!(
            areas(&store).unwrap()[..],
            [Area::Leftover { .. }]
        ));
        remove_areas(&store, &trash).unwrap();
        assert!(areas(&store).unwrap().is_empty());
    }

    #[test]
    fn names_that_arent_areas_are_left_alone() {
        let scratch = Scratch::new("stray");
        let store = store(&scratch);
        std::fs::create_dir(store.root().join(CONTAINERS).join("run-not-hex")).unwrap();
        std::fs::create_dir(store.root().join(CONTAINERS).join("mine")).unwrap();
        assert!(areas(&store).unwrap().is_empty());
    }

    #[test]
    fn a_half_made_area_is_a_leftover_never_stale() {
        let scratch = Scratch::new("half");
        let store = store(&scratch);
        std::fs::create_dir(store.root().join(CONTAINERS).join(".new-run-1-0")).unwrap();
        assert_eq!(
            areas(&store).unwrap(),
            [Area::Leftover {
                name: ".new-run-1-0".to_owned()
            }]
        );
    }
}
