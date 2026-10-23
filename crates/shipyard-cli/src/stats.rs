//! `shipyard top` and `shipyard stats` (step 11.3d2): what a container
//! runs and what it uses, as Docker's `top` and `stats` print them.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal as _, Write as _};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use shipyard_engine::docker::{self, ContainerStats};

use crate::format::Output;
use crate::format::rows::{STATS_TABLE, StatsRow};
use crate::output::printable;
use crate::reporter::{EXIT_FAILURE, report};

/// How long between two samples: the first row's CPU share is over it.
const INTERVAL: Duration = Duration::from_secs(1);

/// `shipyard top`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct TopArgs {
    /// The container: its name, ID, serial or ID prefix.
    pub(crate) container: String,
    /// Not supported: `ps`'s options (Shipyard reads /proc itself, in
    /// `ps -ef`'s columns; any but `-ef` refused).
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "PS OPTIONS"
    )]
    pub(crate) ps_options: Vec<String>,
}

/// `shipyard top`: the container's processes in `ps -ef`'s columns, as
/// Docker's `top` prints them.
pub(crate) fn top(args: &TopArgs) -> Result<ExitCode> {
    // `-ef` is what's shown anyway (Docker's daemon's default).
    if !args.ps_options.is_empty() && args.ps_options != ["-ef"] {
        bail!("ps options aren't supported: `top` shows `ps -ef`'s columns, read from /proc");
    }
    let engine = crate::reporter::engine();
    let top = docker::top(&engine, &args.container)?;
    let mut lines = String::new();
    for row in std::iter::once(&top.titles).chain(&top.processes) {
        let cells: Vec<_> = row.iter().map(|cell| printable(cell)).collect();
        lines.push_str(&cells.join("\t"));
        lines.push('\n');
    }
    // Docker's `tabwriter.NewWriter(out, 20, 1, 3, ' ', 0)`: a column is
    // `max(20, widest + 3)` wide; this port pads after its minimum.
    let mut table = tabwriter::TabWriter::new(Vec::new())
        .minwidth(17)
        .padding(3);
    table.write_all(lines.as_bytes())?;
    std::io::stdout().lock().write_all(&table.into_inner()?)?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard stats`' arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct StatsArgs {
    /// All containers, not only the ones up (without CONTAINER).
    #[arg(short, long)]
    pub(crate) all: bool,
    /// How to print them: `table` (the default), `json` (an object per
    /// line), `table TEMPLATE`, or a Go TEMPLATE (`{{.Name}} {{.CPUPerc}}`).
    #[arg(long, value_name = "FORMAT")]
    pub(crate) format: Option<String>,
    /// Print once and end (without: every second, until Ctrl-C).
    #[arg(long)]
    pub(crate) no_stream: bool,
    /// Don't cut IDs short.
    #[arg(long)]
    pub(crate) no_trunc: bool,
    /// The containers (without: those running, listed again each time).
    #[arg(value_name = "CONTAINER")]
    pub(crate) containers: Vec<String>,
}

/// `shipyard stats`: what each container uses, from two samples a second
/// apart, as Docker's `stats` prints it; with `--no-stream` once, else
/// every second (the screen cleared first on a terminal). A container
/// named that can't be read is said and left out; exits 1 if any was, at
/// once if none is left. Without names, a listing that fails ends it.
pub(crate) fn stats(args: &StatsArgs) -> Result<ExitCode> {
    let output = Output::parse(args.format.as_deref(), STATS_TABLE)?;
    let engine = crate::reporter::engine();
    let terminal = std::io::stdout().is_terminal();
    let mut sampler = Sampler::default();
    // The first sample is the one the first row's CPU share is counted from.
    sampler.sample(&engine, args)?;
    while !sampler.none_left(args) {
        std::thread::sleep(INTERVAL);
        let rows = sampler.sample(&engine, args)?;
        let mut out = std::io::stdout().lock();
        if terminal && !args.no_stream {
            // Docker's: the screen cleared, the cursor home.
            write!(out, "\x1b[2J\x1b[H")?;
        }
        output.write(&mut out, &rows)?;
        out.flush()?;
        drop(out);
        if args.no_stream {
            break;
        }
    }
    Ok(if sampler.failed {
        ExitCode::from(EXIT_FAILURE)
    } else {
        ExitCode::SUCCESS
    })
}

/// Each container's last sample, the next one's `precpu_stats`.
#[derive(Default)]
struct Sampler {
    /// By the reference it was read through.
    last: HashMap<String, ContainerStats>,
    /// A container couldn't be read (each said once).
    failed: bool,
    /// The containers that couldn't be, as named or listed: not read again
    /// (named), or not said again (listed).
    gone: HashSet<String>,
}

impl Sampler {
    /// A row for each container `args` asks for, as it is now.
    ///
    /// # Errors
    ///
    /// Without names, the containers can't be listed.
    fn sample(
        &mut self,
        engine: &shipyard_engine::Engine,
        args: &StatsArgs,
    ) -> Result<Vec<StatsRow>> {
        let named = !args.containers.is_empty();
        let wanted: Vec<String> = if named {
            args.containers
                .iter()
                .filter(|name| !self.gone.contains(*name))
                .cloned()
                .collect()
        } else {
            docker::list_containers(engine, args.all)?
                .into_iter()
                .map(|summary| summary.id)
                .collect()
        };
        let mut rows = Vec::new();
        let mut last = HashMap::new();
        for reference in wanted {
            match docker::stats(engine, &reference, self.last.get(&reference)) {
                Ok(sample) => {
                    rows.push(StatsRow::new(&sample, &reference, args.no_trunc));
                    last.insert(reference, sample);
                }
                // One listed that's gone since: left out.
                Err(err) if !named && err.kind() == shipyard_engine::ErrorKind::NotFound => {}
                Err(err) => {
                    if self.gone.insert(reference) {
                        self.failed = true;
                        report(&anyhow::Error::from(err), EXIT_FAILURE);
                    }
                }
            }
        }
        self.last = last;
        Ok(rows)
    }

    /// Whether every container named has failed: nothing left to show.
    fn none_left(&self, args: &StatsArgs) -> bool {
        !args.containers.is_empty() && args.containers.iter().all(|name| self.gone.contains(name))
    }
}
