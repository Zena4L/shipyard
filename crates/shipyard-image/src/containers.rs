//! Each running container's writable layer: `containers/<name>` in the
//! store.
//!
//! An image's layers are shared and read-only; what a container writes goes
//! to its own area, the overlay's upper layer (with overlayfs's work
//! directory, and the mount point). A foreground run's area (`run-…`) is
//! made per run and removed when the run ends. A container's own
//! (`ctr-<its ID>`, since 9.4a) is made at its first start, taken again by
//! every later start (what it wrote is still there), and removed only with
//! the container.
//!
//! **An area is alive while its directory is locked.** The process running
//! the container holds an `flock(2)` on the area's directory for the
//! container's whole life; the kernel drops it when that process ends,
//! however it ends (SIGKILL included). So whether an area is in use is a
//! try-lock away, never a guess from a PID (which the system reuses), and an
//! area whose lock is free is stale: left by a run that couldn't clean up.
//! Garbage collection keeps the layers that live areas list, and removes the
//! stale areas. A container's own area is never stale ([`Area::Owned`]):
//! free, it's a stopped container's, whose layers are still needed; only
//! its container's removal (or, for one whose container is gone, the
//! engine, which knows the containers) removes it.
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
/// The file that names the container store a container's own area belongs
/// to (its `store-id`): an engine sweeps only its own store's.
const OWNER: &str = "owner";
/// A container's own area's name: this, then its container's ID.
const OWNED_PREFIX: &str = "ctr-";
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
    /// A container's own area stayed locked: its container is running (a
    /// monitor holds it).
    #[error("the writable layer {0} is in use (its container is running)")]
    InUse(String),
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

/// A container's writable area, alive (locked) while this value is (or, once
/// [given up](Self::give_up), while a copy of its [`hold`](Self::hold) is
/// open).
#[derive(Debug)]
pub struct ContainerArea {
    name: String,
    path: PathBuf,
    /// The layer directories its overlay stacks: as made, or (a container's
    /// own, taken again) as its `layers` file says: every start mounts the
    /// same stack under the same upper layer.
    layers: Vec<String>,
    /// Its directory, locked (`flock`, exclusive): unlocked when this value
    /// goes (another thread's child, forked meanwhile, may hold a copy
    /// until it execs), unless it was given up ([`give_up`](Self::give_up)):
    /// then this copy is only closed, and the area stays alive while a
    /// copy (a container's monitor's) is open.
    lock: std::fs::File,
    given_up: bool,
}

impl Drop for ContainerArea {
    fn drop(&mut self) {
        if !self.given_up {
            let _ = self.lock.unlock();
        }
    }
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
            let lock = std::fs::File::from(dir.try_clone().map_err(failed("opening", &temporary))?);
            lock.try_lock().map_err(failed("locking", &temporary))?;
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
                layers: layers.to_vec(),
                lock,
                given_up: false,
            }),
            Err(err) => {
                // Only this process knows the temporary name: nothing else
                // removes it while it lives.
                let _ = fsat::remove_tree_at(&containers, OsStr::new(&temporary));
                Err(err)
            }
        }
    }

    /// The writable area of container `id` (64 hex digits), whose container
    /// store is `owner`: taken (locked) if it exists, its layers as its
    /// `layers` file lists them, waiting up to `wait` for the lock (a
    /// listing's probe holds it for an instant); else made, as
    /// [`create`](Self::create) makes one, of `image` and `layers`, under
    /// the name `ctr-<id>`. The caller makes sure no one else starts the
    /// same container meanwhile (the engine's operation lock): a `.new-`
    /// leftover of its own is then only a crash's, and removed.
    ///
    /// # Errors
    ///
    /// [`AreaError::NotAnArea`] for an ID that isn't one;
    /// [`AreaError::InUse`] if it stays locked; an [`AreaError::Io`] if a
    /// step fails.
    pub fn create_or_take_owned(
        store: &Store,
        held: &StoreLock,
        id: &str,
        owner: &str,
        image: &str,
        layers: &[String],
        wait: std::time::Duration,
    ) -> Result<Self, AreaError> {
        debug_assert_eq!(held.mode(), LockMode::Shared, "made while runs prepare");
        let name = owned_area_name(id);
        if !is_owned_name(&name) {
            return Err(AreaError::NotAnArea(name));
        }
        if let Some(taken) = Self::take_owned(store, id, wait)? {
            return Ok(taken);
        }
        let containers = containers(store)?;
        let path = store.root().join(CONTAINERS).join(&name);
        let temporary = format!(".new-{name}");
        // A crash's, between its making and its naming: no one else makes
        // this container's area (the caller's lock).
        let _ = fsat::remove_tree_at(&containers, OsStr::new(&temporary));
        mkdirat(
            &containers,
            temporary.as_str(),
            Mode::from_bits_truncate(0o700),
        )
        .map_err(failed("creating", &temporary))?;
        let made = (|| {
            let dir = fsat::open_dir_at(&containers, temporary.as_ref())
                .map_err(failed("opening", &temporary))?;
            let lock = std::fs::File::from(dir.try_clone().map_err(failed("opening", &temporary))?);
            lock.try_lock().map_err(failed("locking", &temporary))?;
            write_file(&dir, IMAGE, image.as_bytes())?;
            write_file(&dir, LAYERS, layers.join("\n").as_bytes())?;
            write_file(&dir, OWNER, owner.as_bytes())?;
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
                path,
                name,
                layers: layers.to_vec(),
                lock,
                given_up: false,
            }),
            Err(err) => {
                let _ = fsat::remove_tree_at(&containers, OsStr::new(&temporary));
                Err(err)
            }
        }
    }

    /// Container `id`'s own area, taken (locked, waiting up to `wait`: a
    /// listing's probe holds it for an instant), if it has one: to start
    /// on, or to remove while held (see [`trash_areas`]).
    ///
    /// # Errors
    ///
    /// [`AreaError::NotAnArea`] for an ID that isn't one;
    /// [`AreaError::InUse`] if it stays locked; an [`AreaError::Io`] if it
    /// can't be opened, locked or read.
    pub fn take_owned(
        store: &Store,
        id: &str,
        wait: std::time::Duration,
    ) -> Result<Option<Self>, AreaError> {
        let name = owned_area_name(id);
        if !is_owned_name(&name) {
            return Err(AreaError::NotAnArea(name));
        }
        let containers = containers(store)?;
        let dir = match fsat::open_dir_at(&containers, OsStr::new(&name)) {
            Ok(dir) => dir,
            Err(Errno::ENOENT) => return Ok(None),
            Err(errno) => return Err(failed("opening", &name)(errno)),
        };
        let lock = std::fs::File::from(dir.try_clone().map_err(failed("opening", &name))?);
        let deadline = std::time::Instant::now() + wait;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(AreaError::InUse(name)),
                Err(std::fs::TryLockError::Error(err)) => {
                    return Err(failed("locking", &name)(err));
                }
            }
        }
        let Some(stack) = read_list(&dir, LAYERS, &name)? else {
            return Err(failed("reading", format!("{name}/{LAYERS}"))(Errno::ENOENT));
        };
        Ok(Some(Self {
            path: store.root().join(CONTAINERS).join(&name),
            name,
            layers: stack.lines().map(str::to_owned).collect(),
            lock,
            given_up: false,
        }))
    }

    /// The layer directories its overlay stacks, lowest first.
    #[must_use]
    pub fn layers(&self) -> &[String] {
        &self.layers
    }

    /// The area's name, in `containers/`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its locked directory, for a process that keeps the area alive (a
    /// container's monitor), once this value is [given up](Self::give_up).
    #[must_use]
    pub fn hold(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.lock)
    }

    /// Gives the area up to whoever holds a copy of its
    /// [`hold`](Self::hold): this value's copy is closed, never unlocked,
    /// so the area stays alive while any copy is open.
    pub fn give_up(mut self) {
        self.given_up = true;
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

/// A container's own area (`ctr-<id>`), as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedArea {
    /// Its name, in `containers/`.
    pub name: String,
    /// Its container's ID.
    pub container: String,
    /// The container store it belongs to (its `owner` file; none if it
    /// can't be read).
    pub owner: Option<String>,
    /// Its image's manifest digest.
    pub image: String,
    /// The layer directories its overlay stacks.
    pub layers: Vec<String>,
    /// Locked: its container is running (or starting, or being removed).
    pub held: bool,
}

/// An area found in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Area {
    /// Locked: its container is running.
    Live(LiveArea),
    /// A container's own, running or not: never stale, its layers in use.
    Owned(OwnedArea),
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
        if let Some(container) = name.strip_prefix(OWNED_PREFIX) {
            let held = match Flock::lock(probe, FlockArg::LockExclusiveNonblock) {
                Ok(_) => false,
                Err((_, Errno::EWOULDBLOCK)) => true,
                Err((_, errno)) => return Err(failed("locking", &name)(errno)),
            };
            // Being made or removed, if its lists are gone.
            let (Some(image), Some(layers)) = (
                read_list(&dir, IMAGE, &name)?,
                read_list(&dir, LAYERS, &name)?,
            ) else {
                continue;
            };
            found.push(Area::Owned(OwnedArea {
                container: container.to_owned(),
                owner: read_list(&dir, OWNER, &name)?,
                image,
                layers: layers.lines().map(str::to_owned).collect(),
                held,
                name,
            }));
            continue;
        }
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

/// Whether `name` is an area's (`run-<pid>-<hex>-<hex>`, or a
/// container's own, `ctr-<64 hex>`): a single name, nothing that climbs.
fn is_area_name(name: &str) -> bool {
    is_owned_name(name)
        || name.strip_prefix("run-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        })
}

/// Whether `name` is a container's own area's: `ctr-` and a container ID.
fn is_owned_name(name: &str) -> bool {
    name.strip_prefix(OWNED_PREFIX).is_some_and(|id| {
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A container's own area's name, for container `id`.
#[must_use]
pub fn owned_area_name(id: &str) -> String {
    format!("{OWNED_PREFIX}{id}")
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

    /// An area dropped (not given up) is stale at once, even while a stray
    /// copy of its lock's descriptor is open (another thread's child,
    /// between its fork and its exec): it's unlocked, not only closed.
    #[test]
    fn a_dropped_area_is_stale_despite_a_stray_copy() {
        let scratch = Scratch::new("stray");
        let store = store(&scratch);
        let area = ContainerArea::create(&store, &shared(&store), "sha256:img", &["aa".to_owned()])
            .unwrap();
        let name = area.name().to_owned();
        let stray = area.hold().try_clone_to_owned().unwrap();
        drop(area);
        assert!(
            areas(&store).unwrap().contains(&Area::Stale { name }),
            "stale despite the copy"
        );
        drop(stray);
    }

    /// An area handed over (a copy of its hold, as a container's monitor
    /// keeps) stays live once its holder has gone, and stale once the copy
    /// is closed: dropping the holder only closes, never unlocks.
    #[test]
    fn an_area_handed_over_outlives_its_holder() {
        let scratch = Scratch::new("handed");
        let store = store(&scratch);
        let layers = vec!["aa".to_owned()];
        let area = ContainerArea::create(&store, &shared(&store), "sha256:img", &layers).unwrap();
        let name = area.name().to_owned();
        let copy = area.hold().try_clone_to_owned().unwrap();
        area.give_up();
        let live = |areas: &[Area]| {
            areas
                .iter()
                .any(|area| matches!(area, Area::Live(live) if live.name == name))
        };
        assert!(live(&areas(&store).unwrap()), "held by the copy");
        drop(copy);
        // Closing only (a handed-over hold is never unlocked): another
        // test's child, between its fork and its exec, may have a copy for
        // a moment more.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !areas(&store)
            .unwrap()
            .contains(&Area::Stale { name: name.clone() })
        {
            assert!(
                std::time::Instant::now() < deadline,
                "stale once the copy is closed"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A container's own area: made under its ID's name, taken again (its
    /// files and its layers as made, whatever is asked now), never stale
    /// (free, it's a stopped container's), listed as its container's and
    /// its store's, and held while taken.
    #[test]
    fn a_containers_own_area_is_made_then_taken_again() {
        let scratch = Scratch::new("owned");
        let store = store(&scratch);
        let id = "ab".repeat(32);
        let wait = std::time::Duration::from_millis(100);
        let layers = vec!["aa".to_owned()];
        let made = ContainerArea::create_or_take_owned(
            &store,
            &shared(&store),
            &id,
            "store-1",
            "sha256:img",
            &layers,
            wait,
        )
        .unwrap();
        assert_eq!(made.name(), format!("ctr-{id}"));
        std::fs::write(made.upper().join("kept"), "x").unwrap();
        let listed = |areas: Vec<Area>| -> Option<OwnedArea> {
            areas.into_iter().find_map(|area| match area {
                Area::Owned(owned) => Some(owned),
                _ => None,
            })
        };
        let owned = listed(areas(&store).unwrap()).expect("listed as owned");
        assert!(owned.held);
        assert_eq!(owned.container, id);
        assert_eq!(owned.owner.as_deref(), Some("store-1"));
        // Held: another taker waits, then gives up.
        assert!(matches!(
            ContainerArea::create_or_take_owned(
                &store,
                &shared(&store),
                &id,
                "store-1",
                "sha256:img",
                &layers,
                wait
            ),
            Err(AreaError::InUse(_))
        ));
        drop(made);
        let owned = listed(areas(&store).unwrap()).expect("never stale");
        assert!(!owned.held);
        let other_layers = vec!["bb".to_owned()];
        let taken = ContainerArea::create_or_take_owned(
            &store,
            &shared(&store),
            &id,
            "store-1",
            "sha256:img",
            &other_layers,
            wait,
        )
        .unwrap();
        assert_eq!(taken.layers(), layers.as_slice(), "its own stack, as made");
        assert_eq!(
            std::fs::read_to_string(taken.upper().join("kept")).unwrap(),
            "x"
        );
        assert!(matches!(
            ContainerArea::create_or_take_owned(
                &store,
                &shared(&store),
                "../x",
                "s",
                "i",
                &[],
                wait
            ),
            Err(AreaError::NotAnArea(_))
        ));
    }

    /// A crash between an owned area's making and its naming left its
    /// `.new-` directory: the next start (its container's alone) replaces
    /// it, never loops on it.
    #[test]
    fn a_crashs_half_made_owned_area_is_replaced() {
        let scratch = Scratch::new("owned-crash");
        let store = store(&scratch);
        let id = "cd".repeat(32);
        std::fs::create_dir(store.root().join(CONTAINERS).join(format!(".new-ctr-{id}"))).unwrap();
        let area = ContainerArea::create_or_take_owned(
            &store,
            &shared(&store),
            &id,
            "s",
            "sha256:img",
            &[],
            std::time::Duration::ZERO,
        )
        .unwrap();
        assert!(area.upper().is_dir());
        assert!(
            !store
                .root()
                .join(CONTAINERS)
                .join(format!(".new-ctr-{id}"))
                .exists()
        );
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
