//! A push's requests (step 12.4): is a blob there (`HEAD`), a blob's upload
//! (`POST` to start it, `PATCH` its bytes, `PUT ?digest=` to commit it, as
//! Docker's CLI sends it), a manifest's `PUT`, and what a tag names now.
//!
//! **What an upload doesn't trust.** It follows no redirect (a `POST`
//! turned into a `GET` would pass for a start; a body sent on is the
//! registry's to send nowhere else): any 3xx is an error. Each step goes
//! to the `Location` the one before answered with (distribution and Docker
//! Hub put the upload's state there, a new one each step), resolved
//! against the registry, and only if it's on the registry itself (the
//! default port written or not): an upload led to another host is refused,
//! never sent there with or without credentials. A 401 mid-upload signs in
//! again and starts the upload over from its `POST`, the blob read again
//! from its first byte (a body half read is never sent twice). Before a
//! retry, a `HEAD` asks whether the registry has the blob already (a commit
//! whose answer was lost isn't sent again). Sending a body has a time
//! limit sized to it; the commit's answer, one that grows with the blob
//! (the registry may move it to its storage first).

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt as _;
use std::time::Duration;

use super::{
    Attempt, RESPONSE_TIMEOUT, Registry, RegistryError, body_deadline, challenges, encode,
    same_authority, successful, transport_failure,
};
use crate::digest::Digest;

/// The longest `Location` an upload step's answer is taken with.
const MAX_LOCATION: usize = 8 * 1024;

/// The slowest a registry is taken to commit a blob it has (its hash
/// checked, moved to its storage): the final `PUT`'s answer may take the
/// blob's size at this rate, beyond the usual wait for an answer.
const COMMIT_RATE: u64 = 4 * 1024 * 1024;

impl Registry {
    /// Whether the registry has the blob `digest` in `repository` (`HEAD`).
    ///
    /// # Errors
    ///
    /// A transport failure, a refused or denied sign-in, or a status that
    /// isn't 2xx or 404, after the retries.
    pub fn blob_exists(&self, repository: &str, digest: &Digest) -> Result<bool, RegistryError> {
        let context = format!("looking for the blob {digest}");
        Self::retrying(|| self.exists(repository, digest, &context))
    }

    /// Uploads the blob `digest` of `size` bytes from `file` (read by
    /// offset, from its first byte, at each attempt: the descriptor is
    /// shared with nothing that moves it) to `repository`.
    ///
    /// # Errors
    ///
    /// A transport failure or a status, after the retries; a redirect or an
    /// upload led to another host; the registry storing it under another
    /// digest.
    pub fn upload_blob(
        &self,
        repository: &str,
        digest: &Digest,
        size: u64,
        file: &File,
    ) -> Result<(), RegistryError> {
        let context = format!("pushing the blob {digest}");
        let mut tried = false;
        Self::retrying(|| {
            // One whose commit went through, its answer lost, isn't sent
            // again.
            if std::mem::replace(&mut tried, true) && self.exists(repository, digest, &context)? {
                return Ok(());
            }
            self.upload_once(repository, digest, size, file, &context)
        })
    }

    /// Puts the manifest `bytes` (of `media_type`, its digest `digest`) in
    /// `repository` as `reference` (a tag).
    ///
    /// # Errors
    ///
    /// As [`upload_blob`](Self::upload_blob)'s; the registry naming another
    /// digest for it.
    pub fn put_manifest(
        &self,
        repository: &str,
        reference: &str,
        media_type: &str,
        bytes: &[u8],
        digest: &Digest,
    ) -> Result<(), RegistryError> {
        let url = format!("{}/v2/{repository}/manifests/{reference}", self.base);
        let context = format!("pushing the manifest {repository}:{reference}");
        Self::retrying(|| {
            let response = self.with_auth(repository, |authorization| {
                let request = self
                    .agent
                    .put(&url)
                    .config()
                    .max_redirects(0)
                    .build()
                    .header("Content-Type", media_type);
                signed(request, authorization)
                    .send(bytes)
                    .map_err(|source| transport_failure(&context, source))
            })?;
            let response = self.step(response, repository, &[200, 201], false, &context)?;
            stored_as(&response, digest, &context)
        })
    }

    /// The digest of what `reference` names in `repository` now, if it
    /// names anything (a manifest or an index).
    ///
    /// # Errors
    ///
    /// As [`get_manifest`](Self::get_manifest)'s, but a 404.
    pub fn manifest_digest(
        &self,
        repository: &str,
        reference: &str,
    ) -> Result<Option<Digest>, RegistryError> {
        match self.get_manifest(repository, reference, crate::MANIFEST_SIZE_LIMIT) {
            Ok(fetched) => Ok(Some(Digest::sha256(&fetched.bytes))),
            Err(RegistryError::Status { status: 404, .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// One `HEAD` of a blob: whether it's there.
    fn exists(&self, repository: &str, digest: &Digest, context: &str) -> Result<bool, Attempt> {
        let url = format!("{}/v2/{repository}/blobs/{digest}", self.base);
        let response = self.with_auth(repository, |authorization| {
            signed(self.agent.head(&url), authorization)
                .call()
                .map_err(|source| transport_failure(context, source))
        })?;
        match response.status().as_u16() {
            404 => Ok(false),
            _ => successful(response, context).map(|_| true),
        }
    }

    /// One attempt at an upload: started, its bytes sent, committed.
    fn upload_once(
        &self,
        repository: &str,
        digest: &Digest,
        size: u64,
        file: &File,
        context: &str,
    ) -> Result<(), Attempt> {
        let start = format!("{}/v2/{repository}/blobs/uploads/", self.base);
        let started = self.with_auth(repository, |authorization| {
            let request = self.agent.post(&start).config().max_redirects(0).build();
            signed(request, authorization)
                .send_empty()
                .map_err(|source| transport_failure(context, source))
        })?;
        let started = self.step(started, repository, &[202], false, context)?;
        let location = self.location(&started, context)?;
        let request = self
            .agent
            .patch(&location)
            .config()
            .max_redirects(0)
            .timeout_send_body(Some(body_deadline(size, false)))
            .build()
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", size.to_string());
        let request = signed(request, self.current_authorization()?);
        let mut body = ReadAt {
            file,
            at: 0,
            end: size,
        };
        let patched = request
            .send(ureq::SendBody::from_reader(&mut body))
            .map_err(|source| transport_failure(context, source))?;
        let patched = self.step(patched, repository, &[202], true, context)?;
        let location = self.location(&patched, context)?;
        let commit = with_query(&location, "digest", &digest.to_string());
        let send_commit = |authorization: Option<String>| {
            let request = self
                .agent
                .put(&commit)
                .config()
                .max_redirects(0)
                .timeout_recv_response(Some(
                    RESPONSE_TIMEOUT + Duration::from_secs(size / COMMIT_RATE),
                ))
                .build();
            signed(request, authorization)
                .send_empty()
                .map_err(|source| transport_failure(context, source))
        };
        // The commit carries no body: a token that expired while the bytes
        // went (Docker Hub's last 300 s; a large layer takes longer) is
        // renewed and the commit alone sent again, never the whole blob.
        let committed = self.with_auth(repository, send_commit)?;
        let committed = self.step(committed, repository, &[201], true, context)?;
        stored_as(&committed, digest, context)
    }

    /// An upload step's answer if its status is one of `wanted`; else why
    /// not: a redirect, refused; a 401 from the registry (a token that
    /// expired) signs in again and retries the upload from its start, as a
    /// 500 does, and a 404 `mid_upload` (past its `POST`: an upload the
    /// registry lost); another status as [`successful`] says.
    fn step(
        &self,
        response: ureq::http::Response<ureq::Body>,
        repository: &str,
        wanted: &[u16],
        mid_upload: bool,
        context: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, Attempt> {
        use ureq::ResponseExt as _;
        let status = response.status().as_u16();
        if wanted.contains(&status) {
            return Ok(response);
        }
        if (300..400).contains(&status) {
            return Err(Attempt::Fatal(RegistryError::Body {
                context: context.to_owned(),
                reason: format!(
                    "the registry redirected it (HTTP {status}): an upload isn't sent on"
                ),
            }));
        }
        if status == 401 && same_authority(response.get_uri(), &self.base) {
            self.authenticate(&challenges(&response), repository)
                .map_err(Attempt::Fatal)?;
            return Err(Attempt::Retry(
                RegistryError::Status {
                    context: context.to_owned(),
                    status,
                    detail: None,
                },
                Some(Duration::ZERO),
            ));
        }
        // A step that failed on the registry's side (500), or an upload it
        // lost (404 past the start: `BLOB_UPLOAD_UNKNOWN`, an upload it
        // expired), loses only that upload: started over, as 429 and
        // 502-504 are by `successful`. A 404 at the start is the
        // repository's: said.
        let lost = status == 404 && mid_upload;
        match successful(response, context) {
            Err(Attempt::Fatal(err)) if status == 500 || lost => {
                return Err(Attempt::Retry(err, None));
            }
            Err(failed) => return Err(failed),
            Ok(_) => {}
        }
        Err(Attempt::Fatal(RegistryError::Body {
            context: context.to_owned(),
            reason: format!("the registry answered HTTP {status}, not {}", wanted[0]),
        }))
    }

    /// Where an upload's next step goes: the answer's `Location`, resolved
    /// against the registry, refused unless it's on the registry itself.
    fn location(
        &self,
        response: &ureq::http::Response<ureq::Body>,
        context: &str,
    ) -> Result<String, Attempt> {
        let refused = |reason: String| {
            Attempt::Fatal(RegistryError::Body {
                context: context.to_owned(),
                reason,
            })
        };
        let value = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| refused("the registry gave no upload Location".to_owned()))?;
        if value.len() > MAX_LOCATION {
            return Err(refused(format!(
                "the registry's upload Location is over {MAX_LOCATION} bytes"
            )));
        }
        if value.starts_with('/') && !value.starts_with("//") {
            return Ok(format!("{}{value}", self.base));
        }
        let uri: ureq::http::Uri = value
            .parse()
            .map_err(|_| refused("the registry's upload Location isn't a URL".to_owned()))?;
        if uri.authority().is_none() {
            return Err(refused(
                "the registry's upload Location is neither a path from its root nor a URL"
                    .to_owned(),
            ));
        }
        if !same_authority(&uri, &self.base) {
            return Err(refused(format!(
                "the registry sent the upload to another host ({}): it isn't sent there",
                uri.authority().map_or("?", |authority| authority.as_str())
            )));
        }
        Ok(value.to_owned())
    }
}

/// `request` carrying `authorization`, if there's one.
fn signed<B>(
    request: ureq::RequestBuilder<B>,
    authorization: Option<String>,
) -> ureq::RequestBuilder<B> {
    match authorization {
        Some(authorization) => request.header("Authorization", authorization),
        None => request,
    }
}

/// `Ok` unless the answer names a digest that isn't `digest`.
fn stored_as(
    response: &ureq::http::Response<ureq::Body>,
    digest: &Digest,
    context: &str,
) -> Result<(), Attempt> {
    let given = response
        .headers()
        .get("docker-content-digest")
        .and_then(|value| value.to_str().ok());
    match given {
        Some(given) if given != digest.to_string() => Err(Attempt::Fatal(RegistryError::Body {
            context: context.to_owned(),
            reason: format!("the registry stored it as {given}, not {digest}"),
        })),
        _ => Ok(()),
    }
}

/// `url` with `key=value` added to its query (`&` if it has one already,
/// as an upload's `Location` does).
fn with_query(url: &str, key: &str, value: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{key}={}", encode(value))
}

/// The bytes `[at, end)` of a file, read by offset (`pread`): the file's
/// own position is never moved, so readers of one descriptor never mix.
struct ReadAt<'a> {
    file: &'a File,
    at: u64,
    end: u64,
}

impl Read for ReadAt<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.end.saturating_sub(self.at);
        if left == 0 {
            return Ok(0);
        }
        let most = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let read = self.file.read_at(&mut buf[..most], self.at)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the stored blob ended before its size",
            ));
        }
        self.at += read as u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A commit's digest goes after the upload's own state, never in
    /// place of it.
    #[test]
    fn a_digest_joins_the_location_s_query() {
        assert_eq!(
            with_query(
                "http://r/v2/a/blobs/uploads/u1?_state=s1",
                "digest",
                "sha256:ab"
            ),
            "http://r/v2/a/blobs/uploads/u1?_state=s1&digest=sha256:ab"
        );
        assert_eq!(
            with_query("http://r/v2/a/blobs/uploads/u1", "digest", "sha256:ab"),
            "http://r/v2/a/blobs/uploads/u1?digest=sha256:ab"
        );
    }

    /// A blob read by offset, whole and from its start again, whatever
    /// another reader of the descriptor did.
    #[test]
    fn a_blob_is_read_by_offset() {
        let path = std::env::temp_dir().join(format!("shipyard-readat-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let file = File::open(&path).unwrap();
        let mut moved = &file;
        let mut skip = [0; 3];
        moved.read_exact(&mut skip).unwrap();
        for _ in 0..2 {
            let mut read = Vec::new();
            ReadAt {
                file: &file,
                at: 0,
                end: 10,
            }
            .read_to_end(&mut read)
            .unwrap();
            assert_eq!(read, b"0123456789");
        }
        let short = ReadAt {
            file: &file,
            at: 0,
            end: 11,
        }
        .read_to_end(&mut Vec::new())
        .unwrap_err();
        assert_eq!(short.kind(), io::ErrorKind::UnexpectedEof);
        std::fs::remove_file(&path).unwrap();
    }
}
