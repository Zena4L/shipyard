//! A run, from a request (the CLI's `shipyard run` flags, or an API's) to
//! its end: [`run`] does it all. Its steps are here too, for a caller that
//! runs a container another way ([`detached`](fn@detached): one that outlives its
//! caller): the request
//! checked as far as it can be before an image is pulled ([`check`]), the
//! image made ready (`prepare_image`, Linux), then the run finished with what
//! the image brings ([`Planned::finish`]) into an [`Invocation`] the
//! runtime runs.
//!
//! A mistake in a request costs no download: everything an image doesn't
//! change is checked first. Nothing here prints: what a run has to say goes
//! to the engine's reporter, or comes back as data.

mod bundle;
mod cleanup;
mod command;
mod connect;
mod detached;
mod devices;
mod health;
mod helper;
pub(crate) mod holds;
mod isolation;
mod launch;
mod network;
pub(crate) use network::MAX_NETWORKS;
mod recover;
mod rename;
#[cfg(target_os = "linux")]
mod restart_policy;
mod stop;
#[cfg(test)]
mod tests;
mod update;
mod volumes;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shipyard_network::files::HostEntry;
use shipyard_network::ports::PortRequest;
use shipyard_runtime::devices::DeviceRule;
use shipyard_runtime::isolation::{IdMap, IdMaps, Isolation, Mount, Root};
use shipyard_runtime::process::{
    self, DEFAULT_STOP_TIMEOUT_SECS, Env, ProcessSpec, Rlimit, Signal, StopPolicy,
};
use shipyard_runtime::resources::Resources;
use shipyard_runtime::security::Capability;
use shipyard_storage::{VolumeHold, VolumeMount};

use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::image::PullPolicy;
use crate::network::{NetworkMode, NetworkPlan, RunDir, RunId};
use crate::report::{Warning, WarningKind};
use crate::streams;

pub use cleanup::cleanup_entry_point;
pub use command::{parse_name_or_id, parse_user};
pub use connect::{ConnectRequest, connect, disconnect};
pub use detached::{
    Detached, Foreground, Ran, attached, create, detached, detached_with, remove, start,
    start_attached, start_with,
};
#[cfg(target_os = "linux")]
pub(crate) use detached::{Files, container_files};
pub use devices::DeviceArg;
pub use health::health_entry_point;
pub use launch::{Outcome, run};
pub use recover::{Booted, Stopped, boot, restarts_held_off, stop_for_the_host};
pub use rename::rename;
pub use stop::{StopOptions, kill, restart, stop, wait};
pub use update::{UpdateRequest, update};

use bundle::{from_bundle, load_bundle};
use command::{add_env, add_image_env, last_wins};
use volumes::{bind_etc_files, bind_volumes, canonical_source, hold_volumes};

/// What a run is asked for: what `shipyard run`'s flags say, as data (no command
/// line's syntax). Made with [`RunRequest::default`], then its fields set;
/// [`check`] takes it.
#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a mirror of the command line's switches, one bool each"
)]
pub struct RunRequest {
    /// The run's default network, worked out once (see `default_network`).
    /// Crate-wide only so that a container's config can name every field.
    pub(crate) default_mode: std::cell::OnceCell<NetworkMode>,
    /// A detached container's start (`start`, `run -d`): its streams are
    /// its monitor's (its logs), never the caller's terminal, so none is
    /// looked for (`create -it`, then `start` from a script, works).
    pub(crate) detached: bool,
    /// A detached container's start: its ID, written into its leases (how
    /// `rename` finds them).
    pub(crate) container: Option<crate::containers::ContainerId>,
    /// Its standard input and output (`-i` and `-t`).
    pub streams: Streams,
    /// `KEY=VALUE` variables to set, or bare `KEY`s to copy from the
    /// caller's environment ([`caller_env`](Self::caller_env)).
    pub env: Vec<OsString>,
    /// The caller's values for the bare `KEY`s in [`env`](Self::env): the
    /// engine never reads its own process's environment for them.
    pub caller_env: Vec<(OsString, OsString)>,
    /// The working directory inside the container (absolute).
    pub workdir: Option<PathBuf>,
    /// The user and group to run as: names or numbers.
    pub user: Option<(String, Option<String>)>,
    /// Supplementary groups: names or numbers.
    pub group_add: Vec<String>,
    /// Resource limits (a later one for the same resource wins).
    pub ulimit: Vec<Rlimit>,
    /// The command's OOM score adjustment.
    pub oom_score_adj: Option<i32>,
    /// Host devices given to the container.
    pub device: Vec<DeviceArg>,
    /// Kinds of devices the container may use.
    pub device_cgroup_rule: Vec<DeviceRule>,
    /// The signal that stops the command.
    pub stop_signal: Signal,
    /// Seconds to wait after the stop signal before killing the command.
    pub stop_timeout: u64,
    /// What happens when its container ends (`--restart`).
    pub restart: crate::containers::RestartPolicy,
    /// Its health check (`--health-cmd` and the rest), run while its
    /// container runs.
    pub health: Option<crate::health::HealthCheck>,
    /// Run without isolation.
    pub no_isolation: bool,
    /// The container's hostname.
    pub hostname: Option<String>,
    /// The container's NIS domain name.
    pub domainname: Option<String>,
    /// Kernel settings the container owns, `(key, value)`.
    pub sysctl: Vec<(String, String)>,
    /// The networks asked for, in order (none: the default).
    pub network: Vec<NetworkMode>,
    /// The container's name on its networks.
    pub name: Option<String>,
    /// Its other names on its networks.
    pub network_alias: Vec<String>,
    /// Its address on its one bridge network.
    pub ip: Option<std::net::IpAddr>,
    /// Its IPv6 address on its one bridge network.
    pub ip6: Option<std::net::Ipv6Addr>,
    /// Its hardware address on its one bridge network.
    pub mac_address: Option<[u8; 6]>,
    /// Entries for its `/etc/hosts`.
    pub add_host: Vec<HostEntry>,
    /// DNS servers for its `/etc/resolv.conf`.
    pub dns: Vec<std::net::IpAddr>,
    /// DNS search domains.
    pub dns_search: Vec<String>,
    /// Resolver options.
    pub dns_option: Vec<String>,
    /// Ports published on the host.
    pub publish: Vec<PortRequest>,
    /// Every port the image exposes, on free host ports.
    pub publish_all: bool,
    /// A root filesystem of the caller's own.
    pub rootfs: Option<PathBuf>,
    /// An image to run (a reference).
    pub image: Option<String>,
    /// A command in place of the image's entrypoint (`""`: none).
    pub entrypoint: Option<OsString>,
    /// When to pull the image (`--pull`): `None` is `missing`, the
    /// default; given without an image, it's refused.
    pub pull: Option<PullPolicy>,
    /// A read-only root filesystem.
    pub read_only: bool,
    /// Capabilities added to the default set.
    pub cap_add: Vec<CapabilityArg>,
    /// Capabilities taken out of the default set.
    pub cap_drop: Vec<CapabilityArg>,
    /// Security options.
    pub security_opt: Vec<SecurityOpt>,
    /// User namespace maps for user IDs.
    pub uidmap: Vec<IdMap>,
    /// User namespace maps for group IDs (the user IDs' if none).
    pub gidmap: Vec<IdMap>,
    /// An OCI bundle to run, instead of everything above.
    pub bundle: Option<PathBuf>,
    /// Memory, CPU, process and I/O limits.
    pub limits: Limits,
    /// Volumes and host paths to mount, `SOURCE:DEST[:ro]`.
    pub volume: Vec<String>,
    /// The command and its arguments.
    pub command: Vec<OsString>,
}

/// Without the environment's values (`--env`'s and the caller's: secrets,
/// often): only their names, as the runtime's `Env` shows it.
impl std::fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = |entry: &OsString| {
            let bytes = std::os::unix::ffi::OsStrExt::as_bytes(entry.as_os_str());
            String::from_utf8_lossy(bytes.split(|&b| b == b'=').next().unwrap_or_default())
                .into_owned()
        };
        f.debug_struct("RunRequest")
            .field("env", &self.env.iter().map(name).collect::<Vec<_>>())
            .field(
                "caller_env",
                &self
                    .caller_env
                    .iter()
                    .map(|(key, _)| key)
                    .collect::<Vec<_>>(),
            )
            .field("command", &self.command)
            .field("image", &self.image)
            .field("rootfs", &self.rootfs)
            .field("bundle", &self.bundle)
            .field("no_isolation", &self.no_isolation)
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl RunRequest {
    /// Whether the container has a root filesystem of its own (`rootfs` or
    /// `image`).
    fn has_root(&self) -> bool {
        self.rootfs.is_some() || self.image.is_some()
    }

    /// Who supervises this foreground run: its own monitor, as a container
    /// ([`attached`]: stage 9, steps 9.4c3a and, with `-t`, 9.4c3b), or this
    /// process ([`run`]). A container on Linux for an image or `--rootfs`
    /// run, isolated, without descriptors `passed` to it (they can't go
    /// through a monitor), where `engine` has a state directory and a store
    /// for containers. Without them (a rootless caller
    /// without `XDG_RUNTIME_DIR`) it's run here, saying why.
    #[must_use]
    pub fn supervision(
        &self,
        engine: &Engine,
        passed: &shipyard_runtime::process::passed::PassedFds,
    ) -> Supervision {
        let could = cfg!(target_os = "linux")
            && self.has_root()
            && self.bundle.is_none()
            && !self.no_isolation
            && passed.is_empty();
        if !could {
            return Supervision::Here { why: None };
        }
        let places = engine
            .paths
            .runtime_root()
            .and_then(|_| engine.paths.containers().map(drop));
        match places {
            Ok(()) => Supervision::Container,
            Err(error) => Supervision::Here {
                why: Some(format!(
                    "this run isn't a container (no record, no `ps`): {error}"
                )),
            },
        }
    }
}

/// Who supervises a foreground run ([`RunRequest::supervision`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervision {
    /// Its own monitor: it's a container ([`attached`]).
    Container,
    /// This process ([`run`]): no record. `why`, if a container was meant
    /// and couldn't be made, to say so.
    Here {
        /// Why it isn't a container, when it would have been one.
        why: Option<String>,
    },
}

impl Default for RunRequest {
    fn default() -> Self {
        Self {
            default_mode: std::cell::OnceCell::new(),
            detached: false,
            container: None,
            streams: Streams::default(),
            env: Vec::new(),
            caller_env: Vec::new(),
            workdir: None,
            user: None,
            group_add: Vec::new(),
            ulimit: Vec::new(),
            oom_score_adj: None,
            device: Vec::new(),
            device_cgroup_rule: Vec::new(),
            stop_signal: Signal::TERM,
            stop_timeout: DEFAULT_STOP_TIMEOUT_SECS,
            restart: crate::containers::RestartPolicy::No,
            health: None,
            no_isolation: false,
            hostname: None,
            domainname: None,
            sysctl: Vec::new(),
            network: Vec::new(),
            name: None,
            network_alias: Vec::new(),
            ip: None,
            ip6: None,
            mac_address: None,
            add_host: Vec::new(),
            dns: Vec::new(),
            dns_search: Vec::new(),
            dns_option: Vec::new(),
            publish: Vec::new(),
            publish_all: false,
            rootfs: None,
            image: None,
            entrypoint: None,
            pull: None,
            read_only: false,
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            security_opt: Vec::new(),
            uidmap: Vec::new(),
            gidmap: Vec::new(),
            bundle: None,
            limits: Limits::default(),
            volume: Vec::new(),
            command: Vec::new(),
        }
    }
}

/// A run's standard input and output: whether the caller's input goes to
/// it (`-i`), and whether it gets a terminal of its own (`-t`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Streams {
    /// Pass the caller's input on to the command.
    pub interactive: bool,
    /// Give the command a terminal of the container's own.
    pub tty: bool,
}

/// A run's limits, as the caller worked them out (the CLI from its limit
/// flags): raised, if they couldn't be, where the isolation is made.
#[derive(Debug, Clone)]
pub struct Limits {
    /// The first limit asked for, as the caller names it (`--memory`), if
    /// any: what's refused off Linux.
    pub first_given: Option<&'static str>,
    /// The cgroup limits, or why they can't be had.
    pub resources: std::result::Result<Resources, String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            first_given: None,
            resources: Ok(Resources::default()),
        }
    }
}

impl Limits {
    /// Whether any limit is asked for (or one that couldn't be worked out).
    #[must_use]
    pub fn given(&self) -> bool {
        self.first_given.is_some()
            || self
                .resources
                .as_ref()
                .map_or(true, |resources| !resources.is_empty())
    }
}

impl RunRequest {
    /// Refuses what can't go together, or what needs something it doesn't
    /// have, as the CLI's flags can't (the CLI's parser refuses them
    /// first): what planning takes for granted, so that nothing asked is
    /// quietly left out (a capability dropped, an image's root, an
    /// isolation).
    ///
    /// - A bundle says everything itself: nothing else may be asked with
    ///   one.
    /// - Without isolation (`no_isolation`), nothing a container alone has:
    ///   its devices, names, settings, network, root, capabilities, security
    ///   options, user namespace, volumes or limits.
    /// - One root filesystem: `rootfs` or `image`.
    /// - What needs another: `entrypoint` and `publish_all` an image;
    ///   `read_only` a root; `gidmap` a `uidmap`; `ip`, `ip6`, `mac_address`
    ///   and `network_alias` a `network`.
    /// - A command, unless an image or a bundle gives one.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`], naming what's wrong.
    pub fn validate(&self) -> Result<()> {
        let (process, container) = self.asked();
        if self.bundle.is_some() {
            if given(&process).or_else(|| given(&container)).is_some() {
                bail!(
                    InvalidRequest,
                    "a bundle says what to run and how, in its config.json: nothing else can be \
                     asked with one"
                );
            }
            return Ok(());
        }
        if self.no_isolation
            && let Some(what) = given(&container)
        {
            bail!(
                InvalidRequest,
                "{what} needs isolation: it's a container's, and --no-isolation runs the command \
                 on the host"
            );
        }
        if self.rootfs.is_some() && self.image.is_some() {
            bail!(
                InvalidRequest,
                "--rootfs and an image both give the container a root filesystem: give one"
            );
        }
        self.check_needs()
    }

    /// What's asked of the process (and of a run, beside its container),
    /// and what only a container has: each with whether it's asked. Every
    /// field is named here: one added must be placed.
    fn asked(&self) -> (Asked<14>, Asked<22>) {
        let Self {
            default_mode: _,
            detached: _,
            container: _,
            streams: _,
            env,
            caller_env: _,
            workdir,
            user,
            group_add,
            ulimit,
            oom_score_adj,
            device,
            device_cgroup_rule,
            stop_signal: _,
            stop_timeout: _,
            restart: _,
            health: _,
            no_isolation,
            hostname,
            domainname,
            sysctl,
            network,
            name,
            network_alias,
            ip,
            ip6,
            mac_address,
            add_host,
            dns,
            dns_search,
            dns_option,
            publish,
            publish_all,
            rootfs,
            image,
            entrypoint,
            pull: _,
            read_only,
            cap_add,
            cap_drop,
            security_opt,
            uidmap,
            gidmap,
            bundle: _,
            limits,
            volume,
            command,
        } = self;
        let process = [
            ("a command", !command.is_empty()),
            ("--env", !env.is_empty()),
            ("--workdir", workdir.is_some()),
            ("--user", user.is_some()),
            ("--group-add", !group_add.is_empty()),
            ("--ulimit", !ulimit.is_empty()),
            ("--oom-score-adj", oom_score_adj.is_some()),
            ("--no-isolation", *no_isolation),
            ("--network-alias", !network_alias.is_empty()),
            ("--ip", ip.is_some()),
            ("--ip6", ip6.is_some()),
            ("--mac-address", mac_address.is_some()),
            ("--read-only", *read_only),
            ("--entrypoint", entrypoint.is_some()),
        ];
        let container = [
            ("--device", !device.is_empty()),
            ("--device-cgroup-rule", !device_cgroup_rule.is_empty()),
            ("--hostname", hostname.is_some()),
            ("--domainname", domainname.is_some()),
            ("--sysctl", !sysctl.is_empty()),
            ("--network", !network.is_empty()),
            ("--name", name.is_some()),
            ("--add-host", !add_host.is_empty()),
            ("--dns", !dns.is_empty()),
            ("--dns-search", !dns_search.is_empty()),
            ("--dns-option", !dns_option.is_empty()),
            ("--publish", !publish.is_empty()),
            ("--publish-all", *publish_all),
            ("--rootfs", rootfs.is_some()),
            ("an image", image.is_some()),
            ("--cap-add", !cap_add.is_empty()),
            ("--cap-drop", !cap_drop.is_empty()),
            ("--security-opt", !security_opt.is_empty()),
            ("--uidmap", !uidmap.is_empty()),
            ("--gidmap", !gidmap.is_empty()),
            ("--volume", !volume.is_empty()),
            ("a resource limit", limits.given()),
        ];
        (process, container)
    }

    /// Refuses what's asked without what it needs, and a run without a
    /// command (an image gives one of its own).
    fn check_needs(&self) -> Result<()> {
        let has_image = self.image.is_some();
        let has_network = !self.network.is_empty();
        let needs = [
            (
                "--entrypoint",
                self.entrypoint.is_some(),
                has_image,
                "an image",
            ),
            ("--publish-all", self.publish_all, has_image, "an image"),
            ("--pull", self.pull.is_some(), has_image, "an image"),
            (
                "--read-only",
                self.read_only,
                self.has_root(),
                "an image or --rootfs",
            ),
            (
                "--gidmap",
                !self.gidmap.is_empty(),
                !self.uidmap.is_empty(),
                "--uidmap",
            ),
            ("--ip", self.ip.is_some(), has_network, "--network"),
            ("--ip6", self.ip6.is_some(), has_network, "--network"),
            (
                "--mac-address",
                self.mac_address.is_some(),
                has_network,
                "--network",
            ),
            (
                "--network-alias",
                !self.network_alias.is_empty(),
                has_network,
                "--network",
            ),
        ];
        if let Some((what, _, _, needed)) = needs.iter().find(|(_, given, has, _)| *given && !*has)
        {
            bail!(InvalidRequest, "{what} needs {needed}");
        }
        if self.command.is_empty() && !has_image {
            bail!(
                InvalidRequest,
                "a command is needed (an image gives one of its own)"
            );
        }
        Ok(())
    }
}

/// The warning for a container that is root on the host's own files: no
/// root filesystem and no user namespace, run by the host's root.
fn root_on_host_files(isolation: &Isolation) -> Option<Warning> {
    (isolation.root.is_none() && isolation.id_maps.is_none() && crate::network::host_root()).then(
        || Warning {
            kind: WarningKind::RootOnHostFiles,
            message: "the container is root on the host's own files: with no image or \
                      --rootfs, its root filesystem is the host's, and root in it can change \
                      them (give it an image, or a user namespace with --uidmap)"
                .to_owned(),
        },
    )
}

/// What a request asks, by name, each with whether it's asked.
type Asked<const N: usize> = [(&'static str, bool); N];

/// The first of `list` that's asked, by name.
fn given(list: &[(&'static str, bool)]) -> Option<&'static str> {
    list.iter().find(|(_, given)| *given).map(|(what, _)| *what)
}

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
    spec: ProcessSpec,
    /// How it's stopped.
    stop: StopPolicy,
    /// Its isolation, if it's isolated.
    isolation: Option<Isolation>,
    /// Its standard input, output and error.
    stdio: process::Stdio,
    /// What a bundle asks for that's ignored, and the like, to show the
    /// user.
    warnings: Vec<Warning>,
    /// What its isolation has that the user didn't ask for.
    generated: Generated,
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
struct Generated {
    /// The hostname is the run's ID.
    hostname: bool,
    /// The network is the default one.
    network: bool,
    /// The settings are the default network's own (no `--sysctl` given).
    sysctls: bool,
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
    request: RunRequest,
    stop: StopPolicy,
    /// Without its root yet, for an image.
    isolation: Option<Isolation>,
    /// Its standard input, output and error.
    stdio: process::Stdio,
    /// Why a rootless caller's maps are short, if they are.
    short_maps: Option<String>,
    /// The `--volume`s, parsed: a host path absolute and found.
    volumes: Vec<VolumeMount>,
    /// Its network, for an isolated run.
    network: Option<NetworkPlan>,
}

impl Planned {
    /// Its network, for an isolated run: what's set up before it starts.
    #[must_use]
    pub fn network(&self) -> Option<&NetworkPlan> {
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
    fn volume_mounts(&self) -> Result<Vec<VolumeMount>> {
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

impl Planned {
    /// Finishes the run with `image`'s root and defaults, if it has an
    /// image: each applies only where the request didn't say otherwise. Named
    /// volumes are made if new, and held: keep what's returned with them
    /// until the run ends.
    ///
    /// # Errors
    ///
    /// What the image's defaults make wrong (no command, a user it doesn't
    /// have), or a volume that can't be made or held.
    pub fn finish(
        self,
        engine: &Engine,
        image: Option<ImageDefaults>,
        etc: Option<&RunDir>,
    ) -> Result<(Invocation, Vec<VolumeHold>)> {
        let Self {
            request,
            stop,
            mut isolation,
            stdio,
            short_maps,
            volumes,
            network: _,
        } = self;
        let config = image.as_ref().map(|image| &image.config);
        let mut warnings: Vec<Warning> = short_maps
            .into_iter()
            .map(|message| Warning {
                kind: WarningKind::ShortIdMaps,
                message,
            })
            .collect();

        // The user: an explicit --user wins; else the image's User; else the
        // isolation default (root inside) or the caller (unisolated).
        let user = request.resolve_user(isolation.as_ref(), config)?;

        // Environment: the image's variables first, then --env (which wins),
        // then a default PATH if none. The image's are taken as they are:
        // one without a value is never looked up here, on the host.
        let mut env = Env::new();
        if let Some(config) = config {
            add_image_env(&mut env, &config.env, &mut warnings)?;
        }
        for entry in &request.env {
            add_env(&mut env, entry, &request.caller_env)?;
        }
        env.fill_default_path();

        let mut spec = ProcessSpec::new(request.resolve_command(config)?);
        spec.env = env;
        spec.cwd = request.resolve_workdir(config);
        spec.user = user;
        spec.rlimits = last_wins(request.ulimit.clone());
        spec.oom_score_adj = request.oom_score_adj;
        spec.validate()?;

        let mut held = Vec::new();
        if let Some(isolation) = isolation.as_mut() {
            if let Some(image) = image {
                // The image's layers become the root: an overlay mounted at
                // this path inside the container's namespaces.
                let mut root = Root::new(image.root);
                root.read_only = request.read_only;
                root.overlay = Some(image.overlay);
                isolation.root = Some(root);
            }
            if let (Some(root), Some(etc)) = (isolation.root.as_mut(), etc) {
                bind_etc_files(root, etc);
            }
            let named = match isolation.root.as_mut() {
                Some(root) => bind_volumes(engine, root, volumes)?,
                // `check` refused volumes without a root.
                None => Vec::new(),
            };
            // All checked before a volume is made. An overlay path that can't
            // be used is the image store's.
            isolation.validate().map_err(|err| {
                let store = matches!(
                    err,
                    shipyard_runtime::isolation::IsolationError::InvalidOverlayPath { .. }
                );
                if store {
                    // Where the store is: the host's setting, not the request.
                    Error::wrap(ErrorKind::Environment, err).context(
                        "the image store's path can't be used for an overlay (set \
                         SHIPYARD_IMAGE_STORE to another directory)",
                    )
                } else {
                    Error::from(err)
                }
            })?;
            held = hold_volumes(engine, &named)?;
        }
        if isolation
            .as_ref()
            .and_then(|isolation| isolation.id_maps.as_ref())
            .is_some_and(IdMaps::maps_host_root)
        {
            warnings.push(Warning {
                kind: WarningKind::RootOnHostFiles,
                message: "--uidmap/--gidmap map host ID 0: a user inside is root on the host's \
                          files, which a user namespace is meant to prevent"
                    .to_owned(),
            });
        }
        warnings.extend(isolation.as_ref().and_then(root_on_host_files));
        let generated = Generated {
            hostname: isolation.is_some() && request.hostname.is_none(),
            network: isolation.is_some() && request.network.is_empty(),
            sysctls: isolation.is_some() && request.sysctl.is_empty(),
        };
        Ok((
            Invocation {
                spec,
                stop,
                isolation,
                stdio,
                warnings,
                generated,
            },
            held,
        ))
    }
}

/// Prepares the request's image, if it has one (Linux only): pulls it,
/// extracts its layers and makes the run's writable area, returning the
/// defaults a run applies and the area (to keep until the run ends).
///
/// # Errors
///
/// If the image can't be found, pulled or extracted.
#[cfg(target_os = "linux")]
pub fn prepare_image(
    engine: &Engine,
    planned: &Planned,
) -> Result<Option<(ImageDefaults, crate::image::RunArea)>> {
    let Some(reference) = &planned.request.image else {
        return Ok(None);
    };
    // As root, `--uidmap`'s maps: the layers are shifted by them. (A
    // rootless caller's maps are its own range, which extraction uses.)
    let maps = planned
        .isolation
        .as_ref()
        .and_then(|isolation| isolation.id_maps.as_ref())
        .filter(|_| nix::unistd::geteuid().is_root());
    let prepared = crate::image::prepare(
        engine,
        reference,
        planned.request.pull.unwrap_or(PullPolicy::Missing),
        maps,
    )?;
    Ok(Some((
        ImageDefaults {
            root: prepared.root,
            overlay: prepared.overlay,
            config: prepared.config,
        },
        prepared.area,
    )))
}

/// Prepares a container's image to start ([`crate::image::prepare_owned`]):
/// by the digest it was made from (`image_id`), with the container's own
/// writable area (container `id`, of container store `owner`). Its
/// request's image is the reference it was made with.
///
/// # Errors
///
/// As [`crate::image::prepare_owned`].
#[cfg(target_os = "linux")]
pub(crate) fn prepare_owned_image(
    engine: &Engine,
    planned: &Planned,
    id: &str,
    owner: &str,
    image_id: &str,
) -> Result<Option<(ImageDefaults, crate::image::RunArea)>> {
    let Some(reference) = &planned.request.image else {
        return Ok(None);
    };
    let maps = planned
        .isolation
        .as_ref()
        .and_then(|isolation| isolation.id_maps.as_ref())
        .filter(|_| nix::unistd::geteuid().is_root());
    let prepared = crate::image::prepare_owned(engine, id, owner, reference, image_id, maps)?;
    Ok(Some((
        ImageDefaults {
            root: prepared.root,
            overlay: prepared.overlay,
            config: prepared.config,
        },
        prepared.area,
    )))
}

/// What an image contributes to a run: its root filesystem (an overlay the
/// runtime mounts) and its baked-in defaults, applied under what the request
/// says.
#[derive(Debug, Clone)]
pub struct ImageDefaults {
    /// The merged mount point — the container's root path.
    pub root: PathBuf,
    /// The overlay there (layer dirs + upper + work).
    pub overlay: shipyard_runtime::isolation::Overlay,
    /// The image's runtime config (default command, env, workdir, user).
    pub config: shipyard_image::RuntimeConfig,
}

/// `--cap-add` or `--cap-drop`: one capability, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityArg {
    /// Every capability.
    All,
    /// This one.
    One(Capability),
}

/// `--security-opt`: Docker's spelling, `KEY=VALUE` or `KEY:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityOpt {
    /// `no-new-privileges`: whether setuid programs inside are kept from
    /// gaining privileges.
    NoNewPrivileges(bool),
    /// `seccomp=unconfined`: no filter.
    SeccompUnconfined,
    /// `seccomp=FILE`.
    SeccompProfile(PathBuf),
}
