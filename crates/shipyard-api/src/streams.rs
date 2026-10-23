//! The endpoints whose answer is a stream (steps 11.2b1, 11.3d2): a
//! container's logs, the events, and what a container uses (`stats`). Each
//! is answered at once (its headers), then written as the engine's reader
//! gives it, through a bounded channel: a
//! client that reads slowly holds the reader back, and one that goes ends it
//! at the reader's next look (its `gone` hook), as does the service's stop.

use std::sync::Arc;

use bytes::Bytes;
use hyper::StatusCode;
use shipyard_engine::docker::{self, EventMessage};
use shipyard_engine::events::{EventsOptions, Filters};
use shipyard_engine::logs::{self, Entry, Gone, LogsOptions};
use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::logs::time::Timestamp;

use crate::Shared;
use crate::errors::ApiError;
use crate::query::Query;
use crate::reply::{self, Body, Sender};

type Reply = Result<hyper::Response<Body>, ApiError>;

/// Docker's content type for a container's output with two streams framed.
const MULTIPLEXED: &str = "application/vnd.docker.multiplexed-stream";
/// Docker's for one with a terminal: its bytes as they are.
const RAW: &str = "application/vnd.docker.raw-stream";

/// The content type of a container's output, as Docker sends it: one
/// stream as it is with a `terminal`, two framed without one.
pub(crate) fn content_type(terminal: bool) -> &'static str {
    if terminal { RAW } else { MULTIPLEXED }
}

/// `bytes` a container wrote on `stream`, as Docker sends them: as they
/// are with a `terminal`, framed without one.
pub(crate) fn output(stream: Stream, bytes: Vec<u8>, terminal: bool) -> Vec<u8> {
    if terminal {
        bytes
    } else {
        frame(stream, &bytes)
    }
}

/// The most streams served at once: each holds one of tokio's blocking
/// threads for as long as it runs, and the other calls need theirs.
pub(crate) const MAX_STREAMS: usize = 128;

/// The longest a reader waits for room in its channel before it asks
/// again whether its client has gone (or the service is stopping): room
/// wakes it at once.
const SEND_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// `GET /containers/{id}/logs`: `stdout`, `stderr` (one at least),
/// `follow`, `since`, `until`, `timestamps`, `tail` (`all` or a number:
/// anything else is all, as Docker reads it). Docker's frames when the
/// container has no terminal, its bytes as they are with one.
pub(crate) async fn logs(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let (stdout, stderr) = (query.flag("stdout"), query.flag("stderr"));
    if !stdout && !stderr {
        return Err(ApiError::bad(
            "Bad parameters: you must choose at least one stream",
        ));
    }
    let since = time(query, "since")?;
    let until = time(query, "until")?;
    let tail = query
        .get("tail")
        .and_then(|tail| tail.parse::<usize>().ok());
    let follow = query.flag("follow");
    let permit = stream_permit(shared)?;
    // Found first: a container that isn't there is refused (404), not a
    // stream that ends at once.
    let standing = shared.standing(id).await?;
    let mut writer = LogWriter {
        terminal: standing.terminal,
        lines: logs::LineWriter::new(query.flag("timestamps")),
        streams: [stdout, stderr],
    };
    let (sender, body) = reply::channel();
    // `until` is the reader's: no line past it, and a follow ends there.
    let hook = gone(shared, &sender);
    let options = LogsOptions {
        tail,
        since,
        until,
        follow,
        identity: None,
        gone: Some(hook.clone()),
    };
    let reader = Arc::clone(shared);
    let id = standing.id.as_str().to_owned();
    let terminal = standing.terminal;
    tokio::spawn(async move {
        let _permit = permit;
        let told = sender.clone();
        let read = reader
            .call(move |engine| {
                logs::of_container(engine, &id, &options, |entry| {
                    send(&sender, &hook, writer.write(&entry))
                })
            })
            .await;
        // A failure after the headers is said in the stream, as Docker
        // says it (its client gone, or the service stopping, has no one
        // to tell).
        if let Err(error) = read
            && !told.is_closed()
            && !reader.idle.is_stopping()
        {
            let said = format!("Error grabbing logs: {}\n", error.message).into_bytes();
            let said = if terminal {
                said
            } else {
                frame(Stream::Stderr, &said)
            };
            let _ = told.send(Bytes::from(said)).await;
        }
    });
    Ok(reply::channelled(
        StatusCode::OK,
        content_type(standing.terminal),
        body,
    ))
}

/// How long between two of `stats`' samples (Docker's daemon's second).
const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// `GET /containers/{id}/stats`: Docker's `ContainerStats`, each with the
/// sample before as its `precpu_stats`. With `stream` (the default) one a
/// second, a JSON object a line, as Docker's daemon sends them: zeros while
/// it isn't running, until it's removed, the client goes or the service
/// stops. Without, one object: two samples a second apart, or only one with
/// `one-shot` (or while it isn't running).
pub(crate) async fn stats(shared: &Arc<Shared>, id: String, query: &Query) -> Reply {
    let stream = query.get("stream").is_none_or(|_| query.flag("stream"));
    let one_shot = query.flag("one-shot");
    if !stream {
        let (sample, _) = shared
            .call(move |engine| {
                let first = docker::stats(engine, &id, None)?;
                if one_shot || !docker::standing(engine, &id)?.running() {
                    return Ok(first);
                }
                std::thread::sleep(STATS_INTERVAL);
                docker::stats(engine, &id, Some(&first))
            })
            .await?;
        return Ok(reply::json(StatusCode::OK, &sample));
    }
    let permit = stream_permit(shared)?;
    // The first sample before the headers: a container that isn't there,
    // or can't be read, is refused with its status, not a stream that ends
    // empty. The rest by its ID.
    let (first, _) = shared
        .call(move |engine| docker::stats(engine, &id, None))
        .await?;
    let id = first.id.clone();
    let (sender, body) = reply::channel();
    let hook = gone(shared, &sender);
    let reader = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        // Its end (the client gone, the service stopping, the container
        // removed or unreadable) ends the stream: no one to tell, or
        // Docker's CLI reads the end as the container's.
        let _ = reader
            .call(move |engine| {
                let mut sample = first;
                loop {
                    let mut line = serde_json::to_vec(&sample).unwrap_or_default();
                    line.push(b'\n');
                    send(&sender, &hook, line)?;
                    if waited_out(&hook) {
                        return Ok(());
                    }
                    sample = docker::stats(engine, &id, Some(&sample))?;
                }
            })
            .await;
    });
    Ok(reply::channelled(StatusCode::OK, "application/json", body))
}

/// Waits [`STATS_INTERVAL`] in [`SEND_WAIT`]s, asking `gone` between them:
/// whether it said its client has gone (or the service is stopping).
fn waited_out(gone: &Gone) -> bool {
    let until = std::time::Instant::now() + STATS_INTERVAL;
    while std::time::Instant::now() < until {
        if gone.ask() {
            return true;
        }
        std::thread::sleep(SEND_WAIT);
    }
    gone.ask()
}

/// `GET /events`: `since`, `until`, `filters` (Docker's JSON). One JSON
/// object a line, Docker's shape; followed until `until`, or for good
/// without one.
pub(crate) fn events(shared: &Arc<Shared>, query: &Query) -> Reply {
    let since = time(query, "since")?;
    let until = time(query, "until")?;
    if let (Some(since), Some(until)) = (since, until)
        && until < since
    {
        return Err(ApiError::bad(format!(
            "`since` time ({since}) cannot be after `until` time ({until})"
        )));
    }
    let filters = crate::filters::flattened(query.get("filters"))?;
    // Checked first: a filter it can't read is refused (400), not a stream.
    let filters = Filters::parse(&filters).map_err(ApiError::from)?;
    let permit = stream_permit(shared)?;
    let (sender, body) = reply::channel();
    let hook = gone(shared, &sender);
    let options = EventsOptions {
        since: since.map(system),
        until: until.map(system),
        filters,
        gone: Some(hook.clone()),
    };
    let reader = Arc::clone(shared);
    tokio::spawn(async move {
        let _permit = permit;
        // Its end, its client's, or a journal that can't be read: the
        // stream ends (Docker's says nothing more either).
        let _ = reader
            .call(move |engine| {
                shipyard_engine::events::read(engine, &options, |event| {
                    let mut line = serde_json::to_vec(&EventMessage::from(&event))
                        .map_err(std::io::Error::other)?;
                    line.push(b'\n');
                    send(&sender, &hook, line)
                })
            })
            .await;
    });
    Ok(reply::channelled(StatusCode::OK, "application/json", body))
}

/// A place among the streams served at once, or Docker's "unavailable"
/// (503) when there are [`MAX_STREAMS`] already.
pub(crate) fn stream_permit(
    shared: &Shared,
) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    Arc::clone(&shared.streams)
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{MAX_STREAMS} streams are served already: try again once one ends"),
            )
        })
}

/// The query's `key` as one of Docker's times, if it gives one (`0`: none,
/// as Docker reads it).
fn time(query: &Query, key: &str) -> Result<Option<Timestamp>, ApiError> {
    match query
        .get(key)
        .filter(|text| !text.is_empty() && *text != "0")
    {
        None => Ok(None),
        Some(text) => docker::parse_time(text)
            .map(Some)
            .ok_or_else(|| ApiError::bad(format!("invalid value for {key}: {text:?}"))),
    }
}

/// `time` as the journal's, one before 1970 as the epoch (none is earlier
/// there).
fn system(time: Timestamp) -> std::time::SystemTime {
    shipyard_engine::events::system_time(time).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// A reader's `gone` hook: its client has gone (the channel is closed), or
/// the service is stopping. (An `until` is the reader's own.)
pub(crate) fn gone(shared: &Arc<Shared>, sender: &Sender) -> Gone {
    let (sender, shared) = (sender.clone(), Arc::clone(shared));
    Gone::new(move || sender.is_closed() || shared.idle.is_stopping())
}

/// Sends `bytes` (none: nothing to send). While the channel is full (its
/// client reads slowly, or not at all) it waits for its client to take a
/// chunk, asking `gone` at least every [`SEND_WAIT`]: a client that stopped
/// reading never holds the service's stop up. A client gone, or `gone`, is
/// the sink's error, which ends its reader.
pub(crate) fn send(sender: &Sender, gone: &Gone, bytes: Vec<u8>) -> std::io::Result<()> {
    use tokio::sync::mpsc::error::TrySendError;
    if bytes.is_empty() {
        return Ok(());
    }
    let mut bytes = Bytes::from(bytes);
    loop {
        // Read before the try: a chunk taken between the two still wakes it.
        let seen = sender.progress().seen();
        match sender.try_send(bytes) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(back)) if !gone.ask() => {
                sender.progress().wait_past(seen, SEND_WAIT);
                bytes = back;
            }
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
        }
    }
}

/// How a container's log lines are written: framed or raw, the streams
/// asked for; each line as the CLI prints it
/// ([`logs::LineWriter`]).
#[derive(Debug)]
pub(crate) struct LogWriter {
    /// One stream, its bytes as they are (the container has a terminal).
    terminal: bool,
    /// Each entry as its line's bytes.
    lines: logs::LineWriter,
    /// Stdout's and stderr's, whether they're asked for.
    streams: [bool; 2],
}

impl LogWriter {
    /// What `entry` is written as: nothing if its stream isn't asked for.
    pub(crate) fn write(&mut self, entry: &Entry) -> Vec<u8> {
        if !self.streams[entry.stream.index()] {
            return Vec::new();
        }
        output(entry.stream, self.lines.bytes(entry), self.terminal)
    }
}

/// Docker's frame for `payload` on `stream`: its stream (1 stdout, 2
/// stderr), three zeros, its length (big-endian, 32 bits), then it.
pub(crate) fn frame(stream: Stream, payload: &[u8]) -> Vec<u8> {
    let kind: u8 = match stream {
        Stream::Stdout => 1,
        Stream::Stderr => 2,
    };
    // A line is at most a MiB (the logs' `MAX_LINE`): its length fits.
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut framed = Vec::with_capacity(8 + payload.len());
    framed.extend_from_slice(&[kind, 0, 0, 0]);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

#[cfg(test)]
mod tests;
