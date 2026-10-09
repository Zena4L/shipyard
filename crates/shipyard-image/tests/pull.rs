//! End-to-end pull tests against an in-process fixture registry.
//!
//! No network and no live registry: a tiny threaded HTTP server (std only)
//! serves the OCI distribution API from in-memory fixtures, including the
//! `401` → token → retry dance. The fixture can also misbehave the ways
//! real registries and attackers do: answer `429` then succeed, lie about a
//! blob's size or never end it, substitute a manifest under a pinned digest,
//! send several challenges, or label an index `application/json`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use shipyard_image::{
    BlobSink, Descriptor, Digest, ImageReference, Platform, PullError, Registry, RegistryError,
    Store,
};

/// One response the fixture gives.
#[derive(Clone)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// A `Content-Length` other than the body's (`None`: the truth).
    length: Option<u64>,
    /// Streams zeros after the body until the client hangs up.
    endless: bool,
    /// Sends no `Content-Length`: the body ends where the connection does.
    close_delimited: bool,
    /// Answers `Range: bytes=N-` with a `206` from byte N (a `416` past
    /// the end).
    ranges: bool,
    /// Sends only this many bytes of the body, having promised them all.
    cut: Option<usize>,
}

impl Response {
    fn ok(content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body,
            length: None,
            endless: false,
            close_delimited: false,
            ranges: false,
            cut: None,
        }
    }

    fn status(status: u16, headers: &[(&str, &str)]) -> Self {
        Self {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
            ..Self::ok("text/plain", Vec::new())
        }
    }

    /// A blob that answers ranges.
    fn ranged(body: Vec<u8>) -> Self {
        Self {
            ranges: true,
            ..Self::ok("application/octet-stream", body)
        }
    }
}

/// What the token endpoint wants before it hands a token out.
#[derive(Clone, Default, PartialEq, Eq)]
enum TokenAuth {
    /// Nothing.
    #[default]
    Anonymous,
    /// This `Authorization` header (`Basic …`).
    Header(String),
    /// A refresh-token POST with this token.
    Refresh(String),
}

/// What a request was.
struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    range: Option<String>,
    body: String,
}

/// What the fixture saw, shared with the test.
#[derive(Default)]
struct Seen {
    token_used: AtomicBool,
    /// Requests for `/v2/` paths.
    requests: AtomicUsize,
    /// The last token request's path and query.
    token_request: Mutex<Option<String>>,
    /// The last token POST's form.
    token_form: Mutex<Option<String>>,
    /// Every `Range` header, in order.
    ranges: Mutex<Vec<String>>,
}

/// A fixture registry: `GET <path>` to a queue of responses (each request
/// takes the next; the last repeats), served over a real TCP socket.
#[derive(Default)]
struct FixtureRegistry {
    routes: Mutex<HashMap<String, Vec<Response>>>,
    require_auth: bool,
    /// The `WWW-Authenticate` values of a 401 (`{base}` is replaced by the
    /// server's URL); a Bearer challenge to `/token` if empty.
    challenges: Vec<String>,
    /// What `/token` wants.
    token_auth: TokenAuth,
    /// With `Some`, `/v2/` wants this `Authorization` itself (a Basic
    /// challenge), and no token.
    basic: Option<String>,
    base: String,
    seen: Arc<Seen>,
}

const TOKEN: &str = "fixture-token";

impl FixtureRegistry {
    fn route(&mut self, path: &str, response: Response) {
        self.routes(path, vec![response]);
    }

    fn routes(&mut self, path: &str, responses: Vec<Response>) {
        self.routes
            .get_mut()
            .unwrap()
            .insert(path.to_owned(), responses);
    }

    fn serve(mut self) -> Serving {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding a port");
        let base = format!("http://{}", listener.local_addr().unwrap());
        self.base.clone_from(&base);
        let seen = self.seen.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let state = Arc::new(self);
        listener.set_nonblocking(true).unwrap();
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let state = state.clone();
                        thread::spawn(move || state.handle(stream));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Serving {
            base,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    fn handle(&self, stream: TcpStream) {
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut words = request_line.split_whitespace();
        let mut request = Request {
            method: words.next().unwrap_or("").to_owned(),
            path: words.next().unwrap_or("").to_owned(),
            authorization: None,
            range: None,
            body: String::new(),
        };
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim().to_owned();
                match name.trim().to_ascii_lowercase().as_str() {
                    "authorization" => request.authorization = Some(value),
                    "range" => request.range = Some(value),
                    "content-length" => length = value.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        let mut body = vec![0u8; length];
        if std::io::Read::read_exact(&mut reader, &mut body).is_ok() {
            request.body = String::from_utf8_lossy(&body).into_owned();
        }
        let response = self.respond(&request);
        let _ = write_response(stream, &response);
    }

    fn challenge(&self) -> Response {
        let challenges = if self.basic.is_some() {
            vec![r#"Basic realm="fixture""#.to_owned()]
        } else if self.challenges.is_empty() {
            vec![r#"Bearer realm="{base}/token",service="fixture""#.to_owned()]
        } else {
            self.challenges.clone()
        };
        let mut response = Response::status(401, &[]);
        for challenge in challenges {
            response.headers.push((
                "WWW-Authenticate".to_owned(),
                challenge.replace("{base}", &self.base),
            ));
        }
        response
    }

    fn respond(&self, request: &Request) -> Response {
        let path = request.path.as_str();
        let seen = &self.seen;
        if path.starts_with("/token") {
            *seen.token_request.lock().unwrap() = Some(path.to_owned());
            if request.method == "POST" {
                *seen.token_form.lock().unwrap() = Some(request.body.clone());
            }
            let allowed = match &self.token_auth {
                TokenAuth::Anonymous => true,
                TokenAuth::Header(wanted) => request.authorization.as_ref() == Some(wanted),
                TokenAuth::Refresh(token) => {
                    request.method == "POST"
                        && request.body.contains("grant_type=refresh_token")
                        && request.body.contains(&format!("refresh_token={token}"))
                }
            };
            if !allowed {
                // OAuth2 turns a bad refresh token down with a 400.
                let status = if request.method == "POST" { 400 } else { 401 };
                return Response::status(status, &[]);
            }
            let field = if request.method == "POST" {
                "access_token"
            } else {
                "token"
            };
            return Response::ok(
                "application/json",
                format!(r#"{{"{field}":"{TOKEN}"}}"#).into_bytes(),
            );
        }
        if path.starts_with("/v2/") {
            seen.requests.fetch_add(1, Ordering::Relaxed);
            if let Some(range) = &request.range {
                seen.ranges.lock().unwrap().push(range.clone());
            }
            if let Some(wanted) = &self.basic {
                if request.authorization.as_ref() != Some(wanted) {
                    return self.challenge();
                }
            } else if self.require_auth {
                if request.authorization.as_deref() == Some(&format!("Bearer {TOKEN}")) {
                    seen.token_used.store(true, Ordering::Relaxed);
                } else {
                    return self.challenge();
                }
            }
        }
        let response = {
            let mut routes = self.routes.lock().unwrap();
            // A route is a path: a token request's query isn't part of it.
            let route = path.split_once('?').map_or(path, |(route, _)| route);
            match routes.get_mut(route) {
                Some(queue) if queue.len() > 1 => queue.remove(0),
                Some(queue) => queue[0].clone(),
                None => Response::status(404, &[]),
            }
        };
        let from = request
            .range
            .as_deref()
            .and_then(|range| range.strip_prefix("bytes="))
            .and_then(|range| range.strip_suffix('-'))
            .and_then(|from| from.parse::<usize>().ok());
        match from {
            Some(from) if response.ranges && response.status == 200 => {
                let size = response.body.len();
                if from >= size {
                    return Response::status(416, &[]);
                }
                let mut ranged = response.clone();
                ranged.status = 206;
                ranged.body = response.body[from..].to_vec();
                ranged.headers.push((
                    "Content-Range".to_owned(),
                    format!("bytes {from}-{}/{size}", size - 1),
                ));
                ranged
            }
            _ => response,
        }
    }
}

fn write_response(mut stream: TcpStream, response: &Response) -> std::io::Result<()> {
    write!(stream, "HTTP/1.1 {} Fixture\r\n", response.status)?;
    for (name, value) in &response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    if !response.endless && !response.close_delimited {
        let length = response.length.unwrap_or(response.body.len() as u64);
        write!(stream, "Content-Length: {length}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    let sent = response.cut.unwrap_or(usize::MAX).min(response.body.len());
    stream.write_all(&response.body[..sent])?;
    if response.endless {
        // Until the client gives up (bounded, so a broken test can't hang).
        let zeros = vec![0u8; 64 * 1024];
        for _ in 0..1024 {
            stream.write_all(&zeros)?;
        }
    }
    stream.flush()
}

struct Serving {
    base: String,
    seen: Arc<Seen>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Serving {
    fn registry(&self) -> Registry {
        Registry::with_base("fixture", &self.base)
    }

    fn requests(&self) -> usize {
        self.seen.requests.load(Ordering::Relaxed)
    }

    fn ranges(&self) -> Vec<String> {
        self.seen.ranges.lock().unwrap().clone()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A scratch directory, whose `store` the store is made in.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("shipyard-pull-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> Store {
        Store::open(self.0.join("store")).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

fn blob(content: &[u8]) -> (Digest, Vec<u8>) {
    (Digest::sha256(content), content.to_vec())
}

fn manifest_json(
    config: &(Digest, Vec<u8>),
    config_type: &str,
    layers: &[(Digest, Vec<u8>)],
) -> String {
    let layer_entries: Vec<String> = layers
        .iter()
        .map(|(d, b)| {
            format!(
                r#"{{"mediaType":"{LAYER}","digest":"{d}","size":{}}}"#,
                b.len()
            )
        })
        .collect();
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","config":{{"mediaType":"{config_type}","digest":"{}","size":{}}},"layers":[{}]}}"#,
        config.0,
        config.1.len(),
        layer_entries.join(",")
    )
}

fn config_json(arch: &str, diff_ids: &[&Digest]) -> String {
    let ids: Vec<String> = diff_ids.iter().map(|d| format!("\"{d}\"")).collect();
    format!(
        r#"{{"architecture":"{arch}","os":"linux","rootfs":{{"type":"layers","diff_ids":[{}]}},"config":{{"Cmd":["/bin/sh"]}}}}"#,
        ids.join(",")
    )
}

/// An image hosted in a fixture: its manifest's digest and bytes, and its
/// layers.
struct Hosted {
    manifest: Digest,
    manifest_bytes: Vec<u8>,
    layers: Vec<(Digest, Vec<u8>)>,
}

/// Hosts a config, layers and a manifest under `repository`, tagged `tag`
/// (and served by its digest).
fn host_image(
    registry: &mut FixtureRegistry,
    repository: &str,
    tag: &str,
    arch: &str,
    layer_count: usize,
) -> Hosted {
    host_image_typed(registry, repository, tag, arch, layer_count, OCI_CONFIG)
}

fn host_image_typed(
    registry: &mut FixtureRegistry,
    repository: &str,
    tag: &str,
    arch: &str,
    layer_count: usize,
    config_type: &str,
) -> Hosted {
    let contents: Vec<Vec<u8>> = (0..layer_count)
        .map(|i| format!("layer {i} of {repository} {arch}").into_bytes())
        .collect();
    host_layers(registry, (repository, tag, arch), &contents, config_type)
}

/// Hosts an image of layers with these `contents`.
fn host_layers(
    registry: &mut FixtureRegistry,
    (repository, tag, arch): (&str, &str, &str),
    contents: &[Vec<u8>],
    config_type: &str,
) -> Hosted {
    let layers: Vec<(Digest, Vec<u8>)> = contents.iter().map(|c| blob(c)).collect();
    let diff_ids: Vec<&Digest> = layers.iter().map(|(d, _)| d).collect();
    let config = blob(config_json(arch, &diff_ids).as_bytes());
    registry.route(
        &format!("/v2/{repository}/blobs/{}", config.0),
        Response::ok("application/octet-stream", config.1.clone()),
    );
    for (d, b) in &layers {
        registry.route(
            &format!("/v2/{repository}/blobs/{d}"),
            Response::ok("application/octet-stream", b.clone()),
        );
    }
    let manifest = manifest_json(&config, config_type, &layers).into_bytes();
    let digest = Digest::sha256(&manifest);
    let response = Response::ok(OCI_MANIFEST, manifest.clone());
    registry.route(
        &format!("/v2/{repository}/manifests/{tag}"),
        response.clone(),
    );
    registry.route(&format!("/v2/{repository}/manifests/{digest}"), response);
    Hosted {
        manifest: digest,
        manifest_bytes: manifest,
        layers,
    }
}

fn reference(text: &str) -> ImageReference {
    ImageReference::parse(text).unwrap()
}

fn amd64() -> Platform {
    Platform::new("linux", "amd64")
}

fn pull(
    serving: &Serving,
    image: &str,
    platform: &Platform,
    store: &Store,
) -> Result<shipyard_image::Pulled, PullError> {
    let quiet = |_: &Descriptor, _: bool| {};
    shipyard_image::pull::pull_with(
        &reference(image),
        platform,
        store,
        &serving.registry(),
        shipyard_image::PullOptions::default(),
        &quiet,
    )
}

#[test]
fn a_single_arch_image_pulls_verifies_and_is_stored_whole() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "library/alpine", "3.20", "amd64", 3);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();

    let pulled = pull(&serving, "localhost/library/alpine:3.20", &amd64(), &store).unwrap();
    assert_eq!(pulled.manifest.layers.len(), 3);
    assert_eq!(pulled.manifest_digest, hosted.manifest);
    assert!(store.contains(&pulled.manifest.config.digest));
    assert!(store.contains(&hosted.manifest), "the manifest is stored");
    for layer in &pulled.manifest.layers {
        assert!(store.contains(&layer.digest));
    }
}

#[test]
fn authentication_takes_the_bearer_challenge_among_several() {
    let mut fixture = FixtureRegistry {
        require_auth: true,
        challenges: vec![
            r#"Basic realm="fixture""#.to_owned(),
            r#"Bearer realm="{base}/token?account=me",service="fixture",scope="repository:app:pull,push""#
                .to_owned(),
        ],
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "app", "latest", "amd64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    pull(&serving, "localhost/app", &amd64(), &scratch.store()).unwrap();
    assert!(
        serving.seen.token_used.load(Ordering::Relaxed),
        "the token was used"
    );
    let token_request = serving.seen.token_request.lock().unwrap().clone().unwrap();
    assert_eq!(
        token_request, "/token?account=me&service=fixture&scope=repository:app:pull%2Cpush",
        "the realm's own query kept, the quoted comma intact"
    );
}

#[test]
fn a_registry_that_only_takes_a_password_says_so() {
    let mut fixture = FixtureRegistry {
        require_auth: true,
        challenges: vec![r#"Basic realm="fixture""#.to_owned()],
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "app", "latest", "amd64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let err = pull(&serving, "localhost/app", &amd64(), &scratch.store()).unwrap_err();
    assert!(err.to_string().contains("username and password"), "{err}");
}

#[test]
fn a_multi_arch_index_selects_the_platform_and_is_stored() {
    let mut fixture = FixtureRegistry::default();
    let amd = host_image(&mut fixture, "multi", "amd64-only", "amd64", 1);
    let arm = host_image(&mut fixture, "multi", "arm64-only", "arm64", 2);
    let entry = |hosted: &Hosted, arch: &str, variant: &str| {
        format!(
            r#"{{"mediaType":"{OCI_MANIFEST}","digest":"{}","size":{},"platform":{{"architecture":"{arch}","os":"linux"{variant}}}}}"#,
            hosted.manifest,
            hosted.manifest_bytes.len()
        )
    };
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{},{}]}}"#,
        entry(&amd, "amd64", ""),
        entry(&arm, "arm64", r#","variant":"v8""#)
    )
    .into_bytes();
    // Labelled only as JSON, as some mirrors do: the body says it's an index.
    fixture.route(
        "/v2/multi/manifests/v1",
        Response::ok("application/json", index.clone()),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let mut arm64 = Platform::new("linux", "arm64");
    arm64.variant = Some("v8".to_owned());
    let pulled = pull(&serving, "localhost/multi:v1", &arm64, &store).unwrap();
    assert_eq!(pulled.manifest_digest, arm.manifest);
    assert_eq!(pulled.manifest.layers.len(), 2);
    let index_digest = Digest::sha256(&index);
    assert!(store.contains(&index_digest), "the index is stored");

    // Pinned to the index, it's found by its digest alone (nothing
    // recorded), through the stored index to the platform's manifest.
    let pinned = reference(&format!("localhost/multi@{index_digest}"));
    let found = shipyard_image::from_store(&pinned, &arm64, &store)
        .unwrap()
        .unwrap();
    assert_eq!(found.manifest_digest, arm.manifest);
    std::fs::remove_file(store.path(&arm.manifest)).unwrap();
    assert!(
        shipyard_image::from_store(&pinned, &arm64, &store)
            .unwrap()
            .is_none(),
        "the platform's manifest missing"
    );
}

#[test]
fn an_index_entry_the_registry_serves_wrongly_is_refused() {
    let mut fixture = FixtureRegistry::default();
    let amd = host_image(&mut fixture, "multi", "amd64-only", "amd64", 1);
    let size = amd.manifest_bytes.len();
    // Named by another digest, and served under it: the right size, the
    // wrong bytes.
    let named = Digest::sha256(b"another manifest");
    fixture.route(
        &format!("/v2/multi/manifests/{named}"),
        Response::ok(OCI_MANIFEST, amd.manifest_bytes.clone()),
    );
    let index = |digest: &Digest, size: usize| {
        format!(
            r#"{{"mediaType":"{OCI_INDEX}","manifests":[{{"mediaType":"{OCI_MANIFEST}","digest":"{digest}","size":{size},"platform":{{"architecture":"amd64","os":"linux"}}}}]}}"#
        )
        .into_bytes()
    };
    fixture.route(
        "/v2/multi/manifests/wrong-bytes",
        Response::ok(OCI_INDEX, index(&named, size)),
    );
    // The right bytes, but not the size the index gave.
    fixture.route(
        "/v2/multi/manifests/wrong-size",
        Response::ok(OCI_INDEX, index(&amd.manifest, size + 1)),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    for tag in ["wrong-bytes", "wrong-size"] {
        let image = format!("localhost/multi:{tag}");
        let err = pull(&serving, &image, &amd64(), &scratch.store()).unwrap_err();
        assert!(
            matches!(err, PullError::IndexMismatch { .. }),
            "{tag}: {err}"
        );
    }
}

#[test]
fn a_pinned_digest_is_checked_against_the_bytes() {
    let mut fixture = FixtureRegistry::default();
    let real = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let other = host_image(&mut fixture, "app", "v2", "amd64", 2);
    // The registry answers the pinned digest with another manifest.
    fixture.route(
        &format!("/v2/app/manifests/{}", real.manifest),
        Response::ok(OCI_MANIFEST, other.manifest_bytes),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let err = pull(
        &serving,
        &format!("localhost/app@{}", real.manifest),
        &amd64(),
        &scratch.store(),
    )
    .unwrap_err();
    assert!(matches!(err, PullError::Pinned { .. }), "{err}");
}

#[test]
fn a_lying_registry_is_refused() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "bad", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let mut tampered = bytes.clone();
    tampered[0] ^= 0xff;
    fixture.route(
        &format!("/v2/bad/blobs/{digest}"),
        Response::ok("application/octet-stream", tampered),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let err = pull(&serving, "localhost/bad:v1", &amd64(), &store).unwrap_err();
    assert!(matches!(err, PullError::Store(_)), "{err}");
    assert!(!store.contains(digest));
}

#[test]
fn a_blob_longer_than_its_size_is_refused_at_its_size() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "big", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let mut endless = Response::ok("application/octet-stream", bytes.clone());
    endless.endless = true;
    fixture.route(&format!("/v2/big/blobs/{digest}"), endless);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let err = pull(&serving, "localhost/big:v1", &amd64(), &store).unwrap_err();
    assert!(err.to_string().contains("more than the blob's"), "{err}");
    let leftovers = std::fs::read_dir(store.root().join("blobs/sha256"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .max()
        .unwrap_or(0);
    assert!(leftovers < 1024 * 1024, "nothing big was written");
}

#[test]
fn a_rate_limit_is_waited_out() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "busy", "v1", "amd64", 1);
    fixture.routes(
        "/v2/busy/manifests/v1",
        vec![
            Response::status(429, &[("Retry-After", "0")]),
            Response::ok(OCI_MANIFEST, hosted.manifest_bytes),
        ],
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    pull(&serving, "localhost/busy:v1", &amd64(), &scratch.store()).unwrap();
    // The manifest twice, then the config and the layer.
    assert_eq!(serving.requests(), 4);
}

#[test]
fn an_image_too_big_for_the_disk_is_refused_before_its_layers() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "huge", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    // The same image, claiming an exabyte-sized layer.
    let claimed = String::from_utf8(hosted.manifest_bytes).unwrap().replace(
        &format!(r#""digest":"{digest}","size":{}"#, bytes.len()),
        &format!(r#""digest":"{digest}","size":{}"#, 1u64 << 60),
    );
    fixture.route(
        "/v2/huge/manifests/v1",
        Response::ok(OCI_MANIFEST, claimed.into_bytes()),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let err = pull(&serving, "localhost/huge:v1", &amd64(), &scratch.store()).unwrap_err();
    assert!(matches!(err, PullError::NoSpace { .. }), "{err}");
    // The manifest and the config: no layer.
    assert_eq!(serving.requests(), 2);
}

/// An image with more layers than an image may have is refused from its
/// manifest, before any blob is fetched; one with as many as allowed pulls.
#[test]
fn an_image_with_too_many_layers_is_refused_before_its_blobs() {
    let mut fixture = FixtureRegistry::default();
    host_image(
        &mut fixture,
        "deep",
        "v1",
        "amd64",
        shipyard_image::MAX_LAYERS + 1,
    );
    host_image(
        &mut fixture,
        "deep",
        "v2",
        "amd64",
        shipyard_image::MAX_LAYERS,
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let err = pull(&serving, "localhost/deep:v1", &amd64(), &scratch.store()).unwrap_err();
    assert!(matches!(err, PullError::TooManyLayers(129)), "{err}");
    assert_eq!(serving.requests(), 1, "only the manifest");
    pull(&serving, "localhost/deep:v2", &amd64(), &scratch.store()).unwrap();
}

#[test]
fn a_transfer_cut_short_is_fetched_again() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "flaky", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let half = bytes[..bytes.len() / 2].to_vec();
    // Cut off before the length it promised, then ended by a close with no
    // length at all (a clean end of file, too early), then whole.
    let mut promised = Response::ok("application/octet-stream", half.clone());
    promised.length = Some(bytes.len() as u64);
    let mut closed = Response::ok("application/octet-stream", half);
    closed.close_delimited = true;
    fixture.routes(
        &format!("/v2/flaky/blobs/{digest}"),
        vec![
            promised,
            closed,
            Response::ok("application/octet-stream", bytes.clone()),
        ],
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    pull(&serving, "localhost/flaky:v1", &amd64(), &store).unwrap();
    assert!(store.contains(digest));
    // The manifest, the config, and the layer three times.
    assert_eq!(serving.requests(), 5);
}

#[test]
fn a_sink_that_stops_early_is_taken_at_its_word() {
    // A store that has the blob already (another pull's, just finished)
    // reads none of it: that's no short transfer to retry.
    struct Present;
    impl BlobSink for Present {
        type Error = std::io::Error;
        fn have(&self) -> u64 {
            0
        }
        fn reset(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn append(&mut self, _: &mut dyn std::io::Read) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let serving = fixture.serve();
    serving
        .registry()
        .read_blob("app", digest, bytes.len() as u64, &mut Present)
        .unwrap();
    assert_eq!(serving.requests(), 1);
}

#[test]
fn a_registry_that_stays_down_fails_after_the_retries() {
    let mut fixture = FixtureRegistry::default();
    fixture.route(
        "/v2/down/manifests/v1",
        Response::status(503, &[("Retry-After", "0")]),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let err = pull(&serving, "localhost/down:v1", &amd64(), &scratch.store()).unwrap_err();
    assert!(
        matches!(
            err,
            PullError::Registry(RegistryError::Status { status: 503, .. })
        ),
        "{err}"
    );
    assert_eq!(serving.requests(), 4, "four tries in all");
}

#[test]
fn an_artifact_or_another_platforms_image_is_refused() {
    let mut fixture = FixtureRegistry::default();
    host_image_typed(
        &mut fixture,
        "chart",
        "v1",
        "amd64",
        1,
        "application/vnd.cncf.helm.config.v1+json",
    );
    host_image(&mut fixture, "armonly", "v1", "arm64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let err = pull(&serving, "localhost/chart:v1", &amd64(), &store).unwrap_err();
    assert!(matches!(err, PullError::Artifact(_)), "{err}");
    let err = pull(&serving, "localhost/armonly:v1", &amd64(), &store).unwrap_err();
    assert!(matches!(err, PullError::Platform { .. }), "{err}");
}

#[test]
fn a_pulled_image_is_found_in_the_store_without_the_registry() {
    let mut fixture = FixtureRegistry::default();
    host_image(&mut fixture, "app", "v1", "amd64", 2);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let wanted = reference("localhost/app:v1");
    assert!(
        shipyard_image::from_store(&wanted, &amd64(), &store)
            .unwrap()
            .is_none()
    );
    let pulled = pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    // Recorded, as the CLI does after a pull.
    let record = shipyard_image::ImageRecord {
        manifest: pulled.manifest_digest.to_string(),
        config: pulled.manifest.config.digest.to_string(),
        layers: pulled
            .manifest
            .layers
            .iter()
            .map(|l| l.digest.to_string())
            .collect(),
    };
    shipyard_image::index::Index::record(store.root(), &wanted.to_string(), record).unwrap();
    let before = serving.requests();
    let found = shipyard_image::from_store(&wanted, &amd64(), &store)
        .unwrap()
        .unwrap();
    assert_eq!(found.manifest_digest, pulled.manifest_digest);
    assert_eq!(serving.requests(), before, "no request at all");

    // A layer missing (removed, or never written) is an image to pull again.
    let last = pulled.manifest.layers.last().unwrap();
    std::fs::remove_file(store.path(&last.digest)).unwrap();
    assert!(
        shipyard_image::from_store(&wanted, &amd64(), &store)
            .unwrap()
            .is_none(),
        "an incomplete image isn't found"
    );
}

// --- Signing in (step 3b) --------------------------------------------------

/// A Docker config at `dir/config.json` holding `json`.
fn docker_config(dir: &std::path::Path, json: &str) -> shipyard_image::Credentials {
    let path = dir.join("config.json");
    std::fs::write(&path, json).unwrap();
    shipyard_image::Credentials::at(path)
}

/// `Basic …` for `user` and `password`.
fn basic(user: &str, password: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {encoded}")
}

/// An `auths` config with `user:password` for `localhost`.
fn password_config(
    dir: &std::path::Path,
    user: &str,
    password: &str,
) -> shipyard_image::Credentials {
    use base64::Engine as _;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    docker_config(
        dir,
        &format!(r#"{{"auths":{{"localhost":{{"auth":"{auth}"}}}}}}"#),
    )
}

fn pull_signed(
    serving: &Serving,
    image: &str,
    store: &Store,
    credentials: shipyard_image::Credentials,
) -> Result<shipyard_image::Pulled, PullError> {
    let registry = serving
        .registry()
        .with_credentials(credentials, "localhost");
    shipyard_image::pull::pull_with(
        &reference(image),
        &amd64(),
        store,
        &registry,
        shipyard_image::PullOptions::default(),
        &|_: &Descriptor, _: bool| {},
    )
}

#[test]
fn a_private_image_pulls_with_the_password_from_dockers_config() {
    let mut fixture = FixtureRegistry {
        require_auth: true,
        token_auth: TokenAuth::Header(basic("me", "pw")),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "private", "v1", "amd64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let err = pull(&serving, "localhost/private:v1", &amd64(), &store).unwrap_err();
    assert!(err.to_string().contains("docker login"), "anonymous: {err}");

    let credentials = password_config(&scratch.0, "me", "pw");
    pull_signed(&serving, "localhost/private:v1", &store, credentials).unwrap();
    let token_request = serving.seen.token_request.lock().unwrap().clone().unwrap();
    assert!(token_request.contains("account=me"), "{token_request}");
}

#[test]
fn an_identity_token_is_exchanged_by_the_refresh_grant() {
    let mut fixture = FixtureRegistry {
        require_auth: true,
        token_auth: TokenAuth::Refresh("refresh-123".to_owned()),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "acr", "v1", "amd64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let credentials = docker_config(
        &scratch.0,
        r#"{"auths":{"https://localhost/v1/":{"identitytoken":"refresh-123"}}}"#,
    );
    pull_signed(&serving, "localhost/acr:v1", &scratch.store(), credentials).unwrap();
    let form = serving.seen.token_form.lock().unwrap().clone().unwrap();
    assert!(form.contains("client_id=shipyard"), "{form}");
    assert!(form.contains("scope=repository%3Aacr%3Apull"), "{form}");
}

#[test]
fn credentials_the_registry_refuses_are_named() {
    let mut fixture = FixtureRegistry {
        require_auth: true,
        token_auth: TokenAuth::Header(basic("me", "right")),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "private", "v1", "amd64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let credentials = password_config(&scratch.0, "me", "wrong");
    let err = pull_signed(
        &serving,
        "localhost/private:v1",
        &scratch.store(),
        credentials,
    )
    .unwrap_err();
    assert!(
        matches!(err, PullError::Registry(RegistryError::Refused { .. })),
        "{err}"
    );
    let message = err.to_string();
    assert!(message.contains("config.json"), "{message}");
    assert!(!message.contains("wrong"), "no secret: {message}");
}

#[test]
fn a_registry_that_asks_for_a_password_gets_it() {
    let mut fixture = FixtureRegistry {
        basic: Some(basic("me", "pw")),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "plain", "v1", "amd64", 1);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let err = pull(&serving, "localhost/plain:v1", &amd64(), &store).unwrap_err();
    assert!(err.to_string().contains("docker login"), "{err}");
    let credentials = password_config(&scratch.0, "me", "pw");
    pull_signed(&serving, "localhost/plain:v1", &store, credentials).unwrap();
}

// --- Resuming (step 3b) ------------------------------------------------------

/// The `.partial` file of `digest` in `store`.
fn partial(store: &Store, digest: &Digest) -> std::path::PathBuf {
    store
        .path(digest)
        .with_file_name(format!(".{}.partial", digest.hex()))
}

#[test]
fn a_cut_transfer_goes_on_from_where_it_stopped() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let half = bytes.len() / 2;
    let mut cut = Response::ranged(bytes.clone());
    cut.cut = Some(half);
    fixture.routes(
        &format!("/v2/app/blobs/{digest}"),
        vec![cut, Response::ranged(bytes.clone())],
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    assert_eq!(serving.ranges(), [format!("bytes={half}-")]);
    assert!(store.check_blob(digest).unwrap());
    assert!(
        !partial(&store, digest).exists(),
        "the partial became the blob"
    );
}

#[test]
fn a_registry_without_ranges_or_with_none_left_starts_again() {
    for second in [
        // Ranges ignored: the whole blob, from the first byte.
        |bytes: &[u8]| Response::ok("application/octet-stream", bytes.to_vec()),
        // No such range.
        |_: &[u8]| Response::status(416, &[]),
    ] {
        let mut fixture = FixtureRegistry::default();
        let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
        let (digest, bytes) = &hosted.layers[0];
        let mut cut = Response::ranged(bytes.clone());
        cut.cut = Some(bytes.len() / 2);
        fixture.routes(
            &format!("/v2/app/blobs/{digest}"),
            vec![cut, second(bytes), Response::ranged(bytes.clone())],
        );
        let serving = fixture.serve();
        let scratch = Scratch::new();
        let store = scratch.store();
        pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
        assert!(store.check_blob(digest).unwrap());
    }
}

#[test]
fn a_pull_goes_on_from_what_a_stopped_one_left() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    fixture.route(
        &format!("/v2/app/blobs/{digest}"),
        Response::ranged(bytes.clone()),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let half = bytes.len() / 2;
    std::fs::write(partial(&store, digest), &bytes[..half]).unwrap();
    pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    assert_eq!(serving.ranges(), [format!("bytes={half}-")]);
    assert!(store.check_blob(digest).unwrap());
}

#[test]
fn a_corrupt_partial_is_fetched_again_from_the_start() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    fixture.route(
        &format!("/v2/app/blobs/{digest}"),
        Response::ranged(bytes.clone()),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let half = bytes.len() / 2;
    std::fs::write(partial(&store, digest), vec![b'x'; half]).unwrap();
    pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    // Once from the partial's end, then again from the start (no range).
    assert_eq!(serving.ranges(), [format!("bytes={half}-")]);
    assert!(store.check_blob(digest).unwrap());
    assert!(!partial(&store, digest).exists());
}

#[test]
fn a_partial_another_pull_is_writing_is_left_to_it() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, _) = &hosted.layers[0];
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let path = partial(&store, digest);
    std::fs::write(&path, b"theirs").unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let held = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();
    pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    assert!(serving.ranges().is_empty(), "from the start, privately");
    assert_eq!(std::fs::read(&path).unwrap(), b"theirs", "theirs untouched");
    drop(held);
}

#[test]
fn a_transfer_that_keeps_moving_isnt_counted_as_failing() {
    // Cut after a little over 1 MiB every time: each attempt is progress,
    // so five of them aren't four failures.
    const MIB: usize = 1024 * 1024;
    let big: Vec<u8> = (0..5 * MIB)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let mut fixture = FixtureRegistry::default();
    let hosted = host_layers(
        &mut fixture,
        ("big", "v1", "amd64"),
        std::slice::from_ref(&big),
        OCI_CONFIG,
    );
    let (digest, _) = &hosted.layers[0];
    let mut cut = Response::ranged(big);
    cut.cut = Some(MIB + 4096);
    fixture.route(&format!("/v2/big/blobs/{digest}"), cut);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    let started = std::time::Instant::now();
    pull(&serving, "localhost/big:v1", &amd64(), &store).unwrap();
    assert_eq!(serving.ranges().len(), 4);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "no backoff"
    );
    assert!(store.check_blob(digest).unwrap());
}

#[test]
fn an_explicit_pull_repairs_a_corrupt_stored_blob() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    // A disk that flipped bits.
    let path = store.path(digest);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&path, vec![b'?'; bytes.len()]).unwrap();
    let repair = shipyard_image::PullOptions { recheck: true };
    shipyard_image::pull::pull_with(
        &reference("localhost/app:v1"),
        &amd64(),
        &store,
        &serving.registry(),
        repair,
        &|_: &Descriptor, _: bool| {},
    )
    .unwrap();
    assert!(store.check_blob(digest).unwrap());
}

#[test]
fn a_wrong_password_or_identity_token_is_named_as_refused() {
    let scratch = Scratch::new();
    // A registry with a Basic challenge.
    let mut fixture = FixtureRegistry {
        basic: Some(basic("me", "right")),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "plain", "v1", "amd64", 1);
    let serving = fixture.serve();
    let credentials = password_config(&scratch.0, "me", "wrong");
    let err = pull_signed(
        &serving,
        "localhost/plain:v1",
        &scratch.store(),
        credentials,
    )
    .unwrap_err();
    assert!(
        matches!(err, PullError::Registry(RegistryError::Refused { .. })),
        "{err}"
    );
    // A token service that turns a refresh token down (400).
    let mut fixture = FixtureRegistry {
        require_auth: true,
        token_auth: TokenAuth::Refresh("right".to_owned()),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "acr", "v1", "amd64", 1);
    let serving = fixture.serve();
    let credentials = docker_config(
        &scratch.0,
        r#"{"auths":{"localhost":{"identitytoken":"stale"}}}"#,
    );
    let err = pull_signed(&serving, "localhost/acr:v1", &scratch.store(), credentials).unwrap_err();
    assert!(
        matches!(err, PullError::Registry(RegistryError::Refused { .. })),
        "{err}"
    );
}

#[test]
fn a_helper_is_asked_once_and_a_broken_one_is_no_reason_to_fail() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fixture = FixtureRegistry {
        require_auth: true,
        token_auth: TokenAuth::Header(basic("me", "pw")),
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "app", "v1", "amd64", 3);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let calls = scratch.0.join("calls");
    let helper = scratch.0.join("docker-credential-counting");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\necho x >> '{}'\necho '{{\"Username\":\"me\",\"Secret\":\"pw\"}}'\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let credentials =
        docker_config(&scratch.0, r#"{"credsStore":"counting"}"#).with_helper_dir(&scratch.0);
    pull_signed(&serving, "localhost/app:v1", &scratch.store(), credentials).unwrap();
    assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);

    // A store whose helper is gone: a public image pulls anyway.
    let mut fixture = FixtureRegistry {
        require_auth: true,
        ..FixtureRegistry::default()
    };
    host_image(&mut fixture, "public", "v1", "amd64", 1);
    let serving = fixture.serve();
    let credentials =
        docker_config(&scratch.0, r#"{"credsStore":"uninstalled"}"#).with_helper_dir(&scratch.0);
    pull_signed(
        &serving,
        "localhost/public:v1",
        &scratch.store(),
        credentials,
    )
    .unwrap();
}

/// A token service that redirects elsewhere: with credentials, the
/// redirect isn't followed (an identity token's form, or a username in the
/// query, would go on to wherever it led), and the pull says why. Without,
/// it is (the control: the redirect leads somewhere that's asked).
#[test]
fn credentials_never_follow_a_token_services_redirect() {
    let elsewhere = FixtureRegistry::default().serve();
    let challenge = |fixture: &mut FixtureRegistry| {
        fixture.require_auth = true;
        fixture.challenges = vec![r#"Bearer realm="{base}/moved",service="fixture""#.to_owned()];
        let location = format!("{}/token", elsewhere.base);
        fixture.route("/moved", Response::status(307, &[("Location", &location)]));
    };
    let mut fixture = FixtureRegistry::default();
    challenge(&mut fixture);
    host_image(&mut fixture, "private", "v1", "amd64", 1);
    let serving = fixture.serve();
    // A password (a GET, the username in the query), and an identity token
    // (a POST, the token in its form).
    for config in [
        r#"{"auths":{"localhost":{"auth":"bWU6cHc="}}}"#,
        r#"{"auths":{"https://localhost/v1/":{"identitytoken":"refresh-123"}}}"#,
    ] {
        let scratch = Scratch::new();
        let credentials = docker_config(&scratch.0, config);
        let err = pull_signed(
            &serving,
            "localhost/private:v1",
            &scratch.store(),
            credentials,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("credentials aren't sent on"),
            "{config}: {err}"
        );
        assert!(
            elsewhere.seen.token_request.lock().unwrap().is_none()
                && elsewhere.seen.token_form.lock().unwrap().is_none(),
            "{config}: the credentials went where the redirect led"
        );
    }
    // Anonymous: followed, and the pull goes on from there.
    pull(
        &serving,
        "localhost/private:v1",
        &amd64(),
        &Scratch::new().store(),
    )
    .unwrap();
    assert!(elsewhere.seen.token_request.lock().unwrap().is_some());
}

#[test]
fn a_challenge_from_where_a_redirect_led_gets_no_credentials() {
    // Another server (a CDN, anyone's) the registry redirects a blob to,
    // asking for a token from a realm of its choosing.
    let mut elsewhere = FixtureRegistry::default();
    elsewhere.route(
        "/cdn/blob",
        Response::status(
            401,
            &[(
                "WWW-Authenticate",
                r#"Bearer realm="http://127.0.0.1:9/steal""#,
            )],
        ),
    );
    let elsewhere = elsewhere.serve();
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, _) = &hosted.layers[0];
    let location = format!("{}/cdn/blob", elsewhere.base);
    fixture.route(
        &format!("/v2/app/blobs/{digest}"),
        Response::status(307, &[("Location", &location)]),
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let credentials = password_config(&scratch.0, "me", "pw");
    let err = pull_signed(&serving, "localhost/app:v1", &scratch.store(), credentials).unwrap_err();
    assert!(
        matches!(
            err,
            PullError::Registry(RegistryError::Status { status: 401, .. })
        ),
        "{err}"
    );
}

#[test]
fn a_range_answer_for_another_range_starts_again() {
    let mut fixture = FixtureRegistry::default();
    let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
    let (digest, bytes) = &hosted.layers[0];
    let mut cut = Response::ranged(bytes.clone());
    cut.cut = Some(bytes.len() / 2);
    // Well formed, but from byte 0, not where it was asked to start.
    let whole = format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len());
    let mut wrong = Response::status(206, &[("Content-Range", &whole)]);
    wrong.body = bytes.clone();
    fixture.routes(
        &format!("/v2/app/blobs/{digest}"),
        vec![cut, wrong, Response::ranged(bytes.clone())],
    );
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let store = scratch.store();
    pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
    assert!(store.check_blob(digest).unwrap());
}

#[test]
fn a_partial_too_long_or_whole_but_wrong_is_fetched_again() {
    for leftover in [
        |bytes: &[u8]| [bytes, b"and more"].concat(),
        |bytes: &[u8]| vec![b'?'; bytes.len()],
    ] {
        let mut fixture = FixtureRegistry::default();
        let hosted = host_image(&mut fixture, "app", "v1", "amd64", 1);
        let (digest, bytes) = &hosted.layers[0];
        fixture.route(
            &format!("/v2/app/blobs/{digest}"),
            Response::ranged(bytes.clone()),
        );
        let serving = fixture.serve();
        let scratch = Scratch::new();
        let store = scratch.store();
        std::fs::write(partial(&store, digest), leftover(bytes)).unwrap();
        pull(&serving, "localhost/app:v1", &amd64(), &store).unwrap();
        assert!(serving.ranges().is_empty(), "from the start");
        assert!(store.check_blob(digest).unwrap());
    }
}

#[test]
fn a_registry_without_ranges_that_keeps_cutting_isnt_progress() {
    // Each answer is the whole blob from its start, cut after a little
    // over 1 MiB: after the first, nothing gets further than before.
    const MIB: usize = 1024 * 1024;
    let big: Vec<u8> = (0..3 * MIB)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let mut fixture = FixtureRegistry::default();
    let hosted = host_layers(
        &mut fixture,
        ("big", "v1", "amd64"),
        std::slice::from_ref(&big),
        OCI_CONFIG,
    );
    let (digest, _) = &hosted.layers[0];
    let mut cut = Response::ok("application/octet-stream", big);
    cut.cut = Some(MIB + 4096);
    fixture.route(&format!("/v2/big/blobs/{digest}"), cut);
    let serving = fixture.serve();
    let scratch = Scratch::new();
    let err = pull(&serving, "localhost/big:v1", &amd64(), &scratch.store()).unwrap_err();
    assert!(err.to_string().contains("broke off"), "{err}");
    // The manifest, the config, and the blob: once with progress, then
    // four times without.
    assert_eq!(serving.requests(), 2 + 5);
}
