//! The API's event stream (`GET /events`): what the app's store follows
//! between full reloads. One connection: when it ends (the service stopped,
//! the VM stopped), the caller reloads everything before following again,
//! never trusting a `since` alone (the journal isn't in time order).
//!
//! Read with hyper, a line at a time, rather than `bollard`'s stream, which
//! ends at the first event it can't decode: here such an event (a newer
//! Shipyard's new kind) is skipped, and the stream goes on.

use http_body_util::BodyExt as _;

use crate::api::{self, Client};
use crate::{ClientError, Result};

/// The longest event line read: a longer one is a broken stream.
const MAX_LINE: usize = 1024 * 1024;

/// An event stream whose headers have come: the API follows the journal
/// for it from then on.
#[derive(Debug)]
pub struct Events {
    answer: api::Answer,
}

/// Subscribes to `client`'s events (those since `since`, seconds since the
/// epoch, first, from the API's journal), returning once the API has
/// answered, within the client's bound: an event after this returns is
/// never missed.
///
/// # Errors
///
/// The API's refusal, or past the bound.
pub async fn subscribe(client: &Client, since: Option<i64>) -> Result<Events> {
    let path = since.map_or_else(
        || "/events".to_owned(),
        |since| format!("/events?since={since}"),
    );
    let answer = tokio::time::timeout(api::REQUEST_WAIT, api::request(client.socket(), &path))
        .await
        .map_err(|_| ClientError::Timeout {
            what: "the event stream".to_owned(),
            seconds: api::REQUEST_WAIT.as_secs(),
        })??;
    if answer.status != 200 {
        return Err(ClientError::Api {
            path,
            status: answer.status,
            message: String::new(),
        });
    }
    Ok(Events { answer })
}

impl Events {
    /// Hands each event to `on_event` until the stream ends: its end's
    /// reason. A line that isn't an event this reads is skipped.
    ///
    /// # Errors
    ///
    /// [`ClientError::StreamEnded`] at its end; the connection's failure.
    pub async fn run(
        mut self,
        mut on_event: impl FnMut(bollard::models::EventMessage),
    ) -> Result<()> {
        let mut pending: Vec<u8> = Vec::new();
        while let Some(frame) = self.answer.body.frame().await {
            let Ok(data) = frame.map_err(api::http)?.into_data() else {
                continue;
            };
            pending.extend_from_slice(&data);
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                if let Ok(event) = serde_json::from_slice(&line) {
                    on_event(event);
                }
            }
            if pending.len() > MAX_LINE {
                return Err(ClientError::Http(format!(
                    "an event longer than {MAX_LINE} bytes"
                )));
            }
        }
        Err(ClientError::StreamEnded)
    }
}

/// Follows `client`'s events: [`subscribe`], then [`Events::run`].
///
/// # Errors
///
/// As theirs.
pub async fn follow(
    client: &Client,
    since: Option<i64>,
    on_event: impl FnMut(bollard::models::EventMessage),
) -> Result<()> {
    subscribe(client, since).await?.run(on_event).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::place::Endpoint;

    /// A fake API at `socket`: its version for the negotiation, and for
    /// `/events` the lines of `events`, then the stream's end.
    fn serve(socket: &std::path::Path, events: &'static [&'static str]) {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut head = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let body = if head.contains("/events") {
                    events.join("\n") + "\n"
                } else {
                    r#"{"ApiVersion":"1.43","Version":"0.5.0","MinAPIVersion":"1.24"}"#.to_owned()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
    }

    /// An event this can't read is skipped, the ones around it handed on,
    /// and the stream's end said as its end.
    #[test]
    fn an_unreadable_event_never_ends_the_stream() {
        let dir =
            std::env::temp_dir().join(format!("shipyard-client-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("api.sock");
        serve(
            &socket,
            &[
                r#"{"Type":"container","Action":"start","Actor":{"ID":"a"}}"#,
                r#"{"Type":"container","Action":"start","Actor":"not an actor"}"#,
                r#"{"Type":"container","Action":"die","Actor":{"ID":"a"}}"#,
            ],
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut seen = Vec::new();
        let ended = runtime.block_on(async {
            let client = Client::connect(&Endpoint::Unix(socket.clone()))
                .await
                .unwrap();
            follow(&client, None, |event| {
                seen.push(event.action.unwrap_or_default());
            })
            .await
        });
        assert_eq!(seen, ["start", "die"]);
        assert!(matches!(ended, Err(ClientError::StreamEnded)), "{ended:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
