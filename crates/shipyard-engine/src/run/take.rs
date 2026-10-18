//! Taking a container to work on: "find, lock, read again", and settling
//! what its last start left ([`take`], [`take_or_leave`], [`reconcile`]).

use std::convert::Infallible;
use std::time::{Duration, SystemTime};

use shipyard_runtime::container as runtime;
use shipyard_runtime::container::Status;

use crate::Engine;
use crate::containers::{self, ContainerId, Exit, Record};
use crate::error::{ErrorKind, Result, bail};
use crate::report::WarningKind;

/// What a container being worked on is held by: its operation lock,
/// its `running` file (its last start let go).
pub(super) struct Taken {
    pub(super) record: Record,
    pub(super) running: shipyard_sys::lock::HeldFile,
    pub(super) runtime_root: std::path::PathBuf,
    pub(super) _operation: nix::fcntl::Flock<std::fs::File>,
    /// A restart its policy makes (its number since its user last
    /// started it), or `None`: its user's own start.
    pub(super) restarts: Option<u32>,
}

impl Taken {
    /// Container `record`, taken by its restart policy's waiter (its
    /// `operation` lock and its `running` file held, its runtime state
    /// under `runtime_root` gone): its next restart.
    pub(super) fn for_restart(
        record: Record,
        running: shipyard_sys::lock::HeldFile,
        runtime_root: std::path::PathBuf,
        operation: nix::fcntl::Flock<std::fs::File>,
    ) -> Self {
        Self {
            restarts: Some(record.restart_count.saturating_add(1)),
            record,
            running,
            runtime_root,
            _operation: operation,
        }
    }
}

/// What [`take`] (or [`take_or_leave`]) found. `Up` is what a container
/// left up comes to: its record for [`take_or_leave`]; for [`take`],
/// [`Infallible`], so that its callers have no arm to write for it.
pub(super) enum Took<Up = Infallible> {
    /// Taken, to start, stop or remove.
    Taken(Taken),
    /// Up, and asked to be left so (a `start` of a running container).
    Up(Up),
    /// Removed meanwhile (by its own `--rm` clean-up, which takes no
    /// operation lock).
    Gone(Record),
}

/// What [`take`] does with a container that's up.
#[derive(Clone, Copy)]
pub(super) enum WhenUp {
    /// Refuses ([`ErrorKind::InUse`]).
    Refuse,
    /// Kills it, then waits for its clean-up.
    Kill,
    /// Stops it (its own stop signal and timeout, unless these say
    /// otherwise), then waits for its clean-up.
    Stop(crate::run::StopOptions),
    /// Stops it as `Stop` does, for the host's shutdown (`system stop`):
    /// not its user's stop (its restart policy brings it back at boot).
    HostStop(crate::run::StopOptions),
}

/// Container `reference`, taken to work on (stop, restart or remove it):
/// its operation lock, then (one up dealt with as `when_up` says) its last
/// start let go (its `running` file free), what that start left settled
/// (`reconcile`). A runtime state `created` or `creating` is a dead start's
/// (a start holds the operation lock until it returns, and the container
/// is `created` only while it does): it's deleted, its monitor with it, so
/// its holds go.
pub(super) fn take(engine: &Engine, reference: &str, when_up: WhenUp) -> Result<Took> {
    take_with(engine, reference, IfUp::Do(when_up))
}

/// Container `reference`, taken as [`take`] does to start it (or copy its
/// files while it's stopped); one that's up is left so, [`Took::Up`].
pub(super) fn take_or_leave(engine: &Engine, reference: &str) -> Result<Took<Record>> {
    take_with(engine, reference, IfUp::Leave(std::convert::identity))
}

/// What [`take_with`] does with a container that's up.
enum IfUp<Up> {
    /// As [`WhenUp`] says.
    Do(WhenUp),
    /// Leaves it: [`Took::Up`], of its record. A function, so that only a
    /// caller with an `Up` to make (`take_or_leave`'s record) can ask it:
    /// for [`take`]'s, [`Infallible`], there's none to give.
    Leave(fn(Record) -> Up),
}

// By hand: derived, they'd ask `Up: Copy` of a function's result.
impl<Up> Clone for IfUp<Up> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Up> Copy for IfUp<Up> {}

/// [`take`] and [`take_or_leave`].
fn take_with<Up>(engine: &Engine, reference: &str, if_up: IfUp<Up>) -> Result<Took<Up>> {
    let when_up = match if_up {
        IfUp::Do(when_up) => Some(when_up),
        IfUp::Leave(_) => None,
    };
    let found = containers::find(engine, reference)?;
    // A stop (or a kill) is the user's: recorded before anything is
    // sent, so a clean-up racing it never restarts the container.
    let stopping = matches!(when_up, Some(WhenUp::Stop(_) | WhenUp::Kill));
    if stopping {
        mark_stopped_by_user(engine, &found.id);
    }
    let asked = ask_first(engine, &found, when_up)?;
    // Again, by its ID, now nothing else works on it: a removal that
    // went first (this one waited for its lock) left nothing, not even
    // the directory the `running` file below is in.
    let containers::Locked { record, operation } =
        containers::lock_again(engine, &found.id, containers::OP_WAIT)?;
    // Again, under the lock: a start that held it meanwhile cleared the
    // mark as it started. (`record`, read before it, is read again below
    // before anything reads the mark.)
    if stopping {
        mark_stopped_by_user(engine, &found.id);
    }
    let id = record.id.clone();
    let runtime_root = engine.paths.runtime_root()?.to_owned();
    let delete = |what: &str| -> Result<()> {
        delete_state(&runtime_root, &id)
            .map_err(|error| error.context(format!("container {}: {what}", id.short())))
    };
    // A stop's timeout is waited for too, before its clean-up's.
    let mut running_wait = containers::SETTLE_WAIT;
    match runtime::status(&runtime_root, id.as_str())? {
        Some(Status::Running | Status::Paused) => match if_up {
            IfUp::Leave(leave) => return Ok(Took::Up(leave(record))),
            IfUp::Do(WhenUp::Refuse) => bail!(
                InUse,
                "container {} is running: stop it first, or remove it with -f",
                record.name
            ),
            // Its monitor's `__cleanup` lets go, then frees `running`,
            // which is waited for below. Killed by its monitor if it was
            // asked (its exit recorded, as any end's); else forced.
            IfUp::Do(WhenUp::Kill) if asked => {}
            IfUp::Do(WhenUp::Kill) => delete("killing it")?,
            IfUp::Do(WhenUp::Stop(options) | WhenUp::HostStop(options)) => {
                let policy = options.policy(&record)?;
                running_wait = containers::SETTLE_WAIT.saturating_add(policy.timeout);
                if !asked {
                    crate::run::stop::stop_up(&runtime_root, &record, policy)?;
                }
            }
        },
        Some(Status::Created | Status::Creating) => {
            delete("ending a dead start's")?;
            // What its last start left, back (as the recovery pass does).
            crate::run::recover::undo_a_dead_start(engine, &record)?;
        }
        _ => {}
    }
    let running = match containers::running_lock(engine, &id, running_wait) {
        Ok(Some(running)) => running,
        Ok(None) => bail!(
            Busy,
            "container {} is still ending (its clean-up holds it): try again",
            record.name
        ),
        // Removed meanwhile, by its own `--rm` clean-up.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Took::Gone(record)),
        Err(error) => return Err(error),
    };
    if containers::read_one(engine, &id).is_err() {
        return Ok(Took::Gone(record));
    }
    reconcile(engine, &runtime_root, &id)?;
    // An end nothing recorded (its monitor killed): settled, and said.
    let boot = crate::run::recover::boot_id();
    let record = crate::run::recover::settle_unrecorded_end(engine, &id, boot.as_deref())?;
    Ok(Took::Taken(Taken {
        record,
        running,
        runtime_root,
        _operation: operation,
        restarts: None,
    }))
}

/// Records that container `id`'s user stopped it (`stop`, `kill`,
/// `restart`, `rm -f`): its restart policy leaves it so (Docker's).
/// Removed meanwhile: nothing to record. A record that can't be written
/// (a full disk) is said, and the stop goes on: never kept from it.
pub(super) fn mark_stopped_by_user(engine: &Engine, id: &ContainerId) {
    let marked = containers::update(engine, id, |record| {
        record.stopped_by_user = true;
        // A restart that waits gives up (and the container reads as
        // exited at once).
        record.next_start = None;
    });
    match marked {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => engine.warn(
            WarningKind::LeftForLater,
            format!(
                "container {}: that you stopped it couldn't be recorded (its restart policy \
                 may start it again): {error}",
                id.short()
            ),
        ),
    }
}

/// A stop (`WhenUp::Stop`) or a kill (`WhenUp::Kill`, `rm -f`) of a
/// container that's up, asked of its monitor before the operation lock
/// is waited for: another command (a long `stop`) may hold it, and the
/// monitor keeps the earliest SIGKILL of every stop it's asked. Whether
/// it was asked. A kill that can't be asked so is done under the lock
/// anyway (a forced delete).
fn ask_first(engine: &Engine, found: &Record, when_up: Option<WhenUp>) -> Result<bool> {
    let runtime_root = engine.paths.runtime_root()?;
    let up = runtime::peek_status(runtime_root, found.id.as_str())
        .is_ok_and(|status| status.is_some_and(Status::is_live));
    match when_up {
        Some(WhenUp::Stop(options) | WhenUp::HostStop(options)) if up => {
            let policy = options.policy(found)?;
            crate::run::stop::stop_up(runtime_root, found, policy)?;
            Ok(true)
        }
        Some(WhenUp::Kill) if up => {
            let kill = shipyard_runtime::process::StopPolicy {
                signal: shipyard_runtime::process::Signal::KILL,
                timeout: Duration::ZERO,
            };
            // `rm -f`'s kill, as Docker says it.
            crate::events::emit(
                engine,
                &crate::events::container(found, "kill").with("signal", "SIGKILL"),
            );
            Ok(crate::run::stop::stop_up(runtime_root, found, kill).is_ok())
        }
        _ => Ok(false),
    }
}

/// Container `id`'s runtime state, left by a start whose holders have
/// all let go (its `running` file free): a `__cleanup` killed or failed
/// part way, a monitor killed outright, a crash before a start's end.
/// How it ended is copied into the record if it ran, and the state is
/// deleted (forced: a `created` one can only be stale here).
pub(super) fn reconcile(
    engine: &Engine,
    runtime_root: &std::path::Path,
    id: &ContainerId,
) -> Result<()> {
    if runtime::status(runtime_root, id.as_str())?.is_none() {
        return Ok(());
    }
    if let Ok(Some(exit)) = runtime::exit_record(runtime_root, id.as_str()) {
        match exit.started {
            Some(true) => {
                // Settled here only if its clean-up didn't (killed before
                // it wrote the record): its end is said here, then.
                let mut settled = false;
                let record = containers::update(engine, id, |record| {
                    settled = record.exit.is_none();
                    record.finished = exit.finished.or_else(|| Some(SystemTime::now()));
                    record.exit = Some(Exit::from(&exit));
                })?;
                if settled {
                    crate::events::container_ended(engine, &record);
                }
                // It ran: what it kept aside is no start's to restore now.
                containers::forget_previous_start(engine, id);
            }
            // Ended before it started: its record says it never did.
            Some(false) => {
                let previous = containers::previous_start(engine, id);
                containers::update(engine, id, |record| {
                    containers::settle_unstarted(record, previous);
                })?;
                containers::forget_previous_start(engine, id);
            }
            None => {}
        }
    }
    delete_state(runtime_root, id).map_err(|error| {
        error.context(format!(
            "container {}'s runtime state, left by its last start",
            id.short()
        ))
    })
}

/// Deletes container `id`'s runtime state, forced: what's left of it
/// is killed, its cgroup removed. None left is fine.
pub(super) fn delete_state(runtime_root: &std::path::Path, id: &ContainerId) -> Result<()> {
    match runtime::delete(runtime_root, id.as_str(), true) {
        Ok(_) | Err(runtime::ContainerError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
