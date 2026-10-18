//! A container's locks: its operation lock, its `running` file, its restart
//! lock (the order is in [`super`]'s doc).

use std::fs;
use std::time::Duration;

use super::ContainerId;
use super::lookup::not_found;
use super::store::record_dir;
use super::{OP_LOCK, RESTARTING, RUNNING};
use crate::Engine;
use crate::error::{Error, ErrorKind, Result};

/// Container `id`'s restart lock ([`RESTARTING`]), waited for up to `wait`:
/// held by the one `__cleanup` that waits to start it again.
///
/// # Errors
///
/// [`ErrorKind::Busy`] if it's held longer; [`ErrorKind::Storage`] if the
/// container's directory can't be used.
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
/// they still hold it. A std lock held in a
/// [`HeldFile`](shipyard_sys::lock::HeldFile): let go when it drops (F66:
/// at once, even while another thread's child still holds a copy until it
/// execs), unless it was handed to a monitor and given up
/// (`HeldFile::give_up`), when the monitor's copy keeps it.
///
/// # Errors
///
/// [`ErrorKind::Storage`] if it can't be opened or locked.
pub(crate) fn running_lock(
    engine: &Engine,
    id: &ContainerId,
    wait: Duration,
) -> Result<Option<shipyard_sys::lock::HeldFile>> {
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
    let poll = Duration::from_millis(20);
    match shipyard_sys::lock::try_lock_until(
        &file,
        shipyard_sys::lock::Mode::Exclusive,
        deadline,
        poll,
    ) {
        Ok(true) => Ok(Some(shipyard_sys::lock::HeldFile::new(file))),
        Ok(false) => Ok(None),
        Err(err) => Err(Error::from(err)
            .context(format!("locking {}", path.display()))
            .io_as(ErrorKind::Storage)),
    }
}

/// How long a command working on a container (`start`, `stop`, `rm`,
/// `rename`…) waits for another's operation on it: a start may pull and
/// extract meanwhile.
pub(crate) const OP_WAIT: Duration = Duration::from_secs(120);

/// How long a command waits at a time for a container's end to settle:
/// its last start's holders (its monitor, its `__cleanup`) to let go.
/// `__cleanup` waits up to 10 s for its monitor, then releases, each step
/// waiting up to 15 s for a lock.
pub(crate) const SETTLE_WAIT: Duration = Duration::from_secs(60);

/// A container held to work on: its operation lock, and its record read
/// again under it.
pub(crate) struct Locked {
    /// Its record, read by its ID once the lock was held.
    pub(crate) record: super::Record,
    /// Its operation lock, held until this is dropped.
    pub(crate) operation: nix::fcntl::Flock<fs::File>,
}

/// Container `id`'s operation lock, waited for up to `wait`, then its
/// record read again by its ID alone (no other record read: a record is
/// replaced whole): "find, then lock, then read again", once found.
/// Whatever was read before the lock may have changed meanwhile (a
/// rename, an update, a removal that went first).
///
/// # Errors
///
/// As [`operation_lock`]; [`ErrorKind::NotFound`] if it was removed
/// meanwhile; else as [`read_one`](super::read_one).
pub(crate) fn lock_again(engine: &Engine, id: &ContainerId, wait: Duration) -> Result<Locked> {
    let operation = operation_lock(engine, id, wait)?;
    let record = super::read_one(engine, id)?;
    Ok(Locked { record, operation })
}

/// The container `reference` names ([`find`](super::find)), then
/// [`lock_again`].
///
/// # Errors
///
/// As [`find`](super::find) and [`lock_again`].
pub(crate) fn lock_found(engine: &Engine, reference: &str, wait: Duration) -> Result<Locked> {
    let found = super::find(engine, reference)?;
    lock_again(engine, &found.id, wait)
}

/// Waits up to `wait` for container `id`'s end to settle (its `running`
/// file free: its clean-up has recorded the end, and, with `--rm`, removed
/// it), then reads its record again: `None` if its holders still hold it.
///
/// # Errors
///
/// [`ErrorKind::NotFound`] if it's gone (`--rm`); as [`running_lock`] and
/// [`read_one`](super::read_one).
pub(crate) fn settled(
    engine: &Engine,
    id: &ContainerId,
    wait: Duration,
) -> Result<Option<super::Record>> {
    if running_lock(engine, id, wait)?.is_none() {
        return Ok(None);
    }
    super::read_one(engine, id).map(Some)
}

/// Waits up to `wait` for container `id`'s end to settle, as [`settled`]
/// does, and no more: what came of it is the caller's next read.
pub(crate) fn wait_settled(engine: &Engine, id: &ContainerId, wait: Duration) {
    drop(running_lock(engine, id, wait));
}
