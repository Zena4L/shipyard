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
use super::rlimit::{Rlimit, RlimitError, RlimitKind, RlimitValue};
use super::spawn::SpawnOptions;
use super::spec::{ProcessSpec, SpecError, User};
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
    /// The setup handshake (see `channel.rs`).
    pub(crate) handshake: Handshake,
    /// The container setup to do inside new namespaces, if isolated.
    #[cfg(target_os = "linux")]
    pub(crate) container: Option<ContainerSetup>,
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

/// What the child sets up in its new namespaces, before switching user.
#[cfg(target_os = "linux")]
pub(crate) struct ContainerSetup {
    /// Remount `/` recursively private: in a new mount namespace, first.
    pub(crate) private_mounts: bool,
    /// Mount a fresh `/proc` for the new PID namespace.
    pub(crate) mount_proc: bool,
    pub(crate) hostname: Option<CString>,
}

/// The user and groups to switch to.
#[derive(Debug)]
pub(crate) struct Credentials {
    pub(crate) uid: Uid,
    pub(crate) gid: Gid,
    /// In the layout `setgroups` needs on this platform (see [`group_list`]).
    /// Raw `gid_t`s, since that's what `setgroups` reads.
    pub(crate) groups: Vec<libc::gid_t>,
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
    ) -> Result<(Self, FailureContext)> {
        spec.validate()?;
        let credentials = credentials_for(&spec.user)?;

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
            new_process_group: options.new_process_group,
            keep_ignored: options.keep_ignored.clone(),
            handshake,
            #[cfg(target_os = "linux")]
            container: options
                .isolation
                .as_ref()
                .map(container_setup)
                .transpose()?,
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
        };
        Ok((plan, context))
    }
}

/// The setup steps an isolation needs. `validate` already guaranteed that a
/// PID namespace comes with a mount namespace, and a hostname with a UTS one.
#[cfg(target_os = "linux")]
fn container_setup(isolation: &crate::isolation::Isolation) -> Result<ContainerSetup> {
    use crate::isolation::Namespace;
    isolation.validate()?;
    Ok(ContainerSetup {
        private_mounts: isolation.has(Namespace::Mount),
        mount_proc: isolation.has(Namespace::Pid) && isolation.has(Namespace::Mount),
        hostname: isolation
            .hostname
            .as_deref()
            .map(|name| c_string(name.as_bytes(), "the hostname"))
            .transpose()?,
    })
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
            groups: group_list(user.gid, &user.additional_gids),
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

/// The kernel's number for a resource. libc's constants, not nix's enum: nix
/// leaves out `MEMLOCK` and `NPROC` on macOS, though macOS has both.
#[cfg_attr(
    target_os = "linux",
    allow(
        clippy::unnecessary_wraps,
        reason = "every kind exists on Linux, but not on macOS"
    )
)]
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
        #[cfg(target_os = "linux")]
        RlimitKind::Locks => Some(libc::RLIMIT_LOCKS),
        #[cfg(target_os = "linux")]
        RlimitKind::Msgqueue => Some(libc::RLIMIT_MSGQUEUE),
        #[cfg(target_os = "linux")]
        RlimitKind::Nice => Some(libc::RLIMIT_NICE),
        #[cfg(target_os = "linux")]
        RlimitKind::Rss => Some(libc::RLIMIT_RSS),
        #[cfg(target_os = "linux")]
        RlimitKind::Rtprio => Some(libc::RLIMIT_RTPRIO),
        #[cfg(target_os = "linux")]
        RlimitKind::Rttime => Some(libc::RLIMIT_RTTIME),
        #[cfg(target_os = "linux")]
        RlimitKind::Sigpending => Some(libc::RLIMIT_SIGPENDING),
        #[cfg(not(target_os = "linux"))]
        _ => None,
    }
}

/// `kern.maxfilesperproc`: the most descriptors macOS lets one process have.
#[cfg(target_os = "macos")]
fn max_open_files_per_process() -> Option<libc::rlim_t> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
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
