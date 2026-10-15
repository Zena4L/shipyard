#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::field_reassign_with_default,
    reason = "a panic in a test is a clear failure; requests are built as the CLI builds them"
)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use shipyard_runtime::container::{ExitRecord, Status};
use shipyard_runtime::process::{Rlimit, RlimitKind, RlimitValue, Signal};

use super::*;
use crate::error::Result;
use crate::network::{Joined, NetworkMode};
use crate::report::{Collect, Event, Reporter};
use crate::run::{CapabilityArg, DeviceArg, RunRequest, SecurityOpt};
use crate::{ErrorKind, Paths};

/// An engine on a container store of its own, in a fresh directory.
fn engine(name: &str) -> (Engine, Arc<Collect>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "shipyard-containers-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let collect = Arc::new(Collect::default());
    let engine = Engine::new(
        Paths::from_env().with_containers(dir.join("containers")),
        Arc::clone(&collect) as Arc<dyn Reporter>,
    );
    (engine, collect, dir)
}

fn name() -> Name {
    Name::parse("app").unwrap()
}

fn config() -> Config {
    let mut request = RunRequest::default();
    request.image = Some("alpine".to_owned());
    request.command = vec!["true".into()];
    Config::from_request(&request).unwrap()
}

/// Words for made-up names, from the kernel's random numbers.
fn random_words() -> Result<[u16; 2]> {
    let bytes = crate::random::bytes::<4>()?;
    Ok([
        u16::from_le_bytes([bytes[0], bytes[1]]),
        u16::from_le_bytes([bytes[2], bytes[3]]),
    ])
}

fn new(name: Option<&str>) -> NewContainer {
    NewContainer {
        name: name.map(|name| Name::parse(name).unwrap()),
        config: config(),
        labels: Labels::default(),
        image_id: None,
        auto_remove: false,
    }
}

#[test]
fn a_container_is_made_found_and_removed() {
    let (engine, _, dir) = engine("life");
    let labels = Labels::new(BTreeMap::from([("tier".to_owned(), "web".to_owned())])).unwrap();
    let made = create_record(
        &engine,
        NewContainer {
            labels,
            ..new(Some("web"))
        },
    )
    .unwrap();
    assert_eq!(made.name.as_str(), "web");
    assert_eq!(made.config, config());
    assert_eq!(list(&engine).unwrap(), std::slice::from_ref(&made));
    // By each of its references.
    for reference in [
        made.id.as_str().to_owned(),
        "web".to_owned(),
        made.serial.to_string(),
        format!("shpu{:06}", made.serial.number()),
        made.id.short().to_owned(),
        made.id.short().to_uppercase(),
    ] {
        assert_eq!(find(&engine, &reference).unwrap(), made, "{reference:?}");
    }
    // On disk: the store and the container's directory the caller's alone,
    // its record 0600 (it holds the environment).
    let store = dir.join("containers");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&store), 0o700);
    assert_eq!(mode(&store.join(made.id.as_str())), 0o700);
    assert_eq!(mode(&record_path(&store, &made.id)), 0o600);
    remove_record(&engine, &made.id).unwrap();
    assert!(list(&engine).unwrap().is_empty());
    assert_eq!(
        find(&engine, "web").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        remove_record(&engine, &made.id).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    // Only the lock is left.
    let left: Vec<_> = fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left, [OsString::from(".lock")]);
    fs::remove_dir_all(dir).unwrap();
}

/// An update changes the record in place: what's beside it in the
/// container's directory (its logs) stays, the record stays 0600, and
/// a container that isn't there is `NotFound`.
#[test]
fn an_update_changes_the_record_in_place() {
    let (engine, _, dir) = engine("update");
    let made = create_record(&engine, new(Some("web"))).unwrap();
    let logs = logs_dir(&engine, &made.id).unwrap();
    let log = logs.join(next_logs_name(&logs));
    fs::create_dir_all(&log).unwrap();
    fs::write(log.join("container.log"), "kept\n").unwrap();
    let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let changed = update(&engine, &made.id, |record| record.started = Some(started)).unwrap();
    assert_eq!(changed.started, Some(started));
    let found = find(&engine, "web").unwrap();
    assert_eq!(found, changed, "read back as written");
    assert_eq!(found.name, made.name, "the rest as it was");
    assert_eq!(
        fs::read_to_string(log.join("container.log")).unwrap(),
        "kept\n",
        "the logs beside it are untouched"
    );
    let store = dir.join("containers");
    let mode = fs::metadata(record_path(&store, &made.id))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    remove_record(&engine, &made.id).unwrap();
    assert_eq!(
        update(&engine, &made.id, |_| {}).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    fs::remove_dir_all(dir).unwrap();
}

/// Log directories are numbered from 1, each one past the highest, so they
/// sort by name as the starts came (whatever the clock does).
#[test]
fn log_names_follow_the_starts() {
    let (_, _, dir) = engine("logs");
    let logs = dir.join("logs");
    assert_eq!(next_logs_name(&logs), "0000000001", "none yet");
    fs::create_dir_all(logs.join("0000000001")).unwrap();
    fs::create_dir_all(logs.join("0000000009")).unwrap();
    fs::create_dir_all(logs.join("not-a-start")).unwrap();
    let next = next_logs_name(&logs);
    assert_eq!(next, "0000000010");
    assert!(next.as_str() > "0000000009", "sorts after");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn names_are_unique_and_made_up_when_not_given() {
    let (engine, _, dir) = engine("names");
    let web = create_record(&engine, new(Some("web"))).unwrap();
    let err = create_record(&engine, new(Some("web"))).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Exists);
    assert!(err.to_string().contains(web.id.short()), "{err}");
    let made: Vec<Record> = (0..20)
        .map(|_| create_record(&engine, new(None)).unwrap())
        .collect();
    let names: std::collections::BTreeSet<_> = made.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names.len(), 20);
    for record in &made {
        let (adjective, noun) = record.name.as_str().split_once('_').unwrap();
        assert!(!adjective.is_empty() && !noun.is_empty(), "{}", record.name);
    }
    fs::remove_dir_all(dir).unwrap();
}

/// A record renamed: found by its new name, not its old; another
/// container's name refused (its record unchanged); its own name (checked
/// apart from itself) fine; a name free again once its container has
/// another.
#[test]
fn a_record_is_renamed_to_a_free_name() {
    let (engine, _, dir) = engine("rename");
    let web = create_record(&engine, new(Some("web"))).unwrap();
    let db = create_record(&engine, new(Some("db"))).unwrap();
    let name = |text: &str| Name::parse(text).unwrap();
    let err = rename(&engine, &web.id, &name("db")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Exists);
    assert!(err.to_string().contains(db.id.short()), "{err}");
    assert_eq!(find(&engine, "web").unwrap().id, web.id, "unchanged");
    check_name(&engine, &web.id, &name("web")).unwrap();
    let renamed = rename(&engine, &web.id, &name("site")).unwrap();
    assert_eq!(renamed.name, name("site"));
    assert_eq!(find(&engine, "site").unwrap().id, web.id);
    assert_eq!(
        find(&engine, "web").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    rename(&engine, &db.id, &name("web")).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

/// A serial drawn that's taken is drawn again; none free in the draws is
/// `Busy`.
#[test]
fn serials_are_unique() {
    let (engine, _, dir) = engine("serials");
    let first = Serial::new(42).unwrap();
    let mut always = || Ok(first);
    let one = create_drawing(&engine, new(None), &mut always, &mut random_words).unwrap();
    assert_eq!(one.serial, first);
    let mut draws = [first, first, Serial::new(43).unwrap()].into_iter();
    let mut drawn = || Ok(draws.next().unwrap());
    let two = create_drawing(&engine, new(None), &mut drawn, &mut random_words).unwrap();
    assert_eq!(two.serial.number(), 43);
    let err = create_drawing(&engine, new(None), &mut always, &mut random_words).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Busy);
    fs::remove_dir_all(dir).unwrap();
}

/// A name is looked up before an ID prefix: a container named like the
/// start of another's ID is the one found by it.
#[test]
fn lookup_goes_id_name_serial_prefix() {
    let (engine, _, dir) = engine("lookup");
    let first = create_record(&engine, new(None)).unwrap();
    let named = create_record(&engine, new(Some(first.id.short()))).unwrap();
    assert_eq!(find(&engine, first.id.short()).unwrap(), named);
    assert_eq!(find(&engine, first.id.as_str()).unwrap(), first);
    assert_eq!(find(&engine, &first.id.as_str()[..13]).unwrap(), first);
    // A serial with a wrong check digit is a mistake, not a miss.
    let check = first.serial.check_digit();
    let wrong = format!("SHPU {:06} {}", first.serial.number(), (check + 1) % 10);
    assert_eq!(
        find(&engine, &wrong).unwrap_err().kind(),
        ErrorKind::InvalidRequest
    );
    // A serial no container has, and words no container is.
    let free = (0..1_000_000)
        .map(|n| Serial::new(n).unwrap())
        .find(|s| *s != first.serial && *s != named.serial)
        .unwrap();
    assert_eq!(
        find(&engine, &free.to_string()).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        find(&engine, "nothing").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(find(&engine, "").unwrap_err().kind(), ErrorKind::NotFound);
    fs::remove_dir_all(dir).unwrap();
}

/// A prefix more than one ID starts with finds none, and says which.
#[test]
fn an_ambiguous_prefix_names_the_matches() {
    let (engine, _, dir) = engine("ambiguous");
    // Seventeen IDs: two start with the same hex digit.
    let made: Vec<Record> = (0..17)
        .map(|_| create_record(&engine, new(None)).unwrap())
        .collect();
    let digit = (b'0'..=b'f')
        .map(char::from)
        .find(|digit| {
            made.iter()
                .filter(|r| r.id.as_str().starts_with(*digit))
                .count()
                > 1
        })
        .unwrap();
    let err = find(&engine, &digit.to_string()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidRequest);
    for record in made.iter().filter(|r| r.id.as_str().starts_with(digit)) {
        assert!(err.to_string().contains(record.id.short()), "{err}");
    }
    fs::remove_dir_all(dir).unwrap();
}

/// A record that can't be read (a newer Shipyard's, damaged, not where its
/// ID says) is reported and left out; the rest are listed.
#[test]
fn an_unreadable_record_is_left_out_and_said() {
    let (engine, collect, dir) = engine("unreadable");
    let kept = create_record(&engine, new(Some("kept"))).unwrap();
    let store = dir.join("containers");
    let plant = |name: &str, bytes: &[u8]| {
        fs::create_dir(store.join(name)).unwrap();
        fs::write(store.join(name).join(RECORD), bytes).unwrap();
    };
    let newer = "1".repeat(64);
    plant(&newer, br#"{"version": 4}"#);
    let broken = "2".repeat(64);
    plant(&broken, b"{");
    let moved = "3".repeat(64);
    plant(&moved, &kept.to_json().unwrap());
    assert_eq!(list(&engine).unwrap(), [kept]);
    let warnings: Vec<String> = collect
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Warning(warning) if warning.kind == WarningKind::RecordUnreadable => {
                Some(warning.message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("a newer Shipyard wrote it")),
        "{warnings:?}"
    );
    fs::remove_dir_all(dir).unwrap();
}

/// What a writer killed half way left is swept by the next, and never
/// listed.
#[test]
fn leftovers_are_never_listed_and_are_swept() {
    let (engine, _, dir) = engine("leftovers");
    create_record(&engine, new(None)).unwrap();
    let store = dir.join("containers");
    let half_made = store.join(format!("{NEW}{}", "4".repeat(64)));
    let half_gone = store.join(format!("{GONE}{}", "5".repeat(64)));
    for leftover in [&half_made, &half_gone] {
        fs::create_dir(leftover).unwrap();
        fs::write(leftover.join(RECORD), b"{}").unwrap();
    }
    assert_eq!(list(&engine).unwrap().len(), 1);
    create_record(&engine, new(None)).unwrap();
    assert!(!half_made.exists() && !half_gone.exists());
    fs::remove_dir_all(dir).unwrap();
}

/// A store anyone else can write in, or that's a symlink, is refused.
#[test]
fn a_store_others_could_change_is_refused() {
    let (engine, _, dir) = engine("refused");
    let store = dir.join("containers");
    fs::create_dir_all(&store).unwrap();
    fs::set_permissions(&store, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(list(&engine).unwrap_err().kind(), ErrorKind::Storage);
    fs::remove_dir(&store).unwrap();
    fs::create_dir(dir.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), &store).unwrap();
    assert_eq!(list(&engine).unwrap_err().kind(), ErrorKind::Storage);
    fs::remove_dir_all(dir).unwrap();
}

/// Writers take turns: of sixteen at once with one name, one makes it; of
/// sixteen without, each gets its own name, serial and ID.
#[test]
fn concurrent_creates_keep_names_and_serials_unique() {
    let (engine, _, dir) = engine("concurrent");
    let engine = Arc::new(engine);
    let run = |name: Option<&'static str>| {
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let engine = Arc::clone(&engine);
                std::thread::spawn(move || create_record(&engine, new(name)))
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    };
    let same = run(Some("one"));
    assert_eq!(same.iter().filter(|made| made.is_ok()).count(), 1);
    assert!(
        same.iter()
            .filter_map(|made| made.as_ref().err())
            .all(|err| err.kind() == ErrorKind::Exists)
    );
    let made: Vec<Record> = run(None).into_iter().map(Result::unwrap).collect();
    let unique = |values: Vec<String>| {
        let count = values.len();
        values
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == count
    };
    assert!(unique(made.iter().map(|r| r.name.to_string()).collect()));
    assert!(unique(made.iter().map(|r| r.serial.to_string()).collect()));
    assert!(unique(made.iter().map(|r| r.id.to_string()).collect()));
    assert_eq!(list(&engine).unwrap().len(), 17);
    fs::remove_dir_all(dir).unwrap();
}

/// A reader takes no lock and never sees part of a container: listing
/// while another thread makes and removes them never fails or warns.
#[test]
fn a_reader_never_sees_half_a_container() {
    let (engine, collect, dir) = engine("reader");
    let engine = Arc::new(engine);
    let writer = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            for _ in 0..40 {
                let made = create_record(&engine, new(None)).unwrap();
                remove_record(&engine, &made.id).unwrap();
            }
        })
    };
    while !writer.is_finished() {
        for record in list(&engine).unwrap() {
            assert_eq!(record.config, config());
        }
    }
    writer.join().unwrap();
    assert!(collect.events().is_empty(), "{:?}", collect.events());
    fs::remove_dir_all(dir).unwrap();
}

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
    let written = record.to_json().unwrap();
    assert!(String::from_utf8_lossy(&written).contains("\"version\": 3"));
    assert_eq!(Record::parse(&written).unwrap(), record);
    let mut v2 = record.clone();
    v2.image_id = Some("sha256:0123".to_owned());
    v2.auto_remove = true;
    v2.ports = vec![BoundPort::new("0.0.0.0".parse().unwrap(), 8080, 80, "tcp")];
    assert_eq!(Record::parse(&v2.to_json().unwrap()).unwrap(), v2);
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
    let written = v3.to_json().unwrap();
    assert!(String::from_utf8_lossy(&written).contains("\"restart\": \"on-failure:3\""));
    assert_eq!(Record::parse(&written).unwrap(), v3);
}

/// The released format, pinned: a literal version 3 record (what 0.2.0
/// writes), every field version 3 added set to something other than its
/// default, reads as written, and writing it again gives the same JSON.
#[test]
fn version_3_reads_as_written() {
    let literal = r#"{
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
    let written: serde_json::Value = serde_json::from_slice(&record.to_json().unwrap()).unwrap();
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

/// Restart policies, in Docker's words, both ways; anything else refused.
#[test]
fn restart_policies_read_as_dockers() {
    use crate::containers::RestartPolicy;
    for (text, policy) in [
        ("no", RestartPolicy::No),
        ("always", RestartPolicy::Always),
        ("unless-stopped", RestartPolicy::UnlessStopped),
        ("on-failure", RestartPolicy::OnFailure { max: None }),
        ("on-failure:3", RestartPolicy::OnFailure { max: Some(3) }),
    ] {
        assert_eq!(text.parse::<RestartPolicy>().unwrap(), policy, "{text}");
        assert_eq!(policy.to_string(), text);
    }
    for bad in [
        "",
        "yes",
        "on-failure:",
        "on-failure:0",
        "on-failure:-1",
        "on-failure:x",
        "Always",
    ] {
        let err = bad.parse::<RestartPolicy>().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidRequest, "{bad}");
    }
}

/// Every value of a request is kept, and comes back.
/// A container without an image reads back without a pull policy: the
/// engine refuses `--pull` without an image, and a record's `missing` only
/// means it wasn't given.
#[test]
fn a_request_without_an_image_reads_back_without_a_pull() {
    let mut request = RunRequest::default();
    request.rootfs = Some("/".into());
    request.command = vec!["true".into()];
    let config = Config::from_request(&request).unwrap();
    assert_eq!(config.to_request(&name()).unwrap().pull, None);
}

#[test]
fn a_request_is_kept_whole() {
    let mut request = RunRequest::default();
    request.streams.interactive = true;
    request.streams.tty = true;
    request.env = vec!["A=1".into(), "FROM_CALLER".into(), "UNSET".into()];
    request.caller_env = vec![("FROM_CALLER".into(), "secret".into())];
    request.workdir = Some("/work".into());
    request.user = Some(("app".to_owned(), Some("staff".to_owned())));
    request.group_add = vec!["audio".to_owned()];
    request.ulimit = vec![
        Rlimit::new(
            RlimitKind::Core,
            RlimitValue::Finite(0),
            RlimitValue::Unlimited,
        )
        .unwrap(),
    ];
    request.oom_score_adj = Some(-5);
    request.device = vec![DeviceArg {
        host: "/dev/fuse".into(),
        container: Some("/dev/f".into()),
        access: shipyard_runtime::devices::Access::ALL,
    }];
    request.device_cgroup_rule = vec![
        serde_json::from_str(r#"{"allow":true,"kind":"Char","major":10,"minor":null,"access":3}"#)
            .unwrap(),
    ];
    request.stop_signal = "SIGUSR1".parse().unwrap();
    request.stop_timeout = 3;
    request.hostname = Some("box".to_owned());
    request.domainname = Some("example".to_owned());
    request.sysctl = vec![("net.ipv4.ip_forward".to_owned(), "1".to_owned())];
    request.network = vec![NetworkMode::Bridges(vec![Joined {
        name: "web".to_owned(),
        ip: Some("10.0.0.5".parse().unwrap()),
        ip6: Some("fd00::5".parse().unwrap()),
        mac: Some([2, 0, 0, 0, 0, 0xab]),
        aliases: vec!["db".to_owned()],
    }])];
    request.name = Some("app".to_owned());
    request.network_alias = vec!["alias".to_owned()];
    request.ip = Some("10.0.0.6".parse().unwrap());
    request.ip6 = Some("fd00::6".parse().unwrap());
    request.mac_address = Some([2, 0, 0, 0, 0, 0xcd]);
    request.add_host = vec![shipyard_network::files::HostEntry {
        name: "db".to_owned(),
        ip: "10.0.0.9".parse().unwrap(),
    }];
    request.dns = vec!["1.1.1.1".parse().unwrap()];
    request.dns_search = vec!["example".to_owned()];
    request.dns_option = vec!["ndots:2".to_owned()];
    request.publish = vec![shipyard_network::ports::PortRequest {
        host_ip: Some("127.0.0.1".parse().unwrap()),
        host_port: None,
        container_port: 53,
        protocol: shipyard_network::ports::Protocol::Udp,
    }];
    request.publish_all = true;
    request.image = Some("alpine:3.20".to_owned());
    request.entrypoint = Some("".into());
    request.pull = Some(crate::image::PullPolicy::Never);
    request.read_only = true;
    request.cap_add = vec![CapabilityArg::One("NET_ADMIN".parse().unwrap())];
    request.cap_drop = vec![CapabilityArg::All];
    request.security_opt = vec![
        SecurityOpt::NoNewPrivileges(false),
        SecurityOpt::SeccompProfile("/p.json".into()),
        SecurityOpt::SeccompUnconfined,
    ];
    request.uidmap = vec![shipyard_runtime::isolation::IdMap {
        container: 0,
        host: 1000,
        size: 1,
    }];
    request.gidmap = request.uidmap.clone();
    request.limits.resources.as_mut().unwrap().pids_max = Some(64);
    request.volume = vec!["data:/data:ro".to_owned()];
    request.command = vec!["sh".into(), "-c".into(), "exit 3".into()];
    request.restart = crate::containers::RestartPolicy::OnFailure { max: Some(3) };
    request.health = Some(crate::health::HealthCheck {
        interval_ms: 1500,
        retries: 5,
        ..crate::health::HealthCheck::shell("curl -f localhost")
    });
    let config = Config::from_request(&request).unwrap();
    // The caller's value is kept; an unset one is left out, as a run does.
    assert_eq!(config.env, ["A=1", "FROM_CALLER=secret"]);
    assert_eq!(config.stop_signal, "SIGUSR1");
    assert_eq!(config.mac_address.as_deref(), Some("02:00:00:00:00:cd"));
    let back = config.to_request(&Name::parse("app").unwrap()).unwrap();
    assert!(back.caller_env.is_empty());
    assert_eq!(back.name.as_deref(), Some("app"), "the record's name");
    // The request that comes back is the one asked, field for field, but
    // for what the record resolved: the caller's environment, read into
    // `env`. (Every field is named: a new one must be compared too.)
    let mut expected = request.clone();
    expected.env = vec!["A=1".into(), "FROM_CALLER=secret".into()];
    let RunRequest {
        default_mode: _,
        detached: _,
        container: _,
        caller_env: _,
        streams: _,
        env: _,
        workdir: _,
        user: _,
        group_add: _,
        ulimit: _,
        oom_score_adj: _,
        device: _,
        device_cgroup_rule: _,
        stop_signal: _,
        stop_timeout: _,
        restart: _,
        health: _,
        no_isolation: _,
        hostname: _,
        domainname: _,
        sysctl: _,
        network: _,
        name: _,
        network_alias: _,
        ip: _,
        ip6: _,
        mac_address: _,
        add_host: _,
        dns: _,
        dns_search: _,
        dns_option: _,
        publish: _,
        publish_all: _,
        rootfs: _,
        image: _,
        entrypoint: _,
        pull: _,
        read_only: _,
        cap_add: _,
        cap_drop: _,
        security_opt: _,
        uidmap: _,
        gidmap: _,
        bundle: _,
        limits: _,
        volume: _,
        command: _,
    } = &expected;
    macro_rules! same {
        ($field:ident) => {
            assert_eq!(
                format!("{:?}", back.$field),
                format!("{:?}", expected.$field),
                stringify!($field)
            );
        };
    }
    same!(streams);
    same!(env);
    same!(workdir);
    same!(user);
    same!(group_add);
    same!(ulimit);
    same!(oom_score_adj);
    same!(device);
    same!(device_cgroup_rule);
    same!(stop_signal);
    same!(stop_timeout);
    same!(restart);
    same!(health);
    same!(no_isolation);
    same!(hostname);
    same!(domainname);
    same!(sysctl);
    same!(network);
    same!(name);
    same!(network_alias);
    same!(ip);
    same!(ip6);
    same!(mac_address);
    same!(add_host);
    same!(dns);
    same!(dns_search);
    same!(dns_option);
    same!(publish);
    same!(publish_all);
    same!(rootfs);
    same!(image);
    same!(entrypoint);
    same!(pull);
    same!(read_only);
    same!(cap_add);
    same!(cap_drop);
    same!(security_opt);
    same!(uidmap);
    same!(gidmap);
    same!(bundle);
    same!(limits);
    same!(volume);
    same!(command);
    assert_eq!(Config::from_request(&back).unwrap(), config);
    // And through JSON.
    let json = serde_json::to_string(&config).unwrap();
    assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
    // The bundle and the root filesystem, which can't go with an image.
    let mut bundle = RunRequest::default();
    bundle.bundle = Some("/b".into());
    bundle.rootfs = Some("/r".into());
    let kept = Config::from_request(&bundle).unwrap();
    let back = kept.to_request(&Name::parse("b").unwrap()).unwrap();
    assert_eq!(
        (back.bundle, back.rootfs),
        (Some("/b".into()), Some("/r".into()))
    );
}

/// What a record can't keep is refused: JSON has no bytes.
#[test]
fn a_value_that_isnt_utf8_is_refused() {
    use std::os::unix::ffi::OsStringExt as _;
    let mut request = RunRequest::default();
    request.command = vec![OsString::from_vec(vec![0xff])];
    let err = Config::from_request(&request).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidRequest);
    let mut request = RunRequest::default();
    request.limits.resources = Err("--memory 1x: not a size".to_owned());
    assert_eq!(
        Config::from_request(&request).unwrap_err().kind(),
        ErrorKind::InvalidRequest
    );
}

/// A record's values this Shipyard doesn't know are the record's fault.
#[test]
fn a_config_with_unknown_values_is_storages_error() {
    let mut config = config();
    config.stop_signal = "SIGNOPE".to_owned();
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.cap_add = vec!["CAP_NOPE".to_owned()];
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.mac_address = Some("02:00".to_owned());
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
}

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

/// Once the draws of made-up names are all taken, they're numbered.
#[test]
fn made_up_names_are_numbered_once_the_draws_are_taken() {
    let (engine, _, dir) = engine("suffix");
    let mut same = || Ok([0, 0]);
    let names: Vec<String> = (0..3)
        .map(|_| {
            create_drawing(&engine, new(None), &mut Serial::random, &mut same)
                .unwrap()
                .name
                .to_string()
        })
        .collect();
    assert_eq!(names, ["able_anchor", "able_anchor_2", "able_anchor_3"]);
    fs::remove_dir_all(dir).unwrap();
}

/// A writer reads strictly: a record it can't read may hold the name and
/// serial a new one would get, so nothing is made while it's there.
#[test]
fn an_unreadable_record_stops_a_create() {
    let (engine, _, dir) = engine("strict");
    let kept = create_record(&engine, new(Some("web"))).unwrap();
    let store = dir.join("containers");
    let newer = store.join("9".repeat(64));
    fs::create_dir(&newer).unwrap();
    fs::write(newer.join(RECORD), br#"{"version": 4, "name": "web"}"#).unwrap();
    let err = create_record(&engine, new(Some("web2"))).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Storage);
    assert!(format!("{err:#}").contains(&"9".repeat(64)), "{err:#}");
    // Readers go on, without it.
    assert_eq!(list(&engine).unwrap(), [kept]);
    fs::remove_dir_all(&newer).unwrap();
    assert!(create_record(&engine, new(Some("web2"))).is_ok());
    fs::remove_dir_all(dir).unwrap();
}

/// Two records with one name or serial (made by hand, or by two stores
/// merged): a reference to it finds neither.
#[test]
fn a_name_or_serial_two_records_have_is_ambiguous() {
    let (engine, _, dir) = engine("twice");
    let kept = create_record(&engine, new(Some("web"))).unwrap();
    let mut twin = kept.clone();
    twin.id = ContainerId::parse(&"6".repeat(64)).unwrap();
    let store = dir.join("containers");
    fs::create_dir(store.join(twin.id.as_str())).unwrap();
    fs::write(record_path(&store, &twin.id), twin.to_json().unwrap()).unwrap();
    for reference in ["web".to_owned(), kept.serial.to_string()] {
        let err = find(&engine, &reference).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidRequest, "{reference}");
        assert!(err.to_string().contains(twin.id.short()), "{err}");
    }
    assert_eq!(
        find(&engine, kept.id.as_str()).unwrap(),
        kept,
        "the ID is still one"
    );
    fs::remove_dir_all(dir).unwrap();
}

/// A writer that holds the lock too long makes the next one `Busy`.
#[test]
fn a_lock_held_too_long_is_busy() {
    let (engine, _, dir) = engine("busy");
    create_record(&engine, new(None)).unwrap();
    let store = Store::open(&engine, false).unwrap().unwrap();
    let _held = store.lock(LOCK_WAIT).unwrap();
    let err = store.lock(Duration::from_millis(60)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Busy);
    fs::remove_dir_all(dir).unwrap();
}

/// No store yet is no containers, and listing them doesn't make one; a
/// store that's a file is refused.
#[test]
fn listing_makes_no_store_and_a_file_isnt_one() {
    let (engine, _, dir) = engine("nostore");
    assert!(list(&engine).unwrap().is_empty());
    assert_eq!(
        find(&engine, "web").unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert!(!dir.join("containers").exists());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("containers"), b"").unwrap();
    assert_eq!(list(&engine).unwrap_err().kind(), ErrorKind::Storage);
    assert_eq!(
        create_record(&engine, new(None)).unwrap_err().kind(),
        ErrorKind::Storage
    );
    fs::remove_dir_all(dir).unwrap();
}

/// A bare `--env` name a run would refuse is refused here too, not left
/// out quietly.
#[test]
fn a_bare_env_name_a_run_refuses_is_refused() {
    for bad in ["", "TOKEN secret"] {
        let mut request = RunRequest::default();
        request.env = vec![bad.into()];
        let err = Config::from_request(&request).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidRequest, "{bad:?}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }
}

/// The cgroup limits are the record's own type: every field of the
/// runtime's is kept, and a field the runtime adds fails here, until the
/// record keeps it (and its version says so).
#[test]
fn every_cgroup_limit_is_kept() {
    use shipyard_runtime::resources::{CpuMax, IoMax, Limit, Resources};
    let fields: Vec<String> = serde_json::to_value(Resources::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        fields,
        [
            "cpu_max",
            "cpu_weight",
            "cpuset_cpus",
            "cpuset_mems",
            "io_max",
            "io_weight",
            "memory_low",
            "memory_max",
            "pids_max",
            "swap_is_default",
            "swap_max",
            "unified"
        ],
        "a new limit: keep it in CgroupLimits"
    );
    let io_fields: Vec<String> = serde_json::to_value(IoMax::device(8, 0))
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        io_fields,
        ["major", "minor", "rbps", "riops", "wbps", "wiops"]
    );
    let mut resources = Resources::default();
    resources.memory_max = Some(1 << 30);
    resources.memory_low = Some(1 << 20);
    resources.swap_max = Some(Limit::Max);
    resources.swap_is_default = true;
    resources.cpu_max = Some(CpuMax {
        quota: Some(50_000),
        period: None,
    });
    resources.cpu_weight = Some(200);
    resources.cpuset_cpus = Some("0-1".to_owned());
    resources.cpuset_mems = Some("0".to_owned());
    resources.pids_max = Some(64);
    resources.io_weight = Some(50);
    let mut io = IoMax::device(8, 0);
    io.rbps = Some(1);
    io.wbps = Some(2);
    io.riops = Some(3);
    io.wiops = Some(4);
    resources.io_max = vec![io];
    resources.unified = BTreeMap::from([("memory.high".to_owned(), "1G".to_owned())]);
    let mut request = RunRequest::default();
    request.limits.resources = Ok(resources.clone());
    let config = Config::from_request(&request).unwrap();
    let back = config.to_request(&name()).unwrap();
    assert_eq!(back.limits.resources.unwrap(), resources);
}

/// Values in a record this Shipyard doesn't know: the record's fault.
#[test]
fn unknown_stored_values_are_storages_error() {
    let mut config = config();
    config.pull = "sometimes".to_owned();
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.ulimit = vec![Ulimit {
        kind: "nope".to_owned(),
        soft: None,
        hard: None,
    }];
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.publish = vec![Port {
        host_ip: None,
        host_port: None,
        container_port: 80,
        protocol: "sctp".to_owned(),
    }];
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
}

/// The state, from a runtime state directory: none (never started) is
/// created; one whose init lives is running.
#[cfg(target_os = "linux")]
#[test]
fn the_state_reads_the_runtime_root_paths_give() {
    use std::os::unix::fs::DirBuilderExt as _;
    let (engine, _, dir) = engine("runtime");
    let runtime_root = dir.join("run");
    let engine = Engine::new(
        engine.paths.clone().with_runtime_root(&runtime_root),
        Arc::clone(&engine.reporter),
    );
    let record = create_record(&engine, new(None)).unwrap();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&runtime_root)
        .unwrap();
    assert_eq!(state(&engine, &record).unwrap(), ContainerState::Created);
    let me = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{me}/stat")).unwrap();
    let started: u64 = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let state_dir = runtime_root.join(record.id.as_str());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&state_dir)
        .unwrap();
    fs::write(
        state_dir.join("state.json"),
        format!(r#"{{"bundle":"/b","init":[{me},{started}],"annotations":{{}}}}"#),
    )
    .unwrap();
    assert_eq!(
        state(&engine, &record).unwrap(),
        ContainerState::Running { since: None }
    );
    fs::remove_dir_all(dir).unwrap();
}

/// `network connect`'s record: a network added last; the default network
/// said first if none was; a lone network's addresses kept as its own; one
/// joined already, or past the most, refused; and `disconnect`'s: one left,
/// the last refused, one it isn't on not found.
#[test]
fn a_record_gains_and_loses_bridge_networks() {
    use super::config::{Bridge, Network};
    let names = |config: &Config| -> Vec<String> {
        config
            .network
            .iter()
            .flat_map(|network| match network {
                Network::Bridges(bridges) => {
                    bridges.iter().map(|bridge| bridge.name.clone()).collect()
                }
                _ => vec!["?".to_owned()],
            })
            .collect()
    };
    let mut plain = config();
    plain.network = Vec::new();
    plain.ip = Some("10.88.0.9".parse().unwrap());
    let joined = plain.with_bridge(Bridge::named("app"), "bridge").unwrap();
    assert_eq!(names(&joined), ["bridge", "app"]);
    assert_eq!(joined.ip, None, "its address moved to its network");
    let Network::Bridges(first) = &joined.network[0] else {
        panic!("{:?}", joined.network);
    };
    assert_eq!(first[0].ip, Some("10.88.0.9".parse().unwrap()));
    let twice = joined
        .with_bridge(Bridge::named("app"), "bridge")
        .unwrap_err();
    assert_eq!(twice.kind(), crate::ErrorKind::Exists, "{twice:#}");
    let left = joined.without_bridge("bridge", "bridge").unwrap();
    assert_eq!(names(&left), ["app"]);
    let only = left.without_bridge("app", "bridge").unwrap_err();
    assert_eq!(only.kind(), crate::ErrorKind::InvalidRequest, "{only:#}");
    let not_on = left.without_bridge("db", "bridge").unwrap_err();
    assert_eq!(not_on.kind(), crate::ErrorKind::NotFound, "{not_on:#}");
    assert_eq!(
        plain.without_bridge("bridge", "bridge").unwrap_err().kind(),
        crate::ErrorKind::InvalidRequest,
        "the default, alone"
    );
    // At most 100.
    let mut many = config();
    many.network = vec![Network::Bridges(
        (0..crate::run::MAX_NETWORKS)
            .map(|at| Bridge::named(&format!("n{at}")))
            .collect(),
    )];
    assert!(
        many.with_bridge(Bridge::named("one-more"), "bridge")
            .is_err()
    );
    // Not bridges: refused.
    let mut pasta = config();
    pasta.network = vec![Network::Pasta];
    assert!(pasta.with_bridge(Bridge::named("app"), "bridge").is_err());
    assert!(pasta.without_bridge("app", "bridge").is_err());
}

/// What `network rm` asks of a record: the networks it joins, its default
/// one if it says none, and never one through the host, a loopback or
/// pasta.
#[test]
fn a_record_says_which_networks_it_joins() {
    use super::config::{Bridge, Network};
    let mut plain = config();
    plain.network = Vec::new();
    assert!(plain.joins("bridge", "bridge"), "the default, unsaid");
    assert!(!plain.joins("app", "bridge"));
    let joined = plain.with_bridge(Bridge::named("app"), "bridge").unwrap();
    assert!(joined.joins("app", "bridge"));
    assert!(joined.joins("bridge", "bridge"));
    assert!(!joined.joins("db", "bridge"));
    let left = joined.without_bridge("app", "bridge").unwrap();
    assert!(!left.joins("app", "bridge"), "disconnected");
    for network in [Network::Host, Network::None, Network::Pasta] {
        let mut own = config();
        own.network = vec![network];
        assert!(!own.joins("bridge", "bridge"), "{:?}", own.network);
    }
}
