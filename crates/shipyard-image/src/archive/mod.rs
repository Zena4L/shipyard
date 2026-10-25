//! Image archives (step 12.2a): `save` writes images out as Docker 25 does
//! (an OCI image layout with Docker's `manifest.json` beside it, D12.2-2);
//! `load` reads one back, or BuildKit's OCI layout, or Docker's legacy
//! archive. An archive to load is hostile: its members are matched by name
//! to blobs and documents, never written by their names; every blob is
//! checked against its digest; what's held in memory is bounded; nothing of
//! it reaches the store until the whole archive has been read
//! ([`load`]'s staging).

pub mod load;
pub mod save;
pub mod staging;

pub(crate) mod legacy;

use std::cell::Cell;
use std::io::{BufReader, Read};
use std::rc::Rc;

use crate::digest::DigestError;
use crate::manifest::ManifestError;
use crate::store::StoreError;
use crate::tar_reader::TarError;

/// The OCI image layout's version file's contents.
pub(crate) const OCI_LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

/// The annotation containerd (and BuildKit, and Docker 25) names an image
/// by, in full (`docker.io/library/alpine:3.20`).
pub(crate) const CONTAINERD_NAME: &str = "io.containerd.image.name";

/// The OCI annotation for a manifest's name in its layout: usually the tag
/// alone (`3.20`).
pub(crate) const OCI_REF_NAME: &str = "org.opencontainers.image.ref.name";

/// Why an archive couldn't be written or read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ArchiveError {
    /// Reading or writing the stream, or the store's files.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Why it failed.
        source: std::io::Error,
    },
    /// Reading the archive's own stream (a gzip stream that isn't one, the
    /// client's connection lost): the input's fault, never the store's.
    #[error("reading the archive: {0}")]
    Read(#[source] std::io::Error),
    /// The store refused a blob (its bytes aren't its digest, say).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The archive's tar stream is malformed, or past a limit.
    #[error("the archive: {0}")]
    Tar(#[from] TarError),
    /// A manifest, index or config that doesn't parse.
    #[error("the archive's {what}: {source}")]
    Document {
        /// Which one.
        what: String,
        /// Why.
        source: ManifestError,
    },
    /// A digest that isn't one.
    #[error("the archive names a digest that isn't one: {0}")]
    Digest(#[from] DigestError),
    /// Something the archive says that can't be (a document that isn't the
    /// shape it should be, a path that names nothing in it).
    #[error("the archive: {0}")]
    Malformed(String),
    /// The store is missing one of a saved image's own blobs: the store's
    /// fault, never the request's.
    #[error("the image store has no blob {0} (an image's own): remove the image and pull it again")]
    Missing(crate::digest::Digest),
    /// Past one of the archive's bounds.
    #[error("the archive has {0}")]
    TooLarge(String),
    /// The stream ended before the archive's end-of-archive marker: a cut
    /// archive, never taken for a whole one.
    #[error("the archive is cut short (it ends before its end-of-archive marker)")]
    Cut,
    /// A compression Shipyard doesn't read.
    #[error("a {0} archive isn't supported: give a plain tar or a gzip one")]
    Compression(&'static str),
    /// Writing it would leave the store's filesystem with less than its
    /// reserve.
    #[error("loading it would leave {available} bytes free, below the reserve of {reserve}")]
    NoSpace {
        /// What's free.
        available: u64,
        /// What must stay free.
        reserve: u64,
    },
}

/// Whether a stream [`decoded`] gave has run out.
pub(crate) type Ended = Rc<Cell<bool>>;

/// The bytes [`decoded`] looks at: xz's magic is the longest.
const MAGIC: usize = 6;

/// `input` decoded: a gzip stream (one member or more) as its tar, a plain
/// one as it is; bzip2, xz and zstd refused by name. Wrapped so a stream
/// that has run out says so (the flag): what fails then failed for that, a
/// cut archive ([`ArchiveError::Cut`]), never taken for another fault.
///
/// # Errors
///
/// The input unreadable; a compression not taken.
pub(crate) fn decoded<'a>(
    input: impl Read + 'a,
) -> Result<(Box<dyn Read + 'a>, Ended), ArchiveError> {
    // The first bytes, however the stream splits them (a pipe's first
    // write may be short): what they are decides the decoder.
    let mut input = BufReader::new(input);
    let mut head = Vec::with_capacity(MAGIC);
    // (`read_to_end` reads on until it has them all, or the stream ends.)
    input
        .by_ref()
        .take(MAGIC as u64)
        .read_to_end(&mut head)
        .map_err(ArchiveError::Read)?;
    let input = std::io::Cursor::new(head.clone()).chain(input);
    let decoded: Box<dyn Read + 'a> = match head.as_slice() {
        [0x1f, 0x8b, ..] => Box::new(flate2::read::MultiGzDecoder::new(input)),
        [b'B', b'Z', b'h', ..] => return Err(ArchiveError::Compression("bzip2")),
        [0xfd, b'7', b'z', b'X', b'Z', 0, ..] => return Err(ArchiveError::Compression("xz")),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => return Err(ArchiveError::Compression("zstd")),
        _ => Box::new(input),
    };
    let ended = Rc::new(Cell::new(false));
    let seen = EndSeen {
        inner: decoded,
        ended: Rc::clone(&ended),
    };
    Ok((Box::new(seen), ended))
}

/// A reader that notes when its stream has run out (it read nothing, or a
/// decoder found its input ending early).
struct EndSeen<R> {
    inner: R,
    ended: Rc<Cell<bool>>,
}

impl<R: Read> Read for EndSeen<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.inner.read(buf) {
            Ok(0) if !buf.is_empty() => {
                self.ended.set(true);
                Ok(0)
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.ended.set(true);
                Err(error)
            }
            other => other,
        }
    }
}

impl ArchiveError {
    /// An I/O failure, with what was being done.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests;
