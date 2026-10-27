//! `shipyard run`: its flags, as the engine's
//! [`RunRequest`](shipyard_engine::run::RunRequest), run by the engine; the
//! CLI ends as the run did.
//!
//! The flags and their values' parsers are here; [`request`] turns them into
//! the request, [`commands`] runs or makes it.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, value_parser};
use shipyard_engine::network::{Joined, NetworkMode};
use shipyard_engine::run::{CapabilityArg, SecurityOpt};
use shipyard_network::files::HostEntry;
use shipyard_runtime::isolation::IdMap;
use shipyard_runtime::process::{DEFAULT_STOP_TIMEOUT_SECS, Rlimit, Signal};

use shipyard_cli::limits::LimitArgs;

#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap's switches: each bool is one command-line flag"
)]
pub(crate) struct RunArgs {
    /// Print help (`-h` is the hostname's, as Docker's).
    #[arg(long, action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// `-i` and `-t`.
    #[command(flatten)]
    streams: StreamArgs,

    /// Set an environment variable, or copy KEY from your shell if no value is given.
    #[arg(short, long, value_name = "KEY[=VALUE]", value_parser = value_parser!(OsString))]
    env: Vec<OsString>,

    /// Read variables from a file, as Docker's: a KEY=VALUE per line (a bare
    /// KEY copies yours), # lines and blank ones skipped, values as they
    /// are. Before --env, which wins. Repeatable.
    #[arg(long, value_name = "FILE")]
    env_file: Vec<PathBuf>,

    /// Set a label on the container: KEY=VALUE, or KEY (an empty value).
    /// Repeatable.
    #[arg(short = 'l', long = "label", value_name = "KEY[=VALUE]")]
    labels: Vec<String>,

    /// Read labels from a file: a KEY=VALUE (or KEY) per line, # lines and
    /// blank ones skipped. Before --label, which wins. Repeatable.
    #[arg(long, value_name = "FILE")]
    label_file: Vec<PathBuf>,

    /// Write the container's ID to FILE once it's made (refused if FILE is
    /// there already).
    #[arg(long, value_name = "FILE")]
    cidfile: Option<PathBuf>,

    /// Print no pull progress.
    #[arg(short, long)]
    quiet: bool,

    /// Not supported: give the container the capabilities (--cap-add) and
    /// devices (--device) it needs instead.
    #[arg(long)]
    privileged: bool,

    /// Not supported yet: run the command without it (an image's own init,
    /// if it has one).
    #[arg(long)]
    init: bool,

    /// Working directory inside the container (absolute path): the image's,
    /// or `/`, if not given.
    #[arg(short, long, value_name = "DIR")]
    workdir: Option<PathBuf>,

    /// Run as this user and group: names (from the container's /etc/passwd
    /// and /etc/group) or numbers.
    #[arg(short, long, value_name = "USER[:GROUP]", value_parser = shipyard_engine::run::parse_user)]
    user: Option<(String, Option<String>)>,

    /// Add a supplementary group: a name or a number.
    #[arg(long, value_name = "GROUP", value_parser = shipyard_engine::run::parse_name_or_id)]
    group_add: Vec<String>,

    /// Set a resource limit, e.g. nofile=1024:2048 or core=0.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    ulimit: Vec<Rlimit>,

    /// The command's OOM score adjustment, -1000 to 1000: how willing the
    /// kernel's OOM killer is to pick it (lowering it needs root).
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    oom_score_adj: Option<i32>,

    /// Give the container the host's device HOST (a directory: every device
    /// below it), at CONTAINER inside (HOST's own path if not given), for
    /// PERMS, from rwm (all three if not given). Repeatable (Linux).
    #[arg(
        long,
        value_name = "HOST[:CONTAINER][:PERMS]",
        value_parser = crate::devices::parse_device,
        conflicts_with = "no_isolation"
    )]
    device: Vec<shipyard_engine::run::DeviceArg>,

    /// Let the container use a kind of device: "TYPE MAJOR:MINOR ACCESS",
    /// such as "c 1:3 rwm" or "b *:* r" (a for any type, * for any number).
    /// Repeatable (Linux).
    #[arg(
        long,
        value_name = "RULE",
        value_parser = crate::devices::parse_rule,
        conflicts_with = "no_isolation"
    )]
    device_cgroup_rule: Vec<shipyard_runtime::devices::DeviceRule>,

    /// Signal sent to stop the command.
    #[arg(long, value_name = "SIGNAL", default_value = "SIGTERM")]
    stop_signal: Signal,

    /// Seconds to wait after the stop signal before killing the command.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STOP_TIMEOUT_SECS)]
    stop_timeout: u64,

    /// A container's restart policy, when it ends: no, on-failure (an exit
    /// code other than 0; on-failure:N at most N times), always, or
    /// unless-stopped. Never after you stop it. Not with --rm.
    #[arg(long, value_name = "POLICY", default_value = "no")]
    restart: shipyard_engine::containers::RestartPolicy,

    /// A container's health check: CMD, run in the container by the image's
    /// shell (its SHELL, else /bin/sh -c) every --health-interval while it
    /// runs; its exit code says
    /// whether the container is well (0) or not. `ps` and `inspect` show
    /// what it says: starting, healthy or unhealthy. Without it, the image's
    /// own check (its HEALTHCHECK), which the other --health-* flags tune.
    #[arg(long, value_name = "CMD", value_parser = nonempty)]
    health_cmd: Option<String>,

    /// No health check, not even the image's own.
    #[arg(long, conflicts_with_all = [
        "health_cmd", "health_interval", "health_timeout", "health_start_period",
        "health_start_interval", "health_retries",
    ])]
    no_healthcheck: bool,

    /// Between two health checks (the image's check's, else 30s): a
    /// duration, such as 500ms, 30s, 1m or 2h.
    #[arg(long, value_name = "DURATION", value_parser = shipyard_cli::logs_output::parse_duration)]
    health_interval: Option<Duration>,

    /// The longest a health check may take (the image's check's, else 30s):
    /// then it's a failure.
    #[arg(long, value_name = "DURATION", value_parser = shipyard_cli::logs_output::parse_duration)]
    health_timeout: Option<Duration>,

    /// After a start, how long failed health checks don't count (the image's
    /// check's, else none).
    #[arg(long, value_name = "DURATION", value_parser = shipyard_cli::logs_output::parse_duration)]
    health_start_period: Option<Duration>,

    /// Between two health checks during the start period (the image's
    /// check's, else 5s).
    #[arg(long, value_name = "DURATION", value_parser = shipyard_cli::logs_output::parse_duration)]
    health_start_interval: Option<Duration>,

    /// How many failed health checks in a row make the container unhealthy
    /// (the image's check's, else 3).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    health_retries: Option<u32>,

    /// Run without isolation: the command sees the host's processes, mounts,
    /// hostname, IPC and network, as you. On Linux, commands are isolated by default
    /// (rootless, in a user namespace, without root); on macOS they always
    /// run like this for now.
    #[arg(long, conflicts_with = "limits")]
    no_isolation: bool,

    /// Pass N more descriptors on to the command, from 3 up (after socket
    /// activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as
    /// Podman does.
    #[arg(long, value_name = "N", default_value_t = 0)]
    preserve_fds: usize,

    /// Hostname inside the container (Linux).
    #[arg(
        short = 'h',
        long,
        value_name = "NAME",
        conflicts_with = "no_isolation"
    )]
    hostname: Option<String>,

    /// NIS domain name inside the container (Linux).
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    domainname: Option<String>,

    /// Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824
    /// (Linux): IPC settings, or net.* with a network of its own (any
    /// --network but host and container:).
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_sysctl, conflicts_with = "no_isolation")]
    sysctl: Vec<(String, String)>,

    /// The container's network: bridge (the default network: root's
    /// default), pasta (a network of its own through pasta: the default
    /// without root), host (the host's), none (one of its own with only a
    /// loopback interface), container:NAME (another running container's,
    /// as it is), or a network's name (Linux; bridge and names need root).
    /// Repeatable for bridge networks: the container joins each, in order.
    /// name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own
    /// settings.
    #[arg(long, value_name = "MODE", value_parser = parse_network, conflicts_with = "no_isolation")]
    network: Vec<NetworkMode>,

    /// The container's IPC namespace (Linux): private or shareable (its own:
    /// any running container's can be joined), or container:NAME (another
    /// running container's, with its /dev/shm).
    #[arg(long, value_name = "MODE", value_parser = shipyard_engine::run::IpcMode::parse, conflicts_with = "no_isolation")]
    ipc: Option<shipyard_engine::run::IpcMode>,

    /// The container's name: on each network made with `shipyard network
    /// create` that it joins, other containers find it by this name (Linux).
    /// Letters, digits, _ and -.
    #[arg(long, value_name = "NAME", value_parser = shipyard_network::dns::parse_name, conflicts_with = "no_isolation")]
    name: Option<String>,

    /// Another name the container answers to on each network made with
    /// `shipyard network create` that it joins. Repeatable (Linux).
    #[arg(long, value_name = "NAME", value_parser = shipyard_network::dns::parse_name, requires = "network")]
    network_alias: Vec<String>,

    /// The container's address on its network (a bridge network's; with
    /// one network only: on several, name=NETWORK,ip=IP gives each its own).
    #[arg(long, value_name = "IP", requires = "network")]
    ip: Option<std::net::IpAddr>,

    /// The container's IPv6 address on its network (one made with --ipv6;
    /// with one network only: on several, name=NETWORK,ip6=IP gives each its
    /// own).
    #[arg(long, value_name = "IP", requires = "network")]
    ip6: Option<std::net::Ipv6Addr>,

    /// The container's hardware address on its network (a bridge
    /// network's; with one network only), locally administered, like
    /// 02:42:0a:58:00:02.
    #[arg(long, value_name = "MAC", value_parser = shipyard_network::store::parse_mac, requires = "network")]
    mac_address: Option<[u8; 6]>,

    /// Add NAME with address IP to the container's /etc/hosts. Repeatable
    /// (Linux; with an image or --rootfs).
    #[arg(long, value_name = "NAME:IP", value_parser = shipyard_network::files::parse_host_entry, conflicts_with = "no_isolation")]
    add_host: Vec<HostEntry>,

    /// A DNS server for the container's /etc/resolv.conf, instead of the
    /// host's. Repeatable (Linux; with an image or --rootfs).
    #[arg(long, value_name = "IP", conflicts_with = "no_isolation")]
    dns: Vec<std::net::IpAddr>,

    /// A DNS search domain, instead of the host's. Repeatable (Linux; with
    /// an image or --rootfs).
    #[arg(long, value_name = "DOMAIN", value_parser = shipyard_network::files::parse_resolv_word, conflicts_with = "no_isolation")]
    dns_search: Vec<String>,

    /// A resolver option (like ndots:2), instead of the host's. Repeatable
    /// (Linux; with an image or --rootfs).
    #[arg(long, value_name = "OPTION", value_parser = shipyard_network::files::parse_resolv_word, conflicts_with = "no_isolation")]
    dns_option: Vec<String>,

    /// Publish a container's port on the host, like 8080:80 (the host's port
    /// 8080, on every IPv4 address, to the container's 80), 127.0.0.1:8080:80
    /// (on that address only), 80 (a free host port) or 53:53/udp. Ranges
    /// work (8000-8010:80-90). Repeatable (Linux; a bridge network, or
    /// pasta).
    #[arg(short = 'p', long, value_name = "SPEC", value_parser = parse_publish, conflicts_with = "no_isolation")]
    publish: Vec<PublishSpec>,

    /// Expose a port, or a range (8000-8010), TCP unless /udp: published
    /// with -P beside the image's own, and shown by inspect. Repeatable.
    #[arg(long, value_name = "PORT[/PROTO]", value_parser = parse_expose)]
    expose: Vec<ExposeSpec>,

    /// Publish every port the image says it uses (its exposed ports) on free
    /// host ports (Linux; a bridge network, or pasta).
    #[arg(short = 'P', long, conflicts_with = "no_isolation")]
    publish_all: bool,

    /// Use DIR as the container's root filesystem: it becomes `/` inside,
    /// with its own /proc, /sys and /dev (Linux).
    #[arg(long, value_name = "DIR", conflicts_with = "no_isolation")]
    rootfs: Option<PathBuf>,

    /// Run without an image: the command sees the host's files, isolated in
    /// its own namespaces otherwise (Linux), and COMMAND comes first.
    #[arg(long, conflicts_with_all = ["rootfs", "image"])]
    no_image: bool,

    /// The image, as a flag (once the only way): COMMAND then comes
    /// first. Hidden: the image goes first among the arguments now.
    #[arg(
        long,
        value_name = "REF",
        hide = true,
        conflicts_with_all = ["no_isolation", "rootfs"],
    )]
    image: Option<String>,

    /// Run COMMAND instead of the image's entrypoint (and without its
    /// default arguments); `""` runs the command alone.
    #[arg(long, value_name = "COMMAND", value_parser = value_parser!(OsString))]
    entrypoint: Option<OsString>,

    /// When to pull the image: `missing` (if the store doesn't have it: the
    /// default), `always`, or `never` (the store's, or an error).
    #[arg(long, value_enum, value_name = "WHEN")]
    pull: Option<PullPolicy>,

    /// The image's platform (`linux/amd64`: on a Mac, run through Rosetta);
    /// by default the image's own among what runs here, this host's first.
    #[arg(long, value_name = "PLATFORM")]
    platform: Option<String>,

    /// Make the container's root filesystem read-only (with an image or
    /// --rootfs).
    #[arg(long)]
    read_only: bool,

    /// Give the command a capability on top of the default set, like
    /// `NET_ADMIN` (or `CAP_NET_ADMIN`); `ALL` for every one. Repeatable
    /// (Linux).
    #[arg(long, value_name = "CAP", value_parser = clap::value_parser!(CapabilityArg), conflicts_with = "no_isolation")]
    cap_add: Vec<CapabilityArg>,

    /// Take a capability out of the default set; `ALL` for every one.
    /// Repeatable (Linux).
    #[arg(long, value_name = "CAP", value_parser = clap::value_parser!(CapabilityArg), conflicts_with = "no_isolation")]
    cap_drop: Vec<CapabilityArg>,

    /// A security option: `no-new-privileges=false` lets setuid programs
    /// inside gain privileges (they can't by default); `seccomp=FILE` filters
    /// system calls with a profile of your own, `seccomp=unconfined` not at
    /// all (Docker's profile by default). Repeatable (Linux).
    #[arg(long, value_name = "OPTION", value_parser = clap::value_parser!(SecurityOpt), conflicts_with = "no_isolation")]
    security_opt: Vec<SecurityOpt>,

    /// Run in a user namespace: container IDs CONTAINER.. are host IDs
    /// HOST.., SIZE of them, e.g. 0:100000:65536 (root inside is host user
    /// 100000). Repeatable (Linux, root only: without root, the maps are
    /// chosen for you).
    #[arg(long, value_name = "CONTAINER:HOST:SIZE", value_parser = parse_id_map, conflicts_with = "no_isolation")]
    uidmap: Vec<IdMap>,

    /// The same for group IDs; the --uidmap ranges if not given (Linux).
    #[arg(long, value_name = "CONTAINER:HOST:SIZE", value_parser = parse_id_map, requires = "uidmap")]
    gidmap: Vec<IdMap>,

    /// Run the OCI bundle in DIR: its config.json says what to run and how,
    /// so no COMMAND or container options are given (Linux).
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = [
            "env", "workdir", "user", "group_add", "ulimit", "oom_score_adj",
            "no_isolation", "hostname", "domainname", "sysctl", "network", "name",
            "network_alias", "ip", "ip6", "mac_address",
            "add_host", "dns", "dns_search", "dns_option", "publish", "publish_all", "rootfs",
            "read_only",
            "cap_add", "cap_drop", "security_opt", "uidmap", "gidmap", "limits", "device",
            "device_cgroup_rule", "image", "no_image", "pull", "entrypoint", "volume",
            "env_file", "labels", "label_file", "expose", "tmpfs", "mount",
        ],
    )]
    bundle: Option<PathBuf>,

    /// Memory, CPU, process and I/O limits.
    #[command(flatten)]
    limits: LimitArgs,

    /// Mount a named volume or host path into the container:
    /// `[SOURCE:]DEST[:MODES]`, MODES `ro` and `nocopy`, comma-joined. A
    /// bare SOURCE (no slash) is a named volume, created on first use; DEST
    /// alone, a new anonymous one; either, if empty, is filled with the
    /// image's files at DEST when the container is made (unless `nocopy`).
    /// A path is bind-mounted (it
    /// must exist); DEST alone is a new anonymous volume. Repeatable (Linux,
    /// with a root filesystem).
    #[arg(
        short = 'v',
        long,
        value_name = "[SOURCE:]DEST[:OPTIONS]",
        conflicts_with = "no_isolation"
    )]
    volume: Vec<String>,

    /// Mount a tmpfs (in memory, empty at each start) at PATH, with
    /// `mount(8)`'s OPTIONS: `size=64m`, `mode=1777`, `uid`, `gid`,
    /// `nr_inodes`, `ro`, and `exec`, `suid`, `dev` (it's `noexec`,
    /// `nosuid`, `nodev` without them, as Docker's). Repeatable.
    #[arg(
        long,
        value_name = "PATH[:OPTIONS]",
        value_parser = parse_tmpfs,
        conflicts_with = "no_isolation"
    )]
    tmpfs: Vec<shipyard_engine::run::TmpfsMount>,

    /// Mount as Docker's `--mount` does: `type=bind,source=/path,target=/d`,
    /// `type=volume,source=NAME,target=/d` (the default type), or
    /// `type=tmpfs,target=/d,tmpfs-size=64m,tmpfs-mode=1777`; `readonly`
    /// for any, `volume-nocopy` for a volume (not filled from the image).
    /// Repeatable.
    #[arg(
        long,
        value_name = "KEY=VALUE,...",
        value_parser = parse_mount,
        conflicts_with = "no_isolation"
    )]
    mount: Vec<shipyard_engine::run::DockerMount>,

    /// The image to run (a reference like `alpine:3.20` or
    /// `ghcr.io/owner/app@sha256:...`), then the command to run instead of
    /// its own, with its arguments. With --no-image, --rootfs or
    /// --no-isolation there's no image: the command comes first (--bundle takes
    /// none). Everything after the image is the command's, options too.
    #[arg(
        trailing_var_arg = true,
        value_names = ["IMAGE", "COMMAND"],
        value_parser = value_parser!(OsString),
    )]
    args: Vec<OsString>,
}

/// `shipyard run`: a container's flags, and `run`'s own.
#[derive(Debug, Args)]
pub(crate) struct RunCommand {
    /// Run in the background: the container is made and started, its ID
    /// printed, and it runs on once this command has ended (Linux).
    #[arg(short = 'd', long)]
    detach: bool,

    /// Remove the container once it ends (a foreground run returns once
    /// it's gone).
    #[arg(long)]
    rm: bool,

    /// In the foreground: pass on the signals this command gets (Ctrl-C, a
    /// kill) to the container. With false, Ctrl-C ends this command and
    /// the container runs on.
    #[arg(
        long,
        value_name = "BOOL",
        default_value_t = true,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        action = clap::ArgAction::Set
    )]
    sig_proxy: bool,

    /// In the foreground with -it: the keys that detach (the container goes
    /// on): ctrl-p,ctrl-q, or others separated by commas (empty: none).
    #[arg(long, value_name = "KEYS")]
    detach_keys: Option<String>,

    /// In the foreground: attach only these streams (stdin, stdout,
    /// stderr; repeatable). stdin takes your input, as -i. Without it,
    /// all of them.
    #[arg(short = 'a', long, value_name = "STREAM", value_parser = parse_attach, conflicts_with = "detach")]
    attach: Vec<AttachStream>,

    /// The container's flags, its image and its command.
    #[command(flatten)]
    args: RunArgs,
}

/// `shipyard create`: a container's flags, to start later.
#[derive(Debug, Args)]
pub(crate) struct CreateCommand {
    /// Remove the container once it ends, after its first start.
    #[arg(long)]
    rm: bool,

    /// The container's flags, its image and its command.
    #[command(flatten)]
    args: RunArgs,
}

/// What the command's standard input and output are: Docker's `-i` and `-t`.
#[derive(Debug, Args)]
pub(crate) struct StreamArgs {
    /// Pass your input on to the command. Without it, an isolated command's
    /// stdin is empty (/dev/null), so it can't take keys meant for your
    /// shell.
    #[arg(short, long)]
    interactive: bool,

    /// Give the command a terminal of the container's own (a pseudo-terminal):
    /// full-screen programs, password prompts and job control work. With -i,
    /// what you type goes to it.
    #[arg(short, long)]
    tty: bool,
}

/// When a run pulls its image (`--pull`), as Docker's: the flag's values,
/// for the engine's [`shipyard_engine::image::PullPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PullPolicy {
    /// Only if the store doesn't have it whole.
    Missing,
    /// Every time (a tag may have moved).
    Always,
    /// Never: the store's, or nothing.
    Never,
}

impl From<PullPolicy> for shipyard_engine::image::PullPolicy {
    fn from(policy: PullPolicy) -> Self {
        match policy {
            PullPolicy::Missing => Self::Missing,
            PullPolicy::Always => Self::Always,
            PullPolicy::Never => Self::Never,
        }
    }
}

fn parse_id_map(s: &str) -> Result<IdMap, String> {
    s.parse()
        .map_err(|err: shipyard_runtime::isolation::IdMapError| err.to_string())
}

/// One `-p`: the ports it publishes.
#[derive(Debug, Clone)]
pub(crate) struct PublishSpec(Vec<shipyard_network::ports::PortRequest>);

/// `-p`'s `SPEC`.
fn parse_publish(s: &str) -> Result<PublishSpec, String> {
    shipyard_network::ports::parse_publish(s)
        .map(PublishSpec)
        .map_err(|error| error.to_string())
}

/// One `--expose`: the ports it exposes, as an image's `ExposedPorts` keys.
#[derive(Debug, Clone)]
pub(crate) struct ExposeSpec(Vec<String>);

/// `--expose`'s `PORT[-END][/PROTO]`: `80`, `53/udp`, `8000-8010/tcp`.
fn parse_expose(s: &str) -> Result<ExposeSpec, String> {
    let (ports, protocol) = s.split_once('/').unwrap_or((s, "tcp"));
    if protocol != "tcp" && protocol != "udp" {
        return Err(format!("{protocol}: give tcp or udp"));
    }
    let (start, end) = shipyard_network::ports::parse_range(ports)
        .ok_or_else(|| format!("{ports}: give a port from 1 to 65535, or a range (8000-8010)"))?;
    Ok(ExposeSpec(
        (start..=end)
            .map(|port| format!("{port}/{protocol}"))
            .collect(),
    ))
}

/// A stream `-a` attaches: stdin, stdout or stderr (any case, as Docker's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachStream {
    /// Its input.
    Stdin,
    /// Its output.
    Stdout,
    /// Its errors.
    Stderr,
}

/// `-a`'s `STREAM`.
fn parse_attach(s: &str) -> Result<AttachStream, String> {
    match s.to_ascii_lowercase().as_str() {
        "stdin" => Ok(AttachStream::Stdin),
        "stdout" => Ok(AttachStream::Stdout),
        "stderr" => Ok(AttachStream::Stderr),
        _ => Err(format!("{s}: give stdin, stdout or stderr")),
    }
}

/// `--network`'s mode: `host`, `none`, `pasta`, `container:NAME`, `bridge`
/// (the default network), or a network's name; or Docker's long form for a
/// network,
/// `name=NAME[,ip=IP][,ip6=IP][,mac-address=MAC][,alias=NAME…]`.
fn parse_network(s: &str) -> Result<NetworkMode, String> {
    match s {
        "host" => Ok(NetworkMode::Host),
        "none" => Ok(NetworkMode::None),
        "pasta" => Ok(NetworkMode::Pasta),
        container if let Some(parsed) = NetworkMode::parse_container(container) => {
            parsed.map_err(str::to_owned)
        }
        long if long.contains('=') => {
            parse_long_network(long).map(|joined| NetworkMode::Bridges(vec![joined]))
        }
        name => {
            shipyard_network::store::validate_name(name).map_err(|error| error.to_string())?;
            Ok(NetworkMode::bridge(name))
        }
    }
}

/// `--network`'s long form: `name=NAME`, then any of `ip=IP`, `ip6=IP`,
/// `mac-address=MAC` and `alias=NAME` (repeatable), separated by commas.
fn parse_long_network(text: &str) -> Result<Joined, String> {
    let mut name = None;
    let mut joined = Joined::default();
    for setting in text.split(',') {
        let Some((key, value)) = setting.split_once('=') else {
            return Err(format!("{setting:?} isn't KEY=VALUE"));
        };
        match key {
            "name" if name.is_none() => {
                if shipyard_engine::network::RESERVED.contains(&value) {
                    return Err(format!(
                        "--network {value} takes no settings: only bridge networks do"
                    ));
                }
                shipyard_network::store::validate_name(value).map_err(|error| error.to_string())?;
                name = Some(value.to_owned());
            }
            "ip" if joined.ip.is_none() => {
                joined.ip = Some(
                    value
                        .parse()
                        .map_err(|_| format!("ip={value}: not an IP address"))?,
                );
            }
            "ip6" if joined.ip6.is_none() => {
                joined.ip6 = Some(
                    value
                        .parse()
                        .map_err(|_| format!("ip6={value}: not an IPv6 address"))?,
                );
            }
            "mac-address" if joined.mac.is_none() => {
                joined.mac = Some(shipyard_network::store::parse_mac(value)?);
            }
            "alias" => joined
                .aliases
                .push(shipyard_network::dns::parse_name(value)?),
            "name" | "ip" | "ip6" | "mac-address" => return Err(format!("{key}= is given twice")),
            _ => {
                return Err(format!(
                    "{key}= isn't a network setting: name=, ip=, ip6=, mac-address= or alias="
                ));
            }
        }
    }
    joined.name = name.ok_or("the long form needs name=NETWORK")?;
    Ok(joined)
}

/// `KEY=VALUE`, split at the first `=`. Which keys are allowed is checked
/// with the rest of the isolation.
fn parse_sysctl(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("\"{s}\" isn't KEY=VALUE"))
}

/// A value that can't be empty (`--health-cmd`'s).
fn nonempty(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        Err("it can't be empty".to_owned())
    } else {
        Ok(text.to_owned())
    }
}

mod commands;
pub(crate) mod files;
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only a Mac forwards; its rules are tested everywhere"
    )
)]
mod forwarded;
mod request;
pub(crate) use request::{caller_env_of, copied_env};

pub(crate) use commands::{create, run_command};
#[cfg(test)]
mod tests;

/// `--tmpfs`'s value, checked by the engine's rules.
fn parse_tmpfs(spec: &str) -> Result<shipyard_engine::run::TmpfsMount, String> {
    shipyard_engine::run::TmpfsMount::parse(spec).map_err(|error| error.to_string())
}

/// `--mount`'s value, read by the engine's rules (Docker's keys).
fn parse_mount(spec: &str) -> Result<shipyard_engine::run::DockerMount, String> {
    shipyard_engine::run::parse_mount(spec).map_err(|error| error.to_string())
}
