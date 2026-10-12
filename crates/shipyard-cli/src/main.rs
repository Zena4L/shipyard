//! The `shipyard` command-line interface.
//!
//! This crate only parses flags and reports results; the work happens in
//! `shipyard-runtime`.

mod devices;
mod dns_helper;
mod doctor;
mod image;
#[allow(
    dead_code,
    reason = "shared with `shipyard-runtime`, whose `update` uses what `run` doesn't"
)]
mod limits;
mod network;
mod run;
mod storage_helper;
mod volumes;

use std::io::{BufRead, IsTerminal as _, Write};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
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
    /// Run a command in a container: on Linux, isolated in namespaces, optionally with its own root filesystem.
    Run(Box<run::RunArgs>),
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
        /// Untag an image even while a running container uses it (its layers
        /// stay until the container ends and a prune collects them).
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
    /// Remove every named volume no running container uses (asks first).
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
        /// only.
        #[arg(long)]
        internal: bool,
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
}

fn main() -> ExitCode {
    // First of all: a container's init re-execs this binary to become a
    // clean process, and takes over here (it never returns then).
    process::init_entry_point();
    // The storage helper (a rootless caller's layer work, in a user
    // namespace of its own) is this binary too; making the namespace needs a
    // single-threaded process, so before anything else as well.
    storage_helper::entry_point();
    // A network's DNS server is this binary too, started by a run.
    dns_helper::entry_point();
    // So is `doctor`'s probe (it makes a user namespace: single-threaded).
    doctor::probe_entry_point();
    // And pasta's launcher (a rootless run's network: it joins the
    // container's user namespace, single-threaded too).
    shipyard_network::pasta::entry_point(|| {
        process::refuse_privileged_start().map_err(|error| error.to_string())
    });
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
        Command::Run(args) => run::run(*args),
        Command::Pull { quiet, reference } => pull(&reference, quiet),
        Command::Images | Command::Image(ImageCommand::Ls) => images(),
        Command::Rmi { force, images } => Ok(rmi(&images, force)),
        Command::Image(ImageCommand::Prune) => image_prune(),
        Command::Volume(command) => volume_command(&command),
        Command::Network(command) => network_command(&command),
        Command::System(SystemCommand::Df) => system_df(),
        Command::Doctor => doctor::run(),
    };
    result.unwrap_or_else(|err| fail(&err))
}

/// Reports an error from shipyard itself and returns exit code 125.
fn fail(err: &anyhow::Error) -> ExitCode {
    report(err, EXIT_ENGINE_ERROR)
}

/// `shipyard images`: lists the images in the store.
fn images() -> Result<ExitCode> {
    let mut images = image::list_images()?;
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
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
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
    for row in rows {
        line(out, &mut row.iter().map(String::as_str))?;
    }
    Ok(())
}

/// `shipyard pull IMAGE`: pulls it, then prints its digest.
fn pull(reference: &str, quiet: bool) -> Result<ExitCode> {
    let digest = image::pull_command(reference, quiet)?;
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
        let removed = image::remove_image(reference, force)?;
        let mut out = std::io::stdout().lock();
        writeln!(out, "Untagged: {reference}")?;
        for digest in removed {
            writeln!(out, "Deleted: {digest}")?;
        }
        Ok(())
    })
}

/// Does `act` for each of `names`, reporting each failure and going on;
/// exits 125 if any failed.
fn each(names: &[String], mut act: impl FnMut(&str) -> Result<()>) -> ExitCode {
    let mut code = ExitCode::SUCCESS;
    for name in names {
        if let Err(err) = act(name) {
            code = report(&err, EXIT_ENGINE_ERROR);
        }
    }
    code
}

/// `shipyard image prune`: removes unreferenced blobs and layers.
fn image_prune() -> Result<ExitCode> {
    let removed = image::prune_images()?;
    let mut out = std::io::stdout().lock();
    for digest in &removed {
        writeln!(out, "Deleted: {digest}")?;
    }
    writeln!(out, "Removed {} unreferenced blob(s)", removed.len())?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard system df`: shows what the images, the running containers'
/// writable layers and the volumes take, and how much of it nothing uses.
fn system_df() -> Result<ExitCode> {
    let store = image::disk_usage()?;
    let disk = &store.disk;
    let images = image::Usage {
        count: store.images,
        bytes: disk.blob_bytes + disk.layer_bytes,
        reclaimable: disk.blob_reclaimable + disk.layer_reclaimable,
        complete: disk.complete,
    };
    let rows: Vec<Vec<String>> = [
        ("Images", &images),
        ("Containers", &store.containers),
        ("Volumes", &volumes::usage()?),
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
    match command {
        NetworkCommand::Create {
            subnet,
            gateway,
            ipv6,
            internal,
            name,
        } => {
            let config = network::create(&network::spec(name, subnet, gateway, *ipv6, *internal)?)?;
            writeln!(std::io::stdout().lock(), "{}", config.name)?;
        }
        NetworkCommand::Ls => {
            let rows: Vec<Vec<String>> = network::list(&[])?
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
            let networks: Vec<serde_json::Value> = network::list(names)?
                .into_iter()
                .map(|listed| {
                    let containers: Vec<serde_json::Value> = listed
                        .leases
                        .iter()
                        .map(|lease| {
                            serde_json::json!({
                                "Run": lease.run,
                                "Name": lease.name,
                                "Aliases": lease.aliases,
                                "Hostname": lease.hostname,
                                "Interface": lease.interface,
                                "IPAddress": lease.ip,
                                "IPv6Address": lease.ip6,
                                "MacAddress": shipyard_network::store::format_mac(lease.mac),
                                "HostVeth": lease.host_veth,
                                "Ports": lease.ports,
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "Name": listed.config.name,
                        "Subnet": listed.config.subnet,
                        "Gateway": listed.config.gateway,
                        "Subnet6": listed.config.subnet6,
                        "Gateway6": listed.config.gateway6,
                        "Bridge": listed.config.bridge,
                        "Internal": listed.config.internal,
                        "Created": listed.config.created,
                        "DNSRunning": listed.dns_running,
                        "Containers": containers,
                    })
                })
                .collect();
            let mut out = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut out, &networks)?;
            writeln!(out)?;
        }
        NetworkCommand::Rm { names } => {
            return Ok(each(names, |name| {
                network::remove(name)?;
                writeln!(std::io::stdout().lock(), "{name}")?;
                Ok(())
            }));
        }
        NetworkCommand::Prune { force } => return network_prune(*force),
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
    let pruned = network::prune()?;
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
    match command {
        VolumeCommand::Create { name } => {
            let volume = volumes::create(name)?;
            writeln!(std::io::stdout().lock(), "{}", volume.name)?;
        }
        VolumeCommand::Ls => {
            let rows: Vec<Vec<String>> = volumes::list()?
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
            let volume = volumes::inspect(name)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{:<12} {}", "Name:", volume.name)?;
            writeln!(out, "{:<12} {}", "Mountpoint:", volume.data.display())?;
        }
        VolumeCommand::Rm { names } => {
            return Ok(each(names, |name| {
                volumes::remove(name)?;
                writeln!(std::io::stdout().lock(), "{name}")?;
                Ok(())
            }));
        }
        VolumeCommand::Prune { force } => return volume_prune(*force),
    }
    Ok(ExitCode::SUCCESS)
}

/// `shipyard volume prune`: asks first (on a terminal; elsewhere `-f` is
/// needed), then removes every volume no running container uses.
fn volume_prune(force: bool) -> Result<ExitCode> {
    const QUESTION: &str = "Remove every volume no running container uses, and its data?";
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
    let pruned = volumes::prune()?;
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

/// Prints a warning: something asked for that Shipyard goes on without.
pub(crate) fn warning(text: &str) {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(std::io::stderr().lock(), "shipyard: warning: {text}");
}

/// Prints something the user should know that isn't a problem (a port the
/// kernel chose).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
pub(crate) fn notice(text: &str) {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(std::io::stderr().lock(), "shipyard: {text}");
}

/// Prints a hint after an error: what the user can do about it.
fn hint(text: &str) {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(std::io::stderr().lock(), "shipyard: hint: {text}");
}

/// Prints an error with its full cause chain ("doing X: because Y") and
/// returns `code` as the exit code.
fn report(err: &dyn std::fmt::Display, code: u8) -> ExitCode {
    // Not `eprintln!`: it panics if stderr is a closed pipe.
    let _ = writeln!(std::io::stderr().lock(), "shipyard: {err:#}");
    ExitCode::from(code)
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
/// (`shipyard: warning: …`, as [`warning`] prints them), whichever part of
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
            clap_markdown::help_markdown_custom::<Cli>(&options)
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
        let cli = Cli::try_parse_from(["shipyard", "run", "-D", "ls", "-l"]).unwrap();
        assert!(cli.debug, "-D before the command is shipyard's flag");
        assert!(matches!(cli.command, Command::Run(_)));
    }
}
