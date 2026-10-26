//! `import` (step 12.2b, D12.2-4, D12.2-6): a root filesystem's tarball
//! made a one-layer image. The tarball is anyone's: it's read in one pass
//! that hashes it as a tar (the layer's `diff_id`), gzips it into the store's
//! staging (D12.2-6: the blob's digest is the gzip's) and reads it as a tar
//! all the while (the tar reader's limits; its end-of-archive marker
//! required: a cut or empty input is never an image); nothing reaches the
//! store until it's whole ([`Imported::promote`]). Its files are checked
//! again as they're unpacked, as any layer's are (no `..`, no devices).

use std::io::{Read, Write};

use serde_json::{Map, Value, json};

use crate::archive::ArchiveError;
use crate::archive::staging::Staging;
use crate::changes::Changes;
use crate::digest::{Digest, Verifier};
use crate::manifest::Platform;
use crate::store::Store;
use crate::tar_reader::{TarLimits, TarReader};

/// The most bytes an import's tarball may decode to (16 GiB: a gzip bomb's
/// bound, and the work any client of the API's socket can ask for).
pub const MAX_IMPORT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// What an image made by `import` says of itself, beside its layer.
#[derive(Debug, Clone)]
pub struct About {
    /// Its config's changes (`--change`).
    pub changes: Changes,
    /// Its history's comment (`--message`), else Docker's "Imported from -".
    pub message: Option<String>,
    /// When it was made (RFC 3339), its config's `created` and its history's.
    pub created: String,
    /// Its platform: this host's.
    pub platform: Platform,
}

/// An import read whole into the store's staging: its manifest's digest
/// (its ID once promoted).
#[derive(Debug)]
pub struct Imported {
    staging: Staging,
    /// The image's manifest's digest.
    pub manifest: Digest,
    /// Its blobs: the layer, the config, the manifest.
    blobs: Vec<Digest>,
    /// Whether the tarball's first members are an image archive's
    /// (`oci-layout`, `index.json`, `manifest.json`): it was probably meant
    /// for `load`.
    pub looks_like_an_archive: bool,
}

impl Imported {
    /// Moves its blobs into `store`, under the store's shared lock (the
    /// caller's).
    ///
    /// # Errors
    ///
    /// A blob that can't be moved, or the store's directory synced.
    pub fn promote(&self, store: &Store) -> Result<(), ArchiveError> {
        self.staging.promote(store, &self.blobs)
    }
}

/// Reads the tarball `input` (plain or gzipped) into `store`'s staging,
/// with `about`'s config: the image, staged.
///
/// # Errors
///
/// A compression other than gzip ([`ArchiveError::Compression`]); input
/// that isn't a tar, or past the tar reader's limits ([`ArchiveError::Tar`]);
/// one that ends before its end-of-archive marker, empty included
/// ([`ArchiveError::Cut`]); past [`MAX_IMPORT_BYTES`]
/// ([`ArchiveError::TooLarge`]); the input unreadable
/// ([`ArchiveError::Read`]); the store's reserve
/// ([`ArchiveError::NoSpace`]).
pub fn read(input: impl Read, store: &Store, about: &About) -> Result<Imported, ArchiveError> {
    let mut staging = Staging::new(store)?;
    let (decoded, ended) = crate::archive::decoded(input)?;
    let (layer, diff_id, looks_like_an_archive) = {
        let writer = staging.writer()?;
        let gzip = flate2::write::GzEncoder::new(writer, flate2::Compression::default());
        let mut tee = Tee {
            inner: decoded.take(MAX_IMPORT_BYTES + 1),
            diff_id: Verifier::new(),
            gzip,
            read: 0,
        };
        // What failed, as what it failed for: the store full (its writer
        // says), past the bound, the stream run out; else as it is.
        let read = read_whole_tar(&mut tee).and_then(|looks| {
            tee.gzip
                .try_finish()
                .map(|()| looks)
                .map_err(|source| ArchiveError::io("staging the layer", source))
        });
        let looks = read.map_err(|error| {
            if let Some((available, reserve)) = tee.gzip.get_ref().short() {
                ArchiveError::NoSpace { available, reserve }
            } else if tee.read > MAX_IMPORT_BYTES {
                ArchiveError::TooLarge(format!("more than {MAX_IMPORT_BYTES} bytes once decoded"))
            } else if ended.get() && matches!(error, ArchiveError::Tar(_) | ArchiveError::Read(_)) {
                ArchiveError::Cut
            } else {
                error
            }
        })?;
        if tee.read > MAX_IMPORT_BYTES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_IMPORT_BYTES} bytes once decoded"
            )));
        }
        let Tee { diff_id, gzip, .. } = tee;
        let writer = gzip
            .finish()
            .map_err(|source| ArchiveError::io("staging the layer", source))?;
        (writer.commit()?, diff_id.finish(), looks)
    };
    let config = config_json(about, &diff_id);
    let config = staging.stage_bytes(&config)?;
    let manifest =
        crate::archive::legacy::make_manifest(&mut staging, &config, std::slice::from_ref(&layer))?;
    Ok(Imported {
        staging,
        blobs: vec![layer, config, manifest.clone()],
        manifest,
        looks_like_an_archive,
    })
}

/// Reads `tee` as a tar to its end-of-archive marker, then to the end of
/// the stream (the padding after it is the layer's too: its `diff_id` is
/// the whole stream's, as the unpacker checks it): whether its first
/// members look like an image archive's.
fn read_whole_tar(tee: &mut Tee<impl Read, impl Write>) -> Result<bool, ArchiveError> {
    let mut first = Vec::new();
    {
        let mut tar = TarReader::new(&mut *tee, TarLimits::default());
        while let Some(entry) = tar.next_entry()? {
            if first.len() < 3 {
                first.push(entry.path.to_string_lossy().into_owned());
            }
            std::io::copy(&mut (&mut tar).take(entry.size), &mut std::io::sink())
                .map_err(ArchiveError::Read)?;
        }
        if !tar.marked_end() {
            return Err(ArchiveError::Cut);
        }
    }
    std::io::copy(tee, &mut std::io::sink()).map_err(ArchiveError::Read)?;
    Ok(first
        .iter()
        .any(|name| matches!(name.as_str(), "oci-layout" | "index.json" | "manifest.json")))
}

/// The decoded tarball as it's read: hashed (its `diff_id`), and gzipped
/// into the staged layer.
struct Tee<R, W: Write> {
    inner: R,
    diff_id: Verifier,
    gzip: flate2::write::GzEncoder<W>,
    read: u64,
}

impl<R: Read, W: Write> Read for Tee<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.diff_id.update(&buf[..read]);
        self.gzip.write_all(&buf[..read])?;
        self.read += read as u64;
        Ok(read)
    }
}

/// The image's config, as Docker's `import` writes one: this host's
/// platform, `about`'s changes in `config`, its one layer's `diff_id`, a
/// history of one step.
fn config_json(about: &About, diff_id: &Digest) -> Vec<u8> {
    let changes = &about.changes;
    let mut config = Map::new();
    let mut set = |key: &str, value: Value| {
        config.insert(key.to_owned(), value);
    };
    if let Some(cmd) = &changes.cmd {
        set("Cmd", json!(cmd));
    }
    if let Some(entrypoint) = &changes.entrypoint {
        set("Entrypoint", json!(entrypoint));
    }
    if !changes.env.is_empty() {
        set("Env", json!(changes.env));
    }
    let keys = |keys: &std::collections::BTreeSet<String>| {
        Value::Object(keys.iter().map(|key| (key.clone(), json!({}))).collect())
    };
    if !changes.exposed.is_empty() {
        set("ExposedPorts", keys(&changes.exposed));
    }
    if !changes.labels.is_empty() {
        set("Labels", json!(changes.labels));
    }
    if let Some(user) = &changes.user {
        set("User", json!(user));
    }
    if !changes.volumes.is_empty() {
        set("Volumes", keys(&changes.volumes));
    }
    if let Some(workdir) = &changes.workdir {
        set("WorkingDir", json!(workdir));
    }
    if let Some(signal) = &changes.stop_signal {
        set("StopSignal", json!(signal));
    }
    let mut document = json!({
        "architecture": about.platform.architecture,
        "os": about.platform.os,
        "created": about.created,
        "config": config,
        "rootfs": {"type": "layers", "diff_ids": [diff_id.to_string()]},
        "history": [{
            "created": about.created,
            "comment": about.message.as_deref().unwrap_or("Imported from -"),
        }],
    });
    if let Some(variant) = &about.platform.variant {
        document["variant"] = json!(variant);
    }
    // A `Value` always serializes.
    serde_json::to_vec(&document).unwrap_or_default()
}

#[cfg(test)]
mod tests;
