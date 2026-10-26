//! `shipyard start`, `attach`, `stop`, `restart`, `kill`, `update`, `rename`,
//! `wait`, `exec`, `cp`, `logs`, `ps`, `port`, `inspect` and `rm`: containers
//! by name, ID, serial or ID prefix. The engine does the work and returns
//! data; this prints it.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use shipyard_engine::attach::Settled;
use shipyard_engine::containers::BoundPort;

use crate::format::rows::{self, ContainerRow};

/// How `port --sockets` prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SocketsFormat {
    /// A table, as Docker's.
    #[default]
    Table,
    /// A JSON array, one object per container (or socket).
    Json,
}

/// `shipyard start`: each container started (one running is left as it
/// is), its reference printed as given; exits 1 if any couldn't be.
pub(crate) fn start(references: &[String]) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::start(engine, reference).map(drop)
    })
}

/// What an attachment exits with: the container's code, or 0 if it was
/// left running.
fn attached_exit(settled: Settled) -> ExitCode {
    match settled {
        Settled::Left => ExitCode::SUCCESS,
        Settled::Exited(code) => ExitCode::from(
            u8::try_from(code).unwrap_or(shipyard_runtime::process::EXIT_ENGINE_ERROR),
        ),
    }
}

/// `shipyard start -a`: the container started with this attached, before
/// its command can write anything; exits with its code, or 0 detached.
pub(crate) fn start_attached(
    references: &[String],
    detach_keys: &str,
    interactive: bool,
) -> Result<ExitCode> {
    let [reference] = references else {
        anyhow::bail!("start -a takes one container");
    };
    let engine = crate::reporter::engine();
    let options = shipyard_engine::attach::AttachOptions {
        detach_keys: shipyard_runtime::attach::DetachKeys::parse(detach_keys)?,
        stdin: interactive,
        held: false,
        output: shipyard_engine::attach::Output::BOTH,
    };
    let code = shipyard_engine::run::start_attached(&engine, reference, &options)?;
    Ok(attached_exit(code))
}

/// `shipyard attach`: until detached (0) or the container ends (its code).
pub(crate) fn attach(reference: &str, detach_keys: &str, stdin: bool) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let options = shipyard_engine::attach::AttachOptions {
        detach_keys: shipyard_runtime::attach::DetachKeys::parse(detach_keys)?,
        stdin,
        held: false,
        output: shipyard_engine::attach::Output::BOTH,
    };
    let code = shipyard_engine::attach::to_container(&engine, reference, &options)?;
    Ok(attached_exit(code))
}

/// Does `act` to each container, saying each reference as given once it's
/// done; exits 1 if any couldn't be.
fn each_said(
    references: &[String],
    mut act: impl FnMut(&shipyard_engine::Engine, &str) -> shipyard_engine::Result<()>,
) -> ExitCode {
    let engine = crate::reporter::engine();
    crate::output::each(references, |reference| {
        act(&engine, reference)?;
        writeln!(std::io::stdout().lock(), "{reference}")?;
        Ok(())
    })
}

/// The containers a command acts on, each by any reference: what start,
/// stop, restart, update, kill, wait, inspect and rm share.
#[derive(Debug, clap::Args)]
pub(crate) struct ContainerRefs {
    /// The containers: names, IDs, serials or ID prefixes.
    #[arg(value_name = "CONTAINER", required = true)]
    pub(crate) containers: Vec<String>,
}

/// How `stop` and `restart` stop a container: its own stop signal and
/// timeout unless given.
#[derive(Debug, clap::Args)]
pub(crate) struct StopArgs {
    /// Seconds to wait after the stop signal before SIGKILL (the container's
    /// own, its --stop-timeout, unless given); -1 waits for as long as it
    /// takes, as Docker's.
    #[arg(
        short = 't',
        long = "timeout",
        visible_alias = "time",
        value_name = "SECONDS",
        allow_hyphen_values = true,
        value_parser = clap::value_parser!(i64).range(-1..)
    )]
    time: Option<i64>,
    /// The signal sent first (the container's own, its --stop-signal, unless
    /// given).
    #[arg(short, long, value_name = "SIGNAL")]
    signal: Option<shipyard_runtime::process::Signal>,
}

impl StopArgs {
    /// The engine's options.
    pub(crate) fn options(&self) -> shipyard_engine::run::StopOptions {
        shipyard_engine::run::StopOptions {
            signal: self.signal,
            // -1: for as long as it takes (the engine's rule, as the API's).
            timeout: self.time.map(shipyard_engine::run::StopOptions::timeout_of),
        }
    }
}

/// `shipyard stop`: each container stopped (one not up is left as it is),
/// its reference printed as given once it has ended; exits 1 if any
/// couldn't be.
pub(crate) fn stop(references: &[String], options: shipyard_engine::run::StopOptions) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::stop(engine, reference, options).map(drop)
    })
}

/// `shipyard restart`: each container stopped, then started, its reference
/// printed as given; exits 1 if any couldn't be.
pub(crate) fn restart(
    references: &[String],
    options: shipyard_engine::run::StopOptions,
) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::restart(engine, reference, options).map(drop)
    })
}

/// `shipyard kill`: `signal` sent to each container's main process, its
/// reference printed as given; exits 1 if any isn't running.
pub(crate) fn kill(references: &[String], signal: shipyard_runtime::process::Signal) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::kill(engine, reference, signal).map(drop)
    })
}

/// `shipyard update`: each container's restart policy and limits changed
/// as asked, its reference printed as given; exits 1 if any couldn't be.
pub(crate) fn update(args: &crate::cli::UpdateArgs) -> ExitCode {
    let mut request = shipyard_engine::run::UpdateRequest::default();
    request.restart = args.restart;
    let request = request.with_limits(args.limits.docker());
    each_said(&args.refs.containers, |engine, reference| {
        shipyard_engine::run::update(engine, reference, &request).map(drop)
    })
}

/// `shipyard pause` (`freeze`) and `unpause`: each container frozen or
/// thawed, its reference printed as given; exits 1 if any couldn't be.
pub(crate) fn pause(references: &[String], freeze: bool) -> ExitCode {
    each_said(references, |engine, reference| {
        if freeze {
            shipyard_engine::run::pause(engine, reference).map(drop)
        } else {
            shipyard_engine::run::unpause(engine, reference).map(drop)
        }
    })
}

/// `shipyard rename`: the container renamed; nothing printed (as Docker).
pub(crate) fn rename(reference: &str, new_name: &str) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    shipyard_engine::run::rename(&engine, reference, new_name)?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard exec`'s flags.
#[derive(Debug, clap::Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap's switches: each bool is one command-line flag"
)]
pub(crate) struct ExecArgs {
    /// Pass your input on to it (without: it reads end-of-file).
    #[arg(short, long)]
    interactive: bool,
    /// Give it a terminal.
    #[arg(short, long)]
    tty: bool,
    /// Return once it runs (it goes on).
    #[arg(short, long, conflicts_with = "tty")]
    detach: bool,
    /// Run it as this user, and group if given: names (from the
    /// container's own /etc/passwd and /etc/group) or numbers.
    #[arg(short, long, value_name = "USER[:GROUP]")]
    user: Option<String>,
    /// Set an environment variable for it (KEY=VALUE, or a bare KEY to
    /// copy your shell's value; repeatable).
    #[arg(short, long, value_name = "KEY[=VALUE]")]
    env: Vec<std::ffi::OsString>,
    /// Read variables from a file, as `run --env-file` does (before --env,
    /// which wins). Repeatable.
    #[arg(long, value_name = "FILE")]
    env_file: Vec<std::path::PathBuf>,
    /// Not supported yet: an exec's command ends with this session, so
    /// there's nothing to detach from (refused).
    #[arg(long, value_name = "KEYS")]
    detach_keys: Option<String>,
    /// Not supported: give the container the capabilities it needs
    /// (refused).
    #[arg(long)]
    privileged: bool,
    /// Its working directory, in the container.
    #[arg(short, long, value_name = "DIR")]
    workdir: Option<std::path::PathBuf>,
    /// Where to write its PID (as the host sees it) once it runs: the API
    /// service's, for an exec's inspect (F117). Not Docker's: hidden.
    #[arg(long, value_name = "FILE", hide = true)]
    pid_file: Option<std::path::PathBuf>,
    /// The container: its name, ID, serial or ID prefix.
    container: String,
    /// The command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<std::ffi::OsString>,
}

/// `shipyard exec`: exits with the command's code (126 or 127 if it
/// couldn't be run), 0 once it runs detached, 1 for Shipyard's own
/// failures (Docker's), 125 for what it can't take as asked (a refused
/// flag, an env file that can't be read: usage errors, as clap's).
pub(crate) fn exec(args: ExecArgs) -> ExitCode {
    let usage = |message: &str| {
        let error = clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            format!("{message}\n"),
        );
        let _ = error.print();
        ExitCode::from(shipyard_runtime::process::EXIT_ENGINE_ERROR)
    };
    if args.detach_keys.is_some() {
        return usage(
            "--detach-keys isn't supported for exec yet: its command ends with this session, \
             so there's nothing to detach from (use -d to run it in the background)",
        );
    }
    if args.privileged {
        return usage(
            "--privileged isn't supported: give the container the capabilities (--cap-add) it \
             needs",
        );
    }
    // The files' variables first: `--env` wins (Docker's order).
    let mut env = Vec::new();
    for path in &args.env_file {
        match crate::run::files::env_file(path) {
            Ok(entries) => env.extend(entries),
            Err(error) => return usage(&error.to_string()),
        }
    }
    env.extend(args.env);
    let engine = crate::reporter::engine();
    let caller_env = crate::run::caller_env_of(&env);
    let request = shipyard_engine::exec::ExecRequest {
        args: args.command,
        interactive: args.interactive,
        tty: args.tty,
        detach: args.detach,
        user: args.user,
        env,
        caller_env,
        workdir: args.workdir,
        stop: shipyard_runtime::process::StopPolicy::default(),
        pid_file: args.pid_file,
    };
    match shipyard_engine::exec::in_container(&engine, &args.container, &request) {
        Ok(shipyard_engine::exec::Ran::Exited(code)) => ExitCode::from(code),
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            // The command's own 126 or 127, or Shipyard's failure.
            let code = match shipyard_engine::exec::exit_code(&error) {
                shipyard_runtime::process::EXIT_ENGINE_ERROR => crate::reporter::EXIT_FAILURE,
                code => code,
            };
            crate::reporter::report(&error, code)
        }
    }
}

/// `shipyard cp`'s flags.
#[derive(Debug, clap::Args)]
pub(crate) struct CpArgs {
    /// Keep the files' owners (where the container has those IDs).
    #[arg(short, long)]
    archive: bool,
    /// Follow a source that's a symbolic link.
    #[arg(short = 'L', long)]
    follow_link: bool,
    /// Taken, as Docker's (no progress): Shipyard prints none.
    #[arg(short, long)]
    quiet: bool,
    /// From: CONTAINER:PATH, a host path, or - (a tar stream on stdin).
    source: std::ffi::OsString,
    /// To: CONTAINER:PATH, a host path, or - (a tar stream on stdout).
    destination: std::ffi::OsString,
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only a Mac forwards; its rules are tested everywhere"
    )
)]
impl ExecArgs {
    /// What a forwarded `exec` needs of a Mac (step 10.3b): its input with
    /// `-i`, its env files (host paths), and the variables a bare `-e KEY`
    /// (or an env file's bare `KEY`) copies.
    pub(crate) fn needs(&self) -> crate::forward::Needs {
        let from_files: Vec<std::ffi::OsString> = self
            .env_file
            .iter()
            .filter_map(|path| crate::run::files::env_file(path).ok())
            .flatten()
            .collect();
        let mut copied_env = crate::run::copied_env(&from_files);
        copied_env.extend(crate::run::copied_env(&self.env));
        crate::forward::Needs {
            input: self.interactive,
            paths: self
                .env_file
                .iter()
                .map(crate::forward::HostPath::existing)
                .collect(),
            copied_env,
            ..crate::forward::Needs::default()
        }
    }
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only a Mac forwards; its rules are tested everywhere"
    )
)]
impl CpArgs {
    /// Whether it writes a tar stream to stdout (`-` as its destination).
    pub(crate) fn writes_to_stdout(&self) -> bool {
        self.destination == "-"
    }

    /// What a forwarded `cp` needs of a Mac (step 10.3b): its input when it
    /// reads a tar stream (`-`), and its host side, which the copy may make
    /// as its destination.
    pub(crate) fn needs(&self) -> crate::forward::Needs {
        use shipyard_engine::cp::Operand;
        let host = |operand: &std::ffi::OsString, new| match Operand::parse(operand) {
            Operand::Host(path) if new => Some(crate::forward::HostPath::new_or_existing(path)),
            Operand::Host(path) => Some(crate::forward::HostPath::existing(path)),
            _ => None,
        };
        crate::forward::Needs {
            input: self.source == "-",
            paths: [host(&self.source, false), host(&self.destination, true)]
                .into_iter()
                .flatten()
                .collect(),
            ..crate::forward::Needs::default()
        }
    }
}

/// `shipyard export`'s flags.
#[derive(Debug, clap::Args)]
pub(crate) struct ExportArgs {
    /// Write the archive to FILE instead of standard output.
    #[arg(short, long, value_name = "FILE")]
    pub(crate) output: Option<std::path::PathBuf>,
    /// The container: its name, ID, serial or ID prefix.
    pub(crate) container: String,
}

/// `shipyard export`: the container's own filesystem as one tar archive,
/// to FILE or standard output (never a terminal, as `cp`'s); what was left
/// out said as a warning.
pub(crate) fn export(args: &ExportArgs) -> Result<ExitCode> {
    use std::io::IsTerminal as _;
    let engine = crate::reporter::engine();
    if let Some(path) = &args.output {
        crate::output::write_archive_to(path, |out| {
            Ok(shipyard_engine::cp::export_by_name(
                &engine,
                &args.container,
                out,
            )?)
        })?;
    } else {
        if std::io::stdout().is_terminal() {
            return Err(crate::output::tar_to_terminal_refused(
                crate::output::GIVE_OUTPUT,
            ));
        }
        let mut stdout = std::io::stdout().lock();
        shipyard_engine::cp::export_by_name(&engine, &args.container, &mut stdout)?;
        stdout.flush()?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard cp`: the copy, its container by name, running or stopped;
/// what it left out or changed said as warnings.
pub(crate) fn cp(args: &CpArgs) -> Result<ExitCode> {
    use shipyard_engine::cp::{CpOptions, CpStreams, Operand, Transfer};
    use std::io::IsTerminal as _;
    let engine = crate::reporter::engine();
    let transfer = Transfer::of(
        Operand::parse(&args.source),
        Operand::parse(&args.destination),
    )?;
    // As tar does: an archive on a terminal is noise (and escape sequences
    // a container chose).
    if matches!(transfer, Transfer::Out { to: None, .. }) && std::io::stdout().is_terminal() {
        return Err(crate::output::tar_to_terminal_refused(""));
    }
    let options = CpOptions {
        archive: args.archive,
        follow_link: args.follow_link,
    };
    let mut stdout = std::io::stdout().lock();
    let mut streams = CpStreams {
        input: &mut std::io::stdin().lock(),
        output: &mut stdout,
    };
    let copied = shipyard_engine::cp::copy_by_name(&engine, &transfer, &mut streams, options)?;
    stdout.flush()?;
    drop(stdout);
    for warning in copied.warnings {
        crate::reporter::warn(warning)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard logs`'s flags.
#[derive(Debug, clap::Args)]
pub(crate) struct LogsArgs {
    /// Then print more as it comes, until the container stops (or is
    /// started again).
    #[arg(short, long)]
    follow: bool,
    /// Only the last N lines (across its starts); `all` is every one, as
    /// Docker's.
    #[arg(short = 'n', long, value_name = "N", value_parser = parse_tail)]
    pub(crate) tail: Option<Tail>,
    /// Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix
    /// seconds, or a duration ago (10m, 2h, 30s, 3d).
    #[arg(long, value_name = "TIME", value_parser = shipyard_cli::logs_output::parse_since)]
    since: Option<shipyard_runtime::logs::time::Timestamp>,
    /// Only lines up to TIME (as --since takes it); a follow ends there.
    #[arg(long, value_name = "TIME", value_parser = shipyard_cli::logs_output::parse_since)]
    until: Option<shipyard_runtime::logs::time::Timestamp>,
    /// Start each line with the time it was written (RFC 3339).
    #[arg(short, long)]
    timestamps: bool,
    /// The container: its name, ID, serial or ID prefix.
    container: String,
}

/// `logs -n`'s count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tail {
    /// Every line (`all`).
    All,
    /// The last this many.
    Last(usize),
}

/// `logs -n`'s `N`: a number, or `all`.
fn parse_tail(s: &str) -> Result<Tail, String> {
    if s == "all" {
        return Ok(Tail::All);
    }
    s.parse()
        .map(Tail::Last)
        .map_err(|_| format!("{s}: give a number of lines, or all"))
}

/// `shipyard logs`: the container's logs, every start's, as `args` say.
pub(crate) fn logs(args: &LogsArgs) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let options = shipyard_engine::logs::LogsOptions {
        tail: match args.tail {
            Some(Tail::Last(lines)) => Some(lines),
            Some(Tail::All) | None => None,
        },
        since: args.since,
        until: args.until,
        follow: args.follow,
        identity: None,
        gone: Some(shipyard_engine::logs::Gone::new(
            shipyard_cli::pipe::stdout_gone,
        )),
    };
    let skipped = shipyard_cli::logs_output::printed(args.timestamps, |sink| {
        Ok(shipyard_engine::logs::of_container(
            &engine,
            &args.container,
            &options,
            sink,
        )?)
    })?;
    if let Some(warning) = shipyard_cli::logs_output::skipped_warning(skipped) {
        crate::reporter::warn(warning)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard wait`: each container's exit code once it has ended, one per
/// line; exits 1 if any couldn't be waited for.
pub(crate) fn wait(references: &[String]) -> ExitCode {
    let engine = crate::reporter::engine();
    crate::output::each(references, |reference| {
        let code = shipyard_engine::run::wait(&engine, reference)?;
        writeln!(std::io::stdout().lock(), "{code}")?;
        Ok(())
    })
}

/// `shipyard rm`: each container removed (a running one only with
/// `force`; its anonymous volumes too with `volumes`), its reference
/// printed as given; exits 1 if any couldn't be.
pub(crate) fn rm(references: &[String], options: shipyard_engine::run::RemoveOptions) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::remove_with(engine, reference, options).map(drop)
    })
}

/// `shipyard inspect`: each object named, as Docker shows it: the first
/// of a container, an image, a network and a volume that has the name (or
/// the `--type` asked), as `--format` asks (one JSON array without it);
/// containers in Shipyard's own view with `--shipyard`. One found nowhere
/// is said, the rest still shown, and the command exits 1.
pub(crate) fn inspect(args: &crate::cli::InspectArgs) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let kind = args.kind.map(Into::into);
    crate::output::shown(&args.names, args.shown.format.as_deref(), |name| {
        Ok(if args.shipyard {
            serde_json::to_value(shipyard_engine::containers::inspect(&engine, name)?)?
        } else {
            serde_json::to_value(shipyard_engine::docker::inspect_any(&engine, name, kind)?)?
        })
    })
}

/// A container's port as `port` takes it: `80`, `80/tcp` or `53/udp` (tcp
/// when not said).
pub(crate) fn parse_port(text: &str) -> Result<(u16, String), String> {
    let (number, protocol) = text.split_once('/').unwrap_or((text, "tcp"));
    let number: u16 = number
        .parse()
        .ok()
        .filter(|&number| number != 0)
        .ok_or_else(|| format!("{text:?} isn't a port: a number from 1 to 65535"))?;
    match protocol {
        "tcp" | "udp" => Ok((number, protocol.to_owned())),
        _ => Err(format!("{text:?}: the protocol is tcp or udp")),
    }
}

/// A host address and port, as `port` prints it: `0.0.0.0:8080`,
/// `[::]:8080`.
fn host_side(port: &BoundPort) -> String {
    match port.host_ip {
        std::net::IpAddr::V6(ip) => format!("[{ip}]:{}", port.host_port),
        std::net::IpAddr::V4(ip) => format!("{ip}:{}", port.host_port),
    }
}

/// `shipyard port`: the host ports the container publishes now, one line
/// each (`80/tcp -> 0.0.0.0:8080`); or, for one of its ports, only the
/// host side, and exit 1 if nothing is published there (Docker's).
pub(crate) fn port(reference: &str, wanted: Option<&(u16, String)>) -> Result<ExitCode> {
    let engine = crate::reporter::engine();
    let found = shipyard_engine::containers::ports(
        &engine,
        reference,
        wanted.map(|(number, protocol)| (*number, protocol.as_str())),
    )?;
    let mut out = std::io::stdout().lock();
    // By the container's port and protocol, as Docker sorts them (each
    // port's IPv4 side before its IPv6 one, as bound).
    let mut found = found;
    found
        .ports
        .sort_by(|a, b| (a.container_port, &a.protocol).cmp(&(b.container_port, &b.protocol)));
    match wanted {
        None => {
            for port in &found.ports {
                writeln!(
                    out,
                    "{}/{} -> {}",
                    port.container_port,
                    port.protocol,
                    host_side(port)
                )?;
            }
        }
        Some((number, protocol)) if found.ports.is_empty() => {
            drop(out);
            return Ok(crate::reporter::report(
                &format!(
                    "no public port '{number}/{protocol}' published for {}",
                    found.name
                ),
                1,
            ));
        }
        Some(_) => {
            for port in &found.ports {
                writeln!(out, "{}", host_side(port))?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard port --sockets`: the container's sockets, as a table (`PROTO
/// LOCAL REMOTE STATE PEER`) or JSON.
pub(crate) fn sockets(reference: &str, format: SocketsFormat) -> Result<ExitCode> {
    let found = shipyard_engine::containers::sockets(&crate::reporter::engine(), reference)?;
    let mut out = std::io::stdout().lock();
    match format {
        SocketsFormat::Json => {
            // What a terminal would act on escaped (C1, bidi: serde leaves
            // them), as `--format json` does (11.6b's review).
            let text = serde_json::to_string_pretty(&found.sockets)?;
            writeln!(out, "{}", crate::format::terminal_safe(text))?;
        }
        SocketsFormat::Table => {
            let rows: Vec<Vec<String>> = found
                .sockets
                .iter()
                .map(|sock| {
                    vec![
                        sock.protocol.to_owned(),
                        sock.local.to_string(),
                        sock.remote
                            .map_or_else(|| "-".to_owned(), |remote| remote.to_string()),
                        sock.state.to_owned(),
                        sock.peer.clone().unwrap_or_else(|| "-".to_owned()),
                    ]
                })
                .collect();
            crate::output::table(
                &mut out,
                &["PROTO", "LOCAL", "REMOTE", "STATE", "PEER"],
                &rows,
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard ps`: the containers up (all of them with `-a`), as Docker
/// lists them: newest first, filtered (`-f`), the last made only (`-n`,
/// `-l`), printed as `--format` asks (Docker's table without it), or
/// their IDs alone (`-q`).
pub(crate) fn ps(args: &crate::cli::PsArgs) -> Result<ExitCode> {
    use shipyard_engine::docker::filters::{ContainerFilters, Given};
    let filters = ContainerFilters::parse(Given::from_pairs(args.list.filter.iter().cloned()))?;
    let last = if args.latest {
        Some(1)
    } else {
        args.last
            .and_then(|last| usize::try_from(last).ok())
            .filter(|&last| last > 0)
    };
    let mut summaries = shipyard_engine::docker::list_containers_where(
        &crate::reporter::engine(),
        args.all || last.is_some(),
        &filters,
    )?;
    if let Some(last) = last {
        summaries.truncate(last);
    }
    let now = SystemTime::now();
    let rows: Vec<ContainerRow> = summaries
        .iter()
        .map(|summary| ContainerRow::new(summary, now, args.no_trunc))
        .collect();
    crate::format::print(&args.list, rows::CONTAINERS_TABLE, &rows)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `port`'s argument: a number, tcp unless said; nothing else.
    #[test]
    fn a_port_is_a_number_and_a_protocol() {
        assert_eq!(parse_port("80"), Ok((80, "tcp".to_owned())));
        assert_eq!(parse_port("53/udp"), Ok((53, "udp".to_owned())));
        assert_eq!(parse_port("443/tcp"), Ok((443, "tcp".to_owned())));
        for bad in ["0", "65536", "http", "80/sctp", "80/", "/tcp", ""] {
            assert!(parse_port(bad).is_err(), "{bad}");
        }
    }

    /// The host side, as Docker prints it: IPv6 in brackets.
    #[test]
    fn a_host_side_brackets_ipv6() {
        let v4 = BoundPort::new("0.0.0.0".parse().unwrap(), 8080, 80, "tcp");
        let v6 = BoundPort::new("::".parse().unwrap(), 8080, 80, "tcp");
        assert_eq!(host_side(&v4), "0.0.0.0:8080");
        assert_eq!(host_side(&v6), "[::]:8080");
    }
}

#[cfg(test)]
mod exec_tests {
    use clap::Parser as _;

    use crate::cli::{Cli, Command};

    /// A bare `-e KEY` is named to the Mac (it sends its value), as `run`'s
    /// is; the engine resolves it, as `run`'s.
    #[test]
    fn a_bare_env_key_is_copied_from_the_caller() {
        let cli = Cli::try_parse_from([
            "shipyard", "exec", "-e", "GIVEN=1", "-e", "COPIED", "web", "sh",
        ])
        .unwrap();
        let Command::Exec(args) = cli.command else {
            panic!("an exec");
        };
        assert_eq!(args.needs().copied_env, ["COPIED"]);
    }
}
