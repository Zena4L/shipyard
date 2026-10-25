//! What only runs on Linux: the runtime's state directory, the locks a
//! container is read again under, its `running` file and a start's undo.

use std::fs;
use std::time::SystemTime;

use super::*;
use crate::ErrorKind;

/// The state, from a runtime state directory: none (never started) is
/// created; one whose init lives is running.
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
    let started = shipyard_sys::proc::stat(i32::try_from(me).unwrap())
        .unwrap()
        .start;
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

/// "Find, then lock, then read again": what's read under the lock is the
/// record as it is then, by its ID (renamed meanwhile: its new name), and
/// one removed meanwhile is gone (step 10.1b).
#[test]
fn a_container_locked_again_is_read_again_by_its_id() {
    let (engine, _, dir) = engine("again");
    create_record(&engine, new(Some("web"))).unwrap();
    let found = find(&engine, "web").unwrap();
    // Renamed between the find and the lock.
    rename(&engine, &found.id, &Name::parse("site").unwrap()).unwrap();
    let locked = lock_again(&engine, &found.id, Duration::ZERO).unwrap();
    assert_eq!(locked.record.name.as_str(), "site");
    assert_eq!(locked.record.id, found.id);
    // Held: another waits, then is busy.
    let err = lock_found(&engine, "site", Duration::from_millis(60))
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::Busy);
    drop(locked);
    assert_eq!(
        lock_found(&engine, "site", Duration::ZERO)
            .unwrap()
            .record
            .id,
        found.id
    );
    // Changed while it waits for the lock: what it reads is the change, read
    // under the lock. (The pause only gives a read that came before the
    // lock, the mistake this catches, time to happen first; the right code
    // passes whatever the timing.)
    let held = operation_lock(&engine, &found.id, Duration::ZERO).unwrap();
    let waited = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| lock_again(&engine, &found.id, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(200));
        rename(&engine, &found.id, &Name::parse("later").unwrap()).unwrap();
        drop(held);
        waiting.join().unwrap()
    });
    assert_eq!(waited.unwrap().record.name.as_str(), "later");
    remove_record(&engine, &found.id).unwrap();
    let err = lock_again(&engine, &found.id, Duration::ZERO)
        .err()
        .unwrap();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    fs::remove_dir_all(dir).unwrap();
}

/// A settled end is read once the `running` file is free, not before
/// (held: `None` once the wait is over); a container removed has none
/// (step 10.1b).
#[test]
fn a_settled_end_waits_for_the_running_file() {
    let (engine, _, dir) = engine("settled");
    let made = create_record(&engine, new(None)).unwrap();
    let held = running_lock(&engine, &made.id, Duration::ZERO)
        .unwrap()
        .unwrap();
    assert_eq!(
        settled(&engine, &made.id, Duration::from_millis(60)).unwrap(),
        None
    );
    drop(held);
    let ended = update(&engine, &made.id, |record| {
        record.exit = Some(Exit {
            code: Some(4),
            oom: None,
        });
    })
    .unwrap();
    assert_eq!(
        settled(&engine, &made.id, Duration::ZERO).unwrap(),
        Some(ended)
    );
    remove_record(&engine, &made.id).unwrap();
    let err = settled(&engine, &made.id, Duration::ZERO).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    fs::remove_dir_all(dir).unwrap();
}

/// What a start rewrites before the runtime starts it, kept in
/// `previous-start.json` (v2), is all put back when that start doesn't
/// happen (F55): a user's stop, the restart count and backoff, the next
/// start and the ports, with its start, end, exit and boot.
#[test]
fn a_previous_start_puts_back_all_a_start_rewrote() {
    let (engine, _, dir) = engine("previous-start-v2");
    let made = create_record(&engine, new(Some("kept-stopped"))).unwrap();
    let port = BoundPort::new("0.0.0.0".parse().unwrap(), 8080, 80, "tcp");
    let before = update(&engine, &made.id, |record| {
        record.stopped_by_user = true;
        record.restart_count = 3;
        record.restart_delay_ms = 400;
        record.next_start = Some(SystemTime::UNIX_EPOCH);
        record.ports = vec![port.clone()];
        record.finished = Some(SystemTime::UNIX_EPOCH);
    })
    .unwrap();
    keep_previous_start(&engine, &made.id, &PreviousStart::of(&before)).unwrap();
    // What a start rewrites, as `connect_and_start` does.
    let rewritten = update(&engine, &made.id, |record| {
        record.started = Some(SystemTime::now());
        record.finished = None;
        record.stopped_by_user = false;
        record.restart_count = 0;
        record.restart_delay_ms = 0;
        record.next_start = None;
        record.ports = Vec::new();
    })
    .unwrap();
    assert_ne!(rewritten, before, "the test shows nothing otherwise");
    let previous = previous_start(&engine, &made.id).start().unwrap();
    let mut restored = rewritten.clone();
    previous.restore(&mut restored);
    assert_eq!(restored, before);
    // A user's stop since the start (of a dead start): wins, with no next
    // start, the rest put back.
    let stopped_since = update(&engine, &made.id, |record| {
        record.stopped_by_user = true;
        record.next_start = None;
    })
    .unwrap();
    let mut kept_stop = stopped_since.clone();
    previous_start(&engine, &made.id)
        .start()
        .unwrap()
        .restore(&mut kept_stop);
    assert!(kept_stop.stopped_by_user, "a stop since wins");
    assert_eq!(kept_stop.next_start, None);
    assert_eq!(
        (kept_stop.restart_count, kept_stop.finished),
        (before.restart_count, before.finished)
    );
    // As many ports as a record can hold (a published range): kept, and
    // read again (its bound is the record's).
    let many: Vec<BoundPort> = (1..=5000)
        .map(|n| BoundPort::new("0.0.0.0".parse().unwrap(), n, n, "tcp"))
        .collect();
    let crowded = update(&engine, &made.id, |record| record.ports.clone_from(&many)).unwrap();
    keep_previous_start(&engine, &made.id, &PreviousStart::of(&crowded)).unwrap();
    assert!(
        fs::metadata(
            dir.join("containers")
                .join(made.id.as_str())
                .join("previous-start.json")
        )
        .unwrap()
        .len()
            > 64 * 1024,
        "past the bound it had: the test shows nothing otherwise"
    );
    let mut back = rewritten.clone();
    previous_start(&engine, &made.id)
        .start()
        .unwrap()
        .restore(&mut back);
    assert_eq!(back.ports, many);
    fs::remove_dir_all(dir).unwrap();
}

/// A v1 `previous-start.json` (what Shipyard 0.2 wrote mid-start) is still
/// read: its four fields put back, the rest left as the record has them.
#[test]
fn a_version_1_previous_start_is_read() {
    let (engine, _, dir) = engine("previous-start-v1");
    let made = create_record(&engine, new(Some("from-v1"))).unwrap();
    let record = update(&engine, &made.id, |record| {
        record.started = Some(SystemTime::now());
        record.restart_count = 2;
    })
    .unwrap();
    fs::write(
        dir.join("containers")
            .join(made.id.as_str())
            .join("previous-start.json"),
        r#"{"version":1,"started":null,"finished":null,"exit":null,"boot_id":null}"#,
    )
    .unwrap();
    let previous = previous_start(&engine, &made.id)
        .start()
        .expect("a v1 file read");
    let mut restored = record.clone();
    previous.restore(&mut restored);
    assert_eq!(restored.started, None, "v1's fields put back");
    assert_eq!(restored.restart_count, 2, "the rest as the record has it");
    // A newer version than this Shipyard knows is passed over.
    fs::write(
        dir.join("containers")
            .join(made.id.as_str())
            .join("previous-start.json"),
        r#"{"version":3,"started":null,"finished":null,"exit":null,"boot_id":null}"#,
    )
    .unwrap();
    // A newer version than this Shipyard knows is known as one (H2), never
    // read as what it says.
    let kept = previous_start(&engine, &made.id);
    assert!(matches!(kept, Kept::Newer(3)), "{kept:?}");
    fs::remove_dir_all(dir).unwrap();
}

/// `running` is held as a `HeldFile` (F66): let go when it drops, even
/// while another copy of it is open (a child that hasn't exec'd yet);
/// given up (its copy handed to a monitor), kept by that copy.
#[test]
fn running_is_let_go_at_once_unless_given_up() {
    let (engine, _, dir) = engine("running-held");
    let made = create_record(&engine, new(Some("held"))).unwrap();
    let path = dir.join("containers").join(made.id.as_str()).join(RUNNING);
    let held = locks::running_lock(&engine, &made.id, std::time::Duration::ZERO)
        .unwrap()
        .expect("free");
    // A copy, as a child before its exec has one.
    let copy = held.hold().try_clone_to_owned().unwrap();
    drop(held);
    let other = fs::File::open(&path).unwrap();
    assert!(other.try_lock().is_ok(), "let go at once, its copy open");
    drop(other);
    let mut given = locks::running_lock(&engine, &made.id, std::time::Duration::ZERO)
        .unwrap()
        .expect("free");
    let monitors = given.hold().try_clone_to_owned().unwrap();
    given.give_up();
    drop(given);
    let other = fs::File::open(&path).unwrap();
    assert!(
        other.try_lock().is_err(),
        "given up: the monitor's copy keeps it"
    );
    drop((monitors, copy));
    fs::remove_dir_all(dir).unwrap();
}

/// A start that never ran is settled only in a record without an end
/// (F110): one whose start's own undoing already put its last end back is
/// left as it is (its start, end and exit kept, so a boot still brings it
/// back); one without, settled as never started (the control). What it
/// kept aside goes, unless it's a newer Shipyard's: never read (the record
/// says never started, which is true of this start), kept, and said.
#[test]
fn a_never_run_start_is_settled_once() {
    let (engine, collect, dir) = engine("never-ran");
    let made = create_record(&engine, new(Some("ran-before"))).unwrap();
    let kept_file = dir
        .join("containers")
        .join(made.id.as_str())
        .join("previous-start.json");
    let ended = update(&engine, &made.id, |record| {
        record.started = Some(SystemTime::now());
        record.finished = Some(SystemTime::now());
    })
    .unwrap();
    assert_eq!(
        settle_never_ran(&engine, &made.id).unwrap(),
        ended,
        "its end, put back, kept"
    );
    // Without an end, and a newer Shipyard's file kept aside.
    update(&engine, &made.id, |record| record.finished = None).unwrap();
    let newer = r#"{"version":3,"started":null,"finished":null,"exit":null,"boot_id":null}"#;
    fs::write(&kept_file, newer).unwrap();
    let settled = settle_never_ran(&engine, &made.id).unwrap();
    assert_eq!(
        (settled.started, settled.finished),
        (None, None),
        "never started"
    );
    assert!(kept_file.exists(), "a newer Shipyard's file, kept");
    assert!(
        collect.warnings().iter().any(|warning| warning.kind
            == crate::report::WarningKind::LeftForLater
            && warning.message.contains("previous-start.json is version 3")),
        "{:?}",
        collect.warnings()
    );
    // The control: one this Shipyard reads is let go of once used.
    update(&engine, &made.id, |record| {
        record.started = Some(SystemTime::now());
    })
    .unwrap();
    keep_previous_start(&engine, &made.id, &PreviousStart::of(&ended)).unwrap();
    assert_eq!(
        settle_never_ran(&engine, &made.id).unwrap().finished,
        ended.finished
    );
    assert!(!kept_file.exists(), "used, and let go of");
    fs::remove_dir_all(dir).unwrap();
}
