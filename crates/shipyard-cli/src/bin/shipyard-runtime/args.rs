//! `shipyard-runtime`'s command line: its flags and commands, as clap
//! parses them (runc's, and Shipyard's own: `logs`, `attach`, `cp`).

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use shipyard_runtime::logs::time::Timestamp;
use shipyard_runtime::process::Signal;

use shipyard_cli::limits::{self, LimitArgs};
use shipyard_cli::logs_output;

/// Runs containers from OCI bundles, one lifecycle step per call.
#[derive(Debug, Parser)]
#[command(name = "shipyard-runtime", version)]
pub(crate) struct Cli {
    /// Directory for the containers' state [default: `/run/shipyard` for root,
    /// `$XDG_RUNTIME_DIR/shipyard` for anyone else].
    #[arg(long, global = true, value_name = "DIR")]
    pub(crate) root: Option<PathBuf>,

    /// Also write errors and warnings to FILE (as runc's callers ask for).
    #[arg(long, global = true, value_name = "FILE")]
    pub(crate) log: Option<PathBuf>,

    /// The log file's format: text or json.
    #[arg(long, global = true, value_name = "FORMAT", default_value = "text")]
    pub(crate) log_format: LogFormat,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
pub(crate) struct CreateArgs {
    /// The bundle: a directory with config.json.
    #[arg(short, long, value_name = "DIR", default_value = ".")]
    pub(crate) bundle: PathBuf,
    /// Write the container's init PID to FILE.
    #[arg(long, value_name = "FILE")]
    pub(crate) pid_file: Option<PathBuf>,
    /// A UNIX socket to send the container's terminal to (its
    /// pseudo-terminal's master), as runc does; needed when the bundle
    /// asks for a terminal (`process.terminal`).
    #[arg(long, value_name = "SOCKET")]
    pub(crate) console_socket: Option<PathBuf>,
    /// Keep the container's output in logs in DIR, read with `logs`:
    /// CRI-format files, rotated. `create` makes DIR (it mustn't
    /// exist); it outlives the container. The container then doesn't
    /// get `create`'s stdout and stderr, and its stdin is empty unless
    /// --stdin keeps it open.
    #[arg(long, value_name = "DIR")]
    pub(crate) log_dir: Option<PathBuf>,
    /// The most a log file holds before it's rotated (64k to 1g)
    /// [default: 10m].
    #[arg(long, value_name = "SIZE", requires = "log_dir", value_parser = limits::parse_size)]
    pub(crate) log_max_size: Option<u64>,
    /// How many log files are kept, the newest among them (2 to 100)
    /// [default: 5].
    #[arg(long, value_name = "N", requires = "log_dir")]
    pub(crate) log_max_files: Option<u32>,
    /// Keep the container's stdin open for `attach` to type into, instead of
    /// empty (/dev/null). Needs --log-dir.
    #[arg(long, requires = "log_dir")]
    pub(crate) stdin: bool,
    /// Close the container's stdin when the first client to type goes
    /// (detaches, its input ends, or it's dropped), once what it typed has
    /// been written.
    #[arg(long, requires = "stdin")]
    pub(crate) stdin_once: bool,
    /// Pass N more descriptors on to the container's process, from 3 up
    /// (after socket activation's, if `LISTEN_FDS` and `LISTEN_PID` give it
    /// any), as runc does.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub(crate) preserve_fds: usize,
    /// Have the container's monitor hold descriptor N (open in this
    /// process, above 2 and above those --preserve-fds passes on; a file, a
    /// directory or a socket) until the
    /// container has ended, then give it to the exit command, or close it:
    /// what keeps the caller's resources (a lock's, say) alive once the
    /// caller has gone. The container never has it. LABEL (letters, digits,
    /// : _ . and -; fd-N if not given) names it: the exit command is told
    /// the labels in `SHIPYARD_HOLD_LABELS`. Repeatable.
    #[arg(long = "hold-fd", value_name = "N[=LABEL]", value_parser = parse_hold)]
    pub(crate) hold_fd: Vec<(i32, String)>,
    /// Run PROGRAM (an absolute path) once the container has ended and its
    /// end is recorded: in a session of its own, with this command's
    /// environment, its output discarded, the held descriptors at 3 and up
    /// (`SHIPYARD_HOLDS` says how many, `SHIPYARD_HOLD_LABELS` their labels,
    /// comma-separated, in order), how the container ended in
    /// `SHIPYARD_EXIT_RECORD` (JSON), and the monitor's PID in
    /// `SHIPYARD_MONITOR_PID` (the monitor holds its copies of the
    /// descriptors until it exits). Not waited for.
    #[arg(long, value_name = "PROGRAM")]
    pub(crate) exit_command: Option<PathBuf>,
    /// An argument for the exit command (repeatable, in order).
    #[arg(
        long = "exit-command-arg",
        value_name = "ARG",
        requires = "exit_command",
        allow_hyphen_values = true
    )]
    pub(crate) exit_command_arg: Vec<String>,
    /// Run PROGRAM (an absolute path) beside the container: started once
    /// it's created (the program waits for its start, if it needs to), in a
    /// session of its own, with this command's environment, its output
    /// discarded and none of the held descriptors; killed (SIGKILL), with
    /// its whole process group, once the container has ended, before its end
    /// is recorded (and if the monitor dies). For health checks.
    #[arg(long, value_name = "PROGRAM")]
    pub(crate) companion: Option<PathBuf>,
    /// An argument for the companion (repeatable, in order).
    #[arg(
        long = "companion-arg",
        value_name = "ARG",
        requires = "companion",
        allow_hyphen_values = true
    )]
    pub(crate) companion_arg: Vec<String>,
    /// The container's ID.
    pub(crate) id: String,
}

/// `attach`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct AttachArgs {
    /// The keys that detach, Docker's way: `ctrl-` and a letter (or one of
    /// @[\]^_), or a character, separated by commas; empty: none.
    #[arg(long, value_name = "KEYS", default_value = "ctrl-p,ctrl-q")]
    pub(crate) detach_keys: String,
    /// Don't pass your input on (only watch the output).
    #[arg(long)]
    pub(crate) no_stdin: bool,
    /// The container's ID.
    pub(crate) id: String,
}

/// `exec`'s arguments (runc's).
#[derive(Debug, clap::Args)]
pub(crate) struct ExecArgs {
    /// A runc process.json: the whole process, instead of COMMAND.
    #[arg(short, long, value_name = "FILE", conflicts_with = "command")]
    pub(crate) process: Option<PathBuf>,
    /// Return once the command runs, leaving it running (it isn't stopped
    /// with this one).
    #[arg(short, long)]
    pub(crate) detach: bool,
    /// Give the command a terminal, made in the container: relayed to this
    /// one (raw, and sized as it), or with --console-socket sent there.
    #[arg(short, long)]
    pub(crate) tty: bool,
    /// Send the command's terminal's master to this UNIX socket (runc's
    /// protocol), instead of relaying it; needs a terminal (-t, or the
    /// process.json's). With -d -t, required.
    #[arg(long, value_name = "SOCKET")]
    pub(crate) console_socket: Option<PathBuf>,
    /// With --detach, write the command's PID (as the host sees it) to FILE.
    #[arg(long, value_name = "FILE", requires = "detach")]
    pub(crate) pid_file: Option<PathBuf>,
    /// The working directory, inside the container.
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<PathBuf>,
    /// Set an environment variable (KEY=VALUE) over the container's; may be
    /// given more than once.
    #[arg(short, long, value_name = "KEY=VALUE")]
    pub(crate) env: Vec<std::ffi::OsString>,
    /// The user, UID or UID:GID, as the container numbers them.
    #[arg(short, long, value_name = "UID[:GID]", value_parser = parse_exec_user)]
    pub(crate) user: Option<(u32, Option<u32>)>,
    /// A supplementary group, instead of the container's; may be given more
    /// than once.
    #[arg(short = 'g', long = "additional-gids", value_name = "GID")]
    pub(crate) additional_gids: Vec<u32>,
    /// Add a capability (`SYS_PTRACE`, or `CAP_SYS_PTRACE`), within the
    /// container's bounding set; may be given more than once.
    #[arg(short = 'c', long = "cap", value_name = "CAP")]
    pub(crate) caps: Vec<shipyard_runtime::security::Capability>,
    /// Set `no_new_privs` (on anyway if the container has it).
    #[arg(long)]
    pub(crate) no_new_privs: bool,
    /// Pass N more descriptors on to the command, from 3 up (after socket
    /// activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as runc
    /// does.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub(crate) preserve_fds: usize,
    /// The container's ID.
    pub(crate) id: String,
    /// The command and its arguments.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required_unless_present = "process",
        value_name = "COMMAND"
    )]
    pub(crate) command: Vec<std::ffi::OsString>,
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
pub(crate) struct CpArgs {
    /// Files copied in keep their owners (where the container has those
    /// IDs), instead of becoming the container's root's.
    #[arg(short, long)]
    pub(crate) archive: bool,
    /// Follow SRC if it's a symlink.
    #[arg(short = 'L', long)]
    pub(crate) follow_link: bool,
    /// ID:PATH, a host path, or - (a tar stream on stdin).
    #[arg(value_name = "SRC")]
    pub(crate) source: std::ffi::OsString,
    /// ID:PATH, a host path, or - (a tar stream on stdout).
    #[arg(value_name = "DEST")]
    pub(crate) destination: std::ffi::OsString,
}

/// `logs`' arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct LogsArgs {
    /// Then print more as it comes, until the container has ended (with
    /// --log-dir, until interrupted).
    #[arg(short, long)]
    pub(crate) follow: bool,
    /// Only the last N lines.
    #[arg(long, value_name = "N")]
    pub(crate) tail: Option<usize>,
    /// Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), or a
    /// duration ago (10m, 2h, 30s).
    #[arg(long, value_name = "TIME", value_parser = logs_output::parse_since)]
    pub(crate) since: Option<Timestamp>,
    /// Start each line with the time it was written (RFC 3339).
    #[arg(short, long)]
    pub(crate) timestamps: bool,
    /// Read the logs in DIR instead of a container's (one deleted
    /// since, say).
    #[arg(long, value_name = "DIR", conflicts_with = "id")]
    pub(crate) log_dir: Option<PathBuf>,
    /// The container's ID.
    #[arg(required_unless_present = "log_dir")]
    pub(crate) id: Option<String>,
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

/// How `ps` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PsFormat {
    /// PID, user ID and command, one process per line.
    Table,
    /// A JSON array of PIDs (what engines read).
    Json,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

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
