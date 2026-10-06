//! `shipyard-runtime`: the OCI runtime command line, as runc offers it.
//!
//! A container's life in separate calls: `create` sets it up from a bundle
//! and holds it before its program runs; `start` lets it run; `state`
//! reports it as JSON; `kill` signals it; `delete` removes it. Engines and
//! the OCI conformance suite drive a runtime this way. The work is in
//! `shipyard_runtime::container`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use shipyard_runtime::container::{self, DEFAULT_ROOT};
use shipyard_runtime::process::{self, Signal};

/// Runs containers from OCI bundles, one lifecycle step per call.
#[derive(Debug, Parser)]
#[command(name = "shipyard-runtime", version)]
struct Cli {
    /// Directory for the containers' state.
    #[arg(long, global = true, value_name = "DIR", default_value = DEFAULT_ROOT)]
    root: PathBuf,

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
    Create {
        /// The bundle: a directory with config.json.
        #[arg(short, long, value_name = "DIR", default_value = ".")]
        bundle: PathBuf,
        /// Write the container's init PID to FILE.
        #[arg(long, value_name = "FILE")]
        pid_file: Option<PathBuf>,
        /// A socket for the container's terminal (not supported yet).
        #[arg(long, value_name = "SOCKET")]
        console_socket: Option<PathBuf>,
        /// The container's ID.
        id: String,
    },
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
        /// The container's ID.
        id: String,
        /// A name (TERM, SIGKILL) or a number.
        #[arg(default_value = "TERM")]
        signal: Signal,
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

fn main() -> ExitCode {
    // Before anything else: this binary is also the containers' init and
    // monitor, started again with hidden arguments.
    process::init_entry_point();
    container::monitor_entry_point();
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
    let root = &cli.root;
    match &cli.command {
        Command::Create {
            bundle,
            pid_file,
            console_socket,
            id,
        } => {
            if console_socket.is_some() {
                anyhow::bail!("--console-socket: terminals come in a later version");
            }
            let created = container::create(root, id, bundle)?;
            for warning in &created.warnings {
                report(cli, Level::Warning, warning);
            }
            if let Some(file) = pid_file
                && let Err(err) = write_pid_file(file, created.pid)
            {
                // `create` fails, so nothing is left (as runc does): a
                // caller told "failed" would otherwise find the ID taken.
                let _ = container::delete(root, id, true);
                return Err(err);
            }
        }
        Command::Start { id } => container::start(root, id)?,
        Command::State { id } => {
            let state = container::state(root, id)?;
            let json = serde_json::to_string_pretty(&state)?;
            writeln!(std::io::stdout().lock(), "{json}")?;
        }
        Command::Kill { id, signal } => container::kill(root, id, *signal)?,
        Command::Delete { force, id } => container::delete(root, id, *force)?,
    }
    Ok(())
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
        assert_eq!(cli.root, Path::new("/r"));
        assert!(matches!(cli.command, Command::Create { ref id, .. } if id == "c1"));
        let cli = Cli::try_parse_from(["shipyard-runtime", "kill", "c1", "KILL"]).unwrap();
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
