//! The `shipyard` command-line interface.
//!
//! This crate only parses flags, calls the engine (`shipyard-engine`) and
//! prints what it returns and reports; the work happens there and in
//! `shipyard-runtime`.

mod cli;

/// This build's revision (`build.rs`: git's, `-dirty` for local changes),
/// to compare with the macOS guest's.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "a Mac's guest's to compare")
)]
const REVISION: &str = env!("SHIPYARD_REVISION");
mod containers;
mod devices;
mod doctor;
mod events_output;
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only a Mac forwards; its rules are tested everywhere"
    )
)]
mod forward;
mod images;
mod networks;
mod output;
mod reporter;
mod run;
mod system;
mod vm;
mod volumes;

use std::io::IsTerminal as _;
use std::os::fd::AsFd as _;
use std::process::ExitCode;

use clap::Parser;
use shipyard_runtime::process::{self, EXIT_ENGINE_ERROR};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{Format, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use crate::cli::{Cli, Command, ImageCommand, LogLevel, SystemCommand};

fn main() -> ExitCode {
    // First of all: every helper of Shipyard's (a container's init, its
    // monitor and exit command, the storage helper, a network's DNS server,
    // `doctor`'s probe, pasta's launcher…) is this binary, re-executed, and
    // takes over here, in the engine's one order (it never returns then).
    shipyard_engine::entry_points(shipyard_engine::Hooks {
        init_logging: || init_tracing(None, false),
        refuse_privileged_start: || {
            process::refuse_privileged_start().map_err(|error| error.to_string())
        },
    });
    // The macOS VM's host process (step 10.3): this binary too.
    shipyard_vm::host_entry_point();
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
    // On a Mac, every command but a few runs in Shipyard's VM (step 10.3b).
    #[cfg(target_os = "macos")]
    if let Some(code) = forward::elsewhere(&cli.command) {
        return code;
    }

    let result = match cli.command {
        Command::Run(command) => run::run_command(*command),
        Command::Create(command) => run::create(*command),
        Command::Start {
            attach: false,
            refs,
            ..
        } => Ok(containers::start(&refs.containers)),
        Command::Start {
            attach: true,
            interactive,
            detach_keys,
            refs,
        } => containers::start_attached(&refs.containers, &detach_keys, interactive),
        Command::Attach {
            detach_keys,
            no_stdin,
            container,
        } => containers::attach(&container, &detach_keys, !no_stdin),
        Command::Stop { how, refs } => Ok(containers::stop(&refs.containers, how.options())),
        Command::Restart { how, refs } => Ok(containers::restart(&refs.containers, how.options())),
        Command::Kill { signal, refs } => Ok(containers::kill(&refs.containers, signal)),
        Command::Update { restart, refs } => Ok(containers::update(&refs.containers, restart)),
        Command::Wait { refs } => Ok(containers::wait(&refs.containers)),
        Command::Exec(args) => Ok(containers::exec(args)),
        Command::Cp(args) => containers::cp(&args),
        Command::Logs(args) => containers::logs(&args),
        Command::Events(args) => events_output::events(&args),
        Command::Rename {
            container,
            new_name,
        } => containers::rename(&container, &new_name),
        Command::Ps { all, quiet, format } => {
            containers::ps(all, quiet, format.unwrap_or_default())
        }
        Command::Port {
            container,
            sockets: true,
            format,
            ..
        } => containers::sockets(&container, format.unwrap_or_default()),
        Command::Port {
            container, port, ..
        } => containers::port(&container, port.as_ref()),
        Command::Inspect { refs } => containers::inspect(&refs.containers),
        Command::Rm { force, refs } => Ok(containers::rm(&refs.containers, force)),
        Command::Pull { quiet, reference } => images::pull(&reference, quiet),
        Command::Images | Command::Image(ImageCommand::Ls) => images::list(),
        Command::Rmi { force, images } => Ok(images::rmi(&images, force)),
        Command::Image(ImageCommand::Prune) => images::prune(),
        Command::Volume(command) => volumes::command(&command),
        Command::Network(command) => networks::command(&command),
        Command::System(SystemCommand::Df) => system::df(),
        Command::System(SystemCommand::Recover) => system::recover(),
        Command::System(SystemCommand::Stop) => system::stop(),
        Command::Vm(command) => vm::command(&command),
        Command::Doctor => doctor::run(),
    };
    result.unwrap_or_else(|err| fail(&err))
}

/// Reports an error from shipyard itself and returns exit code 125. Its
/// output's reader gone (`shipyard ps | head -1`) isn't one: quiet, and a
/// success, for every command.
fn fail(err: &anyhow::Error) -> ExitCode {
    if broken_pipe(err) {
        return ExitCode::SUCCESS;
    }
    reporter::report(err, EXIT_ENGINE_ERROR)
}

/// Whether `err` is a write to this process's output whose reader has gone
/// (`EPIPE`): this process's own, or the engine's (an I/O error inside its
/// error, or the sink of `logs`). An `EPIPE` alone isn't enough: an
/// internal pipe's (the storage helper's, say) is a failure to report, so
/// stdout's reader must be gone too.
fn broken_pipe(err: &anyhow::Error) -> bool {
    use shipyard_engine::logs::LogsError;
    let epipe = |io: &std::io::Error| io.kind() == std::io::ErrorKind::BrokenPipe;
    let has_epipe = err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(epipe)
            || cause
                .downcast_ref::<shipyard_engine::Error>()
                .is_some_and(|error| {
                    error.find::<std::io::Error>().is_some_and(epipe)
                        || matches!(error.find::<LogsError>(), Some(LogsError::Sink(io)) if epipe(io))
                })
    });
    has_epipe && shipyard_cli::pipe::reader_gone(std::io::stdout().as_fd())
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
    // Debugging (a flag, or `RUST_LOG`): every line in the full format, so
    // warnings keep their time and place among the rest.
    let debugging = matches!(
        flag,
        Some(LogLevel::Info | LogLevel::Debug | LogLevel::Trace)
    ) || (flag.is_none() && std::env::var_os("RUST_LOG").is_some());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(colour)
        .event_format(Lines {
            full: Format::default(),
            short_warnings: !debugging,
        })
        .init();
}

/// How events print: warnings and errors as the command's own lines
/// (`shipyard: warning: …`, as the [`CliReporter`](reporter::CliReporter)
/// prints them), whichever part of Shipyard they come from; the rest
/// (`--debug`, `RUST_LOG`) in tracing's full format, where the time and the
/// module matter, and while debugging warnings too.
struct Lines {
    full: Format,
    short_warnings: bool,
}

impl<S, N> FormatEvent<S, N> for Lines
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let label = match *event.metadata().level() {
            tracing::Level::WARN if self.short_warnings => "warning",
            tracing::Level::ERROR if self.short_warnings => "error",
            _ => return self.full.format_event(context, writer, event),
        };
        write!(writer, "shipyard: {label}: ")?;
        context.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A broken pipe that isn't stdout's (an internal helper's) is an
    /// error, reported: never taken for `shipyard ps | head -1`. (The test
    /// harness reads this process's stdout.)
    #[test]
    fn an_internal_broken_pipe_is_a_failure() {
        let err = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            .context("sending the storage helper its request");
        assert!(!broken_pipe(&err));
    }
}
