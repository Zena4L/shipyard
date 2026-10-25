//! `shipyard build` and `builder prune` (step 12.3): the flags, as Docker's,
//! read into the engine's `BuildRequest`; what it built said.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use shipyard_engine::build::{BuildRequest, Dockerfile, Output, Secret};

use crate::reporter::{CliReporter, engine_with};

/// `build`'s flags.
#[derive(Debug, clap::Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap's switches: each bool is one command-line flag"
)]
pub(crate) struct BuildArgs {
    /// Name the image `NAME[:TAG]` (repeat it for more names): required
    /// unless -o writes its files (Shipyard keeps no untagged images).
    #[arg(short, long = "tag", value_name = "NAME[:TAG]")]
    pub(crate) tags: Vec<String>,
    /// The Dockerfile (default: CONTEXT/Dockerfile; - reads it from
    /// standard input).
    #[arg(short, long, value_name = "FILE")]
    pub(crate) file: Option<PathBuf>,
    /// A build arg: KEY=VALUE, or KEY for this shell's value (repeat it).
    #[arg(long = "build-arg", value_name = "KEY[=VALUE]")]
    pub(crate) args: Vec<String>,
    /// The stage to build.
    #[arg(long, value_name = "STAGE")]
    pub(crate) target: Option<String>,
    /// Use no cache.
    #[arg(long)]
    pub(crate) no_cache: bool,
    /// Fetch the base images again.
    #[arg(long)]
    pub(crate) pull: bool,
    /// Print only the image's ID (no progress).
    #[arg(short, long)]
    pub(crate) quiet: bool,
    /// A label of the image: KEY=VALUE (repeat it).
    #[arg(long, value_name = "KEY=VALUE")]
    pub(crate) label: Vec<String>,
    /// How progress is shown: auto, plain (BuildKit's lines, as auto) or
    /// quiet.
    #[arg(long, value_name = "TYPE", default_value = "auto")]
    pub(crate) progress: String,
    /// The platform: this host's only.
    #[arg(long, value_name = "PLATFORM")]
    pub(crate) platform: Option<String>,
    /// A secret for `RUN --mount=type=secret`: id=ID,src=FILE (repeat it).
    #[arg(long, value_name = "id=ID,src=FILE")]
    pub(crate) secret: Vec<String>,
    /// Write the build's files instead of an image: type=local,dest=DIR
    /// (or DIR), or type=tar,dest=FILE (- for standard output).
    #[arg(short, long, value_name = "OUTPUT")]
    pub(crate) output: Option<String>,
    /// The context: a directory.
    #[arg(value_name = "CONTEXT")]
    pub(crate) context: PathBuf,
}

/// Where `-o` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutputTo {
    /// A directory.
    Local(PathBuf),
    /// A tar archive: a file, or standard output (`None`).
    Tar(Option<PathBuf>),
}

/// The refusal of `-f -` with a terminal for its standard input.
pub(crate) const DOCKERFILE_FROM_TERMINAL: &str =
    "-f - reads the Dockerfile from standard input: redirect one in";

impl BuildArgs {
    /// `-o` as read: `type=local,dest=DIR` or `DIR`, `type=tar,dest=FILE`
    /// or `-`.
    pub(crate) fn output_to(&self) -> Result<Option<OutputTo>> {
        let Some(given) = &self.output else {
            return Ok(None);
        };
        if given == "-" {
            return Ok(Some(OutputTo::Tar(None)));
        }
        if !given.contains('=') {
            return Ok(Some(OutputTo::Local(PathBuf::from(given))));
        }
        let mut kind = None;
        let mut dest = None;
        for field in given.split(',') {
            match field.split_once('=') {
                Some(("type", value)) => kind = Some(value),
                Some(("dest", value)) => dest = Some(value),
                _ => bail!("-o takes type=local,dest=DIR or type=tar,dest=FILE: not {given:?}"),
            }
        }
        match (kind, dest) {
            (Some("local"), Some(dest)) => Ok(Some(OutputTo::Local(PathBuf::from(dest)))),
            (Some("tar"), Some("-") | None) => Ok(Some(OutputTo::Tar(None))),
            (Some("tar"), Some(dest)) => Ok(Some(OutputTo::Tar(Some(PathBuf::from(dest))))),
            _ => bail!("-o takes type=local,dest=DIR or type=tar,dest=FILE: not {given:?}"),
        }
    }

    /// Whether it reads standard input (`-f -`).
    pub(crate) fn reads_stdin(&self) -> bool {
        self.file
            .as_deref()
            .is_some_and(|file| file.as_os_str() == "-")
    }

    /// The secrets' files (`src=`), each a host path.
    pub(crate) fn secret_files(&self) -> Vec<PathBuf> {
        self.secret
            .iter()
            .filter_map(|given| secret(given).ok())
            .map(|secret| secret.src)
            .collect()
    }

    /// The engine's request.
    fn request(&self) -> Result<BuildRequest> {
        let context = self.context.to_str().unwrap_or_default();
        if context == "-" || context.contains("://") || context.starts_with("git@") {
            bail!(
                "a build's context is a directory here (from standard input or a URL isn't \
                 supported)"
            );
        }
        let mut request = BuildRequest::new(&self.context);
        request.dockerfile = match &self.file {
            None => Dockerfile::Default,
            Some(file) if file.as_os_str() == "-" => {
                if std::io::stdin().is_terminal() {
                    bail!(DOCKERFILE_FROM_TERMINAL);
                }
                let mut text = Vec::new();
                std::io::stdin()
                    .take(shipyard_engine::build::MAX_FILE + 1)
                    .read_to_end(&mut text)
                    .context("reading the Dockerfile from standard input")?;
                Dockerfile::Text(text)
            }
            Some(file) => Dockerfile::File(file.clone()),
        };
        request.tags.clone_from(&self.tags);
        for given in &self.args {
            request.build_args.push(match given.split_once('=') {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                // Docker's: the shell's value; none, left out.
                None => match std::env::var(given) {
                    Ok(value) => (given.clone(), value),
                    Err(_) => continue,
                },
            });
        }
        for given in &self.label {
            let Some((key, value)) = given.split_once('=') else {
                bail!("a label is KEY=VALUE: not {given:?}");
            };
            request.labels.push((key.to_owned(), value.to_owned()));
        }
        request.target.clone_from(&self.target);
        request.no_cache = self.no_cache;
        request.pull = self.pull;
        request.platform.clone_from(&self.platform);
        request.secrets = self
            .secret
            .iter()
            .map(|given| secret(given))
            .collect::<Result<_>>()?;
        request.output = self.output_to()?.map(|to| match to {
            OutputTo::Local(dir) => Output::Local(dir),
            OutputTo::Tar(_) => Output::Tar,
        });
        Ok(request)
    }
}

/// A `--secret`: `id=ID,src=FILE` (`source=` and `type=file` taken, as
/// Docker's).
fn secret(given: &str) -> Result<Secret> {
    let mut id = None;
    let mut src = None;
    for field in given.split(',') {
        match field.split_once('=') {
            Some(("id", value)) => id = Some(value.to_owned()),
            Some(("src" | "source", value)) => src = Some(PathBuf::from(value)),
            Some(("type", "file")) => {}
            _ => bail!("--secret takes id=ID,src=FILE: not {given:?}"),
        }
    }
    match (id, src) {
        (Some(id), Some(src)) => Ok(Secret { id, src }),
        _ => bail!("--secret takes id=ID,src=FILE: not {given:?}"),
    }
}

/// `shipyard build`: BuildKit's progress on stderr (unless `-q`); the
/// image loaded under its names (`-q`: its ID printed), or its files
/// written (`-o`).
pub(crate) fn build(args: &BuildArgs) -> Result<ExitCode> {
    let quiet = args.quiet || args.progress == "quiet";
    if !matches!(args.progress.as_str(), "auto" | "plain" | "tty" | "quiet") {
        bail!("--progress takes auto, plain or quiet");
    }
    let request = args.request()?;
    let engine = engine_with(CliReporter { progress: !quiet });
    let built = match args.output_to()? {
        Some(OutputTo::Tar(Some(path))) => {
            let mut built = None;
            crate::output::write_archive_to(&path, |out| {
                built = Some(shipyard_engine::build::build(&engine, &request, Some(out))?);
                Ok(())
            })?;
            built.unwrap_or_default()
        }
        Some(OutputTo::Tar(None)) => {
            if std::io::stdout().is_terminal() {
                return Err(crate::output::tar_to_terminal_refused(
                    crate::output::GIVE_TAR_DEST,
                ));
            }
            let mut stdout = std::io::stdout().lock();
            let built = shipyard_engine::build::build(&engine, &request, Some(&mut stdout))?;
            stdout.flush()?;
            built
        }
        _ => shipyard_engine::build::build(&engine, &request, None)?,
    };
    for (name, was) in &built.moved {
        crate::reporter::warn(format!(
            "{name} named {was} before: it names this image now"
        ))?;
    }
    if let Some(id) = &built.id {
        let mut out = std::io::stdout().lock();
        if quiet {
            writeln!(out, "{id}")?;
        } else {
            for name in &built.names {
                writeln!(out, "Built {}", crate::output::printable(name))?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `builder prune`'s flags.
#[derive(Debug, clap::Args)]
pub(crate) struct BuilderPruneArgs {
    /// Taken, as Docker's: every build cache goes (each is a context's
    /// whole).
    #[arg(short, long)]
    pub(crate) all: bool,
    /// Don't ask (needed where there's no terminal to ask on).
    #[arg(short, long)]
    pub(crate) force: bool,
}

/// `shipyard builder …`.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum BuilderCommand {
    /// Remove the build cache (every context's: one a build is using is
    /// kept, said).
    Prune(BuilderPruneArgs),
}

/// `shipyard builder prune`: after asking (unless `-f`), every context's
/// build cache no build is using; what it took said, as Docker's.
pub(crate) fn builder_prune(args: &BuilderPruneArgs) -> Result<ExitCode> {
    let _ = args.all;
    if !args.force {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            bail!("a prune removes what it finds: run it on a terminal to be asked, or give -f");
        }
        let mut err = std::io::stderr().lock();
        writeln!(err, "WARNING! This will remove all build cache.")?;
        if !crate::output::confirm(
            "Are you sure you want to continue?",
            &mut stdin.lock(),
            &mut err,
        )? {
            return Ok(ExitCode::SUCCESS);
        }
    }
    let pruned = shipyard_engine::build::cache::prune(&crate::reporter::engine())?;
    for busy in &pruned.busy {
        crate::reporter::warn(format!("the build cache {busy} is in use by a build: kept"))?;
    }
    let mut out = std::io::stdout().lock();
    for name in &pruned.removed {
        writeln!(out, "Deleted: {name}")?;
    }
    let at_least = if pruned.complete { "" } else { "≥" };
    writeln!(
        out,
        "Total:\t{at_least}{}",
        crate::format::rows::human_size(pruned.bytes, 4)
    )?;
    Ok(ExitCode::SUCCESS)
}
