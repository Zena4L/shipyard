//! `shipyard images`, `pull`, `rmi`, `tag`, `save`, `load`, `history` and
//! `image inspect` (`image prune` is `prune.rs`'s).

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;

use crate::format::rows::{self, HistoryRow, ImageRow};
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
pub(crate) fn pull(
    reference: &str,
    quiet: bool,
    handed: Option<shipyard_engine::image::GivenCredential>,
) -> Result<ExitCode> {
    // A Mac's credential, if one was handed to its forwarded pull (step
    // 12.4); else Docker's config.
    let engine = engine_with(CliReporter { progress: !quiet });
    let digest = shipyard_engine::image::pull_with(&engine, reference, handed)?.digest;
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

/// `shipyard save [-o FILE] IMAGE...`: the images as one tar archive, to
/// FILE or standard output (never a terminal, as `cp`'s archive: one there
/// is noise, and its bytes could act on it).
pub(crate) fn save(args: &crate::cli::SaveArgs) -> Result<ExitCode> {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    use std::io::IsTerminal as _;
    // Each blob is held open until it's written: the soft descriptor limit
    // raised as far as the hard one lets it (a refusal leaves it, and the
    // save says if it's too low).
    if let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE)
        && soft < hard
    {
        let _ = setrlimit(Resource::RLIMIT_NOFILE, hard, hard);
    }
    let engine = engine();
    if let Some(path) = &args.output {
        crate::output::write_archive_to(path, |out| {
            Ok(shipyard_engine::image::save(&engine, &args.images, out)?)
        })?;
    } else {
        if std::io::stdout().is_terminal() {
            return Err(crate::output::tar_to_terminal_refused(
                crate::output::GIVE_OUTPUT,
            ));
        }
        shipyard_engine::image::save(&engine, &args.images, std::io::stdout().lock())?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard load [-i FILE] [-q]`: loads the archive from FILE or standard
/// input (never a terminal: an archive can't be typed), then says each name
/// loaded, a tag it moved and what it left out (unless `-q`), each escaped:
/// an archive's names and words are anyone's.
pub(crate) fn load(args: &crate::cli::LoadArgs) -> Result<ExitCode> {
    use anyhow::Context as _;
    use std::io::IsTerminal as _;
    let engine = engine();
    let loaded = if let Some(path) = &args.input {
        let file =
            std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
        shipyard_engine::image::load(&engine, std::io::BufReader::new(file))?
    } else {
        if std::io::stdin().is_terminal() {
            return Err(crate::output::tar_from_terminal_refused(
                crate::output::GIVE_INPUT,
            ));
        }
        shipyard_engine::image::load(&engine, std::io::stdin().lock())?
    };
    let mut out = std::io::stdout().lock();
    for line in loaded.lines(args.quiet) {
        writeln!(out, "{}", crate::output::printable(&line))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard history IMAGE`: how it was built, newest step first, as
/// Docker's table (or `--format`, or the IDs alone with `-q`).
pub(crate) fn history(args: &crate::cli::HistoryArgs) -> Result<ExitCode> {
    let items = shipyard_engine::docker::image_history(&engine(), &args.image)?;
    let now = std::time::SystemTime::now();
    let rows: Vec<HistoryRow> = items
        .iter()
        .map(|item| HistoryRow::of(item, now, args.no_trunc, args.human))
        .collect();
    let list = crate::cli::ListArgs {
        quiet: args.quiet,
        format: args.format.clone(),
        filter: Vec::new(),
    };
    crate::format::print(&list, rows::HISTORY_TABLE, &rows)?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard import FILE|- NAME`: the archive made a one-layer image named
/// NAME, its ID printed (Docker's); a name it moved from another image said
/// on stderr. A URL isn't fetched, a terminal isn't read, another
/// platform isn't made.
pub(crate) fn import(args: &crate::cli::ImportArgs) -> Result<ExitCode> {
    use anyhow::Context as _;
    use std::io::IsTerminal as _;
    if args.source_is_url() {
        anyhow::bail!("importing from a URL isn't supported: download it, then import the file");
    }
    let request = shipyard_engine::image::ImportRequest {
        name: args.name.clone(),
        changes: args.changes.clone(),
        message: args.message.clone(),
        platform: args.platform.clone(),
    };
    let engine = engine();
    let imported = if args.reads_stdin() {
        if std::io::stdin().is_terminal() {
            return Err(crate::output::tar_from_terminal_refused(
                crate::output::GIVE_FILE,
            ));
        }
        shipyard_engine::image::import(&engine, std::io::stdin().lock(), &request)?
    } else {
        let file = std::fs::File::open(&args.source)
            .with_context(|| format!("reading {}", args.source.display()))?;
        shipyard_engine::image::import(&engine, std::io::BufReader::new(file), &request)?
    };
    if let Some(was) = &imported.moved {
        crate::reporter::warn(format!(
            "{} named {was} before: it names this image now",
            imported.name
        ))?;
    }
    writeln!(std::io::stdout().lock(), "{}", imported.id)?;
    Ok(ExitCode::SUCCESS)
}
