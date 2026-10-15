//! Where a container is in its life: worked out from its record and its
//! runtime state each time it's asked, never stored, so that the two
//! can't disagree (a container killed, a host rebooted: the runtime's
//! state goes, and with it what was alive).

use std::time::SystemTime;

use shipyard_runtime::container::{ExitRecord, Status};

use super::record::Record;

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
    /// Its name, as `ps` and `inspect` say it: `created`, `running`,
    /// `paused`, `stopping`, `restarting` or `exited`.
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

    /// Whether it's up (running, paused, still stopping, or about to
    /// restart): what `ps` lists without `-a`.
    #[must_use]
    pub fn is_up(&self) -> bool {
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
