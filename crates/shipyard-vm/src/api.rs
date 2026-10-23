//! Docker's API on the Mac (step 11.5b1, D11.5b-1): the host serves
//! `docker.sock` in the VM's directory and joins each connection to the
//! guest's own API (`forward::API_REQUEST`: its service started there if
//! none answers), the bytes both ways, raw. Docker's clients (`docker`,
//! `docker compose`, Testcontainers) reach Shipyard so, as they would a
//! Docker daemon; an HTTP upgrade (attach, `exec`) is more bytes. Only the
//! host's own refusals are HTTP of its making ([`refusal`]).

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use shipyard_runtime::attach::forward::{API_LINGERS, API_REQUEST, Refusal};
use shipyard_sys::relay;

use crate::forwarding::Unanswered;
use crate::host::log;
use crate::machine::Machine;

/// Docker's clients' connections (`docker.sock`), for the host's one
/// accept loop (`serving`).
pub(crate) const SERVED: crate::serving::Served = crate::serving::Served {
    what: "an API connection",
    refuse: |client, refusal| refuse(client, &refusal.to_string()),
    serve,
};

/// One connection joined to the guest's API (the agent's welcome waited
/// for), then the bytes both ways until the service ends the connection
/// ([`API_LINGERS`]). A refusal (the guest not ready, its service not
/// starting, an older guest) is said to the client as Docker's error.
fn serve(machine: &Machine, client: UnixStream) {
    let (agent, unread, port) = match joined_once_ready(machine) {
        Ok(joined) => joined,
        Err(refusal) => return refuse(client, &refusal.to_string()),
    };
    let relayed = (&client)
        .write_all(&unread)
        .and_then(|()| relay::both_ways_lingering(client.into(), agent.into(), API_LINGERS));
    match relayed {
        // Its client gone (killed in a quiet `exec`, say): the guest can't
        // see it from the vsock, so it's told, and its service hangs the
        // session up as Linux's does (11.7).
        Ok(ended) if ended.one_gone => crate::forwarding::hang_up(machine, port),
        Ok(_) => {}
        Err(error) => log(&format!("an API connection: {error}")),
    }
}

/// How long a connection waits for a guest still getting ready (its data
/// disk checked, or repaired, at boot: D10.4-4), as the CLI waits for it.
const READY_WAIT: Duration = Duration::from_secs(150);

/// [`crate::forwarding::joined_from`] to the guest's API, asked again while the
/// guest says it isn't ready yet (a `docker` that started the VM: Docker's
/// clients don't try again), for at most [`READY_WAIT`].
fn joined_once_ready(machine: &Machine) -> Result<(UnixStream, Vec<u8>, u32), Unanswered> {
    let began = std::time::Instant::now();
    loop {
        match crate::forwarding::joined_from(machine, &[API_REQUEST]) {
            Err(Unanswered::Refused(Refusal::NotReady)) if began.elapsed() < READY_WAIT => {
                std::thread::sleep(Duration::from_millis(250));
            }
            joined => return joined,
        }
    }
}

/// Says `why` to `client` as Docker's daemon says an error (an HTTP 503,
/// `{"message": …}`, which Docker's CLI prints), then lets it go. What it
/// sent needn't be read first: on macOS a UNIX socket closed with bytes
/// unread doesn't reset the client's read of the answer (the test below).
/// Run on the connection's own thread, never the accept loop's.
pub(crate) fn refuse(mut client: UnixStream, why: &str) {
    log(&format!("an API connection refused: {why}"));
    let _ = client.write_all(&refusal(why));
    let _ = client.shutdown(std::net::Shutdown::Write);
}

/// An HTTP/1.1 503 whose body is Docker's error, `{"message": WHY}`.
pub(crate) fn refusal(why: &str) -> Vec<u8> {
    let body = serde_json::json!({ "message": why }).to_string();
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::io::Read as _;

    use super::*;

    /// A refusal is a whole HTTP answer: Docker's 503, its JSON body (the
    /// reason escaped) of the length it says.
    #[test]
    fn a_refusal_is_dockers_error() {
        let answer = String::from_utf8(refusal("the VM is \"stopping\"\n")).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 503 "), "{head}");
        assert!(
            head.contains(&format!("Content-Length: {}", body.len())),
            "{head}"
        );
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["message"], "the VM is \"stopping\"\n");
    }

    /// A refused client reads the whole answer, though it had sent a
    /// request the host never read.
    #[test]
    fn a_refused_client_reads_the_answer() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\n\r\n")
            .unwrap();
        refuse(server, "no");
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, refusal("no"));
    }
}
