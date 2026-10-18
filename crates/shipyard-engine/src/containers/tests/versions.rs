//! The record's versions: each one pinned as written, the upgrade rule,
//! and `inspect`'s JSON.

use std::fs;
use std::time::SystemTime;

use shipyard_runtime::process::Signal;

use super::*;
use crate::ErrorKind;
use crate::network::NetworkMode;

/// The record's format, pinned: a literal version 1 record reads as
/// written, and writing it again gives the same record.
#[test]
fn version_1_reads_as_written() {
    let literal = r#"{
        "version": 1,
        "id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "serial": 304812,
        "name": "brave_tugboat",
        "created": {"secs_since_epoch": 1760000000, "nanos_since_epoch": 0},
        "config": {
            "interactive": false, "tty": false, "env": ["A=1"], "workdir": null,
            "user": null, "group_add": [], "ulimit": [], "oom_score_adj": null,
            "device": [], "device_cgroup_rule": [], "stop_signal": "SIGTERM",
            "stop_timeout": 10, "no_isolation": false, "hostname": null,
            "domainname": null, "sysctl": [], "network": ["pasta"],
            "network_alias": [], "ip": null, "ip6": null, "mac_address": null,
            "add_host": [], "dns": [], "dns_search": [], "dns_option": [],
            "publish": [{"host_ip": null, "host_port": 8080, "container_port": 80,
                         "protocol": "tcp"}],
            "publish_all": false, "rootfs": null, "image": "alpine",
            "entrypoint": null, "pull": "missing", "read_only": false,
            "cap_add": ["NET_ADMIN"], "cap_drop": [], "security_opt": [],
            "uidmap": [], "gidmap": [], "bundle": null,
            "resources": {"swap_is_default": false, "cpu_max": false, "io_max": [],
                          "unified": {}},
            "volume": [], "command": ["sh"]
        },
        "labels": {"tier": "web"},
        "started": null,
        "finished": null,
        "exit": null
    }"#;
    let record = Record::parse(literal.as_bytes()).unwrap();
    assert_eq!(record.serial.to_string(), "SHPU 304812 2");
    assert_eq!(record.name.as_str(), "brave_tugboat");
    assert_eq!(record.started, None);
    // What version 2 added, as a version 1 record has none of it.
    assert_eq!(record.image_id, None);
    assert!(!record.auto_remove);
    assert!(record.ports.is_empty());
    let request = record.to_request().unwrap();
    assert_eq!(request.name.as_deref(), Some("brave_tugboat"));
    assert_eq!(request.network, [NetworkMode::Pasta]);
    assert_eq!(request.publish[0].host_port, Some(8080));
    assert_eq!(request.stop_signal.as_raw(), Signal::TERM.as_raw());
    // What version 3 added, as a version 1 record has none of it.
    assert_eq!(record.config.restart, crate::containers::RestartPolicy::No);
    assert!(!record.stopped_by_user);
    assert_eq!(
        (
            record.restart_count,
            record.boot_id.clone(),
            record.next_start
        ),
        (0, None, None)
    );
    // Written again, it's version 3, and reads back the same.
    let written = record.to_json_new().unwrap();
    assert!(String::from_utf8_lossy(&written).contains("\"version\": 3"));
    assert_eq!(Record::parse(&written).unwrap(), record);
    let mut v2 = record.clone();
    v2.image_id = Some("sha256:0123".to_owned());
    v2.auto_remove = true;
    v2.ports = vec![BoundPort::new("0.0.0.0".parse().unwrap(), 8080, 80, "tcp")];
    assert_eq!(Record::parse(&v2.to_json_new().unwrap()).unwrap(), v2);
    // A newer one, well formed: refused as newer, not read.
    let newer = literal.replacen("\"version\": 1", "\"version\": 4", 1);
    let err = Record::parse(newer.as_bytes()).unwrap_err();
    assert!(
        err.to_string().contains("a newer Shipyard wrote it"),
        "{err}"
    );
    // Newer, none, or no record at all.
    for bad in [r#"{"version": 4}"#, r#"{"id": "x"}"#, "[]", "{"] {
        let err = Record::parse(bad.as_bytes()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Storage, "{bad}");
    }
    // A field no version 1 record has is a damaged one.
    let extra = literal.replacen("\"labels\"", "\"surprise\": 1, \"labels\"", 1);
    assert!(Record::parse(extra.as_bytes()).is_err());
    let huge = vec![b' '; usize::try_from(record::MAX_SIZE).unwrap() + 1];
    assert_eq!(Record::parse(&huge).unwrap_err().kind(), ErrorKind::Storage);
}

/// A literal version 2 record (9.4a's) reads as written: its image, `--rm`
/// and ports kept, what version 3 added defaulted; and a version 3 record
/// keeps every field it adds.
#[test]
fn version_2_reads_as_written() {
    let literal = r#"{
        "version": 2,
        "id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "serial": 304812,
        "name": "brave_tugboat",
        "created": {"secs_since_epoch": 1760000000, "nanos_since_epoch": 0},
        "config": {
            "interactive": false, "tty": false, "env": [], "workdir": null,
            "user": null, "group_add": [], "ulimit": [], "oom_score_adj": null,
            "device": [], "device_cgroup_rule": [], "stop_signal": "SIGTERM",
            "stop_timeout": 10, "no_isolation": false, "hostname": null,
            "domainname": null, "sysctl": [], "network": ["pasta"],
            "network_alias": [], "ip": null, "ip6": null, "mac_address": null,
            "add_host": [], "dns": [], "dns_search": [], "dns_option": [],
            "publish": [], "publish_all": false, "rootfs": null, "image": "alpine",
            "entrypoint": null, "pull": "missing", "read_only": false,
            "cap_add": [], "cap_drop": [], "security_opt": [],
            "uidmap": [], "gidmap": [], "bundle": null,
            "resources": {"swap_is_default": false, "cpu_max": false, "io_max": [],
                          "unified": {}},
            "volume": [], "command": ["sh"]
        },
        "labels": {},
        "started": {"secs_since_epoch": 1760000100, "nanos_since_epoch": 0},
        "finished": null,
        "exit": null,
        "image_id": "sha256:0123",
        "auto_remove": true,
        "ports": []
    }"#;
    let record = Record::parse(literal.as_bytes()).unwrap();
    assert_eq!(record.image_id.as_deref(), Some("sha256:0123"));
    assert!(record.auto_remove);
    assert!(record.started.is_some());
    assert_eq!(record.config.restart, crate::containers::RestartPolicy::No);
    assert!(!record.stopped_by_user);
    assert_eq!(
        (
            record.restart_count,
            record.boot_id.clone(),
            record.next_start
        ),
        (0, None, None)
    );
    // A field version 3 added, in a version 2 record: a damaged one.
    let extra = literal.replacen("\"labels\"", "\"restart_count\": 1, \"labels\"", 1);
    assert!(Record::parse(extra.as_bytes()).is_err());
    // Version 3 keeps them all.
    let mut v3 = record;
    v3.config.restart = crate::containers::RestartPolicy::OnFailure { max: Some(3) };
    v3.stopped_by_user = true;
    v3.restart_count = 2;
    v3.boot_id = Some("b0b0".to_owned());
    v3.next_start = Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9));
    let written = v3.to_json_new().unwrap();
    assert!(String::from_utf8_lossy(&written).contains("\"restart\": \"on-failure:3\""));
    assert_eq!(Record::parse(&written).unwrap(), v3);
}

/// A version 3 record, as one is written: every field set.
const V3_RECORD: &str = r#"{
        "version": 3,
        "id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "serial": 304812,
        "name": "brave_tugboat",
        "created": {"secs_since_epoch": 1760000000, "nanos_since_epoch": 0},
        "config": {
            "interactive": false, "tty": false, "env": ["A=1"], "workdir": null,
            "user": null, "group_add": [], "ulimit": [], "oom_score_adj": null,
            "device": [], "device_cgroup_rule": [], "stop_signal": "SIGTERM",
            "stop_timeout": 10,
            "restart": "on-failure:5",
            "health": {
                "test": ["CMD-SHELL", "test -e /ready"],
                "interval_ms": 2000, "timeout_ms": 1500, "start_period_ms": 7000,
                "start_interval_ms": 500, "retries": 4,
                "shell": ["/bin/bash", "-c"]
            },
            "no_isolation": false, "hostname": null,
            "domainname": null, "sysctl": [], "network": ["pasta"],
            "network_alias": [], "ip": null, "ip6": null, "mac_address": null,
            "add_host": [], "dns": [], "dns_search": [], "dns_option": [],
            "publish": [{"host_ip": null, "host_port": 8080, "container_port": 80,
                         "protocol": "tcp"}],
            "publish_all": false, "rootfs": null, "image": "alpine",
            "entrypoint": null, "pull": "missing", "read_only": false,
            "cap_add": [], "cap_drop": [], "security_opt": [],
            "uidmap": [], "gidmap": [], "bundle": null,
            "resources": {"memory_max": null, "memory_low": null, "swap_max": null,
                          "swap_is_default": false, "cpu_quota": null,
                          "cpu_period": null, "cpu_max": false, "cpu_weight": null,
                          "cpuset_cpus": null, "cpuset_mems": null, "pids_max": null,
                          "io_weight": null, "io_max": [], "unified": {}},
            "volume": [], "command": ["sh"]
        },
        "labels": {"tier": "web"},
        "started": {"secs_since_epoch": 1760000100, "nanos_since_epoch": 5},
        "finished": {"secs_since_epoch": 1760000200, "nanos_since_epoch": 0},
        "exit": {"code": 1, "oom": null},
        "image_id": "sha256:0123",
        "auto_remove": true,
        "ports": [{"host_ip": "0.0.0.0", "host_port": 8080, "container_port": 80,
                   "protocol": "tcp"}],
        "stopped_by_user": true,
        "restart_count": 2,
        "boot_id": "6f1c1f3e-0d6a-4c1b-9a3e-5a1d2b3c4d5e",
        "next_start": {"secs_since_epoch": 1760000210, "nanos_since_epoch": 0},
        "restart_delay_ms": 400
    }"#;

/// The released format, pinned: a literal version 3 record (what 0.2.0
/// writes), every field version 3 added set to something other than its
/// default, reads as written, and writing it again gives the same JSON.
#[test]
fn version_3_reads_as_written() {
    let literal = V3_RECORD;
    let at = |secs: u64, nanos: u32| SystemTime::UNIX_EPOCH + std::time::Duration::new(secs, nanos);
    let record = Record::parse(literal.as_bytes()).unwrap();
    assert_eq!(
        record.id.to_string(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(record.serial.to_string(), "SHPU 304812 2");
    assert_eq!(record.name.as_str(), "brave_tugboat");
    assert_eq!(record.created, at(1_760_000_000, 0));
    assert_eq!(
        record.labels.as_map().get("tier").map(String::as_str),
        Some("web")
    );
    assert_eq!(record.started, Some(at(1_760_000_100, 5)));
    assert_eq!(record.finished, Some(at(1_760_000_200, 0)));
    assert_eq!(
        record.exit,
        Some(record::Exit {
            code: Some(1),
            oom: None
        })
    );
    assert_eq!(record.image_id.as_deref(), Some("sha256:0123"));
    assert!(record.auto_remove);
    assert_eq!(
        record.ports,
        [BoundPort::new("0.0.0.0".parse().unwrap(), 8080, 80, "tcp")]
    );
    // What version 3 added, each as given.
    assert!(record.stopped_by_user);
    assert_eq!(record.restart_count, 2);
    assert_eq!(
        record.boot_id.as_deref(),
        Some("6f1c1f3e-0d6a-4c1b-9a3e-5a1d2b3c4d5e")
    );
    assert_eq!(record.next_start, Some(at(1_760_000_210, 0)));
    assert_eq!(record.restart_delay_ms, 400);
    assert_eq!(
        record.config.restart,
        crate::containers::RestartPolicy::OnFailure { max: Some(5) }
    );
    assert_eq!(
        record.config.health,
        Some(crate::health::HealthCheck {
            test: vec!["CMD-SHELL".to_owned(), "test -e /ready".to_owned()],
            interval_ms: 2000,
            timeout_ms: 1500,
            start_period_ms: 7000,
            start_interval_ms: 500,
            retries: 4,
            shell: vec!["/bin/bash".to_owned(), "-c".to_owned()],
        })
    );
    // Written again: the same record, field for field.
    let written: serde_json::Value =
        serde_json::from_slice(&record.to_json_new().unwrap()).unwrap();
    let given: serde_json::Value = serde_json::from_str(literal).unwrap();
    assert_eq!(written, given);
    // A version 3 record of 9.5a's builds, before the restart delay, the
    // restart policy and the health check: read with their defaults.
    let mut early: serde_json::Value = given.clone();
    early.as_object_mut().unwrap().remove("restart_delay_ms");
    let config = early["config"].as_object_mut().unwrap();
    config.remove("restart");
    config.remove("health");
    let early = Record::parse(&serde_json::to_vec(&early).unwrap()).unwrap();
    assert_eq!(early.restart_delay_ms, 0);
    assert_eq!(early.config.restart, crate::containers::RestartPolicy::No);
    assert_eq!(early.config.health, None);
    assert_eq!(early.restart_count, 2);
    // A field version 3 doesn't have: a damaged record, not one read.
    let extra = literal.replacen("\"labels\"", "\"surprise\": 1, \"labels\"", 1);
    assert_eq!(
        Record::parse(extra.as_bytes()).unwrap_err().kind(),
        ErrorKind::Storage
    );
}

/// The upgrade rule (F43): written again at the version read, unless what's
/// set needs a newer one; never below what it needs.
#[test]
fn a_record_is_written_again_at_the_version_it_was_read_at() {
    use record::written_version;
    for (read_at, needed, written) in [(1, 3, 3), (2, 3, 3), (3, 3, 3), (4, 3, 4), (3, 4, 4)] {
        assert_eq!(
            written_version(read_at, needed),
            written,
            "{read_at}, {needed}"
        );
    }
}

/// Every rewrite (a stop's mark, a rename, an update…) goes through the
/// store's: a version 2 record comes out at 3, what it needs, with what
/// version 3 added at its defaults; a version 3 one stays 3. (While 3 is the
/// newest, a rewrite that ignored the version it read would pass too: the
/// rule itself is `written_version`'s table, until a version 4.)
#[test]
fn a_rewrite_keeps_the_version_it_needs() {
    let (engine, _, dir) = engine("rewrite-version");
    let made = create_record(&engine, new(Some("kept"))).unwrap();
    let path = dir.join("containers").join(made.id.as_str()).join(RECORD);
    let json =
        || -> serde_json::Value { serde_json::from_slice(&fs::read(&path).unwrap()).unwrap() };
    // As version 2 wrote it: none of what version 3 added, in the config
    // either (its restart policy, its health check).
    let mut v2 = json();
    let object = v2.as_object_mut().unwrap();
    object.insert("version".into(), 2.into());
    for added in [
        "stopped_by_user",
        "restart_count",
        "boot_id",
        "next_start",
        "restart_delay_ms",
    ] {
        object.remove(added);
    }
    let config = object["config"].as_object_mut().unwrap();
    config.remove("restart");
    config.remove("health");
    fs::write(&path, serde_json::to_vec(&v2).unwrap()).unwrap();
    assert_eq!(json()["version"], 2, "the control");
    let read = Record::read(&path).unwrap();
    update(&engine, &made.id, |_| {}).unwrap();
    assert_eq!(json()["version"], 3);
    // What it said is kept; what version 3 added is at its defaults.
    let written = Record::read(&path).unwrap();
    assert_eq!(written, read);
    assert_eq!(written.config.restart, RestartPolicy::No);
    assert!(!written.stopped_by_user && written.boot_id.is_none() && written.next_start.is_none());
    assert_eq!((written.restart_count, written.restart_delay_ms), (0, 0));
    update(&engine, &made.id, |record| record.restart_count = 2).unwrap();
    assert_eq!(json()["version"], 3);
    assert_eq!(Record::read(&path).unwrap().restart_count, 2);
    fs::remove_dir_all(dir).unwrap();
}

/// `inspect`'s JSON, as the CLI prints it, for a record as it's written:
/// pinned (its config's form is the record's too, until stage 11).
#[test]
fn inspect_shows_a_record_as_it_always_has() {
    let record = Record::parse(V3_RECORD.as_bytes()).unwrap();
    let state = view::InspectedState {
        status: "exited",
        running: false,
        pid: None,
        exit_code: Some(1),
        oom: None,
        started_at: record.started.map(crate::events::rfc3339),
        finished_at: record.finished.map(crate::events::rfc3339),
        health: None,
        network_lost: None,
    };
    let shown = view::inspected(record, state, Some("/logs/1".to_owned()));
    let shown = serde_json::to_string_pretty(&[shown]).unwrap();
    assert_eq!(shown, INSPECTED);
}

/// [`inspect_shows_a_record_as_it_always_has`]'s pin.
const INSPECTED: &str = r#"[
  {
    "id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "serial": "SHPU 304812 2",
    "name": "brave_tugboat",
    "created": "2025-10-09T08:53:20.000000000Z",
    "image": "alpine",
    "image_id": "sha256:0123",
    "state": {
      "status": "exited",
      "running": false,
      "pid": null,
      "exit_code": 1,
      "oom": null,
      "started_at": "2025-10-09T08:55:00.000000005Z",
      "finished_at": "2025-10-09T08:56:40.000000000Z",
      "health": null,
      "network_lost": null
    },
    "ports": [
      {
        "host_ip": "0.0.0.0",
        "host_port": 8080,
        "container_port": 80,
        "protocol": "tcp"
      }
    ],
    "labels": {
      "tier": "web"
    },
    "auto_remove": true,
    "restart_count": 2,
    "log_dir": "/logs/1",
    "config": {
      "interactive": false,
      "tty": false,
      "env": [
        "A=1"
      ],
      "workdir": null,
      "user": null,
      "group_add": [],
      "ulimit": [],
      "oom_score_adj": null,
      "device": [],
      "device_cgroup_rule": [],
      "stop_signal": "SIGTERM",
      "stop_timeout": 10,
      "restart": "on-failure:5",
      "health": {
        "test": [
          "CMD-SHELL",
          "test -e /ready"
        ],
        "interval_ms": 2000,
        "timeout_ms": 1500,
        "start_period_ms": 7000,
        "start_interval_ms": 500,
        "retries": 4,
        "shell": [
          "/bin/bash",
          "-c"
        ]
      },
      "no_isolation": false,
      "hostname": null,
      "domainname": null,
      "sysctl": [],
      "network": [
        "pasta"
      ],
      "network_alias": [],
      "ip": null,
      "ip6": null,
      "mac_address": null,
      "add_host": [],
      "dns": [],
      "dns_search": [],
      "dns_option": [],
      "publish": [
        {
          "host_ip": null,
          "host_port": 8080,
          "container_port": 80,
          "protocol": "tcp"
        }
      ],
      "publish_all": false,
      "rootfs": null,
      "image": "alpine",
      "entrypoint": null,
      "pull": "missing",
      "read_only": false,
      "cap_add": [],
      "cap_drop": [],
      "security_opt": [],
      "uidmap": [],
      "gidmap": [],
      "bundle": null,
      "resources": {
        "memory_max": null,
        "memory_low": null,
        "swap_max": null,
        "swap_is_default": false,
        "cpu_quota": null,
        "cpu_period": null,
        "cpu_max": false,
        "cpu_weight": null,
        "cpuset_cpus": null,
        "cpuset_mems": null,
        "pids_max": null,
        "io_weight": null,
        "io_max": [],
        "unified": {}
      },
      "volume": [],
      "command": [
        "sh"
      ]
    }
  }
]"#;

/// The upgrade rule's own line: a rewrite keeps the version it read, though
/// less is needed (here, 2: as a record a newer binary read and wrote would
/// need less than its version). Written at 2, or failing for want of a
/// writer, it didn't.
#[test]
fn a_rewrite_writes_at_the_version_it_read_though_less_is_needed() {
    let (engine, _, dir) = engine("rewrite-read-at");
    let made = create_record(&engine, new(Some("kept"))).unwrap();
    let path = dir.join("containers").join(made.id.as_str()).join(RECORD);
    record::tests_hook::NEEDED.set(Some(2));
    let rewritten = update(&engine, &made.id, |record| record.restart_count = 1);
    record::tests_hook::NEEDED.set(None);
    rewritten.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        (json["version"].as_u64(), json["restart_count"].as_u64()),
        (Some(3), Some(1))
    );
    // More needed than there's a writer for: refused, never written in
    // version 3's shape under another number.
    record::tests_hook::NEEDED.set(Some(4));
    let refused = update(&engine, &made.id, |record| record.restart_count = 2);
    record::tests_hook::NEEDED.set(None);
    assert_eq!(refused.unwrap_err().kind(), ErrorKind::Storage);
    assert_eq!(
        Record::read(&path).unwrap().restart_count,
        1,
        "left as it was"
    );
    fs::remove_dir_all(dir).unwrap();
}
