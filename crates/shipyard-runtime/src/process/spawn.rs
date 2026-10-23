//! Starting a process: `fork` + `exec` (TLPI ch. 24–28).
//!
//! [`spawn`] forks and returns at once; the supervisor then watches the
//! child's report pipe and its exit inside one event loop. Spawn only from the
//! main thread: Linux's parent-death signal fires when the *thread* that
//! forked exits, and on macOS a fork in another thread could inherit the
//! report pipe.

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, sigprocmask};
use nix::sys::signal::{Signal as NixSignal, sigaction};
use nix::unistd::{ForkResult, Pid, fork, getpgrp, setpgid};

use super::channel::{
    InitReport, Report, ReportReader, go_pipe, read_init_report, report_pipe, send_go,
};
#[cfg(target_os = "linux")]
use super::channel::{hooks_pipe, status_pipe};
use super::child::{SETUP_FAILED, child_main};
use super::failure::{FailureContext, describe};
#[cfg(target_os = "linux")]
use super::plan::ReexecFds;
use super::plan::{ChildPlan, Handshake};
use super::signal::Signal;
use super::spec::ProcessSpec;
use super::status::ExitStatus;
use super::supervisor::NetworkHook;
use crate::isolation::{Isolation, Join};
use crate::{Error, Result};

/// A `shipyard-runtime` container's start gate, inherited by its init
/// (`init.rs`): descriptors `O_PATH` of FIFOs in its state directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Gate {
    /// The exec FIFO: the init waits there until `start` takes its byte.
    pub(crate) fifo: RawFd,
    /// The started FIFO, if `start` runs `poststart` hooks: the init keeps
    /// it open for writing (close-on-exec) until the command has
    /// `execve`d, and `start` waits until nobody has it open so.
    pub(crate) started: Option<RawFd>,
}

impl Gate {
    /// Its descriptors.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux isolates")
    )]
    pub(crate) fn fds(self) -> impl Iterator<Item = RawFd> {
        std::iter::once(self.fifo).chain(self.started)
    }
}

/// How to start the process.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnOptions {
    /// Put the command in its own process group, so it and everything it
    /// starts can be signalled together. Only without a terminal: on a
    /// terminal, a separate group would be stopped the moment it reads input.
    pub(crate) new_process_group: bool,
    /// Signals the command keeps ignoring instead of having reset to their
    /// defaults: the ones `nohup` or a script's `&` asked shipyard to ignore.
    pub(crate) keep_ignored: Vec<libc::c_int>,
    /// Which namespaces the command starts in.
    pub(crate) placement: Placement,
    /// Isolated: hold the command back after the setup until something
    /// opens this FIFO for reading (an `O_PATH` descriptor: the runtime's
    /// exec FIFO, see `container`). The init closes the report pipe first,
    /// so its end-of-file means "set up, waiting".
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux isolates")
    )]
    pub(crate) gate: Option<Gate>,
    /// Isolated: the cgroup the container starts in (made by the caller,
    /// who removes it once the container is gone).
    #[cfg(target_os = "linux")]
    pub(crate) cgroup: Option<crate::cgroup::Cgroup>,
    /// The descriptors to put on the child's 0, 1 and 2 (`None`: keep
    /// shipyard's own). Open until `spawn` returns (see `stdio.rs`).
    pub(crate) stdio: [Option<RawFd>; 3],
    /// Joined (`exec`): how.
    #[cfg(target_os = "linux")]
    pub(crate) exec: ExecSpawn,
    /// The command's pseudo-terminal: its slave (no isolation), or the
    /// socket the container's init sends it back on (see `stdio.rs`).
    pub(crate) terminal: Option<super::stdio::ChildTerminal>,
    /// The command takes a pseudo-terminal (`terminal`, or one its init
    /// makes), so it starts a session of its own even on the host (see
    /// [`SpawnOptions::starts_session`]).
    pub(crate) takes_terminal: bool,
    /// Descriptors passed on to the command (see `passed.rs`): their
    /// numbers here (held open by the caller until `spawn` returns), and
    /// socket activation's part.
    pub(crate) passing: Option<super::passed::PassedPlan>,
}

impl SpawnOptions {
    /// Whether the command starts a session of its own: isolated or joined
    /// ([`Placement::has_own_session`]), or to take a pseudo-terminal. It
    /// then leads its own group, and nothing here may make it a group
    /// leader first (its `setsid` would fail).
    pub(crate) fn starts_session(&self) -> bool {
        self.placement.has_own_session() || self.takes_terminal
    }
}

/// How a joined command (`exec`) starts.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecSpawn {
    /// Detached (`exec -d`): it outlives shipyard (no parent-death signal).
    pub(crate) detach: bool,
}

/// Which namespaces the command starts in.
#[derive(Debug, Clone, Default)]
pub(crate) enum Placement {
    /// Shipyard's own.
    #[default]
    Host,
    /// New ones (Linux). Boxed: an isolation is much larger than a join.
    New(Box<Isolation>),
    /// A running process's (Linux; see `join.rs`).
    Join(Join),
}

impl SpawnOptions {
    /// The slave the child takes as its terminal itself: one made here (no
    /// isolation). An isolated command's is made by the container's init,
    /// which gives it to the command.
    fn slave(&self) -> Option<RawFd> {
        match self.terminal {
            Some(super::stdio::ChildTerminal::Slave(slave)) => Some(slave),
            Some(
                super::stdio::ChildTerminal::Socket { .. }
                | super::stdio::ChildTerminal::Console { .. },
            )
            | None => None,
        }
    }
}

impl Placement {
    /// The new namespaces, if that's where the command starts.
    #[cfg(target_os = "linux")]
    pub(crate) fn isolation(&self) -> Option<&Isolation> {
        match self {
            Self::New(isolation) => Some(isolation),
            Self::Host | Self::Join(_) => None,
        }
    }

    /// Whether the command starts a session of its own (isolated or joined;
    /// on the host, one that takes a pseudo-terminal does too: see
    /// [`SpawnOptions::starts_session`]). It then has its own process group
    /// too, so `new_process_group` doesn't apply, and it never shares the
    /// terminal's group.
    pub(crate) fn has_own_session(&self) -> bool {
        !matches!(self, Self::Host)
    }
}

/// A started process.
#[derive(Debug)]
pub(crate) struct Child {
    pid: Pid,
    /// The command's own process group, if it has one.
    group: Option<Pid>,
    /// Until `exec` succeeds or fails.
    report: Option<ReportReader>,
    /// The parent's end of the go pipe, held open until `exec` resolves: the
    /// child checks it to know the parent is alive (see `channel.rs`).
    go: Option<OwnedFd>,
    /// Isolated: what the container's clean image says.
    status: Option<StatusPipe>,
    /// Isolated, with hooks to run while it's set up: until the clean image
    /// stops for them.
    #[cfg(target_os = "linux")]
    hooks: Option<PendingHooks>,
    context: FailureContext,
    reaped: bool,
}

/// The runtime's hooks at create (`prestart`, `createRuntime`), and the pipe
/// the clean image waits on until they're done (see `channel.rs`).
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PendingHooks {
    container: crate::hooks::ContainerHooks,
    pipe: OwnedFd,
}

/// What the parent does while the child waits for the go byte: it's still
/// dumpable, with its first credentials, and has run none of the command's
/// code. In order:
///
/// - into the container's cgroup, if the clone didn't place it (`in_cgroup`
///   false: the `clone` fallback);
/// - its user namespace's maps (probe U2);
/// - the host's network devices it's to have, moved into its network
///   namespace (`Isolation::net_devices`);
/// - the caller's network hook, with the child's PID and network namespace
///   (the engine's veth and addresses, or the pasta it starts);
/// - `spec`'s OOM score adjustment, with shipyard's privileges.
///
/// If any fails, the child never runs: it's killed and reaped here, before
/// anyone learns its PID.
#[cfg(target_os = "linux")]
fn before_go(
    pid: Pid,
    spec: &ProcessSpec,
    options: &SpawnOptions,
    network: Option<NetworkHook<'_>>,
    in_cgroup: bool,
) -> Result<()> {
    // A container's cgroup the clone couldn't place it in (its fallback).
    let placed = match &options.cgroup {
        Some(cgroup) if !in_cgroup && options.placement.isolation().is_some() => {
            cgroup.add(pid).map_err(Error::from)
        }
        _ => Ok(()),
    };
    let mapped = placed.and_then(|()| {
        match options
            .placement
            .isolation()
            .filter(|isolation| isolation.makes(crate::isolation::Namespace::User))
            .and_then(|isolation| isolation.id_maps.as_ref())
        {
            // A joined user namespace's maps are its own already.
            Some(maps) => super::user_namespace::write_maps(pid, maps),
            None => Ok(()),
        }
    });
    let connected = mapped.and_then(|()| {
        use std::os::fd::AsFd as _;
        let devices = options
            .placement
            .isolation()
            .map_or(&[][..], |isolation| &isolation.net_devices);
        if devices.is_empty() && network.is_none() {
            return Ok(());
        }
        let netns = super::network::open_netns(pid)?;
        super::network::move_devices(netns.as_fd(), devices)?;
        match network {
            Some(hook) => hook(super::NetworkTarget {
                pid: pid.as_raw(),
                netns: netns.as_fd(),
            })
            .map_err(Error::Network),
            None => Ok(()),
        }
    });
    let ready = connected.and_then(|()| match spec.oom_score_adj {
        Some(adj) => set_oom_score_adj(pid, adj),
        None => Ok(()),
    });
    if ready.is_err() {
        let _ = nix::sys::signal::kill(pid, NixSignal::SIGKILL);
        let _ = nix::sys::wait::waitpid(pid, None);
    }
    ready
}

/// Writes the child's OOM score adjustment, from here: shipyard's own
/// privileges apply, not the child's. Lowering it needs `CAP_SYS_RESOURCE` in
/// the host's user namespace, which a child in a user namespace never has,
/// root's container included; set by a process that has it, the value also
/// becomes the lowest the child may set (`oom_score_adj_min`). Whatever the
/// child starts inherits it (its init's command too).
#[cfg(target_os = "linux")]
fn set_oom_score_adj(pid: Pid, adj: i32) -> Result<()> {
    use std::io::Write as _;
    let path = format!("/proc/{pid}/oom_score_adj");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut file| file.write_all(adj.to_string().as_bytes()));
    written.map_err(|err| {
        let errno = err.raw_os_error().map_or(Errno::EIO, Errno::from_raw);
        let hint = if errno == Errno::EACCES {
            " (lowering it needs root: CAP_SYS_RESOURCE; without it, it can only be raised)"
        } else {
            ""
        };
        Error::sys(
            format!("setting the OOM score adjustment to {adj}{hint}"),
            errno,
        )
    })
}

/// Where an isolated container's clean image says it started, and, as the
/// init, sends the command's exit status (the init itself is the process
/// we reap).
#[derive(Debug)]
struct StatusPipe {
    pipe: OwnedFd,
    /// There's an init (a PID namespace): otherwise the clean image became
    /// the command, and how it ended is the command's.
    has_init: bool,
    /// A sealed copy of shipyard's binary was made for the init, and the
    /// file may stand in if the copy won't run (nobody in the container can
    /// write it): which ran is checked once the command runs (see
    /// [`warn_if_init_unsealed`]).
    #[cfg(target_os = "linux")]
    sealed_init: bool,
}

/// Starts the process `spec` describes and returns without waiting.
///
/// Everything that can fail happens before `fork`: once the child exists,
/// this function can't return an error and leave it unsupervised.
///
/// `started` is called with the new PID while every signal is still blocked,
/// so a signal handler can never see the command running without knowing its
/// PID.
pub(crate) fn spawn(
    spec: &ProcessSpec,
    options: &SpawnOptions,
    network: Option<NetworkHook<'_>>,
    started: impl FnOnce(Pid),
) -> Result<Child> {
    #[cfg(target_os = "linux")]
    if let Placement::Join(join) = &options.placement {
        return spawn_joined(spec, options, join, started);
    }
    let (report_read, report_write) = report_pipe()?;
    let report = ReportReader::new(report_read)?;
    let (go_read, go_write) = go_pipe()?;
    let handshake = Handshake {
        read: go_read.as_raw_fd(),
        parent_end: go_write.as_raw_fd(),
    };
    #[cfg(target_os = "linux")]
    let joins = match options.placement.isolation() {
        Some(isolation) => {
            check_proc_numbers_children(isolation, network.is_some())?;
            // Another container's network is its own to set up, never this
            // one's hook's.
            if network.is_some() && isolation.is_joined(crate::isolation::Namespace::Network) {
                return Err(Error::Network(
                    "a container that joins another's network gets no network of its own"
                        .to_owned(),
                ));
            }
            Some(open_joins(isolation)?)
        }
        None => None,
    };
    #[cfg(target_os = "linux")]
    let isolated = CleanImageFiles::for_spawn(spec, options, joins.as_ref())?;
    #[cfg(target_os = "linux")]
    let reexec_fds = isolated
        .as_ref()
        .map(|files| files.fds(report_write.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let reexec_fds = None;
    // The container's cgroup, open, for the clone (before the plan, which
    // lists every descriptor the child must not keep).
    #[cfg(target_os = "linux")]
    let cgroup_dir = match (&options.cgroup, options.placement.isolation()) {
        (Some(cgroup), Some(_)) => Some(cgroup.open_dir()?),
        _ => None,
    };
    // Last, so its list of open descriptors includes the pipes above.
    let (plan, context) = ChildPlan::new(spec, options, handshake, reexec_fds)?;
    let plan = ChildPlan {
        stdio: options.stdio,
        terminal: options.slave(),
        ..plan
    };

    // Block every signal across `fork`: the supervisor's handler must never
    // run in the child before the child resets it.
    let mask = BlockAllSignals::new()?;
    // SAFETY: the child branch only runs `child_main`, which is written to be
    // safe after `fork` (no allocation, locks or panics; see child.rs).
    let (pid, in_cgroup) = match unsafe {
        launch(
            options,
            #[cfg(target_os = "linux")]
            joins.as_ref(),
            #[cfg(target_os = "linux")]
            cgroup_dir.as_ref().map(AsRawFd::as_raw_fd),
        )
    }? {
        Launched::Child => {
            // SAFETY: this is the child, right after `fork`/`clone`, with all
            // signals blocked; `plan` and the pipes were prepared above.
            unsafe { child_main(&plan, report_write.as_raw_fd()) }
        }
        Launched::Parent { pid, in_cgroup } => (pid, in_cgroup),
    };
    #[cfg(target_os = "linux")]
    before_go(pid, spec, options, network, in_cgroup)?;
    #[cfg(not(target_os = "linux"))]
    let _ = (in_cgroup, network);
    started(pid);
    drop(mask);
    drop(report_write);
    drop(go_read);
    #[cfg(target_os = "linux")]
    let (status, hooks) = isolated.map(|files| files.parent_ends(options)).unzip();
    #[cfg(not(target_os = "linux"))]
    let status = None;
    let group = if options.starts_session() {
        // The child starts its own session: it leads the new group.
        Some(pid)
    } else {
        options.new_process_group.then(|| {
            // The child does this too; doing it here as well closes the
            // window before the child gets to it. Errors mean the child
            // already did it, or already exec'd or exited: all fine.
            let _ = setpgid(pid, pid);
            pid
        })
    };
    // Everything the child waits for is done.
    send_go(&go_write);
    tracing::debug!(pid = pid.as_raw(), program = %context.program.display(), "started");
    Ok(Child {
        pid,
        group,
        report: Some(report),
        go: Some(go_write),
        status,
        #[cfg(target_os = "linux")]
        hooks: hooks.flatten(),
        context,
        reaped: false,
    })
}

/// How long the joiner may take to start `exec`'s command (it takes
/// milliseconds; a host under heavy load, more).
#[cfg(target_os = "linux")]
const JOINER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// [`spawn`] for a command joining a running container (`exec`; see
/// `exec.rs`): the joiner starts it, in the container's namespaces and
/// cgroup, as shipyard's own child.
#[cfg(target_os = "linux")]
fn spawn_joined(
    spec: &ProcessSpec,
    options: &SpawnOptions,
    join: &Join,
    started: impl FnOnce(Pid),
) -> Result<Child> {
    use super::exec::{joiner_files, read_joiner_record, start_joiner};
    super::internal::refuse_unsafe_dumpable("exec")?;
    // A joined command's terminal is made in the container, never on the
    // host: never run it without the one it asked for.
    if let Some(super::stdio::ChildTerminal::Slave(_)) = options.terminal {
        return Err(Error::sys(
            "a joined command's terminal is made in the container, not on the host",
            Errno::EINVAL,
        ));
    }
    let (report_read, report_write) = report_pipe()?;
    let mut report = ReportReader::new(report_read)?;
    let (go_read, go_write) = go_pipe()?;
    let (record_read, record_write) = report_pipe()?;
    let joining = super::join::Joining::open(join)?;
    let cgroup = join
        .cgroup
        .as_ref()
        .map(crate::cgroup::Cgroup::open_dir)
        .transpose()?;
    // The one recorded, checked on the descriptor the clone uses: not a
    // directory made in its place since.
    if let (Some(dir), Some(expected)) = (&cgroup, join.cgroup_inode)
        && nix::sys::stat::fstat(dir).map(|stat| stat.st_ino).ok() != Some(expected)
    {
        return Err(Error::sys(
            "the container's cgroup isn't the one it was given (made again since)",
            Errno::ESTALE,
        ));
    }
    // What explains the command's failures: the plan the joiner builds.
    let context = super::plan::exec_failure_context(spec, join, options.passing.as_ref())?;
    let files = joiner_files(
        spec,
        &options.keep_ignored,
        &join.security,
        &joining,
        super::exec::JoinerPipes {
            cgroup: cgroup.as_ref().map(AsRawFd::as_raw_fd),
            report: report_write.as_raw_fd(),
            go: go_read.as_raw_fd(),
            record: record_write.as_raw_fd(),
            terminal: exec_terminal(options, &joining),
        },
        options.exec.detach,
        options.passing.as_ref(),
    )?;
    let mask = BlockAllSignals::new()?;
    let mut joiner = start_joiner(&joining, &files, options.stdio)?;
    // The joiner and its clone have their copies.
    drop((report_write, go_read, record_write, files, joining, cgroup));
    let record = read_joiner_record(&record_read, JOINER_WAIT);
    if record.is_none() {
        let _ = joiner.kill();
    }
    let _ = joiner.wait();
    let Some((pid, in_cgroup)) = record else {
        // A command it may have started waits for go: with our end
        // closed, it gives up. What the joiner (or it) reported says why.
        drop(go_write);
        let failure = joiner_failure(&mut report, &context);
        reap_stray_exec();
        drop(mask);
        return Err(failure);
    };
    let placed = match &join.cgroup {
        Some(cgroup) if !in_cgroup => cgroup.add(pid).map_err(|err| match err {
            crate::cgroup::CgroupError::Io {
                errno: errno @ (Errno::EBUSY | Errno::EACCES | Errno::ENOENT),
                ..
            } => describe(
                super::failure::ChildFailure {
                    step: super::failure::ChildStep::ExecClone,
                    errno,
                    detail: 0,
                },
                &context,
            )
            .into(),
            _ => Error::from(err),
        }),
        _ => Ok(()),
    };
    if let Err(err) = placed {
        let _ = nix::sys::signal::kill(pid, NixSignal::SIGKILL);
        let _ = nix::sys::wait::waitpid(pid, None);
        return Err(err);
    }
    started(pid);
    drop(mask);
    send_go(&go_write);
    tracing::debug!(pid = pid.as_raw(), program = %context.program.display(), "joined");
    Ok(Child {
        pid,
        // A session of its own.
        group: Some(pid),
        report: Some(report),
        go: Some(go_write),
        status: None,
        hooks: None,
        context,
        reaped: false,
    })
}

/// Reaps a command the joiner cloned before it failed, which it couldn't
/// tell us about: with `CLONE_PARENT` it's our child (named after the
/// joiner), gone by now (its go pipe closed). Unreaped, it would hold up the
/// end of the container's PID namespace.
#[cfg(target_os = "linux")]
fn reap_stray_exec() {
    let me = std::process::id().to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(str::trim)
        };
        if field("PPid:") == Some(me.as_str()) && field("Name:") == Some("shipyard-exec") {
            let _ = nix::sys::signal::kill(Pid::from_raw(pid), NixSignal::SIGKILL);
            let _ = nix::sys::wait::waitpid(Pid::from_raw(pid), None);
        }
    }
}

/// The joined command's terminal, if it has one (`exec -t`): the socket its
/// master goes to (shipyard's relay, which answers, or a console socket,
/// which doesn't), and the init's `/proc` directory, through whose root the
/// joiner reaches the container's devpts.
#[cfg(target_os = "linux")]
fn exec_terminal(
    options: &SpawnOptions,
    joining: &super::join::Joining,
) -> Option<(super::setup::TerminalSetup, RawFd)> {
    // (A slave made on the host: refused by `spawn_joined` already.)
    let (fd, size, acknowledged) = options.terminal?.socket()?;
    Some((
        super::setup::TerminalSetup {
            socket: fd,
            size,
            acknowledged,
        },
        joining.directory.as_raw_fd(),
    ))
}

/// Why the joiner gave up: the failure it (or the command) reported, once
/// both are gone.
#[cfg(target_os = "linux")]
fn joiner_failure(report: &mut ReportReader, context: &FailureContext) -> Error {
    let deadline = std::time::Instant::now() + JOINER_WAIT;
    loop {
        match report.read() {
            Ok(Report::Failed(failure)) => return describe(failure, context).into(),
            Ok(Report::Pending) if std::time::Instant::now() < deadline => {
                let mut fds = [nix::poll::PollFd::new(
                    report.fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                let _ = nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(100_u16));
            }
            Ok(_) => {
                return Error::sys(
                    "exec's joiner ended without starting the command",
                    Errno::ECHILD,
                );
            }
            Err(err) => return err,
        }
    }
}

/// Checks that our `/proc` numbers the child as we do, where the parent
/// writes to `/proc/PID` before the go byte (the maps; the moves of network
/// devices and the network hook, to `/proc/PID/ns/net`): another PID
/// namespace's `/proc` would give the same number to another process.
#[cfg(target_os = "linux")]
fn check_proc_numbers_children(isolation: &Isolation, hooked: bool) -> Result<()> {
    let writes_proc = isolation.id_maps.is_some() || !isolation.net_devices.is_empty() || hooked;
    if writes_proc && !super::proc_is_ours() {
        return Err(Error::ForeignProc);
    }
    Ok(())
}

/// The namespaces `isolation` joins, opened before anything starts. A
/// network namespace that's shipyard's own (the host's, usually) is the
/// container's in nothing: its settings and devices aren't the container's
/// to change.
#[cfg(target_os = "linux")]
fn open_joins(isolation: &Isolation) -> Result<super::joins::NamespaceJoins> {
    use crate::isolation::Namespace;
    let joins = super::joins::NamespaceJoins::open(isolation)?;
    let changes_it = !isolation.net_devices.is_empty()
        || isolation
            .sysctls
            .iter()
            .any(|(key, _)| key.starts_with("net."));
    if changes_it && let Some(whose) = joins.shipyards_own_joined(Namespace::Network) {
        return Err(Error::Network(format!(
            "the network namespace {whose} is shipyard's own (the host's): network settings and \
             devices can't be given to a container that joins it"
        )));
    }
    Ok(joins)
}

/// The command's result, from what the container's init sent (`report`)
/// and how the init itself ended (`init`); `has_init` says whether there
/// was one.
///
/// The init's own exit code always mirrors the command's (128 + N for a
/// signal), so a status record is only believed when it agrees with it:
/// anything in the container that can reach the pipe could forge one. The
/// record adds what the exit code can't say, whether the command died by a
/// signal.
fn init_outcome(report: InitReport, init: ExitStatus, has_init: bool) -> Result<ExitStatus> {
    match report {
        InitReport::Finished(raw) => {
            let command =
                ExitStatus::from_wait_status(raw).filter(|command| command.code() == init.code());
            if command.is_none() {
                tracing::warn!(
                    %init,
                    "the container's init reported a status that doesn't match how it \
                     exited; using its own exit status"
                );
            }
            Ok(command.unwrap_or(init))
        }
        // The init exits as a setup failure when it gives up; with any
        // other code the mark wasn't its (see `Unexpected`).
        InitReport::GaveUp if is_setup_failure(init) => Err(Error::InitGaveUp),
        // An init killed by SIGSYS: its seccomp filter (the container's)
        // refused a call it needs with a kill. (The command's own SIGSYS
        // comes as a status record, and as the init's exit code 159.)
        InitReport::Started
            if has_init
                && matches!(init, ExitStatus::Signaled { signal, .. } if signal.as_raw() == libc::SIGSYS) =>
        {
            Err(Error::InitKilledBySeccomp)
        }
        // No init (the process reaped *was* the command), or an init killed
        // before the command ended (the stop timeout's SIGKILL).
        InitReport::Started => Ok(init),
        InitReport::GaveUp | InitReport::Unexpected => {
            tracing::warn!(
                "unexpected data on the container's status pipe (written from inside the \
                 container?); using the init's own exit status"
            );
            Ok(init)
        }
        // Exited without ever saying it started: the re-exec'd image failed
        // to start. (Killed before it could say so is an ordinary end, say
        // by the stop timeout.)
        InitReport::Nothing => match init {
            ExitStatus::Exited(code) => Err(Error::InitDidNotStart { code }),
            _ => Ok(init),
        },
    }
}

/// Whether `status` is a setup failure's exit (125).
fn is_setup_failure(status: ExitStatus) -> bool {
    matches!(status, ExitStatus::Exited(code) if libc::c_int::from(code) == SETUP_FAILED)
}

/// What an isolated spawn opens for the cloned child, which re-execs the
/// shipyard binary as the container's clean image (see `init.rs`).
#[cfg(target_os = "linux")]
struct CleanImageFiles {
    /// Where the clean image says it started and, as the init, sends the
    /// command's exit status. Only it writes there: the write end is closed
    /// here once the child exists.
    status_read: OwnedFd,
    status_write: OwnedFd,
    /// The binary the child re-execs: a sealed copy of shipyard's (see
    /// [`own_binary`]).
    exe: OwnedFd,
    /// Shipyard's binary file itself, when `exe` is a copy: for a host
    /// that won't run a memfd (a security module's policy).
    host_exe: Option<OwnedFd>,
    /// The setup, in a sealed memfd (see `setup.rs`).
    setup: OwnedFd,
    /// The start gate, if any (`SpawnOptions::gate`).
    gate: Option<Gate>,
    /// The hooks pipe (read, write), if the setup stops for hooks.
    hooks: Option<(OwnedFd, OwnedFd)>,
    /// An image's overlay made here (see [`prebuilt_overlay`]).
    overlay: Option<OwnedFd>,
    /// The mount namespace whose `/dev/shm` the container shares (the
    /// joins hold it).
    shm_mount_ns: Option<RawFd>,
    /// The socket the init sends the container's terminal back on (the
    /// child's end: the supervisor holds it, and closes it after the spawn).
    terminal: Option<RawFd>,
    /// The descriptors passed on to the command (the caller holds them).
    passing: Vec<RawFd>,
}

#[cfg(target_os = "linux")]
impl CleanImageFiles {
    /// [`open`](Self::open)'s, for an isolated spawn (none otherwise), with
    /// the shared memory of the IPC namespace `joins` join, if any.
    fn for_spawn(
        spec: &ProcessSpec,
        options: &SpawnOptions,
        joins: Option<&super::joins::NamespaceJoins>,
    ) -> Result<Option<Self>> {
        options
            .placement
            .isolation()
            .map(|isolation| {
                Self::open(
                    spec,
                    options,
                    isolation,
                    // Only where a root is set up: that's where it's let go.
                    joins
                        .and_then(super::joins::NamespaceJoins::shm_mount_ns)
                        .filter(|_| isolation.root.is_some()),
                )
            })
            .transpose()
    }
}

#[cfg(target_os = "linux")]
impl CleanImageFiles {
    fn open(
        spec: &ProcessSpec,
        options: &SpawnOptions,
        isolation: &Isolation,
        shm_mount_ns: Option<RawFd>,
    ) -> Result<Self> {
        // Already checked by the supervisor; checked again because the
        // setup trusts it (a PID namespace comes with a mount namespace, a
        // hostname with a UTS one).
        isolation.validate()?;
        let (status_read, status_write) = status_pipe()?;
        let hooks = isolation
            .hooks
            .as_ref()
            .is_some_and(|container| container.hooks.during_setup())
            .then(hooks_pipe)
            .transpose()?;
        let overlay = prebuilt_overlay(isolation)?;
        // The binary's file, where no sealed copy can be made or run: only
        // if nobody in the container could write it (CVE-2019-5736).
        let maps = isolation
            .id_maps
            .as_ref()
            .filter(|_| isolation.has(crate::isolation::Namespace::User));
        let (exe, host_exe) = own_binary(
            |file| super::internal::fallback_allowed(file, maps),
            |why| Error::Unsealed {
                what: "the container's init",
                why,
            },
        )?;
        let terminal = options
            .terminal
            .and_then(super::stdio::ChildTerminal::socket);
        Ok(Self {
            status_read,
            status_write,
            exe,
            host_exe,
            setup: {
                let mut setup = super::setup::Setup::new(
                    spec,
                    &options.keep_ignored,
                    isolation,
                    options.gate,
                    options.cgroup.is_some(),
                    hooks.as_ref().map(|(read, _)| read.as_raw_fd()),
                    overlay.as_ref().map(AsRawFd::as_raw_fd),
                );
                setup.terminal =
                    terminal.map(|(fd, size, acknowledged)| super::setup::TerminalSetup {
                        socket: fd,
                        size,
                        acknowledged,
                    });
                setup.passing.clone_from(&options.passing);
                if let Some(root) = &mut setup.root {
                    root.shm_mount_ns = shm_mount_ns;
                }
                setup.to_memfd()?
            },
            gate: options.gate,
            hooks,
            overlay,
            shm_mount_ns,
            terminal: terminal.map(|(fd, ..)| fd),
            passing: options
                .passing
                .as_ref()
                .map(|passing| passing.sources.clone())
                .unwrap_or_default(),
        })
    }

    /// What the parent keeps once the child exists: the status pipe's read
    /// end (only the clean image writes to it; with our copy closed, it
    /// reads as empty once that process is gone), and the hooks to run when
    /// the clean image stops for them, with the hooks pipe's write end. The
    /// rest (the read end among them: the clean image's alone by now) is
    /// closed.
    fn parent_ends(self, options: &SpawnOptions) -> (StatusPipe, Option<PendingHooks>) {
        let status = StatusPipe {
            pipe: self.status_read,
            has_init: options
                .placement
                .isolation()
                .is_some_and(|isolation| isolation.has(crate::isolation::Namespace::Pid)),
            sealed_init: self.host_exe.is_some(),
        };
        let hooks = self.hooks.and_then(|(_, pipe)| {
            let container = options.placement.isolation()?.hooks.clone()?;
            Some(PendingHooks { container, pipe })
        });
        (status, hooks)
    }

    /// The descriptor numbers the child passes on, with the report pipe's
    /// write end.
    fn fds(&self, report: RawFd) -> ReexecFds {
        ReexecFds {
            setup: self.setup.as_raw_fd(),
            status: self.status_write.as_raw_fd(),
            report,
            exe: self.exe.as_raw_fd(),
            host_exe: self.host_exe.as_ref().map(AsRawFd::as_raw_fd),
            gate: self.gate,
            hooks: self.hooks.as_ref().map(|(read, _)| read.as_raw_fd()),
            overlay: self.overlay.as_ref().map(AsRawFd::as_raw_fd),
            shm_mount_ns: self.shm_mount_ns,
            terminal: self.terminal,
            passing: self.passing.clone(),
        }
    }
}

/// An image's overlay, made here by real root for a container with a user
/// namespace of its own (`--uidmap`): the container's root is another host
/// user, which can't reach the image store (private to root), so it can't
/// mount the overlay itself. Overlay reaches its layers as whoever made it.
/// The detached mount, for the init to attach; `None` where the init mounts
/// it (no user namespace, or rootless: the store is the caller's).
#[cfg(target_os = "linux")]
fn prebuilt_overlay(isolation: &Isolation) -> Result<Option<OwnedFd>> {
    match &isolation.root {
        Some(root) => prebuilt_root_overlay(root, isolation.id_maps.as_ref()),
        None => Ok(None),
    }
}

/// [`prebuilt_overlay`] for `root`, with `id_maps` (a helper's too: real
/// root's mount, which the helper's user namespace couldn't make).
#[cfg(target_os = "linux")]
pub(super) fn prebuilt_root_overlay(
    root: &crate::isolation::Root,
    id_maps: Option<&crate::isolation::IdMaps>,
) -> Result<Option<OwnedFd>> {
    let Some(overlay) = &root.overlay else {
        return Ok(None);
    };
    if id_maps.is_none() || !nix::unistd::geteuid().is_root() {
        return Ok(None);
    }
    let lower: Vec<&std::path::Path> = overlay
        .lower
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();
    // `userxattr`: the mapped layers carry only `user.overlay.*` marks
    // (`trusted.*` ones aren't made for them).
    let mounted = super::rootfs::overlay::build(&lower, &overlay.upper, &overlay.work, true)
        .map_err(|err| match err {
            super::rootfs::overlay::NewApi::Unsupported => Error::sys(
                "mounting the image's overlay for a user namespace (--uidmap with an image \
                 needs Linux 6.8 or later, and an image store on a disk filesystem: see \
                 SHIPYARD_IMAGE_STORE)",
                nix::errno::Errno::ENOSYS,
            ),
            super::rootfs::overlay::NewApi::Failed(errno) => {
                Error::sys("mounting the image's overlay for a user namespace", errno)
            }
        })?;
    Ok(Some(mounted))
}

/// Shipyard's binary for a process of its own that runs in a container (the
/// init, `init.rs`; `exec`'s joiner; `cp`'s helper): a sealed in-memory copy
/// of the running one, and the running one's file, for when a copy can't be
/// run.
///
/// Such a process is inside the container, so the container could reach the
/// file it was started from (`/proc/1/exe`, with the right privileges):
/// were that shipyard's binary on disk, container root could overwrite the
/// program host root runs next (CVE-2019-5736). A sealed memfd can't be
/// changed by anyone, as runc does. Both are opened before the clone, by
/// descriptor: the file on disk may be replaced by an upgrade meanwhile.
///
/// The file is run only where `file_allowed` says nobody in the container
/// could write it ([`fallback_allowed`](super::internal::fallback_allowed)):
/// it's given back for a copy that won't run only then, and where no copy
/// can be made (`vm.memfd_noexec` at 2 refuses executable memfds) it's run
/// in its place, with a warning, only then; else `unsealed()`.
#[cfg(target_os = "linux")]
pub(super) fn own_binary(
    file_allowed: impl FnOnce(std::os::fd::BorrowedFd<'_>) -> bool,
    unsealed: impl FnOnce(&'static str) -> Error,
) -> Result<(OwnedFd, Option<OwnedFd>)> {
    let host = nix::fcntl::open(
        "/proc/self/exe",
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|e| Error::sys("opening shipyard's own binary", e))?;
    let allowed = file_allowed(std::os::fd::AsFd::as_fd(&host));
    match with_file_size_raised(|| sealed_copy(&host)) {
        Ok(copy) => Ok((copy, allowed.then_some(host))),
        Err(errno) if allowed => {
            tracing::warn!(
                "couldn't make a sealed copy of shipyard's binary ({}); the binary's file \
                 itself runs in the container",
                errno.desc()
            );
            Ok((host, None))
        }
        Err(Errno::EFBIG) => Err(unsealed(crate::error::UNSEALED_FILE_SIZE)),
        Err(Errno::EACCES | Errno::EPERM | Errno::EINVAL) => {
            Err(unsealed(crate::error::UNSEALED_POLICY))
        }
        // Out of memory, or of descriptors, say.
        Err(errno) => Err(unsealed(errno.desc())),
    }
}

/// Runs `copy` with the soft file-size limit (`RLIMIT_FSIZE`) raised to the
/// hard one, then puts it back: a caller's soft limit (`ulimit -S -f`) is
/// meant for the files the container writes, not for the sealed copy of
/// shipyard's binary, which would stop at it (`EFBIG`). A hard limit
/// smaller than the binary still stops it.
#[cfg(target_os = "linux")]
fn with_file_size_raised<T>(copy: impl FnOnce() -> T) -> T {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let raised = getrlimit(Resource::RLIMIT_FSIZE)
        .ok()
        .filter(|(soft, hard)| soft < hard)
        .filter(|&(_, hard)| setrlimit(Resource::RLIMIT_FSIZE, hard, hard).is_ok());
    let result = copy();
    if let Some((soft, hard)) = raised {
        let _ = setrlimit(Resource::RLIMIT_FSIZE, soft, hard);
    }
    result
}

/// Warns if the container's init (`pid`) runs shipyard's binary file, not
/// the sealed copy made for it: the copy's exec was refused (a security
/// module's policy on memfds, say), and the child, which can't say so
/// itself, fell back to the file. Silent where the link can't be read (the
/// init's gone, or isn't ours to look at): never a false alarm.
#[cfg(target_os = "linux")]
fn warn_if_init_unsealed(pid: Pid) {
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe"))
        && !runs_sealed_copy(&exe)
    {
        tracing::warn!(
            "the container's init runs shipyard's binary file ({}), not a sealed copy of it \
             (this host refused to run the copy); nobody in the container can write the file",
            exe.display()
        );
    }
}

/// Whether `exe` (a process's `/proc/PID/exe` link) is the init's sealed
/// copy: a memfd named as [`sealed_copy`] names it.
#[cfg(target_os = "linux")]
fn runs_sealed_copy(exe: &std::path::Path) -> bool {
    exe.as_os_str()
        .as_encoded_bytes()
        .starts_with(SEALED_COPY_LINK.as_bytes())
}

/// What `/proc/PID/exe` reads for a process running the sealed copy.
#[cfg(target_os = "linux")]
const SEALED_COPY_LINK: &str = "/memfd:shipyard-init";

/// A sealed, executable memfd holding the file `from` (see [`own_binary`]).
#[cfg(target_os = "linux")]
fn sealed_copy(from: &OwnedFd) -> std::result::Result<OwnedFd, Errno> {
    use nix::fcntl::{FcntlArg, SealFlag, fcntl};
    use nix::sys::memfd::{MFdFlags, memfd_create};

    let flags = MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING;
    // `MFD_EXEC` (Linux 6.3) asks for an executable memfd, which
    // `vm.memfd_noexec` at 1 would otherwise not give; an older kernel
    // doesn't know the flag (EINVAL), and its memfds are all executable.
    let memfd = match memfd_create(
        c"shipyard-init",
        flags | MFdFlags::from_bits_retain(libc::MFD_EXEC),
    ) {
        Err(Errno::EINVAL) => memfd_create(c"shipyard-init", flags)?,
        made => made?,
    };
    let io = |error: std::io::Error| super::io_errno(&error);
    // A duplicate shares the file offset, which the exec doesn't use.
    let mut source = std::fs::File::from(from.try_clone().map_err(io)?);
    let mut copy = std::fs::File::from(memfd);
    // `std::io::copy` copies in the kernel where it can (`copy_file_range`,
    // `sendfile`), and reads and writes otherwise.
    let copied = std::io::copy(&mut source, &mut copy).map_err(io)?;
    if copied != source.metadata().map_err(io)?.len() {
        return Err(Errno::EIO);
    }
    let copy = OwnedFd::from(copy);
    fcntl(
        &copy,
        FcntlArg::F_ADD_SEALS(SealFlag::from_bits_retain(super::setup::SEALS)),
    )?;
    Ok(copy)
}

/// Which side of the `fork` or `clone` this is.
enum Launched {
    Child,
    Parent {
        pid: Pid,
        /// Started in the cgroup asked for (`clone3`'s `CLONE_INTO_CGROUP`).
        in_cgroup: bool,
    },
}

/// Starts the child: with `clone` into new namespaces if isolated (in the
/// cgroup whose directory is `cgroup`, where `clone3` can), in the
/// namespaces `joins` opened (entered around the clone, or, with another's
/// user namespace, by the enterer), with `fork` otherwise.
///
/// # Safety
///
/// As for `fork`: the child may only run async-signal-safe code.
unsafe fn launch(
    options: &SpawnOptions,
    #[cfg(target_os = "linux")] joins: Option<&super::joins::NamespaceJoins>,
    #[cfg(target_os = "linux")] cgroup: Option<RawFd>,
) -> Result<Launched> {
    #[cfg(target_os = "linux")]
    if let Some(isolation) = options.placement.isolation() {
        use super::clone::{Cloned, clone_process};
        // Another container's user namespace: entered by a process of its
        // own, which starts the child.
        let cloned = if let Some(joins) = joins.filter(|joins| joins.joins_user()) {
            use super::enterer::Started;
            // SAFETY: the caller's contract.
            match unsafe { super::enterer::start(joins, isolation.clone_flags(), cgroup) }? {
                Started::Cloned(cloned) => cloned,
                Started::InTheChild => return Ok(Launched::Child),
            }
        } else {
            // The namespaces to join: entered for the clone, so the child
            // starts in them (`clone_flags` leaves their flags out), and
            // left right after.
            if let Some(joins) = joins {
                joins.enter()?;
            }
            // SAFETY: the caller's contract; the flags are namespace flags
            // only (`Isolation::validate` refuses CLONE_NEWTIME).
            unsafe { clone_process(isolation.clone_flags(), cgroup) }
        };
        if let Some(joins) = joins.filter(|joins| !joins.joins_user()) {
            match &cloned {
                // The child stays: it's to start in that namespace.
                Ok(Cloned::Child) => {}
                Ok(Cloned::Parent { pid, .. }) => joins.leave(Some(*pid)),
                Err(_) => joins.leave(None),
            }
        }
        return match cloned {
            Ok(Cloned::Child) => Ok(Launched::Child),
            Ok(Cloned::Parent { pid, in_cgroup }) => Ok(Launched::Parent { pid, in_cgroup }),
            // Root and still refused: a container without privileges, or a
            // security policy.
            Err(Errno::EPERM) if nix::unistd::geteuid().is_root() => Err(Error::IsolationRefused),
            // Without root, the user namespace is what the kernel refused:
            // the others are its own.
            // (AppArmor's restriction refuses with EACCES.)
            Err(Errno::EPERM | Errno::EACCES)
                if isolation.makes(crate::isolation::Namespace::User) =>
            {
                Err(Error::UserNamespacesRefused {
                    reason: super::user_namespace::userns_refusal(),
                })
            }
            Err(Errno::EPERM) => Err(Error::IsolationNeedsRoot),
            // The kernel's limit on namespaces is reached (or set to 0).
            Err(Errno::ENOSPC) => Err(Error::sys(
                "starting the command in new namespaces (clone): the system's limit on \
                 namespaces is reached (see /proc/sys/user/max_*_namespaces)",
                Errno::ENOSPC,
            )),
            // `CLONE_INTO_CGROUP`'s own refusals: a cgroup that can't take
            // processes (a threaded or "domain invalid" one; one whose
            // parent holds processes) or one being removed.
            Err(errno @ (Errno::EBUSY | Errno::EOPNOTSUPP | Errno::ENODEV))
                if let Some(cgroup) = &options.cgroup =>
            {
                Err(Error::sys(
                    format!(
                        "starting the command in its cgroup {} (clone3)",
                        cgroup.path().display()
                    ),
                    errno,
                ))
            }
            Err(errno) => Err(Error::sys(
                "starting the command in new namespaces (clone)",
                errno,
            )),
        };
    }
    #[cfg(not(target_os = "linux"))]
    if options.placement.has_own_session() {
        return Err(Error::LinuxOnly {
            feature: "namespace isolation",
            os: std::env::consts::OS,
        });
    }
    // SAFETY: the caller's contract.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => Ok(Launched::Child),
        Ok(ForkResult::Parent { child }) => Ok(Launched::Parent {
            pid: child,
            in_cgroup: false,
        }),
        Err(errno) => Err(fork_error(errno)),
    }
}

/// A failed `fork`, explained.
pub(super) fn fork_error(errno: Errno) -> Error {
    Error::sys(
        if errno == Errno::EAGAIN {
            "starting the command: too many processes"
        } else {
            "starting the command (fork)"
        },
        errno,
    )
}

impl Child {
    pub(crate) fn pid(&self) -> Pid {
        self.pid
    }

    /// The report pipe, while `exec` hasn't resolved yet.
    pub(crate) fn report_fd(&self) -> Option<BorrowedFd<'_>> {
        self.report.as_ref().map(ReportReader::fd)
    }

    /// Reads the report pipe without blocking, if `exec` hasn't resolved yet.
    /// A failure becomes the error that explains it; the caller still has to
    /// reap the child.
    pub(crate) fn check_exec(&mut self) -> Result<()> {
        let Some(report) = self.report.as_mut() else {
            return Ok(());
        };
        let resolved = match report.read()? {
            Report::Pending => return Ok(()),
            // Not resolved, unless a hook failed.
            Report::AtHooks => match self.run_create_hooks() {
                Ok(()) => return Ok(()),
                Err(err) => Err(err),
            },
            Report::Running => {
                #[cfg(target_os = "linux")]
                if self
                    .status
                    .as_ref()
                    .is_some_and(|status| status.sealed_init)
                {
                    warn_if_init_unsealed(self.pid);
                }
                Ok(())
            }
            Report::Failed(failure) => Err(describe(failure, &self.context).into()),
        };
        self.report = None;
        self.go = None;
        resolved
    }

    /// The clean image stopped for hooks: runs `prestart`, then
    /// `createRuntime`, with the container's state, and lets it go on by
    /// writing that state into the hooks pipe. If one fails, the pipe is
    /// closed unwritten, and the clean image gives up.
    #[cfg(target_os = "linux")]
    fn run_create_hooks(&mut self) -> Result<()> {
        use crate::hooks::Phase;
        // It only stops when there's a pipe to wait on: anything else is a
        // forged record.
        let PendingHooks { container, pipe } = self.hooks.take().ok_or_else(|| {
            Error::sys(
                "the container stopped for hooks it doesn't have",
                Errno::EPROTO,
            )
        })?;
        let pid = Some(self.pid.as_raw());
        container.run(Phase::Prestart, pid)?;
        container.run(Phase::CreateRuntime, pid)?;
        let state = crate::hooks::state_json(&container.state(Phase::CreateContainer, pid));
        // Blocking is fine: the clean image reads it all, or dies (EPIPE).
        // Dropped then: end-of-file says it's all there.
        let _ = std::io::Write::write_all(&mut std::fs::File::from(pipe), &state);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    #[expect(
        clippy::unused_self,
        clippy::unnecessary_wraps,
        reason = "the same signature as Linux's, where containers have hooks"
    )]
    fn run_create_hooks(&mut self) -> Result<()> {
        Ok(())
    }

    /// Sends `signal` to the command. Safe from PID reuse: an unreaped child
    /// keeps its PID, and after reaping nothing is sent.
    pub(crate) fn signal(&self, signal: libc::c_int) {
        if self.reaped {
            return;
        }
        // SAFETY: `kill` has no memory-safety preconditions. Raw libc, since
        // nix's `Signal` can't express Linux real-time signals.
        unsafe { libc::kill(self.pid.as_raw(), signal) };
    }

    /// The signal that stopped the command, if it's stopped right now.
    ///
    /// Peeks with `waitid(WSTOPPED | WNOWAIT)`: nothing is consumed, and
    /// without `WEXITED` an exited command is never reaped here. Both Linux
    /// and macOS report only a *current* stop: once SIGCONT resumed the
    /// command, this returns `None` again.
    pub(crate) fn stopped(&self) -> Option<libc::c_int> {
        if self.reaped {
            return None;
        }
        stop_signal_of(self.pid)
    }

    /// Continues the command and, if it has its own group, everything in it:
    /// a stopped process can't act on a stop request.
    pub(crate) fn continue_all(&self) {
        self.signal(libc::SIGCONT);
        if let Some(group) = self.own_group() {
            // SAFETY: `killpg` has no memory-safety preconditions.
            unsafe { libc::killpg(group.as_raw(), libc::SIGCONT) };
        }
    }

    /// Kills the command and, if it has its own group, everything in it.
    pub(crate) fn kill_all(&self) {
        if self.reaped {
            return;
        }
        // The command itself first: right after `fork` its group may not
        // exist yet, and then `killpg` alone would miss it.
        self.signal(libc::SIGKILL);
        if let Some(group) = self.own_group() {
            // SAFETY: `killpg` has no memory-safety preconditions. ESRCH
            // (an empty group) is harmless.
            unsafe { libc::killpg(group.as_raw(), libc::SIGKILL) };
        }
    }

    /// The command's own process group, while it's safe to signal: not yet
    /// reaped (an unreaped command keeps the group ID from being reused),
    /// and never shipyard's own group.
    fn own_group(&self) -> Option<Pid> {
        self.group
            .filter(|group| !self.reaped && *group != getpgrp())
    }

    /// Reaps the command, which must have exited, and returns how it ended.
    ///
    /// Isolated, the process reaped is the container's init, and the
    /// command's own status comes over the status pipe. If the init sent
    /// none (it was killed, by the stop timeout's SIGKILL, or failed before
    /// starting the command), its own status is the answer.
    pub(crate) fn reap(&mut self) -> Result<ExitStatus> {
        let reaped = wait_pid(self.pid).inspect_err(|_| {
            // ECHILD and the like: the PID is no longer ours to signal.
            self.reaped = true;
        })?;
        self.reaped = true;
        let status = match self.status.take() {
            Some(status) => init_outcome(read_init_report(&status.pipe), reaped, status.has_init)?,
            None => reaped,
        };
        tracing::debug!(pid = self.pid.as_raw(), %status, "exited");
        Ok(status)
    }
}

/// The signal that stopped child `pid`, if it's stopped now. Raw
/// `libc::waitid`: nix doesn't provide it on macOS.
fn stop_signal_of(pid: Pid) -> Option<libc::c_int> {
    let id = libc::id_t::try_from(pid.as_raw()).ok()?;
    loop {
        // SAFETY: a zeroed `siginfo_t` is valid; it stays zeroed if no child
        // matches.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is valid, writable storage for the result.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            if Errno::last() == Errno::EINTR {
                continue;
            }
            return None;
        }
        // SAFETY: `waitid` filled `info` (or left it zeroed: `si_pid` 0).
        let (reported, code, status) = unsafe { (info.si_pid(), info.si_code, info.si_status()) };
        return (reported == pid.as_raw() && code == libc::CLD_STOPPED).then_some(status);
    }
}

/// `waitpid` for one child, retried on EINTR, decoded with the raw macros.
fn wait_pid(pid: Pid) -> Result<ExitStatus> {
    loop {
        let mut raw = 0;
        // SAFETY: `raw` is a valid place for the status; `pid` is our child.
        let rc = unsafe { libc::waitpid(pid.as_raw(), &raw mut raw, 0) };
        if rc == -1 {
            match Errno::last() {
                Errno::EINTR => continue,
                errno => return Err(Error::sys("waiting for the command to exit", errno)),
            }
        }
        // Without WUNTRACED, waitpid only reports exits and deaths by signal.
        if let Some(status) = ExitStatus::from_wait_status(raw) {
            return Ok(status);
        }
    }
}

/// Dies from `signal` the default way, as if it had been delivered to us.
///
/// When the command was killed by Ctrl-C, a shell running shipyard in a loop
/// only stops the loop if shipyard *also* appears to have died from SIGINT
/// (an exit code of 130 isn't enough). Returns only if the signal didn't kill
/// the process; callers then exit with `128 + signal`.
///
/// Does nothing for a signal this process ignores (its parent asked for
/// that, as `nohup` does), or one whose default action isn't to terminate:
/// raising SIGTSTP, for example, would stop shipyard instead.
pub fn die_from(signal: Signal) {
    let Ok(signal) = NixSignal::try_from(signal.as_raw()) else {
        return;
    };
    if !terminates_by_default(signal) || is_ignored(signal) {
        return;
    }
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    // SAFETY: installing SIG_DFL runs none of our code in a signal context.
    let _ = unsafe { sigaction(signal, &default) };
    let mut only = SigSet::empty();
    only.add(signal);
    let _ = sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&only), None);
    let _ = nix::sys::signal::raise(signal);
}

/// Whether the default action for `signal` ends the process.
fn terminates_by_default(signal: NixSignal) -> bool {
    use NixSignal::{SIGCHLD, SIGCONT, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH};
    #[cfg(not(target_os = "linux"))]
    if signal == NixSignal::SIGINFO {
        return false;
    }
    ![
        SIGCHLD, SIGCONT, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH,
    ]
    .contains(&signal)
}

/// Whether this process currently ignores `signal`.
fn is_ignored(signal: NixSignal) -> bool {
    // SAFETY: a zeroed `sigaction` is valid storage; a NULL new action only
    // reads the current one.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let rc = unsafe { libc::sigaction(signal as libc::c_int, std::ptr::null(), &raw mut current) };
    rc == 0 && current.sa_sigaction == libc::SIG_IGN
}

/// Blocks every signal until dropped, then restores the previous mask.
struct BlockAllSignals {
    previous: SigSet,
}

impl BlockAllSignals {
    fn new() -> Result<Self> {
        let mut previous = SigSet::empty();
        sigprocmask(
            SigmaskHow::SIG_SETMASK,
            Some(&SigSet::all()),
            Some(&mut previous),
        )
        .map_err(|e| Error::sys("blocking signals around fork", e))?;
        Ok(Self { previous })
    }
}

impl Drop for BlockAllSignals {
    fn drop(&mut self) {
        let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&self.previous), None);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// "Gave up" is believed only with the init's setup-failure exit code:
    /// with any other, something else wrote it.
    #[test]
    fn an_init_that_gave_up_is_an_error() {
        assert!(matches!(
            init_outcome(InitReport::GaveUp, ExitStatus::Exited(125), true),
            Err(Error::InitGaveUp)
        ));
        assert_eq!(
            init_outcome(InitReport::GaveUp, ExitStatus::Exited(0), true).unwrap(),
            ExitStatus::Exited(0)
        );
    }

    /// An init killed by SIGSYS was killed by its filter; without an init,
    /// the process reaped is the command, whose SIGSYS is its own.
    #[test]
    fn an_init_killed_by_sigsys_is_an_error() {
        let sigsys = ExitStatus::Signaled {
            signal: Signal::from_raw(libc::SIGSYS).unwrap(),
            core_dumped: false,
        };
        assert!(matches!(
            init_outcome(InitReport::Started, sigsys, true),
            Err(Error::InitKilledBySeccomp)
        ));
        assert_eq!(
            init_outcome(InitReport::Started, sigsys, false).unwrap(),
            sigsys
        );
    }

    #[test]
    fn stop_peeks_see_only_a_current_stop_and_consume_nothing() {
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(libc::pid_t::try_from(sleeper.id()).unwrap());
        let until = |want: Option<libc::c_int>| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while stop_signal_of(pid) != want {
                assert!(Instant::now() < deadline, "never became {want:?}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        assert_eq!(stop_signal_of(pid), None, "running");
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
        until(Some(libc::SIGSTOP));
        assert_eq!(
            stop_signal_of(pid),
            Some(libc::SIGSTOP),
            "a peek consumes nothing"
        );
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).unwrap();
        until(None);
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[test]
    fn only_terminating_signals_are_re_raised() {
        assert!(terminates_by_default(NixSignal::SIGINT));
        assert!(terminates_by_default(NixSignal::SIGTERM));
        assert!(!terminates_by_default(NixSignal::SIGTSTP));
        assert!(!terminates_by_default(NixSignal::SIGSTOP));
        assert!(!terminates_by_default(NixSignal::SIGCHLD));
    }

    /// The init's binary is a copy of the running one, byte for byte, that
    /// nobody can change: every seal is on, and a write is refused. (The
    /// file it was copied from has none: the check isn't vacuous.)
    /// Only the sealed copy's link counts as it: the binary's file, or any
    /// other memfd, doesn't.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_the_sealed_copys_link_counts() {
        use std::path::Path;
        assert!(runs_sealed_copy(Path::new("/memfd:shipyard-init")));
        assert!(runs_sealed_copy(Path::new(
            "/memfd:shipyard-init (deleted)"
        )));
        assert!(!runs_sealed_copy(Path::new("/usr/bin/shipyard")));
        assert!(!runs_sealed_copy(Path::new("/memfd:other")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_inits_binary_is_a_sealed_copy() {
        use std::io::{Read as _, Seek as _, Write as _};

        let (copy, host) = own_binary(|_| true, |_| unreachable!()).unwrap();
        let host = host.expect("a copy, so the file itself as well");
        assert!(super::super::setup::is_sealed_setup(copy.as_raw_fd()));
        assert!(!super::super::setup::is_sealed_setup(host.as_raw_fd()));
        let read = |fd: &OwnedFd| {
            let mut file = std::fs::File::from(fd.try_clone().unwrap());
            file.rewind().unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        };
        let bytes = read(&copy);
        assert!(!bytes.is_empty());
        assert!(bytes == read(&host), "the copy differs from the binary");
        let mut file = std::fs::File::from(copy);
        let refused = file.write_all(b"x").unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::EPERM), "{refused}");
    }
}
