//! `shipyard run` and `shipyard create`: the flags' request, run or made by
//! the engine.

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;
use shipyard_engine::run::{Outcome, RunRequest, Supervision};
use shipyard_runtime::process;

use super::{CreateCommand, RunArgs, RunCommand};

/// `shipyard run`: in the foreground ([`run`]), or detached (`-d`): the
/// container made and started, its ID printed.
pub(crate) fn run_command(command: RunCommand) -> Result<ExitCode> {
    // Checked with -d too, where they do nothing yet.
    let detach_keys = command
        .detach_keys
        .as_deref()
        .map(shipyard_runtime::attach::DetachKeys::parse)
        .transpose()?;
    if !command.detach {
        let output = match attached_streams(&command) {
            Ok(output) => output,
            Err(error) => return Ok(usage_error(error, "run")),
        };
        let foreground = shipyard_engine::run::Foreground {
            sig_proxy: command.sig_proxy,
            detach_keys,
            output,
        };
        return run(command.args, command.rm, foreground);
    }
    let quiet = command.args.quiet;
    let request = match container_request(command.args, "run") {
        Ok(request) => request,
        Err(code) => return Ok(code),
    };
    shipyard_engine::guest::shares::check_binds(&request.volume)?;
    let engine = engine(quiet);
    let detached = shipyard_engine::run::detached(&engine, request, command.rm)?;
    writeln!(std::io::stdout().lock(), "{}", detached.id.as_str())?;
    // Only now may a Ctrl-C held off while it was made end this process.
    drop(detached);
    Ok(ExitCode::SUCCESS)
}

/// `shipyard create`: the container made, its ID printed.
pub(crate) fn create(command: CreateCommand) -> Result<ExitCode> {
    let quiet = command.args.quiet;
    let request = match container_request(command.args, "create") {
        Ok(request) => request,
        Err(code) => return Ok(code),
    };
    shipyard_engine::guest::shares::check_binds(&request.volume)?;
    let engine = engine(quiet);
    let record = shipyard_engine::run::create(&engine, request, command.rm)?;
    writeln!(std::io::stdout().lock(), "{}", record.id.as_str())?;
    Ok(ExitCode::SUCCESS)
}

/// The engine a run reports to: its pull's progress shown, unless `-q`.
fn engine(quiet: bool) -> shipyard_engine::Engine {
    crate::reporter::engine_with(crate::reporter::CliReporter { progress: !quiet })
}

/// The streams `-a` attaches (all of them without it). `-a stdin` goes
/// with `-i` (your input passed on): one without the other is refused,
/// Docker's would keep an input open that's never written, or attach one
/// that's never open, which a container here can't (its input is passed
/// on, or empty).
///
/// # Errors
///
/// That refusal, as a usage error.
fn attached_streams(
    command: &RunCommand,
) -> std::result::Result<shipyard_engine::attach::Output, clap::Error> {
    use super::AttachStream;
    let asked = &command.attach;
    if asked.is_empty() {
        return Ok(shipyard_engine::attach::Output::BOTH);
    }
    let stdin = asked.contains(&AttachStream::Stdin);
    if command.args.streams.interactive != stdin {
        return Err(clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "-a stdin and -i go together (your input attached and passed on): give both, \
             or neither\n",
        ));
    }
    Ok(shipyard_engine::attach::Output {
        stdout: asked.contains(&AttachStream::Stdout),
        stderr: asked.contains(&AttachStream::Stderr),
    })
}

/// A container's request (`create`, `run -d`): as a run's, without what
/// only a foreground run can have (descriptors passed on). A usage error
/// is printed (with `subcommand`'s usage), and its exit code returned.
fn container_request(args: RunArgs, subcommand: &str) -> std::result::Result<RunRequest, ExitCode> {
    if args.preserve_fds > 0 {
        let error = clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "--preserve-fds passes this command's descriptors on: only a foreground run can",
        );
        return Err(usage_error(error, subcommand));
    }
    args.into_request()
        .map_err(|error| usage_error(error, subcommand))
}

/// Prints `error` as clap's own (with `subcommand`'s usage and the
/// `--help` tip): exit 125 (Docker's).
fn usage_error(error: clap::Error, subcommand: &str) -> ExitCode {
    use clap::CommandFactory as _;
    let mut cli = crate::cli::Cli::command();
    let error = match cli.find_subcommand_mut(subcommand) {
        Some(run) => error.format(run),
        None => error,
    };
    let _ = error.print();
    ExitCode::from(process::EXIT_ENGINE_ERROR)
}

/// Runs the request in the foreground and exits the way the run did: with
/// its exit code, or `128 + N` if signal N killed it. Failures to start it
/// exit 125, 126 or 127. An image or `--rootfs` run is a container
/// (removed at its end with `auto_remove`, `--rm`; the signals this process
/// gets passed on, and its detach keys, as `foreground` says); any other is
/// supervised here.
fn run(
    args: RunArgs,
    auto_remove: bool,
    foreground: shipyard_engine::run::Foreground,
) -> Result<ExitCode> {
    // First, before anything is opened or started (the network's helpers):
    // what this process was given to pass on is 3 and up, and nothing it
    // starts in between may inherit it.
    let passed = process::passed::PassedFds::from_caller(args.preserve_fds)?;
    let quiet = args.quiet;
    // A usage error, as clap's own: printed, exit 125 (Docker's).
    let mut request = match args.into_request() {
        Ok(request) => request,
        Err(error) => return Ok(usage_error(error, "run")),
    };
    // In the macOS guest, a bind only of what the Mac shares (step 13.4).
    shipyard_engine::guest::shares::check_binds(&request.volume)?;
    // Its input attached (`-i`, no terminal): it ends at this run's end of
    // file, and at each later start's, as `docker run -i` makes it
    // (`StdinOnce`, kept with the container).
    request.streams.once = request.streams.interactive && !request.streams.tty;
    let host_command = request
        .image
        .as_deref()
        .filter(|image| is_host_command(image))
        .map(str::to_owned);
    // One engine for the whole run: its places and reporter, read once.
    let engine = engine(quiet);
    match request.supervision(&engine, &passed) {
        Supervision::Container => {
            let ran = shipyard_engine::run::attached(&engine, request, auto_remove, foreground)
                .inspect_err(|_| hint_host_command(host_command.as_deref()))?;
            if let Some(signal) = ran.interrupted {
                // As the container ended: from Ctrl-C passed on, this process
                // too, so a shell loop around it stops (its code the same).
                process::die_from(signal);
            }
            return Ok(ExitCode::from(ran.code));
        }
        Supervision::Here { why } => {
            if let Some(why) = why {
                crate::reporter::warn(why)?;
            }
            // A container's own: said where they do nothing.
            if auto_remove {
                crate::reporter::warn(
                    "--rm: this run isn't a container: it leaves nothing to \
                     remove",
                )?;
            }
            if request.restart != shipyard_engine::containers::RestartPolicy::No {
                crate::reporter::warn(
                    "--restart: this run isn't a container: nothing starts \
                     it again",
                )?;
            }
            if request
                .health
                .as_ref()
                .is_some_and(|health| !health.is_disabled())
            {
                crate::reporter::warn(
                    "--health-*: this run isn't a container: nothing \
                     checks it",
                )?;
            }
            if !foreground.sig_proxy {
                crate::reporter::warn(
                    "--sig-proxy=false: this run isn't a container: its \
                     signals are passed on as they come",
                )?;
            }
            if foreground.detach_keys.is_some() {
                crate::reporter::warn(
                    "--detach-keys: this run isn't a container: it can't be \
                     detached from",
                )?;
            }
        }
    }
    let ran = shipyard_engine::run::run(&engine, request, passed)
        .inspect_err(|_| hint_host_command(host_command.as_deref()))?;
    Ok(match ran {
        Outcome::Exited(code) | Outcome::Failed(code) => ExitCode::from(code),
        Outcome::Interrupted { signal, code } => {
            // Last of all: dying from a signal runs no destructors, and
            // the run has let go of everything already.
            process::die_from(signal);
            ExitCode::from(code)
        }
    })
}

/// After a failed run whose image was a word that's also a program here
/// (`word`): says so, with how to run the host's.
fn hint_host_command(word: Option<&str>) {
    if let Some(word) = word {
        // The old spelling (`run sh …` ran the host's `sh`): the
        // failure is likely a registry's "not found" for `sh`.
        let _ = crate::reporter::hint(format_args!(
            "`{word}` was taken as an image, and is also a program here: to \
             run the host's, give --no-image first (shipyard run --no-image {word} …)"
        ));
    }
}

/// Whether `image` is a bare word (no registry, path, tag or digest) that
/// names a program on this process's `PATH`: what `run sh` meant before
/// the image came first.
pub(super) fn is_host_command(image: &str) -> bool {
    !image.contains(['/', ':', '@', '.'])
        && shipyard_sys::path::find_program(image, std::env::var_os("PATH").as_deref()).is_some()
}
