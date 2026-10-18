//! What a run is asked for ([`RunRequest`]): the CLI's flags (or an API's)
//! as data, with its streams, its limits, its capabilities and security
//! options, and who supervises it ([`Supervision`]).

use std::ffi::OsString;
use std::path::PathBuf;

use shipyard_network::files::HostEntry;
use shipyard_network::ports::PortRequest;
use shipyard_runtime::devices::DeviceRule;
use shipyard_runtime::isolation::IdMap;
use shipyard_runtime::process::{DEFAULT_STOP_TIMEOUT_SECS, Rlimit, Signal};
use shipyard_runtime::resources::Resources;
use shipyard_runtime::security::Capability;

use super::DeviceArg;
use crate::Engine;
use crate::image::PullPolicy;
use crate::network::NetworkMode;

/// What a run is asked for: what `shipyard run`'s flags say, as data (no command
/// line's syntax). Made with [`RunRequest::default`], then its fields set;
/// [`check`](super::check) takes it.
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
    pub streams: RunStreams,
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
    pub(super) fn has_root(&self) -> bool {
        self.rootfs.is_some() || self.image.is_some()
    }

    /// Who supervises this foreground run: its own monitor, as a container
    /// ([`attached`](fn@super::attached): stage 9, steps 9.4c3a and, with
    /// `-t`, 9.4c3b), or this process ([`run`](fn@super::run)). A container on Linux for an image or `--rootfs`
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
    /// Its own monitor: it's a container ([`attached`](fn@super::attached)).
    Container,
    /// This process ([`run`](fn@super::run)): no record. `why`, if a container was meant
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
            streams: RunStreams::default(),
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
pub struct RunStreams {
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
