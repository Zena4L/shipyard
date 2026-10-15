//! A copy's ends on this side, for `shipyard-runtime cp` (its containers by
//! runtime ID; `shipyard cp`'s are by name, through the engine's
//! `copy_by_name`): a tar stream on stdout (never a terminal) or stdin, or
//! a host path. The engine copies.

use std::io::{IsTerminal as _, Write as _};
use std::path::Path;

use shipyard_engine::cp::{CpOptions, CpReport, Transfer, copy_in, copy_out};

/// Does `transfer` (its container a runtime ID under `root`), as `options`
/// say.
///
/// # Errors
///
/// A tar archive asked for on a terminal; the copy's failure.
pub(crate) fn copy(
    root: &Path,
    transfer: Transfer,
    options: CpOptions,
) -> anyhow::Result<CpReport> {
    Ok(match transfer {
        Transfer::Out { id, path, to } => {
            // As tar does: an archive on a terminal is noise (and escape
            // sequences a container chose).
            if to.is_none() && std::io::stdout().is_terminal() {
                anyhow::bail!("refusing to write a tar archive to a terminal (redirect it)");
            }
            let mut stdout = std::io::stdout().lock();
            let report = copy_out(root, &id, &path, to.as_deref(), &mut stdout, options)?;
            stdout.flush()?;
            report
        }
        Transfer::In { from, id, path } => {
            let mut stdin = std::io::stdin().lock();
            copy_in(root, &id, from.as_deref(), &mut stdin, &path, options)?
        }
    })
}
