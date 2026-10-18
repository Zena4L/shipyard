//! `doctor`'s checks on Linux: each a pure function of what was found
//! (tested apart), and the functions that find it on this host.

use std::io::Read as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use nix::errno::Errno;
use shipyard_runtime::cgroup::{self, Controller};
use shipyard_runtime::process::probe::{Probe, Step};

use super::probe::{PROBE_ARG, decode};
use super::{Check, Level, User};

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

/// Who runs the checks, and the checks, in the order a run meets them.
pub(super) fn checks(engine: &Engine) -> (User, Vec<Check>) {
    let rootless = !nix::unistd::geteuid().is_root();
    let uid = nix::unistd::getuid();
    let user = User {
        uid: uid.as_raw(),
        rootless,
    };
    // As a run finds it (an absolute path: the probe looks above it).
    let store = engine.paths().image_store().and_then(|store| {
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
    checks.push(network(engine, rootless));
    checks.extend(firewall(rootless));
    checks.push(state());
    checks.extend(held_off_check(crate::run::restarts_held_off(engine)));
    // Containers after the last logout, and after a reboot (stage 9,
    // step 9.5c): where systemd runs.
    if Path::new("/run/systemd/system").is_dir() {
        let with_policy = with_restart_policy(engine);
        let kill = kill_user_processes_live()
            .or_else(|| kill_user_processes(&logind_configs()))
            .unwrap_or(false);
        if rootless {
            let lingering = lingering(uid);
            checks.push(linger_check(lingering, with_policy));
            checks.extend(logout_check(kill, lingering));
        } else {
            checks.extend(sudo_logout_check(kill, engine.paths().started_by_sudo()));
        }
        checks.extend(boot_unit(
            rootless,
            with_policy,
            longest_stop_timeout(engine),
        ));
        checks.extend(boot_stores_check(
            with_policy,
            engine.paths().store_overrides(),
            rootless,
        ));
    }
    (user, checks)
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

pub(super) fn controllers_check(base: &Path, delegated: &[Controller], rootless: bool) -> Check {
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
                format!("--uidmap with an image needs Linux 6.8 or later, and this is {release}"),
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
fn network(engine: &Engine, rootless: bool) -> Check {
    const NAME: &str = "network";
    if rootless {
        return pasta_check(
            shipyard_network::pasta::find(engine.paths().program_path()).as_deref(),
        );
    }
    if !crate::network::host_root() {
        return Check::new(
            Level::Warn,
            NAME,
            "containers get the host's network: this root isn't the host's (a user \
             namespace of its own)",
        );
    }
    if let Some(why) = crate::network::why_no_network() {
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
    if rootless || !crate::network::host_root() || crate::network::why_no_network().is_some() {
        return None;
    }
    let uplink = crate::network::default_route_interface(shipyard_network::Family::V4);
    // The default network as `shipyard network create` would make it:
    // IPv4 only, with no DNS server of its own.
    let view = shipyard_network::firewalls::NetworkView {
        bridge: shipyard_network::store::DEFAULT_BRIDGE,
        uplink: uplink.as_deref(),
        uplink6: None,
        outward: true,
        ipv6: false,
        bridged: crate::network::bridged_traffic_filtered(),
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
        .map(crate::network::describe_conflict)
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
        Check::new(Level::Warn, NAME, open.join("\n"))
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
/// the stop unit's stop at a shutdown should leave it so, until the boot;
/// a stop run by hand leaves containers that end unrestarted.
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
fn with_restart_policy(engine: &Engine) -> usize {
    crate::containers::list(engine).map_or(0, |records| {
        records
            .iter()
            .filter(|record| record.config.restart != crate::containers::RestartPolicy::No)
            .count()
    })
}

/// The longest stop timeout among the containers a shutdown would stop
/// (up now, or with a restart policy that brings them back), in seconds:
/// they're all stopped at once, and each waits its own.
fn longest_stop_timeout(engine: &Engine) -> Option<u64> {
    crate::containers::list(engine)
        .ok()?
        .iter()
        .filter(|record| {
            record.config.restart != crate::containers::RestartPolicy::No
                || crate::containers::state(engine, record).is_ok_and(|state| state.is_active())
        })
        .map(|record| record.config.stop_timeout)
        .max()
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

/// Whether the boot units (contrib/systemd) are enabled, whether the
/// recover unit failed, and the stop unit's stop timeout against the
/// longest of the containers' (`longest`): root's, or (rootless) the
/// user's, as `systemctl` says (an install with `--global` included).
fn boot_unit(rootless: bool, with_policy: usize, longest: Option<u64>) -> Vec<Check> {
    let ask = |args: &[&str]| {
        let mut command = Command::new("systemctl");
        if rootless {
            command.arg("--user");
        }
        command
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let enabled = |unit: &str| {
        matches!(
            ask(&["is-enabled", unit]).as_str(),
            "enabled" | "enabled-runtime"
        )
    };
    // Installed is enough: the recover unit wants it, so it's started with
    // it, enabled or not.
    let stop_installed = ask(&["show", "-p", "LoadState", "--value", STOP_UNIT]) == "loaded";
    let units = BootUnits {
        recover: enabled(RECOVER_UNIT),
        stop: stop_installed,
        failed: ask(&["is-active", RECOVER_UNIT]) == "failed",
    };
    let mut checks = vec![boot_unit_check(units, with_policy, rootless)];
    if stop_installed {
        let unit = parse_timespan(&ask(&[
            "show",
            "-p",
            "TimeoutStopUSec",
            "--value",
            STOP_UNIT,
        ]));
        let unit = unit.map(|time| StopLimit {
            time,
            by_user_manager: false,
        });
        let limit = if rootless {
            earliest(
                unit,
                user_manager_stop_timeout().map(|time| StopLimit {
                    time,
                    by_user_manager: true,
                }),
            )
        } else {
            unit
        };
        checks.extend(stop_timeout_check(limit, longest, rootless));
    }
    checks
}

/// This user's manager's stop timeout: it's a unit of the system's
/// (`user@UID.service`), which kills it at its own (120 s upstream), so a
/// user's stop unit can't outlast it.
fn user_manager_stop_timeout() -> Option<Duration> {
    let output = Command::new("systemctl")
        .args(["show", "-p", "TimeoutStopUSec", "--value"])
        .arg(format!("user@{}.service", nix::unistd::getuid()))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_timespan(String::from_utf8_lossy(&output.stdout).trim())
}

/// The earlier of two limits, where either is known.
fn earliest(one: Option<StopLimit>, other: Option<StopLimit>) -> Option<StopLimit> {
    match (one, other) {
        (Some(one), Some(other)) => Some(if other.time < one.time { other } else { one }),
        (one, other) => one.or(other),
    }
}

/// How long a shutdown's stop may take, and whose limit that is.
#[derive(Debug, Clone, Copy)]
pub(super) struct StopLimit {
    /// How long.
    pub(super) time: Duration,
    /// A user's manager's own stop timeout (`user@UID.service`, the
    /// system's), not the stop unit's: on Ubuntu 24.04, 5 s.
    pub(super) by_user_manager: bool,
}

/// The unit that starts the containers at boot (contrib/systemd).
const RECOVER_UNIT: &str = "shipyard-recover.service";

/// The unit that stops them at a shutdown (F52: a unit of its own, so
/// that a shutdown while the boot's start is still going still stops them).
const STOP_UNIT: &str = "shipyard-stop.service";

/// What `systemctl` says of the boot units.
#[derive(Debug, Clone, Copy)]
pub(super) struct BootUnits {
    /// The recover unit is enabled.
    pub(super) recover: bool,
    /// The stop unit is installed (the recover unit wants it: started
    /// with it, enabled or not).
    pub(super) stop: bool,
    /// The recover unit failed.
    pub(super) failed: bool,
}

/// A systemd timespan as `systemctl show` prints it (`5min`, `1min 30s`,
/// `1.500s`, `2w 1d`): `None` for `infinity` or anything else it can't
/// read (then nothing is said of it).
pub(super) fn parse_timespan(text: &str) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut any = false;
    for part in text.split_whitespace() {
        let digits = part
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(part.len());
        let (number, unit) = part.split_at(digits);
        let number: f64 = number.parse().ok()?;
        let seconds = match unit {
            "us" => 1e-6,
            "ms" => 1e-3,
            "s" | "" => 1.0,
            "min" => 60.0,
            "h" => 3_600.0,
            "d" => 86_400.0,
            "w" => 604_800.0,
            "month" => 2_629_800.0,
            "y" => 31_557_600.0,
            _ => return None,
        };
        total = total.checked_add(Duration::try_from_secs_f64(number * seconds).ok()?)?;
        any = true;
    }
    any.then_some(total)
}

/// What a shutdown's stop needs after a container's own SIGKILL: its end
/// settled and recorded before the unit's timeout kills the stop itself.
const STOP_SETTLE: Duration = Duration::from_secs(10);

/// The shutdown's stop limit (`limit`: the stop unit's, or for a user the
/// earlier of it and the user manager's own) against the longest of the
/// containers' stop timeouts (`longest`, seconds), with [`STOP_SETTLE`]
/// after it: at a shutdown they're all stopped at once, and one still in
/// its timeout when the limit is up is killed by systemd before its own
/// (F46). Said with the fix for whichever limit it is; nothing when either
/// isn't known (`infinity`: never cut short).
pub(super) fn stop_timeout_check(
    limit: Option<StopLimit>,
    longest: Option<u64>,
    rootless: bool,
) -> Option<Check> {
    let (limit, longest) = (limit?, longest?);
    (Duration::from_secs(longest).saturating_add(STOP_SETTLE) > limit.time).then(|| {
        let text = if limit.by_user_manager {
            format!(
                "a container's stop timeout ({longest}s) isn't within your user manager's \
                 ({}s: the system ends `user@UID.service` then, at a shutdown or your last \
                 logout): it's killed before its own; raise it for every user: `sudo systemctl \
                 edit user@.service`, then `TimeoutStopSec=` above it under `[Service]`",
                limit.time.as_secs(),
            )
        } else {
            format!(
                "a container's stop timeout ({longest}s) isn't within the stop unit's ({}s): \
                 at a shutdown it's killed before its own; raise the unit's: `systemctl {}edit \
                 shipyard-stop`, then `TimeoutStopSec=` above it under `[Service]`",
                limit.time.as_secs(),
                if rootless { "--user " } else { "" },
            )
        };
        Check::new(Level::Warn, "stop at shutdown", text)
    })
}

/// Containers with a restart policy on stores the environment moved
/// (`overrides`): the boot unit, run with systemd's environment, looks in
/// the default ones and won't find them (F44). Said with the fix: the
/// same variables given to the unit.
pub(super) fn boot_stores_check(
    with_policy: usize,
    overrides: &[&str],
    rootless: bool,
) -> Option<Check> {
    (with_policy > 0 && !overrides.is_empty()).then(|| {
        Check::new(
            Level::Warn,
            "boot's stores",
            format!(
                "{with_policy} {} with a restart policy {} on stores this environment sets \
                 ({}): the boot units run with systemd's environment and won't find them; give \
                 them the same: `systemctl {}edit shipyard-recover`, then \
                 `Environment=NAME=VALUE` for each under `[Service]`, and the same for \
                 `shipyard-stop`",
                if with_policy == 1 {
                    "container"
                } else {
                    "containers"
                },
                if with_policy == 1 { "is" } else { "are" },
                overrides.join(", "),
                if rootless { "--user " } else { "" },
            ),
        )
    })
}

/// The boot units' line: a failed one's journal is the user's for a
/// rootless user unit (`journalctl --user`); the recover unit without the
/// stop unit (an install from before 10.2d1, one file) stops nothing at a
/// shutdown (F52).
pub(super) fn boot_unit_check(units: BootUnits, with_policy: usize, rootless: bool) -> Check {
    const NAME: &str = "restart at boot";
    let BootUnits {
        recover: enabled,
        stop,
        failed,
    } = units;
    if enabled && !failed && !stop {
        return Check::new(
            Level::Warn,
            NAME,
            "the recover unit is enabled, but the stop unit isn't installed: at a shutdown \
             containers are ended without their stop signal; install shipyard-stop.service \
             beside it (see contrib/systemd), then `systemctl daemon-reload`",
        );
    }
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
            bail!(Runtime, "the probe didn't finish within {PROBE_DEADLINE:?}");
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
        bail!(Runtime, "the probe ended without an answer ({status})");
    }
    decode(line.trim()).ok_or_else(|| Error::new(ErrorKind::Runtime, "the probe answered nonsense"))
}
