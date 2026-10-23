//! The commands with arguments of their own beyond an ID: `create`, `cp`,
//! `attach`, `exec` and `logs`, and what they share (the PID file).

use std::os::fd::AsFd;
use std::path::Path;

use shipyard_runtime::container::{self, CreateOptions, LogOptions, StdinMode};
use shipyard_runtime::logs::info::LogSettings;

use shipyard_cli::logs_output;

use crate::args::{AttachArgs, Cli, CpArgs, CreateArgs, ExecArgs, LogsArgs};
use crate::console::connect_console;
use crate::cp_output;
use crate::report::{Level, report};

/// The descriptors `--hold-fd` names, borrowed for `create` (which says
/// what can't be held), and made close-on-exec here: nothing this process
/// starts but the monitor gets them. A number that isn't open is refused.
fn take_holds(numbers: &[(i32, String)]) -> anyhow::Result<Vec<container::Hold<'_>>> {
    numbers
        .iter()
        .map(|(fd, label)| {
            let fd = *fd;
            // SAFETY: `F_GETFD` on a number only asks whether it's open.
            if fd < 0 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
                return Err(container::ContainerError::BadHold(fd).into());
            }
            // SAFETY: open (checked), given to this process for this; it
            // stays open as long as this process runs (nothing here closes
            // it).
            let hold = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            let _ = nix::fcntl::fcntl(
                hold,
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
            );
            Ok(container::Hold {
                fd: hold,
                label: label.as_str(),
            })
        })
        .collect()
}

/// `create`: the container, with its console socket or its logs if asked.
pub(crate) fn create(cli: &Cli, root: &Path, args: &CreateArgs) -> anyhow::Result<()> {
    let CreateArgs {
        bundle,
        pid_file,
        console_socket,
        log_dir,
        log_max_size,
        log_max_files,
        stdin,
        stdin_once,
        preserve_fds,
        hold_fd,
        exit_command,
        exit_command_arg,
        companion,
        companion_arg,
        id,
    } = args;
    // First, before this process opens anything: what it was given to pass
    // on is 3 and up, and what it was given to hold.
    let passed = shipyard_runtime::process::passed::PassedFds::from_caller(*preserve_fds)?;
    let holds = take_holds(hold_fd)?;
    let exit_command = exit_command
        .as_ref()
        .map(|program| container::ExitCommand::new(program, exit_command_arg.clone()))
        .transpose()?;
    let companion = companion
        .as_ref()
        .map(|program| container::ExitCommand::new(program, companion_arg.clone()))
        .transpose()?;
    let console = console_socket.as_deref().map(connect_console).transpose()?;
    let defaults = LogSettings::default();
    let options = CreateOptions {
        console: console.as_ref().map(AsFd::as_fd),
        logs: log_dir.as_ref().map(|dir| LogOptions {
            dir: dir.clone(),
            settings: LogSettings {
                max_size: log_max_size.unwrap_or(defaults.max_size),
                max_files: log_max_files.unwrap_or(defaults.max_files),
            },
            stdin: match (stdin, stdin_once) {
                (_, true) => StdinMode::Once,
                (true, false) => StdinMode::Open,
                (false, false) => StdinMode::Closed,
            },
        }),
        passed: Some(&passed),
        holds: &holds,
        exit_command,
        companion,
    };
    let created = container::create(root, id, bundle, &options)?;
    // The init has already sent the master on its copy and closed it.
    // Closing ours too lets the receiver see end-of-file.
    drop(console);
    for warning in &created.warnings {
        report(cli, Level::Warning, warning);
    }
    if let Some(file) = pid_file
        && let Err(err) = write_pid_file(file, created.pid)
    {
        // Where its logs are, before the container's record goes.
        let logs = container::logs_of(root, id).ok();
        // `create` fails, so nothing is left (as runc does): a
        // caller told "failed" would otherwise find the ID taken.
        // Its `poststop` hooks' failures, or why it couldn't be
        // removed, are said too: the caller must know what's left.
        match container::delete(root, id, true) {
            Ok(warnings) => {
                for warning in &warnings {
                    report(cli, Level::Warning, warning);
                }
            }
            Err(left) => report(
                cli,
                Level::Warning,
                &format!("container {id} couldn't be removed: {left}"),
            ),
        }
        // Its logs too, as a failed create's: only the directory it made.
        if let Some(said) = logs.as_ref().and_then(container::remove_failed_logs) {
            report(cli, Level::Warning, &said);
        }
        return Err(err);
    }
    Ok(())
}

/// `cp`: one side a container's path, the other the host's or a stream.
pub(crate) fn cp(cli: &Cli, root: &Path, args: &CpArgs) -> anyhow::Result<()> {
    use shipyard_engine::cp::{CpOptions, Operand, Transfer};
    let options = CpOptions {
        archive: args.archive,
        follow_link: args.follow_link,
    };
    let transfer = Transfer::of(
        Operand::parse(&args.source),
        Operand::parse(&args.destination),
    )?;
    // (Its own message only: cp's errors say their causes already.)
    let copied =
        cp_output::copy(root, transfer, options).map_err(|err| anyhow::anyhow!(err.to_string()))?;
    for warning in copied.warnings {
        report(cli, Level::Warning, &warning);
    }
    Ok(())
}

/// `attach`: until detached (exit 0) or the container ends (its exit code).
pub(crate) fn attach(root: &Path, args: &AttachArgs) -> anyhow::Result<()> {
    use shipyard_runtime::attach::client::{AttachOptions, Settled, to_runtime};
    let options = AttachOptions {
        detach_keys: shipyard_runtime::attach::DetachKeys::parse(&args.detach_keys)?,
        stdin: !args.no_stdin,
        held: false,
        output: shipyard_runtime::attach::client::Output::BOTH,
    };
    match to_runtime(root, &args.id, &options)? {
        Settled::Exited(code) => std::process::exit(code),
        Settled::Left => Ok(()),
    }
}

/// `exec`: exits with the command's code (128 + N for a signal), 0 once it
/// runs with --detach, or 255 if it couldn't run (runc's), never returning.
pub(crate) fn exec(cli: &Cli, root: &Path, args: &ExecArgs) -> ! {
    let mut options = container::ExecOptions::new(args.command.clone());
    options.process.clone_from(&args.process);
    options.cwd.clone_from(&args.cwd);
    options.env.clone_from(&args.env);
    options.user = args.user;
    options.additional_gids.clone_from(&args.additional_gids);
    options.capabilities.clone_from(&args.caps);
    options.no_new_privileges = args.no_new_privs;
    options.detach = args.detach;
    options.terminal = args.tty;
    let failed = |err: anyhow::Error| -> ! {
        report(cli, Level::Error, &format!("{err:#}"));
        std::process::exit(OWN_FAILURE);
    };
    // First, before this process opens anything: what it was given to pass
    // on is 3 and up.
    let passed = shipyard_runtime::process::passed::PassedFds::from_caller(args.preserve_fds)
        .unwrap_or_else(|err| failed(err.into()));
    // runc's rules, before anything connects to the socket (a receiver
    // should see no connection from a call that's refused). A process.json
    // may ask for a terminal: `container::exec` checks that case.
    if args.console_socket.is_some() && !args.tty && args.process.is_none() {
        failed(container::ContainerError::ExecConsoleWithoutTerminal.into());
    }
    if args.detach && args.tty && args.console_socket.is_none() {
        failed(container::ContainerError::ExecTerminalNeedsConsole.into());
    }
    // Open for the whole call: the terminal's master goes there.
    let console = args
        .console_socket
        .as_deref()
        .map(connect_console)
        .transpose()
        .unwrap_or_else(|err| failed(err));
    options.console = console.as_ref().map(std::os::fd::AsRawFd::as_raw_fd);
    match container::exec(root, &args.id, &options, Some(passed)) {
        Ok(container::ExecOutcome::Exited(status)) => std::process::exit(i32::from(status.code())),
        Ok(container::ExecOutcome::Detached { pid }) => {
            if let Some(file) = &args.pid_file {
                let number = i32::try_from(pid).unwrap_or(i32::MAX);
                if let Err(err) = write_pid_file(file, number) {
                    failed(err.context(format!(
                        "the command runs (as PID {pid}), but its PID file couldn't be written"
                    )));
                }
            }
            std::process::exit(0);
        }
        Ok(_) => failed(anyhow::anyhow!(
            "exec ended in a way this Shipyard doesn't know"
        )),
        Err(err) => failed(err.into()),
    }
}

/// `exec`'s and `attach`'s own failures (runc's): any other code they exit
/// with is the command's, or the container's.
pub(crate) const OWN_FAILURE: i32 = 255;

/// `logs`: a container's, or a directory's.
pub(crate) fn logs(cli: &Cli, root: &Path, args: &LogsArgs) -> anyhow::Result<()> {
    use shipyard_runtime::logs::reader::{LogsOptions, RuntimeLogs, of_runtime};
    let LogsArgs {
        follow,
        tail,
        since,
        timestamps,
        log_dir,
        id,
    } = args;
    let target = match (log_dir, id) {
        (Some(dir), _) => RuntimeLogs::Dir(dir),
        (None, Some(id)) => RuntimeLogs::Container(id),
        (None, None) => anyhow::bail!("give a container's ID, or --log-dir"),
    };
    let options = LogsOptions {
        tail: *tail,
        since: *since,
        follow: *follow,
        gone: Some(shipyard_runtime::logs::reader::Gone::new(
            shipyard_cli::pipe::stdout_gone,
        )),
        ..LogsOptions::default()
    };
    let skipped = logs_output::printed(*timestamps, |sink| {
        Ok(of_runtime(root, target, &options, sink)?)
    })?;
    if let Some(warning) = logs_output::skipped_warning(skipped) {
        report(cli, Level::Warning, &warning);
    }
    Ok(())
}

/// Writes the PID whole or not at all (a reader may poll for the file).
///
/// The temporary file is new (`O_EXCL`, which never follows a symlink): in
/// a directory others can write to, a planted file or link there would
/// otherwise get root's write, or become the PID file with its planter's
/// content; a name taken is passed over (`shipyard_sys::fs`).
fn write_pid_file(file: &Path, pid: i32) -> anyhow::Result<()> {
    use anyhow::Context as _;
    // Said as it always was (the write itself would say "no file name").
    file.file_name().context("the PID file needs a file name")?;
    shipyard_sys::fs::write_atomically(
        file,
        pid.to_string().as_bytes(),
        // Not synced, as a PID means nothing after a restart.
        &shipyard_sys::fs::Write::new(0o644).durability(shipyard_sys::fs::Durability::Atomic),
    )
    .map_err(std::io::Error::from)
    .with_context(|| format!("writing the PID file {}", file.display()))
}
