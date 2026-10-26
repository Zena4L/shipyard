//! A registry that takes pushes (step 12.4): an in-memory server of the
//! distribution API's upload and manifest endpoints, enough of it for a
//! push and a pull of what was pushed, std only.
//!
//! It signs in as a registry does (none, Basic, or Bearer from a token
//! service on the same server, its token's scope `pull` or `pull,push`),
//! keeps each upload's state in its `Location` (`?_state=…`, a new one at
//! each step, as distribution and Docker Hub do: a client that reuses an
//! old one is refused), logs every request with its `Authorization`, and
//! misbehaves on demand ([`Misbehave`]).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test support: a panic is a test failure with a clear message"
)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crate::registry::sha256;

/// How it signs clients in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Auth {
    /// It doesn't.
    #[default]
    None,
    /// `Authorization: Basic …` must be this value.
    Basic(String),
    /// Bearer tokens from its own `/token` service, which takes the
    /// credential `Basic …` (or anonymous, `None`); `push_denied`: the
    /// account gets a pull token even when it asks for push, as Docker
    /// Hub's service does for a repository it can't push to.
    Bearer {
        /// The credential the token service takes (`Basic …`).
        credential: Option<String>,
        /// Whether push is refused to that account.
        push_denied: bool,
    },
}

/// What it does wrong, on purpose.
#[derive(Debug, Clone, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "test switches: each bool one misbehaviour, set on its own"
)]
pub struct Misbehave {
    /// An upload's `Location` on this other base (`127.0.0.1:PORT`).
    pub location_elsewhere: Option<String>,
    /// A `POST` that starts an upload is redirected (302) instead.
    pub redirect_post: bool,
    /// This many `PATCH`es answered 500 first.
    pub fail_patches: u32,
    /// The first `PATCH` answered 401 (its token "expired").
    pub patch_unauthorized_once: bool,
    /// This many final `PUT`s of a blob stored, then answered 500 (as a
    /// commit whose answer was lost).
    pub lose_commits: u32,
    /// The first commit (`PUT ?digest=`) answered 401, its token taken for
    /// expired (a large layer outlasting Docker Hub's 300 s tokens).
    pub commit_unauthorized_once: bool,
    /// The first `PATCH` answered 404 (`BLOB_UPLOAD_UNKNOWN`: an upload
    /// the registry lost).
    pub lose_upload_once: bool,
}

/// One request it saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// Its method.
    pub method: String,
    /// Its path and query.
    pub path: String,
    /// Its `Authorization`, if any.
    pub authorization: Option<String>,
    /// How many body bytes it carried.
    pub body: usize,
}

/// What it holds and does.
#[derive(Debug, Default)]
pub struct State {
    /// Its blobs, by digest (`sha256:…`).
    pub blobs: HashMap<String, Vec<u8>>,
    /// Its manifests, by repository and reference (tag or digest): media
    /// type and bytes.
    pub manifests: HashMap<(String, String), (String, Vec<u8>)>,
    /// Every request, in order.
    pub log: Vec<Seen>,
    /// How it signs in.
    pub auth: Auth,
    /// What it does wrong.
    pub misbehave: Misbehave,
    uploads: HashMap<String, Upload>,
    next: u32,
}

#[derive(Debug, Default)]
struct Upload {
    bytes: Vec<u8>,
    state: u32,
}

/// A running registry; stops when dropped.
#[derive(Debug)]
pub struct PushRegistry {
    /// `127.0.0.1:PORT`.
    pub base: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for PushRegistry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl PushRegistry {
    /// A registry signing clients in by `auth`, on a free loopback port.
    #[must_use]
    pub fn start(auth: Auth) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(State {
            auth,
            ..State::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (state_thread, stop_thread, base_thread) = (state.clone(), stop.clone(), base.clone());
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let state = state_thread.clone();
                        let base = base_thread.clone();
                        thread::spawn(move || {
                            let _ = serve(stream, &state, &base);
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            base,
            state,
            stop,
            handle: Some(handle),
        }
    }

    /// What it holds and saw, locked.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

/// A parsed request.
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    authorization: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default().to_owned();
    let (mut length, mut chunked, mut authorization, mut content_type) = (0, false, None, None);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" || header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
            "authorization" => authorization = Some(value),
            "content-type" => content_type = Some(value),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk)?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                key.to_owned(),
                value.replace("%3A", ":").replace("%3a", ":"),
            )
        })
        .collect();
    Ok(Request {
        method,
        path: path.to_owned(),
        query,
        authorization,
        content_type,
        body,
    })
}

/// A response to write.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    fn error(status: u16, code: &str) -> Self {
        Self::status(status)
            .body(format!(r#"{{"errors":[{{"code":"{code}","message":"{code}"}}]}}"#).into_bytes())
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>, base: &str) -> std::io::Result<()> {
    let request = read_request(&stream)?;
    let answer = {
        let mut state = state.lock().unwrap();
        let full = if request.query.is_empty() {
            request.path.clone()
        } else {
            let query: Vec<String> = request
                .query
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            format!("{}?{}", request.path, query.join("&"))
        };
        state.log.push(Seen {
            method: request.method.clone(),
            path: full,
            authorization: request.authorization.clone(),
            body: request.body.len(),
        });
        answer(&mut state, &request, base)
    };
    let reason = match answer.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        302 => "Found",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", answer.status);
    for (name, value) in &answer.headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    // A HEAD says the length a GET would send (a blob's, a manifest's).
    if !answer
        .headers
        .iter()
        .any(|(name, _)| name == "Content-Length")
    {
        let length = if request.method == "HEAD" {
            0
        } else {
            answer.body.len()
        };
        let _ = write!(head, "Content-Length: {length}\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    if request.method != "HEAD" {
        stream.write_all(&answer.body)?;
    }
    stream.flush()
}

/// The repository and what follows it in `path` (`/v2/<name>/<kind>/…`).
fn split(path: &str) -> Option<(&str, &str, &str)> {
    let rest = path.strip_prefix("/v2/")?;
    for kind in ["/blobs/uploads/", "/blobs/", "/manifests/"] {
        if let Some((name, tail)) = rest.split_once(kind) {
            return Some((name, kind, tail));
        }
    }
    None
}

/// Whether `request` is signed in for `push` (or a pull), else the 401 to
/// send.
fn signed_in(
    state: &State,
    request: &Request,
    name: &str,
    push: bool,
    base: &str,
) -> Option<Answer> {
    match &state.auth {
        Auth::None => None,
        Auth::Basic(wanted) => (request.authorization.as_ref() != Some(wanted)).then(|| {
            Answer::error(401, "UNAUTHORIZED").header("WWW-Authenticate", "Basic realm=\"fixture\"")
        }),
        Auth::Bearer { .. } => {
            let token = request.authorization.as_deref();
            let enough = if push {
                token == Some("Bearer tok-push")
            } else {
                matches!(token, Some("Bearer tok-push" | "Bearer tok-pull"))
            };
            (!enough).then(|| {
                let action = if push { "pull,push" } else { "pull" };
                let scope = if name.is_empty() {
                    String::new()
                } else {
                    format!(",scope=\"repository:{name}:{action}\"")
                };
                Answer::error(401, "UNAUTHORIZED").header(
                    "WWW-Authenticate",
                    format!("Bearer realm=\"http://{base}/token\",service=\"fixture\"{scope}"),
                )
            })
        }
    }
}

fn answer(state: &mut State, request: &Request, base: &str) -> Answer {
    let query = |key: &str| {
        request
            .query
            .iter()
            .find(|(had, _)| had == key)
            .map(|(_, value)| value.clone())
    };
    if request.path == "/token" {
        return token(state, request, query("scope").as_deref());
    }
    if request.path == "/v2/" {
        return signed_in(state, request, "", false, base)
            .unwrap_or_else(|| Answer::status(200).body(b"{}".to_vec()));
    }
    let Some((name, kind, tail)) = split(&request.path) else {
        return Answer::status(404);
    };
    let push = !matches!(request.method.as_str(), "GET" | "HEAD");
    if let Some(refused) = signed_in(state, request, name, push, base) {
        return refused;
    }
    match (request.method.as_str(), kind) {
        ("GET" | "HEAD", "/blobs/") => match state.blobs.get(tail) {
            Some(bytes) => Answer::status(200)
                .header("Content-Length", bytes.len().to_string())
                .header("Docker-Content-Digest", tail)
                .body(bytes.clone()),
            None => Answer::error(404, "BLOB_UNKNOWN"),
        },
        ("POST", "/blobs/uploads/") => {
            if state.misbehave.redirect_post {
                return Answer::status(302).header("Location", format!("http://{base}/v2/"));
            }
            state.next += 1;
            let id = format!("u{}", state.next);
            state.uploads.insert(id.clone(), Upload::default());
            let host = state.misbehave.location_elsewhere.clone();
            let location = format!("/v2/{name}/blobs/uploads/{id}?_state=s0");
            let location = match host {
                Some(other) => format!("http://{other}{location}"),
                None => location,
            };
            Answer::status(202)
                .header("Location", location)
                .header("Range", "0-0")
        }
        ("PATCH", "/blobs/uploads/") => patch(state, request, name, tail, base),
        ("PUT", "/blobs/uploads/") => {
            if std::mem::take(&mut state.misbehave.commit_unauthorized_once) {
                return expired(state, name, base);
            }
            let Some(upload) = current(state, tail, query("_state").as_deref()) else {
                return Answer::error(404, "BLOB_UPLOAD_INVALID");
            };
            upload.bytes.extend_from_slice(&request.body);
            let bytes = std::mem::take(&mut upload.bytes);
            let digest = sha256(&bytes);
            if query("digest").as_deref() != Some(digest.as_str()) {
                return Answer::error(400, "DIGEST_INVALID");
            }
            state.uploads.remove(tail);
            state.blobs.insert(digest.clone(), bytes);
            if state.misbehave.lose_commits > 0 {
                state.misbehave.lose_commits -= 1;
                return Answer::error(500, "UNKNOWN");
            }
            Answer::status(201)
                .header("Location", format!("/v2/{name}/blobs/{digest}"))
                .header("Docker-Content-Digest", digest)
        }
        ("GET" | "HEAD", "/manifests/") => {
            match state.manifests.get(&(name.to_owned(), tail.to_owned())) {
                Some((media_type, bytes)) => Answer::status(200)
                    .header("Content-Type", media_type.clone())
                    .header("Content-Length", bytes.len().to_string())
                    .header("Docker-Content-Digest", sha256(bytes))
                    .body(bytes.clone()),
                None => Answer::error(404, "MANIFEST_UNKNOWN"),
            }
        }
        ("PUT", "/manifests/") => {
            let digest = sha256(&request.body);
            let media_type = request.content_type.clone().unwrap_or_default();
            for reference in [tail.to_owned(), digest.clone()] {
                state.manifests.insert(
                    (name.to_owned(), reference),
                    (media_type.clone(), request.body.clone()),
                );
            }
            Answer::status(201)
                .header("Location", format!("/v2/{name}/manifests/{digest}"))
                .header("Docker-Content-Digest", digest)
        }
        _ => Answer::error(405, "UNSUPPORTED"),
    }
}

/// A `PATCH` of an upload: its body appended, a new state in its
/// `Location`.
fn patch(state: &mut State, request: &Request, name: &str, tail: &str, base: &str) -> Answer {
    if std::mem::take(&mut state.misbehave.patch_unauthorized_once) {
        return Answer::error(401, "UNAUTHORIZED").header(
            "WWW-Authenticate",
            match &state.auth {
                Auth::Bearer { .. } => format!(
                    "Bearer realm=\"http://{base}/token\",service=\"fixture\",scope=\"repository:{name}:pull,push\""
                ),
                _ => "Basic realm=\"fixture\"".to_owned(),
            },
        );
    }
    if std::mem::take(&mut state.misbehave.lose_upload_once) {
        return Answer::error(404, "BLOB_UPLOAD_UNKNOWN");
    }
    if state.misbehave.fail_patches > 0 {
        state.misbehave.fail_patches -= 1;
        return Answer::error(500, "UNKNOWN");
    }
    let Some(upload) = current(state, tail, state_of(request).as_deref()) else {
        return Answer::error(404, "BLOB_UPLOAD_INVALID");
    };
    upload.bytes.extend_from_slice(&request.body);
    upload.state += 1;
    let (length, now) = (upload.bytes.len(), upload.state);
    let id = tail.to_owned();
    Answer::status(202)
        .header(
            "Location",
            format!("/v2/{name}/blobs/uploads/{id}?_state=s{now}"),
        )
        .header("Range", format!("0-{}", length.saturating_sub(1)))
}

/// A 401 for a token taken for expired: a challenge for push again.
fn expired(state: &State, name: &str, base: &str) -> Answer {
    Answer::error(401, "UNAUTHORIZED").header(
        "WWW-Authenticate",
        match &state.auth {
            Auth::Bearer { .. } => format!(
                "Bearer realm=\"http://{base}/token\",service=\"fixture\",scope=\"repository:{name}:pull,push\""
            ),
            _ => "Basic realm=\"fixture\"".to_owned(),
        },
    )
}

/// The `_state` a request's query gives.
fn state_of(request: &Request) -> Option<String> {
    request
        .query
        .iter()
        .find(|(key, _)| key == "_state")
        .map(|(_, value)| value.clone())
}

/// Its token service: a token of `scope`'s push when the account may push.
fn token(state: &State, request: &Request, scope: Option<&str>) -> Answer {
    let Auth::Bearer {
        credential,
        push_denied,
    } = &state.auth
    else {
        return Answer::status(404);
    };
    if credential.is_some()
        && request.authorization.is_some()
        && request.authorization != *credential
    {
        return Answer::error(401, "UNAUTHORIZED");
    }
    let signed = credential.is_none() || request.authorization == *credential;
    let push = scope.is_some_and(|scope| scope.contains("push"));
    let token = if push && signed && !push_denied {
        "tok-push"
    } else {
        "tok-pull"
    };
    Answer::status(200).body(format!(r#"{{"token":"{token}"}}"#).into_bytes())
}

/// The upload `tail` names, if `given` is its current state (one the
/// client kept from an earlier step is stale).
fn current<'s>(state: &'s mut State, tail: &str, given: Option<&str>) -> Option<&'s mut Upload> {
    let upload = state.uploads.get_mut(tail)?;
    (given == Some(format!("s{}", upload.state).as_str())).then_some(upload)
}
