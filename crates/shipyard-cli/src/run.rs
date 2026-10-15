//! `shipyard run`: its flags, as the engine's [`RunRequest`], run by the
//! engine; the CLI ends as the run did.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
#[cfg(test)]
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, value_parser};
use shipyard_engine::network::{Joined, NetworkMode};
use shipyard_engine::run::{
    CapabilityArg, Limits, Outcome, RunRequest, SecurityOpt, Streams, Supervision,
};
use shipyard_network::files::HostEntry;
use shipyard_runtime::isolation::IdMap;
use shipyard_runtime::process::{self, DEFAULT_STOP_TIMEOUT_SECS, Rlimit, Signal};

use crate::limits::LimitArgs;

#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap's switches: each bool is one command-line flag"
)]
pub(crate) struct RunArgs {
    /// `-i` and `-t`.
    #[command(flatten)]
    streams: StreamArgs,

    /// Set an environment variable, or copy KEY from your shell if no value is given.
    #[arg(short, long, value_name = "KEY[=VALUE]", value_parser = value_parser!(OsString))]
    env: Vec<OsString>,

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
    #[arg(long, value_name = "DURATION", value_parser = crate::logs_output::parse_duration)]
    health_interval: Option<Duration>,

    /// The longest a health check may take (the image's check's, else 30s):
    /// then it's a failure.
    #[arg(long, value_name = "DURATION", value_parser = crate::logs_output::parse_duration)]
    health_timeout: Option<Duration>,

    /// After a start, how long failed health checks don't count (the image's
    /// check's, else none).
    #[arg(long, value_name = "DURATION", value_parser = crate::logs_output::parse_duration)]
    health_start_period: Option<Duration>,

    /// Between two health checks during the start period (the image's
    /// check's, else 5s).
    #[arg(long, value_name = "DURATION", value_parser = crate::logs_output::parse_duration)]
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
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    hostname: Option<String>,

    /// NIS domain name inside the container (Linux).
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    domainname: Option<String>,

    /// Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824
    /// (Linux): IPC settings, or net.* with a network of its own (any
    /// --network but host).
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_sysctl, conflicts_with = "no_isolation")]
    sysctl: Vec<(String, String)>,

    /// The container's network: bridge (the default network: root's
    /// default), pasta (a network of its own through pasta: the default
    /// without root), host (the host's), none (one of its own with only a
    /// loopback interface), or a network's name (Linux; bridge and names
    /// need root). Repeatable for bridge networks: the container joins each,
    /// in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one
    /// its own settings.
    #[arg(long, value_name = "MODE", value_parser = parse_network, conflicts_with = "no_isolation")]
    network: Vec<NetworkMode>,

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

    /// Make the container's root filesystem read-only (with an image or
    /// --rootfs).
    #[arg(long)]
    read_only: bool,

    /// Give the command a capability on top of the default set, like
    /// `NET_ADMIN` (or `CAP_NET_ADMIN`); `ALL` for every one. Repeatable
    /// (Linux).
    #[arg(long, value_name = "CAP", value_parser = parse_capability, conflicts_with = "no_isolation")]
    cap_add: Vec<CapabilityArg>,

    /// Take a capability out of the default set; `ALL` for every one.
    /// Repeatable (Linux).
    #[arg(long, value_name = "CAP", value_parser = parse_capability, conflicts_with = "no_isolation")]
    cap_drop: Vec<CapabilityArg>,

    /// A security option: `no-new-privileges=false` lets setuid programs
    /// inside gain privileges (they can't by default); `seccomp=FILE` filters
    /// system calls with a profile of your own, `seccomp=unconfined` not at
    /// all (Docker's profile by default). Repeatable (Linux).
    #[arg(long, value_name = "OPTION", value_parser = parse_security_opt, conflicts_with = "no_isolation")]
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
        ],
    )]
    bundle: Option<PathBuf>,

    /// Memory, CPU, process and I/O limits.
    #[command(flatten)]
    limits: LimitArgs,

    /// Mount a named volume or host path into the container:
    /// `SOURCE:DEST[:ro]`. A bare SOURCE (no slash) is a named volume, created
    /// on first use; a path is bind-mounted. Repeatable (Linux, with a root
    /// filesystem).
    #[arg(long, value_name = "SOURCE:DEST[:ro]", conflicts_with = "no_isolation")]
    volume: Vec<String>,

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

impl RunArgs {
    /// The image these arguments name, and the command: the first argument
    /// is the image unless a flag says there's none (`--no-image`,
    /// `--rootfs`, `--bundle`, `--no-isolation`) or gives it (`--image`).
    /// Then the rules that need an image or a root filesystem.
    ///
    /// # Errors
    ///
    /// A usage error (printed as clap's, exit 125): no image where one is needed, or a
    /// flag that needs one (or a root filesystem, or a command) without it.
    fn image_and_command(&mut self) -> Result<(Option<String>, Vec<OsString>), clap::Error> {
        use clap::error::ErrorKind;
        let usage = |kind, message: &str| clap::Error::raw(kind, message.to_owned());
        let args = std::mem::take(&mut self.args);
        let without_image =
            self.no_image || self.rootfs.is_some() || self.bundle.is_some() || self.no_isolation;
        let (image, command) = if let Some(image) = self.image.take() {
            (Some(image), args)
        } else if without_image {
            (None, args)
        } else {
            let mut args = args.into_iter();
            let Some(image) = args.next() else {
                return Err(usage(
                    ErrorKind::MissingRequiredArgument,
                    "an IMAGE is needed (or --no-image, --rootfs, --bundle or --no-isolation to run \
                     without one)",
                ));
            };
            let image = image.into_string().map_err(|image| {
                usage(
                    ErrorKind::InvalidValue,
                    &format!("the image {} isn't UTF-8", image.display()),
                )
            })?;
            (Some(image), args.collect())
        };
        let needs_image = |flag: &str| {
            usage(
                ErrorKind::MissingRequiredArgument,
                &format!("{flag} needs an image"),
            )
        };
        if image.is_none() {
            if self.publish_all {
                return Err(needs_image("--publish-all"));
            }
            if self.entrypoint.is_some() {
                return Err(needs_image("--entrypoint"));
            }
            if self.pull.is_some() {
                return Err(needs_image("--pull"));
            }
            if self.read_only && self.rootfs.is_none() {
                return Err(usage(
                    ErrorKind::MissingRequiredArgument,
                    "--read-only needs an image or --rootfs",
                ));
            }
        }
        if self.bundle.is_some() && !command.is_empty() {
            return Err(usage(
                ErrorKind::ArgumentConflict,
                "--bundle takes no COMMAND: its config.json says what to run",
            ));
        }
        if image.is_none() && self.bundle.is_none() && command.is_empty() {
            return Err(usage(
                ErrorKind::MissingRequiredArgument,
                "a COMMAND is needed without an image",
            ));
        }
        Ok((image, command))
    }

    /// The run these flags ask for, as the engine takes it.
    ///
    /// # Errors
    ///
    /// A usage error: see [`image_and_command`](Self::image_and_command).
    fn into_request(mut self) -> Result<RunRequest, clap::Error> {
        let (image, command) = self.image_and_command()?;
        // A bare `--env KEY` copies the caller's value: this process's
        // environment is the caller's, read here, never by the engine.
        let caller_env = self
            .env
            .iter()
            .filter(|entry| !entry.as_bytes().contains(&b'='))
            .filter_map(|key| Some((key.clone(), std::env::var_os(key)?)))
            .collect();
        let limits = Limits {
            first_given: self.limits.first_given(),
            resources: self
                .limits
                .resources()
                .map_err(|error| format!("{error:#}")),
        };
        let health = self.health()?;
        let mut request = RunRequest::default();
        request.streams = Streams {
            interactive: self.streams.interactive,
            tty: self.streams.tty,
        };
        request.env = self.env;
        request.caller_env = caller_env;
        request.workdir = self.workdir;
        request.user = self.user;
        request.group_add = self.group_add;
        request.ulimit = self.ulimit;
        request.oom_score_adj = self.oom_score_adj;
        request.device = self.device;
        request.device_cgroup_rule = self.device_cgroup_rule;
        request.stop_signal = self.stop_signal;
        request.stop_timeout = self.stop_timeout;
        request.restart = self.restart;
        request.health = health;
        request.no_isolation = self.no_isolation;
        request.hostname = self.hostname;
        request.domainname = self.domainname;
        request.sysctl = self.sysctl;
        request.network = self.network;
        request.name = self.name;
        request.network_alias = self.network_alias;
        request.ip = self.ip;
        request.ip6 = self.ip6;
        request.mac_address = self.mac_address;
        request.add_host = self.add_host;
        request.dns = self.dns;
        request.dns_search = self.dns_search;
        request.dns_option = self.dns_option;
        request.publish = self.publish.into_iter().flat_map(|spec| spec.0).collect();
        request.publish_all = self.publish_all;
        request.rootfs = self.rootfs;
        request.image = image;
        request.entrypoint = self.entrypoint;
        request.pull = self.pull.map(Into::into);
        request.read_only = self.read_only;
        request.cap_add = self.cap_add;
        request.cap_drop = self.cap_drop;
        request.security_opt = self.security_opt;
        request.uidmap = self.uidmap;
        request.gidmap = self.gidmap;
        request.bundle = self.bundle;
        request.limits = limits;
        request.volume = self.volume;
        request.command = command;
        Ok(request)
    }
}

fn parse_id_map(s: &str) -> Result<IdMap, String> {
    s.parse()
        .map_err(|err: shipyard_runtime::isolation::IdMapError| err.to_string())
}

fn parse_capability(s: &str) -> Result<CapabilityArg, String> {
    if s.eq_ignore_ascii_case("ALL") {
        return Ok(CapabilityArg::All);
    }
    s.parse()
        .map(CapabilityArg::One)
        .map_err(|err| format!("{err}"))
}

fn parse_security_opt(s: &str) -> Result<SecurityOpt, String> {
    let (key, value) = match s.split_once(['=', ':']) {
        Some((key, value)) => (key, Some(value)),
        None => (s, None),
    };
    match (key, value) {
        ("no-new-privileges", None | Some("true")) => Ok(SecurityOpt::NoNewPrivileges(true)),
        ("no-new-privileges", Some("false")) => Ok(SecurityOpt::NoNewPrivileges(false)),
        ("no-new-privileges", Some(other)) => Err(format!(
            "no-new-privileges takes true or false, not \"{other}\""
        )),
        ("seccomp", Some("unconfined")) => Ok(SecurityOpt::SeccompUnconfined),
        ("seccomp", Some(path)) if !path.is_empty() => {
            Ok(SecurityOpt::SeccompProfile(PathBuf::from(path)))
        }
        ("seccomp", _) => Err("seccomp takes a profile file, or unconfined".to_owned()),
        _ => Err(format!(
            "unknown security option \"{s}\" (known: no-new-privileges[=true|false], \
             seccomp=FILE|unconfined)"
        )),
    }
}

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
        let foreground = shipyard_engine::run::Foreground {
            sig_proxy: command.sig_proxy,
            detach_keys,
        };
        return run(command.args, command.rm, foreground);
    }
    let request = match container_request(command.args, "run") {
        Ok(request) => request,
        Err(code) => return Ok(code),
    };
    let engine = crate::engine();
    let detached = shipyard_engine::run::detached(&engine, request, command.rm)?;
    writeln!(std::io::stdout().lock(), "{}", detached.id.as_str())?;
    // Only now may a Ctrl-C held off while it was made end this process.
    drop(detached);
    Ok(ExitCode::SUCCESS)
}

/// `shipyard create`: the container made, its ID printed.
pub(crate) fn create(command: CreateCommand) -> Result<ExitCode> {
    let request = match container_request(command.args, "create") {
        Ok(request) => request,
        Err(code) => return Ok(code),
    };
    let record = shipyard_engine::run::create(&crate::engine(), request, command.rm)?;
    writeln!(std::io::stdout().lock(), "{}", record.id.as_str())?;
    Ok(ExitCode::SUCCESS)
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
    let mut cli = crate::Cli::command();
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
pub(crate) fn run(
    args: RunArgs,
    auto_remove: bool,
    foreground: shipyard_engine::run::Foreground,
) -> Result<ExitCode> {
    // First, before anything is opened or started (the network's helpers):
    // what this process was given to pass on is 3 and up, and nothing it
    // starts in between may inherit it.
    let passed = process::passed::PassedFds::from_caller(args.preserve_fds)?;
    // A usage error, as clap's own: printed, exit 125 (Docker's).
    let request = match args.into_request() {
        Ok(request) => request,
        Err(error) => return Ok(usage_error(error, "run")),
    };
    let host_command = request
        .image
        .as_deref()
        .filter(|image| is_host_command(image))
        .map(str::to_owned);
    // One engine for the whole run: its places and reporter, read once.
    let engine = crate::engine();
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
                crate::warn(why)?;
            }
            // A container's own: said where they do nothing.
            if auto_remove {
                crate::warn(
                    "--rm: this run isn't a container: it leaves nothing to \
                     remove",
                )?;
            }
            if request.restart != shipyard_engine::containers::RestartPolicy::No {
                crate::warn(
                    "--restart: this run isn't a container: nothing starts \
                     it again",
                )?;
            }
            if request
                .health
                .as_ref()
                .is_some_and(|health| !health.is_disabled())
            {
                crate::warn(
                    "--health-*: this run isn't a container: nothing \
                     checks it",
                )?;
            }
            if !foreground.sig_proxy {
                crate::warn(
                    "--sig-proxy=false: this run isn't a container: its \
                     signals are passed on as they come",
                )?;
            }
            if foreground.detach_keys.is_some() {
                crate::warn(
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
        let _ = crate::hint(format_args!(
            "`{word}` was taken as an image, and is also a program here: to \
             run the host's, give --no-image first (shipyard run --no-image {word} …)"
        ));
    }
}

/// Whether `image` is a bare word (no registry, path, tag or digest) that
/// names a program on this process's `PATH`: what `run sh` meant before
/// the image came first.
fn is_host_command(image: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    if image.contains(['/', ':', '@', '.']) {
        return false;
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(image))
                .is_ok_and(|file| file.is_file() && file.permissions().mode() & 0o111 != 0)
        })
    })
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

/// `--network`'s mode: `host`, `none`, `pasta`, `bridge` (the default
/// network), or a network's name; or Docker's long form for a network,
/// `name=NAME[,ip=IP][,ip6=IP][,mac-address=MAC][,alias=NAME…]`.
fn parse_network(s: &str) -> Result<NetworkMode, String> {
    match s {
        "host" => Ok(NetworkMode::Host),
        "none" => Ok(NetworkMode::None),
        "pasta" => Ok(NetworkMode::Pasta),
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

#[cfg(test)]
use shipyard_engine::run::{Checked, Invocation};

#[cfg(test)]
impl RunArgs {
    /// Checked and finished without an image, as a run without one is.
    fn into_invocation(self) -> Result<Invocation> {
        let engine = crate::engine();
        match shipyard_engine::run::check(&engine, self.into_request()?)? {
            Checked::Bundle(invocation) => Ok(*invocation),
            Checked::Command(planned) => Ok(planned.finish(&engine, None, None)?.0),
        }
    }
}

impl RunArgs {
    /// The health check asked: `--no-healthcheck`'s none; `--health-cmd`'s
    /// (tuned by the other flags); the other flags alone, tuning the
    /// image's own (an empty test); or nothing asked. A duration under a
    /// millisecond (not zero) is refused, as Docker refuses it.
    fn health(&self) -> Result<Option<shipyard_engine::health::HealthCheck>, clap::Error> {
        use shipyard_engine::health::{HealthCheck, flag_millis};
        if self.no_healthcheck {
            return Ok(Some(HealthCheck::disabled()));
        }
        let tuned = self.health_interval.is_some()
            || self.health_timeout.is_some()
            || self.health_start_period.is_some()
            || self.health_start_interval.is_some()
            || self.health_retries.is_some();
        if self.health_cmd.is_none() && !tuned {
            return Ok(None);
        }
        let millis = |flag: &str, given: Option<Duration>| {
            flag_millis(flag, given).map_err(|error| {
                clap::Error::raw(
                    clap::error::ErrorKind::ValueValidation,
                    format!("{error}\n"),
                )
            })
        };
        Ok(Some(HealthCheck {
            test: self
                .health_cmd
                .as_deref()
                .map(|script| HealthCheck::shell(script).test)
                .unwrap_or_default(),
            interval_ms: millis("--health-interval", self.health_interval)?,
            timeout_ms: millis("--health-timeout", self.health_timeout)?,
            start_period_ms: millis("--health-start-period", self.health_start_period)?,
            start_interval_ms: millis("--health-start-interval", self.health_start_interval)?,
            retries: self.health_retries.unwrap_or(0),
            shell: Vec::new(),
        }))
    }
}

/// A value that can't be empty (`--health-cmd`'s).
fn nonempty(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        Err("it can't be empty".to_owned())
    } else {
        Ok(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::Parser;
    use std::time::Duration;

    use shipyard_runtime::isolation::Namespace;
    use shipyard_runtime::process::{ProcessSpec, RlimitKind, StopPolicy, User};

    use super::*;

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        run: RunArgs,
    }

    fn parse(args: &[&str]) -> Result<RunArgs, clap::Error> {
        let argv = std::iter::once("run").chain(args.iter().copied());
        Harness::try_parse_from(argv).map(|h| h.run)
    }

    fn spec(args: &[&str]) -> ProcessSpec {
        parse(args)
            .unwrap()
            .into_invocation()
            .unwrap()
            .spec()
            .clone()
    }

    /// The request these arguments make, or the usage error.
    fn request(args: &[&str]) -> Result<RunRequest, clap::Error> {
        parse(args)?.into_request()
    }

    /// The health flags as the engine takes them: `--health-cmd`'s check
    /// (tuned); the others alone, tuning the image's (no test);
    /// `--no-healthcheck`, off (refused with any other); nothing asked.
    #[test]
    fn health_flags_ask_as_dockers() {
        use shipyard_engine::health::HealthCheck;
        let health = |args: &[&str]| {
            let mut all = args.to_vec();
            all.push("alpine");
            request(&all).map(|run| run.health)
        };
        assert_eq!(health(&[]).unwrap(), None);
        let full = health(&["--health-cmd", "true", "--health-retries", "2"])
            .unwrap()
            .unwrap();
        assert_eq!((full.test.len(), full.retries), (2, 2));
        let tuned = health(&["--health-interval", "2s"]).unwrap().unwrap();
        // A millisecond is the least a flag can say (`500us` isn't a
        // duration here): kept as given. (Under one is refused by the
        // engine's `flag_millis`, tested there.)
        assert_eq!(
            health(&["--health-timeout", "1ms"])
                .unwrap()
                .unwrap()
                .timeout_ms,
            1
        );
        assert!(tuned.test.is_empty());
        assert_eq!(tuned.interval_ms, 2000);
        assert_eq!(
            health(&["--no-healthcheck"]).unwrap(),
            Some(HealthCheck::disabled())
        );
        assert!(health(&["--no-healthcheck", "--health-retries", "1"]).is_err());
    }

    /// The first argument is the image, unless a flag says there's none;
    /// everything after it is the command's.
    #[test]
    fn the_image_comes_first() {
        let run = request(&["-e", "A=1", "alpine", "ls", "--rm", "-e", "B"]).unwrap();
        assert_eq!(run.image.as_deref(), Some("alpine"));
        assert_eq!(
            run.command,
            ["ls", "--rm", "-e", "B"],
            "options after it are the command's"
        );
        assert_eq!(run.env, ["A=1"]);
        let alone = request(&["alpine"]).unwrap();
        assert!(alone.command.is_empty(), "the image's own command");
        for (flags, first) in [
            (&["--no-image"][..], "ls"),
            (&["--rootfs", "/r"][..], "ls"),
            (&["--no-isolation"][..], "ls"),
        ] {
            let run = request(&[flags, &["ls", "-l"]].concat()).unwrap();
            assert!(run.image.is_none(), "{flags:?}");
            assert_eq!(run.command, [first, "-l"], "{flags:?}");
        }
        let alias = request(&["--image", "alpine", "ls"]).unwrap();
        assert_eq!(alias.image.as_deref(), Some("alpine"), "--image, hidden");
        assert_eq!(alias.command, ["ls"]);
        let dashed = request(&["alpine", "--", "-x"]).unwrap();
        assert_eq!(
            dashed.command,
            ["--", "-x"],
            "after the image, `--` is the command's too"
        );
        let before = request(&["--", "alpine", "ls"]).unwrap();
        assert_eq!(
            before.image.as_deref(),
            Some("alpine"),
            "`--` before the image"
        );
        assert_eq!(before.command, ["ls"]);
        let both = request(&["--image", "alpine", "busybox", "ls"]).unwrap();
        assert_eq!(
            both.command,
            ["busybox", "ls"],
            "with --image, all are the command"
        );
        let mut args = parse(&[]).unwrap();
        args.args = vec![OsString::from_vec(vec![b'a', 0xff]), "true".into()];
        assert!(
            args.into_request()
                .unwrap_err()
                .to_string()
                .contains("isn't UTF-8")
        );
    }

    #[test]
    fn a_bare_word_on_the_path_is_a_host_command() {
        assert!(is_host_command("sh"), "/bin/sh, everywhere");
        assert!(!is_host_command("alpine:3.20"));
        assert!(!is_host_command("ghcr.io/owner/app"));
        assert!(!is_host_command("no-such-program-anywhere"));
    }

    /// The flags that need an image: with a positional one they're fine,
    /// without one each is a usage error (exit 125, as clap's).
    #[test]
    fn flags_that_need_an_image() {
        for flag in [
            &["-P"][..],
            &["--entrypoint", "x"],
            &["--pull", "always"],
            &["--read-only"],
        ] {
            assert!(
                request(&[flag, &["alpine", "true"]].concat()).is_ok(),
                "{flag:?}"
            );
            let refused = request(&[&["--no-image"], flag, &["true"]].concat()).unwrap_err();
            assert!(
                refused.to_string().contains("needs an image"),
                "{flag:?}: {refused}"
            );
        }
        assert!(request(&["--rootfs", "/r", "--read-only", "true"]).is_ok());
        let none = request(&[]).unwrap_err();
        assert!(none.to_string().contains("an IMAGE is needed"), "{none}");
        let no_command = request(&["--no-image"]).unwrap_err();
        assert!(
            no_command.to_string().contains("a COMMAND is needed"),
            "{no_command}"
        );
        assert!(
            parse(&["--bundle", "/b", "--entrypoint", "x"]).is_err(),
            "a clap conflict"
        );
        assert!(
            request(&["--bundle", "/b", "true"]).is_err(),
            "a bundle takes no command"
        );
        assert!(parse(&["--no-image", "--image", "alpine", "true"]).is_err());
        assert!(parse(&["--no-image", "--rootfs", "/r", "true"]).is_err());
    }

    #[test]
    fn a_volume_needs_a_root_filesystem() {
        let err = parse(&["--no-image", "--volume", "data:/data", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        let expected = if cfg!(target_os = "linux") {
            "needs a root filesystem"
        } else {
            "--volume needs Linux"
        };
        assert!(err.to_string().contains(expected), "{err}");
    }

    #[test]
    fn command_flags_are_not_ours() {
        let run = parse(&[
            "--no-image",
            "-e",
            "A=1",
            "ls",
            "-l",
            "--env",
            "X",
            "-u",
            "5",
        ])
        .unwrap();
        assert_eq!(run.args, ["ls", "-l", "--env", "X", "-u", "5"]);
        assert_eq!(run.env, ["A=1"]);
        assert!(run.user.is_none());
    }

    #[test]
    fn double_dash_works() {
        let run = parse(&["--no-image", "--", "--weird-program", "x"]).unwrap();
        assert_eq!(run.args, ["--weird-program", "x"]);
    }

    #[test]
    fn unknown_flags_before_the_command_are_errors() {
        assert!(parse(&["--no-image", "--bogus", "ls"]).is_err());
    }

    #[test]
    fn command_is_required() {
        assert!(request(&["--no-image", "-e", "A=1"]).is_err());
    }

    #[test]
    fn user_and_groups_are_resolved_where_the_command_runs() {
        let user = spec(&[
            "--no-image",
            "-u",
            "alice:staff",
            "--group-add",
            "wheel",
            "true",
        ])
        .user;
        assert_eq!(user.names.user.as_deref(), Some("alice"));
        assert_eq!(user.names.group.as_deref(), Some("staff"));
        assert_eq!(user.names.additional_groups, ["wheel"]);
        // A container's own /etc/group, never the host's.
        assert_eq!(user.names.memberships, cfg!(target_os = "linux"));
        let host = spec(&["--no-isolation", "-u", "alice", "true"]).user;
        assert!(!host.names.memberships);
    }

    #[test]
    fn without_a_user_flag_isolated_runs_are_root_inside() {
        let ids = |user: User| (user.uid, user.gid);
        let current = ids(User::current());
        assert_eq!(ids(spec(&["--no-isolation", "true"]).user), current);
        // Isolated (on Linux): root inside, whoever runs shipyard.
        let isolated = if cfg!(target_os = "linux") {
            (0, 0)
        } else {
            current
        };
        assert_eq!(ids(spec(&["--no-image", "true"]).user), isolated);
    }

    #[test]
    fn groups_are_deduplicated_in_order() {
        let user = spec(&[
            "--no-image",
            "--group-add",
            "20",
            "--group-add",
            "10",
            "--group-add",
            "20",
            "true",
        ])
        .user;
        assert_eq!(user.names.additional_groups, ["20", "10"]);
    }

    #[test]
    fn later_ulimit_wins() {
        let spec = spec(&[
            "--no-image",
            "--ulimit",
            "nofile=10",
            "--ulimit",
            "core=0",
            "--ulimit",
            "nofile=20:30",
            "true",
        ]);
        assert_eq!(spec.rlimits.len(), 2);
        assert_eq!(spec.rlimits[0].kind, RlimitKind::Nofile);
        assert_eq!(spec.rlimits[0].to_string(), "nofile=20:30");
    }

    #[test]
    fn bad_values_are_rejected_at_parse_time() {
        assert!(parse(&["--no-image", "--ulimit", "nofile=10:5", "true"]).is_err());
        assert!(parse(&["--no-image", "--stop-signal", "FOO", "true"]).is_err());
        assert!(parse(&["--no-image", "--stop-timeout", "-1", "true"]).is_err());
        assert!(parse(&["--no-image", "--group-add", "a:b", "true"]).is_err());
        assert!(parse(&["--no-image", "--sysctl", "kernel.shmmax", "true"]).is_err());
    }

    #[test]
    fn relative_workdir_fails_validation() {
        let err = parse(&["--no-image", "-w", "tmp", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn env_is_not_inherited_and_has_defaults() {
        let env = spec(&["--no-image", "-e", "A=1", "true"]).env;
        assert_eq!(env.get(OsStr::new("A")), Some(OsStr::new("1")));
        assert!(env.get(OsStr::new("PATH")).is_some());
        // HOME is set where the command runs, from the user's /etc/passwd.
        assert_eq!(env.len(), 2, "nothing from the host leaked in: {env:?}");
    }

    #[test]
    fn bare_env_key_copies_from_the_shell_or_is_skipped() {
        // HOME is set on any real host and never equals our default ("/" or
        // "/root" for root), so a match proves the value was really copied.
        let Some(host_home) = std::env::var_os("HOME") else {
            return;
        };
        let env = spec(&[
            "--no-image",
            "-e",
            "HOME",
            "-e",
            "SHIPYARD_SURELY_UNSET_9F3A",
            "true",
        ])
        .env;
        assert_eq!(env.get(OsStr::new("HOME")), Some(host_home.as_os_str()));
        assert!(env.get(OsStr::new("SHIPYARD_SURELY_UNSET_9F3A")).is_none());
    }

    #[test]
    fn env_errors_never_echo_values() {
        for entry in ["=hunter2", "A=b\0hunter2"] {
            let err = parse(&["--no-image", "-e", entry, "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{entry}: {err:#}");
        }
    }

    #[test]
    fn malformed_bare_env_keys_are_rejected_not_skipped() {
        for entry in ["", "TOKEN hunter2"] {
            let err = parse(&["--no-image", "-e", entry, "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
        }
    }

    #[test]
    fn stop_policy() {
        let stop = parse(&[
            "--no-image",
            "--stop-signal",
            "INT",
            "--stop-timeout",
            "3",
            "true",
        ])
        .unwrap()
        .into_invocation()
        .unwrap()
        .stop();
        assert_eq!(stop.signal, "SIGINT".parse().unwrap());
        assert_eq!(stop.timeout, Duration::from_secs(3));
        let default = parse(&["--no-image", "true"])
            .unwrap()
            .into_invocation()
            .unwrap()
            .stop();
        assert_eq!(default, StopPolicy::default());
    }

    /// A request with every flag given.
    fn every_flag() -> RunRequest {
        parse(&[
            "-i",
            "-t",
            "-e",
            "A=1",
            "-w",
            "/work",
            "-u",
            "5:6",
            "--group-add",
            "7",
            "--ulimit",
            "nofile=1:2",
            "--oom-score-adj",
            "3",
            "--stop-signal",
            "SIGINT",
            "--stop-timeout",
            "4",
            "--hostname",
            "box",
            "--domainname",
            "dom",
            "--sysctl",
            "kernel.shmmax=1",
            "--network",
            "app",
            "--name",
            "web",
            "--network-alias",
            "www",
            "--ip",
            "10.0.0.2",
            "--ip6",
            "fd00::2",
            "--mac-address",
            "02:42:0a:00:00:02",
            "--add-host",
            "db:10.0.0.3",
            "--dns",
            "1.1.1.1",
            "--dns-search",
            "example",
            "--dns-option",
            "ndots:2",
            "-p",
            "8080:80",
            "-P",
            "--entrypoint",
            "/e",
            "--pull",
            "never",
            "--read-only",
            "--cap-add",
            "KILL",
            "--cap-drop",
            "CHOWN",
            "--security-opt",
            "no-new-privileges=false",
            "--uidmap",
            "0:1:1",
            "--gidmap",
            "0:2:1",
            "-m",
            "64m",
            "--volume",
            "v:/v",
            "--device",
            "/dev/null",
            "--device-cgroup-rule",
            "c 1:3 r",
            "alpine",
            "cmd",
            "arg",
        ])
        .unwrap()
        .into_request()
        .unwrap()
    }

    /// Every flag reaches the engine's request, as given (`into_request`'s
    /// mapping, field by field): the process and the container's names.
    #[test]
    fn every_flag_reaches_the_request() {
        let request = every_flag();
        assert!(request.streams.interactive && request.streams.tty);
        assert_eq!(request.env, ["A=1"]);
        assert_eq!(
            request.workdir.as_deref(),
            Some(std::path::Path::new("/work"))
        );
        assert_eq!(request.user, Some(("5".into(), Some("6".into()))));
        assert_eq!(request.group_add, ["7"]);
        assert_eq!(request.ulimit.len(), 1);
        assert_eq!(request.oom_score_adj, Some(3));
        assert_eq!(request.stop_signal, Signal::INT);
        assert_eq!(request.stop_timeout, 4);
        assert_eq!(request.hostname.as_deref(), Some("box"));
        assert_eq!(request.domainname.as_deref(), Some("dom"));
        assert_eq!(request.sysctl.len(), 1);
        assert_eq!(request.network, [NetworkMode::bridge("app")]);
        assert_eq!(request.name.as_deref(), Some("web"));
        assert_eq!(request.network_alias, ["www"]);
        assert!(request.ip.is_some() && request.ip6.is_some() && request.mac_address.is_some());
        assert_eq!(request.add_host.len(), 1);
        assert_eq!(request.dns.len(), 1);
    }

    /// The same: the network, the root, security, the rest.
    #[test]
    fn every_other_flag_reaches_the_request() {
        let request = every_flag();
        assert_eq!(request.dns_search, ["example"]);
        assert_eq!(request.dns_option, ["ndots:2"]);
        assert_eq!(request.publish.len(), 1);
        assert!(request.publish_all);
        assert_eq!(request.image.as_deref(), Some("alpine"));
        assert_eq!(
            request.entrypoint.as_deref(),
            Some(std::ffi::OsStr::new("/e"))
        );
        assert_eq!(
            request.pull,
            Some(shipyard_engine::image::PullPolicy::Never)
        );
        assert!(request.read_only);
        assert_eq!(request.cap_add.len(), 1);
        assert_eq!(request.cap_drop.len(), 1);
        assert_eq!(request.security_opt, [SecurityOpt::NoNewPrivileges(false)]);
        assert_eq!(request.uidmap.len(), 1);
        assert_eq!(request.gidmap.len(), 1);
        assert_eq!(request.limits.first_given, Some("--memory"));
        assert!(request.limits.given());
        assert_eq!(request.volume, ["v:/v"]);
        assert_eq!(request.device.len(), 1);
        assert_eq!(request.device_cgroup_rule.len(), 1);
        assert_eq!(request.command, ["cmd", "arg"]);
    }

    /// A root filesystem, a bundle and no isolation reach it too.
    #[test]
    fn the_root_bundle_and_isolation_reach_the_request() {
        let rootfs = parse(&["--rootfs", "/r", "--no-isolation", "true"]);
        assert!(rootfs.is_err(), "clap's: they can't go together");
        let rootfs = request(&["--rootfs", "/r", "true"]).unwrap();
        assert_eq!(rootfs.rootfs.as_deref(), Some(std::path::Path::new("/r")));
        let plain = parse(&["--no-isolation", "--bundle", "/b"]);
        assert!(plain.is_err());
        let bundle = request(&["--bundle", "/b"]).unwrap();
        assert_eq!(bundle.bundle.as_deref(), Some(std::path::Path::new("/b")));
        assert!(request(&["--no-isolation", "true"]).unwrap().no_isolation);
    }

    /// Docker's spellings of capabilities and security options, as the
    /// engine takes them.
    #[test]
    fn capability_and_security_spellings() {
        use shipyard_runtime::security::Capability;
        let cap = |name: &str| name.parse::<Capability>().unwrap();
        assert_eq!(parse_capability("ALL"), Ok(CapabilityArg::All));
        assert_eq!(parse_capability("all"), Ok(CapabilityArg::All));
        assert_eq!(
            parse_capability("chown"),
            Ok(CapabilityArg::One(cap("CHOWN")))
        );
        assert_eq!(
            parse_capability("CAP_SYS_ADMIN"),
            Ok(CapabilityArg::One(cap("SYS_ADMIN")))
        );
        for (option, on) in [
            ("no-new-privileges", true),
            ("no-new-privileges=true", true),
            ("no-new-privileges:false", false),
            ("no-new-privileges=false", false),
        ] {
            assert_eq!(
                parse_security_opt(option),
                Ok(SecurityOpt::NoNewPrivileges(on)),
                "{option}"
            );
        }
        assert_eq!(
            parse_security_opt("seccomp=/p.json"),
            Ok(SecurityOpt::SeccompProfile("/p.json".into()))
        );
    }

    #[test]
    fn seccomp_options_are_checked() {
        assert!(parse(&["--no-image", "--security-opt", "seccomp=", "true"]).is_err());
        assert!(parse(&["--no-image", "--security-opt", "seccomp", "true"]).is_err());
        assert!(
            parse(&[
                "--no-isolation",
                "--security-opt",
                "seccomp=unconfined",
                "true"
            ])
            .is_err()
        );
        assert!(parse(&["--bundle", ".", "--security-opt", "seccomp=unconfined"]).is_err());
        assert_eq!(
            parse(&["--no-image", "--security-opt", "seccomp:unconfined", "true"])
                .unwrap()
                .security_opt,
            [SecurityOpt::SeccompUnconfined]
        );
    }

    #[test]
    fn id_map_flags_parse_and_conflict() {
        assert!(
            parse(&["--no-image", "--gidmap", "0:1:1", "true"]).is_err(),
            "--gidmap needs --uidmap"
        );
        assert!(parse(&["--no-image", "--uidmap", "0:1", "true"]).is_err());
        assert!(parse(&["--no-isolation", "--uidmap", "0:1:1", "true"]).is_err());
        assert!(parse(&["--bundle", ".", "--uidmap", "0:1:1"]).is_err());
        if !cfg!(target_os = "linux") {
            let err = parse(&["--no-image", "--uidmap", "0:100000:65536", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("--uidmap needs Linux"), "{err}");
        }
    }

    #[test]
    fn the_oom_score_adjustment_is_checked() {
        if !cfg!(target_os = "linux") {
            let err = parse(&["--no-image", "--oom-score-adj", "5", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("needs Linux"), "{err}");
            return;
        }
        assert_eq!(
            spec(&["--no-image", "--oom-score-adj", "-500", "true"]).oom_score_adj,
            Some(-500)
        );
        let err = parse(&["--no-image", "--oom-score-adj", "1001", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("between -1000 and 1000"), "{err}");
        assert!(parse(&["--bundle", ".", "--oom-score-adj", "5"]).is_err());
    }

    #[test]
    fn limit_flags_need_a_container() {
        for limit in [
            &["--memory", "64m"][..],
            &["--pids-limit", "5"],
            &["--cpus", "1"],
        ] {
            let args = [&["--no-isolation"], limit, &["true"]].concat();
            assert!(parse(&args).is_err(), "{args:?}");
            let args = [&["--bundle", "."], limit].concat();
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let limited = parse(&["--no-image", "-m", "64m", "-c", "512", "true"]).unwrap();
        assert_eq!(limited.args, ["true"], "-m and -c are ours");
        if !cfg!(target_os = "linux") {
            let err = limited.into_invocation().unwrap_err();
            assert!(
                err.to_string().contains("--memory needs Linux cgroups"),
                "{err}"
            );
        }
    }

    #[test]
    fn capability_flags_are_checked() {
        assert!(parse(&["--no-image", "--cap-add", "FLY", "true"]).is_err());
        assert!(parse(&["--no-image", "--security-opt", "apparmor=x", "true"]).is_err());
        assert!(
            parse(&[
                "--no-image",
                "--security-opt",
                "no-new-privileges=maybe",
                "true"
            ])
            .is_err()
        );
        for flag in ["--cap-add", "--cap-drop"] {
            assert!(
                parse(&["--no-isolation", flag, "KILL", "true"]).is_err(),
                "{flag}"
            );
            assert!(parse(&["--bundle", ".", flag, "KILL"]).is_err(), "{flag}");
        }
        if !cfg!(target_os = "linux") {
            let err = parse(&["--no-image", "--cap-add", "KILL", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("--cap-add needs Linux"), "{err}");
        }
    }

    #[test]
    fn network_modes() {
        assert_eq!(parse_network("host"), Ok(NetworkMode::Host));
        assert_eq!(parse_network("none"), Ok(NetworkMode::None));
        assert_eq!(parse_network("bridge"), Ok(NetworkMode::bridge("bridge")));
        assert_eq!(parse_network("app"), Ok(NetworkMode::bridge("app")));
        assert!(parse_network("a/b").is_err());
        assert!(parse(&["--network", "none", "--no-isolation", "true"]).is_err());
        assert!(parse(&["--no-image", "--network", "-x", "true"]).is_err());
        // Addresses need a network to be on; /etc files a root to be in.
        assert!(parse(&["--no-image", "--ip", "10.0.0.2", "true"]).is_err());
        assert!(parse(&["--no-image", "--mac-address", "02:42:0a:00:00:02", "true"]).is_err());
        assert!(
            parse(&[
                "--no-image",
                "--mac-address",
                "00:42:0a:00:00:02",
                "--network",
                "bridge",
                "true"
            ])
            .is_err()
        );
        assert!(parse(&["--no-image", "--add-host", "db", "true"]).is_err());
        let isolation = |args: &[&str]| {
            parse(args)
                .unwrap()
                .into_invocation()
                .map(|run| run.isolation().cloned())
        };
        if cfg!(target_os = "linux") {
            let own = isolation(&["--no-image", "--network", "none", "true"])
                .unwrap()
                .unwrap();
            assert!(own.makes_network_namespace());
            let host = isolation(&["--no-image", "--network", "host", "true"])
                .unwrap()
                .unwrap();
            assert!(!host.has(Namespace::Network));
            // The default: a network of its own (the bridge network for the
            // host's root, pasta without), except for a root that can't
            // make one. (Without pasta installed, a rootless run is refused.)
            if let Ok(Some(default)) = isolation(&["--no-image", "true"]) {
                assert_eq!(
                    default.has(Namespace::Network),
                    shipyard_engine::network::can_network()
                        || !shipyard_engine::network::host_root()
                );
            }
            // A network setting needs a network of its own, and the error
            // says how to give it one.
            let error = isolation(&[
                "--no-image",
                "--network",
                "host",
                "--sysctl",
                "net.ipv4.ip_forward=1",
                "true",
            ])
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("any --network but host gives it one"),
                "{error}"
            );
            // Others reach the container whatever its network.
            let ipc = isolation(&[
                "--no-image",
                "--network",
                "host",
                "--sysctl",
                "kernel.shmmax=1",
                "true",
            ])
            .unwrap()
            .unwrap();
            assert_eq!(ipc.sysctls, [("kernel.shmmax".to_owned(), "1".to_owned())]);
            let set = isolation(&[
                "--no-image",
                "--network",
                "none",
                "--sysctl",
                "net.ipv4.ip_forward=1",
                "true",
            ])
            .unwrap()
            .unwrap();
            assert!(
                set.sysctls
                    .contains(&("net.ipv4.ip_forward".into(), "1".into()))
            );
        } else {
            let error = isolation(&["--no-image", "--network", "none", "true"]).unwrap_err();
            assert!(
                error.to_string().contains("--network needs Linux"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_long_form_gives_a_network_its_own_settings() {
        let parsed =
            parse_network("name=app,ip=10.89.0.5,mac-address=02:42:0a:59:00:05,alias=web,alias=x")
                .unwrap();
        assert_eq!(
            parsed,
            NetworkMode::Bridges(vec![Joined {
                name: "app".into(),
                ip: Some("10.89.0.5".parse().unwrap()),
                ip6: None,
                mac: Some([2, 0x42, 10, 0x59, 0, 5]),
                aliases: vec!["web".into(), "x".into()],
            }])
        );
        for bad in [
            "ip=10.89.0.5",
            "name=host",
            "name=app,ip=nope",
            "name=app,ip=10.0.0.1,ip=10.0.0.2",
            "name=app,name=db",
            "name=app,gw-priority=1",
            "name=app,alias=a.b",
            "name=app,mac-address=01:00:00:00:00:00",
            "name=app,ip",
        ] {
            assert!(parse_network(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_are_dns_labels() {
        assert!(parse(&["--no-image", "--name", "db.app", "true"]).is_err());
        assert!(parse(&["--name", "db", "--no-isolation", "true"]).is_err());
        // An alias needs the network named.
        assert!(parse(&["--no-image", "--network-alias", "db", "true"]).is_err());
    }

    #[test]
    fn isolation_is_the_default_on_linux_only() {
        let isolation = |args: &[&str]| {
            parse(args)
                .unwrap()
                .into_invocation()
                .unwrap()
                .isolation()
                .cloned()
        };
        assert_eq!(
            isolation(&["--no-image", "true"]).is_some(),
            cfg!(target_os = "linux"),
            "isolated by default exactly on Linux"
        );
        assert_eq!(isolation(&["--no-isolation", "true"]), None);
        if cfg!(target_os = "linux") {
            let named = isolation(&["--no-image", "--hostname", "box", "true"]).unwrap();
            assert_eq!(named.hostname.as_deref(), Some("box"));
        }
    }

    #[test]
    fn a_root_filesystem_is_canonical_and_checked() {
        let isolation = |args: &[&str]| parse(args).unwrap().into_invocation();
        // Contradictions, whatever the system.
        assert!(parse(&["--no-isolation", "--rootfs", "/", "true"]).is_err());
        assert!(
            request(&["--no-image", "--read-only", "true"]).is_err(),
            "needs an image or --rootfs"
        );
        assert!(request(&["--read-only", "alpine", "true"]).is_ok());
        let missing = isolation(&["--rootfs", "/no/such/dir", "true"]).unwrap_err();
        assert!(format!("{missing:#}").contains("--rootfs"), "{missing:#}");
        if cfg!(target_os = "linux") {
            assert!(
                format!("{missing:#}").contains("/no/such/dir"),
                "{missing:#}"
            );
            let dir = std::env::temp_dir();
            let given = dir.join(".").join("");
            let invocation =
                isolation(&["--rootfs", given.to_str().unwrap(), "--read-only", "true"]).unwrap();
            let root = invocation.isolation().cloned().unwrap().root.unwrap();
            assert_eq!(root.path, std::fs::canonicalize(dir).unwrap());
            assert!(root.read_only);
        }
    }

    #[test]
    fn a_bundle_is_the_whole_description() {
        assert!(parse(&["--bundle", "/b"]).is_ok(), "no COMMAND needed");
        assert!(request(&["--bundle", "/b", "true"]).is_err(), "no COMMAND");
        for conflicting in [
            &["--bundle", "/b", "-e", "A=1"][..],
            &["--bundle", "/b", "-w", "/tmp"],
            &["--bundle", "/b", "--rootfs", "/r"],
            &["--bundle", "/b", "--no-isolation"],
        ] {
            assert!(parse(conflicting).is_err(), "{conflicting:?}");
        }
        let missing = parse(&["--bundle", "/no/such/bundle"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        let expected = if cfg!(target_os = "linux") {
            "/no/such/bundle/config.json"
        } else {
            "--bundle"
        };
        assert!(format!("{missing:#}").contains(expected), "{missing:#}");
    }

    #[test]
    fn hostname_problems_are_usage_errors() {
        // Both: together they contradict each other.
        assert!(parse(&["--no-isolation", "--hostname", "box", "true"]).is_err());
        // An invalid name, or (macOS) no namespaces to give it to.
        let err = parse(&["--no-image", "--hostname", "bad_name", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(format!("{err:#}").contains("hostname"), "{err:#}");
    }
}
