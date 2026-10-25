//! End-to-end `save`, `load` and `history` (step 12.2a): an image written
//! out of one store and read into another, with no registry; what a load
//! refuses (a cut archive, one missing a layer) leaving the store as it
//! was; a tag a load moves, said; a load beside a prune.
//!
//! Linux only: a load extracts its layers, as a pull does. As tester and as
//! root (each test has stores of its own); a run of a loaded image is
//! skipped where image runs can't work here, as `image.rs`'s.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::Ordering;

use shipyard_test_support::registry::{Scratch, fixture, fixture_with, image_runs_here};
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

/// The store's images, `REPOSITORY:TAG ID` a line, sorted.
fn listed(store: &Scratch) -> Vec<String> {
    let mut lines: Vec<String> = ok(
        store,
        &[
            "images",
            "--no-trunc",
            "--format",
            "{{.Repository}}:{{.Tag}} {{.ID}}",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    lines.sort();
    lines
}

/// The ID of the image `name` (`REPOSITORY:TAG`) in `store`.
fn id_of(store: &Scratch, name: &str) -> String {
    listed(store)
        .iter()
        .find_map(|line| line.strip_prefix(&format!("{name} ")).map(str::to_owned))
        .unwrap_or_else(|| panic!("no {name} in {:?}", listed(store)))
}

/// `load < archive` on `store`, the archive through a pipe (as `save |
/// load` gives it).
fn load_piped(store: &Scratch, archive: &[u8]) -> Output {
    let mut child = store
        .on(&mut Command::new(SHIPYARD.path()))
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // A load that fails early stops reading: its broken pipe is the
    // failure's, not this write's.
    let _ = stdin.write_all(archive);
    drop(stdin);
    child.wait_with_output().unwrap()
}

/// What's left in the store's `blobs/` besides `sha256/`: a load's staging
/// it didn't remove.
fn staging_left(store: &Scratch) -> Vec<String> {
    std::fs::read_dir(store.store().join("blobs"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name != "sha256")
                .collect()
        })
        .unwrap_or_default()
}

/// A saved image loads into another store with its registry gone, keeps
/// its ID, names and history, and runs; `-o` and `-i` as files, and the
/// same archive through a pipe.
#[test]
fn a_saved_image_loads_elsewhere_and_runs() {
    let (serving, image) = fixture_with(2);
    let from = Scratch::new();
    ok(&from, &["pull", &image]);
    ok(&from, &["tag", &image, "saved/second:2"]);
    let archive = from.0.join("images.tar");
    let archive_path = archive.to_str().unwrap();
    // Both names in one archive, one of them by the group's command.
    ok(
        &from,
        &[
            "image",
            "save",
            "-o",
            archive_path,
            &image,
            "saved/second:2",
        ],
    );
    drop(serving);
    assert!(
        !std::fs::read_dir(&from.0).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".shipyard-")),
        "save -o left its temporary file"
    );

    let to = Scratch::new();
    let said = ok(&to, &["load", "-i", archive_path]);
    assert!(
        said.contains(&format!("Loaded image: {image}:latest")),
        "{said}"
    );
    assert!(said.contains("Loaded image: saved/second:2"), "{said}");
    assert_eq!(listed(&to), listed(&from), "the same names and IDs");
    assert!(staging_left(&to).is_empty(), "{:?}", staging_left(&to));

    // Newest first: the image's ID, then a step a layer (no history in
    // the fixture's config).
    let history = ok(&to, &["history", "-q", "--no-trunc", "saved/second:2"]);
    let ids: Vec<&str> = history.lines().collect();
    assert_eq!(ids.len(), 3, "{history}");
    assert!(ids[0].starts_with("sha256:"), "{history}");
    assert_eq!(&ids[1..], ["<missing>", "<missing>"], "{history}");

    // Again, through a pipe: the same archive loads the same.
    let piped = Scratch::new();
    let output = load_piped(&piped, &std::fs::read(&archive).unwrap());
    assert!(output.status.success(), "{}", text(&output).1);
    assert_eq!(listed(&piped), listed(&from));

    let run = shipyard(
        &to,
        &[
            "run",
            "--rm",
            "--pull",
            "never",
            "saved/second:2",
            "cat",
            "/top",
        ],
    );
    if !image_runs_here(&run) {
        return;
    }
    let (stdout, stderr) = text(&run);
    assert!(run.status.success(), "{stderr}");
    assert_eq!(stdout.trim(), "1", "the top layer's file");
}

/// A missing image fails `save` before anything is written: `-o` leaves
/// neither the file nor its temporary behind. (Positive control: the test
/// above, whose `-o` is written.)
#[test]
fn a_save_of_a_missing_image_writes_nothing() {
    let store = Scratch::new();
    let archive = store.0.join("nothing.tar");
    let output = shipyard(
        &store,
        &["save", "-o", archive.to_str().unwrap(), "no/such:image"],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        text(&output).1.contains("no such image: no/such:image"),
        "{output:?}"
    );
    assert!(!archive.exists());
    assert!(
        std::fs::read_dir(&store.0).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".shipyard-")),
        "a temporary file left"
    );
}

/// A load over a tag another image has moves it to the archive's, and
/// says so (a hostile archive can't retag an image unsaid); the old image
/// keeps its other names.
#[test]
fn a_load_moves_a_tag_and_says_so() {
    let (serving_one, one) = fixture();
    let (serving_two, two) = fixture_with(1);
    let from = Scratch::new();
    ok(&from, &["pull", &two]);
    ok(&from, &["tag", &two, "app:1"]);
    let archive = from.0.join("app.tar");
    ok(&from, &["save", "-o", archive.to_str().unwrap(), "app:1"]);

    let to = Scratch::new();
    ok(&to, &["pull", &one]);
    ok(&to, &["tag", &one, "app:1"]);
    drop((serving_one, serving_two));
    let old = id_of(&to, "app:1");
    let said = ok(&to, &["load", "-i", archive.to_str().unwrap()]);
    assert!(
        said.contains(&format!(
            "The image app:1 already exists, renaming the old one with ID {old}"
        )),
        "{said}"
    );
    assert!(said.contains("Loaded image: app:1"), "{said}");
    let new = id_of(&to, "app:1");
    assert_ne!(new, old);
    assert_eq!(new, id_of(&from, "app:1"));
    assert_eq!(id_of(&to, &format!("{one}:latest")), old);
}

/// The archive's members, in order: name and bytes.
fn members(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut read = tar::Archive::new(archive);
    read.entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

/// `members` as an archive again, but for those `drop` names.
fn rebuilt(members: &[(String, Vec<u8>)], drop: &dyn Fn(&str) -> bool) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in members.iter().filter(|(name, _)| !drop(name)) {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o444);
        header.set_entry_type(if name.ends_with('/') {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        builder
            .append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// An archive missing a layer fails the load, and nothing is asked of a
/// registry for it (the image's registry is up, holding back any blob it's
/// asked for: none is); an archive cut short fails too. Each leaves the
/// store as it was: no image, no staging. Positive control: the whole
/// archive loads.
#[test]
fn an_unsound_archive_loads_nothing_and_asks_no_registry() {
    let (serving, image) = fixture_with(1);
    let from = Scratch::new();
    ok(&from, &["pull", &image]);
    let path = from.0.join("whole.tar");
    ok(&from, &["save", "-o", path.to_str().unwrap(), &image]);
    let whole = std::fs::read(&path).unwrap();
    let all = members(&whole);
    let manifest: serde_json::Value = serde_json::from_slice(
        &all.iter()
            .find(|(name, _)| name == "manifest.json")
            .unwrap()
            .1,
    )
    .unwrap();
    let layer = manifest[0]["Layers"][0].as_str().unwrap().to_owned();
    serving.hold.store(true, Ordering::Relaxed);

    let to = Scratch::new();
    let missing = load_piped(&to, &rebuilt(&all, &|name| name == layer));
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    let cut = load_piped(&to, &whole[..whole.len() / 2]);
    assert_eq!(cut.status.code(), Some(1), "{cut:?}");
    assert!(text(&cut).1.contains("cut short"), "{}", text(&cut).1);
    assert_eq!(
        serving.held.load(Ordering::Relaxed),
        0,
        "a blob was asked for"
    );
    assert!(listed(&to).is_empty(), "{:?}", listed(&to));
    assert!(staging_left(&to).is_empty(), "{:?}", staging_left(&to));

    serving.hold.store(false, Ordering::Relaxed);
    let output = load_piped(&to, &whole);
    assert!(output.status.success(), "{}", text(&output).1);
    assert_eq!(listed(&to).len(), 1);
}

/// A prune while a load is still reading its archive (the stream held
/// open half way) takes none of what it staged: the load then finishes,
/// and its image runs.
#[test]
fn a_prune_beside_a_load_takes_nothing_of_it() {
    let (serving, image) = fixture_with(1);
    let from = Scratch::new();
    ok(&from, &["pull", &image]);
    let path = from.0.join("whole.tar");
    ok(&from, &["save", "-o", path.to_str().unwrap(), &image]);
    drop(serving);
    let whole = std::fs::read(&path).unwrap();

    let to = Scratch::new();
    let mut load = to
        .on(&mut Command::new(SHIPYARD.path()))
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = load.stdin.take().unwrap();
    // All but the last blocks: every blob is staged, the end not yet come.
    let half = whole.len() - 1024;
    stdin.write_all(&whole[..half]).unwrap();
    shipyard_test_support::eventually_every(
        std::time::Duration::from_secs(20),
        std::time::Duration::from_millis(20),
        "the load staged",
        || !staging_left(&to).is_empty(),
    );
    let staged = staging_left(&to);
    ok(&to, &["system", "prune", "-a", "-f"]);
    assert_eq!(
        staging_left(&to),
        staged,
        "the prune took the load's staging"
    );

    stdin.write_all(&whole[half..]).unwrap();
    drop(stdin);
    let loaded = load.wait_with_output().unwrap();
    assert!(loaded.status.success(), "{}", text(&loaded).1);
    assert!(staging_left(&to).is_empty(), "{:?}", staging_left(&to));
    let run = shipyard(
        &to,
        &["run", "--rm", "--pull", "never", &image, "cat", "/top"],
    );
    if !image_runs_here(&run) {
        return;
    }
    assert!(run.status.success(), "{}", text(&run).1);
}

/// A staging directory a killed load left is swept by the next load,
/// once nothing holds it (the store's collection sweeps it too: the unit
/// test `a_killed_load_s_staging_is_swept`).
#[test]
fn a_killed_loads_staging_is_swept() {
    let (serving, image) = fixture();
    let from = Scratch::new();
    ok(&from, &["pull", &image]);
    let path = from.0.join("whole.tar");
    ok(&from, &["save", "-o", path.to_str().unwrap(), &image]);
    drop(serving);
    let whole = std::fs::read(&path).unwrap();

    let to = Scratch::new();
    let mut load = to
        .on(&mut Command::new(SHIPYARD.path()))
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = load.stdin.take().unwrap();
    stdin.write_all(&whole[..whole.len() - 1024]).unwrap();
    shipyard_test_support::eventually_every(
        std::time::Duration::from_secs(20),
        std::time::Duration::from_millis(20),
        "the load staged",
        || !staging_left(&to).is_empty(),
    );
    load.kill().unwrap();
    load.wait().unwrap();
    drop(stdin);
    assert!(!staging_left(&to).is_empty(), "a SIGKILL leaves it");
    // The next load sweeps it, and loads.
    let output = load_piped(&to, &whole);
    assert!(output.status.success(), "{}", text(&output).1);
    assert!(staging_left(&to).is_empty(), "{:?}", staging_left(&to));
    assert_eq!(listed(&to).len(), 1);
}

/// `history`'s default table: Docker's columns, a row per step; a
/// missing image fails.
#[test]
fn history_shows_docker_s_table() {
    let (serving, image) = fixture_with(1);
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let table = ok(&store, &["history", &image]);
    let mut lines = table.lines();
    let header = lines.next().unwrap();
    for column in ["IMAGE", "CREATED", "CREATED BY", "SIZE", "COMMENT"] {
        assert!(header.contains(column), "{header}");
    }
    assert_eq!(lines.count(), 2, "{table}");
    let missing = shipyard(&store, &["history", "no/such:image"]);
    assert_eq!(missing.status.code(), Some(1));
}

/// `save`'s archive, read back: Docker 25's layout (`oci-layout`,
/// `index.json`, `manifest.json`, the blobs), the same bytes each time.
#[test]
fn a_save_is_docker_s_layout_and_the_same_each_time() {
    let (serving, image) = fixture();
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let save = |name: &str| {
        let path = store.0.join(name);
        ok(&store, &["save", "-o", path.to_str().unwrap(), &image]);
        std::fs::read(path).unwrap()
    };
    let first = save("a.tar");
    assert_eq!(first, save("b.tar"), "a save is deterministic");
    let names: Vec<String> = members(&first).into_iter().map(|(name, _)| name).collect();
    for expected in ["oci-layout", "index.json", "manifest.json", "blobs/sha256/"] {
        assert!(names.iter().any(|name| name == expected), "{names:?}");
    }
    assert!(
        Path::new(&store.0).join("a.tar").metadata().unwrap().len() > 0,
        "written"
    );
}

/// `save -o` a FIFO or a device writes into it, never replaces it (Murphy's
/// review: a rename over `/dev/null` as root would replace the host's).
/// The FIFO's reader gets the same bytes a file gets.
#[test]
fn save_into_a_fifo_or_a_device_never_replaces_it() {
    use std::os::unix::fs::FileTypeExt as _;
    let (serving, image) = fixture();
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let file = store.0.join("a.tar");
    ok(&store, &["save", "-o", file.to_str().unwrap(), &image]);
    let whole = std::fs::read(&file).unwrap();

    // First: a save that replaced it fails here (no right to `/dev`),
    // never hangs on the FIFO below.
    ok(&store, &["save", "-o", "/dev/null", &image]);
    assert!(
        std::fs::metadata("/dev/null")
            .unwrap()
            .file_type()
            .is_char_device(),
        "/dev/null replaced"
    );

    let fifo = store.0.join("fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    let reading = {
        let fifo = fifo.clone();
        std::thread::spawn(move || std::fs::read(fifo).unwrap())
    };
    ok(&store, &["save", "-o", fifo.to_str().unwrap(), &image]);
    assert_eq!(reading.join().unwrap(), whole);
    assert!(
        std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo()
    );
}

/// A save of more blobs than the process may hold open is refused before
/// it writes, said (Murphy's review: it failed half way, `EMFILE`). The
/// control: the same save with the limit as it was (above) succeeds.
#[test]
fn a_save_past_the_descriptor_limit_is_refused_up_front() {
    use std::os::unix::process::CommandExt as _;
    let (serving, image) = fixture_with(2);
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    drop(serving);
    let file = store.0.join("a.tar");
    let mut command = Command::new(SHIPYARD.path());
    store
        .on(&mut command)
        .args(["save", "-o", file.to_str().unwrap(), &image]);
    // SAFETY: `setrlimit` is a system call, async-signal-safe; nothing else
    // runs between the fork and the exec.
    unsafe {
        command.pre_exec(|| {
            nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE, 72, 72)
                .map_err(std::io::Error::from)
        });
    }
    let output = command.output().unwrap();
    let (_, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("more than this process may hold open"),
        "{stderr}"
    );
    assert!(!file.exists());
}

/// The OCI architecture of this host (Go's names).
fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// A hostile archive's words never reach a terminal raw: a name with an
/// escape is left out, said escaped; the history's instruction and comment
/// with escapes are shown escaped, in the table and in a template.
#[test]
fn an_archive_s_words_are_escaped() {
    use shipyard_test_support::registry::sha256;
    let mut layer = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(2);
    header.set_mode(0o644);
    layer.append_data(&mut header, "f", &b"x\n"[..]).unwrap();
    let layer = layer.into_inner().unwrap();
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": host_architecture(), "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": [sha256(&layer)]},
        "config": {"Cmd": ["true"]},
        "history": [{"created_by": "evil \u{1b}]0;owned\u{7} step", "comment": "\u{1b}[2J"}],
    }))
    .unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": sha256(&config), "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": sha256(&layer), "size": layer.len()}],
    }))
    .unwrap();
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                       "digest": sha256(&manifest), "size": manifest.len(),
                       "annotations": {"io.containerd.image.name": "docker.io/library/hostile:1"}}],
    }))
    .unwrap();
    let blob = |bytes: &[u8]| {
        (
            format!("blobs/sha256/{}", &sha256(bytes)["sha256:".len()..]),
            bytes.to_vec(),
        )
    };
    let manifest_json = serde_json::to_vec(&serde_json::json!([{
        "Config": blob(&config).0, "RepoTags": ["hostile:1", "bad\u{1b}[31mname"],
        "Layers": [blob(&layer).0],
    }]))
    .unwrap();
    let archive = rebuilt(
        &[
            ("index.json".to_owned(), index),
            blob(&layer),
            blob(&config),
            blob(&manifest),
            ("manifest.json".to_owned(), manifest_json),
        ],
        &|_| false,
    );

    let store = Scratch::new();
    let output = load_piped(&store, &archive);
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stderr}");
    assert!(!output.stdout.contains(&0x1b), "{stdout:?}");
    assert!(stdout.contains("Loaded image: hostile:1"), "{stdout}");
    assert!(stdout.contains(r"bad\u{1b}[31mname"), "{stdout}");

    for args in [
        &["history", "--no-trunc", "hostile:1"][..],
        &[
            "history",
            "--format",
            "{{.CreatedBy}} {{.Comment}}",
            "hostile:1",
        ],
    ] {
        let output = shipyard(&store, args);
        let (stdout, stderr) = text(&output);
        assert!(output.status.success(), "{stderr}");
        assert!(!output.stdout.contains(&0x1b), "{args:?}: {stdout:?}");
        assert!(stdout.contains("evil"), "{args:?}: {stdout}");
    }
}

/// Two images in one archive claiming one tag: the last names it, said
/// once (one `Loaded image:` line, one record), never twice.
#[test]
fn a_tag_two_images_claim_names_the_last() {
    let (serving_one, one) = fixture();
    let (serving_two, two) = fixture_with(1);
    let store = Scratch::new();
    ok(&store, &["pull", &one]);
    ok(&store, &["pull", &two]);
    drop((serving_one, serving_two));
    ok(&store, &["tag", &one, "first:1"]);
    ok(&store, &["tag", &two, "second:1"]);
    let path = store.0.join("both.tar");
    ok(
        &store,
        &["save", "-o", path.to_str().unwrap(), "first:1", "second:1"],
    );
    // Both images given the one tag, in manifest.json's order; the index
    // kept (their IDs with it), its names gone.
    let all = members(&std::fs::read(&path).unwrap());
    let edited: Vec<(String, Vec<u8>)> = all
        .into_iter()
        .map(|(name, bytes)| {
            let mut document: serde_json::Value = match name.as_str() {
                "manifest.json" | "index.json" => serde_json::from_slice(&bytes).unwrap(),
                _ => return (name, bytes),
            };
            if name == "manifest.json" {
                for image in document.as_array_mut().unwrap() {
                    image["RepoTags"] = serde_json::json!(["both:1"]);
                }
            } else {
                for entry in document["manifests"].as_array_mut().unwrap() {
                    entry.as_object_mut().unwrap().remove("annotations");
                }
            }
            (name, serde_json::to_vec(&document).unwrap())
        })
        .collect();
    let to = Scratch::new();
    let output = load_piped(&to, &rebuilt(&edited, &|_| false));
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stdout.matches("Loaded image: both:1").count(),
        1,
        "{stdout}"
    );
    assert!(
        stdout.contains("both:1 names two images in the archive"),
        "{stdout}"
    );
    assert_eq!(
        listed(&to),
        [format!("both:1 {}", id_of(&store, "second:1"))]
    );
}
