//! A run's image made ready (Linux): pulled if need be, its layers
//! extracted and a writable area made (`prepare_image`; a container's own
//! area, `prepare_owned_image`), and what the image brings to the run
//! ([`ImageDefaults`]).

use std::path::PathBuf;

#[cfg(target_os = "linux")]
use super::plan::Planned;
#[cfg(target_os = "linux")]
use crate::Engine;
#[cfg(target_os = "linux")]
use crate::error::Result;
#[cfg(target_os = "linux")]
use crate::image::PullPolicy;

/// Prepares the request's image, if it has one (Linux only): pulls it,
/// extracts its layers and makes the run's writable area, returning the
/// defaults a run applies and the area (to keep until the run ends).
///
/// # Errors
///
/// If the image can't be found, pulled or extracted.
#[cfg(target_os = "linux")]
pub(crate) fn prepare_image(
    engine: &Engine,
    planned: &Planned,
) -> Result<Option<(ImageDefaults, crate::image::RunArea)>> {
    let Some(reference) = &planned.request.image else {
        return Ok(None);
    };
    // As root, `--uidmap`'s maps: the layers are shifted by them. (A
    // rootless caller's maps are its own range, which extraction uses.)
    let maps = planned
        .isolation
        .as_ref()
        .and_then(|isolation| isolation.id_maps.as_ref())
        .filter(|_| nix::unistd::geteuid().is_root());
    let prepared = crate::image::prepare(
        engine,
        reference,
        planned.request.pull.unwrap_or(PullPolicy::Missing),
        maps,
    )?;
    Ok(Some((
        ImageDefaults {
            root: prepared.root,
            overlay: prepared.overlay,
            config: prepared.config,
        },
        prepared.area,
    )))
}

/// Prepares a container's image to start ([`crate::image::prepare_owned`]):
/// by the digest it was made from (`image_id`), with the container's own
/// writable area (container `id`, of container store `owner`). Its
/// request's image is the reference it was made with.
///
/// # Errors
///
/// As [`crate::image::prepare_owned`].
#[cfg(target_os = "linux")]
pub(crate) fn prepare_owned_image(
    engine: &Engine,
    planned: &Planned,
    id: &str,
    owner: &str,
    image_id: &str,
) -> Result<Option<(ImageDefaults, crate::image::RunArea)>> {
    let Some(reference) = &planned.request.image else {
        return Ok(None);
    };
    let maps = planned
        .isolation
        .as_ref()
        .and_then(|isolation| isolation.id_maps.as_ref())
        .filter(|_| nix::unistd::geteuid().is_root());
    let prepared = crate::image::prepare_owned(engine, id, owner, reference, image_id, maps)?;
    Ok(Some((
        ImageDefaults {
            root: prepared.root,
            overlay: prepared.overlay,
            config: prepared.config,
        },
        prepared.area,
    )))
}

/// What an image contributes to a run: its root filesystem (an overlay the
/// runtime mounts) and its baked-in defaults, applied under what the request
/// says.
#[derive(Debug, Clone)]
pub struct ImageDefaults {
    /// The merged mount point — the container's root path.
    pub root: PathBuf,
    /// The overlay there (layer dirs + upper + work).
    pub overlay: shipyard_runtime::isolation::Overlay,
    /// The image's runtime config (default command, env, workdir, user).
    pub config: shipyard_image::RuntimeConfig,
}
