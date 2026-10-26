//! A container's user namespace, from the parent's side: which maps a
//! caller gets (stage 4, step 5), and writing them (steps 4 and 5).
//!
//! Anyone may create a user namespace (step 1's probe, U1) and be root in
//! it, over what it owns. Only its maps take care:
//!
//! - root may write any maps, straight to `/proc/PID/uid_map` and `gid_map`;
//! - anyone else may map their own ID alone, after denying `setgroups`
//!   (U3);
//! - more takes the setuid helpers `newuidmap` and `newgidmap`, which hand
//!   out the ranges root granted the user in `/etc/subuid` and
//!   `/etc/subgid`: how a rootless container gets 65536 IDs.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag};
use nix::unistd::{Pid, User, getgid, getuid};

use crate::id_map::{render, subordinate_ranges};
use crate::isolation::{IdMap, IdMaps, Isolation, MapKind, Namespace, RootlessMaps};
use crate::{Error, Result};

/// The maps for a container of the calling user: theirs plus their
/// subordinate ranges when the helpers and the ranges are there, else their
/// own ID alone, with the reason.
#[must_use]
pub fn rootless_id_maps() -> RootlessMaps {
    let (uid, gid) = (getuid().as_raw(), getgid().as_raw());
    let name = User::from_uid(getuid())
        .ok()
        .flatten()
        .map(|user| user.name);
    let ranges = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .map(|text| subordinate_ranges(&text, name.as_deref(), id))
            .unwrap_or_default()
    };
    let (subuids, subgids) = (ranges("/etc/subuid", uid), ranges("/etc/subgid", gid));
    choose_rootless_maps(
        (uid, gid),
        &name.unwrap_or_else(|| uid.to_string()),
        HELPERS.iter().all(|helper| find_helper(helper).is_some()),
        &subuids,
        &subgids,
    )
}

/// Makes a bundle's `isolation` runnable by this caller: without root, and
/// without a user namespace of its own (the bundle doesn't ask for one), it
/// gets one, with the caller's maps ([`rootless_id_maps`]). A user namespace
/// only adds confinement. Returns what to tell the user: that one was added,
/// and why its maps are short if they are. Root, or an isolation that has
/// one, is left as it is.
pub(crate) fn bundle_user_namespace(isolation: &mut Isolation) -> Option<String> {
    add_user_namespace(
        isolation,
        nix::unistd::geteuid().is_root(),
        rootless_id_maps,
    )
}

fn add_user_namespace(
    isolation: &mut Isolation,
    root: bool,
    rootless: impl FnOnce() -> RootlessMaps,
) -> Option<String> {
    if root || isolation.has(Namespace::User) {
        return None;
    }
    let RootlessMaps { maps, shortfall } = rootless();
    isolation.namespaces.push(Namespace::User);
    isolation.id_maps = Some(maps);
    let added = "the bundle has no user namespace, which running without root needs: one was \
                 added (root inside is you)";
    Some(match shortfall {
        Some(short) => format!("{added}; {short}"),
        None => added.to_owned(),
    })
}

/// The setuid helpers that map subordinate IDs.
const HELPERS: [&str; 2] = ["newuidmap", "newgidmap"];

/// The full maps if `helpers` are there and the user `who` has ranges,
/// else the single ID with what's missing.
fn choose_rootless_maps(
    ids: (u32, u32),
    who: &str,
    helpers: bool,
    subuids: &[(u32, u32)],
    subgids: &[(u32, u32)],
) -> RootlessMaps {
    let missing = if !helpers {
        Some(format!(
            "{} aren't on PATH (the uidmap package)",
            HELPERS.join(" and ")
        ))
    } else if subuids.is_empty() {
        Some(format!("{who} has no subordinate user IDs in /etc/subuid"))
    } else if subgids.is_empty() {
        Some(format!("{who} has no subordinate group IDs in /etc/subgid"))
    } else {
        None
    };
    let short = |why: String| RootlessMaps {
        maps: IdMaps::single(ids),
        shortfall: Some(format!(
            "only root inside the container is mapped (to you): {why}; for 65536 IDs, \
             install uidmap and add `{who}:100000:65536` to /etc/subuid and /etc/subgid"
        )),
    };
    if let Some(why) = missing {
        return short(why);
    }
    let maps = IdMaps::rootless(ids, subuids, subgids);
    // Ranges the kernel would refuse (one overlapping the user's own ID, or
    // running past the last ID) are the files' mistake, said as such: not
    // maps the user never wrote.
    match maps.validate() {
        Ok(()) => RootlessMaps {
            maps,
            shortfall: None,
        },
        Err(err) => short(format!(
            "{who}'s ranges in /etc/subuid and /etc/subgid can't be used ({err})"
        )),
    }
}

/// How the parent writes a user namespace's maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapWriter {
    /// Root: directly, any maps, `setgroups` allowed.
    Root,
    /// Not root, mapping only its own IDs: directly, after denying
    /// `setgroups` (the kernel's condition).
    OwnIds,
    /// Not root, more: `newuidmap` and `newgidmap`.
    Helpers,
}

impl MapWriter {
    /// The writer this caller needs for `maps`.
    fn for_maps(maps: &IdMaps) -> Self {
        Self::choose(
            nix::unistd::geteuid().is_root(),
            (getuid().as_raw(), getgid().as_raw()),
            maps,
        )
    }

    fn choose(root: bool, own: (u32, u32), maps: &IdMaps) -> Self {
        if root {
            Self::Root
        } else if *maps == IdMaps::single(own) {
            Self::OwnIds
        } else {
            Self::Helpers
        }
    }
}

/// Whether this process's user namespace allows `setgroups`: not where
/// `/proc/self/setgroups` says `deny` (see `child.rs`,
/// `setgroups_allowed_here`, the same for after a clone). A kernel without
/// the file (before 3.19) has no such denial.
pub(crate) fn setgroups_allowed() -> bool {
    std::fs::read("/proc/self/setgroups").map_or(true, |text| !text.starts_with(b"deny"))
}

/// Writes the maps for the child `pid`, as this caller may:
///
/// - root, or a caller mapping only its own IDs: `/proc/PID/uid_map`, then
///   `gid_map`, directly; the latter after writing `deny` to
///   `/proc/PID/setgroups`, the kernel's condition for it;
/// - otherwise, through `newuidmap` and `newgidmap`.
pub(crate) fn write_maps(pid: Pid, maps: &IdMaps) -> Result<()> {
    match MapWriter::for_maps(maps) {
        MapWriter::Root => write_maps_directly(pid, maps),
        MapWriter::OwnIds => {
            write_proc_file(pid, "setgroups", "deny").map_err(|errno| {
                Error::sys("denying setgroups in the container's user namespace", errno)
            })?;
            write_maps_directly(pid, maps)
        }
        MapWriter::Helpers => {
            for (kind, map) in maps.each() {
                run_map_helper(pid, kind, map)?;
            }
            Ok(())
        }
    }
}

/// Writes both maps to `/proc/PID`, as root or for the caller's own IDs.
fn write_maps_directly(pid: Pid, maps: &IdMaps) -> Result<()> {
    for (kind, map) in maps.each() {
        let failed = |errno| {
            // Host root can map any host ID, but only those its own user
            // namespace has: nested in one, shipyard has fewer.
            let hint = if errno == Errno::EPERM {
                " (the host IDs must exist in shipyard's own user namespace: see \
                 /proc/self/uid_map and gid_map)"
            } else {
                ""
            };
            Error::sys(format!("writing the container's {kind}{hint}"), errno)
        };
        write_proc_file(pid, &kind.to_string(), &render(map)).map_err(failed)?;
    }
    Ok(())
}

/// Writes `contents` to `/proc/PID/name` in one `write`: the kernel takes
/// each map in one, once, so a short write is an error, not retried.
fn write_proc_file(pid: Pid, name: &str, contents: &str) -> std::result::Result<(), Errno> {
    use std::io::Write as _;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{pid}/{name}"))
        .and_then(|mut file| file.write(contents.as_bytes()))
        .map_err(|err| super::io_errno(&err))?;
    if written == contents.len() {
        Ok(())
    } else {
        Err(Errno::EIO)
    }
}

/// `newuidmap PID c h n …` (or `newgidmap`): the setuid helper checks the
/// ranges against `/etc/subuid` (`/etc/subgid`) and writes the map.
fn run_map_helper(pid: Pid, kind: MapKind, map: &[IdMap]) -> Result<()> {
    let helper = match kind {
        MapKind::Uids => HELPERS[0],
        MapKind::Gids => HELPERS[1],
    };
    let failed = |message: String| Error::MapHelper { helper, message };
    let path = find_helper(helper)
        .ok_or_else(|| failed("it isn't on PATH (the uidmap package)".to_owned()))?;
    let numbers = map
        .iter()
        .flat_map(|range| [range.container, range.host, range.size])
        .map(|number| number.to_string());
    let mut child = std::process::Command::new(path)
        .arg(pid.to_string())
        .args(numbers)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| failed(err.to_string()))?;
    let mut stderr = StderrDrain::new(&mut child);
    let status = match wait_with_deadline(&mut child, &mut stderr) {
        Waited::Exited(status) => status,
        Waited::TimedOut => {
            stop(&mut child);
            return Err(failed(format!(
                "it didn't finish within {} s, and was stopped",
                HELPER_DEADLINE.as_secs()
            )));
        }
        // Not waitable (SIGCHLD ignored makes the kernel reap it at once):
        // how it ended is lost, and its PID may be someone else's by now,
        // so it isn't signalled.
        Waited::Lost(err) => return Err(failed(format!("waiting for it failed: {err}"))),
    };
    if status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&stderr.kept);
    let said = match stderr.trim() {
        "" => status.to_string(),
        said => said.to_owned(),
    };
    let (file, capability) = match kind {
        MapKind::Uids => ("/etc/subuid", "setuid"),
        MapKind::Gids => ("/etc/subgid", "setgid"),
    };
    Err(failed(format!(
        "{said} ({helper} must be setuid root or have the {capability} capability, and {file} \
         must give you these IDs)"
    )))
}

/// How long a helper may take. It runs while the parent has every signal
/// blocked (see `spawn.rs`): a helper stuck on a hung file system (`/etc`
/// over NFS, say) must not leave shipyard deaf to Ctrl-C for good.
const HELPER_DEADLINE: Duration = Duration::from_secs(10);

/// How long [`stop`] waits for a killed helper to go.
const STOP_WAIT: Duration = Duration::from_secs(1);

/// How often a helper is checked on while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The most of a helper's stderr kept for the message.
const STDERR_KEPT: usize = 4096;

/// How waiting for a helper ended.
enum Waited {
    /// It exited.
    Exited(std::process::ExitStatus),
    /// It was still running at the deadline.
    TimedOut,
    /// It can't be waited for.
    Lost(std::io::Error),
}

/// Waits for `child` until [`HELPER_DEADLINE`], draining its `stderr`
/// meanwhile: a helper that fills the pipe would otherwise block, and pass
/// for hung.
fn wait_with_deadline(child: &mut Child, stderr: &mut StderrDrain) -> Waited {
    let started = Instant::now();
    loop {
        stderr.drain();
        match child.try_wait() {
            Ok(Some(status)) => {
                // What it wrote just before exiting.
                stderr.drain();
                return Waited::Exited(status);
            }
            Ok(None) if started.elapsed() < HELPER_DEADLINE => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => return Waited::TimedOut,
            Err(err) => return Waited::Lost(err),
        }
    }
}

/// Kills a helper that overran its deadline and reaps it, waiting a moment
/// at most: one stuck in the kernel (on a hung NFS server) dies only when
/// its I/O ends, maybe never, and shipyard must not wait for that. It's then
/// left unreaped.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let started = Instant::now();
    while matches!(child.try_wait(), Ok(None)) && started.elapsed() < STOP_WAIT {
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// A helper's stderr, read without blocking while it runs: the first
/// [`STDERR_KEPT`] bytes are kept for the message, the rest read and
/// dropped, so the helper never blocks on a full pipe.
struct StderrDrain {
    /// The pipe, until its end (or if it can't be made non-blocking, none:
    /// read nothing rather than block).
    pipe: Option<ChildStderr>,
    kept: Vec<u8>,
}

impl StderrDrain {
    /// Takes `child`'s stderr pipe.
    fn new(child: &mut Child) -> Self {
        let pipe = child
            .stderr
            .take()
            .filter(|pipe| nix::fcntl::fcntl(pipe, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).is_ok());
        Self {
            pipe,
            kept: Vec::new(),
        }
    }

    /// Reads what's there now.
    fn drain(&mut self) {
        use std::io::Read as _;
        let mut buffer = [0u8; 1024];
        while let Some(pipe) = self.pipe.as_mut() {
            match pipe.read(&mut buffer) {
                Ok(0) => self.pipe = None,
                Ok(n) => {
                    let room = STDERR_KEPT.saturating_sub(self.kept.len());
                    self.kept.extend(buffer.iter().take(n.min(room)));
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                // Nothing more for now (or unreadable: left alone).
                Err(_) => return,
            }
        }
    }
}

/// `name` on `PATH`, if it's there and executable, in an absolute
/// directory: the helpers are found the way a shell would find them
/// (Podman does the same), but never in the directory the caller stands
/// in.
fn find_helper(name: &str) -> Option<PathBuf> {
    shipyard_sys::path::find_program(name, std::env::var_os("PATH").as_deref())
}

// --- A helper process in a user namespace of its own ---------------------------
//
// Not a container: a process that makes its own user namespace, has the
// caller map it (as for a container: directly, or through `newuidmap` and
// `newgidmap`), and then works as root over the IDs it was given. How a
// rootless caller extracts image layers owned by the image's users, or
// removes them again (Podman's `unshare`). The helper's side is
// [`unshare_user_namespace`] then [`become_namespace_root`]; the caller's is
// [`map_user_namespace`] in between.

/// Moves this process into a new user namespace (`unshare(CLONE_NEWUSER)`),
/// where it has every capability over nothing yet: its IDs show as the
/// overflow ID until the parent writes its maps. The process must be
/// single-threaded (the kernel refuses otherwise).
///
/// # Errors
///
/// [`Error::Sys`] if the kernel refuses, with the host setting that's why.
pub fn unshare_user_namespace() -> Result<()> {
    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWUSER).map_err(|errno| {
        let why = if matches!(errno, Errno::EPERM | Errno::ENOSPC | Errno::EACCES) {
            format!(" ({})", userns_refusal())
        } else {
            String::new()
        };
        Error::sys(format!("making a user namespace{why}"), errno)
    })
}

/// Writes the maps of the process `pid`, which made a user namespace of its
/// own ([`unshare_user_namespace`]), as for a container: directly for root
/// or a caller's own IDs, else through `newuidmap` and `newgidmap`.
///
/// # Errors
///
/// The [`Error`] of writing them.
pub fn map_user_namespace(pid: u32, maps: &IdMaps) -> Result<()> {
    let pid =
        i32::try_from(pid).map_err(|_| Error::sys("mapping a user namespace", Errno::ESRCH))?;
    write_maps(Pid::from_raw(pid), maps)
}

/// Becomes root of this process's user namespace, once its maps are
/// written: no supplementary groups (where `setgroups` is allowed), then
/// group and user 0.
///
/// # Errors
///
/// [`Error::Sys`] if a step fails, including when the maps were never
/// written (ID 0 isn't mapped).
pub fn become_namespace_root() -> Result<()> {
    if setgroups_allowed() {
        nix::unistd::setgroups(&[])
            .map_err(|errno| Error::sys("dropping supplementary groups", errno))?;
    }
    let root = (nix::unistd::Gid::from_raw(0), nix::unistd::Uid::from_raw(0));
    nix::unistd::setresgid(root.0, root.0, root.0)
        .map_err(|errno| Error::sys("becoming group 0 in the user namespace", errno))?;
    nix::unistd::setresuid(root.1, root.1, root.1)
        .map_err(|errno| Error::sys("becoming root in the user namespace", errno))
}

/// This process's own maps (`/proc/self/uid_map` and `gid_map`): which IDs
/// it can give files. In the initial user namespace, every ID maps to
/// itself.
///
/// # Errors
///
/// [`Error::Sys`] if a map can't be read or parsed.
pub fn current_id_maps() -> Result<IdMaps> {
    let read = |kind: &str| -> Result<String> {
        let path = format!("/proc/self/{kind}");
        std::fs::read_to_string(&path)
            .map_err(|err| Error::sys(format!("reading {path}"), super::io_errno(&err)))
    };
    IdMaps::from_proc(&read("uid_map")?, &read("gid_map")?)
        .ok_or_else(|| Error::sys("parsing /proc/self/uid_map and gid_map", Errno::EINVAL))
}

/// Why the kernel refuses user namespaces to an unprivileged caller, from
/// what the host's settings read.
pub(crate) fn userns_refusal() -> String {
    userns_restriction().unwrap_or_else(|| {
        "the system refused to create a user namespace (a security policy, or a container \
         without privileges)"
            .to_owned()
    })
}

/// The host setting that restricts unprivileged user namespaces for this
/// program, and how to lift it, if one does (`shipyard doctor` says so
/// before a run fails). AppArmor's restriction doesn't count once
/// Shipyard's profile confines this process: it lifts it.
#[must_use]
pub fn userns_restriction() -> Option<String> {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_owned())
    };
    // The profile confining this process: "shipyard (unconfined)" with
    // Shipyard's loaded and attached, "unconfined" without.
    let profile =
        read("/proc/self/attr/apparmor/current").or_else(|| read("/proc/self/attr/current"));
    restriction_of(
        read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref(),
        read("/proc/sys/kernel/unprivileged_userns_clone").as_deref(),
        read("/proc/sys/user/max_user_namespaces").as_deref(),
        profile.as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

/// Where Shipyard's AppArmor profile attaches (`contrib/apparmor/shipyard`).
const PROFILE_PATHS: [&str; 2] = ["/usr/local/bin", "/usr/bin"];

/// The restriction the three settings' values make (`None`: not there),
/// for this process confined by `profile` and running `exe`.
fn restriction_of(
    apparmor: Option<&str>,
    clone: Option<&str>,
    max: Option<&str>,
    profile: Option<&str>,
    exe: Option<&Path>,
) -> Option<String> {
    if apparmor == Some("1") {
        if profile.is_some_and(|profile| profile.starts_with("shipyard ")) {
            return None;
        }
        let mut message = "AppArmor restricts unprivileged user namespaces here \
                           (kernel.apparmor_restrict_unprivileged_userns=1, Ubuntu's default \
                           since 23.10); Shipyard's AppArmor profile \
                           (contrib/apparmor/shipyard) allows them through Shipyard's \
                           binaries, or kernel.apparmor_restrict_unprivileged_userns=0 for \
                           every program"
            .to_owned();
        if let Some(exe) = exe.filter(|exe| {
            !exe.parent()
                .is_some_and(|dir| PROFILE_PATHS.iter().any(|path| dir == Path::new(path)))
        }) {
            let _ = write!(
                message,
                " (the profile attaches to shipyard in /usr/local/bin and /usr/bin, and this \
                 one is {}: add its path to the profile)",
                exe.display()
            );
        }
        Some(message)
    } else if clone == Some("0") {
        Some(
            "unprivileged user namespaces are disabled here \
             (kernel.unprivileged_userns_clone=0); setting it to 1 allows them"
                .to_owned(),
        )
    } else if max == Some("0") {
        Some("user namespaces are disabled here (user.max_user_namespaces=0)".to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBORDINATE: [(u32, u32); 1] = [(100_000, 65536)];

    #[test]
    fn full_maps_need_the_helpers_and_both_ranges() {
        let full = choose_rootless_maps((1000, 1000), "alice", true, &SUBORDINATE, &SUBORDINATE);
        assert_eq!(
            full.maps,
            IdMaps::rootless((1000, 1000), &SUBORDINATE, &SUBORDINATE)
        );
        assert_eq!(full.shortfall, None);
        for (helpers, subuids, subgids, missing) in [
            (
                false,
                &SUBORDINATE[..],
                &SUBORDINATE[..],
                "newuidmap and newgidmap aren't on PATH",
            ),
            (
                true,
                &[][..],
                &SUBORDINATE[..],
                "alice has no subordinate user IDs in /etc/subuid",
            ),
            (
                true,
                &SUBORDINATE[..],
                &[][..],
                "alice has no subordinate group IDs in /etc/subgid",
            ),
        ] {
            let short = choose_rootless_maps((1000, 1000), "alice", helpers, subuids, subgids);
            assert_eq!(short.maps, IdMaps::single((1000, 1000)));
            let why = short.shortfall.unwrap();
            assert!(why.contains(missing), "{why}");
            assert!(why.contains("`alice:100000:65536`"), "{why}");
        }
    }

    /// Ranges the kernel would refuse fall back to the single ID, blaming the
    /// files, not maps the user never wrote.
    #[test]
    fn unusable_ranges_fall_back_with_the_reason() {
        for ranges in [&[(1000, 65536)][..], &[(100_000, u32::MAX)][..]] {
            let short = choose_rootless_maps((1000, 1000), "alice", true, ranges, ranges);
            assert_eq!(short.maps, IdMaps::single((1000, 1000)));
            let why = short.shortfall.unwrap();
            assert!(
                why.contains("alice's ranges in /etc/subuid and /etc/subgid can't be used"),
                "{why}"
            );
        }
    }

    #[test]
    fn a_user_namespace_is_added_only_where_one_is_needed() {
        let maps = || RootlessMaps {
            maps: IdMaps::single((1000, 1000)),
            shortfall: Some("short".to_owned()),
        };
        let mut rootless = Isolation::standard();
        let said = add_user_namespace(&mut rootless, false, maps).unwrap();
        assert!(rootless.has(Namespace::User));
        assert_eq!(rootless.id_maps, Some(IdMaps::single((1000, 1000))));
        assert!(
            said.contains("one was added") && said.ends_with("; short"),
            "{said}"
        );
        rootless.validate().unwrap();

        let mut rootful = Isolation::standard();
        assert_eq!(add_user_namespace(&mut rootful, true, maps), None);
        assert!(!rootful.has(Namespace::User));

        let mut own = Isolation::standard();
        own.namespaces.push(Namespace::User);
        own.id_maps = Some(IdMaps::single((2000, 2000)));
        assert_eq!(add_user_namespace(&mut own, false, maps), None);
        assert_eq!(
            own.id_maps,
            Some(IdMaps::single((2000, 2000))),
            "the bundle's own"
        );
    }

    #[test]
    fn the_writer_follows_who_writes_what() {
        let own = (1000, 1000);
        let single = IdMaps::single(own);
        let full = IdMaps::rootless(own, &SUBORDINATE, &SUBORDINATE);
        assert_eq!(MapWriter::choose(true, own, &full), MapWriter::Root);
        assert_eq!(MapWriter::choose(true, own, &single), MapWriter::Root);
        assert_eq!(MapWriter::choose(false, own, &single), MapWriter::OwnIds);
        assert_eq!(MapWriter::choose(false, own, &full), MapWriter::Helpers);
        // Someone else's single ID is beyond the kernel's rule: the helpers'.
        assert_eq!(
            MapWriter::choose(false, own, &IdMaps::single((2000, 2000))),
            MapWriter::Helpers
        );
    }

    #[test]
    fn refusals_name_the_setting() {
        let installed = Some(Path::new("/usr/local/bin/shipyard"));
        let apparmor =
            restriction_of(Some("1"), None, Some("1000"), Some("unconfined"), installed).unwrap();
        assert!(apparmor.contains("apparmor_restrict"), "{apparmor}");
        assert!(apparmor.contains("contrib/apparmor/shipyard"), "{apparmor}");
        assert!(!apparmor.contains("add its path"), "{apparmor}");
        // Shipyard's profile loaded and attached: lifted, nothing to name.
        assert_eq!(
            restriction_of(
                Some("1"),
                None,
                None,
                Some("shipyard (unconfined)"),
                installed
            ),
            None
        );
        // Installed where the profile doesn't attach: said so.
        let elsewhere = restriction_of(
            Some("1"),
            None,
            None,
            Some("unconfined"),
            Some(Path::new("/home/u/.cargo/bin/shipyard")),
        )
        .unwrap();
        assert!(
            elsewhere.contains("/home/u/.cargo/bin/shipyard: add its path"),
            "{elsewhere}"
        );
        assert!(
            restriction_of(Some("0"), Some("0"), None, None, installed)
                .unwrap()
                .contains("unprivileged_userns_clone=0")
        );
        assert!(
            restriction_of(None, None, Some("0"), None, installed)
                .unwrap()
                .contains("max_user_namespaces=0")
        );
        // Nothing restricts: no restriction to name (a refusal then blames a
        // policy or a container).
        assert_eq!(
            restriction_of(Some("0"), Some("1"), Some("100"), None, installed),
            None
        );
        assert_eq!(restriction_of(None, None, None, None, installed), None);
    }

    #[test]
    fn helpers_are_found_on_path() {
        assert!(find_helper("sh").is_some());
        assert!(find_helper("no-such-helper-anywhere").is_none());
    }
}
