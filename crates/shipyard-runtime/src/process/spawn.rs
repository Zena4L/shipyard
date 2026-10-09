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
    pub(crate) gate: Option<RawFd>,
    /// Isolated: the cgroup the container starts in (made by the caller,
    /// who removes it once the container is gone).
    #[cfg(target_os = "linux")]
    pub(crate) cgroup: Option<crate::cgroup::Cgroup>,
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

impl Placement {
    /// The new namespaces, if that's where the command starts.
    #[cfg(target_os = "linux")]
    pub(crate) fn isolation(&self) -> Option<&Isolation> {
        match self {
            Self::New(isolation) => Some(isolation),
            Self::Host | Self::Join(_) => None,
        }
    }

    /// Whether the command starts a session of its own (isolated or joined).
    /// It then has its own process group too, so `new_process_group`
    /// doesn't apply, and it never shares the terminal's group.
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
            .and_then(|isolation| isolation.id_maps.as_ref())
        {
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
    let (report_read, report_write) = report_pipe()?;
    let report = ReportReader::new(report_read)?;
    let (go_read, go_write) = go_pipe()?;
    let handshake = Handshake {
        read: go_read.as_raw_fd(),
        parent_end: go_write.as_raw_fd(),
    };
    #[cfg(target_os = "linux")]
    let network_join = match options.placement.isolation() {
        Some(isolation) => {
            check_proc_numbers_children(isolation, network.is_some())?;
            open_network_join(isolation)?
        }
        None => None,
    };
    #[cfg(target_os = "linux")]
    let isolated = options
        .placement
        .isolation()
        .map(|isolation| CleanImageFiles::open(spec, options, isolation))
        .transpose()?;
    #[cfg(target_os = "linux")]
    let reexec_fds = isolated
        .as_ref()
        .map(|files| files.fds(report_write.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let reexec_fds = None;
    #[cfg(target_os = "linux")]
    let joining = match &options.placement {
        Placement::Join(join) => Some(super::join::Joining::open(join)?),
        Placement::Host | Placement::New(_) => None,
    };
    // The container's cgroup, open, for the clone (before the plan, which
    // lists every descriptor the child must not keep).
    #[cfg(target_os = "linux")]
    let cgroup_dir = match (&options.cgroup, options.placement.isolation()) {
        (Some(cgroup), Some(_)) => Some(cgroup.open_dir()?),
        _ => None,
    };
    // Last, so its list of open descriptors includes the pipes above.
    let (plan, context) = ChildPlan::new(
        spec,
        options,
        handshake,
        reexec_fds,
        #[cfg(target_os = "linux")]
        joining.as_ref().map(super::join::Joining::setup),
    )?;

    // Block every signal across `fork`: the supervisor's handler must never
    // run in the child before the child resets it.
    let mask = BlockAllSignals::new()?;
    // SAFETY: the child branch only runs `child_main`, which is written to be
    // safe after `fork` (no allocation, locks or panics; see child.rs).
    let (pid, in_cgroup) = match unsafe {
        launch(
            options,
            #[cfg(target_os = "linux")]
            joining.as_ref(),
            #[cfg(target_os = "linux")]
            network_join.as_ref(),
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
    let group = if options.placement.has_own_session() {
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

/// The network namespace `isolation` joins, opened before anything starts.
/// One that's shipyard's own (the host's, usually) is the container's in
/// nothing: its settings and devices aren't the container's to change.
#[cfg(target_os = "linux")]
fn open_network_join(isolation: &Isolation) -> Result<Option<super::network::NetworkJoin>> {
    let Some(path) = isolation.network_path.as_deref() else {
        return Ok(None);
    };
    let join = super::network::NetworkJoin::open(path)?;
    let changes_it = !isolation.net_devices.is_empty()
        || isolation
            .sysctls
            .iter()
            .any(|(key, _)| key.starts_with("net."));
    if changes_it && join.is_shipyards_own() {
        return Err(Error::Network(format!(
            "the network namespace {} is shipyard's own (the host's): network settings and \
             devices can't be given to a container that joins it",
            join.path().display()
        )));
    }
    Ok(Some(join))
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
    /// The shipyard binary.
    exe: OwnedFd,
    /// The setup, in a sealed memfd (see `setup.rs`).
    setup: OwnedFd,
    /// The start gate, if any (`SpawnOptions::gate`).
    gate: Option<RawFd>,
    /// The hooks pipe (read, write), if the setup stops for hooks.
    hooks: Option<(OwnedFd, OwnedFd)>,
    /// An image's overlay made here (see [`prebuilt_overlay`]).
    overlay: Option<OwnedFd>,
}

#[cfg(target_os = "linux")]
impl CleanImageFiles {
    fn open(spec: &ProcessSpec, options: &SpawnOptions, isolation: &Isolation) -> Result<Self> {
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
        Ok(Self {
            status_read,
            status_write,
            exe: open_own_binary()?,
            setup: super::setup::Setup::new(
                spec,
                &options.keep_ignored,
                isolation,
                options.gate,
                options.cgroup.is_some(),
                hooks.as_ref().map(|(read, _)| read.as_raw_fd()),
                overlay.as_ref().map(AsRawFd::as_raw_fd),
            )
            .to_memfd()?,
            gate: options.gate,
            hooks,
            overlay,
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
            gate: self.gate,
            hooks: self.hooks.as_ref().map(|(read, _)| read.as_raw_fd()),
            overlay: self.overlay.as_ref().map(AsRawFd::as_raw_fd),
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
    let Some(root) = &isolation.root else {
        return Ok(None);
    };
    let Some(overlay) = &root.overlay else {
        return Ok(None);
    };
    if isolation.id_maps.is_none() || !nix::unistd::geteuid().is_root() {
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

/// The running shipyard binary, for the cloned child to re-exec (see
/// `init.rs`). Opened by descriptor, before the clone: the file on disk may
/// be replaced by an upgrade meanwhile.
#[cfg(target_os = "linux")]
fn open_own_binary() -> Result<OwnedFd> {
    nix::fcntl::open(
        "/proc/self/exe",
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|e| Error::sys("opening shipyard's own binary for the container init", e))
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
/// cgroup whose directory is `cgroup`, where `clone3` can), with `fork`
/// otherwise.
///
/// # Safety
///
/// As for `fork`: the child may only run async-signal-safe code.
unsafe fn launch(
    options: &SpawnOptions,
    #[cfg(target_os = "linux")] joining: Option<&super::join::Joining>,
    #[cfg(target_os = "linux")] network_join: Option<&super::network::NetworkJoin>,
    #[cfg(target_os = "linux")] cgroup: Option<RawFd>,
) -> Result<Launched> {
    #[cfg(target_os = "linux")]
    if let Some(joining) = joining {
        // SAFETY: the caller's contract.
        return match unsafe { joining.fork() }? {
            ForkResult::Child => Ok(Launched::Child),
            ForkResult::Parent { child } => Ok(Launched::Parent {
                pid: child,
                in_cgroup: false,
            }),
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(isolation) = options.placement.isolation() {
        use super::clone::{Cloned, clone_process};
        // A network namespace to join: entered for the clone, so the child
        // starts in it (`clone_flags` leaves `CLONE_NEWNET` out), and left
        // right after.
        if let Some(join) = network_join {
            join.enter()?;
        }
        // SAFETY: the caller's contract; the flags are namespace flags only
        // (`Isolation::validate` refuses CLONE_NEWTIME).
        let cloned = unsafe { clone_process(isolation.clone_flags(), cgroup) };
        if let Some(join) = network_join {
            match &cloned {
                // The child stays: it's to start in that namespace.
                Ok(Cloned::Child) => {}
                Ok(Cloned::Parent { pid, .. }) => join.leave(Some(*pid)),
                Err(_) => join.leave(None),
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
            Err(Errno::EPERM) if isolation.has(crate::isolation::Namespace::User) => {
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
            Report::Running => Ok(()),
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
}
