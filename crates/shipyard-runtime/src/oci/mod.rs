//! OCI bundles: a directory with a `config.json` and a root filesystem, as
//! the OCI runtime spec describes a container (what `runc` and Podman read).
//!
//! [`load_bundle`] turns one into what Shipyard runs: a [`ProcessSpec`] and
//! an [`Isolation`]. Shipyard doesn't do everything the spec describes yet,
//! so the config's fields fall in three groups:
//!
//! - **supported**: the process, its capabilities, `noNewPrivileges` and
//!   seccomp profile, the root filesystem, the hostname and domain name, the
//!   PID, mount, UTS, IPC, network, user and cgroup namespaces (a network
//!   one joined by its `path` too), the host's network devices moved in
//!   (`netDevices`), masked and read-only paths, bind and tmpfs mounts, the
//!   cgroup mount, the IPC and network sysctls, and `linux.resources` and
//!   `cgroupsPath` (`resources.rs`), and the hooks (`hooks.rs`);
//! - **refused**: anything whose absence would leave the container *less
//!   confined than asked*: a namespace of another kind joined by its path,
//!   limits cgroup v2 can't apply, and so on. Running without them would be
//!   a silent security downgrade, so the bundle is refused, with every such
//!   field named at once;
//! - **ignored, with a warning**: what doesn't confine, and what can't
//!   matter here (a seccomp field from a newer spec, a mount's `atime`
//!   option, the cgroup mount without a cgroup namespace).
//!
//! Fields the spec doesn't define are ignored, as the spec requires.

mod resources;

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::devices::{DeviceKind, DeviceNode};
use crate::isolation::{
    CgroupMount, IdMap, IdMaps, Isolation, Mount, MountFlag, MountKind, Namespace, NetDevice, Root,
};
use crate::process::{Env, ProcessSpec, Rlimit, RlimitValue, User};
use crate::security::seccomp::Profile;
use crate::security::{Capabilities, Capability, CapabilitySet, CapabilitySets, Seccomp, Security};
use crate::small_file::{SmallFileError, read_small_file};

/// The largest `config.json` read. Real ones are a few kilobytes; a huge
/// one is a mistake or an attack on memory.
pub const MAX_CONFIG_SIZE: u64 = 4 * 1024 * 1024;

/// What a bundle runs, ready for [`Supervisor::supervise`](crate::process::Supervisor::supervise).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Bundle {
    /// The process (`config.json`'s `process`).
    pub spec: ProcessSpec,
    /// The namespaces, hostname and root filesystem.
    pub isolation: Isolation,
    /// What the config asks for that Shipyard ignores, to show the user.
    pub warnings: Vec<String>,
    /// The config's `annotations`: free-form keys and values for tools.
    pub annotations: std::collections::BTreeMap<String, String>,
    /// Its hooks, checked (absolute paths, timeouts above 0). They run only
    /// once given to the isolation with the container's ID
    /// ([`Isolation::hooks`]): their state names it.
    pub hooks: crate::hooks::Hooks,
    /// Where the container's cgroup goes, below the cgroup root
    /// (`linux.cgroupsPath`), if the config says: honored by
    /// `shipyard-runtime`; a `shipyard run`'s cgroup is always its own.
    pub cgroups_path: Option<PathBuf>,
}

/// Why a bundle can't be run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    /// `config.json` couldn't be read.
    #[error("reading {}: {error}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What went wrong. (Not the error's `source()`: the message
        /// already says it, and a cause would be printed twice.)
        error: std::io::Error,
    },
    /// `config.json` isn't a regular file (a FIFO would block the read).
    #[error("{} isn't a regular file", path.display())]
    NotAFile {
        /// The file.
        path: PathBuf,
    },
    /// `config.json` is larger than [`MAX_CONFIG_SIZE`].
    #[error("{} is larger than 4 MiB", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// Not valid JSON, or not the shape the spec gives.
    #[error("{}: line {line}, column {column}: {message}", path.display())]
    Json {
        /// The file.
        path: PathBuf,
        /// Where the problem is.
        line: usize,
        /// Where on the line.
        column: usize,
        /// What it is.
        message: String,
    },
    /// Valid JSON with a value that can't work.
    #[error("{}: {reason}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What's wrong.
        reason: String,
    },
    /// Asks for confinement Shipyard can't provide yet.
    #[error(
        "{} asks for what Shipyard can't do yet; running without it would leave the \
         container less confined than asked:\n  {}",
        path.display(),
        fields.join("\n  ")
    )]
    Unsupported {
        /// The file.
        path: PathBuf,
        /// Each field, with why.
        fields: Vec<String>,
    },
}

/// Reads the bundle in `dir`: its `config.json`, and the root filesystem
/// that names.
///
/// # Errors
///
/// A missing or malformed config, a value that can't work, or a field
/// whose absence would leave the container less confined than asked (see
/// [`BundleError`]).
pub fn load_bundle(dir: &Path) -> Result<Bundle, BundleError> {
    let path = dir.join("config.json");
    let config = read_config(&path)?;
    let mut reader = Reader {
        dir,
        path: &path,
        refused: Vec::new(),
        warnings: Vec::new(),
        cgroup_mount: None,
    };
    let bundle = reader.bundle(config)?;
    let Reader {
        refused, warnings, ..
    } = reader;
    if !refused.is_empty() {
        return Err(BundleError::Unsupported {
            path,
            fields: refused,
        });
    }
    Ok(Bundle { warnings, ..bundle })
}

/// [`load_bundle`], as this caller runs it: without root, rootless, in a
/// user namespace, added if the bundle has none (with a warning); without a
/// cgroup of its own (nothing delegated, no cgroup v2), no cgroup mount (with
/// a warning). Every runner loads bundles this way (`shipyard run --bundle`, and both
/// processes of `shipyard-runtime create`), so they run the same container.
///
/// # Errors
///
/// As [`load_bundle`].
#[cfg(target_os = "linux")]
pub fn load_bundle_for_caller(dir: &Path) -> Result<Bundle, BundleError> {
    let mut bundle = load_bundle(dir)?;
    let added = crate::process::bundle_user_namespace(&mut bundle.isolation);
    bundle.warnings.extend(added);
    if bundle.isolation.cgroup_mount.is_some()
        && let Err(why) = crate::cgroup::Tree::find()
    {
        bundle.isolation.cgroup_mount = None;
        bundle.warnings.push(format!(
            "the cgroup mount at /sys/fs/cgroup is left out: the container can't have a cgroup \
             of its own here: {why}"
        ));
    }
    Ok(bundle)
}

/// Reads and parses `config.json`, refusing what isn't a small regular file
/// (see `small_file.rs`).
fn read_config(path: &Path) -> Result<Config, BundleError> {
    let path_buf = || path.to_owned();
    let text = read_small_file(path, MAX_CONFIG_SIZE).map_err(|err| match err {
        SmallFileError::Io(error) => BundleError::Read {
            path: path_buf(),
            error,
        },
        SmallFileError::NotAFile => BundleError::NotAFile { path: path_buf() },
        SmallFileError::TooLarge => BundleError::TooLarge { path: path_buf() },
    })?;
    serde_json::from_slice(&text).map_err(|err| json_error(path, &err))
}

/// serde's error for the JSON file at `path`, without the position its
/// message ends with (the error gives it).
fn json_error(path: &Path, err: &serde_json::Error) -> BundleError {
    let message = err.to_string();
    let position = format!(" at line {} column {}", err.line(), err.column());
    BundleError::Json {
        path: path.to_owned(),
        line: err.line(),
        column: err.column(),
        message: message
            .strip_suffix(&position)
            .unwrap_or(&message)
            .to_owned(),
    }
}

/// A `linux.resources` object on its own (what `shipyard-runtime update
/// --resources` reads), checked as a bundle's: the same mapping, the same
/// refusals. `source` names it in errors.
///
/// # Errors
///
/// Not JSON, a value that can't work, or a field that can't be applied.
pub fn resources_from_json(
    text: &str,
    source: &Path,
) -> Result<crate::resources::Resources, BundleError> {
    let value: Value = serde_json::from_str(text).map_err(|err| json_error(source, &err))?;
    let mut reader = Reader {
        dir: source.parent().unwrap_or(Path::new(".")),
        path: source,
        refused: Vec::new(),
        warnings: Vec::new(),
        cgroup_mount: None,
    };
    let (resources, rules) = reader.resources(Some(&value))?;
    if !rules.is_empty() {
        reader.refuse(
            "linux.resources.devices",
            "update doesn't change device rules (runc's doesn't either)",
        );
    }
    if reader.refused.is_empty() {
        Ok(resources)
    } else {
        Err(BundleError::Unsupported {
            path: source.to_owned(),
            fields: reader.refused,
        })
    }
}

// --- The parts of the config Shipyard knows ----------------------------------
//
// Fields Shipyard refuses or ignores are `Option<Value>`: only their
// presence matters.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    oci_version: String,
    root: Option<RootConfig>,
    process: Option<ProcessConfig>,
    hostname: Option<String>,
    domainname: Option<String>,
    #[serde(default)]
    mounts: Vec<MountConfig>,
    #[serde(default)]
    hooks: crate::hooks::Hooks,
    linux: Option<LinuxConfig>,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RootConfig {
    path: PathBuf,
    #[serde(default)]
    readonly: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessConfig {
    #[serde(default)]
    terminal: bool,
    user: UserConfig,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    cwd: PathBuf,
    capabilities: Option<CapabilitiesConfig>,
    #[serde(default)]
    rlimits: Vec<RlimitConfig>,
    #[serde(default)]
    no_new_privileges: bool,
    apparmor_profile: Option<String>,
    selinux_label: Option<String>,
    oom_score_adj: Option<i64>,
    scheduler: Option<Value>,
    io_priority: Option<Value>,
    #[serde(rename = "execCPUAffinity")]
    exec_cpu_affinity: Option<Value>,
}

/// `process.capabilities`: the five sets, by name (`CAP_KILL`).
#[derive(Deserialize, Default)]
struct CapabilitiesConfig {
    #[serde(default)]
    bounding: Vec<String>,
    #[serde(default)]
    effective: Vec<String>,
    #[serde(default)]
    inheritable: Vec<String>,
    #[serde(default)]
    permitted: Vec<String>,
    #[serde(default)]
    ambient: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserConfig {
    uid: u32,
    gid: u32,
    umask: Option<u32>,
    #[serde(default)]
    additional_gids: Vec<u32>,
}

#[derive(Deserialize)]
struct RlimitConfig {
    #[serde(rename = "type")]
    kind: String,
    hard: u64,
    soft: u64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LinuxConfig {
    #[serde(default)]
    namespaces: Vec<NamespaceConfig>,
    masked_paths: Option<Vec<PathBuf>>,
    readonly_paths: Option<Vec<PathBuf>>,
    rootfs_propagation: Option<String>,
    #[serde(default)]
    uid_mappings: Vec<Value>,
    #[serde(default)]
    gid_mappings: Vec<Value>,
    seccomp: Option<Value>,
    resources: Option<Value>,
    #[serde(default)]
    devices: Vec<DeviceConfig>,
    mount_label: Option<String>,
    sysctl: Option<serde_json::Map<String, Value>>,
    cgroups_path: Option<String>,
    intel_rdt: Option<Value>,
    memory_policy: Option<Value>,
    personality: Option<Value>,
    /// The host's network devices to move in, by name.
    net_devices: Option<serde_json::Map<String, Value>>,
    time_offsets: Option<Value>,
}

/// One of `linux.devices`: a node to make in the container.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceConfig {
    path: PathBuf,
    /// `c` (or `u`, the same), `b`, or `p` (a FIFO).
    #[serde(rename = "type")]
    kind: String,
    /// Required for a device (not a FIFO).
    major: Option<i64>,
    minor: Option<i64>,
    file_mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
}

/// One entry of `linux.uidMappings` or `linux.gidMappings`.
#[derive(Deserialize)]
struct MappingConfig {
    #[serde(rename = "containerID")]
    container: u32,
    #[serde(rename = "hostID")]
    host: u32,
    size: u32,
}

impl From<MappingConfig> for IdMap {
    fn from(mapping: MappingConfig) -> Self {
        Self {
            container: mapping.container,
            host: mapping.host,
            size: mapping.size,
        }
    }
}

#[derive(Deserialize)]
struct NamespaceConfig {
    #[serde(rename = "type")]
    kind: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct MountConfig {
    destination: PathBuf,
    #[serde(rename = "type")]
    kind: Option<String>,
    source: Option<PathBuf>,
    #[serde(default)]
    options: Vec<String>,
}

/// The mounts every container gets from Shipyard (`rootfs.rs`): listed in
/// a config, they're accepted and Shipyard's own are used.
const STANDARD_MOUNTS: [(&str, &str); 6] = [
    ("/proc", "proc"),
    ("/sys", "sysfs"),
    ("/dev", "tmpfs"),
    ("/dev/pts", "devpts"),
    ("/dev/shm", "tmpfs"),
    ("/dev/mqueue", "mqueue"),
];

/// Whether a config value asks for nothing: null, or an empty list or
/// object, or an object of such values (`{"devices": []}`).
fn asks_nothing(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.values().all(asks_nothing),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// Mount options that set or clear a flag: (option, flag, set).
const FLAG_OPTIONS: [(&str, MountFlag, bool); 8] = [
    ("ro", MountFlag::ReadOnly, true),
    ("rw", MountFlag::ReadOnly, false),
    ("nosuid", MountFlag::NoSuid, true),
    ("suid", MountFlag::NoSuid, false),
    ("nodev", MountFlag::NoDev, true),
    ("dev", MountFlag::NoDev, false),
    ("noexec", MountFlag::NoExec, true),
    ("exec", MountFlag::NoExec, false),
];

/// Mount options about access times (`mount(8)`): ignored, with a warning.
const ATIME_OPTIONS: [&str; 10] = [
    "atime",
    "noatime",
    "diratime",
    "nodiratime",
    "relatime",
    "norelatime",
    "strictatime",
    "nostrictatime",
    "lazytime",
    "nolazytime",
];

/// tmpfs options passed on to the kernel (`tmpfs(5)`).
const TMPFS_OPTIONS: [&str; 6] = ["size", "mode", "nr_inodes", "nr_blocks", "uid", "gid"];

/// Turns a parsed config into a [`Bundle`], collecting what's refused and
/// what's ignored along the way.
struct Reader<'a> {
    dir: &'a Path,
    path: &'a Path,
    refused: Vec<String>,
    warnings: Vec<String>,
    /// The cgroup mount the config lists, if any (see `mounts`).
    cgroup_mount: Option<CgroupMount>,
}

impl Reader<'_> {
    fn invalid(&self, reason: impl Into<String>) -> BundleError {
        BundleError::Invalid {
            path: self.path.to_owned(),
            reason: reason.into(),
        }
    }

    fn refuse(&mut self, field: &str, why: &str) {
        self.refused.push(format!("{field}: {why}"));
    }

    /// `linux.devices` as nodes. A FIFO (`p`) is refused: Shipyard makes
    /// device nodes only.
    fn device_nodes(&mut self, devices: &[DeviceConfig]) -> Result<Vec<DeviceNode>, BundleError> {
        let mut nodes = Vec::new();
        for (index, device) in devices.iter().enumerate() {
            let field = format!("linux.devices[{index}]");
            let kind = match device.kind.as_str() {
                "c" | "u" => DeviceKind::Char,
                "b" => DeviceKind::Block,
                "p" => {
                    self.refuse(
                        &format!("linux.devices ({}, a FIFO)", device.path.display()),
                        "only device nodes are made",
                    );
                    continue;
                }
                other => {
                    return Err(self.invalid(format!("{field}: type {other:?} isn't c, u, b or p")));
                }
            };
            let number = |n: Option<i64>, name: &str| {
                let n =
                    n.ok_or_else(|| self.invalid(format!("{field}: a device needs its {name}")))?;
                u32::try_from(n)
                    .map_err(|_| self.invalid(format!("{field}: {name} {n} isn't a device number")))
            };
            let mut node = DeviceNode::new(
                device.path.clone(),
                kind,
                number(device.major, "major")?,
                number(device.minor, "minor")?,
            );
            node.mode = device.file_mode.unwrap_or(node.mode) & 0o7777;
            node.uid = device.uid.unwrap_or(0);
            node.gid = device.gid.unwrap_or(0);
            nodes.push(node);
        }
        Ok(nodes)
    }

    /// Refuses each of `fields` (name, value) that asks for anything (see
    /// [`asks_nothing`]): settings Shipyard can't apply yet.
    fn refuse_if_asked(&mut self, fields: &[(&str, &Option<Value>)]) {
        for (field, value) in fields {
            if value.as_ref().is_some_and(|value| !asks_nothing(value)) {
                self.refuse(field, "it comes in a later version");
            }
        }
    }

    fn warn(&mut self, field: &str, why: &str) {
        self.warnings.push(format!(
            "{}: {field} is ignored ({why})",
            self.path.display()
        ));
    }

    fn bundle(&mut self, config: Config) -> Result<Bundle, BundleError> {
        if !config.oci_version.starts_with("1.") {
            return Err(self.invalid(format!(
                "ociVersion {} isn't supported (only 1.x)",
                config.oci_version
            )));
        }
        let process = config
            .process
            .ok_or_else(|| self.invalid("there's no process to run"))?;
        let root_config = config
            .root
            .ok_or_else(|| self.invalid("there's no root filesystem (root.path)"))?;
        let annotations = config.annotations;
        let hooks = config.hooks;
        hooks.check().map_err(|why| self.invalid(why))?;
        let security = self.security(&process)?;
        let spec = self.process(process)?;
        let mut linux = config.linux.unwrap_or_default();
        let mut isolation = Isolation::standard();
        (isolation.namespaces, isolation.network_path) = self.namespaces(&linux.namespaces)?;
        isolation.id_maps = self.id_maps(&linux, isolation.has(Namespace::User))?;
        isolation.security = security;
        isolation.security.seccomp = self.seccomp(linux.seccomp.take())?;
        isolation.hostname = config.hostname.filter(|name| !name.is_empty());
        isolation.domainname = config.domainname.filter(|name| !name.is_empty());
        isolation.sysctls = self.sysctls(linux.sysctl.as_ref())?;
        isolation.net_devices = self.net_devices(linux.net_devices.as_ref())?;
        let mut root = Root::new(self.root_path(&root_config.path)?);
        root.read_only = root_config.readonly;
        // As the spec says: what the config lists, and nothing else.
        root.masked_paths = linux.masked_paths.clone().unwrap_or_default();
        root.readonly_paths = linux.readonly_paths.clone().unwrap_or_default();
        self.linux(&linux);
        root.mounts = self.mounts(config.mounts)?;
        isolation.root = Some(root);
        let (resources, rules) = self.resources(linux.resources.as_ref())?;
        isolation.resources = resources;
        // The bundle's rules, then runc's own list, as runc builds them.
        isolation.devices =
            crate::devices::Devices::bundle(rules, self.device_nodes(&linux.devices)?);
        let cgroups_path = self.cgroups_path(linux.cgroups_path.as_deref())?;
        // Only what the config lists: no cgroup mount unless it asks. And
        // only with a cgroup namespace: outside one, a `cgroup2` mount shows
        // the host's whole tree (runc binds the container's own cgroup
        // instead).
        isolation.cgroup_mount = self.cgroup_mount;
        if isolation.cgroup_mount.is_some() && !isolation.has(Namespace::Cgroup) {
            self.warn(
                "the cgroup mount at /sys/fs/cgroup",
                "it needs a cgroup namespace, or it would show the host's whole cgroup tree",
            );
            isolation.cgroup_mount = None;
        }
        // What's refused is reported first (by `load_bundle`): a container
        // missing what was refused may not validate, and the list is what
        // the user has to fix.
        if !self.refused.is_empty() {
            return Ok(Bundle {
                spec,
                isolation,
                warnings: Vec::new(),
                annotations,
                hooks,
                cgroups_path,
            });
        }
        isolation
            .validate()
            .map_err(|err| self.invalid(err.to_string()))?;
        spec.validate()
            .map_err(|err| self.invalid(err.to_string()))?;
        Ok(Bundle {
            spec,
            isolation,
            warnings: Vec::new(),
            annotations: annotations.clone(),
            hooks,
            cgroups_path,
        })
    }

    /// `process.capabilities`, exactly (an unknown name is an error), and
    /// `process.noNewPrivileges` (absent: false, as the spec says). Without
    /// capabilities the process gets none, with a warning: runc would leave
    /// it root's full set, which a forgotten field shouldn't give.
    fn security(&mut self, process: &ProcessConfig) -> Result<Security, BundleError> {
        let capabilities = if let Some(config) = &process.capabilities {
            let set = |field: &str, names: &[String]| {
                names
                    .iter()
                    .map(|name| {
                        name.parse::<Capability>().map_err(|err| {
                            self.invalid(format!("process.capabilities.{field}: {err}"))
                        })
                    })
                    .collect::<Result<CapabilitySet, _>>()
            };
            let sets = CapabilitySets {
                bounding: set("bounding", &config.bounding)?,
                effective: set("effective", &config.effective)?,
                permitted: set("permitted", &config.permitted)?,
                inheritable: set("inheritable", &config.inheritable)?,
                ambient: set("ambient", &config.ambient)?,
            };
            sets.check()
                .map_err(|why| self.invalid(format!("process.capabilities: {why}")))?;
            sets
        } else {
            self.warnings.push(format!(
                "{}: there's no process.capabilities, so the process gets no capabilities \
                 (runc would leave it all of root's)",
                self.path.display()
            ));
            CapabilitySets::default()
        };
        let mut security = Security::standard();
        security.capabilities = Capabilities::Exact(capabilities);
        security.no_new_privileges = process.no_new_privileges;
        Ok(security)
    }

    /// `linux.seccomp`: a profile, checked here so that a mistake shows
    /// before anything starts. Absent, the process is unfiltered (as the spec
    /// and runc have it).
    ///
    /// Fields Shipyard doesn't know are ignored, with a warning each, as the
    /// spec requires.
    fn seccomp(&mut self, config: Option<Value>) -> Result<Seccomp, BundleError> {
        let Some(config) = config else {
            return Ok(Seccomp::Unconfined);
        };
        let (profile, dropped) = Profile::from_oci(config)
            .map_err(|err| self.invalid(format!("linux.seccomp: {err}")))?;
        for field in dropped {
            self.warn(
                &format!("linux.seccomp.{field}"),
                "Shipyard doesn't know it",
            );
        }
        profile
            .validate()
            .map_err(|err| self.invalid(format!("linux.seccomp: {err}")))?;
        Ok(Seccomp::Profile(Box::new(profile)))
    }

    fn process(&mut self, process: ProcessConfig) -> Result<ProcessSpec, BundleError> {
        if process.terminal {
            self.refuse(
                "process.terminal",
                "a terminal inside comes in a later version",
            );
        }
        if process.apparmor_profile.is_some_and(|p| !p.is_empty()) {
            self.refuse("process.apparmorProfile", "AppArmor isn't supported");
        }
        if process.selinux_label.is_some_and(|l| !l.is_empty()) {
            self.refuse("process.selinuxLabel", "SELinux isn't supported");
        }
        self.refuse_if_asked(&[
            ("process.scheduler", &process.scheduler),
            ("process.ioPriority", &process.io_priority),
            ("process.execCPUAffinity", &process.exec_cpu_affinity),
        ]);
        if process.args.is_empty() {
            return Err(self.invalid("process.args is empty: there's no program to run"));
        }
        let mut spec = ProcessSpec::new(process.args.into_iter().map(Into::into).collect());
        let mut env = Env::new();
        for entry in &process.env {
            env.push_entry(entry.as_ref())
                .map_err(|err| self.invalid(format!("process.env: {err}")))?;
        }
        spec.env = env;
        spec.cwd = process.cwd;
        let mut user = User::new(process.user.uid, process.user.gid);
        user.umask = process.user.umask;
        user.additional_gids = process.user.additional_gids;
        spec.user = user;
        spec.rlimits = process
            .rlimits
            .iter()
            .map(|limit| {
                let soft = RlimitValue::from_oci(limit.soft)?;
                let hard = RlimitValue::from_oci(limit.hard)?;
                Rlimit::new(limit.kind.parse()?, soft, hard)
            })
            .collect::<Result<_, _>>()
            .map_err(|err| self.invalid(format!("process.rlimits: {err}")))?;
        // Checked with the rest of the spec (-1000 to 1000); a number too
        // large for that is out of range the same way.
        spec.oom_score_adj = process
            .oom_score_adj
            .map(|adj| i32::try_from(adj).unwrap_or(i32::MAX));
        Ok(spec)
    }

    /// The namespaces to create, and a network namespace to join (its
    /// `path`) instead of creating one. One the spec knows but Shipyard can't
    /// make yet, or a path to another kind, is refused; an unknown type is an
    /// error (the spec says so).
    fn namespaces(
        &mut self,
        listed: &[NamespaceConfig],
    ) -> Result<(Vec<Namespace>, Option<PathBuf>), BundleError> {
        let mut namespaces = Vec::new();
        let mut network_path = None;
        for namespace in listed {
            let path = namespace.path.as_ref().filter(|path| !path.is_empty());
            if let Some(path) = path {
                if namespace.kind != "network" {
                    self.refuse(
                        &format!("linux.namespaces ({}, path)", namespace.kind),
                        "joining an existing namespace of this kind comes in a later version",
                    );
                    continue;
                }
                network_path = Some(PathBuf::from(path));
            }
            match namespace.kind.as_str() {
                "pid" => namespaces.push(Namespace::Pid),
                "mount" => namespaces.push(Namespace::Mount),
                "uts" => namespaces.push(Namespace::Uts),
                "ipc" => namespaces.push(Namespace::Ipc),
                "network" => namespaces.push(Namespace::Network),
                "user" => namespaces.push(Namespace::User),
                "cgroup" => namespaces.push(Namespace::Cgroup),
                later @ "time" => self.refuse(
                    &format!("linux.namespaces ({later})"),
                    "this namespace comes in a later version",
                ),
                unknown => {
                    return Err(self.invalid(format!(
                        "linux.namespaces: unknown namespace type \"{unknown}\""
                    )));
                }
            }
        }
        Ok((namespaces, network_path))
    }

    /// `linux.netDevices`: `{"HOST NAME": {"name": "NAME INSIDE"}}`, the name
    /// inside being the host's when it's left out.
    fn net_devices(
        &self,
        devices: Option<&serde_json::Map<String, Value>>,
    ) -> Result<Vec<NetDevice>, BundleError> {
        let Some(devices) = devices else {
            return Ok(Vec::new());
        };
        devices
            .iter()
            .map(|(host, device)| {
                let invalid = || {
                    self.invalid(format!(
                        "linux.netDevices.{host}: must be {{\"name\": \"…\"}}"
                    ))
                };
                let device = device.as_object().ok_or_else(invalid)?;
                let name = match device.get("name") {
                    None => host.clone(),
                    Some(Value::String(name)) if !name.is_empty() => name.clone(),
                    Some(_) => return Err(invalid()),
                };
                Ok(NetDevice::new(host.clone(), name))
            })
            .collect()
    }

    /// `linux.uidMappings` and `linux.gidMappings`, which a `user` namespace
    /// needs and nothing else may have. Each entry is
    /// `{containerID, hostID, size}`; other fields are ignored, as the spec
    /// says. The maps are checked as the kernel would.
    fn id_maps(&mut self, linux: &LinuxConfig, user: bool) -> Result<Option<IdMaps>, BundleError> {
        let given = !linux.uid_mappings.is_empty() || !linux.gid_mappings.is_empty();
        if !user && !given {
            return Ok(None);
        }
        if !user {
            return Err(self.invalid(
                "linux.uidMappings/gidMappings need a user namespace in linux.namespaces",
            ));
        }
        if !given {
            return Err(
                self.invalid("a user namespace needs linux.uidMappings and linux.gidMappings")
            );
        }
        let maps = IdMaps {
            uids: self.mappings("linux.uidMappings", &linux.uid_mappings)?,
            gids: self.mappings("linux.gidMappings", &linux.gid_mappings)?,
        };
        maps.validate()
            .map_err(|err| self.invalid(format!("linux.uidMappings/gidMappings: {err}")))?;
        if maps.maps_host_root() {
            self.warnings.push(format!(
                "{}: linux.uidMappings/gidMappings map host ID 0: a user inside is root on the \
                 host's files, which a user namespace is meant to prevent",
                self.path.display()
            ));
        }
        Ok(Some(maps))
    }

    /// One of the two mapping lists, each entry read on its own so an error
    /// names it.
    fn mappings(&self, field: &str, entries: &[Value]) -> Result<Vec<IdMap>, BundleError> {
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                MappingConfig::deserialize(entry)
                    .map(IdMap::from)
                    .map_err(|err| self.invalid(format!("{field}[{index}]: {err}")))
            })
            .collect()
    }

    /// `root.path`: relative to the bundle, and made canonical.
    fn root_path(&self, path: &Path) -> Result<PathBuf, BundleError> {
        let path = self.dir.join(path);
        std::fs::canonicalize(&path)
            .map_err(|err| self.invalid(format!("root.path {}: {err}", path.display())))
    }

    /// The rest of `linux`: what's refused and what's ignored.
    fn linux(&mut self, linux: &LinuxConfig) {
        match linux.rootfs_propagation.as_deref() {
            None | Some("" | "slave" | "rslave" | "private" | "rprivate") => {}
            Some(unbindable @ ("unbindable" | "runbindable")) => self.refuse(
                "linux.rootfsPropagation",
                &format!("\"{unbindable}\" comes in a later version"),
            ),
            Some(other) => self.refuse(
                "linux.rootfsPropagation",
                &format!("\"{other}\" would let the container's mounts reach the host"),
            ),
        }
        if linux
            .mount_label
            .as_ref()
            .is_some_and(|label| !label.is_empty())
        {
            self.refuse("linux.mountLabel", "SELinux isn't supported");
        }
        self.refuse_if_asked(&[
            ("linux.intelRdt", &linux.intel_rdt),
            ("linux.memoryPolicy", &linux.memory_policy),
            ("linux.personality", &linux.personality),
            ("linux.timeOffsets", &linux.time_offsets),
        ]);
    }

    /// `linux.sysctl`: its values must be strings. Which keys are allowed is
    /// checked with the rest of the isolation (a disallowed one would change
    /// the host, so it's an error, never ignored).
    fn sysctls(
        &self,
        sysctl: Option<&serde_json::Map<String, Value>>,
    ) -> Result<Vec<(String, String)>, BundleError> {
        sysctl
            .into_iter()
            .flatten()
            .map(|(key, value)| match value {
                Value::String(value) => Ok((key.clone(), value.clone())),
                _ => Err(self.invalid(format!("linux.sysctl: the value of {key} isn't a string"))),
            })
            .collect()
    }

    /// The config's mounts: the standard ones are Shipyard's own, binds and
    /// tmpfs are the container's, anything else is refused.
    fn mounts(&mut self, listed: Vec<MountConfig>) -> Result<Vec<Mount>, BundleError> {
        let mut mounts = Vec::new();
        for entry in listed {
            let destination = entry.destination.to_string_lossy().into_owned();
            let kind = entry.kind.as_deref().unwrap_or("");
            if STANDARD_MOUNTS.contains(&(destination.as_str(), kind)) {
                continue;
            }
            if destination == "/sys/fs/cgroup" && ["cgroup", "cgroup2"].contains(&kind) {
                // The container's own cgroup (v2 only: `cgroup` means it
                // too), `nosuid`, `nodev` and `noexec` whatever the options
                // say. Mounted only if the container has a cgroup of its own.
                self.cgroup_mount = Some(CgroupMount {
                    read_only: entry.options.iter().any(|option| option == "ro"),
                });
                continue;
            }
            let is_bind = kind == "bind"
                || entry
                    .options
                    .iter()
                    .any(|option| option == "bind" || option == "rbind");
            let mut mount = if is_bind {
                let source = entry.source.as_deref().ok_or_else(|| {
                    self.invalid(format!("mounts: the bind at {destination} has no source"))
                })?;
                let source = self.dir.join(source);
                let source = std::fs::canonicalize(&source).map_err(|err| {
                    self.invalid(format!(
                        "mounts: the source of {destination}, {}: {err}",
                        source.display()
                    ))
                })?;
                let mut mount = Mount::bind(source, &entry.destination);
                // Only `rbind` brings the mounts below along (as with runc):
                // a bind asks for no more of the host than its source.
                if let MountKind::Bind { recursive, .. } = &mut mount.kind {
                    *recursive = false;
                }
                mount
            } else if kind == "tmpfs" {
                Mount::tmpfs(&entry.destination)
            } else {
                let why = if kind.is_empty() {
                    "it has no type (only bind and tmpfs are supported)".to_owned()
                } else {
                    format!("type \"{kind}\" isn't supported (only bind and tmpfs)")
                };
                self.refuse(&format!("mounts ({destination})"), &why);
                continue;
            };
            self.mount_options(&destination, &entry.options, &mut mount)?;
            mounts.push(mount);
        }
        Ok(mounts)
    }

    /// Applies a mount's OCI options (`mount(8)` names) to `mount`.
    fn mount_options(
        &mut self,
        destination: &str,
        options: &[String],
        mount: &mut Mount,
    ) -> Result<(), BundleError> {
        let is_tmpfs = mount.kind == MountKind::Tmpfs;
        let mut data = Vec::new();
        for option in options {
            if let Some(&(_, flag, on)) = FLAG_OPTIONS.iter().find(|(name, ..)| name == option) {
                mount.flags.retain(|&set| set != flag);
                if on {
                    mount.flags.push(flag);
                }
                continue;
            }
            match option.as_str() {
                "bind" | "rbind" => {
                    if let MountKind::Bind { recursive, .. } = &mut mount.kind {
                        *recursive = option == "rbind";
                    }
                }
                // The container's mounts never propagate to the host:
                // these ask for no more than that.
                "private" | "rprivate" | "slave" | "rslave" => {}
                "shared" | "rshared" => self.refuse(
                    &format!("mounts ({destination}, {option})"),
                    "the container's mounts would reach the host",
                ),
                "unbindable" | "runbindable" => self.refuse(
                    &format!("mounts ({destination}, {option})"),
                    "unbindable mounts come in a later version",
                ),
                // When files are accessed is no confinement: the mount keeps
                // the source's (a bind) or the kernel's default (a tmpfs).
                atime if ATIME_OPTIONS.contains(&atime) => self.warn(
                    &format!("the option {atime} of {destination}"),
                    "access-time options aren't applied yet",
                ),
                other => match other.split_once('=') {
                    // A comma in the value would smuggle in more options.
                    Some((key, value))
                        if is_tmpfs
                            && TMPFS_OPTIONS.contains(&key)
                            && !value.is_empty()
                            && !value.contains(',') =>
                    {
                        data.push(other.to_owned());
                    }
                    _ => {
                        return Err(self.invalid(format!(
                            "mounts: unknown option \"{other}\" for {destination}"
                        )));
                    }
                },
            }
        }
        if !data.is_empty() {
            mount.data = Some(data.join(","));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::json;

    use super::*;
    use crate::process::RlimitKind;
    use crate::resources::{CpuMax, IoMax, Limit};

    /// A bundle in a temporary directory: `rootfs/` and a `config.json`.
    struct TestBundle {
        dir: PathBuf,
    }

    impl TestBundle {
        fn new(config: &Value) -> Self {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "shipyard-bundle-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(dir.join("rootfs")).unwrap();
            std::fs::create_dir_all(dir.join("data")).unwrap();
            std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
            Self { dir }
        }

        fn load(&self) -> Result<Bundle, BundleError> {
            load_bundle(&self.dir)
        }
    }

    impl Drop for TestBundle {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The smallest config Shipyard runs, with `extra` merged in at the top.
    fn config(extra: &Value) -> Value {
        let mut config = json!({
            "ociVersion": "1.2.0",
            "root": {"path": "rootfs"},
            "process": {
                "user": {"uid": 0, "gid": 0},
                "args": ["sh"],
                "cwd": "/",
                "capabilities": {"bounding": ["CAP_KILL"]}
            },
            "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}]}
        });
        if let (Some(base), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        config
    }

    fn refused(config: &Value) -> Vec<String> {
        match TestBundle::new(config).load() {
            Err(BundleError::Unsupported { fields, .. }) => fields,
            other => panic!("not refused: {other:?}"),
        }
    }

    fn invalid(config: &Value) -> String {
        match TestBundle::new(config).load() {
            Err(err @ BundleError::Invalid { .. }) => err.to_string(),
            other => panic!("not invalid: {other:?}"),
        }
    }

    #[test]
    fn a_full_config_becomes_a_spec_and_an_isolation() {
        let bundle = TestBundle::new(&config(&json!({
            "hostname": "box",
            "root": {"path": "rootfs", "readonly": true},
            "process": {
                "user": {"uid": 1000, "gid": 100, "umask": 18, "additionalGids": [5, 6]},
                "args": ["sh", "-c", "true"],
                "env": ["A=1", "PATH=/bin"],
                "cwd": "/work",
                "capabilities": {"bounding": ["CAP_KILL"]},
                "rlimits": [
                    {"type": "RLIMIT_NOFILE", "hard": 2048, "soft": 1024},
                    {"type": "RLIMIT_CORE", "hard": u64::MAX, "soft": u64::MAX}
                ]
            },
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}, {"type": "uts"}, {"type": "ipc"}],
                "maskedPaths": ["/proc/kcore"],
                "readonlyPaths": ["/proc/sys"]
            }
        })));
        let loaded = bundle.load().unwrap();
        let spec = &loaded.spec;
        assert_eq!(spec.args, ["sh", "-c", "true"]);
        assert_eq!(spec.env.get("A".as_ref()), Some("1".as_ref()));
        assert_eq!(
            spec.env.len(),
            2,
            "nothing added: the config is the environment"
        );
        assert_eq!(spec.cwd, Path::new("/work"));
        assert_eq!(
            (spec.user.uid, spec.user.gid, spec.user.umask),
            (1000, 100, Some(18))
        );
        assert_eq!(spec.user.additional_gids, [5, 6]);
        let core = spec
            .rlimits
            .iter()
            .find(|l| l.kind == RlimitKind::Core)
            .unwrap();
        assert_eq!(
            (core.soft, core.hard),
            (RlimitValue::Unlimited, RlimitValue::Unlimited)
        );
        let isolation = &loaded.isolation;
        assert_eq!(isolation.hostname.as_deref(), Some("box"));
        assert_eq!(isolation.namespaces.len(), 4);
        let root = isolation.root.as_ref().unwrap();
        assert_eq!(
            root.path,
            std::fs::canonicalize(bundle.dir.join("rootfs")).unwrap()
        );
        assert!(root.read_only);
        assert_eq!(root.masked_paths, [PathBuf::from("/proc/kcore")]);
        assert_eq!(root.readonly_paths, [PathBuf::from("/proc/sys")]);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn a_user_namespace_and_its_mappings_are_the_configs() {
        let mut linux = config(&json!({}))["linux"].clone();
        linux["namespaces"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "user"}));
        linux["uidMappings"] = json!([
            {"containerID": 0, "hostID": 1000, "size": 1, "comment": "ignored"},
            {"containerID": 1, "hostID": 100_000, "size": 65536}
        ]);
        linux["gidMappings"] = json!([{"containerID": 0, "hostID": 1000, "size": 1}]);
        let loaded = TestBundle::new(&config(&json!({"linux": linux})))
            .load()
            .unwrap();
        assert!(loaded.isolation.has(Namespace::User));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let maps = loaded.isolation.id_maps.unwrap();
        assert_eq!(maps.uids.len(), 2);
        assert_eq!(maps.uids[1].host, 100_000);
        assert_eq!(maps.gids.len(), 1);
    }

    /// Mappings that include host root are allowed (as root), but warned
    /// about, as `--uidmap` is.
    #[test]
    fn mapping_host_root_warns() {
        let mut linux = config(&json!({}))["linux"].clone();
        linux["namespaces"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "user"}));
        let all = json!([{"containerID": 0, "hostID": 0, "size": 65536}]);
        linux["uidMappings"] = all.clone();
        linux["gidMappings"] = all;
        let loaded = TestBundle::new(&config(&json!({"linux": linux})))
            .load()
            .unwrap();
        assert!(
            loaded.warnings.iter().any(|w| w.contains("map host ID 0")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn user_namespaces_and_mappings_must_agree_and_be_valid() {
        let with = |namespaces: serde_json::Value, extra: serde_json::Value| {
            let mut linux = config(&json!({}))["linux"].clone();
            linux["namespaces"] = namespaces;
            for (key, value) in extra.as_object().unwrap() {
                linux[key] = value.clone();
            }
            TestBundle::new(&config(&json!({"linux": linux})))
                .load()
                .unwrap_err()
                .to_string()
        };
        let standard = json!([{"type": "pid"}, {"type": "mount"}]);
        let with_user = json!([{"type": "pid"}, {"type": "mount"}, {"type": "user"}]);
        let one = json!([{"containerID": 0, "hostID": 1000, "size": 1}]);
        for (namespaces, extra, wanted) in [
            (
                with_user.clone(),
                json!({}),
                "a user namespace needs linux.uidMappings and linux.gidMappings",
            ),
            (
                standard,
                json!({"uidMappings": one.clone(), "gidMappings": one.clone()}),
                "need a user namespace",
            ),
            (
                with_user.clone(),
                json!({"uidMappings": [{"containerID": 0, "hostID": -1, "size": 1}],
                       "gidMappings": one.clone()}),
                "linux.uidMappings[0]:",
            ),
            (
                with_user.clone(),
                json!({"uidMappings": [{"containerID": 1, "hostID": 1000, "size": 1}],
                       "gidMappings": one.clone()}),
                "linux.uidMappings/gidMappings: the uid_map must map ID 0",
            ),
            (
                json!([{"type": "pid"}, {"type": "mount"}, {"type": "user", "path": "/proc/1/ns/user"}]),
                json!({}),
                "linux.namespaces (user, path)",
            ),
        ] {
            let err = with(namespaces, extra);
            assert!(err.contains(wanted), "{wanted}: {err}");
        }
    }

    #[test]
    fn seccomp_is_the_configs_and_absent_means_none() {
        let absent = TestBundle::new(&config(&json!({}))).load().unwrap();
        assert_eq!(absent.isolation.security.seccomp, Seccomp::Unconfined);

        let mut linux = config(&json!({}))["linux"].clone();
        linux["seccomp"] = json!({
            "defaultAction": "SCMP_ACT_ERRNO",
            "syscalls": [{"names": ["read"], "action": "SCMP_ACT_ALLOW", "weight": 1}]
        });
        let loaded = TestBundle::new(&config(&json!({"linux": linux})))
            .load()
            .unwrap();
        let Seccomp::Profile(profile) = &loaded.isolation.security.seccomp else {
            panic!("{:?}", loaded.isolation.security.seccomp);
        };
        assert_eq!(profile.syscalls[0].names, ["read"]);
        // An unknown field is ignored, as the spec says, but named.
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(
            loaded.warnings[0].contains("linux.seccomp.syscalls[0].weight is ignored"),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn a_wrong_seccomp_profile_is_an_error() {
        for (seccomp, wanted) in [
            (
                json!({"defaultAction": "SCMP_ACT_ERRNO",
                       "syscalls": [{"names": ["raed"], "action": "SCMP_ACT_ALLOW"}]}),
                "linux.seccomp: syscalls[0]: \"raed\" isn't a system call",
            ),
            (
                json!({"defaultAction": "SCMP_ACT_NOTIFY"}),
                "linux.seccomp: SCMP_ACT_NOTIFY isn't supported",
            ),
            (
                json!({"syscalls": []}),
                "linux.seccomp: missing field `defaultAction`",
            ),
        ] {
            let mut linux = config(&json!({}))["linux"].clone();
            linux["seccomp"] = seccomp;
            let err = TestBundle::new(&config(&json!({"linux": linux})))
                .load()
                .unwrap_err()
                .to_string();
            assert!(err.contains(wanted), "{err}");
        }
    }

    #[test]
    fn capabilities_and_no_new_privileges_are_the_configs() {
        let mut process = config(&json!({}))["process"].clone();
        process["capabilities"] = json!({
            "bounding": ["CAP_KILL", "CAP_CHOWN"],
            "effective": ["CAP_KILL"],
            "permitted": ["CAP_KILL"],
            "ambient": []
        });
        process["noNewPrivileges"] = json!(true);
        let loaded = TestBundle::new(&config(&json!({"process": process})))
            .load()
            .unwrap();
        let security = loaded.isolation.security;
        assert!(security.no_new_privileges);
        let Capabilities::Exact(sets) = security.capabilities else {
            panic!("{security:?}");
        };
        let kill: Capability = "CAP_KILL".parse().unwrap();
        assert!(sets.bounding.contains(kill) && sets.bounding.contains("CHOWN".parse().unwrap()));
        assert_eq!(sets.effective.iter().collect::<Vec<_>>(), [kill]);
        assert!(sets.inheritable.is_empty() && sets.ambient.is_empty());
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        // Absent noNewPrivileges is false, as the spec says.
        assert!(
            !TestBundle::new(&config(&json!({})))
                .load()
                .unwrap()
                .isolation
                .security
                .no_new_privileges
        );
    }

    #[test]
    fn absent_capabilities_mean_none_with_a_warning() {
        let mut process = config(&json!({}))["process"].clone();
        process.as_object_mut().unwrap().remove("capabilities");
        let loaded = TestBundle::new(&config(&json!({"process": process})))
            .load()
            .unwrap();
        assert_eq!(
            loaded.isolation.security.capabilities,
            Capabilities::Exact(CapabilitySets::default())
        );
        let warnings = loaded.warnings.join("\n");
        assert!(
            warnings.contains("no capabilities") && warnings.contains("runc"),
            "{warnings}"
        );
    }

    #[test]
    fn capabilities_the_kernel_couldnt_apply_are_errors() {
        let mut process = config(&json!({}))["process"].clone();
        process["capabilities"] = json!({"bounding": ["CAP_KILL"], "ambient": ["CAP_KILL"]});
        let message = invalid(&config(&json!({"process": process})));
        assert!(
            message.contains("process.capabilities: ambient has CAP_KILL"),
            "{message}"
        );
    }

    #[test]
    fn an_unknown_capability_is_an_error() {
        let mut process = config(&json!({}))["process"].clone();
        process["capabilities"] = json!({"effective": ["CAP_FLY"]});
        let message = invalid(&config(&json!({"process": process})));
        assert!(
            message.contains("process.capabilities.effective") && message.contains("CAP_FLY"),
            "{message}"
        );
    }

    #[test]
    fn a_network_namespace_is_made_or_joined_by_path() {
        let made = TestBundle::new(&config(&json!({
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}, {"type": "network"}],
                "sysctl": {"net.ipv4.ip_forward": "1"}
            }
        })))
        .load()
        .unwrap();
        assert!(made.isolation.makes_network_namespace());
        assert_eq!(made.isolation.network_path, None);
        assert_eq!(
            made.isolation.sysctls,
            [("net.ipv4.ip_forward".to_owned(), "1".to_owned())]
        );
        let joined = TestBundle::new(&config(&json!({
            "linux": {
                "namespaces": [
                    {"type": "pid"}, {"type": "mount"},
                    {"type": "network", "path": "/var/run/netns/blue"}
                ]
            }
        })))
        .load()
        .unwrap();
        assert!(joined.isolation.has(Namespace::Network));
        assert_eq!(
            joined.isolation.network_path.as_deref(),
            Some(Path::new("/var/run/netns/blue"))
        );
        // A path to another kind stays refused.
        let fields = refused(&config(&json!({
            "linux": {"namespaces": [{"type": "mount"}, {"type": "ipc", "path": "/proc/1/ns/ipc"}]}
        })));
        assert!(
            fields
                .iter()
                .any(|field| field.starts_with("linux.namespaces (ipc, path)")),
            "{fields:#?}"
        );
    }

    #[test]
    fn network_devices_are_moved_in_by_name() {
        let loaded = TestBundle::new(&config(&json!({
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}, {"type": "network"}],
                "netDevices": {"eth1": {"name": "eth0"}, "veth9": {}}
            }
        })))
        .load()
        .unwrap();
        assert_eq!(
            loaded.isolation.net_devices,
            [
                NetDevice::new("eth1", "eth0"),
                NetDevice::new("veth9", "veth9")
            ]
        );
        for bad in [
            json!({"eth1": "eth0"}),
            json!({"eth1": {"name": 5}}),
            json!({"eth1": {"name": ""}}),
        ] {
            let mut config = config(&json!({}));
            config["linux"]["namespaces"] = json!([{"type": "mount"}, {"type": "network"}]);
            config["linux"]["netDevices"] = bad.clone();
            assert!(invalid(&config).contains("linux.netDevices.eth1"), "{bad}");
        }
        // Without a network namespace to move them into: an error.
        let mut config = config(&json!({}));
        config["linux"]["namespaces"] = json!([{"type": "mount"}]);
        config["linux"]["netDevices"] = json!({"eth1": {}});
        assert!(invalid(&config).contains("network namespace"));
    }

    #[test]
    fn the_domain_name_and_ipc_sysctls_are_applied() {
        let loaded = TestBundle::new(&config(&json!({
            "domainname": "example.com",
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}, {"type": "uts"}, {"type": "ipc"}],
                "sysctl": {"kernel.shmmax": "1048576", "fs.mqueue.msg_max": "20"}
            }
        })))
        .load()
        .unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.isolation.domainname.as_deref(), Some("example.com"));
        assert_eq!(
            loaded.isolation.sysctls,
            [
                ("fs.mqueue.msg_max".to_owned(), "20".to_owned()),
                ("kernel.shmmax".to_owned(), "1048576".to_owned())
            ]
        );
        // One that would change the host (no network namespace): an error,
        // never ignored.
        let mut host = config(&json!({}));
        host["linux"]["namespaces"] = json!([{"type": "mount"}, {"type": "ipc"}]);
        host["linux"]["sysctl"] = json!({"net.ipv4.ip_forward": "1"});
        assert!(invalid(&host).contains("net.ipv4.ip_forward"));
        let mut not_a_string = config(&json!({}));
        not_a_string["linux"]["namespaces"] = json!([{"type": "mount"}, {"type": "ipc"}]);
        not_a_string["linux"]["sysctl"] = json!({"kernel.shmmax": 5});
        assert!(invalid(&not_a_string).contains("isn't a string"));
    }

    #[test]
    fn absent_masked_paths_mean_none() {
        let loaded = TestBundle::new(&config(&json!({}))).load().unwrap();
        let root = loaded.isolation.root.unwrap();
        assert!(root.masked_paths.is_empty() && root.readonly_paths.is_empty());
    }

    #[test]
    fn what_would_confine_less_is_refused_all_at_once() {
        let fields = refused(&config(&json!({
            "process": {
                "terminal": true,
                "user": {"uid": 0, "gid": 0},
                "args": ["sh"],
                "cwd": "/",
                "capabilities": {"bounding": ["CAP_KILL"]},
                "noNewPrivileges": true,
                "apparmorProfile": "p",
                "selinuxLabel": "l",
                "scheduler": {"policy": "SCHED_FIFO", "priority": 1},
                "ioPriority": {"class": "IOPRIO_CLASS_IDLE"},
                "execCPUAffinity": {"initial": "0"}
            },
            "linux": {
                "namespaces": [
                    {"type": "pid"}, {"type": "mount"}, {"type": "time"},
                    {"type": "uts", "path": "/proc/1/ns/uts"}
                ],
                "seccomp": {"defaultAction": "SCMP_ACT_ERRNO"},
                "resources": {"hugepageLimits": [{"pageSize": "2MB", "limit": 1}]},
                "devices": [{"path": "/dev/pipe", "type": "p"}],
                "mountLabel": "m",
                "rootfsPropagation": "shared",
                "intelRdt": {"closID": "c"},
                "memoryPolicy": {"mode": "MPOL_BIND", "nodes": "0"},
                "personality": {"domain": "LINUX32"},
                "timeOffsets": {"monotonic": {"secs": 1}}
            },
            "mounts": [
                {"destination": "/data", "type": "bind", "source": "data", "options": ["rbind", "rshared"]},
                {"destination": "/o", "type": "overlay", "source": "overlay"}
            ]
        })));
        for expected in [
            "process.terminal",
            "process.apparmorProfile",
            "process.selinuxLabel",
            "process.scheduler",
            "process.ioPriority",
            "process.execCPUAffinity",
            "linux.intelRdt",
            "linux.memoryPolicy",
            "linux.personality",
            "linux.timeOffsets",
            "linux.namespaces (time)",
            "linux.namespaces (uts, path)",
            "linux.resources",
            "linux.devices",
            "linux.mountLabel",
            "linux.rootfsPropagation",
            "mounts (/data, rshared)",
            "mounts (/o)",
        ] {
            assert!(
                fields.iter().any(|field| field.starts_with(expected)),
                "{expected} not in {fields:#?}"
            );
        }
    }

    #[test]
    fn what_asks_for_nothing_passes_quietly() {
        let loaded = TestBundle::new(&config(&json!({
            "process": {
                "user": {"uid": 0, "gid": 0},
                "args": ["sh"],
                "cwd": "/",
                "oomScoreAdj": 100
            },
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}],
                "resources": {"devices": [], "memory": {}, "cpu": null},
                "cgroupsPath": "/machine/c1",
                "personality": {}
            },
            "annotations": {"anything": "goes"},
            "aFieldFromTheFuture": true
        })))
        .load()
        .unwrap();
        let warnings = loaded.warnings.join("\n");
        // linux.cgroupsPath isn't ignored: it's the runtime's (see below).
        assert_eq!(loaded.cgroups_path, Some(PathBuf::from("machine/c1")));
        assert_eq!(loaded.spec.oom_score_adj, Some(100));
        // Only the missing capabilities are worth a word.
        assert_eq!(loaded.warnings.len(), 1, "{warnings}");
        assert!(warnings.contains("process.capabilities"), "{warnings}");
    }

    #[test]
    fn devices_are_read_as_runc_builds_them() {
        use crate::devices::{Access, DeviceKind};
        let loaded = TestBundle::new(&config(&json!({
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}],
                "resources": {"devices": [
                    {"allow": false, "access": "rwm"},
                    {"allow": true, "type": "b", "major": 8, "minor": -1, "access": "r"}
                ]},
                "devices": [{"path": "/dev/fuse", "type": "c", "major": 10, "minor": 229,
                             "fileMode": 8630, "uid": 1, "gid": 2}]
            }
        })))
        .load()
        .unwrap();
        let devices = &loaded.isolation.devices;
        assert!(devices.asked);
        let node = &devices.nodes[0];
        // 8630 is 0o20666: the spec's fileMode carries the type bits too.
        assert_eq!((node.mode, node.uid, node.gid), (0o666, 1, 2));
        assert!(devices.allows(DeviceKind::Block, 8, 3, Access::READ));
        assert!(!devices.allows(DeviceKind::Block, 8, 3, Access::WRITE));
        assert!(
            devices.allows(DeviceKind::Char, 1, 3, Access::ALL),
            "Shipyard's /dev"
        );
        // Its node gets no rule of its own, as with runc: its rules say.
        assert!(!devices.allows(DeviceKind::Char, 10, 229, Access::READ));
        assert!(!devices.allows(DeviceKind::Char, 1, 11, Access::READ));
        // A bundle without either may use only runc's own list.
        let plain = TestBundle::new(&config(&json!({}))).load().unwrap();
        assert!(!plain.isolation.devices.asked);
        assert!(
            plain
                .isolation
                .devices
                .allows(DeviceKind::Block, 8, 0, Access::MKNOD),
            "mknod, as runc allows"
        );
    }

    #[test]
    fn devices_that_cant_be_used_are_named() {
        let with = |linux: Value| {
            let mut linux = linux;
            linux["namespaces"] = json!([{"type": "pid"}, {"type": "mount"}]);
            TestBundle::new(&config(&json!({"linux": linux})))
                .load()
                .unwrap_err()
                .to_string()
        };
        let err = with(json!({"devices": [{"path": "/dev/p", "type": "p"}]}));
        assert!(err.contains("linux.devices (/dev/p, a FIFO)"), "{err}");
        let err =
            with(json!({"devices": [{"path": "/dev/x", "type": "x", "major": 1, "minor": 1}]}));
        assert!(err.contains("linux.devices[0]: type \"x\""), "{err}");
        let err = with(json!({"devices": [{"path": "/dev/x", "type": "c", "major": 1}]}));
        assert!(
            err.contains("linux.devices[0]: a device needs its minor"),
            "{err}"
        );
        let err =
            with(json!({"devices": [{"path": "dev/x", "type": "c", "major": 1, "minor": 1}]}));
        assert!(err.contains("absolute"), "{err}");
        let err = with(json!({"resources": {"devices": [{"allow": true, "access": "rwx"}]}}));
        assert!(err.contains("linux.resources.devices[0]"), "{err}");
        let err = with(json!({"resources": {"devices": [{"allow": true, "major": -2}]}}));
        assert!(err.contains("major -2"), "{err}");
        // `update -r` changes limits, never device rules.
        let err = resources_from_json(
            r#"{"devices": [{"allow": true, "access": "r"}], "pids": {"limit": 5}}"#,
            Path::new("-"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("linux.resources.devices"), "{err}");
    }

    #[test]
    fn hooks_are_read_and_checked() {
        let with = |hooks: Value| TestBundle::new(&config(&json!({"hooks": hooks}))).load();
        let loaded = with(json!({
            "prestart": [{"path": "/usr/bin/gpu-hook", "args": ["gpu-hook", "prestart"]}],
            "createContainer": [{"path": "/bin/c", "env": ["A=1"], "timeout": 10}],
            "poststop": [{"path": "/bin/cleanup"}],
            "aPhaseFromTheFuture": [{"path": "/bin/x"}]
        }))
        .unwrap();
        assert_eq!(loaded.hooks.prestart[0].args, ["gpu-hook", "prestart"]);
        assert_eq!(loaded.hooks.create_container[0].timeout, Some(10));
        assert_eq!(loaded.hooks.poststop.len(), 1);
        assert!(
            loaded.warnings.iter().all(|w| !w.contains("hooks")),
            "{:?}",
            loaded.warnings
        );
        // Given to the isolation only by whoever names the container.
        assert!(loaded.isolation.hooks.is_none());
        for (hooks, expected) in [
            (json!({"poststart": [{"path": "hook"}]}), "absolute"),
            (
                json!({"poststart": [{"path": "/h", "timeout": 0}]}),
                "above 0",
            ),
            (
                json!({"poststart": [{"path": "/h", "env": ["X"]}]}),
                "KEY=VALUE",
            ),
            (
                json!({"poststart": [{"path": "/h", "timeout": -1}]}),
                "invalid value",
            ),
            (
                json!({"poststart": [{"args": ["x"]}]}),
                "missing field `path`",
            ),
        ] {
            let err = with(hooks).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected}: {err}");
        }
    }

    #[test]
    fn the_cgroup_namespace_and_mount_are_the_configs() {
        let with = |mounts: Value| {
            TestBundle::new(&config(&json!({
                "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}, {"type": "cgroup"}]},
                "mounts": mounts
            })))
            .load()
            .unwrap()
        };
        let loaded = with(json!([
            {"destination": "/sys/fs/cgroup", "type": "cgroup", "source": "cgroup",
             "options": ["nosuid", "noexec", "nodev", "relatime", "ro"]}
        ]));
        assert!(loaded.isolation.namespaces.contains(&Namespace::Cgroup));
        assert_eq!(
            loaded.isolation.cgroup_mount,
            Some(CgroupMount { read_only: true })
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let writable = with(json!([
            {"destination": "/sys/fs/cgroup", "type": "cgroup2", "source": "cgroup2"}
        ]));
        assert_eq!(
            writable.isolation.cgroup_mount,
            Some(CgroupMount { read_only: false })
        );
        // Not listed, not mounted (`shipyard run`'s standard has one).
        assert_eq!(with(json!([])).isolation.cgroup_mount, None);
        // Listed without a cgroup namespace: left out, as it would show the
        // host's whole tree.
        let outside = TestBundle::new(&config(&json!({
            "mounts": [{"destination": "/sys/fs/cgroup", "type": "cgroup2", "source": "cgroup2"}]
        })))
        .load()
        .unwrap();
        assert_eq!(outside.isolation.cgroup_mount, None);
        assert!(
            outside
                .warnings
                .join("\n")
                .contains("needs a cgroup namespace"),
            "{:?}",
            outside.warnings
        );
        // Joining one is still refused.
        let fields = refused(&config(&json!({
            "linux": {"namespaces": [{"type": "pid"}, {"type": "cgroup", "path": "/proc/1/ns/cgroup"}]}
        })));
        assert_eq!(fields.len(), 1);
        assert!(
            fields[0].starts_with("linux.namespaces (cgroup, path)"),
            "{fields:?}"
        );
    }

    #[test]
    fn linux_resources_become_cgroup_limits_as_runc_reads_them() {
        let loaded = TestBundle::new(&config(&json!({
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}],
                "resources": {
                    "devices": [],
                    "memory": {"limit": 536_870_912, "reservation": 268_435_456,
                               "swap": 805_306_368, "disableOOMKiller": false},
                    "cpu": {"shares": 512, "quota": 50000, "period": 100_000,
                            "cpus": "0-1", "mems": "", "burst": 0},
                    "pids": {"limit": 64},
                    "blockIO": {
                        "weight": 500,
                        "throttleReadBpsDevice": [{"major": 8, "minor": 0, "rate": 1_048_576}],
                        "throttleWriteIOPSDevice": [
                            {"major": 8, "minor": 0, "rate": 100},
                            {"major": 8, "minor": 16, "rate": 0}
                        ]
                    },
                    "unified": {"memory.high": "400M"}
                }
            }
        })))
        .load()
        .unwrap();
        let limits = &loaded.isolation.resources;
        assert_eq!(limits.memory_max, Some(512 << 20));
        assert_eq!(limits.memory_low, Some(256 << 20));
        // Memory plus swap, as the spec says: 768 MiB total is 256 MiB swap.
        assert_eq!(limits.swap_max, Some(Limit::Value(256 << 20)));
        assert!(!limits.swap_is_default);
        assert_eq!(limits.cpu_weight, crate::resources::weight_from_shares(512));
        assert_eq!(
            limits.cpu_max,
            Some(CpuMax {
                quota: Some(50_000),
                period: 100_000
            })
        );
        assert_eq!(limits.cpuset_cpus.as_deref(), Some("0-1"));
        assert_eq!(limits.cpuset_mems, None, "an empty list is no list");
        assert_eq!(limits.pids_max, Some(64));
        // Both of 8:0's limits on one line; a rate of 0 is no limit.
        let mut io = IoMax::device(8, 0);
        (io.rbps, io.wiops) = (Some(1_048_576), Some(100));
        assert_eq!(limits.io_max, [io]);
        assert_eq!(
            limits.unified.get("memory.high").map(String::as_str),
            Some("400M")
        );
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

        // -1 and 0 are "no limit" and "not set", as runc reads them.
        let unset = TestBundle::new(&config(&json!({
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}],
                "resources": {"memory": {"limit": -1, "swap": -1}, "cpu": {"quota": -1},
                              "pids": {"limit": -1}}
            }
        })))
        .load()
        .unwrap();
        let limits = &unset.isolation.resources;
        assert_eq!(
            (
                limits.memory_max,
                limits.swap_max,
                limits.cpu_max,
                limits.pids_max
            ),
            (None, Some(Limit::Max), None, None)
        );

        // What engines write for "the default" asks for nothing.
        for memory in [
            json!({"kernel": -1, "kernelTCP": -1}),
            json!({"swappiness": -1}),
            json!({"swappiness": u64::MAX}),
            json!({"swappiness": null}),
        ] {
            let loaded = TestBundle::new(&config(&json!({
                "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}],
                          "resources": {"memory": memory}}
            })))
            .load();
            assert!(loaded.is_ok(), "{memory}: {loaded:?}");
        }
    }

    #[test]
    fn limits_cgroup_v2_cant_apply_are_refused() {
        let fields = refused(&config(&json!({
            "linux": {
                "namespaces": [{"type": "pid"}, {"type": "mount"}],
                "resources": {
                    "memory": {"kernel": 1_048_576, "kernelTCP": 0, "swappiness": 60,
                               "disableOOMKiller": true},
                    "cpu": {"realtimeRuntime": 950_000, "burst": 1000, "idle": 1},
                    "blockIO": {"leafWeight": 300, "weightDevice": [{"major": 8, "minor": 0, "weight": 50}]},
                    "hugepageLimits": [{"pageSize": "2MB", "limit": 1}],
                    "network": {"classID": 1},
                    "rdma": {"mlx5_1": {"hcaHandles": 3}}
                }
            }
        })));
        for expected in [
            "linux.resources.memory.kernel:",
            "linux.resources.memory.swappiness",
            "linux.resources.memory.disableOOMKiller",
            "linux.resources.cpu.realtimeRuntime",
            "linux.resources.cpu.burst",
            "linux.resources.cpu.idle",
            "linux.resources.blockIO.leafWeight",
            "linux.resources.blockIO.weightDevice",
            "linux.resources.hugepageLimits",
            "linux.resources.network",
            "linux.resources.rdma",
        ] {
            assert!(
                fields.iter().any(|field| field.starts_with(expected)),
                "{expected} not in {fields:#?}"
            );
        }
        assert!(
            !fields.iter().any(|field| field.contains("kernelTCP")),
            "0 asks for nothing: {fields:#?}"
        );
        // Values the kernel wouldn't take are errors, naming the field.
        for (resources, expected) in [
            (json!({"memory": {"limit": 1024}}), "at least"),
            (
                json!({"memory": {"limit": -5}}),
                "linux.resources.memory.limit",
            ),
            (
                json!({"memory": {"limit": 536_870_912, "swap": 1024}}),
                "can't be less",
            ),
            (json!({"cpu": {"cpus": "3-1"}}), "isn't a list"),
            (json!({"blockIO": {"weight": 5}}), "between 10 and 1000"),
            (json!({"unified": {"cgroup.procs": "1"}}), "cgroup.procs"),
            (json!({"pids": {"limit": "lots"}}), "linux.resources"),
        ] {
            let err = invalid(&config(&json!({
                "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}], "resources": resources}
            })));
            assert!(err.contains(expected), "{expected}: {err}");
        }
    }

    #[test]
    fn a_cgroups_path_is_a_place_below_the_root() {
        let path = |cgroups_path: &str| {
            TestBundle::new(&config(&json!({
                "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}],
                          "cgroupsPath": cgroups_path}
            })))
            .load()
            .unwrap()
            .cgroups_path
        };
        assert_eq!(path("/default/web-1"), Some(PathBuf::from("default/web-1")));
        // Relative: below Shipyard's own place for runtime containers.
        assert_eq!(
            path("pods/web-1"),
            Some(PathBuf::from("shipyard/runtime/pods/web-1"))
        );
        assert_eq!(path(""), None);
        for bad in ["/", "/a/../b", "../escape", "./here", "/shipyard/run/4321"] {
            let err = invalid(&config(&json!({
                "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}], "cgroupsPath": bad}
            })));
            assert!(err.contains("linux.cgroupsPath"), "{bad}: {err}");
        }
        let fields = refused(&config(&json!({
            "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}],
                      "cgroupsPath": "system.slice:docker:web-1"}
        })));
        assert!(fields[0].contains("systemd"), "{fields:?}");
    }

    #[test]
    fn mounts_become_binds_and_tmpfs() {
        let bundle = TestBundle::new(&config(&json!({
            "mounts": [
                {"destination": "/proc", "type": "proc", "source": "proc"},
                {"destination": "/dev/pts", "type": "devpts", "source": "devpts", "options": ["newinstance"]},
                {"destination": "/data", "type": "bind", "source": "data", "options": ["rbind", "ro", "nosuid", "rprivate"]},
                {"destination": "/one", "type": "none", "source": "data", "options": ["bind"]},
                {"destination": "/cache", "type": "tmpfs", "source": "tmpfs", "options": ["nodev", "size=64m", "mode=1777", "noatime"]},
                {"destination": "/plain", "type": "bind", "source": "data"}
            ]
        })));
        let loaded = bundle.load().unwrap();
        assert!(
            loaded.warnings.iter().any(|w| w.contains("noatime")),
            "{:?}",
            loaded.warnings
        );
        let mounts = loaded.isolation.root.unwrap().mounts;
        assert_eq!(
            mounts.len(),
            4,
            "the standard ones are Shipyard's own: {mounts:#?}"
        );
        let data = std::fs::canonicalize(bundle.dir.join("data")).unwrap();
        let mut expected = Mount::bind(&data, "/data");
        expected.flags = vec![MountFlag::ReadOnly, MountFlag::NoSuid];
        assert_eq!(mounts.first(), Some(&expected));
        assert_eq!(
            mounts.get(1).map(|m| &m.kind),
            Some(&MountKind::Bind {
                source: data,
                recursive: false
            })
        );
        let mut cache = Mount::tmpfs("/cache");
        cache.flags = vec![MountFlag::NoDev];
        cache.data = Some("size=64m,mode=1777".into());
        assert_eq!(mounts.get(2), Some(&cache));
        // A bind without `rbind` leaves the mounts below its source out.
        assert!(matches!(
            mounts.get(3).map(|m| &m.kind),
            Some(MountKind::Bind {
                recursive: false,
                ..
            })
        ));
    }

    #[test]
    fn bad_mounts_are_errors() {
        let with = |mount: Value| invalid(&config(&json!({"mounts": [mount]})));
        assert!(
            with(json!({"destination": "/x", "type": "bind", "source": "missing"}))
                .contains("missing")
        );
        assert!(with(json!({"destination": "/x", "type": "bind"})).contains("no source"));
        assert!(
            with(json!({"destination": "/x", "type": "tmpfs", "options": ["bogus"]}))
                .contains("bogus")
        );
        assert!(with(json!({"destination": "/x", "type": "bind", "source": "data", "options": ["size=1m"]})).contains("size=1m"));
        assert!(with(json!({"destination": "/proc/x", "type": "tmpfs"})).contains("/proc"));
        // A comma in a value would pass another option to the kernel.
        assert!(
            with(json!({"destination": "/x", "type": "tmpfs", "options": ["size=1m,huge=always"]}))
                .contains("huge")
        );
        let untyped = refused(&config(
            &json!({"mounts": [{"destination": "/x", "source": "y"}]}),
        ));
        assert!(
            untyped.iter().any(|field| field.contains("no type")),
            "{untyped:?}"
        );
    }

    #[test]
    fn broken_configs_say_what_and_where() {
        let bundle = TestBundle::new(&json!({}));
        std::fs::write(
            bundle.dir.join("config.json"),
            "{\n  \"ociVersion\": 1.0,\n}",
        )
        .unwrap();
        match bundle.load() {
            Err(err @ BundleError::Json { line: 2, .. }) => {
                let text = err.to_string();
                assert_eq!(
                    text.matches("line 2").count(),
                    1,
                    "the position once: {text}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(invalid(&config(&json!({"ociVersion": "2.0.0"}))).contains("ociVersion"));
        assert!(
            invalid(&json!({"ociVersion": "1.0.0", "root": {"path": "rootfs"}}))
                .contains("no process")
        );
        let mut no_args = config(&json!({}));
        no_args["process"]["args"] = json!([]);
        assert!(invalid(&no_args).contains("process.args"));
        let mut missing_root = config(&json!({}));
        missing_root["root"]["path"] = json!("nowhere");
        assert!(invalid(&missing_root).contains("nowhere"));
        let mut unknown = config(&json!({}));
        unknown["linux"]["namespaces"] = json!([{"type": "mount"}, {"type": "bogus"}]);
        assert!(invalid(&unknown).contains("bogus"));
    }

    #[test]
    fn only_small_regular_files_are_read() {
        let bundle = TestBundle::new(&json!({}));
        let path = bundle.dir.join("config.json");
        std::fs::remove_file(&path).unwrap();
        let err = bundle.load().unwrap_err();
        assert!(matches!(err, BundleError::Read { .. }));
        // The reason is in the message, not also its cause: printed as a
        // chain (the CLI does), it would appear twice.
        assert!(std::error::Error::source(&err).is_none());
        // A FIFO: reading it would wait for a writer forever.
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(matches!(bundle.load(), Err(BundleError::NotAFile { .. })));
        std::fs::remove_file(&path).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_CONFIG_SIZE + 1)
            .unwrap();
        assert!(matches!(bundle.load(), Err(BundleError::TooLarge { .. })));
    }
}
