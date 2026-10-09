//! `shipyard doctor`: whether this host can run containers, and what to do
//! where it can't.
//!
//! Each check is a line: `ok`; `warn`, when something is limited but a plain
//! `shipyard run` works; `fail`, when a plain `run`, or `run --image`,
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
    /// A plain `run`, or `run --image`, fails here.
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
            "{:<5} {:<16} {}",
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

    use super::{Check, Level};

    /// The hidden first argument the probe helper is started with.
    const PROBE_ARG: &str = "__shipyard-probe";

    /// How long the probe may take: it makes namespaces and mounts, which
    /// take milliseconds.
    const PROBE_DEADLINE: Duration = Duration::from_secs(5);

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
        let store = crate::image::store_dir().and_then(|store| {
            std::path::absolute(&store).with_context(|| format!("finding {}", store.display()))
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
        checks.push(state());
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
                shipyard_network::pasta::find(std::env::var_os("PATH").as_deref()).as_deref(),
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

    /// Where `shipyard-runtime` keeps its containers' state (`shipyard run`
    /// does without).
    fn state() -> Check {
        const NAME: &str = "runtime state";
        match shipyard_runtime::container::default_root() {
            Ok(root) => Check::new(Level::Ok, NAME, root.display().to_string()),
            Err(err) => Check::new(
                Level::Warn,
                NAME,
                format!("shipyard-runtime (the OCI runtime) can't keep containers here: {err}"),
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

        use super::super::Level;
        use super::super::linux::{
            ProbedAt, controllers_check, decode, encode, left_by_gone, mounts, overlay_check,
            pasta_check, user_namespaces,
        };

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
    }
}
