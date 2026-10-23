use super::*;

#[test]
fn ago_reads_as_dockers() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let before = |seconds| now - Duration::from_secs(seconds);
    for (seconds, said) in [
        (0, "Less than a second"),
        (1, "1 second"),
        (59, "59 seconds"),
        (60, "About a minute"),
        (180, "3 minutes"),
        (3600, "About an hour"),
        (7200, "2 hours"),
        (2 * 86_400, "2 days"),
        (14 * 86_400, "2 weeks"),
        (90 * 86_400, "3 months"),
        (800 * 86_400, "2 years"),
    ] {
        assert_eq!(ago(before(seconds), now), said, "{seconds}");
    }
    assert_eq!(ago(now + Duration::from_secs(5), now), "Less than a second");
}

#[test]
fn states_read_as_dockers() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let since = Some(now - Duration::from_secs(180));
    assert_eq!(
        status(&ContainerState::Running { since }, now),
        "Up 3 minutes"
    );
    assert_eq!(status(&ContainerState::Created, now), "Created");
    let exited = ContainerState::Exited {
        code: Some(137),
        oom: None,
        finished: Some(now - Duration::from_secs(7200)),
    };
    assert_eq!(status(&exited, now), "Exited (137) 2 hours ago");
    let unknown = ContainerState::Exited {
        code: None,
        oom: None,
        finished: None,
    };
    assert_eq!(status(&unknown, now), "Exited (?)");
    let restarting = ContainerState::Restarting {
        code: Some(1),
        finished: Some(now - Duration::from_secs(3)),
        attempt: 2,
        at: Some(now),
    };
    assert_eq!(status(&restarting, now), "Restarting (1) 3 seconds ago");
}

/// Health as Docker adds it to the status.
#[test]
fn health_reads_as_dockers() {
    use crate::health::Status;
    let up = || "Up 3 minutes".to_owned();
    assert_eq!(with_health(up(), None), "Up 3 minutes");
    assert_eq!(
        with_health(up(), Some(Status::Starting)),
        "Up 3 minutes (health: starting)"
    );
    assert_eq!(
        with_health(up(), Some(Status::Healthy)),
        "Up 3 minutes (healthy)"
    );
    assert_eq!(
        with_health(up(), Some(Status::Unhealthy)),
        "Up 3 minutes (unhealthy)"
    );
    // A lost network after any health.
    assert_eq!(
        with_network(with_health(up(), Some(Status::Healthy)), true),
        "Up 3 minutes (healthy) (no network)"
    );
    assert_eq!(with_network(up(), false), "Up 3 minutes");
}

/// The released record, running with a port, listed as Docker lists it.
#[test]
fn a_container_is_listed_as_docker_lists_it() {
    let mut record =
        crate::containers::Record::parse(crate::containers::tests::versions::V3_RECORD.as_bytes())
            .unwrap();
    // An explicit network: the default one is the host's to choose.
    record.config.network = vec![crate::containers::Network::Host];
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(488_889);
    let summary = Summary {
        state: ContainerState::Running {
            since: Some(now - Duration::from_secs(180)),
        },
        command: crate::containers::shown_command(&record, None),
        record,
        health: None,
        network_lost: None,
    };
    let shown = serde_json::to_value(summarized(&summary, &[], None, now)).unwrap();
    let expected = serde_json::json!({
        "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Names": ["/brave_tugboat"],
        "Image": "alpine",
        "ImageID": "sha256:0123",
        "Command": "sh",
        "Created": 1_760_000_000,
        "Ports": [{"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}],
        "Labels": {"tier": "web"},
        "State": "running",
        "Status": "Up 3 minutes",
        "HostConfig": {"NetworkMode": "host"},
        "NetworkSettings": {"Networks": {"host": {
            "Aliases": null, "MacAddress": "", "NetworkID": "", "EndpointID": "", "IPAMConfig": null,
            "Gateway": "", "IPAddress": "", "IPPrefixLen": 0, "IPv6Gateway": "",
            "GlobalIPv6Address": "", "GlobalIPv6PrefixLen": 0
        }}},
        "Mounts": []
    });
    assert_eq!(shown, expected);
}

/// A paused container is up, paused, as Docker says it (2026-10-20's
/// capture: `Up 5 seconds (Paused)`), since its start; no health (Docker's
/// `State.String` says paused first).
#[test]
fn a_paused_container_is_up_and_paused() {
    let mut record =
        crate::containers::Record::parse(crate::containers::tests::versions::V3_RECORD.as_bytes())
            .unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(488_889);
    record.started = Some(now - Duration::from_secs(5));
    let summary = Summary {
        state: ContainerState::Paused,
        command: Vec::new(),
        record,
        health: None,
        network_lost: None,
    };
    assert_eq!(status_text(&summary, now), "Up 5 seconds (Paused)");
    let never = Summary {
        record: crate::containers::Record {
            started: None,
            ..summary.record.clone()
        },
        ..summary
    };
    assert_eq!(status_text(&never, now), "Up (Paused)");
}

/// Health in the status while it runs only, as Docker's `State.String`:
/// paused or ended, none (F133: inspect keeps the last, the status doesn't).
#[test]
fn health_is_in_the_status_while_it_runs_only() {
    let mut record =
        crate::containers::Record::parse(crate::containers::tests::versions::V3_RECORD.as_bytes())
            .unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(488_889);
    record.started = Some(now - Duration::from_secs(5));
    let mut unhealthy = crate::health::Health::starting();
    unhealthy.status = crate::health::Status::Unhealthy;
    let summary = |state| Summary {
        state,
        command: Vec::new(),
        record: record.clone(),
        health: Some(unhealthy.clone()),
        network_lost: None,
    };
    let running = summary(ContainerState::Running {
        since: Some(now - Duration::from_secs(5)),
    });
    assert_eq!(status_text(&running, now), "Up 5 seconds (unhealthy)");
    let paused = summary(ContainerState::Paused);
    assert_eq!(status_text(&paused, now), "Up 5 seconds (Paused)");
    let ended = summary(ContainerState::Exited {
        code: Some(0),
        finished: Some(now - Duration::from_secs(5)),
        oom: None,
    });
    assert!(
        !status_text(&ended, now).contains("unhealthy"),
        "{}",
        status_text(&ended, now)
    );
}
