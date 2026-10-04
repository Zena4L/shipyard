use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use shipyard_runtime::Backend;
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Doctor => doctor(),
    }
}

fn doctor() -> Result<()> {
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
    Ok(())
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
