//! `shipyard volume …`.

use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;

use anyhow::Result;

use crate::cli::VolumeCommand;
use crate::format::rows::{self, VolumeRow};
use crate::output::{confirm, each};
use crate::reporter::{EXIT_FAILURE, engine, report};

/// `shipyard volume …`: create, list, inspect, remove, prune named volumes.
pub(crate) fn command(command: &VolumeCommand) -> Result<ExitCode> {
    use shipyard_engine::volumes;
    let engine = engine();
    match command {
        VolumeCommand::Create {
            labels,
            driver,
            options,
            name,
        } => {
            volumes::check_driver(Some(driver), !options.is_empty())?;
            let labels = labels.iter().cloned().collect();
            let volume = volumes::create_with(&engine, name.as_deref(), &labels)?;
            writeln!(std::io::stdout().lock(), "{}", volume.name)?;
        }
        VolumeCommand::Ls(args) => {
            use shipyard_engine::docker::filters::{Given, VolumeFilters};
            let filters =
                VolumeFilters::parse(Given::from_pairs(args.list.filter.iter().cloned()))?;
            let listed = shipyard_engine::docker::list_volumes_where(&engine, &filters)?;
            let rows: Vec<VolumeRow> = listed.volumes.iter().map(VolumeRow::from).collect();
            crate::format::print(&args.list, rows::VOLUMES_TABLE, &rows)?;
        }
        VolumeCommand::Inspect { names, shown } => {
            return crate::output::shown(names, shown.format.as_deref(), |name| {
                Ok(serde_json::to_value(
                    shipyard_engine::docker::inspect_volume(&engine, name)?,
                )?)
            });
        }
        VolumeCommand::Rm { names } => {
            return Ok(each(names, |name| {
                volumes::remove(&engine, name)?;
                writeln!(std::io::stdout().lock(), "{name}")?;
                Ok(())
            }));
        }
        VolumeCommand::Prune { force } => return prune(*force),
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard volume prune`: asks first (on a terminal; elsewhere `-f` is
/// needed), then removes every volume no container uses, running or not.
fn prune(force: bool) -> Result<ExitCode> {
    const QUESTION: &str = "Remove every volume no container uses, and its data?";
    if !force {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            anyhow::bail!(
                "volume prune deletes data: run it on a terminal to be asked, or give -f"
            );
        }
        if !confirm(QUESTION, &mut stdin.lock(), &mut std::io::stderr().lock())? {
            writeln!(std::io::stderr().lock(), "Nothing removed")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let pruned = shipyard_engine::volumes::prune(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &pruned.removed {
        writeln!(out, "{name}")?;
    }
    writeln!(out, "Removed {} volume(s)", pruned.removed.len())?;
    if !pruned.in_use.is_empty() {
        writeln!(
            out,
            "Kept {} in use: {}",
            pruned.in_use.len(),
            pruned.in_use.join(", ")
        )?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for err in &pruned.failed {
        code = report(err, EXIT_FAILURE);
    }
    Ok(code)
}
