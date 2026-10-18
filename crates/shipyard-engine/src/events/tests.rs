//! The journal's tests: its lines, appends and rotations, followers,
//! times and filters.

#![allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::*;
use crate::{Engine, ErrorKind};

fn event() -> Event {
    Event::new(Kind::Container, "die", "0123abcd")
        .with("name", "web")
        .with("exitCode", 3)
        .at(SystemTime::UNIX_EPOCH + Duration::new(1_760_000_000, 123_456_789))
}

/// A line round trips, and a pinned version 1 line reads as written.
#[test]
fn a_line_reads_back_and_version_1_is_pinned() {
    let line = event().to_line();
    assert_eq!(Event::from_line(&line), Some(event()));
    let pinned = r#"{"v":1,"time":"2025-10-09T08:53:20.123456789Z","type":"container","action":"die","id":"0123abcd","attributes":{"exitCode":"3","name":"web"}}"#;
    assert_eq!(line, pinned);
    assert_eq!(Event::from_line(pinned), Some(event()));
    // Another version, or not an event: skipped.
    assert_eq!(
        Event::from_line(&pinned.replace(r#""v":1"#, r#""v":2"#)),
        None
    );
    assert_eq!(Event::from_line("{"), None);
    assert_eq!(Event::from_line(r#"{"v":1}"#), None);
}

/// A line never grows past [`MAX_LINE`]: the longest values are cut,
/// and an ID or action past its bound (on a character, in bytes); what's
/// written still reads.
#[test]
fn a_line_is_kept_short() {
    let big = event().with("note", "é".repeat(10_000));
    let line = big.to_line();
    assert!(line.len() < MAX_LINE, "{}", line.len());
    let read = Event::from_line(&line).unwrap();
    assert_eq!(read.attributes["name"], "web");
    assert!(read.attributes["note"].starts_with('é'));
    // Escaped six bytes each, at their bounds: still a line that reads.
    let worst = Event::new(
        Kind::Volume,
        &"\u{1}".repeat(1_000),
        &"é\u{1}".repeat(3_000),
    );
    let line = worst.to_line();
    assert!(line.len() < MAX_LINE, "{}", line.len());
    let read = Event::from_line(&line).unwrap();
    assert!(read.id.len() <= MAX_ID && read.id.starts_with('é'));
    assert_eq!(read.action.len(), MAX_ACTION);
}

/// An engine whose journal is in a new directory of its own.
fn journal(name: &str) -> (Engine, PathBuf) {
    let (engine, _, parent) = crate::testing::engine_on_a_new_store(name);
    let dir = parent.join("events");
    let paths = engine.paths().clone().with_events(&dir);
    (
        Engine::new(paths, std::sync::Arc::clone(engine.reporter())),
        dir,
    )
}

/// From now on, for a while: a follower that misses an event ends,
/// failing its test, rather than waiting for ever.
fn for_a_while() -> EventsOptions {
    let now = SystemTime::now();
    EventsOptions {
        since: Some(now),
        until: Some(now + Duration::from_secs(30)),
        ..EventsOptions::default()
    }
}

/// Every event `engine`'s journal has, oldest first.
fn everything(engine: &Engine) -> Vec<Event> {
    let options = EventsOptions {
        since: Some(SystemTime::UNIX_EPOCH),
        until: Some(SystemTime::now()),
        ..EventsOptions::default()
    };
    let mut read = Vec::new();
    super::read(engine, &options, |event| {
        read.push(event);
        Ok(())
    })
    .unwrap();
    read
}

/// Many processes' appends at once (threads here: the same `write`s)
/// give whole lines, each once, across a rotation; the journal is
/// private.
#[test]
fn concurrent_appends_stay_whole_across_a_rotation() {
    use std::os::unix::fs::PermissionsExt as _;
    let (engine, dir) = journal("events-concurrent");
    let note = "x".repeat(100);
    std::thread::scope(|scope| {
        for thread in 0..16 {
            let (engine, note) = (&engine, &note);
            scope.spawn(move || {
                for n in 0..500 {
                    let event = Event::new(Kind::Container, "start", &format!("{thread}"))
                        .with("n", n)
                        .with("note", note);
                    emit(engine, &event);
                }
            });
        }
    });
    assert!(dir.join(ROTATED).exists(), "rotated past {ROTATE_AT}");
    let read = everything(&engine);
    assert_eq!(read.len(), 16 * 500);
    for thread in 0..16 {
        let ns: Vec<String> = read
            .iter()
            .filter(|event| event.id == thread.to_string())
            .map(|event| event.attributes["n"].clone())
            .collect();
        let wanted: Vec<String> = (0..500).map(|n: i32| n.to_string()).collect();
        assert_eq!(ns, wanted, "thread {thread}'s events, in its order");
    }
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(ROTATED)), 0o600);
    assert_eq!(mode(&dir.join(FILE)), 0o600);
}

/// A follower started before a rotation reads every event after it
/// started, the old file's to its end and the new one's, while several
/// processes (threads here) write at once.
#[test]
fn a_follower_reads_across_a_rotation() {
    let (engine, dir) = journal("events-follow");
    emit(&engine, &Event::new(Kind::Image, "pull", "before"));
    let (writers, each) = (8, 1_000);
    let note = "y".repeat(100);
    let read = std::thread::scope(|scope| {
        let follower = scope.spawn(|| {
            let mut read = Vec::new();
            let done = super::read(&engine, &for_a_while(), |event| {
                read.push(event.id);
                if read.len() == writers * each {
                    // Ends it, as a closed pipe does.
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                Ok(())
            });
            assert!(done.is_err(), "it ended before reading them all");
            read
        });
        // Begun: the follower has opened the file at its end.
        std::thread::sleep(POLL * 2);
        for writer in 0..writers {
            let (engine, note) = (&engine, &note);
            scope.spawn(move || {
                for n in 0..each {
                    let id = format!("{writer}-{n}");
                    emit(
                        engine,
                        &Event::new(Kind::Image, "pull", &id).with("note", note),
                    );
                }
            });
        }
        follower.join().unwrap()
    });
    assert!(dir.join(ROTATED).exists());
    for writer in 0..writers {
        let mine: Vec<&String> = read
            .iter()
            .filter(|id| id.starts_with(&format!("{writer}-")))
            .collect();
        let wanted: Vec<String> = (0..each).map(|n| format!("{writer}-{n}")).collect();
        assert_eq!(mine, wanted.iter().collect::<Vec<_>>(), "writer {writer}");
    }
}

/// Two rotations between a follower's looks: the file in the middle
/// (rotated in, then out) is read too, in its place.
#[test]
fn a_follower_reads_a_file_rotated_twice_between_looks() {
    let (engine, dir) = journal("events-twice");
    emit(&engine, &Event::new(Kind::Volume, "create", "before"));
    let line = |id: &str| format!("{}\n", Event::new(Kind::Volume, "create", id).to_line());
    let (seen, follows) = std::sync::mpsc::channel();
    let read = std::thread::scope(|scope| {
        let engine = &engine;
        let follower = scope.spawn(move || {
            let mut read = Vec::new();
            let _ = super::read(engine, &for_a_while(), |event| {
                // Its first `ready` says it follows; the rest are the
                // test's.
                if event.id == "ready" {
                    let _ = seen.send(());
                    return Ok(());
                }
                read.push(event.id);
                if read.len() == 3 {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                Ok(())
            });
            read
        });
        // Begun: `ready` events until the follower has read one (it
        // reads new ones, so it has the file open at its end by then).
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            emit(engine, &Event::new(Kind::Volume, "create", "ready"));
            if follows.recv_timeout(POLL).is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never followed");
        }
        // As appenders and two rotations would leave it, at once.
        let append = |text: String| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(dir.join(FILE))
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        append(line("first"));
        std::fs::rename(dir.join(FILE), dir.join(ROTATED)).unwrap();
        append(line("middle"));
        std::fs::rename(dir.join(FILE), dir.join(ROTATED)).unwrap();
        append(line("last"));
        follower.join().unwrap()
    });
    assert_eq!(read, ["first", "middle", "last"]);
}

/// `since` and `until` bound it by each event's own time (a die is
/// written with its exit's time, which may be before what's ahead of it
/// in the file); filters narrow it.
#[test]
fn reading_bounds_by_the_events_own_times() {
    let (engine, _) = journal("events-times");
    let at = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    for (id, secs) in [("a", 100), ("c", 300), ("b", 200)] {
        emit(
            &engine,
            &Event::new(Kind::Volume, "create", id).at(at(secs)),
        );
    }
    emit(
        &engine,
        &Event::new(Kind::Network, "create", "n").at(at(250)),
    );
    let mut read = Vec::new();
    let options = EventsOptions {
        since: Some(at(150)),
        until: Some(at(300)),
        filters: Filters::parse(&["type=volume".to_owned()]).unwrap(),
        ..EventsOptions::default()
    };
    super::read(&engine, &options, |event| {
        read.push(event.id);
        Ok(())
    })
    .unwrap();
    assert_eq!(read, ["c", "b"], "in the journal's order");
    // `until` alone: from the start (Docker's); before `since`: refused.
    let mut read = Vec::new();
    let options = EventsOptions {
        until: Some(at(150)),
        ..EventsOptions::default()
    };
    super::read(&engine, &options, |event| {
        read.push(event.id);
        Ok(())
    })
    .unwrap();
    assert_eq!(read, ["a"]);
    let backwards = EventsOptions {
        since: Some(at(300)),
        until: Some(at(100)),
        ..EventsOptions::default()
    };
    let error = super::read(&engine, &backwards, |_| Ok(())).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidRequest);
}

/// Budget (H7): an event adds well under a millisecond to the command
/// that writes it (tens of microseconds; the best of many short rounds,
/// so a machine busy with other work still has a quiet one).
#[test]
fn an_emit_is_cheap() {
    let (engine, _) = journal("events-budget");
    let event = Event::new(Kind::Container, "start", "0123abcd").with("name", "web");
    let best = (0..20)
        .map(|_| {
            let started = std::time::Instant::now();
            for _ in 0..25 {
                emit(&engine, &event);
            }
            started.elapsed() / 25
        })
        .min()
        .unwrap();
    assert!(best < Duration::from_millis(1), "{best:?} an event");
}

/// Filters as Docker's: each key's values any, every key given.
#[test]
fn filters_narrow_as_dockers() {
    let given = |filters: &[&str]| {
        Filters::parse(&filters.iter().map(|&f| f.to_owned()).collect::<Vec<_>>()).unwrap()
    };
    let die = event().with("image", "alpine");
    assert!(given(&[]).pass(&die));
    assert!(given(&["type=container", "event=die"]).pass(&die));
    assert!(given(&["event=start", "event=die"]).pass(&die));
    assert!(!given(&["event=start"]).pass(&die));
    assert!(given(&["container=web"]).pass(&die));
    assert!(given(&["container=0123"]).pass(&die));
    assert!(!given(&["container=db"]).pass(&die));
    assert!(given(&["image=alpine"]).pass(&die));
    assert!(!given(&["type=image"]).pass(&die));
    assert!(!given(&["volume=data"]).pass(&die));
    // Only a container's ID by its start: other names whole.
    let volume = Event::new(Kind::Volume, "create", "one");
    assert!(given(&["volume=one"]).pass(&volume));
    assert!(!given(&["volume=on"]).pass(&volume));
    assert!(!given(&["image=alp"]).pass(&die));
    // An image however its reference is written.
    let pull = Event::new(Kind::Image, "pull", "docker.io/library/alpine:latest");
    assert!(given(&["image=alpine"]).pass(&pull));
    assert!(given(&["image=alpine:latest"]).pass(&pull));
    assert!(!given(&["image=alpine:3.20"]).pass(&pull));
    assert!(given(&["image=docker.io/library/alpine"]).pass(&die));
    // A network's connect is the container's too, by its ID or a
    // start of it, and the network's by its name.
    let connect = Event::new(Kind::Network, "connect", "app")
        .with("name", "app")
        .with("container", "0123456789ab");
    assert!(given(&["container=0123"]).pass(&connect));
    assert!(given(&["network=app"]).pass(&connect));
    assert!(!given(&["container=db"]).pass(&connect));
    assert!(!given(&["container=0123", "network=other"]).pass(&connect));
    for bad in [["nope=1"], ["type=thing"], ["noequals"], ["volume="]] {
        let error = Filters::parse(&bad.map(str::to_owned)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{bad:?}");
    }
}
