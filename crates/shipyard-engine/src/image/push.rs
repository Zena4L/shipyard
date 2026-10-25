//! `shipyard push` (step 12.4).
//!
//! A push sends an image's stored blobs and manifest as they are
//! (`shipyard_image::push`): each name's image is found, and its blobs
//! opened, under the store's shared lock, which is let go before anything
//! is sent (a prune meanwhile takes nothing the push reads). It signs in as
//! a pull does: the credential a client handed in, else Docker's config;
//! one that can't be read fails it.

use shipyard_image::push::{PushEvent, ToPush};
use shipyard_image::{ImageManifest, ImageReference, LockMode, USE_WAIT};

use super::{GivenCredential, open_store};
use crate::Engine;
use crate::error::{Context as _, Result, bail};
use crate::report::{Message, Progress};

/// What `push` is asked.
#[derive(Debug, Clone)]
pub struct PushRequest {
    /// The name: `NAME[:TAG]` (`:latest` if no tag), or a repository with
    /// `all_tags`.
    pub name: String,
    /// Every tag of the name's repository the store has (`-a`).
    pub all_tags: bool,
    /// The credential a client handed in (the API's `X-Registry-Auth`, the
    /// Mac's own: D12.4-2); `None`: Docker's config.
    pub given: Option<GivenCredential>,
}

/// One name pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedName {
    /// The name, as its canonical reference.
    pub name: String,
    /// Its tag.
    pub tag: String,
    /// Its manifest's digest (the image's ID).
    pub digest: String,
    /// Its manifest's size.
    pub size: u64,
}

/// `shipyard push`: each name `request` gives, pushed in turn; a `push`
/// event for each.
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a name
/// with a digest; [`ErrorKind::NotFound`](crate::ErrorKind::NotFound): no
/// such image; [`ErrorKind::Image`](crate::ErrorKind::Image): the registry
/// failed, refused or denied it.
pub fn push(engine: &Engine, request: &PushRequest) -> Result<Vec<PushedName>> {
    let asked = ImageReference::parse(&request.name)?;
    if asked.digest.is_some() {
        bail!(
            InvalidRequest,
            "push takes a tag, not a digest: {}",
            request.name
        );
    }
    if request.all_tags && names_a_tag(&request.name) {
        bail!(
            InvalidRequest,
            "-a pushes every tag of a repository: give it without one ({})",
            request.name
        );
    }
    let names = names_to_push(engine, &asked, request.all_tags)?;
    let credentials = GivenCredential::or_docker(request.given.clone());
    let registry = shipyard_image::Registry::new(asked.host())
        .with_credentials(credentials, &asked.registry)
        .for_push();
    let mut pushed = Vec::new();
    for name in names {
        pushed.push(push_one(engine, &registry, &name)?);
    }
    Ok(pushed)
}

/// Whether `name` (as written) gives a tag: a `:` after its last `/`
/// (`registry:5000/app` gives none). A parsed reference can't say: it
/// fills in `latest`.
#[must_use]
pub fn names_a_tag(name: &str) -> bool {
    let last = name.rsplit('/').next().unwrap_or(name);
    !name.contains('@') && last.contains(':')
}

/// The names to push: `asked` (its tag, or `latest`), or every tagged name
/// of its repository the store has.
fn names_to_push(
    engine: &Engine,
    asked: &ImageReference,
    all_tags: bool,
) -> Result<Vec<ImageReference>> {
    if !all_tags {
        let mut one = asked.clone();
        one.tag.get_or_insert_with(|| "latest".to_owned());
        return Ok(vec![one]);
    }
    let store = open_store(engine)?;
    let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
    let index = shipyard_image::index::Index::open(store.root())?;
    let names: Vec<ImageReference> = super::tags_of_repository(&index, asked)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if names.is_empty() {
        bail!(NotFound, "no tag of {} is in the image store", asked);
    }
    Ok(names)
}

/// One name: its image found and opened under the store's lock, then sent.
fn push_one(
    engine: &Engine,
    registry: &shipyard_image::Registry,
    name: &ImageReference,
) -> Result<PushedName> {
    let shown = name.to_string();
    let tag = name.tag.clone().unwrap_or_else(|| "latest".to_owned());
    let opened = {
        let store = open_store(engine)?;
        let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
        let index = shipyard_image::index::Index::open(store.root())?;
        let Some(record) = index.get(&shown) else {
            bail!(NotFound, "{shown} isn't in the image store");
        };
        // A damaged index digest is said: never taken for none, which would
        // turn off the guard against flattening the index it names.
        let (manifest_digest, index_digest) = super::record_digests(record, &shown)?;
        let bytes = store
            .read_blob(&manifest_digest, shipyard_image::MANIFEST_SIZE_LIMIT)
            .with_context(|| format!("reading {shown}'s manifest"))?;
        let image = ToPush {
            manifest_digest,
            index_digest,
            manifest: ImageManifest::parse(&bytes)
                .map_err(|error| crate::Error::wrap(crate::ErrorKind::Storage, error))
                .with_context(|| format!("reading {shown}'s manifest"))?,
        };
        shipyard_image::push::open(&store, &image)
            .map_err(crate::Error::from)
            .with_context(|| format!("pushing {shown}"))?
    };
    engine.report(Message::Progress(Progress::Pushing {
        name: shown.clone(),
    }));
    let progress = |event: &PushEvent| {
        let (digest, sent) = match event {
            PushEvent::Pushed { digest, size } => (digest, Some(*size)),
            PushEvent::Exists { digest } => (digest, None),
        };
        engine.report(Message::Progress(Progress::Pushed {
            short_digest: digest.hex().chars().take(12).collect(),
            sent,
        }));
    };
    let done = shipyard_image::push::push(&opened, registry, &name.repository, &tag, &progress)
        .map_err(crate::Error::from)
        .with_context(|| format!("pushing {shown}"))?;
    crate::events::emit_about(engine, crate::events::Kind::Image, "push", &shown);
    Ok(PushedName {
        name: shown,
        tag,
        digest: done.digest.to_string(),
        size: done.size,
    })
}

impl From<shipyard_image::PushError> for crate::Error {
    fn from(error: shipyard_image::PushError) -> Self {
        use shipyard_image::{PushError, StoreError};
        let kind = match &error {
            PushError::Store(StoreError::Busy) => crate::ErrorKind::Busy,
            PushError::Missing(_) | PushError::Store(_) => crate::ErrorKind::Storage,
            PushError::WouldFlatten { .. } => crate::ErrorKind::InvalidRequest,
            _ => crate::ErrorKind::Image,
        };
        Self::wrap(kind, error)
    }
}

#[cfg(test)]
mod tests {
    /// A tag is read from the name as written: after its last `/` only (a
    /// registry's port isn't one), never in a digest.
    #[test]
    fn a_name_gives_a_tag_after_its_last_slash() {
        for (name, tagged) in [
            ("app", false),
            ("app:1", true),
            ("ghcr.io/me/app", false),
            ("127.0.0.1:5000/app", false),
            ("127.0.0.1:5000/app:2", true),
            ("app@sha256:abc", false),
        ] {
            assert_eq!(super::names_a_tag(name), tagged, "{name}");
        }
    }
}
