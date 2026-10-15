//! The `shipyard` command-line interface.
//!
//! This crate only parses flags, calls the engine (`shipyard-engine`) and
//! prints what it returns and reports; the work happens there and in
//! `shipyard-runtime`.

mod containers;
mod devices;
mod doctor;
mod events_output;
#[allow(
    dead_code,
    reason = "shared with `shipyard-runtime`, whose `update` uses what `run` doesn't"
)]
mod limits;
mod logs_output;
mod run;

use std::borrow::Cow;
use std::io::{BufRead, IsTerminal as _, Write};
use std::os::fd::AsFd as _;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use shipyard_engine::report::{Event, Progress, Reporter};
use shipyard_engine::{Engine, Paths, network};
use shipyard_runtime::process::{self, EXIT_ENGINE_ERROR};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{Format, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// A lightweight container engine.
#[derive(Debug, Parser)]
#[command(name = "shipyard", version, propagate_version = true)]
struct Cli {
    /// Show debug logs (same as --log-level debug).
    #[arg(short = 'D', long, global = true)]
    debug: bool,

    /// How much to log.
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<LogLevel>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_filter(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run an image in a container (its command, or COMMAND): on Linux, isolated in namespaces, with the image's files as its root. --no-image runs COMMAND on the host's files. With -d, in the background, by name.
    #[command(override_usage = "shipyard run [OPTIONS] IMAGE [COMMAND]...\n       \
                                shipyard run [OPTIONS] --no-image|--no-isolation|--rootfs DIR COMMAND...\n       \
                                shipyard run [OPTIONS] --bundle DIR")]
    Run(Box<run::RunCommand>),
    /// Make a container, to start later (`shipyard start`): its image pulled, nothing run (Linux).
    #[command(
        override_usage = "shipyard create [OPTIONS] IMAGE [COMMAND]...\n       \
                                shipyard create [OPTIONS] --rootfs DIR COMMAND..."
    )]
    Create(Box<run::CreateCommand>),
    /// Start containers, in the background (one running is left as it is),
    /// or one attached (-a).
    Start {
        /// Attach to it as it starts (one container): its output from its
        /// first byte, until it ends (its exit code) or Ctrl-P Ctrl-Q.
        #[arg(short, long)]
        attach: bool,
        /// With -a: pass your input on to it.
        #[arg(short, long, requires = "attach")]
        interactive: bool,
        /// With -a: the keys that detach (the container goes on), as
        /// attach's.
        #[arg(
            long,
            value_name = "KEYS",
            requires = "attach",
            default_value = "ctrl-p,ctrl-q"
        )]
        detach_keys: String,
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Attach to a running container: its output, and your input (unless
    /// --no-stdin), until it ends (its exit code) or the detach keys.
    Attach {
        /// The keys that detach (the container goes on): ctrl-p,ctrl-q, or
        /// others separated by commas (empty: none).
        #[arg(long, value_name = "KEYS", default_value = "ctrl-p,ctrl-q")]
        detach_keys: String,
        /// Don't pass your input on.
        #[arg(long)]
        no_stdin: bool,
        /// The container: its name, ID, serial or ID prefix.
        container: String,
    },
    /// Stop running containers: their stop signal, then SIGKILL once their
    /// stop timeout is up. Returns once each has ended.
    Stop {
        #[command(flatten)]
        how: containers::StopArgs,
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Stop containers (as `stop`), then start them again.
    Restart {
        #[command(flatten)]
        how: containers::StopArgs,
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Change containers' settings without making them again: their restart
    /// policy.
    Update {
        /// The restart policy: no, on-failure (or on-failure:N), always or
        /// unless-stopped.
        #[arg(long, value_name = "POLICY", required = true)]
        restart: shipyard_engine::containers::RestartPolicy,
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Send a signal to running containers' main process (SIGKILL unless
    /// told), without waiting.
    Kill {
        /// The signal: a name (KILL, SIGTERM) or a number.
        #[arg(short, long, value_name = "SIGNAL", default_value = "SIGKILL")]
        signal: shipyard_runtime::process::Signal,
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Rename a container (a running one's name on its networks too, at
    /// once).
    Rename {
        /// The container: its name, ID, serial or ID prefix.
        container: String,
        /// Its new name.
        new_name: String,
    },
    /// Run a command in a running container. Exits with its exit code (127
    /// if it isn't found, 126 if it can't be run).
    Exec(containers::ExecArgs),
    /// Copy files between a container (running or not) and the host:
    /// CONTAINER:PATH to a host path (or - for a tar stream on stdout), or
    /// back.
    Cp(containers::CpArgs),
    /// Print a container's logs: every start's, oldest first.
    Logs(containers::LogsArgs),
    /// Print what happens (containers made, started, ended; images pulled;
    /// volumes and networks made and removed) as it happens, or what did
    /// with --since.
    Events(events_output::EventsArgs),
    /// Wait for containers to end, and print each one's exit code.
    Wait {
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// List containers: the running ones, or all of them with -a.
    Ps {
        /// All containers, not only the ones up.
        #[arg(short, long)]
        all: bool,
        /// Only their IDs (short).
        #[arg(short, long, conflicts_with = "format")]
        quiet: bool,
        /// How to print them: a table, or JSON (one object per container).
        #[arg(long, value_enum)]
        format: Option<containers::Format>,
    },
    /// Show the host ports a container publishes (all of them, or those to
    /// one of its ports).
    Port {
        /// The container: its name, ID, serial or ID prefix.
        #[arg(value_name = "CONTAINER")]
        container: String,
        /// One of its ports: 80, 80/tcp or 53/udp (tcp if not said).
        #[arg(value_name = "PORT[/PROTO]", value_parser = containers::parse_port)]
        port: Option<(u16, String)>,
        /// Show its sockets instead: what it listens on, and its
        /// connections, each peer by its container's name where it's one
        /// (Linux).
        #[arg(long, conflicts_with = "port")]
        sockets: bool,
        /// How to show its sockets: a table, or JSON.
        #[arg(long, value_enum, requires = "sockets")]
        format: Option<containers::Format>,
    },
    /// Show containers in full, as JSON.
    Inspect {
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Remove containers (their writable layers too). A running one needs -f.
    Rm {
        /// Remove a running container too (it's killed first).
        #[arg(short, long)]
        force: bool,
        /// The containers: names, IDs, serials or ID prefixes.
        #[arg(value_name = "CONTAINER", required = true)]
        containers: Vec<String>,
    },
    /// Pull an image into the local store (and extract it, on Linux), so a
    /// run starts at once and needs no registry.
    Pull {
        /// Print nothing but the image's digest.
        #[arg(short, long)]
        quiet: bool,
        /// The image, like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`.
        #[arg(value_name = "IMAGE")]
        reference: String,
    },
    /// List images in the local store.
    Images,
    /// Remove one or more images from the local store (reference-counted; shared layers survive).
    Rmi {
        /// Untag an image even while a container uses it (a container keeps
        /// what it needs until it's removed; a foreground run's layers stay
        /// until it ends and a prune collects them).
        #[arg(short, long)]
        force: bool,
        /// The image references to remove.
        #[arg(value_name = "IMAGE", required = true)]
        images: Vec<String>,
    },
    /// Manage images.
    #[command(subcommand)]
    Image(ImageCommand),
    /// Manage named volumes.
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// Manage networks (bridges containers share; Linux, root).
    #[command(subcommand)]
    Network(NetworkCommand),
    /// System-wide information and maintenance.
    #[command(subcommand)]
    System(SystemCommand),
    /// Check whether this host can run containers, and what to fix.
    Doctor,
}

/// `shipyard volume …`.
#[derive(Debug, Subcommand)]
enum VolumeCommand {
    /// Create a named volume.
    Create {
        /// The volume's name.
        name: String,
    },
    /// List named volumes.
    Ls,
    /// Show a volume's details.
    Inspect {
        /// The volume's name.
        name: String,
    },
    /// Remove one or more named volumes (never one a running container
    /// uses).
    Rm {
        /// The volumes to remove.
        #[arg(value_name = "VOLUME", required = true)]
        names: Vec<String>,
    },
    /// Remove every named volume no container uses, running or not (asks first).
    Prune {
        /// Don't ask (needed where there's no terminal to ask on).
        #[arg(short, long)]
        force: bool,
    },
}

/// `shipyard network …`.
#[derive(Debug, Subcommand)]
enum NetworkCommand {
    /// Create a network: a bridge with a subnet of its own.
    Create {
        /// Its subnet, like `10.90.0.0/24` (a free one by default); once
        /// more with an IPv6 prefix, like `fd00:1::/64`, for IPv6 too.
        #[arg(long, value_name = "CIDR")]
        subnet: Vec<shipyard_network::IpNet>,
        /// Its gateway, the bridge's address (the subnet's first by
        /// default); once per family.
        #[arg(long, value_name = "IP")]
        gateway: Vec<std::net::IpAddr>,
        /// IPv6 too: a prefix of its own (a random ULA /64 unless --subnet
        /// gives one), and a way out through the host's IPv6 address.
        #[arg(long)]
        ipv6: bool,
        /// No way out of it: containers on it reach each other and the host
        /// only (each other only, with --isolate-host).
        #[arg(long)]
        internal: bool,
        /// Keep its containers from the host itself (its services, on any of
        /// its addresses) and from the cloud's metadata addresses, but for
        /// the network's DNS server. Not for the default network.
        #[arg(long)]
        isolate_host: bool,
        /// Limit each container address on it to 8192 open connections (of
        /// those it opens: to the world, the host and other networks); more
        /// are refused.
        #[arg(long)]
        conntrack_limit: bool,
        /// The network's name.
        name: String,
    },
    /// List networks.
    Ls,
    /// Show networks' details, and the containers on them, as JSON.
    Inspect {
        /// The networks.
        #[arg(value_name = "NETWORK", required = true)]
        names: Vec<String>,
    },
    /// Remove one or more networks (never one a container uses, nor the
    /// default one).
    Rm {
        /// The networks to remove.
        #[arg(value_name = "NETWORK", required = true)]
        names: Vec<String>,
    },
    /// Remove every network no container uses but the default one (asks
    /// first).
    Prune {
        /// Don't ask (needed where there's no terminal to ask on).
        #[arg(short, long)]
        force: bool,
    },
    /// Connect a container to a network: at once if it's running, at its
    /// next start otherwise.
    Connect {
        /// Its address on the network (the next free one if not given).
        #[arg(long, value_name = "IP")]
        ip: Option<std::net::Ipv4Addr>,
        /// Its IPv6 address on the network (one that has IPv6).
        #[arg(long, value_name = "IP")]
        ip6: Option<std::net::Ipv6Addr>,
        /// Another name it answers to on the network (repeatable; a
        /// network made with `network create`).
        #[arg(long = "alias", value_name = "NAME", value_parser = shipyard_network::dns::parse_name)]
        aliases: Vec<String>,
        /// The network.
        #[arg(value_name = "NETWORK")]
        network: String,
        /// The container: its name, ID, serial or ID prefix.
        #[arg(value_name = "CONTAINER")]
        container: String,
    },
    /// Disconnect a container from a network: at once if it's running, at
    /// its next start otherwise.
    Disconnect {
        /// Disconnect it from the network that carries its way out (its
        /// default route and published ports) too.
        #[arg(short, long)]
        force: bool,
        /// The network.
        #[arg(value_name = "NETWORK")]
        network: String,
        /// The container: its name, ID, serial or ID prefix.
        #[arg(value_name = "CONTAINER")]
        container: String,
    },
}

/// `shipyard image …`.
#[derive(Debug, Subcommand)]
enum ImageCommand {
    /// List images in the local store.
    Ls,
    /// Remove unreferenced images, layers and blobs from the store.
    Prune,
}

/// `shipyard system …`.
#[derive(Debug, Subcommand)]
enum SystemCommand {
    /// Show the store's disk usage.
    Df,
    /// Settle containers that ended unwatched, then start those whose
    /// restart policy brings them back when the host starts (what the boot
    /// unit runs: see contrib/systemd).
    Recover,
    /// Stop every container that's up, for the host's shutdown, each with its
    /// own stop signal and timeout: not as your stop (their restart policy
    /// brings them back at boot), and none restarted until `system recover`
    /// (what the boot unit runs as it stops).
    Stop,
}

fn main() -> ExitCode {
    // First of all: a container's init re-execs this binary to become a
    // clean process, and takes over here (it never returns then).
    process::init_entry_point();
    // A detached container's monitor is this binary too, and its exit
    // command (`__cleanup`, which takes the descriptors it inherits before
    // anything here opens one); so is `cp`'s helper in a container.
    shipyard_engine::run::cleanup_entry_point();
    shipyard_engine::run::health_entry_point();
    shipyard_runtime::container::monitor_entry_point();
    shipyard_engine::cp::helper_entry_point();
    // The storage helper (a rootless caller's layer work, in a user
    // namespace of its own) is this binary too; making the namespace needs a
    // single-threaded process, so before anything else as well.
    shipyard_engine::image::storage_helper_entry_point(|| init_tracing(None, false));
    // A network's DNS server is this binary too, started by a run.
    network::dns_entry_point();
    // So is `doctor`'s probe (it makes a user namespace: single-threaded).
    doctor::probe_entry_point();
    // And pasta's launcher (a rootless run's network: it joins the
    // container's user namespace, single-threaded too). Its pasta's death
    // while the container runs is said by the engine's hook.
    shipyard_network::pasta::entry_point(
        || process::refuse_privileged_start().map_err(|error| error.to_string()),
        shipyard_engine::network::pasta_lost,
    );
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

    let result = match cli.command {
        Command::Run(command) => run::run_command(*command),
        Command::Create(command) => run::create(*command),
        Command::Start {
            attach: false,
            containers,
            ..
        } => Ok(containers::start(&containers)),
        Command::Start {
            attach: true,
            interactive,
            detach_keys,
            containers,
        } => containers::start_attached(&containers, &detach_keys, interactive),
        Command::Attach {
            detach_keys,
            no_stdin,
            container,
        } => containers::attach(&container, &detach_keys, !no_stdin),
        Command::Stop { how, containers } => Ok(containers::stop(&containers, how.options())),
        Command::Restart { how, containers } => Ok(containers::restart(&containers, how.options())),
        Command::Kill { signal, containers } => Ok(containers::kill(&containers, signal)),
        Command::Update {
            restart,
            containers,
        } => Ok(containers::update(&containers, restart)),
        Command::Wait { containers } => Ok(containers::wait(&containers)),
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
        Command::Inspect { containers } => containers::inspect(&containers),
        Command::Rm { force, containers } => Ok(containers::rm(&containers, force)),
        Command::Pull { quiet, reference } => pull(&reference, quiet),
        Command::Images | Command::Image(ImageCommand::Ls) => images(),
        Command::Rmi { force, images } => Ok(rmi(&images, force)),
        Command::Image(ImageCommand::Prune) => image_prune(),
        Command::Volume(command) => volume_command(&command),
        Command::Network(command) => network_command(&command),
        Command::System(SystemCommand::Df) => system_df(),
        Command::System(SystemCommand::Recover) => system_recover(),
        Command::System(SystemCommand::Stop) => system_stop(),
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
    report(err, EXIT_ENGINE_ERROR)
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
    has_epipe && reader_gone(std::io::stdout().as_fd())
}

/// Whether `fd` is a pipe or socket nobody reads any more. The kernel says
/// so without a write: `poll` on a write end whose readers have all closed
/// returns `POLLERR` (Linux) or `POLLHUP` (macOS), and never blocks with a
/// zero timeout.
fn reader_gone(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut fds = [PollFd::new(fd, PollFlags::POLLOUT)];
    poll(&mut fds, PollTimeout::ZERO).is_ok_and(|_| {
        fds[0]
            .revents()
            .is_some_and(|got| got.intersects(PollFlags::POLLERR | PollFlags::POLLHUP))
    })
}

/// `shipyard images`: lists the images in the store.
fn images() -> Result<ExitCode> {
    let mut images = shipyard_engine::image::list(&engine())?;
    images.sort_by(|a, b| a.reference.cmp(&b.reference));
    let rows: Vec<Vec<String>> = images
        .into_iter()
        .map(|image| {
            let manifest = &image.record.manifest;
            let short: String = manifest
                .strip_prefix("sha256:")
                .unwrap_or(manifest)
                .chars()
                .take(12)
                .collect();
            vec![
                image.reference,
                short,
                image.record.layers.len().to_string(),
            ]
        })
        .collect();
    table(
        &mut std::io::stdout().lock(),
        &["REFERENCE", "MANIFEST", "LAYERS"],
        &rows,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Writes `rows` under `header`, in columns as wide as their widest cell,
/// three spaces apart (the last one unpadded).
fn table(out: &mut impl Write, header: &[&str], rows: &[Vec<String>]) -> std::io::Result<()> {
    let rows: Vec<Vec<Cow<'_, str>>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| printable(cell)).collect())
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |out: &mut dyn Write, cells: &mut dyn Iterator<Item = &str>| {
        use std::fmt::Write as _;
        let mut text = String::new();
        for (index, (cell, width)) in cells.zip(&widths).enumerate() {
            if index + 1 < widths.len() {
                let _ = write!(text, "{cell:<width$}   ");
            } else {
                text.push_str(cell);
            }
        }
        writeln!(out, "{}", text.trim_end())
    };
    line(out, &mut header.iter().copied())?;
    for row in &rows {
        line(out, &mut row.iter().map(AsRef::as_ref))?;
    }
    Ok(())
}

/// `text` with what a terminal would act on escaped (`\u{1b}`), so a name,
/// a command or a label is printed, never sent: control characters (C0 and
/// C1), and the bidirectional and format ones `char::is_control` misses,
/// which reorder what's shown (U+202E writes the rest right to left).
pub(crate) fn printable(text: &str) -> Cow<'_, str> {
    escaped_but(text, &[])
}

/// `text` as [`printable`] has it, but for a message on stderr: its line
/// breaks and tabs kept (a setup's failure quotes its output's lines), so
/// it can only print text, never act on the terminal.
fn printable_message(text: &str) -> Cow<'_, str> {
    escaped_but(text, &['\n', '\t'])
}

/// `text` with what a terminal acts on escaped, except the characters in
/// `kept`.
fn escaped_but<'a>(text: &'a str, kept: &[char]) -> Cow<'a, str> {
    let escaped = |c: char| {
        (c.is_control() && !kept.contains(&c))
            || matches!(
                c,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    };
    if !text.chars().any(escaped) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if escaped(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// `shipyard pull IMAGE`: pulls it, then prints its digest.
fn pull(reference: &str, quiet: bool) -> Result<ExitCode> {
    let digest =
        shipyard_engine::image::pull(&engine_with(Stderr { progress: !quiet }), reference)?;
    let mut out = std::io::stdout().lock();
    if quiet {
        writeln!(out, "{digest}")?;
    } else {
        writeln!(out, "Digest: {digest}")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard rmi IMAGE...`: removes images, collecting unshared blobs. One
/// that can't be removed is reported, and the rest still are.
fn rmi(references: &[String], force: bool) -> ExitCode {
    each(references, |reference| {
        let removed = shipyard_engine::image::remove(&engine(), reference, force)?;
        let mut out = std::io::stdout().lock();
        writeln!(out, "Untagged: {reference}")?;
        for digest in removed {
            writeln!(out, "Deleted: {digest}")?;
        }
        Ok(())
    })
}

/// Does `act` for each of `names`, reporting each failure and going on;
/// exits 125 if any failed. Its output's reader gone stops it, quietly, as
/// `fail` does: the rest are left as they are, and that isn't a failure.
fn each(names: &[String], mut act: impl FnMut(&str) -> Result<()>) -> ExitCode {
    let mut code = ExitCode::SUCCESS;
    for name in names {
        if let Err(err) = act(name) {
            if broken_pipe(&err) {
                break;
            }
            code = report(&err, EXIT_ENGINE_ERROR);
        }
    }
    code
}

/// `shipyard image prune`: removes unreferenced blobs and layers.
fn image_prune() -> Result<ExitCode> {
    let removed = shipyard_engine::image::prune(&engine())?;
    let mut out = std::io::stdout().lock();
    for digest in &removed {
        writeln!(out, "Deleted: {digest}")?;
    }
    writeln!(out, "Removed {} unreferenced blob(s)", removed.len())?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard system recover`: containers that ended unwatched settled,
/// those their restart policy brings back started, each one's name printed;
/// each that couldn't be started said, and the exit code 125.
fn system_recover() -> Result<ExitCode> {
    let booted = shipyard_engine::run::boot(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &booted.started {
        writeln!(out, "{name}")?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for (name, error) in booted.failed {
        let error = anyhow::Error::from(error).context(format!("container {name}"));
        code = report(&error, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}

/// `shipyard system stop`: every container that's up stopped for the
/// host's shutdown, each one's name printed; each that couldn't be said,
/// and the exit code 125.
fn system_stop() -> Result<ExitCode> {
    let stopped = shipyard_engine::run::stop_for_the_host(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &stopped.stopped {
        writeln!(out, "{name}")?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for (name, error) in stopped.failed {
        let error = anyhow::Error::from(error).context(format!("container {name}"));
        code = report(&error, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}

/// `shipyard system df`: shows what the images, the containers' writable
/// layers and the volumes take, and how much of it nothing uses.
fn system_df() -> Result<ExitCode> {
    let engine = engine();
    let store = shipyard_engine::image::disk_usage(&engine)?;
    let disk = &store.disk;
    let images = shipyard_engine::image::Usage {
        count: store.images,
        bytes: disk.blob_bytes + disk.layer_bytes,
        reclaimable: disk.blob_reclaimable + disk.layer_reclaimable,
        complete: disk.complete,
    };
    let rows: Vec<Vec<String>> = [
        ("Images", &images),
        ("Containers", &store.containers),
        ("Volumes", &shipyard_engine::volumes::usage(&engine)?),
    ]
    .into_iter()
    .map(|(kind, usage)| {
        // Directories it couldn't read (another user's): at least this.
        let at_least = if usage.complete { "" } else { "≥ " };
        vec![
            kind.to_owned(),
            usage.count.to_string(),
            format!("{at_least}{}", human_bytes(usage.bytes)),
            format!("{at_least}{}", human_bytes(usage.reclaimable)),
        ]
    })
    .collect();
    table(
        &mut std::io::stdout().lock(),
        &["TYPE", "COUNT", "SIZE", "RECLAIMABLE"],
        &rows,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard network …`: create, list, inspect, remove, prune networks.
fn network_command(command: &NetworkCommand) -> Result<ExitCode> {
    let engine = engine();
    match command {
        NetworkCommand::Create {
            subnet,
            gateway,
            ipv6,
            internal,
            isolate_host,
            conntrack_limit,
            name,
        } => {
            let mut options = network::SpecOptions::default();
            options.ipv6 = *ipv6;
            options.internal = *internal;
            options.isolate_host = *isolate_host;
            options.conntrack_limit = *conntrack_limit;
            let config = network::create(&engine, &network::spec(name, subnet, gateway, options)?)?;
            writeln!(std::io::stdout().lock(), "{}", config.name)?;
        }
        NetworkCommand::Ls => {
            let rows: Vec<Vec<String>> = network::list(&engine, &[])?
                .into_iter()
                .map(|listed| {
                    vec![
                        listed.config.name,
                        listed.config.subnet.to_string(),
                        listed
                            .config
                            .subnet6
                            .map_or_else(|| "-".to_owned(), |subnet6| subnet6.to_string()),
                        listed.config.gateway.to_string(),
                        listed.config.bridge,
                        listed.leases.len().to_string(),
                    ]
                })
                .collect();
            table(
                &mut std::io::stdout().lock(),
                &[
                    "NETWORK",
                    "SUBNET",
                    "SUBNET6",
                    "GATEWAY",
                    "BRIDGE",
                    "CONTAINERS",
                ],
                &rows,
            )?;
        }
        NetworkCommand::Inspect { names } => {
            let networks = network::inspect(&engine, names)?;
            let mut out = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut out, &networks)?;
            writeln!(out)?;
        }
        NetworkCommand::Rm { names } => {
            return Ok(each(names, |name| {
                network::remove(&engine, name)?;
                writeln!(std::io::stdout().lock(), "{name}")?;
                Ok(())
            }));
        }
        NetworkCommand::Prune { force } => return network_prune(*force),
        NetworkCommand::Connect {
            ip,
            ip6,
            aliases,
            network,
            container,
        } => {
            let mut request = shipyard_engine::run::ConnectRequest::new(network, container);
            request.ip = ip.map(std::net::IpAddr::V4);
            request.ip6 = *ip6;
            request.aliases.clone_from(aliases);
            shipyard_engine::run::connect(&engine, &request)?;
        }
        NetworkCommand::Disconnect {
            force,
            network,
            container,
        } => shipyard_engine::run::disconnect(&engine, network, container, *force)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard network prune`: asks first (on a terminal; elsewhere `-f` is
/// needed), then removes every network no container uses but the default
/// one.
fn network_prune(force: bool) -> Result<ExitCode> {
    const QUESTION: &str = "Remove every network no container uses (but the default one)?";
    if !force {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            anyhow::bail!(
                "network prune removes networks: run it on a terminal to be asked, or give -f"
            );
        }
        if !confirm(QUESTION, &mut stdin.lock(), &mut std::io::stderr().lock())? {
            writeln!(std::io::stderr().lock(), "Nothing removed")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let pruned = network::prune(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &pruned.removed {
        writeln!(out, "{name}")?;
    }
    writeln!(out, "Removed {} network(s)", pruned.removed.len())?;
    if !pruned.in_use.is_empty() {
        writeln!(
            out,
            "Kept {} in use: {}",
            pruned.in_use.len(),
            pruned.in_use.join(", ")
        )?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for error in &pruned.failed {
        code = report(error, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}

/// `shipyard volume …`: create, list, inspect, remove, prune named volumes.
fn volume_command(command: &VolumeCommand) -> Result<ExitCode> {
    use shipyard_engine::volumes;
    let engine = engine();
    match command {
        VolumeCommand::Create { name } => {
            let volume = volumes::create(&engine, name)?;
            writeln!(std::io::stdout().lock(), "{}", volume.name)?;
        }
        VolumeCommand::Ls => {
            let rows: Vec<Vec<String>> = volumes::list(&engine)?
                .into_iter()
                .map(|volume| vec![volume.name, volume.data.display().to_string()])
                .collect();
            table(
                &mut std::io::stdout().lock(),
                &["VOLUME", "MOUNTPOINT"],
                &rows,
            )?;
        }
        VolumeCommand::Inspect { name } => {
            let volume = volumes::inspect(&engine, name)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{:<12} {}", "Name:", volume.name)?;
            writeln!(out, "{:<12} {}", "Mountpoint:", volume.data.display())?;
        }
        VolumeCommand::Rm { names } => {
            return Ok(each(names, |name| {
                volumes::remove(&engine, name)?;
                writeln!(std::io::stdout().lock(), "{name}")?;
                Ok(())
            }));
        }
        VolumeCommand::Prune { force } => return volume_prune(*force),
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard volume prune`: asks first (on a terminal; elsewhere `-f` is
/// needed), then removes every volume no container uses, running or not.
fn volume_prune(force: bool) -> Result<ExitCode> {
    const QUESTION: &str = "Remove every volume no container uses, and its data?";
    if !force {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            anyhow::bail!(
                "volume prune deletes data: run it on a terminal to be asked, or give -f"
            );
        }
        if !confirm(QUESTION, &mut stdin.lock(), &mut std::io::stderr().lock())? {
            writeln!(std::io::stderr().lock(), "Nothing removed")?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let pruned = shipyard_engine::volumes::prune(&engine())?;
    let mut out = std::io::stdout().lock();
    for name in &pruned.removed {
        writeln!(out, "{name}")?;
    }
    writeln!(out, "Removed {} volume(s)", pruned.removed.len())?;
    if !pruned.in_use.is_empty() {
        writeln!(
            out,
            "Kept {} in use: {}",
            pruned.in_use.len(),
            pruned.in_use.join(", ")
        )?;
    }
    drop(out);
    let mut code = ExitCode::SUCCESS;
    for err in &pruned.failed {
        code = report(err, EXIT_ENGINE_ERROR);
    }
    Ok(code)
}

/// Asks `question` on `output`, `[y/N]`, and reads the answer from `input`:
/// whether it's yes (no, unless it says so).
fn confirm(question: &str, input: &mut impl BufRead, output: &mut impl Write) -> Result<bool> {
    write!(output, "{question} [y/N] ")?;
    output.flush()?;
    // Bytes, not a `String`: an answer that isn't UTF-8 is a no, not an
    // error.
    let mut answer = Vec::new();
    input.read_until(b'\n', &mut answer)?;
    Ok(matches!(
        answer.trim_ascii().to_ascii_lowercase().as_slice(),
        b"y" | b"yes"
    ))
}

/// A byte count in a human unit (B, KiB, MiB, GiB).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(
        clippy::cast_precision_loss,
        reason = "display only; precision loss at these magnitudes is immaterial"
    )]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The engine, as every command uses it: its places from the environment,
/// what it has to say printed on stderr.
pub(crate) fn engine() -> Engine {
    engine_with(Stderr { progress: true })
}

/// The engine, reporting to `reporter`.
fn engine_with(reporter: Stderr) -> Engine {
    Engine::new(Paths::from_env(), Arc::new(reporter))
}

/// Prints what the engine reports, as the CLI's own lines: warnings,
/// notices, hints and a run's failure with their `shipyard:` prefixes, a pull's progress
/// (unless `progress` is off: `pull -q`).
struct Stderr {
    progress: bool,
}

impl Stderr {
    /// The line `event` prints, if any.
    fn line(&self, event: Event) -> Option<String> {
        Some(match event {
            Event::Warning(warning) => format!("shipyard: warning: {}", warning.message),
            // A failure prints as the CLI's errors do.
            Event::Notice(text) | Event::Failure(text) => format!("shipyard: {text}"),
            Event::Hint(text) => format!("shipyard: hint: {text}"),
            Event::Progress(step) if self.progress => match step {
                Progress::Pulling { image } => format!("Pulling {image}"),
                Progress::Layer {
                    short_digest,
                    downloaded: Some(bytes),
                } => format!("{short_digest}: downloaded ({})", human_bytes(bytes)),
                Progress::Layer {
                    short_digest,
                    downloaded: None,
                } => format!("{short_digest}: already here"),
            },
            Event::Progress(_) => return None,
        })
    }
}

impl Reporter for Stderr {
    fn report(&self, event: Event) {
        if let Some(line) = self.line(event) {
            // Not `eprintln!`: it panics if stderr is a closed pipe.
            let _ = writeln!(std::io::stderr().lock(), "{}", printable_message(&line));
        }
    }
}

/// Prints an error with its full cause chain ("doing X: because Y") and
/// returns `code` as the exit code.
fn report(err: &dyn std::fmt::Display, code: u8) -> ExitCode {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(
        std::io::stderr().lock(),
        "shipyard: {}",
        printable_message(&format!("{err:#}"))
    );
    ExitCode::from(code)
}

/// Prints `message` as a warning (`shipyard: warning: …`).
fn warn(message: impl std::fmt::Display) -> std::io::Result<()> {
    writeln!(
        std::io::stderr().lock(),
        "shipyard: warning: {}",
        printable_message(&message.to_string())
    )
}

/// Prints `message` as a hint (`shipyard: hint: …`).
fn hint(message: impl std::fmt::Display) -> std::io::Result<()> {
    writeln!(
        std::io::stderr().lock(),
        "shipyard: hint: {}",
        printable_message(&message.to_string())
    )
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
/// (`shipyard: warning: …`, as the [`Stderr`] reporter prints them), whichever part of
/// Shipyard they come from; the rest (`--debug`, `RUST_LOG`) in tracing's
/// full format, where the time and the module matter, and while debugging
/// warnings too.
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
    use std::path::Path;

    use clap::Parser;

    use super::*;

    #[test]
    fn a_prompt_means_no_unless_it_says_yes() {
        let ask = |answer: &str| {
            let mut output = Vec::new();
            let yes = confirm("Sure?", &mut answer.as_bytes(), &mut output).unwrap();
            assert_eq!(output, b"Sure? [y/N] ");
            yes
        };
        assert!(ask("y\n") && ask("YES\n") && ask(" yes "));
        assert!(!ask("\n") && !ask("n\n") && !ask("") && !ask("yep\n"));
    }

    #[test]
    fn the_engines_events_print_as_the_clis_lines() {
        use shipyard_engine::report::{Warning, WarningKind};
        let loud = Stderr { progress: true };
        let warning = Event::Warning(Warning {
            kind: WarningKind::UnpackSkipped,
            message: "left out x".to_owned(),
        });
        assert_eq!(
            loud.line(warning.clone()).as_deref(),
            Some("shipyard: warning: left out x")
        );
        assert_eq!(
            loud.line(Event::Notice("published 80".to_owned()))
                .as_deref(),
            Some("shipyard: published 80")
        );
        assert_eq!(
            loud.line(Event::Hint("try -f".to_owned())).as_deref(),
            Some("shipyard: hint: try -f")
        );
        assert_eq!(
            loud.line(Event::Failure("not found".to_owned())).as_deref(),
            Some("shipyard: not found")
        );
        let pulling = Event::Progress(Progress::Pulling {
            image: "docker.io/library/alpine:latest".to_owned(),
        });
        assert_eq!(
            loud.line(pulling.clone()).as_deref(),
            Some("Pulling docker.io/library/alpine:latest")
        );
        let downloaded = Event::Progress(Progress::Layer {
            short_digest: "0123456789ab".to_owned(),
            downloaded: Some(2048),
        });
        assert_eq!(
            loud.line(downloaded.clone()).as_deref(),
            Some("0123456789ab: downloaded (2.0 KiB)")
        );
        let stored = Event::Progress(Progress::Layer {
            short_digest: "0123456789ab".to_owned(),
            downloaded: None,
        });
        assert_eq!(
            loud.line(stored.clone()).as_deref(),
            Some("0123456789ab: already here")
        );
        // `pull -q`: no progress, the rest as ever.
        let quiet = Stderr { progress: false };
        for event in [pulling, downloaded, stored] {
            assert_eq!(quiet.line(event), None);
        }
        assert_eq!(
            quiet.line(warning).as_deref(),
            Some("shipyard: warning: left out x")
        );
    }

    #[test]
    fn byte_counts_in_human_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human_bytes(3 << 40), "3.0 TiB");
        assert_eq!(human_bytes(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn columns_are_as_wide_as_their_widest_cell() {
        let mut out = Vec::new();
        let rows = vec![
            vec!["a-much-longer-name".to_owned(), "1".to_owned()],
            vec!["b".to_owned(), "22".to_owned()],
        ];
        table(&mut out, &["NAME", "N"], &rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "NAME                 N\na-much-longer-name   1\nb                    22\n"
        );
    }

    /// What a terminal acts on is printed, never sent (stage 9, step 9.9c):
    /// a control character, an OSC sequence, a bidirectional override; plain
    /// text is left as it is, not copied.
    #[test]
    fn printable_escapes_what_a_terminal_acts_on() {
        assert!(matches!(
            printable("web-1 ünïcode"),
            Cow::Borrowed("web-1 ünïcode")
        ));
        assert_eq!(printable("a\nb\u{7f}\u{9b}"), "a\\u{a}b\\u{7f}\\u{9b}");
        assert_eq!(
            printable("\x1b]0;owned\x07name"),
            "\\u{1b}]0;owned\\u{7}name"
        );
        assert_eq!(printable("evil\u{202e}txt.exe"), "evil\\u{202e}txt.exe");
        for c in [
            '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{2066}', '\u{2069}',
        ] {
            assert_eq!(printable(&c.to_string()), c.escape_unicode().to_string());
        }
    }

    /// A message on stderr keeps its lines and tabs, but what a terminal
    /// acts on is escaped as in a table (a hostile registry's error, a
    /// layer's file name).
    #[test]
    fn a_message_keeps_its_lines_but_never_acts() {
        assert!(matches!(
            printable_message("setup failed:\n\tline"),
            Cow::Borrowed("setup failed:\n\tline")
        ));
        assert_eq!(
            printable_message("pull: \x1b]52;c;owned\x07\nnext\u{202e}"),
            "pull: \\u{1b}]52;c;owned\\u{7}\nnext\\u{202e}"
        );
        assert_eq!(printable("a\tb"), "a\\u{9}b");
    }

    /// A pipe's write end says when its reader has gone, without a write
    /// (and doesn't while it's there: the control).
    #[test]
    fn a_pipe_says_its_reader_is_gone() {
        let (read, write) = nix::unistd::pipe().unwrap();
        assert!(!reader_gone(write.as_fd()));
        drop(read);
        assert!(reader_gone(write.as_fd()));
    }

    /// A broken pipe that isn't stdout's (an internal helper's) is an
    /// error, reported: never taken for `shipyard ps | head -1`. (The test
    /// harness reads this process's stdout.)
    #[test]
    fn an_internal_broken_pipe_is_a_failure() {
        let err = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            .context("sending the storage helper its request");
        assert!(!broken_pipe(&err));
    }

    /// A table's cells are printed, never sent, and its columns are as wide
    /// as what's shown.
    #[test]
    fn table_cells_are_printable() {
        let mut out = Vec::new();
        let rows = vec![
            vec!["\x1b[2J".to_owned(), "1".to_owned()],
            vec!["b".to_owned(), "2".to_owned()],
        ];
        table(&mut out, &["NAME", "N"], &rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "NAME        N\n\\u{1b}[2J   1\nb           2\n"
        );
    }

    /// clap-markdown's usage lines, tidied: a usage written out in full
    /// (`override_usage`, which starts with `shipyard` itself) isn't
    /// prefixed with the command's path again, and one of several lines is a
    /// code block rather than an inline span across lines.
    fn tidy_usage(markdown: &str) -> String {
        let mut tidy = String::new();
        let mut rest = markdown;
        while let Some(start) = rest.find("**Usage:** `") {
            tidy.push_str(&rest[..start]);
            let after = &rest[start + "**Usage:** `".len()..];
            let Some(end) = after.find('`') else {
                rest = &rest[start..];
                break;
            };
            let usage = after[..end].replacen("shipyard shipyard ", "shipyard ", 1);
            if usage.contains('\n') {
                // Its lines lose the indent that lines them up after
                // "Usage: " on a terminal.
                let lines: Vec<&str> = usage.lines().map(str::trim_start).collect();
                tidy.push_str("**Usage:**\n\n```text\n");
                tidy.push_str(&lines.join("\n"));
                tidy.push_str("\n```");
            } else {
                tidy.push_str("**Usage:** `");
                tidy.push_str(&usage);
                tidy.push('`');
            }
            rest = &after[end + 1..];
        }
        tidy.push_str(rest);
        tidy
    }

    /// `docs/user/cli.md` is generated from the flag definitions above, so the
    /// published reference can never drift from the real CLI.
    #[test]
    fn cli_reference_is_up_to_date() {
        const HEADER: &str = "<!-- Generated from the CLI's flag definitions. Do not edit by hand.\n     \
             Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference -->\n\n";
        let options = clap_markdown::MarkdownOptions::new()
            .title("Command reference".to_owned())
            .show_footer(false)
            .show_table_of_contents(true);
        let generated = format!(
            "{HEADER}{}",
            tidy_usage(&clap_markdown::help_markdown_custom::<Cli>(&options))
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/user/cli.md");

        if std::env::var_os("UPDATE_DOCS").is_some() {
            std::fs::write(&path, &generated).expect("writing docs/user/cli.md");
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "docs/user/cli.md is out of date with the CLI. Regenerate it with:\n\n    \
             UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference\n"
        );
    }

    #[test]
    fn global_logging_flags() {
        let cli = Cli::try_parse_from(["shipyard", "-D", "doctor"]).unwrap();
        assert!(cli.debug);
        let cli = Cli::try_parse_from(["shipyard", "doctor", "--log-level", "trace"]).unwrap();
        assert_eq!(cli.log_level, Some(LogLevel::Trace));
        assert!(Cli::try_parse_from(["shipyard", "--log-level", "loud", "doctor"]).is_err());
    }

    #[test]
    fn dash_v_is_reserved_for_volumes() {
        assert!(Cli::try_parse_from(["shipyard", "-v", "doctor"]).is_err());
    }

    #[test]
    fn run_subcommand_parses() {
        let cli = Cli::try_parse_from(["shipyard", "run", "-D", "alpine", "ls", "-l"]).unwrap();
        assert!(cli.debug, "-D before the command is shipyard's flag");
        assert!(matches!(cli.command, Command::Run(_)));
    }
}
