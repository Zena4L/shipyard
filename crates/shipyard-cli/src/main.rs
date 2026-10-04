//! The `shipyard` command-line interface.
//!
//! This crate only parses flags and reports results; the work happens in
//! `shipyard-runtime`.

mod run;

use std::io::{IsTerminal as _, Write};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use shipyard_runtime::Backend;
use shipyard_runtime::process::{self, EXIT_ENGINE_ERROR};
use tracing_subscriber::EnvFilter;

/// A lightweight container engine.
#[derive(Debug, Parser)]
#[command(name = "shipyard", version, propagate_version = true)]
struct Cli {
    /// Show debug logs (same as --log-level debug).
    #[arg(short = 'D', long, global = true)]
    debug: bool,

    /// How much to log.
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<LogLevel>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_filter(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a command the way a container would (no isolation yet).
    Run(run::RunArgs),
    /// Show how this host will run containers.
    Doctor,
}

fn main() -> ExitCode {
    // Before anything else: a privileged start must not get any further.
    if let Err(err) = process::refuse_privileged_start() {
        return fail(&err.into());
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // `--help` and `--version` are not failures. Usage errors exit 125
            // like Docker, so they can't be mistaken for the command's own code.
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::from(EXIT_ENGINE_ERROR)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    init_tracing(cli.log_level, cli.debug);

    let result = match cli.command {
        Command::Run(args) => run::run(args),
        Command::Doctor => doctor(),
    };
    result.unwrap_or_else(|err| fail(&err))
}

/// Reports an error from shipyard itself and returns exit code 125.
fn fail(err: &anyhow::Error) -> ExitCode {
    report(err, EXIT_ENGINE_ERROR)
}

/// Prints an error with its full cause chain ("doing X: because Y") and
/// returns `code` as the exit code.
fn report(err: &dyn std::fmt::Display, code: u8) -> ExitCode {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(std::io::stderr().lock(), "shipyard: {err:#}");
    ExitCode::from(code)
}

fn doctor() -> Result<ExitCode> {
    let backend = match shipyard_runtime::host_backend() {
        Backend::Native => "native (host kernel)",
        Backend::LinuxVm => "linux-vm (not implemented yet)",
        _ => "unknown",
    };
    // `writeln!` instead of `println!`: a closed stdout becomes an error, not a panic.
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "host:    {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(out, "backend: {backend}")?;
    Ok(ExitCode::SUCCESS)
}

/// Flags win over `RUST_LOG`, which wins over the default (`warn`).
fn init_tracing(log_level: Option<LogLevel>, debug: bool) {
    let flag = log_level.or(debug.then_some(LogLevel::Debug));
    let filter = match flag {
        Some(level) => EnvFilter::new(level.as_filter()),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
    };
    // Colour only on a terminal, and never with NO_COLOR set (no-color.org):
    // escape codes in a log file or CI output are just noise.
    let colour = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(colour)
        .init();
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use super::*;

    /// `docs/user/cli.md` is generated from the flag definitions above, so the
    /// published reference can never drift from the real CLI.
    #[test]
    fn cli_reference_is_up_to_date() {
        const HEADER: &str = "<!-- Generated from the CLI's flag definitions. Do not edit by hand.\n     \
             Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference -->\n\n";
        let options = clap_markdown::MarkdownOptions::new()
            .title("Command reference".to_owned())
            .show_footer(false)
            .show_table_of_contents(true);
        let generated = format!(
            "{HEADER}{}",
            clap_markdown::help_markdown_custom::<Cli>(&options)
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/user/cli.md");

        if std::env::var_os("UPDATE_DOCS").is_some() {
            std::fs::write(&path, &generated).expect("writing docs/user/cli.md");
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "docs/user/cli.md is out of date with the CLI. Regenerate it with:\n\n    \
             UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference\n"
        );
    }

    #[test]
    fn global_logging_flags() {
        let cli = Cli::try_parse_from(["shipyard", "-D", "doctor"]).unwrap();
        assert!(cli.debug);
        let cli = Cli::try_parse_from(["shipyard", "doctor", "--log-level", "trace"]).unwrap();
        assert_eq!(cli.log_level, Some(LogLevel::Trace));
        assert!(Cli::try_parse_from(["shipyard", "--log-level", "loud", "doctor"]).is_err());
    }

    #[test]
    fn dash_v_is_reserved_for_volumes() {
        assert!(Cli::try_parse_from(["shipyard", "-v", "doctor"]).is_err());
    }

    #[test]
    fn run_subcommand_parses() {
        let cli = Cli::try_parse_from(["shipyard", "run", "-D", "ls", "-l"]).unwrap();
        assert!(cli.debug, "-D before the command is shipyard's flag");
        assert!(matches!(cli.command, Command::Run(_)));
    }
}
