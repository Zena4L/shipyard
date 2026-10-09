//! Everything the child needs, prepared before `fork`.
//!
//! After `fork` the child may not allocate (another thread could have held the
//! allocator's lock at the moment of the fork). So every string, array and
//! number it uses is built here, in the parent, where allocating is fine.

use std::ffi::{CString, OsStr};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use libc::c_char;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::{Gid, Uid, geteuid, getgid, getuid};

use super::env::DEFAULT_PATH;
use super::failure::FailureContext;
#[cfg(target_os = "linux")]
use super::failure::FilterStart;
use super::rlimit::{Rlimit, RlimitError, RlimitKind, RlimitValue};
use super::spawn::SpawnOptions;
use super::spec::{ProcessSpec, SpecError, User};
#[cfg(target_os = "linux")]
use super::user_namespace::setgroups_allowed;
#[cfg(target_os = "linux")]
use crate::security::Confinement;
use crate::{Error, Result};

/// The default file mode creation mask (Docker and runc use the same).
const DEFAULT_UMASK: libc::mode_t = 0o022;

/// What the child runs and how. Built once per spawn; read-only afterwards.
pub(crate) struct ChildPlan {
    pub(crate) argv: CStringArray,
    pub(crate) envp: CStringArray,
    /// Full paths to try with `execve`, in order.
    pub(crate) candidates: Vec<CString>,
    pub(crate) cwd: CString,
    /// Set only when running as root; then the child always applies it.
    pub(crate) credentials: Option<Credentials>,
    pub(crate) umask: libc::mode_t,
    pub(crate) rlimits: Vec<RawLimit>,
    pub(crate) inherited_fds: FdSweep,
    /// Whether the child starts its own process group.
    pub(crate) new_process_group: bool,
    /// Signals that stay ignored in the command; every other one is reset to
    /// its default (see `SpawnOptions::keep_ignored`).
    pub(crate) keep_ignored: Vec<libc::c_int>,
    /// The setup handshake with shipyard (see `channel.rs`). `None` for the
    /// command a container's init starts, which has no handshake.
    pub(crate) handshake: Option<Handshake>,
    /// Isolated: how the cloned child re-execs into the container's clean
    /// image, which does the rest (see `init.rs`).
    #[cfg(target_os = "linux")]
    pub(crate) reexec: Option<Reexec>,
    /// The namespaces to join, if joining a running process's.
    #[cfg(target_os = "linux")]
    pub(crate) join: Option<JoinSetup>,
    /// The command a container's clean image becomes without a PID
    /// namespace: shipyard's PID, its parent (see `die_with_parent`).
    #[cfg(target_os = "linux")]
    pub(crate) parent: Option<libc::pid_t>,
    /// A container's command: its capabilities and `no_new_privs`, applied
    /// around the credential switch (see `child.rs`). `None` otherwise.
    #[cfg(target_os = "linux")]
    pub(crate) confinement: Option<Confinement>,
}

/// The namespaces the child joins (see `join.rs`), in order. The PID
/// namespace isn't here: shipyard sets it up around the fork.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct JoinSetup {
    pub(crate) namespaces: Vec<JoinedNamespace>,
}

/// One namespace for the child to join.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct JoinedNamespace {
    /// A descriptor for it, opened before the fork.
    pub(crate) fd: RawFd,
    /// Its `CLONE_NEW*` flag, for `setns` to check the kind.
    pub(crate) flag: libc::c_int,
    /// Its index in `Namespace::ALL`, to name it in a failure report.
    pub(crate) index: u64,
}

/// The child's view of the go pipe.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Handshake {
    /// The end the child reads "go" from, and later watches for the parent's
    /// end closing.
    pub(crate) read: RawFd,
    /// The parent's end. The child inherits a copy and closes it at once:
    /// only then does the pipe report the parent's end closing.
    pub(crate) parent_end: RawFd,
}

/// What the cloned child needs to re-exec into the container's clean image
/// (see `init.rs`): built here, so the child only makes system calls.
#[cfg(target_os = "linux")]
pub(crate) struct Reexec {
    /// The binary to exec, opened before the clone: a sealed copy of
    /// shipyard's (see `spawn.rs`).
    pub(crate) exe: RawFd,
    /// `/proc/self/fd/<exe>`: the same binary as a path, for when `execveat`
    /// isn't available.
    pub(crate) exe_path: CString,
    /// Shipyard's binary file (and its `/proc/self/fd` path), when `exe` is
    /// a copy: tried only if the copy won't run (a security module's policy
    /// on memfds, say). Close-on-exec, so the init never holds it.
    pub(crate) host_exe: Option<(RawFd, CString)>,
    /// The clean image's arguments: the hidden entry point and the passed
    /// descriptors' numbers.
    pub(crate) argv: CStringArray,
    /// The same, plus [`SIGPIPE_PENDING_ARG`](super::SIGPIPE_PENDING_ARG):
    /// used when a SIGPIPE is pending at the exec.
    pub(crate) argv_sigpipe: CStringArray,
    /// Its environment: empty.
    pub(crate) env: CStringArray,
    /// The descriptors the clean image inherits (setup, status, report, and
    /// the gate and hooks pipe if there are):
    /// their close-on-exec flag is cleared just before the exec.
    pub(crate) passed: Vec<RawFd>,
    /// Every other descriptor from 3 up is closed before the exec: these
    /// ranges (for `close_range`), everything but `passed`, `exe` and
    /// `host_exe`.
    pub(crate) close_ranges: Vec<(u32, u32)>,
    /// In a new user namespace: become its root (uid and gid 0 inside)
    /// before the exec, or the exec would leave the clean image without
    /// capabilities (see `child.rs`).
    pub(crate) namespace_root: bool,
}

/// The descriptors an isolated spawn opens for the re-exec (see `Reexec`).
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux isolates")
)]
pub(crate) struct ReexecFds {
    pub(crate) setup: RawFd,
    pub(crate) status: RawFd,
    pub(crate) report: RawFd,
    pub(crate) exe: RawFd,
    /// Shipyard's binary file, when `exe` is a copy of it.
    pub(crate) host_exe: Option<RawFd>,
    /// The start gate (`SpawnOptions::gate`), passed on too.
    pub(crate) gate: Option<RawFd>,
    /// The hooks pipe's read end, if the setup stops for hooks: passed on
    /// too.
    pub(crate) hooks: Option<RawFd>,
    /// An overlay real root made: passed on too.
    pub(crate) overlay: Option<RawFd>,
}

/// The user and groups to switch to.
#[derive(Debug)]
pub(crate) struct Credentials {
    pub(crate) uid: Uid,
    pub(crate) gid: Gid,
    /// In the layout `setgroups` needs on this platform (see [`group_list`]).
    /// Raw `gid_t`s, since that's what `setgroups` reads. `None`: leave the
    /// list as it is (in a user namespace that denies `setgroups`).
    pub(crate) groups: Option<Vec<libc::gid_t>>,
}

/// Whether a plan switches the command's user and groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux isolates, and has user namespaces")
)]
pub(crate) enum UserSwitch {
    /// Not at all: an isolated run's parent-side plan, whose child re-execs
    /// (the command's credentials are worked out inside the container).
    None,
    /// User, group and supplementary groups (as root).
    Full,
    /// User and group, but not the supplementary groups: a user namespace
    /// that denies `setgroups`. None may be asked for.
    WithoutGroups,
}

#[cfg(target_os = "linux")]
impl UserSwitch {
    /// The whole switch where the user namespace allows `setgroups`, and
    /// without the groups where it denies it.
    fn as_namespace_allows(setgroups_allowed: bool) -> Self {
        if setgroups_allowed {
            Self::Full
        } else {
            Self::WithoutGroups
        }
    }
}

/// The C type of a resource number: glibc declares its own enum type.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) type RawResource = libc::__rlimit_resource_t;
/// The C type of a resource number.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) type RawResource = libc::c_int;

/// One resource limit, in the kernel's terms.
pub(crate) struct RawLimit {
    pub(crate) resource: RawResource,
    pub(crate) soft: libc::rlim_t,
    pub(crate) hard: libc::rlim_t,
    /// A soft limit to retry with if the kernel rejects `soft` as invalid.
    pub(crate) fallback_soft: Option<libc::rlim_t>,
}

/// Which inherited descriptors the child must mark close-on-exec.
pub(crate) enum FdSweep {
    /// The descriptors (3 and up) that were open when the plan was built.
    Listed(Vec<RawFd>),
    /// `/dev/fd` couldn't be read completely: every descriptor from 3 below
    /// this bound.
    Below(RawFd),
}

/// C strings plus the NULL-terminated pointer array that `execve` takes.
///
/// The pointers point into the strings' heap buffers, which don't move when
/// the `Vec` holding them moves, so they stay valid as long as `self` lives.
pub(crate) struct CStringArray {
    _strings: Vec<CString>,
    pointers: Vec<*const c_char>,
}

impl CStringArray {
    fn new(strings: Vec<CString>) -> Self {
        let pointers = strings
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Self {
            _strings: strings,
            pointers,
        }
    }

    pub(crate) fn as_ptr(&self) -> *const *const c_char {
        self.pointers.as_ptr()
    }
}

impl ChildPlan {
    /// Builds the plan, plus what the parent needs to explain a failure.
    ///
    /// Build it right before `fork`: it lists the descriptors open *now*.
    pub(crate) fn new(
        spec: &ProcessSpec,
        options: &SpawnOptions,
        handshake: Handshake,
        #[cfg_attr(
            not(target_os = "linux"),
            allow(unused_variables, reason = "only Linux isolates")
        )]
        reexec: Option<ReexecFds>,
        #[cfg(target_os = "linux")] join: Option<JoinSetup>,
    ) -> Result<(Self, FailureContext)> {
        // An isolated run's child re-execs; the command's credentials are
        // worked out inside, where its user is.
        #[cfg(target_os = "linux")]
        let switch = if options.placement.isolation().is_some() {
            UserSwitch::None
        } else {
            // Shipyard may itself run in a user namespace that denies
            // `setgroups` (started inside `unshare -U -r`, say).
            UserSwitch::as_namespace_allows(setgroups_allowed())
        };
        #[cfg(not(target_os = "linux"))]
        let switch = UserSwitch::Full;
        let (plan, context) = Self::for_command(
            spec,
            options.keep_ignored.clone(),
            options.new_process_group,
            Some(handshake),
            switch,
        )?;
        #[cfg(target_os = "linux")]
        let (plan, context) = (
            Self {
                reexec: reexec
                    .map(|fds| {
                        let namespace_root = options
                            .placement
                            .isolation()
                            .is_some_and(|isolation| isolation.id_maps.is_some());
                        reexec_plan(fds, namespace_root)
                    })
                    .transpose()?,
                join,
                ..plan
            },
            FailureContext {
                container_root: options
                    .placement
                    .isolation()
                    .and_then(|isolation| isolation.root.clone()),
                sysctls: options
                    .placement
                    .isolation()
                    .map(|isolation| isolation.sysctls.clone())
                    .unwrap_or_default(),
                seccomp: options
                    .placement
                    .isolation()
                    .and_then(|isolation| FilterStart::of(&isolation.security)),
                namespace_root: options
                    .placement
                    .isolation()
                    .and_then(|isolation| isolation.id_maps.as_ref())
                    .and_then(|maps| maps.host_uid(0)),
                pids_max: options
                    .placement
                    .isolation()
                    .and_then(|isolation| isolation.resources.pids_max),
                hooks: options
                    .placement
                    .isolation()
                    .and_then(|isolation| isolation.hooks.as_ref())
                    .map(|container| container.hooks.clone())
                    .unwrap_or_default(),
                devices: options
                    .placement
                    .isolation()
                    .map(|isolation| isolation.devices.nodes.clone())
                    .unwrap_or_default(),
                ..context
            },
        );
        Ok((plan, context))
    }

    /// The plan for the command a container's clean init starts: no
    /// handshake, no namespaces (it's already inside them). `parent` is
    /// shipyard's PID when the command will be its child (no PID namespace).
    /// Without `setgroups_allowed` (a user namespace that denies it), the
    /// command keeps the group list it has, and may not ask for others.
    #[cfg(target_os = "linux")]
    pub(crate) fn for_container_command(
        spec: &ProcessSpec,
        keep_ignored: Vec<libc::c_int>,
        parent: Option<libc::pid_t>,
        confinement: Confinement,
        setgroups_allowed: bool,
    ) -> Result<(Self, FailureContext)> {
        let switch = UserSwitch::as_namespace_allows(setgroups_allowed);
        let (plan, context) = Self::for_command(spec, keep_ignored, false, None, switch)?;
        let seccomp = if confinement.filter_before_setup().is_some() {
            Some(FilterStart::BeforeSetup)
        } else {
            confinement
                .filter_before_exec()
                .map(|_| FilterStart::BeforeExec)
        };
        Ok((
            Self {
                parent,
                confinement: Some(confinement),
                ..plan
            },
            FailureContext { seccomp, ..context },
        ))
    }

    fn for_command(
        spec: &ProcessSpec,
        keep_ignored: Vec<libc::c_int>,
        new_process_group: bool,
        handshake: Option<Handshake>,
        switch: UserSwitch,
    ) -> Result<(Self, FailureContext)> {
        spec.validate()?;
        let credentials = match switch {
            UserSwitch::None => None,
            UserSwitch::Full => credentials_for(&spec.user)?,
            UserSwitch::WithoutGroups => {
                // First what the caller may do at all: without root, adding
                // groups needs root before it needs `setgroups`.
                let credentials = credentials_for(&spec.user)?;
                if let Some(&gid) = spec.user.additional_gids.first() {
                    return Err(Error::GroupsDenied { gid });
                }
                credentials.map(|credentials| Credentials {
                    groups: None,
                    ..credentials
                })
            }
        };

        let program = spec.args.first().ok_or(SpecError::EmptyArgs)?;
        let path_var = spec
            .env
            .get(OsStr::new("PATH"))
            .unwrap_or(OsStr::new(DEFAULT_PATH));
        let candidates = exec_candidates(program, path_var);

        let argv = spec
            .args
            .iter()
            .map(|arg| c_string(arg.as_bytes(), "an argument"))
            .collect::<Result<_, _>>()?;
        let envp = spec
            .env
            .iter()
            .map(|(key, value)| {
                let entry = [key.as_bytes(), b"=", value.as_bytes()].concat();
                c_string(&entry, "an environment variable")
            })
            .collect::<Result<_, _>>()?;

        let plan = Self {
            argv: CStringArray::new(argv),
            envp: CStringArray::new(envp),
            candidates: candidates
                .iter()
                .map(|c| c_string(c, "the program path"))
                .collect::<Result<_, _>>()?,
            cwd: c_string(spec.cwd.as_os_str().as_bytes(), "the working directory")?,
            credentials,
            umask: match spec.user.umask {
                None => DEFAULT_UMASK,
                // `validate` already rejected anything above 0o777.
                Some(mask) => {
                    libc::mode_t::try_from(mask).map_err(|_| SpecError::UmaskOutOfRange(mask))?
                }
            },
            rlimits: spec
                .rlimits
                .iter()
                .map(raw_limit)
                .collect::<Result<_, _>>()?,
            inherited_fds: inherited_fds(),
            new_process_group,
            keep_ignored,
            handshake,
            #[cfg(target_os = "linux")]
            reexec: None,
            #[cfg(target_os = "linux")]
            join: None,
            #[cfg(target_os = "linux")]
            parent: None,
            #[cfg(target_os = "linux")]
            confinement: None,
        };
        let context = FailureContext {
            program: PathBuf::from(program),
            candidates: candidates
                .into_iter()
                .map(|c| PathBuf::from(OsStr::from_bytes(&c)))
                .collect(),
            cwd: spec.cwd.clone(),
            uid: spec.user.uid,
            gid: spec.user.gid,
            rlimits: spec.rlimits.clone(),
            root: geteuid().is_root(),
            container_root: None,
            names: spec.user.names.clone(),
            sysctls: Vec::new(),
            seccomp: None,
            namespace_root: None,
            pids_max: None,
            hooks: crate::hooks::Hooks::default(),
            devices: Vec::new(),
        };
        Ok((plan, context))
    }
}

/// The re-exec plan for the descriptors in `fds`.
#[cfg(target_os = "linux")]
fn reexec_plan(fds: ReexecFds, namespace_root: bool) -> Result<Reexec> {
    let number = |fd: RawFd, field| c_string(fd.to_string().as_bytes(), field);
    let argv = vec![
        // What `ps -f` shows for PID 1 (its name is set too).
        c"shipyard-init".to_owned(),
        CString::from(super::INIT_ENTRY_ARG),
        number(fds.setup, "the setup")?,
        number(fds.status, "the status pipe")?,
        number(fds.report, "the report pipe")?,
    ];
    let passed: Vec<RawFd> = [fds.setup, fds.status, fds.report]
        .into_iter()
        .chain(fds.gate)
        .chain(fds.hooks)
        .chain(fds.overlay)
        .collect();
    let kept: Vec<u32> = passed
        .iter()
        .chain([&fds.exe])
        .chain(fds.host_exe.as_ref())
        .filter_map(|&fd| u32::try_from(fd).ok())
        .collect();
    let path = |fd: RawFd| c_string(format!("/proc/self/fd/{fd}").as_bytes(), "the binary");
    Ok(Reexec {
        exe: fds.exe,
        exe_path: path(fds.exe)?,
        host_exe: fds
            .host_exe
            .map(|fd| Ok::<_, Error>((fd, path(fd)?)))
            .transpose()?,
        argv: CStringArray::new(argv.clone()),
        argv_sigpipe: CStringArray::new(
            argv.into_iter()
                .chain([CString::from(super::SIGPIPE_PENDING_ARG)])
                .collect(),
        ),
        env: CStringArray::new(Vec::new()),
        passed,
        close_ranges: ranges_excluding(&kept),
        namespace_root,
    })
}

/// The descriptor ranges from 3 to the maximum that leave out `kept`, for
/// `close_range`.
#[cfg(any(target_os = "linux", test))]
fn ranges_excluding(kept: &[u32]) -> Vec<(u32, u32)> {
    let mut kept: Vec<u32> = kept.iter().copied().filter(|&fd| fd >= 3).collect();
    kept.sort_unstable();
    kept.dedup();
    let mut ranges = Vec::new();
    let mut first = 3_u32;
    for fd in kept {
        if fd > first {
            ranges.push((first, fd - 1));
        }
        match fd.checked_add(1) {
            Some(next) => first = next,
            None => return ranges,
        }
    }
    ranges.push((first, u32::MAX));
    ranges
}

fn c_string(bytes: &[u8], field: &'static str) -> Result<CString, SpecError> {
    CString::new(bytes).map_err(|_| SpecError::ContainsNul { field })
}

/// The paths `execve` should try, like `execvp` does (TLPI §27.2.1).
///
/// A program containing `/` is used as given (relative to the working
/// directory). Otherwise each `PATH` entry is tried in order; an empty entry
/// means the working directory, as POSIX specifies.
fn exec_candidates(program: &OsStr, path_var: &OsStr) -> Vec<Vec<u8>> {
    let program = program.as_bytes();
    if program.contains(&b'/') {
        return vec![program.to_vec()];
    }
    path_var
        .as_bytes()
        .split(|&b| b == b':')
        .map(|dir| {
            let dir = if dir.is_empty() { &b"."[..] } else { dir };
            let mut full = dir.to_vec();
            if !full.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(program);
            full
        })
        .collect()
}

/// Credentials to apply in the child, or `None` to keep shipyard's own.
///
/// Only root can switch user or group. As anyone else, asking for a different
/// user or group fails here, before anything starts, with a message naming
/// exactly what differs.
fn credentials_for(user: &User) -> Result<Option<Credentials>> {
    if geteuid().is_root() {
        return Ok(Some(Credentials {
            uid: Uid::from_raw(user.uid),
            gid: Gid::from_raw(user.gid),
            groups: Some(group_list(user.gid, &user.additional_gids)),
        }));
    }
    let (my_uid, my_gid) = (getuid().as_raw(), getgid().as_raw());
    let problem = if user.uid != my_uid {
        Some(format!("user {} differs from your user {my_uid}", user.uid))
    } else if user.gid != my_gid {
        Some(format!(
            "group {} differs from your group {my_gid} (use --user {}:{my_gid} to keep it)",
            user.gid, user.uid
        ))
    } else if !user.additional_gids.is_empty() {
        Some("adding supplementary groups".to_owned())
    } else {
        None
    };
    problem.map_or(Ok(None), |problem| Err(Error::NeedsRoot(problem)))
}

/// The list to pass to `setgroups`.
///
/// On macOS the kernel keeps the effective group in the same list (as entry
/// 0), so `setgid` would overwrite whatever is there. Putting the primary group
/// first, and not repeating it, keeps every supplementary group intact.
fn group_list(gid: u32, additional: &[u32]) -> Vec<libc::gid_t> {
    let extras = additional.iter().copied();
    if cfg!(target_os = "linux") {
        extras.collect()
    } else {
        std::iter::once(gid)
            .chain(extras.filter(|g| *g != gid))
            .collect()
    }
}

fn raw_limit(limit: &Rlimit) -> Result<RawLimit, SpecError> {
    let resource = resource_of(limit.kind).ok_or_else(|| RlimitError::unsupported(limit.kind))?;
    let raw = |value| match value {
        RlimitValue::Finite(n) => n,
        RlimitValue::Unlimited => libc::RLIM_INFINITY,
    };
    let soft = raw(limit.soft);
    let hard = raw(limit.hard);
    // Older macOS versions reject an unlimited soft NOFILE (EINVAL); newer
    // ones accept it. So the child tries unlimited first and only then falls
    // back to "as high as this system allows".
    let fallback_soft = (limit.kind == RlimitKind::Nofile && limit.soft == RlimitValue::Unlimited)
        .then(max_open_files_per_process)
        .flatten();
    Ok(RawLimit {
        resource,
        soft,
        hard,
        fallback_soft,
    })
}

/// The kernel's number for a resource, if this platform has it. libc's
/// constants, not nix's enum: nix leaves out `MEMLOCK` and `NPROC` on macOS,
/// though macOS has both.
fn resource_of(kind: RlimitKind) -> Option<RawResource> {
    match kind {
        RlimitKind::As => Some(libc::RLIMIT_AS),
        RlimitKind::Core => Some(libc::RLIMIT_CORE),
        RlimitKind::Cpu => Some(libc::RLIMIT_CPU),
        RlimitKind::Data => Some(libc::RLIMIT_DATA),
        RlimitKind::Fsize => Some(libc::RLIMIT_FSIZE),
        RlimitKind::Memlock => Some(libc::RLIMIT_MEMLOCK),
        RlimitKind::Nofile => Some(libc::RLIMIT_NOFILE),
        RlimitKind::Nproc => Some(libc::RLIMIT_NPROC),
        RlimitKind::Stack => Some(libc::RLIMIT_STACK),
        // Listed, not `_`: a new kind must be placed on purpose.
        RlimitKind::Locks
        | RlimitKind::Msgqueue
        | RlimitKind::Nice
        | RlimitKind::Rss
        | RlimitKind::Rtprio
        | RlimitKind::Rttime
        | RlimitKind::Sigpending => linux_only_resource(kind),
    }
}

/// The resources only Linux has.
#[cfg(target_os = "linux")]
fn linux_only_resource(kind: RlimitKind) -> Option<RawResource> {
    match kind {
        RlimitKind::Locks => Some(libc::RLIMIT_LOCKS),
        RlimitKind::Msgqueue => Some(libc::RLIMIT_MSGQUEUE),
        RlimitKind::Nice => Some(libc::RLIMIT_NICE),
        RlimitKind::Rss => Some(libc::RLIMIT_RSS),
        RlimitKind::Rtprio => Some(libc::RLIMIT_RTPRIO),
        RlimitKind::Rttime => Some(libc::RLIMIT_RTTIME),
        RlimitKind::Sigpending => Some(libc::RLIMIT_SIGPENDING),
        // The kinds every platform has are handled by `resource_of`.
        _ => None,
    }
}

/// The resources only Linux has: none here.
#[cfg(not(target_os = "linux"))]
fn linux_only_resource(_kind: RlimitKind) -> Option<RawResource> {
    None
}

/// `kern.maxfilesperproc`: the most descriptors macOS lets one process have.
#[cfg(target_os = "macos")]
fn max_open_files_per_process() -> Option<libc::rlim_t> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>();
    // SAFETY: `value` and `len` describe a valid, writable c_int buffer, the
    // name is a NUL-terminated string, and no new value is being set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    libc::rlim_t::try_from(value)
        .ok()
        .filter(|max| rc == 0 && *max > 0)
}

/// Linux accepts any NOFILE up to `fs.nr_open`, so there's nothing to retry.
#[cfg(not(target_os = "macos"))]
fn max_open_files_per_process() -> Option<libc::rlim_t> {
    None
}

/// The descriptors (3 and up) open right now, which the child must not leak
/// into the command.
///
/// On Linux the child marks them all with one `close_range` call, so the list
/// is only a fallback; it isn't built when `close_range` is known to work
/// here (the child runs under the same kernel and seccomp filter).
fn inherited_fds() -> FdSweep {
    #[cfg(target_os = "linux")]
    if close_range_works() {
        return FdSweep::Below(sweep_bound());
    }
    listed_fds().unwrap_or_else(|| FdSweep::Below(sweep_bound()))
}

/// Whether `close_range(CLOSE_RANGE_CLOEXEC)` works in this process. Tried
/// once, on a range no descriptor can be in (above `RawFd::MAX`), so it
/// changes nothing.
#[cfg(target_os = "linux")]
fn close_range_works() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WORKS.get_or_init(|| {
        let above_any_fd = RawFd::MAX.unsigned_abs().saturating_add(1);
        // SAFETY: `close_range` only changes descriptor flags, and no
        // descriptor can be in this range.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                above_any_fd,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        rc == 0
    })
}

/// The list from `/dev/fd`, or `None` if any part of it couldn't be read (an
/// unusual chroot without `/dev/fd`, or an I/O error): a partial list could
/// miss a descriptor.
fn listed_fds() -> Option<FdSweep> {
    let mut fds = Vec::new();
    for entry in std::fs::read_dir("/dev/fd").ok()? {
        let fd: RawFd = entry.ok()?.file_name().to_str()?.parse().ok()?;
        if fd >= 3 {
            fds.push(fd);
        }
    }
    Some(FdSweep::Listed(fds))
}

/// How far a blind sweep goes. The *hard* limit, not the soft one: a
/// descriptor opened before the soft limit was lowered can sit above it.
/// Capped, so an unlimited hard limit doesn't mean billions of calls; on
/// macOS by `kern.maxfilesperproc`, which no descriptor number can reach.
fn sweep_bound() -> RawFd {
    const CAP: libc::rlim_t = 1 << 20;
    let cap = max_open_files_per_process().map_or(CAP, |max| max.min(CAP));
    let hard = getrlimit(Resource::RLIMIT_NOFILE).map_or(cap, |(_, hard)| hard.min(cap));
    RawFd::try_from(hard.max(4)).unwrap_or(RawFd::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_leave_out_only_the_kept_descriptors() {
        assert_eq!(ranges_excluding(&[5, 10]), [(3, 4), (6, 9), (11, u32::MAX)]);
        // Next to each other, next to fd 3, unsorted, repeated: no empty
        // ranges, and stdio (below 3) is never in one.
        assert_eq!(ranges_excluding(&[4, 3, 4, 1]), [(5, u32::MAX)]);
        assert_eq!(ranges_excluding(&[]), [(3, u32::MAX)]);
        assert_eq!(
            ranges_excluding(&[4, u32::MAX]),
            [(3, 3), (5, u32::MAX - 1)]
        );
    }

    fn candidates(program: &str, path: &str) -> Vec<String> {
        exec_candidates(OsStr::new(program), OsStr::new(path))
            .into_iter()
            .map(|c| String::from_utf8(c).unwrap())
            .collect()
    }

    #[test]
    fn a_program_with_a_slash_is_used_as_given() {
        assert_eq!(candidates("/bin/ls", "/a:/b"), ["/bin/ls"]);
        assert_eq!(candidates("./run.sh", "/a"), ["./run.sh"]);
        assert_eq!(candidates("bin/tool", "/a"), ["bin/tool"]);
    }

    #[test]
    fn path_entries_are_tried_in_order() {
        assert_eq!(
            candidates("ls", "/usr/bin:/bin"),
            ["/usr/bin/ls", "/bin/ls"]
        );
    }

    #[test]
    fn empty_entries_mean_the_working_directory() {
        assert_eq!(candidates("ls", ":/bin:"), ["./ls", "/bin/ls", "./ls"]);
    }

    #[test]
    fn trailing_slashes_are_not_doubled() {
        assert_eq!(candidates("ls", "/usr/bin/"), ["/usr/bin/ls"]);
    }

    #[test]
    fn group_list_layout_matches_the_platform() {
        let groups = group_list(1000, &[20, 1000, 30]);
        if cfg!(target_os = "linux") {
            assert_eq!(groups, [20, 1000, 30]);
        } else {
            // Primary group first, and not repeated.
            assert_eq!(groups, [1000, 20, 30]);
        }
    }

    #[test]
    fn unprivileged_users_cannot_switch_identity() {
        if geteuid().is_root() {
            return;
        }
        let me = User::current();
        assert!(credentials_for(&me).unwrap().is_none());

        let other_user = User {
            uid: me.uid.wrapping_add(1),
            ..me.clone()
        };
        let err = credentials_for(&other_user).unwrap_err().to_string();
        assert!(
            err.contains("needs root") && err.contains("differs from your user"),
            "{err}"
        );

        let other_group = User {
            gid: me.gid.wrapping_add(1),
            ..me.clone()
        };
        let err = credentials_for(&other_group).unwrap_err().to_string();
        assert!(err.contains("differs from your group"), "{err}");

        let extra_groups = User {
            additional_gids: vec![me.gid],
            ..me
        };
        assert!(credentials_for(&extra_groups).is_err());
    }

    /// Only macOS needs a fallback for an unlimited `nofile` (Linux refuses
    /// one when it's parsed).
    #[cfg(target_os = "macos")]
    #[test]
    fn unlimited_nofile_has_a_fallback_on_macos() {
        let limit = raw_limit(&"nofile=unlimited".parse().unwrap()).unwrap();
        assert_eq!(limit.soft, libc::RLIM_INFINITY);
        assert!(limit.fallback_soft.is_some());
    }

    #[test]
    fn finite_limits_have_no_fallback() {
        let finite = raw_limit(&"nofile=100:200".parse().unwrap()).unwrap();
        assert_eq!(finite.fallback_soft, None);
    }

    #[test]
    fn open_descriptors_are_found() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        match inherited_fds() {
            FdSweep::Listed(fds) => {
                assert!(fds.iter().all(|fd| *fd >= 3));
                assert!(fds.contains(&fd), "{fd} missing from {fds:?}");
            }
            FdSweep::Below(bound) => assert!(bound > fd),
        }
    }

    #[test]
    fn the_blind_sweep_is_bounded_by_the_hard_limit_and_capped() {
        let bound = sweep_bound();
        let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        assert!(bound > 3, "{bound}");
        assert!(bound <= 1 << 20, "capped: {bound}");
        if hard < 1 << 20 && max_open_files_per_process().is_none() {
            assert_eq!(libc::rlim_t::try_from(bound).unwrap(), hard.max(4));
        }
    }
}
