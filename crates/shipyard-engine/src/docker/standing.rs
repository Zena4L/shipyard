//! Where a container stands, as Docker's answers about its life need it
//! (step 11.2a): whether `start` and `stop` have anything to do (Docker
//! answers 304 when they haven't), and how it last ended, for `wait`'s
//! conditions (an end its restart policy follows counts, as moby's
//! `SetRestarting` wakes its waiters; one removed before it was seen is
//! read from the events journal).

use std::time::SystemTime;

use crate::Engine;
use crate::containers::{self, ContainerId, ContainerState};
use crate::error::Result;

/// A container's standing: read without a lock, as `ps` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// Its ID.
    pub id: ContainerId,
    /// Its state.
    pub state: ContainerState,
    /// Whether it has a terminal of its own (`-t`): its output is one
    /// stream then, as Docker sends it raw.
    pub terminal: bool,
    /// Whether it's removed once it ends (`--rm`).
    pub auto_remove: bool,
}

/// How a container last ended, as `wait` tells one end from the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct End {
    /// Its exit code (128 + N for signal N), if known.
    pub code: Option<i32>,
    /// When, if known.
    pub finished: Option<SystemTime>,
}

impl Standing {
    /// Whether Docker says it's running (`State.Running`): running,
    /// paused, still stopping, or about to restart, as moby's is.
    #[must_use]
    pub fn running(&self) -> bool {
        self.state.is_active()
    }

    /// Whether Docker's `start` has nothing to do: it's running already
    /// (a paused one isn't, so that its start is refused, as Docker's is).
    #[must_use]
    pub fn started(&self) -> bool {
        self.running() && self.state != ContainerState::Paused
    }

    /// Whether its restart policy is about to start it again.
    #[must_use]
    pub fn restarting(&self) -> bool {
        matches!(self.state, ContainerState::Restarting { .. })
    }

    /// How it last ended, if it has ended (and stays so, or its restart
    /// policy is about to start it again).
    #[must_use]
    pub fn ended(&self) -> Option<End> {
        match self.state {
            ContainerState::Exited { code, finished, .. }
            | ContainerState::Restarting { code, finished, .. } => Some(End { code, finished }),
            _ => None,
        }
    }
}

/// The exit code the events journal gives container `id`'s last end
/// (`die`'s `exitCode`) since `since`: for a wait whose container was
/// removed (`--rm`) before its end was seen. `None` if it has none there.
///
/// # Errors
///
/// The journal can't be read.
pub fn last_exit(engine: &Engine, id: &ContainerId, since: SystemTime) -> Result<Option<i32>> {
    let options = crate::events::EventsOptions {
        since: Some(since),
        until: Some(SystemTime::now()),
        filters: crate::events::Filters::parse(&[
            format!("container={id}"),
            "event=die".to_owned(),
        ])?,
        ..crate::events::EventsOptions::default()
    };
    let mut code = None;
    crate::events::read(engine, &options, |event| {
        if event.id == id.as_str() {
            code = event
                .attributes
                .get("exitCode")
                .and_then(|code| code.parse().ok());
        }
        Ok(())
    })?;
    Ok(code)
}

/// Where the container `reference` names stands now.
///
/// # Errors
///
/// As [`containers::find`] and [`containers::state`]: no such container
/// ([`ErrorKind::NotFound`](crate::ErrorKind::NotFound)), a reference to
/// more than one, its state unreadable.
pub fn standing(engine: &Engine, reference: &str) -> Result<Standing> {
    let record = containers::find(engine, reference)?;
    let state = containers::state(engine, &record)?;
    Ok(Standing {
        terminal: record.config.tty,
        auto_remove: record.auto_remove,
        id: record.id,
        state,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    fn standing(state: ContainerState) -> Standing {
        Standing {
            id: ContainerId::new().unwrap(),
            state,
            terminal: false,
            auto_remove: false,
        }
    }

    /// Docker's `start` is a no-op for a running container and refused for a
    /// paused one; its `stop` a no-op for one that isn't running.
    #[test]
    fn started_and_running_are_docker_s() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
        let exited = ContainerState::Exited {
            code: Some(3),
            oom: None,
            finished: Some(at),
        };
        let restarting = ContainerState::Restarting {
            code: Some(1),
            finished: Some(at),
            attempt: 1,
            at: None,
        };
        let cases = [
            (ContainerState::Created, false, false),
            (ContainerState::Running { since: None }, true, true),
            (ContainerState::Paused, true, false),
            (ContainerState::Stopping, true, true),
            (restarting, true, true),
            (exited.clone(), false, false),
        ];
        for (state, running, started) in cases {
            let standing = standing(state.clone());
            assert_eq!(standing.running(), running, "{state:?}");
            assert_eq!(standing.started(), started, "{state:?}");
        }
        assert_eq!(
            standing(exited).ended(),
            Some(End {
                code: Some(3),
                finished: Some(at)
            })
        );
        assert_eq!(standing(ContainerState::Created).ended(), None);
        // An end its restart policy follows is an end, for `wait`.
        let restarting = standing(ContainerState::Restarting {
            code: Some(1),
            finished: Some(at),
            attempt: 1,
            at: None,
        });
        assert!(restarting.restarting());
        assert_eq!(
            restarting.ended(),
            Some(End {
                code: Some(1),
                finished: Some(at)
            })
        );
        assert!(!standing(ContainerState::Running { since: None }).restarting());
    }
}
