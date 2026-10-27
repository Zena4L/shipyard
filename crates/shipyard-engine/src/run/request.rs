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
    /// Another container's namespaces it joins, found for this start
    /// ([`resolve_joins`](super::resolve_joins)): none at a create, which
    /// needs the others only to exist.
    pub(crate) joined: Joined,
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
    /// Its IPC namespace (`--ipc`): its own, or another container's.
    pub ipc: IpcMode,
    /// The container's name on its networks.
    pub name: Option<String>,
    /// Its labels (the API's `Labels`): checked as the record keeps them
    /// ([`Labels`](crate::containers::Labels)'s bounds).
    pub labels: std::collections::BTreeMap<String, String>,
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
    /// Ports it exposes beside its image's (`--expose`, the API's
    /// `ExposedPorts`): keys like `80/tcp`, each one port. Published only
    /// with [`publish_all`](Self::publish_all); shown by `inspect`.
    pub expose: Vec<String>,
    /// A file to write the container's ID to (`--cidfile`), once it's made:
    /// refused if it's there already, as Docker's.
    pub cidfile: Option<PathBuf>,
    /// A root filesystem of the caller's own.
    pub rootfs: Option<PathBuf>,
    /// An image to run (a reference).
    pub image: Option<String>,
    /// A command in place of the image's entrypoint (`""`: none).
    pub entrypoint: Option<OsString>,
    /// When to pull the image (`--pull`): `None` is `missing`, the
    /// default; given without an image, it's refused.
    pub pull: Option<PullPolicy>,
    /// The image's platform (`--platform`, step 13.3): it must run here;
    /// `None`, the image's own among what runs here.
    pub platform: Option<shipyard_image::Platform>,
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
    /// tmpfs mounts (`--tmpfs`, `--mount type=tmpfs`), fresh at each start.
    /// Kept beside the record (`created.json`), as `expose`.
    pub tmpfs: Vec<super::TmpfsMount>,
    /// Bind sources made if they're missing (the API's `Binds` host paths
    /// and `BindOptions.CreateMountpoint`), as the caller, 0755 less the
    /// umask: at the create, and at each start (kept beside the record, in
    /// `created.json`), as Docker's daemon makes them.
    pub make_sources: Vec<PathBuf>,
    /// Which of them Shipyard made (at the create, kept in `created.json`
    /// v6, or at this start): bound by descriptor from where they were made
    /// (step 11.7). Set by the engine, never by a caller.
    pub(crate) made_sources: Vec<super::volumes::MadeSource>,
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

/// Another container's namespaces a start joins ([`RunRequest::joined`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct Joined {
    /// Its network's (`--network container:NAME`).
    pub(crate) network: Option<crate::network::JoinedNetwork>,
    /// Its IPC namespace's (`--ipc container:NAME`).
    pub(crate) ipc: Option<crate::network::JoinedProcess>,
}

/// A container's IPC namespace (`--ipc`, the API's `IpcMode`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IpcMode {
    /// Its own (Docker's `private`, and `shareable`: any running
    /// container's can be joined, D11.4b2b-4).
    #[default]
    Private,
    /// Another container's (Docker's `container:NAME`), with its
    /// `/dev/shm`: as given, then by its ID once a create has found it.
    Container(String),
}

impl IpcMode {
    /// Docker's words: `private`, `shareable` (the same here),
    /// `container:NAME`; none is `private`.
    ///
    /// # Errors
    ///
    /// Another mode (`host`, `none`), or `container:` with no name.
    pub fn parse(text: &str) -> std::result::Result<IpcMode, String> {
        match text {
            "" | "private" | "shareable" => Ok(IpcMode::Private),
            other => match other.strip_prefix("container:") {
                Some("") => Err("container: needs the container's name or ID".to_owned()),
                Some(name) => Ok(IpcMode::Container(name.to_owned())),
                None => Err(format!(
                    "{other:?}: give private, shareable or container:NAME (a container has an \
                     IPC namespace of its own, or another container's)"
                )),
            },
        }
    }

    /// The container whose IPC namespace it joins, if it's another's.
    #[must_use]
    pub fn joined_container(&self) -> Option<&str> {
        match self {
            IpcMode::Container(container) => Some(container),
            IpcMode::Private => None,
        }
    }
}

impl RunRequest {
    /// The container whose network it asks to join (`--network
    /// container:NAME`), as given.
    pub(crate) fn network_container(&self) -> Option<&str> {
        self.network.iter().find_map(NetworkMode::joined_container)
    }
}

impl Default for RunRequest {
    fn default() -> Self {
        Self {
            default_mode: std::cell::OnceCell::new(),
            detached: false,
            container: None,
            joined: Joined::default(),
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
            ipc: IpcMode::Private,
            name: None,
            labels: std::collections::BTreeMap::new(),
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
            expose: Vec::new(),
            cidfile: None,
            rootfs: None,
            image: None,
            entrypoint: None,
            pull: None,
            platform: None,
            read_only: false,
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            security_opt: Vec::new(),
            uidmap: Vec::new(),
            gidmap: Vec::new(),
            bundle: None,
            limits: Limits::default(),
            volume: Vec::new(),
            tmpfs: Vec::new(),
            make_sources: Vec::new(),
            made_sources: Vec::new(),
            command: Vec::new(),
        }
    }
}

/// A run's standard input and output: whether the caller's input goes to
/// it (`-i`), whether it gets a terminal of its own (`-t`), and whether its
/// input ends at its first attached client's end at each start.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunStreams {
    /// Pass the caller's input on to the command.
    pub interactive: bool,
    /// Give the command a terminal of the container's own.
    pub tty: bool,
    /// Its input closed once its first attached client's ends, at each
    /// start (Docker's `StdinOnce`, the API's): kept in `created.json`, not
    /// the record's frozen config (F116).
    pub once: bool,
}

/// A container's process limit when none was asked (F62, D11.3-3:
/// Podman's), where the `pids` controller reaches its cgroup.
pub const DEFAULT_PIDS_MAX: u64 = 2048;

/// A run's limits, as the caller worked them out (the CLI from its limit
/// flags): raised, if they couldn't be, where the isolation is made.
#[derive(Debug, Clone)]
pub struct Limits {
    /// The first limit asked for, as the caller names it (`--memory`), if
    /// any: what's refused off Linux.
    pub first_given: Option<&'static str>,
    /// The cgroup limits, or why they can't be had.
    pub resources: std::result::Result<Resources, String>,
    /// Whether a process limit was asked at all (`--pids-limit`, the API's
    /// `PidsLimit`), even "none" (-1, 0): only one never asked gets the
    /// default ([`DEFAULT_PIDS_MAX`], F62).
    pub pids_given: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            first_given: None,
            resources: Ok(Resources::default()),
            pids_given: false,
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

/// `ALL` (any case), or a capability's name, with or without `CAP_`.
impl std::str::FromStr for CapabilityArg {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        if s.eq_ignore_ascii_case("ALL") {
            return Ok(Self::All);
        }
        s.parse().map(Self::One).map_err(|err| format!("{err}"))
    }
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

/// Docker's spelling, as `--security-opt` and the API's `SecurityOpt`
/// give it: `no-new-privileges[=true|false]`, `seccomp=FILE|unconfined`
/// (`:` for `=` too).
impl std::str::FromStr for SecurityOpt {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let (key, value) = match s.split_once(['=', ':']) {
            Some((key, value)) => (key, Some(value)),
            None => (s, None),
        };
        match (key, value) {
            ("no-new-privileges", None | Some("true")) => Ok(Self::NoNewPrivileges(true)),
            ("no-new-privileges", Some("false")) => Ok(Self::NoNewPrivileges(false)),
            ("no-new-privileges", Some(other)) => Err(format!(
                "no-new-privileges takes true or false, not \"{other}\""
            )),
            ("seccomp", Some("unconfined")) => Ok(Self::SeccompUnconfined),
            ("seccomp", Some(path)) if !path.is_empty() => {
                Ok(Self::SeccompProfile(PathBuf::from(path)))
            }
            ("seccomp", _) => Err("seccomp takes a profile file, or unconfined".to_owned()),
            _ => Err(format!(
                "unknown security option \"{s}\" (known: no-new-privileges[=true|false], \
                 seccomp=FILE|unconfined)"
            )),
        }
    }
}
