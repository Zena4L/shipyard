//! Where a container is in its life: worked out from its record and its
//! runtime state each time it's asked, never stored, so that the two
//! can't disagree (a container killed, a host rebooted: the runtime's
//! state goes, and with it what was alive).

use std::time::SystemTime;

use shipyard_runtime::container::{ExitRecord, Status};

use super::id::ContainerId;
use super::record::Record;
use crate::Engine;
use crate::error::{Result, bail};

/// A container's state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerState {
    /// Made, and never started (or started and not yet let go).
    Created,
    /// Its command runs.
    Running {
        /// Since when, if its record says.
        since: Option<SystemTime>,
    },
    /// Its processes are frozen.
    Paused,
    /// Its command has ended, and its monitor is cleaning up.
    Stopping,
    /// Ended, and its restart policy starts it again (stage 9, step 9.5b).
    Restarting {
        /// How it last ended: its exit code, if known.
        code: Option<i32>,
        /// When it last ended, if known.
        finished: Option<SystemTime>,
        /// Which restart it waits for (1: the first since its user started
        /// it).
        attempt: u32,
        /// When it's tried.
        at: Option<SystemTime>,
    },
    /// It has ended.
    Exited {
        /// Its exit code (128 + N for signal N), if known.
        code: Option<i32>,
        /// What the out-of-memory killer did, if it ended it.
        oom: Option<String>,
        /// When, if known.
        finished: Option<SystemTime>,
    },
}

impl ContainerState {
    /// Its name, as `ps` and `inspect --shipyard` say it: `created`,
    /// `running`, `paused`, `stopping`, `restarting` or `exited`.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running { .. } => "running",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Restarting { .. } => "restarting",
            Self::Exited { .. } => "exited",
        }
    }

    /// Its name as Docker says it (`docker inspect`'s `State.Status`):
    /// Docker has no stopping, so a container still stopping is
    /// `running`, as Docker's is until its end.
    #[must_use]
    pub fn docker_word(&self) -> &'static str {
        match self {
            Self::Stopping => "running",
            other => other.word(),
        }
    }

    /// Whether it's active: running, paused, still stopping, or about to
    /// restart. What `ps` lists without `-a`, and what both views of
    /// `inspect` say is running (as Docker says a restarting container is:
    /// moby's `SetRestarting`).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Running { .. } | Self::Paused | Self::Stopping | Self::Restarting { .. }
        )
    }
}

/// The state of the container `record` is, its runtime saying `runtime`
/// (`None`: it has no runtime state, never made or gone with a reboot),
/// and `exit` how it ended there, if it has.
#[must_use]
pub(crate) fn derive_state(
    record: &Record,
    runtime: Option<Status>,
    exit: Option<ExitRecord>,
) -> ContainerState {
    match runtime {
        // Ended, its clean-up waiting to start it again (its runtime state
        // gone with that end).
        None if record.next_start.is_some() => ContainerState::Restarting {
            code: record.exit.as_ref().and_then(|exit| exit.code),
            finished: record.finished,
            attempt: record.restart_count.saturating_add(1),
            at: record.next_start,
        },
        None if record.started.is_none() => ContainerState::Created,
        // Started before, and its runtime state is gone (a reboot, or it
        // was cleaned up): it ended, however its record says.
        None => ContainerState::Exited {
            code: record.exit.as_ref().and_then(|exit| exit.code),
            oom: record.exit.as_ref().and_then(|exit| exit.oom.clone()),
            finished: record.finished,
        },
        Some(Status::Running) => ContainerState::Running {
            since: record.started,
        },
        Some(Status::Paused) => ContainerState::Paused,
        Some(Status::Stopping) => ContainerState::Stopping,
        Some(Status::Stopped) => match exit {
            // Ended before it started (deleted while created): nothing ran,
            // as its clean-up will record.
            Some(exit) if exit.started == Some(false) && record.started.is_none() => {
                ContainerState::Created
            }
            Some(exit) => ContainerState::Exited {
                code: exit.code,
                oom: exit.oom,
                finished: exit.finished,
            },
            None => ContainerState::Exited {
                code: None,
                oom: None,
                finished: None,
            },
        },
        // Its policy's restart, being made: restarting still.
        Some(Status::Created | Status::Creating) if record.next_start.is_some() => {
            ContainerState::Restarting {
                code: record.exit.as_ref().and_then(|exit| exit.code),
                finished: record.finished,
                attempt: record.restart_count.saturating_add(1),
                at: record.next_start,
            }
        }
        // Creating, created, and whatever a later runtime adds: not yet
        // running.
        Some(_) => ContainerState::Created,
    }
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
///
/// [`ErrorKind::Environment`]: crate::error::ErrorKind::Environment
/// [`ErrorKind::Runtime`]: crate::error::ErrorKind::Runtime
pub fn state(engine: &Engine, record: &Record) -> Result<ContainerState> {
    use shipyard_runtime::container::{self as runtime, ContainerError, Status};
    if cfg!(not(target_os = "linux")) {
        return Ok(derive_state(record, None, None));
    }
    let root = engine.paths.runtime_root()?;
    let id = record.id.as_str();
    let status = runtime::peek_status(root, id)?;
    let exit = match status {
        Some(Status::Stopped) => match runtime::exit_record(root, id) {
            Ok(exit) => exit,
            // Deleted since: it has no runtime state any more.
            Err(ContainerError::NotFound(_)) => {
                return Ok(derive_state(record, None, None));
            }
            Err(err) => return Err(err.into()),
        },
        _ => None,
    };
    Ok(derive_state(record, status, exit))
}

/// What a command needs of a container to reach it (stage 9, step 9.4c1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "only `sockets` needs one off Linux (`Attachable`)"
    )
)]
pub(crate) enum Needs {
    /// Its program running, not frozen (`exec`).
    Running,
    /// Its init there to reach: running, frozen, or being started
    /// (`attach`, its sockets). Not one stopping: its init is gone.
    Attachable,
}

/// Its runtime's word on container `id`: whether it has a monitor (being
/// started, running, frozen or stopping) or not. Read without its lock, and
/// cheap (one file): a follower asks it again and again. Without Linux
/// nothing ever has one.
pub(crate) fn has_monitor(engine: &Engine, id: &ContainerId) -> bool {
    use shipyard_runtime::container as runtime;
    monitor_there(
        engine
            .paths
            .runtime_root()
            .ok()
            .and_then(|root| runtime::peek_status(root, id.as_str()).ok().flatten()),
    )
}

/// [`has_monitor`]'s answer for a runtime state of `status`.
pub(super) fn monitor_there(status: Option<Status>) -> bool {
    status.is_some_and(|status| status != Status::Stopped)
}

/// Why a container whose runtime state is `status` isn't as `needs` says,
/// if it isn't ([`require`]'s refusals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unmet {
    /// Frozen, where its program must run.
    Paused,
    /// Not running at all.
    NotRunning,
}

/// [`require`]'s answer for a runtime state of `status`.
pub(crate) fn unmet(status: Option<Status>, needs: Needs) -> Option<Unmet> {
    match (status, needs) {
        (Some(Status::Running), _)
        | (Some(Status::Paused | Status::Created | Status::Creating), Needs::Attachable) => None,
        (Some(Status::Paused), Needs::Running) => Some(Unmet::Paused),
        _ => Some(Unmet::NotRunning),
    }
}

/// Checks container `record` is as `needs` says, so that what's wanted
/// (`to`: "run a command in it") can be done; if it isn't, says why and what
/// would do.
///
/// # Errors
///
/// - [`ErrorKind::NotRunning`]: it isn't (paused, for [`Needs::Running`]).
/// - As [`state()`]'s runtime root and runtime state.
///
/// [`ErrorKind::NotRunning`]: crate::error::ErrorKind::NotRunning
pub(crate) fn require(engine: &Engine, record: &Record, needs: Needs, to: &str) -> Result<()> {
    use shipyard_runtime::container as runtime;
    let status = if cfg!(target_os = "linux") {
        runtime::peek_status(engine.paths.runtime_root()?, record.id.as_str())?
    } else {
        None
    };
    match unmet(status, needs) {
        None => Ok(()),
        Some(Unmet::Paused) => bail!(
            NotRunning,
            "container {} is paused: its processes are frozen until it's resumed, so it can't \
             {to}",
            record.name
        ),
        Some(Unmet::NotRunning) => bail!(
            NotRunning,
            "container {} is not running: start it to {to}",
            record.name
        ),
    }
}
