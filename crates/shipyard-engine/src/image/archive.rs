//! Image archives (step 12.2a): `save` and `load`, as Docker's (D12.2-2,
//! D12.2-3). The archive's own rules (its formats, what's checked of it)
//! are `shipyard_image::archive`'s; here, which images a name means, the
//! store's lock, the extraction and the records.
//!
//! A load is resolved by each image's own digest, never by its names: an
//! archive that names `alpine:latest` loads its image, never whatever the
//! store or a registry has under that name, and a blob the archive doesn't
//! carry fails the load (nothing is ever pulled for it).

use std::io::{Read, Write};

use shipyard_image::archive::{load, save};
use shipyard_image::index::Index;
use shipyard_image::{ImageRecord, ImageReference, LockMode, Platform, USE_WAIT};

use crate::Engine;
use crate::error::{Context as _, Result, bail};

/// What a `load` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// The names loaded, as Docker shows them (`alpine:3.20`).
    pub loaded: Vec<String>,
    /// Names that pointed at another image before, and that one's ID: a
    /// load moves a tag, as a pull does, and says so (a hostile archive
    /// can't retag `alpine:latest` unsaid).
    pub moved: Vec<(String, String)>,
    /// What was left out of the archive, each a sentence (an image with no
    /// name, one for another platform).
    pub left_out: Vec<String>,
    /// Each image loaded, its ID (its manifest's digest), in order.
    pub ids: Vec<String>,
}

impl Loaded {
    /// What a load says, a line each, as Docker's does: each tag it moved
    /// (unless `quiet`), what it left out, then each name loaded. The
    /// archive's words are in them: escape them before a terminal.
    #[must_use]
    pub fn lines(&self, quiet: bool) -> Vec<String> {
        let moved = self.moved.iter().filter(|_| !quiet).map(|(name, was)| {
            format!("The image {name} already exists, renaming the old one with ID {was} to empty string")
        });
        let loaded = self
            .loaded
            .iter()
            .map(|name| format!("Loaded image: {name}"));
        moved
            .chain(self.left_out.iter().cloned())
            .chain(loaded)
            .collect()
    }
}

/// Writes the images `names` name to `out`, as one archive. A name with a
/// tag or digest, or an image's ID, is that image (by ID: no names, an
/// untagged archive, as Docker's); a repository alone (`alpine`) is every
/// tag of it the store has, as Docker saves it. Every name is found before
/// anything is written.
///
/// # Errors
///
/// A name that names no image ([`ErrorKind::NotFound`](crate::ErrorKind::NotFound));
/// the store can't be read; `out` can't be written.
pub fn save(engine: &Engine, names: &[String], out: impl Write) -> Result<()> {
    let store = super::open_store(engine)?;
    let opened = {
        let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
        let images = to_save(&store, names)?;
        // Every blob opened under the lock; written after it's let go, so a
        // reader taking its time holds no `prune` or `rmi` up.
        save::open(&store, &images)?
    };
    opened
        .write(out)
        .with_context(|| format!("saving {}", names.join(", ")))
}

/// Whether [`save()`] would find every one of `names` (the API's check,
/// before its answer's headers): as `save` finds them.
///
/// # Errors
///
/// As [`save()`]'s, before it writes.
pub fn check_saved(engine: &Engine, names: &[String]) -> Result<()> {
    let store = super::open_store(engine)?;
    let _shared = store.lock(LockMode::Shared, USE_WAIT)?;
    to_save(&store, names).map(drop)
}

/// The images `names` save, each once with all its names: under the store's
/// lock, the caller's.
fn to_save(store: &shipyard_image::Store, names: &[String]) -> Result<Vec<save::ToSave>> {
    let index = Index::open(store.root())?;
    let mut images: Vec<save::ToSave> = Vec::new();
    for name in names {
        for (reference, manifest, through) in saved_as(&index, name)? {
            let pulled = stored_image(store, reference.as_ref(), &manifest)?;
            match images
                .iter_mut()
                .find(|image| image.manifest_digest == pulled.manifest_digest)
            {
                Some(image) => {
                    if let Some(reference) = reference.filter(|r| !image.names.contains(r)) {
                        image.names.push(reference);
                    }
                    if image.index_digest.is_none() {
                        image.index_digest = through;
                    }
                }
                None => images.push(save::ToSave {
                    names: reference.into_iter().collect(),
                    manifest_digest: pulled.manifest_digest,
                    index_digest: through,
                    manifest: pulled.manifest,
                }),
            }
        }
    }
    Ok(images)
}

/// One name an image is saved as (none for an ID), its manifest's digest,
/// and the index it came through, if one (kept with it: a name with that
/// index's digest loads back).
type SavedAs = (
    Option<ImageReference>,
    shipyard_image::Digest,
    Option<shipyard_image::Digest>,
);

/// What `name` saves: each name it means, as the index records them.
fn saved_as(index: &Index, name: &str) -> Result<Vec<SavedAs>> {
    let digest_of = |record: &ImageRecord| super::record_digests(record, name);
    // A repository alone: every tag of it.
    if repository_alone(name)
        && let Ok(asked) = ImageReference::parse(name)
    {
        let tags = super::tags_of_repository(index, &asked);
        if !tags.is_empty() {
            return tags
                .into_iter()
                .map(|(reference, record)| {
                    let (manifest, through) = digest_of(&record)?;
                    Ok((Some(reference), manifest, through))
                })
                .collect();
        }
    }
    let (found, by_id) = super::named(index, name)?;
    let Some(first) = found.first() else {
        bail!(NotFound, "no such image: {name}");
    };
    if by_id {
        let (manifest, through) = digest_of(&first.record)?;
        return Ok(vec![(None, manifest, through)]);
    }
    found
        .iter()
        .map(|image| {
            let (manifest, through) = digest_of(&image.record)?;
            Ok((
                ImageReference::parse(&image.reference).ok(),
                manifest,
                through,
            ))
        })
        .collect()
}

/// Whether `name` is a repository alone: no tag and no digest (`alpine`,
/// `ghcr.io/me/app`, `localhost:5000/app`: a port isn't a tag).
fn repository_alone(name: &str) -> bool {
    !name.contains('@') && !super::names_a_tag(name)
}

/// The image whose manifest is `manifest`, from the store's blobs by that
/// digest alone (never the index's records, whatever it's named).
pub(super) fn stored_image(
    store: &shipyard_image::Store,
    named: Option<&ImageReference>,
    manifest: &shipyard_image::Digest,
) -> Result<shipyard_image::Pulled> {
    // What it's called in the result only (an image saved by its ID has no
    // name): never looked up.
    let called = named.cloned().unwrap_or_else(|| ImageReference {
        registry: "docker.io".to_owned(),
        repository: "library/image".to_owned(),
        tag: None,
        digest: Some(manifest.clone()),
    });
    match shipyard_image::from_store_by_digest(&called, manifest, &Platform::host(), store)? {
        Some(pulled) => Ok(pulled),
        None => bail!(
            NotFound,
            "the image {manifest}'s blobs aren't all in the store: remove it and pull it again"
        ),
    }
}

/// Loads the archive `input`: read whole into a staging directory first,
/// with no lock held; then, under the store's shared lock, each image found
/// sound moved into the store, its layers extracted (Linux), and every name
/// recorded at once. What was left out (an untagged image, another
/// platform's) is said, never failing the rest.
///
/// # Errors
///
/// The archive is cut, malformed, past a bound, or holds a blob that isn't
/// its digest ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest));
/// none of its images can be loaded; the store can't be written.
pub fn load(engine: &Engine, input: impl Read) -> Result<Loaded> {
    load_as(engine, input, None)
}

/// Loads the archive `input` as [`load()`] does, but as one image named
/// `names` alone (a build's: the archive's own names are never taken, so a
/// build's output can't retag another image, step 12.3).
///
/// # Errors
///
/// As [`load()`]'s; an archive of more or fewer than one image
/// ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest)).
pub fn load_named(engine: &Engine, input: impl Read, names: &[ImageReference]) -> Result<Loaded> {
    load_as(engine, input, Some(names))
}

/// `images`, a build's output, as its one image named `names` alone: the
/// names the archive gave it, and what it left out, dropped.
///
/// # Errors
///
/// It holds more or fewer than one image
/// ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest)).
fn named_only(images: &mut load::Images, names: &[ImageReference]) -> Result<()> {
    let [found] = images.found.as_mut_slice() else {
        bail!(
            InvalidRequest,
            "the build's output holds {} images, not one",
            images.found.len()
        );
    };
    found.names = names.to_vec();
    images.refused.clear();
    Ok(())
}

/// [`load()`] and [`load_named`].
fn load_as(engine: &Engine, input: impl Read, names: Option<&[ImageReference]>) -> Result<Loaded> {
    let store = super::open_store(engine)?;
    let mut archive = load::read(input, &store)?;
    let mut images = archive.images(&Platform::host())?;
    if let Some(names) = names {
        named_only(&mut images, names)?;
    }
    let mut done = Loaded {
        left_out: images.refused.iter().map(ToString::to_string).collect(),
        ..Loaded::default()
    };
    if images.found.is_empty() {
        bail!(
            InvalidRequest,
            "the archive has no image to load: {}",
            done.left_out.join("; ")
        );
    }
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "macOS keeps the blobs: nothing to extract")
    )]
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    // What the names named before (none if the index is corrupt: as a
    // pull's, the load goes on, and `record_many` sets it aside, said).
    let before = Index::open(store.root()).ok();
    let before_of = |key: &str| before.as_ref().and_then(|index| index.get(key));
    // Each name's record, and the name as Docker shows it.
    let mut records: Vec<(String, ImageRecord)> = Vec::new();
    let mut shown: Vec<String> = Vec::new();
    let now = std::time::SystemTime::now();
    for found in &images.found {
        archive.promote(&store, &archive.blobs_of(found)?)?;
        // By the manifest the archive's reading chose (an index of Docker
        // 25's carries one platform's blobs: chosen again from the index, it
        // might name another), then the index it came through.
        let mut pulled = stored_image(&store, found.names.first(), &found.manifest_digest)?;
        pulled.index_digest.clone_from(&found.index_digest);
        #[cfg(target_os = "linux")]
        {
            let layers = shipyard_image::layer_specs(&pulled.manifest, &pulled.config)
                .with_context(|| format!("reading the layers of {}", found.manifest_digest))?;
            super::extract(engine, &store, &shared, &layers, None)
                .with_context(|| format!("extracting the layers of {}", found.manifest_digest))?;
        }
        for name in &found.names {
            let key = name.to_string();
            let manifest = pulled.manifest_digest.to_string();
            if !done.ids.contains(&manifest) {
                done.ids.push(manifest.clone());
            }
            if let Some(at) = records.iter().position(|(had, _)| *had == key) {
                done.left_out.push(format!(
                    "{} names two images in the archive: it names the last one",
                    name.familiar()
                ));
                records.remove(at);
                shown.remove(at);
            }
            let kept = before_of(&key).filter(|had| had.manifest == manifest);
            let tagged_at = kept.and_then(|had| had.tagged_at).unwrap_or(now);
            records.push((key, ImageRecord::of(&pulled, Some(tagged_at))));
            shown.push(name.familiar());
        }
    }
    // Said from what's recorded: each name once, a moved one against the
    // image it named before.
    for ((key, record), name) in records.iter().zip(&shown) {
        if let Some(had) = before_of(key).filter(|had| had.manifest != record.manifest) {
            done.moved.push((name.clone(), had.manifest.clone()));
        }
        done.loaded.push(name.clone());
    }
    if let Some(aside) = Index::record_many(store.root(), &records)? {
        super::warn_index_reset(engine, &aside);
    }
    for name in &done.loaded {
        crate::events::emit_about(engine, crate::events::Kind::Image, "load", name);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build's output loads under the build's names alone: the names its
    /// archive claims (a tampered one's `postgres:16`) never taken; an
    /// archive of two images, or none, refused.
    #[test]
    fn a_build_s_output_takes_its_names_alone() {
        let image = |name: &str| load::Found {
            names: vec![ImageReference::parse(name).unwrap()],
            manifest_digest: shipyard_image::Digest::sha256(name.as_bytes()),
            index_digest: None,
            blobs: Vec::new(),
        };
        let wanted = [ImageReference::parse("app:1").unwrap()];
        let mut one = load::Images {
            found: vec![image("postgres:16")],
            refused: vec![load::LeftOut::NotAName("x".to_owned())],
        };
        named_only(&mut one, &wanted).unwrap();
        assert_eq!(one.found[0].names, wanted);
        assert!(one.refused.is_empty());
        for found in [vec![image("a:1"), image("b:1")], Vec::new()] {
            let mut images = load::Images {
                found,
                refused: Vec::new(),
            };
            assert_eq!(
                named_only(&mut images, &wanted).map_err(|error| error.kind()),
                Err(crate::ErrorKind::InvalidRequest)
            );
        }
    }
}
