//! The registry client: fetching manifests and blobs over the OCI
//! [distribution API][spec], signing in as the registry asks.
//!
//! A pull talks to one registry. The client wraps a [`ureq::Agent`] (a
//! connection pool it reuses across the manifest and every layer) and the
//! authorization the registry leads to, shared by the layer workers. The
//! flow for any request is:
//!
//! 1. send it;
//! 2. if the registry itself (not where a redirect led) answers `401` with a
//!    `WWW-Authenticate: Bearer realm=…,service=…,scope=…` challenge, fetch
//!    a token from that realm (with the credentials Docker keeps for the
//!    registry, if any: [`crate::credentials`]) and send the request again
//!    with `Authorization: Bearer <token>`; with a `Basic` challenge, send
//!    it again with the username and password;
//! 3. on `429` or `502`/`503`/`504`, or a dropped connection, wait (as long
//!    as `Retry-After` asks, or 1, 2, 4… seconds) and try again, a few times;
//!    a blob goes on from what it has (`Range`), and an attempt that got
//!    well past the most it had before tries again at once, uncounted;
//! 4. any other non-success status is an error that says what it means.
//!
//! **What the client doesn't trust.** Every response is bounded: metadata is
//! read with a size cap, and a blob with its descriptor's exact size and a
//! time budget. The token realm is HTTPS (unless the registry itself is on
//! this machine). Neither the token nor the credentials follow a redirect to
//! another host (a blob CDN), nor answer a challenge from one. Plain HTTP is
//! refused for any registry not on this machine. TLS trusts the operating
//! system's certificates, as Docker and Podman do.
//!
//! **What it doesn't do: start a thread.** ureq can only time a DNS lookup
//! out from another thread, left running if it times out; a thread alive
//! when the CLI forks the container is what the code-after-fork rules
//! forbid. So there's no DNS timeout: connecting has one (30 s), waiting
//! for a response one (60 s), and receiving a body one sized to it (60 s,
//! plus the time it takes at 32 KiB/s, but 5 minutes at most while the
//! registry sends ranges, after which the blob goes on where it stopped).
//!
//! [spec]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md

use std::cell::Cell;
use std::io::{self, Read};
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::credentials::{Credential, Credentials, FoundCredential};
use crate::digest::Digest;

/// The `Accept` header value offering every manifest and index media type the
/// parsers understand, so the registry serves whichever the image uses.
const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.manifest.v1+json",
    ", application/vnd.oci.image.index.v1+json",
    ", application/vnd.docker.distribution.manifest.v2+json",
    ", application/vnd.docker.distribution.manifest.list.v2+json",
);

/// The largest token response, or error body, read into memory.
const MAX_SMALL_RESPONSE: u64 = 64 * 1024;
/// How long connecting may take (DNS lookup aside: see the module docs).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the registry may take to start answering.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
/// The base time to receive a body, before its size is counted.
const BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest one attempt at a body may take: past it, a blob goes on
/// from where it stopped (`Range`), so a stalled transfer costs this much
/// at most, and a slow one that moves loses nothing.
const LONGEST_BODY: Duration = Duration::from_mins(5);
/// How far past the most a blob ever had an attempt must bring it to count
/// as progress: one that does is retried at once, and resets the count of
/// [`ATTEMPTS`].
const PROGRESS: u64 = 1024 * 1024;
/// The most attempts at one request, progress or not: a registry that
/// trickles can't keep a pull going forever.
const MOST_ATTEMPTS: u32 = 64;
/// The slowest transfer rate that isn't taken for a stall, in bytes a
/// second.
const SLOWEST_RATE: u64 = 32 * 1024;
/// How many attempts without progress a request gets.
const ATTEMPTS: u32 = 4;
/// The longest wait between tries, whatever `Retry-After` asks.
const LONGEST_WAIT: Duration = Duration::from_secs(60);

/// A fetched piece of registry metadata: its bytes and the media type the
/// registry labelled it with (the `Content-Type`).
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The response body.
    pub bytes: Vec<u8>,
    /// The `Content-Type` the registry returned, if any.
    pub media_type: Option<String>,
}

/// Why a registry request failed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The request couldn't be sent or its response read (a network,
    /// TLS, or protocol failure), after the retries.
    #[error("{context}: {source}")]
    Transport {
        /// What was being fetched.
        context: String,
        /// The underlying ureq error.
        source: Box<ureq::Error>,
    },
    /// The registry answered with a non-success status, after the retries.
    #[error("{context}: {}{}", status_meaning(*.status), detail_suffix(.detail.as_deref()))]
    Status {
        /// What was being fetched.
        context: String,
        /// The HTTP status code.
        status: u16,
        /// What the registry said (its error body's code and message).
        detail: Option<String>,
    },
    /// Authenticating to the registry failed.
    #[error("authenticating to {registry}: {reason}")]
    Auth {
        /// The registry host.
        registry: String,
        /// What went wrong.
        reason: &'static str,
    },
    /// The registry turned the credentials down.
    #[error(
        "{registry} refused the credentials from {from}: sign in again (`docker login \
         {registry}`)"
    )]
    Refused {
        /// The registry.
        registry: String,
        /// Where the credentials came from.
        from: String,
    },
    /// The credentials couldn't be read.
    #[error(transparent)]
    Credentials(#[from] crate::credentials::CredentialError),
    /// A response body was unreadable, too large, or the wrong size.
    #[error("{context}: {reason}")]
    Body {
        /// What was being read.
        context: String,
        /// Why it was refused.
        reason: String,
    },
}

/// What an HTTP status means, in words.
fn status_meaning(status: u16) -> String {
    match status {
        401 | 403 => "access denied: it's private and you're not signed in, or the account \
                     can't pull it (`docker login`)"
            .to_owned(),
        404 => "not found".to_owned(),
        429 => "rate-limited by the registry; try again later".to_owned(),
        other => format!("the registry answered HTTP {other}"),
    }
}

/// What the registry's error body said, as a suffix to the message.
fn detail_suffix(detail: Option<&str>) -> String {
    detail
        .map(|detail| format!(" ({detail})"))
        .unwrap_or_default()
}

/// Why reading a blob failed: the registry, or what the caller did with the
/// bytes (writing them to the store).
#[derive(Debug, thiserror::Error)]
pub enum BlobError<E> {
    /// The registry failed, lied about the size, or the network did.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The caller's sink failed.
    #[error(transparent)]
    Sink(E),
}

/// Where a blob's bytes go, as they come, keeping what it has across
/// attempts so a transfer goes on from where it stopped (a pull's sink
/// writes to the store's [`BlobWriter`](crate::store::BlobWriter)).
pub trait BlobSink {
    /// What can go wrong with it (shown when a transfer breaks off).
    type Error: std::fmt::Display;
    /// How many bytes of the blob it has.
    fn have(&self) -> u64;
    /// Throws them away, to start from the first byte.
    ///
    /// # Errors
    ///
    /// The sink's own.
    fn reset(&mut self) -> Result<(), Self::Error>;
    /// Appends what `body` gives, to its end.
    ///
    /// # Errors
    ///
    /// The sink's own, or `body`'s (the transfer's).
    fn append(&mut self, body: &mut dyn Read) -> Result<(), Self::Error>;
}

/// A client for one registry, shared by a pull's layer workers.
pub struct Registry {
    agent: ureq::Agent,
    /// `https://host` (or `http://host` for a registry on this machine).
    base: String,
    /// The registry host, for messages.
    host: String,
    /// Whether the registry is reached over HTTPS (all but loopback).
    secure: bool,
    /// The `Authorization` value a 401 challenge led to (`Bearer …` or
    /// `Basic …`): a refresh by one layer worker serves the others.
    authorization: Mutex<Option<String>>,
    /// Where credentials come from, and the registry's name there.
    credentials: Option<(Credentials, String)>,
    /// The credential, once looked up (at the first challenge), under a
    /// lock: two workers never ask a helper (and a keychain) twice.
    found: Mutex<Lookup>,
}

/// Whether a registry's credential was looked up, and what it was.
enum Lookup {
    /// Not yet.
    Pending,
    /// Done: the credential, or none (pull anonymously).
    Done(Option<FoundCredential>),
}

impl std::fmt::Debug for Registry {
    /// Never prints a token or credential: a `Debug` log of the client must
    /// not leak them.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registry")
            .field("base", &self.base)
            .field("host", &self.host)
            .field("authorization", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// What a failed attempt calls for.
enum Attempt<E = RegistryError> {
    /// Try again, after this long if the registry said.
    Retry(RegistryError, Option<Duration>),
    /// Try again at once: this attempt got somewhere (a blob grew by at
    /// least [`PROGRESS`]), so it isn't counted against [`ATTEMPTS`].
    Again(RegistryError),
    /// Give up: trying again can't help.
    Fatal(E),
}

impl Attempt {
    /// The same, with a fatal error widened to `E`.
    fn widen<E: From<RegistryError>>(self) -> Attempt<E> {
        match self {
            Self::Retry(err, after) => Attempt::Retry(err, after),
            Self::Again(err) => Attempt::Again(err),
            Self::Fatal(err) => Attempt::Fatal(err.into()),
        }
    }
}

/// One GET to send.
struct Get<'a> {
    url: &'a str,
    /// What it's for, in messages.
    context: &'a str,
    accept: Option<&'a str>,
    /// The most the body is expected to be: its receive deadline.
    size: u64,
    /// Where to start (`Range: bytes=<from>-`), past 0.
    from: u64,
    /// Whether its body gets at most [`LONGEST_BODY`]: only while the
    /// registry is known (or hoped) to send what's left of a blob.
    capped: bool,
}

impl Registry {
    /// A client for the registry at `host`: over HTTPS, or plain HTTP for a
    /// registry on this machine (`localhost`, `127.0.0.1`, `[::1]`).
    #[must_use]
    pub fn new(host: &str) -> Self {
        let scheme = if is_loopback(host) { "http" } else { "https" };
        Self::with_base(host, &format!("{scheme}://{host}"))
    }

    /// A client with an explicit base URL (`scheme://host[:port]`), plain
    /// HTTP to any host included: for tests, pointing at a fixture server.
    /// Everything else uses [`new`](Self::new).
    #[doc(hidden)]
    #[must_use]
    pub fn with_base(host: &str, base: &str) -> Self {
        use ureq::config::RedirectAuthHeaders;
        use ureq::tls::{RootCerts, TlsConfig};
        let secure = base.starts_with("https://");
        let config = ureq::Agent::config_builder()
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            // Inspect 4xx/5xx ourselves: a 401 carries the auth challenge we
            // need, so it must not be swallowed as a transport error.
            .http_status_as_error(false)
            // A blob is often redirected to a CDN: the token stays behind
            // (it's for the registry), but follows a redirect within it (by
            // host name: another port on the same host is the same
            // machine, and gets it too).
            .max_redirects(10)
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .https_only(secure)
            .user_agent(concat!("shipyard/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
            base: base.trim_end_matches('/').to_owned(),
            host: host.to_owned(),
            secure,
            authorization: Mutex::new(None),
            credentials: None,
            found: Mutex::new(Lookup::Pending),
        }
    }

    /// The same client, signing in with `registry`'s credentials from
    /// `credentials` when the registry asks (`registry` as references name
    /// it: `docker.io` for Docker Hub).
    #[must_use]
    pub fn with_credentials(mut self, credentials: Credentials, registry: &str) -> Self {
        self.credentials = Some((credentials, registry.to_owned()));
        self
    }

    /// Fetches the manifest (or index) at `reference` (a tag or a digest)
    /// in `repository`, offering every manifest media type; at most `limit`
    /// bytes.
    ///
    /// # Errors
    ///
    /// Returns a [`RegistryError`] for a transport failure, a non-success
    /// status, a failed authentication, or a body over `limit`.
    pub fn get_manifest(
        &self,
        repository: &str,
        reference: &str,
        limit: u64,
    ) -> Result<Fetched, RegistryError> {
        let url = format!("{}/v2/{repository}/manifests/{reference}", self.base);
        // A tag has no `:`; a digest does, and is written after an `@`.
        let separator = if reference.contains(':') { '@' } else { ':' };
        let context = format!("fetching the manifest {repository}{separator}{reference}");
        let get = Get {
            url: &url,
            context: &context,
            accept: Some(ACCEPT_MANIFESTS),
            size: limit,
            from: 0,
            capped: true,
        };
        Self::retrying(|| {
            let mut response = self.authorized(&get, repository)?;
            let media_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(';').next().unwrap_or(value).trim().to_owned());
            let bytes = read_capped(&mut response, limit, &context)?;
            Ok(Fetched { bytes, media_type })
        })
    }

    /// Fetches the blob `digest` in `repository`, of `size` bytes exactly,
    /// into `sink`, going on from what the sink has (`Range`): across
    /// attempts here, and from a pull that stopped before. A registry that
    /// can't send a range sends it whole, and the sink starts again.
    ///
    /// A blob that ends short is fetched again. An attempt that took the
    /// sink at least 1 MiB past the most it ever had retries at once and
    /// isn't counted; one attempt's body gets at most 5 minutes, unless the
    /// registry has shown it can't send a range. The whole blob gets twice
    /// the time its size takes at 32 KiB/s. One that sends more than `size`
    /// is refused as soon as the sink reads past it; a sink that stops early
    /// is taken at its word (it checks the digest).
    ///
    /// # Errors
    ///
    /// [`BlobError::Registry`] if the registry fails (after the retries), the
    /// blob isn't `size` bytes, or it's too slow; [`BlobError::Sink`] with
    /// the sink's own error.
    pub fn read_blob<S: BlobSink>(
        &self,
        repository: &str,
        digest: &Digest,
        size: u64,
        sink: &mut S,
    ) -> Result<(), BlobError<S::Error>> {
        let url = format!("{}/v2/{repository}/blobs/{digest}", self.base);
        let context = format!("fetching the blob {digest}");
        let budget = blob_budget(size);
        let started = Instant::now();
        // The most the sink has had, and whether the registry sends ranges
        // (`None`: not known yet).
        let mut furthest = sink.have();
        let mut ranges: Option<bool> = None;
        Self::retrying(|| {
            let fatal = |err| Attempt::Fatal(BlobError::Sink(err));
            if started.elapsed() > budget {
                return Err(Attempt::Fatal(BlobError::Registry(RegistryError::Body {
                    context: context.clone(),
                    reason: format!("too slow: {size} bytes didn't come within {budget:?}"),
                })));
            }
            if sink.have() > size {
                sink.reset().map_err(fatal)?;
            }
            let from = sink.have();
            let get = Get {
                url: &url,
                context: &context,
                accept: None,
                size: size - from,
                from,
                capped: ranges != Some(false),
            };
            let response = match self.authorized(&get, repository) {
                Ok(response) => response,
                // The registry has no such range (what the sink has is
                // longer than the blob it serves, say): start again.
                Err(Attempt::Fatal(RegistryError::Status { status: 416, .. })) if from > 0 => {
                    sink.reset().map_err(fatal)?;
                    return Err(Attempt::Retry(range_error(&context), Some(Duration::ZERO)));
                }
                Err(err) => return Err(err.widen()),
            };
            match (response.status().as_u16(), from) {
                (206, _) if content_range(&response) == Some((from, size)) => ranges = Some(true),
                // A range it wasn't asked for, or not the one asked for.
                (206, _) => {
                    sink.reset().map_err(fatal)?;
                    return Err(Attempt::Retry(range_error(&context), None));
                }
                // The whole blob, asked for.
                (_, 0) => {}
                // The whole blob, though a range was asked for: what the sink
                // has goes, and later attempts get all the time it takes.
                (_, _) => {
                    tracing::debug!("{context}: the registry sends no ranges; starting again");
                    ranges = Some(false);
                    sink.reset().map_err(fatal)?;
                }
            }
            let start = sink.have();
            let state = Cell::new(ReadState::Reading);
            let mut exact = Exact {
                // One byte past the size is enough to know it's too long.
                inner: response
                    .into_body()
                    .into_reader()
                    .take((size - start).saturating_add(1)),
                remaining: size - start,
                state: &state,
            };
            let appended = sink.append(&mut exact);
            let gained = sink.have().saturating_sub(furthest);
            furthest = furthest.max(sink.have());
            match (appended, state.get()) {
                // All of it, or as much as the sink wanted.
                (Ok(()), ReadState::Reading) => Ok(()),
                (_, ReadState::TooLong) => {
                    Err(Attempt::Fatal(BlobError::Registry(RegistryError::Body {
                        context: context.clone(),
                        reason: format!("the registry sent more than the blob's {size} bytes"),
                    })))
                }
                // It ended early, or the network dropped mid-blob: whatever
                // the sink made of that, the transfer is to blame.
                (appended, ReadState::Short | ReadState::Failed) => {
                    let err = RegistryError::Body {
                        context: context.clone(),
                        reason: match appended {
                            Err(err) => format!("the transfer broke off: {err}"),
                            Ok(()) => "the transfer ended early".to_owned(),
                        },
                    };
                    Err(if gained >= PROGRESS {
                        Attempt::Again(err)
                    } else {
                        Attempt::Retry(err, None)
                    })
                }
                (Err(err), ReadState::Reading) => Err(Attempt::Fatal(BlobError::Sink(err))),
            }
        })
    }

    /// Runs `attempt` while it asks to be retried: up to [`ATTEMPTS`] times
    /// without progress, waiting between them (`Retry-After` if the registry
    /// gave one, in seconds; a date is ignored; else 1, 2, 4… seconds), and
    /// at once after progress; [`MOST_ATTEMPTS`] in all.
    fn retrying<T, E: From<RegistryError>>(
        mut attempt: impl FnMut() -> Result<T, Attempt<E>>,
    ) -> Result<T, E> {
        let mut stuck = 0;
        let mut tries = 0;
        loop {
            tries += 1;
            let (err, wait) = match attempt() {
                Ok(value) => return Ok(value),
                Err(Attempt::Fatal(err)) => return Err(err),
                Err(Attempt::Again(err)) => {
                    stuck = 0;
                    (err, Duration::ZERO)
                }
                Err(Attempt::Retry(err, after)) => {
                    stuck += 1;
                    if stuck >= ATTEMPTS {
                        return Err(err.into());
                    }
                    let backoff = Duration::from_secs(1 << (stuck - 1));
                    (err, after.unwrap_or(backoff).min(LONGEST_WAIT))
                }
            };
            if tries >= MOST_ATTEMPTS {
                return Err(err.into());
            }
            tracing::debug!("{err}; trying again in {wait:?}");
            std::thread::sleep(wait);
        }
    }

    /// Sends a GET, authenticating once on a 401, and returns a successful
    /// response (or says whether the failure is worth retrying).
    fn authorized(
        &self,
        get: &Get<'_>,
        repository: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, Attempt> {
        use ureq::ResponseExt as _;
        let mut response = self.send(get, self.current_authorization()?)?;
        // A challenge only from the registry itself: one from where a
        // redirect led (a blob CDN, anyone's) gets no credentials.
        if response.status() == 401 && same_authority(response.get_uri(), &self.base) {
            let challenges: Vec<String> = response
                .headers()
                .get_all("www-authenticate")
                .iter()
                .filter_map(|value| value.to_str().ok().map(str::to_owned))
                .collect();
            let found = self
                .authenticate(&challenges, repository)
                .map_err(Attempt::Fatal)?;
            response = self.send(get, self.current_authorization()?)?;
            if let (401, Some(found)) = (response.status().as_u16(), found) {
                return Err(Attempt::Fatal(RegistryError::Refused {
                    registry: self.host.clone(),
                    from: found.source,
                }));
            }
        }
        successful(response, get.context)
    }

    /// Sends one GET with `authorization`, given time to receive its body
    /// (see [`LONGEST_BODY`]).
    fn send(
        &self,
        get: &Get<'_>,
        authorization: Option<String>,
    ) -> Result<ureq::http::Response<ureq::Body>, Attempt> {
        let mut request = self
            .agent
            .get(get.url)
            .config()
            .timeout_recv_body(Some(body_deadline(get.size, get.capped)))
            .build();
        if let Some(accept) = get.accept {
            request = request.header("Accept", accept);
        }
        if get.from > 0 {
            request = request.header("Range", format!("bytes={}-", get.from));
        }
        if let Some(authorization) = authorization {
            request = request.header("Authorization", authorization);
        }
        request
            .call()
            .map_err(|source| transport_failure(get.context, source))
    }

    /// The `Authorization` value, if one is held.
    fn current_authorization(&self) -> Result<Option<String>, Attempt> {
        Ok(self.authorization().map_err(Attempt::Fatal)?.clone())
    }

    /// The authorization slot, locked. Poisoned only by a worker that
    /// panicked holding it: never taken for a success.
    fn authorization(&self) -> Result<std::sync::MutexGuard<'_, Option<String>>, RegistryError> {
        self.authorization
            .lock()
            .map_err(|_| self.auth_error("a pull worker failed while holding the token"))
    }

    /// The credential for this registry, looked up once. One that can't be
    /// read (a helper that's gone, say) is a warning: the pull goes on
    /// without, which a public image needs no more.
    fn credential(&self) -> Result<Option<FoundCredential>, RegistryError> {
        let Some((credentials, registry)) = &self.credentials else {
            return Ok(None);
        };
        let mut found = self
            .found
            .lock()
            .map_err(|_| self.auth_error("a pull worker failed while looking up credentials"))?;
        if let Lookup::Done(found) = &*found {
            return Ok(found.clone());
        }
        let looked_up = credentials.lookup(registry).unwrap_or_else(|err| {
            tracing::warn!("{err}; going on without signing in to {registry}");
            None
        });
        *found = Lookup::Done(looked_up.clone());
        Ok(looked_up)
    }

    /// Handles a 401: the first Bearer challenge among `challenges` (every
    /// `WWW-Authenticate` value), a token from its realm (asked with the
    /// credential, if there is one); else a Basic challenge, answered with
    /// the credential. What it leads to is kept for the retry and every
    /// later request. Returns the credential it signed in with, if any.
    fn authenticate(
        &self,
        challenges: &[String],
        repository: &str,
    ) -> Result<Option<FoundCredential>, RegistryError> {
        let parsed: Vec<Challenge> = challenges
            .iter()
            .flat_map(|header| parse_challenges(header))
            .collect();
        let found = self.credential()?;
        let Some(bearer) = parsed.iter().find(|challenge| challenge.scheme == "bearer") else {
            if !parsed.iter().any(|challenge| challenge.scheme == "basic") {
                return Err(self.auth_error("a 401 without a Bearer or Basic challenge"));
            }
            let authorization = match found.as_ref().map(|found| &found.credential) {
                Some(Credential::Basic { username, password }) => basic(username, password),
                Some(Credential::IdentityToken(_)) => {
                    return Err(self.auth_error(
                        "the registry asks for a password, and the credential is an identity \
                         token",
                    ));
                }
                None => {
                    return Err(self.auth_error(
                        "the registry asks for a username and password: sign in with `docker \
                         login`",
                    ));
                }
            };
            *self.authorization()? = Some(authorization);
            return Ok(found);
        };
        let realm = bearer
            .param("realm")
            .ok_or_else(|| self.auth_error("the challenge names no realm"))?;
        // A token over plain HTTP could be read or swapped on the way, and a
        // realm can't move a pull off HTTPS (or send credentials off it).
        if self.secure && !realm.starts_with("https://") {
            return Err(self.auth_error("the token realm isn't HTTPS"));
        }
        let scope = bearer
            .param("scope")
            .map_or_else(|| format!("repository:{repository}:pull"), str::to_owned);
        let service = bearer.param("service");
        let context = format!("fetching a token from {realm}");
        let token = Self::retrying(|| {
            let response = self.token_request(realm, service, &scope, found.as_ref(), &context)?;
            let status = response.status().as_u16();
            // An OAuth2 server turns a bad refresh token down with a 400
            // (`invalid_grant`).
            let refused = match found.as_ref().map(|found| &found.credential) {
                Some(Credential::IdentityToken(_)) => matches!(status, 400 | 401 | 403),
                Some(Credential::Basic { .. }) => matches!(status, 401 | 403),
                None => false,
            };
            if let (true, Some(found)) = (refused, &found) {
                return Err(Attempt::Fatal(RegistryError::Refused {
                    registry: self.host.clone(),
                    from: found.source.clone(),
                }));
            }
            let bytes = read_capped(
                &mut successful(response, &context)?,
                MAX_SMALL_RESPONSE,
                &context,
            )?;
            parse_token(&bytes)
                .ok_or_else(|| Attempt::Fatal(self.auth_error("the token response had no token")))
        })?;
        *self.authorization()? = Some(format!("Bearer {token}"));
        Ok(found)
    }

    /// Asks the token `realm` for a token for `scope`: anonymously, with a
    /// username and password, or (an identity token) by `OAuth2`'s
    /// refresh-token grant, as Docker does. Never with the registry's own
    /// authorization: the realm is another service.
    fn token_request(
        &self,
        realm: &str,
        service: Option<&str>,
        scope: &str,
        found: Option<&FoundCredential>,
        context: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, Attempt> {
        let deadline = body_deadline(MAX_SMALL_RESPONSE, true);
        let failed = |source| transport_failure(context, source);
        if let Some(Credential::IdentityToken(token)) = found.map(|found| &found.credential) {
            let mut form = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", token.as_str()),
                ("client_id", "shipyard"),
                ("scope", scope),
            ];
            if let Some(service) = service {
                form.push(("service", service));
            }
            return self
                .agent
                .post(realm)
                .config()
                .timeout_recv_body(Some(deadline))
                .build()
                .send_form(form)
                .map_err(failed);
        }
        let mut url = realm.to_owned();
        url.push(if realm.contains('?') { '&' } else { '?' });
        if let Some(service) = service {
            url.push_str("service=");
            url.push_str(&encode(service));
            url.push('&');
        }
        url.push_str("scope=");
        url.push_str(&encode(scope));
        let mut authorization = None;
        if let Some(Credential::Basic { username, password }) = found.map(|found| &found.credential)
        {
            url.push_str("&account=");
            url.push_str(&encode(username));
            authorization = Some(basic(username, password));
        }
        let mut request = self
            .agent
            .get(&url)
            .config()
            .timeout_recv_body(Some(deadline))
            .build();
        if let Some(authorization) = authorization {
            request = request.header("Authorization", authorization);
        }
        request.call().map_err(failed)
    }

    /// An authentication error, for `reason`.
    fn auth_error(&self, reason: &'static str) -> RegistryError {
        RegistryError::Auth {
            registry: self.host.clone(),
            reason,
        }
    }
}

/// How long receiving a body of `size` bytes may take: [`BODY_TIMEOUT`],
/// plus its size at [`SLOWEST_RATE`]; at most [`LONGEST_BODY`] if `capped`.
fn body_deadline(size: u64, capped: bool) -> Duration {
    let deadline = BODY_TIMEOUT + Duration::from_secs(size / SLOWEST_RATE);
    if capped {
        deadline.min(LONGEST_BODY)
    } else {
        deadline
    }
}

/// How long a blob of `size` bytes may take in all, every attempt
/// counted: twice its uncapped body time, and the waits between attempts.
fn blob_budget(size: u64) -> Duration {
    body_deadline(size, false) * 2 + LONGEST_WAIT * ATTEMPTS
}

/// Whether `uri` is on the registry at `base` (the same host and port).
fn same_authority(uri: &ureq::http::Uri, base: &str) -> bool {
    let authority = base
        .split_once("://")
        .map_or(base, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    uri.authority()
        .is_some_and(|there| there.as_str().eq_ignore_ascii_case(authority))
}

/// A transport failure: worth a retry, unless it's one a retry can't
/// change (`https_only`, TLS refusals, a bad URL).
fn transport_failure(context: &str, source: ureq::Error) -> Attempt {
    let fatal = matches!(
        source,
        ureq::Error::RequireHttpsOnly(_)
            | ureq::Error::Tls(_)
            | ureq::Error::Rustls(_)
            | ureq::Error::BadUri(_)
    );
    let err = RegistryError::Transport {
        context: context.to_owned(),
        source: Box::new(source),
    };
    if fatal {
        Attempt::Fatal(err)
    } else {
        Attempt::Retry(err, None)
    }
}

/// `Authorization: Basic` for `username` and `password`.
fn basic(username: &str, password: &str) -> String {
    use base64::Engine as _;
    let pair = format!("{username}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

/// A `206`'s `Content-Range` (`bytes <first>-<last>/<size or *>`), as where
/// it starts and the blob's size; `None` unless it runs to the blob's end.
fn content_range(response: &ureq::http::Response<ureq::Body>) -> Option<(u64, u64)> {
    let value = response.headers().get("content-range")?.to_str().ok()?;
    let range = value.trim().strip_prefix("bytes ")?;
    let (span, total) = range.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let (first, last): (u64, u64) = (first.trim().parse().ok()?, last.trim().parse().ok()?);
    let size = last.checked_add(1)?;
    match total.trim() {
        "*" => Some((first, size)),
        total => (total.parse::<u64>().ok()? == size).then_some((first, size)),
    }
}

/// The error for a range answer that isn't the one asked for.
fn range_error(context: &str) -> RegistryError {
    RegistryError::Body {
        context: context.to_owned(),
        reason: "the registry answered with another range than the one asked for".to_owned(),
    }
}

/// How a blob's body read went, as [`Exact`] saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    /// Fine so far.
    Reading,
    /// More bytes than the blob's size: the registry lied.
    TooLong,
    /// The body ended before the blob's size.
    Short,
    /// The network failed mid-body.
    Failed,
}

/// A blob body held to its size: past it is [`ReadState::TooLong`], an end
/// before it [`ReadState::Short`], a failed read [`ReadState::Failed`], so
/// the caller can tell a lying registry (refused) and a cut transfer
/// (retried) from its own errors.
struct Exact<'a, R> {
    inner: R,
    remaining: u64,
    state: &'a Cell<ReadState>,
}

impl<R: Read> Read for Exact<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf).inspect_err(|_| {
            if self.state.get() == ReadState::Reading {
                self.state.set(ReadState::Failed);
            }
        })?;
        let Some(rest) = self.remaining.checked_sub(read as u64) else {
            self.state.set(ReadState::TooLong);
            return Err(io::Error::other("more than the blob's size"));
        };
        if read == 0 && rest > 0 && !buf.is_empty() {
            self.state.set(ReadState::Short);
        }
        self.remaining = rest;
        Ok(read)
    }
}

/// `response` if it's a success; otherwise the error its status means,
/// retried for `429` and `502`/`503`/`504` (after `Retry-After`, if it gives
/// seconds).
fn successful(
    mut response: ureq::http::Response<ureq::Body>,
    context: &str,
) -> Result<ureq::http::Response<ureq::Body>, Attempt> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .map(Duration::from_secs);
    let err = RegistryError::Status {
        context: context.to_owned(),
        status,
        detail: error_detail(&mut response),
    };
    if matches!(status, 429 | 502 | 503 | 504) {
        Err(Attempt::Retry(err, retry_after))
    } else {
        Err(Attempt::Fatal(err))
    }
}

/// Reads a response body of at most `limit` bytes.
fn read_capped(
    response: &mut ureq::http::Response<ureq::Body>,
    limit: u64,
    context: &str,
) -> Result<Vec<u8>, Attempt> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| {
            Attempt::Retry(
                RegistryError::Body {
                    context: context.to_owned(),
                    reason: err.to_string(),
                },
                None,
            )
        })?;
    if bytes.len() as u64 > limit {
        return Err(Attempt::Fatal(RegistryError::Body {
            context: context.to_owned(),
            reason: format!("the response is larger than the {limit}-byte limit"),
        }));
    }
    Ok(bytes)
}

/// What an error response says: the distribution spec's
/// `{"errors":[{"code":…,"message":…}]}`, read with a small cap.
fn error_detail(response: &mut ureq::http::Response<ureq::Body>) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Errors {
        errors: Vec<ErrorEntry>,
    }
    #[derive(serde::Deserialize)]
    struct ErrorEntry {
        code: Option<String>,
        message: Option<String>,
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_SMALL_RESPONSE)
        .read_to_end(&mut bytes)
        .ok()?;
    let first = serde_json::from_slice::<Errors>(&bytes)
        .ok()?
        .errors
        .into_iter()
        .next()?;
    match (first.code, first.message) {
        (Some(code), Some(message)) => Some(format!("{code}: {message}")),
        (code, message) => code.or(message),
    }
}

/// One parsed authentication challenge: its scheme (lowercase) and
/// parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Challenge {
    scheme: String,
    params: Vec<(String, String)>,
}

impl Challenge {
    /// The value of parameter `name` (case-insensitive), if given.
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Parses a `WWW-Authenticate` value into its challenges (RFC 7235): a
/// scheme, then `key=value` parameters whose values may be quoted strings
/// (with `\` escapes, and commas inside). One header may hold several
/// challenges: a token not followed by `=` starts the next.
pub(crate) fn parse_challenges(header: &str) -> Vec<Challenge> {
    let bytes = header.as_bytes();
    let mut at = 0;
    let mut challenges: Vec<Challenge> = Vec::new();
    let is_token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~/:".contains(&b);
    let skip = |at: &mut usize, also_commas: bool| {
        while *at < bytes.len()
            && (bytes[*at] == b' ' || bytes[*at] == b'\t' || (also_commas && bytes[*at] == b','))
        {
            *at += 1;
        }
    };
    loop {
        skip(&mut at, true);
        let start = at;
        while at < bytes.len() && is_token(bytes[at]) {
            at += 1;
        }
        if at == start {
            return challenges;
        }
        let token = &header[start..at];
        let mut after = at;
        skip(&mut after, false);
        if after < bytes.len() && bytes[after] == b'=' {
            // A parameter of the current challenge.
            at = after + 1;
            skip(&mut at, false);
            let value = if at < bytes.len() && bytes[at] == b'"' {
                at += 1;
                let mut value = String::new();
                while at < bytes.len() && bytes[at] != b'"' {
                    if bytes[at] == b'\\' && at + 1 < bytes.len() {
                        at += 1;
                    }
                    let length = header[at..].chars().next().map_or(1, char::len_utf8);
                    value.push_str(&header[at..at + length]);
                    at += length;
                }
                at += 1;
                value
            } else {
                let start = at;
                while at < bytes.len() && bytes[at] != b',' && bytes[at] != b' ' {
                    at += 1;
                }
                header[start..at].to_owned()
            };
            let Some(current) = challenges.last_mut() else {
                return challenges;
            };
            current.params.push((token.to_owned(), value));
        } else {
            challenges.push(Challenge {
                scheme: token.to_ascii_lowercase(),
                params: Vec::new(),
            });
        }
    }
}

/// Pulls the token out of a token-endpoint response (`token`, or `OAuth2`'s
/// spelling `access_token`).
pub(crate) fn parse_token(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        token: Option<String>,
        access_token: Option<String>,
    }
    let response: TokenResponse = serde_json::from_slice(bytes).ok()?;
    response
        .token
        .or(response.access_token)
        .filter(|token| !token.is_empty())
}

/// Whether `host` (with an optional port) is this machine.
fn is_loopback(host: &str) -> bool {
    let without_port = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:port` -> `::1`
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    without_port == "localhost" || without_port == "127.0.0.1" || without_port == "::1"
}

/// Percent-encodes a query parameter value (minimal: the characters that
/// actually appear in services and scopes and would break the query).
fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(byte as char);
            }
            // Writing to a String is infallible.
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_challenge_parses() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#;
        let challenges = parse_challenges(header);
        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].scheme, "bearer");
        assert_eq!(
            challenges[0].param("realm"),
            Some("https://auth.docker.io/token")
        );
        assert_eq!(challenges[0].param("service"), Some("registry.docker.io"));
        assert_eq!(
            challenges[0].param("scope"),
            Some("repository:library/alpine:pull")
        );
    }

    #[test]
    fn quoted_commas_escapes_and_several_challenges() {
        let header = r#"Basic realm="x", BEARER realm="https://a/t?x=1",scope="repository:a/b:pull,push",service="s\"q""#;
        let challenges = parse_challenges(header);
        assert_eq!(
            challenges
                .iter()
                .map(|c| c.scheme.as_str())
                .collect::<Vec<_>>(),
            ["basic", "bearer"]
        );
        assert_eq!(
            challenges[1].param("scope"),
            Some("repository:a/b:pull,push")
        );
        assert_eq!(challenges[1].param("service"), Some("s\"q"));
        assert!(parse_challenges("").is_empty());
        assert!(parse_challenges("=broken").is_empty());
    }

    #[test]
    fn a_token_is_pulled_from_either_field() {
        assert_eq!(parse_token(br#"{"token":"abc"}"#).as_deref(), Some("abc"));
        assert_eq!(
            parse_token(br#"{"access_token":"xyz"}"#).as_deref(),
            Some("xyz")
        );
        assert_eq!(parse_token(br#"{"token":""}"#), None, "empty is no token");
        assert_eq!(parse_token(b"not json"), None);
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(encode("registry.docker.io"), "registry.docker.io");
        assert_eq!(
            encode("repository:library/alpine:pull"),
            "repository:library/alpine:pull"
        );
        assert_eq!(encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn the_accept_header_lists_every_manifest_media_type() {
        use crate::manifest::media_type;
        for expected in [
            media_type::OCI_MANIFEST,
            media_type::OCI_INDEX,
            media_type::DOCKER_MANIFEST,
            media_type::DOCKER_MANIFEST_LIST,
        ] {
            assert!(
                ACCEPT_MANIFESTS.contains(expected),
                "Accept header is missing {expected}"
            );
        }
    }

    #[test]
    fn retries_stop_without_progress_or_at_the_cap() {
        let mut calls = 0;
        let _ = Registry::retrying::<(), RegistryError>(|| {
            calls += 1;
            Err(Attempt::Retry(range_error("x"), Some(Duration::ZERO)))
        });
        assert_eq!(calls, ATTEMPTS);
        let mut calls = 0;
        let _ = Registry::retrying::<(), RegistryError>(|| {
            calls += 1;
            Err(Attempt::Again(range_error("x")))
        });
        assert_eq!(calls, MOST_ATTEMPTS);
    }

    #[test]
    fn debug_never_prints_the_token() {
        let registry = Registry::with_base("example.com", "http://example.com");
        *registry.authorization.lock().unwrap() = Some("Bearer super-secret-token".to_owned());
        let debug = format!("{registry:?}");
        assert!(
            !debug.contains("super-secret-token"),
            "token leaked: {debug}"
        );
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn loopback_registries_use_http_others_https() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("localhost:5000"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.0.0.1:41145"));
        assert!(is_loopback("[::1]:5000"));
        assert!(!is_loopback("registry-1.docker.io"));
        assert!(!is_loopback("ghcr.io"));
        assert!(Registry::new("localhost:5000").base.starts_with("http://"));
        let remote = Registry::new("ghcr.io");
        assert!(remote.base.starts_with("https://") && remote.secure);
    }

    #[test]
    fn a_token_realm_off_https_is_refused_before_any_request() {
        let registry = Registry::with_base("ghcr.io", "https://ghcr.io");
        let err = registry
            .authenticate(
                &[r#"Bearer realm="http://ghcr.io/token""#.to_owned()],
                "app",
            )
            .unwrap_err();
        assert!(err.to_string().contains("isn't HTTPS"), "{err}");
    }

    #[test]
    fn statuses_say_what_they_mean() {
        let err = |status| RegistryError::Status {
            context: "fetching x".to_owned(),
            status,
            detail: Some("CODE: words".to_owned()),
        };
        assert!(err(404).to_string().contains("not found (CODE: words)"));
        assert!(err(429).to_string().contains("rate-limited"));
        assert!(err(401).to_string().contains("access denied"));
    }
}
