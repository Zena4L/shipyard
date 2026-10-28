//! The `shipyard` command-line interface.
//!
//! This crate only parses flags, calls the engine (`shipyard-engine`) and
//! prints what it returns and reports; the work happens there and in
//! `shipyard-runtime`.

mod about;
mod building;
mod cli;
mod compose;

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
mod format;
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
mod prune;
mod reporter;
mod run;
mod signing;
mod stats;
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

use crate::cli::{Cli, Command, ContainerCommand, ImageCommand, LogLevel, SystemCommand};

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
        return fail(&err.into(), EXIT_ENGINE_ERROR);
    }
    // A credential handed to a forwarded push or pull (step 12.4): taken
    // before anything opens a descriptor or starts a thread.
    // SAFETY: one thread so far, nothing opened since the entry points.
    let handed = match unsafe { signing::take_handed() } {
        Ok(handed) => handed,
        Err(err) => return fail(&err, EXIT_ENGINE_ERROR),
    };

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
    let command = cli.command.flattened();
    // On a Mac, every command but a few runs in Shipyard's VM (step 10.3b).
    #[cfg(target_os = "macos")]
    if let Some(code) = forward::elsewhere(&command) {
        return code;
    }
    let failed = command.failure_code();
    dispatch(command, handed).unwrap_or_else(|err| fail(&err, failed))
}

/// Runs `command` (a `push` or `pull` with the credential it was `handed`,
/// if one): its exit code, or the error it failed with.
fn dispatch(
    command: Command,
    handed: Option<shipyard_engine::image::GivenCredential>,
) -> anyhow::Result<ExitCode> {
    match command {
        Command::Run(command) => run::run_command(*command),
        Command::Create(command) => run::create(*command),
        Command::Start(args) if args.attach => {
            containers::start_attached(&args.refs.containers, &args.detach_keys, args.interactive)
        }
        Command::Start(args) => Ok(containers::start(&args.refs.containers)),
        Command::Attach(args) => {
            containers::attach(&args.container, &args.detach_keys, !args.no_stdin)
        }
        Command::Stop(args) => Ok(containers::stop(&args.refs.containers, args.how.options())),
        Command::Restart(args) => Ok(containers::restart(
            &args.refs.containers,
            args.how.options(),
        )),
        Command::Kill(args) => Ok(containers::kill(&args.refs.containers, args.signal)),
        Command::Update(args) => Ok(containers::update(&args)),
        Command::Pause(args) => Ok(containers::pause(&args.containers, true)),
        Command::Top(args) => stats::top(&args),
        Command::Stats(args) => stats::stats(&args),
        Command::Unpause(args) => Ok(containers::pause(&args.containers, false)),
        Command::Wait(refs) => Ok(containers::wait(&refs.containers)),
        Command::Exec(args) => Ok(containers::exec(args)),
        Command::Cp(args) => containers::cp(&args),
        Command::Export(args) => containers::export(&args),
        Command::Logs(args) => containers::logs(&args),
        Command::Events(args) => events_output::events(&args),
        Command::Rename(args) => containers::rename(&args.container, &args.new_name),
        Command::Ps(args) => containers::ps(&args),
        Command::Port(args) if args.sockets => {
            containers::sockets(&args.container, args.format.unwrap_or_default())
        }
        Command::Port(args) => containers::port(&args.container, args.port.as_ref()),
        Command::Inspect(args) => containers::inspect(&args),
        Command::Rm(args) => Ok(containers::rm(
            &args.refs.containers,
            shipyard_engine::run::RemoveOptions::new(args.force, args.volumes),
        )),
        Command::Pull(args) => images::pull(
            &args.reference,
            args.platform.as_deref(),
            args.quiet,
            handed,
        ),
        Command::Images(args) => images::list(&args),
        Command::Rmi(args) => Ok(images::rmi(&args.images, args.force)),
        Command::Tag(args) => images::tag(&args.source, &args.target),
        Command::Save(args) => images::save(&args),
        Command::Load(args) => images::load(&args),
        Command::History(args) => images::history(&args),
        Command::Build(args) => building::build(&args),
        Command::Builder(building::BuilderCommand::Prune(args)) => building::builder_prune(&args),
        Command::Import(args) => images::import(&args),
        Command::Push(args) => signing::push(&args, handed),
        Command::Login(args) => signing::login(&args),
        Command::Logout { server } => signing::logout(server.as_deref()),
        Command::Image(ImageCommand::Prune(args)) => prune::images(&args),
        Command::Container(ContainerCommand::Prune(args)) => prune::containers(&args),
        Command::Image(ImageCommand::Inspect { images, shown }) => {
            images::inspect(&images, shown.format.as_deref())
        }
        // A group's alias: `main` flattens it before it gets here.
        Command::Container(_)
        | Command::Image(
            ImageCommand::Ls(_)
            | ImageCommand::Rm(_)
            | ImageCommand::Pull(_)
            | ImageCommand::Tag(_)
            | ImageCommand::Save(_)
            | ImageCommand::Load(_)
            | ImageCommand::History(_)
            | ImageCommand::Build(_)
            | ImageCommand::Import(_)
            | ImageCommand::Push(_),
        ) => anyhow::bail!("a group's command reached the dispatch unflattened"),
        Command::Volume(command) => volumes::command(&command),
        Command::Network(command) => networks::command(&command),
        Command::System(SystemCommand::Prune(args)) => prune::system(&args),
        Command::System(SystemCommand::Df { format }) => system::df(format.as_deref()),
        Command::System(SystemCommand::Recover) => system::recover(),
        Command::System(SystemCommand::Stop) => system::stop(),
        Command::System(SystemCommand::Service {
            socket,
            ensure: false,
        }) => system::service(socket.as_deref(), REVISION),
        Command::System(SystemCommand::Service {
            socket,
            ensure: true,
        }) => system::ensure_service(socket.as_deref()),
        Command::Vm(command) => vm::command(&command),
        Command::Compose(args) => compose::compose(&args),
        Command::Doctor => doctor::run(),
        Command::Version(args) => about::version(REVISION, args.format.as_deref()),
        Command::Info(args) => about::info(REVISION, args.format.as_deref()),
    }
}

/// Reports an error from shipyard itself and returns `code` (the
/// command's [`failure_code`](Command::failure_code)). Its output's reader
/// gone (`shipyard ps | head -1`) isn't one: quiet, and a success, for
/// every command.
fn fail(err: &anyhow::Error, code: u8) -> ExitCode {
    if broken_pipe(err) {
        return ExitCode::SUCCESS;
    }
    reporter::report_error(err, code)
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
