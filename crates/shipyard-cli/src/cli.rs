//! `shipyard`'s flags: the commands, as clap reads them (a group's own
//! arguments live beside its commands: `run::RunArgs`, `containers::StopArgs`…).
//! Their doc comments are the help text (and `docs/user/cli.md`).

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{containers, events_output, run};

/// A lightweight container engine.
#[derive(Debug, Parser)]
#[command(
    name = "shipyard",
    version,
    propagate_version = true,
    disable_version_flag = true
)]
pub(crate) struct Cli {
    /// Print version (`-v` as Docker's, `-V` too).
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    version: Option<bool>,

    /// Show debug logs (same as --log-level debug).
    #[arg(short = 'D', long, global = true)]
    pub(crate) debug: bool,

    /// How much to log.
    #[arg(long, global = true, value_name = "LEVEL")]
    pub(crate) log_level: Option<LogLevel>,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub(crate) fn as_filter(self) -> &'static str {
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
pub(crate) enum Command {
    /// Run an image in a container (its command, or COMMAND): on Linux, isolated in namespaces, with the image's files as its root. --no-image runs COMMAND on the host's files. With -d, in the background, by name.
    #[command(
        override_usage = "shipyard run [OPTIONS] IMAGE [COMMAND]...\n       \
                                shipyard run [OPTIONS] --no-image|--no-isolation|--rootfs DIR COMMAND...\n       \
                                shipyard run [OPTIONS] --bundle DIR",
        disable_help_flag = true
    )]
    Run(Box<run::RunCommand>),
    /// Make a container, to start later (`shipyard start`): its image pulled, nothing run (Linux).
    #[command(
        override_usage = "shipyard create [OPTIONS] IMAGE [COMMAND]...\n       \
                                shipyard create [OPTIONS] --rootfs DIR COMMAND...",
        disable_help_flag = true
    )]
    Create(Box<run::CreateCommand>),
    /// Start containers, in the background (one running is left as it is),
    /// or one attached (-a).
    Start(StartArgs),
    /// Attach to a running container: its output, and your input (unless
    /// --no-stdin), until it ends (its exit code) or the detach keys.
    Attach(AttachArgs),
    /// Stop running containers: their stop signal, then SIGKILL once their
    /// stop timeout is up. Returns once each has ended.
    Stop(StopRestartArgs),
    /// Stop containers (as `stop`), then start them again.
    Restart(StopRestartArgs),
    /// Change containers' settings without making them again: their restart
    /// policy, and their resource limits (in their cgroup at once while they
    /// run, and for their next starts).
    Update(UpdateArgs),
    /// Send a signal to running containers' main process (SIGKILL unless
    /// told), without waiting.
    Kill(KillArgs),
    /// Freeze running containers' processes where they are (their cgroup's
    /// freezer), until `unpause`.
    Pause(containers::ContainerRefs),
    /// Let paused containers' processes run on.
    Unpause(containers::ContainerRefs),
    /// Show a running container's processes, in `ps -ef`'s columns.
    Top(crate::stats::TopArgs),
    /// Show what containers use (CPU, memory, network, block I/O,
    /// processes), every second until Ctrl-C, or once with --no-stream.
    Stats(crate::stats::StatsArgs),
    /// Run Docker's `docker compose` against Shipyard (its API's service
    /// started if nothing serves it yet): `shipyard compose up -d`. Needs
    /// Docker's CLI and its compose plugin.
    #[command(disable_help_flag = true)]
    Compose(crate::compose::ComposeArgs),
    /// Rename a container (a running one's name on its networks too, at
    /// once).
    Rename(RenameArgs),
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
    Wait(containers::ContainerRefs),
    /// List containers: the running ones, or all of them with -a.
    Ps(PsArgs),
    /// Show the host ports a container publishes (all of them, or those to
    /// one of its ports).
    Port(PortArgs),
    /// Show objects in full, as JSON: a container, an image, a network or
    /// a volume, by name, in Docker's shape (`docker inspect`'s).
    Inspect(InspectArgs),
    /// Remove containers (their writable layers too). A running one needs -f.
    Rm(RmArgs),
    /// Pull an image into the local store (and extract it, on Linux), so a
    /// run starts at once and needs no registry.
    Pull(PullArgs),
    /// List images in the local store.
    Images(ImagesArgs),
    /// Remove one or more images from the local store (reference-counted; shared layers survive).
    Rmi(RmiArgs),
    /// Give an image another name (TARGET, a reference) beside its own.
    Tag(TagArgs),
    /// Manage containers (the same commands as at the top: `container ls`
    /// is `ps`, `container rm` is `rm`…).
    #[command(subcommand)]
    Container(ContainerCommand),
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
    /// The Linux VM containers run in on a Mac: its status, start and stop.
    #[command(subcommand)]
    Vm(VmCommand),
    /// Check whether this host can run containers, and what to fix.
    Doctor,
    /// Show Shipyard's version and its host's (`--format` a Go template
    /// over `{Client, Server}`, as Docker's).
    Version(Shown),
    /// Show what Shipyard has and what its host is (`--format` a Go
    /// template over Docker's `SystemInfo`).
    Info(Shown),
}

/// `start`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct StartArgs {
    /// Attach to it as it starts (one container): its output from its
    /// first byte, until it ends (its exit code) or Ctrl-P Ctrl-Q.
    #[arg(short, long)]
    pub(crate) attach: bool,
    /// With -a: pass your input on to it.
    #[arg(short, long, requires = "attach")]
    pub(crate) interactive: bool,
    /// With -a: the keys that detach (the container goes on), as
    /// attach's.
    #[arg(
        long,
        value_name = "KEYS",
        requires = "attach",
        default_value = "ctrl-p,ctrl-q"
    )]
    pub(crate) detach_keys: String,
    #[command(flatten)]
    pub(crate) refs: containers::ContainerRefs,
}

/// `attach`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct AttachArgs {
    /// The keys that detach (the container goes on): ctrl-p,ctrl-q, or
    /// others separated by commas (empty: none).
    #[arg(long, value_name = "KEYS", default_value = "ctrl-p,ctrl-q")]
    pub(crate) detach_keys: String,
    /// Don't pass your input on.
    #[arg(long)]
    pub(crate) no_stdin: bool,
    /// The container: its name, ID, serial or ID prefix.
    pub(crate) container: String,
}

/// `stop`'s and `restart`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct StopRestartArgs {
    #[command(flatten)]
    pub(crate) how: containers::StopArgs,
    #[command(flatten)]
    pub(crate) refs: containers::ContainerRefs,
}

/// `update`'s arguments: what to change, at least one.
#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    /// The restart policy: no, on-failure (or on-failure:N), always or
    /// unless-stopped.
    #[arg(long, value_name = "POLICY")]
    pub(crate) restart: Option<shipyard_engine::containers::RestartPolicy>,
    /// Its resource limits: changed in its cgroup at once while it runs,
    /// and kept for its next starts (those not given stay as they are).
    #[command(flatten)]
    pub(crate) limits: shipyard_cli::limits::UpdateLimitArgs,
    #[command(flatten)]
    pub(crate) refs: containers::ContainerRefs,
}

/// `kill`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct KillArgs {
    /// The signal: a name (KILL, SIGTERM) or a number.
    #[arg(short, long, value_name = "SIGNAL", default_value = "SIGKILL")]
    pub(crate) signal: shipyard_runtime::process::Signal,
    #[command(flatten)]
    pub(crate) refs: containers::ContainerRefs,
}

/// `rename`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct RenameArgs {
    /// The container: its name, ID, serial or ID prefix.
    pub(crate) container: String,
    /// Its new name.
    pub(crate) new_name: String,
}

/// A list's `--format`, `-q` and `-f` (Docker's).
#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    /// Only their IDs (the format is then ignored, as Docker's).
    #[arg(short, long)]
    pub(crate) quiet: bool,
    /// How to print them: `table` (the default), `json` (an object per
    /// line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE
    /// (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and
    /// functions).
    #[arg(long, value_name = "FORMAT")]
    pub(crate) format: Option<String>,
    /// Only those that match: KEY=VALUE, as Docker's (repeat a key for any
    /// of its values; `label` for all of them).
    #[arg(short, long, value_name = "KEY=VALUE", value_parser = parse_filter)]
    pub(crate) filter: Vec<(String, String)>,
}

/// `ps`'s arguments.
#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap's switches: each bool is one command-line flag"
)]
pub(crate) struct PsArgs {
    /// All containers, not only the ones up.
    #[arg(short, long)]
    pub(crate) all: bool,
    #[command(flatten)]
    pub(crate) list: ListArgs,
    /// Don't cut IDs and commands short.
    #[arg(long)]
    pub(crate) no_trunc: bool,
    /// Only the N made last (of all of them, up or not).
    #[arg(short = 'n', long, value_name = "N", allow_negative_numbers = true)]
    pub(crate) last: Option<i64>,
    /// Only the one made last (of all of them, up or not).
    #[arg(short, long)]
    pub(crate) latest: bool,
    /// Taken, as Docker's: Shipyard doesn't measure a container's size
    /// (`Size` is `0B`).
    #[arg(short, long)]
    pub(crate) size: bool,
}

/// `images`' arguments.
#[derive(Debug, Args)]
pub(crate) struct ImagesArgs {
    /// Only images of this name (`alpine`, `alpine:3.*`, as the
    /// `reference` filter).
    #[arg(value_name = "REPOSITORY[:TAG]")]
    pub(crate) reference: Option<String>,
    /// Taken, as Docker's: Shipyard keeps no intermediate images.
    #[arg(short, long)]
    pub(crate) all: bool,
    #[command(flatten)]
    pub(crate) list: ListArgs,
    /// Don't cut IDs short.
    #[arg(long)]
    pub(crate) no_trunc: bool,
    /// Show each image's digest too.
    #[arg(long)]
    pub(crate) digests: bool,
}

/// `volume ls`' arguments.
#[derive(Debug, Args)]
pub(crate) struct VolumeLsArgs {
    #[command(flatten)]
    pub(crate) list: ListArgs,
}

/// `network ls`' arguments.
#[derive(Debug, Args)]
pub(crate) struct NetworkLsArgs {
    #[command(flatten)]
    pub(crate) list: ListArgs,
    /// Don't cut IDs short.
    #[arg(long)]
    pub(crate) no_trunc: bool,
}

/// `-f`'s `KEY=VALUE`, as Docker's `FilterOpt` takes it: the key lower
/// case, both sides trimmed; one without `=` is a usage error.
pub(crate) fn parse_filter(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(key, value)| (key.trim().to_lowercase(), value.trim().to_owned()))
        .ok_or_else(|| "bad format of filter (expected name=value)".to_owned())
}

/// `port`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct PortArgs {
    /// The container: its name, ID, serial or ID prefix.
    #[arg(value_name = "CONTAINER")]
    pub(crate) container: String,
    /// One of its ports: 80, 80/tcp or 53/udp (tcp if not said).
    #[arg(value_name = "PORT[/PROTO]", value_parser = containers::parse_port)]
    pub(crate) port: Option<(u16, String)>,
    /// Show its sockets instead: what it listens on, and its
    /// connections, each peer by its container's name where it's one
    /// (Linux).
    #[arg(long, conflicts_with = "port")]
    pub(crate) sockets: bool,
    /// How to show its sockets: a table, or JSON.
    #[arg(long, value_enum, requires = "sockets")]
    pub(crate) format: Option<containers::SocketsFormat>,
}

/// `inspect`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct InspectArgs {
    /// Shipyard's own view of containers instead (its serial, its health
    /// history, its network's loss, the config as its record keeps it).
    #[arg(long, conflicts_with_all = ["kind", "format"])]
    pub(crate) shipyard: bool,
    /// Only this kind of object (without it: the first of a container, an
    /// image, a network, a volume that has the name, as Docker's).
    #[arg(long = "type", value_name = "TYPE", value_enum)]
    pub(crate) kind: Option<InspectKind>,
    #[command(flatten)]
    pub(crate) shown: Shown,
    /// Taken, as Docker's: Shipyard doesn't measure a container's size.
    #[arg(short, long)]
    pub(crate) size: bool,
    /// The objects: names, IDs or ID prefixes.
    #[arg(value_name = "NAME", required = true)]
    pub(crate) names: Vec<String>,
}

/// `container inspect`'s arguments: `inspect`'s, containers only (as
/// Docker's, which has no `--type` there).
#[derive(Debug, Args)]
pub(crate) struct ContainerInspectArgs {
    /// Shipyard's own view instead (its serial, its health history, its
    /// network's loss, the config as its record keeps it).
    #[arg(long, conflicts_with = "format")]
    pub(crate) shipyard: bool,
    #[command(flatten)]
    pub(crate) shown: Shown,
    /// Taken, as Docker's: Shipyard doesn't measure a container's size.
    #[arg(short, long)]
    pub(crate) size: bool,
    /// The containers: names, IDs, serials or ID prefixes.
    #[arg(value_name = "CONTAINER", required = true)]
    pub(crate) names: Vec<String>,
}

impl From<ContainerInspectArgs> for InspectArgs {
    fn from(args: ContainerInspectArgs) -> Self {
        Self {
            shipyard: args.shipyard,
            kind: Some(InspectKind::Container),
            shown: args.shown,
            size: args.size,
            names: args.names,
        }
    }
}

/// A kind of object `inspect --type` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum InspectKind {
    /// A container.
    Container,
    /// An image.
    Image,
    /// A network.
    Network,
    /// A volume.
    Volume,
}

impl From<InspectKind> for shipyard_engine::docker::Kind {
    fn from(kind: InspectKind) -> Self {
        match kind {
            InspectKind::Container => Self::Container,
            InspectKind::Image => Self::Image,
            InspectKind::Network => Self::Network,
            InspectKind::Volume => Self::Volume,
        }
    }
}

/// How an object is shown: `-f`/`--format`, Docker's (`inspect`'s,
/// `version`'s, `info`'s).
#[derive(Debug, Args)]
pub(crate) struct Shown {
    /// How to show it: as Docker's command does without it (a JSON array
    /// for `inspect`); `json`, as JSON; or a Go TEMPLATE over it
    /// (`{{.Name}}`, `{{json .Config}}`), a line each.
    #[arg(short, long, value_name = "FORMAT")]
    pub(crate) format: Option<String>,
}

/// `rm`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct RmArgs {
    /// Remove a running container too (it's killed first).
    #[arg(short, long)]
    pub(crate) force: bool,
    /// Remove its anonymous volumes too (`-v /path`, the image's own), each
    /// that no other container names; a named volume is kept.
    #[arg(short, long)]
    pub(crate) volumes: bool,
    #[command(flatten)]
    pub(crate) refs: containers::ContainerRefs,
}

/// `pull`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct PullArgs {
    /// Print nothing but the image's digest.
    #[arg(short, long)]
    pub(crate) quiet: bool,
    /// The image, like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`.
    #[arg(value_name = "IMAGE")]
    pub(crate) reference: String,
}

/// `rmi`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct RmiArgs {
    /// Untag an image even while a container uses it (a container keeps
    /// what it needs until it's removed; a foreground run's layers stay
    /// until it ends and a prune collects them).
    #[arg(short, long)]
    pub(crate) force: bool,
    /// The images to remove: references or IDs.
    #[arg(value_name = "IMAGE", required = true)]
    pub(crate) images: Vec<String>,
}

/// `tag`'s arguments.
#[derive(Debug, Args)]
pub(crate) struct TagArgs {
    /// The image: a reference or an ID.
    #[arg(value_name = "SOURCE_IMAGE")]
    pub(crate) source: String,
    /// Its new name, like `registry.example/app:1.0`.
    #[arg(value_name = "TARGET_IMAGE")]
    pub(crate) target: String,
}

/// `shipyard container …`: the container commands at the top, under
/// Docker's group (each flattened to its top-level command at once,
/// [`Command::flattened`]).
#[derive(Debug, Subcommand)]
pub(crate) enum ContainerCommand {
    /// List containers (`ps`).
    #[command(visible_aliases = ["ps", "list"])]
    Ls(PsArgs),
    /// Run an image in a container (`run`).
    #[command(disable_help_flag = true)]
    Run(Box<run::RunCommand>),
    /// Make a container, to start later (`create`).
    #[command(disable_help_flag = true)]
    Create(Box<run::CreateCommand>),
    /// Start containers (`start`).
    Start(StartArgs),
    /// Stop running containers (`stop`).
    Stop(StopRestartArgs),
    /// Stop containers, then start them again (`restart`).
    Restart(StopRestartArgs),
    /// Send a signal to running containers (`kill`).
    Kill(KillArgs),
    /// Freeze running containers (`pause`).
    Pause(containers::ContainerRefs),
    /// Let paused containers run on (`unpause`).
    Unpause(containers::ContainerRefs),
    /// Show a container's processes (`top`).
    Top(crate::stats::TopArgs),
    /// Show what containers use (`stats`).
    Stats(crate::stats::StatsArgs),
    /// Remove containers (`rm`).
    Rm(RmArgs),
    /// Print a container's logs (`logs`).
    Logs(containers::LogsArgs),
    /// Run a command in a running container (`exec`).
    Exec(containers::ExecArgs),
    /// Show containers in full, as JSON: Docker's shape (`inspect
    /// --type container`).
    Inspect(ContainerInspectArgs),
    /// Copy files between a container and the host (`cp`).
    Cp(containers::CpArgs),
    /// Show the host ports a container publishes (`port`).
    Port(PortArgs),
    /// Rename a container (`rename`).
    Rename(RenameArgs),
    /// Wait for containers to end (`wait`).
    Wait(containers::ContainerRefs),
    /// Attach to a running container (`attach`).
    Attach(AttachArgs),
    /// Change containers' settings (`update`).
    Update(UpdateArgs),
    /// Remove every stopped container.
    Prune(crate::prune::PruneArgs),
}

impl From<ContainerCommand> for Command {
    fn from(command: ContainerCommand) -> Self {
        match command {
            ContainerCommand::Ls(args) => Self::Ps(args),
            ContainerCommand::Run(args) => Self::Run(args),
            ContainerCommand::Create(args) => Self::Create(args),
            ContainerCommand::Start(args) => Self::Start(args),
            ContainerCommand::Stop(args) => Self::Stop(args),
            ContainerCommand::Restart(args) => Self::Restart(args),
            ContainerCommand::Kill(args) => Self::Kill(args),
            ContainerCommand::Pause(args) => Self::Pause(args),
            ContainerCommand::Unpause(args) => Self::Unpause(args),
            ContainerCommand::Top(args) => Self::Top(args),
            ContainerCommand::Stats(args) => Self::Stats(args),
            ContainerCommand::Rm(args) => Self::Rm(args),
            ContainerCommand::Logs(args) => Self::Logs(args),
            ContainerCommand::Exec(args) => Self::Exec(args),
            ContainerCommand::Inspect(args) => Self::Inspect(args.into()),
            ContainerCommand::Cp(args) => Self::Cp(args),
            ContainerCommand::Port(args) => Self::Port(args),
            ContainerCommand::Rename(args) => Self::Rename(args),
            ContainerCommand::Wait(args) => Self::Wait(args),
            ContainerCommand::Attach(args) => Self::Attach(args),
            ContainerCommand::Update(args) => Self::Update(args),
            ContainerCommand::Prune(args) => Self::Container(ContainerCommand::Prune(args)),
        }
    }
}

impl Command {
    /// This command as the top level has it: a group's alias
    /// (`container rm`, `image rm`…) as the command it names, so
    /// everything after parsing (the dispatch, a Mac's forwarding) sees
    /// one command under one name.
    pub(crate) fn flattened(self) -> Self {
        match self {
            Self::Container(command) => command.into(),
            Self::Image(ImageCommand::Ls(args)) => Self::Images(args),
            Self::Image(ImageCommand::Rm(args)) => Self::Rmi(args),
            Self::Image(ImageCommand::Pull(args)) => Self::Pull(args),
            Self::Image(ImageCommand::Tag(args)) => Self::Tag(args),
            command => command,
        }
    }

    /// The exit code of this command's own failure, as Docker's CLI
    /// gives it: 125 for `run` (a code a container's own command rarely
    /// uses, so the two can be told apart), 1 for every other command.
    pub(crate) fn failure_code(&self) -> u8 {
        match self {
            Self::Run(_) => shipyard_runtime::process::EXIT_ENGINE_ERROR,
            _ => crate::reporter::EXIT_FAILURE,
        }
    }
}

/// `-l`'s `KEY[=VALUE]`: a bare KEY is the key with an empty value.
fn parse_label(s: &str) -> Result<(String, String), String> {
    let (key, value) = s.split_once('=').unwrap_or((s, ""));
    if key.is_empty() {
        return Err("a label needs a key before its =".to_owned());
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// `-o`'s `KEY=VALUE`.
fn parse_option(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("{s}: give KEY=VALUE")),
    }
}

/// `shipyard volume …`.
#[derive(Debug, Subcommand)]
pub(crate) enum VolumeCommand {
    /// Create a named volume (one with a new name of its own without NAME;
    /// one that's there already is left as it is).
    Create {
        /// Set a label on it: KEY=VALUE, or KEY (an empty value).
        /// Repeatable.
        #[arg(short = 'l', long = "label", value_name = "KEY[=VALUE]", value_parser = parse_label)]
        labels: Vec<(String, String)>,
        /// Its driver: local, the only one (a directory of the store's).
        #[arg(short, long, value_name = "DRIVER", default_value = "local")]
        driver: String,
        /// A driver option: refused (a local volume is a directory, with no
        /// options; `type=nfs` and the like aren't mounted).
        #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE", value_parser = parse_option)]
        options: Vec<(String, String)>,
        /// The volume's name.
        name: Option<String>,
    },
    /// List named volumes.
    #[command(visible_alias = "list")]
    Ls(VolumeLsArgs),
    /// Show a volume's details.
    Inspect {
        #[command(flatten)]
        shown: Shown,
        /// The volumes.
        #[arg(value_name = "VOLUME", required = true)]
        names: Vec<String>,
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

/// `network create`'s arguments.
#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap's switches: each bool is one command-line flag"
)]
pub(crate) struct NetworkCreateArgs {
    /// Its subnet, like `10.90.0.0/24` (a free one by default); once
    /// more with an IPv6 prefix, like `fd00:1::/64`, for IPv6 too.
    #[arg(long, value_name = "CIDR")]
    pub(crate) subnet: Vec<shipyard_network::IpNet>,
    /// Its gateway, the bridge's address (the subnet's first by
    /// default); once per family.
    #[arg(long, value_name = "IP")]
    pub(crate) gateway: Vec<std::net::IpAddr>,
    /// IPv6 too: a prefix of its own (a random ULA /64 unless --subnet
    /// gives one), and a way out through the host's IPv6 address.
    #[arg(long)]
    pub(crate) ipv6: bool,
    /// No way out of it: containers on it reach each other and the host
    /// only (each other only, with --isolate-host).
    #[arg(long)]
    pub(crate) internal: bool,
    /// Keep its containers from the host itself (its services, on any of
    /// its addresses) and from the cloud's metadata addresses, but for
    /// the network's DNS server. Not for the default network.
    #[arg(long)]
    pub(crate) isolate_host: bool,
    /// Limit each container address on it to 8192 open connections (of
    /// those it opens: to the world, the host and other networks); more
    /// are refused.
    #[arg(long)]
    pub(crate) conntrack_limit: bool,
    /// Set a label on it: KEY=VALUE, or KEY (an empty value).
    /// Repeatable.
    #[arg(short = 'l', long = "label", value_name = "KEY[=VALUE]", value_parser = parse_label)]
    pub(crate) labels: Vec<(String, String)>,
    /// Its driver: bridge, the only one.
    #[arg(short, long, value_name = "DRIVER", default_value = "bridge")]
    pub(crate) driver: String,
    /// A driver option, KEY=VALUE: Docker's bridge options at their
    /// defaults, and Shipyard's own (`com.shipyard.network.isolate-host`,
    /// `com.shipyard.network.conntrack-limit`); others are refused.
    /// Repeatable.
    #[arg(short = 'o', long = "opt", value_name = "KEY=VALUE", value_parser = parse_option)]
    pub(crate) options: Vec<(String, String)>,
    /// The network's name.
    pub(crate) name: String,
}

/// `shipyard network …`.
#[derive(Debug, Subcommand)]
pub(crate) enum NetworkCommand {
    /// Create a network: a bridge with a subnet of its own.
    Create(NetworkCreateArgs),
    /// List networks.
    #[command(visible_alias = "list")]
    Ls(NetworkLsArgs),
    /// Show networks' details, and the containers on them, as JSON.
    Inspect {
        /// Shipyard's own view instead (the containers on it by their
        /// leases, its bridge, its DNS server).
        #[arg(long, conflicts_with = "format")]
        shipyard: bool,
        #[command(flatten)]
        shown: Shown,
        /// The networks.
        #[arg(value_name = "NETWORK", required = true)]
        names: Vec<String>,
    },
    /// Remove one or more networks (never one a container that's up is
    /// on, nor the default one; a stopped container doesn't keep its
    /// networks, as Docker's don't).
    Rm {
        /// The networks to remove.
        #[arg(value_name = "NETWORK", required = true)]
        names: Vec<String>,
    },
    /// Remove every network no container that's up is on, but the default
    /// one (asks first).
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
pub(crate) enum ImageCommand {
    /// List images in the local store (`images`).
    #[command(visible_alias = "list")]
    Ls(ImagesArgs),
    /// Remove dangling images (every image no container was made from,
    /// with -a), and what nothing in the store needs.
    Prune(crate::prune::ImagePruneArgs),
    /// Remove images (`rmi`).
    Rm(RmiArgs),
    /// Pull an image (`pull`).
    Pull(PullArgs),
    /// Give an image another name (`tag`).
    Tag(TagArgs),
    /// Show images in full, as JSON: Docker's shape (`docker image
    /// inspect`'s).
    Inspect {
        #[command(flatten)]
        shown: Shown,
        /// The images: references or IDs.
        #[arg(value_name = "IMAGE", required = true)]
        images: Vec<String>,
    },
}

/// `shipyard system …`.
#[derive(Debug, Subcommand)]
pub(crate) enum SystemCommand {
    /// Remove what nothing uses: stopped containers, unused networks,
    /// dangling images (every unused one with -a), unused anonymous volumes
    /// with --volumes.
    Prune(crate::prune::SystemPruneArgs),
    /// Show the store's disk usage.
    Df {
        /// How to show it: Docker's table without it; `json`, an object
        /// per kind; or a Go TEMPLATE (`{{.Type}} {{.Size}}`).
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
    },
    /// Settle containers that ended unwatched, then start those whose
    /// restart policy brings them back when the host starts (what the boot
    /// unit runs: see contrib/systemd).
    Recover,
    /// Stop every container that's up, for the host's shutdown, each with its
    /// own stop signal and timeout: not as your stop (their restart policy
    /// brings them back at boot), and none restarted until `system recover`
    /// (what shipyard-stop.service runs at a shutdown).
    Stop,
    /// Serve Docker's Engine API (a subset, version 1.43) on a UNIX socket,
    /// for Docker's own clients (`DOCKER_HOST=unix://SOCKET docker ps`);
    /// systemd's shipyard.socket starts it at the first connection (see
    /// contrib/systemd). It exits once nothing has been asked for 60 s
    /// (`SHIPYARD_API_IDLE`: other whole seconds).
    Service {
        /// The socket to listen on, when systemd passes none (by default
        /// api.sock in the containers' state directory: /run/shipyard for
        /// root, shipyard in the user's runtime directory for anyone else).
        #[arg(long, value_name = "PATH")]
        socket: Option<std::path::PathBuf>,
    },
}

/// `shipyard vm`'s commands (macOS: step 10.3).
#[derive(Debug, Subcommand)]
pub(crate) enum VmCommand {
    /// Start the VM, if it isn't running, and wait for its guest to boot.
    Start,
    /// Say whether the VM runs, which guest it boots, and the ports it forwards to this Mac.
    Status,
    /// Stop the VM: its containers stopped in order, then the guest powered
    /// off.
    Stop,
    /// Let launchd hold Docker's socket while the VM is stopped and start
    /// the VM at its first connection (`docker` works any time; nothing runs
    /// until then). Run again after moving shipyard.
    Install,
    /// Undo `vm install`: launchd lets go of Docker's socket (a VM it
    /// started is stopped first).
    Uninstall,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

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

    /// `shipyard -v` is the version, as `docker -v`; `-V` too; `run -v` is
    /// a volume.
    #[test]
    fn dash_v_is_the_version_before_a_command() {
        for flag in ["-v", "-V", "--version"] {
            let error = Cli::try_parse_from(["shipyard", flag]).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayVersion,
                "{flag}"
            );
        }
        let Command::Run(_) = parsed(&["run", "-v", "data:/d", "alpine", "true"]) else {
            panic!("not a run");
        };
    }

    /// `run`'s and `create`'s `-h` is the hostname (Docker's); help is
    /// `--help` there, and `-h` everywhere else.
    #[test]
    fn dash_h_is_the_hostname_for_run_and_create() {
        assert!(Cli::try_parse_from(["shipyard", "run", "-h", "box", "alpine", "true"]).is_ok());
        assert!(Cli::try_parse_from(["shipyard", "create", "-h", "box", "alpine"]).is_ok());
        for argv in [
            &["shipyard", "run", "--help"][..],
            &["shipyard", "container", "run", "--help"],
            &["shipyard", "ps", "-h"],
        ] {
            let error = Cli::try_parse_from(argv).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "{argv:?}"
            );
        }
        // Without a value, a usage error, never a run.
        let error = Cli::try_parse_from(["shipyard", "run", "-h"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    /// The lists' Docker flags: `--format` as text, `-q`, `-f KEY=VALUE`
    /// (repeatable; a filter without `=` is a usage error, 125; its key
    /// lower-cased and both sides trimmed, as Docker's), `ps`'s
    /// `--no-trunc`, `-n`, `-l`, `-s`, `images`' REPOSITORY, `--digests`.
    #[test]
    fn the_lists_take_docker_s_flags() {
        let Command::Ps(ps) = parsed(&[
            "ps",
            "-a",
            "-q",
            "--format",
            "{{.Names}}",
            "-f",
            "status=exited",
            "--filter",
            "label=a=b",
            "--no-trunc",
            "-n",
            "2",
            "-s",
        ]) else {
            panic!("not ps");
        };
        assert!(ps.all && ps.list.quiet && ps.no_trunc && ps.size);
        assert_eq!(ps.list.format.as_deref(), Some("{{.Names}}"));
        assert_eq!(
            ps.list.filter,
            [
                ("status".to_owned(), "exited".to_owned()),
                ("label".to_owned(), "a=b".to_owned())
            ]
        );
        assert_eq!(ps.last, Some(2));
        let Command::Ps(ps) = parsed(&["container", "ls", "-l"]) else {
            panic!("not ps");
        };
        assert!(ps.latest);
        let Command::Ps(ps) = parsed(&["ps", "-f", " Status = exited "]) else {
            panic!("not ps");
        };
        assert_eq!(ps.list.filter, [("status".to_owned(), "exited".to_owned())]);
        let refused = Cli::try_parse_from(["shipyard", "ps", "-f", "name"]).unwrap_err();
        assert_eq!(refused.kind(), clap::error::ErrorKind::ValueValidation);
        let Command::Images(images) =
            parsed(&["image", "ls", "alpine", "--digests", "-q", "--no-trunc"])
        else {
            panic!("not images");
        };
        assert_eq!(images.reference.as_deref(), Some("alpine"));
        assert!(images.digests && images.list.quiet && images.no_trunc);
        let Command::Volume(VolumeCommand::Ls(volumes)) =
            parsed(&["volume", "list", "-f", "dangling=true", "--format", "json"])
        else {
            panic!("not volume ls");
        };
        assert_eq!(volumes.list.format.as_deref(), Some("json"));
        let Command::Network(NetworkCommand::Ls(networks)) =
            parsed(&["network", "ls", "-q", "--no-trunc"])
        else {
            panic!("not network ls");
        };
        assert!(networks.list.quiet && networks.no_trunc);
    }

    /// `update` takes Docker's limit flags beside `--restart` (none
    /// required by clap: the engine refuses an update of nothing); `pause`
    /// and `unpause`, in the `container` group too.
    #[test]
    fn update_and_pause_parse() {
        let Command::Update(update) = parsed(&[
            "update",
            "-m",
            "64m",
            "--cpus",
            "0.5",
            "--pids-limit",
            "50",
            "--restart",
            "no",
            "web",
        ]) else {
            panic!("not update");
        };
        assert_eq!(
            update.restart,
            Some(shipyard_engine::containers::RestartPolicy::No)
        );
        let limits = update.limits.docker();
        assert_eq!(limits.memory, Some(64 << 20));
        assert_eq!(limits.cpus.as_deref(), Some("0.5"));
        assert_eq!(limits.pids_limit, Some(50));
        let Command::Update(bare) = parsed(&["update", "web"]) else {
            panic!("not update");
        };
        assert!(bare.restart.is_none());
        assert_eq!(
            bare.limits.docker(),
            shipyard_runtime::resources::DockerLimits::default()
        );
        assert!(matches!(
            parsed(&["container", "pause", "web"]),
            Command::Pause(_)
        ));
        assert!(matches!(parsed(&["unpause", "web"]), Command::Unpause(_)));
    }

    /// Docker's prunes: `container prune`, `image prune -a`, `system prune
    /// -a --volumes`, each with `-f` and `--filter`.
    #[test]
    fn prunes_parse() {
        let Command::Container(ContainerCommand::Prune(containers)) = parsed(&[
            "container",
            "prune",
            "-f",
            "--filter",
            "until=24h",
            "--filter",
            "label=a",
        ]) else {
            panic!("not container prune");
        };
        assert!(containers.force);
        assert_eq!(
            containers.filter,
            [
                ("until".to_owned(), "24h".to_owned()),
                ("label".to_owned(), "a".to_owned())
            ]
        );
        let Command::Image(ImageCommand::Prune(images)) = parsed(&["image", "prune", "-a"]) else {
            panic!("not image prune");
        };
        assert!(images.all && !images.prune.force);
        let Command::System(SystemCommand::Prune(system)) =
            parsed(&["system", "prune", "-a", "--volumes", "-f"])
        else {
            panic!("not system prune");
        };
        assert!(system.all && system.volumes && system.prune.force);
    }

    /// `top` takes `ps`'s options after the container (to refuse them, as
    /// a usage Docker's has); `stats` Docker's flags, its containers
    /// optional; both in the `container` group too.
    #[test]
    fn top_and_stats_parse() {
        let Command::Top(top) = parsed(&["container", "top", "web", "-o", "pid"]) else {
            panic!("not top");
        };
        assert_eq!(top.container, "web");
        assert_eq!(top.ps_options, ["-o", "pid"]);
        let Command::Stats(stats) = parsed(&[
            "stats",
            "-a",
            "--no-stream",
            "--no-trunc",
            "--format",
            "json",
        ]) else {
            panic!("not stats");
        };
        assert!(stats.all && stats.no_stream && stats.no_trunc);
        assert_eq!(stats.format.as_deref(), Some("json"));
        assert!(stats.containers.is_empty());
        let Command::Stats(named) = parsed(&["container", "stats", "a", "b"]) else {
            panic!("not stats");
        };
        assert_eq!(named.containers, ["a", "b"]);
    }

    /// Docker's other short forms and names: `logs -n`, `stop -t` and
    /// `--timeout` (`--time` kept), `rm -v`, `cp -q`, `image prune -f`.
    #[test]
    fn docker_s_short_forms_parse() {
        let Command::Logs(logs) = parsed(&["logs", "-n", "5", "web"]) else {
            panic!("not logs");
        };
        assert_eq!(logs.tail, Some(containers::Tail::Last(5)));
        let Command::Logs(logs) = parsed(&["logs", "--tail", "all", "web"]) else {
            panic!("not logs");
        };
        assert_eq!(logs.tail, Some(containers::Tail::All));
        for flag in ["-t", "--timeout", "--time"] {
            let Command::Stop(stop) = parsed(&["stop", flag, "3", "web"]) else {
                panic!("not stop");
            };
            assert_eq!(
                stop.how.options().timeout,
                Some(std::time::Duration::from_secs(3)),
                "{flag}"
            );
        }
        // -1: for as long as it takes (Docker's); below -1 refused.
        let Command::Stop(stop) = parsed(&["stop", "-t", "-1", "web"]) else {
            panic!("not stop");
        };
        assert_eq!(
            stop.how.options().timeout,
            Some(shipyard_engine::run::StopOptions::timeout_of(-1))
        );
        assert!(
            shipyard_engine::run::StopOptions::timeout_of(-1)
                > std::time::Duration::from_hours(24 * 365)
        );
        assert!(Cli::try_parse_from(["shipyard", "stop", "-t", "-2", "web"]).is_err());
        let Command::Rm(rm) = parsed(&["rm", "-fv", "web"]) else {
            panic!("not rm");
        };
        assert!(rm.force && rm.volumes);
        assert!(matches!(
            parsed(&["cp", "-q", "web:/a", "b"]),
            Command::Cp(_)
        ));
        assert!(matches!(
            parsed(&["image", "prune", "-f"]),
            Command::Image(ImageCommand::Prune(_))
        ));
    }

    /// `-l`'s `KEY[=VALUE]` (a bare KEY's value empty; no key refused) and
    /// `-o`'s `KEY=VALUE` (both needed).
    #[test]
    fn labels_and_options_parse() {
        assert_eq!(parse_label("a=b=c"), Ok(("a".to_owned(), "b=c".to_owned())));
        assert_eq!(parse_label("bare"), Ok(("bare".to_owned(), String::new())));
        assert!(parse_label("=x").is_err());
        assert_eq!(parse_option("k=v"), Ok(("k".to_owned(), "v".to_owned())));
        assert!(parse_option("k").is_err());
        assert!(parse_option("=v").is_err());
    }

    /// Each group's alias is its top-level command, once flattened.
    #[test]
    fn groups_flatten_to_their_commands() {
        /// An argv, and what it must parse to.
        type Case = (&'static [&'static str], fn(&Command) -> bool);
        let cases: &[Case] = &[
            (
                &["container", "ls", "-a"],
                |c| matches!(c, Command::Ps(ps) if ps.all),
            ),
            (&["container", "ps"], |c| matches!(c, Command::Ps(_))),
            (&["container", "list"], |c| matches!(c, Command::Ps(_))),
            (
                &["container", "rm", "-f", "web"],
                |c| matches!(c, Command::Rm(rm) if rm.force),
            ),
            (&["container", "stop", "web"], |c| {
                matches!(c, Command::Stop(_))
            }),
            (&["container", "restart", "web"], |c| {
                matches!(c, Command::Restart(_))
            }),
            (
                &["container", "start", "-a", "web"],
                |c| matches!(c, Command::Start(s) if s.attach),
            ),
            (&["container", "kill", "web"], |c| {
                matches!(c, Command::Kill(_))
            }),
            (&["container", "logs", "web"], |c| {
                matches!(c, Command::Logs(_))
            }),
            (&["container", "exec", "web", "ls"], |c| {
                matches!(c, Command::Exec(_))
            }),
            (&["container", "inspect", "web"], |c| {
                matches!(c, Command::Inspect(_))
            }),
            (&["container", "cp", "web:/a", "b"], |c| {
                matches!(c, Command::Cp(_))
            }),
            (&["container", "port", "web"], |c| {
                matches!(c, Command::Port(_))
            }),
            (&["container", "rename", "a", "b"], |c| {
                matches!(c, Command::Rename(_))
            }),
            (&["container", "wait", "web"], |c| {
                matches!(c, Command::Wait(_))
            }),
            (&["container", "attach", "web"], |c| {
                matches!(c, Command::Attach(_))
            }),
            (&["container", "update", "--restart", "no", "web"], |c| {
                matches!(c, Command::Update(_))
            }),
            (&["container", "run", "alpine"], |c| {
                matches!(c, Command::Run(_))
            }),
            (&["container", "create", "alpine"], |c| {
                matches!(c, Command::Create(_))
            }),
            (&["image", "ls"], |c| matches!(c, Command::Images(_))),
            (&["image", "list"], |c| matches!(c, Command::Images(_))),
            (
                &["image", "rm", "-f", "a"],
                |c| matches!(c, Command::Rmi(r) if r.force),
            ),
            (
                &["image", "pull", "-q", "a"],
                |c| matches!(c, Command::Pull(p) if p.quiet),
            ),
            (
                &["image", "tag", "a", "b"],
                |c| matches!(c, Command::Tag(t) if t.source == "a" && t.target == "b"),
            ),
        ];
        for (argv, is) in cases {
            let command = parsed(argv);
            assert!(is(&command), "{argv:?}: {command:?}");
            assert!(
                !matches!(
                    command,
                    Command::Container(_) | Command::Image(ImageCommand::Ls(_))
                ),
                "{argv:?} left a group"
            );
        }
        // `image inspect` and `image prune` are the group's own.
        assert!(matches!(
            parsed(&["image", "inspect", "a"]),
            Command::Image(ImageCommand::Inspect { .. })
        ));
    }

    /// A failure exits as Docker's: 125 for `run`, 1 for every other
    /// command (a group's alias as its command).
    #[test]
    fn failures_exit_as_docker_s() {
        assert_eq!(parsed(&["run", "alpine"]).failure_code(), 125);
        assert_eq!(parsed(&["container", "run", "alpine"]).failure_code(), 125);
        for argv in [
            &["create", "alpine"][..],
            &["rm", "web"],
            &["inspect", "web"],
            &["exec", "web", "ls"],
            &["start", "-a", "web"],
            &["image", "rm", "a"],
            &["volume", "rm", "v"],
            &["doctor"],
        ] {
            assert_eq!(parsed(argv).failure_code(), 1, "{argv:?}");
        }
    }

    /// `argv` after `shipyard`, parsed and flattened as `main` does.
    fn parsed(argv: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("shipyard").chain(argv.iter().copied()))
            .unwrap()
            .command
            .flattened()
    }

    #[test]
    fn run_subcommand_parses() {
        let cli = Cli::try_parse_from(["shipyard", "run", "-D", "alpine", "ls", "-l"]).unwrap();
        assert!(cli.debug, "-D before the command is shipyard's flag");
        assert!(matches!(cli.command, Command::Run(_)));
    }
}
