//! End-to-end `build` (step 12.3): a Dockerfile built by BuildKit in a
//! container of its own, root and rootless; its cache kept per context and
//! pruned; what's refused before anything runs; a failed build, a killed
//! one; `-o`; a secret kept out of the image.
//!
//! These need the network: BuildKit's image (fetched once per target
//! directory, then loaded from an archive kept there) and the base image
//! (`busybox`, pulled by BuildKit). Without it they say so and return,
//! unless `SHIPYARD_TEST_NETWORK=required` (as `test-linux.sh` sets).

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use shipyard_test_support::push_registry::{Auth, PushRegistry};
use shipyard_test_support::registry::Scratch;
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

/// BuildKit's image, as the engine pins it.
const BUILDKIT: &str =
    "moby/buildkit:v0.34.1@sha256:99368f3e46177c6405b9740e4461d91cee004128daeecca78d4ceac50ec1da5c";

/// The base image the tests' Dockerfiles build on, pinned.
const BASE: &str = "busybox:1.37";

/// A store with BuildKit's image in it, ready to build, if the network
/// (once) lets it be had: loaded from an archive kept in cargo's temporary
/// directory, made from a pull the first time.
fn ready() -> Option<Scratch> {
    if std::env::var("SHIPYARD_TEST_BUILD").as_deref() == Ok("later") {
        // `test-linux.sh`'s main passes: these run on their own after them.
        eprintln!("skipping: the build tests run on their own (SHIPYARD_TEST_BUILD=later)");
        return None;
    }
    let archive = Path::new(env!("CARGO_TARGET_TMPDIR")).join("buildkit-v0.34.1.tar");
    let store = Scratch::new();
    if !archive.exists() {
        // One test makes it (a lock against the others), then all load it.
        let lock = std::fs::File::create(archive.with_extension("lock")).unwrap();
        lock.lock().unwrap();
        if !archive.exists() {
            let pulled = shipyard(&store, &["pull", "-q", BUILDKIT]);
            if !pulled.status.success() {
                assert!(
                    std::env::var("SHIPYARD_TEST_NETWORK").as_deref() != Ok("required"),
                    "the network is required here (SHIPYARD_TEST_NETWORK): {}",
                    text(&pulled).1
                );
                eprintln!("skipping: no network for BuildKit's image");
                return None;
            }
            let partial = archive.with_extension("partial");
            ok(&store, &["save", "-o", partial.to_str().unwrap(), BUILDKIT]);
            std::fs::rename(&partial, &archive).unwrap();
            return Some(store);
        }
    }
    ok(&store, &["load", "-q", "-i", archive.to_str().unwrap()]);
    Some(store)
}

/// A context directory with `files` (path, contents), its Dockerfile among
/// them; readable to every builder (as root, the builder's root is another
/// ID).
fn context(files: &[(&str, &str)]) -> Scratch {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Scratch::new();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut parent = dir.0.parent();
    // Its parents enterable by the builder's ID too.
    while let Some(up) = parent {
        if up == Path::new("/tmp") || up == Path::new("/") {
            break;
        }
        let _ = std::fs::set_permissions(up, std::fs::Permissions::from_mode(0o755));
        parent = up.parent();
    }
    for (path, contents) in files {
        let path = dir.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    dir
}

/// `build ARGS… CONTEXT` on `store`.
fn build(store: &Scratch, args: &[&str], context: &Path) -> Output {
    let mut all: Vec<&str> = args.to_vec();
    all.push(context.to_str().unwrap());
    shipyard(store, &all)
}

/// The builders left (containers labelled as a build's).
fn builders(store: &Scratch) -> String {
    ok(
        store,
        &["ps", "-a", "-q", "-f", "label=com.shipyard.builder"],
    )
}

/// A multi-stage Dockerfile with a build arg, a cache mount and a secret
/// builds into a named image that runs; the secret reached its `RUN` (its
/// hash is right: the positive control) and is in no layer.
#[test]
fn a_dockerfile_builds_and_runs() {
    let Some(store) = ready() else { return };
    let dockerfile = format!(
        "FROM {BASE} AS base\nARG GREETING=hi\nRUN echo \"$GREETING\" > /greeting\n\
         FROM {BASE}\nCOPY --from=base /greeting /greeting\nCOPY hello.txt /hello.txt\n\
         RUN --mount=type=cache,target=/cache sh -c 'date >> /cache/stamps'\n\
         RUN --mount=type=secret,id=token sh -c 'sha256sum /run/secrets/token | cut -c1-16 > /secret-hash'\n\
         CMD [\"cat\", \"/greeting\", \"/hello.txt\", \"/secret-hash\"]\n"
    );
    let ctx = context(&[("Dockerfile", &dockerfile), ("hello.txt", "hello\n")]);
    let secret = store.0.join("token");
    std::fs::write(&secret, "s3cret-value\n").unwrap();
    let source = format!("id=token,src={}", secret.display());
    let output = build(
        &store,
        &[
            "build",
            "-t",
            "built:1",
            "--build-arg",
            "GREETING=bonjour",
            "--secret",
            &source,
        ],
        &ctx.0,
    );
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("Built built:1"), "{stdout}");
    let ran = ok(&store, &["run", "--rm", "--pull", "never", "built:1"]);
    let wanted = {
        use std::io::Write as _;
        let mut digest = Command::new("sha256sum")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        digest
            .stdin
            .take()
            .unwrap()
            .write_all(b"s3cret-value\n")
            .unwrap();
        String::from_utf8(digest.wait_with_output().unwrap().stdout).unwrap()[..16].to_owned()
    };
    assert_eq!(
        ran.lines().collect::<Vec<_>>(),
        ["bonjour", "hello", &wanted]
    );
    // In no layer: the image saved, its bytes searched.
    let saved = store.0.join("built.tar");
    ok(&store, &["save", "-o", saved.to_str().unwrap(), "built:1"]);
    let mut layers = Vec::new();
    for entry in tar::Archive::new(std::fs::File::open(&saved).unwrap())
        .entries()
        .unwrap()
    {
        let mut entry = entry.unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        let mut decoded = Vec::new();
        if std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut decoded)
            .is_ok()
        {
            layers.extend(decoded);
        }
        layers.extend(bytes);
    }
    assert!(
        !layers.windows(12).any(|window| window == b"s3cret-value"),
        "the secret is in the image"
    );
    assert_eq!(builders(&store).trim(), "", "a builder left");
}

/// The v0.4 milestone, as a user runs it: an image built, tagged for a
/// registry, pushed, then pulled by its digest into another store, where it
/// runs; its ID the same everywhere.
#[test]
fn a_built_image_is_tagged_pushed_and_runs_elsewhere() {
    let Some(store) = ready() else { return };
    let registry = PushRegistry::start(Auth::None);
    let dockerfile =
        format!("FROM {BASE}\nRUN echo built-here > /made\nCMD [\"cat\", \"/made\"]\n");
    let ctx = context(&[("Dockerfile", &dockerfile)]);
    let built = build(&store, &["build", "-q", "-t", "milestone:1"], &ctx.0);
    let (stdout, stderr) = text(&built);
    assert!(built.status.success(), "{stderr}");
    let id = stdout.trim().to_owned();
    assert!(id.starts_with("sha256:"), "{stdout}");
    let name = format!("{}/milestone:1", registry.base);
    ok(&store, &["tag", "milestone:1", &name]);
    let pushed = ok(&store, &["push", &name]);
    assert!(
        pushed.starts_with(&format!("1: digest: {id} size: ")),
        "{pushed}"
    );
    let elsewhere = Scratch::new();
    let by_digest = format!("{}/milestone@{id}", registry.base);
    ok(&elsewhere, &["pull", "-q", &by_digest]);
    let ran = ok(&elsewhere, &["run", "--rm", "--pull", "never", &by_digest]);
    assert_eq!(ran, "built-here\n");
    assert_eq!(
        ok(
            &elsewhere,
            &["image", "inspect", "--format", "{{.Id}}", &by_digest]
        )
        .trim(),
        id,
        "the same image"
    );
}

/// The cache is the context's, kept: a second build's steps are cached;
/// `system df` counts it; `builder prune -f` empties it.
#[test]
fn the_build_cache_is_kept_and_pruned() {
    let Some(store) = ready() else { return };
    let ctx = context(&[(
        "Dockerfile",
        &format!("FROM {BASE}\nRUN echo once > /once\n"),
    )]);
    let first = build(&store, &["build", "-t", "cached:1"], &ctx.0);
    assert!(first.status.success(), "{}", text(&first).1);
    let second = build(&store, &["build", "-t", "cached:2"], &ctx.0);
    let (_, stderr) = text(&second);
    assert!(second.status.success(), "{stderr}");
    // Its `RUN` step's own line: cached, not run again.
    let step = stderr
        .lines()
        .position(|line| line.contains("RUN echo once > /once"))
        .unwrap_or_else(|| panic!("no RUN step: {stderr}"));
    assert!(
        stderr
            .lines()
            .skip(step)
            .take(3)
            .any(|line| line.ends_with("CACHED")),
        "{stderr}"
    );
    let df = ok(
        &store,
        &["system", "df", "--format", "{{.Type}} {{.TotalCount}}"],
    );
    assert!(df.lines().any(|line| line == "Build Cache 1"), "{df}");
    // A prune with filters leaves it (a cache has no labels to match).
    ok(
        &store,
        &["system", "prune", "-f", "--filter", "label=nothing=here"],
    );
    let df = ok(
        &store,
        &["system", "df", "--format", "{{.Type}} {{.TotalCount}}"],
    );
    assert!(df.lines().any(|line| line == "Build Cache 1"), "{df}");
    let pruned = ok(&store, &["builder", "prune", "-f"]);
    assert!(pruned.contains("Deleted: state-"), "{pruned}");
    let df = ok(
        &store,
        &["system", "df", "--format", "{{.Type}} {{.TotalCount}}"],
    );
    assert!(df.lines().any(|line| line == "Build Cache 0"), "{df}");
    // Another context's cache unused for a week goes at the next build; one
    // used a day ago stays (the control).
    let dir = builder_dir(&store);
    let days_ago = |days: u64| std::time::SystemTime::now() - Duration::from_hours(days * 24);
    for (state, when) in [
        ("state-00000000000000aa", days_ago(30)),
        ("state-00000000000000bb", days_ago(1)),
    ] {
        std::fs::create_dir(dir.join(state)).unwrap();
        std::fs::write(dir.join(state).join("cache"), b"cached").unwrap();
        std::fs::write(dir.join(format!("{state}.lock")), b"").unwrap();
        for path in [dir.join(state), dir.join(format!("{state}.lock"))] {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
    }
    let third = build(&store, &["build", "-t", "cached:3"], &ctx.0);
    let (_, stderr) = text(&third);
    assert!(third.status.success(), "{stderr}");
    assert!(
        stderr.contains("removed the build cache of 1 other build context unused for a week"),
        "{stderr}"
    );
    assert!(!dir.join("state-00000000000000aa").exists());
    assert!(dir.join("state-00000000000000bb/cache").exists());
}

/// The builder's directory: in the store, or beside it as root.
fn builder_dir(store: &Scratch) -> PathBuf {
    let mut beside = store.store().into_os_string();
    beside.push("-builder");
    let beside = PathBuf::from(beside);
    if beside.is_dir() {
        beside
    } else {
        store.store().join("builder")
    }
}

/// An image only the store has is built from, never pulled: `FROM` it and
/// `COPY --from` it, by its name and by Docker Hub's long form of it. With
/// `--pull` (Docker's rule: each base image fetched again) it's asked of
/// the registry, which has none: the control that the first build's came
/// from the store.
#[test]
fn a_local_image_is_built_from_without_a_pull() {
    let Some(store) = ready() else { return };
    let base = context(&[(
        "Dockerfile",
        &format!("FROM {BASE}\nRUN echo local > /local\n"),
    )]);
    let made = build(&store, &["build", "-t", "shipyard-local-base:1"], &base.0);
    assert!(made.status.success(), "{}", text(&made).1);
    let on_it = context(&[(
        "Dockerfile",
        "FROM shipyard-local-base:1 AS first\n\
         FROM docker.io/library/shipyard-local-base:1\n\
         COPY --from=first /local /again\n\
         COPY --from=shipyard-local-base:1 /local /third\n",
    )]);
    let built = build(&store, &["build", "-t", "on-local:1"], &on_it.0);
    assert!(built.status.success(), "{}", text(&built).1);
    let ran = ok(
        &store,
        &[
            "run",
            "--rm",
            "on-local:1",
            "cat",
            "/local",
            "/again",
            "/third",
        ],
    );
    assert_eq!(ran, "local\nlocal\nlocal\n");
    let pulled = build(&store, &["build", "--pull", "-t", "on-local:2"], &on_it.0);
    let (_, stderr) = text(&pulled);
    assert_eq!(pulled.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("shipyard-local-base"), "{stderr}");
}

/// No `-t` and no `-o`: refused before anything runs (no builder made,
/// nothing pulled).
#[test]
fn a_build_without_a_name_runs_nothing() {
    let store = Scratch::new();
    let ctx = context(&[("Dockerfile", "FROM busybox\n")]);
    let output = build(&store, &["build"], &ctx.0);
    let (_, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("give the image a name"), "{stderr}");
    assert_eq!(ok(&store, &["images", "-q"]).trim(), "", "nothing pulled");
    assert_eq!(builders(&store).trim(), "");
}

/// A failing `RUN`: exit 1, BuildKit's words said, no image, no builder
/// and no build directory left.
#[test]
fn a_failing_build_leaves_nothing() {
    let Some(store) = ready() else { return };
    let ctx = context(&[(
        "Dockerfile",
        &format!("FROM {BASE}\nRUN echo boom >&2; exit 3\n"),
    )]);
    let output = build(&store, &["build", "-t", "bad:1"], &ctx.0);
    let (_, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("the build failed"), "{stderr}");
    assert!(stderr.contains("exit code: 3"), "{stderr}");
    assert!(!ok(&store, &["images", "--format", "{{.Repository}}"]).contains("bad"));
    assert_eq!(builders(&store).trim(), "");
    let left: Vec<String> = builder_dirs(&store)
        .into_iter()
        .filter(|name| name.contains(".run-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// The builder's directory's entries (wherever it is: in the store, or
/// beside it as root).
fn builder_dirs(store: &Scratch) -> Vec<String> {
    let inside = store.store().join("builder");
    let mut beside = store.store().into_os_string();
    beside.push("-builder");
    [inside, PathBuf::from(beside)]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// `-o` writes the build's files, never an image: `local` as the caller's
/// own (a file the build gave another owner too), `tar` to a file.
#[test]
fn a_build_s_files_come_out() {
    use std::os::unix::fs::MetadataExt as _;
    let Some(store) = ready() else { return };
    let ctx = context(&[(
        "Dockerfile",
        &format!("FROM {BASE}\nRUN echo made > /made && chown 1000:1000 /made\n"),
    )]);
    let dest = store.0.join("files");
    let local = build(
        &store,
        &[
            "build",
            "-o",
            &format!("type=local,dest={}", dest.display()),
        ],
        &ctx.0,
    );
    assert!(local.status.success(), "{}", text(&local).1);
    let made = std::fs::metadata(dest.join("made")).unwrap();
    assert_eq!(made.uid(), nix::unistd::getuid().as_raw(), "the caller's");
    assert_eq!(
        std::fs::read_to_string(dest.join("made")).unwrap(),
        "made\n"
    );
    let tar = store.0.join("files.tar");
    let packed = build(
        &store,
        &["build", "-o", &format!("type=tar,dest={}", tar.display())],
        &ctx.0,
    );
    assert!(packed.status.success(), "{}", text(&packed).1);
    let names: Vec<String> = tar::Archive::new(std::fs::File::open(&tar).unwrap())
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(names.iter().any(|name| name == "made"), "{names:?}");
    assert_eq!(
        ok(&store, &["images", "-q"]).lines().count(),
        1,
        "only BuildKit's"
    );
}

/// A build whose CLI was killed leaves its builder running: the next build
/// of that context, started while it still runs, removes it and builds.
#[test]
fn a_killed_build_leaves_the_next_free() {
    let Some(store) = ready() else { return };
    let ctx = context(&[(
        "Dockerfile",
        &format!("FROM {BASE}\nARG WAIT=0\nRUN sleep $WAIT\n"),
    )]);
    let mut killed = store
        .on(&mut Command::new(SHIPYARD.path()))
        .args(["build", "-q", "-t", "slow:1", "--build-arg", "WAIT=120"])
        .arg(&ctx.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    shipyard_test_support::eventually_every(
        std::time::Duration::from_secs(120),
        std::time::Duration::from_millis(200),
        "the builder there",
        || !builders(&store).trim().is_empty(),
    );
    killed.kill().unwrap();
    killed.wait().unwrap();
    assert!(!builders(&store).trim().is_empty(), "the orphan still runs");
    let next = build(&store, &["build", "-q", "-t", "fast:1"], &ctx.0);
    assert!(next.status.success(), "{}", text(&next).1);
    assert_eq!(builders(&store).trim(), "");
    let left: Vec<String> = builder_dirs(&store)
        .into_iter()
        .filter(|name| name.contains(".run-"))
        .collect();
    assert!(left.is_empty(), "the killed build's directory: {left:?}");
}

/// A killed build's builder is its build's: stopped (as one is between its
/// create and its start, or after its end), `container prune` leaves it (a
/// stopped container beside it goes: the control); `builder prune` removes
/// it with the cache it was using.
#[test]
fn a_killed_build_s_builder_goes_with_its_cache() {
    let Some(store) = ready() else { return };
    let ctx = context(&[("Dockerfile", &format!("FROM {BASE}\nRUN sleep 120\n"))]);
    let mut killed = store
        .on(&mut Command::new(SHIPYARD.path()))
        .args(["build", "-q", "-t", "slow:1"])
        .arg(&ctx.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Running (listed once made, before it's started).
    let running = || ok(&store, &["ps", "-q", "-f", "label=com.shipyard.builder"]);
    let builder = shipyard_test_support::eventually_some(
        Duration::from_secs(120),
        Duration::from_millis(200),
        "the builder running",
        || Some(running().trim().to_owned()).filter(|id| !id.is_empty()),
    );
    killed.kill().unwrap();
    killed.wait().unwrap();
    // Stopped, as a builder is between its create and its start, or after
    // its end (container prune takes only stopped containers).
    ok(&store, &["kill", &builder]);
    shipyard_test_support::eventually(Duration::from_secs(30), "the builder stopped", || {
        running().trim().is_empty()
    });
    let stopped = ok(
        &store,
        &[
            "create", "--pull", "never", "--name", "stopped", BUILDKIT, "true",
        ],
    );
    assert!(!stopped.trim().is_empty());
    let pruned = ok(&store, &["container", "prune", "-f"]);
    assert!(pruned.contains(stopped.trim()), "the control: {pruned}");
    assert!(
        !builders(&store).trim().is_empty(),
        "container prune took the builder"
    );
    ok(&store, &["builder", "prune", "-f"]);
    assert_eq!(
        builders(&store).trim(),
        "",
        "the builder outlived its cache"
    );
    let left: Vec<String> = builder_dirs(&store)
        .into_iter()
        .filter(|name| Path::new(name).extension().is_none_or(|end| end != "lock"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// As root, a context file the builder's root (IDs from 2100000000) can't
/// read is refused before anything runs, said (BuildKit itself would copy
/// it empty: found in 12.3); the control: the same context, the file made
/// readable, builds, and the file is copied whole.
#[test]
#[ignore = "needs root"]
fn an_unreadable_context_file_is_said() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(store) = ready() else { return };
    let ctx = context(&[
        (
            "Dockerfile",
            &format!("FROM {BASE}\nCOPY closed.txt /closed.txt\nCMD [\"cat\", \"/closed.txt\"]\n"),
        ),
        ("closed.txt", "closed\n"),
    ]);
    let mode = |mode| {
        std::fs::set_permissions(
            ctx.0.join("closed.txt"),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap();
    };
    mode(0o600);
    let refused = build(&store, &["build", "-t", "closed:1"], &ctx.0);
    let (_, stderr) = text(&refused);
    assert_eq!(refused.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("closed.txt isn't readable to the builder"),
        "{stderr}"
    );
    assert_eq!(builders(&store).trim(), "", "refused before a builder");
    // The context's own directory too, not only what's in it.
    mode(0o644);
    std::fs::set_permissions(&ctx.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let closed_dir = build(&store, &["build", "-t", "closed:2"], &ctx.0);
    let (_, stderr) = text(&closed_dir);
    assert_eq!(closed_dir.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("the build context itself isn't readable"),
        "{stderr}"
    );
    // Readable but not enterable (o+r, no o+x): the builder can't list it.
    std::fs::set_permissions(&ctx.0, std::fs::Permissions::from_mode(0o744)).unwrap();
    let unenterable = build(&store, &["build", "-t", "closed:3"], &ctx.0);
    let (_, stderr) = text(&unenterable);
    assert_eq!(unenterable.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("the build context itself isn't readable"),
        "{stderr}"
    );
    // A context under a directory the builder can't enter.
    std::fs::set_permissions(&ctx.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inner = ctx.0.join("inner");
    std::fs::create_dir(&inner).unwrap();
    std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(inner.join("Dockerfile"), format!("FROM {BASE}\n")).unwrap();
    std::fs::set_permissions(
        inner.join("Dockerfile"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let under = build(&store, &["build", "-t", "closed:4"], &inner);
    let (_, stderr) = text(&under);
    assert_eq!(under.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("which the builder"), "{stderr}");
    assert!(stderr.contains("can't enter"), "{stderr}");
    std::fs::remove_dir_all(&inner).unwrap();
    std::fs::set_permissions(&ctx.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let control = build(&store, &["build", "-t", "open:1"], &ctx.0);
    assert!(control.status.success(), "{}", text(&control).1);
    assert_eq!(
        ok(&store, &["run", "--rm", "open:1"]),
        "closed\n",
        "copied whole"
    );
}

/// A Ctrl-C mid-build (SIGINT to `shipyard`): the builder goes, and so
/// does the build's directory with its copied secret, before the signal
/// ends the command.
#[test]
fn an_interrupted_build_leaves_nothing() {
    let Some(store) = ready() else { return };
    let ctx = context(&[(
        "Dockerfile",
        &format!("FROM {BASE}\nRUN --mount=type=secret,id=token sleep 120\n"),
    )]);
    let secret = store.0.join("token");
    std::fs::write(&secret, "kept-out\n").unwrap();
    let mut running = store
        .on(&mut Command::new(SHIPYARD.path()))
        .args(["build", "-q", "-t", "stopped:1", "--secret"])
        .arg(format!("id=token,src={}", secret.display()))
        .arg(&ctx.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    shipyard_test_support::eventually_every(
        std::time::Duration::from_secs(120),
        std::time::Duration::from_millis(200),
        "the builder there",
        || !builders(&store).trim().is_empty(),
    );
    let pid = nix::unistd::Pid::from_raw(i32::try_from(running.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
    let asked = Instant::now();
    let ended = running.wait().unwrap();
    assert!(!ended.success(), "it ended for the signal");
    // At once, not when the step's two minutes are up.
    assert!(
        asked.elapsed() < Duration::from_secs(60),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(builders(&store).trim(), "", "a builder left");
    let left: Vec<String> = builder_dirs(&store)
        .into_iter()
        .filter(|name| name.contains(".run-"))
        .collect();
    assert!(
        left.is_empty(),
        "the build's directory (its secret): {left:?}"
    );
}
