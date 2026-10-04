use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use shipyard_runtime::Backend;
use shipyard_runtime::process::{self, EXIT_ENGINE_ERROR};
use tracing_subscriber::EnvFilter;

/// A lightweight container engine.
#[derive(Debug, Parser)]
#[command(name = "shipyard", version, about, propagate_version = true)]
struct Cli {
    /// Increase log verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show how this host will run containers.
    Doctor,
}

fn main() -> ExitCode {
    // Security check first: a setuid binary must not do anything at all.
    if let Err(err) = process::refuse_setid_execution() {
        return fail(&err.into());
    }
    // Before anything else opens a file; see `ensure_std_fds_open`.
    if let Err(err) = process::ensure_std_fds_open() {
        return fail(&err.into());
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // `--help` and `--version` are not failures. Usage errors exit 125
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::from(EXIT_ENGINE_ERROR)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Doctor => doctor(),
    };
    result.unwrap_or_else(|err| fail(&err))
}

/// Reports an error from shipyard itself, with its full cause chain
/// ("doing X: because Y"), and returns exit code 125.
fn fail(err: &anyhow::Error) -> ExitCode {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(std::io::stderr().lock(), "shipyard: {err:#}");
    ExitCode::from(EXIT_ENGINE_ERROR)
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

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    // `RUST_LOG` overrides the -v flags when set.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
