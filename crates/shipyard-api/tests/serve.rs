//! The API served end to end, over a real socket (step 11.2a): what a
//! client sees of the protocol (statuses, Docker's headers and refusals, the
//! body bound) and the idle exit. On macOS too: none of it reaches a
//! container (the CLI's tests drive containers through it on Linux).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use shipyard_test_support::api;

/// A service on a socket of its own, served on a thread: its socket, and
/// how its serving ended (once it has).
struct Served {
    socket: PathBuf,
    ended: mpsc::Receiver<std::io::Result<()>>,
    _dir: Scratch,
}

/// A scratch directory, removed after.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn serve(idle: Duration) -> Served {
    let dir = std::env::temp_dir().join(format!(
        "shipyard-api-serve-{}-{:x}",
        std::process::id(),
        rand_word()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let service = shipyard_api::Service {
        paths: shipyard_engine::Paths::from_env(),
        idle,
        build: shipyard_api::Build {
            version: "0.3.0".to_owned(),
            revision: "abc1234".to_owned(),
        },
    };
    let (done, ended) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(shipyard_api::serve(listener, service));
    });
    Served {
        socket,
        ended,
        _dir: Scratch(dir),
    }
}

/// A word that differs between the tests running at once.
fn rand_word() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u8(0);
    hasher.finish()
}

/// `_ping` answers `OK`, by GET and by HEAD (no body), with Docker's
/// headers: the API version a client negotiates down to.
#[test]
fn ping_answers_with_the_api_version() {
    let served = serve(Duration::from_secs(60));
    let ping = api::get(&served.socket, "/_ping").unwrap();
    assert_eq!(ping.status, 200);
    assert_eq!(ping.text(), "OK");
    assert_eq!(ping.header("Api-Version"), Some("1.43"));
    assert_eq!(ping.header("Ostype"), Some("linux"));
    assert_eq!(ping.header("Docker-Experimental"), Some("false"));
    assert_eq!(ping.header("Server"), Some("Shipyard/0.3.0 (linux)"));
    let head = api::request(&served.socket, "HEAD", "/_ping", None).unwrap();
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(head.header("Api-Version"), Some("1.43"));
}

/// `/version` says the API's versions and this build's.
#[test]
fn version_names_the_engine_and_its_api() {
    let served = serve(Duration::from_secs(60));
    let version = api::get(&served.socket, "/v1.43/version").unwrap();
    assert_eq!(version.status, 200);
    let json = version.json();
    assert_eq!(json["ApiVersion"], "1.43");
    assert_eq!(json["MinAPIVersion"], "1.24");
    assert_eq!(json["Version"], "0.3.0");
    assert_eq!(json["GitCommit"], "abc1234");
    assert_eq!(json["Os"], "linux");
    assert_eq!(json["Platform"]["Name"], "Shipyard");
    assert_eq!(json["Components"][0]["Name"], "Engine");
    assert_eq!(json["Components"][0]["Details"]["ApiVersion"], "1.43");
}

/// Refusals are Docker's: a status and `{"message"}`, with the headers.
#[test]
fn refusals_are_docker_s() {
    let served = serve(Duration::from_secs(60));
    let too_new = api::get(&served.socket, "/v1.44/version").unwrap();
    assert_eq!(too_new.status, 400);
    assert_eq!(
        too_new.json()["message"],
        "client version 1.44 is too new. Maximum supported API version is 1.43"
    );
    assert_eq!(too_new.header("Api-Version"), Some("1.43"));
    assert_eq!(too_new.header("Content-Type"), Some("application/json"));
    let unknown = api::get(&served.socket, "/plugins").unwrap();
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["message"], "page not found");
    let no_body = api::post(&served.socket, "/containers/create", None).unwrap();
    assert_eq!(no_body.status, 400);
    let bad = api::request(
        &served.socket,
        "POST",
        "/containers/create",
        Some(b"{not json"),
    )
    .unwrap();
    assert_eq!(bad.status, 400);
    assert!(
        bad.json()["message"]
            .as_str()
            .unwrap()
            .starts_with("invalid JSON")
    );
    let refused = api::post(
        &served.socket,
        "/containers/create",
        Some(&serde_json::json!({"Image": "alpine", "HostConfig": {"Privileged": true}})),
    )
    .unwrap();
    assert_eq!(refused.status, 400);
    assert!(
        refused.json()["message"]
            .as_str()
            .unwrap()
            .starts_with("Privileged")
    );
}

/// A body past the bound is refused (413) without being kept; one under it
/// is read.
#[test]
fn a_body_past_the_bound_is_refused() {
    let served = serve(Duration::from_secs(60));
    let mut big = br#"{"Image":"alpine","Env":[""#.to_vec();
    big.resize(shipyard_api::MAX_BODY + 1024, b'x');
    big.extend_from_slice(br#""]}"#);
    let refused = api::request(&served.socket, "POST", "/containers/create", Some(&big)).unwrap();
    assert_eq!(refused.status, 413);
    // Under it: read, and refused for what it says (positive control).
    let small = api::post(
        &served.socket,
        "/containers/create",
        Some(&serde_json::json!({"Image": ""})),
    )
    .unwrap();
    assert_eq!(small.status, 400);
    assert_eq!(small.json()["message"], "an Image is needed");
}

/// Idle for its idle time, it ends; an open connection holds it up for as
/// long as it's open. The control: unheld, a service ends within the very
/// window a held one is seen to outlast.
#[test]
fn it_ends_once_idle_and_not_before() {
    let idle = Duration::from_millis(500);
    let window = idle * 6;
    let unheld = serve(idle);
    api::wait_for(&unheld.socket, Duration::from_secs(5));
    let ended = unheld
        .ended
        .recv_timeout(window)
        .expect("an idle service ends within the window");
    assert!(ended.is_ok(), "{ended:?}");

    let served = serve(idle);
    api::wait_for(&served.socket, Duration::from_secs(5));
    let held = UnixStream::connect(&served.socket).unwrap();
    assert!(
        served
            .ended
            .recv_timeout(window)
            .is_err_and(|error| error == mpsc::RecvTimeoutError::Timeout),
        "it ended while a connection was open"
    );
    drop(held);
    let ended = served
        .ended
        .recv_timeout(Duration::from_secs(10))
        .expect("it ends once idle");
    assert!(ended.is_ok(), "{ended:?}");
    assert!(
        UnixStream::connect(&served.socket).is_err(),
        "nothing listens once it has ended"
    );
}
