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

#[cfg(not(target_os = "linux"))]
pub use elsewhere::{
    attach_stream, create, delete, exec, exit_code, helper_entry_point, kill, kill_all, logs_of,
    memory_limit, monitor_entry_point, pause, ps, remove_failed_logs, resume, start, start_helper,
    state, stats, update,
};
#[cfg(target_os = "linux")]
pub use lifecycle::{
    attach_stream, create, delete, exec, exit_code, helper_entry_point, kill, kill_all, logs_of,
    memory_limit, monitor_entry_point, pause, ps, remove_failed_logs, resume, start, start_helper,
    state, stats, update,
};

pub use helper::{Helper, HelperFailure, HelperIo, RunningHelper};

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

/// [`default_root`]'s decision: `owner` is the uid owning `runtime_dir`,
/// if it's a directory.
fn choose_root(
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
}

/// What [`exec`] runs in a container, and how (runc's `exec` flags). What
/// isn't given is the container's own process's: its environment, working
/// directory, user, capabilities, `no_new_privs`, limits; its seccomp
/// filter always. None of it may widen what the container has.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
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
    /// Its processes are frozen (`pause`), until `resume`.
    Paused,
    /// It has ended.
    Stopped,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Creating => "creating",
            Self::Created => "created",
            Self::Running => "running",
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
