//! `--format`: Docker's Go templates over Docker's rows (step 11.3c).

pub(crate) mod rows;
mod template;

use std::io::Write as _;

pub(crate) use template::{Output, Row, objects, terminal_safe};

use crate::cli::ListArgs;

/// Prints `rows` as `list` asks: their IDs alone with `-q` (a format given
/// too is ignored, and Docker's warning says so), else as its `--format`,
/// or `default` (the list's own table) without one.
///
/// # Errors
///
/// A template that doesn't parse or can't run ([`template::FormatError`]); a failed
/// write.
pub(crate) fn print<R: Row>(list: &ListArgs, default: &str, rows: &[R]) -> anyhow::Result<()> {
    if list.quiet {
        if list
            .format
            .as_deref()
            .is_some_and(|format| !format.is_empty())
        {
            writeln!(
                std::io::stderr().lock(),
                "WARNING: Ignoring custom format, because both --format and --quiet are set."
            )?;
        }
        let mut out = std::io::stdout().lock();
        for row in rows {
            writeln!(out, "{}", crate::output::printable(row.id()))?;
        }
        return Ok(());
    }
    let output = Output::parse(list.format.as_deref(), default)?;
    let mut out = std::io::stdout().lock();
    output.write(&mut out, rows)?;
    Ok(())
}
