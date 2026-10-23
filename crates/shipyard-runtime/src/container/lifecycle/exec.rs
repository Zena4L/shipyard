//! `exec` (stage 8, steps 8.4a and 8.4b): a command run inside a running container,
//! joining its namespaces, in its cgroup, confined as its own process is
//! (see `process/exec.rs` for how it gets there).
//!
//! **Its confinement comes from a snapshot**, `exec.json` in the state
//! directory, which the monitor writes from the bundle it actually started
//! the container with: never from `config.json` again, which may have
//! changed since (or gone). Versioned (H2): an `exec` of a later Shipyard
//! reads it as it was written. What `exec` is given (a runc `process.json`,
//! flags) may narrow it, never widen it.

use std::os::fd::OwnedFd;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    ContainerError, Result, Status, existing, lock, read_record, status_of, write_atomically_at,
    wrong,
};
use crate::container::{ExecOptions, ExecOutcome};
use crate::isolation::{Join, Namespace};
use crate::process::{ProcessSpec, RlimitValue, SpecSetup, Supervisor};
use crate::security::{Capabilities, Security};

/// The snapshot's name in the state directory.
pub(super) const EXEC_FILE: &str = "exec.json";

/// The snapshot's format: what this Shipyard writes and reads.
const EXEC_VERSION: u32 = 1;

/// The most of a snapshot read: the process again, with its security
/// (a seccomp profile among it), so the state file's bound.
const EXEC_MAX: u64 = super::super::versions::STATE_MAX;

/// Only a snapshot's version: read first, whatever else the file holds.
#[derive(Deserialize)]
struct Versioned {
    version: u32,
}

/// What `exec` starts from (see the module docs).
#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    /// The container's process: its environment, working directory, user,
    /// limits, OOM score adjustment.
    process: SpecSetup,
    /// Its capabilities, `no_new_privs` and seccomp filter.
    security: Security,
}

/// Writes the snapshot of the container's process (`spec`, `security`, as
/// the monitor starts it) in the state directory open at `dir`. What the
/// spec doesn't set, the process gets from this one (the monitor): its
/// limits of every other kind, its OOM score adjustment. Those are recorded
/// too, so that `exec`'s commands get them, and nothing beyond.
pub(super) fn write_snapshot(
    dir: &OwnedFd,
    spec: &ProcessSpec,
    security: &Security,
) -> std::result::Result<(), String> {
    let mut spec = spec.clone();
    for limit in crate::process::current_rlimits() {
        if !spec.rlimits.iter().any(|given| given.kind == limit.kind) {
            spec.rlimits.push(limit);
        }
    }
    if spec.oom_score_adj.is_none() {
        spec.oom_score_adj = std::fs::read_to_string("/proc/self/oom_score_adj")
            .ok()
            .and_then(|adj| adj.trim().parse().ok());
    }
    let snapshot = Snapshot {
        version: EXEC_VERSION,
        process: SpecSetup::new(&spec),
        security: security.clone(),
    };
    let json = serde_json::to_vec(&snapshot)
        .map_err(|err| format!("encoding the container's process for exec: {err}"))?;
    write_atomically_at(dir, EXEC_FILE, &json)
        .map_err(|errno| format!("writing {EXEC_FILE}: {}", errno.desc()))
}

/// The container's process, from its snapshot.
fn read_snapshot(dir: &Path, id: &str) -> Result<(ProcessSpec, Security)> {
    let not_executable = |why: String| ContainerError::NotExecutable {
        id: id.to_owned(),
        why,
    };
    let bytes = match shipyard_sys::fs::read_bounded(&dir.join(EXEC_FILE), EXEC_MAX) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_executable(
                "it was created by a Shipyard from before exec: recreate it".to_owned(),
            ));
        }
        Err(err) => {
            return Err(ContainerError::Io {
                doing: format!("reading {}", dir.join(EXEC_FILE).display()),
                error: err,
            });
        }
    };
    // The version first, whatever else the file holds.
    let versioned: Versioned = serde_json::from_slice(&bytes)
        .map_err(|err| not_executable(format!("its {EXEC_FILE} can't be read ({err})")))?;
    if versioned.version != EXEC_VERSION {
        return Err(not_executable(format!(
            "its {EXEC_FILE} is version {}, which this Shipyard doesn't know (it knows \
             {EXEC_VERSION})",
            versioned.version
        )));
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes)
        .map_err(|err| not_executable(format!("its {EXEC_FILE} can't be read ({err})")))?;
    let spec = snapshot
        .process
        .spec()
        .map_err(|err| not_executable(format!("its {EXEC_FILE}: {err}")))?;
    Ok((spec, snapshot.security))
}

/// Runs a command in container `id`, as `options` say (see [`ExecOptions`]),
/// until it ends; or, detached, until it runs. `passed` descriptors go to it
/// at 3 and up (`--preserve-fds`, socket activation); they're closed here
/// once it has its own.
///
/// The container's lock is held until the command has started (so a
/// `pause` can't freeze it half-way), not while it runs.
///
/// **This process supervises the command** (a [`Supervisor`]): it becomes a
/// child subreaper for good, and while the command runs any other child it
/// has is treated as the command's (reaped, and killed at the end). Call it
/// from a process (or its main thread) that starts no other children
/// meanwhile, one at a time, as the CLI does; detached (`-d`), the command
/// is the caller's child to reap.
///
/// # Errors
///
/// No such container, one that isn't created or running
/// ([`ContainerError::WrongStatus`]: paused would hang it), one made before
/// `exec` ([`ContainerError::NotExecutable`]), options asking for more than
/// the container has ([`ContainerError::ExecBeyondContainer`]), its cgroup
/// rearranged from inside ([`ContainerError::ExecCgroupBusy`]), a rootless
/// caller outside its delegated cgroup ([`ContainerError::ExecCgroupDenied`]),
/// a console socket without a terminal
/// ([`ContainerError::ExecConsoleWithoutTerminal`]), `-d -t` without one
/// ([`ContainerError::ExecTerminalNeedsConsole`]), or the command couldn't
/// start ([`ContainerError::Exec`]).
pub fn exec(
    root: &Path,
    id: &str,
    options: &ExecOptions,
    passed: Option<crate::process::passed::PassedFds>,
) -> Result<ExecOutcome> {
    let dir = existing(root, id)?;
    let lock = lock(&dir)?;
    let status = status_of(&dir)?;
    if !matches!(status, Status::Created | Status::Running) {
        return Err(wrong(id, status, "exec", "created or running"));
    }
    let record = read_record(&dir)?.ok_or_else(|| ContainerError::NotFound(id.to_owned()))?;
    let Some((pid, started)) = record.init else {
        return Err(wrong(id, Status::Creating, "exec", "created or running"));
    };
    let target = u32::try_from(pid).map_err(|_| ContainerError::NotFound(id.to_owned()))?;
    let (mut base, base_security) = read_snapshot(&dir, id)?;
    // What the container was given, not found for its user: its own `HOME`
    // (`-e`, an image's `Env`) and the groups added to its user's
    // (`--group-add`), which an exec as another user keeps (Docker's).
    let given = Given {
        home: base.env.get(std::ffi::OsStr::new("HOME")).is_some(),
        gids: base.user.additional_gids.clone(),
        groups: base.user.names.additional_groups.clone(),
    };
    // Its user and groups by name (a plan's, an image's `USER`): as its
    // init resolved them, in its own files, before anything is narrowed
    // from them (its capabilities are by uid).
    if !base.user.names.is_empty() {
        resolve_names_in(&mut base, target, started)?;
    }
    // Its user by name: made numbers first, as `--user` gives them, so its
    // capabilities (by uid) are the right user's.
    let (options, groups) = with_user_resolved(options, &given, target, started)?;
    let options = &options;
    let (mut spec, security, process_terminal) = narrowed(&base, &base_security, options)?;
    if let Some(groups) = groups {
        spec.user.additional_gids = groups;
    }
    // Its terminal: `-t`, or the `process.json`'s, to a relay or a console
    // socket (runc's rules).
    let terminal = options.terminal || process_terminal.is_some();
    let size = process_terminal.and_then(|terminal| terminal.size);
    if options.console.is_some() && !terminal {
        return Err(ContainerError::ExecConsoleWithoutTerminal);
    }
    if options.detach && terminal && options.console.is_none() {
        return Err(ContainerError::ExecTerminalNeedsConsole);
    }
    let console = options
        .console
        .map(|fd| (fd, size.unwrap_or(crate::process::terminal::DEFAULT_SIZE)));
    // Gone meanwhile, its namespaces read as none: it's stopped.
    let namespaces = namespaces_of(target, id).map_err(|err| match status_of(&dir) {
        Ok(status @ (Status::Stopping | Status::Stopped)) => {
            wrong(id, status, "exec", "created or running")
        }
        _ => err,
    })?;
    // Recorded but not the one made (replaced, or not set up yet): refused,
    // never run without one.
    let cgroup = match (&record.cgroup, record.container_cgroup()) {
        (None, _) => None,
        (Some(_), Some(found)) => Some(found?),
        (Some(_), None) => {
            return Err(ContainerError::NotExecutable {
                id: id.to_owned(),
                why: "its cgroup isn't the one made for it (replaced, or not set up yet)"
                    .to_owned(),
            });
        }
    };
    let mut join = Join::standard(target);
    join.namespaces = namespaces;
    join.start_time = Some(started);
    join.security = security;
    join.cgroup = cgroup;
    join.cgroup_inode = record.cgroup_inode;
    let mut supervisor = Supervisor::install().map_err(|err| exec_failed(&dir, id, err))?;
    if let Some(passed) = passed {
        supervisor.pass(passed);
    }
    if terminal && console.is_none() {
        // Relayed: raw if this process's stdin is a terminal (every key a
        // byte for the container's), as it is otherwise.
        let raw = nix::unistd::isatty(std::io::stdin()).unwrap_or(false);
        supervisor.set_stdio(crate::process::Stdio {
            terminal: Some(crate::process::Pty {
                input: !options.no_input,
                raw: raw && !options.no_input,
                size,
            }),
            ..crate::process::Stdio::default()
        });
    } else if !terminal {
        // Never the user's terminal: relayed, or `/dev/null` once detached
        // (or without input).
        let mut stdio = crate::process::Stdio::sparing_terminals(options.detach);
        if options.no_input {
            stdio.stdin = crate::process::Stream::Null;
        }
        supervisor.set_stdio(stdio);
    }
    let pid_file = options.pid_file.clone();
    let outcome = if options.detach {
        let started = supervisor.start_detached(&spec, &join, console);
        drop(lock);
        if let (Ok(pid), Some(file)) = (&started, &pid_file) {
            write_pid_file(file, *pid);
        }
        started.map(|pid| ExecOutcome::Detached { pid })
    } else {
        // Let go of once the command runs (or never, if it doesn't: then
        // when the run's over), its PID said then.
        supervisor
            .supervise_exec(&spec, options.stop, &join, console, move |pid| {
                drop(lock);
                if let Some(file) = &pid_file {
                    write_pid_file(file, pid);
                }
            })
            .map(ExecOutcome::Exited)
    };
    outcome.map_err(|err| exec_failed(&dir, id, err))
}

/// Writes `pid` to `file` whole or not at all (a reader may poll for it),
/// as a new file (`O_EXCL` temporary: never through a planted link). A
/// failure is logged: the command runs all the same.
fn write_pid_file(file: &Path, pid: u32) {
    let written = shipyard_sys::fs::write_atomically(
        file,
        pid.to_string().as_bytes(),
        // Not synced: a PID means nothing after a restart.
        &shipyard_sys::fs::Write::new(0o600).durability(shipyard_sys::fs::Durability::Atomic),
    );
    if let Err(error) = written {
        tracing::warn!("the exec's PID file {}: {error}", file.display());
    }
}

/// The process `options` ask for, from the container's (`base`,
/// `base_security`): checked never to have more than it.
fn narrowed(
    base: &ProcessSpec,
    base_security: &Security,
    options: &ExecOptions,
) -> Result<(ProcessSpec, Security, Option<crate::oci::BundleTerminal>)> {
    // The most anything in it may have: its own process's bounding set.
    let container = base_security.capabilities.resolve(base.user.uid);
    let (mut spec, listed, mut no_new_privileges, terminal) = if let Some(path) = &options.process {
        let text = std::fs::read_to_string(path).map_err(|error| ContainerError::Io {
            doing: format!("reading {}", path.display()),
            error,
        })?;
        let process = crate::oci::process_from_json(&text, path)?;
        let mut spec = process.spec;
        // Kind by kind: what it lists, the container's for the rest.
        let given = std::mem::take(&mut spec.rlimits);
        spec.rlimits = base
            .rlimits
            .iter()
            .filter(|theirs| !given.iter().any(|ours| ours.kind == theirs.kind))
            .copied()
            .collect();
        spec.rlimits.extend(given);
        (
            spec,
            process.capabilities,
            process.no_new_privileges,
            process.terminal,
        )
    } else {
        let mut spec = base.clone();
        spec.args.clone_from(&options.args);
        (spec, None, base_security.no_new_privileges, None)
    };
    if let Some(cwd) = &options.cwd {
        spec.cwd.clone_from(cwd);
    }
    for entry in &options.env {
        spec.env.push_entry(entry).map_err(|err| {
            ContainerError::Setup(format!("--env {}: {err}", entry.to_string_lossy()))
        })?;
    }
    if let Some((uid, gid)) = options.user {
        spec.user.uid = uid;
        if let Some(gid) = gid {
            spec.user.gid = gid;
        }
    }
    if !options.additional_gids.is_empty() {
        spec.user
            .additional_gids
            .clone_from(&options.additional_gids);
    }
    // Not listed: the container's, for the user exec runs as (Docker's
    // rule gives a user other than root only what was added).
    let mut sets = listed.unwrap_or_else(|| base_security.capabilities.resolve(spec.user.uid));
    // runc's `--cap`: bounding, effective, permitted; ambient too if it
    // has inheritable ones to go with it.
    for &capability in &options.capabilities {
        sets.bounding.insert(capability);
        sets.effective.insert(capability);
        sets.permitted.insert(capability);
        if !sets.inheritable.is_empty() {
            sets.inheritable.insert(capability);
            sets.ambient.insert(capability);
        }
    }
    no_new_privileges |= options.no_new_privileges;
    if spec.oom_score_adj.is_none() {
        spec.oom_score_adj = base.oom_score_adj;
    }
    beyond(
        base,
        base_security,
        &container,
        &spec,
        &sets,
        no_new_privileges,
    )?;
    sets.check()
        .map_err(|why| ContainerError::Setup(format!("exec's capabilities: {why}")))?;
    let mut security = base_security.clone();
    security.capabilities = Capabilities::Exact(sets);
    security.no_new_privileges = no_new_privileges;
    Ok((spec, security, terminal))
}

/// Refuses whatever `spec`, `sets` and `no_new_privileges` would have
/// beyond the container's (`base`, `base_security`, `container`'s sets).
fn beyond(
    base: &ProcessSpec,
    base_security: &Security,
    container: &crate::security::CapabilitySets,
    spec: &ProcessSpec,
    sets: &crate::security::CapabilitySets,
    no_new_privileges: bool,
) -> Result<()> {
    let refuse = |what: String| Err(ContainerError::ExecBeyondContainer(what));
    if let Some(capability) = sets
        .all()
        .iter()
        .find(|capability| !container.bounding.contains(*capability))
    {
        return refuse(format!("{capability} isn't in its bounding set"));
    }
    if base_security.no_new_privileges && !no_new_privileges {
        return refuse("no_new_privs is set in it".to_owned());
    }
    for limit in &spec.rlimits {
        let theirs = base.rlimits.iter().find(|theirs| theirs.kind == limit.kind);
        if let Some(theirs) = theirs
            && above(limit.hard, theirs.hard)
        {
            return refuse(format!(
                "a {} limit of {} is above its {}",
                limit.kind.name(),
                limit.hard,
                theirs.hard
            ));
        }
    }
    if let (Some(ours), Some(theirs)) = (spec.oom_score_adj, base.oom_score_adj)
        && ours < theirs
    {
        return refuse(format!(
            "an OOM score adjustment of {ours} is below its {theirs}"
        ));
    }
    Ok(())
}

/// Whether limit `ours` is above `theirs` (unlimited is the most).
fn above(ours: RlimitValue, theirs: RlimitValue) -> bool {
    match (ours, theirs) {
        (_, RlimitValue::Unlimited) => false,
        (RlimitValue::Unlimited, RlimitValue::Finite(_)) => true,
        (RlimitValue::Finite(ours), RlimitValue::Finite(theirs)) => ours > theirs,
    }
}

/// What a container's process was given rather than found for its user.
struct Given {
    /// A `HOME` of its own.
    home: bool,
    /// Groups added to its user's own, by number.
    gids: Vec<u32>,
    /// And by name (or number), looked up in its `/etc/group`.
    groups: Vec<String>,
}

/// `options`, its user given by name or number (`user_spec`) made numbers:
/// looked up in the container's own `/etc` (through its init's root, `pid`
/// started at `started`), its groups (those that list it, as Docker's, and
/// those the container was `given`), and its home as `HOME` unless
/// `options` or the container (`given`) sets one. With the groups, which
/// replace the container's process's.
fn with_user_resolved(
    options: &ExecOptions,
    given: &Given,
    pid: u32,
    started: u64,
) -> Result<(ExecOptions, Option<Vec<u32>>)> {
    let Some(user) = &options.user_spec else {
        return Ok((options.clone(), None));
    };
    let (name, group) = match user.split_once(':') {
        Some((name, group)) => (name, Some(group)),
        None => (user.as_str(), None),
    };
    let mut scratch = ProcessSpec::new(Vec::new());
    scratch.user.names.user = Some(name.to_owned());
    scratch.user.names.group = group.map(str::to_owned);
    scratch.user.names.memberships = true;
    scratch.user.additional_gids.clone_from(&given.gids);
    scratch
        .user
        .names
        .additional_groups
        .clone_from(&given.groups);
    resolve_names_in(&mut scratch, pid, started)?;
    let mut resolved = options.clone();
    resolved.user = Some((scratch.user.uid, Some(scratch.user.gid)));
    resolved.user_spec = None;
    let has_home = given.home
        || options
            .env
            .iter()
            .any(|entry| entry.as_encoded_bytes().starts_with(b"HOME="));
    if !has_home && let Some(home) = scratch.env.get(std::ffi::OsStr::new("HOME")) {
        let mut entry = std::ffi::OsString::from("HOME=");
        entry.push(home);
        resolved.env.push(entry);
    }
    // Groups given (`-g`) win; else the container's added ones and the
    // user's own.
    let groups = options
        .additional_gids
        .is_empty()
        .then(|| scratch.user.additional_gids.clone());
    Ok((resolved, groups))
}

/// Resolves `spec`'s user and group names in the `/etc` of the container
/// whose init is `pid` (started at `started`, so a PID reused since is
/// refused), through its root as it sees it.
fn resolve_names_in(spec: &mut ProcessSpec, pid: u32, started: u64) -> Result<()> {
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;
    let dir = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let gone = || ContainerError::Exec(Box::new(crate::Error::NoSuchProcess { pid }));
    let proc_dir = open(
        format!("/proc/{pid}").as_str(),
        OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| gone())?;
    if crate::process::join::start_time_at(&proc_dir) != Some(started) {
        return Err(gone());
    }
    let root =
        openat(&proc_dir, "root", dir, Mode::empty()).map_err(|errno| ContainerError::Io {
            doing: format!("opening /proc/{pid}/root"),
            error: errno.into(),
        })?;
    crate::process::resolve_names_in_root(spec, &root)
        .map_err(|err| ContainerError::Exec(Box::new(err.into())))
}

/// The namespaces of process `target` that aren't this process's own (one
/// it shares with the host is joined already). The PID and mount ones must
/// be among them: a container always has them.
fn namespaces_of(target: u32, id: &str) -> Result<Vec<Namespace>> {
    let namespaces = super::unshared(
        target,
        &[
            Namespace::User,
            Namespace::Pid,
            Namespace::Mount,
            Namespace::Uts,
            Namespace::Ipc,
            Namespace::Network,
            Namespace::Cgroup,
        ],
    )
    .map_err(|err| ContainerError::Exec(Box::new(err)))?;
    if !namespaces.contains(&Namespace::Pid) || !namespaces.contains(&Namespace::Mount) {
        return Err(ContainerError::NotExecutable {
            id: id.to_owned(),
            why: "it has no PID and mount namespaces of its own".to_owned(),
        });
    }
    Ok(namespaces)
}

/// Why `exec` failed, for the caller: typed where the caller can act on it,
/// the container's end where it ended meanwhile.
fn exec_failed(dir: &Path, id: &str, err: crate::Error) -> ContainerError {
    use crate::process::SpawnErrorKind;
    if let crate::Error::Spawn(spawn) = &err {
        match spawn.kind() {
            SpawnErrorKind::CgroupBusy => return ContainerError::ExecCgroupBusy(id.to_owned()),
            SpawnErrorKind::CgroupDenied => return ContainerError::ExecCgroupDenied(id.to_owned()),
            _ => {}
        }
    }
    if let Ok(status @ (Status::Stopping | Status::Stopped)) = status_of(dir) {
        return wrong(id, status, "exec", "created or running");
    }
    ContainerError::Exec(Box::new(err))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::fd::OwnedFd;

    use super::*;
    use crate::process::{Rlimit, RlimitKind, User};
    use crate::security::{Capability, CapabilitySet, CapabilitySets};

    /// A container's process: `sh` as root, three capabilities, a limit, an
    /// OOM score adjustment, `no_new_privs`.
    fn container() -> (ProcessSpec, Security) {
        let mut spec = ProcessSpec::new(vec![OsString::from("sh")]);
        spec.user = User::new(0, 0);
        spec.rlimits = vec![
            Rlimit::new(
                RlimitKind::Nofile,
                RlimitValue::Finite(512),
                RlimitValue::Finite(512),
            )
            .unwrap(),
            Rlimit::new(
                RlimitKind::Nproc,
                RlimitValue::Finite(100),
                RlimitValue::Finite(100),
            )
            .unwrap(),
        ];
        spec.oom_score_adj = Some(100);
        let set: CapabilitySet = [Capability::KILL, cap("CHOWN"), cap("NET_RAW")]
            .into_iter()
            .collect();
        let mut security = Security::standard();
        security.capabilities = Capabilities::Exact(CapabilitySets {
            bounding: set,
            effective: [Capability::KILL].into_iter().collect(),
            permitted: [Capability::KILL].into_iter().collect(),
            ..CapabilitySets::default()
        });
        security.no_new_privileges = true;
        (spec, security)
    }

    fn cap(name: &str) -> Capability {
        name.parse().unwrap()
    }

    fn options(args: &[&str]) -> ExecOptions {
        ExecOptions::new(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn exec_starts_from_the_containers_process() {
        let (base, security) = container();
        let mut asked = options(&["ls"]);
        asked.env = vec![OsString::from("A=1")];
        asked.user = Some((7, None));
        asked.capabilities = vec![cap("CHOWN")];
        let (spec, confined, _) = narrowed(&base, &security, &asked).unwrap();
        assert_eq!(spec.args, ["ls"]);
        assert_eq!((spec.user.uid, spec.user.gid), (7, 0), "the group stays");
        assert_eq!(spec.env.get(std::ffi::OsStr::new("A")).unwrap(), "1");
        assert_eq!(spec.rlimits, base.rlimits);
        assert_eq!(spec.oom_score_adj, Some(100));
        assert!(confined.no_new_privileges);
        let Capabilities::Exact(sets) = confined.capabilities else {
            panic!("exact sets");
        };
        assert!(sets.effective.contains(cap("CHOWN")));
        assert_eq!(confined.seccomp, security.seccomp, "always the container's");
    }

    #[test]
    fn exec_never_has_more_than_the_container() {
        let (base, security) = container();
        let beyond = |asked: &ExecOptions| match narrowed(&base, &security, asked) {
            Err(ContainerError::ExecBeyondContainer(what)) => what,
            other => panic!("{other:?}"),
        };
        let mut asked = options(&["ls"]);
        asked.capabilities = vec![cap("SYS_ADMIN")];
        assert!(beyond(&asked).contains("CAP_SYS_ADMIN"));
        let process = |json: &str| {
            let path = std::env::temp_dir().join(format!(
                "shipyard-exec-process-{}-{}.json",
                std::process::id(),
                json.len()
            ));
            std::fs::write(&path, json).unwrap();
            let mut asked = options(&[]);
            asked.process = Some(path.clone());
            let result = narrowed(&base, &security, &asked);
            let _ = std::fs::remove_file(path);
            result
        };
        let refused = |json: &str| match process(json) {
            Err(ContainerError::ExecBeyondContainer(what)) => what,
            other => panic!("{json}: {other:?}"),
        };
        let start = r#"{"args": ["ls"], "cwd": "/", "user": {"uid": 0, "gid": 0}"#;
        assert!(refused(&format!("{start}}}")).contains("no_new_privs"));
        assert!(
            refused(&format!(
                r#"{start}, "noNewPrivileges": true, "rlimits": [{{"type": "RLIMIT_NOFILE", "hard": 513, "soft": 1}}]}}"#
            ))
            .contains("nofile")
        );
        assert!(
            refused(&format!(
                r#"{start}, "noNewPrivileges": true, "oomScoreAdj": 99}}"#
            ))
            .contains("OOM score")
        );
        assert!(
            refused(&format!(
                r#"{start}, "noNewPrivileges": true, "capabilities": {{"bounding": ["CAP_SYS_PTRACE"]}}}}"#
            ))
            .contains("CAP_SYS_PTRACE")
        );
        // Within it: a lower limit, a higher OOM score, the flag on. The
        // limits it doesn't list are the container's (NPROC).
        let within = process(&format!(
            r#"{start}, "noNewPrivileges": true, "oomScoreAdj": 500, "rlimits": [{{"type": "RLIMIT_NOFILE", "hard": 64, "soft": 64}}]}}"#
        ))
        .unwrap();
        assert_eq!(within.0.oom_score_adj, Some(500));
        let hard = |kind: RlimitKind| {
            within
                .0
                .rlimits
                .iter()
                .find(|limit| limit.kind == kind)
                .map(|limit| limit.hard)
        };
        assert_eq!(hard(RlimitKind::Nofile), Some(RlimitValue::Finite(64)));
        assert_eq!(hard(RlimitKind::Nproc), Some(RlimitValue::Finite(100)));
    }

    /// Docker's rule (a `shipyard run` container's): a user other than root
    /// gets only what was added, whatever the container's own user.
    #[test]
    fn capabilities_are_the_exec_users() {
        let (base, mut security) = container();
        security.capabilities = Capabilities::Default {
            kept: [Capability::KILL, cap("CHOWN")].into_iter().collect(),
            added: [cap("NET_RAW")].into_iter().collect(),
        };
        let mut asked = options(&["ls"]);
        asked.user = Some((1000, None));
        let (_, confined, _) = narrowed(&base, &security, &asked).unwrap();
        let Capabilities::Exact(sets) = confined.capabilities else {
            panic!("exact sets");
        };
        assert_eq!(sets.effective, [cap("NET_RAW")].into_iter().collect());
        assert_eq!(sets.bounding.iter().count(), 3);
    }

    #[test]
    fn unlimited_is_the_most() {
        let finite = RlimitValue::Finite;
        assert!(above(RlimitValue::Unlimited, finite(5)));
        assert!(!above(finite(5), RlimitValue::Unlimited));
        assert!(!above(RlimitValue::Unlimited, RlimitValue::Unlimited));
        assert!(above(finite(6), finite(5)));
        assert!(!above(finite(5), finite(5)));
    }

    fn state_dir() -> (std::path::PathBuf, OwnedFd) {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-exec-snapshot-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fd = nix::fcntl::open(
            &dir,
            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY,
            nix::sys::stat::Mode::empty(),
        )
        .unwrap();
        (dir, fd)
    }

    #[test]
    fn the_snapshot_is_read_as_written() {
        let (dir, fd) = state_dir();
        let (spec, security) = container();
        write_snapshot(&fd, &spec, &security).unwrap();
        let mode = std::fs::metadata(dir.join(EXEC_FILE)).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
        let (read, read_security) = read_snapshot(&dir, "c").unwrap();
        assert_eq!(read_security, security);
        assert_eq!((read.args, read.user), (spec.args, spec.user));
        assert_eq!(read.oom_score_adj, Some(100));
        // Its own limits, and those it inherits (this process's) for every
        // other kind: what it really has.
        for limit in &spec.rlimits {
            assert!(read.rlimits.contains(limit), "{limit:?}");
        }
        let kinds = |limits: &[Rlimit]| limits.iter().map(|limit| limit.kind).collect::<Vec<_>>();
        for limit in crate::process::current_rlimits() {
            assert!(
                kinds(&read.rlimits).contains(&limit.kind),
                "{:?}",
                limit.kind
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_snapshot_this_shipyard_cant_read_is_refused() {
        let (dir, _fd) = state_dir();
        let not_executable = |dir: &Path| match read_snapshot(dir, "c") {
            Err(ContainerError::NotExecutable { why, .. }) => why,
            other => panic!("{other:?}"),
        };
        // Made before exec: none at all.
        assert!(not_executable(&dir).contains("before exec"));
        std::fs::write(dir.join(EXEC_FILE), r#"{"version": 2, "anything": "new"}"#).unwrap();
        assert!(not_executable(&dir).contains("version 2"));
        std::fs::write(dir.join(EXEC_FILE), "not json").unwrap();
        assert!(not_executable(&dir).contains("can't be read"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Version 1 as it's written (H2): a later Shipyard must read it so.
    /// (Its process travels in the same form as the init's setup: a change
    /// there must not change this.)
    #[test]
    fn version_1_reads_as_written() {
        const V1: &str = r#"{"version":1,"process":{"args":[[115,104]],"env":[],"cwd":[47,119,111,114,107],"uid":7,"gid":8,"umask":null,"additional_gids":[],"names":{"user":null,"group":null,"additional_groups":[],"memberships":false},"rlimits":[["nofile",64,128]],"oom_score_adj":5},"security":{"capabilities":{"Exact":{"bounding":32,"effective":32,"permitted":32,"inheritable":0,"ambient":0}},"no_new_privileges":true,"seccomp":"Unconfined"}}"#;
        let (dir, _fd) = state_dir();
        std::fs::write(dir.join(EXEC_FILE), V1).unwrap();
        let (spec, security) = read_snapshot(&dir, "c").unwrap();
        assert_eq!(spec.args, ["sh"]);
        assert_eq!(spec.cwd, Path::new("/work"));
        assert_eq!((spec.user.uid, spec.user.gid), (7, 8));
        assert_eq!(spec.oom_score_adj, Some(5));
        assert_eq!(spec.rlimits[0].hard, RlimitValue::Finite(128));
        assert!(security.no_new_privileges);
        assert_eq!(security.seccomp, crate::security::Seccomp::Unconfined);
        let Capabilities::Exact(sets) = security.capabilities else {
            panic!("exact sets");
        };
        assert!(sets.effective.contains(Capability::KILL));
        let _ = std::fs::remove_dir_all(dir);
    }
}
