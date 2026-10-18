//! `shipyard system recover`, `stop` and `df`.

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;
use shipyard_runtime::process::EXIT_ENGINE_ERROR;

use crate::output::{human_bytes, table};
use crate::reporter::{engine, report};

/// `shipyard system recover`: containers that ended unwatched settled,
/// those their restart policy brings back started, each one's name printed;
/// each that couldn't be started said, and the exit code 125.
pub(crate) fn recover() -> Result<ExitCode> {
    let booted = shipyard_engine::run::boot(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &booted.started {
        writeln!(out, "{name}")?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for (name, error) in booted.failed {
        let error = anyhow::Error::from(error).context(format!("container {name}"));
        code = report(&error, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}

/// `shipyard system stop`: every container that's up stopped for the
/// host's shutdown, each one's name printed; each that couldn't be said,
/// and the exit code 125.
pub(crate) fn stop() -> Result<ExitCode> {
    let stopped = shipyard_engine::run::stop_for_the_host(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &stopped.stopped {
        writeln!(out, "{name}")?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for (name, error) in stopped.failed {
        let error = anyhow::Error::from(error).context(format!("container {name}"));
        code = report(&error, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}

/// `shipyard system df`: shows what the images, the containers' writable
/// layers and the volumes take, and how much of it nothing uses.
pub(crate) fn df() -> Result<ExitCode> {
    let engine = engine();
    let store = shipyard_engine::image::disk_usage(&engine)?;
    let disk = &store.disk;
    let images = shipyard_engine::image::Usage {
        count: store.images,
        bytes: disk.blob_bytes + disk.layer_bytes,
        reclaimable: disk.blob_reclaimable + disk.layer_reclaimable,
        complete: disk.complete,
    };
    let rows: Vec<Vec<String>> = [
        ("Images", &images),
        ("Containers", &store.containers),
        ("Volumes", &shipyard_engine::volumes::usage(&engine)?),
    ]
    .into_iter()
    .map(|(kind, usage)| {
        // Directories it couldn't read (another user's): at least this.
        let at_least = if usage.complete { "" } else { "≥ " };
        vec![
            kind.to_owned(),
            usage.count.to_string(),
            format!("{at_least}{}", human_bytes(usage.bytes)),
            format!("{at_least}{}", human_bytes(usage.reclaimable)),
        ]
    })
    .collect();
    table(
        &mut std::io::stdout().lock(),
        &["TYPE", "COUNT", "SIZE", "RECLAIMABLE"],
        &rows,
    )?;
    Ok(ExitCode::SUCCESS)
}
