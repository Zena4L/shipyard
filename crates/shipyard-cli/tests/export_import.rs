//! End-to-end `export` and `import` (step 12.2b): a container's own
//! filesystem out (running: its root mount cloned alone; stopped: its view
//! with no volume), never its volumes; a root filesystem's archive in as a
//! one-layer image that runs; what an import refuses records nothing.
//!
//! Linux only, as tester and as root (each test has stores of its own); a
//! run is skipped where image runs can't work here, as `image.rs`'s.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use shipyard_test_support::registry::{
    LayerEntry, Scratch, built_layer, fixture, fixture_with_layer, image_runs_here,
};
use shipyard_test_support::{Shipyard, text};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `shipyard ARGS…` on `store`.
fn shipyard(store: &Scratch, args: &[&str]) -> Output {
    store.shipyard(SHIPYARD, args)
}

/// `shipyard ARGS…` on `store`, which must succeed: its stdout.
fn ok(store: &Scratch, args: &[&str]) -> String {
    store.ok(SHIPYARD, args)
}

/// The names in a tar archive, each with its kind and size; and that every
/// directory's name ends with `/`, as Docker's export writes them.
fn listed(archive: &[u8]) -> Vec<(String, tar::EntryType, u64)> {
    tar::Archive::new(archive)
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let header = entry.header();
            (
                // A directory's name ends with `/` (Docker's): compared without.
                entry
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .trim_end_matches('/')
                    .to_owned(),
                header.entry_type(),
                header.size().unwrap(),
            )
        })
        .collect()
}

/// Whether every directory in `archive` is named with a trailing `/`.
fn directories_named_as_docker_s(archive: &[u8]) -> bool {
    tar::Archive::new(archive).entries().unwrap().all(|entry| {
        let entry = entry.unwrap();
        entry.header().entry_type() != tar::EntryType::Directory
            || entry.path_bytes().ends_with(b"/")
    })
}

/// Whether `archive` has `name`.
fn has(archive: &[(String, tar::EntryType, u64)], name: &str) -> bool {
    archive.iter().any(|(had, _, _)| had == name)
}

/// An image with `covered/own` (the image's, under a directory a volume
/// will cover), and a container of it up with `data` on `covered`, a file
/// written there (the volume's) and one at its root (its own layer's). None
/// if image runs can't work here.
fn up_with_a_volume(store: &Scratch) -> Option<()> {
    let layer = built_layer(&[
        LayerEntry::Dir {
            path: "covered",
            owner: (0, 0),
            mode: 0o755,
        },
        LayerEntry::File {
            path: "covered/own",
            owner: (0, 0),
            mode: 0o644,
            content: b"the image's",
        },
        LayerEntry::Link {
            path: "out",
            target: "/",
        },
    ]);
    let (serving, image) = fixture_with_layer(layer, r#"{"Cmd":["/bin/cat","/marker"]}"#);
    ok(store, &["pull", &image]);
    drop(serving);
    let run = shipyard(
        store,
        &[
            "run",
            "-d",
            "--name",
            "up",
            "--network",
            "none",
            "-v",
            "data:/covered",
            &image,
            "sh",
            "-c",
            "echo v > /covered/in-volume; echo w > /written; mknod /fifo p; \
             touch /ready; sleep 300",
        ],
    );
    if !image_runs_here(&run) {
        return None;
    }
    assert!(run.status.success(), "{}", text(&run).1);
    shipyard_test_support::eventually(
        std::time::Duration::from_secs(20),
        "the container ready",
        || {
            shipyard(store, &["exec", "up", "sh", "-c", "test -e /ready"])
                .status
                .success()
        },
    );
    Some(())
}

/// `export` holds the container's own files, never its volume's: running,
/// what the volume covers is there as it is underneath (its root mount
/// cloned alone); stopped, the same. A link out is a link, never followed;
/// a FIFO never blocks it. The control: `cp` of the running container sees
/// the volume's file.
#[test]
fn export_leaves_out_volumes() {
    let store = Scratch::new();
    if up_with_a_volume(&store).is_none() {
        return;
    }
    let seen = shipyard(&store, &["cp", "up:/covered/in-volume", "-"]);
    assert!(seen.status.success(), "the control: {}", text(&seen).1);

    let running = shipyard(&store, &["export", "up"]);
    assert!(running.status.success(), "{}", text(&running).1);
    ok(&store, &["stop", "-t", "1", "up"]);
    let path = store.0.join("stopped.tar");
    ok(
        &store,
        &["container", "export", "-o", path.to_str().unwrap(), "up"],
    );
    let stopped = std::fs::read(&path).unwrap();
    for (which, archive) in [("running", &running.stdout), ("stopped", &stopped)] {
        let names = listed(archive);
        assert!(directories_named_as_docker_s(archive), "{which}");
        assert!(has(&names, "covered/own"), "{which}: the covered file");
        assert!(has(&names, "written"), "{which}: its own layer's file");
        assert!(has(&names, "marker"), "{which}: the image's");
        assert!(
            !has(&names, "covered/in-volume"),
            "{which}: the volume's file"
        );
        let out = names.iter().find(|(name, _, _)| name == "out").unwrap();
        assert_eq!(out.1, tar::EntryType::Symlink, "{which}");
        let fifo = names.iter().find(|(name, _, _)| name == "fifo").unwrap();
        assert_eq!(fifo.1, tar::EntryType::Fifo, "{which}");
        assert!(
            names.iter().all(|(name, _, _)| !name.starts_with("out/")),
            "{which}: the link was followed"
        );
    }
}

/// Where the clone isn't used (the walk a refused clone falls back to,
/// forced by a test build), every other mount is written as its place
/// alone (the volume's directory, `/proc`, `/etc/hosts`) and the own
/// files still are: a file copied up to the writable layer among them.
#[test]
fn export_s_walk_keeps_to_the_root_mount() {
    let store = Scratch::new();
    if up_with_a_volume(&store).is_none() {
        return;
    }
    let walked = store
        .on(&mut Command::new(SHIPYARD.path()))
        .args(["export", "up"])
        .env("SHIPYARD_TEST_EXPORT_WALK", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(walked.status.success(), "{}", text(&walked).1);
    let names = listed(&walked.stdout);
    let covered = names.iter().find(|(name, _, _)| name == "covered").unwrap();
    assert_eq!(covered.1, tar::EntryType::Directory);
    assert!(!has(&names, "covered/own"), "under a mount: not entered");
    assert!(!has(&names, "covered/in-volume"));
    assert!(has(&names, "proc"), "a mount point's place is kept");
    assert!(!names.iter().any(|(name, _, _)| name.starts_with("proc/")));
    let hosts = names
        .iter()
        .find(|(name, _, _)| name == "etc/hosts")
        .unwrap();
    assert_eq!(
        (hosts.1, hosts.2),
        (tar::EntryType::Regular, 0),
        "a bound file"
    );
    assert!(has(&names, "written") && has(&names, "marker"));
    ok(&store, &["rm", "-f", "up"]);
}

/// A created container's export takes none of its volumes: nothing made
/// or held (the same volumes before and after, and a volume of its removed
/// meanwhile is nothing to it).
#[test]
fn a_created_container_s_export_makes_no_volume() {
    let (serving, image) = fixture();
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let source = store.0.join("bind-source");
    std::fs::create_dir(&source).unwrap();
    let bind = format!("{}:/b", source.display());
    let created = shipyard(
        &store,
        &[
            "create",
            "--name",
            "made",
            "-v",
            "newvol:/d",
            "-v",
            &bind,
            &image,
        ],
    );
    assert!(created.status.success(), "{}", text(&created).1);
    // Its bind's source gone since: nothing an export reads.
    std::fs::remove_dir(&source).unwrap();
    let before = ok(&store, &["volume", "ls", "-q"]);
    let exported = shipyard(&store, &["export", "made"]);
    if !image_runs_here(&exported) {
        return;
    }
    assert!(exported.status.success(), "{}", text(&exported).1);
    assert!(has(&listed(&exported.stdout), "marker"));
    assert_eq!(ok(&store, &["volume", "ls", "-q"]), before);
}

/// A container's export imported is an image that runs, its `--change`
/// in its config; the same archive through standard input too.
#[test]
fn an_exported_container_imports_and_runs() {
    let (serving, image) = fixture();
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let created = shipyard(&store, &["create", "--name", "source", &image]);
    assert!(created.status.success(), "{}", text(&created).1);
    let path = store.0.join("rootfs.tar");
    let exported = shipyard(&store, &["export", "-o", path.to_str().unwrap(), "source"]);
    if !image_runs_here(&exported) {
        return;
    }
    assert!(exported.status.success(), "{}", text(&exported).1);
    let id = ok(
        &store,
        &[
            "import",
            "-c",
            r#"CMD ["/bin/cat", "/marker"]"#,
            "-c",
            "ENV IMPORTED=yes",
            "-m",
            "from a test",
            path.to_str().unwrap(),
            "imported:1",
        ],
    );
    assert!(id.trim().starts_with("sha256:"), "{id}");
    let run = shipyard(&store, &["run", "--rm", "--pull", "never", "imported:1"]);
    let (stdout, stderr) = text(&run);
    assert!(run.status.success(), "{stderr}");
    assert!(stdout.contains("shipyard test root filesystem"), "{stdout}");
    let env = ok(
        &store,
        &["run", "--rm", "imported:1", "sh", "-c", "echo $IMPORTED"],
    );
    assert_eq!(env.trim(), "yes");
    let history = ok(
        &store,
        &["history", "--format", "{{.Comment}}", "imported:1"],
    );
    assert_eq!(history.trim(), "from a test");

    let mut piped = store
        .on(&mut Command::new(SHIPYARD.path()))
        .args(["image", "import", "-", "imported:2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    piped
        .stdin
        .take()
        .unwrap()
        .write_all(&std::fs::read(&path).unwrap())
        .unwrap();
    let piped = piped.wait_with_output().unwrap();
    assert!(piped.status.success(), "{}", text(&piped).1);
}

/// What an import refuses records nothing: no name (D12.2-9), a URL, a
/// change it doesn't take, another platform, an empty or cut archive. Each
/// exits 1 with nothing listed after.
#[test]
fn an_unsound_import_records_nothing() {
    let store = Scratch::new();
    let mut whole = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(2);
    header.set_mode(0o644);
    whole.append_data(&mut header, "f", &b"x\n"[..]).unwrap();
    let whole = whole.into_inner().unwrap();
    let file = |name: &str, bytes: &[u8]| {
        let path = store.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let good = file("good.tar", &whole);
    let empty = file("empty.tar", b"");
    let cut = file("cut.tar", &whole[..whole.len() - 1024]);
    for (what, args) in [
        ("no name", vec!["import", good.as_str()]),
        ("a URL", vec!["import", "https://example.com/x.tar", "x:1"]),
        (
            "RUN",
            vec!["import", "-c", "RUN make", good.as_str(), "x:1"],
        ),
        (
            "platform",
            vec!["import", "--platform", "linux/s390x", good.as_str(), "x:1"],
        ),
        ("empty", vec!["import", empty.as_str(), "x:1"]),
        ("cut", vec!["import", cut.as_str(), "x:1"]),
    ] {
        let output = shipyard(&store, &args);
        assert_eq!(output.status.code(), Some(1), "{what}: {output:?}");
    }
    let listed = ok(&store, &["images", "-q"]);
    assert!(listed.trim().is_empty(), "{listed}");
    // The control: the same archive, named, is imported.
    ok(&store, &["import", good.as_str(), "x:1"]);
    assert!(!ok(&store, &["images", "-q"]).trim().is_empty());
}

/// A `--rm` container that ends during its export (the export held up on a
/// reader that doesn't read): its writable layer goes with its end, under
/// the walk, so the export fails, said, never exit 0 with an archive that
/// mixes its files with its image's (Murphy's review of 12.2b).
#[test]
fn a_rm_container_ending_during_its_export_fails_it() {
    let (serving, image) = fixture();
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let run = shipyard(
        &store,
        &[
            "run",
            "-d",
            "--rm",
            "--name",
            "brief",
            "--network",
            "none",
            &image,
            "sleep",
            "300",
        ],
    );
    if !image_runs_here(&run) {
        return;
    }
    assert!(run.status.success(), "{}", text(&run).1);
    let mut export = store
        .on(&mut Command::new(SHIPYARD.path()))
        .args(["export", "brief"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Its first bytes: the export is under way, and holds on the full pipe.
    let mut stdout = export.stdout.take().unwrap();
    let mut first = [0_u8; 512];
    std::io::Read::read_exact(&mut stdout, &mut first).unwrap();
    ok(&store, &["kill", "brief"]);
    shipyard_test_support::eventually_every(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
        "it removed",
        || {
            ok(&store, &["ps", "-a", "-q", "-f", "name=brief"])
                .trim()
                .is_empty()
        },
    );
    std::io::copy(&mut stdout, &mut std::io::sink()).unwrap();
    let ended = export.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&ended.stderr);
    assert_eq!(ended.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("ended during the export"), "{stderr}");
}
