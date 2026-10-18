//! `shipyard-runtime`: the OCI runtime command line, as runc offers it.
//!
//! A container's life in separate calls: `create` sets it up from a bundle
//! and holds it before its program runs; `start` lets it run; `state`
//! reports it as JSON; `kill` signals it; `delete` removes it. Its cgroup
//! gives the rest: `pause` and `resume`, `update`, `ps`, `events`. Engines
//! and the OCI conformance suite drive a runtime this way. The work is in
//! `shipyard_runtime::container`.

mod args;
mod cgroup_commands;
mod commands;
mod console;
mod cp_output;
mod report;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser as _;
use shipyard_runtime::container;
use shipyard_runtime::process;

use crate::args::{Cli, Command};
use crate::commands::{OWN_FAILURE, attach, cp, create, exec, logs};
use crate::report::{Level, report};

fn main() -> ExitCode {
    // Before anything else: this binary is also the containers' init and
    // monitor, and cp's helper, started again with hidden arguments.
    process::init_entry_point();
    container::monitor_entry_point();
    shipyard_engine::cp::helper_entry_point();
    // Then: a privileged start (setuid, file capabilities) must not get any
    // further. (The entry points above check for themselves.)
    if let Err(err) = process::refuse_privileged_start() {
        let _ = writeln!(std::io::stderr().lock(), "shipyard-runtime: {err}");
        return ExitCode::FAILURE;
    }
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(&cli, Level::Error, &format!("{err:#}"));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    // (`logs --log-dir` reads a directory, and needs no state root.)
    let root = &match (&cli.root, &cli.command) {
        (Some(root), _) => root.clone(),
        (None, Command::Logs(args)) if args.log_dir.is_some() => PathBuf::new(),
        // exec's and attach's own failures are 255 (runc's), this one too.
        (None, Command::Exec(_) | Command::Attach(_)) => {
            container::default_root().unwrap_or_else(|err| {
                report(cli, Level::Error, &err.to_string());
                std::process::exit(OWN_FAILURE);
            })
        }
        (None, _) => container::default_root()?,
    };
    match &cli.command {
        Command::Create(args) => create(cli, root, args)?,
        Command::Logs(args) => logs(cli, root, args)?,
        Command::Attach(args) => {
            // Its own failures are 255, as exec's: any other code is the
            // container's.
            if let Err(err) = attach(root, args) {
                report(cli, Level::Error, &format!("{err:#}"));
                std::process::exit(OWN_FAILURE);
            }
        }
        Command::Exec(args) => exec(cli, root, args),
        Command::Cp(args) => cp(cli, root, args)?,
        Command::Start { id } => {
            for warning in container::start(root, id)? {
                report(cli, Level::Warning, &warning);
            }
        }
        Command::State { id } => {
            let state = container::state(root, id)?;
            let json = serde_json::to_string_pretty(&state)?;
            writeln!(std::io::stdout().lock(), "{json}")?;
        }
        Command::Kill {
            all: false,
            id,
            signal,
        } => container::kill(root, id, *signal)?,
        Command::Kill {
            all: true,
            id,
            signal,
        } => container::kill_all(root, id, *signal)?,
        Command::Pause { id } => container::pause(root, id)?,
        Command::Resume { id } => container::resume(root, id)?,
        Command::Ps { format, id } => cgroup_commands::ps(root, id, *format)?,
        Command::Events {
            stats,
            interval,
            id,
        } => cgroup_commands::events(root, id, *stats, *interval)?,
        Command::Update {
            resources,
            limits,
            id,
        } => {
            let skipped = cgroup_commands::update(root, id, resources.as_deref(), limits)?;
            for warning in skipped {
                report(cli, Level::Warning, &warning);
            }
        }
        Command::Delete { force, id } => {
            for warning in container::delete(root, id, *force)? {
                report(cli, Level::Warning, &warning);
            }
        }
    }
    Ok(())
}
