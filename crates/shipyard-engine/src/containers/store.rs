//! The store's writers: a container made, renamed, changed, removed, each
//! under the store's lock, and the store's identity.

use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::time::Duration;

use super::id;
use super::{Config, ContainerId, Labels, Name, Record, Serial};
use super::{DRAWS, GONE, LOCK, LOCK_WAIT, LOGS, NEW, RECORD, STORE_ID};
use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::report::WarningKind;

/// A container to make.
#[derive(Debug, Clone)]
pub struct NewContainer {
    /// Its name; one is made up if none is given.
    pub name: Option<Name>,
    /// How to run it.
    pub config: Config,
    /// Its labels.
    pub labels: Labels,
    /// The image it's made from, by its manifest's digest (none without an
    /// image; [`crate::run::create`](fn@crate::run::create) records it once the image is pinned,
    /// which takes the container's ID).
    pub image_id: Option<String>,
    /// Removed once it ends (`--rm`).
    pub auto_remove: bool,
    /// What its create settled beside its record (step 11.3b1).
    pub created: super::Created,
}

/// Makes container `new`'s record: a new ID and serial, and its name
/// (taken, or made up). Nothing runs. The record alone: a container (its
/// image ready and pinned too) is made with [`crate::run::create`](fn@crate::run::create).
///
/// # Errors
///
/// - [`ErrorKind::Exists`]: its name is taken.
/// - [`ErrorKind::Busy`]: no free serial was found, or the store's lock was
///   held too long.
/// - [`ErrorKind::InvalidRequest`]: the record would be too big.
/// - [`ErrorKind::Storage`]: the store can't be used, or a record in it
///   can't be read (its name and serial may be the ones drawn).
/// - [`ErrorKind::Environment`]: there's nowhere to keep records.
/// - [`ErrorKind::Io`]: the kernel's random numbers couldn't be read.
pub fn create_record(engine: &Engine, new: NewContainer) -> Result<Record> {
    create_record_avoiding(engine, new, &[])
}

/// [`create_record`], a made-up name drawn so that it's none of `avoid`
/// either (names held elsewhere: on the container's networks, F63),
/// whatever their case.
///
/// # Errors
///
/// As [`create_record`].
pub(crate) fn create_record_avoiding(
    engine: &Engine,
    new: NewContainer,
    avoid: &[String],
) -> Result<Record> {
    let mut words = || {
        let bytes = crate::random::bytes::<4>()?;
        Ok([
            u16::from_le_bytes([bytes[0], bytes[1]]),
            u16::from_le_bytes([bytes[2], bytes[3]]),
        ])
    };
    create_drawing(engine, new, avoid, &mut Serial::random, &mut words)
}

/// [`create_record`], its serials drawn by `serials` and its made-up names' words
/// by `words` (tests force collisions).
pub(super) fn create_drawing(
    engine: &Engine,
    new: NewContainer,
    avoid: &[String],
    serials: &mut dyn FnMut() -> Result<Serial>,
    words: &mut dyn FnMut() -> Result<[u16; 2]>,
) -> Result<Record> {
    let store = Store::made(engine)?;
    let _lock = store.lock(LOCK_WAIT)?;
    store.sweep();
    let existing = store.read_all(engine, Strict::Yes)?;
    let id = loop {
        let id = ContainerId::new()?;
        if !store.dir.join(id.as_str()).exists() {
            break id;
        }
    };
    let serial = free_serial(&existing, serials)?;
    let name = match new.name {
        Some(name) => {
            refuse_taken_name(&existing, &name, None)?;
            name
        }
        None => free_name(&existing, avoid, words)?,
    };
    let mut record = Record::new(id, serial, name, new.config, new.labels);
    record.image_id = new.image_id;
    record.auto_remove = new.auto_remove;
    store.publish(engine, &record, &new.created)?;
    Ok(record)
}

/// `name` refused if a container in `existing` other than `except` has it.
fn refuse_taken_name(existing: &[Record], name: &Name, except: Option<&ContainerId>) -> Result<()> {
    if let Some(other) = existing
        .iter()
        .find(|record| record.name == *name && Some(&record.id) != except)
    {
        bail!(
            Exists,
            "the name {name} is taken, by container {}",
            other.id.short()
        );
    }
    Ok(())
}

/// Whether container `id` could be named `name`: no other container has
/// it (checked as [`create_record`] does, under the store's lock).
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: there's no container store.
/// - [`ErrorKind::Exists`]: another container has it.
/// - [`ErrorKind::Busy`]: the store's lock is held too long.
/// - [`ErrorKind::Storage`]: the store can't be read.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "a rename is checked on Linux, where it renames a running one too"
    )
)]
pub(crate) fn check_name(engine: &Engine, id: &ContainerId, name: &Name) -> Result<()> {
    let Some(store) = Store::open(engine, false)? else {
        bail!(NotFound, "no container {}", id.short());
    };
    let _lock = store.lock(LOCK_WAIT)?;
    refuse_taken_name(&store.read_all(engine, Strict::Yes)?, name, Some(id))
}

/// Renames container `id` `name` in its record: under the store's lock,
/// the name checked against every other container's (as [`create_record`]
/// does), then the record rewritten (atomically). Its name on its networks
/// is the caller's (`run::rename`).
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: no such container.
/// - [`ErrorKind::Exists`]: another container has the name.
/// - [`ErrorKind::Busy`]: the store's lock is held too long.
/// - [`ErrorKind::Storage`]: the store can't be read or written.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "`run::rename` is Linux's")
)]
pub(crate) fn rename(engine: &Engine, id: &ContainerId, name: &Name) -> Result<Record> {
    let Some(store) = Store::open(engine, false)? else {
        bail!(NotFound, "no container {}", id.short());
    };
    let _lock = store.lock(LOCK_WAIT)?;
    let existing = store.read_all(engine, Strict::Yes)?;
    refuse_taken_name(&existing, name, Some(id))?;
    rewrite(engine, &store, id, |record| record.name = name.clone())
}

/// A serial no record in `existing` has.
fn free_serial(existing: &[Record], draw: &mut dyn FnMut() -> Result<Serial>) -> Result<Serial> {
    for _ in 0..DRAWS {
        let serial = draw()?;
        if !existing.iter().any(|record| record.serial == serial) {
            return Ok(serial);
        }
    }
    bail!(
        Busy,
        "no free container serial was found in {DRAWS} tries ({} containers)",
        existing.len()
    )
}

/// A made-up name no record in `existing` has, and none of `avoid` (any
/// case: as a network compares names): drawn from `words`, then, once
/// [`DRAWS`] were all taken, numbered (`_2`, `_3`, …) until one isn't.
fn free_name(
    existing: &[Record],
    avoid: &[String],
    words: &mut dyn FnMut() -> Result<[u16; 2]>,
) -> Result<Name> {
    let taken = |name: &Name| {
        existing.iter().any(|record| record.name == *name)
            || avoid
                .iter()
                .any(|other| other.eq_ignore_ascii_case(name.as_str()))
    };
    let mut draws = 0;
    let mut suffix: Option<u32> = None;
    loop {
        let name = Name::generated(words()?, suffix);
        if !taken(&name) {
            return Ok(name);
        }
        draws += 1;
        if draws >= DRAWS {
            // Each number is tried with a new draw: at most as many tries
            // as there are containers, plus the draws.
            suffix = Some(suffix.map_or(2, |n| n.saturating_add(1)));
        }
    }
}

/// Removes container `id`'s record (nothing else: what it ran, the runtime
/// removes; its writable layer and its image's pin, [`crate::run::remove`](fn@crate::run::remove),
/// which removes a whole container). Done once its directory is renamed out of place: what can't
/// be removed after that is reported ([`WarningKind::LeftForLater`]) and
/// left to the next writer's sweep.
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: there's no such container.
/// - [`ErrorKind::Busy`]: the store's lock was held too long.
/// - [`ErrorKind::Storage`]: it couldn't be removed.
/// - [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn remove_record(engine: &Engine, id: &ContainerId) -> Result<()> {
    let Some(store) = Store::open(engine, false)? else {
        bail!(NotFound, "no container {}", id.short());
    };
    let _lock = store.lock(LOCK_WAIT)?;
    let dir = store.dir.join(id.as_str());
    if !dir.is_dir() {
        bail!(NotFound, "no container {}", id.short());
    }
    let gone = store.dir.join(format!("{GONE}{id}"));
    fs::rename(&dir, &gone)
        .with_context(|| format!("removing {}", dir.display()))
        .map_err(|err| err.io_as(ErrorKind::Storage))?;
    store.synced(engine);
    if let Err(err) = fs::remove_dir_all(&gone) {
        engine.warn(
            WarningKind::LeftForLater,
            format!(
                "container {}'s files are left in {} ({err}): the next change to the store \
                 removes them",
                id.short(),
                gone.display()
            ),
        );
    }
    Ok(())
}

/// Changes container `id`'s record as `change` says, under the store's
/// lock: the record read again (another writer may have changed it), then
/// its `record.json` replaced in place (a temporary file renamed over it,
/// both synced). Never by renaming its directory: its logs are in it, and
/// a running container's monitor writes them.
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: there's no such container.
/// - [`ErrorKind::Busy`]: the store's lock was held too long.
/// - [`ErrorKind::Storage`]: it couldn't be read or written.
/// - [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn update(
    engine: &Engine,
    id: &ContainerId,
    change: impl FnOnce(&mut Record),
) -> Result<Record> {
    let Some(store) = Store::open(engine, false)? else {
        bail!(NotFound, "no container {}", id.short());
    };
    let _lock = store.lock(LOCK_WAIT)?;
    rewrite(engine, &store, id, change)
}

/// Container `id`'s record re-read, changed by `change` and written again
/// (atomically), its store's lock held by the caller.
fn rewrite(
    engine: &Engine,
    store: &Store,
    id: &ContainerId,
    change: impl FnOnce(&mut Record),
) -> Result<Record> {
    let dir = store.dir.join(id.as_str());
    if !dir.is_dir() {
        bail!(NotFound, "no container {}", id.short());
    }
    // The version it was read at is the one it's written at again (the
    // upgrade rule, F43).
    let (mut record, read_at) = Record::read_versioned(&dir.join(RECORD))?;
    if record.id != *id {
        bail!(
            Storage,
            "container {}'s record is container {}'s",
            id.short(),
            record.id.short()
        );
    }
    change(&mut record);
    let json = record.to_json_at(read_at)?;
    crate::fs::write_atomically(&dir, RECORD, &json, 0o600)
        .map_err(|err| err.io_as(ErrorKind::Storage))?
        .said(engine);
    Ok(record)
}

/// Container `id`'s directory in the store (its record, its logs), if the
/// store is somewhere.
///
/// # Errors
///
/// [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn record_dir(engine: &Engine, id: &ContainerId) -> Result<PathBuf> {
    Ok(engine.paths.containers()?.join(id.as_str()))
}

/// The directory of container `id`'s logs: one directory in it for each
/// time it started, numbered in order from 1 (zero-padded, so they sort by
/// name: the last is the latest, whatever the clock did meanwhile), as the
/// runtime makes a log directory afresh for each container it creates.
///
/// # Errors
///
/// As [`record_dir`].
pub fn logs_dir(engine: &Engine, id: &ContainerId) -> Result<PathBuf> {
    Ok(record_dir(engine, id)?.join(LOGS))
}

/// Runs `act` on every record, read strictly, while the store's lock is
/// held: what it decides from them (a volume no container names, say) is
/// still so as it acts, since a writer (`create_record`) takes the same
/// lock (F56). No store here yet is no records; a store whose place isn't
/// known (a relative `SHIPYARD_CONTAINER_DIR`, no home) isn't: its records
/// might name what `act` would remove, so `what` is kept. The lock's and
/// the read's own failures say `what` (a volume, every volume) is kept,
/// and why; `act`'s pass as they are.
///
/// # Errors
///
/// The store can't be locked or a record read (one that can't be read
/// might name what `act` would remove), or `act`'s.
pub(crate) fn with_records_locked<T>(
    engine: &Engine,
    what: &str,
    act: impl FnOnce(&[Record]) -> Result<T>,
) -> Result<T> {
    let kept = |why: &'static str| {
        move |error: crate::Error| error.context(format!("{what} is kept: {why}"))
    };
    let store = match Store::open(engine, false) {
        Ok(Some(store)) => store,
        Ok(None) => return act(&[]),
        Err(error) if error.kind() == ErrorKind::Environment => {
            return Err(kept("the container store's place isn't known")(error));
        }
        Err(error) => return Err(kept("the container store can't be opened")(error)),
    };
    let _lock = store
        .lock(super::LOCK_WAIT)
        .map_err(kept("the container store is busy"))?;
    let records = store
        .read_all(engine, Strict::Yes)
        .map_err(kept("the containers' records can't all be read"))?;
    act(&records)
}

/// Whether a reader reads every record or skips the ones it can't.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Strict {
    /// A writer's: one it can't read is an error.
    Yes,
    /// A reader's: one it can't read is reported and left out.
    No,
}

/// The store, open.
pub(super) struct Store {
    dir: PathBuf,
}

impl Store {
    /// The caller's store, made if it's missing and `make` (else `None`),
    /// and checked as [`made`](Self::made) checks it.
    pub(super) fn open(engine: &Engine, make: bool) -> Result<Option<Self>> {
        let dir = engine.paths.containers()?;
        if !make
            && fs::symlink_metadata(dir)
                .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(None);
        }
        Self::made(engine).map(Some)
    }

    /// The caller's store, made if it's missing, and checked: a real
    /// directory (not a symlink to one), the caller's, that nobody else can
    /// write in.
    fn made(engine: &Engine) -> Result<Self> {
        let dir = engine.paths.containers()?.to_path_buf();
        let missing =
            fs::symlink_metadata(&dir).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound);
        if missing {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("making {}", dir.display()))
                .map_err(|err| err.io_as(ErrorKind::Storage))?;
        }
        crate::fs::check_private_dir(&dir, "the container store")
            .map_err(|err| err.io_as(ErrorKind::Storage))?;
        Ok(Self { dir })
    }

    /// The store's lock, held until dropped, waited for up to `wait`.
    pub(super) fn lock(&self, wait: Duration) -> Result<nix::fcntl::Flock<fs::File>> {
        crate::fs::lock_waiting(&self.dir.join(LOCK), wait, "the container store")
            .map_err(|err| err.io_as(ErrorKind::Storage))
    }

    /// Syncs the store's directory, a rename in it done: a failure is
    /// reported, not returned (the change is made, only maybe not on the
    /// disk yet).
    fn synced(&self, engine: &Engine) {
        if let Err(err) = crate::fs::sync_directory(&self.dir) {
            engine.warn(WarningKind::NotSynced, format!("{err:#}"));
        }
    }

    /// Removes what writers killed half way left. Under the lock.
    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(NEW) || name.starts_with(GONE) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Every record, oldest first. One that can't be read fails a strict
    /// reader, and is reported and left out otherwise. No lock: records
    /// appear and go whole.
    pub(super) fn read_all(&self, engine: &Engine, strict: Strict) -> Result<Vec<Record>> {
        let reading = || format!("reading {}", self.dir.display());
        let mut records = Vec::new();
        let entries = fs::read_dir(&self.dir)
            .with_context(reading)
            .map_err(|err| err.io_as(ErrorKind::Storage))?;
        for entry in entries {
            let entry = entry
                .with_context(reading)
                .map_err(|err| err.io_as(ErrorKind::Storage))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !id::is_id(name) {
                // The lock, a container half made or half removed, and
                // whatever else isn't a container's.
                continue;
            }
            let problem = match Record::read(&entry.path().join(RECORD)) {
                Ok(record) if record.id.as_str() == name => {
                    records.push(record);
                    continue;
                }
                Ok(record) => Error::new(
                    ErrorKind::Storage,
                    format!("container {name}'s record is container {}'s", record.id),
                ),
                // Removed between the listing and the read: gone, not broken.
                Err(_) if !entry.path().exists() => continue,
                Err(err) => err,
            };
            match strict {
                Strict::Yes => {
                    return Err(problem.context(format!(
                        "container {name}'s record can't be read, and its name and serial may \
                         be the ones a new container would get (remove it with `rm -f {name}`, \
                         or use the Shipyard that wrote it)"
                    )));
                }
                Strict::No => engine.warn(
                    WarningKind::RecordUnreadable,
                    format!("container {name} is left out: {problem:#}"),
                ),
            }
        }
        records.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        Ok(records)
    }

    /// Puts `record` in the store whole: its directory made under a
    /// temporary name, its record written and synced, then renamed into
    /// place, and the store synced (a failure there reported: the record is
    /// in place), what its create settled beside it. Under the lock.
    fn publish(&self, engine: &Engine, record: &Record, created: &super::Created) -> Result<()> {
        let json = record.to_json_new()?;
        let staging = self.dir.join(format!("{NEW}{}", record.id));
        let dir = self.dir.join(record.id.as_str());
        let placed = (|| {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&staging)
                .with_context(|| format!("making {}", staging.display()))?;
            // A new container's directory must hold its record on the disk
            // before it's renamed into place: unsynced is a failure here.
            if let Some(unsynced) =
                crate::fs::write_atomically(&staging, RECORD, &json, 0o600)?.unsynced()
            {
                return Err(unsynced);
            }
            super::write_created(&staging, created)?;
            fs::rename(&staging, &dir).with_context(|| format!("making {}", dir.display()))
        })();
        if placed.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        placed.map_err(|err| err.io_as(ErrorKind::Storage))?;
        self.synced(engine);
        Ok(())
    }
}

/// The most of [`STORE_ID`] read: 32 characters and a line's end; more
/// isn't an identity.
const STORE_ID_MAX: u64 = 4 * 1024;

/// This container store's identity ([`STORE_ID`]): read, or (`make`)
/// made with the store if it has none yet. `None` without a store (or an
/// identity, not making one).
///
/// # Errors
///
/// [`ErrorKind::Storage`]: the store or its identity can't be read or
/// made; [`ErrorKind::Environment`]: there's nowhere to keep records.
pub(crate) fn store_id(engine: &Engine, make: bool) -> Result<Option<String>> {
    let Some(store) = Store::open(engine, make)? else {
        return Ok(None);
    };
    let read = || {
        shipyard_sys::fs::read_bounded(&store.dir.join(STORE_ID), STORE_ID_MAX)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|text| text.trim().to_owned())
            .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    if let Some(id) = read() {
        return Ok(Some(id));
    }
    if !make {
        return Ok(None);
    }
    let _lock = store.lock(LOCK_WAIT)?;
    if let Some(id) = read() {
        return Ok(Some(id));
    }
    let id = crate::random::hex(&crate::random::bytes::<16>()?);
    crate::fs::write_atomically(&store.dir, STORE_ID, id.as_bytes(), 0o600)
        .map_err(|err| err.io_as(ErrorKind::Storage))?
        .said(engine);
    Ok(Some(id))
}

/// This container store's identity, made with the store if need be: for a
/// caller that needs one.
///
/// # Errors
///
/// [`store_id`]'s; [`ErrorKind::Storage`] if the store has gone.
#[cfg(target_os = "linux")]
pub(crate) fn require_store_id(engine: &Engine) -> Result<String> {
    store_id(engine, true)?
        .ok_or_else(|| Error::new(ErrorKind::Storage, "the container store has gone"))
}
