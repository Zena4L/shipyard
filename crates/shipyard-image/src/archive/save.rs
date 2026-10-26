//! `save`: images written out as an archive, as Docker 25 writes one
//! (D12.2-2): an OCI image layout (`oci-layout`, `index.json`, each blob at
//! `blobs/sha256/<hex>`) and Docker's `manifest.json` beside it, pointing
//! at the same blobs. Each image's own stored blobs go out as they are (its
//! manifest, config and compressed layers), so it keeps its ID and digests;
//! a blob two images share goes once. Headers are the same every time
//! (owner 0, mode 0444, time 0): the same images make the same archive.
//!
//! The blobs are opened first ([`open`], under the store's shared lock, the
//! caller's), then written from those descriptors ([`Opened::write`]) with
//! no lock held: a client that reads slowly holds no `prune` or `rmi` up.

use std::fs::File;
use std::io::Write;

use serde_json::{Value, json};

use super::{ArchiveError, CONTAINERD_NAME, OCI_LAYOUT, OCI_REF_NAME};
use crate::digest::Digest;
use crate::manifest::{ImageManifest, media_type};
use crate::reference::ImageReference;
use crate::store::Store;

/// An image to save: its names (references with a tag, or a digest), its
/// manifest and the manifest's digest (the platform's, never an index's).
#[derive(Debug, Clone)]
pub struct ToSave {
    /// Its names; none for an image saved by its ID (an untagged archive,
    /// as Docker's).
    pub names: Vec<ImageReference>,
    /// Its manifest's digest: the image's ID.
    pub manifest_digest: Digest,
    /// The index it came through, if one: saved with it (only this
    /// platform's manifest in the archive, as Docker 25's), so a name with
    /// the index's digest (`moby/buildkit:v0.34.1@sha256:<index>`) loads
    /// back, and the image keeps it (step 12.3).
    pub index_digest: Option<Digest>,
    /// Its manifest.
    pub manifest: ImageManifest,
}

/// Images whose blobs are open, to write out.
#[derive(Debug)]
pub struct Opened {
    images: Vec<OpenedImage>,
    /// Every blob once, in the order first needed: its digest, size and
    /// file.
    blobs: Vec<(Digest, u64, File)>,
}

/// One image of [`Opened`]: its names, and its manifest's media type and
/// size (what `index.json`'s descriptor says).
#[derive(Debug)]
struct OpenedImage {
    names: Vec<ImageReference>,
    manifest_digest: Digest,
    manifest_media_type: String,
    manifest_size: u64,
    /// What `index.json` points at for it: the index it came through
    /// (digest, media type, size), else its manifest.
    top: Option<(Digest, String, u64)>,
    config: Digest,
    layers: Vec<Digest>,
}

/// Opens every blob of `images` in `store`: under the store's lock, held
/// by the caller, so none is collected meanwhile; once open, they're read
/// whatever happens to their names.
///
/// # Errors
///
/// A blob that isn't in the store ([`ArchiveError::Malformed`]: an image
/// pulled on another host keeps them all, so this is a damaged store), or
/// can't be opened or read.
pub fn open(store: &Store, images: &[ToSave]) -> Result<Opened, ArchiveError> {
    // Each blob is held open until it's written: past what this process may
    // open, refused before the first, never failed half way.
    let mut distinct: Vec<&Digest> = Vec::new();
    for image in images {
        for digest in std::iter::once(&image.manifest_digest)
            .chain(std::iter::once(&image.manifest.config.digest))
            .chain(image.manifest.layers.iter().map(|layer| &layer.digest))
        {
            if !distinct.contains(&digest) {
                distinct.push(digest);
            }
        }
    }
    let room = descriptors_free();
    if distinct.len() > room {
        return Err(ArchiveError::TooLarge(format!(
            "{} blobs, more than this process may hold open ({room} with its descriptor \
             limit): save fewer images at once, or raise the limit (ulimit -n)",
            distinct.len()
        )));
    }
    let mut opened = Opened {
        images: Vec::new(),
        blobs: Vec::new(),
    };
    for image in images {
        let manifest_size = opened.add(store, &image.manifest_digest)?;
        let manifest_bytes = store.read_blob(&image.manifest_digest, crate::MANIFEST_SIZE_LIMIT)?;
        // The index it came through, if the store still has it.
        let top = match &image.index_digest {
            Some(index) if store.contains(index) => {
                let size = opened.add(store, index)?;
                let bytes = store.read_blob(index, crate::MANIFEST_SIZE_LIMIT)?;
                let media = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|value| value.get("mediaType")?.as_str().map(str::to_owned))
                    .filter(|given| media_type::is_index(given))
                    .unwrap_or_else(|| media_type::OCI_INDEX.to_owned());
                Some((index.clone(), media, size))
            }
            _ => None,
        };
        for blob in std::iter::once(&image.manifest.config).chain(&image.manifest.layers) {
            opened.add(store, &blob.digest)?;
        }
        opened.images.push(OpenedImage {
            names: image.names.clone(),
            manifest_digest: image.manifest_digest.clone(),
            manifest_media_type: media_type::of_manifest(&manifest_bytes),
            manifest_size,
            top,
            config: image.manifest.config.digest.clone(),
            layers: image
                .manifest
                .layers
                .iter()
                .map(|layer| layer.digest.clone())
                .collect(),
        });
    }
    Ok(opened)
}

/// How many more descriptors this process may open: its soft limit, less
/// what it holds now and room for the work around it.
fn descriptors_free() -> usize {
    const AROUND: usize = 64;
    let limit = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .ok()
        .and_then(|(soft, _)| usize::try_from(soft).ok())
        .unwrap_or(1024);
    let open = std::fs::read_dir("/proc/self/fd")
        .or_else(|_| std::fs::read_dir("/dev/fd"))
        .map_or(0, Iterator::count);
    limit.saturating_sub(open).saturating_sub(AROUND)
}

impl Opened {
    /// Opens blob `digest` once: its size.
    fn add(&mut self, store: &Store, digest: &Digest) -> Result<u64, ArchiveError> {
        if let Some((_, size, _)) = self.blobs.iter().find(|(had, _, _)| had == digest) {
            return Ok(*size);
        }
        let (file, size) = store
            .open_sized(digest)?
            .ok_or_else(|| ArchiveError::Missing(digest.clone()))?;
        self.blobs.push((digest.clone(), size, file));
        Ok(size)
    }

    /// Writes the archive to `out`: the layout's files, then the blobs, then
    /// `manifest.json`.
    ///
    /// # Errors
    ///
    /// `out` can't be written (its reader gone), or a blob can't be read.
    pub fn write(self, out: impl Write) -> Result<(), ArchiveError> {
        let writing = |source| ArchiveError::io("writing the archive", source);
        // Every header is made here (`header`, `directory`): the same each
        // time, whatever the store's files say.
        let mut tar = tar::Builder::new(out);
        append(&mut tar, "oci-layout", OCI_LAYOUT).map_err(writing)?;
        append(&mut tar, "index.json", &self.index_json()).map_err(writing)?;
        directory(&mut tar, "blobs/").map_err(writing)?;
        directory(&mut tar, "blobs/sha256/").map_err(writing)?;
        let manifest_json = self.manifest_json();
        for (digest, size, file) in self.blobs {
            let mut header = header(size);
            tar.append_data(&mut header, format!("blobs/sha256/{}", digest.hex()), file)
                .map_err(writing)?;
        }
        append(&mut tar, "manifest.json", &manifest_json).map_err(writing)?;
        tar.into_inner()
            .and_then(|mut out| out.flush())
            .map_err(writing)
    }

    /// `index.json`: a descriptor of each image's manifest for each of its
    /// names (a tag's `ref.name` the tag alone, as Docker 25 writes it;
    /// containerd's the name in full), one with none for an untagged image.
    fn index_json(&self) -> Vec<u8> {
        let mut manifests = Vec::new();
        for image in &self.images {
            let descriptor = |annotations: Option<Value>| {
                let mut entry = match &image.top {
                    Some((digest, media, size)) => json!({
                        "mediaType": media,
                        "digest": digest.to_string(),
                        "size": size,
                    }),
                    None => json!({
                        "mediaType": image.manifest_media_type,
                        "digest": image.manifest_digest.to_string(),
                        "size": image.manifest_size,
                    }),
                };
                if let Some(annotations) = annotations {
                    entry["annotations"] = annotations;
                }
                entry
            };
            if image.names.is_empty() {
                manifests.push(descriptor(None));
            }
            for name in &image.names {
                let mut annotations = json!({ CONTAINERD_NAME: name.to_string() });
                if let (Some(tag), None) = (&name.tag, &name.digest) {
                    annotations[OCI_REF_NAME] = json!(tag);
                }
                manifests.push(descriptor(Some(annotations)));
            }
        }
        to_json(&json!({
            "schemaVersion": 2,
            "mediaType": media_type::OCI_INDEX,
            "manifests": manifests,
        }))
    }

    /// `manifest.json`, Docker's: each image's config and layers by path,
    /// and its tags as Docker shows them (a name with a digest isn't a tag:
    /// Docker's loader refuses one there; it's in `index.json`).
    fn manifest_json(&self) -> Vec<u8> {
        let path = |digest: &Digest| format!("blobs/sha256/{}", digest.hex());
        let entries: Vec<Value> = self
            .images
            .iter()
            .map(|image| {
                let tags: Vec<String> = image
                    .names
                    .iter()
                    .filter(|name| name.digest.is_none())
                    .map(ImageReference::familiar)
                    .collect();
                json!({
                    "Config": path(&image.config),
                    "RepoTags": if tags.is_empty() { Value::Null } else { json!(tags) },
                    "Layers": image.layers.iter().map(path).collect::<Vec<_>>(),
                })
            })
            .collect();
        to_json(&Value::Array(entries))
    }
}

/// `value` as JSON bytes.
fn to_json(value: &Value) -> Vec<u8> {
    // A `Value` always serializes.
    serde_json::to_vec(value).unwrap_or_default()
}

/// A file's header: `size` bytes, read-only, owned by 0, at time 0.
fn header(size: u64) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o444);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header
}

/// Appends a file `name` holding `bytes`.
fn append<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut header = header(bytes.len() as u64);
    tar.append_data(&mut header, name, bytes)
}

/// Appends a directory `name`.
fn directory<W: Write>(tar: &mut tar::Builder<W>, name: &str) -> std::io::Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_mtime(0);
    tar.append_data(&mut header, name, std::io::empty())
}
