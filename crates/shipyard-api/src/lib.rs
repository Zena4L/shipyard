//! Shipyard's Docker Engine API (stage 11, D25): a subset of Docker's,
//! version 1.43 (D11.2-3), served over a UNIX socket by `shipyard system
//! service`, so that Docker's own clients (`docker`, `docker compose`,
//! Testcontainers) drive Shipyard's engine.
//!
//! - **One thread for the connections**, a tokio current-thread runtime
//!   (D11-1) with hyper's HTTP/1.1; the engine is blocking, so each of its
//!   calls runs on tokio's blocking threads, with an engine of its own
//!   whose reporter collects what it warns (`Shared::call`).
//! - **Only the socket's owner gets in:** the socket is the caller's alone
//!   (root's: 0600), and each connection's peer is checked again
//!   (`SO_PEERCRED`): another user is closed on before a byte is read.
//! - **It exits when idle** (`idle`): systemd's socket starts it again at
//!   the next connection (D11.2-2), so nothing runs while nothing is asked
//!   (D20).
//! - **Requests are bounded:** a header read is timed out, a JSON body is
//!   read up to [`MAX_BODY`], and every value is parsed by the parser the
//!   CLI's flag for it uses.

mod archive;
mod attach;
mod containers;
mod create;
mod errors;
mod exec;
mod filters;
mod hijack;
mod idle;
mod images;
mod networks;
mod own;
mod query;
mod reply;
mod routes;
mod socket;
mod streams;
mod system;
mod volumes;

use std::io;
use std::os::unix::net::UnixListener as StdListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Limited};
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use nix::sys::signal::{SigSet, Signal};
use shipyard_engine::report::Collect;
use shipyard_engine::{Engine, Paths};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

pub use idle::idle_time;
pub use routes::{API_VERSION, MIN_API_VERSION};
pub use socket::{BindError, Bound, bind};

use crate::errors::ApiError;
use crate::idle::{Busy, Idle};
use crate::query::Query;
use crate::reply::Body;
use crate::routes::Route;

/// The largest JSON body a request may send (a create's config, with its
/// environment and labels, is a few KiB).
pub const MAX_BODY: usize = 1 << 20;

/// How long a client may take to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// After an accept fails for want of descriptors, how long before trying
/// again (no busy loop while the limit lasts).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The idle time, unless `SHIPYARD_API_IDLE` says another (D11.2-2).
pub const DEFAULT_IDLE: Duration = Duration::from_secs(60);

/// What this build says of itself (`/version`): the engine's.
pub use shipyard_engine::docker::Build;

/// The service: where the engine keeps what it keeps, its idle time, and
/// what it says of itself.
#[derive(Debug, Clone)]
pub struct Service {
    /// The engine's places ([`Paths::from_env`], read once by the caller).
    pub paths: Paths,
    /// How long with nothing under way before it exits.
    pub idle: Duration,
    /// What `/version` says.
    pub build: Build,
}

/// What every request's task shares.
#[derive(Debug)]
pub(crate) struct Shared {
    paths: Paths,
    idle: Arc<Idle>,
    /// Places for the streams served at once ([`streams::MAX_STREAMS`]).
    streams: Arc<tokio::sync::Semaphore>,
    /// Attaches waiting for their container's start.
    pending: attach::Pending,
    /// The exec instances.
    execs: exec::Execs,
    build: Build,
    /// The `Server` header.
    server: HeaderValue,
}

/// Serves the API on `listener` (systemd's, or one the caller bound) until
/// nothing has been under way for the service's idle time, or until it's
/// asked to stop (SIGTERM, SIGINT, SIGHUP or SIGQUIT: systemd's stop, a
/// Ctrl-C) and its engine calls have ended.
///
/// The stop signals are blocked on the calling thread, and so on every
/// thread it starts (the mask is inherited), and a thread of their own
/// waits for them: a stop never lands in the middle of a create or a start
/// (the engine's own holding off is only its calling thread's). The
/// processes it starts get the signals back (`std::process` empties the
/// mask in a child).
///
/// # Errors
///
/// The runtime can't be made, or the listener fails (but for a
/// connection that went before it was accepted, or descriptors running
/// out for a while).
pub fn serve(listener: StdListener, service: Service) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let stops = stop_signals();
    let before = stops.thread_swap_mask(nix::sys::signal::SigmaskHow::SIG_BLOCK)?;
    let served = serve_blocked(listener, service, stops);
    let _ = before.thread_set_mask();
    served
}

/// The signals that stop the service.
fn stop_signals() -> SigSet {
    let mut set = SigSet::empty();
    for signal in [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGQUIT,
    ] {
        set.add(signal);
    }
    set
}

/// [`serve`], with `stops` blocked on this thread.
fn serve_blocked(listener: StdListener, service: Service, stops: SigSet) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let server = HeaderValue::from_str(&format!("Shipyard/{} (linux)", service.build.version))
        .unwrap_or_else(|_| HeaderValue::from_static("Shipyard"));
    let shared = Arc::new(Shared {
        paths: service.paths,
        idle: Idle::new(service.idle, Instant::now()),
        streams: Arc::new(tokio::sync::Semaphore::new(streams::MAX_STREAMS)),
        pending: attach::Pending::default(),
        execs: exec::Execs::default(),
        build: service.build,
        server,
    });
    // What execs of services gone before left (F117's PID files).
    if let Ok(root) = shared.paths.runtime_root() {
        exec::sweep_pid_files(root);
    }
    let stopped = Arc::new(Notify::new());
    {
        let (idle, stopped) = (Arc::clone(&shared.idle), Arc::clone(&stopped));
        // Blocked here too (inherited): it takes the signal when it comes,
        // and lives on, unused, if none does.
        std::thread::Builder::new()
            .name("stop-signals".to_owned())
            .spawn(move || {
                if stops.wait().is_ok() {
                    idle.stop();
                    stopped.notify_one();
                }
            })?;
    }
    // Dropped at the end, the runtime waits for the engine calls still
    // running (none by then: an idle or stopped service has none).
    runtime.block_on(async move {
        let listener = UnixListener::from_std(listener)?;
        accept(&listener, &shared, &stopped).await
    })
}

/// Accepts connections until the service is idle, or stopped (`stopped`
/// is told: then none is accepted any more).
async fn accept(listener: &UnixListener, shared: &Arc<Shared>, stopped: &Notify) -> io::Result<()> {
    let owner = nix::unistd::geteuid().as_raw();
    let mut stopping = false;
    loop {
        let Some(wait) = shared.idle.left(Instant::now()) else {
            // Idle, but one already waiting (a `shipyard compose` that saw
            // it answer a moment ago) is served, never left to a closed
            // socket: then it's busy again.
            if !stopping
                && let std::task::Poll::Ready(Ok((stream, _))) = std::future::poll_fn(|context| {
                    std::task::Poll::Ready(listener.poll_accept(context))
                })
                .await
                && peer_is(&stream, owner)
            {
                let busy = shared.idle.connection();
                tokio::spawn(connection(stream, Arc::clone(shared), busy));
                continue;
            }
            return Ok(());
        };
        if stopping {
            tokio::time::sleep(wait).await;
            continue;
        }
        // Timed out: time to look at the idle time again.
        let Ok(next) = tokio::time::timeout(wait, accepted_or_stopped(listener, stopped)).await
        else {
            continue;
        };
        let Some(accepted) = next else {
            stopping = true;
            continue;
        };
        match accepted {
            Ok((stream, _)) => {
                if !peer_is(&stream, owner) {
                    // Another user: closed on, nothing read.
                    continue;
                }
                let busy = shared.idle.connection();
                tokio::spawn(connection(stream, Arc::clone(shared), busy));
            }
            Err(error) => match error.raw_os_error() {
                // Gone before it was accepted: the next one.
                Some(libc::ECONNABORTED | libc::EINTR) => {}
                // Out of descriptors: they come back as connections end.
                Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
                _ => return Err(error),
            },
        }
    }
}

/// The next connection, or `None` once `stopped` is told.
async fn accepted_or_stopped(
    listener: &UnixListener,
    stopped: &Notify,
) -> Option<io::Result<(UnixStream, tokio::net::unix::SocketAddr)>> {
    let mut accept = std::pin::pin!(listener.accept());
    let mut told = std::pin::pin!(stopped.notified());
    std::future::poll_fn(|context| {
        if told.as_mut().poll(context).is_ready() {
            return std::task::Poll::Ready(None);
        }
        accept.as_mut().poll(context).map(Some)
    })
    .await
}

/// Whether the peer on `stream` runs as `owner` (`SO_PEERCRED`: the
/// kernel's word, taken when it connected).
fn peer_is(stream: &UnixStream, owner: u32) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|credentials| credentials.uid() == owner)
}

/// One connection: its requests, one at a time (HTTP/1.1), until the
/// client closes it.
async fn connection(stream: UnixStream, shared: Arc<Shared>, busy: Busy) {
    let service = hyper::service::service_fn(move |request| {
        let shared = Arc::clone(&shared);
        async move { Ok::<_, std::convert::Infallible>(handle(&shared, request).await) }
    });
    // A connection that fails (the client went, sent nonsense, or was too
    // slow) ends; the next one is another's.
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
    drop(busy);
}

/// One request's answer.
async fn handle(shared: &Arc<Shared>, mut request: Request<Incoming>) -> Response<Body> {
    // The connection taken over, if it's asked (attach): its future, which
    // resolves once the 101 has gone.
    let upgrade = hijack::asked(request.headers()).then(|| hyper::upgrade::on(&mut request));
    let (parts, body) = request.into_parts();
    let mut response = match answer(shared, &parts, body, upgrade).await {
        Ok(response) => response,
        Err(error) => reply::refused(&error),
    };
    reply::add_headers(&mut response, &shared.server);
    response
}

/// A prune of `kind`, its filters in `query`; the API version `path`
/// asks for (a volume prune's default changed at 1.42).
async fn prune(
    shared: &Arc<Shared>,
    kind: routes::Prune,
    query: &Query,
    path: &str,
) -> Result<Response<Body>, ApiError> {
    match kind {
        routes::Prune::Containers => containers::prune(shared, query).await,
        routes::Prune::Images => images::prune(shared, query).await,
        routes::Prune::Volumes => volumes::prune(shared, query, routes::asked_version(path)).await,
        routes::Prune::Networks => networks::prune(shared, query).await,
        routes::Prune::Build => Ok(images::prune_build()),
    }
}

/// The endpoint's answer, or why there's none.
async fn answer(
    shared: &Arc<Shared>,
    request: &hyper::http::request::Parts,
    body: Incoming,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Result<Response<Body>, ApiError> {
    let route = routes::route(&request.method, request.uri.path())?;
    let query = Query::parse(request.uri.query())?;
    let body = if route.takes_body() {
        Some(body)
    } else {
        // Not read: let go, so that the connection's end (the client gone)
        // is seen while the answer is awaited (a `wait`'s).
        drop(body);
        None
    };
    match route {
        Route::Ping => Ok(system::ping()),
        Route::Prune(kind) => prune(shared, kind, &query, request.uri.path()).await,
        Route::Version => Ok(system::version(shared)),
        Route::Info => system::info(shared).await,
        Route::ListContainers => containers::list(shared, &query).await,
        Route::CreateContainer => {
            let body = body.ok_or_else(|| ApiError::internal("the create's body"))?;
            containers::create(shared, &query, body).await
        }
        Route::Inspect(id) => containers::inspect(shared, id).await,
        Route::Logs(id) => streams::logs(shared, id, &query).await,
        Route::Top(id) => containers::top(shared, id, &query).await,
        Route::Stats(id) => streams::stats(shared, id, &query).await,
        Route::Attach(id) => attach::attach(shared, id, &query, upgrade).await,
        Route::Resize(id) => attach::resize(shared, id, &query).await,
        Route::ExecCreate(id) => {
            let body = body.ok_or_else(|| ApiError::internal("the exec's body"))?;
            exec::create(shared, id, body).await
        }
        Route::ExecStart(id) => {
            let body = body.ok_or_else(|| ApiError::internal("the start's body"))?;
            exec::start(shared, id, body, upgrade).await
        }
        Route::ExecInspect(id) => exec::inspect(shared, id),
        Route::ExecResize(id) => exec::resize(shared, &id, &query),
        Route::Events => streams::events(shared, &query),
        Route::Start(id) => containers::start(shared, id).await,
        Route::Stop(id) => containers::stop(shared, id, &query).await,
        Route::Restart(id) => containers::restart(shared, id, &query).await,
        Route::Kill(id) => containers::kill(shared, id, &query).await,
        Route::Pause(id) => containers::pause(shared, id, true).await,
        Route::Unpause(id) => containers::pause(shared, id, false).await,
        Route::Rename(id) => containers::rename(shared, id, &query).await,
        Route::Update(id) => {
            let body = body.ok_or_else(|| ApiError::internal("the update's body"))?;
            containers::update(shared, id, body).await
        }
        Route::Wait(id) => containers::wait(shared, id, &query).await,
        Route::Remove(id) => containers::remove(shared, id, &query).await,
        Route::PullImage => images::pull(shared, &query, &request.headers),
        Route::Auth => {
            let body = body.ok_or_else(|| ApiError::internal("the credentials' body"))?;
            images::auth(shared, body).await
        }
        Route::ListImages => images::list(shared, &query).await,
        Route::InspectImage(name) => images::inspect(shared, name).await,
        Route::TagImage(name) => images::tag(shared, name, &query).await,
        Route::RemoveImage(name) => images::remove(shared, name, &query).await,
        Route::ListVolumes => volumes::list(shared, &query).await,
        Route::CreateVolume => {
            let body = body.ok_or_else(|| ApiError::internal("the volume's body"))?;
            volumes::create(shared, body).await
        }
        Route::InspectVolume(name) => volumes::inspect(shared, name).await,
        Route::RemoveVolume(name) => volumes::remove(shared, name, &query).await,
        Route::ArchiveGet(id) => archive::get(shared, id, &query).await,
        Route::ArchiveHead(id) => archive::head(shared, id, &query).await,
        Route::ArchivePut(id) => {
            let body = body.ok_or_else(|| ApiError::internal("the archive's body"))?;
            archive::put(shared, id, &query, body).await
        }
        Route::OwnContainer(id) => own::container(shared, id).await,
        Route::OwnNetwork(name) => own::network(shared, name).await,
        Route::ListNetworks => networks::list(shared, &query).await,
        Route::CreateNetwork => {
            let body = body.ok_or_else(|| ApiError::internal("the network's body"))?;
            networks::create(shared, body).await
        }
        Route::InspectNetwork(id) => networks::inspect(shared, id).await,
        Route::RemoveNetwork(id) => networks::remove(shared, id).await,
        Route::ConnectNetwork(id) => {
            let body = body.ok_or_else(|| ApiError::internal("the connect's body"))?;
            networks::connect(shared, id, body).await
        }
        Route::DisconnectNetwork(id) => {
            let body = body.ok_or_else(|| ApiError::internal("the disconnect's body"))?;
            networks::disconnect(shared, id, body).await
        }
    }
}

impl Shared {
    /// Where the container `reference` names stands (the engine's answer if
    /// it's not there: 404).
    ///
    /// # Errors
    ///
    /// As [`Shared::call`].
    async fn standing(
        &self,
        reference: String,
    ) -> Result<shipyard_engine::docker::Standing, ApiError> {
        Ok(self
            .call(move |engine| shipyard_engine::docker::standing(engine, &reference))
            .await?
            .0)
    }

    /// Runs `call` on a blocking thread with an engine of its own: its
    /// value, and what the engine warned meanwhile. The service stays up
    /// until it returns, even if its client has gone or it's asked to stop.
    ///
    /// # Errors
    ///
    /// The engine's error, as its status; the service stopping (503: the
    /// call isn't made); a call that panicked (500 in a debug build: a
    /// release build aborts on a panic, the service with it).
    async fn call<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Engine) -> shipyard_engine::Result<T> + Send + 'static,
    ) -> Result<(T, Vec<String>), ApiError> {
        let collect = Arc::new(Collect::default());
        let value = self.call_reporting(Arc::clone(&collect) as _, call).await?;
        let warnings = collect
            .warnings()
            .into_iter()
            .map(|warning| warning.message)
            .collect();
        Ok((value, warnings))
    }

    /// [`Shared::call`], its engine reporting to `reporter` (a pull's
    /// progress, written to its client as it comes).
    ///
    /// # Errors
    ///
    /// As [`Shared::call`].
    async fn call_reporting<T: Send + 'static>(
        &self,
        reporter: Arc<dyn shipyard_engine::report::Reporter>,
        call: impl FnOnce(&Engine) -> shipyard_engine::Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let engine = Engine::new(self.paths.clone(), reporter);
        let busy = self.idle.call().ok_or_else(|| {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the service is stopping")
        })?;
        let joined = tokio::task::spawn_blocking(move || {
            let _busy = busy;
            call(&engine)
        })
        .await;
        Ok(joined.map_err(|error| ApiError::internal(format!("the engine failed: {error}")))??)
    }
}

/// A request's JSON body, up to [`MAX_BODY`].
///
/// # Errors
///
/// A body that's too big (413), can't be read, or isn't the JSON asked for
/// (400).
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
    body: Incoming,
) -> Result<T, ApiError> {
    read_json_or(body, None).await
}

/// A request's JSON body, up to [`MAX_BODY`], or `T`'s default if it's
/// empty (Docker's clients send none for some requests).
///
/// # Errors
///
/// As [`read_json`]'s, but for an empty body.
pub(crate) async fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(
    body: Incoming,
) -> Result<T, ApiError> {
    read_json_or(body, Some(T::default)).await
}

/// A request's JSON body, or what `empty` makes of none (a 400 without it).
async fn read_json_or<T: serde::de::DeserializeOwned>(
    body: Incoming,
    empty: Option<fn() -> T>,
) -> Result<T, ApiError> {
    let bytes: Bytes = Limited::new(body, MAX_BODY)
        .collect()
        .await
        .map_err(|error| {
            if error.is::<http_body_util::LengthLimitError>() {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("the body is over {MAX_BODY} bytes"),
                )
            } else {
                ApiError::bad(format!("the body couldn't be read: {error}"))
            }
        })?
        .to_bytes();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return empty
            .map(|empty| empty())
            .ok_or_else(|| ApiError::bad("a JSON body is needed"));
    }
    serde_json::from_slice(&bytes).map_err(|error| ApiError::bad(format!("invalid JSON: {error}")))
}

/// The request parsers, for the fuzz targets (`fuzz/`): never part of the
/// API.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    /// Everything a request's path, query, filters, bodies and credentials
    /// header go through, on `data` (its first byte the method): any input
    /// is an answer or a refusal, never a panic.
    pub fn request(data: &[u8]) {
        let Some((&first, rest)) = data.split_first() else {
            return;
        };
        let method = match first % 5 {
            0 => hyper::Method::GET,
            1 => hyper::Method::POST,
            2 => hyper::Method::DELETE,
            3 => hyper::Method::HEAD,
            _ => hyper::Method::PUT,
        };
        let text = String::from_utf8_lossy(rest);
        let _ = crate::routes::route(&method, &text);
        let _ = crate::query::Query::parse(Some(&text));
        let filters = Some(text.as_ref());
        let _ = crate::filters::flattened(filters);
        if let Ok(given) = crate::filters::given(filters) {
            use shipyard_engine::docker::filters as docker;
            let _ = docker::ContainerFilters::parse(given.clone());
            let _ = docker::ImageFilters::parse(given.clone());
            let _ = docker::VolumeFilters::parse(given.clone());
            let _ = docker::NetworkFilters::parse(given.clone());
            let _ = docker::PruneFilters::parse(given.clone());
            let _ = docker::ContainerPruneFilters::parse(given.clone(), 0);
            let _ = docker::ImagePruneFilters::parse(given, 0);
        }
        if let Ok(body) = serde_json::from_slice::<crate::create::CreateBody>(rest) {
            let _ = crate::create::request(&body, Some("fuzzed"));
        }
        if let Ok(body) = serde_json::from_slice::<crate::networks::CreateBody>(rest) {
            let _ = crate::networks::network_create(body);
        }
        let _ = serde_json::from_slice::<crate::volumes::CreateBody>(rest);
        if let Ok(body) = serde_json::from_slice::<crate::exec::CreateBody>(rest)
            && let Ok(config) = crate::exec::config(body)
        {
            let _ = crate::exec::child_command("fuzzed", &config, None);
        }
        if let Ok(value) = hyper::header::HeaderValue::from_bytes(rest) {
            let mut headers = hyper::header::HeaderMap::new();
            headers.insert(crate::images::AUTH_HEADER, value);
            let _ = crate::images::credentials(&headers);
        }
    }
}
