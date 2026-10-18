//! `shipyard`'s flags: the commands, as clap reads them (a group's own
//! arguments live beside its commands: `run::RunArgs`, `containers::StopArgs`…).
//! Their doc comments are the help text (and `docs/user/cli.md`).

use clap::{Parser, Subcommand, ValueEnum};

use crate::{containers, events_output, run};

/// A lightweight container engine.
#[derive(Debug, Parser)]
#[command(name = "shipyard", version, propagate_version = true)]
pub(crate) struct Cli {
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
        #[command(flatten)]
        refs: containers::ContainerRefs,
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
        #[command(flatten)]
        refs: containers::ContainerRefs,
    },
    /// Stop containers (as `stop`), then start them again.
    Restart {
        #[command(flatten)]
        how: containers::StopArgs,
        #[command(flatten)]
        refs: containers::ContainerRefs,
    },
    /// Change containers' settings without making them again: their restart
    /// policy.
    Update {
        /// The restart policy: no, on-failure (or on-failure:N), always or
        /// unless-stopped.
        #[arg(long, value_name = "POLICY", required = true)]
        restart: shipyard_engine::containers::RestartPolicy,
        #[command(flatten)]
        refs: containers::ContainerRefs,
    },
    /// Send a signal to running containers' main process (SIGKILL unless
    /// told), without waiting.
    Kill {
        /// The signal: a name (KILL, SIGTERM) or a number.
        #[arg(short, long, value_name = "SIGNAL", default_value = "SIGKILL")]
        signal: shipyard_runtime::process::Signal,
        #[command(flatten)]
        refs: containers::ContainerRefs,
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
        #[command(flatten)]
        refs: containers::ContainerRefs,
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
        #[command(flatten)]
        refs: containers::ContainerRefs,
    },
    /// Remove containers (their writable layers too). A running one needs -f.
    Rm {
        /// Remove a running container too (it's killed first).
        #[arg(short, long)]
        force: bool,
        #[command(flatten)]
        refs: containers::ContainerRefs,
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
    /// The Linux VM containers run in on a Mac: its status, start and stop.
    #[command(subcommand)]
    Vm(VmCommand),
    /// Check whether this host can run containers, and what to fix.
    Doctor,
}

/// `shipyard volume …`.
#[derive(Debug, Subcommand)]
pub(crate) enum VolumeCommand {
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
pub(crate) enum NetworkCommand {
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
pub(crate) enum ImageCommand {
    /// List images in the local store.
    Ls,
    /// Remove unreferenced images, layers and blobs from the store.
    Prune,
}

/// `shipyard system …`.
#[derive(Debug, Subcommand)]
pub(crate) enum SystemCommand {
    /// Show the store's disk usage.
    Df,
    /// Settle containers that ended unwatched, then start those whose
    /// restart policy brings them back when the host starts (what the boot
    /// unit runs: see contrib/systemd).
    Recover,
    /// Stop every container that's up, for the host's shutdown, each with its
    /// own stop signal and timeout: not as your stop (their restart policy
    /// brings them back at boot), and none restarted until `system recover`
    /// (what shipyard-stop.service runs at a shutdown).
    Stop,
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
