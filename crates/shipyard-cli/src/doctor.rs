//! `shipyard doctor`: whether this host can run containers, and what to do
//! where it can't.
//!
//! Each check is a line: `ok`; `warn`, when something is limited but a plain
//! `shipyard run` works; `fail`, when a plain `run --no-image`, or `run IMAGE`,
//! fails here. The exit code is 1 if a check failed, so a script can ask.
//!
//! Some answers only trying can give. A host's settings can allow a user
//! namespace that a security policy (AppArmor's restriction, Ubuntu's
//! default since 23.10) then takes the capabilities from; an image store on
//! a filesystem overlayfs can't write to (inside another container) shows
//! nothing until a mount fails. So on Linux, doctor starts a probe helper
//! (this binary again, as `__shipyard-probe`) that makes a user namespace,
//! a mount namespace, a tmpfs and an overlay, as a run would, in a scratch
//! directory next to the image store. Everything it made goes with it.
#![cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "off Linux, nothing is checked: the checks are Linux's"
    )
)]

use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Result;

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Works.
    Ok,
    /// Limited: a plain `run` works, something asked for may not.
    Warn,
    /// A plain `run --no-image`, or `run IMAGE`, fails here.
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One line of the report.
#[derive(Debug, PartialEq, Eq)]
struct Check {
    level: Level,
    /// What's checked: "network".
    name: &'static str,
    /// What was found, and the fix where it isn't `ok`.
    text: String,
}

impl Check {
    fn new(level: Level, name: &'static str, text: impl Into<String>) -> Self {
        Check {
            level,
            name,
            text: text.into(),
        }
    }
}

/// A report line's columns: the level, the check's name; a text's next
/// lines start under its first.
const LEVEL_WIDTH: usize = 5;
const NAME_WIDTH: usize = 16;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the checks are Linux's")
)]
const INDENT: usize = LEVEL_WIDTH + 1 + NAME_WIDTH + 1;

/// The report's last line, and the exit code: 1 if any check failed.
fn verdict(checks: &[Check]) -> (String, ExitCode) {
    let count = |level| checks.iter().filter(|check| check.level == level).count();
    let (fails, warns) = (count(Level::Fail), count(Level::Warn));
    if fails > 0 {
        (
            format!(
                "containers can't run here as they are: {fails} {} to fix",
                plural(fails, "problem", "problems")
            ),
            ExitCode::FAILURE,
        )
    } else if warns > 0 {
        (
            format!(
                "containers run here; {warns} {} limited",
                plural(warns, "thing", "things")
            ),
            ExitCode::SUCCESS,
        )
    } else {
        ("containers run here".to_owned(), ExitCode::SUCCESS)
    }
}

/// `one` for 1, else `many`.
fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// `shipyard doctor`.
pub(crate) fn run() -> Result<ExitCode> {
    let backend = match shipyard_runtime::host_backend() {
        shipyard_runtime::Backend::Native => "native (host kernel)",
        shipyard_runtime::Backend::LinuxVm => "linux-vm (not implemented yet)",
        _ => "unknown",
    };
    // `writeln!` instead of `println!`: a closed stdout becomes an error,
    // not a panic.
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "host:      {} / {}{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        kernel()
    )?;
    writeln!(out, "backend:   {backend}")?;
    let checks = checks(&mut out)?;
    if checks.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out)?;
    for check in &checks {
        writeln!(
            out,
            "{:<LEVEL_WIDTH$} {:<NAME_WIDTH$} {}",
            check.level.label(),
            check.name,
            check.text
        )?;
    }
    let (last, code) = verdict(&checks);
    writeln!(out, "\n{last}")?;
    Ok(code)
}

/// The kernel's release, for the header (Linux).
#[cfg(target_os = "linux")]
fn kernel() -> String {
    format!(" (kernel {})", shipyard_runtime::kernel::release())
}

#[cfg(not(target_os = "linux"))]
fn kernel() -> String {
    String::new()
}

/// Off Linux, there's nothing to isolate with yet, so nothing to check.
#[cfg(not(target_os = "linux"))]
fn checks(out: &mut impl std::io::Write) -> Result<Vec<Check>> {
    writeln!(
        out,
        "isolation: none on this system yet: commands run as ordinary processes, until \
         Shipyard runs them in a Linux VM of its own (v0.4)"
    )?;
    Ok(Vec::new())
}

/// Runs this process as the probe helper, if that's what it was started as;
/// otherwise returns at once. Called first thing in `main`.
#[cfg(not(target_os = "linux"))]
pub(crate) fn probe_entry_point() {}

#[cfg(target_os = "linux")]
use linux::checks;
#[cfg(target_os = "linux")]
pub(crate) use linux::probe_entry_point;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result, bail};
    use nix::errno::Errno;
    use shipyard_runtime::cgroup::{self, Controller};
    use shipyard_runtime::process::probe::{Probe, Step};

    use super::{Check, INDENT, Level};

    /// The hidden first argument the probe helper is started with.
    const PROBE_ARG: &str = "__shipyard-probe";

    /// How long the probe may take: it makes namespaces and mounts, which
    /// take milliseconds, but a first start on a busy machine with a cold
    /// page cache (a large binary read from disk again) can take seconds.
    const PROBE_DEADLINE: Duration = Duration::from_secs(15);

    /// The controllers limits need (`cpuset` is only for `--cpuset-*`).
    const LIMITS_NEED: [Controller; 4] = [
        Controller::Cpu,
        Controller::Io,
        Controller::Memory,
        Controller::Pids,
    ];

    /// The checks, in the order a run meets them; the `user` line is
    /// written to `out` first.
    pub(super) fn checks(out: &mut impl std::io::Write) -> Result<Vec<Check>> {
        let rootless = !nix::unistd::geteuid().is_root();
        let uid = nix::unistd::getuid();
        writeln!(
            out,
            "user:      {uid} ({})",
            if rootless { "rootless" } else { "root" }
        )?;
        // As a run finds it (an absolute path: the probe looks above it).
        let store = shipyard_engine::Paths::from_env()
            .image_store()
            .map_err(anyhow::Error::from)
            .and_then(|store| {
                std::path::absolute(store).with_context(|| format!("finding {}", store.display()))
            });
        // The overlay is probed only where a run's would go, and only where
        // nobody else could swap the scratch directory for a link.
        let place = store.as_ref().ok().map(|store| probe_place(store));
        let probed = match (&store, &place) {
            (Ok(store), Some(Ok(dir))) => Some(probe_at(store, dir, rootless)),
            _ => None,
        };
        let mut checks = Vec::new();
        // The probe tries the user namespace (rootless), mounts and overlay:
        // where it couldn't run at all, that's a line of its own.
        if let Some(Err(err)) = &probed {
            checks.push(Check::new(
                Level::Fail,
                "probe",
                format!("couldn't try what a run needs: {err:#}"),
            ));
        }
        if rootless {
            if let Some(Ok((probe, _))) = &probed {
                checks.extend(user_namespaces(
                    probe,
                    shipyard_runtime::process::userns_restriction().as_deref(),
                ));
            }
            checks.push(id_mapping());
        }
        if let Some(Ok((probe, _))) = &probed {
            checks.extend(mounts(probe, rootless));
        }
        checks.push(cgroups(rootless));
        checks.extend(images(&store, place.as_ref(), probed.as_ref(), rootless));
        checks.push(network(rootless));
        checks.extend(firewall(rootless));
        checks.push(state());
        checks.extend(held_off_check(shipyard_engine::run::restarts_held_off(
            &crate::engine(),
        )));
        // Containers after the last logout, and after a reboot (stage 9,
        // step 9.5c): where systemd runs.
        if Path::new("/run/systemd/system").is_dir() {
            let with_policy = with_restart_policy();
            let kill = kill_user_processes_live()
                .or_else(|| kill_user_processes(&logind_configs()))
                .unwrap_or(false);
            if rootless {
                let lingering = lingering(uid);
                checks.push(linger_check(lingering, with_policy));
                checks.extend(logout_check(kill, lingering));
            } else {
                checks.extend(sudo_logout_check(
                    kill,
                    std::env::var_os("SUDO_USER").is_some(),
                ));
            }
            checks.push(boot_unit(rootless, with_policy));
        }
        Ok(checks)
    }

    // --- The checks ---------------------------------------------------------

    /// Whether a user namespace can be made, and has its privileges.
    pub(super) fn user_namespaces(probe: &Probe, restriction: Option<&str>) -> Option<Check> {
        const NAME: &str = "user namespaces";
        let why = restriction
            .unwrap_or("a security policy, or a container without privileges, confines them");
        match (probe.user_namespace?, probe.mount) {
            (Err(errno), _) => Some(Check::new(
                Level::Fail,
                NAME,
                format!("refused ({}): {why}", errno.desc()),
            )),
            (Ok(()), Some(Err(Errno::EPERM | Errno::EACCES))) => Some(Check::new(
                Level::Fail,
                NAME,
                format!("one is made, but without privileges in it: {why}"),
            )),
            (Ok(()), _) => Some(Check::new(Level::Ok, NAME, "allowed (probed)")),
        }
    }

    /// Whether a mount namespace, and a mount in it, can be had (a run's
    /// isolation needs both). A rootless refusal for want of privileges is
    /// the user namespace's line.
    pub(super) fn mounts(probe: &Probe, rootless: bool) -> Option<Check> {
        let Some(Err(errno)) = probe.mount else {
            return None;
        };
        if rootless && matches!(errno, Errno::EPERM | Errno::EACCES) {
            return None;
        }
        Some(Check::new(
            Level::Fail,
            "mounts",
            format!(
                "a mount namespace, or a mount in it, is refused ({}): isolated runs fail here \
                 (inside a container without privileges, or under a security policy); \
                 --no-isolation runs without",
                errno.desc()
            ),
        ))
    }

    /// The IDs a rootless container gets.
    fn id_mapping() -> Check {
        id_mapping_of(&shipyard_runtime::process::rootless_id_maps())
    }

    pub(super) fn id_mapping_of(maps: &shipyard_runtime::isolation::RootlessMaps) -> Check {
        const NAME: &str = "ID mapping";
        if let Some(short) = &maps.shortfall {
            return Check::new(Level::Warn, NAME, short.clone());
        }
        // The user's own ID, then their subordinate ranges.
        let more: u64 = maps
            .maps
            .uids
            .iter()
            .skip(1)
            .map(|map| u64::from(map.size))
            .sum();
        Check::new(
            Level::Ok,
            NAME,
            format!(
                "your ID and {more} more from /etc/subuid and /etc/subgid (newuidmap and \
                 newgidmap)"
            ),
        )
    }

    /// Whether limits can be had: a cgroup v2 tree this user may write,
    /// probed by making and removing a cgroup in it, with the controllers
    /// limits need.
    fn cgroups(rootless: bool) -> Check {
        const NAME: &str = "cgroups";
        const REFUSED: &str = "resource limits (--memory, --cpus, --pids-limit, …) are refused";
        let tree = match cgroup::Tree::find() {
            Ok(tree) => tree,
            Err(why) => return Check::new(Level::Warn, NAME, format!("{why}: {REFUSED}")),
        };
        let parent = tree.base().join("shipyard");
        if let Ok(entries) = std::fs::read_dir(&parent) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| left_by_gone(name, CGROUP))
                {
                    // Empty: nothing ever runs in one.
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }
        let scratch = Path::new("shipyard").join(unique(CGROUP));
        match tree.create(&scratch) {
            Ok(made) => {
                let _ = made.remove(Duration::from_secs(1));
            }
            Err(err) => {
                return Check::new(Level::Warn, NAME, format!("{err}: {REFUSED}"));
            }
        }
        let delegated = std::fs::read_to_string(tree.base().join("cgroup.controllers"))
            .map(|text| cgroup::parse_controllers(&text))
            .unwrap_or_default();
        controllers_check(tree.base(), &delegated, rootless)
    }

    pub(super) fn controllers_check(
        base: &Path,
        delegated: &[Controller],
        rootless: bool,
    ) -> Check {
        const NAME: &str = "cgroups";
        let missing: Vec<&str> = LIMITS_NEED
            .iter()
            .filter(|controller| !delegated.contains(controller))
            .map(|controller| controller.name())
            .collect();
        let place = if rootless {
            format!("v2, delegated at {}", base.display())
        } else {
            format!("v2 at {}", base.display())
        };
        if missing.is_empty() {
            Check::new(Level::Ok, NAME, place)
        } else {
            Check::new(
                Level::Warn,
                NAME,
                format!(
                    "{place}, without the {} controller{}: limits that need {} are refused",
                    missing.join(", "),
                    if missing.len() == 1 { "" } else { "s" },
                    if missing.len() == 1 { "it" } else { "them" }
                ),
            )
        }
    }

    /// Whether overlayfs mounts where the image store is (the probe), and
    /// for root, whether the kernel can map an image for `--uidmap`.
    fn images(
        store: &Result<PathBuf>,
        place: Option<&Result<PathBuf, String>>,
        probed: Option<&Result<(Probe, ProbedAt)>>,
        rootless: bool,
    ) -> Vec<Check> {
        const NAME: &str = "images";
        let mut checks = Vec::new();
        let store = match store {
            Ok(store) => store,
            Err(err) => {
                return vec![Check::new(
                    Level::Fail,
                    NAME,
                    format!("no image store: {err:#}"),
                )];
            }
        };
        // What a run's opening the store would say of it.
        match shipyard_image::Store::check_root(store) {
            Err(err) => checks.push(Check::new(Level::Fail, NAME, err.to_string())),
            Ok(Some(warning)) => checks.push(Check::new(Level::Warn, NAME, warning)),
            Ok(None) => {}
        }
        match (place, probed) {
            (Some(Err(why)), _) => checks.push(Check::new(
                Level::Warn,
                NAME,
                format!("overlayfs wasn't tried: {why}"),
            )),
            (_, Some(Ok((probe, at)))) => {
                checks.extend(overlay_check(probe.overlay, at, rootless));
            }
            // The probe's own line says why.
            _ => {}
        }
        if !rootless {
            let release = shipyard_runtime::kernel::release();
            if shipyard_runtime::kernel::version(&release).is_some_and(|v| v < (6, 8)) {
                checks.push(Check::new(
                    Level::Warn,
                    NAME,
                    format!(
                        "--uidmap with an image needs Linux 6.8 or later, and this is {release}"
                    ),
                ));
            }
        }
        checks
    }

    /// Where the overlay was probed: the store, or (before the first pull)
    /// the nearest directory above it that exists.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct ProbedAt {
        pub(super) store: PathBuf,
        pub(super) dir: PathBuf,
    }

    pub(super) fn overlay_check(overlay: Step, at: &ProbedAt, rootless: bool) -> Option<Check> {
        const NAME: &str = "images";
        let place = if at.dir == at.store {
            format!("in the store ({})", at.store.display())
        } else {
            format!(
                "where the store will be ({}, not made yet: probed in {})",
                at.store.display(),
                at.dir.display()
            )
        };
        match overlay? {
            Ok(()) => Some(Check::new(
                Level::Ok,
                NAME,
                format!("overlayfs works {place}"),
            )),
            Err(errno) => Some(Check::new(
                Level::Fail,
                NAME,
                format!(
                    "overlayfs can't mount {place}: {}",
                    shipyard_runtime::process::overlay_refusal(errno, rootless)
                ),
            )),
        }
    }

    /// A rootless container's network (pasta), or root's (a bridge, with
    /// `nf_tables`).
    fn network(rootless: bool) -> Check {
        const NAME: &str = "network";
        if rootless {
            return pasta_check(
                shipyard_network::pasta::find(shipyard_engine::Paths::from_env().program_path())
                    .as_deref(),
            );
        }
        if !shipyard_engine::network::host_root() {
            return Check::new(
                Level::Warn,
                NAME,
                "containers get the host's network: this root isn't the host's (a user \
                 namespace of its own)",
            );
        }
        if let Some(why) = shipyard_engine::network::why_no_network() {
            return Check::new(
                Level::Warn,
                NAME,
                format!("containers get the host's network: the bridge network needs {why}"),
            );
        }
        match shipyard_network::nft::Nftables::new().and_then(|mut nft| nft.ruleset()) {
            Ok(_) => Check::new(Level::Ok, NAME, "bridge networks, with nf_tables"),
            // A run sets its firewall up first, and fails without one.
            Err(err) => Check::new(
                Level::Fail,
                NAME,
                format!(
                    "nf_tables can't be used ({err}): runs on a bridge network fail; \
                     --network host or --network none run without"
                ),
            ),
        }
    }

    /// Other programs' firewalls that would drop the default network's
    /// traffic (root's bridge network: a rootless one's goes through pasta,
    /// the host's own sockets). Nothing where there's no bridge network.
    fn firewall(rootless: bool) -> Option<Check> {
        if rootless
            || !shipyard_engine::network::host_root()
            || shipyard_engine::network::why_no_network().is_some()
        {
            return None;
        }
        let uplink =
            shipyard_engine::network::default_route_interface(shipyard_network::Family::V4);
        // The default network as `shipyard network create` would make it:
        // IPv4 only, with no DNS server of its own.
        let view = shipyard_network::firewalls::NetworkView {
            bridge: shipyard_network::store::DEFAULT_BRIDGE,
            uplink: uplink.as_deref(),
            uplink6: None,
            outward: true,
            ipv6: false,
            bridged: shipyard_engine::network::bridged_traffic_filtered(),
            dns: None,
        };
        let conflicts = shipyard_network::nft::Nftables::new()
            .and_then(|mut nft| shipyard_network::firewalls::conflicts(&mut nft, &view))
            .map_err(|err| err.to_string());
        Some(firewall_check(conflicts.as_deref().map_err(String::as_str)))
    }

    pub(super) fn firewall_check(
        conflicts: Result<&[shipyard_network::firewalls::Conflict], &str>,
    ) -> Check {
        const NAME: &str = "firewall";
        let conflicts = match conflicts {
            Ok(conflicts) => conflicts,
            Err(err) => {
                return Check::new(Level::Warn, NAME, format!("couldn't be read: {err}"));
            }
        };
        let open: Vec<String> = conflicts
            .iter()
            .filter(|conflict| !conflict.fixed)
            .map(shipyard_engine::network::describe_conflict)
            .collect();
        if open.is_empty() {
            Check::new(
                Level::Ok,
                NAME,
                "no other firewall in nftables drops the containers' traffic (iptables-legacy's \
                 rules can't be read)",
            )
        } else {
            // A plain run works; its containers can't reach out. One
            // finding per line, under the first one's text.
            Check::new(Level::Warn, NAME, open.join(&format!("\n{:INDENT$}", "")))
        }
    }

    pub(super) fn pasta_check(pasta: Option<&Path>) -> Check {
        const NAME: &str = "network";
        match pasta {
            Some(pasta) => Check::new(Level::Ok, NAME, format!("pasta ({})", pasta.display())),
            None => Check::new(
                Level::Fail,
                NAME,
                "a rootless container's network needs pasta: install the passt package, or \
                 run with --network host (the host's network) or --network none",
            ),
        }
    }

    /// Where containers' runtime state is kept: `run -d`, `create`, `start`
    /// and foreground image runs as containers need it (without it, a run
    /// is supervised by `run` itself, with no record).
    fn state() -> Check {
        const NAME: &str = "runtime state";
        match shipyard_runtime::container::default_root() {
            Ok(root) => Check::new(Level::Ok, NAME, root.display().to_string()),
            Err(err) => Check::new(
                Level::Warn,
                NAME,
                format!(
                    "containers can't be kept here (no `run -d`, `create` or `start`; a run is \
                     supervised by `run` itself): {err}"
                ),
            ),
        }
    }

    /// Whether `shipyard system stop` still holds restart policies off
    /// (`held`): it ran, and `shipyard system recover` hasn't since. Only
    /// the boot unit's stop should leave it so, until the boot; a stop run
    /// by hand leaves containers that end unrestarted.
    pub(super) fn held_off_check(held: bool) -> Option<Check> {
        held.then(|| {
            Check::new(
                Level::Warn,
                "restarts",
                "held off: `shipyard system stop` ran and `shipyard system recover` hasn't \
                 since, so a container that ends isn't restarted (the boot unit runs it at the \
                 next boot; if the host isn't shutting down, run it yourself)",
            )
        })
    }

    /// Whether user `uid`'s systemd user manager lingers (runs without a
    /// login session: `loginctl enable-linger`): logind's marker file.
    fn lingering(uid: nix::unistd::Uid) -> bool {
        nix::unistd::User::from_uid(uid)
            .ok()
            .flatten()
            .is_some_and(|user| {
                Path::new("/var/lib/systemd/linger")
                    .join(user.name)
                    .exists()
            })
    }

    pub(super) fn linger_check(lingering: bool, with_policy: usize) -> Check {
        const NAME: &str = "linger";
        match (lingering, with_policy) {
            (true, _) => Check::new(
                Level::Ok,
                NAME,
                "on: your user manager runs from boot (the user unit starts your containers then)",
            ),
            (false, 0) => Check::new(
                Level::Ok,
                NAME,
                "off (no container has a restart policy that would need it)",
            ),
            (false, _) => Check::new(
                Level::Warn,
                NAME,
                "off: after a reboot your containers come back only once you log in, and those \
                 the user unit starts end at your last logout (`sudo loginctl enable-linger $USER`)",
            ),
        }
    }

    /// logind's settings files, as it reads them: its main one, then its
    /// drop-ins in name order (one in `/etc` hides one of the same name in
    /// `/run` or `/usr/lib`).
    fn logind_configs() -> Vec<String> {
        let mut configs: Vec<String> = std::fs::read_to_string("/etc/systemd/logind.conf")
            .into_iter()
            .collect();
        let mut drop_ins = std::collections::BTreeMap::new();
        for dir in [
            "/usr/lib/systemd/logind.conf.d",
            "/run/systemd/logind.conf.d",
            "/etc/systemd/logind.conf.d",
        ] {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "conf") {
                    // Later directories hide earlier ones' of the same name.
                    drop_ins.insert(entry.file_name(), path);
                }
            }
        }
        configs.extend(
            drop_ins
                .values()
                .filter_map(|path| std::fs::read_to_string(path).ok()),
        );
        configs
    }

    /// Whether logind ends a login session's processes at logout, as it
    /// says itself (its `KillUserProcesses` property, over D-Bus): what's in
    /// effect, whatever its build's default. `None` if it can't be asked.
    fn kill_user_processes_live() -> Option<bool> {
        let output = Command::new("busctl")
            .args([
                "get-property",
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "KillUserProcesses",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        match String::from_utf8_lossy(&output.stdout).trim() {
            "b true" => Some(true),
            "b false" => Some(false),
            _ => None,
        }
    }

    /// Whether logind's settings files (`configs`, in the order it reads
    /// them) set `KillUserProcesses` in its `[Login]` section, and to what:
    /// the last setting wins; `None` if none does (then its build decides:
    /// upstream's default is `yes`, most distributions' `no`).
    pub(super) fn kill_user_processes(configs: &[String]) -> Option<bool> {
        let mut kill = None;
        for config in configs {
            let mut login = false;
            for line in config.lines().map(str::trim) {
                if line.starts_with('[') {
                    login = line == "[Login]";
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                if login && key.trim() == "KillUserProcesses" {
                    kill = match value.trim().to_ascii_lowercase().as_str() {
                        "yes" | "y" | "true" | "t" | "on" | "1" => Some(true),
                        "no" | "n" | "false" | "f" | "off" | "0" => Some(false),
                        _ => kill,
                    };
                }
            }
        }
        kill
    }

    /// A container's monitor stays in the cgroup of the command that made
    /// it: made in a login session, it's in the session's scope, which logind
    /// ends at logout with `KillUserProcesses=yes` (linger or not; seen on
    /// Ubuntu 24.04, stage 9, step 9.5c). Under the user manager it isn't.
    pub(super) fn logout_check(kill_user_processes: bool, lingering: bool) -> Option<Check> {
        let fix = if lingering {
            "start them under your user manager: `systemd-run --user --scope shipyard run -d …`"
        } else {
            "`sudo loginctl enable-linger $USER`, then start them under your user manager: \
             `systemd-run --user --scope shipyard run -d …`"
        };
        kill_user_processes.then(|| {
            Check::new(
                Level::Warn,
                "logout",
                format!(
                    "logind ends a login session's processes at logout (KillUserProcesses=yes): \
                     containers made in one end with it ({fix})"
                ),
            )
        })
    }

    /// How many containers have a restart policy (one that isn't `no`).
    fn with_restart_policy() -> usize {
        shipyard_engine::containers::list(&crate::engine()).map_or(0, |records| {
            records
                .iter()
                .filter(|record| {
                    record.config.restart != shipyard_engine::containers::RestartPolicy::No
                })
                .count()
        })
    }

    /// Root's side of [`logout_check`]: a container made with `sudo` from a
    /// user's login session has its monitor in that session's scope, and
    /// logind ends it at the user's logout with `KillUserProcesses=yes` (its
    /// `KillExcludeUsers=root` spares root's own sessions only; seen on
    /// Ubuntu 24.04, stage 9, step 9.5c).
    pub(super) fn sudo_logout_check(kill_user_processes: bool, sudo: bool) -> Option<Check> {
        (kill_user_processes && sudo).then(|| {
            Check::new(
                Level::Warn,
                "logout",
                "logind ends a login session's processes at logout (KillUserProcesses=yes): \
                 containers made with sudo from your session end with it (start them outside \
                 it: `sudo systemd-run --scope shipyard run -d …`; the boot unit's are)",
            )
        })
    }

    /// Whether the boot unit (contrib/systemd) is enabled, and whether it
    /// failed: root's, or (rootless) the user's, as `systemctl` says (an
    /// install with `--global` included).
    fn boot_unit(rootless: bool, with_policy: usize) -> Check {
        let ask = |what: &str| {
            let mut command = Command::new("systemctl");
            if rootless {
                command.arg("--user");
            }
            command
                .args([what, "shipyard-recover.service"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .unwrap_or_default()
        };
        let enabled = matches!(ask("is-enabled").as_str(), "enabled" | "enabled-runtime");
        let failed = ask("is-active") == "failed";
        boot_unit_check(enabled, failed, with_policy, rootless)
    }

    /// The boot unit's line: a failed one's journal is the user's for a
    /// rootless user unit (`journalctl --user`).
    pub(super) fn boot_unit_check(
        enabled: bool,
        failed: bool,
        with_policy: usize,
        rootless: bool,
    ) -> Check {
        const NAME: &str = "restart at boot";
        match (enabled, failed, with_policy) {
            (true, true, _) => Check::new(
                Level::Warn,
                NAME,
                format!(
                    "the boot unit failed: see `journalctl {}-u shipyard-recover`",
                    if rootless { "--user " } else { "" }
                ),
            ),
            (true, false, _) => Check::new(Level::Ok, NAME, "the boot unit is enabled"),
            (false, _, 0) => Check::new(
                Level::Ok,
                NAME,
                "the boot unit isn't enabled (no container has a restart policy)",
            ),
            (false, _, count) => Check::new(
                Level::Warn,
                NAME,
                format!(
                    "{count} {} a restart policy, and none comes back after a reboot: the boot \
                     unit isn't enabled (see contrib/systemd)",
                    super::plural(count, "container has", "containers have")
                ),
            ),
        }
    }

    // --- The probe, the caller's side --------------------------------------

    /// Where to probe the overlay: the store, or (before the first pull)
    /// the nearest directory above it that exists; only one that's this
    /// user's, or sticky (`/tmp`): in anyone else's, its owner could swap
    /// the scratch directory for a link while the probe runs, and have
    /// this process work on what it leads to.
    fn probe_place(store: &Path) -> Result<PathBuf, String> {
        use std::os::unix::fs::MetadataExt as _;
        let dir = store
            .ancestors()
            .find(|dir| dir.is_dir())
            .ok_or_else(|| format!("nothing above {} exists", store.display()))?;
        let meta = std::fs::metadata(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let mine = meta.uid() == nix::unistd::geteuid().as_raw();
        if mine || meta.mode() & 0o1000 != 0 {
            Ok(dir.to_path_buf())
        } else {
            Err(format!(
                "{} belongs to user {}, not to you",
                dir.display(),
                meta.uid()
            ))
        }
    }

    /// The prefix of the probe's scratch directories and cgroups, for this
    /// process: `.shipyard-doctor-PID-`, `doctor-PID-`.
    const SCRATCH: &str = ".shipyard-doctor-";
    const CGROUP: &str = "doctor-";

    /// A name no other doctor has: this process's ID, and the time.
    fn unique(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        format!("{prefix}{}-{nanos}", std::process::id())
    }

    /// Whether the doctor that named something `PREFIX`PID`-…` is gone
    /// (killed before it cleaned up).
    pub(super) fn left_by_gone(name: &str, prefix: &str) -> bool {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<i32>().ok())
            .is_some_and(|pid| {
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) == Err(Errno::ESRCH)
            })
    }

    /// Runs the probe helper with a scratch directory in `dir` (see
    /// [`probe_place`]), removed again after, as are those of doctors
    /// killed before they could.
    fn probe_at(store: &Path, dir: &Path, rootless: bool) -> Result<(Probe, ProbedAt)> {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name
                    .to_str()
                    .is_some_and(|name| left_by_gone(name, SCRATCH))
                {
                    remove_scratch(&entry.path());
                }
            }
        }
        let scratch = dir.join(unique(SCRATCH));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&scratch)
            .with_context(|| format!("making {}", scratch.display()))?;
        let probed = (|| {
            for sub in ["lower", "upper", "work", "merged", "tmp"] {
                std::fs::create_dir(scratch.join(sub))
                    .with_context(|| format!("making {}", scratch.join(sub).display()))?;
            }
            run_probe(&scratch, rootless)
        })();
        remove_scratch(&scratch);
        Ok((
            probed?,
            ProbedAt {
                store: store.to_path_buf(),
                dir: dir.to_path_buf(),
            },
        ))
    }

    /// Removes the probe's scratch directory. Overlay made `work/work` in
    /// it, mode 0 (its own scratch, kept from everyone): opened up first, or
    /// it can't be listed to be emptied (root reads it anyway).
    fn remove_scratch(scratch: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(
            scratch.join("work/work"),
            std::fs::Permissions::from_mode(0o700),
        );
        let _ = std::fs::remove_dir_all(scratch);
    }

    /// Starts the helper, and reads its one line, within the deadline.
    fn run_probe(scratch: &Path, rootless: bool) -> Result<Probe> {
        let mut helper = Command::new("/proc/self/exe")
            .arg0("shipyard-probe")
            .arg(PROBE_ARG)
            .arg(scratch)
            .arg(if rootless { "rootless" } else { "root" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the probe")?;
        let deadline = Instant::now() + PROBE_DEADLINE;
        let status = loop {
            if let Some(status) = helper.try_wait().context("waiting for the probe")? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = helper.kill();
                let _ = helper.wait();
                bail!("the probe didn't finish within {PROBE_DEADLINE:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut line = String::new();
        if let Some(mut stdout) = helper.stdout.take() {
            stdout
                .read_to_string(&mut line)
                .context("reading the probe")?;
        }
        if !status.success() {
            bail!("the probe ended without an answer ({status})");
        }
        decode(line.trim()).context("the probe answered nonsense")
    }

    /// One step as the helper writes it: `-` (not tried), `0` (worked), or
    /// the errno.
    fn encode_step(step: Step) -> String {
        match step {
            None => "-".to_owned(),
            Some(Ok(())) => "0".to_owned(),
            Some(Err(errno)) => (errno as i32).to_string(),
        }
    }

    fn decode_step(text: &str) -> Option<Step> {
        Some(match text {
            "-" => None,
            "0" => Some(Ok(())),
            errno => Some(Err(Errno::from_raw(errno.parse().ok()?))),
        })
    }

    /// The helper's line: `probe USERNS MOUNT OVERLAY`.
    pub(super) fn encode(probe: &Probe) -> String {
        format!(
            "probe {} {} {}",
            encode_step(probe.user_namespace),
            encode_step(probe.mount),
            encode_step(probe.overlay)
        )
    }

    pub(super) fn decode(line: &str) -> Option<Probe> {
        let mut words = line.split(' ');
        if words.next()? != "probe" {
            return None;
        }
        let probe = Probe {
            user_namespace: decode_step(words.next()?)?,
            mount: decode_step(words.next()?)?,
            overlay: decode_step(words.next()?)?,
        };
        words.next().is_none().then_some(probe)
    }

    // --- The probe, the helper's side --------------------------------------

    /// Runs this process as the probe helper, if that's what it was started
    /// as; otherwise returns at once. **Call it first thing in `main`**,
    /// before anything starts a thread: making a user namespace needs a
    /// single-threaded process.
    pub(crate) fn probe_entry_point() {
        let args: Vec<OsString> = std::env::args_os().collect();
        if args.get(1).map(OsString::as_os_str) != Some(PROBE_ARG.as_ref()) {
            return;
        }
        // As for the CLI itself: a privileged start (setuid, file
        // capabilities) gets no further, not even to its arguments.
        if let Err(err) = shipyard_runtime::process::refuse_privileged_start() {
            let _ = writeln!(std::io::stderr(), "shipyard-probe: {err}");
            std::process::exit(125);
        }
        let (Some(scratch), Some(rootless)) = (
            args.get(2),
            args.get(3).and_then(|mode| match mode.to_str()? {
                "rootless" => Some(true),
                "root" => Some(false),
                _ => None,
            }),
        ) else {
            let _ = writeln!(std::io::stderr(), "shipyard-probe: bad arguments");
            std::process::exit(125);
        };
        let probe = shipyard_runtime::process::probe::probe(Path::new(scratch), rootless);
        let code = i32::from(writeln!(std::io::stdout(), "{}", encode(&probe)).is_err());
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(level: Level) -> Check {
        Check::new(level, "x", "")
    }

    #[test]
    fn the_verdict_counts_what_failed_and_what_is_limited() {
        let (text, code) = verdict(&[check(Level::Ok), check(Level::Ok)]);
        assert_eq!(text, "containers run here");
        assert_eq!(code, ExitCode::SUCCESS);
        let (text, code) = verdict(&[check(Level::Warn), check(Level::Ok), check(Level::Warn)]);
        assert_eq!(text, "containers run here; 2 things limited");
        assert_eq!(code, ExitCode::SUCCESS);
        let (text, code) = verdict(&[check(Level::Warn), check(Level::Fail)]);
        assert_eq!(
            text,
            "containers can't run here as they are: 1 problem to fix"
        );
        assert_eq!(code, ExitCode::FAILURE);
        let (text, _) = verdict(&[check(Level::Fail), check(Level::Fail)]);
        assert_eq!(
            text,
            "containers can't run here as they are: 2 problems to fix"
        );
        let (text, _) = verdict(&[check(Level::Warn)]);
        assert_eq!(text, "containers run here; 1 thing limited");
        assert_eq!(verdict(&[]).1, ExitCode::SUCCESS);
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use std::path::{Path, PathBuf};

        use nix::errno::Errno;
        use shipyard_runtime::cgroup::Controller;
        use shipyard_runtime::process::probe::Probe;

        use super::super::linux::{
            ProbedAt, boot_unit_check, controllers_check, decode, encode, firewall_check,
            held_off_check, kill_user_processes, left_by_gone, linger_check, logout_check, mounts,
            overlay_check, pasta_check, sudo_logout_check, user_namespaces,
        };
        use super::super::{INDENT, Level};

        /// A refused mount fails doctor, whoever runs it, unless it's a
        /// rootless user namespace's want of privileges (that line's).
        #[test]
        fn a_refused_mount_namespace_fails() {
            let refused = |errno| Probe {
                user_namespace: None,
                mount: Some(Err(errno)),
                overlay: None,
            };
            let root = mounts(&refused(Errno::EPERM), false).unwrap();
            assert_eq!(root.level, Level::Fail);
            assert!(root.text.contains("--no-isolation"), "{}", root.text);
            assert_eq!(mounts(&refused(Errno::EPERM), true), None);
            assert_eq!(mounts(&refused(Errno::EACCES), true), None);
            assert_eq!(
                mounts(&refused(Errno::EINVAL), true).unwrap().level,
                Level::Fail
            );
            let fine = Probe {
                mount: Some(Ok(())),
                ..Probe::default()
            };
            assert_eq!(mounts(&fine, false), None);
            assert_eq!(mounts(&Probe::default(), false), None);
        }

        /// A gone doctor's leftovers are recognised by its process ID; a
        /// live one's (this process) and other names aren't.
        #[test]
        fn leftovers_are_a_gone_doctors() {
            let live = format!(".shipyard-doctor-{}-1", std::process::id());
            assert!(!left_by_gone(&live, ".shipyard-doctor-"));
            // Beyond any PID the kernel hands out (pid_max is at most 2^22).
            assert!(left_by_gone(
                ".shipyard-doctor-99999999-1",
                ".shipyard-doctor-"
            ));
            assert!(left_by_gone("doctor-99999999-1", "doctor-"));
            assert!(!left_by_gone("doctor-99999999", "doctor-"));
            assert!(!left_by_gone("run", "doctor-"));
            assert!(!left_by_gone(".shipyard-doctor-x-1", ".shipyard-doctor-"));
        }

        #[test]
        fn the_probes_line_reads_back() {
            for probe in [
                Probe::default(),
                Probe {
                    user_namespace: Some(Ok(())),
                    mount: Some(Err(Errno::EPERM)),
                    overlay: None,
                },
                Probe {
                    user_namespace: None,
                    mount: Some(Ok(())),
                    overlay: Some(Err(Errno::EINVAL)),
                },
            ] {
                assert_eq!(decode(&encode(&probe)), Some(probe));
            }
            for nonsense in [
                "",
                "probe",
                "probe 0 0",
                "probe 0 0 0 0",
                "prob 0 0 0",
                "probe x 0 0",
            ] {
                assert_eq!(decode(nonsense), None, "{nonsense:?}");
            }
        }

        /// Refused outright, or made without privileges (AppArmor's
        /// restriction): both fail, naming the setting; root's isn't
        /// checked.
        #[test]
        fn a_user_namespace_without_privileges_fails() {
            let restriction = Some("AppArmor restricts them");
            let refused = Probe {
                user_namespace: Some(Err(Errno::EPERM)),
                ..Probe::default()
            };
            let check = user_namespaces(&refused, restriction).unwrap();
            assert_eq!(check.level, Level::Fail);
            assert!(
                check.text.contains("AppArmor restricts them"),
                "{}",
                check.text
            );
            // Ubuntu 24.04's restriction refuses with EACCES.
            let stripped_eacces = Probe {
                user_namespace: Some(Ok(())),
                mount: Some(Err(Errno::EACCES)),
                overlay: None,
            };
            let check = user_namespaces(&stripped_eacces, restriction).unwrap();
            assert_eq!(check.level, Level::Fail);
            assert!(check.text.contains("without privileges"), "{}", check.text);
            let stripped = Probe {
                user_namespace: Some(Ok(())),
                mount: Some(Err(Errno::EPERM)),
                overlay: None,
            };
            let check = user_namespaces(&stripped, restriction).unwrap();
            assert_eq!(check.level, Level::Fail);
            assert!(check.text.contains("without privileges"), "{}", check.text);
            let stripped_unexplained = user_namespaces(&stripped, None).unwrap();
            assert!(
                stripped_unexplained.text.contains("security policy"),
                "{}",
                stripped_unexplained.text
            );
            // A mount refused for another reason isn't the namespace's.
            let fine = Probe {
                user_namespace: Some(Ok(())),
                mount: Some(Err(Errno::ENODEV)),
                overlay: None,
            };
            assert_eq!(user_namespaces(&fine, None).unwrap().level, Level::Ok);
            assert_eq!(user_namespaces(&Probe::default(), None), None);
        }

        #[test]
        fn overlay_says_where_it_was_probed() {
            let store = PathBuf::from("/home/u/.local/share/shipyard/images");
            let there = ProbedAt {
                store: store.clone(),
                dir: store.clone(),
            };
            let check = overlay_check(Some(Ok(())), &there, true).unwrap();
            assert_eq!(check.level, Level::Ok);
            assert!(check.text.contains("in the store"), "{}", check.text);
            let above = ProbedAt {
                store,
                dir: PathBuf::from("/home/u/.local/share"),
            };
            let check = overlay_check(Some(Err(Errno::EINVAL)), &above, true).unwrap();
            assert_eq!(check.level, Level::Fail);
            assert!(check.text.contains("not made yet"), "{}", check.text);
            assert!(
                check.text.contains("SHIPYARD_IMAGE_STORE"),
                "{}",
                check.text
            );
            assert_eq!(overlay_check(None, &above, true), None);
        }

        #[test]
        fn missing_controllers_limit_but_dont_fail() {
            let base = Path::new("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service");
            let all = [
                Controller::Cpu,
                Controller::Io,
                Controller::Memory,
                Controller::Pids,
            ];
            assert_eq!(controllers_check(base, &all, true).level, Level::Ok);
            let check = controllers_check(base, &[Controller::Memory, Controller::Pids], true);
            assert_eq!(check.level, Level::Warn);
            assert!(
                check.text.contains("without the cpu, io controllers"),
                "{}",
                check.text
            );
            assert!(
                check.text.ends_with("need them are refused"),
                "{}",
                check.text
            );
            let one = controllers_check(
                base,
                &[Controller::Cpu, Controller::Memory, Controller::Pids],
                true,
            );
            assert!(
                one.text.contains("without the io controller:"),
                "{}",
                one.text
            );
            assert!(one.text.ends_with("need it are refused"), "{}", one.text);
        }

        #[test]
        fn rootless_without_pasta_fails() {
            assert_eq!(
                pasta_check(Some(Path::new("/usr/bin/pasta"))).level,
                Level::Ok
            );
            let check = pasta_check(None);
            assert_eq!(check.level, Level::Fail);
            assert!(check.text.contains("passt"), "{}", check.text);
        }

        #[test]
        fn restarts_held_off_is_a_warning() {
            assert!(held_off_check(false).is_none());
            let held = held_off_check(true).unwrap();
            assert_eq!(held.level, Level::Warn);
            assert!(
                held.text.contains("shipyard system recover"),
                "{}",
                held.text
            );
        }

        #[test]
        fn linger_off_is_a_warning() {
            assert_eq!(linger_check(true, 2).level, Level::Ok);
            // Nothing that needs it: not a warning.
            assert_eq!(linger_check(false, 0).level, Level::Ok);
            let off = linger_check(false, 1);
            assert_eq!(off.level, Level::Warn);
            assert!(off.text.contains("loginctl enable-linger"), "{}", off.text);
            assert!(off.text.contains("reboot"), "{}", off.text);
        }

        #[test]
        fn kill_user_processes_is_read_as_logind_reads_it() {
            let configs = |texts: &[&str]| {
                texts
                    .iter()
                    .map(|&text| text.to_owned())
                    .collect::<Vec<_>>()
            };
            // Unset, commented out, or another section's: not said.
            assert_eq!(kill_user_processes(&[]), None);
            assert_eq!(
                kill_user_processes(&configs(&["[Login]\n#KillUserProcesses=yes\n"])),
                None
            );
            assert_eq!(
                kill_user_processes(&configs(&["[Other]\nKillUserProcesses=yes\n"])),
                None
            );
            assert_eq!(
                kill_user_processes(&configs(&["[Login]\nKillUserProcesses=yes\n"])),
                Some(true)
            );
            // Spaces around `=`, any case.
            assert_eq!(
                kill_user_processes(&configs(&["[Login]\nKillUserProcesses = No\n"])),
                Some(false)
            );
            // The last setting wins: a drop-in after the main file.
            assert_eq!(
                kill_user_processes(&configs(&[
                    "[Login]\nKillUserProcesses=yes\n",
                    "[Login]\nKillUserProcesses=no\n",
                ])),
                Some(false)
            );
            assert_eq!(
                kill_user_processes(&configs(&[
                    "[Login]\nKillUserProcesses=no\n",
                    "[Login]\n KillUserProcesses=yes \n",
                ])),
                Some(true)
            );
            // Said whenever logind ends a session's processes: linger
            // doesn't keep a monitor that's in a session's scope.
            assert!(logout_check(true, false).is_some());
            let lingering = logout_check(true, true).unwrap();
            assert!(
                lingering.text.contains("systemd-run --user"),
                "{}",
                lingering.text
            );
            assert!(logout_check(false, false).is_none());
            assert!(logout_check(false, true).is_none());
            // Root: only through sudo, from a user's session.
            let sudo = sudo_logout_check(true, true).unwrap();
            assert!(sudo.text.contains("systemd-run --scope"), "{}", sudo.text);
            assert!(sudo_logout_check(true, false).is_none());
            assert!(sudo_logout_check(false, true).is_none());
        }

        #[test]
        fn a_restart_policy_without_the_boot_unit_is_a_warning() {
            assert_eq!(boot_unit_check(true, false, 3, false).level, Level::Ok);
            assert_eq!(boot_unit_check(false, false, 0, true).level, Level::Ok);
            let missing = boot_unit_check(false, false, 2, false);
            assert_eq!(missing.level, Level::Warn);
            assert!(missing.text.contains("contrib/systemd"), "{}", missing.text);
            assert!(
                missing.text.contains("2 containers have"),
                "{}",
                missing.text
            );
            let failed = boot_unit_check(true, true, 1, false);
            assert_eq!(failed.level, Level::Warn);
            assert!(
                failed.text.contains("`journalctl -u shipyard-recover`"),
                "{}",
                failed.text
            );
            // A rootless user unit's journal is the user's.
            let user = boot_unit_check(true, true, 1, true);
            assert_eq!(user.level, Level::Warn);
            assert!(
                user.text
                    .contains("`journalctl --user -u shipyard-recover`"),
                "{}",
                user.text
            );
        }

        #[test]
        fn another_firewall_in_the_way_is_a_warning_until_fixed() {
            use shipyard_network::firewalls::{Blocks, Conflict, Firewall};
            let firewalld = |fixed| Conflict {
                firewall: Firewall::Firewalld,
                bridge: "shipyard0".to_owned(),
                blocks: Blocks {
                    way_out: true,
                    ..Blocks::default()
                },
                dns: None,
                fixed,
                fix: vec!["sudo firewall-cmd --reload".to_owned()],
            };
            assert_eq!(firewall_check(Ok(&[])).level, Level::Ok);
            assert_eq!(firewall_check(Ok(&[firewalld(true)])).level, Level::Ok);
            let check = firewall_check(Ok(&[firewalld(false), firewalld(false)]));
            assert_eq!(check.level, Level::Warn);
            assert_eq!(
                check.text.lines().count(),
                2,
                "one per line: {}",
                check.text
            );
            assert!(
                check
                    .text
                    .lines()
                    .nth(1)
                    .unwrap()
                    .starts_with(&" ".repeat(INDENT)),
                "{}",
                check.text
            );
            assert_eq!(firewall_check(Err("EPERM")).level, Level::Warn);
        }
    }
}
