//! Making, updating and listing records: the store's files, its locks and
//! what it refuses.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::SystemTime;

use super::*;
use crate::ErrorKind;
use crate::report::{Message, WarningKind};

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
    plant(&moved, &kept.to_json_new().unwrap());
    assert_eq!(list(&engine).unwrap(), [kept]);
    let warnings: Vec<String> = collect
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Message::Warning(warning) if warning.kind == WarningKind::RecordUnreadable => {
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

/// A record larger than any Shipyard writes isn't read at all: refused as
/// such, a storage failure, as a record that doesn't parse is.
#[test]
fn a_record_over_its_bound_is_refused_unread() {
    let (engine, _, dir) = engine("oversize");
    let kept = create_record(&engine, new(Some("kept"))).unwrap();
    let path = dir.join("containers").join(kept.id.as_str()).join(RECORD);
    // The control: the record as written reads.
    assert_eq!(Record::read(&path).unwrap(), kept);
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(record::MAX_SIZE + 1).unwrap();
    let err = Record::read(&path).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Storage);
    assert!(
        format!("{err:#}").contains("a container's record of more than 1048576 bytes"),
        "{err}"
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
