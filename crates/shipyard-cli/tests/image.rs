//! End-to-end `shipyard run IMAGE` (stage 6, phase 6.6): a pulled image
//! becomes the container's root filesystem.
//!
//! A fixture registry (in-process, std-only HTTP) serves a one-layer image
//! built from the host's own programs (via `shipyard-test-support`'s
//! `RootFs`), so there's no network and no real registry. The test runs the
//! real `shipyard` binary with `run localhost:PORT/...` and checks the
//! command ran inside the image's root (it can read the image's `/marker`).
//!
//! Linux only. The runs need root, or rootless containers and a kernel that
//! lets a normal user mount overlay; where a normal user can't, the tests
//! say why and return (unless `SHIPYARD_TEST_ROOTLESS=required`, as in CI
//! and `test-linux.sh`, where that fails them). The few that need root are
//! `#[ignore]`d and run in the root passes.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::process::Command;
use std::sync::atomic::Ordering;
use std::thread;

use shipyard_test_support::Shipyard;
use shipyard_test_support::registry::{
    Scratch, Serving, fixture, fixture_layers, fixture_of, fixture_with, image_runs_here,
    implying_layer, layer_tar,
};
use shipyard_test_support::rootfs::RootFs;

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `shipyard ARGS`, with its image store in `store`. A foreground `run` is
/// a container (stage 9, step 9.4c3a), which would keep its image pinned:
/// these tests are the images', so it's removed at its end (`--rm`).
fn shipyard(store: &Scratch, args: &[&str]) -> std::process::Output {
    on(store, &mut Command::new(SHIPYARD.path()))
        .args(removed_at_its_end(args))
        .output()
        .expect("running shipyard")
}

/// An error that repeats what it was given (or what a registry or a layer
/// said) never sends what a terminal acts on: here an OSC sequence that
/// would set the terminal's title, escaped (the v0.2 review).
#[test]
fn an_error_never_sends_a_terminal_escape() {
    let store = Scratch::new();
    let output = shipyard(&store, &["rmi", "a\x1b]0;owned\x07b"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.stderr.contains(&0x1b), "{stderr:?}");
    assert!(stderr.contains("a\\u{1b}]0;owned\\u{7}b"), "{stderr}");
}

#[test]
fn run_from_an_image_sees_its_root_filesystem() {
    let (_serving, image) = fixture();
    // No command: the image's Cmd.
    let store = Scratch::new();
    let output = shipyard(&store, &["run", &image]);

    if !image_runs_here(&output) {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "shipyard run IMAGE failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("shipyard test root filesystem"),
        "the command ran in the image's root and read /marker; got:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Without root, the image's files are made in the storage helper's user
/// namespace: owned on the host by the caller's subordinate IDs (the
/// image's users, shifted), not by the caller. And removing the image
/// removes them, through the helper again.
#[test]
fn rootless_layers_belong_to_subordinate_ids_and_are_removed() {
    use shipyard_test_support::{Rootless, is_root, rootless};
    use std::os::unix::fs::MetadataExt as _;
    if is_root() || !rootless(Rootless::Full) {
        return;
    }
    let (_serving, image) = fixture();
    let store = Scratch::new();
    // Whether the run itself works here (overlay) doesn't matter: the layers
    // are extracted before it.
    let _ = shipyard(&store, &["run", &image, "sh", "-c", "exit 0"]);

    let layers = store.store().join("layers");
    let layer = std::fs::read_dir(&layers)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| !path.file_name().unwrap().to_string_lossy().starts_with('.'))
        .expect("a layer was extracted");
    let marker = std::fs::metadata(layer.join("diff/marker")).unwrap();
    // The layer was made from files the caller owns: in the image, owned by
    // the caller's UID. Inside the namespace that's a subordinate ID: the
    // namespace's UID 1 is the first of the caller's range, and so on.
    let caller = nix::unistd::getuid();
    let name = nix::unistd::User::from_uid(caller)
        .unwrap()
        .map(|user| user.name)
        .unwrap();
    let first = std::fs::read_to_string("/etc/subuid")
        .unwrap()
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let owner = fields.next()?;
            (owner == name || owner == caller.to_string())
                .then(|| fields.next()?.parse::<u32>().ok())?
        })
        .expect("the caller has subordinate IDs");
    assert_eq!(
        marker.uid(),
        first + caller.as_raw() - 1,
        "the image's user {caller}, shifted into the caller's range"
    );

    let listed = shipyard(&store, &["images", "--format", "{{.Repository}}:{{.Tag}}"]);
    let reference = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .next()
        .map(str::to_owned)
        .expect("the image is listed");
    let removed = shipyard(&store, &["rmi", &reference]);
    assert!(
        removed.status.success(),
        "rmi: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!layer.exists(), "the layer's files were removed");
}

/// The names in the store's `containers/` (the runs' writable areas).
fn areas(store: &Scratch) -> Vec<String> {
    std::fs::read_dir(store.store().join("containers"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default()
}

/// The image layer directories in the store (not temporaries).
fn layers(store: &Scratch) -> Vec<String> {
    std::fs::read_dir(store.store().join("layers"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

/// `command` on `store`: its image store, and a container store of its own
/// beside it (a container's writable layer is in the image store its record
/// goes with: one image store per container store, as the engine has it).
fn on<'c>(store: &Scratch, command: &'c mut Command) -> &'c mut Command {
    use std::os::unix::fs::DirBuilderExt as _;
    let containers = store.0.join("container-records");
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&containers);
    shipyard_test_support::with_container_places(command)
        .env("SHIPYARD_IMAGE_STORE", store.store())
        .env("SHIPYARD_CONTAINER_DIR", containers)
}

/// `args`, with `--rm` after a foreground `run`.
fn removed_at_its_end<'a>(args: &[&'a str]) -> Vec<&'a str> {
    match args {
        // `-d` among run's own flags (before its image), not its command's.
        ["run", rest @ ..]
            if !rest
                .iter()
                .take_while(|arg| arg.starts_with('-'))
                .any(|arg| *arg == "-d") =>
        {
            [&["run", "--rm"], rest].concat()
        }
        _ => args.to_vec(),
    }
}

/// The runs' areas in the store (not leftovers): a container's (`ctr-`),
/// or a run's supervised here (`run-`).
fn runs(store: &Scratch) -> usize {
    areas(store)
        .iter()
        .filter(|name| name.starts_with("run-") || name.starts_with("ctr-"))
        .count()
}

/// Starts `shipyard run IMAGE sleep SECONDS`, and waits until there
/// are `areas` runs' areas in the store (its own among them).
fn start_sleeping(
    store: &Scratch,
    image: &str,
    seconds: &str,
    areas: usize,
) -> std::process::Child {
    start_sleeping_with(store, image, &[], seconds, areas)
}

/// As [`start_sleeping`], with these run flags.
fn start_sleeping_with(
    store: &Scratch,
    image: &str,
    flags: &[&str],
    seconds: &str,
    areas: usize,
) -> std::process::Child {
    let child = on(store, &mut Command::new(SHIPYARD.path()))
        .args(["run", "--rm"])
        .args(flags)
        .args([image, "sleep", seconds])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while runs(store) < areas {
        assert!(
            std::time::Instant::now() < deadline,
            "the run's area never appeared"
        );
        thread::sleep(std::time::Duration::from_millis(50));
    }
    child
}

/// An image store with the fixture image pulled and run once, if image runs
/// work here at all.
fn ready() -> Option<(Serving, String, Scratch)> {
    let (serving, image) = fixture();
    let store = Scratch::new();
    let first = shipyard(&store, &["run", &image, "sh", "-c", "exit 0"]);
    if !image_runs_here(&first) {
        return None;
    }
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    Some((serving, image, store))
}

/// A run's writable area goes when the run ends; two at once each get their
/// own.
#[test]
fn each_run_has_its_own_writable_layer_and_removes_it() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    assert_eq!(runs(&store), 0, "left behind: {:?}", areas(&store));
    let one = start_sleeping(&store, &image, "2", 1);
    let two = start_sleeping(&store, &image, "2", 2);
    for mut run in [one, two] {
        assert!(run.wait().unwrap().success());
    }
    assert!(areas(&store).is_empty(), "left behind: {:?}", areas(&store));
}

/// The first runs of a new image, both at once: each extracts its layers,
/// one of them places them, and the other uses those (it never removes a
/// layer the first one may already be running on). Both run, and nothing
/// is left but the one layer.
#[test]
fn two_first_runs_of_an_image_at_once_share_its_layer() {
    let (_serving, image) = fixture();
    let store = Scratch::new();
    let start = || {
        on(&store, &mut Command::new(SHIPYARD.path()))
            .args(["run", "--rm", &image, "cat", "/marker"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (one, two) = (start(), start());
    for run in [one, two] {
        let output = run.wait_with_output().unwrap();
        if !image_runs_here(&output) {
            return;
        }
        let (stdout, stderr) = text(&output);
        assert!(output.status.success(), "{stderr}");
        assert!(stdout.contains("shipyard test root filesystem"), "{stdout}");
    }
    let everything: Vec<String> = std::fs::read_dir(store.store().join("layers"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(layers(&store).len(), 1, "{everything:?}");
    assert!(
        !everything
            .iter()
            .any(|name| name.starts_with(".tmp-") || name.starts_with(".trash-")),
        "left behind: {everything:?}"
    );
}

/// A prune while a pull is in the middle of its downloads waits for it (the
/// pull holds the store's lock, shared), so it never takes what the pull has
/// fetched: the image is whole afterwards, and runs with the registry gone.
#[test]
fn a_prune_during_a_pull_waits_for_it() {
    let (serving, image) = fixture();
    let store = Scratch::new();
    serving.hold.store(true, Ordering::Relaxed);
    let pull = Command::new(SHIPYARD.path())
        .args(["pull", &image])
        .env("SHIPYARD_IMAGE_STORE", store.store())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Once it asks for a blob, the pull has the store's lock.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while serving.held.load(Ordering::Relaxed) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the pull never got to a blob"
        );
        thread::sleep(std::time::Duration::from_millis(20));
    }
    let prune = Command::new(SHIPYARD.path())
        .args(["image", "prune", "-f"])
        .env("SHIPYARD_IMAGE_STORE", store.store())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(std::time::Duration::from_millis(500));
    serving.hold.store(false, Ordering::Relaxed);
    let pulled = pull.wait_with_output().unwrap();
    assert!(pulled.status.success(), "{}", text(&pulled).1);
    let pruned = prune.wait_with_output().unwrap();
    assert!(pruned.status.success(), "{}", text(&pruned).1);
    drop(serving);
    let run = shipyard(&store, &["run", &image]);
    if !image_runs_here(&run) {
        return;
    }
    let (stdout, stderr) = text(&run);
    assert!(run.status.success(), "{stderr}");
    assert!(stdout.contains("shipyard test root filesystem"), "{stdout}");
}

/// An image a running container uses isn't removed, unless forced; its
/// layers then stay until the container ends and a prune collects them.
#[test]
fn an_image_in_use_is_kept_until_its_container_ends() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    let listed = shipyard(&store, &["images", "--format", "{{.Repository}}:{{.Tag}}"]);
    let listed = String::from_utf8_lossy(&listed.stdout);
    let reference = listed
        .lines()
        .next()
        .unwrap_or_else(|| panic!("no image listed: {listed}"))
        .to_owned();
    let mut running = start_sleeping(&store, &image, "3", 1);
    let refused = shipyard(&store, &["rmi", &reference]);
    assert!(!refused.status.success(), "removed while in use");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("used by container"));
    let forced = shipyard(&store, &["rmi", "--force", &reference]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert_eq!(layers(&store).len(), 1, "kept while the container runs");
    assert!(running.wait().unwrap().success(), "the container ran on");
    assert!(shipyard(&store, &["image", "prune", "-f"]).status.success());
    assert!(layers(&store).is_empty(), "collected once it ended");
}

/// A run killed outright (`kill -9`) leaves its container running, as a
/// detached one (its monitor's, not the run's): its writable layer stays
/// until the container goes, here by `rm -f`.
#[test]
fn a_killed_runs_container_runs_on_until_removed() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    // Its own name: the tests of this binary share a container store.
    let name = format!("killed-{}", std::process::id());
    let mut killed = start_sleeping_with(&store, &image, &["--name", &name], "30", 1);
    // Killed once it runs (killed while it's made, its start is undone).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !text(&shipyard(&store, &["ps", "--format", "json"]))
        .0
        .contains(&name)
    {
        assert!(std::time::Instant::now() < deadline, "it never ran");
        thread::sleep(std::time::Duration::from_millis(50));
    }
    let init = |store: &Scratch| {
        let shown: serde_json::Value =
            serde_json::from_slice(&shipyard(store, &["inspect", "--shipyard", &name]).stdout)
                .unwrap();
        shown[0]["state"]["pid"].as_i64()
    };
    let before = init(&store).expect("its init's PID");
    killed.kill().unwrap();
    killed.wait().unwrap();
    // Not only listed just after: the same init, alive a while later (an
    // end following the run's would have come by then).
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(runs(&store), 1, "the container's layer stays");
    let listed = shipyard(&store, &["ps", "--format", "json"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&name),
        "it runs on: {listed:?}"
    );
    assert_eq!(init(&store), Some(before), "the same init");
    assert!(
        std::path::Path::new(&format!("/proc/{before}")).exists(),
        "its init is gone"
    );
    let removed = shipyard(&store, &["rm", "-f", &name]);
    assert!(removed.status.success(), "{}", text(&removed).1);
    assert!(areas(&store).is_empty(), "left behind: {:?}", areas(&store));
}

/// `shipyard pull` stores an image whole: runs then need no registry (it's
/// stopped here), the store lists it by its canonical name, and `rmi` finds
/// it by the name it was pulled with.
#[test]
fn a_pulled_image_runs_without_its_registry() {
    let (serving, image) = fixture();
    let store = Scratch::new();
    let pulled = shipyard(&store, &["pull", &image]);
    assert!(
        pulled.status.success(),
        "{}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    assert!(String::from_utf8_lossy(&pulled.stdout).starts_with("Digest: sha256:"));
    let listed = shipyard(&store, &["images", "--format", "{{.Repository}}:{{.Tag}}"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .any(|line| line == format!("{image}:latest")),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );

    drop(serving);
    let offline = shipyard(
        &store,
        &["run", "--pull", "never", &image, "cat", "/marker"],
    );
    if !image_runs_here(&offline) {
        return;
    }
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    assert!(String::from_utf8_lossy(&offline.stdout).contains("shipyard test root filesystem"));
    let missing = shipyard(&store, &["run", &image, "cat", "/marker"]);
    assert!(
        missing.status.success(),
        "the default (missing) needs no registry either"
    );
    let always = shipyard(
        &store,
        &["run", "--pull", "always", &image, "sh", "-c", "exit 0"],
    );
    assert!(
        !always.status.success(),
        "--pull=always asked the (stopped) registry"
    );

    let removed = shipyard(&store, &["rmi", &image]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
}

/// `pull` tells stderr of each layer as it arrives (downloaded, or already
/// in the store); `pull -q` prints nothing but the digest.
#[test]
fn pull_reports_its_layers_unless_quiet() {
    let (_serving, image) = fixture();
    let store = Scratch::new();
    let first = shipyard(&store, &["pull", &image]);
    let (stdout, stderr) = text(&first);
    assert!(first.status.success(), "{stderr}");
    assert!(stdout.starts_with("Digest: sha256:"), "{stdout}");
    let mut lines = stderr.lines();
    assert_eq!(
        lines.next(),
        Some(format!("Pulling {image}:latest").as_str()),
        "{stderr}"
    );
    let layer = lines.next().unwrap_or_default();
    let (digest, what) = layer.split_once(": ").unwrap_or_default();
    assert!(
        digest.len() == 12 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "{stderr}"
    );
    assert!(
        what.starts_with("downloaded (") && what.ends_with("B)"),
        "{stderr}"
    );

    let again = shipyard(&store, &["pull", &image]);
    let (_, stderr) = text(&again);
    assert!(again.status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!("{digest}: already here")),
        "{stderr}"
    );

    let quiet = shipyard(&store, &["pull", "-q", &image]);
    let (stdout, stderr) = text(&quiet);
    assert!(quiet.status.success(), "{stderr}");
    assert!(stderr.is_empty(), "-q: {stderr}");
    assert!(
        stdout.starts_with("sha256:") && stdout.lines().count() == 1,
        "{stdout}"
    );
}

/// The text of an output's stdout and stderr.
fn text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A volume a running container uses is kept by `prune` and refused by
/// `rm`; once the container ends, both remove it.
#[test]
fn a_volume_in_use_is_never_removed() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    // Named, and asleep for longer than the test: it's up for every check
    // (`stop`ped at the end).
    let mut running = start_sleeping_with(
        &store,
        &image,
        &["--name", "keeper", "--volume", "keep:/data"],
        "120",
        1,
    );
    // The run's area comes before its volumes are held (as root, its
    // network is set up in between): until then, there's nothing to prune.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let pruned = shipyard(&store, &["volume", "prune", "-f"]);
        let (stdout, stderr) = text(&pruned);
        assert!(pruned.status.success(), "{stderr}");
        if stdout.contains("Kept 1 in use: keep") {
            break;
        }
        assert!(stdout.contains("Removed 0 volume(s)"), "{stdout}");
        assert!(std::time::Instant::now() < deadline, "{stdout}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let refused = shipyard(&store, &["volume", "rm", "keep"]);
    assert_eq!(refused.status.code(), Some(1));
    // By the container's name (a foreground run with an image has a
    // record).
    assert!(
        text(&refused)
            .1
            .contains("the volume keep is in use by keeper"),
        "{}",
        text(&refused).1
    );
    let stopped = shipyard(&store, &["stop", "-t", "1", "keeper"]);
    assert!(stopped.status.success(), "{}", text(&stopped).1);
    let _ = running.wait().unwrap();
    let removed = shipyard(&store, &["volume", "rm", "keep"]);
    assert!(removed.status.success(), "{}", text(&removed).1);
    assert_eq!(
        text(&shipyard(&store, &["volume", "ls"])).0.lines().count(),
        1
    );
}

/// What a rootless container wrote into a volume as its other users
/// belongs to subordinate IDs on the host: removing the volume goes through
/// the storage helper.
#[test]
fn a_volume_a_rootless_container_wrote_as_another_user_is_removed() {
    use shipyard_test_support::{Rootless, is_root, rootless};
    if is_root() || !rootless(Rootless::Full) {
        return;
    }
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    let wrote = shipyard(
        &store,
        &[
            "run",
            "--volume",
            "theirs:/data",
            &image,
            "sh",
            "-c",
            "mkdir /data/d && touch /data/d/f && chown -R 1000:1000 /data/d && chmod 700 /data/d",
        ],
    );
    assert!(wrote.status.success(), "{}", text(&wrote).1);
    let removed = shipyard(&store, &["volume", "rm", "theirs"]);
    assert!(removed.status.success(), "{}", text(&removed).1);
    let names = |dir: &str| -> Vec<String> {
        std::fs::read_dir(store.store().join(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect()
    };
    let volumes: Vec<String> = names("volumes")
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert!(volumes.is_empty(), "left behind: {volumes:?}");
    assert!(names("volumes/.trash").is_empty(), "trash left behind");
}

/// `rmi` reports an image it can't remove and goes on with the rest,
/// exiting 1.
#[test]
fn rmi_goes_on_past_an_image_it_cant_remove() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    let removed = shipyard(&store, &["rmi", "nosuch", &image]);
    let (stdout, stderr) = text(&removed);
    assert_eq!(removed.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("nosuch"), "{stderr}");
    assert!(stdout.contains(&format!("Untagged: {image}")), "{stdout}");
    assert_eq!(text(&shipyard(&store, &["images"])).0.lines().count(), 1);
}

/// `system df` shows images, containers and volumes.
#[test]
fn system_df_counts_images_containers_and_volumes() {
    let Some((_serving, _image, store)) = ready() else {
        return;
    };
    assert!(
        shipyard(&store, &["volume", "create", "v"])
            .status
            .success()
    );
    let df = shipyard(&store, &["system", "df"]);
    let (stdout, stderr) = text(&df);
    assert!(df.status.success(), "{stderr}");
    let rows: Vec<Vec<&str>> = stdout
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    // Docker's columns (step 11.3c2): how many, how many in use.
    assert_eq!(rows[0], ["TYPE", "TOTAL", "ACTIVE", "SIZE", "RECLAIMABLE"]);
    assert_eq!(rows[1][..3], ["Images", "1", "0"], "{stdout}");
    assert_eq!(rows[2][..3], ["Containers", "0", "0"], "{stdout}");
    assert_eq!(rows[3][..4], ["Local", "Volumes", "1", "0"], "{stdout}");
    let formatted = shipyard(
        &store,
        &["system", "df", "--format", "{{.Type}}={{.TotalCount}}"],
    );
    assert_eq!(
        text(&formatted).0,
        "Images=1\nContainers=0\nLocal Volumes=1\nBuild Cache=0\n"
    );
}

/// A mistake in a flag is found before anything is pulled.
#[test]
fn a_bad_flag_costs_no_pull() {
    let store = Scratch::new();
    let run = shipyard(
        &store,
        &[
            "run",
            "--hostname",
            "bad_name",
            "localhost:1/nothing",
            "true",
        ],
    );
    let (_, stderr) = text(&run);
    assert_eq!(run.status.code(), Some(125), "{stderr}");
    assert!(!stderr.contains("Pulling"), "{stderr}");
    let expected = if cfg!(target_os = "linux") {
        "hostname"
    } else {
        "an image needs Linux"
    };
    assert!(stderr.contains(expected), "{stderr}");
}

/// `volume prune` asks first; with no terminal to ask on, it needs -f.
#[test]
fn volume_prune_without_a_terminal_needs_force() {
    let store = Scratch::new();
    let pruned = shipyard(&store, &["volume", "prune"]);
    assert_eq!(pruned.status.code(), Some(1));
    assert!(text(&pruned).1.contains("-f"), "{}", text(&pruned).1);
}

/// `volume rm` goes on past a name it can't remove; a host directory mounts
/// read-only with `:ro`; `--entrypoint` runs instead of the image's own.
#[test]
fn volumes_and_entrypoints_as_docker_has_them() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    assert!(
        shipyard(&store, &["volume", "create", "a"])
            .status
            .success()
    );
    let removed = shipyard(&store, &["volume", "rm", "missing", "a"]);
    let (stdout, stderr) = text(&removed);
    assert_eq!(removed.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("no such volume: missing"), "{stderr}");
    assert_eq!(stdout.trim(), "a", "the rest still removed");

    let host = Scratch::new();
    std::fs::create_dir_all(&host.0).unwrap();
    let mount = format!("{}:/host:ro", host.0.display());
    let wrote = shipyard(
        &store,
        &["run", "--volume", &mount, &image, "touch", "/host/f"],
    );
    assert!(!wrote.status.success(), "a read-only bind took a write");
    assert!(!host.0.join("f").exists());

    let entry = shipyard(
        &store,
        &["run", "--entrypoint", "/bin/cat", &image, "/marker"],
    );
    assert!(entry.status.success(), "{}", text(&entry).1);
    assert!(text(&entry).0.contains("shipyard test root filesystem"));
}

/// A layer that implies a directory (`work/sub/file`, no `work/sub/`) is
/// extracted for the layers below it, under its name on them (`-on-`), and
/// found there by the next run, rootless (through the storage helper) and
/// as root.
#[test]
fn a_layer_implying_directories_is_placed_on_its_base() {
    let store = Scratch::new();
    let base = RootFs::minimal();
    let (serving, image) = fixture_layers(vec![
        layer_tar(base.path()),
        implying_layer("work/sub/file", b"implied"),
    ]);
    for _ in 0..2 {
        let output = shipyard(&store, &["run", &image, "cat", "/work/sub/file"]);
        if !image_runs_here(&output) {
            return;
        }
        let (stdout, stderr) = text(&output);
        assert_eq!(stdout, "implied", "{stderr}");
    }
    let stacked: Vec<String> = layers(&store)
        .into_iter()
        .filter(|name| name.contains("-on-"))
        .collect();
    assert_eq!(stacked.len(), 1, "{:?}", layers(&store));
    assert_eq!(layers(&store).len(), 2, "{:?}", layers(&store));
    drop(serving);
}

/// More layers than one old-style mount's options hold: the new mount API
/// stacks them all.
#[test]
fn an_image_of_sixty_layers_runs() {
    let (_serving, image) = fixture_with(59);
    let store = Scratch::new();
    let run = shipyard(
        &store,
        &["run", &image, "cat", "/layer-0", "/layer-58", "/top"],
    );
    let (stdout, stderr) = text(&run);
    // Before Linux 6.8, one mount stacks about 50 such layers: refused,
    // saying so.
    let release = shipyard_runtime::kernel::release();
    if shipyard_runtime::kernel::version(&release).unwrap_or_default() < (6, 8) {
        assert!(
            stderr.contains("more than this kernel can stack"),
            "{stderr}"
        );
        return;
    }
    if !image_runs_here(&run) {
        return;
    }
    assert!(run.status.success(), "{stderr}");
    assert_eq!(stdout, "05858", "every layer, the top one over the rest");
}

/// A working directory the image doesn't have is made, as Docker does.
#[test]
fn a_missing_working_directory_is_made() {
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    let run = shipyard(
        &store,
        &[
            "run",
            "-w",
            "/new/dir",
            &image,
            "sh",
            "-c",
            "pwd && stat -c %a .",
        ],
    );
    let (stdout, stderr) = text(&run);
    assert!(run.status.success(), "{stderr}");
    assert_eq!(stdout, "/new/dir\n755\n");
}

/// Root with `--uidmap`: the image's files are the container root's
/// inside (a copy of the layers shifted by the map), and it can change
/// them; the copy goes with the image.
#[test]
#[ignore = "needs root; run via test-linux.sh"]
fn an_image_with_uidmap_belongs_to_the_containers_root() {
    if !shipyard_test_support::is_root() {
        return;
    }
    let Some((_serving, image, store)) = ready() else {
        return;
    };
    let run = shipyard(
        &store,
        &[
            "run",
            "--uidmap",
            "0:100000:65536",
            &image,
            "sh",
            "-c",
            "stat -c %u /marker && touch /marker && echo changed",
        ],
    );
    let (stdout, stderr) = text(&run);
    assert!(run.status.success(), "{stderr}");
    assert_eq!(stdout, "0\nchanged\n");

    // A host directory under /tmp binds in, though the overlay is attached
    // through a staging tmpfs on /tmp.
    let host = Scratch::new();
    std::fs::write(host.0.join("from-host"), "here\n").unwrap();
    let volume = format!("{}:/data:ro", host.0.display());
    let bound = shipyard(
        &store,
        &[
            "run",
            "--uidmap",
            "0:100000:65536",
            "--volume",
            &volume,
            &image,
            "cat",
            "/data/from-host",
        ],
    );
    assert!(bound.status.success(), "{}", text(&bound).1);
    assert_eq!(text(&bound).0, "here\n");
    assert!(
        layers(&store).iter().any(|name| name.contains("-map-")),
        "{:?}",
        layers(&store)
    );
    assert!(shipyard(&store, &["rmi", &image]).status.success());
    assert!(shipyard(&store, &["image", "prune", "-f"]).status.success());
    assert!(layers(&store).is_empty(), "left: {:?}", layers(&store));

    // Maps that leave the container's root out: refused, nothing extracted.
    let (_serving, image) = fixture();
    let refused = shipyard(
        &store,
        &["run", "--uidmap", "1:100000:65536", &image, "true"],
    );
    assert_eq!(refused.status.code(), Some(125), "{}", text(&refused).1);
    assert!(
        !layers(&store).iter().any(|name| name.contains("-map-")),
        "{:?}",
        layers(&store)
    );
}

/// A store on overlayfs can't hold a container's writable layer: the
/// refusal says what to do.
#[test]
#[ignore = "needs root; run via test-linux.sh"]
fn a_store_on_overlayfs_is_explained() {
    let root_is_overlay = std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .any(|line| line.split(' ').nth(4) == Some("/") && line.contains(" - overlay "));
    if !shipyard_test_support::is_root() || !root_is_overlay {
        return;
    }
    let (_serving, image) = fixture();
    let dir = std::path::PathBuf::from(format!(
        "/var/tmp/shipyard-on-overlay-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let run = Command::new(SHIPYARD.path())
        .args(["run", &image, "true"])
        .env("SHIPYARD_IMAGE_STORE", dir.join("store"))
        .output()
        .unwrap();
    let (_, stderr) = text(&run);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(run.status.code(), Some(125), "{stderr}");
    assert!(stderr.contains("SHIPYARD_IMAGE_STORE"), "{stderr}");
}

/// A rootless run with a named volume, where `newuidmap` and `newgidmap`
/// aren't setuid root but have file capabilities (Fedora's): the volume's
/// fill helper, non-dumpable, had its `/proc/PID/uid_map` root's, which such
/// a helper (run as the user) can't open, and every such run was refused
/// (found by 12.5c2's Fedora milestone). The control: the same tools run a
/// container with no volume.
#[test]
#[ignore = "needs root"]
fn a_volume_fills_where_newuidmap_has_file_capabilities() {
    use shipyard_test_support::net::TestUser;
    use std::os::unix::fs::PermissionsExt as _;
    let Some(user) = TestUser::find() else {
        eprintln!("no unprivileged user to run rootless containers as; skipping");
        return;
    };
    let (_serving, image) = shipyard_test_support::registry::fixture();
    let scratch = Scratch::new();
    std::os::unix::fs::chown(&scratch.0, Some(user.uid), Some(user.gid)).unwrap();
    let tools = scratch.0.join("tools");
    std::fs::create_dir(&tools).unwrap();
    std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (name, capability) in [
        ("newuidmap", "cap_setuid+ep"),
        ("newgidmap", "cap_setgid+ep"),
    ] {
        let found = shipyard_sys::path::find_program(
            name,
            Some(std::ffi::OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin")),
        )
        .unwrap();
        let copy = tools.join(name);
        std::fs::copy(&found, &copy).unwrap();
        // No setuid bit: only the file capability, as Fedora's.
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let set = Command::new("setcap")
            .arg(capability)
            .arg(&copy)
            .status()
            .unwrap();
        assert!(set.success(), "setcap {capability} {}", copy.display());
    }
    let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", tools.display());
    let run = |args: &[&str]| {
        user.command(Command::new(SHIPYARD.path()), &scratch.0.join("runtime"))
            .args(args)
            .env("PATH", &path)
            .env("SHIPYARD_IMAGE_STORE", scratch.store())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let plain = run(&[
        "run",
        "--rm",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "exit 0",
    ]);
    assert!(
        plain.status.success(),
        "the control: {}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let filled = run(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--volume",
        "filled:/data",
        &image,
        "sh",
        "-c",
        "exit 0",
    ]);
    assert!(
        filled.status.success(),
        "a volume with these tools: {}",
        String::from_utf8_lossy(&filled.stderr)
    );
}

/// Milestone v0.1, end to end: a normal user's interactive shell in an
/// image pulled from a registry, isolated, resource-limited and networked
/// (`shipyard run --memory 64m IMAGE sh`), on the user's terminal (a
/// pseudo-terminal the test types into, as a person would). Root sets the world up, each test in
/// a network namespace of its own: an [`Upstream`] that's the way out,
/// with a web server and a DNS server on it, and the host's resolver
/// pointed at that DNS server (in a mount namespace of the run's own, so
/// only pasta sees it). The user is [`TestUser`], in a cgroup delegated to
/// it where there's one (`--memory` needs it).
#[test]
#[ignore = "needs root"]
fn the_v0_1_milestone_a_rootless_interactive_shell_in_a_pulled_image_is_networked() {
    use shipyard_test_support::net::{FakeResolver, Server, TestUser, Upstream};
    shipyard_test_support::in_new_netns(|| {
        let Some(user) = TestUser::find() else {
            eprintln!("no unprivileged user to run rootless containers as; skipping");
            return;
        };
        let upstream = Upstream::new();
        upstream.route_default();
        let _resolver = FakeResolver::start(upstream.netns(), Upstream::ADDRESS);
        let _server = Server::start(upstream.netns(), 80, "hello from upstream\n");
        // bash: its `/dev/tcp` is the image's `wget`.
        let root = RootFs::minimal();
        root.add_host_program("bash");
        let (_serving, image) = fixture_of(&root, 0);
        let scratch = Scratch::new();
        std::os::unix::fs::chown(&scratch.0, Some(user.uid), Some(user.gid)).unwrap();
        let resolv = scratch.0.join("resolv.conf");
        std::fs::write(&resolv, format!("nameserver {}\n", Upstream::ADDRESS)).unwrap();
        let cgroup = user.delegated_cgroup();
        let memory: &[&str] = if cgroup.is_some() {
            &["--memory", "64m"]
        } else {
            eprintln!("no cgroup delegated to the user: the run has no --memory");
            &[]
        };
        let mut command =
            user.environment(Command::new(SHIPYARD.path()), &scratch.0.join("runtime"));
        // `-it`: a terminal of the container's own, as Docker's; a container
        // (stage 9, step 9.4c3b), its terminal its monitor's, removed after.
        command
            .args(["run", "-it", "--rm"])
            .args(memory)
            .args([image.as_str(), "sh"])
            .env("SHIPYARD_IMAGE_STORE", scratch.store());
        let mut console = Console::start(command, &user, &resolv, cgroup.as_deref());

        // The image was pulled, and its shell is up: root's prompt.
        console.expect("the shell's prompt", |seen| seen.contains("# "));
        // Its own network, through pasta: a default route...
        console.type_line("cat /proc/net/route");
        console.expect("a default route", |seen| seen.contains("\t00000000\t"));
        // ...and pasta's resolver, which forwards to the host's.
        console.type_line("cat /etc/resolv.conf");
        console.expect("pasta's resolver", |seen| {
            seen.contains("nameserver 169.254.1.1")
        });
        // A name looked up through it, and a server upstream reached.
        console.type_line("bash -c 'exec 3<>/dev/tcp/upstream/80 && cat <&3'");
        console.expect("the upstream server's reply", |seen| {
            seen.contains("hello from upstream")
        });
        // Its own hostname: the run's ID.
        console.type_line("echo host=$(cat /proc/sys/kernel/hostname)");
        console.expect("the run ID as hostname", |seen| {
            // After the prompt: `# host=0123456789ab`.
            seen.lines().any(|line| {
                line.trim_end().rsplit_once("host=").is_some_and(|(_, id)| {
                    id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit())
                })
            })
        });
        // Its memory limit.
        if cgroup.is_some() {
            console.type_line("echo mem=$(cat /sys/fs/cgroup/memory.max)");
            console.expect("the memory limit", |seen| seen.contains("mem=67108864"));
        }
        console.type_line("exit 3");
        assert_eq!(console.wait(), Some(3), "{}", console.seen);
    });
}

/// A command on a pseudo-terminal of its own (its controlling terminal),
/// and everything it has written there.
struct Console {
    leader: std::fs::File,
    child: std::process::Child,
    seen: String,
}

impl Console {
    /// How long anything may take to show up (a pull included).
    const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Starts `command` on a new terminal, as `user`: before it drops to
    /// the user, the child (still root) bind-mounts `resolv` over the
    /// host's `/etc/resolv.conf` in a mount namespace of its own, and moves
    /// into `cgroup`.
    fn start(
        mut command: Command,
        user: &shipyard_test_support::net::TestUser,
        resolv: &std::path::Path,
        cgroup: Option<&std::path::Path>,
    ) -> Console {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::process::CommandExt as _;
        let pty = nix::pty::openpty(None, None).expect("a pseudo-terminal");
        let follower = std::fs::File::from(pty.slave);
        command
            .stdin(follower.try_clone().unwrap())
            .stdout(follower.try_clone().unwrap())
            .stderr(follower);
        // Made here: the child may not allocate.
        let path = |path: &std::path::Path| CString::new(path.as_os_str().as_bytes()).unwrap();
        let resolv = path(resolv);
        let procs = cgroup.map(|dir| path(&dir.join("cgroup.procs")));
        let (uid, gid) = (user.uid, user.gid);
        let check = |result: libc::c_int| {
            if result == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        // SAFETY: the closure runs in the forked child before exec, and
        // calls only async-signal-safe system calls on data prepared above
        // (no allocation, no locks); `last_os_error` only reads errno.
        unsafe {
            command.pre_exec(move || {
                check(libc::setsid())?;
                check(libc::ioctl(0, libc::TIOCSCTTY, 0))?;
                check(libc::unshare(libc::CLONE_NEWNS))?;
                check(libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ))?;
                check(libc::mount(
                    resolv.as_ptr(),
                    c"/etc/resolv.conf".as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ))?;
                if let Some(procs) = &procs {
                    let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    check(fd)?;
                    let written = libc::write(fd, c"0".as_ptr().cast(), 1);
                    libc::close(fd);
                    if written != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                check(libc::setgroups(1, &raw const gid))?;
                check(libc::setresgid(gid, gid, gid))?;
                check(libc::setresuid(uid, uid, uid))
            });
        }
        let child = command.spawn().expect("starting shipyard");
        // Our copies of the terminal go: the child's last close is its end.
        drop(command);
        Console {
            leader: std::fs::File::from(pty.master),
            child,
            seen: String::new(),
        }
    }

    /// Types `line` and Enter.
    fn type_line(&mut self, line: &str) {
        use std::io::Write as _;
        self.leader
            .write_all(format!("{line}\n").as_bytes())
            .expect("typing");
    }

    /// Reads what's there (up to a tenth of a second's wait); false once
    /// the terminal has no other end.
    fn read(&mut self) -> bool {
        use std::io::Read as _;
        use std::os::fd::AsFd as _;
        let mut fds = [nix::poll::PollFd::new(
            self.leader.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        if nix::poll::poll(&mut fds, 100u16).unwrap_or(0) == 0 {
            return true;
        }
        let mut buffer = [0u8; 4096];
        match self.leader.read(&mut buffer) {
            Ok(0) | Err(_) => false,
            Ok(read) => {
                self.seen
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
                true
            }
        }
    }

    /// Waits until what's been written satisfies `done`.
    fn expect(&mut self, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = std::time::Instant::now() + Self::WAIT;
        while !done(&self.seen) {
            assert!(
                std::time::Instant::now() < deadline && self.read(),
                "no {what} on the terminal:\n{}",
                self.seen
            );
        }
    }

    /// Its exit code, once it has ended (reading the terminal meanwhile,
    /// so it's never blocked writing).
    fn wait(&mut self) -> Option<i32> {
        let deadline = std::time::Instant::now() + Self::WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("waiting") {
                return status.code();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "it never ended:\n{}",
                self.seen
            );
            if !self.read() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
