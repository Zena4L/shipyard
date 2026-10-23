//! Responses as Docker writes them: JSON bodies, empty ones, refusals,
//! and the headers every response carries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, combinators::BoxBody};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{Response, StatusCode};
use serde::Serialize;

use crate::errors::ApiError;
use crate::routes::API_VERSION;

/// A response's body. Its error is a stream's cut short ([`Cut`]): hyper
/// then ends the connection without the body's end, so the client sees a
/// stream that broke off, never one that ended.
pub(crate) type Body = BoxBody<Bytes, std::io::Error>;

/// A body of `bytes`.
pub(crate) fn body(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

/// `value` as JSON, with `status`.
pub(crate) fn json<T: Serialize>(status: StatusCode, value: &T) -> Response<Body> {
    match serde_json::to_vec(value) {
        Ok(bytes) => with_type(status, "application/json", bytes),
        Err(error) => refused(&ApiError::internal(format!(
            "the answer couldn't be written: {error}"
        ))),
    }
}

/// No body, with `status` (204, 304).
pub(crate) fn empty(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(body(Bytes::new()));
    *response.status_mut() = status;
    response
}

/// `bytes` of `content_type`, with `status`.
pub(crate) fn with_type(
    status: StatusCode,
    content_type: &'static str,
    bytes: Vec<u8>,
) -> Response<Body> {
    typed(status, content_type, body(bytes))
}

/// `body` of `content_type`, with `status`.
fn typed(status: StatusCode, content_type: &'static str, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// A body written as it comes: what's sent on its [`Sender`], until every
/// sender is dropped (then it ends, or breaks off if it was [`Cut`]).
/// Bounded, so a client that reads slowly holds its writer back.
pub(crate) struct Channel {
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
    cut: Arc<AtomicBool>,
    progress: Arc<Progress>,
}

/// How far a channel's async end has got: bumped each time it takes what
/// a blocking thread sent it (or goes), so that thread waits for room or
/// for more only as long as it must, never a sleep's worth (11.7: a 50 ms
/// sleep whenever 16 chunks waited held every stream to some 320 chunks a
/// second). Waits are on a condvar, not tokio's timers: the service's
/// runtime is a current-thread one, whose timers stop with it.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    moves: Mutex<u64>,
    moved: Condvar,
}

impl Progress {
    /// How many moves so far: read before a try, waited past after one
    /// that found no room (or nothing), so a move between the two is never
    /// missed.
    pub(crate) fn seen(&self) -> u64 {
        *self.moves.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One more move: wakes whoever waits.
    pub(crate) fn bump(&self) {
        let mut moves = self.moves.lock().unwrap_or_else(PoisonError::into_inner);
        *moves = moves.wrapping_add(1);
        self.moved.notify_all();
    }

    /// Waits until there has been a move since `seen`, for at most `wait`
    /// (the caller then asks again whether to go on).
    pub(crate) fn wait_past(&self, seen: u64, wait: Duration) {
        let moves = self.moves.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .moved
            .wait_timeout_while(moves, wait, |moves| *moves == seen);
    }
}

/// A [`Progress`] bumped as its owner is dropped: a task's, so its end
/// (whichever way it ends, or the task dropped with the runtime) wakes the
/// thread that waits on it.
pub(crate) struct BumpOnDrop(pub(crate) Arc<Progress>);

impl Drop for BumpOnDrop {
    fn drop(&mut self) {
        self.0.bump();
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // Closed first: the writer woken next sees its client gone at once.
        self.receiver.close();
        self.progress.bump();
    }
}

/// What breaks a [`Channel`] off: once its senders are gone, it ends in an
/// error rather than as a whole stream (a copy that failed half way).
pub(crate) struct Cut(Arc<AtomicBool>);

impl Cut {
    /// Breaks the stream off at its senders' end.
    pub(crate) fn cut(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// [`Channel`]'s sender: closed once its client has gone.
#[derive(Debug, Clone)]
pub(crate) struct Sender {
    inner: tokio::sync::mpsc::Sender<Bytes>,
    progress: Arc<Progress>,
}

impl Sender {
    /// Whether its client has gone (the channel closed).
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Sends `bytes` from async code, waiting for room.
    pub(crate) async fn send(&self, bytes: Bytes) -> Result<(), Bytes> {
        self.inner.send(bytes).await.map_err(|error| error.0)
    }

    /// Sends `bytes` if there's room now.
    pub(crate) fn try_send(
        &self,
        bytes: Bytes,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Bytes>> {
        self.inner.try_send(bytes)
    }

    /// How far its channel has been read: see [`Progress`].
    pub(crate) fn progress(&self) -> &Progress {
        &self.progress
    }
}

/// How many chunks a [`Channel`] holds before its writer waits.
const CHANNEL_CHUNKS: usize = 16;

/// A body to write as it comes, and where to send it.
pub(crate) fn channel() -> (Sender, Channel) {
    let (sender, _, channel) = cuttable();
    (sender, channel)
}

/// [`channel`], and what breaks it off.
pub(crate) fn cuttable() -> (Sender, Cut, Channel) {
    let (inner, receiver) = tokio::sync::mpsc::channel(CHANNEL_CHUNKS);
    let cut = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Progress::default());
    let channel = Channel {
        receiver,
        cut: Arc::clone(&cut),
        progress: Arc::clone(&progress),
    };
    (Sender { inner, progress }, Cut(cut), channel)
}

impl hyper::body::Body for Channel {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
        let polled = self.receiver.poll_recv(context);
        if let std::task::Poll::Ready(Some(_)) = &polled {
            self.progress.bump();
        }
        polled.map(|bytes| match bytes {
            Some(bytes) => Some(Ok(hyper::body::Frame::data(bytes))),
            // Cut: once, an error; its end after.
            None if self.cut.swap(false, Ordering::AcqRel) => {
                Some(Err(std::io::Error::other("the stream was cut short")))
            }
            None => None,
        })
    }
}

/// A sender whose receiver is the test's own (no [`Channel`]: nothing
/// bumps its progress, so a full one waits its bound each time).
#[cfg(test)]
pub(crate) fn bare(capacity: usize) -> (Sender, tokio::sync::mpsc::Receiver<Bytes>) {
    let (inner, receiver) = tokio::sync::mpsc::channel(capacity);
    let progress = Arc::new(Progress::default());
    (Sender { inner, progress }, receiver)
}

/// `body`, written as it comes, of `content_type`, with `status`: its
/// headers sent at once.
pub(crate) fn channelled(
    status: StatusCode,
    content_type: &'static str,
    body: Channel,
) -> Response<Body> {
    typed(status, content_type, body.boxed())
}

/// A refusal: its status, and `{"message": …}`.
pub(crate) fn refused(error: &ApiError) -> Response<Body> {
    #[derive(Serialize)]
    struct Message<'a> {
        message: &'a str,
    }
    let bytes = serde_json::to_vec(&Message {
        message: &error.message,
    })
    .unwrap_or_else(|_| b"{\"message\":\"\"}".to_vec());
    with_type(error.status, "application/json", bytes)
}

/// The headers Docker puts on every response: the API version (what a
/// client negotiates down to), the OS, no experimental features, and the
/// server's name.
pub(crate) fn add_headers(response: &mut Response<Body>, server: &HeaderValue) {
    let headers = response.headers_mut();
    headers.insert("Api-Version", HeaderValue::from_static(API_VERSION));
    headers.insert("Docker-Experimental", HeaderValue::from_static("false"));
    headers.insert("Ostype", HeaderValue::from_static("linux"));
    headers.insert(hyper::header::SERVER, server.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames a channel body gives, to its end: each one's data, or
    /// `None` for an error.
    fn frames(body: Channel) -> Vec<Option<Bytes>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut body = std::pin::pin!(body);
            let mut frames = Vec::new();
            while let Some(frame) = body.as_mut().frame().await {
                frames.push(frame.ok().and_then(|frame| frame.into_data().ok()));
            }
            frames
        })
    }

    /// A channel body ends with its senders; cut, it breaks off with an
    /// error instead (once): a stream cut short is never a whole one.
    #[test]
    fn a_cut_stream_breaks_off() {
        let (sender, cut, body) = cuttable();
        sender.try_send(Bytes::from_static(b"some")).unwrap();
        cut.cut();
        drop(sender);
        assert_eq!(frames(body), [Some(Bytes::from_static(b"some")), None]);
        let (sender, _, body) = cuttable();
        sender.try_send(Bytes::from_static(b"all")).unwrap();
        drop(sender);
        assert_eq!(frames(body), [Some(Bytes::from_static(b"all"))]);
    }
}
