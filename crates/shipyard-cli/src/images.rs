//! `shipyard images`, `pull`, `rmi`, `tag` and `image inspect` (`image
//! prune` is `prune.rs`'s).

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;

use crate::format::rows::{self, ImageRow};
use crate::output::each;
use crate::reporter::{CliReporter, engine, engine_with};

/// `shipyard images`: the images in the store, as Docker lists them:
/// newest first, a row per name, filtered (`-f`, or a `REPOSITORY[:TAG]`
/// given), printed as `--format` asks (Docker's table without it), or
/// their IDs alone (`-q`).
pub(crate) fn list(args: &crate::cli::ImagesArgs) -> Result<ExitCode> {
    use shipyard_engine::docker::filters::{Given, ImageFilters};
    let given = args.list.filter.iter().cloned().chain(
        args.reference
            .iter()
            .map(|reference| ("reference".to_owned(), reference.clone())),
    );
    let filters = ImageFilters::parse(Given::from_pairs(given))?;
    let images = shipyard_engine::docker::list_images_where(&engine(), &filters)?;
    let now = std::time::SystemTime::now();
    let rows: Vec<ImageRow> = images
        .iter()
        .flat_map(|image| ImageRow::rows(image, now, args.no_trunc))
        .collect();
    let default = if args.digests {
        rows::IMAGES_DIGESTS_TABLE
    } else {
        rows::IMAGES_TABLE
    };
    crate::format::print(&args.list, default, &rows)?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard pull IMAGE`: pulls it, then prints its digest.
pub(crate) fn pull(reference: &str, quiet: bool) -> Result<ExitCode> {
    let digest =
        shipyard_engine::image::pull(&engine_with(CliReporter { progress: !quiet }), reference)?;
    let mut out = std::io::stdout().lock();
    if quiet {
        writeln!(out, "{digest}")?;
    } else {
        writeln!(out, "Digest: {digest}")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard rmi IMAGE...`: removes images, collecting unshared blobs. One
/// that can't be removed is reported, and the rest still are.
pub(crate) fn rmi(references: &[String], force: bool) -> ExitCode {
    each(references, |reference| {
        let removed = shipyard_engine::image::remove(&engine(), reference, force)?;
        let mut out = std::io::stdout().lock();
        // A reference: as typed; an ID: each of its image's references.
        if removed.by_id {
            for untagged in &removed.untagged {
                writeln!(out, "Untagged: {untagged}")?;
            }
        } else {
            writeln!(out, "Untagged: {reference}")?;
        }
        for digest in removed.deleted {
            writeln!(out, "Deleted: {digest}")?;
        }
        Ok(())
    })
}

/// `shipyard tag SOURCE TARGET`: gives the image SOURCE names another
/// name, TARGET (Docker's `tag`: prints nothing).
pub(crate) fn tag(source: &str, target: &str) -> Result<ExitCode> {
    shipyard_engine::image::tag(&engine(), source, target)?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard image inspect IMAGE...`: each image in Docker's shape, as
/// `format` asks (one JSON array without it: Docker's `image inspect`);
/// one that can't be found is said, and the rest still shown.
pub(crate) fn inspect(images: &[String], format: Option<&str>) -> Result<ExitCode> {
    let engine = engine();
    crate::output::shown(images, format, |image| {
        Ok(serde_json::to_value(
            shipyard_engine::docker::inspect_image(&engine, image)?,
        )?)
    })
}
