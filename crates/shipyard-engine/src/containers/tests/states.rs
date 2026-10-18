//! A container's state, from its record and its runtime, and what "up"
//! means.

use std::fs;
use std::time::SystemTime;

use shipyard_runtime::container::{ExitRecord, Status};

use super::*;

/// The state is worked out from the record and the runtime, every case.
/// A container its runtime ended before it started (deleted while created:
/// its exit record says so) is `Created`, never `Exited` with the forced
/// end's code; one that ran is `Exited` with its own.
#[test]
fn a_container_ended_before_it_started_is_created() {
    let (engine, _, dir) = engine("never-started");
    let record = create_record(&engine, new(None)).unwrap();
    let ended = |started: bool| {
        shipyard_runtime::container::ExitRecord::parse(
            format!(
                r#"{{"version":2,"description":"killed by SIGKILL (exit code 137)","code":137,"started":{started}}}"#
            )
            .as_bytes(),
        )
    };
    assert_eq!(
        state::derive_state(&record, Some(Status::Stopped), Some(ended(false))),
        ContainerState::Created
    );
    assert!(matches!(
        state::derive_state(&record, Some(Status::Stopped), Some(ended(true))),
        ContainerState::Exited {
            code: Some(137),
            ..
        }
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_state_follows_the_record_and_the_runtime() {
    let (engine, _, dir) = engine("state");
    let mut record = create_record(&engine, new(None)).unwrap();
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
    assert_eq!(
        state::derive_state(&record, None, None),
        ContainerState::Created
    );
    assert_eq!(
        state::derive_state(&record, Some(Status::Created), None),
        ContainerState::Created
    );
    assert_eq!(
        state::derive_state(&record, Some(Status::Creating), None),
        ContainerState::Created
    );
    record.started = Some(at);
    assert_eq!(
        state::derive_state(&record, Some(Status::Running), None),
        ContainerState::Running { since: Some(at) }
    );
    assert_eq!(
        state::derive_state(&record, Some(Status::Paused), None),
        ContainerState::Paused
    );
    assert_eq!(
        state::derive_state(&record, Some(Status::Stopping), None),
        ContainerState::Stopping
    );
    let exit: ExitRecord = serde_json::from_str(
        r#"{"version":1,"description":"exited (exit code 3)","code":3,
            "finished":{"secs_since_epoch":1760000000,"nanos_since_epoch":0}}"#,
    )
    .unwrap();
    assert_eq!(
        state::derive_state(&record, Some(Status::Stopped), Some(exit)),
        ContainerState::Exited {
            code: Some(3),
            oom: None,
            finished: Some(at)
        }
    );
    // Its runtime state gone (a reboot): ended, as its record last said.
    record.exit = Some(Exit {
        code: Some(137),
        oom: Some("out of memory".to_owned()),
    });
    record.finished = Some(at);
    assert_eq!(
        state::derive_state(&record, None, None),
        ContainerState::Exited {
            code: Some(137),
            oom: Some("out of memory".to_owned()),
            finished: Some(at)
        }
    );
    fs::remove_dir_all(dir).unwrap();
}

/// What "up" means, for every runtime state and container state: each
/// predicate (step 10.1b) answers as the check it replaced did, written out
/// here as it was: `is_up`'s "not stopped" (now [`has_monitor`]), the
/// inlined "running or paused" (now `Status::is_live`), `require`'s two
/// needs, and `ContainerState::is_up` (now `is_active`).
#[test]
fn every_meaning_of_up_is_as_it_was() {
    use crate::containers::state::{Unmet, monitor_there, unmet};
    // Runtime state → has a monitor, is live, needs Running, needs Created.
    for (status, monitor, live, running, created) in [
        (
            Some(Status::Creating),
            true,
            false,
            Some(Unmet::NotRunning),
            None,
        ),
        (
            Some(Status::Created),
            true,
            false,
            Some(Unmet::NotRunning),
            None,
        ),
        (Some(Status::Running), true, true, None, None),
        (Some(Status::Paused), true, true, Some(Unmet::Paused), None),
        (
            Some(Status::Stopping),
            true,
            false,
            Some(Unmet::NotRunning),
            Some(Unmet::NotRunning),
        ),
        (
            Some(Status::Stopped),
            false,
            false,
            Some(Unmet::NotRunning),
            Some(Unmet::NotRunning),
        ),
        (
            None,
            false,
            false,
            Some(Unmet::NotRunning),
            Some(Unmet::NotRunning),
        ),
    ] {
        assert_eq!(monitor_there(status), monitor, "{status:?}: has a monitor");
        assert_eq!(
            status.is_some_and(Status::is_live),
            live,
            "{status:?}: live"
        );
        assert_eq!(
            unmet(status, Needs::Running),
            running,
            "{status:?}: Running"
        );
        assert_eq!(
            unmet(status, Needs::Attachable),
            created,
            "{status:?}: Created"
        );
    }
    for (state, active) in [
        (ContainerState::Created, false),
        (ContainerState::Running { since: None }, true),
        (ContainerState::Paused, true),
        (ContainerState::Stopping, true),
        (
            ContainerState::Restarting {
                code: None,
                finished: None,
                attempt: 1,
                at: None,
            },
            true,
        ),
        (
            ContainerState::Exited {
                code: Some(0),
                oom: None,
                finished: None,
            },
            false,
        ),
    ] {
        assert_eq!(state.is_active(), active, "{state:?}");
    }
}

/// A start that never ran leaves no start, no end and no exit, whichever
/// path settled it (F64), and nothing else changes.
#[test]
fn a_reset_to_never_started_clears_start_end_and_exit() {
    let (engine, _, dir) = engine("never");
    let mut ran = create_record(&engine, new(None)).unwrap();
    ran.started = Some(SystemTime::UNIX_EPOCH);
    ran.finished = Some(SystemTime::UNIX_EPOCH);
    ran.exit = Some(Exit {
        code: Some(3),
        oom: None,
    });
    let mut record = ran.clone();
    record.mark_never_started();
    assert_eq!(
        (record.started, record.finished, &record.exit),
        (None, None, &None)
    );
    // Nothing else.
    record.started = ran.started;
    record.exit.clone_from(&ran.exit);
    record.finished = ran.finished;
    assert_eq!(record, ran);
    fs::remove_dir_all(dir).unwrap();
}
