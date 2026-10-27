//! What can go wrong between `fork` and `exec`, and how the parent explains it.
//!
//! The child can only report a step number, an errno and one number of detail
//! (it can't allocate after fork). The parent turns that into a message, using
//! what it knows about the spec and, for exec failures, a look at the files.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use nix::errno::Errno;

use super::identity::{Which, describe as describe_lookup};
use super::rlimit::{Rlimit, RlimitKind};
use super::rootfs::{DEV_ENTRIES, Target};
use super::spec::Names;
use super::status::{EXIT_CANNOT_EXECUTE, EXIT_ENGINE_ERROR, EXIT_NOT_FOUND};
use crate::hooks::{HookError, Phase};
use crate::isolation::{MountKind, Root};
use crate::security::Capability;
#[cfg(target_os = "linux")]
use crate::security::{Seccomp, Security};

/// The kernel interfaces nothing may be mounted on (see the rootfs guard).
const KERNEL_INTERFACES: &str = "/proc, /sys, the cgroup tree, /dev/pts or /dev/mqueue";

/// The child's setup steps. The numbers travel over the report pipe, so
/// they never change; new steps get new numbers, wherever they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum ChildStep {
    ResetSignals = 1,
    CloseFds = 2,
    Rlimit = 3,
    Chdir = 4,
    SetGroups = 5,
    SetGid = 6,
    SetUid = 7,
    VerifyCredentials = 8,
    ParentDeathSignal = 9,
    Exec = 10,
    ProcessGroup = 11,
    SignalMask = 12,
    Handshake = 13,
    Session = 14,
    MountPrivate = 15,
    MountProc = 16,
    Hostname = 17,
    InitFork = 18,
    JoinNamespace = 19,
    Reexec = 20,
    ReadSetup = 21,
    InitPanicked = 22,
    BindRoot = 23,
    RootTarget = 24,
    MountSys = 25,
    MountDev = 26,
    PivotRoot = 27,
    DetachOldRoot = 28,
    MaskPath = 29,
    ReadonlyPath = 30,
    ReadOnlyRoot = 31,
    MountTarget = 32,
    Mount = 33,
    MountRestrict = 34,
    UserLookup = 35,
    Domainname = 36,
    Sysctl = 37,
    KeepCaps = 38,
    BoundingDrop = 39,
    Capset = 40,
    AmbientRaise = 41,
    NoNewPrivs = 42,
    CapLastCap = 43,
    CapabilityUnknown = 44,
    TooManyCapabilities = 45,
    Seccomp = 46,
    SeccompCompile = 47,
    NamespaceRoot = 48,
    UnmappedId = 49,
    GroupsDenied = 50,
    SessionKeyring = 51,
    CgroupNamespace = 52,
    MountCgroup = 53,
    /// Not a failure: the clean image stopped before `pivot_root` for the
    /// runtime's hooks (`prestart`, `createRuntime`), and waits.
    AtHooks = 54,
    CreateContainerHook = 55,
    StartContainerHook = 56,
    DeviceNode = 57,
    /// The runner of the `startContainer` hooks (a child of the init)
    /// couldn't drop the init's capabilities before running them.
    HookRunnerCapabilities = 58,
    /// Mounting the image overlay as the root filesystem failed.
    MountOverlay = 59,
    /// Attaching the image overlay real root made (`--uidmap`) failed.
    AttachOverlay = 60,
    /// Bringing up the loopback interface of the container's network
    /// namespace failed.
    Loopback = 61,
    /// Putting the command's standard input, output or error in place
    /// failed (`dup2`). The detail is the descriptor: 0, 1 or 2.
    Stdio = 62,
    /// Mounting a devpts instance of the container's own over the host's
    /// `/dev/pts` (no root filesystem of its own) failed.
    OwnTerminals = 63,
    /// Making the container's pseudo-terminal failed. The detail: 0 opening
    /// its `ptmx`, 1 unlocking, 2 sizing, 3 opening the slave, 4 giving the
    /// slave to the command's user.
    TerminalOpen = 64,
    /// Passing the pseudo-terminal's master to Shipyard failed (detail 0),
    /// or Shipyard went away before saying it was relayed (detail 1).
    TerminalSend = 65,
    /// The command couldn't start a session of its own, to take its
    /// terminal.
    TerminalSession = 66,
    /// The command couldn't make the terminal its controlling terminal.
    ControllingTerminal = 67,
    /// Putting the terminal on the command's 0, 1 or 2 (the detail) failed.
    TerminalStdio = 68,
    /// Making `exec`'s process non-dumpable failed (see `exec.rs`).
    Undumpable = 69,
    /// Closing every descriptor `exec`'s process inherited but doesn't need
    /// failed.
    CloseHostFds = 70,
    /// `exec`'s joiner couldn't set the command's OOM score adjustment.
    OomScoreAdj = 71,
    /// `exec`'s joiner couldn't start the command in the container (its
    /// clone into the container's PID namespace and cgroup).
    ExecClone = 72,
    /// `exec`'s joiner couldn't tell shipyard the command's PID.
    ExecRecord = 73,
    /// The descriptors passed on (`--preserve-fds`, socket activation)
    /// couldn't be put in place, from 3.
    PassFds = 74,
    /// The started FIFO (for `poststart` hooks: see `Gate::started`)
    /// couldn't be opened.
    StartedFifo = 75,
}

impl ChildStep {
    /// The step a number names. An explicit table rather than a list to
    /// search: a step left out of it would turn the child's real failure
    /// into "unknown step".
    pub(crate) fn from_raw(raw: u16) -> Option<Self> {
        Some(match raw {
            1 => Self::ResetSignals,
            2 => Self::CloseFds,
            3 => Self::Rlimit,
            4 => Self::Chdir,
            5 => Self::SetGroups,
            6 => Self::SetGid,
            7 => Self::SetUid,
            8 => Self::VerifyCredentials,
            9 => Self::ParentDeathSignal,
            10 => Self::Exec,
            11 => Self::ProcessGroup,
            12 => Self::SignalMask,
            13 => Self::Handshake,
            14 => Self::Session,
            15 => Self::MountPrivate,
            16 => Self::MountProc,
            17 => Self::Hostname,
            18 => Self::InitFork,
            19 => Self::JoinNamespace,
            20 => Self::Reexec,
            21 => Self::ReadSetup,
            22 => Self::InitPanicked,
            23 => Self::BindRoot,
            24 => Self::RootTarget,
            25 => Self::MountSys,
            26 => Self::MountDev,
            27 => Self::PivotRoot,
            28 => Self::DetachOldRoot,
            29 => Self::MaskPath,
            30 => Self::ReadonlyPath,
            31 => Self::ReadOnlyRoot,
            32 => Self::MountTarget,
            33 => Self::Mount,
            34 => Self::MountRestrict,
            35 => Self::UserLookup,
            36 => Self::Domainname,
            37 => Self::Sysctl,
            38 => Self::KeepCaps,
            39 => Self::BoundingDrop,
            40 => Self::Capset,
            41 => Self::AmbientRaise,
            42 => Self::NoNewPrivs,
            43 => Self::CapLastCap,
            44 => Self::CapabilityUnknown,
            45 => Self::TooManyCapabilities,
            46 => Self::Seccomp,
            47 => Self::SeccompCompile,
            48 => Self::NamespaceRoot,
            49 => Self::UnmappedId,
            50 => Self::GroupsDenied,
            51 => Self::SessionKeyring,
            52 => Self::CgroupNamespace,
            53 => Self::MountCgroup,
            54 => Self::AtHooks,
            55 => Self::CreateContainerHook,
            56 => Self::StartContainerHook,
            57 => Self::DeviceNode,
            58 => Self::HookRunnerCapabilities,
            59 => Self::MountOverlay,
            60 => Self::AttachOverlay,
            61 => Self::Loopback,
            62 => Self::Stdio,
            63 => Self::OwnTerminals,
            64 => Self::TerminalOpen,
            65 => Self::TerminalSend,
            66 => Self::TerminalSession,
            67 => Self::ControllingTerminal,
            68 => Self::TerminalStdio,
            69 => Self::Undumpable,
            70 => Self::CloseHostFds,
            71 => Self::OomScoreAdj,
            72 => Self::ExecClone,
            73 => Self::ExecRecord,
            74 => Self::PassFds,
            75 => Self::StartedFifo,
            _ => return None,
        })
    }
}

/// A failure reported by the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChildFailure {
    pub(crate) step: ChildStep,
    pub(crate) errno: Errno,
    /// Step-specific: which rlimit, or which exec candidate.
    pub(crate) detail: u64,
}

/// What the parent knows about the spawn, for explaining a failure.
#[derive(Debug, Clone)]
pub(crate) struct FailureContext {
    pub(crate) program: PathBuf,
    pub(crate) candidates: Vec<PathBuf>,
    pub(crate) cwd: PathBuf,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) rlimits: Vec<Rlimit>,
    /// Whether shipyard runs as root (as root, an EPERM isn't about privileges).
    pub(crate) root: bool,
    /// The user and group names, if given: how failures name the user.
    pub(crate) names: Names,
    /// The container's sysctls, which failures name by index.
    pub(crate) sysctls: Vec<(String, String)>,
    /// The container's own root filesystem, if it has one: where the
    /// command's paths are, and which paths the setup's details name.
    pub(crate) container_root: Option<Root>,
    /// When the command's seccomp filter goes on, if it has one: a failure
    /// after that may be the filter refusing a call.
    pub(crate) seccomp: Option<FilterStart>,
    /// In a user namespace: the host user the container's root is. The
    /// host's permissions apply to it, not to host root.
    pub(crate) namespace_root: Option<u32>,
    /// The container's process limit (`pids.max`), if it has one: what an
    /// EAGAIN from a fork inside it most likely hit.
    pub(crate) pids_max: Option<u64>,
    /// The container's hooks, which a hook's failure names by place.
    pub(crate) hooks: crate::hooks::Hooks,
    /// The device nodes made in it, which a failure names by place.
    pub(crate) devices: Vec<crate::devices::DeviceNode>,
}

/// When a command's seccomp filter goes on (see `child.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "seccomp is Linux's: elsewhere there's never a filter"
    )
)]
pub(crate) enum FilterStart {
    /// Before the setup: without `no_new_privs`, while root still has
    /// `CAP_SYS_ADMIN`.
    BeforeSetup,
    /// Right before `exec`.
    BeforeExec,
}

impl FilterStart {
    /// When `security`'s filter goes on, if it has one.
    #[cfg(target_os = "linux")]
    pub(crate) fn of(security: &Security) -> Option<Self> {
        match security.seccomp {
            Seccomp::Unconfined => None,
            _ if security.no_new_privileges => Some(Self::BeforeExec),
            _ => Some(Self::BeforeSetup),
        }
    }

    /// When `confinement`'s filter goes on, if it has one.
    #[cfg(target_os = "linux")]
    pub(crate) fn of_confinement(confinement: &crate::security::Confinement) -> Option<Self> {
        if confinement.filter_before_setup().is_some() {
            Some(Self::BeforeSetup)
        } else {
            confinement.filter_before_exec().map(|_| Self::BeforeExec)
        }
    }

    /// Whether `step` runs under the filter.
    fn covers(self, step: ChildStep) -> bool {
        match self {
            Self::BeforeExec => step == ChildStep::Exec,
            Self::BeforeSetup => matches!(
                step,
                ChildStep::KeepCaps
                    | ChildStep::BoundingDrop
                    | ChildStep::SetGroups
                    | ChildStep::SetGid
                    | ChildStep::SetUid
                    | ChildStep::VerifyCredentials
                    | ChildStep::Capset
                    | ChildStep::AmbientRaise
                    | ChildStep::NoNewPrivs
                    | ChildStep::Chdir
                    | ChildStep::ParentDeathSignal
                    | ChildStep::SignalMask
                    | ChildStep::Exec
            ),
        }
    }
}

/// Errors from `execve` that mean "no program at this path", so a PATH
/// search moves on to the next directory. Shared with the child (it's a
/// plain comparison, safe after `fork`).
pub(crate) const fn means_not_found(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP | Errno::ENAMETOOLONG
    )
}

impl FailureContext {
    /// Where the child found `path`, as shipyard sees it: relative paths are
    /// relative to the command's working directory (always absolute, see
    /// `ProcessSpec`), and with a root filesystem of its own, inside that.
    /// (For hints only: an absolute symlink inside the root filesystem would
    /// be followed on the host here.)
    ///
    /// An image's root is an overlay mounted only inside the container
    /// (its directory on the host stays empty): the path is looked up
    /// through its layers, roughly as overlay does, the writable one first,
    /// then the image's, top first; a whiteout (a 0:0 character device) of
    /// the path itself hides it. (Not opaque directories, nor whiteouts of a
    /// directory above it, nor an absolute symlink on the way, which is
    /// followed on the host: for hints only.)
    fn resolve(&self, path: &Path) -> PathBuf {
        let inside = self.cwd.join(path);
        let relative = inside.strip_prefix("/").unwrap_or(&inside);
        let Some(root) = &self.container_root else {
            return inside;
        };
        let Some(overlay) = &root.overlay else {
            return root.path.join(relative);
        };
        let hidden = root.path.join(relative);
        for layer in std::iter::once(&overlay.upper).chain(overlay.lower.iter().rev()) {
            let candidate = layer.join(relative);
            match std::fs::symlink_metadata(&candidate) {
                Ok(meta) if is_whiteout(&meta) => return hidden,
                Ok(_) => return candidate,
                Err(_) => {}
            }
        }
        hidden
    }

    /// The user, as given: by name if there is one, else by number.
    fn user_label(&self) -> String {
        self.names
            .user
            .clone()
            .unwrap_or_else(|| self.uid.to_string())
    }

    /// The primary group, as given: by name if there is one, else by number.
    fn group_label(&self) -> String {
        self.names
            .group
            .clone()
            .unwrap_or_else(|| self.gid.to_string())
    }

    /// "the root filesystem DIR", for messages.
    fn root_filesystem(&self) -> String {
        match &self.container_root {
            Some(root) => format!("the root filesystem {}", root.path.display()),
            None => "the root filesystem".to_owned(),
        }
    }
}

/// Why a command never got to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnErrorKind {
    /// The command, or its `#!` interpreter, wasn't found (exit 127).
    NotFound,
    /// The command was found but couldn't be executed (exit 126).
    CannotExecute,
    /// Setting up the process failed before exec (exit 125).
    Setup,
    /// Setting up the container's namespaces (or joining them) failed
    /// before exec (exit 125): the steps a run without isolation skips.
    Isolation,
    /// `exec` couldn't start its command in the container's cgroup: it has
    /// sub-cgroups with controllers (made inside the container), and a
    /// cgroup like that holds no processes itself (exit 125).
    CgroupBusy,
    /// `exec` couldn't start its command in the container's cgroup: this
    /// user may not move processes there from the cgroup it runs in (a
    /// rootless `exec` from a login whose cgroup isn't delegated) (exit 125).
    CgroupDenied,
}

/// A command that was started but never ran, with a message saying why.
///
/// The message is complete on its own (it already explains the errno in
/// words), so the raw errno isn't chained as a source; [`SpawnError::errno`]
/// still gives it to programs that want it.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SpawnError {
    kind: SpawnErrorKind,
    message: String,
    errno: Errno,
}

impl SpawnError {
    fn new(kind: SpawnErrorKind, message: impl Into<String>, errno: Errno) -> Self {
        Self {
            kind,
            message: message.into(),
            errno,
        }
    }

    /// The same error, with `note` added to its message.
    fn noting(self, note: &str) -> Self {
        Self {
            message: format!("{} ({note})", self.message),
            ..self
        }
    }

    /// Why the command didn't run.
    #[must_use]
    pub fn kind(&self) -> SpawnErrorKind {
        self.kind
    }

    /// The error number the kernel returned.
    #[must_use]
    pub fn errno(&self) -> Errno {
        self.errno
    }

    /// 127 for not found, 126 for can't execute, 125 for setup failures.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self.kind {
            SpawnErrorKind::NotFound => EXIT_NOT_FOUND,
            SpawnErrorKind::CannotExecute => EXIT_CANNOT_EXECUTE,
            SpawnErrorKind::Setup
            | SpawnErrorKind::Isolation
            | SpawnErrorKind::CgroupBusy
            | SpawnErrorKind::CgroupDenied => EXIT_ENGINE_ERROR,
        }
    }
}

/// Turns the child's report into an explained error.
pub(crate) fn describe(failure: ChildFailure, ctx: &FailureContext) -> SpawnError {
    let error = describe_step(failure, ctx);
    // EPERM is what a seccomp profile usually answers (Docker's does).
    let refused_by_filter = failure.errno == Errno::EPERM
        && ctx.seccomp.is_some_and(|start| start.covers(failure.step));
    if refused_by_filter {
        return error.noting("the seccomp profile, in force by then, may have refused it");
    }
    // Host root could; the user the container's root really is can't.
    // (`exec`'s EACCES isn't always about permissions: `describe_exec`
    // notes it where it is.)
    if failure.errno == Errno::EACCES && failure.step != ChildStep::Exec {
        namespace_root_note(error, ctx)
    } else {
        error
    }
}

/// `error`, noting who the container's root is on the host, in a user
/// namespace: the host's permissions apply to that user, not to root.
fn namespace_root_note(error: SpawnError, ctx: &FailureContext) -> SpawnError {
    match ctx.namespace_root {
        Some(host) => error.noting(&format!(
            "in its user namespace, the container's root is host user {host}, and the host's \
             permissions apply to it"
        )),
        None => error,
    }
}

/// [`ChildStep::VerifyCredentials`]'s message.
const LEFTOVER_PRIVILEGES: &str = "the user and groups did not change as expected; refusing to \
                                   run with leftover privileges";

/// What the command was doing when taking its terminal failed.
fn taking_the_terminal(step: ChildStep, detail: u64) -> String {
    match step {
        ChildStep::TerminalSession => "starting a session for the command's terminal".to_owned(),
        ChildStep::ControllingTerminal => {
            "making the pseudo-terminal the command's controlling terminal".to_owned()
        }
        _ => format!(
            "putting the command's terminal on its {}",
            stdio_name(detail)
        ),
    }
}

/// Which standard descriptor a [`ChildStep::Stdio`] or
/// [`ChildStep::TerminalStdio`] failure was about.
fn stdio_name(fd: u64) -> &'static str {
    match fd {
        0 => "standard input",
        1 => "standard output",
        2 => "standard error",
        _ => "standard descriptors",
    }
}

/// What a [`ChildStep::TerminalOpen`] failure was doing (its detail).
fn opening_the_terminal(detail: u64, errno: Errno) -> &'static str {
    match detail {
        0 => "opening the container's /dev/pts/ptmx for its terminal",
        1 => "unlocking the container's terminal",
        2 => "sizing the container's terminal",
        3 => "opening the container's terminal",
        4 if errno == Errno::EINVAL => {
            "giving the container's terminal to its user (that user isn't mapped in the \
                     container's user namespace)"
        }
        4 => "giving the container's terminal to its user",
        _ => {
            "the container's /dev/pts/ptmx isn't its devpts's terminal multiplexer \
                     (replaced inside the container): refused"
        }
    }
}

/// What failed at `failure`'s step, and why.
fn describe_step(failure: ChildFailure, ctx: &FailureContext) -> SpawnError {
    use SpawnErrorKind::Setup;
    let ChildFailure {
        step,
        errno,
        detail,
    } = failure;
    // Setup messages say what was being done; the errno says why.
    let setup = |doing: String| SpawnError::new(Setup, format!("{doing}: {}", errno.desc()), errno);
    match step {
        ChildStep::Exec => describe_exec(errno, detail, ctx),
        ChildStep::ResetSignals => setup("resetting signal handlers".into()),
        ChildStep::CloseFds => setup("closing inherited file descriptors".into()),
        ChildStep::PassFds => setup("putting the passed descriptors in place, from 3".into()),
        ChildStep::StartedFifo => setup("opening the started FIFO, for the poststart hooks".into()),
        ChildStep::Stdio => setup(format!("giving the command its {}", stdio_name(detail))),
        ChildStep::TerminalSession | ChildStep::ControllingTerminal | ChildStep::TerminalStdio => {
            setup(taking_the_terminal(step, detail))
        }
        ChildStep::Rlimit => SpawnError::new(Setup, describe_rlimit(errno, detail, ctx), errno),
        ChildStep::Chdir => setup(format!("changing directory to {}", ctx.cwd.display())),
        ChildStep::SetGroups => setup("setting supplementary groups".into()),
        ChildStep::SetGid => setup(format!("switching to group {}", ctx.group_label())),
        ChildStep::SetUid => setup(format!("switching to user {}", ctx.user_label())),
        ChildStep::VerifyCredentials => setup(LEFTOVER_PRIVILEGES.into()),
        ChildStep::ParentDeathSignal => setup("setting the parent-death signal".into()),
        ChildStep::ProcessGroup => setup("starting a new process group".into()),
        ChildStep::SignalMask => setup("clearing the signal mask".into()),
        ChildStep::Handshake => setup("waiting for shipyard to finish its setup".into()),
        ChildStep::Session => isolation_setup("starting a new session", errno, ctx),
        ChildStep::MountPrivate => {
            isolation_setup("making the container's mounts private", errno, ctx)
        }
        ChildStep::MountProc => isolation_setup("mounting /proc for the container", errno, ctx),
        ChildStep::Hostname => isolation_setup("setting the container's hostname", errno, ctx),
        ChildStep::SessionKeyring => SpawnError::new(Setup, describe_keyring(errno), errno),
        ChildStep::CgroupNamespace => {
            isolation_setup("giving the container a cgroup namespace", errno, ctx)
        }
        ChildStep::InitFork => init_fork(errno, ctx),
        ChildStep::JoinNamespace => join_setup(errno, detail, ctx),
        ChildStep::Undumpable
        | ChildStep::CloseHostFds
        | ChildStep::OomScoreAdj
        | ChildStep::ExecClone
        | ChildStep::ExecRecord => exec_step(step, errno, ctx),
        ChildStep::Reexec => describe_reexec(errno, ctx),
        ChildStep::ReadSetup => {
            isolation_setup("the container's init reading its setup", errno, ctx)
        }
        ChildStep::InitPanicked => isolation_setup(
            "the container's init failed unexpectedly (a bug in shipyard; its message is above)",
            errno,
            ctx,
        ),
        ChildStep::KeepCaps
        | ChildStep::BoundingDrop
        | ChildStep::Capset
        | ChildStep::AmbientRaise
        | ChildStep::NoNewPrivs
        | ChildStep::TooManyCapabilities
        | ChildStep::CapLastCap
        | ChildStep::CapabilityUnknown
        | ChildStep::Seccomp
        | ChildStep::SeccompCompile
        | ChildStep::NamespaceRoot
        | ChildStep::UnmappedId
        | ChildStep::GroupsDenied => describe_confinement(step, errno, detail, ctx),
        ChildStep::BindRoot
        | ChildStep::MountOverlay
        | ChildStep::AttachOverlay
        | ChildStep::RootTarget
        | ChildStep::MountSys
        | ChildStep::MountCgroup
        | ChildStep::MountDev
        | ChildStep::PivotRoot
        | ChildStep::DetachOldRoot
        | ChildStep::MaskPath
        | ChildStep::ReadonlyPath
        | ChildStep::ReadOnlyRoot
        | ChildStep::MountTarget
        | ChildStep::Mount
        | ChildStep::MountRestrict
        | ChildStep::UserLookup
        | ChildStep::Domainname
        | ChildStep::Sysctl
        | ChildStep::Loopback
        | ChildStep::OwnTerminals
        | ChildStep::TerminalOpen
        | ChildStep::TerminalSend
        | ChildStep::AtHooks => describe_container(step, errno, detail, ctx),
        ChildStep::DeviceNode => describe_device(errno, detail, ctx),
        ChildStep::CreateContainerHook
        | ChildStep::StartContainerHook
        | ChildStep::HookRunnerCapabilities => describe_hook(failure, ctx),
    }
}

/// A device node that couldn't be made (`detail`: which).
/// The container's init couldn't re-exec shipyard's binary: refused (no
/// sealed copy runs here, and the file may not stand in), or another
/// failure.
fn describe_reexec(errno: Errno, ctx: &FailureContext) -> SpawnError {
    let doing = if matches!(errno, Errno::EACCES | Errno::EPERM) {
        "starting the container's init: this host refused to run shipyard's binary for it (a \
         sealed copy: vm.memfd_noexec, or a security module's policy on memfds; the binary's \
         file stands in only where nobody in the container could write it)"
    } else {
        "starting the container's init (re-executing shipyard's own binary)"
    };
    isolation_setup(doing, errno, ctx)
}

fn describe_device(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
    let node = usize::try_from(detail)
        .ok()
        .and_then(|index| ctx.devices.get(index));
    let name = node.map_or_else(
        || "a device node".to_owned(),
        |node| format!("the device {node}"),
    );
    let why = match errno {
        Errno::ENODEV => {
            "the host's node at that path is another device (in a user namespace, the host's \
             node is bound into the container: it must be the same)"
                .to_owned()
        }
        Errno::ENOENT if ctx.namespace_root.is_some() => {
            "there's no such node on the host to bind (in a user namespace, the host's node is \
             bound into the container)"
                .to_owned()
        }
        Errno::EEXIST => "something else is at that path already".to_owned(),
        Errno::EXDEV => format!("the path leads into a kernel interface ({KERNEL_INTERFACES})"),
        other => other.desc().to_owned(),
    };
    SpawnError::new(
        SpawnErrorKind::Isolation,
        format!("making {name} in the container: {why}"),
        errno,
    )
}

/// A hook the container's init ran that failed, or the runner of the
/// `startContainer` hooks that couldn't drop its capabilities: the container
/// can't start.
fn describe_hook(failure: ChildFailure, ctx: &FailureContext) -> SpawnError {
    let phase = match failure.step {
        ChildStep::CreateContainerHook => Phase::CreateContainer,
        ChildStep::StartContainerHook => Phase::StartContainer,
        _ => {
            return isolation_setup(
                "dropping the init's capabilities to run the startContainer hooks",
                failure.errno,
                ctx,
            );
        }
    };
    let message = HookError::from_record(phase, ctx.hooks.of(phase), failure.errno, failure.detail)
        .map_or_else(
            || format!("a {phase} hook failed (its report makes no sense)"),
            |err| err.to_string(),
        );
    SpawnError::new(SpawnErrorKind::Setup, message, failure.errno)
}

/// The command's confinement (stage 4): its capabilities, `no_new_privs` and
/// seccomp filter. `detail` names the capability, or the filter's length.
fn describe_confinement(
    step: ChildStep,
    errno: Errno,
    detail: u64,
    ctx: &FailureContext,
) -> SpawnError {
    let message = match step {
        ChildStep::KeepCaps => doing("keeping capabilities across the switch of user", errno),
        ChildStep::BoundingDrop => doing(
            &format!(
                "dropping {} from the capability bounding set",
                capability_name(detail)
            ),
            errno,
        ),
        ChildStep::Capset => doing("setting the command's capabilities", errno),
        ChildStep::AmbientRaise => doing(
            &format!(
                "raising {} as an ambient capability",
                capability_name(detail)
            ),
            errno,
        ),
        ChildStep::NoNewPrivs => doing("setting no_new_privs", errno),
        ChildStep::CapLastCap => doing(
            "reading the kernel's last capability (/proc/sys/kernel/cap_last_cap)",
            errno,
        ),
        ChildStep::TooManyCapabilities => format!(
            "this kernel has {} capabilities: more than the 64 Shipyard can drop",
            detail.saturating_add(1)
        ),
        ChildStep::CapabilityUnknown => format!(
            "{} isn't a capability this kernel has (it's older than the name)",
            capability_name(detail)
        ),
        ChildStep::SeccompCompile if detail > 0 => format!(
            "the seccomp filter is {detail} instructions long, more than the kernel's 4096; a \
             smaller profile is needed"
        ),
        ChildStep::SeccompCompile => "the seccomp profile can't be compiled".to_owned(),
        // EINVAL is the kernel refusing the filter itself: under x86-64
        // emulation (Rosetta) every filter is refused (step 1's probe, S5);
        // kernels before 4.14 refuse `SCMP_ACT_LOG` and `SCMP_ACT_KILL_PROCESS`.
        ChildStep::Seccomp if errno == Errno::EINVAL => format!(
            "{} (the kernel refused it: under x86-64 emulation, such as Rosetta, no filter can \
             be installed; `--security-opt seccomp=unconfined` runs without one, as does a \
             bundle without linux.seccomp)",
            doing("installing the seccomp filter", errno)
        ),
        ChildStep::Seccomp => doing("installing the seccomp filter", errno),
        ChildStep::NamespaceRoot => {
            with_userns_restriction(doing(NamespaceRootStep::doing(detail), errno), errno, ctx)
        }
        ChildStep::UnmappedId => describe_unmapped(detail),
        ChildStep::GroupsDenied => format!(
            "supplementary group {detail} can't be set: the container's user namespace denies \
             setgroups (a rootless container that maps only your own IDs, which ranges in \
             /etc/subuid and /etc/subgid with newuidmap installed avoid; or shipyard started \
             inside a user namespace that denies it)"
        ),
        // The other steps are `describe`'s.
        _ => doing("confining the command", errno),
    };
    SpawnError::new(SpawnErrorKind::Setup, message, errno)
}

/// Which of the command's IDs a [`ChildStep::UnmappedId`] failure is
/// about: the high 32 bits of its detail (the ID is the low 32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum UnmappedId {
    User = 1,
    Group = 2,
    SupplementaryGroup = 3,
}

impl UnmappedId {
    /// The failure's detail for `id`.
    #[cfg(target_os = "linux")]
    pub(crate) fn detail(self, id: u32) -> usize {
        usize::try_from((u64::from(self as u8) << 32) | u64::from(id)).unwrap_or(usize::MAX)
    }

    /// Which ID, and the ID, from a failure's detail.
    fn from_detail(detail: u64) -> (Self, u64) {
        let which = match detail >> 32 {
            1 => Self::User,
            2 => Self::Group,
            _ => Self::SupplementaryGroup,
        };
        (which, detail & u64::from(u32::MAX))
    }
}

/// Which call of [`ChildStep::NamespaceRoot`] failed (`setgroups`,
/// `setresgid`, `setresuid`): its detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux's child makes them")
)]
pub(crate) enum NamespaceRootStep {
    Groups = 1,
    Gid = 2,
    Uid = 3,
}

impl NamespaceRootStep {
    /// The failure's detail.
    #[cfg(target_os = "linux")]
    pub(crate) fn detail(self) -> usize {
        usize::from(self as u8)
    }

    /// What it was doing, from a failure's detail.
    fn doing(detail: u64) -> &'static str {
        if detail == Self::Groups as u64 {
            "dropping supplementary groups in the container's user namespace"
        } else if detail == Self::Gid as u64 {
            "becoming group 0 in the container's user namespace"
        } else {
            "becoming root in the container's user namespace"
        }
    }
}

/// `message`, with the host setting that confines unprivileged user
/// namespaces, when a rootless container's namespace refused its root a
/// step (`EPERM`, or AppArmor's `EACCES`): AppArmor's restriction can let
/// one be made, then take its capabilities away, so this is where it shows.
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_variables, reason = "user namespaces are Linux's")
)]
fn with_userns_restriction(message: String, errno: Errno, ctx: &FailureContext) -> String {
    #[cfg(target_os = "linux")]
    if !ctx.root
        && matches!(errno, Errno::EPERM | Errno::EACCES)
        && let Some(restriction) = super::user_namespace::userns_restriction()
    {
        return format!("{message}: {restriction}");
    }
    message
}

/// An ID the container's user namespace doesn't map.
fn describe_unmapped(detail: u64) -> String {
    let (which, id) = UnmappedId::from_detail(detail);
    let (what, map) = match which {
        UnmappedId::User => ("user", "uid"),
        UnmappedId::Group => ("group", "gid"),
        UnmappedId::SupplementaryGroup => ("supplementary group", "gid"),
    };
    format!(
        "{what} {id} isn't mapped in the container's user namespace: there it doesn't exist \
         (its {map} map comes from --{map}map, or for a rootless container from your \
         /etc/sub{map} range)"
    )
}

/// A container's session keyring that couldn't be made.
fn describe_keyring(errno: Errno) -> String {
    let doing = doing("giving the container a session keyring of its own", errno);
    if errno == Errno::EDQUOT {
        format!(
            "{doing} (this user has as many keys as the kernel allows: see the sysctls \
             kernel.keys.maxkeys and maxbytes, or root_maxkeys and root_maxbytes for root)"
        )
    } else {
        doing
    }
}

/// What was being done, and the errno's reason.
fn doing(what: &str, errno: Errno) -> String {
    format!("{what}: {}", errno.desc())
}

/// A capability by the number a failure's detail carries.
fn capability_name(detail: u64) -> String {
    u8::try_from(detail)
        .ok()
        .and_then(Capability::from_number)
        .map_or_else(
            || format!("capability {detail}"),
            |cap| cap.name().to_owned(),
        )
}

/// The container's own setup steps, from step 3 on: its root filesystem,
/// mounts, names, domain name and sysctls.
fn describe_container(
    step: ChildStep,
    errno: Errno,
    detail: u64,
    ctx: &FailureContext,
) -> SpawnError {
    match step {
        ChildStep::MountOverlay => describe_overlay(errno, detail, ctx, &crate::kernel::release()),
        ChildStep::AttachOverlay => isolation_setup(
            "attaching the image's overlay in the container (made for its user namespace)",
            errno,
            ctx,
        ),
        ChildStep::BindRoot => isolation_setup(
            &format!("binding {} onto itself", ctx.root_filesystem()),
            errno,
            ctx,
        ),
        ChildStep::RootTarget => describe_root_target(errno, detail),
        ChildStep::MountSys => isolation_setup("mounting /sys for the container", errno, ctx),
        ChildStep::MountCgroup => isolation_setup(
            "mounting the container's cgroup at /sys/fs/cgroup",
            errno,
            ctx,
        ),
        ChildStep::MountDev => {
            let entry = usize::try_from(detail)
                .ok()
                .and_then(|index| DEV_ENTRIES.get(index).copied())
                .unwrap_or("/dev");
            isolation_setup(&format!("setting up {entry} in the container"), errno, ctx)
        }
        ChildStep::PivotRoot => isolation_setup(
            &format!("switching to {} (pivot_root)", ctx.root_filesystem()),
            errno,
            ctx,
        ),
        ChildStep::DetachOldRoot => isolation_setup(
            "detaching the host's file system from the container",
            errno,
            ctx,
        ),
        ChildStep::MaskPath => {
            let path = container_path(ctx, detail, |root| &root.masked_paths);
            isolation_setup(&format!("masking {path} in the container"), errno, ctx)
        }
        ChildStep::ReadonlyPath => {
            let path = container_path(ctx, detail, |root| &root.readonly_paths);
            isolation_setup(
                &format!("making {path} read-only in the container"),
                errno,
                ctx,
            )
        }
        ChildStep::ReadOnlyRoot => isolation_setup(
            "making the container's root filesystem read-only",
            errno,
            ctx,
        ),
        ChildStep::MountTarget | ChildStep::Mount | ChildStep::MountRestrict => {
            describe_mount(step, errno, detail, ctx)
        }
        ChildStep::UserLookup => {
            let which = Which::from_detail(detail);
            let name = which.name(&ctx.names).unwrap_or("?");
            let message = describe_lookup(which, name, errno, "the container's /etc");
            SpawnError::new(SpawnErrorKind::Setup, message, errno)
        }
        ChildStep::Domainname => isolation_setup("setting the container's domain name", errno, ctx),
        ChildStep::Sysctl => describe_sysctl(errno, detail, ctx),
        // Never a failure (see `ReportReader`); said plainly if it ever
        // shows up as one.
        ChildStep::AtHooks => isolation_setup(
            "the container's init stopped for hooks at the wrong time",
            errno,
            ctx,
        ),
        ChildStep::OwnTerminals => {
            isolation_setup("mounting the container's own /dev/pts", errno, ctx)
        }
        ChildStep::TerminalOpen => isolation_setup(opening_the_terminal(detail, errno), errno, ctx),
        ChildStep::TerminalSend => isolation_setup(
            if detail == 0 {
                "passing the container's terminal on (to shipyard, or to the console socket)"
            } else {
                "waiting for shipyard to relay the container's terminal"
            },
            errno,
            ctx,
        ),
        ChildStep::Loopback => isolation_setup(
            "bringing up the loopback interface of the container's network namespace",
            errno,
            ctx,
        ),
        // The other steps are `describe`'s.
        _ => isolation_setup("setting up the container", errno, ctx),
    }
}

/// A sysctl (`detail`: its index) couldn't be set.
fn describe_sysctl(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
    let found = usize::try_from(detail)
        .ok()
        .and_then(|index| ctx.sysctls.get(index));
    let sysctl = found.map_or_else(
        || "a sysctl".to_owned(),
        |(key, value)| format!("{key}={value}"),
    );
    // A network namespace has only the settings that are its own: the
    // host's alone aren't in its /proc/sys/net.
    let host_wide_net_setting =
        errno == Errno::ENOENT && found.is_some_and(|(key, _)| key.starts_with("net."));
    if host_wide_net_setting {
        return SpawnError::new(
            SpawnErrorKind::Isolation,
            format!(
                "setting sysctl {sysctl}: a network namespace has no such setting (it may be the \
                 whole machine's, which a container can't change)"
            ),
            errno,
        );
    }
    isolation_setup(&format!("setting sysctl {sysctl}"), errno, ctx)
}

/// One of the container's own mounts (`detail`: its index) failed.
fn describe_mount(step: ChildStep, errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
    let mount = ctx
        .container_root
        .as_ref()
        .zip(usize::try_from(detail).ok())
        .and_then(|(root, index)| root.mounts.get(index));
    let Some(mount) = mount else {
        return isolation_setup("setting up one of the container's mounts", errno, ctx);
    };
    let destination = mount.destination.display();
    let what = match &mount.kind {
        MountKind::Bind { source, .. } => format!("{}", source.display()),
        _ => "a tmpfs".to_owned(),
    };
    let by_descriptor = matches!(
        &mount.kind,
        MountKind::Bind {
            made_below: Some(_),
            ..
        }
    );
    let doing = match (step, errno) {
        (ChildStep::MountTarget, Errno::ENOTDIR | Errno::EISDIR) => format!(
            "making the mount point {destination} in the root filesystem \
             (a file and a directory don't match: the source and what's there)"
        ),
        (ChildStep::MountTarget, Errno::EXDEV) => {
            return SpawnError::new(
                SpawnErrorKind::Setup,
                format!(
                    "the mount point {destination} leads into a kernel interface \
                     ({KERNEL_INTERFACES}) or /dev through a symbolic link in the root \
                     filesystem, where mounts aren't allowed (one meant for /dev is written \
                     there: /dev/x)"
                ),
                errno,
            );
        }
        (ChildStep::MountTarget, Errno::ELOOP) => format!(
            "making the mount point {destination} in the root filesystem (too many symbolic links)"
        ),
        (ChildStep::MountTarget, _) => {
            format!("making the mount point {destination} in the root filesystem")
        }
        (ChildStep::Mount, Errno::ENOENT) => {
            format!("mounting {what} at {destination} (it doesn't exist)")
        }
        // Reached without following a link (a source Shipyard made, or one
        // in a Mac's share: 11.7, 13.8c): one put on its path since.
        (ChildStep::Mount, Errno::ELOOP | Errno::ENOTDIR) if by_descriptor => format!(
            "mounting {what} at {destination} (a symbolic link or a file is now on its path, \
             put there since it was checked: refused, never followed)"
        ),
        (ChildStep::Mount, _) => format!("mounting {what} at {destination}"),
        _ => format!("restricting the mount at {destination} (read-only, nosuid, …)"),
    };
    isolation_setup(&doing, errno, ctx)
}

/// One of the root filesystem's mount targets (`detail`: a
/// `rootfs::Target`) couldn't be used.
fn describe_root_target(errno: Errno, detail: u64) -> SpawnError {
    let name = Target::from_detail(detail).map_or("a mount point", Target::name);
    let message = match errno {
        Errno::ELOOP => format!(
            "the root filesystem's /{name} is a symbolic link: refusing to mount on it \
             (it could point out of the root filesystem)"
        ),
        Errno::ENOTDIR => format!("the root filesystem's /{name} isn't a directory"),
        Errno::EROFS => format!(
            "the root filesystem has no /{name} directory, and it's on a read-only file \
             system: create /{name} in it"
        ),
        _ => format!("preparing /{name} in the root filesystem: {}", errno.desc()),
    };
    SpawnError::new(SpawnErrorKind::Isolation, message, errno)
}

/// The masked or read-only path a failure's detail names.
fn container_path(ctx: &FailureContext, detail: u64, list: impl Fn(&Root) -> &[PathBuf]) -> String {
    ctx.container_root
        .as_ref()
        .zip(usize::try_from(detail).ok())
        .and_then(|(root, index)| list(root).get(index))
        .map_or_else(|| "a path".to_owned(), |path| path.display().to_string())
}

/// The init couldn't start the command (its fork, inside the container).
fn init_fork(errno: Errno, ctx: &FailureContext) -> SpawnError {
    let doing = match (errno, ctx.pids_max) {
        (Errno::EAGAIN, Some(limit)) => format!(
            "starting the command inside the container (its process limit is {limit}, its \
             init included)"
        ),
        _ => "starting the command inside the container".to_owned(),
    };
    isolation_setup(&doing, errno, ctx)
}

/// The steps only `exec`'s joiner and command take (see `exec.rs`).
fn exec_step(step: ChildStep, errno: Errno, ctx: &FailureContext) -> SpawnError {
    let doing = match step {
        ChildStep::ExecClone => return exec_clone(errno, ctx),
        ChildStep::Undumpable => "hiding the command from the container until it runs",
        ChildStep::CloseHostFds => "closing the descriptors the command mustn't keep",
        ChildStep::OomScoreAdj if errno == Errno::EACCES => {
            "setting the command's OOM score adjustment (lowering it needs root: \
             CAP_SYS_RESOURCE)"
        }
        ChildStep::OomScoreAdj => "setting the command's OOM score adjustment",
        ChildStep::ExecRecord => "telling shipyard the command's process ID",
        _ => "starting the command in the container",
    };
    SpawnError::new(
        SpawnErrorKind::Setup,
        format!("{doing}: {}", errno.desc()),
        errno,
    )
}

/// `exec`'s joiner couldn't start the command in the container: into its
/// cgroup (`CLONE_INTO_CGROUP`), its PID namespace, or past its limits.
fn exec_clone(errno: Errno, ctx: &FailureContext) -> SpawnError {
    match errno {
        Errno::EBUSY => SpawnError::new(
            SpawnErrorKind::CgroupBusy,
            "starting the command in the container's cgroup: it has sub-cgroups with \
             controllers enabled (made inside the container), and such a cgroup takes no \
             process itself",
            errno,
        ),
        Errno::EACCES | Errno::ENOENT => SpawnError::new(
            SpawnErrorKind::CgroupDenied,
            "starting the command in the container's cgroup: this user may not move processes \
             there from the cgroup this runs in (a login whose cgroup isn't delegated to you, \
             such as an SSH session's: run it in your user session's, with \
             `systemd-run --user --scope shipyard-runtime exec …`)",
            errno,
        ),
        Errno::EAGAIN => SpawnError::new(
            SpawnErrorKind::Isolation,
            match ctx.pids_max {
                Some(limit) => format!(
                    "starting the command in the container: it has reached its process limit \
                     ({limit})"
                ),
                None => {
                    "starting the command in the container: a process limit is reached".to_owned()
                }
            },
            errno,
        ),
        Errno::ENOMEM => SpawnError::new(
            SpawnErrorKind::Isolation,
            "starting the command in the container: it is ending (its init has exited)",
            errno,
        ),
        _ => SpawnError::new(
            SpawnErrorKind::Isolation,
            format!("starting the command in the container: {}", errno.desc()),
            errno,
        ),
    }
}

/// Joining one of a running container's namespaces failed; `detail` says
/// which (its index in `Namespace::ALL`).
fn join_setup(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
    let kind = usize::try_from(detail)
        .ok()
        .and_then(|index| crate::isolation::Namespace::ALL.get(index));
    let mut message = match kind {
        Some(kind) => format!("joining the container's {kind} namespace: {}", errno.desc()),
        None => format!("joining the container's namespaces: {}", errno.desc()),
    };
    if errno == Errno::EPERM {
        message.push_str(if ctx.root {
            " (the system refused it: inside a container without privileges, or under a \
             security policy)"
        } else {
            " (not this user's container: its user namespace isn't yours)"
        });
    }
    SpawnError::new(SpawnErrorKind::Isolation, message, errno)
}

/// Why a namespace step got EPERM: missing root, or, as root, the system's
/// refusal (a container without privileges, or a security policy). How to
/// do without isolation is the caller's to say (`shipyard run` adds a hint
/// where `--no-isolation` would work).
fn permission_hint(root: bool, needing_root: &str) -> String {
    if root {
        " (the system refused it: inside a container without privileges, \
         or under a security policy)"
            .to_owned()
    } else {
        format!(" ({needing_root} needs root)")
    }
}

/// Whether `path` is on a mount that forbids running programs (probe 14:
/// for example a root filesystem under a hardened host's `/tmp`).
fn on_noexec_mount(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        nix::sys::statvfs::statvfs(path)
            .is_ok_and(|stat| stat.flags().contains(nix::sys::statvfs::FsFlags::ST_NOEXEC))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// Why an image's overlay couldn't be mounted, and what to do about it:
/// what the kernel's answer means for overlay (`release`: the kernel's, as
/// `uname -r` prints it, if known).
fn describe_overlay(errno: Errno, detail: u64, ctx: &FailureContext, release: &str) -> SpawnError {
    const DOING: &str = "mounting the image's overlay as the root filesystem";
    let Some(why) = overlay_reason(errno, detail, !ctx.root, release) else {
        return isolation_setup(DOING, errno, ctx);
    };
    SpawnError::new(
        SpawnErrorKind::Isolation,
        format!("{DOING}: {}: {why}", errno.desc()),
        errno,
    )
}

/// Why an overlay mount failed with `errno`, and what to do, as a
/// container's would be told (`shipyard doctor`'s probe tries one): the
/// errno's own description, and the reason where it's known. (Not for
/// `E2BIG`, too many layers, which a probe's one layer can't hit.)
#[must_use]
pub fn overlay_refusal(errno: Errno, rootless: bool) -> String {
    match overlay_reason(errno, 0, rootless, &crate::kernel::release()) {
        Some(why) => format!("{}: {why}", errno.desc()),
        None => errno.desc().to_owned(),
    }
}

/// The reason behind an overlay mount's `errno` (`detail`: its layer
/// count), where it's known.
fn overlay_reason(errno: Errno, detail: u64, rootless: bool, release: &str) -> Option<String> {
    Some(match errno {
        Errno::E2BIG => format!(
            "the image has {detail} layers, more than this kernel can stack in one mount \
             (Linux 6.8 or later stacks up to 500)"
        ),
        Errno::ENODEV => {
            "this kernel has no overlay filesystem (load it: modprobe overlay)".to_owned()
        }
        Errno::EINVAL => {
            let what = if rootless {
                "it may be overlayfs itself, or lack user extended attributes"
            } else {
                "it may be overlayfs itself"
            };
            format!(
                "the image store's filesystem can't hold an overlay's writable layer ({what}): \
                 set SHIPYARD_IMAGE_STORE to a directory on a disk filesystem"
            )
        }
        Errno::EPERM | Errno::EACCES if rootless => match crate::kernel::version(release) {
            Some(version) if version < (5, 11) => format!(
                "a rootless overlay needs Linux 5.11 or later, and this is {release}: run as \
                 root, or on a newer kernel"
            ),
            _ => "this system doesn't let the container's root mount overlay here (an \
                  AppArmor or other security policy, or an image store it can't reach): run \
                  as root, or lift the policy"
                .to_owned(),
        },
        _ => return None,
    })
}

/// A namespace setup step that failed. EPERM there means missing privileges:
/// root's, or (when shipyard is root) the container's or a security policy's.
fn isolation_setup(doing: &str, errno: Errno, ctx: &FailureContext) -> SpawnError {
    let mut message = format!("{doing}: {}", errno.desc());
    if errno == Errno::EPERM {
        message.push_str(&permission_hint(ctx.root, "isolation"));
    }
    SpawnError::new(SpawnErrorKind::Isolation, message, errno)
}

fn describe_rlimit(errno: Errno, detail: u64, ctx: &FailureContext) -> String {
    let limit = usize::try_from(detail)
        .ok()
        .and_then(|index| ctx.rlimits.get(index));
    let mut message = match limit {
        Some(limit) => format!("setting resource limit {limit}: {}", errno.desc()),
        None => format!("setting a resource limit: {}", errno.desc()),
    };
    if errno == Errno::EPERM {
        let nofile = limit.is_some_and(|limit| limit.kind == RlimitKind::Nofile);
        let hint = match (ctx.root, nofile) {
            (false, _) => " (raising a hard limit needs root)",
            (true, true) => " (the kernel's maximum is lower; on Linux see fs.nr_open)",
            (true, false) => "",
        };
        message.push_str(hint);
    }
    message
}

fn describe_exec(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
    use SpawnErrorKind::{CannotExecute, NotFound, Setup};
    let program = ctx.program.display();
    let candidate = usize::try_from(detail)
        .ok()
        .and_then(|index| ctx.candidates.get(index))
        .unwrap_or(&ctx.program);
    let path = candidate.display();

    match errno {
        errno if means_not_found(errno) => {
            // The file can exist and still give ENOENT: then it's the `#!`
            // interpreter or the dynamic loader that's missing.
            // Checked where the child looked: relative candidates are
            // relative to the command's working directory, not shipyard's.
            if let Some(found) = ctx.candidates.iter().find(|c| ctx.resolve(c).is_file()) {
                let message = format!(
                    "{}: found, but its interpreter or loader is missing \
                     (check the #! line)",
                    found.display()
                );
                return SpawnError::new(NotFound, message, errno);
            }
            let message = if has_slash(&ctx.program) {
                format!("{program}: no such file")
            } else {
                let mut message = format!("{program}: command not found");
                if let Some(dirs) = searched_dirs(ctx) {
                    let _ = write!(message, " (searched {dirs})");
                }
                message
            };
            SpawnError::new(NotFound, message, errno)
        }
        Errno::EACCES => {
            let resolved = ctx.resolve(candidate);
            if resolved.is_dir() {
                return SpawnError::new(CannotExecute, format!("{path}: is a directory"), errno);
            }
            if on_noexec_mount(&resolved) {
                let message = format!(
                    "{path}: permission denied (the file system it's on is mounted noexec, \
                     which forbids running programs from it)"
                );
                return SpawnError::new(CannotExecute, message, errno);
            }
            // The rest is about permissions.
            let message = if resolved.exists() {
                format!("{path}: permission denied (is it executable?)")
            } else {
                // `exists` follows the same path, so this also catches a
                // file shipyard can see but the command's user can't reach.
                format!("{path}: permission denied (a directory on the way isn't searchable)")
            };
            namespace_root_note(SpawnError::new(CannotExecute, message, errno), ctx)
        }
        Errno::ENOEXEC => SpawnError::new(
            CannotExecute,
            format!(
                "{path}: not a program this system can run \
                 (built for another CPU architecture, or a script without a #! line)"
            ),
            errno,
        ),
        Errno::ETXTBSY => SpawnError::new(
            CannotExecute,
            format!("{path}: file is busy (it is being written)"),
            errno,
        ),
        Errno::EAGAIN => SpawnError::new(
            CannotExecute,
            format!(
                "{path}: too many processes for user {} (RLIMIT_NPROC)",
                ctx.uid
            ),
            errno,
        ),
        Errno::E2BIG => SpawnError::new(
            Setup,
            "the argument list and environment are too large",
            errno,
        ),
        _ => SpawnError::new(
            CannotExecute,
            format!("{path}: cannot execute: {}", errno.desc()),
            errno,
        ),
    }
}

fn has_slash(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().contains(&b'/')
}

/// Whether `meta` is an overlay whiteout: a character device numbered 0:0.
fn is_whiteout(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// The directories a PATH search went through, for "command not found".
fn searched_dirs(ctx: &FailureContext) -> Option<String> {
    let dirs: Vec<String> = ctx
        .candidates
        .iter()
        .filter_map(|c| c.parent())
        .map(|dir| dir.display().to_string())
        .collect();
    (!dirs.is_empty()).then(|| dirs.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overlay_refusal_says_why() {
        let mut rootless = ctx("sh", &[]);
        rootless.root = false;
        let message = |errno, release: &str, ctx: &FailureContext| {
            describe_overlay(errno, 61, ctx, release).to_string()
        };
        assert!(message(Errno::E2BIG, "6.1.0", &rootless).contains("61 layers"));
        assert!(message(Errno::ENODEV, "6.1.0", &rootless).contains("modprobe overlay"));
        assert!(message(Errno::EINVAL, "6.1.0", &rootless).contains("SHIPYARD_IMAGE_STORE"));
        assert!(message(Errno::EPERM, "5.10.0-26-amd64", &rootless).contains("5.11 or later"));
        assert!(message(Errno::EPERM, "6.8.0", &rootless).contains("AppArmor"));
        let mut root = rootless.clone();
        root.root = true;
        assert!(!message(Errno::EPERM, "5.10.0", &root).contains("5.11"));
    }

    #[test]
    fn an_image_programs_hints_look_through_its_layers() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("shipyard-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let make = |path: &str, mode: u32| {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/nowhere\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        make("base/bin/tool", 0o755);
        make("top/bin/other", 0o755);
        std::fs::create_dir_all(dir.join("upper")).unwrap();
        let mut root = Root::new(dir.join("merged"));
        root.overlay = Some(crate::isolation::Overlay::new(
            vec![dir.join("base"), dir.join("top")],
            dir.join("upper"),
            dir.join("work"),
        ));
        let mut context = ctx("/bin/tool", &["/bin/tool"]);
        context.cwd = "/".into();
        context.container_root = Some(root);
        assert_eq!(
            context.resolve(Path::new("/bin/tool")),
            dir.join("base/bin/tool")
        );
        assert_eq!(
            context.resolve(Path::new("/bin/other")),
            dir.join("top/bin/other")
        );
        let missing = context.resolve(Path::new("/bin/none"));
        assert!(!missing.exists());
        let message = describe_exec(Errno::ENOENT, 0, &context).to_string();
        assert!(
            message.contains("interpreter or loader is missing"),
            "{message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two rules `resolve` adds: the writable layer before the image's,
    /// and a whiteout hiding what's below it. (A whiteout is a 0:0
    /// character device: making one needs root.)
    #[test]
    #[ignore = "needs root"]
    fn an_upper_file_shadows_and_a_whiteout_hides() {
        let dir = std::env::temp_dir().join(format!("shipyard-whiteout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for path in ["base/bin/tool", "base/bin/gone", "upper/bin/tool"] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x").unwrap();
        }
        nix::sys::stat::mknod(
            &dir.join("upper/bin/gone"),
            nix::sys::stat::SFlag::S_IFCHR,
            nix::sys::stat::Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();
        let mut root = Root::new(dir.join("merged"));
        root.overlay = Some(crate::isolation::Overlay::new(
            vec![dir.join("base")],
            dir.join("upper"),
            dir.join("work"),
        ));
        let mut context = ctx("/bin/tool", &["/bin/tool"]);
        context.cwd = "/".into();
        context.container_root = Some(root);
        assert_eq!(
            context.resolve(Path::new("/bin/tool")),
            dir.join("upper/bin/tool")
        );
        assert!(
            !context.resolve(Path::new("/bin/gone")).exists(),
            "whited out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ctx(program: &str, candidates: &[&str]) -> FailureContext {
        FailureContext {
            program: program.into(),
            candidates: candidates.iter().map(PathBuf::from).collect(),
            cwd: "/srv/app".into(),
            uid: 1000,
            gid: 1000,
            rlimits: vec!["nofile=10:20".parse().unwrap()],
            root: false,
            container_root: None,
            names: Names::default(),
            sysctls: Vec::new(),
            seccomp: None,
            namespace_root: None,
            pids_max: None,
            hooks: crate::hooks::Hooks::default(),
            devices: Vec::new(),
        }
    }

    fn exec(errno: Errno, detail: u64, ctx: &FailureContext) -> SpawnError {
        describe(
            ChildFailure {
                step: ChildStep::Exec,
                errno,
                detail,
            },
            ctx,
        )
    }

    /// The init's re-exec refused (no sealed copy runs here, and the file
    /// may not stand in) says why; another failure there says only what
    /// failed.
    #[test]
    fn a_refused_reexec_names_the_sealed_copy() {
        let reexec = |errno| {
            describe(
                ChildFailure {
                    step: ChildStep::Reexec,
                    errno,
                    detail: 0,
                },
                &ctx("sh", &[]),
            )
            .to_string()
        };
        assert!(reexec(Errno::EACCES).contains("refused to run shipyard's binary"));
        let other = reexec(Errno::ENOMEM);
        assert!(
            other.contains("re-executing shipyard's own binary"),
            "{other}"
        );
        assert!(!other.contains("sealed copy"), "{other}");
    }

    #[test]
    fn steps_round_trip_through_their_numbers() {
        // Every number from 1 up names a step, with no gaps, until the first
        // unknown one; and each step maps back to its number.
        let known: Vec<ChildStep> = (1..).map_while(ChildStep::from_raw).collect();
        assert_eq!(known.len(), 75, "a new step needs a number in from_raw");
        for step in known {
            assert_eq!(ChildStep::from_raw(step as u16), Some(step));
        }
        assert_eq!(ChildStep::from_raw(0), None);
        assert_eq!(ChildStep::from_raw(999), None);
    }

    #[test]
    fn command_not_found() {
        let ctx = ctx("nope", &["/no/such/dir/nope", "/also/missing/nope"]);
        let err = exec(Errno::ENOENT, 1, &ctx);
        assert_eq!(err.exit_code(), 127);
        assert!(
            err.to_string().starts_with("nope: command not found"),
            "{err}"
        );
        assert!(
            err.to_string().contains("/no/such/dir:/also/missing"),
            "{err}"
        );
    }

    #[test]
    fn missing_file_given_by_path() {
        let ctx = ctx("/no/such/thing", &["/no/such/thing"]);
        let err = exec(Errno::ENOENT, 0, &ctx);
        assert_eq!(err.exit_code(), 127);
        assert_eq!(err.to_string(), "/no/such/thing: no such file");
    }

    #[test]
    fn existing_file_with_enoent_blames_the_interpreter() {
        // /etc/hosts exists, so ENOENT can't mean "not found".
        let ctx = ctx("hosts", &["/no/such/dir/hosts", "/etc/hosts"]);
        let err = exec(Errno::ENOENT, 0, &ctx);
        assert_eq!(err.exit_code(), 127);
        assert!(err.to_string().contains("interpreter or loader"), "{err}");
    }

    #[test]
    fn relative_candidates_are_checked_from_the_commands_directory() {
        // `hosts` exists in /etc, not in shipyard's own directory.
        let mut in_etc = ctx("./hosts", &["./hosts"]);
        in_etc.cwd = "/etc".into();
        let err = exec(Errno::ENOENT, 0, &in_etc);
        assert!(err.to_string().contains("interpreter or loader"), "{err}");

        // And a file next to shipyard doesn't count when the command ran in /.
        let here = std::env::current_dir().unwrap().join("Cargo.toml");
        assert!(here.is_file(), "the test runs in the crate directory");
        let mut in_root = ctx("./Cargo.toml", &["./Cargo.toml"]);
        in_root.cwd = "/".into();
        let err = exec(Errno::ENOENT, 0, &in_root);
        assert_eq!(err.to_string(), "./Cargo.toml: no such file");
    }

    #[test]
    fn permission_problems() {
        let dir = exec(Errno::EACCES, 0, &ctx("/tmp", &["/tmp"]));
        assert_eq!(dir.exit_code(), 126);
        assert!(dir.to_string().ends_with("is a directory"), "{dir}");

        let file = exec(Errno::EACCES, 0, &ctx("/etc/hosts", &["/etc/hosts"]));
        assert_eq!(file.exit_code(), 126);
        assert!(file.to_string().contains("permission denied"), "{file}");

        // EACCES for a file that isn't there: a directory on the way can't
        // be searched.
        let missing = exec(Errno::EACCES, 0, &ctx("/no/such/x", &["/no/such/x"]));
        assert_eq!(missing.exit_code(), 126);
        assert!(
            missing.to_string().contains("isn't searchable"),
            "{missing}"
        );
    }

    #[test]
    fn other_exec_errors() {
        let ctx = ctx("/x", &["/x"]);
        assert!(
            exec(Errno::ENOEXEC, 0, &ctx)
                .to_string()
                .contains("another CPU architecture")
        );
        assert!(exec(Errno::ETXTBSY, 0, &ctx).to_string().contains("busy"));
        assert!(
            exec(Errno::EAGAIN, 0, &ctx)
                .to_string()
                .contains("RLIMIT_NPROC")
        );
        let too_big = exec(Errno::E2BIG, 0, &ctx);
        assert_eq!(too_big.exit_code(), 125);
        assert_eq!(exec(Errno::ENOMEM, 0, &ctx).exit_code(), 126);
    }

    #[test]
    fn out_of_range_detail_falls_back_to_the_program() {
        let err = exec(Errno::ENOEXEC, u64::MAX, &ctx("/x", &["/x"]));
        assert!(err.to_string().starts_with("/x:"), "{err}");
    }

    #[test]
    fn network_namespace_failures_say_what_and_why() {
        let mut ctx = ctx("/x", &["/x"]);
        ctx.sysctls = vec![
            ("net.core.rmem_max".into(), "1".into()),
            ("kernel.shmmax".into(), "1".into()),
        ];
        let fail = |step, errno, detail| {
            describe(
                ChildFailure {
                    step,
                    errno,
                    detail,
                },
                &ctx,
            )
            .to_string()
        };
        assert_eq!(
            fail(ChildStep::Loopback, Errno::EPERM, 0),
            format!(
                "bringing up the loopback interface of the container's network namespace: {}{}",
                Errno::EPERM.desc(),
                permission_hint(false, "isolation")
            )
        );
        // A network setting only the whole machine has: said so.
        let host_only = fail(ChildStep::Sysctl, Errno::ENOENT, 0);
        assert!(
            host_only.starts_with(
                "setting sysctl net.core.rmem_max=1: a network namespace has no such setting"
            ),
            "{host_only}"
        );
        // Any other missing setting: the plain error.
        assert_eq!(
            fail(ChildStep::Sysctl, Errno::ENOENT, 1),
            format!("setting sysctl kernel.shmmax=1: {}", Errno::ENOENT.desc())
        );
    }

    #[test]
    fn confinement_failures_say_what_and_why() {
        let ctx = ctx("/x", &["/x"]);
        let fail = |step, errno, detail| {
            describe(
                ChildFailure {
                    step,
                    errno,
                    detail,
                },
                &ctx,
            )
            .to_string()
        };
        assert_eq!(
            fail(ChildStep::BoundingDrop, Errno::EPERM, 21),
            "dropping CAP_SYS_ADMIN from the capability bounding set: Operation not permitted"
        );
        let refused = fail(ChildStep::Seccomp, Errno::EINVAL, 0);
        assert!(
            refused.starts_with("installing the seccomp filter: Invalid argument (the kernel")
                && refused.contains("seccomp=unconfined"),
            "{refused}"
        );
        assert_eq!(
            fail(ChildStep::Seccomp, Errno::EACCES, 0),
            "installing the seccomp filter: Permission denied"
        );
        assert!(fail(ChildStep::SeccompCompile, Errno::E2BIG, 5000).contains("5000 instructions"));
        assert_eq!(
            fail(ChildStep::SeccompCompile, Errno::EINVAL, 0),
            "the seccomp profile can't be compiled"
        );
        // (Where AppArmor restricts user namespaces, Ubuntu's, the message
        // goes on to say so: this host's, not the test's.)
        let root = fail(
            ChildStep::NamespaceRoot,
            Errno::EPERM,
            NamespaceRootStep::Uid as u64,
        );
        assert!(
            root.starts_with(
                "becoming root in the container's user namespace: Operation not permitted"
            ),
            "{root}"
        );
        #[cfg(target_os = "linux")]
        for (which, id, wanted) in [
            (UnmappedId::User, 70000, "user 70000 isn't mapped"),
            (UnmappedId::Group, 5, "group 5 isn't mapped"),
            (
                UnmappedId::SupplementaryGroup,
                4_000_000_000,
                "supplementary group 4000000000 isn't mapped",
            ),
        ] {
            let detail = u64::try_from(which.detail(id)).unwrap();
            let message = fail(ChildStep::UnmappedId, Errno::EINVAL, detail);
            assert!(message.starts_with(wanted), "{message}");
        }
    }

    /// In a user namespace, a permission error names the host user the
    /// container's root is.
    #[test]
    fn permission_errors_in_a_user_namespace_name_the_mapped_root() {
        let with = |namespace_root, errno| {
            let ctx = FailureContext {
                namespace_root,
                ..ctx("/x", &["/x"])
            };
            describe(
                ChildFailure {
                    step: ChildStep::BindRoot,
                    errno,
                    detail: 0,
                },
                &ctx,
            )
            .to_string()
        };
        let note = "the container's root is host user 100000";
        assert!(with(Some(100_000), Errno::EACCES).contains(note));
        assert!(!with(Some(100_000), Errno::ENOENT).contains(note));
        assert!(!with(None, Errno::EACCES).contains("host user"));

        // `exec`'s EACCES is noted only where it's about permissions: not
        // for a directory.
        let exec_of = |program: &str| {
            let ctx = FailureContext {
                namespace_root: Some(100_000),
                ..ctx(program, &[program])
            };
            exec(Errno::EACCES, 0, &ctx).to_string()
        };
        let directory = exec_of("/");
        assert!(directory.ends_with("is a directory"), "{directory}");
        let unreachable = exec_of("/nonexistent/program");
        assert!(unreachable.contains(note), "{unreachable}");
    }

    /// A failure under the filter, with EPERM, may be the filter's refusal:
    /// the message says so, and only then.
    #[test]
    fn failures_under_a_filter_mention_it() {
        let note = "the seccomp profile, in force by then, may have refused it";
        let with = |seccomp, step, errno| {
            let ctx = FailureContext {
                seccomp,
                ..ctx("/x", &["/x"])
            };
            describe(
                ChildFailure {
                    step,
                    errno,
                    detail: 21,
                },
                &ctx,
            )
            .to_string()
        };
        let early = Some(FilterStart::BeforeSetup);
        let late = Some(FilterStart::BeforeExec);
        assert!(with(early, ChildStep::BoundingDrop, Errno::EPERM).ends_with(&format!("({note})")));
        assert!(with(early, ChildStep::SetUid, Errno::EPERM).contains(note));
        assert!(with(early, ChildStep::Exec, Errno::EPERM).contains(note));
        assert!(with(late, ChildStep::Exec, Errno::EPERM).contains(note));
        // Before the filter, without one, or another errno: nothing added.
        assert!(!with(late, ChildStep::BoundingDrop, Errno::EPERM).contains(note));
        assert!(!with(None, ChildStep::Exec, Errno::EPERM).contains(note));
        assert!(!with(early, ChildStep::Exec, Errno::EACCES).contains(note));
        assert!(!with(early, ChildStep::Seccomp, Errno::EPERM).contains(note));
    }

    /// A keyring quota failure names the limits to look at.
    #[test]
    fn a_keyring_quota_names_the_limits() {
        let keyring = describe(
            ChildFailure {
                step: ChildStep::SessionKeyring,
                errno: Errno::EDQUOT,
                detail: 0,
            },
            &ctx("/x", &["/x"]),
        )
        .to_string();
        assert!(
            keyring.starts_with("giving the container a session keyring of its own: ")
                && keyring.contains("kernel.keys.maxkeys"),
            "{keyring}"
        );
    }

    #[test]
    fn setup_failures_name_the_step_and_value() {
        let ctx = ctx("/x", &["/x"]);
        let fail = |step, errno, detail| {
            describe(
                ChildFailure {
                    step,
                    errno,
                    detail,
                },
                &ctx,
            )
        };

        let chdir = fail(ChildStep::Chdir, Errno::ENOENT, 0);
        assert_eq!(chdir.exit_code(), 125);
        assert_eq!(
            chdir.to_string(),
            "changing directory to /srv/app: No such file or directory"
        );

        let limit = fail(ChildStep::Rlimit, Errno::EPERM, 0);
        assert!(
            limit
                .to_string()
                .starts_with("setting resource limit nofile=10:20"),
            "{limit}"
        );
        assert!(limit.to_string().contains("needs root"), "{limit}");

        // As root, an EPERM can't mean "needs root".
        let as_root = FailureContext {
            root: true,
            container_root: None,
            names: Names::default(),
            sysctls: Vec::new(),
            ..ctx.clone()
        };
        let limit = describe(
            ChildFailure {
                step: ChildStep::Rlimit,
                errno: Errno::EPERM,
                detail: 0,
            },
            &as_root,
        );
        assert!(!limit.to_string().contains("needs root"), "{limit}");
        assert!(limit.to_string().contains("kernel's maximum"), "{limit}");

        assert_eq!(
            fail(ChildStep::SetUid, Errno::EPERM, 0).to_string(),
            "switching to user 1000: Operation not permitted"
        );

        let proc = fail(ChildStep::MountProc, Errno::EPERM, 0);
        assert_eq!(proc.exit_code(), 125);
        assert_eq!(
            proc.to_string(),
            "mounting /proc for the container: Operation not permitted \
             (isolation needs root)"
        );
        assert!(
            !fail(ChildStep::Hostname, Errno::EINVAL, 0)
                .to_string()
                .contains("needs root")
        );
        // As root, EPERM isn't about being root.
        let as_root = FailureContext {
            root: true,
            container_root: None,
            names: Names::default(),
            sysctls: Vec::new(),
            ..ctx.clone()
        };
        let refused = describe(
            ChildFailure {
                step: ChildStep::MountProc,
                errno: Errno::EPERM,
                detail: 0,
            },
            &as_root,
        );
        assert!(refused.to_string().contains("refused"), "{refused}");
        assert!(!refused.to_string().contains("needs root"), "{refused}");

        // Joining: the namespace is named, and the hint has no
        // `--no-isolation` (there's nothing to opt out of).
        let mount = crate::isolation::Namespace::Mount.index();
        let join = fail(ChildStep::JoinNamespace, Errno::EPERM, mount);
        assert_eq!(join.exit_code(), 125);
        assert_eq!(
            join.to_string(),
            "joining the container's mnt namespace: Operation not permitted \
             (not this user's container: its user namespace isn't yours)"
        );
        let refused = describe(
            ChildFailure {
                step: ChildStep::JoinNamespace,
                errno: Errno::EPERM,
                detail: mount,
            },
            &as_root,
        );
        assert!(refused.to_string().contains("refused"), "{refused}");
        // A detail that names no namespace still makes a message.
        assert_eq!(
            fail(ChildStep::JoinNamespace, Errno::EINVAL, u64::MAX).to_string(),
            "joining the container's namespaces: Invalid argument"
        );
    }
}
