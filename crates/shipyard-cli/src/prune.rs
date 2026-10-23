//! `shipyard container prune`, `image prune` and `system prune` (step
//! 11.3e1): Docker's warnings and output over the engine's prunes.

use std::fmt::Write as _;
use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;

use anyhow::{Result, bail};
use shipyard_engine::docker::filters::{ContainerPruneFilters, Given, ImagePruneFilters};
use shipyard_engine::docker::unix_now;
use shipyard_engine::prune::{ContainersPruned, ImagesPruned, SystemPrune};

use crate::format::rows::human_size;
use crate::output::{confirm, printable};
use crate::reporter::{EXIT_FAILURE, engine, report};

/// A prune's arguments, Docker's.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct PruneArgs {
    /// Don't ask (needed where there's no terminal to ask on).
    #[arg(short, long)]
    pub(crate) force: bool,
    /// Only what matches: `until=TIME` (a time, or a duration such as
    /// `24h`: made before), `label=KEY[=VALUE]`, `label!=KEY[=VALUE]`;
    /// `image prune` takes `dangling=false` too (as -a).
    #[arg(long, value_name = "KEY=VALUE", value_parser = crate::cli::parse_filter)]
    pub(crate) filter: Vec<(String, String)>,
}

/// `shipyard image prune`'s arguments.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct ImagePruneArgs {
    /// Every image no container was made from, not only dangling ones.
    #[arg(short, long)]
    pub(crate) all: bool,
    #[command(flatten)]
    pub(crate) prune: PruneArgs,
}

/// `shipyard system prune`'s arguments.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct SystemPruneArgs {
    /// Every image no container was made from, not only dangling ones.
    #[arg(short, long)]
    pub(crate) all: bool,
    /// The anonymous volumes no container uses too.
    #[arg(long)]
    pub(crate) volumes: bool,
    #[command(flatten)]
    pub(crate) prune: PruneArgs,
}

/// `shipyard container prune`: every stopped container removed (after
/// Docker's question, unless `-f`), their IDs and the space reclaimed.
pub(crate) fn containers(args: &PruneArgs) -> Result<ExitCode> {
    let filters = ContainerPruneFilters::parse(given(args), unix_now())?;
    if !asked(args, "WARNING! This will remove all stopped containers.")? {
        return Ok(ExitCode::SUCCESS);
    }
    let pruned = shipyard_engine::prune::containers(&engine(), &filters)?;
    let mut out = std::io::stdout().lock();
    write_containers(&mut out, &pruned)?;
    writeln!(
        out,
        "Total reclaimed space: {}",
        human_size(pruned.bytes, 4)
    )?;
    drop(out);
    Ok(if said(&pruned.failed) {
        ExitCode::from(EXIT_FAILURE)
    } else {
        ExitCode::SUCCESS
    })
}

/// `shipyard image prune`: dangling images (every unused one with `-a`)
/// and what nothing needs in the store, removed (after Docker's question,
/// unless `-f`); what was untagged and deleted, and the space reclaimed.
pub(crate) fn images(args: &ImagePruneArgs) -> Result<ExitCode> {
    let mut filters = ImagePruneFilters::parse(given(&args.prune), unix_now())?;
    filters.dangling &= !args.all;
    let warning = if args.all {
        "WARNING! This will remove all images without at least one container associated to them."
    } else {
        "WARNING! This will remove all dangling images."
    };
    if !asked(&args.prune, warning)? {
        return Ok(ExitCode::SUCCESS);
    }
    let pruned = shipyard_engine::prune::images(&engine(), &filters)?;
    let mut out = std::io::stdout().lock();
    write_images(&mut out, &pruned)?;
    writeln!(
        out,
        "Total reclaimed space: {}",
        human_size(pruned.bytes, 4)
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard system prune`: stopped containers, unused networks, unused
/// anonymous volumes with `--volumes`, dangling images (every unused one
/// with `-a`), after Docker's list of what goes (unless `-f`); each
/// part's section, and the space reclaimed in all. A part that fails is
/// said, the rest go on, and it exits 1.
pub(crate) fn system(args: &SystemPruneArgs) -> Result<ExitCode> {
    let asked = SystemPrune::new(args.all, args.volumes, given(&args.prune));
    let mut warning = String::from(
        "WARNING! This will remove:\n  - all stopped containers\n  - all networks not used by \
         at least one container\n",
    );
    if args.volumes {
        warning.push_str("  - all anonymous volumes not used by at least one container\n");
    }
    warning.push_str(if args.all {
        "  - all images without at least one container associated to them\n"
    } else {
        "  - all dangling images\n"
    });
    if !args.prune.filter.is_empty() {
        warning.push_str("\n  Items to be pruned will be filtered with:\n");
        for (key, value) in &args.prune.filter {
            let _ = writeln!(warning, "  - {}", printable(&format!("{key}={value}")));
        }
    }
    // A filter that isn't read is refused before it's asked.
    asked.filters(unix_now())?;
    if !self::asked(&args.prune, warning.trim_end())? {
        return Ok(ExitCode::SUCCESS);
    }
    let pruned = shipyard_engine::prune::system(&engine(), &asked, unix_now())?;
    let mut failed = false;
    let mut bytes = 0_u64;
    let mut out = std::io::stdout().lock();
    match &pruned.containers {
        Ok(containers) => {
            write_containers(&mut out, containers)?;
            bytes = bytes.saturating_add(containers.bytes);
            failed |= said(&containers.failed);
        }
        Err(error) => failed |= fail(error),
    }
    match &pruned.networks {
        Ok(Some(networks)) => {
            section(&mut out, "Deleted Networks:", &networks.removed)?;
            failed |= said(&networks.failed);
        }
        Ok(None) => {}
        Err(error) => failed |= fail(error),
    }
    match &pruned.volumes {
        Some(Ok(volumes)) => {
            section(&mut out, "Deleted Volumes:", &volumes.removed)?;
            bytes = bytes.saturating_add(volumes.bytes);
            failed |= said(&volumes.failed);
        }
        Some(Err(error)) => failed |= fail(error),
        None => {}
    }
    match &pruned.images {
        Ok(images) => {
            write_images(&mut out, images)?;
            bytes = bytes.saturating_add(images.bytes);
        }
        Err(error) => failed |= fail(error),
    }
    writeln!(out, "Total reclaimed space: {}", human_size(bytes, 4))?;
    Ok(if failed {
        ExitCode::from(EXIT_FAILURE)
    } else {
        ExitCode::SUCCESS
    })
}

/// The filters `args` gives, as the engine reads them.
fn given(args: &PruneArgs) -> Given {
    Given::from_pairs(args.filter.iter().cloned())
}

/// Whether to go on: `-f`, or a yes to Docker's `warning` and question
/// on a terminal. Without one, and without `-f`, it's refused (Shipyard's
/// rule, as `volume prune`'s: Docker reads an answer from any input).
fn asked(args: &PruneArgs, warning: &str) -> Result<bool> {
    if args.force {
        return Ok(true);
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("a prune removes what it finds: run it on a terminal to be asked, or give -f");
    }
    let mut err = std::io::stderr().lock();
    writeln!(err, "{warning}")?;
    confirm(
        "Are you sure you want to continue?",
        &mut stdin.lock(),
        &mut err,
    )
}

/// `Deleted Containers:` and their IDs, then a blank line (none: nothing).
fn write_containers(out: &mut impl std::io::Write, pruned: &ContainersPruned) -> Result<()> {
    let ids: Vec<String> = pruned
        .removed
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    section(out, "Deleted Containers:", &ids)
}

/// `Deleted Images:`, each `untagged: REFERENCE` then `deleted: DIGEST`,
/// then a blank line (none: nothing).
fn write_images(out: &mut impl std::io::Write, pruned: &ImagesPruned) -> Result<()> {
    if pruned.untagged.is_empty() && pruned.deleted.is_empty() {
        return Ok(());
    }
    writeln!(out, "Deleted Images:")?;
    for reference in &pruned.untagged {
        writeln!(out, "untagged: {}", printable(reference))?;
    }
    for digest in &pruned.deleted {
        writeln!(out, "deleted: {digest}")?;
    }
    writeln!(out)?;
    Ok(())
}

/// A section of Docker's prune output: `title` and each of `names`, then
/// a blank line; nothing if there are none.
fn section(out: &mut impl std::io::Write, title: &str, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    writeln!(out, "{title}")?;
    for name in names {
        writeln!(out, "{}", printable(name))?;
    }
    writeln!(out)?;
    Ok(())
}

/// Says each of `failed`: whether there were any.
fn said(failed: &[shipyard_engine::Error]) -> bool {
    for error in failed {
        fail(error);
    }
    !failed.is_empty()
}

/// Says `error`: a failure, always.
fn fail(error: &impl std::fmt::Display) -> bool {
    report(error, EXIT_FAILURE);
    true
}
