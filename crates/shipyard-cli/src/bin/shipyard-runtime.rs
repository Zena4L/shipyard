//! `shipyard-runtime`: the OCI runtime command line, as runc offers it.
//!
//! A container's life in separate calls: `create` sets it up from a bundle
//! and holds it before its program runs; `start` lets it run; `state`
//! reports it as JSON; `kill` signals it; `delete` removes it. Its cgroup
//! gives the rest: `pause` and `resume`, `update`, `ps`, `events`. Engines
//! and the OCI conformance suite drive a runtime this way. The work is in
//! `shipyard_runtime::container`.

#[path = "../cp_output.rs"]
mod cp_output;
#[path = "../limits.rs"]
#[allow(
    dead_code,
    reason = "shared with `shipyard`, which uses what `update` doesn't"
)]
mod limits;
#[path = "../logs_output.rs"]
mod logs_output;
#[path = "../runtime_commands.rs"]
mod runtime_commands;

use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use shipyard_runtime::container::{self, CreateOptions, LogOptions, StdinMode};
use shipyard_runtime::logs::info::LogSettings;
use shipyard_runtime::logs::time::Timestamp;
use shipyard_runtime::process::{self, Signal};

use crate::limits::LimitArgs;
use crate::runtime_commands::PsFormat;

/// Runs containers from OCI bundles, one lifecycle step per call.
#[derive(Debug, Parser)]
#[command(name = "shipyard-runtime", version)]
struct Cli {
    /// Directory for the containers' state [default: `/run/shipyard` for root,
    /// `$XDG_RUNTIME_DIR/shipyard` for anyone else].
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Also write errors and warnings to FILE (as runc's callers ask for).
    #[arg(long, global = true, value_name = "FILE")]
    log: Option<PathBuf>,

    /// The log file's format: text or json.
    #[arg(long, global = true, value_name = "FORMAT", default_value = "text")]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a container from a bundle, set up and waiting for `start`.
    Create(CreateArgs),
    /// Print a container's logs (one created with --log-dir): what it wrote
    /// on stdout to stdout, on stderr to stderr.
    Logs(LogsArgs),
    /// Attach to a container created with --log-dir: its output from now
    /// on, and your typing (with `create --stdin`). Ctrl-P Ctrl-Q detaches;
    /// otherwise it ends with the container, with its exit code.
    Attach(AttachArgs),
    /// Run a command inside a created or running container: in its
    /// namespaces and cgroup, confined as its own process is (what isn't
    /// given is its process's). Exits with the command's code, or 255 if it
    /// couldn't run.
    Exec(ExecArgs),
    /// Copy files into or out of a created, running or paused container:
    /// `cp ID:SRC DEST`, `cp SRC ID:DEST`, with `-` for a tar stream on
    /// stdout or stdin. Directories are copied whole.
    Cp(CpArgs),
    /// Run a created container's program.
    Start {
        /// The container's ID.
        id: String,
    },
    /// Print a container's state as JSON.
    State {
        /// The container's ID.
        id: String,
    },
    /// Send a signal to a container (default TERM).
    Kill {
        /// Signal every process in the container's cgroup, not only its
        /// init.
        #[arg(short, long)]
        all: bool,
        /// The container's ID.
        id: String,
        /// A name (TERM, SIGKILL) or a number.
        #[arg(default_value = "TERM")]
        signal: Signal,
    },
    /// Freeze a created or running container's processes.
    Pause {
        /// The container's ID.
        id: String,
    },
    /// Thaw a paused container.
    Resume {
        /// The container's ID.
        id: String,
    },
    /// List the processes in a container's cgroup, by host PID.
    Ps {
        /// table, or json (an array of PIDs).
        #[arg(long, value_enum, default_value = "table")]
        format: PsFormat,
        /// The container's ID.
        id: String,
    },
    /// Print a container's resource use as JSON lines, and OOM kills, until
    /// it stops.
    Events {
        /// Print once and exit.
        #[arg(long)]
        stats: bool,
        /// How often (such as 5s or 500ms).
        #[arg(long, value_name = "DURATION", default_value = "5s", value_parser = logs_output::parse_duration)]
        interval: Duration,
        /// The container's ID.
        id: String,
    },
    /// Change a container's resource limits (created, running or paused).
    Update {
        /// Read the limits from FILE, a linux.resources JSON object (- for
        /// stdin), instead of flags.
        #[arg(short, long, value_name = "FILE", conflicts_with = "limits")]
        resources: Option<PathBuf>,
        /// The limits to change.
        #[command(flatten)]
        limits: Box<LimitArgs>,
        /// The container's ID.
        id: String,
    },
    /// Remove a stopped (or created) container.
    Delete {
        /// Kill a running container first.
        #[arg(short, long)]
        force: bool,
        /// The container's ID.
        id: String,
    },
}

/// `create`'s arguments.
#[derive(Debug, clap::Args)]
struct CreateArgs {
    /// The bundle: a directory with config.json.
    #[arg(short, long, value_name = "DIR", default_value = ".")]
    bundle: PathBuf,
    /// Write the container's init PID to FILE.
    #[arg(long, value_name = "FILE")]
    pid_file: Option<PathBuf>,
    /// A UNIX socket to send the container's terminal to (its
    /// pseudo-terminal's master), as runc does; needed when the bundle
    /// asks for a terminal (`process.terminal`).
    #[arg(long, value_name = "SOCKET")]
    console_socket: Option<PathBuf>,
    /// Keep the container's output in logs in DIR, read with `logs`:
    /// CRI-format files, rotated. `create` makes DIR (it mustn't
    /// exist); it outlives the container. The container then doesn't
    /// get `create`'s stdout and stderr, and its stdin is empty unless
    /// --stdin keeps it open.
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
    /// The most a log file holds before it's rotated (64k to 1g)
    /// [default: 10m].
    #[arg(long, value_name = "SIZE", requires = "log_dir", value_parser = limits::parse_size)]
    log_max_size: Option<u64>,
    /// How many log files are kept, the newest among them (2 to 100)
    /// [default: 5].
    #[arg(long, value_name = "N", requires = "log_dir")]
    log_max_files: Option<u32>,
    /// Keep the container's stdin open for `attach` to type into, instead of
    /// empty (/dev/null). Needs --log-dir.
    #[arg(long, requires = "log_dir")]
    stdin: bool,
    /// Close the container's stdin when the first client to type goes
    /// (detaches, its input ends, or it's dropped), once what it typed has
    /// been written.
    #[arg(long, requires = "stdin")]
    stdin_once: bool,
    /// Pass N more descriptors on to the container's process, from 3 up
    /// (after socket activation's, if `LISTEN_FDS` and `LISTEN_PID` give it
    /// any), as runc does.
    #[arg(long, value_name = "N", default_value_t = 0)]
    preserve_fds: usize,
    /// Have the container's monitor hold descriptor N (open in this
    /// process, above 2 and above those --preserve-fds passes on) until the
    /// container has ended, then give it to the exit command, or close it:
    /// what keeps the caller's resources (a lock's, say) alive once the
    /// caller has gone. The container never has it. LABEL (letters, digits,
    /// : _ . and -; fd-N if not given) names it: the exit command is told
    /// the labels in `SHIPYARD_HOLD_LABELS`. Repeatable.
    #[arg(long = "hold-fd", value_name = "N[=LABEL]", value_parser = parse_hold)]
    hold_fd: Vec<(i32, String)>,
    /// Run PROGRAM (an absolute path) once the container has ended and its
    /// end is recorded: in a session of its own, with this command's
    /// environment, its output discarded, the held descriptors at 3 and up
    /// (`SHIPYARD_HOLDS` says how many, `SHIPYARD_HOLD_LABELS` their labels,
    /// comma-separated, in order), how the container ended in
    /// `SHIPYARD_EXIT_RECORD` (JSON), and the monitor's PID in
    /// `SHIPYARD_MONITOR_PID` (the monitor holds its copies of the
    /// descriptors until it exits). Not waited for.
    #[arg(long, value_name = "PROGRAM")]
    exit_command: Option<PathBuf>,
    /// An argument for the exit command (repeatable, in order).
    #[arg(
        long = "exit-command-arg",
        value_name = "ARG",
        requires = "exit_command",
        allow_hyphen_values = true
    )]
    exit_command_arg: Vec<String>,
    /// Run PROGRAM (an absolute path) beside the container: started once
    /// it's created (the program waits for its start, if it needs to), in a
    /// session of its own, with this command's environment, its output
    /// discarded and none of the held descriptors; killed (SIGKILL), with
    /// its whole process group, once the container has ended, before its end
    /// is recorded (and if the monitor dies). For health checks.
    #[arg(long, value_name = "PROGRAM")]
    companion: Option<PathBuf>,
    /// An argument for the companion (repeatable, in order).
    #[arg(
        long = "companion-arg",
        value_name = "ARG",
        requires = "companion",
        allow_hyphen_values = true
    )]
    companion_arg: Vec<String>,
    /// The container's ID.
    id: String,
}

/// `attach`'s arguments.
#[derive(Debug, clap::Args)]
struct AttachArgs {
    /// The keys that detach, Docker's way: `ctrl-` and a letter (or one of
    /// @[\]^_), or a character, separated by commas; empty: none.
    #[arg(long, value_name = "KEYS", default_value = "ctrl-p,ctrl-q")]
    detach_keys: String,
    /// Don't pass your input on (only watch the output).
    #[arg(long)]
    no_stdin: bool,
    /// The container's ID.
    id: String,
}

/// `exec`'s arguments (runc's).
#[derive(Debug, clap::Args)]
struct ExecArgs {
    /// A runc process.json: the whole process, instead of COMMAND.
    #[arg(short, long, value_name = "FILE", conflicts_with = "command")]
    process: Option<PathBuf>,
    /// Return once the command runs, leaving it running (it isn't stopped
    /// with this one).
    #[arg(short, long)]
    detach: bool,
    /// Give the command a terminal, made in the container: relayed to this
    /// one (raw, and sized as it), or with --console-socket sent there.
    #[arg(short, long)]
    tty: bool,
    /// Send the command's terminal's master to this UNIX socket (runc's
    /// protocol), instead of relaying it; needs a terminal (-t, or the
    /// process.json's). With -d -t, required.
    #[arg(long, value_name = "SOCKET")]
    console_socket: Option<PathBuf>,
    /// With --detach, write the command's PID (as the host sees it) to FILE.
    #[arg(long, value_name = "FILE", requires = "detach")]
    pid_file: Option<PathBuf>,
    /// The working directory, inside the container.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Set an environment variable (KEY=VALUE) over the container's; may be
    /// given more than once.
    #[arg(short, long, value_name = "KEY=VALUE")]
    env: Vec<std::ffi::OsString>,
    /// The user, UID or UID:GID, as the container numbers them.
    #[arg(short, long, value_name = "UID[:GID]", value_parser = parse_exec_user)]
    user: Option<(u32, Option<u32>)>,
    /// A supplementary group, instead of the container's; may be given more
    /// than once.
    #[arg(short = 'g', long = "additional-gids", value_name = "GID")]
    additional_gids: Vec<u32>,
    /// Add a capability (`SYS_PTRACE`, or `CAP_SYS_PTRACE`), within the
    /// container's bounding set; may be given more than once.
    #[arg(short = 'c', long = "cap", value_name = "CAP")]
    caps: Vec<shipyard_runtime::security::Capability>,
    /// Set `no_new_privs` (on anyway if the container has it).
    #[arg(long)]
    no_new_privs: bool,
    /// Pass N more descriptors on to the command, from 3 up (after socket
    /// activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as runc
    /// does.
    #[arg(long, value_name = "N", default_value_t = 0)]
    preserve_fds: usize,
    /// The container's ID.
    id: String,
    /// The command and its arguments.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required_unless_present = "process",
        value_name = "COMMAND"
    )]
    command: Vec<std::ffi::OsString>,
}

/// `exec --user`: UID or UID:GID.
fn parse_exec_user(text: &str) -> Result<(u32, Option<u32>), String> {
    let number = |part: &str| {
        part.parse::<u32>()
            .map_err(|_| format!("{text}: a user is UID or UID:GID, as numbers"))
    };
    match text.split_once(':') {
        Some((uid, gid)) => Ok((number(uid)?, Some(number(gid)?))),
        None => Ok((number(text)?, None)),
    }
}

/// `cp`'s arguments.
#[derive(Debug, clap::Args)]
struct CpArgs {
    /// Files copied in keep their owners (where the container has those
    /// IDs), instead of becoming the container's root's.
    #[arg(short, long)]
    archive: bool,
    /// Follow SRC if it's a symlink.
    #[arg(short = 'L', long)]
    follow_link: bool,
    /// ID:PATH, a host path, or - (a tar stream on stdin).
    #[arg(value_name = "SRC")]
    source: std::ffi::OsString,
    /// ID:PATH, a host path, or - (a tar stream on stdout).
    #[arg(value_name = "DEST")]
    destination: std::ffi::OsString,
}

/// `logs`' arguments.
#[derive(Debug, clap::Args)]
struct LogsArgs {
    /// Then print more as it comes, until the container has ended (with
    /// --log-dir, until interrupted).
    #[arg(short, long)]
    follow: bool,
    /// Only the last N lines.
    #[arg(long, value_name = "N")]
    tail: Option<usize>,
    /// Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), or a
    /// duration ago (10m, 2h, 30s).
    #[arg(long, value_name = "TIME", value_parser = logs_output::parse_since)]
    since: Option<Timestamp>,
    /// Start each line with the time it was written (RFC 3339).
    #[arg(short, long)]
    timestamps: bool,
    /// Read the logs in DIR instead of a container's (one deleted
    /// since, say).
    #[arg(long, value_name = "DIR", conflicts_with = "id")]
    log_dir: Option<PathBuf>,
    /// The container's ID.
    #[arg(required_unless_present = "log_dir")]
    id: Option<String>,
}

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
        Command::Ps { format, id } => runtime_commands::ps(root, id, *format)?,
        Command::Events {
            stats,
            interval,
            id,
        } => runtime_commands::events(root, id, *stats, *interval)?,
        Command::Update {
            resources,
            limits,
            id,
        } => {
            let skipped = runtime_commands::update(root, id, resources.as_deref(), limits)?;
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

/// `--hold-fd N[=LABEL]`: the number, and its label (`fd-N` if not given;
/// `create` says whether it's one).
fn parse_hold(text: &str) -> Result<(i32, String), String> {
    let (number, label) = match text.split_once('=') {
        Some((_, "")) => return Err(format!("{text:?}: a label after = (or none, and no =)")),
        Some((number, label)) => (number, Some(label)),
        None => (text, None),
    };
    let fd: i32 = number
        .parse()
        .map_err(|_| format!("{number:?} isn't a descriptor's number"))?;
    let label = label.map_or_else(|| format!("fd-{fd}"), str::to_owned);
    Ok((fd, label))
}

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
fn create(cli: &Cli, root: &Path, args: &CreateArgs) -> anyhow::Result<()> {
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
fn cp(cli: &Cli, root: &Path, args: &CpArgs) -> anyhow::Result<()> {
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

/// `text` with its control characters escaped (`\u{1b}`), so it can't move
/// the cursor or change the terminal it's printed on. Newlines stay: a
/// message's own lines.
fn escape_controls(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escaped: Vec<char> = if c.is_control() && c != '\n' {
                c.escape_default().collect()
            } else {
                vec![c]
            };
            escaped
        })
        .collect()
}

/// `attach`: until detached (exit 0) or the container ends (its exit code).
fn attach(root: &Path, args: &AttachArgs) -> anyhow::Result<()> {
    use shipyard_engine::attach::Options;
    let options = Options {
        detach_keys: shipyard_runtime::attach::DetachKeys::parse(&args.detach_keys)?,
        stdin: !args.no_stdin,
        held: false,
    };
    match shipyard_engine::attach::to_runtime(root, &args.id, &options)? {
        shipyard_engine::attach::Settled::Exited(code) => std::process::exit(code),
        shipyard_engine::attach::Settled::Left => Ok(()),
    }
}

/// `exec`: exits with the command's code (128 + N for a signal), 0 once it
/// runs with --detach, or 255 if it couldn't run (runc's), never returning.
fn exec(cli: &Cli, root: &Path, args: &ExecArgs) -> ! {
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
const OWN_FAILURE: i32 = 255;

/// `logs`: a container's, or a directory's.
fn logs(cli: &Cli, root: &Path, args: &LogsArgs) -> anyhow::Result<()> {
    let LogsArgs {
        follow,
        tail,
        since,
        timestamps,
        log_dir,
        id,
    } = args;
    let target = match (log_dir, id) {
        (Some(dir), _) => shipyard_engine::logs::RuntimeLogs::Dir(dir),
        (None, Some(id)) => shipyard_engine::logs::RuntimeLogs::Container(id),
        (None, None) => anyhow::bail!("give a container's ID, or --log-dir"),
    };
    let options = shipyard_engine::logs::Options {
        tail: *tail,
        since: *since,
        follow: *follow,
        ..shipyard_engine::logs::Options::default()
    };
    let skipped = logs_output::printed(*timestamps, |sink| {
        shipyard_engine::logs::of_runtime(root, target, &options, sink)
    })?;
    if let Some(warning) = logs_output::skipped_warning(skipped) {
        report(cli, Level::Warning, &warning);
    }
    Ok(())
}

/// The most a socket's path can be, with its terminating NUL
/// (`sizeof(sun_path)`, TLPI §57.2).
const SUN_PATH_MAX: usize = 108;

/// Connects to the console socket at `path` (`--console-socket`), as the
/// caller: the container's init (`create`) or exec's joiner (`exec -t`)
/// sends a terminal there.
///
/// A path too long for a socket address is reached through its directory:
/// opened, then named as `/proc/self/fd/N/NAME`, which is short.
fn connect_console(path: &Path) -> anyhow::Result<UnixStream> {
    use anyhow::Context as _;
    let named = || format!("--console-socket {}", path.display());
    let metadata = std::fs::metadata(path).with_context(named)?;
    if !metadata.file_type().is_socket() {
        anyhow::bail!("{}: not a socket", named());
    }
    let connected = if path.as_os_str().len() < SUN_PATH_MAX {
        UnixStream::connect(path)
    } else {
        connect_through_directory(path)
    };
    connected.with_context(named)
}

/// Connects to the socket at the long path `path` through its directory.
#[cfg(target_os = "linux")]
fn connect_through_directory(path: &Path) -> std::io::Result<UnixStream> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return UnixStream::connect(path);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(parent)?;
    let mut short = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    short.push(name);
    UnixStream::connect(short)
}

/// Connects to the socket at `path` (no `/proc` to shorten it with here).
#[cfg(not(target_os = "linux"))]
fn connect_through_directory(path: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Writes the PID whole or not at all (a reader may poll for the file).
///
/// The temporary file is new (`O_EXCL`, which never follows a symlink): in
/// a directory others can write to, a planted file or link there would
/// otherwise get root's write, or become the PID file with its planter's
/// content.
fn write_pid_file(file: &Path, pid: i32) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    use anyhow::Context as _;
    let name = file.file_name().context("the PID file needs a file name")?;
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".{}.tmp", std::process::id()));
    let temporary = file.with_file_name(temporary_name);
    let context = || format!("writing the PID file {}", file.display());
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        // O_CREAT|O_EXCL: fails on anything there, a symlink included
        // (which then isn't ours to remove).
        .create_new(true)
        .mode(0o644)
        .open(&temporary)
        .with_context(context)?;
    let written = out
        .write_all(pid.to_string().as_bytes())
        .and_then(|()| std::fs::rename(&temporary, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.with_context(context)
}

/// How serious a message is, for stderr and the log.
#[derive(Debug, Clone, Copy)]
enum Level {
    Error,
    Warning,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Prints a message to stderr (`shipyard-runtime: …` for an error,
/// `shipyard-runtime: warning: …` for a warning) and appends it to the log.
fn report(cli: &Cli, level: Level, message: &str) {
    // A container's file names, IDs in its records, its program's name: what
    // a message quotes can hold anything. Shown, never obeyed.
    let message = &escape_controls(message);
    let prefix = match level {
        Level::Error => "",
        Level::Warning => "warning: ",
    };
    let _ = writeln!(
        std::io::stderr().lock(),
        "shipyard-runtime: {prefix}{message}"
    );
    if let Some(log) = &cli.log {
        append_log(log, cli.log_format, level, message);
    }
}

/// Appends a message to the log file, as text or as runc's JSON lines. Best
/// effort: the message is on stderr anyway.
fn append_log(log: &Path, format: LogFormat, level: Level, message: &str) {
    let level = level.as_str();
    let line = match format {
        LogFormat::Text => format!("{level}: {message}\n"),
        LogFormat::Json => {
            let mut line = serde_json::json!({"level": level, "msg": message}).to_string();
            line.push('\n');
            line
        }
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a message quotes of a container's can't drive the terminal; a
    /// message's own lines stay lines.
    #[test]
    fn control_characters_are_shown_escaped() {
        assert_eq!(
            escape_controls("a\u{1b}[2Jb\rc\nd é"),
            "a\\u{1b}[2Jb\\rc\nd é"
        );
    }

    /// `docs/user/runtime-cli.md` is generated from the flags above, like
    /// `cli.md` is from `shipyard`'s.
    #[test]
    fn runtime_reference_is_up_to_date() {
        const HEADER: &str = "<!-- Generated from shipyard-runtime's flag definitions. Do not edit by hand.\n     \
             Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli runtime_reference -->\n\n";
        let options = clap_markdown::MarkdownOptions::new()
            .title("shipyard-runtime reference".to_owned())
            .show_footer(false)
            .show_table_of_contents(true);
        let generated = format!(
            "{HEADER}{}",
            clap_markdown::help_markdown_custom::<Cli>(&options)
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/user/runtime-cli.md");
        if std::env::var_os("UPDATE_DOCS").is_some() {
            std::fs::write(&path, &generated).expect("writing docs/user/runtime-cli.md");
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "docs/user/runtime-cli.md is out of date. Regenerate it with:\n\n    \
             UPDATE_DOCS=1 cargo test -p shipyard-cli runtime_reference\n"
        );
    }

    #[test]
    fn runc_style_command_lines_parse() {
        let cli = Cli::try_parse_from([
            "shipyard-runtime",
            "--root",
            "/r",
            "--log",
            "/l",
            "--log-format",
            "json",
            "create",
            "--bundle",
            "/b",
            "--pid-file",
            "/p",
            "c1",
        ])
        .unwrap();
        assert_eq!(cli.root.as_deref(), Some(Path::new("/r")));
        assert!(matches!(cli.command, Command::Create(ref args) if args.id == "c1"));
        let cli = Cli::try_parse_from(["shipyard-runtime", "kill", "c1", "KILL"]).unwrap();
        assert_eq!(cli.root, None, "the default is worked out when it's used");
        assert!(
            matches!(cli.command, Command::Kill { signal, .. } if signal.as_raw() == libc::SIGKILL)
        );
        let cli = Cli::try_parse_from(["shipyard-runtime", "kill", "c1"]).unwrap();
        assert!(
            matches!(cli.command, Command::Kill { signal, .. } if signal.as_raw() == libc::SIGTERM)
        );
        let cli = Cli::try_parse_from(["shipyard-runtime", "delete", "-f", "c1"]).unwrap();
        assert!(matches!(cli.command, Command::Delete { force: true, .. }));
        assert!(Cli::try_parse_from(["shipyard-runtime", "kill", "c1", "NOPE"]).is_err());
    }
}
