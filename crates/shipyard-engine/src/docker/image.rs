//! Images as Docker's Engine API shapes them (step 11.2c1): `GET
//! /images/json`'s `ImageSummary` and `GET /images/{name}/json`'s
//! `ImageInspect`, at API 1.43, checked against Docker 29.8's.
//!
//! An image's ID is its manifest's digest (D11.2c-2): what Shipyard's
//! containers already say they run, and what Docker 29's containerd store
//! shows for an image of one platform (for one of several, Docker shows the
//! index's). Its sizes are its layers' as pulled (compressed): Shipyard
//! keeps no total of their unpacked bytes.

use std::collections::BTreeMap;

use serde::Serialize;
use shipyard_image::index::Index;
use shipyard_image::{ImageReference, ListedImage, LockMode, Platform, Pulled, USE_WAIT};

use super::filters::ImageFilters;
use super::{HealthConfig, ZERO_TIME};
use crate::Engine;
use crate::error::{Context as _, Result};

/// An image in `GET /images/json` (Docker's `ImageSummary`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct ImageSummary {
    /// Its ID: `sha256:` and its manifest's digest.
    pub id: String,
    /// Its parent's ID: always empty (a pulled image has none).
    pub parent_id: String,
    /// Its references, as Docker shows them (`alpine:3.20`).
    pub repo_tags: Vec<String>,
    /// Its repositories at its digest (`alpine@sha256:…`).
    pub repo_digests: Vec<String>,
    /// When it was built, in Unix seconds (0 if it doesn't say).
    pub created: i64,
    /// Its layers' bytes, as pulled.
    pub size: u64,
    /// The bytes it shares with other images: not counted (-1, as
    /// Docker's unless asked).
    pub shared_size: i64,
    /// As [`size`](Self::size) (Docker's older name).
    pub virtual_size: u64,
    /// Its labels, or `null`.
    pub labels: Option<BTreeMap<String, String>>,
    /// Its containers: not counted (-1), as Docker's list says (its daemon
    /// counts them for `system df` alone).
    pub containers: i64,
}

/// An image in `GET /images/{name}/json` (Docker's `ImageInspect`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct ImageInspect {
    /// Its ID ([`ImageSummary::id`]).
    pub id: String,
    /// Its references, as Docker shows them.
    pub repo_tags: Vec<String>,
    /// Its repositories at its digest.
    pub repo_digests: Vec<String>,
    /// Its parent's ID: always empty.
    pub parent: String,
    /// A comment: always empty (Shipyard reads no build history).
    pub comment: String,
    /// When it was built (RFC 3339), or empty.
    pub created: String,
    /// The container it was committed from: always empty.
    pub container: String,
    /// That container's config: always `null`.
    pub container_config: Option<()>,
    /// The Docker version that built it, or empty.
    pub docker_version: String,
    /// Its author, or empty.
    pub author: String,
    /// What it runs by default.
    pub config: ImageConfig,
    /// Its CPU architecture.
    pub architecture: String,
    /// Its CPU variant, if it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Its operating system.
    pub os: String,
    /// Its layers' bytes, as pulled.
    pub size: u64,
    /// As [`size`](Self::size).
    pub virtual_size: u64,
    /// Where its layers are kept.
    pub graph_driver: GraphDriver,
    /// Its layers, by their unpacked digests.
    #[serde(rename = "RootFS")]
    pub root_fs: RootFs,
    /// When a reference was last pointed at it (a pull, a tag); Docker's
    /// zero time if none was kept (an older Shipyard's).
    pub metadata: Metadata,
}

/// What an image runs by default, as Docker's inspect shows it: its
/// container config, the fields an image sets.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Docker's shape: the streams' flags, always false for an image"
)]
pub struct ImageConfig {
    /// Always empty.
    pub hostname: String,
    /// Always empty.
    pub domainname: String,
    /// Its user, or empty.
    pub user: String,
    /// Always false.
    pub attach_stdin: bool,
    /// Always false.
    pub attach_stdout: bool,
    /// Always false.
    pub attach_stderr: bool,
    /// The ports it says it listens on, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposed_ports: Option<BTreeMap<String, serde_json::Map<String, serde_json::Value>>>,
    /// Always false.
    pub tty: bool,
    /// Always false.
    pub open_stdin: bool,
    /// Always false.
    pub stdin_once: bool,
    /// Its environment.
    pub env: Vec<String>,
    /// Its command, or `null`.
    pub cmd: Option<Vec<String>>,
    /// Its health check, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,
    /// Always empty.
    pub image: String,
    /// Its volumes' paths, or `null`.
    pub volumes: Option<BTreeMap<String, serde_json::Map<String, serde_json::Value>>>,
    /// Its working directory, or empty.
    pub working_dir: String,
    /// Its entrypoint, or `null`.
    pub entrypoint: Option<Vec<String>>,
    /// Always `null`.
    pub on_build: Option<()>,
    /// Its labels, or `null`.
    pub labels: Option<BTreeMap<String, String>>,
    /// The signal that stops it, if it sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    /// The shell its shell forms run with, if it sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<Vec<String>>,
}

/// Where an image's layers are kept.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct GraphDriver {
    /// Always `overlayfs` (Docker 29's name for its snapshotter's).
    pub name: String,
    /// Always `null`.
    pub data: Option<()>,
}

/// An image's layers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct RootFs {
    /// Always `layers`.
    #[serde(rename = "Type")]
    pub kind: String,
    /// The unpacked layers' digests, base first.
    pub layers: Vec<String>,
}

/// An image's own bookkeeping.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Metadata {
    /// When it was last tagged.
    pub last_tag_time: String,
}

/// One image, as read from the store: its references and what it is.
struct Read {
    references: Vec<ImageReference>,
    /// Each reference's index, as its record kept it (in `references`'
    /// order): a pull's, never a tag's.
    index_digests: Vec<Option<String>>,
    pulled: Pulled,
    /// When a reference of it was last pointed at it (a pull, a tag): the
    /// latest its records say (F118).
    tagged: Option<std::time::SystemTime>,
}

impl Read {
    /// Its ID: its manifest's digest.
    fn id(&self) -> String {
        self.pulled.manifest_digest.to_string()
    }

    fn repo_tags(&self) -> Vec<String> {
        self.references
            .iter()
            .filter(|reference| reference.tag.is_some())
            .map(ImageReference::familiar)
            .collect()
    }

    /// Each name `@` the digest its registry knows it by: the
    /// multi-platform index that name was pulled through, else the
    /// manifest (F118, Docker's: a tag adds none of its own).
    fn repo_digests(&self) -> Vec<String> {
        let digests: std::collections::BTreeSet<String> = self
            .references
            .iter()
            .zip(&self.index_digests)
            .map(|(reference, index)| {
                let digest = index.clone().unwrap_or_else(|| self.id());
                format!("{}@{digest}", reference.familiar_name())
            })
            .collect();
        digests.into_iter().collect()
    }

    /// Its layers' bytes, as pulled.
    fn size(&self) -> u64 {
        self.pulled
            .manifest
            .layers
            .iter()
            .fold(0, |total, layer| total.saturating_add(layer.size))
    }

    fn labels(&self) -> Option<BTreeMap<String, String>> {
        Some(self.pulled.config.config.labels.clone()).filter(|labels| !labels.is_empty())
    }
}

/// `GET /images/json`: every image in the store, one entry each (its
/// references together), by reference. Read under the store's shared lock,
/// so no removal takes an image's blobs half way; an image that can't be
/// read is left out (and logged), never the list.
///
/// # Errors
///
/// The store or its index can't be read.
pub fn list_images(engine: &Engine) -> Result<Vec<ImageSummary>> {
    let store = crate::image::open_store(engine)?;
    let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let index = Index::open(store.root())?;
    let mut images: BTreeMap<String, Vec<ListedImage>> = BTreeMap::new();
    for image in index.images() {
        images
            .entry(image.record.manifest.clone())
            .or_default()
            .push(image);
    }
    let mut summaries = Vec::new();
    for listed in images.into_values() {
        let read = match read(&store, &listed) {
            Ok(Some(read)) => read,
            Ok(None) => continue,
            Err(error) => {
                tracing::debug!("leaving an image out of the list: {error}");
                continue;
            }
        };
        let size = read.size();
        summaries.push(ImageSummary {
            id: read.id(),
            parent_id: String::new(),
            repo_tags: read.repo_tags(),
            repo_digests: read.repo_digests(),
            created: created_seconds(&read.pulled),
            size,
            shared_size: -1,
            virtual_size: size,
            labels: read.labels(),
            containers: -1,
        });
    }
    Ok(summaries)
}

/// `GET /images/{name}/json`: the image `name` names (a reference, or an
/// ID: [`crate::image::remove`]'s rule), with all its references.
///
/// # Errors
///
/// No such image ([`crate::ErrorKind::NotFound`]), an ambiguous ID
/// ([`crate::ErrorKind::InvalidRequest`]), or a store that can't be read.
pub fn inspect_image(engine: &Engine, name: &str) -> Result<ImageInspect> {
    let store = crate::image::open_store(engine)?;
    let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let index = Index::open(store.root())?;
    let (found, _) = crate::image::named(&index, name)?;
    let Some(manifest) = found.first().map(|image| image.record.manifest.clone()) else {
        crate::error::bail!(NotFound, "no such image: {name}");
    };
    let all: Vec<ListedImage> = index
        .images()
        .into_iter()
        .filter(|image| image.record.manifest == manifest)
        .collect();
    let Some(read) = read(&store, &all)? else {
        crate::error::bail!(NotFound, "no such image: {name} (its blobs are gone)");
    };
    Ok(inspected(&read))
}

/// What `GET /images/{name}/json` says of `read`.
fn inspected(read: &Read) -> ImageInspect {
    let config = &read.pulled.config;
    let runtime = &config.config;
    let size = read.size();
    let set = |list: &[String]| Some(list.to_vec()).filter(|list| !list.is_empty());
    let keyed = |keys: &[String]| {
        Some(
            keys.iter()
                .map(|key| (key.clone(), serde_json::Map::new()))
                .collect::<BTreeMap<_, _>>(),
        )
        .filter(|keyed| !keyed.is_empty())
    };
    ImageInspect {
        id: read.id(),
        repo_tags: read.repo_tags(),
        repo_digests: read.repo_digests(),
        parent: String::new(),
        comment: String::new(),
        created: config.about.created.clone().unwrap_or_default(),
        container: String::new(),
        container_config: None,
        docker_version: config.about.docker_version.clone().unwrap_or_default(),
        author: config.about.author.clone().unwrap_or_default(),
        config: ImageConfig {
            hostname: String::new(),
            domainname: String::new(),
            user: runtime.user.clone().unwrap_or_default(),
            attach_stdin: false,
            attach_stdout: false,
            attach_stderr: false,
            exposed_ports: keyed(&runtime.exposed_ports),
            tty: false,
            open_stdin: false,
            stdin_once: false,
            env: runtime.env.clone(),
            cmd: set(&runtime.cmd),
            healthcheck: runtime.healthcheck.as_ref().map(|check| HealthConfig {
                test: check.test.clone(),
                interval: nanos(check.interval),
                timeout: nanos(check.timeout),
                start_period: nanos(check.start_period),
                start_interval: nanos(check.start_interval),
                retries: check.retries,
            }),
            image: String::new(),
            volumes: keyed(&runtime.volumes),
            working_dir: runtime.working_dir.clone().unwrap_or_default(),
            entrypoint: set(&runtime.entrypoint),
            on_build: None,
            labels: read.labels(),
            stop_signal: runtime.stop_signal.clone(),
            shell: set(&runtime.shell),
        },
        architecture: config.architecture.clone(),
        variant: config.about.variant.clone(),
        os: config.os.clone(),
        size,
        virtual_size: size,
        graph_driver: GraphDriver {
            name: "overlayfs".to_owned(),
            data: None,
        },
        root_fs: RootFs {
            kind: "layers".to_owned(),
            layers: config.diff_ids.iter().map(ToString::to_string).collect(),
        },
        metadata: Metadata {
            last_tag_time: read
                .tagged
                .map_or_else(|| ZERO_TIME.to_owned(), super::time),
        },
    }
}

/// `listed` (one image's references) read from the store: `None` if its
/// blobs can't be found (an image the index lists but the store lost).
///
/// # Errors
///
/// A reference the index holds that isn't one, or a store that can't be
/// read.
fn read(store: &shipyard_image::Store, listed: &[ListedImage]) -> Result<Option<Read>> {
    let mut references = Vec::new();
    let index_digests = listed
        .iter()
        .map(|image| image.record.index_digest.clone())
        .collect();
    for image in listed {
        references.push(
            ImageReference::parse(&image.reference)
                .with_context(|| format!("the image store's reference {}", image.reference))?,
        );
    }
    let Some(first) = references.first() else {
        return Ok(None);
    };
    let pulled = shipyard_image::from_store(first, &Platform::host(), store)
        .with_context(|| format!("reading {first} from the image store"))?;
    let tagged = listed
        .iter()
        .filter_map(|image| image.record.tagged_at)
        .max();
    Ok(pulled.map(|pulled| Read {
        references,
        index_digests,
        pulled,
        tagged,
    }))
}

/// When `pulled` was built, in Unix seconds (0 if it doesn't say, or says
/// something that isn't a time).
fn created_seconds(pulled: &Pulled) -> i64 {
    pulled
        .config
        .about
        .created
        .as_deref()
        .and_then(super::parse_time)
        .map_or(0, shipyard_runtime::logs::time::Timestamp::secs)
}

/// A duration in nanoseconds, as Docker's configs count them.
fn nanos(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// `GET /images/json` and `images`: the images `filters` pass, newest
/// first (Docker's order).
///
/// # Errors
///
/// As [`list_images`].
pub fn list_images_where(engine: &Engine, filters: &ImageFilters) -> Result<Vec<ImageSummary>> {
    let mut images = list_images(engine)?;
    images.retain(|image| filters.pass(image));
    images.sort_by(|a, b| {
        b.created
            .cmp(&a.created)
            .then_with(|| a.repo_tags.cmp(&b.repo_tags))
    });
    Ok(images)
}

#[cfg(test)]
mod tests;
