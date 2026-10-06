//! OCI bundles: a directory with a `config.json` and a root filesystem, as
//! the OCI runtime spec describes a container (what `runc` and Podman read).
//!
//! [`load_bundle`] turns one into what Shipyard runs: a [`ProcessSpec`] and
//! an [`Isolation`]. Shipyard doesn't do everything the spec describes yet,
//! so the config's fields fall in three groups:
//!
//! - **supported**: the process, the root filesystem, the hostname and
//!   domain name, the PID, mount, UTS and IPC namespaces, masked and
//!   read-only paths, bind and tmpfs mounts, and the IPC sysctls;
//! - **refused**: anything whose absence would leave the container *less
//!   confined than asked*: capabilities to drop, seccomp, a user or network
//!   namespace, resource limits, and so on. Running without them would be
//!   a silent security downgrade, so the bundle is refused, with every such
//!   field named at once;
//! - **ignored, with a warning**: what doesn't confine (hooks,
//!   `oomScoreAdj`, the cgroup mount), until the stages that add them.
//!
//! Fields the spec doesn't define are ignored, as the spec requires.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::isolation::{Isolation, Mount, MountFlag, MountKind, Namespace, Root};
use crate::process::{Env, ProcessSpec, Rlimit, RlimitValue, User};

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
}

/// Why a bundle can't be run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    /// `config.json` couldn't be read.
    #[error("reading {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
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

/// Reads and parses `config.json`, refusing what isn't a small regular file.
///
/// The checks are made on the file as opened, not on its path beforehand: a
/// FIFO swapped in between would block the read forever. Opened
/// non-blocking (a FIFO's open would otherwise wait for a writer), and read
/// with a limit (the file may grow after the check).
fn read_config(path: &Path) -> Result<Config, BundleError> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let read = |source| BundleError::Read {
        path: path.to_owned(),
        source,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(read)?;
    if !file.metadata().map_err(read)?.is_file() {
        return Err(BundleError::NotAFile {
            path: path.to_owned(),
        });
    }
    let mut text = Vec::new();
    file.take(MAX_CONFIG_SIZE + 1)
        .read_to_end(&mut text)
        .map_err(read)?;
    if u64::try_from(text.len()).map_or(true, |len| len > MAX_CONFIG_SIZE) {
        return Err(BundleError::TooLarge {
            path: path.to_owned(),
        });
    }
    serde_json::from_slice(&text).map_err(|err| {
        // serde_json's message ends with the position, which the error
        // already gives.
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
    })
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
    hooks: Option<Value>,
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
    capabilities: Option<Value>,
    #[serde(default)]
    rlimits: Vec<RlimitConfig>,
    #[serde(default)]
    no_new_privileges: bool,
    apparmor_profile: Option<String>,
    selinux_label: Option<String>,
    oom_score_adj: Option<Value>,
    scheduler: Option<Value>,
    io_priority: Option<Value>,
    #[serde(rename = "execCPUAffinity")]
    exec_cpu_affinity: Option<Value>,
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
    devices: Vec<Value>,
    mount_label: Option<String>,
    sysctl: Option<serde_json::Map<String, Value>>,
    cgroups_path: Option<String>,
    intel_rdt: Option<Value>,
    memory_policy: Option<Value>,
    personality: Option<Value>,
    /// Device names to move in: a name with `{}` still asks for its device.
    net_devices: Option<serde_json::Map<String, Value>>,
    time_offsets: Option<Value>,
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
        if config.hooks.is_some() {
            self.warn("hooks", "Shipyard doesn't run hooks");
        }
        let spec = self.process(process)?;
        let linux = config.linux.unwrap_or_default();
        let mut isolation = Isolation::standard();
        isolation.namespaces = self.namespaces(&linux.namespaces)?;
        isolation.hostname = config.hostname.filter(|name| !name.is_empty());
        isolation.domainname = config.domainname.filter(|name| !name.is_empty());
        isolation.sysctls = self.sysctls(linux.sysctl.as_ref())?;
        let mut root = Root::new(self.root_path(&root_config.path)?);
        root.read_only = root_config.readonly;
        // As the spec says: what the config lists, and nothing else.
        root.masked_paths = linux.masked_paths.clone().unwrap_or_default();
        root.readonly_paths = linux.readonly_paths.clone().unwrap_or_default();
        self.linux(&linux);
        root.mounts = self.mounts(config.mounts)?;
        isolation.root = Some(root);
        // What's refused is reported first (by `load_bundle`): a container
        // missing what was refused may not validate, and the list is what
        // the user has to fix.
        if !self.refused.is_empty() {
            return Ok(Bundle {
                spec,
                isolation,
                warnings: Vec::new(),
                annotations,
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
        })
    }

    fn process(&mut self, process: ProcessConfig) -> Result<ProcessSpec, BundleError> {
        if process.terminal {
            self.refuse(
                "process.terminal",
                "a terminal inside comes in a later version",
            );
        }
        if process.capabilities.is_some() {
            self.refuse(
                "process.capabilities",
                "dropping capabilities comes in a later version (without the field, the \
                 process keeps root's)",
            );
        }
        if process.no_new_privileges {
            self.refuse("process.noNewPrivileges", "it comes in a later version");
        }
        if process.apparmor_profile.is_some_and(|p| !p.is_empty()) {
            self.refuse("process.apparmorProfile", "AppArmor isn't supported");
        }
        if process.selinux_label.is_some_and(|l| !l.is_empty()) {
            self.refuse("process.selinuxLabel", "SELinux isn't supported");
        }
        if process.oom_score_adj.is_some() {
            self.warn("process.oomScoreAdj", "it comes in a later version");
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
        Ok(spec)
    }

    /// The namespaces to create. One the spec knows but Shipyard can't make
    /// yet is refused; an unknown type is an error (the spec says so).
    fn namespaces(&mut self, listed: &[NamespaceConfig]) -> Result<Vec<Namespace>, BundleError> {
        let mut namespaces = Vec::new();
        for namespace in listed {
            if namespace.path.as_ref().is_some_and(|path| !path.is_empty()) {
                self.refuse(
                    &format!("linux.namespaces ({}, path)", namespace.kind),
                    "joining an existing namespace comes in a later version",
                );
                continue;
            }
            match namespace.kind.as_str() {
                "pid" => namespaces.push(Namespace::Pid),
                "mount" => namespaces.push(Namespace::Mount),
                "uts" => namespaces.push(Namespace::Uts),
                "ipc" => namespaces.push(Namespace::Ipc),
                later @ ("network" | "user" | "cgroup" | "time") => self.refuse(
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
        Ok(namespaces)
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
        if !linux.uid_mappings.is_empty() || !linux.gid_mappings.is_empty() {
            self.refuse(
                "linux.uidMappings/gidMappings",
                "user namespaces come in a later version",
            );
        }
        if linux.seccomp.is_some() {
            self.refuse("linux.seccomp", "seccomp comes in a later version");
        }
        if linux
            .resources
            .as_ref()
            .is_some_and(|resources| !asks_nothing(resources))
        {
            self.refuse("linux.resources", "resource limits come in a later version");
        }
        if !linux.devices.is_empty() {
            self.refuse("linux.devices", "extra devices come in a later version");
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
        if linux
            .net_devices
            .as_ref()
            .is_some_and(|devices| !devices.is_empty())
        {
            self.refuse("linux.netDevices", "networking comes in a later version");
        }
        if linux
            .cgroups_path
            .as_ref()
            .is_some_and(|path| !path.is_empty())
        {
            self.warn("linux.cgroupsPath", "cgroups come in a later version");
        }
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
                self.warn("the cgroup mount", "cgroups come in a later version");
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
                "cwd": "/"
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
        // One that would change the host: an error, never ignored.
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
                    {"type": "pid"}, {"type": "mount"}, {"type": "network"},
                    {"type": "uts", "path": "/proc/1/ns/uts"}
                ],
                "uidMappings": [{"containerID": 0, "hostID": 1000, "size": 1}],
                "seccomp": {"defaultAction": "SCMP_ACT_ERRNO"},
                "resources": {"devices": [{"allow": false, "access": "rwm"}]},
                "devices": [{"path": "/dev/fuse", "type": "c", "major": 10, "minor": 229}],
                "mountLabel": "m",
                "rootfsPropagation": "shared",
                "intelRdt": {"closID": "c"},
                "memoryPolicy": {"mode": "MPOL_BIND", "nodes": "0"},
                "personality": {"domain": "LINUX32"},
                "netDevices": {"eth1": {}},
                "timeOffsets": {"monotonic": {"secs": 1}}
            },
            "mounts": [
                {"destination": "/data", "type": "bind", "source": "data", "options": ["rbind", "rshared"]},
                {"destination": "/o", "type": "overlay", "source": "overlay"}
            ]
        })));
        for expected in [
            "process.terminal",
            "process.capabilities",
            "process.noNewPrivileges",
            "process.apparmorProfile",
            "process.selinuxLabel",
            "process.scheduler",
            "process.ioPriority",
            "process.execCPUAffinity",
            "linux.intelRdt",
            "linux.memoryPolicy",
            "linux.personality",
            "linux.netDevices",
            "linux.timeOffsets",
            "linux.namespaces (network)",
            "linux.namespaces (uts, path)",
            "linux.uidMappings",
            "linux.seccomp",
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
    fn what_doesnt_confine_is_ignored_with_a_warning() {
        let loaded = TestBundle::new(&config(&json!({
            "hooks": {"prestart": []},
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
            "mounts": [
                {"destination": "/sys/fs/cgroup", "type": "cgroup", "source": "cgroup"},
                {"destination": "/sys/fs/cgroup", "type": "cgroup2", "source": "cgroup"}
            ],
            "annotations": {"anything": "goes"},
            "aFieldFromTheFuture": true
        })))
        .load()
        .unwrap();
        let warnings = loaded.warnings.join("\n");
        for ignored in [
            "hooks",
            "process.oomScoreAdj",
            "cgroup",
            "linux.cgroupsPath",
        ] {
            assert!(warnings.contains(ignored), "{ignored}: {warnings}");
        }
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
        assert!(matches!(bundle.load(), Err(BundleError::Read { .. })));
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
