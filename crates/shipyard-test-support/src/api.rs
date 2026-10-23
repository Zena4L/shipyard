//! A raw HTTP/1.1 client over a UNIX socket, for the Docker API's tests
//! (step 11.2a): a request written by hand, the answer read whole (the
//! connection closed after it), so a test sees exactly what a client would.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// An answer: its status, headers and body.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The status code.
    pub status: u16,
    /// The headers, names lowercased, in order.
    pub headers: Vec<(String, String)>,
    /// The body, de-chunked.
    pub body: Vec<u8>,
}

impl Answer {
    /// The first header `name` (any case).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON.
    ///
    /// # Panics
    ///
    /// It isn't JSON.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "{error}: {:?} (status {})",
                String::from_utf8_lossy(&self.body),
                self.status
            )
        })
    }

    /// The body's text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `METHOD PATH` on `socket`, with `body` (JSON) if there's one.
///
/// # Errors
///
/// The socket can't be reached, or the answer can't be read.
pub fn request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> std::io::Result<Answer> {
    send_request(socket, method, path, (&[], body))
}

/// `METHOD PATH` on `socket`, with these `headers` and no body.
///
/// # Errors
///
/// As [`request`].
pub fn request_with(
    socket: &Path,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> std::io::Result<Answer> {
    send_request(socket, method, path, (headers, None))
}

/// [`request`] and [`request_with`]'s request: its headers, then its body.
fn send_request(
    socket: &Path,
    method: &str,
    path: &str,
    (headers, body): (&[(&str, &str)], Option<&[u8]>),
) -> std::io::Result<Answer> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n");
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if let Some(body) = body {
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse(&raw).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("not an HTTP answer: {:?}", String::from_utf8_lossy(&raw)),
        )
    })
}

/// `GET PATH`.
///
/// # Errors
///
/// As [`request`].
pub fn get(socket: &Path, path: &str) -> std::io::Result<Answer> {
    request(socket, "GET", path, None)
}

/// `POST PATH` with `body` as JSON, if there's one.
///
/// # Errors
///
/// As [`request`].
pub fn post(
    socket: &Path,
    path: &str,
    body: Option<&serde_json::Value>,
) -> std::io::Result<Answer> {
    let body = body.map(|body| serde_json::to_vec(body).unwrap_or_default());
    request(socket, "POST", path, body.as_deref())
}

/// `POST PATH` with `body` as JSON, asking to take the connection over
/// (`Upgrade: tcp`, as Docker's clients attach): the answer's status, and
/// the stream after its head, the session's.
///
/// # Errors
///
/// As [`request`].
pub fn upgrade(
    socket: &Path,
    path: &str,
    body: &serde_json::Value,
) -> std::io::Result<(u16, UnixStream)> {
    let body = serde_json::to_vec(body).unwrap_or_default();
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: docker\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    // The head, a byte at a time: what follows it is the session's.
    let mut raw = Vec::new();
    let mut byte = [0_u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        raw.extend_from_slice(&byte);
    }
    let status = String::from_utf8_lossy(&raw)
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("not an HTTP answer: {:?}", String::from_utf8_lossy(&raw)),
            )
        })?;
    Ok((status, stream))
}

/// Waits (up to `bound`) until something answers on `socket`.
///
/// # Panics
///
/// Nothing did.
pub fn wait_for(socket: &Path, bound: Duration) {
    let deadline = Instant::now() + bound;
    while UnixStream::connect(socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "nothing answered on {} in {bound:?}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An answer's bytes: status line, headers, then the body (a chunked one
/// put back together).
fn parse(raw: &[u8]) -> Option<Answer> {
    let end = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let rest = &raw[end + 4..];
    let chunked = headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(rest)?
    } else {
        rest.to_vec()
    };
    Some(Answer {
        status,
        headers,
        body,
    })
}

/// A chunked body's data.
fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_whole_or_chunked() {
        let plain = parse(b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nApi-Version: 1.43\r\n\r\n{\"message\":\"x\"}").unwrap();
        assert_eq!(plain.status, 404);
        assert_eq!(plain.header("api-version"), Some("1.43"));
        assert_eq!(plain.json()["message"], "x");
        let chunked =
            parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n")
                .unwrap();
        assert_eq!(chunked.text(), "abcde");
        assert!(parse(b"garbage").is_none());
    }
}
