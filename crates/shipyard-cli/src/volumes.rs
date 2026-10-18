//! `shipyard volume …`.

use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;

use anyhow::Result;
use shipyard_runtime::process::EXIT_ENGINE_ERROR;

use crate::cli::VolumeCommand;
use crate::output::{confirm, each, table};
use crate::reporter::{engine, report};

/// `shipyard volume …`: create, list, inspect, remove, prune named volumes.
pub(crate) fn command(command: &VolumeCommand) -> Result<ExitCode> {
    use shipyard_engine::volumes;
    let engine = engine();
    match command {
        VolumeCommand::Create { name } => {
            let volume = volumes::create(&engine, name)?;
            writeln!(std::io::stdout().lock(), "{}", volume.name)?;
        }
        VolumeCommand::Ls => {
            let rows: Vec<Vec<String>> = volumes::list(&engine)?
                .into_iter()
                .map(|volume| vec![volume.name, volume.data.display().to_string()])
                .collect();
            table(
                &mut std::io::stdout().lock(),
                &["VOLUME", "MOUNTPOINT"],
                &rows,
            )?;
        }
        VolumeCommand::Inspect { name } => {
            let volume = volumes::inspect(&engine, name)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{:<12} {}", "Name:", volume.name)?;
            writeln!(out, "{:<12} {}", "Mountpoint:", volume.data.display())?;
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
        code = report(err, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}
