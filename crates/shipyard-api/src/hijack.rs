//! The connection taken over (step 11.2b2a): HTTP's upgrade, as Docker's
//! clients ask for it to attach (`Connection: Upgrade`, `Upgrade: tcp`). The answer is 101 Switching Protocols with Docker's content type;
//! after it, the connection is a raw stream both ways, no longer HTTP.

use hyper::header::{CONNECTION, CONTENT_TYPE, HeaderMap, HeaderValue, UPGRADE};
use std::os::fd::BorrowedFd;

use bytes::Bytes;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use crate::reply::{self, Body};

/// Whether `headers` ask for the connection to be taken over.
pub(crate) fn asked(headers: &HeaderMap) -> bool {
    let has = |name, word: &str| {
        headers.get_all(name).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case(word))
            })
        })
    };
    has(CONNECTION, "upgrade") && has(UPGRADE, "tcp")
}

/// The 101 that takes the connection over, its stream of `content_type`.
pub(crate) fn switching(content_type: &'static str) -> Response<Body> {
    let mut response = reply::empty(StatusCode::SWITCHING_PROTOCOLS);
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("tcp"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// The connection taken over: the UNIX stream it is, and what hyper had
/// read past the request (the client's first input). `None` if the upgrade
/// failed.
pub(crate) async fn take_over(upgrade: OnUpgrade) -> Option<(UnixStream, Bytes)> {
    let upgraded: Upgraded = upgrade.await.ok()?;
    let parts = upgraded.downcast::<TokioIo<UnixStream>>().ok()?;
    Some((parts.io.into_inner(), parts.read_buf))
}

/// Whether the peer on `socket` has hung up: closed both ways (`POLLHUP`),
/// not only its writing (Docker's CLI half-closes at once when it sends no
/// input, and still reads).
pub(crate) fn hung_up(socket: BorrowedFd<'_>) -> bool {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut fds = [PollFd::new(socket, PollFlags::POLLIN)];
    poll(&mut fds, PollTimeout::ZERO).is_ok()
        && fds[0]
            .revents()
            .is_some_and(|events| events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Docker's clients' headers ask; one without either doesn't.
    #[test]
    fn an_upgrade_is_asked_as_docker_asks_it() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.append(*name, HeaderValue::from_static(value));
            }
            map
        };
        assert!(asked(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "tcp")
        ])));
        assert!(asked(&headers(&[
            ("connection", "keep-alive, upgrade"),
            ("upgrade", "TCP")
        ])));
        assert!(!asked(&headers(&[("upgrade", "tcp")])));
        assert!(!asked(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "h2c")
        ])));
        assert!(!asked(&HeaderMap::new()));
        let answer = switching("application/vnd.docker.raw-stream");
        assert_eq!(answer.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(answer.headers()[UPGRADE], "tcp");
    }
}
