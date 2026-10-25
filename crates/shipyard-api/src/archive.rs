//! `docker cp` (step 11.2c3a): `GET /containers/{id}/archive?path=` sends
//! a tar of the path, `HEAD` only what the path is, `PUT` extracts a tar
//! into a directory: as `shipyard cp` does it (`cp::copy_by_name`), the
//! container running or not.
//!
//! What a path is travels in `X-Docker-Container-Path-Stat`: base64 of
//! Docker's `{"name","size","mode","mtime","linkTarget"}`, its mode Go's
//! `os.FileMode`, as Docker's CLI reads it. A GET's tar goes as it's made;
//! a copy that fails half way breaks the stream off (the client sees a
//! broken stream, never a whole one). A PUT's body is a plain tar or a gzip
//! one (D11.2c3-2; bzip2 and xz are refused), read as it comes through a
//! bounded channel to the copy's blocking thread: never whole in memory,
//! and an error in it is an error, never the archive's end.
//!
//! Either way the copy holds its container (its operation lock): a client
//! that stops reading or sending for [`STALL`] is let go, and so is one
//! when the service stops, so a container is never held for good.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::{Buf as _, Bytes};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Response, StatusCode};
use serde::Serialize;
use shipyard_engine::cp::{CpError, CpOptions, CpStreams, PathStat, Transfer};
use shipyard_engine::logs::Gone;
use tokio::sync::mpsc;

use crate::Shared;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body, Sender};
use crate::streams::{self, send};

type Reply = Result<Response<Body>, ApiError>;

/// The header a path's stat travels in.
const STAT_HEADER: &str = "x-docker-container-path-stat";

/// How many of a PUT body's frames wait for the copy at most.
pub(crate) const BODY_FRAMES: usize = 16;

/// How long a copy waits for its client to read or send more before it
/// lets the container go.
const STALL: Duration = Duration::from_mins(5);

/// The longest a PUT's copy waits for its body's next frame before it
/// asks again whether to go on (a frame wakes it at once).
const BODY_LOOK: Duration = Duration::from_millis(50);

/// The most of an archive's start read to know its compression (xz's
/// magic, the longest).
const MAGIC: usize = 6;

/// Docker's stat of a path, as its header carries it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DockerStat {
    name: String,
    size: u64,
    mode: u32,
    mtime: String,
    link_target: String,
}

/// `stat` as Docker's header value: base64 of its JSON. A time that can't
/// be written (past what RFC 3339 holds) is the epoch's.
fn stat_header(stat: &PathStat) -> Result<HeaderValue, ApiError> {
    let failed =
        |error: &dyn std::fmt::Display| ApiError::internal(format!("the path's stat: {error}"));
    let mtime = std::time::UNIX_EPOCH
        .checked_add(Duration::new(
            u64::try_from(stat.mtime_secs).unwrap_or(0),
            stat.mtime_nanos,
        ))
        .map(shipyard_engine::docker::time)
        .filter(|written| !written.is_empty())
        .unwrap_or_else(|| shipyard_engine::docker::time(std::time::UNIX_EPOCH));
    let shown = DockerStat {
        name: stat.name.clone(),
        size: stat.size,
        mode: stat.docker_mode(),
        mtime,
        link_target: stat.link_target.clone(),
    };
    let json = serde_json::to_vec(&shown).map_err(|error| failed(&error))?;
    HeaderValue::from_str(&base64::engine::general_purpose::STANDARD.encode(json))
        .map_err(|error| failed(&error))
}

/// The query's `path`.
fn path_of(query: &Query) -> Result<PathBuf, ApiError> {
    query
        .get("path")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ApiError::bad("an archive needs path"))
}

/// `error`, in Docker's words if it's the path not there (a copy's
/// [`CpError::NotFound`]: never the container's).
fn path_missing(error: shipyard_engine::Error, id: &str, path: &Path) -> shipyard_engine::Error {
    if matches!(error.find::<CpError>(), Some(CpError::NotFound(_))) {
        shipyard_engine::Error::new(
            shipyard_engine::ErrorKind::NotFound,
            format!(
                "Could not find the file {} in container {id}",
                path.display()
            ),
        )
    } else {
        error
    }
}

/// What `path` is in container `id`: Docker's 404 if it isn't there.
async fn stat(shared: &Arc<Shared>, id: String, path: PathBuf) -> Result<PathStat, ApiError> {
    let (stat, _) = shared
        .call(move |engine| {
            shipyard_engine::cp::stat(engine, &id, &path)
                .map_err(|error| path_missing(error, &id, &path))
        })
        .await?;
    Ok(stat)
}

/// `HEAD /containers/{id}/archive?path=`: 200 with the path's stat, 404 if
/// it isn't there.
///
/// # Errors
///
/// No `path` (400); as the engine's.
pub(crate) async fn head(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let path = path_of(query)?;
    let stat = stat(shared, id, path).await?;
    let mut response = reply::empty(StatusCode::OK);
    response
        .headers_mut()
        .insert(STAT_HEADER, stat_header(&stat)?);
    Ok(response)
}

/// `GET /containers/{id}/archive?path=`: the path's stat, then a tar of it
/// as it's made. A copy that fails after the headers (the container removed
/// meanwhile, the service stopping) breaks the stream off.
///
/// # Errors
///
/// No `path` (400); not there (404); as the engine's.
pub(crate) async fn get(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let path = path_of(query)?;
    let stat = stat(shared, id.clone(), path.clone()).await?;
    let header = stat_header(&stat)?;
    let permit = streams::stream_permit(shared)?;
    let (sender, cut, body) = reply::cuttable();
    let writer = ChannelWriter::new(shared, sender.clone());
    let copier = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        // Held until the copy's fate is known: the stream ends only after.
        let _sender = sender;
        let transfer = Transfer::Out { id, path, to: None };
        let copied = copier
            .call(move |engine| {
                let mut output = writer;
                let mut input = std::io::empty();
                let mut streams = CpStreams {
                    input: &mut input,
                    output: &mut output,
                };
                shipyard_engine::cp::copy_by_name(
                    engine,
                    &transfer,
                    &mut streams,
                    CpOptions::default(),
                )
            })
            .await;
        if let Err(error) = copied {
            tracing::debug!("a copy out broke off: {}", error.message);
            cut.cut();
        }
    });
    let mut response = reply::channelled(StatusCode::OK, "application/x-tar", body);
    response.headers_mut().insert(STAT_HEADER, header);
    Ok(response)
}

/// `GET /containers/{id}/export` (`docker export`, step 12.2b): the
/// container's own filesystem as one tar, as the engine's
/// `cp::export_by_name` writes it, sent as it's made; never its volumes.
/// One that isn't there is 404 before the headers; a failure after them
/// breaks the stream off.
///
/// # Errors
///
/// Not there (404); more streams than are served (503).
pub(crate) async fn export(shared: &Arc<Shared>, id: String) -> Reply {
    let name = id.clone();
    shared
        .call(move |engine| shipyard_engine::docker::inspect_container(engine, &name))
        .await
        .map_err(|error| match error.status {
            StatusCode::NOT_FOUND => {
                ApiError::new(StatusCode::NOT_FOUND, format!("No such container: {id}"))
            }
            _ => error,
        })?;
    let permit = streams::stream_permit(shared)?;
    let (sender, cut, body) = reply::cuttable();
    let mut writer = ChannelWriter::new(shared, sender.clone());
    let exporter = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        // Held until the export's fate is known: the stream ends only after.
        let _sender = sender;
        let exported = exporter
            .call(move |engine| shipyard_engine::cp::export_by_name(engine, &id, &mut writer))
            .await;
        if let Err(error) = exported {
            tracing::warn!(error = %error.message, "an export broke off: its archive was cut");
            cut.cut();
        }
    });
    Ok(reply::channelled(StatusCode::OK, "application/x-tar", body))
}

/// A tar written into a stream's channel, as the client reads it; given up
/// on when the client has gone, the service stops, or nothing has been
/// taken for [`STALL`].
pub(crate) struct ChannelWriter {
    sender: Sender,
    gone: Gone,
    last: Arc<Mutex<Instant>>,
}

impl ChannelWriter {
    pub(crate) fn new(shared: &Arc<Shared>, sender: Sender) -> Self {
        let base = streams::gone(shared, &sender);
        let last = Arc::new(Mutex::new(Instant::now()));
        let since = Arc::clone(&last);
        let gone = Gone::new(move || {
            base.ask()
                || since
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .elapsed()
                    > STALL
        });
        Self { sender, gone, last }
    }
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        send(&self.sender, &self.gone, bytes.to_vec())?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `PUT /containers/{id}/archive?path=` (`copyUIDGID`: the archive's
/// owners kept; `noOverwriteDirNonDir`: always so): the body (a tar, plain
/// or gzip) extracted into the directory `path`. 200; `path` not a
/// directory, or an entry that would replace a directory with a file or
/// the reverse, 400; not there, 404; a read-only root or volume, 403.
///
/// # Errors
///
/// As above; bzip2 or xz (400, once its container is found); more
/// transfers than the service serves at once (503).
pub(crate) async fn put(shared: &Arc<Shared>, id: String, query: &Query, body: Incoming) -> Reply {
    let path = path_of(query)?;
    let archive = query.flag("copyUIDGID");
    let permit = streams::stream_permit(shared)?;
    let (frames, receiver) = mpsc::channel(BODY_FRAMES);
    let progress = Arc::new(reply::Progress::default());
    tokio::spawn(pump_body(body, frames, Arc::clone(&progress)));
    let service = Arc::clone(shared);
    let stopping = Gone::new(move || service.idle.is_stopping());
    let (refused, _) = shared
        .call(move |engine| {
            let _permit = permit;
            // Its container first, as Docker's: a missing one is 404,
            // whatever the body is (F121).
            shipyard_engine::docker::standing(engine, &id)?;
            let mut input = match decompressed(BodyReader::new(receiver, progress, stopping)) {
                Ok(Ok(input)) => input,
                Ok(Err(why)) => return Ok(Some(why)),
                Err(error) => {
                    return Err(shipyard_engine::Error::new(
                        shipyard_engine::ErrorKind::InvalidRequest,
                        format!("the archive couldn't be read: {error}"),
                    ));
                }
            };
            let mut output = std::io::sink();
            let mut streams = CpStreams {
                input: &mut input,
                output: &mut output,
            };
            let options = CpOptions {
                archive,
                ..CpOptions::default()
            };
            let transfer = Transfer::In {
                from: None,
                id: id.clone(),
                path: path.clone(),
            };
            match shipyard_engine::cp::copy_by_name(engine, &transfer, &mut streams, options) {
                Ok(_) => Ok(None),
                Err(error) if read_only(&error) => {
                    Ok(Some(Refusal::ReadOnly(format!("{error:#}"))))
                }
                Err(error) => Err(path_missing(error, &id, &path)),
            }
        })
        .await?;
    match refused {
        None => Ok(reply::empty(StatusCode::OK)),
        Some(Refusal::Compressed(what)) => Err(ApiError::bad(format!(
            "a {what} archive isn't supported: send a plain tar or a gzip one"
        ))),
        Some(Refusal::ReadOnly(why)) => Err(ApiError::new(StatusCode::FORBIDDEN, why)),
    }
}

/// Why a PUT is refused, decided on its blocking thread.
#[derive(Debug)]
enum Refusal {
    /// A compression Shipyard doesn't read.
    Compressed(&'static str),
    /// The destination is read-only (the root, or a volume).
    ReadOnly(String),
}

/// Whether `error` is the container's file system refusing to be written
/// (`--read-only`, a read-only volume: `EROFS`): Docker's 403. Another
/// refusal (a directory the container's root can't write) is an error of
/// its own, as Docker's.
fn read_only(error: &shipyard_engine::Error) -> bool {
    matches!(
        error.find::<CpError>(),
        Some(CpError::InContainer(failure)) if failure.errno == libc::EROFS
    )
}

/// The body's frames into `frames`, as they come: an error in it sent as
/// one (the copy fails), its end the channel's. Each frame sent, and its
/// end however it comes, bumps `progress`: the copy's reader waits on it.
pub(crate) async fn pump_body(
    mut body: Incoming,
    frames: mpsc::Sender<std::io::Result<Bytes>>,
    progress: Arc<reply::Progress>,
) {
    // (Its end: a guard for a task dropped mid-way, and below, the sender
    // dropped first, so the reader woken sees the body's end at once.)
    let _ended = reply::BumpOnDrop(Arc::clone(&progress));
    let reader_gone = pump_frames(&mut body, &frames, &progress).await;
    drop(frames);
    progress.bump();
    if reader_gone {
        drain(&mut body).await;
    }
}

/// The most of a body read and dropped once its reader has gone.
const DRAIN_MOST: u64 = 1024 * 1024 * 1024;

/// How long a body is read and dropped once its reader has gone.
const DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Reads and drops what's left of `body` (at most [`DRAIN_MOST`], for
/// [`DRAIN_WAIT`]): its reader refused it before its end (a load or an
/// import with no name, an archive found unsound half way), and a client
/// still sending would otherwise meet a closed connection, a broken pipe,
/// instead of reading the refusal (Docker's CLI does: 12.2b).
async fn drain(body: &mut Incoming) {
    /// Drains at once: past it, a refused body's client meets a closed
    /// connection, as before, rather than any number of them reading.
    const MOST: usize = 8;
    static DRAINING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct Draining;
    impl Drop for Draining {
        fn drop(&mut self) {
            DRAINING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if DRAINING.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MOST {
        DRAINING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let _draining = Draining;
    let deadline = tokio::time::Instant::now() + DRAIN_WAIT;
    let mut drained = 0_u64;
    while drained <= DRAIN_MOST {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                drained += frame.data_ref().map_or(0, |data| data.len() as u64);
            }
            _ => return,
        }
    }
}

/// [`pump_body`]'s frames, until the body's end, an error in it, or its
/// reader gone: whether it was that.
async fn pump_frames(
    body: &mut Incoming,
    frames: &mpsc::Sender<std::io::Result<Bytes>>,
    progress: &reply::Progress,
) -> bool {
    while let Some(frame) = body.frame().await {
        let next = match frame {
            Ok(frame) => match frame.into_data() {
                Ok(data) => Ok(data),
                // Trailers: nothing of the archive's.
                Err(_) => continue,
            },
            Err(error) => Err(std::io::Error::other(error)),
        };
        let failed = next.is_err();
        if frames.send(next).await.is_err() {
            return true;
        }
        if failed {
            return false;
        }
        progress.bump();
    }
    false
}

/// A request body as an engine call reads it (`load`'s, `import`'s): a
/// stream permit, held for the call, and the body as a blocking reader,
/// its frames pumped as they come (the rest drained if the call refuses it
/// before its end, so the client reads the refusal).
///
/// # Errors
///
/// More streams than are served (503).
pub(crate) fn body_input(
    shared: &Arc<Shared>,
    body: Incoming,
) -> Result<(tokio::sync::OwnedSemaphorePermit, BodyReader), ApiError> {
    let permit = streams::stream_permit(shared)?;
    let (frames, receiver) = mpsc::channel(BODY_FRAMES);
    let progress = Arc::new(reply::Progress::default());
    tokio::spawn(pump_body(body, frames, Arc::clone(&progress)));
    let service = Arc::clone(shared);
    let stopping = Gone::new(move || service.idle.is_stopping());
    Ok((permit, BodyReader::new(receiver, progress, stopping)))
}

/// A request body, read on a blocking thread: its frames as they come. An
/// error in it is one here too, and stays one (never its end); so is
/// [`STALL`] without a byte, or the service stopping.
pub(crate) struct BodyReader {
    frames: mpsc::Receiver<std::io::Result<Bytes>>,
    /// Bumped by the pump as frames come and as it ends.
    progress: Arc<reply::Progress>,
    current: Bytes,
    stopping: Gone,
    /// The error it ended on, said again to every later read.
    failed: Option<(std::io::ErrorKind, String)>,
}

impl BodyReader {
    pub(crate) fn new(
        frames: mpsc::Receiver<std::io::Result<Bytes>>,
        progress: Arc<reply::Progress>,
        stopping: Gone,
    ) -> Self {
        Self {
            frames,
            progress,
            current: Bytes::new(),
            stopping,
            failed: None,
        }
    }

    /// The next frame: `None` at the body's end.
    fn next_frame(&mut self) -> std::io::Result<Option<Bytes>> {
        use tokio::sync::mpsc::error::TryRecvError;
        let since = Instant::now();
        loop {
            // Read before the try: a frame sent between the two still wakes it.
            let seen = self.progress.seen();
            match self.frames.try_recv() {
                Ok(frame) => return frame.map(Some),
                Err(TryRecvError::Disconnected) => return Ok(None),
                Err(TryRecvError::Empty) if self.stopping.ask() => {
                    return Err(std::io::Error::other("the service is stopping"));
                }
                Err(TryRecvError::Empty) if since.elapsed() > STALL => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "the archive stopped coming",
                    ));
                }
                Err(TryRecvError::Empty) => self.progress.wait_past(seen, BODY_LOOK),
            }
        }
    }
}

impl Read for BodyReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if let Some((kind, why)) = &self.failed {
            return Err(std::io::Error::new(*kind, why.clone()));
        }
        while !self.current.has_remaining() {
            match self.next_frame() {
                Ok(Some(frame)) => self.current = frame,
                Ok(None) => return Ok(0),
                Err(error) => {
                    self.failed = Some((error.kind(), error.to_string()));
                    return Err(error);
                }
            }
        }
        let n = buffer.len().min(self.current.remaining());
        self.current.copy_to_slice(&mut buffer[..n]);
        Ok(n)
    }
}

/// `body` as a tar: as it is, or gzip's decoded; another compression a
/// refusal. Its first bytes are read whole to know (however the body's
/// frames split them).
///
/// # Errors
///
/// The body failed before its first bytes.
fn decompressed(
    mut body: impl Read + Send + 'static,
) -> std::io::Result<Result<Box<dyn Read + Send>, Refusal>> {
    let mut start = Vec::with_capacity(MAGIC);
    let mut byte = [0_u8; MAGIC];
    while start.len() < MAGIC {
        let wanted = MAGIC - start.len();
        let read = body.read(&mut byte[..wanted])?;
        if read == 0 {
            break;
        }
        start.extend_from_slice(&byte[..read]);
    }
    let kind = compression(&start);
    let whole = Read::chain(std::io::Cursor::new(start), body);
    Ok(match kind {
        Compression::Gzip => Ok(Box::new(flate2::read::MultiGzDecoder::new(BufReader::new(
            whole,
        )))),
        Compression::Bzip2 => Err(Refusal::Compressed("bzip2")),
        Compression::Xz => Err(Refusal::Compressed("xz")),
        Compression::None => Ok(Box::new(BufReader::new(whole))),
    })
}

/// What an archive's first bytes say it's compressed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
}

/// The compression `start` (an archive's first bytes) says, by its magic.
fn compression(start: &[u8]) -> Compression {
    if start.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else if start.starts_with(b"BZh") {
        Compression::Bzip2
    } else if start.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Compression::Xz
    } else {
        Compression::None
    }
}

#[cfg(test)]
mod tests;
