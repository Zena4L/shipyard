//! `shipyard events` end to end (stage 9, step 9.6a): the journal the
//! commands write, read back as Docker prints it or as JSON lines, past
//! ones with `--since`, new ones as they come, narrowed by `--filter`.
//!
//! Volumes give the events (they need no image or Linux): each test has a
//! store and a journal of its own.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::io::BufRead as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use shipyard_test_support::Shipyard;

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// A test's stores, removed at its end.
struct Stores(PathBuf);

impl Stores {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-events-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("containers")).unwrap();
        Self(dir)
    }

    /// `shipyard ARGS` on these stores.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(SHIPYARD.path());
        command
            .args(args)
            .env("SHIPYARD_IMAGE_STORE", self.0.join("images"))
            .env("SHIPYARD_CONTAINER_DIR", self.0.join("containers"))
            .env("SHIPYARD_EVENTS_DIR", self.0.join("events"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running shipyard")
    }

    /// `shipyard ARGS`, which must succeed: its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Stores {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A reader that has gone (`shipyard volume ls | head -0`) ends every
/// command quietly, as a success (stage 9, step 9.9a, D4): `events` (its
/// own case before) and any other, here `volume ls`, which said "Broken
/// pipe" with 125.
#[test]
fn a_reader_gone_ends_any_command_quietly() {
    let stores = Stores::new("gone");
    stores.ok(&["volume", "create", "data"]);
    // The control: read, it prints.
    assert!(stores.ok(&["volume", "ls"]).contains("data"));
    // `events` with a format too: a backlog written to no reader (each
    // write fails at once, before any wait).
    for args in [
        &["volume", "ls"][..],
        &["events", "--until", "0s"],
        &["events", "--until", "0s", "--format", "json"],
        &["events", "--until", "0s", "--format", "{{.Type}}"],
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let output = stores
            .command(args)
            .stdout(writer)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    }
}

/// A command that acts per item (`volume rm a b`, as `rm`, `stop`, `rmi`
/// …) stops at its reader gone, quietly, as a success (stage 9, step 9.9c):
/// it said "Broken pipe" with 125.
#[test]
fn a_reader_gone_ends_a_command_per_item_quietly() {
    let stores = Stores::new("gone-each");
    // The control: read, it prints both.
    stores.ok(&["volume", "create", "a"]);
    stores.ok(&["volume", "create", "b"]);
    assert_eq!(stores.ok(&["volume", "rm", "a", "b"]), "a\nb\n");
    stores.ok(&["volume", "create", "a"]);
    stores.ok(&["volume", "create", "b"]);
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = stores
        .command(&["volume", "rm", "a", "b"])
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

/// The past, with `--since` and a `--until` gone by: printed, then the
/// command ends; Docker's event messages (`json`), a template over them,
/// or Docker's line.
#[test]
fn past_events_are_listed_and_it_ends() {
    let stores = Stores::new("past");
    stores.ok(&["volume", "create", "one"]);
    stores.ok(&["volume", "create", "two"]);
    stores.ok(&["volume", "rm", "one"]);
    let json = stores.ok(&[
        "events", "--since", "1h", "--until", "0s", "--format", "json",
    ]);
    let said: Vec<(String, String)> = json
        .lines()
        .map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(event["Type"], "volume", "{line}");
            assert_eq!(event["scope"], "local", "{line}");
            (
                event["Action"].as_str().unwrap().to_owned(),
                event["Actor"]["ID"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let said: Vec<(&str, &str)> = said.iter().map(|(a, i)| (a.as_str(), i.as_str())).collect();
    assert_eq!(
        said,
        [("create", "one"), ("create", "two"), ("destroy", "one")]
    );
    // `--until` alone: from the start, as Docker's.
    assert_eq!(
        stores.ok(&["events", "--until", "0s", "--format", "json"]),
        json
    );
    // A template over the same messages.
    assert_eq!(
        stores.ok(&[
            "events",
            "--until",
            "0s",
            "--format",
            "{{.Type}} {{.Action}} {{.Actor.ID}}",
        ]),
        "volume create one\nvolume create two\nvolume destroy one\n"
    );
    // Go's field names (`.Time`, `.Scope`, `.Status`), each there; an
    // attribute an event hasn't is `<no value>`, never the end (Docker's).
    let go = stores.ok(&[
        "events",
        "--until",
        "0s",
        "--format",
        "{{.Scope}}|{{.Status}}|{{.Actor.Attributes.exitCode}}|{{gt .Time 0}}",
    ]);
    assert_eq!(go, "local||<no value>|true\n".repeat(3), "{go}");
    // Backwards: refused.
    let backwards = stores.run(&["events", "--since", "0s", "--until", "1h"]);
    assert_eq!(backwards.status.code(), Some(1), "{backwards:?}");
    let text = stores.ok(&["events", "--since", "1h", "--until", "0s"]);
    let first = text.lines().next().unwrap();
    let words: Vec<&str> = first.split(' ').collect();
    assert!(words[0].ends_with('Z') && words[0].contains('T'), "{first}");
    assert_eq!(words[1..], ["volume", "create", "one"], "{first}");
    // Since later than all of it: nothing.
    assert_eq!(stores.ok(&["events", "--since", "0s", "--until", "0s"]), "");
}

/// `--filter` narrows it; a key it doesn't know is refused (1).
#[test]
fn filters_narrow_it() {
    let stores = Stores::new("filter");
    stores.ok(&["volume", "create", "one"]);
    stores.ok(&["volume", "create", "two"]);
    stores.ok(&["volume", "rm", "two"]);
    let only = |filters: &[&str]| -> Vec<String> {
        let mut args = vec!["events", "--since", "1h", "--until", "0s"];
        for filter in filters {
            args.extend(["--filter", filter]);
        }
        stores
            .ok(&args)
            .lines()
            .map(|line| line.split(' ').skip(2).collect::<Vec<_>>().join(" "))
            .collect()
    };
    assert_eq!(only(&["volume=two"]), ["create two", "destroy two"]);
    assert_eq!(only(&["event=destroy"]), ["destroy two"]);
    assert_eq!(only(&["type=image"]), Vec::<String>::new());
    assert_eq!(
        only(&["event=create", "event=destroy", "volume=one"]),
        ["create one"]
    );
    let refused = stores.run(&[
        "events",
        "--since",
        "1h",
        "--until",
        "0s",
        "-f",
        "colour=red",
    ]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
}

/// Without `--until`, it waits: a new event is printed within a second of
/// it, and a reader that goes (`events | head -n1`) ends it, at once, even
/// if nothing else happens.
#[test]
fn new_events_are_followed() {
    /// The follower, killed if the test ends before it does (a failed
    /// assertion never leaves it, or its reader, behind).
    struct Follower(std::process::Child);
    impl Drop for Follower {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stores = Stores::new("follow");
    stores.ok(&["volume", "create", "before"]);
    let mut events = Follower(
        stores
            .command(&["events"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // Its lines, read on a thread (each wait below has a deadline), which
    // stops reading, its end of the pipe closed, once `after` has come.
    let stdout = events.0.stdout.take().unwrap();
    let (sent, lines) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let last = line.ends_with(" volume create after");
            if sent.send(line).is_err() || last {
                return;
            }
        }
    });
    // Begun (it opens the journal at its end, so it misses what came
    // before): `ready-N` volumes until it prints one.
    let deadline = Instant::now() + Duration::from_secs(20);
    for n in 0.. {
        stores.ok(&["volume", "create", &format!("ready-{n}")]);
        if lines.recv_timeout(Duration::from_millis(300)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "it never followed");
    }
    let made = Instant::now();
    stores.ok(&["volume", "create", "after"]);
    let line = loop {
        let line = lines
            .recv_timeout(Duration::from_secs(5))
            .expect("the new event, printed");
        if !line.contains(" volume create ready-") {
            break line;
        }
    };
    assert!(
        made.elapsed() < Duration::from_secs(1),
        "{:?}",
        made.elapsed()
    );
    assert!(line.ends_with(" volume create after"), "{line}");
    // Its reader gone: it ends, quietly, with no other event to write
    // (between its looks, it asks whether its reader is there).
    reader.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = events.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "it never ended");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut events.0.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(status.success(), "{status:?}: {stderr}");
    assert_eq!(stderr, "");
}

/// Budget (H7): `events --since 1h` over 50,000 events (5.5 MB, more than
/// the journal ever keeps: written here directly) in under a second, the
/// best of a few. Linear, about 0.3 s in a debug build; reading that cut
/// each line off the front of its buffer (moving the rest each time) was
/// quadratic: 25 times 10,000 lines' cost, over 2 s.
#[test]
fn fifty_thousand_events_are_read_in_linear_time() {
    let stores = Stores::new("budget");
    stores.ok(&["volume", "create", "first"]);
    let journal = stores.0.join("events/events.log");
    let line = std::fs::read_to_string(&journal).unwrap();
    std::fs::write(&journal, line.repeat(50_000)).unwrap();
    let best = (0..3)
        .map(|_| {
            let started = Instant::now();
            let read = stores.ok(&["events", "--since", "1h", "--until", "0s"]);
            let took = started.elapsed();
            assert_eq!(read.lines().count(), 50_000);
            took
        })
        .min()
        .unwrap();
    assert!(best < Duration::from_secs(1), "{best:?}");
}
