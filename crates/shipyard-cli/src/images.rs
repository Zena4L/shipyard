//! `shipyard images`, `pull`, `rmi` and `image prune`.

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;

use crate::output::{each, table};
use crate::reporter::{CliReporter, engine, engine_with};

/// `shipyard images`: lists the images in the store.
pub(crate) fn list() -> Result<ExitCode> {
    let mut images = shipyard_engine::image::list(&engine())?;
    images.sort_by(|a, b| a.reference.cmp(&b.reference));
    let rows: Vec<Vec<String>> = images
        .into_iter()
        .map(|image| {
            let manifest = &image.record.manifest;
            let short: String = manifest
                .strip_prefix("sha256:")
                .unwrap_or(manifest)
                .chars()
                .take(12)
                .collect();
            vec![
                image.reference,
                short,
                image.record.layers.len().to_string(),
            ]
        })
        .collect();
    table(
        &mut std::io::stdout().lock(),
        &["REFERENCE", "MANIFEST", "LAYERS"],
        &rows,
    )?;
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
        writeln!(out, "Untagged: {reference}")?;
        for digest in removed {
            writeln!(out, "Deleted: {digest}")?;
        }
        Ok(())
    })
}

/// `shipyard image prune`: removes unreferenced blobs and layers.
pub(crate) fn prune() -> Result<ExitCode> {
    let removed = shipyard_engine::image::prune(&engine())?;
    let mut out = std::io::stdout().lock();
    for digest in &removed {
        writeln!(out, "Deleted: {digest}")?;
    }
    writeln!(out, "Removed {} unreferenced blob(s)", removed.len())?;
    Ok(ExitCode::SUCCESS)
}
