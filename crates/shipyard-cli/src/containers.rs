//! `shipyard start`, `attach`, `stop`, `restart`, `kill`, `update`, `rename`,
//! `wait`, `exec`, `cp`, `logs`, `ps`, `port`, `inspect` and `rm`: containers
//! by name, ID, serial or ID prefix. The engine does the work and returns
//! data; this prints it.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use shipyard_engine::attach::Settled;
use shipyard_engine::containers::{BoundPort, ContainerState, Listed};

/// How `ps` and `port --sockets` print.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Format {
    /// A table, as Docker's.
    #[default]
    Table,
    /// A JSON array, one object per container (or socket).
    Json,
}

/// `shipyard start`: each container started (one running is left as it
/// is), its reference printed as given; exits 125 if any couldn't be.
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
    let engine = crate::engine();
    let options = shipyard_engine::attach::Options {
        detach_keys: shipyard_runtime::attach::DetachKeys::parse(detach_keys)?,
        stdin: interactive,
        held: false,
    };
    let code = shipyard_engine::run::start_attached(&engine, reference, &options)?;
    Ok(attached_exit(code))
}

/// `shipyard attach`: until detached (0) or the container ends (its code).
pub(crate) fn attach(reference: &str, detach_keys: &str, stdin: bool) -> Result<ExitCode> {
    let engine = crate::engine();
    let options = shipyard_engine::attach::Options {
        detach_keys: shipyard_runtime::attach::DetachKeys::parse(detach_keys)?,
        stdin,
        held: false,
    };
    let code = shipyard_engine::attach::to_container(&engine, reference, &options)?;
    Ok(attached_exit(code))
}

/// Does `act` to each container, saying each reference as given once it's
/// done; exits 125 if any couldn't be.
fn each_said(
    references: &[String],
    mut act: impl FnMut(&shipyard_engine::Engine, &str) -> shipyard_engine::Result<()>,
) -> ExitCode {
    let engine = crate::engine();
    crate::each(references, |reference| {
        act(&engine, reference)?;
        writeln!(std::io::stdout().lock(), "{reference}")?;
        Ok(())
    })
}

/// How `stop` and `restart` stop a container: its own stop signal and
/// timeout unless given.
#[derive(Debug, clap::Args)]
pub(crate) struct StopArgs {
    /// Seconds to wait after the stop signal before SIGKILL (the container's
    /// own, its --stop-timeout, unless given).
    #[arg(short = 't', long = "time", value_name = "SECONDS")]
    time: Option<u64>,
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
            timeout: self.time.map(Duration::from_secs),
        }
    }
}

/// `shipyard stop`: each container stopped (one not up is left as it is),
/// its reference printed as given once it has ended; exits 125 if any
/// couldn't be.
pub(crate) fn stop(references: &[String], options: shipyard_engine::run::StopOptions) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::stop(engine, reference, options).map(drop)
    })
}

/// `shipyard restart`: each container stopped, then started, its reference
/// printed as given; exits 125 if any couldn't be.
pub(crate) fn restart(
    references: &[String],
    options: shipyard_engine::run::StopOptions,
) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::restart(engine, reference, options).map(drop)
    })
}

/// `shipyard kill`: `signal` sent to each container's main process, its
/// reference printed as given; exits 125 if any isn't running.
pub(crate) fn kill(references: &[String], signal: shipyard_runtime::process::Signal) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::kill(engine, reference, signal).map(drop)
    })
}

/// `shipyard update`: each container's restart policy set to `restart`,
/// its reference printed as given; exits 125 if any couldn't be.
pub(crate) fn update(
    references: &[String],
    restart: shipyard_engine::containers::RestartPolicy,
) -> ExitCode {
    let request = shipyard_engine::run::UpdateRequest::restart(restart);
    each_said(references, |engine, reference| {
        shipyard_engine::run::update(engine, reference, &request).map(drop)
    })
}

/// `shipyard rename`: the container renamed; nothing printed (as Docker).
pub(crate) fn rename(reference: &str, new_name: &str) -> Result<ExitCode> {
    let engine = crate::engine();
    shipyard_engine::run::rename(&engine, reference, new_name)?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard exec`'s flags.
#[derive(Debug, clap::Args)]
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
    /// Set an environment variable for it (KEY=VALUE; repeatable).
    #[arg(short, long, value_name = "KEY=VALUE")]
    env: Vec<std::ffi::OsString>,
    /// Its working directory, in the container.
    #[arg(short, long, value_name = "DIR")]
    workdir: Option<std::path::PathBuf>,
    /// The container: its name, ID, serial or ID prefix.
    container: String,
    /// The command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<std::ffi::OsString>,
}

/// `shipyard exec`: exits with the command's code (126 or 127 if it
/// couldn't be run), 0 once it runs detached, 125 for Shipyard's own
/// failures.
pub(crate) fn exec(args: ExecArgs) -> ExitCode {
    let engine = crate::engine();
    let request = shipyard_engine::exec::ExecRequest {
        args: args.command,
        interactive: args.interactive,
        tty: args.tty,
        detach: args.detach,
        user: args.user,
        env: args.env,
        workdir: args.workdir,
        stop: shipyard_runtime::process::StopPolicy::default(),
    };
    match shipyard_engine::exec::in_container(&engine, &args.container, &request) {
        Ok(shipyard_engine::exec::Ran::Exited(code)) => ExitCode::from(code),
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => crate::report(&error, shipyard_engine::exec::exit_code(&error)),
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
    /// From: CONTAINER:PATH, a host path, or - (a tar stream on stdin).
    source: std::ffi::OsString,
    /// To: CONTAINER:PATH, a host path, or - (a tar stream on stdout).
    destination: std::ffi::OsString,
}

/// `shipyard cp`: the copy, its container by name, running or stopped;
/// what it left out or changed said as warnings.
pub(crate) fn cp(args: &CpArgs) -> Result<ExitCode> {
    use shipyard_engine::cp::{CpOptions, Operand, Streams, Transfer};
    use std::io::IsTerminal as _;
    let engine = crate::engine();
    let transfer = Transfer::of(
        Operand::parse(&args.source),
        Operand::parse(&args.destination),
    )?;
    // As tar does: an archive on a terminal is noise (and escape sequences
    // a container chose).
    if matches!(transfer, Transfer::Out { to: None, .. }) && std::io::stdout().is_terminal() {
        anyhow::bail!("refusing to write a tar archive to a terminal (redirect it)");
    }
    let options = CpOptions {
        archive: args.archive,
        follow_link: args.follow_link,
    };
    let mut stdout = std::io::stdout().lock();
    let mut streams = Streams {
        input: &mut std::io::stdin().lock(),
        output: &mut stdout,
    };
    let copied = shipyard_engine::cp::copy_by_name(&engine, &transfer, &mut streams, options)?;
    stdout.flush()?;
    drop(stdout);
    for warning in copied.warnings {
        crate::warn(warning)?;
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
    /// Only the last N lines (across its starts).
    #[arg(long, value_name = "N")]
    tail: Option<usize>,
    /// Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix
    /// seconds, or a duration ago (10m, 2h, 30s, 3d).
    #[arg(long, value_name = "TIME", value_parser = crate::logs_output::parse_since)]
    since: Option<shipyard_runtime::logs::time::Timestamp>,
    /// Start each line with the time it was written (RFC 3339).
    #[arg(short, long)]
    timestamps: bool,
    /// The container: its name, ID, serial or ID prefix.
    container: String,
}

/// `shipyard logs`: the container's logs, every start's, as `args` say.
pub(crate) fn logs(args: &LogsArgs) -> Result<ExitCode> {
    let engine = crate::engine();
    let options = shipyard_engine::logs::Options {
        tail: args.tail,
        since: args.since,
        follow: args.follow,
        identity: None,
    };
    let skipped = crate::logs_output::printed(args.timestamps, |sink| {
        shipyard_engine::logs::of_container(&engine, &args.container, &options, sink)
    })?;
    if let Some(warning) = crate::logs_output::skipped_warning(skipped) {
        crate::warn(warning)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard wait`: each container's exit code once it has ended, one per
/// line; exits 125 if any couldn't be waited for.
pub(crate) fn wait(references: &[String]) -> ExitCode {
    let engine = crate::engine();
    crate::each(references, |reference| {
        let code = shipyard_engine::run::wait(&engine, reference)?;
        writeln!(std::io::stdout().lock(), "{code}")?;
        Ok(())
    })
}

/// `shipyard rm`: each container removed (a running one only with
/// `force`), its reference printed as given; exits 125 if any couldn't be.
pub(crate) fn rm(references: &[String], force: bool) -> ExitCode {
    each_said(references, |engine, reference| {
        shipyard_engine::run::remove(engine, reference, force).map(drop)
    })
}

/// `shipyard inspect`: the containers as a JSON array (the ones found);
/// exits 125 if any wasn't.
pub(crate) fn inspect(references: &[String]) -> Result<ExitCode> {
    let engine = crate::engine();
    let mut found = Vec::new();
    let code = crate::each(references, |reference| {
        found.push(shipyard_engine::containers::inspect(&engine, reference)?);
        Ok(())
    });
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &found)?;
    writeln!(out)?;
    Ok(code)
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
    let engine = crate::engine();
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
            return Ok(crate::report(
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
pub(crate) fn sockets(reference: &str, format: Format) -> Result<ExitCode> {
    let found = shipyard_engine::containers::sockets(&crate::engine(), reference)?;
    let mut out = std::io::stdout().lock();
    match format {
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, &found.sockets)?;
            writeln!(out)?;
        }
        Format::Table => {
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
            crate::table(
                &mut out,
                &["PROTO", "LOCAL", "REMOTE", "STATE", "PEER"],
                &rows,
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard ps`: the containers up (all of them with `all`), as a table,
/// their IDs alone (`quiet`), or JSON.
pub(crate) fn ps(all: bool, quiet: bool, format: Format) -> Result<ExitCode> {
    let summaries = shipyard_engine::containers::summaries(&crate::engine(), all)?;
    let now = SystemTime::now();
    let mut out = std::io::stdout().lock();
    if quiet {
        for summary in &summaries {
            writeln!(out, "{}", summary.record.id.short())?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    match format {
        Format::Json => {
            let rows: Vec<Listed> = summaries.iter().map(Listed::from).collect();
            serde_json::to_writer_pretty(&mut out, &rows)?;
            writeln!(out)?;
        }
        Format::Table => {
            let rows: Vec<Vec<String>> = summaries
                .iter()
                .map(|summary| {
                    let record = &summary.record;
                    vec![
                        record.id.short().to_owned(),
                        record
                            .config
                            .image
                            .clone()
                            .unwrap_or_else(|| "-".to_owned()),
                        truncated(&format!("\"{}\"", summary.command().join(" ")), 22),
                        format!("{} ago", ago(record.created, now)),
                        with_network(
                            with_health(
                                status(&summary.state, now),
                                summary.health.as_ref().map(|health| health.status),
                            ),
                            summary.network_lost.is_some(),
                        ),
                        // Only while it runs (or is paused), as `port`: an
                        // ended one, or one waiting to restart, holds none.
                        if matches!(
                            summary.state,
                            ContainerState::Running { .. } | ContainerState::Paused
                        ) {
                            ports(&record.ports)
                        } else {
                            String::new()
                        },
                        record.name.as_str().to_owned(),
                    ]
                })
                .collect();
            crate::table(
                &mut out,
                &[
                    "CONTAINER ID",
                    "IMAGE",
                    "COMMAND",
                    "CREATED",
                    "STATUS",
                    "PORTS",
                    "NAMES",
                ],
                &rows,
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `text`, cut to `width` characters with an ellipsis if it's longer.
fn truncated(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// A state as `ps` says it: `Up 3 minutes`, `Exited (0) 2 hours ago`.
fn status(state: &ContainerState, now: SystemTime) -> String {
    match state {
        ContainerState::Running { since: Some(since) } => format!("Up {}", ago(*since, now)),
        ContainerState::Running { since: None } => "Up".to_owned(),
        ContainerState::Paused => "Paused".to_owned(),
        ContainerState::Stopping => "Stopping".to_owned(),
        // Docker's: how it last ended, and when.
        ContainerState::Exited { code, finished, .. } => ended("Exited", *code, *finished, now),
        ContainerState::Restarting { code, finished, .. } => {
            ended("Restarting", *code, *finished, now)
        }
        _ => "Created".to_owned(),
    }
}

/// `status` with what the health checks say, as Docker: `Up 3 minutes
/// (healthy)`, `(unhealthy)`, `(health: starting)`.
fn with_health(status: String, health: Option<shipyard_engine::health::Status>) -> String {
    use shipyard_engine::health::Status;
    match health {
        None => status,
        Some(Status::Starting) => format!("{status} (health: starting)"),
        Some(said) => format!("{status} ({said})"),
    }
}

/// `status` with ` (no network)` if this start lost its network (its pasta
/// died while it ran).
fn with_network(status: String, lost: bool) -> String {
    if lost {
        format!("{status} (no network)")
    } else {
        status
    }
}

/// `WORD (CODE) N ago`: `?` for a code not known, no time for none.
fn ended(word: &str, code: Option<i32>, finished: Option<SystemTime>, now: SystemTime) -> String {
    let code = code.map_or_else(|| "?".to_owned(), |code| code.to_string());
    match finished {
        Some(finished) => format!("{word} ({code}) {} ago", ago(finished, now)),
        None => format!("{word} ({code})"),
    }
}

/// How long ago `then` was, as Docker says it: `Less than a second`,
/// `5 seconds`, `About a minute`, `3 minutes`, `About an hour`, `2 hours`,
/// `4 days`, `2 weeks`, `3 months`, `2 years`.
fn ago(then: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    let plural =
        |count: u64, unit: &str| format!("{count} {unit}{}", if count == 1 { "" } else { "s" });
    match seconds {
        0 => "Less than a second".to_owned(),
        1..60 => plural(seconds, "second"),
        60..120 => "About a minute".to_owned(),
        120..3600 => plural(seconds / 60, "minute"),
        3600..7200 => "About an hour".to_owned(),
        7200..172_800 => plural(seconds / 3600, "hour"),
        172_800..1_209_600 => plural(seconds / 86_400, "day"),
        1_209_600..5_184_000 => plural(seconds / 604_800, "week"),
        5_184_000..63_072_000 => plural(seconds / 2_592_000, "month"),
        _ => plural(seconds / 31_536_000, "year"),
    }
}

/// The ports, as `ps` shows them: `0.0.0.0:8080->80/tcp, …`.
fn ports(ports: &[BoundPort]) -> String {
    ports
        .iter()
        .map(|port| {
            let host = match port.host_ip {
                std::net::IpAddr::V6(ip) => format!("[{ip}]"),
                std::net::IpAddr::V4(ip) => ip.to_string(),
            };
            format!(
                "{host}:{}->{}/{}",
                port.host_port, port.container_port, port.protocol
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
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
        assert_eq!(ports(&[v4, v6]), "0.0.0.0:8080->80/tcp, [::]:8080->80/tcp");
    }

    #[test]
    fn ago_reads_as_dockers() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let before = |seconds| now - Duration::from_secs(seconds);
        for (seconds, said) in [
            (0, "Less than a second"),
            (1, "1 second"),
            (59, "59 seconds"),
            (60, "About a minute"),
            (180, "3 minutes"),
            (3600, "About an hour"),
            (7200, "2 hours"),
            (2 * 86_400, "2 days"),
            (14 * 86_400, "2 weeks"),
            (90 * 86_400, "3 months"),
            (800 * 86_400, "2 years"),
        ] {
            assert_eq!(ago(before(seconds), now), said, "{seconds}");
        }
        assert_eq!(ago(now + Duration::from_secs(5), now), "Less than a second");
    }

    #[test]
    fn states_read_as_dockers() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let since = Some(now - Duration::from_secs(180));
        assert_eq!(
            status(&ContainerState::Running { since }, now),
            "Up 3 minutes"
        );
        assert_eq!(status(&ContainerState::Created, now), "Created");
        let exited = ContainerState::Exited {
            code: Some(137),
            oom: None,
            finished: Some(now - Duration::from_secs(7200)),
        };
        assert_eq!(status(&exited, now), "Exited (137) 2 hours ago");
        let unknown = ContainerState::Exited {
            code: None,
            oom: None,
            finished: None,
        };
        assert_eq!(status(&unknown, now), "Exited (?)");
        let restarting = ContainerState::Restarting {
            code: Some(1),
            finished: Some(now - Duration::from_secs(3)),
            attempt: 2,
            at: Some(now),
        };
        assert_eq!(status(&restarting, now), "Restarting (1) 3 seconds ago");
    }

    /// Health as Docker adds it to the status.
    #[test]
    fn health_reads_as_dockers() {
        use shipyard_engine::health::Status;
        let up = || "Up 3 minutes".to_owned();
        assert_eq!(with_health(up(), None), "Up 3 minutes");
        assert_eq!(
            with_health(up(), Some(Status::Starting)),
            "Up 3 minutes (health: starting)"
        );
        assert_eq!(
            with_health(up(), Some(Status::Healthy)),
            "Up 3 minutes (healthy)"
        );
        assert_eq!(
            with_health(up(), Some(Status::Unhealthy)),
            "Up 3 minutes (unhealthy)"
        );
        // A lost network after any health.
        assert_eq!(
            with_network(with_health(up(), Some(Status::Healthy)), true),
            "Up 3 minutes (healthy) (no network)"
        );
        assert_eq!(with_network(up(), false), "Up 3 minutes");
    }

    #[test]
    fn long_commands_are_cut() {
        assert_eq!(truncated("short", 22), "short");
        let cut = truncated("\"sh -c 'while true; do sleep 1; done'\"", 22);
        assert_eq!(cut.chars().count(), 22);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn ports_read_as_dockers() {
        let bound = [
            BoundPort::new("0.0.0.0".parse().unwrap(), 8080, 80, "tcp"),
            BoundPort::new("::".parse().unwrap(), 53, 53, "udp"),
        ];
        assert_eq!(ports(&bound), "0.0.0.0:8080->80/tcp, [::]:53->53/udp");
    }
}
