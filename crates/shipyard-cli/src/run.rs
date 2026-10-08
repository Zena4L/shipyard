//! `shipyard run`: flags in, a validated `ProcessSpec` out.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, value_parser};
use shipyard_runtime::hooks::ContainerHooks;
use shipyard_runtime::isolation::{IdMap, IdMaps, Isolation, Mount, Namespace, Root, RootlessMaps};
use shipyard_runtime::process::{self, ExitStatus};
use shipyard_runtime::process::{
    DEFAULT_STOP_TIMEOUT_SECS, Env, ProcessSpec, Rlimit, Signal, StopPolicy, User,
};
use shipyard_runtime::security::seccomp::Profile;
use shipyard_runtime::security::{Capabilities, Capability, CapabilitySet, Seccomp, Security};
use shipyard_storage::{VolumeHold, VolumeMount};

use crate::limits::LimitArgs;

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Set an environment variable, or copy KEY from your shell if no value is given.
    #[arg(short, long, value_name = "KEY[=VALUE]", value_parser = value_parser!(OsString))]
    env: Vec<OsString>,

    /// Working directory inside the container (absolute path): the image's,
    /// or `/`, if not given.
    #[arg(short, long, value_name = "DIR")]
    workdir: Option<PathBuf>,

    /// Run as this user and group: names (from the container's /etc/passwd
    /// and /etc/group) or numbers.
    #[arg(short, long, value_name = "USER[:GROUP]", value_parser = parse_user)]
    user: Option<(String, Option<String>)>,

    /// Add a supplementary group: a name or a number.
    #[arg(long, value_name = "GROUP", value_parser = parse_name_or_id)]
    group_add: Vec<String>,

    /// Set a resource limit, e.g. nofile=1024:2048 or core=0.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    ulimit: Vec<Rlimit>,

    /// The command's OOM score adjustment, -1000 to 1000: how willing the
    /// kernel's OOM killer is to pick it (lowering it needs root).
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    oom_score_adj: Option<i32>,

    /// Give the container the host's device HOST (a directory: every device
    /// below it), at CONTAINER inside (HOST's own path if not given), for
    /// PERMS, from rwm (all three if not given). Repeatable (Linux).
    #[arg(
        long,
        value_name = "HOST[:CONTAINER][:PERMS]",
        value_parser = crate::devices::parse_device,
        conflicts_with = "no_isolation"
    )]
    device: Vec<crate::devices::DeviceArg>,

    /// Let the container use a kind of device: "TYPE MAJOR:MINOR ACCESS",
    /// such as "c 1:3 rwm" or "b *:* r" (a for any type, * for any number).
    /// Repeatable (Linux).
    #[arg(
        long,
        value_name = "RULE",
        value_parser = crate::devices::parse_rule,
        conflicts_with = "no_isolation"
    )]
    device_cgroup_rule: Vec<shipyard_runtime::devices::DeviceRule>,

    /// Signal sent to stop the command.
    #[arg(long, value_name = "SIGNAL", default_value = "SIGTERM")]
    stop_signal: Signal,

    /// Seconds to wait after the stop signal before killing the command.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STOP_TIMEOUT_SECS)]
    stop_timeout: u64,

    /// Run without isolation: the command sees the host's processes, mounts,
    /// hostname and IPC, as you. On Linux, commands are isolated by default
    /// (rootless, in a user namespace, without root); on macOS they always
    /// run like this for now.
    #[arg(long, conflicts_with = "limits")]
    no_isolation: bool,

    /// Hostname inside the container (Linux).
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    hostname: Option<String>,

    /// NIS domain name inside the container (Linux).
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    domainname: Option<String>,

    /// Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824
    /// (Linux; IPC settings only).
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_sysctl, conflicts_with = "no_isolation")]
    sysctl: Vec<(String, String)>,

    /// Use DIR as the container's root filesystem: it becomes `/` inside,
    /// with its own /proc, /sys and /dev (Linux).
    #[arg(
        long,
        value_name = "DIR",
        group = "root",
        conflicts_with = "no_isolation"
    )]
    rootfs: Option<PathBuf>,

    /// Run a pulled image: its layers become the container's root filesystem,
    /// stacked read-only under a writable layer with overlayfs, and its
    /// baked-in command, environment, working directory and user are the
    /// defaults (Linux). Give a reference like `alpine:3.20` or
    /// `ghcr.io/owner/app@sha256:...`.
    #[arg(
        long,
        value_name = "REF",
        group = "root",
        conflicts_with_all = ["no_isolation", "rootfs"],
    )]
    image: Option<String>,

    /// Run COMMAND instead of the image's entrypoint (and without its
    /// default arguments); `""` runs the command alone.
    #[arg(long, value_name = "COMMAND", requires = "image", value_parser = value_parser!(OsString))]
    entrypoint: Option<OsString>,

    /// When to pull the image: `missing` (if the store doesn't have it),
    /// `always`, or `never` (the store's, or an error).
    #[arg(
        long,
        value_enum,
        value_name = "WHEN",
        default_value = "missing",
        requires = "image"
    )]
    pull: crate::image::PullPolicy,

    /// Make the container's root filesystem read-only (with --rootfs or
    /// --image).
    #[arg(long, requires = "root")]
    read_only: bool,

    /// Give the command a capability on top of the default set, like
    /// `NET_ADMIN` (or `CAP_NET_ADMIN`); `ALL` for every one. Repeatable
    /// (Linux).
    #[arg(long, value_name = "CAP", value_parser = parse_capability, conflicts_with = "no_isolation")]
    cap_add: Vec<CapabilityArg>,

    /// Take a capability out of the default set; `ALL` for every one.
    /// Repeatable (Linux).
    #[arg(long, value_name = "CAP", value_parser = parse_capability, conflicts_with = "no_isolation")]
    cap_drop: Vec<CapabilityArg>,

    /// A security option: `no-new-privileges=false` lets setuid programs
    /// inside gain privileges (they can't by default); `seccomp=FILE` filters
    /// system calls with a profile of your own, `seccomp=unconfined` not at
    /// all (Docker's profile by default). Repeatable (Linux).
    #[arg(long, value_name = "OPTION", value_parser = parse_security_opt, conflicts_with = "no_isolation")]
    security_opt: Vec<SecurityOpt>,

    /// Run in a user namespace: container IDs CONTAINER.. are host IDs
    /// HOST.., SIZE of them, e.g. 0:100000:65536 (root inside is host user
    /// 100000). Repeatable (Linux, root only: without root, the maps are
    /// chosen for you).
    #[arg(long, value_name = "CONTAINER:HOST:SIZE", value_parser = parse_id_map, conflicts_with = "no_isolation")]
    uidmap: Vec<IdMap>,

    /// The same for group IDs; the --uidmap ranges if not given (Linux).
    #[arg(long, value_name = "CONTAINER:HOST:SIZE", value_parser = parse_id_map, requires = "uidmap")]
    gidmap: Vec<IdMap>,

    /// Run the OCI bundle in DIR: its config.json says what to run and how,
    /// so no COMMAND or container options are given (Linux).
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = [
            "command", "env", "workdir", "user", "group_add", "ulimit", "oom_score_adj",
            "no_isolation", "hostname", "domainname", "sysctl", "rootfs", "read_only",
            "cap_add", "cap_drop", "security_opt", "uidmap", "gidmap", "limits", "device",
            "device_cgroup_rule", "image", "pull", "volume",
        ],
    )]
    bundle: Option<PathBuf>,

    /// Memory, CPU, process and I/O limits.
    #[command(flatten)]
    limits: LimitArgs,

    /// Mount a named volume or host path into the container:
    /// `SOURCE:DEST[:ro]`. A bare SOURCE (no slash) is a named volume, created
    /// on first use; a path is bind-mounted. Repeatable (Linux, with a root
    /// filesystem).
    #[arg(long, value_name = "SOURCE:DEST[:ro]", conflicts_with = "no_isolation")]
    volume: Vec<String>,

    /// The command to run, followed by its arguments.
    #[arg(
        required_unless_present_any = ["bundle", "image"],
        trailing_var_arg = true,
        value_name = "COMMAND",
        value_parser = value_parser!(OsString),
    )]
    command: Vec<OsString>,
}

/// Everything a `shipyard run` needs, checked.
#[derive(Debug)]
pub(crate) struct Invocation {
    spec: ProcessSpec,
    stop: StopPolicy,
    isolation: Option<Isolation>,
    /// What a bundle asks for that's ignored, to show the user.
    warnings: Vec<String>,
}

/// A `shipyard run` checked as far as it can be before its image is
/// pulled: a mistake in a flag costs no download.
pub(crate) enum Checked {
    /// An OCI bundle's run, complete.
    Bundle(Box<Invocation>),
    /// A command's run, finished once its image (if any) is ready.
    Command(Box<Planned>),
}

/// A command's run, checked: everything but what an image brings.
pub(crate) struct Planned {
    args: RunArgs,
    stop: StopPolicy,
    /// Without its root yet, for an image.
    isolation: Option<Isolation>,
    /// Why a rootless caller's maps are short, if they are.
    short_maps: Option<String>,
    /// The `--volume`s, parsed: a host path absolute and found.
    volumes: Vec<VolumeMount>,
}

impl RunArgs {
    /// Checks and builds everything an image doesn't change, with no side
    /// effects: the stop policy, the isolation (without an image's root),
    /// the process (as far as it's given), the volumes.
    pub(crate) fn check(self) -> Result<Checked> {
        let stop = StopPolicy {
            signal: self.stop_signal,
            timeout: Duration::from_secs(self.stop_timeout),
        };
        stop.validate()?;
        if let Some(dir) = &self.bundle {
            return Ok(Checked::Bundle(Box::new(from_bundle(dir, stop)?)));
        }
        if self.image.is_some() && !cfg!(target_os = "linux") {
            bail!(
                "--image needs Linux (overlayfs), which {} doesn't have",
                std::env::consts::OS
            );
        }
        let (isolation, short_maps) = self.isolation()?;
        let volumes = self.volume_mounts()?;
        if let Some(dir) = self.workdir.as_ref().filter(|dir| !dir.is_absolute()) {
            bail!("--workdir must be an absolute path, not {}", dir.display());
        }
        if self.oom_score_adj.is_some() && !cfg!(target_os = "linux") {
            bail!(
                "--oom-score-adj needs Linux's OOM killer, which {} doesn't have",
                std::env::consts::OS
            );
        }
        self.check_process(isolation.as_ref())?;
        Ok(Checked::Command(Box::new(Planned {
            args: self,
            stop,
            isolation,
            short_maps,
            volumes,
        })))
    }

    /// The process as far as the flags give it, checked (the image's
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
            add_env(&mut spec.env, entry)?;
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
        if !self.volume.is_empty() && self.rootfs.is_none() && self.image.is_none() {
            bail!("--volume needs a root filesystem (--image or --rootfs)");
        }
        self.volume
            .iter()
            .map(|spec| {
                let mut mount = VolumeMount::parse(spec)
                    .map_err(|err| anyhow::anyhow!("--volume {spec}: {err}"))?;
                if !shipyard_storage::volume::is_named_volume(&mount.source) {
                    mount.source = canonical_source(&mount.source)
                        .with_context(|| format!("--volume {spec}"))?;
                }
                let stand_in = Path::new("/").join(&mount.source);
                Mount::bind(stand_in, &mount.destination)
                    .validate()
                    .with_context(|| format!("--volume {spec}"))?;
                Ok(mount)
            })
            .collect()
    }
}

impl Planned {
    /// Finishes the run with `image`'s root and defaults, if it has an
    /// image: each applies only where a flag didn't say otherwise. Named
    /// volumes are made if new, and held: keep what's returned with them
    /// until the run ends.
    pub(crate) fn finish(
        self,
        image: Option<ImageDefaults>,
    ) -> Result<(Invocation, Vec<VolumeHold>)> {
        let Self {
            args,
            stop,
            mut isolation,
            short_maps,
            volumes,
        } = self;
        let config = image.as_ref().map(|image| &image.config);
        let mut warnings: Vec<String> = short_maps.into_iter().collect();

        // The user: an explicit --user wins; else the image's User; else the
        // isolation default (root inside) or the caller (unisolated).
        let user = args.resolve_user(isolation.as_ref(), config)?;

        // Environment: the image's variables first, then --env (which wins),
        // then a default PATH if none. The image's are taken as they are:
        // one without a value is never looked up here, on the host.
        let mut env = Env::new();
        if let Some(config) = config {
            add_image_env(&mut env, &config.env, &mut warnings)?;
        }
        for entry in &args.env {
            add_env(&mut env, entry)?;
        }
        env.fill_default_path();

        let mut spec = ProcessSpec::new(args.resolve_command(config)?);
        spec.env = env;
        spec.cwd = args.resolve_workdir(config);
        spec.user = user;
        spec.rlimits = last_wins(args.ulimit.clone());
        spec.oom_score_adj = args.oom_score_adj;
        spec.validate()?;

        let mut held = Vec::new();
        if let Some(isolation) = isolation.as_mut() {
            if let Some(image) = image {
                // The image's layers become the root: an overlay mounted at
                // this path inside the container's namespaces.
                let mut root = Root::new(image.root);
                root.read_only = args.read_only;
                root.overlay = Some(image.overlay);
                isolation.root = Some(root);
            }
            let named = match isolation.root.as_mut() {
                Some(root) => bind_volumes(root, volumes)?,
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
                let err = anyhow::Error::from(err);
                if store {
                    err.context(
                        "the image store's path can't be used for an overlay (set \
                         SHIPYARD_IMAGE_STORE to another directory)",
                    )
                } else {
                    err
                }
            })?;
            held = hold_volumes(&named)?;
        }
        if isolation
            .as_ref()
            .and_then(|isolation| isolation.id_maps.as_ref())
            .is_some_and(IdMaps::maps_host_root)
        {
            warnings.push(
                "--uidmap/--gidmap map host ID 0: a user inside is root on the host's files, \
                 which a user namespace is meant to prevent"
                    .to_owned(),
            );
        }
        Ok((
            Invocation {
                spec,
                stop,
                isolation,
                warnings,
            },
            held,
        ))
    }
}

/// Adds an image's `Env` `entries` to `env`, as they are: one without a
/// value (`NAME`, not `NAME=value`) is skipped with a warning, and never
/// looked up on the host, as a `--env NAME` is (the image would read the
/// caller's secrets).
fn add_image_env(env: &mut Env, entries: &[String], warnings: &mut Vec<String>) -> Result<()> {
    for entry in entries {
        if entry.contains('=') {
            env.push_entry(OsStr::new(entry))
                .context("the image's Env")?;
        } else {
            warnings.push(format!(
                "the image's Env entry `{entry}` has no value; ignored"
            ));
        }
    }
    Ok(())
}

/// `source` made absolute, symlinks resolved, as UTF-8.
fn canonical_source(source: &str) -> Result<String> {
    std::fs::canonicalize(source)
        .with_context(|| source.to_owned())?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("{source}: a path that isn't UTF-8"))
}

/// Binds `volumes` into `root`: a host path as it is (resolved again, as
/// close to the run as can be), a named volume's directory (made or not:
/// [`hold_volumes`] makes it). Returns the named volumes' names.
fn bind_volumes(root: &mut Root, volumes: Vec<VolumeMount>) -> Result<Vec<String>> {
    let mut named = Vec::new();
    let mut store = None;
    for mount in volumes {
        let source = if shipyard_storage::volume::is_named_volume(&mount.source) {
            let store = match &store {
                Some(store) => store,
                None => store.insert(crate::volumes::volumes()?),
            };
            let data = store.data_path(&mount.source)?;
            named.push(mount.source);
            data
        } else {
            PathBuf::from(canonical_source(&mount.source)?)
        };
        let mut bind = Mount::bind(source, mount.destination);
        if mount.read_only {
            bind.flags
                .push(shipyard_runtime::isolation::MountFlag::ReadOnly);
        }
        root.mounts.push(bind);
    }
    Ok(named)
}

/// Makes the named volumes `names` if new, and holds them: nothing removes
/// a volume while its hold lives.
fn hold_volumes(names: &[String]) -> Result<Vec<VolumeHold>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::volumes::volumes()?;
    names
        .iter()
        .map(|name| {
            store
                .hold(name)
                .with_context(|| format!("the volume {name}"))
        })
        .collect()
}

/// A run of the OCI bundle in `dir`: everything but the stop policy comes
/// from its `config.json`.
fn from_bundle(dir: &Path, stop: StopPolicy) -> Result<Invocation> {
    if !cfg!(target_os = "linux") {
        bail!(
            "--bundle needs Linux namespaces, which aren't available on {} yet",
            std::env::consts::OS
        );
    }
    // Without root, it runs rootless (see `load_bundle_for_caller`).
    #[cfg(target_os = "linux")]
    let bundle = shipyard_runtime::oci::load_bundle_for_caller(dir)?;
    #[cfg(not(target_os = "linux"))]
    let bundle = shipyard_runtime::oci::load_bundle(dir)?;
    let mut warnings = bundle.warnings;
    if let Some(path) = &bundle.cgroups_path {
        warnings.push(format!(
            "linux.cgroupsPath ({}) is ignored by shipyard run: each run's cgroup is its own \
             (shipyard-runtime honors it)",
            path.display()
        ));
    }
    let mut isolation = bundle.isolation;
    if !bundle.hooks.is_empty() {
        // Their state names the run as its cgroup does (`run/<pid>`), and
        // the bundle by its absolute path, as `shipyard-runtime` records it.
        let dir =
            std::fs::canonicalize(dir).with_context(|| format!("the bundle {}", dir.display()))?;
        // The hooks' state is JSON, which only takes UTF-8.
        if dir.to_str().is_none() {
            bail!(
                "the bundle {}: a bundle with hooks needs a path that is UTF-8 (its hooks get \
                 it in JSON)",
                dir.display()
            );
        }
        isolation.hooks = Some(ContainerHooks::new(
            bundle.hooks,
            format!("run-{}", std::process::id()),
            dir,
            bundle.annotations,
        ));
    }
    Ok(Invocation {
        spec: bundle.spec,
        stop,
        isolation: Some(isolation),
        warnings,
    })
}

impl RunArgs {
    /// The isolation for this run (the standard namespaces on Linux unless
    /// `--no-isolation`; none on macOS, where namespaces don't exist yet:
    /// the VM comes in a later version), and a warning if a rootless
    /// caller's user namespace maps fewer IDs than it could.
    fn isolation(&self) -> Result<(Option<Isolation>, Option<String>)> {
        if self.no_isolation {
            return Ok((None, None));
        }
        if !cfg!(target_os = "linux") {
            for (given, flag) in [
                (self.hostname.is_some(), "--hostname"),
                (self.domainname.is_some(), "--domainname"),
                (!self.sysctl.is_empty(), "--sysctl"),
                (self.rootfs.is_some(), "--rootfs"),
                (!self.cap_add.is_empty(), "--cap-add"),
                (!self.cap_drop.is_empty(), "--cap-drop"),
                (!self.security_opt.is_empty(), "--security-opt"),
                (!self.uidmap.is_empty(), "--uidmap"),
                (!self.device.is_empty(), "--device"),
                (!self.device_cgroup_rule.is_empty(), "--device-cgroup-rule"),
                (!self.volume.is_empty(), "--volume"),
            ] {
                if given {
                    bail!(
                        "{flag} needs Linux namespaces, which aren't available on {} yet",
                        std::env::consts::OS
                    );
                }
            }
            if let Some(flag) = self.limits.first_given() {
                bail!(
                    "{flag} needs Linux cgroups, which aren't available on {} yet",
                    std::env::consts::OS
                );
            }
            tracing::debug!("no namespaces on this system; running without isolation");
            return Ok((None, None));
        }
        let mut isolation = Isolation::standard();
        isolation.hostname.clone_from(&self.hostname);
        isolation.domainname.clone_from(&self.domainname);
        isolation.sysctls.clone_from(&self.sysctl);
        isolation.security = self.security()?;
        isolation.resources = self.limits.resources()?;
        // Shipyard refuses to start setuid (`refuse_privileged_start`), so
        // the real user is the effective one.
        #[cfg(target_os = "linux")]
        let (maps, short_maps) = self.user_namespace(
            User::current().uid == 0,
            shipyard_runtime::process::rootless_id_maps,
        )?;
        #[cfg(not(target_os = "linux"))]
        let (maps, short_maps) = (self.id_maps(), None);
        isolation.id_maps = maps;
        if isolation.id_maps.is_some() {
            isolation.namespaces.push(Namespace::User);
        }
        // An image's root comes once it's pulled (`Planned::finish`).
        if let Some(dir) = &self.rootfs {
            // Absolute and without symlinks, as the runtime wants it; and a
            // mistyped path is caught here, with its name.
            let path = std::fs::canonicalize(dir)
                .with_context(|| format!("--rootfs {}", dir.display()))?;
            let mut root = Root::new(path);
            root.read_only = self.read_only;
            isolation.root = Some(root);
        }
        #[cfg(target_os = "linux")]
        crate::devices::add(
            &mut isolation.devices,
            &self.device,
            &self.device_cgroup_rule,
            self.rootfs.is_some() || self.image.is_some(),
        )?;
        if self.image.is_some() {
            isolation.validate_expecting_root()?;
        } else {
            isolation.validate()?;
        }
        Ok((Some(isolation), short_maps))
    }
}

impl RunArgs {
    /// The user to run as: an explicit `--user` wins; else the image's `User`
    /// (`name`, `uid`, or `uid:gid`); else root inside an isolated run, or the
    /// caller without isolation.
    fn resolve_user(
        &self,
        isolation: Option<&Isolation>,
        image: Option<&shipyard_image::RuntimeConfig>,
    ) -> Result<User> {
        let spec = match &self.user {
            Some((name, group)) => Some((name.clone(), group.clone())),
            None => match image.and_then(|config| config.user.as_deref()) {
                Some(user) => Some(parse_user(user).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            },
        };
        let mut user = match spec {
            Some((name, group)) => {
                let mut user = User::named(name, group);
                user.names.memberships = isolation.is_some();
                user
            }
            None if isolation.is_some() => User::new(0, 0),
            None => User::current(),
        };
        user.names.additional_groups = dedup(self.group_add.clone());
        Ok(user)
    }

    /// The command to run, as Docker makes it: the entrypoint (the image's,
    /// or `--entrypoint`'s; none for `--entrypoint ""`), then the command
    /// given, or else the image's `Cmd` (unless `--entrypoint` replaced its
    /// entrypoint: then that goes too).
    fn resolve_command(
        &self,
        image: Option<&shipyard_image::RuntimeConfig>,
    ) -> Result<Vec<OsString>> {
        let Some(config) = image else {
            // Clap requires one without --image or --bundle.
            return Ok(self.command.clone());
        };
        let owned = |list: &[String]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let (entrypoint, default) = match &self.entrypoint {
            // As Docker: no entrypoint, and the image's `Cmd` if nothing's
            // given.
            Some(given) if given.is_empty() => (Vec::new(), owned(&config.cmd)),
            Some(given) => (vec![given.clone()], Vec::new()),
            None => (owned(&config.entrypoint), owned(&config.cmd)),
        };
        let arguments = if self.command.is_empty() {
            default
        } else {
            self.command.clone()
        };
        let command: Vec<OsString> = entrypoint.into_iter().chain(arguments).collect();
        if command.is_empty() {
            bail!("the image sets no command, so give one");
        }
        Ok(command)
    }

    /// The working directory: `--workdir`, else the image's `WorkingDir`,
    /// else `/`.
    fn resolve_workdir(&self, image: Option<&shipyard_image::RuntimeConfig>) -> PathBuf {
        self.workdir
            .clone()
            .or_else(|| {
                image
                    .and_then(|config| config.working_dir.as_deref())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// The user namespace for this run, and why its maps are short if they
    /// are: for root, the one `--uidmap` asks for (if any); for anyone else,
    /// always one, with maps chosen for them by `rootless` (their own ID,
    /// and their `/etc/subuid` ranges when the helpers are there).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux has user namespaces")
    )]
    fn user_namespace(
        &self,
        root: bool,
        rootless: impl FnOnce() -> RootlessMaps,
    ) -> Result<(Option<IdMaps>, Option<String>)> {
        if root {
            return Ok((self.id_maps(), None));
        }
        if !self.uidmap.is_empty() {
            bail!(
                "--uidmap and --gidmap need root: a rootless container's maps are chosen for \
                 you (your own ID, and your range in /etc/subuid)"
            );
        }
        let rootless = rootless();
        Ok((Some(rootless.maps), rootless.shortfall))
    }

    /// The user namespace's maps, if `--uidmap` asks for one: `--gidmap`, or
    /// the same ranges for groups.
    fn id_maps(&self) -> Option<IdMaps> {
        if self.uidmap.is_empty() {
            return None;
        }
        let gids = if self.gidmap.is_empty() {
            &self.uidmap
        } else {
            &self.gidmap
        };
        Some(IdMaps {
            uids: self.uidmap.clone(),
            gids: gids.clone(),
        })
    }

    /// The container's capabilities and `no_new_privs`, from Docker's default
    /// set and `--cap-drop`, `--cap-add` and `--security-opt`.
    fn security(&self) -> Result<Security> {
        let named = |args: &[CapabilityArg]| -> CapabilitySet {
            args.iter()
                .filter_map(|arg| match arg {
                    CapabilityArg::One(capability) => Some(*capability),
                    CapabilityArg::All => None,
                })
                .collect()
        };
        let (dropped, added) = (named(&self.cap_drop), named(&self.cap_add));
        if let Some(both) = dropped
            .iter()
            .find(|&capability| added.contains(capability))
        {
            bail!("--cap-add and --cap-drop both name {both}");
        }
        let mut kept = if self.cap_drop.contains(&CapabilityArg::All) {
            CapabilitySet::EMPTY
        } else {
            CapabilitySet::DOCKER_DEFAULT
        };
        for capability in dropped.iter() {
            kept.remove(capability);
        }
        let added = if self.cap_add.contains(&CapabilityArg::All) {
            CapabilitySet::ALL
        } else {
            added
        };
        let mut security = Security::standard();
        security.capabilities = Capabilities::Default { kept, added };
        for option in &self.security_opt {
            match option {
                SecurityOpt::NoNewPrivileges(on) => security.no_new_privileges = *on,
                SecurityOpt::SeccompUnconfined => security.seccomp = Seccomp::Unconfined,
                SecurityOpt::SeccompProfile(path) => {
                    security.seccomp = Seccomp::Profile(Box::new(read_profile(path)?));
                }
            }
        }
        Ok(security)
    }
}

fn parse_id_map(s: &str) -> Result<IdMap, String> {
    s.parse()
        .map_err(|err: shipyard_runtime::isolation::IdMapError| err.to_string())
}

/// `--cap-add` or `--cap-drop`: one capability, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityArg {
    All,
    One(Capability),
}

fn parse_capability(s: &str) -> Result<CapabilityArg, String> {
    if s.eq_ignore_ascii_case("ALL") {
        return Ok(CapabilityArg::All);
    }
    s.parse()
        .map(CapabilityArg::One)
        .map_err(|err| format!("{err}"))
}

/// A seccomp profile file, checked (`--security-opt seccomp=FILE`): a
/// mistake shows here, naming the file, not when the container starts.
fn read_profile(path: &Path) -> Result<Profile> {
    Profile::from_file(path).with_context(|| format!("seccomp profile {}", path.display()))
}

/// `--security-opt`: Docker's spelling, `KEY=VALUE` or `KEY:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SecurityOpt {
    NoNewPrivileges(bool),
    /// `seccomp=unconfined`: no filter.
    SeccompUnconfined,
    /// `seccomp=FILE`.
    SeccompProfile(PathBuf),
}

fn parse_security_opt(s: &str) -> Result<SecurityOpt, String> {
    let (key, value) = match s.split_once(['=', ':']) {
        Some((key, value)) => (key, Some(value)),
        None => (s, None),
    };
    match (key, value) {
        ("no-new-privileges", None | Some("true")) => Ok(SecurityOpt::NoNewPrivileges(true)),
        ("no-new-privileges", Some("false")) => Ok(SecurityOpt::NoNewPrivileges(false)),
        ("no-new-privileges", Some(other)) => Err(format!(
            "no-new-privileges takes true or false, not \"{other}\""
        )),
        ("seccomp", Some("unconfined")) => Ok(SecurityOpt::SeccompUnconfined),
        ("seccomp", Some(path)) if !path.is_empty() => {
            Ok(SecurityOpt::SeccompProfile(PathBuf::from(path)))
        }
        ("seccomp", _) => Err("seccomp takes a profile file, or unconfined".to_owned()),
        _ => Err(format!(
            "unknown security option \"{s}\" (known: no-new-privileges[=true|false], \
             seccomp=FILE|unconfined)"
        )),
    }
}

/// Runs the command under the supervisor and exits the way it did: with its
/// exit code, or `128 + N` if signal N killed it. Failures to start it exit
/// 125, 126 or 127.
pub(crate) fn run(args: RunArgs) -> Result<ExitCode> {
    let planned = match args.check()? {
        Checked::Bundle(invocation) => return Ok(run_invocation(*invocation)?.exit()),
        Checked::Command(planned) => planned,
    };
    // The flags checked, the image is made ready: pulled if it must be, its
    // layers extracted, and the run's writable area made. The overlay is
    // mounted by the runtime inside the container's namespaces (not here).
    // The area and the volumes are the run's for its whole life: let go
    // when it ends, before the CLI exits (or on an error on the way, when
    // they drop).
    #[cfg(target_os = "linux")]
    let (defaults, area) = prepare_image(&planned)?.unzip();
    #[cfg(not(target_os = "linux"))]
    let defaults = None;
    let (invocation, held_volumes) = planned.finish(defaults)?;
    let ended = run_invocation(invocation)?;
    drop(held_volumes);
    #[cfg(target_os = "linux")]
    if let Some(area) = area {
        area.remove();
    }
    Ok(ended.exit())
}

/// Prepares the image named by `--image`, if any (Linux only): pulls it,
/// extracts its layers and makes the run's writable area, returning the
/// defaults a run applies and the area (to keep until the run ends).
#[cfg(target_os = "linux")]
fn prepare_image(planned: &Planned) -> Result<Option<(ImageDefaults, crate::image::RunArea)>> {
    let Some(reference) = &planned.args.image else {
        return Ok(None);
    };
    // As root, `--uidmap`'s maps: the layers are shifted by them. (A
    // rootless caller's maps are its own range, which extraction uses.)
    let maps = planned
        .isolation
        .as_ref()
        .and_then(|isolation| isolation.id_maps.as_ref())
        .filter(|_| nix::unistd::geteuid().is_root());
    let prepared = crate::image::prepare(reference, planned.args.pull, maps)?;
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
/// runtime mounts) and its baked-in defaults, applied under explicit flags.
#[derive(Debug, Clone)]
pub(crate) struct ImageDefaults {
    /// The merged mount point — the container's root path.
    pub root: PathBuf,
    /// The overlay there (layer dirs + upper + work).
    pub overlay: shipyard_runtime::isolation::Overlay,
    /// The image's runtime config (default command, env, workdir, user).
    pub config: shipyard_image::RuntimeConfig,
}

/// How a run ended, for the CLI to end the same way.
enum Ended {
    /// With this exit code.
    Code(ExitCode),
    /// Interrupted (Ctrl-C), so that a shell loop around `shipyard run`
    /// stops too: the CLI dies from the same signal (or exits with `code`
    /// if it can't).
    Interrupted { signal: Signal, code: ExitCode },
}

impl Ended {
    /// Ends the CLI as the run ended: dies from the signal, or returns the
    /// code. (Last of all: dying from a signal runs no destructors.)
    fn exit(self) -> ExitCode {
        match self {
            Self::Code(code) => code,
            Self::Interrupted { signal, code } => {
                process::die_from(signal);
                code
            }
        }
    }
}

/// Runs a built invocation under the supervisor.
fn run_invocation(invocation: Invocation) -> Result<Ended> {
    let Invocation {
        spec,
        stop,
        isolation,
        warnings,
    } = invocation;
    for warning in &warnings {
        crate::warning(warning);
    }
    tracing::debug!(?spec, ?stop, ?isolation, "running");
    let mut supervisor = process::Supervisor::install()?;
    let status = match supervisor.supervise(&spec, stop, isolation.as_ref()) {
        Ok(status) => status,
        Err(err) => {
            let code = err.exit_code();
            let isolation_failed = is_isolation_failure(&err);
            let exit = crate::report(&anyhow::Error::from(err), code);
            // The runtime can't name the CLI's flag; this is where it's known.
            // Only when it would work: `--no-isolation` can't be combined with
            // a root filesystem, a hostname, a domain name or sysctls.
            if isolation_failed && isolation.as_ref().is_some_and(only_namespaces) {
                crate::hint("--no-isolation runs the command without namespaces");
            }
            return Ok(Ended::Code(exit));
        }
    };
    let code = ExitCode::from(status.code());
    if let ExitStatus::Signaled { signal, .. } = status
        && signal == Signal::INT
    {
        return Ok(Ended::Interrupted { signal, code });
    }
    Ok(Ended::Code(code))
}

/// Whether `isolation` asks for nothing but the namespaces themselves, so
/// that `--no-isolation` could replace it: no limits or device flags
/// either, which need the container's cgroup.
fn only_namespaces(isolation: &Isolation) -> bool {
    isolation.root.is_none()
        && isolation.hostname.is_none()
        && isolation.domainname.is_none()
        && isolation.sysctls.is_empty()
        && isolation.security == Security::standard()
        && isolation.id_maps.is_none()
        && isolation.resources.is_empty()
        && !isolation.devices.asked
}

/// Whether `err` is isolation going wrong (missing privileges, a refusal,
/// or a failed namespace step), which `--no-isolation` avoids.
fn is_isolation_failure(err: &shipyard_runtime::Error) -> bool {
    use shipyard_runtime::Error;
    match err {
        Error::IsolationNeedsRoot
        | Error::IsolationRefused
        | Error::UserNamespacesRefused { .. }
        | Error::InitDidNotStart { .. } => true,
        Error::Spawn(spawn) => spawn.kind() == process::SpawnErrorKind::Isolation,
        _ => false,
    }
}

/// `KEY=VALUE` sets a variable; a bare `KEY` copies it from shipyard's own
/// environment, and is skipped if unset (Docker's behavior).
///
/// Error messages name the variable but never echo the value, which may be a
/// secret.
fn add_env(env: &mut Env, entry: &OsStr) -> Result<()> {
    let bytes = entry.as_bytes();
    let key = OsStr::from_bytes(bytes.split(|&b| b == b'=').next().unwrap_or_default());
    let context = || format!("invalid --env \"{}\"", key.to_string_lossy());

    if bytes.contains(&b'=') {
        return env.push_entry(entry).with_context(context);
    }
    // A bare key is looked up in the shell, so check it first: an empty or
    // spaced name would otherwise just be "not found" and silently dropped.
    if key.is_empty() {
        bail!("invalid --env: the variable name is empty");
    }
    // Not named in the message: without `=`, a typo like `TOKEN secret`
    // would put the secret in the "name".
    if bytes.iter().any(u8::is_ascii_whitespace) {
        bail!("invalid --env: names can't contain spaces (did you mean KEY=VALUE?)");
    }
    let Some(value) = std::env::var_os(key) else {
        // Not named here either: a value passed without `KEY=` (`-e "$TOKEN"`)
        // looks exactly like a name that isn't set.
        tracing::debug!("a bare --env name isn't set in the shell; skipped");
        return Ok(());
    };
    env.set(key, &value).with_context(context)
}

/// Removes repeated groups, keeping the first occurrence of each.
fn dedup(groups: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    groups
        .into_iter()
        .filter(|group| seen.insert(group.clone()))
        .collect()
}

/// A later `--ulimit` for the same resource replaces an earlier one.
fn last_wins(limits: Vec<Rlimit>) -> Vec<Rlimit> {
    let mut merged: Vec<Rlimit> = Vec::new();
    for limit in limits {
        match merged.iter_mut().find(|m| m.kind == limit.kind) {
            Some(existing) => *existing = limit,
            None => merged.push(limit),
        }
    }
    merged
}

/// Parses `USER` or `USER:GROUP`, each a name or a number.
fn parse_user(s: &str) -> Result<(String, Option<String>), String> {
    match s.split_once(':') {
        Some((user, group)) => Ok((parse_name_or_id(user)?, Some(parse_name_or_id(group)?))),
        None => Ok((parse_name_or_id(s)?, None)),
    }
}

/// A user or group: a number (checked as [`parse_id`] does), or a name to
/// look up in the container's `/etc/passwd` or `/etc/group`.
fn parse_name_or_id(s: &str) -> Result<String, String> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return parse_id(s).map(|_| s.to_owned());
    }
    // A sign or a space: a malformed number, more likely than a name.
    let malformed = s.is_empty()
        || s.starts_with(['-', '+'])
        || s.contains(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '/' | '\0'));
    if malformed {
        return Err(format!("\"{s}\" is neither a name nor a number"));
    }
    Ok(s.to_owned())
}

/// `KEY=VALUE`, split at the first `=`. Which keys are allowed is checked
/// with the rest of the isolation.
fn parse_sysctl(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("\"{s}\" isn't KEY=VALUE"))
}

/// A strict numeric user or group ID: digits only, and never `u32::MAX`, which
/// the kernel reads as "leave unchanged".
fn parse_id(s: &str) -> Result<u32, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("\"{s}\" is not a valid numeric ID"));
    }
    match s.parse::<u32>() {
        Ok(u32::MAX) => Err(format!(
            "{} is reserved: the kernel reads it as \"leave unchanged\"",
            u32::MAX
        )),
        Ok(id) => Ok(id),
        Err(_) => Err(format!("\"{s}\" is too large for an ID")),
    }
}

#[cfg(test)]
impl RunArgs {
    /// Checked and finished without an image, as a run without one is.
    fn into_invocation(self) -> Result<Invocation> {
        match self.check()? {
            Checked::Bundle(invocation) => Ok(*invocation),
            Checked::Command(planned) => Ok(planned.finish(None)?.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::Parser;
    use proptest::prelude::*;
    use shipyard_runtime::process::RlimitKind;

    use super::*;

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        run: RunArgs,
    }

    fn parse(args: &[&str]) -> Result<RunArgs, clap::Error> {
        let argv = std::iter::once("run").chain(args.iter().copied());
        Harness::try_parse_from(argv).map(|h| h.run)
    }

    fn spec(args: &[&str]) -> ProcessSpec {
        parse(args).unwrap().into_invocation().unwrap().spec
    }

    /// An image's runtime config with this entrypoint and command.
    fn image(entrypoint: &[&str], cmd: &[&str]) -> shipyard_image::RuntimeConfig {
        let owned = |list: &[&str]| list.iter().map(|&s| s.to_owned()).collect();
        shipyard_image::RuntimeConfig {
            entrypoint: owned(entrypoint),
            cmd: owned(cmd),
            working_dir: Some("/app".to_owned()),
            ..shipyard_image::RuntimeConfig::default()
        }
    }

    #[test]
    fn the_command_is_dockers() {
        let command = |args: &[&str], config: &shipyard_image::RuntimeConfig| {
            let mut argv = vec!["--image", "x"];
            argv.extend_from_slice(args);
            parse(&argv)
                .unwrap()
                .resolve_command(Some(config))
                .map(|command| command.join(OsStr::new(" ")).into_string().unwrap())
        };
        let both = image(&["/entry", "-q"], &["serve"]);
        assert_eq!(command(&[], &both).unwrap(), "/entry -q serve");
        assert_eq!(
            command(&["other", "arg"], &both).unwrap(),
            "/entry -q other arg",
            "arguments replace Cmd, after the entrypoint"
        );
        assert_eq!(
            command(&["--entrypoint", "/bin/echo"], &both).unwrap(),
            "/bin/echo",
            "--entrypoint drops Cmd too"
        );
        assert_eq!(
            command(&["--entrypoint", "/bin/echo", "hi"], &both).unwrap(),
            "/bin/echo hi"
        );
        assert_eq!(
            command(&["--entrypoint", "", "ls", "/"], &both).unwrap(),
            "ls /"
        );
        assert_eq!(
            command(&["--entrypoint", ""], &both).unwrap(),
            "serve",
            "no entrypoint, the image's Cmd (as Docker)"
        );
        let cmd_only = image(&[], &["/bin/sh"]);
        assert_eq!(command(&[], &cmd_only).unwrap(), "/bin/sh");
        assert_eq!(command(&["true"], &cmd_only).unwrap(), "true");
        let err = command(&[], &image(&[], &[])).unwrap_err();
        assert!(err.to_string().contains("sets no command"), "{err}");
        assert!(
            parse(&["--entrypoint", "x", "true"]).is_err(),
            "--entrypoint needs --image"
        );
    }

    #[test]
    fn an_explicit_workdir_wins_even_slash() {
        let config = image(&[], &["sh"]);
        let workdir = |args: &[&str]| {
            let mut argv = vec!["--image", "x"];
            argv.extend_from_slice(args);
            parse(&argv).unwrap().resolve_workdir(Some(&config))
        };
        assert_eq!(workdir(&["-w", "/", "true"]), Path::new("/"));
        assert_eq!(workdir(&["true"]), Path::new("/app"), "the image's");
        assert_eq!(
            parse(&["true"]).unwrap().resolve_workdir(None),
            Path::new("/")
        );
    }

    #[test]
    fn an_images_env_never_reads_the_host() {
        // HOME is set wherever tests run: an image naming it must not get it.
        assert!(std::env::var_os("HOME").is_some());
        let mut env = Env::new();
        let mut warnings = Vec::new();
        let entries = ["HOME".to_owned(), "LANG=C.UTF-8".to_owned()];
        add_image_env(&mut env, &entries, &mut warnings).unwrap();
        assert_eq!(env.get(OsStr::new("HOME")), None);
        assert_eq!(env.get(OsStr::new("LANG")), Some(OsStr::new("C.UTF-8")));
        assert!(warnings[0].contains("`HOME` has no value"), "{warnings:?}");
    }

    #[test]
    fn a_volume_is_a_name_or_an_absolute_host_path_that_exists() {
        let mounts = |spec: &str| {
            parse(&["--rootfs", "/", "--volume", spec, "true"])
                .unwrap()
                .volume_mounts()
        };
        assert!(mounts("./here:/mnt").is_err(), "relative");
        let missing = mounts("/no/such/dir:/mnt").unwrap_err();
        assert!(
            format!("{missing:#}").contains("/no/such/dir"),
            "{missing:#}"
        );
        let found = mounts("/tmp:/mnt:ro").unwrap();
        assert!(Path::new(&found[0].source).is_absolute() && found[0].read_only);
        assert_eq!(mounts("data:/data").unwrap()[0].source, "data");
        let err = parse(&["--volume", "data:/data", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        let expected = if cfg!(target_os = "linux") {
            "needs a root filesystem"
        } else {
            "--volume needs Linux"
        };
        assert!(err.to_string().contains(expected), "{err}");
    }

    #[test]
    fn command_flags_are_not_ours() {
        let run = parse(&["-e", "A=1", "ls", "-l", "--env", "X", "-u", "5"]).unwrap();
        assert_eq!(run.command, ["ls", "-l", "--env", "X", "-u", "5"]);
        assert_eq!(run.env, ["A=1"]);
        assert!(run.user.is_none());
    }

    #[test]
    fn double_dash_works() {
        let run = parse(&["--", "--weird-program", "x"]).unwrap();
        assert_eq!(run.command, ["--weird-program", "x"]);
    }

    #[test]
    fn unknown_flags_before_the_command_are_errors() {
        assert!(parse(&["--bogus", "ls"]).is_err());
    }

    #[test]
    fn command_is_required() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["-e", "A=1"]).is_err());
    }

    #[test]
    fn user_forms() {
        assert_eq!(parse_user("1000"), Ok(("1000".into(), None)));
        assert_eq!(
            parse_user("1000:20"),
            Ok(("1000".into(), Some("20".into())))
        );
        assert_eq!(parse_user("0:0"), Ok(("0".into(), Some("0".into()))));
        for bad in [
            "",
            "+5",
            "-1",
            " 5",
            "5 ",
            "5:",
            ":5",
            "1:2:3",
            "4294967295",
            "5:4294967295",
            "99999999999",
        ] {
            assert!(parse_user(bad).is_err(), "{bad:?} should be rejected");
        }
        let named =
            |user: &str, group: Option<&str>| Ok((user.to_owned(), group.map(str::to_owned)));
        assert_eq!(parse_user("nobody"), named("nobody", None));
        assert_eq!(parse_user("alice:staff"), named("alice", Some("staff")));
        assert_eq!(parse_user("alice:20"), named("alice", Some("20")));
        for not_a_name in ["a b", "a,b", "a/b"] {
            assert!(parse_user(not_a_name).is_err(), "{not_a_name}");
        }
    }

    #[test]
    fn user_and_groups_are_resolved_where_the_command_runs() {
        let user = spec(&["-u", "alice:staff", "--group-add", "wheel", "true"]).user;
        assert_eq!(user.names.user.as_deref(), Some("alice"));
        assert_eq!(user.names.group.as_deref(), Some("staff"));
        assert_eq!(user.names.additional_groups, ["wheel"]);
        // A container's own /etc/group, never the host's.
        assert_eq!(user.names.memberships, cfg!(target_os = "linux"));
        let host = spec(&["--no-isolation", "-u", "alice", "true"]).user;
        assert!(!host.names.memberships);
    }

    #[test]
    fn without_a_user_flag_isolated_runs_are_root_inside() {
        let ids = |user: User| (user.uid, user.gid);
        let current = ids(User::current());
        assert_eq!(ids(spec(&["--no-isolation", "true"]).user), current);
        // Isolated (on Linux): root inside, whoever runs shipyard.
        let isolated = if cfg!(target_os = "linux") {
            (0, 0)
        } else {
            current
        };
        assert_eq!(ids(spec(&["true"]).user), isolated);
    }

    #[test]
    fn groups_are_deduplicated_in_order() {
        let user = spec(&[
            "--group-add",
            "20",
            "--group-add",
            "10",
            "--group-add",
            "20",
            "true",
        ])
        .user;
        assert_eq!(user.names.additional_groups, ["20", "10"]);
    }

    #[test]
    fn later_ulimit_wins() {
        let spec = spec(&[
            "--ulimit",
            "nofile=10",
            "--ulimit",
            "core=0",
            "--ulimit",
            "nofile=20:30",
            "true",
        ]);
        assert_eq!(spec.rlimits.len(), 2);
        assert_eq!(spec.rlimits[0].kind, RlimitKind::Nofile);
        assert_eq!(spec.rlimits[0].to_string(), "nofile=20:30");
    }

    #[test]
    fn bad_values_are_rejected_at_parse_time() {
        assert!(parse(&["--ulimit", "nofile=10:5", "true"]).is_err());
        assert!(parse(&["--stop-signal", "FOO", "true"]).is_err());
        assert!(parse(&["--stop-timeout", "-1", "true"]).is_err());
        assert!(parse(&["--group-add", "a:b", "true"]).is_err());
        assert!(parse(&["--sysctl", "kernel.shmmax", "true"]).is_err());
    }

    #[test]
    fn relative_workdir_fails_validation() {
        let err = parse(&["-w", "tmp", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn env_is_not_inherited_and_has_defaults() {
        let env = spec(&["-e", "A=1", "true"]).env;
        assert_eq!(env.get(OsStr::new("A")), Some(OsStr::new("1")));
        assert!(env.get(OsStr::new("PATH")).is_some());
        // HOME is set where the command runs, from the user's /etc/passwd.
        assert_eq!(env.len(), 2, "nothing from the host leaked in: {env:?}");
    }

    #[test]
    fn bare_env_key_copies_from_the_shell_or_is_skipped() {
        // HOME is set on any real host and never equals our default ("/" or
        // "/root" for root), so a match proves the value was really copied.
        let Some(host_home) = std::env::var_os("HOME") else {
            return;
        };
        let env = spec(&["-e", "HOME", "-e", "SHIPYARD_SURELY_UNSET_9F3A", "true"]).env;
        assert_eq!(env.get(OsStr::new("HOME")), Some(host_home.as_os_str()));
        assert!(env.get(OsStr::new("SHIPYARD_SURELY_UNSET_9F3A")).is_none());
    }

    #[test]
    fn env_errors_never_echo_values() {
        for entry in ["=hunter2", "A=b\0hunter2"] {
            let err = parse(&["-e", entry, "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{entry}: {err:#}");
        }
    }

    #[test]
    fn malformed_bare_env_keys_are_rejected_not_skipped() {
        for entry in ["", "TOKEN hunter2"] {
            let err = parse(&["-e", entry, "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
        }
    }

    #[test]
    fn stop_policy() {
        let stop = parse(&["--stop-signal", "INT", "--stop-timeout", "3", "true"])
            .unwrap()
            .into_invocation()
            .unwrap()
            .stop;
        assert_eq!(stop.signal, "SIGINT".parse().unwrap());
        assert_eq!(stop.timeout, Duration::from_secs(3));
        let default = parse(&["true"]).unwrap().into_invocation().unwrap().stop;
        assert_eq!(default, StopPolicy::default());
    }

    #[test]
    fn capability_flags_build_dockers_rule() {
        let security = |args: &[&str]| parse(args).unwrap().security().unwrap();
        let rule = |args: &[&str]| match security(args).capabilities {
            Capabilities::Default { kept, added } => (kept, added),
            other => panic!("{other:?}"),
        };
        let cap = |name: &str| name.parse::<Capability>().unwrap();
        assert_eq!(
            rule(&["true"]),
            (CapabilitySet::DOCKER_DEFAULT, CapabilitySet::EMPTY)
        );
        let (kept, added) = rule(&["--cap-drop", "chown", "--cap-add", "CAP_SYS_ADMIN", "true"]);
        assert!(!kept.contains(cap("CHOWN")) && kept.contains(cap("KILL")));
        assert_eq!(added.iter().collect::<Vec<_>>(), [cap("SYS_ADMIN")]);
        assert_eq!(rule(&["--cap-drop", "ALL", "true"]).0, CapabilitySet::EMPTY);
        assert_eq!(rule(&["--cap-add", "all", "true"]).1, CapabilitySet::ALL);
        // Dropping everything, then adding one: just that one.
        let (kept, added) = rule(&["--cap-drop", "ALL", "--cap-add", "KILL", "true"]);
        assert!(kept.is_empty() && added.contains(cap("KILL")));
        assert!(security(&["true"]).no_new_privileges);
        for (option, on) in [
            ("no-new-privileges", true),
            ("no-new-privileges=true", true),
            ("no-new-privileges:false", false),
            ("no-new-privileges=false", false),
        ] {
            assert_eq!(
                security(&["--security-opt", option, "true"]).no_new_privileges,
                on,
                "{option}"
            );
        }
    }

    #[test]
    fn seccomp_options_choose_the_filter() {
        let seccomp = |args: &[&str]| parse(args).unwrap().security().map(|s| s.seccomp);
        assert_eq!(seccomp(&["true"]).unwrap(), Seccomp::Default);
        assert_eq!(
            seccomp(&["--security-opt", "seccomp=unconfined", "true"]).unwrap(),
            Seccomp::Unconfined
        );
        let dir = std::env::temp_dir().join(format!("shipyard-seccomp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, json: &str| {
            let path = dir.join(name);
            std::fs::write(&path, json).unwrap();
            format!("seccomp={}", path.display())
        };
        let good = file(
            "good.json",
            r#"{"defaultAction": "SCMP_ACT_ERRNO",
                "syscalls": [{"names": ["read"], "action": "SCMP_ACT_ALLOW"}]}"#,
        );
        let Seccomp::Profile(profile) = seccomp(&["--security-opt", &good, "true"]).unwrap() else {
            panic!("not a profile");
        };
        assert_eq!(profile.syscalls[0].names, ["read"]);
        // The last one wins, as with Docker.
        assert_eq!(
            seccomp(&[
                "--security-opt",
                &good,
                "--security-opt",
                "seccomp:unconfined",
                "true"
            ])
            .unwrap(),
            Seccomp::Unconfined
        );
        // Mistakes name the file, and what's wrong.
        for (name, json, wanted) in [
            (
                "typo.json",
                r#"{"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [{"names": ["raed"], "action": "SCMP_ACT_ALLOW"}]}"#,
                "\"raed\" isn't a system call",
            ),
            (
                "field.json",
                r#"{"defaultAction": "SCMP_ACT_ERRNO", "allowAll": true}"#,
                "unknown field `allowAll`",
            ),
            ("broken.json", "{", "EOF while parsing"),
        ] {
            let option = file(name, json);
            let err = format!(
                "{:#}",
                seccomp(&["--security-opt", &option, "true"]).unwrap_err()
            );
            assert!(err.contains(name) && err.contains(wanted), "{err}");
        }
        let missing = format!(
            "{:#}",
            seccomp(&["--security-opt", "seccomp=/no/such.json", "true"]).unwrap_err()
        );
        assert!(
            missing.contains("seccomp profile /no/such.json"),
            "{missing}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(parse(&["--security-opt", "seccomp=", "true"]).is_err());
        assert!(parse(&["--security-opt", "seccomp", "true"]).is_err());
        assert!(
            parse(&[
                "--no-isolation",
                "--security-opt",
                "seccomp=unconfined",
                "true"
            ])
            .is_err()
        );
        assert!(parse(&["--bundle", ".", "--security-opt", "seccomp=unconfined"]).is_err());
    }

    #[test]
    fn id_map_flags_parse_and_conflict() {
        assert!(
            parse(&["--gidmap", "0:1:1", "true"]).is_err(),
            "--gidmap needs --uidmap"
        );
        assert!(parse(&["--uidmap", "0:1", "true"]).is_err());
        assert!(parse(&["--no-isolation", "--uidmap", "0:1:1", "true"]).is_err());
        assert!(parse(&["--bundle", ".", "--uidmap", "0:1:1"]).is_err());
        if !cfg!(target_os = "linux") {
            let err = parse(&["--uidmap", "0:100000:65536", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("--uidmap needs Linux"), "{err}");
        }
    }

    #[test]
    fn the_oom_score_adjustment_is_checked() {
        if !cfg!(target_os = "linux") {
            let err = parse(&["--oom-score-adj", "5", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("needs Linux"), "{err}");
            return;
        }
        assert_eq!(
            spec(&["--oom-score-adj", "-500", "true"]).oom_score_adj,
            Some(-500)
        );
        let err = parse(&["--oom-score-adj", "1001", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("between -1000 and 1000"), "{err}");
        assert!(parse(&["--bundle", ".", "--oom-score-adj", "5"]).is_err());
    }

    #[test]
    fn limit_flags_need_a_container() {
        for limit in [
            &["--memory", "64m"][..],
            &["--pids-limit", "5"],
            &["--cpus", "1"],
        ] {
            let args = [&["--no-isolation"], limit, &["true"]].concat();
            assert!(parse(&args).is_err(), "{args:?}");
            let args = [&["--bundle", "."], limit].concat();
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let limited = parse(&["-m", "64m", "-c", "512", "true"]).unwrap();
        assert_eq!(limited.command, ["true"], "-m and -c are ours");
        if !cfg!(target_os = "linux") {
            let err = limited.into_invocation().unwrap_err();
            assert!(
                err.to_string().contains("--memory needs Linux cgroups"),
                "{err}"
            );
        }
    }

    /// Root's user namespace is the one `--uidmap` asks for (if any);
    /// anyone else always gets one, with maps chosen for them, and may not
    /// pass `--uidmap`.
    #[test]
    fn the_user_namespace_depends_on_who_runs() {
        let range = |s: &str| s.parse::<IdMap>().unwrap();
        let chosen = || RootlessMaps {
            maps: IdMaps::single((1000, 1000)),
            shortfall: Some("short".to_owned()),
        };
        let namespace =
            |args: &[&str], root: bool| parse(args).unwrap().user_namespace(root, chosen);

        let (maps, short) = namespace(&["--uidmap", "0:100000:65536", "true"], true).unwrap();
        let maps = maps.unwrap();
        assert_eq!(maps.uids, [range("0:100000:65536")]);
        assert_eq!(maps.gids, maps.uids, "--gidmap defaults to --uidmap");
        assert_eq!(short, None);
        let args = [
            "--uidmap",
            "0:100000:1",
            "--uidmap",
            "1:200000:65535",
            "--gidmap",
            "0:300000:65536",
            "true",
        ];
        let both = namespace(&args, true).unwrap().0.unwrap();
        assert_eq!(both.uids.len(), 2);
        assert_eq!(both.gids, [range("0:300000:65536")]);
        assert_eq!(namespace(&["true"], true).unwrap(), (None, None), "rootful");

        let (maps, short) = namespace(&["true"], false).unwrap();
        assert_eq!(maps, Some(IdMaps::single((1000, 1000))), "rootless, always");
        assert_eq!(short.as_deref(), Some("short"));
        let err = namespace(&["--uidmap", "0:100000:65536", "true"], false).unwrap_err();
        assert!(err.to_string().contains("need root"), "{err}");
    }

    #[test]
    fn capability_flags_are_checked() {
        assert!(parse(&["--cap-add", "FLY", "true"]).is_err());
        assert!(parse(&["--security-opt", "apparmor=x", "true"]).is_err());
        assert!(parse(&["--security-opt", "no-new-privileges=maybe", "true"]).is_err());
        for flag in ["--cap-add", "--cap-drop"] {
            assert!(
                parse(&["--no-isolation", flag, "KILL", "true"]).is_err(),
                "{flag}"
            );
            assert!(parse(&["--bundle", ".", flag, "KILL"]).is_err(), "{flag}");
        }
        let both = parse(&["--cap-add", "KILL", "--cap-drop", "kill", "true"]).unwrap();
        assert!(
            both.security()
                .unwrap_err()
                .to_string()
                .contains("both name CAP_KILL")
        );
        if !cfg!(target_os = "linux") {
            let err = parse(&["--cap-add", "KILL", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("--cap-add needs Linux"), "{err}");
        }
    }

    /// `--no-isolation` is suggested only where it could replace what was
    /// asked: never for limits or device flags, which it can't be combined
    /// with.
    #[test]
    fn no_isolation_replaces_only_bare_namespaces() {
        assert!(only_namespaces(&Isolation::standard()));
        let mut limited = Isolation::standard();
        limited.resources.pids_max = Some(1);
        assert!(!only_namespaces(&limited));
        let mut devices = Isolation::standard();
        devices.devices.asked = true;
        assert!(!only_namespaces(&devices));
    }

    #[test]
    fn isolation_is_the_default_on_linux_only() {
        let isolation = |args: &[&str]| parse(args).unwrap().into_invocation().unwrap().isolation;
        assert_eq!(
            isolation(&["true"]).is_some(),
            cfg!(target_os = "linux"),
            "isolated by default exactly on Linux"
        );
        assert_eq!(isolation(&["--no-isolation", "true"]), None);
        if cfg!(target_os = "linux") {
            let named = isolation(&["--hostname", "box", "true"]).unwrap();
            assert_eq!(named.hostname.as_deref(), Some("box"));
        }
    }

    #[test]
    fn a_root_filesystem_is_canonical_and_checked() {
        let isolation = |args: &[&str]| parse(args).unwrap().into_invocation();
        // Contradictions, whatever the system.
        assert!(parse(&["--no-isolation", "--rootfs", "/", "true"]).is_err());
        assert!(
            parse(&["--read-only", "true"]).is_err(),
            "needs --rootfs or --image"
        );
        assert!(parse(&["--read-only", "--image", "alpine", "true"]).is_ok());
        let missing = isolation(&["--rootfs", "/no/such/dir", "true"]).unwrap_err();
        assert!(format!("{missing:#}").contains("--rootfs"), "{missing:#}");
        if cfg!(target_os = "linux") {
            assert!(
                format!("{missing:#}").contains("/no/such/dir"),
                "{missing:#}"
            );
            let dir = std::env::temp_dir();
            let given = dir.join(".").join("");
            let invocation =
                isolation(&["--rootfs", given.to_str().unwrap(), "--read-only", "true"]).unwrap();
            let root = invocation.isolation.unwrap().root.unwrap();
            assert_eq!(root.path, std::fs::canonicalize(dir).unwrap());
            assert!(root.read_only);
        }
    }

    #[test]
    fn a_bundle_is_the_whole_description() {
        assert!(parse(&["--bundle", "/b"]).is_ok(), "no COMMAND needed");
        for conflicting in [
            &["--bundle", "/b", "true"][..],
            &["--bundle", "/b", "-e", "A=1"],
            &["--bundle", "/b", "-w", "/tmp"],
            &["--bundle", "/b", "--rootfs", "/r"],
            &["--bundle", "/b", "--no-isolation"],
        ] {
            assert!(parse(conflicting).is_err(), "{conflicting:?}");
        }
        assert!(parse(&[]).is_err(), "a COMMAND or a bundle");
        let missing = parse(&["--bundle", "/no/such/bundle"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        let expected = if cfg!(target_os = "linux") {
            "/no/such/bundle/config.json"
        } else {
            "--bundle"
        };
        assert!(format!("{missing:#}").contains(expected), "{missing:#}");
    }

    #[test]
    fn hostname_problems_are_usage_errors() {
        // Both: together they contradict each other.
        assert!(parse(&["--no-isolation", "--hostname", "box", "true"]).is_err());
        // An invalid name, or (macOS) no namespaces to give it to.
        let err = parse(&["--hostname", "bad_name", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(format!("{err:#}").contains("hostname"), "{err:#}");
    }

    proptest! {
        #[test]
        fn user_parser_never_panics(s in any::<String>()) {
            let _ = parse_user(&s);
        }

        #[test]
        fn valid_ids_round_trip(uid in 0..u32::MAX, gid in 0..u32::MAX) {
            prop_assert_eq!(
                parse_user(&format!("{uid}:{gid}")),
                Ok((uid.to_string(), Some(gid.to_string())))
            );
        }
    }
}
