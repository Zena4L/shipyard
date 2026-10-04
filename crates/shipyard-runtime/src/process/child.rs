//! The child's side of a spawn: everything between `fork` and `exec`.
//!
//! This code runs in a copy of the parent made by `fork`. If the parent had
//! other threads, they don't exist here, but any lock they held (the memory
//! allocator's, stdio's) stays locked forever. So until `exec` this code must:
//!
//! - never allocate, never panic, never take a lock;
//! - call only async-signal-safe functions (POSIX lists them; TLPI §21.1.2),
//!   or nix wrappers that are thin calls to them;
//! - report failures as raw numbers over the report pipe, then `_exit`.
//!
//! The lints below turn the most likely panics into compile errors.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::os::fd::RawFd;

use nix::errno::Errno;
use nix::sys::signal::{SigSet, SigmaskHow, sigprocmask};
use nix::sys::stat::{Mode, umask};
use nix::unistd::{Gid, Uid};

use super::channel::encode;
use super::failure::ChildStep;
use super::plan::{ChildPlan, Credentials, FdSweep};

/// The child's exit code when setup fails. The parent normally reads the
/// failure from the report pipe instead; this is only what's left if it can't.
const SETUP_FAILED: libc::c_int = 125;

/// The highest signal number to reset: Linux's real-time signals end at 64;
/// macOS's last signal is 31.
#[cfg(target_os = "linux")]
const MAX_SIGNAL: libc::c_int = 64;
#[cfg(not(target_os = "linux"))]
const MAX_SIGNAL: libc::c_int = 31;

/// A failed setup step, as it travels to the parent.
struct Failure {
    step: ChildStep,
    errno: Errno,
    detail: u64,
}

impl Failure {
    fn at(step: ChildStep, errno: Errno) -> Self {
        Self {
            step,
            errno,
            detail: 0,
        }
    }
}

/// If anything unwinds (it shouldn't: see the lints above), exit instead of
/// running the parent's code in this copy of its process.
struct ExitOnUnwind;

impl Drop for ExitOnUnwind {
    fn drop(&mut self) {
        // SAFETY: `_exit` is async-signal-safe and ends the process without
        // running destructors or atexit handlers that belong to the parent.
        unsafe { libc::_exit(SETUP_FAILED) }
    }
}

/// Sets the process up as `plan` describes and `exec`s the command. Never
/// returns: either `exec` replaces this process, or the failure is written to
/// `report` and the process exits.
///
/// # Safety
///
/// Must be called only in the child right after `fork`, with all signals
/// blocked, and with `plan` and `report` prepared by the parent.
pub(crate) unsafe fn child_main(plan: &ChildPlan, report: RawFd) -> ! {
    let _guard = ExitOnUnwind;
    let Err(failure) = set_up_and_exec(plan);
    report_failure(report, &failure);
    // SAFETY: see `ExitOnUnwind::drop`.
    unsafe { libc::_exit(SETUP_FAILED) }
}

/// Runs the setup steps in order, then tries each exec candidate.
/// Only returns on failure.
fn set_up_and_exec(plan: &ChildPlan) -> Result<std::convert::Infallible, Failure> {
    reset_signal_dispositions()?;
    mark_inherited_fds_cloexec(&plan.inherited_fds)?;
    apply_rlimits(plan)?;
    umask(Mode::from_bits_truncate(plan.umask));

    // Change directory as the original user first (root may be able to enter
    // a directory the target user can't). If that fails and the user is about
    // to change, try again afterwards, when the new user's permissions apply.
    let chdir_error = chdir(plan).err();
    if let Some(credentials) = &plan.credentials {
        switch_credentials(credentials)?;
    }
    if let Some(errno) = chdir_error {
        if plan.credentials.is_none() {
            return Err(Failure::at(ChildStep::Chdir, errno));
        }
        chdir(plan).map_err(|errno| Failure::at(ChildStep::Chdir, errno))?;
    }

    // macOS has no parent-death signal; step 4's supervisor covers that case.
    #[cfg(target_os = "linux")]
    die_with_parent(plan)?;
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
        .map_err(|errno| Failure::at(ChildStep::ResetSignals, errno))?;
    Err(exec_first_candidate(plan))
}

/// Puts every signal back to its default action. Shipyard ignores SIGPIPE
/// (Rust does by default) and may ignore others; an ignored signal stays
/// ignored across `exec`, which would silently change how the command behaves.
fn reset_signal_dispositions() -> Result<(), Failure> {
    // SAFETY: a zeroed `sigaction` is a valid value (no handler, no flags,
    // empty mask); we then set the handler to SIG_DFL.
    let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
    default.sa_sigaction = libc::SIG_DFL;
    for signal in 1..=MAX_SIGNAL {
        // SAFETY: `sigaction` is async-signal-safe; installing SIG_DFL runs no
        // code of ours. EINVAL is expected for SIGKILL, SIGSTOP and the
        // signals the C library reserves.
        let rc = unsafe { libc::sigaction(signal, &raw const default, std::ptr::null_mut()) };
        if rc == -1 && Errno::last() != Errno::EINVAL {
            return Err(Failure::at(ChildStep::ResetSignals, Errno::last()));
        }
    }
    Ok(())
}

/// Marks every descriptor above 2 close-on-exec, so the command inherits only
/// stdin, stdout and stderr. (Marking rather than closing keeps the report
/// pipe usable until `exec` closes it.)
fn mark_inherited_fds_cloexec(sweep: &FdSweep) -> Result<(), Failure> {
    #[cfg(target_os = "linux")]
    if close_range_cloexec() {
        return Ok(());
    }
    match sweep {
        FdSweep::Listed(fds) => fds.iter().try_for_each(|fd| set_cloexec(*fd)),
        FdSweep::Below(bound) => (3..*bound).try_for_each(set_cloexec),
    }
}

/// One system call marks them all, on Linux 5.11 and later. Older kernels
/// (ENOSYS, or EINVAL before the flag existed) and some seccomp profiles
/// (EPERM) need the slower loop instead.
#[cfg(target_os = "linux")]
fn close_range_cloexec() -> bool {
    // SAFETY: `close_range` with CLOSE_RANGE_CLOEXEC only changes descriptor
    // flags; the arguments are plain integers.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    rc == 0
}

fn set_cloexec(fd: RawFd) -> Result<(), Failure> {
    // SAFETY: F_SETFD only changes the descriptor's flags; a closed descriptor
    // fails with EBADF, which just means there is nothing to protect.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc == -1 && Errno::last() != Errno::EBADF {
        return Err(Failure::at(ChildStep::CloseFds, Errno::last()));
    }
    Ok(())
}

/// Applies the resource limits. Done before switching user, because raising
/// a hard limit needs privileges the new user may not have.
fn apply_rlimits(plan: &ChildPlan) -> Result<(), Failure> {
    for (index, limit) in plan.rlimits.iter().enumerate() {
        let value = libc::rlimit {
            rlim_cur: limit.soft,
            rlim_max: limit.hard,
        };
        // SAFETY: `value` is a valid `rlimit`; `setrlimit` is a direct system
        // call wrapper with no locks or allocation.
        if unsafe { libc::setrlimit(limit.resource, &raw const value) } == -1 {
            return Err(Failure {
                step: ChildStep::Rlimit,
                errno: Errno::last(),
                detail: u64::try_from(index).unwrap_or(u64::MAX),
            });
        }
    }
    Ok(())
}

fn chdir(plan: &ChildPlan) -> Result<(), Errno> {
    // SAFETY: `plan.cwd` is a NUL-terminated string that outlives the call.
    let rc = unsafe { libc::chdir(plan.cwd.as_ptr()) };
    if rc == -1 { Err(Errno::last()) } else { Ok(()) }
}

/// Switches to the target user and groups, in the only safe order (groups,
/// then group, then user: after `setuid` the others are no longer allowed),
/// then checks that it worked.
fn switch_credentials(credentials: &Credentials) -> Result<(), Failure> {
    set_groups(&credentials.groups)?;
    set_gid(credentials.gid)?;
    set_uid(credentials.uid)?;
    verify_credentials(credentials)
}

#[cfg(target_os = "linux")]
fn set_groups(groups: &[Gid]) -> Result<(), Failure> {
    nix::unistd::setgroups(groups).map_err(|errno| Failure::at(ChildStep::SetGroups, errno))
}

/// nix doesn't offer `setgroups` on macOS, so call it directly.
#[cfg(not(target_os = "linux"))]
fn set_groups(groups: &[Gid]) -> Result<(), Failure> {
    let count = libc::c_int::try_from(groups.len())
        .map_err(|_| Failure::at(ChildStep::SetGroups, Errno::EINVAL))?;
    // SAFETY: `Gid` is a transparent wrapper around `gid_t`, so the slice is a
    // valid array of `count` group IDs for the duration of the call.
    let rc = unsafe { libc::setgroups(count, groups.as_ptr().cast()) };
    if rc == -1 {
        return Err(Failure::at(ChildStep::SetGroups, Errno::last()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_gid(gid: Gid) -> Result<(), Failure> {
    nix::unistd::setresgid(gid, gid, gid).map_err(|errno| Failure::at(ChildStep::SetGid, errno))
}

/// As root, `setgid` sets the real, effective and saved group IDs.
#[cfg(not(target_os = "linux"))]
fn set_gid(gid: Gid) -> Result<(), Failure> {
    nix::unistd::setgid(gid).map_err(|errno| Failure::at(ChildStep::SetGid, errno))
}

#[cfg(target_os = "linux")]
fn set_uid(uid: Uid) -> Result<(), Failure> {
    nix::unistd::setresuid(uid, uid, uid).map_err(|errno| Failure::at(ChildStep::SetUid, errno))
}

/// As root, `setuid` sets the real, effective and saved user IDs.
#[cfg(not(target_os = "linux"))]
fn set_uid(uid: Uid) -> Result<(), Failure> {
    nix::unistd::setuid(uid).map_err(|errno| Failure::at(ChildStep::SetUid, errno))
}

/// Refuses to continue unless every ID changed and root's privileges are
/// really gone. A partial switch would run the command with more rights than
/// asked for.
fn verify_credentials(credentials: &Credentials) -> Result<(), Failure> {
    let mismatch = || Failure::at(ChildStep::VerifyCredentials, Errno::EPERM);
    if !ids_match(credentials) || !groups_match(&credentials.groups) {
        return Err(mismatch());
    }
    // Dropping root must be permanent: getting it back has to fail.
    if !credentials.uid.is_root() && nix::unistd::setuid(Uid::from_raw(0)).is_ok() {
        return Err(mismatch());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ids_match(credentials: &Credentials) -> bool {
    let (Ok(uids), Ok(gids)) = (nix::unistd::getresuid(), nix::unistd::getresgid()) else {
        return false;
    };
    let uid = credentials.uid;
    let gid = credentials.gid;
    [uids.real, uids.effective, uids.saved] == [uid; 3]
        && [gids.real, gids.effective, gids.saved] == [gid; 3]
}

#[cfg(not(target_os = "linux"))]
fn ids_match(credentials: &Credentials) -> bool {
    use nix::unistd::{getegid, geteuid, getgid, getuid};
    getuid() == credentials.uid
        && geteuid() == credentials.uid
        && getgid() == credentials.gid
        && getegid() == credentials.gid
}

/// On Linux, `setgroups` sets the list exactly, so there is nothing to check.
#[cfg(target_os = "linux")]
fn groups_match(_expected: &[Gid]) -> bool {
    true
}

/// On macOS the group list shares storage with the effective group, so check
/// that every requested group survived `setgid`.
#[cfg(not(target_os = "linux"))]
fn groups_match(expected: &[Gid]) -> bool {
    let mut actual: [libc::gid_t; 16] = [0; 16];
    // SAFETY: `actual` is a writable array of 16 `gid_t`, and 16 is passed as
    // its length, so the kernel can't write past it.
    let count = unsafe { libc::getgroups(16, actual.as_mut_ptr()) };
    let Ok(count) = usize::try_from(count) else {
        return false;
    };
    let Some(actual) = actual.get(..count) else {
        return false;
    };
    expected.iter().all(|gid| actual.contains(&gid.as_raw()))
}

/// On Linux, makes the command die if shipyard dies, so it can't be left
/// running unsupervised. Set after the credential change, which clears it.
#[cfg(target_os = "linux")]
fn die_with_parent(plan: &ChildPlan) -> Result<(), Failure> {
    nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)
        .map_err(|errno| Failure::at(ChildStep::ParentDeathSignal, errno))?;
    // The parent may have died before the call above: then the signal will
    // never come, so check by hand.
    if nix::unistd::getppid().as_raw() != plan.parent_pid {
        // SAFETY: see `ExitOnUnwind::drop`. Nobody is left to report to.
        unsafe { libc::_exit(SETUP_FAILED) }
    }
    Ok(())
}

/// Tries each candidate path in order, like `execvp`. Only returns on
/// failure: "not found"-type errors move on to the next candidate, the first
/// "permission denied" is remembered, anything else stops the search.
fn exec_first_candidate(plan: &ChildPlan) -> Failure {
    let mut not_found = Failure::at(ChildStep::Exec, Errno::ENOENT);
    let mut denied: Option<Failure> = None;
    for (index, path) in plan.candidates.iter().enumerate() {
        // SAFETY: every pointer is NUL-terminated or a NULL-terminated array
        // owned by `plan`, which outlives the call.
        unsafe { libc::execve(path.as_ptr(), plan.argv.as_ptr(), plan.envp.as_ptr()) };
        let failure = Failure {
            step: ChildStep::Exec,
            errno: Errno::last(),
            detail: u64::try_from(index).unwrap_or(u64::MAX),
        };
        match failure.errno {
            Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP | Errno::ENAMETOOLONG => {
                not_found = failure;
            }
            Errno::EACCES => {
                denied.get_or_insert(failure);
            }
            _ => return failure,
        }
    }
    denied.unwrap_or(not_found)
}

/// Writes the failure record. Best effort: if the write fails there is no one
/// to tell, and the parent falls back to the exit status.
fn report_failure(report: RawFd, failure: &Failure) {
    let record = encode(failure.step, failure.errno as i32, failure.detail);
    loop {
        // SAFETY: `record` is a valid buffer of `record.len()` bytes, and
        // `write` is async-signal-safe.
        let written = unsafe { libc::write(report, record.as_ptr().cast(), record.len()) };
        if written != -1 || Errno::last() != Errno::EINTR {
            return;
        }
    }
}
