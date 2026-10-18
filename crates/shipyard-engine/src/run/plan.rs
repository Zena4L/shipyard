//! A request checked as far as it can be before its image is pulled
//! ([`check`]): a bundle's run complete, or a command's [`Planned`], with
//! no side effects; and the [`Invocation`] a run comes to.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shipyard_runtime::isolation::{Isolation, Mount};
use shipyard_runtime::process::{self, ProcessSpec, StopPolicy};
use shipyard_storage::VolumeMount;

use super::RunRequest;
use super::bundle::{from_bundle, load_bundle};
use super::command::{add_env, last_wins};
use super::volumes::canonical_source;
use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::network::{NetworkPlan, RunId};
use crate::report::Warning;
use crate::streams;

/// Checks `request` and builds everything an image doesn't change (see
/// [`Checked`]). It changes nothing on the host: it reads the files the
/// request names (a bundle, a seccomp profile, devices), and may report a
/// warning (the default network's fallback).
///
/// # Errors
///
/// What's wrong with the request (most often
/// [`ErrorKind::InvalidRequest`], [`ErrorKind::Unsupported`] off Linux,
/// [`ErrorKind::NeedsRoot`]), or a bundle that can't be read.
pub fn check(engine: &Engine, request: RunRequest) -> Result<Checked> {
    request.validate()?;
    request.plan(engine)
}

/// Everything a run needs, checked.
#[derive(Debug)]
pub struct Invocation {
    /// The command.
    pub(super) spec: ProcessSpec,
    /// How it's stopped.
    pub(super) stop: StopPolicy,
    /// Its isolation, if it's isolated.
    pub(super) isolation: Option<Isolation>,
    /// Its standard input, output and error.
    pub(super) stdio: process::Stdio,
    /// What a bundle asks for that's ignored, and the like, to show the
    /// user.
    pub(super) warnings: Vec<Warning>,
    /// What its isolation has that the user didn't ask for.
    pub(super) generated: Generated,
}

impl Invocation {
    /// The command, as it will run.
    #[must_use]
    pub fn spec(&self) -> &ProcessSpec {
        &self.spec
    }

    /// How it's stopped.
    #[must_use]
    pub fn stop(&self) -> StopPolicy {
        self.stop
    }

    /// Its isolation, if it's isolated.
    #[must_use]
    pub fn isolation(&self) -> Option<&Isolation> {
        self.isolation.as_ref()
    }

    /// What the run will say, as warnings, when it starts.
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// What a run's isolation has that the user didn't ask for: Shipyard's own
/// defaults, which `--no-isolation` could do without.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Generated {
    /// The hostname is the run's ID.
    pub(super) hostname: bool,
    /// The network is the default one.
    pub(super) network: bool,
    /// The settings are the default network's own (no `--sysctl` given).
    pub(super) sysctls: bool,
}

/// A run checked as far as it can be before its image is pulled: a mistake
/// in a request costs no download.
#[derive(Debug)]
pub enum Checked {
    /// An OCI bundle's run, complete.
    Bundle(Box<Invocation>),
    /// A command's run, finished once its image (if any) is ready.
    Command(Box<Planned>),
}

/// A command's run, checked: everything but what an image brings.
#[derive(Debug)]
pub struct Planned {
    pub(super) request: RunRequest,
    pub(super) stop: StopPolicy,
    /// Without its root yet, for an image.
    pub(super) isolation: Option<Isolation>,
    /// Its standard input, output and error.
    pub(super) stdio: process::Stdio,
    /// Why a rootless caller's maps are short, if they are.
    pub(super) short_maps: Option<String>,
    /// The `--volume`s, parsed: a host path absolute and found.
    pub(super) volumes: Vec<VolumeMount>,
    /// Its network, for an isolated run.
    pub(super) network: Option<NetworkPlan>,
}

impl Planned {
    /// Its network, for an isolated run: what's set up before it starts.
    #[must_use]
    pub(super) fn network(&self) -> Option<&NetworkPlan> {
        self.network.as_ref()
    }
}

impl RunRequest {
    /// Checks and builds everything an image doesn't change, with no side
    /// effects: the stop policy, the isolation (without an image's root),
    /// the process (as far as it's given), the volumes.
    fn plan(self, engine: &Engine) -> Result<Checked> {
        let stop = StopPolicy {
            signal: self.stop_signal,
            timeout: Duration::from_secs(self.stop_timeout),
        };
        stop.validate()?;
        if let Some(dir) = &self.bundle {
            // Refused before the bundle is read: `-t` is wrong whatever its
            // config says (a bundle asks for a terminal itself).
            if self.streams.tty {
                return Err(crate::StreamsError::TerminalFromBundle.into());
            }
            let bundle = load_bundle(dir)?;
            // Its own `process.terminal` is `-t`.
            let stdio = self.stdio(streams::Runs::Bundle {
                terminal: bundle.terminal.is_some(),
                size: bundle.terminal.and_then(|terminal| terminal.size),
            })?;
            return Ok(Checked::Bundle(Box::new(from_bundle(
                dir, bundle, stop, stdio,
            )?)));
        }
        if self.image.is_some() && !cfg!(target_os = "linux") {
            bail!(
                Unsupported,
                "an image needs Linux (overlayfs), which {} doesn't have",
                std::env::consts::OS
            );
        }
        let run = RunId::new()?;
        let (isolation, short_maps) = self.isolation(engine, &run)?;
        let stdio = self.stdio(if isolation.is_some() {
            streams::Runs::Isolated
        } else {
            streams::Runs::Unisolated
        })?;
        let network = isolation
            .as_ref()
            .map(|_| self.network_plan(engine, run))
            .transpose()?;
        let volumes = self.volume_mounts()?;
        if let Some(dir) = self.workdir.as_ref().filter(|dir| !dir.is_absolute()) {
            bail!(
                InvalidRequest,
                "--workdir must be an absolute path, not {}",
                dir.display()
            );
        }
        if self.oom_score_adj.is_some() && !cfg!(target_os = "linux") {
            bail!(
                Unsupported,
                "--oom-score-adj needs Linux's OOM killer, which {} doesn't have",
                std::env::consts::OS
            );
        }
        self.check_process(isolation.as_ref())?;
        Ok(Checked::Command(Box::new(Planned {
            request: self,
            stop,
            isolation,
            stdio,
            short_maps,
            volumes,
            network,
        })))
    }

    /// The command's standard input, output and error, as `-i` and `-t` ask
    /// and the engine decides for a container on a terminal (see
    /// `shipyard_engine::streams`).
    fn stdio(&self, runs: streams::Runs) -> Result<process::Stdio> {
        if self.detached {
            // The monitor's: the runtime gives it its logs and its stdin.
            return Ok(process::Stdio::default());
        }
        let request = streams::Request {
            tty: self.streams.tty,
            interactive: self.streams.interactive,
            runs,
        };
        Ok(streams::plan(request, streams::Terminals::detect())?)
    }

    /// The process as far as the request gives it, checked (the image's
    /// command, environment and working directory aside: a placeholder
    /// stands in for them, and they're checked once the image is there).
    fn check_process(&self, isolation: Option<&Isolation>) -> Result<()> {
        let mut command: Vec<OsString> = self
            .entrypoint
            .iter()
            .filter(|given| !given.is_empty())
            .cloned()
            .chain(self.command.iter().cloned())
            .collect();
        if command.is_empty() {
            command.push(OsString::from("true"));
        }
        let mut spec = ProcessSpec::new(command);
        for entry in &self.env {
            add_env(&mut spec.env, entry, &self.caller_env)?;
        }
        spec.cwd = self.workdir.clone().unwrap_or_else(|| PathBuf::from("/"));
        spec.user = self.resolve_user(isolation, None)?;
        spec.rlimits = last_wins(self.ulimit.clone());
        spec.oom_score_adj = self.oom_score_adj;
        spec.validate()?;
        Ok(())
    }

    /// The `--volume`s, parsed and checked as the runtime will (a named
    /// volume's place stands in for its directory); a host path made
    /// absolute (symlinks resolved) and found.
    pub(super) fn volume_mounts(&self) -> Result<Vec<VolumeMount>> {
        if !self.volume.is_empty() && !self.has_root() {
            bail!(
                InvalidRequest,
                "--volume needs a root filesystem (an image or --rootfs)"
            );
        }
        self.volume
            .iter()
            .map(|spec| {
                let mut mount = VolumeMount::parse(spec).map_err(|err| {
                    Error::new(ErrorKind::InvalidRequest, format!("--volume {spec}: {err}"))
                })?;
                if !shipyard_storage::volume::is_named_volume(&mount.source) {
                    mount.source = canonical_source(&mount.source)
                        .with_context(|| format!("--volume {spec}"))?;
                }
                let stand_in = Path::new("/").join(&mount.source);
                Mount::bind(stand_in, &mount.destination)
                    .validate()
                    .map_err(Error::from)
                    .with_context(|| format!("--volume {spec}"))?;
                Ok(mount)
            })
            .collect()
    }
}
