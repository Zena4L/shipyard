//! Attach (step 11.2b2a): `POST /containers/{id}/attach`, the connection
//! taken over (HTTP's upgrade) and joined to the container's monitor, as
//! Docker's clients attach (`docker attach`, `docker run` without `-d`,
//! `docker start -a`); and `POST /containers/{id}/resize`.
//!
//! The monitor is spoken to in its own frames, over its attach socket
//! (`runtime::container::attach_stream`): output comes as `Stdout`/`Stderr`
//! frames, written to the client as Docker writes them
//! ([`streams::output`]); the client's bytes go as `Stdin` frames, its
//! half-close as `StdinEnd`. The attach is held (`HelloHeld`): a client that
//! reads slowly holds its container's output back, as Docker's attach does,
//! never the service (each session is a task of its own). A client that
//! hangs up (closes both ways, not a half-close) ends its session, or its
//! wait.
//!
//! An attach to a container not running (made, or ended) waits in
//! [`Pending`] for its next start: the API's start lets it in before the
//! container's start gate opens ([`hand_over`]), so nothing the command
//! writes first is missed.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use hyper::StatusCode;
use hyper::upgrade::OnUpgrade;
use shipyard_engine::containers::ContainerState;
use shipyard_engine::docker::{End, Standing};
use shipyard_runtime::attach::client::{AttachError, AttachOptions, WelcomedParts};
use shipyard_runtime::attach::{Decoder, DetachKeys, Frame, FrameError};
use shipyard_runtime::container::ContainerError;
use shipyard_runtime::logs::cri::Stream;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

use crate::Shared;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::Body;
use crate::streams;

type Reply = Result<hyper::Response<Body>, ApiError>;

/// How often an attach waiting for its container's start looks whether it
/// was started another way (the CLI, a restart policy), or its client has
/// hung up; and how often a quiet session asks after its client.
const LOOK: Duration = Duration::from_millis(200);

/// The most a read moves at once (a `Stdin` frame's payload is at most
/// this).
const CHUNK: usize = 32 * 1024;

/// What an attach asks for.
#[derive(Debug, Clone)]
struct Wanted {
    /// The client's input passed on.
    stdin: bool,
    /// Stdout's and stderr's output passed on.
    streams: [bool; 2],
    /// The keys that detach.
    keys: DetachKeys,
}

/// Why an attach couldn't join its container's monitor.
#[derive(Debug, thiserror::Error)]
pub(crate) enum JoinError {
    /// Its socket couldn't be reached (the container ended, or never ran).
    #[error(transparent)]
    Connect(#[from] ContainerError),
    /// The monitor didn't let it in (too many clients, ended).
    #[error(transparent)]
    Welcome(#[from] AttachError),
}

/// `POST /containers/{id}/attach` (`stream`, `stdin`, `stdout`, `stderr`,
/// `detachKeys`): 101, then the session. A paused or restarting container
/// is refused (409, as Docker's); one that isn't running (made, or ended:
/// `docker start -a` attaches first) is waited for until its next start.
pub(crate) async fn attach(
    shared: &Arc<Shared>,
    id: String,
    query: &Query,
    upgrade: Option<OnUpgrade>,
) -> Reply {
    let upgrade = upgrade.ok_or_else(|| {
        ApiError::bad("attach takes the connection over: ask with `Upgrade: tcp`")
    })?;
    if query.flag("logs") {
        return Err(ApiError::bad(
            "logs=1 isn't supported on attach yet: read them with GET /containers/{id}/logs",
        ));
    }
    let keys = match query.get("detachKeys").filter(|keys| !keys.is_empty()) {
        Some(keys) => DetachKeys::parse(keys).map_err(|error| ApiError::bad(error.to_string()))?,
        None => DetachKeys::default_keys(),
    };
    let wanted = Wanted {
        stdin: query.flag("stdin"),
        streams: [query.flag("stdout"), query.flag("stderr")],
        keys,
    };
    let standing = shared.standing(id).await?;
    if standing.state == ContainerState::Paused || standing.restarting() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "container {} is {}: attach once it's running",
                standing.id,
                standing.state.word()
            ),
        ));
    }
    let id = standing.id.as_str().to_owned();
    // Waiting for its start: in the table before the answer, so a start
    // asked right after it (`docker run`, `docker start -a`) finds it.
    let waiting = (!standing.running())
        .then(|| (shared.pending.register(&id, &wanted.keys), standing.ended()));
    let busy = shared.idle.connection();
    let session = Arc::clone(shared);
    tokio::spawn(async move {
        let _busy = busy;
        let Some((client, typed)) = crate::hijack::take_over(upgrade).await else {
            return;
        };
        let joined = match waiting {
            Some((waiting, before)) => {
                wait_for_start(&session, &id, &wanted, waiting, before, &client).await
            }
            None => welcome(&session, &id, &wanted).await,
        };
        if let Some(parts) = joined {
            run(client, typed, parts, &wanted).await;
        }
    });
    Ok(crate::hijack::switching(streams::content_type(
        standing.terminal,
    )))
}

/// Joins container `id`'s monitor now: welcomed, its parts. `None` if it
/// can't be (it has ended, or its monitor refused: said in the service's
/// log).
async fn welcome(shared: &Arc<Shared>, id: &str, wanted: &Wanted) -> Option<WelcomedParts> {
    let root = shared.paths.runtime_root().ok()?.to_owned();
    let (id, keys) = (id.to_owned(), wanted.keys.clone());
    let joined = tokio::task::spawn_blocking(move || welcomed(&root, &id, keys))
        .await
        .ok()?;
    joined
        .inspect_err(|error| tracing::debug!("an attach couldn't join its container: {error}"))
        .ok()
}

/// Waits for container `id`'s next start to let it in (the API's start
/// hands it over before the gate opens); one started another way is
/// joined as soon as it's seen running. `None` if its client hangs up, or
/// the container goes, or ends again without a start seen (`before` is
/// how it had ended when the attach came).
async fn wait_for_start(
    shared: &Arc<Shared>,
    id: &str,
    wanted: &Wanted,
    waiting: oneshot::Receiver<Result<WelcomedParts, JoinError>>,
    before: Option<End>,
    client: &UnixStream,
) -> Option<WelcomedParts> {
    // The start's handover, while it may still come; then looks alone.
    let mut waiting = Some(waiting);
    loop {
        if let Some(receiver) = waiting.as_mut() {
            match tokio::time::timeout(LOOK, receiver).await {
                Ok(Ok(Ok(parts))) => return Some(parts),
                Ok(Ok(Err(error))) => {
                    tracing::debug!("an attach wasn't let in at its container's start: {error}");
                    waiting = None;
                }
                // Dropped from the table: as it is now, by looking.
                Ok(Err(_)) => waiting = None,
                // Timed out: looked at below.
                Err(_) => {}
            }
        } else {
            tokio::time::sleep(LOOK).await;
        }
        if crate::hijack::hung_up(client.as_fd()) {
            return None;
        }
        let standing = shared.standing(id.to_owned()).await.ok()?;
        // Handed over while it was looked at: the handover first, so the
        // output from the start on isn't missed.
        if let Some(receiver) = waiting.as_mut()
            && let Ok(Ok(parts)) = receiver.try_recv()
        {
            return Some(parts);
        }
        if standing.running() {
            return welcome(shared, id, wanted).await;
        }
        if standing.ended() != before {
            return None;
        }
    }
}

/// Welcomed by container `id`'s monitor, held, with `keys`: blocking.
fn welcomed(root: &Path, id: &str, keys: DetachKeys) -> Result<WelcomedParts, JoinError> {
    let stream = shipyard_runtime::container::attach_stream(root, id)?;
    // On the wire: the keys, and held. (`stdin` is the CLI client's own.)
    let options = AttachOptions {
        detach_keys: keys,
        stdin: false,
        held: true,
        output: shipyard_runtime::attach::client::Output::BOTH,
    };
    Ok(shipyard_runtime::attach::client::welcomed(stream, &options)?.into_parts())
}

/// The session: the monitor's output to the client (only the streams
/// asked for, as Docker writes them), the client's input to the monitor
/// (only if asked, and the container takes it), until the container ends,
/// the detach keys are typed, the monitor lets go, or the client hangs up.
async fn run(client: UnixStream, typed: Bytes, parts: WelcomedParts, wanted: &Wanted) {
    let Ok(monitor) = parts
        .stream
        .set_nonblocking(true)
        .and_then(|()| UnixStream::from_std(parts.stream))
    else {
        return;
    };
    let (mut from_monitor, to_monitor) = monitor.into_split();
    let (from_client, mut to_client) = client.into_split();
    let input = tokio::spawn(pass_input(
        from_client,
        typed,
        to_monitor,
        wanted.stdin && parts.stdin,
    ));
    let mut decoder = Decoder::new();
    decoder.push(&parts.unread);
    let mut buffer = vec![0_u8; CHUNK];
    'session: loop {
        loop {
            let (stream, bytes) = match decoder.next_frame() {
                Ok(Some(Frame::Stdout(bytes))) => (Stream::Stdout, bytes),
                Ok(Some(Frame::Stderr(bytes))) => (Stream::Stderr, bytes),
                Ok(Some(Frame::Exited(_) | Frame::Detached | Frame::Dropped(_))) => {
                    break 'session;
                }
                // Another frame, or one of a kind this end doesn't know:
                // skipped, as the runtime's client skips it.
                Ok(Some(_)) | Err(FrameError::Kind(_)) => continue,
                Ok(None) => break,
                // Nothing after it can be trusted.
                Err(_) => break 'session,
            };
            if !wanted.streams[stream.index()] {
                continue;
            }
            let out = streams::output(stream, bytes, parts.terminal);
            if to_client.write_all(&out).await.is_err() {
                break 'session;
            }
        }
        match tokio::time::timeout(LOOK, from_monitor.read(&mut buffer)).await {
            Ok(Ok(read)) if read > 0 => decoder.push(buffer.get(..read).unwrap_or_default()),
            Ok(_) => break,
            // Quiet: still there?
            Err(_) if crate::hijack::hung_up(to_client.as_ref().as_fd()) => break,
            Err(_) => {}
        }
    }
    let _ = to_client.shutdown().await;
    input.abort();
}

/// The client's input to the monitor: what was read with its request
/// (`typed`) first, then what it sends, as `Stdin` frames if `stdin`, and
/// `StdinEnd` at its end; read and dropped otherwise (its end still seen).
/// The monitor's write half is held until the session ends: dropped, it
/// shuts the socket down for writing, and the monitor takes that for the
/// client leaving (Docker's CLI half-closes at once when it sends no
/// input).
async fn pass_input(
    mut from_client: tokio::net::unix::OwnedReadHalf,
    typed: Bytes,
    mut to_monitor: tokio::net::unix::OwnedWriteHalf,
    stdin: bool,
) {
    let send = |bytes: &[u8]| {
        let mut framed = Vec::new();
        Frame::Stdin(bytes.to_vec()).encode(&mut framed);
        framed
    };
    let mut ok = true;
    if stdin {
        for piece in typed.chunks(CHUNK) {
            ok = ok && to_monitor.write_all(&send(piece)).await.is_ok();
        }
    }
    let mut buffer = vec![0_u8; CHUNK];
    while ok {
        let read = from_client.read(&mut buffer).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        if stdin {
            ok = to_monitor
                .write_all(&send(buffer.get(..read).unwrap_or_default()))
                .await
                .is_ok();
        }
    }
    if ok && stdin {
        let mut bytes = Vec::new();
        Frame::StdinEnd.encode(&mut bytes);
        let _ = to_monitor.write_all(&bytes).await;
    }
    std::future::pending::<()>().await;
    drop(to_monitor);
}

/// Attaches waiting for their container's start, by container ID.
#[derive(Debug, Default)]
pub(crate) struct Pending(Mutex<HashMap<String, Vec<Waiting>>>);

/// One attach waiting: its keys, and where its welcome goes.
#[derive(Debug)]
struct Waiting {
    keys: DetachKeys,
    sender: oneshot::Sender<Result<WelcomedParts, JoinError>>,
}

impl Pending {
    /// An attach to container `id` waits for its start, with `keys`.
    fn register(
        &self,
        id: &str,
        keys: &DetachKeys,
    ) -> oneshot::Receiver<Result<WelcomedParts, JoinError>> {
        let (sender, receiver) = oneshot::channel();
        let mut table = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Those whose wait has ended (their client gone) are dropped as
        // others come, whichever container they waited for.
        table.retain(|_, waiting| {
            waiting.retain(|waiting| !waiting.sender.is_closed());
            !waiting.is_empty()
        });
        table.entry(id.to_owned()).or_default().push(Waiting {
            keys: keys.clone(),
            sender,
        });
        receiver
    }

    /// The attaches waiting for container `id`, taken from the table.
    fn take(&self, id: &str) -> Vec<Waiting> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .unwrap_or_default()
    }
}

/// The start's `between` (blocking, before the container's start gate
/// opens, its `op.lock` held): each attach waiting for container `id`
/// welcomed by its monitor now, and handed its connection; one whose wait
/// has ended is skipped ([`hand_over_with`]).
pub(crate) fn hand_over(pending: &Pending, root: &Path, id: &str) {
    hand_over_with(pending.take(id), &|keys| welcomed(root, id, keys));
}

/// How many attaches are welcomed: the monitor's own bound on its clients
/// (`MAX_CLIENTS`), past which it refuses one.
const WELCOMES_AT_ONCE: usize = 16;

/// `waiting`'s attaches each welcomed by `welcome` and handed what it
/// gives, in parallel (F116: one by one, up to 10 s each, held a `stop` or
/// `rm` of the container back), so the start waits one welcome's wait at
/// most. Only the first [`WELCOMES_AT_ONCE`]: the monitor would refuse
/// more. The others, and any whose thread can't be made, are let go (their
/// sender dropped): each looks for the container running itself.
fn hand_over_with(
    waiting: Vec<Waiting>,
    welcome: &(dyn Fn(DetachKeys) -> Result<WelcomedParts, JoinError> + Sync),
) {
    std::thread::scope(|scope| {
        for one in waiting
            .into_iter()
            .filter(|waiting| !waiting.sender.is_closed())
            .take(WELCOMES_AT_ONCE)
        {
            // Refused (a task limit), it's dropped with its sender.
            let _ = std::thread::Builder::new().spawn_scoped(scope, move || {
                let _ = one.sender.send(welcome(one.keys));
            });
        }
    });
}

/// `POST /containers/{id}/resize` (`h`, `w`): the container's terminal
/// resized, through its monitor. One without a terminal: nothing to do
/// (200, as Docker's); one not running: 409.
pub(crate) async fn resize(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let rows: u16 = query
        .parsed("h")?
        .ok_or_else(|| ApiError::bad("resize needs h and w"))?;
    let cols: u16 = query
        .parsed("w")?
        .ok_or_else(|| ApiError::bad("resize needs h and w"))?;
    let standing: Standing = shared.standing(id).await?;
    if !standing.terminal {
        return Ok(crate::reply::empty(StatusCode::OK));
    }
    let root = shared
        .paths
        .runtime_root()
        .map_err(ApiError::from)?
        .to_owned();
    let id = standing.id.as_str().to_owned();
    let resized = tokio::task::spawn_blocking(move || -> Result<(), JoinError> {
        let stream = shipyard_runtime::container::attach_stream(&root, &id)?;
        Ok(shipyard_runtime::attach::client::resize(
            stream, rows, cols,
        )?)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?;
    resized.map_err(|why| {
        ApiError::new(
            StatusCode::CONFLICT,
            format!("the container's terminal can't be resized: {why}"),
        )
    })?;
    Ok(crate::reply::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests;
