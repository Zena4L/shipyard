use std::borrow::Cow;

use nix::errno::Errno;

use crate::isolation::IsolationError;
use crate::process::{EXIT_ENGINE_ERROR, SpawnError, SpecError, StopSignalError};

/// Why there's no sealed copy of shipyard's binary, mostly
/// ([`Error::Unsealed`]): the host refuses executable memfds.
#[cfg(target_os = "linux")]
pub(crate) const UNSEALED_POLICY: &str = "vm.memfd_noexec, or a security module's policy on memfds";
/// Why there's no sealed copy: a hard file-size limit (`ulimit -f`, which
/// sets it in the shell) smaller than the binary.
#[cfg(target_os = "linux")]
pub(crate) const UNSEALED_FILE_SIZE: &str = "a file-size limit (ulimit -f) smaller than the binary";

/// Errors produced by the runtime.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Limits were asked for, and the container can't have a cgroup to hold
    /// them.
    #[cfg(target_os = "linux")]
    #[error("resource limits need a cgroup of the container's own, and it can't have one: {why}")]
    LimitsNeedACgroup {
        /// Why not.
        why: std::borrow::Cow<'static, str>,
    },

    /// The container's cgroup couldn't be made, filled or removed.
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Cgroup(#[from] crate::cgroup::CgroupError),

    /// The feature needs Linux kernel support this host doesn't have.
    #[error("{feature} requires a Linux kernel (current host: {os})")]
    LinuxOnly {
        /// What was attempted, e.g. "PID namespaces".
        feature: &'static str,
        /// The host operating system (`std::env::consts::OS`).
        os: &'static str,
    },

    /// Shipyard was started with privileges its caller doesn't have (a
    /// setuid/setgid bit or file capabilities), which would let any user run
    /// commands with elevated privileges.
    #[error(
        "refusing to run: shipyard was started with elevated privileges ({reason}); \
         reinstall it without setuid/setgid bits or file capabilities"
    )]
    PrivilegedStart {
        /// What made the start privileged.
        reason: String,
    },

    /// The process spec is invalid; nothing was started.
    #[error(transparent)]
    InvalidSpec(#[from] SpecError),

    /// The isolation (namespaces, hostname) is invalid; nothing was started.
    #[error(transparent)]
    InvalidIsolation(#[from] IsolationError),

    /// The stop signal can't end a process (it stops or continues it).
    #[error(transparent)]
    StopSignal(#[from] StopSignalError),

    /// An OCI bundle can't be run: its `config.json` is missing, malformed,
    /// or asks for what Shipyard can't do yet.
    #[error(transparent)]
    Bundle(#[from] crate::oci::BundleError),

    /// A user or group name couldn't be resolved.
    #[error(transparent)]
    Lookup(#[from] crate::process::LookupError),

    /// User or group names were given for a command joining a container:
    /// they'd have to be read from the container's files, which isn't
    /// supported yet.
    #[error("user and group names can't be used when joining a container yet; give numbers")]
    NamesWhenJoining,

    /// The container's root filesystem can't be used: the directory is
    /// missing, or isn't a directory.
    #[error("the root filesystem {}: {reason}", path.display())]
    RootFilesystem {
        /// The directory given.
        path: std::path::PathBuf,
        /// What's wrong with it.
        reason: String,
    },

    /// Isolating the command in new namespaces needs privileges: root, or a
    /// user namespace of the caller's own (rootless).
    #[error("isolating the command in namespaces needs root, or a user namespace (rootless)")]
    IsolationNeedsRoot,

    /// `newuidmap` or `newgidmap` couldn't write a rootless container's map.
    #[error("{helper} couldn't map the container's IDs: {message}")]
    MapHelper {
        /// Which helper.
        helper: &'static str,
        /// What it said, or why it couldn't run.
        message: String,
    },

    /// The kernel refused a user namespace to a caller without root.
    #[error("{reason}")]
    UserNamespacesRefused {
        /// Why, from the host's settings.
        reason: String,
    },

    /// Running as root, the system still refused to create namespaces.
    #[error(
        "the system refused to create namespaces: shipyard may be running inside a \
         container without privileges, or under a security policy (seccomp, AppArmor, \
         SELinux); run it with privileges"
    )]
    IsolationRefused,

    /// Isolation was asked for, but this program never called
    /// [`init_entry_point`](crate::process::init_entry_point), so its binary
    /// can't serve as the container's init. A bug in the program, not in
    /// the request.
    #[error(
        "this program can't run isolated commands: it doesn't call \
         shipyard_runtime::process::init_entry_point() at startup"
    )]
    NoInitEntryPoint,

    /// The container's init exited without ever running: the shipyard
    /// binary, re-executed as the init, failed to start.
    #[error(
        "the container's init failed to start (exit code {code}): re-executing shipyard's own \
         binary didn't work; is shipyard started through the dynamic loader (ld.so), or does it \
         need environment variables (like LD_LIBRARY_PATH) to start?"
    )]
    InitDidNotStart {
        /// What the init exited with.
        code: u8,
    },

    /// Supplementary groups asked for where `setgroups` is denied: in a user
    /// namespace that denies it, only the groups a process has stay.
    #[error(
        "supplementary group {gid} can't be set: setgroups is denied in the user namespace \
         shipyard runs in (it was started inside one that denies it, like `unshare -U -r`)"
    )]
    GroupsDenied {
        /// The first group asked for.
        gid: u32,
    },

    /// The container's init couldn't supervise its command: a system call
    /// it needs was refused, so it ended the container.
    #[error(
        "the container's init couldn't supervise its command (wait for it, or pass a signal \
         on), and ended the container: a system call it needs was refused (the init runs \
         under the container's seccomp profile, which must allow rt_sigtimedwait, wait4 and \
         kill)"
    )]
    InitGaveUp,

    /// The container's init was killed by SIGSYS: its seccomp profile
    /// killed it over a call it needs.
    #[error(
        "the container's init was killed by SIGSYS, and the container with it: the init runs \
         under the container's seccomp profile, which must allow rt_sigtimedwait, wait4 and \
         kill"
    )]
    InitKilledBySeccomp,

    /// A process that runs in a container (its init, `what`, or `exec`'s
    /// joiner) can't use a sealed copy of shipyard's binary
    /// (`vm.memfd_noexec` at 2, or a security module's policy on memfds),
    /// and someone in the container could write the binary's file (its
    /// owner, or a group that may write it, is mapped inside; or anyone
    /// may): the container could change the program run next
    /// (CVE-2019-5736).
    #[error(
        "{what} needs a sealed copy of shipyard's binary, which can't be made or run here \
         ({why}): without one, someone in the container could write shipyard's binary file \
         (its owner, or a group that may write it, is mapped inside, or anyone may write \
         it; without a user namespace, root inside is host root)"
    )]
    Unsealed {
        /// What would have run it: "the container's init", "exec".
        what: &'static str,
        /// Why there's no copy: the host's policy on memfds, or a file-size
        /// limit.
        why: &'static str,
    },

    /// A descriptor to pass on (`--preserve-fds`, or socket activation's
    /// `LISTEN_FDS`) isn't open.
    #[error(
        "descriptor {fd} isn't open: --preserve-fds and LISTEN_FDS pass on descriptors 3 to \
         {last}, which must all be open"
    )]
    FdNotOpen {
        /// The one that isn't.
        fd: i32,
        /// The last one asked for.
        last: i32,
    },

    /// More descriptors to pass on than this process may hold (its soft
    /// descriptor limit).
    #[error(
        "{count} descriptors to pass on: at most {max} can be (raise the descriptor limit, \
         `ulimit -n`, to pass more)"
    )]
    TooManyFds {
        /// How many were asked for.
        count: usize,
        /// The most.
        max: usize,
    },

    /// The command's own descriptor limit (its `RLIMIT_NOFILE`, the
    /// bundle's) leaves it no room above the passed descriptors: every
    /// `open` it made would fail.
    #[error(
        "{count} descriptors passed on, and the process's own descriptor limit is {soft}: it \
         needs at least {needed} (raise its RLIMIT_NOFILE)"
    )]
    NofileTooLow {
        /// How many are passed on.
        count: usize,
        /// The process's soft limit.
        soft: u64,
        /// The least it needs.
        needed: u64,
    },

    /// `exec`'s command was held up before it ran (the reason): stopped or
    /// frozen from inside the container, or killed. It's been killed.
    #[error("exec's command never ran: {0}")]
    ExecHeldUp(&'static str),

    /// `exec` or `cp` on a host where a credential change makes a process
    /// dumpable (`fs.suid_dumpable` 1).
    #[error(
        "{what} can't hide its process from the container on this host (fs.suid_dumpable is \
         1): set it to 0 or 2 (sysctl fs.suid_dumpable=2)"
    )]
    UnsafeDumpable {
        /// Which command: `exec` or `cp`.
        what: &'static str,
    },

    /// Without root, the namespaces can't be opened: they're another
    /// user's container's (its user namespace isn't this user's).
    #[error("the container's namespaces aren't this user's to join (another user's container?)")]
    JoinNotYours,

    /// Running as root, the system still refused to let us join the
    /// namespaces.
    #[error(
        "the system refused to join the container's namespaces: shipyard may be running \
         inside a container without privileges, or under a security policy (seccomp, \
         AppArmor, SELinux)"
    )]
    JoinRefused,

    /// A pseudo-terminal (`-t`) for an isolated command needs a mount
    /// namespace of its own: the container's devpts would otherwise cover
    /// the host's.
    #[error("a terminal of the container's own (-t) needs a mount namespace of its own")]
    TerminalNeedsMount,

    /// `/proc` belongs to another PID namespace than shipyard's, so a PID
    /// can't be looked up in it safely.
    #[error(
        "/proc shows another PID namespace's processes than shipyard's own, so the \
         container's process can't be found by its PID; mount a /proc for shipyard's \
         PID namespace"
    )]
    ForeignProc,

    /// The process whose namespaces were to be joined doesn't exist (any
    /// more).
    #[error("no process {pid} to join (has the container exited?)")]
    NoSuchProcess {
        /// The PID asked for, on the host.
        pid: u32,
    },

    /// The container is exiting: its init has ended, so no new process can
    /// start in its PID namespace.
    #[error("can't join process {pid}'s container: it is exiting")]
    ContainerExiting {
        /// The PID asked for, on the host.
        pid: u32,
    },

    /// Running as another user or group, or adding groups, needs root.
    #[error("running as a different user or group needs root: {0}")]
    NeedsRoot(String),

    /// A [`Supervisor`](crate::process::Supervisor) already exists in this
    /// process. It owns the process's signal handlers, so there can be only
    /// one at a time.
    #[error("a supervisor is already running in this process")]
    AlreadySupervising,

    /// The command was started but never got to run.
    #[error(transparent)]
    Spawn(#[from] SpawnError),

    /// A hook failed while the container was set up: it doesn't run.
    #[error(transparent)]
    Hook(#[from] crate::hooks::HookError),

    /// Setting up the container's network failed: what was being done and
    /// why (a netlink request the kernel refused, with its reason).
    #[error("{0}")]
    Network(String),

    /// Joining a namespace that isn't a network one failed (an IPC or UTS
    /// namespace a container joins, or the process that joins another
    /// container's user namespace): what was being done and why.
    #[error("{0}")]
    Namespace(String),

    /// Another container's user namespace, to be joined, has other ID maps
    /// than the container was given (its setup goes by them).
    #[error("process {pid}'s user namespace has other ID maps than the container was given")]
    JoinedMapsDiffer {
        /// The other container's init.
        pid: u32,
    },

    /// A system call failed in shipyard itself. `op` says what we were doing;
    /// the errno is the error's source, so full-chain printing shows both once.
    #[error("{op}")]
    Sys {
        /// What shipyard was doing, in words.
        op: Cow<'static, str>,
        /// The error number the kernel returned.
        #[source]
        source: Errno,
    },
}

impl Error {
    /// The errno of a system error (`None` for any other).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "used by the init and the joiner")
    )]
    pub(crate) fn errno(&self) -> Option<Errno> {
        match self {
            Self::Sys { source, .. } => Some(*source),
            _ => None,
        }
    }

    pub(crate) fn sys(op: impl Into<Cow<'static, str>>, source: Errno) -> Self {
        Self::Sys {
            op: op.into(),
            source,
        }
    }

    /// The exit code that reports this error: 126 or 127 when the command
    /// itself couldn't be executed or found, 125 for everything else.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Spawn(err) => err.exit_code(),
            _ => EXIT_ENGINE_ERROR,
        }
    }
}

/// A `Result` whose error defaults to the runtime's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
