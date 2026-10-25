//! `load`: an archive read into a staging directory of its own in the store
//! ([`read`]), its images found in it ([`Archive::images`]), then those
//! images' blobs moved into the store ([`Archive::promote`], under the
//! store's shared lock, the caller's).
//!
//! The archive is hostile (anyone made it). One pass over it, with the tar
//! reader's limits ([`TarReader`]): a member is matched by its name, never
//! written by it; `blobs/sha256/<hex>` is staged checked against `<hex>`, a
//! legacy archive's `<hex>.json` config too, a legacy `<dir>/layer.tar`
//! staged under the digest its bytes have; `index.json` and `manifest.json`
//! are held (16 MiB at most, both); anything else (`oci-layout` among them)
//! is skipped. Links are names only: a `layer.tar` that's a link to another
//! (Docker's legacy save writes a duplicate layer so) resolves to that one's
//! digest, lexically, 8 links at most. Nothing reaches the store until the
//! whole archive has been read and found sound, and a stream that ends
//! before the tar's end-of-archive marker is a cut archive, never a whole
//! one.
//!
//! Formats: Docker 25's (and Shipyard's own `save`): an OCI layout with
//! `manifest.json`; BuildKit's OCI layout alone (names from its index's
//! annotations); Docker's legacy archive (a manifest is made for it:
//! `legacy`); any of them gzipped.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::staging::Staging;
use super::{ArchiveError, CONTAINERD_NAME, OCI_REF_NAME};
use crate::digest::Digest;
use crate::manifest::{ImageConfig, ImageManifest, Index, Platform, media_type};
use crate::reference::ImageReference;
use crate::store::Store;
use crate::tar_reader::{Kind, TarLimits, TarReader};

/// What a load holds in memory at most: `index.json` and `manifest.json`
/// together.
pub const MAX_DOCUMENTS: u64 = 16 * 1024 * 1024;

/// The most legacy paths (a `layer.tar`, a config, a link) an archive may
/// name.
pub const MAX_PATHS: usize = 4096;

/// The most names (tags) one archive may give its images.
pub const MAX_NAMES: usize = 1024;

/// The most entries one document may list: `manifest.json`'s images,
/// `index.json`'s manifests, an index's within it. Checked before any of
/// them is read, so the work a load does is bounded by it, not by what the
/// documents' 16 MiB can hold.
pub const MAX_ENTRIES: usize = 1024;

/// The most links followed resolving one legacy path.
const MAX_HOPS: usize = 8;

/// The most bytes one archive may decode to (a gzipped one's bomb).
pub const MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// What `index.json`'s annotations name a manifest it leads to.
#[derive(Debug)]
struct Named {
    manifest: Digest,
    index: Option<Digest>,
    names: Vec<String>,
}

/// An archive read: its blobs staged, what it says held.
#[derive(Debug)]
pub struct Archive {
    staging: Staging,
    index_json: Option<Vec<u8>>,
    manifest_json: Option<Vec<u8>>,
    /// A legacy archive's paths, by their name in it: a staged blob, or a
    /// link to another path.
    paths: BTreeMap<PathBuf, LegacyPath>,
}

/// A legacy archive's path.
#[derive(Debug, Clone)]
enum LegacyPath {
    /// A staged blob.
    Blob(Digest),
    /// A link, to this path (as written: relative to the link's directory,
    /// or the archive's root for a hard link).
    Link(PathBuf),
}

/// An image the archive carries, found sound: its names, its manifest (and
/// the index it was chosen from, if any), and every blob it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Its names, canonical; at least one.
    pub names: Vec<ImageReference>,
    /// Its manifest's digest: its ID.
    pub manifest_digest: Digest,
    /// The index its manifest was chosen from, if it came through one.
    pub index_digest: Option<Digest>,
    /// Its own blobs: the index's (if any) and the manifest's
    /// ([`Archive::blobs_of`] adds its config's and its layers').
    pub blobs: Vec<Digest>,
}

/// The images an archive carries: those found sound, and why the others
/// were left out (said to the user; the archive still loads the rest).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Images {
    /// The images to load.
    pub found: Vec<Found>,
    /// What was left out, and why (each said as a sentence).
    pub refused: Vec<LeftOut>,
}

/// Why an image in an archive was left out of its load (the rest still
/// load).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LeftOut {
    /// A name that isn't one (as given, quoted).
    #[error("the name {0:?} isn't one: left out")]
    NotAName(String),
    /// A name with a digest that isn't the image's own.
    #[error("the name {0:?} names another image's digest: left out")]
    ForeignDigest(String),
    /// An image the archive gives no name (D12.2-3).
    #[error(
        "the image {0} has no name: save it by its name (shipyard save NAME:TAG), and load that"
    )]
    Unnamed(Digest),
    /// An image that can't be read, or whose blobs aren't all there.
    #[error("{name}: {why}: left out")]
    Unsound {
        /// Its first name.
        name: String,
        /// What's wrong with it.
        why: String,
    },
    /// An image for another platform.
    #[error("{name}: it's for {theirs}, not this host's {host}: left out")]
    OtherPlatform {
        /// Its first name.
        name: String,
        /// Its platform (from its config: quoted, it's anyone's).
        theirs: String,
        /// This host's.
        host: String,
    },
}

/// Reads `input`, an archive, into a staging directory in `store`: the
/// whole stream, to its end-of-archive marker.
///
/// # Errors
///
/// The archive is cut, malformed, past a bound, compressed with what isn't
/// gzip, or a blob in it isn't its digest; the store's filesystem would be
/// left with less than its reserve.
pub fn read(input: impl Read, store: &Store) -> Result<Archive, ArchiveError> {
    let staging = Staging::new(store)?;
    let (decoded, ended) = super::decoded(input)?;
    let mut tar = TarReader::new(decoded.take(MAX_BYTES), TarLimits::default());
    let mut archive = Archive {
        staging,
        index_json: None,
        manifest_json: None,
        paths: BTreeMap::new(),
    };
    match read_members(&mut tar, &mut archive) {
        // Whatever failed once the stream had run out (an entry, a header
        // or a gzip stream cut short) failed for that: a cut archive.
        Err(ArchiveError::Tar(_) | ArchiveError::Read(_)) if ended.get() => Err(ArchiveError::Cut),
        Err(error) => Err(error),
        Ok(()) if !tar.marked_end() => Err(ArchiveError::Cut),
        Ok(()) => Ok(archive),
    }
}

/// Reads every member of `tar` into `archive`: [`read`]'s pass.
fn read_members(tar: &mut TarReader<impl Read>, archive: &mut Archive) -> Result<(), ArchiveError> {
    let mut held: u64 = 0;
    while let Some(entry) = tar.next_entry()? {
        let path = entry.path.clone();
        match entry.kind {
            Kind::File => {
                let document = match path.to_str() {
                    Some("index.json") => Some(&mut archive.index_json),
                    Some("manifest.json") => Some(&mut archive.manifest_json),
                    _ => None,
                };
                if let Some(document) = document {
                    if document.is_some() {
                        return Err(ArchiveError::Malformed(format!(
                            "it has {} twice",
                            path.display()
                        )));
                    }
                    held = held.saturating_add(entry.size);
                    if held > MAX_DOCUMENTS {
                        return Err(ArchiveError::TooLarge(format!(
                            "documents past {MAX_DOCUMENTS} bytes"
                        )));
                    }
                    let mut bytes = Vec::new();
                    (&mut *tar)
                        .take(entry.size)
                        .read_to_end(&mut bytes)
                        .map_err(ArchiveError::Read)?;
                    *document = Some(bytes);
                    continue;
                }
                if let Some(hex) = oci_blob(&path) {
                    archive.staging.stage_named(&hex, tar, entry.size)?;
                } else if let Some(hex) = legacy_config(&path) {
                    let digest = archive.staging.stage_named(&hex, tar, entry.size)?;
                    archive.add_path(path, LegacyPath::Blob(digest))?;
                } else if is_legacy_layer(&path) {
                    if archive.paths.contains_key(&path) {
                        return Err(ArchiveError::Malformed(format!(
                            "it has {} twice",
                            path.display()
                        )));
                    }
                    let digest = archive.staging.stage(tar, entry.size)?;
                    archive.add_path(path, LegacyPath::Blob(digest))?;
                }
                // Anything else (`repositories`, a legacy `<dir>/json`,
                // `VERSION`): skipped.
            }
            Kind::Symlink | Kind::Hardlink if is_legacy_layer(&path) => {
                let target = PathBuf::from(entry.link.unwrap_or_default());
                let target = if entry.kind == Kind::Symlink {
                    target
                } else {
                    // A hard link names its target from the archive's root:
                    // made relative to the link's own directory (one down
                    // from the root), as a symlink's is.
                    Path::new("..").join(&target)
                };
                archive.add_path(path, LegacyPath::Link(target))?;
            }
            // Directories, and anything else: names in the archive, nothing
            // to load.
            _ => {}
        }
    }
    Ok(())
}

/// The hex of `blobs/sha256/<hex>`, if `path` is one.
fn oci_blob(path: &Path) -> Option<String> {
    let mut components = path.components();
    match (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    ) {
        (
            Some(Component::Normal(blobs)),
            Some(Component::Normal(sha)),
            Some(Component::Normal(hex)),
            None,
        ) if blobs == "blobs" && sha == "sha256" => {
            let hex = hex.to_str()?;
            Digest::parse(&format!("sha256:{hex}")).ok()?;
            Some(hex.to_owned())
        }
        _ => None,
    }
}

/// The hex of a legacy archive's config, `<hex>.json` at its root.
fn legacy_config(path: &Path) -> Option<String> {
    let name = path.to_str()?;
    let hex = name.strip_suffix(".json")?;
    if name.contains('/') || hex.len() != 64 {
        return None;
    }
    Digest::parse(&format!("sha256:{hex}")).ok()?;
    Some(hex.to_owned())
}

/// Whether `path` is a legacy archive's layer: `<dir>/layer.tar`.
fn is_legacy_layer(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(Component::Normal(_)), Some(Component::Normal(name)), None) if name == "layer.tar"
    )
}

impl Archive {
    /// Keeps a legacy path, within [`MAX_PATHS`]; one named twice is
    /// refused (never the last one winning).
    fn add_path(&mut self, path: PathBuf, what: LegacyPath) -> Result<(), ArchiveError> {
        if self.paths.len() >= MAX_PATHS {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_PATHS} legacy paths"
            )));
        }
        if self.paths.contains_key(&path) {
            return Err(ArchiveError::Malformed(format!(
                "it has {} twice",
                path.display()
            )));
        }
        self.paths.insert(path, what);
        Ok(())
    }

    /// The blob a path in `manifest.json` names: `blobs/sha256/<hex>`, or a
    /// legacy path (its links followed lexically, [`MAX_HOPS`] at most).
    fn blob_at(&self, path: &str) -> Result<Digest, ArchiveError> {
        let named = |what: String| ArchiveError::Malformed(format!("{path:?}: {what}"));
        let mut at: PathBuf = Path::new(path).components().collect();
        for _ in 0..=MAX_HOPS {
            if let Some(hex) = oci_blob(&at) {
                return self.staged(&Digest::parse(&format!("sha256:{hex}"))?, path);
            }
            match self.paths.get(&at) {
                Some(LegacyPath::Blob(digest)) => return Ok(digest.clone()),
                Some(LegacyPath::Link(target)) => {
                    let from = at.parent().unwrap_or(Path::new(""));
                    at = lexical(&from.join(target))
                        .ok_or_else(|| named("a link out of the archive".to_owned()))?;
                }
                None => return Err(named("names nothing the archive carries".to_owned())),
            }
        }
        Err(named(format!("more than {MAX_HOPS} links")))
    }

    /// `digest`, if staged.
    fn staged(&self, digest: &Digest, named: &str) -> Result<Digest, ArchiveError> {
        if self.staging.has(digest) {
            Ok(digest.clone())
        } else {
            Err(ArchiveError::Malformed(format!(
                "{named:?} names the blob {digest}, which it doesn't carry"
            )))
        }
    }

    /// The images the archive carries for `platform`: by `manifest.json` if
    /// it has one (Docker's, and Shipyard's), else by its OCI index. An
    /// image with no name, or not for `platform`, is left out, said.
    ///
    /// # Errors
    ///
    /// A document that doesn't parse, or names a blob the archive doesn't
    /// carry; an archive with no images at all.
    pub fn images(&mut self, platform: &Platform) -> Result<Images, ArchiveError> {
        let mut images = if let Some(bytes) = self.manifest_json.clone() {
            self.by_manifest_json(&bytes, platform)?
        } else if let Some(bytes) = self.index_json.clone() {
            self.by_index(&bytes, platform)?
        } else {
            return Err(ArchiveError::Malformed(
                "it has neither manifest.json nor index.json: it isn't an image archive".to_owned(),
            ));
        };
        if images.found.is_empty() && images.refused.is_empty() {
            return Err(ArchiveError::Malformed("it carries no image".to_owned()));
        }
        let mut names = 0;
        let mut kept = Vec::new();
        for found in std::mem::take(&mut images.found) {
            match self.check(&found, platform) {
                Ok(()) => {
                    names += found.names.len();
                    kept.push(found);
                }
                Err(why) => images.refused.push(why),
            }
        }
        if names > MAX_NAMES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_NAMES} names"
            )));
        }
        images.found = kept;
        Ok(images)
    }

    /// The images `manifest.json` lists: each one's manifest found in the
    /// index (an OCI layout's: its ID kept) or made (a legacy archive's).
    fn by_manifest_json(
        &mut self,
        bytes: &[u8],
        platform: &Platform,
    ) -> Result<Images, ArchiveError> {
        #[derive(Deserialize)]
        struct Entry {
            #[serde(rename = "Config")]
            config: String,
            #[serde(rename = "RepoTags", default)]
            repo_tags: Option<Vec<String>>,
            #[serde(rename = "Layers", default)]
            layers: Vec<String>,
        }
        let entries: Vec<Entry> = serde_json::from_slice(bytes).map_err(|error| {
            ArchiveError::Malformed(format!("its manifest.json isn't Docker's: {error}"))
        })?;
        if entries.len() > MAX_ENTRIES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_ENTRIES} images in its manifest.json"
            )));
        }
        let tags: usize = entries
            .iter()
            .map(|entry| entry.repo_tags.as_ref().map_or(0, Vec::len))
            .sum();
        if tags > MAX_NAMES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_NAMES} names"
            )));
        }
        if entries
            .iter()
            .any(|entry| entry.layers.len() > crate::MAX_LAYERS)
        {
            return Err(ArchiveError::TooLarge(format!(
                "an image of more than {} layers",
                crate::MAX_LAYERS
            )));
        }
        let (in_index, annotated) = match self.index_json.clone() {
            Some(index) => {
                let manifests = self.index_manifests(&index, platform)?;
                let annotated = annotated_names(&index, &manifests)?;
                (manifests, annotated)
            }
            None => (Vec::new(), Vec::new()),
        };
        let mut images = Images::default();
        for entry in entries {
            let config = self.blob_at(&entry.config)?;
            let layers = entry
                .layers
                .iter()
                .map(|layer| self.blob_at(layer))
                .collect::<Result<Vec<_>, _>>()?;
            let found = in_index
                .iter()
                .find(|(_, _, manifest)| {
                    manifest.config.digest == config
                        && manifest
                            .layers
                            .iter()
                            .map(|layer| &layer.digest)
                            .eq(layers.iter())
                })
                .map(|(digest, index, _)| (digest.clone(), index.clone()));
            let (manifest_digest, index_digest) = match found {
                Some(found) => found,
                None => (
                    super::legacy::make_manifest(&mut self.staging, &config, &layers)?,
                    None,
                ),
            };
            // Its tags, and what the index names it (a name with a digest is
            // there only: Docker's loader refuses one in `RepoTags`).
            let mut names: Vec<String> = entry.repo_tags.unwrap_or_default();
            if let Some(had) = annotated.iter().find(|had| had.manifest == manifest_digest) {
                names.extend(had.names.iter().cloned());
            }
            add_named(&mut images, names, manifest_digest, index_digest);
        }
        Ok(images)
    }

    /// The images an OCI layout's index names (BuildKit's `type=oci`), by
    /// their annotations: containerd's name, or `ref.name` when it's a
    /// name in full.
    fn by_index(&mut self, bytes: &[u8], platform: &Platform) -> Result<Images, ArchiveError> {
        let mut images = Images::default();
        let manifests = self.index_manifests(bytes, platform)?;
        for named in annotated_names(bytes, &manifests)? {
            add_named(&mut images, named.names, named.manifest, named.index);
        }
        Ok(images)
    }

    /// The image manifests `index.json` leads to, each with the index it was
    /// chosen from: a manifest entry as it is; an index entry (BuildKit's
    /// multi-platform, Docker 25's whole index) by the platform among the
    /// manifests the archive carries. Only staged manifests count.
    fn index_manifests(
        &self,
        bytes: &[u8],
        platform: &Platform,
    ) -> Result<Vec<IndexManifest>, ArchiveError> {
        let top = Index::parse(bytes).map_err(|source| ArchiveError::Document {
            what: "index.json".to_owned(),
            source,
        })?;
        if top.manifests.len() > MAX_ENTRIES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_ENTRIES} manifests in its index.json"
            )));
        }
        let mut found: Vec<IndexManifest> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for entry in &top.manifests {
            // Each read once, however many entries name it.
            if !self.staging.has(&entry.digest) || !seen.insert(entry.digest.to_string()) {
                continue;
            }
            let blob = self
                .staging
                .read(&entry.digest, crate::MANIFEST_SIZE_LIMIT)?;
            if media_type::is_index(&entry.media_type) {
                let inner = Index::parse(&blob).map_err(|source| ArchiveError::Document {
                    what: format!("index {}", entry.digest),
                    source,
                })?;
                if inner.manifests.len() > MAX_ENTRIES {
                    return Err(ArchiveError::TooLarge(format!(
                        "more than {MAX_ENTRIES} manifests in its index {}",
                        entry.digest
                    )));
                }
                let carried = Index {
                    manifests: inner
                        .manifests
                        .into_iter()
                        .filter(|manifest| self.staging.has(&manifest.digest))
                        .collect(),
                };
                let Ok(chosen) = carried.select(platform) else {
                    continue;
                };
                let bytes = self
                    .staging
                    .read(&chosen.digest, crate::MANIFEST_SIZE_LIMIT)?;
                found.push((
                    chosen.digest.clone(),
                    Some(entry.digest.clone()),
                    parse_manifest(&bytes, &chosen.digest)?,
                ));
            } else if media_type::is_manifest(&entry.media_type) {
                found.push((
                    entry.digest.clone(),
                    None,
                    parse_manifest(&blob, &entry.digest)?,
                ));
            }
        }
        Ok(found)
    }

    /// Whether `found` can be loaded here: its manifest and config parse,
    /// every blob is in the archive, and it runs on `platform`.
    fn check(&self, found: &Found, platform: &Platform) -> Result<(), LeftOut> {
        let first = found
            .names
            .first()
            .map_or_else(String::new, ImageReference::familiar);
        let left_out = |why: String| LeftOut::Unsound {
            name: first.clone(),
            why,
        };
        let manifest = self
            .staging
            .read(&found.manifest_digest, crate::MANIFEST_SIZE_LIMIT)
            .map_err(|error| left_out(error.to_string()))
            .and_then(|bytes| {
                parse_manifest(&bytes, &found.manifest_digest)
                    .map_err(|error| left_out(error.to_string()))
            })?;
        if manifest.layers.len() > crate::MAX_LAYERS {
            return Err(left_out(format!("more than {} layers", crate::MAX_LAYERS)));
        }
        for blob in std::iter::once(&manifest.config).chain(&manifest.layers) {
            if !self.staging.has(&blob.digest) {
                return Err(left_out(format!(
                    "the archive doesn't carry its blob {}",
                    blob.digest
                )));
            }
        }
        let config = self
            .staging
            .read(&manifest.config.digest, crate::MANIFEST_SIZE_LIMIT)
            .map_err(|error| left_out(error.to_string()))?;
        let config = ImageConfig::parse(&config).map_err(|error| left_out(error.to_string()))?;
        let theirs = config.platform();
        if !theirs.runs_on(platform) {
            return Err(LeftOut::OtherPlatform {
                name: first,
                theirs: format!("{:?}", theirs.to_string()),
                host: platform.to_string(),
            });
        }
        Ok(())
    }

    /// Every blob `found` needs: its own list, and its manifest's config and
    /// layers.
    ///
    /// # Errors
    ///
    /// Its manifest can't be read again.
    pub fn blobs_of(&self, found: &Found) -> Result<Vec<Digest>, ArchiveError> {
        let bytes = self
            .staging
            .read(&found.manifest_digest, crate::MANIFEST_SIZE_LIMIT)?;
        let manifest = parse_manifest(&bytes, &found.manifest_digest)?;
        let mut blobs = found.blobs.clone();
        blobs.push(manifest.config.digest);
        blobs.extend(manifest.layers.into_iter().map(|layer| layer.digest));
        Ok(blobs)
    }

    /// Moves `digests` from the staging directory into `store`: the caller
    /// holds the store's lock (shared), so no collection runs meanwhile; a
    /// blob the store has already stays as it is.
    ///
    /// # Errors
    ///
    /// A blob that can't be moved, or the store's directory synced.
    pub fn promote(&self, store: &Store, digests: &[Digest]) -> Result<(), ArchiveError> {
        self.staging.promote(store, digests)
    }
}

/// Adds the image `manifest` under `given` names: each one parsed and
/// made canonical; one with a digest only if it's the image's own; an
/// image with none left out, said (D12.2-3).
fn add_named(images: &mut Images, given: Vec<String>, manifest: Digest, index: Option<Digest>) {
    let mut names = Vec::new();
    for text in given {
        let Ok(name) = ImageReference::parse(&text) else {
            images.refused.push(LeftOut::NotAName(text));
            continue;
        };
        if let Some(digest) = &name.digest
            && *digest != manifest
            && Some(digest) != index.as_ref()
        {
            images.refused.push(LeftOut::ForeignDigest(text));
            continue;
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        images.refused.push(LeftOut::Unnamed(manifest));
        return;
    }
    let mut blobs: Vec<Digest> = index.iter().cloned().collect();
    blobs.push(manifest.clone());
    images.found.push(Found {
        names,
        manifest_digest: manifest,
        index_digest: index,
        blobs,
    });
}

/// The names `index.json`'s annotations give each manifest it leads to,
/// in its order (`manifests`, [`Archive::index_manifests`]'s): containerd's name, or `ref.name` when it's a name in
/// full (a tag alone, `latest`, isn't).
fn annotated_names(bytes: &[u8], manifests: &[IndexManifest]) -> Result<Vec<Named>, ArchiveError> {
    let index: Value = serde_json::from_slice(bytes)
        .map_err(|error| ArchiveError::Malformed(format!("its index.json: {error}")))?;
    let entries = index
        .get("manifests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut named: Vec<Named> = Vec::new();
    for entry in &entries {
        let Some(digest) = entry
            .get("digest")
            .and_then(Value::as_str)
            .and_then(|digest| Digest::parse(digest).ok())
        else {
            continue;
        };
        let Some((manifest, index, _)) = manifests
            .iter()
            .find(|(manifest, index, _)| *manifest == digest || index.as_ref() == Some(&digest))
        else {
            continue;
        };
        let annotation = |key: &str| {
            entry
                .get("annotations")
                .and_then(|annotations| annotations.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let name = annotation(CONTAINERD_NAME).or_else(|| {
            annotation(OCI_REF_NAME).filter(|name| name.contains('/') || name.contains(':'))
        });
        match named.iter_mut().find(|had| had.manifest == *manifest) {
            Some(had) => had.names.extend(name),
            None => named.push(Named {
                manifest: manifest.clone(),
                index: index.clone(),
                names: name.into_iter().collect(),
            }),
        }
    }
    Ok(named)
}

/// `bytes` as an image manifest.
fn parse_manifest(bytes: &[u8], digest: &Digest) -> Result<ImageManifest, ArchiveError> {
    let manifest = ImageManifest::parse(bytes).map_err(|source| ArchiveError::Document {
        what: format!("manifest {digest}"),
        source,
    })?;
    // At once: a manifest of more is never kept, nor its layers looked at.
    if manifest.layers.len() > crate::MAX_LAYERS {
        return Err(ArchiveError::TooLarge(format!(
            "a manifest of more than {} layers ({digest})",
            crate::MAX_LAYERS
        )));
    }
    Ok(manifest)
}

/// A manifest `index.json` leads to: its digest, the index it was chosen
/// from (if one), and the manifest.
type IndexManifest = (Digest, Option<Digest>, ImageManifest);

/// `path` with its `.` and `..` resolved by name alone; `None` if it climbs
/// out of the archive.
fn lexical(path: &Path) -> Option<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => resolved.push(name),
            Component::ParentDir => {
                if !resolved.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(resolved)
}
