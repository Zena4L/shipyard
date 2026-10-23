use std::time::{Duration, SystemTime};

use shipyard_engine::containers::{ContainerId, ContainerState};
use shipyard_engine::docker::Standing;

use super::*;

fn id() -> ContainerId {
    ContainerId::parse(&"a".repeat(64)).unwrap()
}

fn standing(state: ContainerState) -> Standing {
    Standing {
        id: id(),
        state,
        terminal: false,
        auto_remove: false,
    }
}

fn running() -> Standing {
    standing(ContainerState::Running { since: None })
}

fn exited(code: i32, secs: u64) -> Standing {
    standing(ContainerState::Exited {
        code: Some(code),
        oom: None,
        finished: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
    })
}

fn restarting(code: i32, secs: u64) -> Standing {
    standing(ContainerState::Restarting {
        code: Some(code),
        finished: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        attempt: 1,
        at: None,
    })
}

/// Each look, in turn: what the wait does after the last one it took (the
/// first that isn't another look).
fn watch(condition: Condition, looks: &[Option<Standing>]) -> Next {
    let mut watch = Watch::new(condition);
    for look in looks {
        let next = watch.look(look.as_ref());
        if next != Next::Again {
            return next;
        }
    }
    Next::Again
}

fn code(code: i64) -> Next {
    Next::Answer(WaitAnswer {
        status_code: code,
        error: None,
    })
}

/// `not-running`: at once for one that isn't (its last code, 0 if it never
/// ran); else once it ends.
#[test]
fn not_running_is_now_or_its_end() {
    assert_eq!(watch(Condition::NotRunning, &[Some(exited(3, 1))]), code(3));
    assert_eq!(
        watch(
            Condition::NotRunning,
            &[Some(standing(ContainerState::Created))]
        ),
        code(0)
    );
    assert_eq!(
        watch(
            Condition::NotRunning,
            &[Some(running()), Some(running()), Some(exited(7, 2))]
        ),
        code(7)
    );
    assert_eq!(
        watch(Condition::NotRunning, &[Some(running()), Some(running())]),
        Next::Again
    );
}

/// `next-exit`: never the end it had when the wait began; the next one.
#[test]
fn next_exit_waits_for_a_new_end() {
    assert_eq!(
        watch(
            Condition::NextExit,
            &[Some(exited(3, 1)), Some(exited(3, 1))]
        ),
        Next::Again
    );
    assert_eq!(
        watch(
            Condition::NextExit,
            &[Some(exited(3, 1)), Some(running()), Some(exited(4, 2))]
        ),
        code(4)
    );
    // Created, then started and ended: its first end.
    assert_eq!(
        watch(
            Condition::NextExit,
            &[
                Some(standing(ContainerState::Created)),
                Some(running()),
                Some(exited(0, 5))
            ]
        ),
        code(0)
    );
}

/// An end its restart policy follows is an end, as moby wakes its waiters
/// at a restart; the one it had when the wait began isn't.
#[test]
fn an_end_before_a_restart_is_an_end() {
    for condition in [Condition::NotRunning, Condition::NextExit] {
        assert_eq!(
            watch(condition, &[Some(running()), Some(restarting(5, 2))]),
            code(5),
            "{condition:?}"
        );
        assert_eq!(
            watch(condition, &[Some(restarting(5, 2)), Some(restarting(5, 2))]),
            Next::Again,
            "{condition:?}: the end it began with"
        );
        assert_eq!(
            watch(
                condition,
                &[
                    Some(restarting(5, 2)),
                    Some(running()),
                    Some(restarting(6, 3))
                ]
            ),
            code(6),
            "{condition:?}"
        );
    }
}

/// `removed`: once it's gone, with the last end seen; one gone before an
/// end was seen is the journal's to tell, never a made-up code.
#[test]
fn removed_waits_for_it_to_go() {
    assert_eq!(
        watch(
            Condition::Removed,
            &[Some(exited(3, 1)), Some(exited(3, 1))]
        ),
        Next::Again
    );
    assert_eq!(
        watch(
            Condition::Removed,
            &[Some(running()), Some(exited(9, 1)), None]
        ),
        code(9)
    );
    for condition in [
        Condition::NotRunning,
        Condition::NextExit,
        Condition::Removed,
    ] {
        assert_eq!(
            watch(condition, &[Some(running()), None]),
            Next::GoneUnseen,
            "{condition:?}"
        );
    }
    assert_eq!(WaitAnswer::unseen().status_code, -1);
    assert!(WaitAnswer::unseen().error.is_some());
}

/// `t` and `signal`: Docker's -1 is as long as it takes.
#[test]
fn stop_options_are_read_from_the_query() {
    let options = stop_options(&Query::parse(Some("t=3&signal=SIGINT")).unwrap()).unwrap();
    assert_eq!(options.timeout, Some(Duration::from_secs(3)));
    assert_eq!(options.signal, Some(Signal::INT));
    let forever = stop_options(&Query::parse(Some("t=-1")).unwrap()).unwrap();
    assert_eq!(
        forever.timeout,
        Some(Duration::from_secs(u64::from(u32::MAX)))
    );
    let none = stop_options(&Query::default()).unwrap();
    assert_eq!((none.timeout, none.signal), (None, None));
    assert!(stop_options(&Query::parse(Some("signal=NOPE")).unwrap()).is_err());
}

/// Docker's update body: its CLI always sends `RestartPolicy`, `{"Name":
/// ""}` without `--restart`, which changes nothing (Docker's daemon's
/// rule); a named one is taken; zeros ask nothing; what Shipyard can't
/// change here is refused, never ignored.
#[test]
fn an_update_asks_what_docker_s_does() {
    let request = |body: &str| {
        let asked: crate::create::HostConfig = serde_json::from_str(body).unwrap();
        update_request(&asked)
    };
    let memory = request(
        r#"{"Memory": 67108864, "PidsLimit": 0, "CpuShares": 0,
            "RestartPolicy": {"Name": "", "MaximumRetryCount": 0}}"#,
    )
    .unwrap();
    assert_eq!(memory.restart, None, "no --restart: the policy kept");
    assert_eq!(
        memory.limits.as_ref().and_then(|limits| limits.memory),
        Some(64 << 20)
    );
    let restart = request(r#"{"RestartPolicy": {"Name": "always"}}"#).unwrap();
    assert_eq!(
        restart.restart,
        Some(shipyard_engine::containers::RestartPolicy::Always)
    );
    assert!(restart.limits.is_none());
    let zeros = request(r#"{"Memory": 0, "NanoCpus": 0, "MemorySwappiness": -1}"#).unwrap();
    assert_eq!(zeros, shipyard_engine::run::UpdateRequest::default());
    for refused in [
        r#"{"OomKillDisable": true}"#,
        r#"{"Ulimits": [{"Name": "nofile", "Soft": 1, "Hard": 1}]}"#,
        r#"{"MemorySwappiness": 10}"#,
        r#"{"CpuRealtimeRuntime": 1000}"#,
    ] {
        assert_eq!(
            request(refused).unwrap_err().status,
            hyper::StatusCode::BAD_REQUEST,
            "{refused}"
        );
    }
}
