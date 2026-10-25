//! Docker's prunes of what nothing uses (step 11.3e1): stopped containers
//! ([`containers`]), unused images ([`images`]), and all of them at once
//! with the unused networks and volumes ([`system`]). Each picks from a
//! listing, then removes through the engine's own removal, which decides
//! again under its lock: a container started meanwhile, or an image a
//! container was made from meanwhile, is kept.

use std::collections::BTreeSet;

use crate::Engine;
use crate::containers::ContainerId;
use crate::docker::filters::{ContainerPruneFilters, Given, ImagePruneFilters};
use crate::error::{Error, ErrorKind, Result};

/// The states of a container `container prune` removes (Docker's: never
/// one running, paused, restarting or being removed).
const STOPPED: [&str; 2] = ["created", "exited"];

/// What [`containers`] did.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ContainersPruned {
    /// The containers removed.
    pub removed: Vec<ContainerId>,
    /// The bytes their writable layers held (what could be read of them).
    pub bytes: u64,
    /// What couldn't be removed, and why.
    pub failed: Vec<Error>,
}

/// What [`images`] did.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ImagesPruned {
    /// The references untagged.
    pub untagged: Vec<String>,
    /// The blobs deleted (images' manifests and configs, layers), sorted.
    pub deleted: Vec<String>,
    /// The store's bytes freed.
    pub bytes: u64,
}

/// What `system prune` asks: every unused image, not only dangling ones
/// (`-a`); the unused anonymous volumes too (`--volumes`); Docker's
/// filters (`until`, `label`, `label!`).
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct SystemPrune {
    /// Every unused image (`-a`).
    pub all: bool,
    /// The unused anonymous volumes too.
    pub volumes: bool,
    /// The filters asked for.
    pub filters: Given,
}

impl SystemPrune {
    /// A prune of `filters`, with every unused image if `all`, and the
    /// unused anonymous volumes if `volumes`.
    #[must_use]
    pub fn new(all: bool, volumes: bool, filters: Given) -> Self {
        Self {
            all,
            volumes,
            filters,
        }
    }
    /// Its filters, as the containers' and the images' prunes read them
    /// (an `until` duration counted back from `now`): checked before it's
    /// asked, so a filter that isn't read prunes nothing.
    ///
    /// # Errors
    ///
    /// A filter that isn't read, or `until` with the volumes
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn filters(&self, now: i64) -> Result<(ContainerPruneFilters, ImagePruneFilters)> {
        let containers = ContainerPruneFilters::parse(self.filters.clone(), now)?;
        if self.volumes && containers.has_until() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "the until filter isn't taken with --volumes (a volume's age isn't kept)",
            ));
        }
        let mut images = ImagePruneFilters::parse(self.filters.clone(), now)?;
        images.dangling = !self.all;
        Ok((containers, images))
    }
}

/// What [`system`] did, each part's own report; a part that couldn't run
/// at all, its error.
#[derive(Debug)]
#[non_exhaustive]
pub struct SystemPruned {
    /// The containers.
    pub containers: Result<ContainersPruned>,
    /// The networks (none where there are no networks of Shipyard's:
    /// rootless, or not Linux); as [`volumes`](Self::volumes), a part that
    /// may not be one.
    pub networks: Option<Result<crate::network::Pruned>>,
    /// The volumes, if asked for.
    pub volumes: Option<Result<crate::volumes::PruneReport>>,
    /// The build cache (step 12.3): pruned before the images, so nothing
    /// a prune needs is gone (and a prune never pulls).
    pub build_cache: Result<crate::build::cache::Pruned>,
    /// The images.
    pub images: Result<ImagesPruned>,
}

/// `container prune`: removes every stopped container `filters` takes
/// (created, exited or dead), measuring each one's writable layer first;
/// one started meanwhile (refused under its lock) is kept, not a failure.
///
/// # Errors
///
/// The containers can't be listed.
pub fn containers(engine: &Engine, filters: &ContainerPruneFilters) -> Result<ContainersPruned> {
    let mut pruned = ContainersPruned::default();
    // Once, not before each removal (each would read every record).
    crate::run::settle_for_prune(engine);
    for summary in crate::docker::list_containers(engine, true)? {
        if !STOPPED.contains(&summary.state) || !filters.wants(&summary) {
            continue;
        }
        // A build's builder is its build's (made, then started; ended, then
        // read): a killed build's goes with its cache (`builder prune`).
        if summary
            .labels
            .contains_key(crate::build::cache::BUILDER_LABEL)
        {
            continue;
        }
        let bytes = crate::image::owned_area_bytes(engine, &summary.id);
        match crate::run::remove_for_prune(engine, &summary.id) {
            Ok(id) => {
                pruned.removed.push(id);
                pruned.bytes = pruned.bytes.saturating_add(bytes);
            }
            // Started (or about to restart), or removed, since it was
            // listed: not this prune's. Busy (its clean-up, or the store,
            // held for long) is said.
            Err(error) if matches!(error.kind(), ErrorKind::InUse | ErrorKind::NotFound) => {}
            Err(error) => pruned.failed.push(error),
        }
    }
    crate::events::emit_reclaimed(
        engine,
        crate::events::Kind::Container,
        pruned.removed.len(),
        pruned.bytes,
    );
    Ok(pruned)
}

/// `image prune`: removes the images `filters` takes that nothing uses
/// (no container made from it, no run: decided again under the store's
/// lock), dangling ones only unless `filters.dangling` is off (`-a`), then
/// what nothing needs in the store, as `image prune` always did.
///
/// # Errors
///
/// The images can't be listed, or the store can't be updated.
pub fn images(engine: &Engine, filters: &ImagePruneFilters) -> Result<ImagesPruned> {
    // No store: nothing to prune (and a prune never makes one).
    if !engine
        .paths()
        .image_store()
        .is_ok_and(std::path::Path::exists)
    {
        return Ok(ImagesPruned::default());
    }
    let before = crate::image::disk_usage(engine)?.images.bytes;
    let wanted: BTreeSet<String> = crate::docker::list_images(engine)?
        .into_iter()
        .filter(|image| filters.wants(image))
        .map(|image| image.id)
        .collect();
    let (untagged, mut deleted) = if wanted.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        crate::image::remove_unused(engine, &wanted)?
    };
    deleted.extend(crate::image::collect_unreferenced(engine)?);
    deleted.sort();
    deleted.dedup();
    // What it freed, as near as the store's size says (a pull meanwhile
    // makes it less); unmeasured, none, never a failure of what's done.
    let bytes = crate::image::disk_usage(engine)
        .map_or(0, |after| before.saturating_sub(after.images.bytes));
    crate::events::emit_reclaimed(engine, crate::events::Kind::Image, deleted.len(), bytes);
    Ok(ImagesPruned {
        untagged,
        deleted,
        bytes,
    })
}

/// `system prune`: the stopped containers, then the unused networks, the
/// unused anonymous volumes if asked, then the unused images (dangling
/// ones, or every one with `all`), each with the filters (`until` for
/// containers, networks and images; the labels for all). A part that
/// fails is said in its place, and the next goes on.
///
/// # Errors
///
/// Filters that aren't read, or `until` with the volumes (Docker's rule:
/// a volume's age isn't kept) ([`ErrorKind::InvalidRequest`]): nothing is
/// pruned then.
pub fn system(engine: &Engine, asked: &SystemPrune, now: i64) -> Result<SystemPruned> {
    let (containers_filters, image_filters) = asked.filters(now)?;
    let containers = containers(engine, &containers_filters);
    let networks = crate::network::prune_where_any(engine, &|labels, created| {
        containers_filters.wants_made(created, labels)
    })
    .transpose();
    let volumes = asked.volumes.then(|| {
        crate::volumes::prune_where(engine, &|_, labels| {
            labels.contains_key(crate::docker::ANONYMOUS_VOLUME)
                && containers_filters.wants_labels(labels)
        })
    });
    // A prune with filters leaves the build cache (a context's cache has
    // no labels or time a filter could read: never wiped for one).
    let build_cache = if asked.filters.is_empty() {
        crate::build::cache::prune(engine)
    } else {
        Ok(crate::build::cache::Pruned::default())
    };
    let images = images(engine, &image_filters);
    Ok(SystemPruned {
        containers,
        networks,
        volumes,
        build_cache,
        images,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `system prune`'s filters: both kinds' from one given, `-a` every
    /// unused image; `until` refused with the volumes, a key not read
    /// refused.
    #[test]
    fn system_prune_s_filters_are_docker_s() {
        let given = |pairs: &[(&str, &str)]| {
            Given::from_pairs(
                pairs
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
            )
        };
        let (_, images) = SystemPrune::new(false, false, given(&[]))
            .filters(0)
            .unwrap();
        assert!(images.dangling);
        let (containers, images) = SystemPrune::new(true, false, given(&[("until", "1h")]))
            .filters(10_000)
            .unwrap();
        assert!(!images.dangling, "-a: every unused one");
        assert!(containers.has_until());
        for refused in [
            SystemPrune::new(false, true, given(&[("until", "1h")])),
            SystemPrune::new(false, false, given(&[("dangling", "true")])),
        ] {
            assert_eq!(
                refused.filters(0).unwrap_err().kind(),
                ErrorKind::InvalidRequest,
                "{refused:?}"
            );
        }
        assert!(
            SystemPrune::new(false, true, given(&[("label", "a")]))
                .filters(0)
                .is_ok(),
            "labels with the volumes"
        );
    }
}
