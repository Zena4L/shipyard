//! Containers' records: the engine's own, persistent account of each
//! container (who it is, how to run it, how it last ran), beside the
//! runtime's state, which lasts only while the container does.
//!
//! **On disk** ([`Paths::containers`](crate::Paths::containers)): one
//! directory per container, named by its ID, holding `record.json`
//! ([`Record`], versioned), its starts' `logs/<N>`, `cleanup.log` (what its
//! last `__cleanup` couldn't do), `previous-start.json` while a start is in
//! flight (what the start before it left), and three lock files: `op.lock`, which
//! `start` and `rm` take in turn; `running`, held by a start's monitor and
//! its `__cleanup` until all of that start is let go; and `restarting`,
//! held by the `__cleanup` that waits to start the container again. The
//! store's own `store-id` names it in the image store's areas. The store is
//! the caller's alone (checked), and a record is only theirs to read
//! (0600: it holds the container's environment).
//!
//! **The locks' order:** a container's `op.lock`, then its `running`, then
//! the image store's lock, then this store's (briefly). `__cleanup` takes
//! neither of a container's: nothing waits on it holding what it needs.
//! `restarting` is outside the order: its holder only tries the others, and
//! a holder of the others only tries it, never waiting.
//!
//! **Writers take turns, readers never wait.** Making and removing a
//! container hold the store's lock (`.lock`, `flock`): names and serials
//! stay unique. A container appears whole (its directory made under a
//! temporary name, then renamed into place) and goes whole (renamed out,
//! then removed), so a reader ([`list`], [`find`]) that takes no lock sees
//! a container or nothing, never part of one. What a writer killed half
//! way leaves (`.new-…`, `.gone-…`) is swept by the next.
//!
//! **A writer reads strictly, a reader leniently.** [`list`] leaves out a
//! record it can't read (a newer Shipyard's, a damaged one), and says so;
//! [`create_record`] refuses to go on, since that record's name and serial
//! are still its container's.
//!
//! Known limits: the store's own directory is checked, not its parents'
//! (as the runtime's state root); a serial or a name is free again once
//! its container is removed, as Docker's names are.

mod config;
mod id;
mod labels;
mod names;
mod record;
mod state;
mod view;

use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::time::Duration;

pub use config::{
    Amount, Bridge, CgroupLimits, Config, Device, DeviceCgroupRule, Host, IdRange, IoLimit,
    Network, Port, RestartPolicy, Security, Sysctl, Ulimit, User,
};
pub use id::{ContainerId, Serial};
pub use labels::Labels;
pub use names::Name;
pub use record::{BoundPort, Exit, Record};
pub use state::ContainerState;
pub use view::{
    Inspected, InspectedState, Listed, Ports, Sock, Sockets, Summary, inspect, ports, sockets,
    summaries,
};

use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::report::WarningKind;

/// A record's file in its container's directory.
const RECORD: &str = "record.json";

/// The directory of a container's logs, in its own (see [`logs_dir`]).
const LOGS: &str = "logs";
/// The store's lock, taken by writers.
const LOCK: &str = ".lock";
/// A container being made: its directory before it's renamed into place.
const NEW: &str = ".new-";
/// A container being removed: its directory once renamed out of place.
const GONE: &str = ".gone-";
/// A container's operation lock, in its directory: taken by `start` and
/// `rm` (9.4a), so two of them on one container never interleave.
#[cfg(target_os = "linux")]
const OP_LOCK: &str = "op.lock";
/// A container's liveness, in its directory: locked by a start and handed
/// to its monitor, then to `__cleanup`, which closes it last (9.4a). Free,
/// everything of the last start has been let go.
#[cfg(target_os = "linux")]
const RUNNING: &str = "running";
/// The lock file held while a container's `__cleanup` decides whether to
/// restart it, and while it waits to (stage 9, step 9.5b): free, no restart
/// waits.
#[cfg(target_os = "linux")]
const RESTARTING: &str = "restarting";
/// What a container's last start left (its start, end, code and boot),
/// kept beside its record while a new start is in flight (stage 9, step
/// 9.9c, R2): a file of its own, never a record field (an older binary
/// refuses a record with a field it doesn't know).
#[cfg(target_os = "linux")]
const PREVIOUS_START: &str = "previous-start.json";
/// The store's identity (random, made once), written into the writable
/// layers its containers own in an image store: an engine sweeps only its
/// own store's (two container stores may share an image store).
const STORE_ID: &str = "store-id";

/// How long a writer waits for the store's lock: far longer than any
/// writer holds it.
const LOCK_WAIT: Duration = Duration::from_secs(15);

/// How many serials and generated names are drawn before giving up on
/// finding a free one (the serials), or numbering one (the names).
const DRAWS: usize = 64;

/// How many matches an ambiguous reference's error names.
const MATCHES_SHOWN: usize = 10;

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
    /// image; [`crate::run::create`] records it once the image is pinned,
    /// which takes the container's ID).
    pub image_id: Option<String>,
    /// Removed once it ends (`--rm`).
    pub auto_remove: bool,
}

/// Makes container `new`'s record: a new ID and serial, and its name
/// (taken, or made up). Nothing runs. The record alone: a container (its
/// image ready and pinned too) is made with [`crate::run::create`].
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
    let mut words = || {
        let bytes = crate::random::bytes::<4>()?;
        Ok([
            u16::from_le_bytes([bytes[0], bytes[1]]),
            u16::from_le_bytes([bytes[2], bytes[3]]),
        ])
    };
    create_drawing(engine, new, &mut Serial::random, &mut words)
}

/// [`create_record`], its serials drawn by `serials` and its made-up names' words
/// by `words` (tests force collisions).
fn create_drawing(
    engine: &Engine,
    new: NewContainer,
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
        None => free_name(&existing, words)?,
    };
    let mut record = Record::new(id, serial, name, new.config, new.labels);
    record.image_id = new.image_id;
    record.auto_remove = new.auto_remove;
    store.publish(engine, &record)?;
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
pub fn check_name(engine: &Engine, id: &ContainerId, name: &Name) -> Result<()> {
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
pub fn rename(engine: &Engine, id: &ContainerId, name: &Name) -> Result<Record> {
    let Some(store) = Store::open(engine, false)? else {
        bail!(NotFound, "no container {}", id.short());
    };
    let _lock = store.lock(LOCK_WAIT)?;
    let existing = store.read_all(engine, Strict::Yes)?;
    refuse_taken_name(&existing, name, Some(id))?;
    rewrite(&store, id, |record| record.name = name.clone())
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

/// A made-up name no record in `existing` has: drawn from `words`, then,
/// once [`DRAWS`] were all taken, numbered (`_2`, `_3`, …) until one isn't.
fn free_name(existing: &[Record], words: &mut dyn FnMut() -> Result<[u16; 2]>) -> Result<Name> {
    let taken = |name: &Name| existing.iter().any(|record| record.name == *name);
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

/// Every container's record, oldest first. One that can't be read is
/// reported ([`WarningKind::RecordUnreadable`]) and left out. No store yet
/// is none (it isn't made).
///
/// # Errors
///
/// [`ErrorKind::Storage`]: the store can't be used.
/// [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn list(engine: &Engine) -> Result<Vec<Record>> {
    match Store::open(engine, false)? {
        Some(store) => store.read_all(engine, Strict::No),
        None => Ok(Vec::new()),
    }
}

/// The container `reference` names: in order, its full ID, its name, its
/// serial written with its owner code (`SHPU 304812 2`, the check digit
/// optional), or a prefix of its ID that no other container's has.
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: there's no such container.
/// - [`ErrorKind::InvalidRequest`]: more than one (a prefix of several IDs,
///   or a name or serial two records have: some are named), or a serial
///   with the wrong check digit.
/// - [`ErrorKind::Storage`]: the store can't be used.
/// - [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn find(engine: &Engine, reference: &str) -> Result<Record> {
    let records = list(engine)?;
    let by_id: Vec<&Record> = records
        .iter()
        .filter(|record| record.id.as_str() == reference)
        .collect();
    if !by_id.is_empty() {
        return one(reference, &by_id);
    }
    let by_name: Vec<&Record> = records
        .iter()
        .filter(|record| record.name.as_str() == reference)
        .collect();
    if !by_name.is_empty() {
        return one(reference, &by_name);
    }
    if let Some(serial) = Serial::parse(reference)? {
        let by_serial: Vec<&Record> = records
            .iter()
            .filter(|record| record.serial == serial)
            .collect();
        return one(reference, &by_serial);
    }
    if reference.is_empty() || !reference.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(not_found(reference));
    }
    let prefix = reference.to_ascii_lowercase();
    let by_prefix: Vec<&Record> = records
        .iter()
        .filter(|record| record.id.as_str().starts_with(&prefix))
        .collect();
    one(reference, &by_prefix)
}

/// The one record of `matches`, or the error for none or many.
fn one(reference: &str, matches: &[&Record]) -> Result<Record> {
    match matches {
        [] => Err(not_found(reference)),
        [record] => Ok((*record).clone()),
        _ => {
            let mut named: Vec<String> = matches
                .iter()
                .take(MATCHES_SHOWN)
                .map(|record| format!("{} ({})", record.id.short(), record.name))
                .collect();
            if matches.len() > MATCHES_SHOWN {
                named.push(format!("and {} more", matches.len() - MATCHES_SHOWN));
            }
            bail!(
                InvalidRequest,
                "{reference:?} names more than one container: {}",
                named.join(", ")
            )
        }
    }
}

fn not_found(reference: &str) -> Error {
    Error::new(ErrorKind::NotFound, format!("no container {reference:?}"))
}

/// Removes container `id`'s record (nothing else: what it ran, the runtime
/// removes; its writable layer and its image's pin, [`crate::run::remove`],
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
    rewrite(&store, id, change)
}

/// Container `id`'s record re-read, changed by `change` and written again
/// (atomically), its store's lock held by the caller.
fn rewrite(store: &Store, id: &ContainerId, change: impl FnOnce(&mut Record)) -> Result<Record> {
    let dir = store.dir.join(id.as_str());
    if !dir.is_dir() {
        bail!(NotFound, "no container {}", id.short());
    }
    let mut record = Record::read(&dir.join(RECORD))?;
    if record.id != *id {
        bail!(
            Storage,
            "container {}'s record is container {}'s",
            id.short(),
            record.id.short()
        );
    }
    change(&mut record);
    let json = record.to_json()?;
    crate::fs::write_atomically(&dir, RECORD, &json, 0o600)
        .map_err(|err| err.io_as(ErrorKind::Storage))?;
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

/// What container `record`'s health checks say of its current start, if
/// it has checks and is running or paused (`state`), and its companion has said anything
/// yet (its start's [`health::FILE`](crate::health::FILE)).
#[must_use]
pub fn health(
    engine: &Engine,
    record: &Record,
    state: &ContainerState,
) -> Option<crate::health::Health> {
    // Running or paused only: a container restarting (or stopping) has
    // no checks under way, and the last start's would be out of date.
    let up = matches!(
        state,
        ContainerState::Running { .. } | ContainerState::Paused
    );
    if record.config.health.is_none() || !up {
        return None;
    }
    let logs = logs_dir(engine, &record.id).ok()?;
    crate::health::Health::read(&logs.join(last_logs_name(&logs)?))
}

/// Whether container `record`'s current start lost its network (its pasta
/// died while it ran: [`network::lost`](crate::network::lost)), if it's
/// running or paused.
#[must_use]
pub fn network_lost(
    engine: &Engine,
    record: &Record,
    state: &ContainerState,
) -> Option<crate::network::lost::NetworkLost> {
    if !matches!(
        state,
        ContainerState::Running { .. } | ContainerState::Paused
    ) {
        return None;
    }
    last_start_lost(engine, record)
}

/// Whether container `record`'s last start lost its network, whatever its
/// state (a foreground run asks once its container has ended).
#[must_use]
pub fn last_start_lost(
    engine: &Engine,
    record: &Record,
) -> Option<crate::network::lost::NetworkLost> {
    let logs = logs_dir(engine, &record.id).ok()?;
    crate::network::lost::NetworkLost::read(&logs.join(last_logs_name(&logs)?))
}

/// The name of the last start's log directory in `logs` (see
/// [`logs_dir`]): the highest there, if any.
pub(crate) fn last_logs_name(logs: &std::path::Path) -> Option<String> {
    let (last, _) = starts(logs).ok()?.pop()?;
    Some(format!("{last:010}"))
}

/// The name of the next start's log directory in `logs` (see
/// [`logs_dir`]): one past the highest there.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "detached containers are Linux's")
)]
pub(crate) fn next_logs_name(logs: &std::path::Path) -> String {
    // Past every number there, whatever it names (a file in the way too).
    let last = fs::read_dir(logs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{:010}", last.saturating_add(1))
}

/// Where container `record` is in its life, from its runtime's state
/// (under [`Paths::runtime_root`](crate::Paths::runtime_root)), read
/// without taking its lock: a listing never waits behind another command.
/// Without Linux nothing runs, and a container has no runtime state.
///
/// # Errors
///
/// - [`ErrorKind::Environment`]: there's no runtime state root to look in
///   (`XDG_RUNTIME_DIR` unset, say): its state can't be known.
/// - [`ErrorKind::Runtime`]: its runtime state can't be read.
pub fn state(engine: &Engine, record: &Record) -> Result<ContainerState> {
    use shipyard_runtime::container::{self as runtime, ContainerError, Status};
    if cfg!(not(target_os = "linux")) {
        return Ok(state::derive_state(record, None, None));
    }
    let root = engine.paths.runtime_root()?;
    let id = record.id.as_str();
    let status = runtime::peek_status(root, id)?;
    let exit = match status {
        Some(Status::Stopped) => match runtime::exit_record(root, id) {
            Ok(exit) => exit,
            // Deleted since: it has no runtime state any more.
            Err(ContainerError::NotFound(_)) => {
                return Ok(state::derive_state(record, None, None));
            }
            Err(err) => return Err(err.into()),
        },
        _ => None,
    };
    Ok(state::derive_state(record, status, exit))
}

/// What a command needs of a container to reach it (stage 9, step 9.4c1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only `sockets` needs one off Linux (`Up`)")
)]
pub(crate) enum Needs {
    /// Its program running, not frozen (`exec`).
    Running,
    /// Its monitor there: running, frozen, or being started (`attach`).
    Up,
}

/// Its runtime's word on container `id`: up (being started, running,
/// frozen or stopping) or not. Read without its lock, and cheap (one file):
/// a follower asks it again and again. Without Linux nothing is ever up.
pub(crate) fn is_up(engine: &Engine, id: &ContainerId) -> bool {
    use shipyard_runtime::container::{self as runtime, Status};
    engine
        .paths
        .runtime_root()
        .ok()
        .and_then(|root| runtime::peek_status(root, id.as_str()).ok().flatten())
        .is_some_and(|status| status != Status::Stopped)
}

/// Checks container `record` is as `needs` says, so that what's wanted
/// (`to`: "run a command in it") can be done; if it isn't, says why and what
/// would do.
///
/// # Errors
///
/// - [`ErrorKind::NotRunning`]: it isn't (paused, for [`Needs::Running`]).
/// - As [`state()`]'s runtime root and runtime state.
pub(crate) fn require(engine: &Engine, record: &Record, needs: Needs, to: &str) -> Result<()> {
    use shipyard_runtime::container::{self as runtime, Status};
    let status = if cfg!(target_os = "linux") {
        runtime::peek_status(engine.paths.runtime_root()?, record.id.as_str())?
    } else {
        None
    };
    match (status, needs) {
        (Some(Status::Running), _)
        | (Some(Status::Paused | Status::Created | Status::Creating), Needs::Up) => Ok(()),
        (Some(Status::Paused), Needs::Running) => bail!(
            NotRunning,
            "container {} is paused: its processes are frozen until it's resumed, so it can't \
             {to}",
            record.name
        ),
        _ => bail!(
            NotRunning,
            "container {} is not running: start it to {to}",
            record.name
        ),
    }
}

/// The start directories in `logs` (see [`logs_dir`]), oldest first, with
/// their numbers: directories (never through a link) named by digits alone;
/// anything else there is left out. None if there's no `logs` yet.
///
/// # Errors
///
/// `logs` can't be listed.
pub(crate) fn starts(logs: &std::path::Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let entries = match fs::read_dir(logs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut numbered = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let (Ok(number), true) = (name.parse::<u64>(), entry.file_type()?.is_dir()) else {
            continue;
        };
        numbered.push((number, entry.path()));
    }
    numbered.sort();
    Ok(numbered)
}

/// Whether a reader reads every record or skips the ones it can't.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strict {
    /// A writer's: one it can't read is an error.
    Yes,
    /// A reader's: one it can't read is reported and left out.
    No,
}

/// The store, open.
struct Store {
    dir: PathBuf,
}

impl Store {
    /// The caller's store, made if it's missing and `make` (else `None`),
    /// and checked as [`made`](Self::made) checks it.
    fn open(engine: &Engine, make: bool) -> Result<Option<Self>> {
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
    fn lock(&self, wait: Duration) -> Result<nix::fcntl::Flock<fs::File>> {
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
    fn read_all(&self, engine: &Engine, strict: Strict) -> Result<Vec<Record>> {
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
    /// in place). Under the lock.
    fn publish(&self, engine: &Engine, record: &Record) -> Result<()> {
        let json = record.to_json()?;
        let staging = self.dir.join(format!("{NEW}{}", record.id));
        let dir = self.dir.join(record.id.as_str());
        let placed = (|| {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&staging)
                .with_context(|| format!("making {}", staging.display()))?;
            crate::fs::write_atomically(&staging, RECORD, &json, 0o600)?;
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
        fs::read_to_string(store.dir.join(STORE_ID))
            .ok()
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
        .map_err(|err| err.io_as(ErrorKind::Storage))?;
    store.synced(engine);
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

/// Container `id`'s record, read again by its path (no other record
/// read): for a caller that holds its locks, or one that only looks (a
/// record is replaced whole, by a rename: never read half written).
///
/// # Errors
///
/// [`ErrorKind::NotFound`] if it's gone (removed: its directory renamed
/// away); else [`Record::read`]'s.
#[cfg(target_os = "linux")]
pub(crate) fn read_one(engine: &Engine, id: &ContainerId) -> Result<Record> {
    match Record::read(&record_dir(engine, id)?.join(RECORD)) {
        Err(error)
            if error
                .find::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Err(not_found(id.as_str()))
        }
        read => read,
    }
}

/// What a container's record says of its last start: kept while a new
/// start, which overwrites it, is in flight ([`keep_previous_start`]), and
/// put back if that start doesn't happen, even if what made it was killed.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviousStart {
    /// The file's version: 1.
    version: u32,
    /// When it started (none: never).
    started: Option<std::time::SystemTime>,
    /// When it ended.
    finished: Option<std::time::SystemTime>,
    /// How it ended.
    exit: Option<Exit>,
    /// The boot it started in.
    boot_id: Option<String>,
}

#[cfg(target_os = "linux")]
impl PreviousStart {
    /// The only version written and read.
    const VERSION: u32 = 1;

    /// What `record` says of its last start.
    pub(crate) fn of(record: &Record) -> Self {
        Self {
            version: Self::VERSION,
            started: record.started,
            finished: record.finished,
            exit: record.exit.clone(),
            boot_id: record.boot_id.clone(),
        }
    }

    /// `record` saying it again.
    pub(crate) fn restore(self, record: &mut Record) {
        record.started = self.started;
        record.finished = self.finished;
        record.exit = self.exit;
        record.boot_id = self.boot_id;
    }
}

/// Keeps `previous` (what container `id`'s last start left) beside its
/// record, before a new start overwrites it there: written whole.
///
/// # Errors
///
/// [`ErrorKind::Storage`]: it couldn't be written.
#[cfg(target_os = "linux")]
pub(crate) fn keep_previous_start(
    engine: &Engine,
    id: &ContainerId,
    previous: &PreviousStart,
) -> Result<()> {
    let json = serde_json::to_vec(previous)?;
    crate::fs::write_atomically(&record_dir(engine, id)?, PREVIOUS_START, &json, 0o600)
        .map_err(|err| err.io_as(ErrorKind::Storage))
}

/// What [`keep_previous_start`] kept for container `id`, if it's there and
/// readable (one of another version is passed over).
#[cfg(target_os = "linux")]
pub(crate) fn previous_start(engine: &Engine, id: &ContainerId) -> Option<PreviousStart> {
    let bytes = std::fs::read(record_dir(engine, id).ok()?.join(PREVIOUS_START)).ok()?;
    serde_json::from_slice::<PreviousStart>(&bytes)
        .ok()
        .filter(|previous| previous.version == PreviousStart::VERSION)
}

/// Removes what [`keep_previous_start`] kept for container `id`: its start
/// happened, or what it overwrote was put back. Gone already is removed.
#[cfg(target_os = "linux")]
pub(crate) fn forget_previous_start(engine: &Engine, id: &ContainerId) {
    if let Ok(dir) = record_dir(engine, id) {
        let _ = std::fs::remove_file(dir.join(PREVIOUS_START));
    }
}

/// Container `id`'s restart lock ([`RESTARTING`]), waited for up to `wait`:
/// held by the one `__cleanup` that waits to start it again.
///
/// # Errors
///
/// [`ErrorKind::Busy`] if it's held longer; [`ErrorKind::Storage`] if the
/// container's directory can't be used.
#[cfg(target_os = "linux")]
pub(crate) fn restarting_lock(
    engine: &Engine,
    id: &ContainerId,
    wait: Duration,
) -> Result<nix::fcntl::Flock<fs::File>> {
    crate::fs::lock_waiting(
        &record_dir(engine, id)?.join(RESTARTING),
        wait,
        &format!("container {}'s restart", id.short()),
    )
}

/// Container `id`'s operation lock ([`OP_LOCK`]), waited for up to `wait`
/// (another `start` or `rm` of it may take that long): held until dropped.
///
/// # Errors
///
/// [`ErrorKind::Busy`] if it's held longer; [`ErrorKind::Storage`] if the
/// container's directory can't be used ([`ErrorKind::NotFound`] if it's
/// gone).
#[cfg(target_os = "linux")]
pub(crate) fn operation_lock(
    engine: &Engine,
    id: &ContainerId,
    wait: Duration,
) -> Result<nix::fcntl::Flock<fs::File>> {
    let dir = record_dir(engine, id)?;
    // A removal that went first: its directory is gone.
    crate::fs::lock_waiting(
        &dir.join(OP_LOCK),
        wait,
        &format!("container {}", id.short()),
    )
    .map_err(|error| {
        if fs::symlink_metadata(&dir).is_err() {
            not_found(id.as_str())
        } else {
            error
        }
    })
}

/// Container `id`'s [`RUNNING`] file, locked, waiting up to `wait` for the
/// last start's holders (its monitor, its `__cleanup`) to let go: `None` if
/// they still hold it. A std lock, so a copy handed to a monitor keeps it
/// once this one is closed.
///
/// # Errors
///
/// [`ErrorKind::Storage`] if it can't be opened or locked.
#[cfg(target_os = "linux")]
pub(crate) fn running_lock(
    engine: &Engine,
    id: &ContainerId,
    wait: Duration,
) -> Result<Option<fs::File>> {
    let dir = record_dir(engine, id)?;
    let path = dir.join(RUNNING);
    let file = crate::fs::open_lock_file(&path).map_err(|err| {
        // Removed meanwhile (`rm`, or its own `--rm` clean-up).
        if fs::symlink_metadata(&dir).is_err() {
            not_found(id.as_str())
        } else {
            err.io_as(ErrorKind::Storage)
        }
    })?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(err)) => {
                return Err(Error::from(err)
                    .context(format!("locking {}", path.display()))
                    .io_as(ErrorKind::Storage));
            }
        }
    }
}

/// Whether container `id` (as text: an image store's area names it so) has
/// a directory in this store, a record or one being written. Read with no
/// lock: a container appears and goes whole.
pub(crate) fn exists(engine: &Engine, id: &str) -> bool {
    id::is_id(id)
        && engine
            .paths
            .containers()
            .is_ok_and(|store| fs::symlink_metadata(store.join(id)).is_ok())
}

/// Where container `id`'s record is in `store` (for tests).
#[cfg(test)]
fn record_path(store: &std::path::Path, id: &ContainerId) -> PathBuf {
    store.join(id.as_str()).join(RECORD)
}

#[cfg(test)]
mod tests;
