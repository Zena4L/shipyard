//! Removing a container ([`remove`]), readable or not, and the last of
//! every removal: its pin and record let go of ([`forget`]).

use std::time::Duration;

use shipyard_runtime::container as runtime;

use super::take::{Taken, Took, WhenUp, delete_state, take};
use crate::Engine;
use crate::containers::{self, ContainerId};
use crate::error::{ErrorKind, Result, bail};

pub(super) fn remove(engine: &Engine, reference: &str, force: bool) -> Result<ContainerId> {
    crate::run::recover::recover_others(engine, Some(reference));
    let when_up = if force { WhenUp::Kill } else { WhenUp::Refuse };
    let took = match take(engine, reference, when_up) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return match unreadable(engine, reference) {
                Some(id) if force => remove_unreadable(engine, &id).map(|()| id),
                Some(id) => Err(containers::read_one(engine, &id)
                    .err()
                    .map_or(error, |why| {
                        why.context(format!(
                            "container {}'s record can't be read: `rm -f {id}` removes what's \
                         left of it",
                            id.short()
                        ))
                    })),
                None => Err(error),
            };
        }
        took => took?,
    };
    // `_operation` bound, not `..`: the operation lock is held until the
    // removal is done (a field left to `..` would go at once).
    let Taken {
        record,
        running,
        runtime_root: _,
        _operation,
        restarts: _,
    } = match took {
        Took::Taken(taken) => taken,
        // Its own `--rm` clean-up removed it meanwhile: removed (but for
        // its monitor's cgroup, which that clean-up was inside).
        Took::Gone(record) => {
            runtime::remove_monitor_cgroup(record.id.as_str());
            return Ok(record.id);
        }
    };
    // Its policy about to start it again: up, as Docker sees it. (Unless
    // what was to restart it is gone: its lock free.)
    if !force && record.next_start.is_some() {
        if containers::restarting_lock(engine, &record.id, Duration::ZERO).is_ok() {
            containers::update(engine, &record.id, |record| record.next_start = None)?;
        } else {
            bail!(
                InUse,
                "container {} is restarting: stop it first, or remove it with -f",
                record.name
            );
        }
    }
    let id = record.id.clone();
    crate::image::remove_owned_area(engine, id.as_str())
        .map_err(|error| error.context("removing the container's writable layer"))?;
    forget(engine, &id)?;
    drop(running);
    // Its monitor's, from outside it: its last processes have let go.
    runtime::remove_monitor_cgroup(id.as_str());
    Ok(id)
}

/// The container `reference` is the full ID of, if it has a directory
/// in the store but no record [`containers::find`] could read.
fn unreadable(engine: &Engine, reference: &str) -> Option<ContainerId> {
    ContainerId::parse(reference)
        .ok()
        .filter(|id| containers::exists(engine, id.as_str()))
}

/// Removes container `id`, whose record can't be read, by what its ID
/// names: under its operation lock, what's left of its last start
/// killed and its clean-up waited for (its `running` file free), then
/// its writable layer, its pin and its directory, as [`remove`] does.
fn remove_unreadable(engine: &Engine, id: &ContainerId) -> Result<()> {
    let _operation = containers::operation_lock(engine, id, containers::OP_WAIT)?;
    delete_state(engine.paths.runtime_root()?, id)?;
    let Some(running) = containers::running_lock(engine, id, containers::SETTLE_WAIT)? else {
        bail!(
            Busy,
            "container {}'s last start is still being let go: try again",
            id.short()
        );
    };
    crate::image::remove_owned_area(engine, id.as_str())
        .map_err(|error| error.context("removing the container's writable layer"))?;
    unmake(engine, id)?;
    drop(running);
    runtime::remove_monitor_cgroup(id.as_str());
    crate::events::emit_about(
        engine,
        crate::events::Kind::Container,
        "destroy",
        id.as_str(),
    );
    Ok(())
}

/// Lets go of container `id`'s image (its pin), then removes its record
/// (with its logs): the last of a container's removal, its writable layer
/// already gone, saying it's destroyed. `rm`'s, `--rm`'s (`__cleanup`) and
/// recovery's.
///
/// # Errors
///
/// The pin or the record couldn't be removed (the pin first: a record left
/// is removed again by a later `rm`).
pub(super) fn forget(engine: &Engine, id: &ContainerId) -> Result<()> {
    // Read first: once it's gone, its name is too.
    let gone = crate::containers::read_one(engine, id)
        .map(|record| crate::events::container(&record, "destroy"));
    unmake(engine, id)?;
    if let Ok(event) = gone {
        crate::events::emit(engine, &event);
    }
    Ok(())
}

/// [`forget`]'s work, unsaid: for a create undone before it was said to
/// be made.
pub(super) fn unmake(engine: &Engine, id: &ContainerId) -> Result<()> {
    crate::image::unpin(engine, id.as_str())
        .map_err(|error| error.context("letting go of the container's image"))?;
    crate::containers::remove_record(engine, id)
}
