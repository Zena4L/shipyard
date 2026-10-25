//! End-to-end `push`, `login` and `logout` (step 12.4) against the
//! in-memory registry of `shipyard-test-support` (`push_registry`): an
//! image pushed and pulled back into another store keeps its ID; a
//! sign-in is checked, kept where Docker keeps it (a `DOCKER_CONFIG` of
//! the test's own) and removed; a credential handed on descriptor 3 (as the
//! Mac's agent hands one) signs a pull in.
//!
//! Linux only: a pull extracts its layers. As tester and as root.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use shipyard_test_support::push_registry::{Auth, PushRegistry};
use shipyard_test_support::registry::{Scratch, fixture};
use shipyard_test_support::{Shipyard, text};

/// A password no other word holds, so its absence from what's said means
/// something.
const SECRET: &str = "s3cret-4821";

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `command` on `store`, with a Docker config directory of its own (never
/// the host's).
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
        .env("DOCKER_CONFIG", docker_config(store))
}

fn docker_config(store: &Scratch) -> PathBuf {
    store.0.join("docker")
}

fn shipyard(store: &Scratch, args: &[&str]) -> Output {
    on(store, &mut Command::new(SHIPYARD.path()))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running shipyard")
}

/// `shipyard ARGS` with `input` on its standard input.
fn shipyard_given(store: &Scratch, args: &[&str], input: &str) -> Output {
    let mut child = on(store, &mut Command::new(SHIPYARD.path()))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    child.wait_with_output().unwrap()
}

fn ok(store: &Scratch, args: &[&str]) -> String {
    let output = shipyard(store, args);
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{args:?}: {stderr}");
    stdout
}

/// The ID of the image `name` in `store`.
fn id_of(store: &Scratch, name: &str) -> String {
    ok(store, &["image", "inspect", "--format", "{{.Id}}", name])
        .trim()
        .to_owned()
}

fn basic(password: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("me:{password}"))
    )
}

/// A store holding the fixture image, tagged `{registry}/app:1`.
fn with_image(registry: &PushRegistry) -> (Scratch, String, impl Drop) {
    let (serving, image) = fixture();
    let store = Scratch::new();
    ok(&store, &["pull", &image]);
    let name = format!("{}/app:1", registry.base);
    ok(&store, &["tag", &image, &name]);
    (store, name, serving)
}

/// A pushed image pulls back into another store with its ID; pushed
/// again, its blobs are already there; `-a` pushes every tag; Docker's
/// last line for each.
#[test]
fn a_pushed_image_pulls_back_the_same() {
    let registry = PushRegistry::start(Auth::None);
    let (store, name, _serving) = with_image(&registry);
    let id = id_of(&store, &name);
    let pushed = shipyard(&store, &["push", &name]);
    let (stdout, stderr) = text(&pushed);
    assert!(pushed.status.success(), "{stderr}");
    assert!(
        stdout.starts_with(&format!("1: digest: {id} size: ")),
        "{stdout}"
    );
    assert!(stderr.contains(": pushed ("), "{stderr}");
    let back = Scratch::new();
    ok(&back, &["pull", &name]);
    assert_eq!(id_of(&back, &name), id, "the same image");
    let again = shipyard(&store, &["push", &name]);
    assert!(
        text(&again).1.contains(": already there"),
        "{}",
        text(&again).1
    );
    ok(&store, &["tag", &name, &format!("{}/app:2", registry.base)]);
    let all = ok(&store, &["push", "-a", &format!("{}/app", registry.base)]);
    assert_eq!(all.lines().count(), 2, "{all}");
    assert!(
        registry
            .state()
            .manifests
            .contains_key(&("app".to_owned(), "2".to_owned()))
    );
    // What isn't in the store is said, nothing sent.
    let missing = shipyard(&store, &["push", &format!("{}/nothing:1", registry.base)]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        text(&missing).1.contains("isn't in the image store"),
        "{}",
        text(&missing).1
    );
}

/// `login` checks the credential first (a wrong one: refused, nothing
/// kept), then keeps it in Docker's config (warned: unencrypted); a push
/// signs in with it; `logout` removes it, and a push is then refused.
#[test]
fn a_sign_in_is_kept_and_removed() {
    let registry = PushRegistry::start(Auth::Basic(basic(SECRET)));
    let (store, name, _serving) = with_image(&registry);
    let login = [
        "login",
        "-u",
        "me",
        "--password-stdin",
        registry.base.as_str(),
    ];
    let refused = shipyard_given(&store, &login, "wrong\n");
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused).1);
    assert!(
        text(&refused).1.contains("unauthorized"),
        "{}",
        text(&refused).1
    );
    assert!(
        !docker_config(&store).join("config.json").exists(),
        "nothing kept"
    );
    let signed = shipyard_given(&store, &login, "s3cret-4821\n");
    let (stdout, stderr) = text(&signed);
    assert!(signed.status.success(), "{stderr}");
    assert_eq!(stdout, "Login Succeeded\n");
    assert!(stderr.contains("stored unencrypted"), "{stderr}");
    assert!(!stderr.contains(SECRET), "no secret said: {stderr}");
    ok(&store, &["push", &name]);
    let out = ok(&store, &["logout", &registry.base]);
    assert_eq!(
        out,
        format!("Removing login credentials for {}\n", registry.base)
    );
    let unsigned = shipyard(&store, &["push", &name]);
    assert_eq!(unsigned.status.code(), Some(1), "{}", text(&unsigned).1);
    assert_eq!(
        ok(&store, &["logout", &registry.base]),
        format!("Not logged in to {}\n", registry.base)
    );
    // A password on the command line works, and is warned of.
    let argv = shipyard(&store, &["login", "-u", "me", "-p", SECRET, &registry.base]);
    assert!(argv.status.success(), "{}", text(&argv).1);
    assert!(
        text(&argv).1.contains("use --password-stdin"),
        "{}",
        text(&argv).1
    );
}

/// A credential handed on descriptor 3 (`SHIPYARD_REGISTRY_AUTH_FD`, as
/// the Mac's agent hands one) signs a pull in, with no Docker config; the
/// control: the same pull without it is refused. A variable naming any
/// other descriptor is refused before anything runs.
#[test]
fn a_handed_credential_signs_a_pull_in() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    let registry = PushRegistry::start(Auth::Basic(basic("pw")));
    let (store, name, _serving) = with_image(&registry);
    let given = shipyard_given(
        &store,
        &[
            "login",
            "-u",
            "me",
            "--password-stdin",
            registry.base.as_str(),
        ],
        "pw\n",
    );
    assert!(given.status.success());
    ok(&store, &["push", &name]);
    let fresh = Scratch::new();
    let unsigned = shipyard(&fresh, &["pull", &name]);
    assert_eq!(
        unsigned.status.code(),
        Some(1),
        "the control: refused without it"
    );
    assert!(
        text(&unsigned)
            .1
            .contains("the registry asks for a username and password"),
        "the control, refused by the registry (not by the test's setup): {}",
        text(&unsigned).1
    );
    let auth = {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
            r#"{{"username":"me","password":"pw","serveraddress":"{}"}}"#,
            registry.base
        ))
    };
    let (read, mut write) = std::io::pipe().unwrap();
    write.write_all(auth.as_bytes()).unwrap();
    drop(write);
    let from = read.as_raw_fd();
    let mut command = Command::new(SHIPYARD.path());
    on(&fresh, &mut command)
        .args(["pull", &name])
        .env("SHIPYARD_REGISTRY_AUTH_FD", "3")
        .stdin(Stdio::null());
    // SAFETY: `dup2` and `fcntl` only, between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if from != 3 && libc::dup2(from, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let pulled = command.output().unwrap();
    assert!(pulled.status.success(), "{}", text(&pulled).1);
    let other = on(&fresh, &mut Command::new(SHIPYARD.path()))
        .args(["pull", &name])
        .env("SHIPYARD_REGISTRY_AUTH_FD", "4")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(other.status.code(), Some(125), "{}", text(&other).1);
}
