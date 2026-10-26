//! The OCI runtime lifecycle: `create`, `start`, `state`, `kill` and
//! `delete`, as runc implements them, each a separate call; and the calls on
//! a container's cgroup: `pause`, `resume`, `update`, `ps`, `stats` and
//! `kill --all`. Containers need
//! Linux: elsewhere every call is [`ContainerError::LinuxOnly`]. The
//! implementation is in `lifecycle.rs`.
//!
//! **A container outlives the call that created it.** Shipyard has no daemon:
//! `create` starts a small **monitor** process (this binary again, through
//! [`monitor_entry_point`]) that spawns the container and stays its parent.
//! The monitor records how the container ended, and the container dies with
//! it (the init's parent-death signal).
//!
//! **Created, then started.** The container's init sets everything up, then
//! waits at a **start gate**: it opens the state directory's `exec.fifo` for
//! writing, which blocks until `start` opens it for reading. Until then the
//! user's program hasn't run.
//!
//! **The state directory** is `ROOT/ID` (mode 0700): `state.json` (the
//! bundle, the monitor's and the init's PIDs and start times, the
//! annotations, the hooks, and the cgroup), `exec.fifo` (until started), and
//! `exit` (once it ended). Every call takes an exclusive `flock` on the
//! directory while it reads and changes it.
//!
//! **PIDs are checked, not trusted:** a PID is only the init's if
//! `/proc/PID/stat` still shows the start time recorded with it. A reused PID
//! is someone else's process.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::oci::BundleError;

#[cfg(not(target_os = "linux"))]
mod elsewhere;
mod helper;
#[cfg(target_os = "linux")]
mod lifecycle;
mod plan;
mod versions;

#[cfg(not(target_os = "linux"))]
pub use elsewhere::{
    attach_stream, create, create_planned, delete, exec, exit_code, exit_record,
    helper_entry_point, hold, init_process, is_process, kill, kill_all, logs_of, memory_limit,
    monitor_entry_point, network_namespace, network_namespace_live, pause, peek_init, peek_monitor,
    peek_status, process_started, ps, release, remove_failed_logs, resume, start, start_helper,
    start_helper_alone, start_helper_in_view, state, stats, status, stop, update, wait,
};
#[cfg(target_os = "linux")]
pub use lifecycle::{Waiter, waiter};
#[cfg(target_os = "linux")]
pub use lifecycle::{
    attach_stream, create, create_planned, delete, exec, exit_code, exit_record,
    helper_entry_point, hold, init_process, is_process, kill, kill_all, logs_of, memory_limit,
    monitor_entry_point, network_namespace, network_namespace_live, pause, peek_init, peek_monitor,
    peek_status, process_started, ps, raise_own_descriptor_limit, release, remove_failed_logs,
    remove_monitor_cgroup, resume, running_init, start, start_helper, start_helper_alone,
    start_helper_in_view, state, stats, status, stop, update, wait,
};

pub use helper::{Helper, HelperFailure, HelperIo, HelperRoot, RootView, RunningHelper};
pub use plan::{Plan, PlanError};
pub use versions::ExitRecord;

/// Where root's state directories go unless told otherwise (runc's is
/// `/run/runc`).
pub const DEFAULT_ROOT: &str = "/run/shipyard";

/// Where this caller's state directories go unless told otherwise:
/// [`DEFAULT_ROOT`] for root; for anyone else `$XDG_RUNTIME_DIR/shipyard`
/// (as runc and Podman do), a per-user directory the login session creates
/// and cleans. Without it there's no safe place to guess (a `/tmp` path
/// invites races), so the caller must say.
///
/// # Errors
///
/// Not root, and `XDG_RUNTIME_DIR` isn't set, isn't absolute, isn't a
/// directory, or isn't the caller's. Anywhere but Linux,
/// [`ContainerError::LinuxOnly`], as every call is: that's the problem to
/// report, not a missing directory.
pub fn default_root() -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    if cfg!(not(target_os = "linux")) {
        return Err(ContainerError::LinuxOnly);
    }
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let owner = runtime_dir
        .as_ref()
        .and_then(|dir| std::fs::metadata(dir).ok())
        .filter(std::fs::Metadata::is_dir)
        .map(|meta| meta.uid());
    let me = nix::unistd::geteuid();
    choose_root(me.is_root(), runtime_dir.as_deref(), owner, me.as_raw())
}

/// [`default_root`]'s decision, for a caller that read the environment
/// itself (the engine's `Paths`): root's (`root`) is [`DEFAULT_ROOT`];
/// anyone else's is `shipyard` in their `runtime_dir` (`XDG_RUNTIME_DIR`),
/// which must be absolute, a directory, and theirs: `owner` is the uid
/// owning it, if it's a directory, and `me` the caller's.
///
/// # Errors
///
/// [`ContainerError::NoStateRoot`], saying why.
pub fn choose_root(
    root: bool,
    runtime_dir: Option<&Path>,
    owner: Option<u32>,
    me: u32,
) -> Result<PathBuf> {
    if root {
        return Ok(PathBuf::from(DEFAULT_ROOT));
    }
    let problem = match (runtime_dir, owner) {
        (None, _) => "XDG_RUNTIME_DIR isn't set".to_owned(),
        (Some(dir), _) if !dir.is_absolute() => {
            format!("XDG_RUNTIME_DIR ({}) isn't an absolute path", dir.display())
        }
        (Some(dir), None) => format!("XDG_RUNTIME_DIR ({}) isn't a directory", dir.display()),
        (Some(dir), Some(owner)) if owner != me => format!(
            "XDG_RUNTIME_DIR ({}) belongs to user {owner}, not you",
            dir.display()
        ),
        (Some(dir), Some(_)) => return Ok(dir.join("shipyard")),
    };
    Err(ContainerError::NoStateRoot(problem))
}

/// The version of the OCI runtime spec the state follows.
pub const OCI_VERSION: &str = "1.2.0";

/// What [`create`] does besides what the bundle says.
#[derive(Debug, Default)]
pub struct CreateOptions<'a> {
    /// A connected UNIX socket to send the container's terminal to (runc's
    /// `--console-socket`), for a bundle that asks for one.
    pub console: Option<std::os::fd::BorrowedFd<'a>>,
    /// Capture the container's output into logs (`--log-dir`), instead of
    /// giving it the caller's stdio.
    pub logs: Option<LogOptions>,
    /// Descriptors to pass on to the container's process, at 3 and up
    /// (`--preserve-fds`, socket activation): see
    /// [`PassedFds`](crate::process::passed::PassedFds). The container's
    /// init holds them from `create` until its program starts.
    pub passed: Option<&'a crate::process::passed::PassedFds>,
    /// Descriptors the container's monitor holds from `create` until the
    /// container has ended (`--hold-fd`): what keeps the caller's
    /// resources alive (a `flock`'s, say) once the caller has gone. Then
    /// they're the exit command's, or closed. The container never has them.
    /// Each must be above 2, given once, and above the descriptors
    /// [`passed`](Self::passed) puts at 3 and up; each has a label
    /// ([`Hold`]), which the exit command is told, and by which it can be
    /// let go of while the container runs ([`release`]).
    pub holds: &'a [Hold<'a>],
    /// What the monitor runs once the container has ended
    /// (`--exit-command`): given the holds, to clean up after it.
    pub exit_command: Option<ExitCommand>,
    /// What the monitor runs beside the container (`--companion`): started
    /// once the container is created (held at its gate: the companion waits
    /// for the start itself), in a session of its own, with the monitor's
    /// environment, `/dev/null` for its standard streams and none of the
    /// holds, killed by the monitor's death (its parent-death signal); its
    /// process group is killed, and it's reaped, once the container has
    /// ended, before that's recorded. For an engine's health checks.
    pub companion: Option<ExitCommand>,
}

/// A program the monitor runs: its exit command, or its companion
/// ([`CreateOptions::companion`]). An exit command runs once its container
/// has ended (conmon's `--exit-command`): after its end is recorded, in a
/// session of its own, with the monitor's environment (the creator's),
/// `/dev/null` for its standard streams, the holds at 3 and up
/// ([`HOLDS_ENV`] says how many, [`HOLD_LABELS_ENV`] their labels), how the
/// container ended ([`EXIT_RECORD_ENV`]), and the monitor's PID
/// ([`MONITOR_PID_ENV`]: its copies of the holds go when it exits). The
/// monitor doesn't wait for it, and a failure to start it is silent: what
/// it would have cleaned up is left to the sweeps of whatever held it.
///
/// It may find the container's state gone (`delete --force` doesn't wait
/// for it), and an OCI caller may reuse the ID meanwhile: it mustn't act on
/// the container by ID alone. Its arguments are UTF-8, as the record that
/// keeps them is JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedExitCommand")]
#[non_exhaustive]
pub struct ExitCommand {
    /// The program (an absolute path).
    pub program: PathBuf,
    /// Its arguments, after its name.
    pub args: Vec<String>,
}

impl ExitCommand {
    /// `program`, with `args`.
    ///
    /// # Errors
    ///
    /// [`ContainerError::BadExitCommand`]: the program isn't an absolute
    /// path (the monitor runs in `/`, with the creator's `PATH` no help).
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Result<Self> {
        let program = program.into();
        // Absolute, UTF-8 (JSON keeps it), and no NUL anywhere: an `execve`
        // would refuse that at the container's end, too late to say.
        let valid = program.is_absolute()
            && program.to_str().is_some_and(|text| !text.contains('\0'))
            && !args.iter().any(|arg| arg.contains('\0'));
        if !valid {
            return Err(ContainerError::BadExitCommand(program));
        }
        Ok(Self { program, args })
    }
}

/// An [`ExitCommand`] as JSON gives it: checked by [`ExitCommand::new`].
#[derive(Deserialize)]
struct UncheckedExitCommand {
    program: PathBuf,
    args: Vec<String>,
}

impl TryFrom<UncheckedExitCommand> for ExitCommand {
    type Error = ContainerError;

    fn try_from(unchecked: UncheckedExitCommand) -> Result<Self> {
        Self::new(unchecked.program, unchecked.args)
    }
}

/// A created container's init, for a caller that follows it
/// ([`init_process`]).
#[derive(Debug)]
#[non_exhaustive]
pub struct InitProcess {
    /// Its PID, as the host sees it.
    pub pid: i32,
    /// Its start time (clock ticks since boot, `/proc/PID/stat`'s 22nd
    /// field): with the PID, it names this process alone.
    pub start_time: u64,
    /// A pidfd for it (readable once it has exited), where the kernel has
    /// them (Linux 5.3).
    pub pidfd: Option<std::os::fd::OwnedFd>,
}

/// A descriptor the container's monitor holds (`--hold-fd N=LABEL`), and
/// what it's called.
#[derive(Debug, Clone, Copy)]
pub struct Hold<'a> {
    /// The descriptor.
    pub fd: std::os::fd::BorrowedFd<'a>,
    /// Its label ([`crate::attach::is_label`]): told to the exit command
    /// ([`HOLD_LABELS_ENV`]), and what [`release`] names.
    pub label: &'a str,
}

/// The variable that tells an exit command how many holds it has, at 3 and
/// up.
pub const HOLDS_ENV: &str = "SHIPYARD_HOLDS";

/// The variable that tells an exit command its holds' labels, in their
/// order (3 and up), comma-separated: those given at `create` that weren't
/// let go of, then those held since ([`hold`]).
pub const HOLD_LABELS_ENV: &str = "SHIPYARD_HOLD_LABELS";

/// The most descriptors a container's monitor holds for its caller (see
/// [`CreateOptions::holds`]): room for a container publishing a whole `-p`
/// range (1024 ports, on IPv4 and IPv6: two sockets each), beside its
/// networks and volumes. The caller's descriptor limit bounds those given
/// at `create` (they're open in the caller first); the monitor raises its
/// own towards this many and room beside them once its container is
/// spawned, as far as the hard limit lets it (F49), so the holds added
/// since aren't bound by the caller's soft limit.
pub const MOST_HOLDS: usize = 2_100;

/// The descriptors a monitor keeps of its own beside its holds when it
/// hands them to its exit command.
const PLACING_SLACK: usize = 64;

/// The descriptor numbers a monitor needs to hand `holds` holds to its
/// exit command: it copies each above where they go (3 and up) before
/// placing them, so the holds, their copies and its own are open at once.
#[must_use]
pub const fn exit_command_descriptors(holds: usize) -> usize {
    holds.saturating_mul(2).saturating_add(3 + PLACING_SLACK)
}

/// The most holds a monitor can hand its exit command under the hard
/// descriptor limit `hard` ([`exit_command_descriptors`]), never more than
/// [`MOST_HOLDS`]: a container given more would end without its exit
/// command (its end unrecorded, its restart policy lost).
#[must_use]
pub fn most_holds_under(hard: u64) -> usize {
    let hard = usize::try_from(hard).unwrap_or(usize::MAX);
    (hard.saturating_sub(3 + PLACING_SLACK) / 2).min(MOST_HOLDS)
}

/// What a monitor says, after the number, when it holds all it can (in a
/// refused hold's [`ContainerError::NotHeld`]). A version 4 monitor says
/// [`HoldRefusal::Full`](crate::attach::HoldRefusal::Full) too; the words
/// are read only from a version 3 one (9.7b1 to 10.2c1), and stay as long
/// as one may still run.
pub const HOLDS_FULL: &str = "descriptors are held already";

/// The most bytes of hold labels a monitor keeps, all told: they reach the
/// exit command in one environment variable ([`HOLD_LABELS_ENV`]), and one
/// string of an exec's is at most 128 KiB (`MAX_ARG_STRLEN`).
pub const MOST_HOLD_LABEL_BYTES: usize = 96 * 1024;

/// The variable that gives an exit command how the container ended: its
/// [`ExitRecord`], as JSON (the state directory may be gone by then).
pub const EXIT_RECORD_ENV: &str = "SHIPYARD_EXIT_RECORD";

/// The variable that gives an exit command its monitor's PID: the monitor
/// keeps its own copies of the holds until it has exited (it doesn't wait
/// for the exit command), so what they keep alive isn't free before then.
/// While the exit command's parent is this PID, the monitor is there.
pub const MONITOR_PID_ENV: &str = "SHIPYARD_MONITOR_PID";

/// Where an exit command finds its monitor's start time (clock ticks since
/// boot, `/proc/PID/stat` field 22), beside [`MONITOR_PID_ENV`]: with the
/// PID, what tells that monitor's start from another's, whose state took
/// its place (F102). A monitor from before 10.4 sets none.
pub const MONITOR_STARTED_ENV: &str = "SHIPYARD_MONITOR_STARTED";

/// What [`exec`] runs in a container, and how (runc's `exec` flags). What
/// isn't given is the container's own process's: its environment, working
/// directory, user, capabilities, `no_new_privs`, limits; its seccomp
/// filter always. None of it may widen what the container has.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
#[allow(
    clippy::struct_excessive_bools,
    reason = "exec's flags (--no-new-privs, -d, -t, no -i), each its own"
)]
pub struct ExecOptions {
    /// The command and its arguments (`exec ID CMD…`); empty with
    /// [`process`](Self::process).
    pub args: Vec<std::ffi::OsString>,
    /// A runc `process.json` (`--process`): the whole process instead.
    pub process: Option<PathBuf>,
    /// The working directory (`--cwd`), inside the container.
    pub cwd: Option<PathBuf>,
    /// `KEY=VALUE` entries (`--env`) added to (or replacing ones of) the
    /// container's environment.
    pub env: Vec<std::ffi::OsString>,
    /// The user (`--user UID[:GID]`): numeric, as the container sees them.
    pub user: Option<(u32, Option<u32>)>,
    /// The user by name or number, `USER[:GROUP]` (`shipyard exec -u`),
    /// taking the place of [`user`](Self::user) (ignored when this is
    /// given): looked up in the container's own `/etc/passwd` and
    /// `/etc/group`, with the groups that list it and those the container
    /// was given (as Docker's), and its home as `HOME` unless
    /// [`env`](Self::env) or the container's own environment sets one.
    pub user_spec: Option<String>,
    /// No input (`shipyard exec` without `-i`): its stdin is `/dev/null`,
    /// and a terminal (`-t`) takes no typing.
    pub no_input: bool,
    /// Supplementary groups (`--additional-gids`), instead of the
    /// container's process's.
    pub additional_gids: Vec<u32>,
    /// Capabilities to add (`--cap`), within the container's bounding set.
    pub capabilities: Vec<crate::security::Capability>,
    /// Set `no_new_privs` (`--no-new-privs`).
    pub no_new_privileges: bool,
    /// Return once it runs (`-d`), leaving it running.
    pub detach: bool,
    /// A pseudo-terminal for it (`-t`), made in the container: relayed to
    /// this process's own descriptors (raw if its stdin is a terminal), or,
    /// with [`console`](Self::console), sent there.
    pub terminal: bool,
    /// An OCI console socket (`--console-socket`), connected, open for the
    /// whole call: the terminal's master goes there (runc's protocol).
    pub console: Option<std::os::fd::RawFd>,
    /// How a stop request (SIGTERM or SIGINT to this process) ends the
    /// command: its signal, then SIGKILL to all it started after its
    /// timeout (by default SIGTERM, then 10 s).
    pub stop: crate::process::StopPolicy,
    /// Where to write the command's PID (as the host sees it) once it runs,
    /// detached or not (runc's `--pid-file`; F117: the API's exec inspect).
    /// A failure to write it is logged, never the exec's.
    pub pid_file: Option<PathBuf>,
}

impl ExecOptions {
    /// Runs `args`, the rest as the container's process has it.
    #[must_use]
    pub fn new(args: Vec<std::ffi::OsString>) -> Self {
        Self {
            args,
            ..Self::default()
        }
    }
}

/// How an [`exec`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecOutcome {
    /// The command ran and ended so.
    Exited(crate::process::ExitStatus),
    /// Detached (`-d`): running, with this PID (as the host sees it).
    Detached {
        /// Its PID.
        pid: u32,
    },
}

/// Whether a container takes input through attach (`create --stdin`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StdinMode {
    /// `/dev/null`: attach is output only.
    #[default]
    Closed,
    /// A pipe (or the terminal) the monitor holds open; attached clients
    /// type into it, and detaching leaves it open.
    Open,
    /// As [`Open`](Self::Open), closed when the first client to type goes
    /// or its input ends, once what it typed is written (`--stdin-once`).
    Once,
}

/// Where and how a container's output is kept ([`CreateOptions::logs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOptions {
    /// A directory that doesn't exist yet: `create` makes it (0700), and
    /// the logs outlive the container (they're the caller's to remove).
    pub dir: PathBuf,
    /// Their rotation.
    pub settings: crate::logs::info::LogSettings,
    /// Whether the container takes input through attach.
    pub stdin: StdinMode,
}

/// A container's logs ([`logs_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogsOf {
    /// Their directory.
    pub dir: PathBuf,
    /// Whether they're complete: the container has ended, and everything it
    /// wrote is in them.
    pub finished: bool,
    /// The directory `create` made, by identity: what's at [`dir`](Self::dir)
    /// now may be another, put there since. `None` for a container created
    /// before 8.8c, which recorded only the path.
    pub identity: Option<DirIdentity>,
    /// Which container wrote them: its monitor's PID and start time. A
    /// directory deleted and made again can get the same identity (ext4 and
    /// xfs hand an inode number on), but never the same monitor.
    pub instance: Option<(i32, u64)>,
}

/// A directory's identity: its device and inode. No other directory has it
/// while this one exists, whereas a path can come to name another (removed
/// and made again, or renamed and replaced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirIdentity {
    /// Its filesystem's device (`st_dev`).
    pub dev: u64,
    /// Its inode there (`st_ino`).
    pub ino: u64,
}

impl DirIdentity {
    /// The identity of the directory open at `fd`.
    ///
    /// # Errors
    ///
    /// What `fstat` gives.
    #[cfg(target_os = "linux")]
    pub fn of(fd: std::os::fd::BorrowedFd<'_>) -> nix::Result<Self> {
        nix::sys::stat::fstat(fd).map(|stat| Self::from_stat(&stat))
    }

    /// The identity `stat` describes.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn from_stat(stat: &nix::sys::stat::FileStat) -> Self {
        Self {
            dev: stat.st_dev,
            ino: stat.st_ino,
        }
    }
}

/// A container `create` made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Created {
    /// Its init's PID (for `--pid-file`).
    pub pid: i32,
    /// What the bundle asked for that is ignored, one message each (as
    /// `shipyard run --bundle` prints them).
    pub warnings: Vec<String>,
}

/// Where a container is in its life (OCI's `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Status {
    /// `create` is still setting it up.
    Creating,
    /// Set up, waiting for `start`.
    Created,
    /// Its program runs.
    Running,
    /// Its init has ended and its monitor is cleaning up (its cgroup
    /// removed, its output drained into the logs): stopped once that's
    /// done. Derived from what's alive, never stored. OCI's `state` says
    /// `stopped` (its process has exited); what needs it running says it's
    /// stopping.
    Stopping,
    /// Its processes are frozen (`pause`), until `resume`.
    Paused,
    /// It has ended.
    Stopped,
}

impl Status {
    /// Whether its program is there to reach: running, or frozen (paused).
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Creating => "creating",
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        })
    }
}

/// A container's state, as OCI's `state` prints it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct State {
    /// The OCI runtime spec version followed ([`OCI_VERSION`]).
    pub oci_version: String,
    /// Its ID.
    pub id: String,
    /// Where it is in its life.
    pub status: Status,
    /// Its init's PID, while it's created, running or paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    /// The bundle it was created from (absolute).
    pub bundle: PathBuf,
    /// Its config's annotations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// Why a lifecycle call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ContainerError {
    /// Not an ID a container can have.
    #[error(
        "invalid container ID \"{0}\": a letter or digit, then letters, digits, _ . + or -, \
         at most 255 in all"
    )]
    InvalidId(String),
    /// `create` with an ID that's taken.
    #[error("a container with ID {0} exists")]
    Exists(String),
    /// No container with that ID.
    #[error("no container with ID {0}")]
    NotFound(String),
    /// The container isn't where the call needs it to be.
    #[error("container {id} is {status}; {action} needs it {wanted}")]
    WrongStatus {
        /// Its ID.
        id: String,
        /// Where it is.
        status: Status,
        /// The call.
        action: &'static str,
        /// Where it must be for that.
        wanted: &'static str,
    },
    /// The bundle has no PID namespace, and the container can't have a
    /// cgroup either (the reason given): then nothing can find, signal or
    /// clean up all its processes.
    #[error(
        "the bundle has no pid namespace, and without one its processes can only all be found \
         and stopped through a cgroup of its own, which it can't have here: {0}"
    )]
    NoPidNamespace(String),
    /// The bundle asks for limits, and the container can't have a cgroup to
    /// hold them (the reason given).
    #[error(
        "the bundle asks for resource limits (linux.resources), which need a cgroup of the \
         container's own, and it can't have one here: {0}"
    )]
    LimitsNeedACgroup(String),
    /// A command that works on the container's cgroup (pause, update, ps,
    /// `kill --all`, events), for a container that has none.
    #[error(
        "container {0} has no cgroup of its own (made where none could be: no delegated \
         subtree, or cgroup v1), which this needs"
    )]
    NeedsCgroup(String),
    /// No default directory for a non-root caller's state.
    #[error(
        "no directory for the containers' state: {0} (a login session sets it); give one \
         with --root"
    )]
    NoStateRoot(String),
    /// `kill` with SIGSTOP: it can't be caught, so it would stop the
    /// container's init, which then passes nothing on, instead of its
    /// program.
    #[error(
        "SIGSTOP would stop container {0}'s init, not its program; send TSTP to stop the \
         program, or pause the whole container"
    )]
    StopSignal(String),
    /// A signal that can't stop container `id`: one that would stop or
    /// continue its init rather than end it (SIGSTOP, SIGTSTP, SIGTTIN,
    /// SIGTTOU, SIGCONT), or that the init can't pass on (SIGCHLD, 32, 33).
    #[error(
        "signal {signal} can't stop container {id}: it would pause or continue it, or never \
         reach its command"
    )]
    CantStop {
        /// Its ID.
        id: String,
        /// The signal's number.
        signal: i32,
    },
    /// The bundle asks for a terminal (`process.terminal`), and no console
    /// socket was given to send it to.
    #[error(
        "the bundle asks for a terminal (process.terminal), and there's no console socket to \
         send it to: give one with --console-socket (or keep its output in logs with --log-dir)"
    )]
    TerminalWithoutConsoleSocket,
    /// A console socket was given, and the bundle asks for no terminal.
    #[error(
        "a console socket was given, and the bundle asks for no terminal (process.terminal is \
         false)"
    )]
    ConsoleSocketWithoutTerminal,
    /// A console socket and logs both: a terminal goes to one or the other.
    #[error(
        "a console socket and a log directory both: the container's terminal goes either to the \
         console socket or into the logs"
    )]
    ConsoleSocketWithLogs,
    /// The log directory exists already: logs never go into one that may be
    /// another container's.
    #[error("the log directory {} exists: give one that doesn't (create makes it)", .0.display())]
    LogDirExists(PathBuf),
    /// Rotation settings out of bounds.
    #[error(transparent)]
    LogSettings(#[from] crate::logs::info::SettingsError),
    /// `--stdin-once` for a container with a terminal.
    #[error(
        "--stdin-once can't go with a terminal (process.terminal): closing a terminal is a \
         hang-up, not an end of input"
    )]
    StdinOnceWithTerminal,
    /// The container can't be attached to: created without `--log-dir`, or
    /// by a Shipyard before attach.
    #[error("container {0} can't be attached to: create it with --log-dir")]
    NotAttachable(String),
    /// The container's monitor has no control channel (`stop`, `wait`):
    /// one of a Shipyard before them.
    #[error(
        "container {0}'s monitor can't be asked to stop it (an older Shipyard's): end it with \
         kill, or rm -f"
    )]
    NoControl(String),
    /// The container's monitor can't hold descriptors for it (an older
    /// Shipyard's, before protocol version 3).
    #[error(
        "container {0}'s monitor can't hold or let go of descriptors (an older Shipyard's): \
         restart the container"
    )]
    MonitorTooOld(String),
    /// The container's monitor didn't answer a hold or release (no answer
    /// in time, or the connection lost while the container's up): it may or
    /// may not have done it.
    #[error("container {id}'s monitor: {why} (it may or may not have done it)")]
    HoldUnanswered {
        /// Its ID.
        id: String,
        /// What happened.
        why: String,
    },
    /// The container's monitor didn't hold (or let go of) a descriptor:
    /// why, as it says (or the container has ended).
    #[error("container {id}'s monitor: {why}")]
    NotHeld {
        /// Its ID.
        id: String,
        /// Why: what a caller tells refusals apart by (F73).
        reason: crate::attach::HoldRefusal,
        /// Why, in words.
        why: String,
    },
    /// The container's monitor turned a control client away (too many of
    /// its kind), or didn't answer.
    #[error("container {id}'s monitor: {reason}")]
    MonitorRefused {
        /// Its ID.
        id: String,
        /// Why.
        reason: String,
    },
    /// Nothing answers on the container's attach socket: its monitor was
    /// killed, or let go as the container ended.
    #[error("container {0}'s monitor is gone: nothing can be attached to")]
    MonitorGone(String),
    /// `exec` can't run in this container (the reason): made by a Shipyard
    /// before `exec`, or without PID and mount namespaces of its own.
    #[error("container {id} can't be exec'd into: {why}")]
    NotExecutable {
        /// Its ID.
        id: String,
        /// Why.
        why: String,
    },
    /// `exec`'s process would have more than the container's own (what):
    /// a capability outside its bounding set, `no_new_privs` off, a higher
    /// limit or a lower OOM score adjustment.
    #[error("exec can't give the process more than the container has: {0}")]
    ExecBeyondContainer(String),
    /// The container's cgroup has sub-cgroups with controllers (made inside
    /// it): `exec` can't start a process in it.
    #[error(
        "container {0}'s cgroup has sub-cgroups with controllers enabled (made inside the \
         container), and takes no process itself: exec can't run there"
    )]
    ExecCgroupBusy(String),
    /// A rootless `exec` from a cgroup that isn't delegated to the user (an
    /// SSH session's, say).
    #[error(
        "exec can't move its process into container {0}'s cgroup from the cgroup it runs in (a \
         login whose cgroup isn't delegated to you, such as an SSH session's): run it in your \
         user session's, with `systemd-run --user --scope shipyard-runtime exec …`"
    )]
    ExecCgroupDenied(String),
    /// `exec --console-socket` without a terminal (`-t`, or the
    /// `process.json`'s `terminal`).
    #[error("a console socket was given, and exec has no terminal (give -t)")]
    ExecConsoleWithoutTerminal,
    /// `exec -d -t` without a console socket: nothing would hold the
    /// terminal (runc's rule).
    #[error("exec -d -t needs --console-socket: nothing else would hold the terminal")]
    ExecTerminalNeedsConsole,
    /// `exec` failed (its process never ran): the reason.
    #[error(transparent)]
    Exec(Box<crate::Error>),
    /// A helper (`cp`'s) needs the container's own filesystem view, and it
    /// has no mount namespace of its own.
    #[error(
        "container {0} has no mount namespace of its own: its files are the host's, not its own"
    )]
    HelperNeedsMount(String),
    /// A helper couldn't be started (the reason).
    #[error(transparent)]
    Helper(Box<crate::Error>),
    /// A helper said what went wrong.
    #[error(transparent)]
    HelperFailed(HelperFailure),
    /// A helper ended without saying why.
    #[error("the helper ended without saying why ({0})")]
    HelperEnded(String),
    /// The container's output isn't captured (it was created without
    /// `--log-dir`): there are no logs to read.
    #[error("container {0} has no logs: it was created without --log-dir")]
    NotCaptured(String),
    /// A plan ([`create_planned`]) that can't be run: what's wrong with it.
    /// Found before anything is made, as a bundle's mistakes are.
    #[error("the container's plan can't be run: {0}")]
    InvalidPlan(#[from] PlanError),
    /// A `plan.json` of a version this binary doesn't read (one binary
    /// writes and reads it, so another can only be a mistake, or a newer
    /// binary's plan).
    #[error(
        "{} has version {found}, and this Shipyard reads versions 1 to {}",
        plan::PLAN_FILE,
        plan::PLAN_VERSION
    )]
    UnknownPlanVersion {
        /// Its version.
        found: u32,
    },
    /// A hold (`--hold-fd`) that isn't one: not open, a standard stream
    /// (0 to 2), given twice, one of the descriptors passed to the
    /// container's process, or not a file, a directory or a socket.
    #[error(
        "--hold-fd {0}: not a descriptor the monitor can hold (it must be open, above 2, given \
         once, not passed to the container, and a file, a directory or a socket)"
    )]
    BadHold(i32),
    /// A hold whose label isn't one ([`crate::attach::is_label`]).
    #[error(
        "{0:?} isn't a hold's label (letters, digits, : _ . and -, at most {max})",
        max = crate::attach::MAX_LABEL
    )]
    BadHoldLabel(String),
    /// More holds than a monitor keeps ([`MOST_HOLDS`]), or labels longer
    /// than it passes on, all told.
    #[error(
        "--hold-fd: {0} descriptors, and a monitor holds at most {most} (with labels of at most \
         {bytes} bytes, all told)",
        most = MOST_HOLDS,
        bytes = MOST_HOLD_LABEL_BYTES
    )]
    TooManyHolds(usize),
    /// An exit command that isn't an absolute UTF-8 path, or has a NUL in
    /// it or its arguments.
    #[error(
        "the exit command {} isn't one: it must be an absolute path, in UTF-8, with no NUL in \
         it or its arguments",
        .0.display()
    )]
    BadExitCommand(PathBuf),
    /// A file in the state directory was written by a newer Shipyard, in a
    /// version this one can't read.
    #[error(
        "{file} has version {found}, which this Shipyard doesn't know (it reads version {} and \
         older): a newer Shipyard wrote it",
        versions::STATE_VERSION
    )]
    UnknownVersion {
        /// The file.
        file: &'static str,
        /// Its version.
        found: u32,
    },
    /// Another call has held the container's lock too long (stopped, or
    /// stuck on a hung file system).
    #[error("container {0} is busy: another call has held its lock for over 15 seconds")]
    Busy(String),
    /// The state root could be changed by someone else.
    #[error("refusing the state root {}: {reason}", path.display())]
    UnsafeRoot {
        /// The root given.
        path: PathBuf,
        /// What's wrong with it.
        reason: &'static str,
    },
    /// Containers need a Linux kernel.
    #[error(
        "containers need a Linux kernel (current host: {})",
        std::env::consts::OS
    )]
    LinuxOnly,
    /// The bundle can't be run.
    #[error(transparent)]
    Bundle(#[from] BundleError),
    /// Its cgroup couldn't be made, limited, frozen, signalled or removed.
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Cgroup(#[from] crate::cgroup::CgroupError),
    /// Setting the container up failed (the monitor's message).
    #[error("{0}")]
    Setup(String),
    /// A file or system call in the state directory failed.
    #[error("{doing}: {error}")]
    Io {
        /// What was being done.
        doing: String,
        /// Why it failed. (Not the error's `source()`: the message already
        /// says it, and a cause would be printed twice.)
        error: std::io::Error,
    },
}

/// A lifecycle call's result.
pub type Result<T, E = ContainerError> = std::result::Result<T, E>;

/// Accepts IDs as runc does: a letter or digit first, then letters, digits
/// and `_ . + -`, at most 255 (one directory name). No `/`, `.` or `..`.
///
/// # Errors
///
/// [`ContainerError::InvalidId`].
pub fn validate_id(id: &str) -> Result<()> {
    let valid = id.len() <= 255
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ContainerError::InvalidId(id.to_owned()))
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    #[test]
    fn roots_state_is_in_run_and_others_in_their_runtime_dir() {
        let dir = || Some(Path::new("/run/user/1000"));
        assert_eq!(
            choose_root(true, None, None, 0).unwrap(),
            Path::new(DEFAULT_ROOT)
        );
        assert_eq!(
            choose_root(false, dir(), Some(1000), 1000).unwrap(),
            Path::new("/run/user/1000/shipyard")
        );
        for (runtime_dir, owner, wanted) in [
            (None, None, "isn't set"),
            (
                Some(Path::new("run/user/1000")),
                Some(1000),
                "isn't an absolute path",
            ),
            (dir(), None, "isn't a directory"),
            (dir(), Some(0), "belongs to user 0, not you"),
        ] {
            let err = choose_root(false, runtime_dir, owner, 1000)
                .unwrap_err()
                .to_string();
            assert!(err.contains(wanted) && err.contains("--root"), "{err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor needs room for its holds twice (and its own) to hand
    /// them on; under a hard limit, the most holds are what fits that,
    /// never past `MOST_HOLDS`.
    #[test]
    fn holds_are_bound_by_the_room_to_hand_them_on() {
        assert_eq!(exit_command_descriptors(0), 67);
        assert_eq!(exit_command_descriptors(1_300), 2_667);
        assert_eq!(most_holds_under(4_096), 2_014);
        assert!(exit_command_descriptors(most_holds_under(4_096)) <= 4_096);
        assert!(exit_command_descriptors(most_holds_under(4_096) + 1) > 4_096);
        assert_eq!(most_holds_under(1_048_576), MOST_HOLDS);
        assert_eq!(most_holds_under(u64::MAX), MOST_HOLDS);
        assert_eq!(most_holds_under(10), 0);
    }

    /// An exit command is an absolute UTF-8 path with no NUL, its
    /// arguments without one; read from JSON, it's checked the same.
    #[test]
    fn exit_commands_are_checked_however_made() {
        assert!(ExitCommand::new("/bin/cleanup", vec!["web".to_owned()]).is_ok());
        for (program, args) in [
            ("cleanup", vec![]),
            ("/bin/clean\0up", vec![]),
            ("/bin/cleanup", vec!["w\0eb".to_owned()]),
        ] {
            assert!(
                matches!(
                    ExitCommand::new(program, args),
                    Err(ContainerError::BadExitCommand(_))
                ),
                "{program:?}"
            );
        }
        let read: ExitCommand =
            serde_json::from_str(r#"{"program":"/bin/cleanup","args":["a"]}"#).unwrap();
        assert_eq!(read.args, ["a"]);
        assert!(serde_json::from_str::<ExitCommand>(r#"{"program":"cleanup","args":[]}"#).is_err());
    }

    /// The reason is in the message, not also its cause: printed as a chain
    /// (the CLI does), it would appear twice.
    #[test]
    fn an_io_error_is_said_once() {
        let err = ContainerError::Io {
            doing: "reading x".into(),
            error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn ids_are_checked() {
        for good in ["a", "web-1", "db.v2", "x_y+z", "0abc"] {
            assert!(validate_id(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "-x",
            "_x",
            "a/b",
            "a b",
            "ä",
            &"x".repeat(256),
        ] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_state_is_ocis_json() {
        let state = State {
            oci_version: OCI_VERSION.into(),
            id: "c1".into(),
            status: Status::Created,
            pid: Some(42),
            bundle: "/b".into(),
            annotations: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
        };
        let json: serde_json::Value = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ociVersion": OCI_VERSION, "id": "c1", "status": "created",
                "pid": 42, "bundle": "/b", "annotations": {"k": "v"}
            })
        );
        let stopped = State {
            status: Status::Stopped,
            pid: None,
            annotations: BTreeMap::new(),
            ..state
        };
        let json = serde_json::to_value(&stopped).unwrap();
        assert!(json.get("pid").is_none() && json.get("annotations").is_none());
    }
}
