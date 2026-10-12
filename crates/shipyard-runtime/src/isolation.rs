//! What a command is isolated from: Linux namespaces (`namespaces(7)`).
//!
//! A namespace gives a process its own copy of one global resource: its own
//! process IDs (PID namespace), mount table (mount), hostname (UTS), System V
//! IPC objects (IPC), and so on. A container is a process in a fresh set of
//! them. This module only *describes* the isolation and checks that it's
//! safe to create; creating it is the process code's job.
//!
//! The shape follows the OCI runtime spec's `linux.namespaces` and `hostname`,
//! so stage 3 can fill it from a `config.json`.
//!
//! [`Join`] describes the other direction: a new process entering the
//! namespaces of one that's already running (`setns(2)`), as `exec` into a
//! container does.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::id_map::{IdMap, IdMapError, IdMaps, MapKind, RootlessMaps};

/// A kind of Linux namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Namespace {
    /// Process IDs: the first process inside is PID 1 (`pid_namespaces(7)`).
    Pid,
    /// The mount table (`mount_namespaces(7)`).
    Mount,
    /// Hostname and NIS domain name (`uts_namespaces(7)`).
    Uts,
    /// System V IPC objects and POSIX message queues (`ipc_namespaces(7)`).
    Ipc,
    /// Network devices, addresses, ports, routes (`network_namespaces(7)`).
    Network,
    /// User and group IDs (`user_namespaces(7)`).
    User,
    /// The cgroup root (`cgroup_namespaces(7)`).
    Cgroup,
    /// The boot and monotonic clocks (`time_namespaces(7)`).
    Time,
}

impl Namespace {
    /// Every kind, sorted by their `/proc/PID/ns` names.
    pub const ALL: [Self; 8] = [
        Self::Cgroup,
        Self::Ipc,
        Self::Mount,
        Self::Network,
        Self::Pid,
        Self::Time,
        Self::User,
        Self::Uts,
    ];

    /// Its name in `/proc/PID/ns/` (and in OCI's `linux.namespaces[].type`,
    /// except that OCI says `mount` and `network`).
    #[must_use]
    pub fn proc_name(self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::Mount => "mnt",
            Self::Uts => "uts",
            Self::Ipc => "ipc",
            Self::Network => "net",
            Self::User => "user",
            Self::Cgroup => "cgroup",
            Self::Time => "time",
        }
    }

    /// The `CLONE_NEW*` flag that creates it.
    ///
    /// `CLONE_NEWTIME` (0x80) only works with `clone3` and `unshare`: in the
    /// older `clone` call the low byte of the flags is the exit signal, so
    /// it would be misread there.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_flag(self) -> libc::c_int {
        match self {
            Self::Pid => libc::CLONE_NEWPID,
            Self::Mount => libc::CLONE_NEWNS,
            Self::Uts => libc::CLONE_NEWUTS,
            Self::Ipc => libc::CLONE_NEWIPC,
            Self::Network => libc::CLONE_NEWNET,
            Self::User => libc::CLONE_NEWUSER,
            Self::Cgroup => libc::CLONE_NEWCGROUP,
            Self::Time => libc::CLONE_NEWTIME,
        }
    }

    /// Its position in [`Namespace::ALL`], which is how a forked child names
    /// it in a failure report.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn index(self) -> u64 {
        // Every kind is in ALL; the fallback can't happen.
        Self::ALL
            .iter()
            .position(|&kind| kind == self)
            .and_then(|index| u64::try_from(index).ok())
            .unwrap_or(u64::MAX)
    }

    /// Whether it can be made. The time namespace isn't planned yet.
    fn is_supported(self) -> bool {
        matches!(
            self,
            Self::Pid
                | Self::Mount
                | Self::Uts
                | Self::Ipc
                | Self::Network
                | Self::User
                | Self::Cgroup
        )
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.proc_name())
    }
}

/// The longest hostname the kernel accepts (`HOST_NAME_MAX`).
pub const MAX_HOSTNAME_LEN: usize = 64;
/// The longest part between dots that RFC 1123 allows.
const MAX_LABEL_LEN: usize = 63;

/// How a command is isolated: which new namespaces it gets, and the hostname
/// inside them.
///
/// Start from [`Isolation::standard`] and change fields: it will gain more
/// (a domain name, then OCI's namespace paths), so it can't be written out
/// in full outside this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Isolation {
    /// The namespaces to create. Each kind at most once.
    pub namespaces: Vec<Namespace>,
    /// The hostname inside the container. Needs a UTS namespace. `None` keeps
    /// the host's (which the new UTS namespace starts with a copy of).
    pub hostname: Option<String>,
    /// The NIS domain name inside the container (`domainname`). Needs a UTS
    /// namespace. `None` keeps the host's.
    pub domainname: Option<String>,
    /// Kernel settings for the container (`sysctl(8)` names, such as
    /// `kernel.shmmax`), in order. Only the ones a container owns: see
    /// [`validate_sysctl`].
    pub sysctls: Vec<(String, String)>,
    /// A root filesystem of the container's own. Needs a mount namespace.
    /// `None` keeps the host's (with a fresh `/proc` over the host's).
    pub root: Option<Root>,
    /// What the command may do: its capabilities, and `no_new_privs`.
    pub security: crate::security::Security,
    /// The ID maps of its user namespace: present exactly when
    /// [`Namespace::User`] is in `namespaces`.
    pub id_maps: Option<IdMaps>,
    /// A `cgroup2` mount at `/sys/fs/cgroup`, showing the container's own
    /// cgroup (with a cgroup namespace). Made only when the container has a
    /// cgroup of its own: otherwise it would show shipyard's.
    pub cgroup_mount: Option<CgroupMount>,
    /// What the container may use: its cgroup's limits (none by default).
    pub resources: crate::resources::Resources,
    /// The bundle's hooks, with the container their state names (none by
    /// default). Run at create (`prestart`, `createRuntime`,
    /// `createContainer`, `startContainer`) by the spawn, and after it
    /// (`poststart`, `poststop`) by whoever runs the container.
    pub hooks: Option<crate::hooks::ContainerHooks>,
    /// Which devices it may use, and the nodes made in its `/dev` (Docker's
    /// defaults to begin with): enforced by its cgroup's device program.
    pub devices: crate::devices::Devices,
    /// An existing network namespace to join (a path to it, such as
    /// `/proc/PID/ns/net` or a bind mount of one) instead of making one.
    /// Needs [`Namespace::Network`] in `namespaces`. `None` makes a new one,
    /// if `Network` is listed.
    pub network_path: Option<PathBuf>,
    /// Network devices of the host moved into the container's network
    /// namespace (OCI's `linux.netDevices`). Needs [`Namespace::Network`].
    pub net_devices: Vec<NetDevice>,
}

/// A network device of the host, moved into the container's network
/// namespace when it starts. When the namespace goes, the kernel gives a
/// physical device back to the host's; a virtual one is destroyed with it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetDevice {
    /// Its name on the host.
    pub host: String,
    /// Its name in the container.
    pub name: String,
}

impl NetDevice {
    /// The host's device `host`, named `name` in the container.
    #[must_use]
    pub fn new(host: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            name: name.into(),
        }
    }
}

/// The container's view of its cgroup at `/sys/fs/cgroup` (always `nosuid`,
/// `nodev` and `noexec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CgroupMount {
    /// Read-only, as Docker mounts it for containers that aren't privileged:
    /// the container sees its usage and limits, and can't change them.
    pub read_only: bool,
}

impl CgroupMount {
    /// A read-only view.
    #[must_use]
    pub fn read_only() -> Self {
        Self { read_only: true }
    }

    /// The flags to mount it with.
    #[cfg(target_os = "linux")]
    pub(crate) fn mount_flags(self) -> nix::mount::MsFlags {
        use nix::mount::MsFlags;
        let restricted = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
        if self.read_only {
            restricted | MsFlags::MS_RDONLY
        } else {
            restricted
        }
    }
}

impl Isolation {
    /// The standard set: new PID, mount, UTS, IPC and cgroup namespaces, the
    /// host's hostname, Docker's confinement, and a read-only view of the
    /// container's cgroup.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            namespaces: vec![
                Namespace::Pid,
                Namespace::Mount,
                Namespace::Uts,
                Namespace::Ipc,
                Namespace::Cgroup,
            ],
            hostname: None,
            domainname: None,
            sysctls: Vec::new(),
            root: None,
            security: crate::security::Security::standard(),
            id_maps: None,
            cgroup_mount: Some(CgroupMount::read_only()),
            resources: crate::resources::Resources::default(),
            hooks: None,
            devices: crate::devices::Devices::docker_default(),
            network_path: None,
            net_devices: Vec::new(),
        }
    }

    /// The `CLONE_NEW*` flags for its namespaces, the ones made at the clone.
    /// Not the cgroup namespace: the clean image makes that one itself, once
    /// it's in its cgroup, so the namespace's root is the container's cgroup
    /// whichever way it got there (see `init.rs`). Nor a network namespace
    /// that's joined: the parent enters it for the clone (see `spawn.rs`).
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_flags(&self) -> libc::c_int {
        self.namespaces
            .iter()
            .filter(|&&namespace| namespace != Namespace::Cgroup)
            .filter(|&&namespace| namespace != Namespace::Network || self.makes_network_namespace())
            .fold(0, |flags, namespace| flags | namespace.clone_flag())
    }

    /// Whether the container may set its network namespace's settings: one
    /// made for it, or one it joins outside a user namespace of its own (in
    /// one, the joined namespace belongs to another user namespace, whose
    /// settings root inside can't write).
    fn owns_network_settings(&self) -> bool {
        self.makes_network_namespace()
            || (self.has(Namespace::Network) && !self.has(Namespace::User))
    }

    /// Whether it makes a network namespace (rather than joining one, or
    /// sharing the host's).
    #[must_use]
    pub fn makes_network_namespace(&self) -> bool {
        self.has(Namespace::Network) && self.network_path.is_none()
    }

    /// Whether it includes a new `namespace`.
    #[must_use]
    pub fn has(&self, namespace: Namespace) -> bool {
        self.namespaces.contains(&namespace)
    }

    /// Rejects what the kernel would refuse or what this stage can't do yet,
    /// before anything starts.
    ///
    /// # Errors
    ///
    /// A namespace given twice or not supported yet, a PID namespace without
    /// a mount namespace, a hostname without a UTS namespace, a hostname
    /// that isn't valid, or a root filesystem without a mount namespace or
    /// with paths that aren't absolute.
    pub fn validate(&self) -> Result<(), IsolationError> {
        self.check(self.root.is_some())?;
        if let Some(root) = &self.root {
            root.validate()?;
        }
        Ok(())
    }

    /// As [`validate`](Self::validate), for an isolation whose root
    /// filesystem comes later (an image's, made once it's pulled): every
    /// check but the root's own, as if it were there. So a mistake shows
    /// before the root costs anything to make.
    ///
    /// # Errors
    ///
    /// As [`validate`](Self::validate), the root's paths aside.
    pub fn validate_expecting_root(&self) -> Result<(), IsolationError> {
        self.check(true)
    }

    /// The network namespace's path and the devices moved into it.
    fn check_network(&self) -> Result<(), IsolationError> {
        if let Some(path) = &self.network_path {
            if !self.has(Namespace::Network) {
                return Err(IsolationError::NetworkPathNeedsNetwork);
            }
            if !is_plain_absolute(path) {
                return Err(IsolationError::InvalidNetworkPath(path.clone()));
            }
        }
        if self.net_devices.is_empty() {
            return Ok(());
        }
        if !self.has(Namespace::Network) {
            return Err(IsolationError::NetDevicesNeedNetwork);
        }
        let (mut names, mut hosts) = (Vec::new(), Vec::new());
        for device in &self.net_devices {
            let invalid = |reason: String| IsolationError::InvalidNetDevice {
                host: device.host.clone(),
                reason,
            };
            for name in [&device.host, &device.name] {
                shipyard_network::route::validate_name(name)
                    .map_err(|error| invalid(error.to_string()))?;
                if name == "lo" {
                    return Err(invalid("the loopback device can't be moved".to_owned()));
                }
            }
            if names.contains(&&device.name) {
                return Err(invalid(format!(
                    "another device is also named {} in the container",
                    device.name
                )));
            }
            if hosts.contains(&&device.host) {
                return Err(invalid("it's listed twice".to_owned()));
            }
            names.push(&device.name);
            hosts.push(&device.host);
        }
        Ok(())
    }

    /// Every check but the root's paths, with or without a root.
    fn check(&self, with_root: bool) -> Result<(), IsolationError> {
        check_kinds(&self.namespaces)?;
        match (&self.id_maps, self.has(Namespace::User)) {
            (Some(maps), true) => maps.validate()?,
            (None, false) => {}
            (Some(_), false) => return Err(IsolationError::MapsNeedUser),
            (None, true) => return Err(IsolationError::UserNeedsMaps),
        }
        // The container gets a fresh /proc for its PID namespace. Without its
        // own mount namespace that mount would replace the host's /proc, and
        // every process on the machine would see the container's PIDs.
        if self.has(Namespace::Pid) && !self.has(Namespace::Mount) {
            return Err(IsolationError::PidNeedsMount);
        }
        self.devices.validate()?;
        // Nodes are made in the container's own `/dev`: without a root
        // filesystem, `/dev` is the host's.
        if !self.devices.nodes.is_empty() && !with_root {
            return Err(IsolationError::DeviceNodesNeedRoot);
        }
        if let Some(container) = &self.hooks {
            container
                .hooks
                .check()
                .map_err(IsolationError::InvalidHook)?;
        }
        if let Some(hostname) = &self.hostname {
            if !self.has(Namespace::Uts) {
                return Err(IsolationError::HostnameNeedsUts);
            }
            validate_hostname(hostname)?;
        }
        if let Some(domainname) = &self.domainname {
            if !self.has(Namespace::Uts) {
                return Err(IsolationError::DomainnameNeedsUts);
            }
            validate_hostname(domainname).map_err(|err| match err {
                IsolationError::InvalidHostname { reason, .. } => {
                    IsolationError::InvalidDomainname {
                        domainname: domainname.clone(),
                        reason,
                    }
                }
                other => other,
            })?;
        }
        self.check_network()?;
        for (key, value) in &self.sysctls {
            validate_sysctl(
                key,
                value,
                self.has(Namespace::Ipc),
                self.owns_network_settings(),
            )?;
        }
        if with_root {
            // Without its own mount namespace, the pivot would move the
            // host's processes into the container's root.
            if !self.has(Namespace::Mount) {
                return Err(IsolationError::RootNeedsMount);
            }
            // A root filesystem gets a fresh /proc, and only root of the user
            // namespace that owns the PID namespace may mount one: in a user
            // namespace, the host's PID namespace isn't its own.
            if self.has(Namespace::User) && !self.has(Namespace::Pid) {
                return Err(IsolationError::UserRootNeedsPid);
            }
        }
        Ok(())
    }
}

/// The paths a container's `/proc` and `/sys` hide (Docker's defaults):
/// kernel memory, keys, timers and hardware details that would leak
/// information about the host or let the container disturb it.
pub const DEFAULT_MASKED_PATHS: [&str; 12] = [
    "/proc/acpi",
    "/proc/asound",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];

/// The paths a container's `/proc` keeps read-only (Docker's defaults): the
/// kernel settings and interfaces that are the host's, not the container's.
pub const DEFAULT_READONLY_PATHS: [&str; 5] = [
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// A root filesystem of the container's own: a directory on the host that
/// becomes the container's `/` (`pivot_root(2)`), with a fresh `/proc`, a
/// read-only `/sys` and a minimal `/dev` mounted in it.
///
/// Start from [`Root::new`], which fills in the default masked and
/// read-only paths. The shape follows the OCI runtime spec's `root`,
/// `linux.maskedPaths` and `linux.readonlyPaths`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Root {
    /// The directory on the host. Absolute.
    pub path: PathBuf,
    /// Make `/` read-only inside (`/dev`, `/proc` and the other mounts on it
    /// stay as they are).
    pub read_only: bool,
    /// Paths inside the container hidden from it: a file reads as empty, a
    /// directory as empty and read-only. Missing ones are skipped.
    pub masked_paths: Vec<PathBuf>,
    /// Paths inside the container made read-only. Missing ones are skipped.
    pub readonly_paths: Vec<PathBuf>,
    /// More mounts, in order.
    pub mounts: Vec<Mount>,
    /// Where the standard mounts go among [`mounts`](Self::mounts), as an
    /// OCI config lists them: each just before `mounts[index]` (or after
    /// them all, at their length), in this order. Those not listed come
    /// first, in [`StandardMount::ORDER`]; one below a listed one
    /// (`/dev/pts` below `/dev`) right after it. Empty: all first.
    pub standard_at: Vec<(StandardMount, usize)>,
    /// If set, the root is an overlay of an image's layers: mounted at
    /// [`path`](Self::path) *inside the container's namespaces*, where the
    /// kernel permits it (rootless overlay needs the container's user
    /// namespace); or, for a user namespace real root sets up (`--uidmap`),
    /// whose root can't reach the layers, made by real root and attached
    /// inside.
    pub overlay: Option<Overlay>,
}

/// An overlay to mount as the root filesystem: read-only lower (image) layers
/// stacked under a writable upper layer. The engine extracts the layers to
/// directories; see [`Root::overlay`] for who mounts them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Overlay {
    /// The read-only layer directories, base first (the init reverses them
    /// into overlay's top-first option order).
    pub lower: Vec<PathBuf>,
    /// The writable upper directory.
    pub upper: PathBuf,
    /// The overlay work directory.
    pub work: PathBuf,
}

impl Overlay {
    /// An overlay from its layer directories (base first), writable upper, and
    /// work directory.
    #[must_use]
    pub fn new(lower: Vec<PathBuf>, upper: PathBuf, work: PathBuf) -> Self {
        Self { lower, upper, work }
    }

    /// Rejects what overlay would refuse, or misread: no layer; a path that
    /// isn't plainly absolute, or holds a `,`, `:` or `\\` (which an old
    /// kernel's mount options separate or escape with); an upper and work
    /// directory on
    /// different parents (overlay wants them on one filesystem, and the
    /// engine makes them side by side).
    fn validate(&self) -> Result<(), IsolationError> {
        let invalid = |path: &Path, reason| IsolationError::InvalidOverlayPath {
            path: path.to_owned(),
            reason,
        };
        if self.lower.is_empty() {
            return Err(IsolationError::OverlayWithoutLayers);
        }
        for path in self.lower.iter().chain([&self.upper, &self.work]) {
            if !is_plain_absolute(path) {
                return Err(invalid(path, "it must be absolute, without . or .."));
            }
            if path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|b| matches!(b, b',' | b':' | b'\\'))
            {
                return Err(invalid(path, "it can't hold a `,`, `:` or `\\`"));
            }
        }
        if self.upper.parent() != self.work.parent() {
            return Err(invalid(&self.work, "it must be beside the upper directory"));
        }
        Ok(())
    }
}

/// A mount in the container: a host directory or file bound in, or a fresh
/// tmpfs. The shape follows the OCI runtime spec's `mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mount {
    /// Where it appears inside the container. Absolute, and not in `/proc`
    /// or `/sys` (the kernel's interfaces), nor `/dev` itself.
    pub destination: PathBuf,
    /// What's mounted.
    pub kind: MountKind,
    /// Restrictions on it.
    pub flags: Vec<MountFlag>,
    /// File-system options, such as a tmpfs's `size=64m`: comma-separated,
    /// as `mount(8)`'s `-o`.
    pub data: Option<String>,
}

/// What a [`Mount`] mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountKind {
    /// A host directory or file, seen at the destination too (`MS_BIND`).
    Bind {
        /// The host path. Absolute.
        source: PathBuf,
        /// With the mounts below it (`rbind`), rather than the source's own
        /// mount only.
        recursive: bool,
    },
    /// A fresh, empty file system in memory.
    Tmpfs,
}

/// A mount Shipyard makes in every container itself, with its own options
/// (`rootfs.rs`): a config that lists one only says where it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StandardMount {
    /// `/proc`.
    Proc,
    /// `/sys`, read-only.
    Sys,
    /// The container's own cgroup at `/sys/fs/cgroup`, where asked for.
    Cgroup,
    /// `/dev`, a tmpfs with the device nodes and links in it.
    Dev,
    /// `/dev/pts`, the container's own terminals.
    Devpts,
    /// `/dev/shm`.
    Shm,
    /// `/dev/mqueue`, with an `ipc` namespace of the container's own.
    Mqueue,
}

impl StandardMount {
    /// Their order when nothing says otherwise: each after the one it's
    /// mounted in.
    pub const ORDER: [Self; 7] = [
        Self::Proc,
        Self::Sys,
        Self::Cgroup,
        Self::Dev,
        Self::Devpts,
        Self::Shm,
        Self::Mqueue,
    ];

    /// Where it's mounted.
    #[must_use]
    pub fn destination(self) -> &'static str {
        match self {
            Self::Proc => "/proc",
            Self::Sys => "/sys",
            Self::Cgroup => "/sys/fs/cgroup",
            Self::Dev => "/dev",
            Self::Devpts => "/dev/pts",
            Self::Shm => "/dev/shm",
            Self::Mqueue => "/dev/mqueue",
        }
    }

    /// The standard mount it's made in, which must come first.
    #[must_use]
    pub fn parent(self) -> Option<Self> {
        match self {
            Self::Cgroup => Some(Self::Sys),
            Self::Devpts | Self::Shm | Self::Mqueue => Some(Self::Dev),
            Self::Proc | Self::Sys | Self::Dev => None,
        }
    }
}

/// One step of making a root's mounts, in order (see [`Root::mount_order`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MountStep {
    /// A standard mount.
    Standard(StandardMount),
    /// `mounts[index]`.
    Mount(usize),
}

/// A restriction on a [`Mount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountFlag {
    /// Nothing can be written (with `rbind`, the mounts below it too).
    ReadOnly,
    /// Set-user-ID and set-group-ID bits are ignored.
    NoSuid,
    /// Device nodes can't be opened.
    NoDev,
    /// Programs can't be run from it.
    NoExec,
}

impl Mount {
    /// The host path `source` bound at `destination`, recursively, with no
    /// restrictions.
    #[must_use]
    pub fn bind(source: impl Into<PathBuf>, destination: impl Into<PathBuf>) -> Self {
        Self {
            destination: destination.into(),
            kind: MountKind::Bind {
                source: source.into(),
                recursive: true,
            },
            flags: Vec::new(),
            data: None,
        }
    }

    /// A tmpfs at `destination`, with no restrictions.
    #[must_use]
    pub fn tmpfs(destination: impl Into<PathBuf>) -> Self {
        Self {
            destination: destination.into(),
            kind: MountKind::Tmpfs,
            flags: Vec::new(),
            data: None,
        }
    }

    /// Whether it has `flag`.
    #[must_use]
    pub fn has(&self, flag: MountFlag) -> bool {
        self.flags.contains(&flag)
    }

    /// Rejects a mount the container must not have: a destination that
    /// isn't plainly absolute, or is `/`, `/dev`, or in `/proc` or `/sys`; a
    /// bind source that isn't plainly absolute; options with a NUL byte.
    ///
    /// # Errors
    ///
    /// [`IsolationError::InvalidMount`], saying why.
    pub fn validate(&self) -> Result<(), IsolationError> {
        let invalid = |reason| IsolationError::InvalidMount {
            destination: self.destination.clone(),
            reason,
        };
        if !is_plain_absolute(&self.destination) {
            return Err(invalid("the destination must be absolute, without . or .."));
        }
        // The kernel's own interfaces: a mount there could hide or replace
        // what the container must not change (runc's /proc CVEs).
        if ["/proc", "/sys"]
            .iter()
            .any(|kernel| self.destination.starts_with(kernel))
            || self.destination == Path::new("/dev")
            || self.destination.parent().is_none()
        {
            return Err(invalid("it can't be /, /dev, or in /proc or /sys"));
        }
        if let MountKind::Bind { source, .. } = &self.kind
            && !is_plain_absolute(source)
        {
            return Err(invalid("the source must be absolute, without . or .."));
        }
        if self.data.as_ref().is_some_and(|data| data.contains('\0')) {
            return Err(invalid("its options contain a NUL byte"));
        }
        Ok(())
    }
}

impl Root {
    /// Every mount, in the order made: the standard ones not listed in
    /// [`standard_at`](Self::standard_at) first (or, below a listed one,
    /// right after it), then the listed ones and [`mounts`](Self::mounts) in
    /// their order.
    pub(crate) fn mount_order(&self) -> Vec<MountStep> {
        let listed = |kind: StandardMount| self.standard_at.iter().any(|&(at, _)| at == kind);
        let missing: Vec<StandardMount> = StandardMount::ORDER
            .into_iter()
            .filter(|&kind| !listed(kind))
            .collect();
        let mut steps: Vec<MountStep> = missing
            .iter()
            .filter(|kind| !kind.parent().is_some_and(listed))
            .map(|&kind| MountStep::Standard(kind))
            .collect();
        for index in 0..=self.mounts.len() {
            for &(kind, _) in self.standard_at.iter().filter(|&&(_, at)| at == index) {
                steps.push(MountStep::Standard(kind));
                steps.extend(
                    missing
                        .iter()
                        .filter(|child| child.parent() == Some(kind))
                        .map(|&child| MountStep::Standard(child)),
                );
            }
            if index < self.mounts.len() {
                steps.push(MountStep::Mount(index));
            }
        }
        steps
    }

    /// Rejects an order that can't work: a mount below a standard one made
    /// after it (`/dev`'s tmpfs, or `/dev/shm`'s, would hide it), a standard
    /// one before the one it's in (`/sys/fs/cgroup` before `/sys`), or one
    /// listed twice.
    ///
    /// # Errors
    ///
    /// [`IsolationError::MountOrder`], saying what to move.
    pub fn check_mount_order(&self) -> Result<(), IsolationError> {
        for (index, &(kind, at)) in self.standard_at.iter().enumerate() {
            if at > self.mounts.len() {
                return Err(IsolationError::MountOrder(format!(
                    "{} is placed after the last mount",
                    kind.destination()
                )));
            }
            if self
                .standard_at
                .iter()
                .take(index)
                .any(|&(other, _)| other == kind)
            {
                return Err(IsolationError::MountOrder(format!(
                    "{} is listed twice",
                    kind.destination()
                )));
            }
        }
        let mut made = Vec::new();
        for step in self.mount_order() {
            match step {
                MountStep::Standard(kind) => {
                    if let Some(parent) = kind.parent()
                        && !made.contains(&parent)
                    {
                        return Err(IsolationError::MountOrder(format!(
                            "{} is listed before {}, which it's mounted in: list it after",
                            kind.destination(),
                            parent.destination()
                        )));
                    }
                    made.push(kind);
                }
                MountStep::Mount(index) => {
                    let Some(mount) = self.mounts.get(index) else {
                        continue;
                    };
                    if let Some(over) = StandardMount::ORDER.into_iter().find(|standard| {
                        mount.destination.starts_with(standard.destination())
                            && !made.contains(standard)
                    }) {
                        return Err(IsolationError::MountOrder(format!(
                            "{} is listed before {}, which would hide it: list it after",
                            mount.destination.display(),
                            over.destination()
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// `path` as the root, writable, with the default masked and read-only
    /// paths ([`DEFAULT_MASKED_PATHS`], [`DEFAULT_READONLY_PATHS`]).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            read_only: false,
            masked_paths: DEFAULT_MASKED_PATHS.map(PathBuf::from).to_vec(),
            readonly_paths: DEFAULT_READONLY_PATHS.map(PathBuf::from).to_vec(),
            mounts: Vec::new(),
            standard_at: Vec::new(),
            overlay: None,
        }
    }

    /// Rejects paths that don't say exactly what they mean: relative ones,
    /// and ones with `.` or `..` in them.
    fn validate(&self) -> Result<(), IsolationError> {
        if !is_plain_absolute(&self.path) {
            return Err(IsolationError::InvalidRootPath(self.path.clone()));
        }
        // The host's own root can't be switched to: a bind stacked on `/` is
        // never reached by a lookup of `/`, so `pivot_root` would get the
        // current root (EBUSY). Sharing the host's files is "no root".
        if self.path.parent().is_none() {
            return Err(IsolationError::RootIsHostRoot);
        }
        let inside = self.masked_paths.iter().chain(&self.readonly_paths);
        if let Some(path) = inside.into_iter().find(|path| !is_plain_absolute(path)) {
            return Err(IsolationError::InvalidContainerPath(path.clone()));
        }
        if let Some(overlay) = &self.overlay {
            overlay.validate()?;
        }
        self.mounts.iter().try_for_each(Mount::validate)?;
        self.check_mount_order()
    }
}

/// Absolute, and made only of names: no `.` or `..` (which `Path` would keep
/// or resolve differently from the kernel once symlinks are involved), and
/// no NUL byte (which no system call can take).
fn is_plain_absolute(path: &Path) -> bool {
    !path.as_os_str().as_encoded_bytes().contains(&0)
        && path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && !path
            .as_os_str()
            .as_encoded_bytes()
            .split(|&b| b == b'/')
            .any(|part| part == b".")
}

/// Which running process's namespaces a command joins, which of them, and
/// how the command is confined there (`exec`).
///
/// The target is normally a container's init (PID 1 inside, seen from the
/// host). Start from [`Join::standard`]: like [`Isolation`], it will gain
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Join {
    /// The process whose namespaces to join, by its PID as shipyard sees it
    /// (in shipyard's own PID namespace; on a plain host, the host's PID).
    pub target: u32,
    /// The namespaces to join. Each kind at most once. A user namespace is
    /// joined first, the mount namespace last.
    pub namespaces: Vec<Namespace>,
    /// The target's start time (`/proc/PID/stat` field 22), if known: a
    /// process with its PID that started at another time isn't joined.
    pub start_time: Option<u64>,
    /// The command's capabilities, `no_new_privs` and seccomp filter: the
    /// container's (default: [`Security::standard`](crate::security::Security::standard),
    /// Docker's).
    pub security: crate::security::Security,
    /// The cgroup it's started in (`clone3`'s `CLONE_INTO_CGROUP`): the
    /// container's, if it has one (Linux).
    #[cfg(target_os = "linux")]
    pub cgroup: Option<crate::cgroup::Cgroup>,
    /// The cgroup's inode, as recorded when it was made: the directory
    /// opened for the clone must be that one (not one made in its place).
    #[cfg(target_os = "linux")]
    pub cgroup_inode: Option<u64>,
}

impl Join {
    /// Joins every namespace a [standard](Isolation::standard) container has,
    /// confined as Docker's defaults say, in no cgroup.
    #[must_use]
    pub fn standard(target: u32) -> Self {
        Self {
            target,
            namespaces: Isolation::standard().namespaces,
            start_time: None,
            security: crate::security::Security::standard(),
            #[cfg(target_os = "linux")]
            cgroup: None,
            #[cfg(target_os = "linux")]
            cgroup_inode: None,
        }
    }

    /// Whether it joins `namespace`.
    #[must_use]
    pub fn has(&self, namespace: Namespace) -> bool {
        self.namespaces.contains(&namespace)
    }

    /// Rejects what can't work, before anything starts.
    ///
    /// The mount namespace may be joined without the PID namespace (to work
    /// on the container's files): the process then sees the container's
    /// `/proc`, whose PIDs aren't its own, so `ps` and `/proc/self` mislead
    /// it.
    ///
    /// # Errors
    ///
    /// No namespaces, a namespace given twice or not supported yet, or a PID
    /// namespace without the mount namespace (whose `/proc` shows its PIDs).
    pub fn validate(&self) -> Result<(), IsolationError> {
        if self.namespaces.is_empty() {
            return Err(IsolationError::NothingToJoin);
        }
        check_kinds(&self.namespaces)?;
        // In the container's PID namespace but with the host's /proc, the
        // process would see the wrong processes: `ps`, `kill $(pidof …)` and
        // `/proc/self` would all mislead it.
        if self.has(Namespace::Pid) && !self.has(Namespace::Mount) {
            return Err(IsolationError::PidNeedsMount);
        }
        Ok(())
    }
}

/// Each namespace at most once, and only the kinds this stage supports.
fn check_kinds(namespaces: &[Namespace]) -> Result<(), IsolationError> {
    for (index, namespace) in namespaces.iter().enumerate() {
        if namespaces
            .get(..index)
            .is_some_and(|before| before.contains(namespace))
        {
            return Err(IsolationError::Duplicate(*namespace));
        }
        if !namespace.is_supported() {
            return Err(IsolationError::Unsupported(*namespace));
        }
    }
    Ok(())
}

/// The sysctls of a container's IPC namespace: the kernel keeps a copy of
/// each per namespace, so setting one changes only the container's.
pub const IPC_SYSCTLS: [&str; 8] = [
    "kernel.msgmax",
    "kernel.msgmnb",
    "kernel.msgmni",
    "kernel.sem",
    "kernel.shmall",
    "kernel.shmmax",
    "kernel.shmmni",
    "kernel.shm_rmid_forced",
];

/// Network settings that are the whole machine's even though a network
/// namespace made by root shows them, writable: conntrack's table size and
/// its helpers (`nf_conntrack_standalone.c` makes them read-only only in a
/// namespace a user namespace owns).
pub const HOST_WIDE_NET_SYSCTLS: [&str; 3] = [
    "net.netfilter.nf_conntrack_max",
    "net.netfilter.nf_conntrack_expect_max",
    "net.netfilter.nf_conntrack_helper",
];

/// Accepts only the sysctls a container owns: [`IPC_SYSCTLS`] and
/// `fs.mqueue.*`, with an IPC namespace (`has_ipc`), and `net.*`, with a
/// network namespace whose settings are the container's (`has_net`: one made
/// for it, or one it joins outside a user namespace of its own). Any other
/// one isn't namespaced, so setting it from the container would change the
/// host.
///
/// Which `net.*` settings a network namespace has is mostly the kernel's to
/// say: a setting that belongs to the whole machine isn't in a container's
/// `/proc/sys/net`, so setting it fails when the container starts. The
/// exceptions are [`HOST_WIDE_NET_SYSCTLS`]: a network namespace made by
/// root shows them, writable, though they're the whole machine's.
///
/// # Errors
///
/// [`IsolationError::Sysctl`], saying why.
pub fn validate_sysctl(
    key: &str,
    value: &str,
    has_ipc: bool,
    has_net: bool,
) -> Result<(), IsolationError> {
    let refuse = |reason| {
        Err(IsolationError::Sysctl {
            key: key.to_owned(),
            reason,
        })
    };
    let well_formed = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && key.split('.').all(|part| !part.is_empty());
    if !well_formed {
        return refuse("not a sysctl name (lowercase letters, digits, _ and dots)");
    }
    if value.contains(['\0', '\n']) {
        return refuse("its value contains a NUL or a newline");
    }
    if key.starts_with("net.") {
        if HOST_WIDE_NET_SYSCTLS.contains(&key) {
            return refuse("it's the whole machine's, even in a network namespace");
        }
        if has_net {
            return Ok(());
        }
        return refuse(
            "network settings need a network namespace of the container's own: without one, it \
             would change the host's",
        );
    }
    if key == "kernel.hostname" || key == "kernel.domainname" {
        return refuse("set the hostname or domain name with their own settings");
    }
    if !(IPC_SYSCTLS.contains(&key) || key.starts_with("fs.mqueue.")) {
        return refuse("it isn't namespaced: setting it would change the host");
    }
    if !has_ipc {
        return refuse("it needs an IPC namespace");
    }
    Ok(())
}

/// A hostname as RFC 1123 describes one: dot-separated labels of letters,
/// digits and hyphens, each 1–63 characters and not starting or ending with a
/// hyphen; at most [`MAX_HOSTNAME_LEN`] bytes in all.
///
/// Stricter than the kernel (which takes any bytes but NUL): a hostname that
/// other programs can't resolve or print only causes trouble later.
///
/// # Errors
///
/// [`IsolationError::InvalidHostname`], saying what's wrong.
pub fn validate_hostname(hostname: &str) -> Result<(), IsolationError> {
    let invalid = |reason: &'static str| IsolationError::InvalidHostname {
        hostname: hostname.to_owned(),
        reason,
    };
    if hostname.is_empty() {
        return Err(invalid("it is empty"));
    }
    if hostname.len() > MAX_HOSTNAME_LEN {
        return Err(invalid("it is longer than 64 bytes"));
    }
    for label in hostname.split('.') {
        if label.is_empty() {
            return Err(invalid(
                "it has an empty part (two dots, or a dot at an end)",
            ));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(invalid("a part between dots is longer than 63 characters"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(invalid(
                "only letters, digits, hyphens and dots are allowed",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("a part can't start or end with a hyphen"));
        }
    }
    Ok(())
}

/// Why an [`Isolation`] can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IsolationError {
    /// The same namespace is listed twice.
    #[error("the {0} namespace is listed more than once")]
    Duplicate(Namespace),
    /// A namespace kind a later stage adds.
    #[error("the {0} namespace isn't supported yet")]
    Unsupported(Namespace),
    /// A PID namespace without a mount namespace: a new one's `/proc` would
    /// be mounted over the host's; a joined one would be seen through the
    /// host's `/proc`.
    #[error("a PID namespace needs a mount namespace too (for its own /proc)")]
    PidNeedsMount,
    /// A root filesystem in a user namespace, without a PID namespace: its
    /// `/proc` can't be mounted (only root of the user namespace owning the
    /// PID namespace may mount one, and the host's isn't).
    #[error(
        "a root filesystem in a user namespace needs a PID namespace too: its /proc can only be \
         mounted by the root of a PID namespace's own user namespace"
    )]
    UserRootNeedsPid,
    /// A hostname was given, but no UTS namespace to set it in: it would
    /// change the host's hostname.
    #[error("setting the hostname needs a UTS namespace")]
    HostnameNeedsUts,
    /// A root filesystem without a mount namespace to switch it in.
    #[error("a root filesystem needs a mount namespace")]
    RootNeedsMount,
    /// The root filesystem is the host's own `/`.
    #[error(
        "the root filesystem can't be the host's own / (without a root filesystem, the \
         container shares the host's files)"
    )]
    RootIsHostRoot,
    /// The root filesystem's path isn't absolute, or has `.` or `..` in it.
    #[error("the root filesystem's path must be absolute, without . or .. (got {})", .0.display())]
    InvalidRootPath(PathBuf),
    /// An image's overlay has no layer.
    #[error("an image's overlay needs at least one layer")]
    OverlayWithoutLayers,
    /// A path of an image's overlay can't be used.
    #[error("the overlay path {}: {reason}", path.display())]
    InvalidOverlayPath {
        /// The path.
        path: PathBuf,
        /// Why.
        reason: &'static str,
    },
    /// A masked or read-only path isn't absolute, or has `.` or `..` in it.
    #[error("paths inside the container must be absolute, without . or .. (got {})", .0.display())]
    InvalidContainerPath(PathBuf),
    /// A [`Mount`] that can't be used.
    #[error("the mount at {}: {reason}", destination.display())]
    InvalidMount {
        /// Where it was to appear.
        destination: PathBuf,
        /// What's wrong with it.
        reason: &'static str,
    },
    /// Mounts in an order that can't work (see [`Root::check_mount_order`]).
    #[error("mounts: {0}")]
    MountOrder(String),
    /// A domain name was given, but no UTS namespace to set it in.
    #[error("setting the domain name needs a UTS namespace")]
    DomainnameNeedsUts,
    /// The domain name isn't valid (the same rules as a hostname's).
    #[error("invalid domain name \"{domainname}\": {reason}")]
    InvalidDomainname {
        /// The domain name given.
        domainname: String,
        /// What's wrong with it.
        reason: &'static str,
    },
    /// A network namespace path, but no network namespace listed.
    #[error("joining a network namespace needs the network namespace listed")]
    NetworkPathNeedsNetwork,
    /// A network namespace path that isn't absolute, or has `.` or `..`.
    #[error("the network namespace's path must be absolute, without . or .. (got {})", .0.display())]
    InvalidNetworkPath(PathBuf),
    /// Network devices to move, but no network namespace to move them into.
    #[error(
        "moving network devices into the container needs a network namespace of its own (without \
         one, they'd stay the host's)"
    )]
    NetDevicesNeedNetwork,
    /// A network device that can't be moved as asked.
    #[error("the network device {host}: {reason}")]
    InvalidNetDevice {
        /// Its name on the host.
        host: String,
        /// Why not.
        reason: String,
    },
    /// A sysctl the container can't set.
    #[error("sysctl {key}: {reason}")]
    Sysctl {
        /// Its name.
        key: String,
        /// Why not.
        reason: &'static str,
    },
    /// Device rules or nodes that can't be used.
    #[error(transparent)]
    Devices(#[from] crate::devices::DeviceError),
    /// Device nodes asked for without a root filesystem to make them in.
    #[error(
        "device nodes can only be made in a root filesystem of the container's own (without \
         one, /dev is the host's)"
    )]
    DeviceNodesNeedRoot,
    /// A hook that can't run as given (`hooks.PHASE[N]: why`).
    #[error("{0}")]
    InvalidHook(String),
    /// A [`Join`] with no namespaces to join.
    #[error("no namespaces to join")]
    NothingToJoin,
    /// A user namespace without ID maps.
    #[error("a user namespace needs ID maps (--uidmap)")]
    UserNeedsMaps,
    /// ID maps without a user namespace to apply them to.
    #[error("ID maps need a user namespace")]
    MapsNeedUser,
    /// ID maps the kernel would refuse.
    #[error(transparent)]
    IdMap(#[from] IdMapError),
    /// The hostname isn't valid.
    #[error("invalid hostname \"{hostname}\": {reason}")]
    InvalidHostname {
        /// The hostname given.
        hostname: String,
        /// What's wrong with it.
        reason: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root that comes later is checked as if it were there, but for its
    /// own paths.
    #[test]
    fn expecting_a_root_checks_all_but_the_roots_paths() {
        use crate::devices::{DeviceKind, DeviceNode};
        let mut isolation = Isolation::standard();
        isolation
            .devices
            .nodes
            .push(DeviceNode::new("/dev/fuse", DeviceKind::Char, 10, 229));
        assert_eq!(
            isolation.validate(),
            Err(IsolationError::DeviceNodesNeedRoot)
        );
        assert_eq!(isolation.validate_expecting_root(), Ok(()));
        isolation.namespaces = vec![Namespace::Uts];
        assert_eq!(
            isolation.validate_expecting_root(),
            Err(IsolationError::RootNeedsMount)
        );
        let mut hostname = Isolation::standard();
        hostname.hostname = Some("bad_name".to_owned());
        assert!(hostname.validate_expecting_root().is_err());
    }

    #[test]
    fn an_overlay_is_checked_before_it_reaches_the_kernel() {
        let good = || {
            Overlay::new(
                vec!["/s/layers/a/diff".into(), "/s/layers/b/diff".into()],
                "/s/containers/run-1/upper".into(),
                "/s/containers/run-1/work".into(),
            )
        };
        let check = |overlay: Overlay| {
            let mut root = Root::new("/s/containers/run-1/merged");
            root.overlay = Some(overlay);
            root.validate()
        };
        assert_eq!(check(good()), Ok(()));
        let mut none = good();
        none.lower.clear();
        assert_eq!(check(none), Err(IsolationError::OverlayWithoutLayers));
        for bad in [
            "/s:x/layers/a/diff",
            "/s,x/layers/a/diff",
            "s/layers/a",
            "/s/../a",
        ] {
            let mut overlay = good();
            overlay.lower[0] = bad.into();
            assert!(
                matches!(
                    check(overlay),
                    Err(IsolationError::InvalidOverlayPath { .. })
                ),
                "{bad}"
            );
        }
        for bad in ["/s/containers/run,1/upper", "/s/containers/run\\1/upper"] {
            let mut overlay = good();
            overlay.upper = bad.into();
            overlay.work = std::path::Path::new(bad).with_file_name("work");
            let checked = check(overlay);
            assert!(
                matches!(checked, Err(IsolationError::InvalidOverlayPath { .. })),
                "{bad}: {checked:?}"
            );
        }
        let mut apart = good();
        apart.work = "/elsewhere/work".into();
        assert!(matches!(
            check(apart),
            Err(IsolationError::InvalidOverlayPath { .. })
        ));
    }

    #[test]
    fn a_root_needs_a_mount_namespace_and_plain_absolute_paths() {
        let mut isolation = Isolation::standard();
        isolation.root = Some(Root::new("/srv/root"));
        assert_eq!(isolation.validate(), Ok(()));

        let mut host_root = isolation.clone();
        host_root.root = Some(Root::new("/"));
        assert_eq!(host_root.validate(), Err(IsolationError::RootIsHostRoot));

        let mut no_mount = isolation.clone();
        no_mount.namespaces = vec![Namespace::Uts];
        assert_eq!(no_mount.validate(), Err(IsolationError::RootNeedsMount));

        for bad in [
            "srv/root",
            "/srv/../etc",
            "/srv/./root",
            "./root",
            "",
            "/srv/\0root",
        ] {
            let mut relative = isolation.clone();
            relative.root = Some(Root::new(bad));
            assert_eq!(
                relative.validate(),
                Err(IsolationError::InvalidRootPath(bad.into())),
                "{bad:?}"
            );
        }
        for bad in ["proc/kcore", "/proc/../etc/shadow", "/proc/./kcore"] {
            let mut masked = isolation.clone();
            if let Some(root) = &mut masked.root {
                root.masked_paths.push(bad.into());
            }
            assert_eq!(
                masked.validate(),
                Err(IsolationError::InvalidContainerPath(bad.into())),
                "{bad:?}"
            );
            let mut readonly = isolation.clone();
            if let Some(root) = &mut readonly.root {
                root.readonly_paths.push(bad.into());
            }
            assert_eq!(
                readonly.validate(),
                Err(IsolationError::InvalidContainerPath(bad.into())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn only_the_containers_own_sysctls_are_allowed() {
        assert_eq!(
            validate_sysctl("kernel.shmmax", "1048576", true, false),
            Ok(())
        );
        assert_eq!(
            validate_sysctl("fs.mqueue.msg_max", "20", true, false),
            Ok(())
        );
        // With a network namespace of its own, network settings are its own.
        assert_eq!(
            validate_sysctl("net.ipv4.ip_forward", "1", false, true),
            Ok(())
        );
        assert_eq!(
            validate_sysctl("net.ipv4.ping_group_range", "0 2147483647", false, true),
            Ok(())
        );
        for (key, value, has_ipc, has_net) in [
            ("kernel.shmmax", "1", false, true),
            ("net.ipv4.ip_forward", "1", true, false),
            ("kernel.hostname", "x", true, true),
            ("vm.swappiness", "1", true, true),
            ("kernel..shmmax", "1", true, true),
            ("net..ipv4", "1", true, true),
            ("../../etc", "1", true, true),
            ("kernel.shmmax", "1\n2", true, true),
            ("net.ipv4.ip_forward", "1\n2", true, true),
            ("", "1", true, true),
        ] {
            assert!(
                matches!(
                    validate_sysctl(key, value, has_ipc, has_net),
                    Err(IsolationError::Sysctl { .. })
                ),
                "{key}={value:?} with IPC {has_ipc}, network {has_net}"
            );
        }
        let error = validate_sysctl("net.ipv4.ip_forward", "1", true, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("network namespace of the container's own"),
            "{error}"
        );
        // One a root-made namespace shows, but that's the whole machine's.
        assert!(validate_sysctl("net.netfilter.nf_conntrack_max", "1", true, true).is_err());
    }

    #[test]
    fn a_network_namespace_is_made_or_joined() {
        let mut isolation = Isolation::standard();
        isolation.namespaces.push(Namespace::Network);
        assert_eq!(isolation.validate(), Ok(()));
        assert!(isolation.makes_network_namespace());
        isolation.network_path = Some("/proc/42/ns/net".into());
        assert_eq!(isolation.validate(), Ok(()));
        assert!(!isolation.makes_network_namespace());
        isolation.network_path = Some("relative/net".into());
        assert_eq!(
            isolation.validate(),
            Err(IsolationError::InvalidNetworkPath("relative/net".into()))
        );
        // Joined, its settings are the container's, but not from a user
        // namespace of its own (the namespace belongs to another).
        isolation.network_path = Some("/proc/42/ns/net".into());
        isolation.sysctls = vec![("net.ipv4.ip_forward".into(), "1".into())];
        assert_eq!(isolation.validate(), Ok(()));
        isolation.namespaces.push(Namespace::User);
        isolation.id_maps = Some(IdMaps::single((1000, 1000)));
        let refused = isolation.validate();
        assert!(
            matches!(refused, Err(IsolationError::Sysctl { .. })),
            "{refused:?}"
        );
        let mut without = Isolation::standard();
        without.network_path = Some("/proc/42/ns/net".into());
        assert_eq!(
            without.validate(),
            Err(IsolationError::NetworkPathNeedsNetwork)
        );
        assert!(!without.makes_network_namespace());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_joined_network_namespace_isnt_made_at_the_clone() {
        let mut isolation = Isolation::standard();
        isolation.namespaces.push(Namespace::Network);
        assert_ne!(isolation.clone_flags() & libc::CLONE_NEWNET, 0);
        isolation.network_path = Some("/proc/42/ns/net".into());
        assert_eq!(isolation.clone_flags() & libc::CLONE_NEWNET, 0);
        assert_ne!(isolation.clone_flags() & libc::CLONE_NEWPID, 0);
    }

    #[test]
    fn network_devices_need_a_network_namespace_and_good_names() {
        let mut isolation = Isolation::standard();
        isolation.net_devices = vec![NetDevice::new("eth1", "eth0")];
        assert_eq!(
            isolation.validate(),
            Err(IsolationError::NetDevicesNeedNetwork)
        );
        isolation.namespaces.push(Namespace::Network);
        assert_eq!(isolation.validate(), Ok(()));
        for bad in [
            vec![NetDevice::new("lo", "lo0")],
            vec![NetDevice::new("eth1", "lo")],
            vec![NetDevice::new("eth1", "a-name-longer-than-15")],
            vec![NetDevice::new("eth/1", "eth0")],
            vec![
                NetDevice::new("eth1", "eth0"),
                NetDevice::new("eth2", "eth0"),
            ],
        ] {
            isolation.net_devices.clone_from(&bad);
            assert!(
                matches!(
                    isolation.validate(),
                    Err(IsolationError::InvalidNetDevice { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_domain_name_needs_a_uts_namespace() {
        let mut isolation = Isolation::standard();
        isolation.domainname = Some("example.com".into());
        assert_eq!(isolation.validate(), Ok(()));
        isolation.domainname = Some("bad_name".into());
        assert!(matches!(
            isolation.validate(),
            Err(IsolationError::InvalidDomainname { .. })
        ));
        isolation.domainname = Some("example.com".into());
        isolation.namespaces.retain(|&kind| kind != Namespace::Uts);
        assert_eq!(
            isolation.validate(),
            Err(IsolationError::DomainnameNeedsUts)
        );
    }

    #[test]
    fn mounts_are_checked() {
        let with = |mount: Mount| {
            let mut isolation = Isolation::standard();
            let mut root = Root::new("/r");
            root.mounts.push(mount);
            isolation.root = Some(root);
            isolation.validate()
        };
        assert_eq!(with(Mount::bind("/srv/data", "/data")), Ok(()));
        assert_eq!(with(Mount::tmpfs("/dev/cache")), Ok(()));
        for destination in [
            "/proc/x",
            "/proc",
            "/sys/kernel",
            "/dev",
            "/",
            "data",
            "/a/../b",
        ] {
            assert!(
                matches!(
                    with(Mount::tmpfs(destination)),
                    Err(IsolationError::InvalidMount { .. })
                ),
                "{destination}"
            );
        }
        assert!(matches!(
            with(Mount::bind("relative", "/data")),
            Err(IsolationError::InvalidMount { .. })
        ));
        let mut nul = Mount::tmpfs("/t");
        nul.data = Some("size=1\0m".into());
        assert!(matches!(
            with(nul),
            Err(IsolationError::InvalidMount { .. })
        ));
    }

    #[test]
    fn a_new_root_has_the_default_paths() {
        let root = Root::new("/r");
        assert!(!root.read_only);
        assert!(root.masked_paths.contains(&PathBuf::from("/proc/kcore")));
        assert!(root.readonly_paths.contains(&PathBuf::from("/proc/sys")));
    }

    #[test]
    fn the_standard_isolation_is_valid() {
        let standard = Isolation::standard();
        assert_eq!(standard.validate(), Ok(()));
        for namespace in [
            Namespace::Pid,
            Namespace::Mount,
            Namespace::Uts,
            Namespace::Ipc,
        ] {
            assert!(standard.has(namespace), "{namespace}");
        }
    }

    #[test]
    fn duplicates_and_later_stages_are_rejected() {
        let mut twice = Isolation::standard();
        twice.namespaces.push(Namespace::Pid);
        assert_eq!(
            twice.validate(),
            Err(IsolationError::Duplicate(Namespace::Pid))
        );

        let mut isolation = Isolation::standard();
        isolation.namespaces.push(Namespace::Time);
        assert_eq!(
            isolation.validate(),
            Err(IsolationError::Unsupported(Namespace::Time))
        );
    }

    #[test]
    fn a_pid_namespace_needs_a_mount_namespace() {
        let isolation = Isolation {
            namespaces: vec![Namespace::Pid, Namespace::Uts],
            hostname: None,
            domainname: None,
            sysctls: Vec::new(),
            root: None,
            security: crate::security::Security::standard(),
            id_maps: None,
            cgroup_mount: None,
            resources: crate::resources::Resources::default(),
            hooks: None,
            devices: crate::devices::Devices::docker_default(),
            network_path: None,
            net_devices: Vec::new(),
        };
        assert_eq!(isolation.validate(), Err(IsolationError::PidNeedsMount));
        // A mount namespace alone is fine: its /proc still shows the host's
        // processes, but nothing leaks.
        let mount_only = Isolation {
            namespaces: vec![Namespace::Mount],
            hostname: None,
            domainname: None,
            sysctls: Vec::new(),
            root: None,
            security: crate::security::Security::standard(),
            id_maps: None,
            cgroup_mount: None,
            resources: crate::resources::Resources::default(),
            hooks: None,
            devices: crate::devices::Devices::docker_default(),
            network_path: None,
            net_devices: Vec::new(),
        };
        assert_eq!(mount_only.validate(), Ok(()));
    }

    #[test]
    fn a_root_filesystem_in_a_user_namespace_needs_a_pid_namespace() {
        let mut isolation = Isolation::standard();
        isolation.namespaces = vec![Namespace::Mount, Namespace::User];
        isolation.id_maps = Some(crate::id_map::IdMaps::single((1000, 1000)));
        isolation.root = Some(Root::new(PathBuf::from("/srv/root")));
        assert_eq!(isolation.validate(), Err(IsolationError::UserRootNeedsPid));
        // Without a root filesystem, no /proc is mounted: the host's stays.
        isolation.root = None;
        assert_eq!(isolation.validate(), Ok(()));
        // As root, without a user namespace, the host's PID namespace is root's.
        isolation.namespaces = vec![Namespace::Mount];
        isolation.id_maps = None;
        isolation.root = Some(Root::new(PathBuf::from("/srv/root")));
        assert_eq!(isolation.validate(), Ok(()));
    }

    #[test]
    fn joins_are_checked_like_isolations() {
        assert_eq!(Join::standard(42).validate(), Ok(()));
        let join = |namespaces: Vec<Namespace>| Join {
            namespaces,
            ..Join::standard(42)
        };
        assert_eq!(
            join(Vec::new()).validate(),
            Err(IsolationError::NothingToJoin)
        );
        assert_eq!(
            join(vec![Namespace::Uts, Namespace::Uts]).validate(),
            Err(IsolationError::Duplicate(Namespace::Uts))
        );
        assert_eq!(join(vec![Namespace::Network]).validate(), Ok(()));
        assert_eq!(
            join(vec![Namespace::Time]).validate(),
            Err(IsolationError::Unsupported(Namespace::Time))
        );
        assert_eq!(
            join(vec![Namespace::Pid, Namespace::Uts]).validate(),
            Err(IsolationError::PidNeedsMount)
        );
        assert_eq!(join(vec![Namespace::Uts]).validate(), Ok(()));
        assert_eq!(
            join(vec![Namespace::User, Namespace::Uts]).validate(),
            Ok(())
        );
    }

    #[test]
    fn a_user_namespace_comes_with_its_maps() {
        let maps = IdMaps {
            uids: vec!["0:100000:65536".parse().unwrap()],
            gids: vec!["0:100000:65536".parse().unwrap()],
        };
        let mut isolation = Isolation::standard();
        isolation.namespaces.push(Namespace::User);
        assert_eq!(isolation.validate(), Err(IsolationError::UserNeedsMaps));
        isolation.id_maps = Some(maps.clone());
        assert_eq!(isolation.validate(), Ok(()));
        let mut no_root = maps.clone();
        no_root.uids = vec!["1:100000:65536".parse().unwrap()];
        isolation.id_maps = Some(no_root);
        assert!(matches!(
            isolation.validate(),
            Err(IsolationError::IdMap(IdMapError::NoRoot(MapKind::Uids)))
        ));
        let mut without_user = Isolation::standard();
        without_user.id_maps = Some(maps);
        assert_eq!(without_user.validate(), Err(IsolationError::MapsNeedUser));
    }

    #[test]
    fn hooks_are_checked() {
        let mut hooks = crate::hooks::Hooks::default();
        hooks.poststop.push(crate::hooks::Hook::new("relative"));
        let mut isolation = Isolation::standard();
        isolation.hooks = Some(crate::hooks::ContainerHooks::new(
            hooks,
            "c1",
            "/b",
            std::collections::BTreeMap::new(),
        ));
        assert_eq!(
            isolation.validate(),
            Err(IsolationError::InvalidHook(
                "hooks.poststop[0]: relative isn't an absolute path".into()
            ))
        );
    }

    #[test]
    fn a_hostname_needs_a_uts_namespace() {
        let isolation = Isolation {
            namespaces: vec![Namespace::Pid, Namespace::Mount],
            hostname: Some("box".into()),
            domainname: None,
            sysctls: Vec::new(),
            root: None,
            security: crate::security::Security::standard(),
            id_maps: None,
            cgroup_mount: None,
            resources: crate::resources::Resources::default(),
            hooks: None,
            devices: crate::devices::Devices::docker_default(),
            network_path: None,
            net_devices: Vec::new(),
        };
        assert_eq!(isolation.validate(), Err(IsolationError::HostnameNeedsUts));
    }

    #[test]
    fn good_hostnames() {
        for hostname in [
            "a",
            "box",
            "web-1",
            "db.internal",
            "A1",
            &"x".repeat(63),
            &format!("{}.{}", "a".repeat(31), "b".repeat(32)),
        ] {
            assert_eq!(validate_hostname(hostname), Ok(()), "{hostname}");
        }
    }

    #[test]
    fn bad_hostnames() {
        for hostname in [
            "",
            "-box",
            "box-",
            "a..b",
            ".box",
            "box.",
            "under_score",
            "spa ce",
            "nul\0byte",
            "émoji",
            &"x".repeat(64),
            &format!("{}.{}", "a".repeat(32), "b".repeat(32)),
        ] {
            assert!(
                matches!(
                    validate_hostname(hostname),
                    Err(IsolationError::InvalidHostname { .. })
                ),
                "{hostname:?} was accepted"
            );
        }
    }

    #[test]
    fn proc_names_are_unique_and_cover_every_kind() {
        let mut names: Vec<&str> = Namespace::ALL.iter().map(|n| n.proc_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Namespace::ALL.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_standard_flags() {
        assert_eq!(
            Isolation::standard().clone_flags(),
            libc::CLONE_NEWPID | libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clone_flags_are_distinct() {
        let flags: Vec<libc::c_int> = Namespace::ALL.iter().map(|n| n.clone_flag()).collect();
        for (i, a) in flags.iter().enumerate() {
            for b in &flags[i + 1..] {
                assert_eq!(a & b, 0, "{a:#x} and {b:#x} overlap");
            }
        }
    }

    mod mount_order {
        use super::*;

        fn root(standard_at: &[(StandardMount, usize)], mounts: &[&str]) -> Root {
            let mut root = Root::new("/r");
            root.mounts = mounts.iter().map(|&path| Mount::tmpfs(path)).collect();
            root.standard_at = standard_at.to_vec();
            root
        }

        use MountStep::{Mount as M, Standard as S};
        use StandardMount::{Cgroup, Dev, Devpts, Mqueue, Proc, Shm, Sys};

        #[test]
        fn nothing_listed_is_the_standard_order_first() {
            assert_eq!(
                root(&[], &["/mnt"]).mount_order(),
                [
                    S(Proc),
                    S(Sys),
                    S(Cgroup),
                    S(Dev),
                    S(Devpts),
                    S(Shm),
                    S(Mqueue),
                    M(0)
                ]
            );
        }

        #[test]
        fn a_configs_order_is_kept() {
            // runtime-tools' default config, with a bind after /dev/mqueue.
            let listed = [
                (Proc, 0),
                (Dev, 0),
                (Devpts, 0),
                (Shm, 0),
                (Mqueue, 0),
                (Sys, 1),
            ];
            let root = root(&listed, &["/mnt"]);
            assert_eq!(
                root.mount_order(),
                // The cgroup isn't listed: right after /sys.
                [
                    S(Proc),
                    S(Dev),
                    S(Devpts),
                    S(Shm),
                    S(Mqueue),
                    M(0),
                    S(Sys),
                    S(Cgroup)
                ]
            );
            assert!(root.check_mount_order().is_ok());
        }

        #[test]
        fn what_isnt_listed_follows_its_parent() {
            // Only /dev listed, late: its /dev/pts, /dev/shm and /dev/mqueue
            // must come after it, not first (its tmpfs would hide them).
            let root = root(&[(Dev, 1)], &["/a"]);
            assert_eq!(
                root.mount_order(),
                [
                    S(Proc),
                    S(Sys),
                    S(Cgroup),
                    M(0),
                    S(Dev),
                    S(Devpts),
                    S(Shm),
                    S(Mqueue)
                ]
            );
            assert!(root.check_mount_order().is_ok());
        }

        #[test]
        fn an_order_that_hides_a_mount_is_refused() {
            let hidden = root(&[(Dev, 1)], &["/dev/foo"]);
            assert!(matches!(
                hidden.check_mount_order(),
                Err(IsolationError::MountOrder(why)) if why.contains("/dev/foo is listed before /dev,")
            ));
            // Below a mount in /dev, listed before it (after /dev): hidden
            // by its own file system.
            let shm = root(&[(Dev, 0), (Shm, 1)], &["/dev/shm/x"]);
            assert!(matches!(
                shm.check_mount_order(),
                Err(IsolationError::MountOrder(why)) if why.contains("/dev/shm/x is listed before /dev/shm,")
            ));
            // Not one that only shares the start of its name.
            assert!(
                root(&[(Dev, 0), (Shm, 1)], &["/dev/shmem"])
                    .check_mount_order()
                    .is_ok()
            );
            let orphan = root(&[(Devpts, 0), (Dev, 0)], &[]);
            assert!(matches!(
                orphan.check_mount_order(),
                Err(IsolationError::MountOrder(why)) if why.contains("/dev/pts is listed before /dev")
            ));
            let cgroup = root(&[(Cgroup, 0), (Sys, 0)], &[]);
            assert!(cgroup.check_mount_order().is_err());
            let twice = root(&[(Proc, 0), (Proc, 0)], &[]);
            assert!(twice.check_mount_order().is_err());
            let beyond = root(&[(Proc, 2)], &[]);
            assert!(beyond.check_mount_order().is_err());
            // A mount below /dev after it is fine; so is one with /dev unlisted
            // (first).
            assert!(root(&[(Dev, 0)], &["/dev/foo"]).check_mount_order().is_ok());
            assert!(root(&[], &["/dev/foo"]).check_mount_order().is_ok());
            // And validation refuses it too.
            assert!(matches!(
                hidden.validate(),
                Err(IsolationError::MountOrder(_))
            ));
        }
    }
}
