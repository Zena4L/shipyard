use std::borrow::Cow;

use nix::errno::Errno;

use crate::isolation::IsolationError;
use crate::process::{EXIT_ENGINE_ERROR, SpawnError, SpecError, StopSignalError};

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

    /// Joining a running process's namespaces needs privileges (root, until
    /// rootless support with user namespaces arrives).
    #[error("joining a container's namespaces needs root")]
    JoinNeedsRoot,

    /// Running as root, the system still refused to let us join the
    /// namespaces.
    #[error(
        "the system refused to join the container's namespaces: shipyard may be running \
         inside a container without privileges, or under a security policy (seccomp, \
         AppArmor, SELinux)"
    )]
    JoinRefused,

    /// A joined command keeps shipyard's own standard descriptors: there's
    /// no relay for one yet (see `Supervisor::set_stdio`).
    #[error("a command joining a container can't have its standard input and output relayed yet")]
    StdioForJoin,

    /// A pseudo-terminal (`-t`) for an isolated command needs a mount
    /// namespace of its own: the container's devpts would otherwise cover
    /// the host's.
    #[error("a terminal of the container's own (-t) needs a mount namespace of its own")]
    TerminalNeedsMount,

    /// A pseudo-terminal for a container that waits at a start gate
    /// (`shipyard-runtime create`): the init would wait for a relay that
    /// isn't there. (The OCI console socket comes with step 8.3.) A guard:
    /// nothing asks for both yet (`container::create` never passes a
    /// terminal).
    #[error("a terminal can't be relayed for a container that waits to be started")]
    TerminalAtGate,

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
