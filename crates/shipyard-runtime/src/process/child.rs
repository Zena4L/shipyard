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
use nix::unistd::{Gid, Pid, Uid, getpgrp, getpid, setpgid};

use super::channel::encode;

use super::failure::{ChildStep, means_not_found};
use super::plan::{ChildPlan, Credentials, FdSweep};
use super::signal::MAX_SIGNAL;

/// The child's exit code when setup fails. The parent normally reads the
/// failure from the report pipe instead; this is only what's left if it can't.
pub(super) const SETUP_FAILED: libc::c_int = 125;

/// A failed setup step, as it travels to the parent.
pub(super) struct Failure {
    step: ChildStep,
    errno: Errno,
    detail: u64,
}

impl Failure {
    pub(super) fn at(step: ChildStep, errno: Errno) -> Self {
        Self {
            step,
            errno,
            detail: 0,
        }
    }
}

/// If anything unwinds (it shouldn't: see the lints above), exit instead of
/// running the parent's code in this copy of its process.
///
/// A last resort, not a safety net: in builds that unwind (debug, tests), the
/// panic hook runs *before* this, and it allocates and locks stderr. Release
/// builds abort on panic and never get here. The real protection is the
/// lints, which keep panics out of this module.
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

/// Runs the setup steps in order, then tries each exec candidate. Only
/// returns on failure.
///
/// Isolated with a new PID namespace, this process is its PID 1: after the
/// namespace setup it becomes the container's init, which starts the
/// command (PID 2) and runs the rest of the steps there (see `init.rs`).
fn set_up_and_exec(plan: &ChildPlan) -> Result<std::convert::Infallible, Failure> {
    // Our copy of the parent's end of the go pipe: closed at once, so that
    // the pipe reports when the *parent's* copy closes.
    // SAFETY: `close` is async-signal-safe; the descriptor is ours.
    unsafe { libc::close(plan.handshake.parent_end) };
    reset_signal_dispositions(&plan.keep_ignored)?;
    wait_for_go(plan.handshake.read)?;
    #[cfg(target_os = "linux")]
    if let Some(join) = &plan.join {
        // Already in the container's PID namespace (see `join.rs`).
        join_namespaces(join)?;
        // A session of its own, as an isolated command has.
        start_session()?;
        return prepare_and_exec(plan);
    }
    #[cfg(target_os = "linux")]
    if let Some(container) = &plan.container {
        set_up_container(container)?;
        if let Some(init) = &container.init {
            // The init must die with shipyard too: then the kernel kills the
            // whole namespace. It never switches user, so nothing clears this.
            die_with_parent(plan)?;
            return super::init::run(plan, init);
        }
        // No PID namespace, so no init: this process becomes the command.
        // (It already leads a session of its own.)
        return prepare_and_exec(plan);
    }
    if plan.new_process_group {
        // Its own group, so the supervisor can signal everything the command
        // starts at once. Only without a terminal; see `SpawnOptions`.
        start_process_group()?;
    }
    prepare_and_exec(plan)
}

/// The steps that belong to the command itself, then `exec`: the rest of the
/// setup, run in the command's own process. Only returns on failure.
pub(super) fn prepare_and_exec(plan: &ChildPlan) -> Result<std::convert::Infallible, Failure> {
    mark_inherited_fds_cloexec(&plan.inherited_fds)?;
    apply_rlimits(plan)?;
    umask(Mode::from_bits_truncate(plan.umask));

    // Change directory as the original user first (root may be able to enter
    // a directory the target user can't). If that fails and the user is about
    // to change, try again afterwards, when the new user's permissions apply.
    // Only a permission error is worth retrying (runc does the same): any
    // other error, like a missing directory, won't change with the user.
    let chdir_error = chdir(plan).err();
    if let Some(credentials) = &plan.credentials {
        switch_credentials(credentials)?;
    }
    match chdir_error {
        None => {}
        Some(Errno::EACCES | Errno::EPERM) if plan.credentials.is_some() => {
            chdir(plan).map_err(|errno| Failure::at(ChildStep::Chdir, errno))?;
        }
        Some(errno) => return Err(Failure::at(ChildStep::Chdir, errno)),
    }

    // macOS has no parent-death signal: if shipyard is killed with SIGKILL
    // there, the command keeps running (documented in docs/user/run.md).
    #[cfg(target_os = "linux")]
    die_with_parent(plan)?;
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
        .map_err(|errno| Failure::at(ChildStep::SignalMask, errno))?;
    Err(exec_first_candidate(plan))
}

/// Puts every signal back to its default action, except those in
/// `keep_ignored`. Shipyard ignores SIGPIPE (Rust does by default) and
/// catches others; an ignored signal stays ignored across `exec`, which would
/// silently change how the command behaves.
///
/// Setting a signal whose default is to be ignored (SIGCHLD, SIGWINCH,
/// SIGURG, SIGCONT) to its default discards one that's pending, so a
/// SIGWINCH forwarded this early is lost. Harmless: a command reads the
/// window size itself when it starts.
fn reset_signal_dispositions(keep_ignored: &[libc::c_int]) -> Result<(), Failure> {
    // SAFETY: a zeroed `sigaction` is a valid value (no handler, no flags,
    // empty mask); we then set the handler to SIG_DFL.
    let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
    default.sa_sigaction = libc::SIG_DFL;
    for signal in (1..=MAX_SIGNAL).filter(|signal| !keep_ignored.contains(signal)) {
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

/// Waits for the parent's "go" (see `channel.rs`). End-of-file means the
/// parent died before sending it: nobody is left to supervise the command,
/// or to report to, so exit quietly.
fn wait_for_go(pipe: RawFd) -> Result<(), Failure> {
    let mut byte = 0u8;
    loop {
        // SAFETY: `read` is async-signal-safe; `byte` is one writable byte.
        let n = unsafe { libc::read(pipe, (&raw mut byte).cast(), 1) };
        match n {
            1 => return Ok(()),
            // SAFETY: see `ExitOnUnwind::drop`.
            0 => unsafe { libc::_exit(SETUP_FAILED) },
            _ if Errno::last() == Errno::EINTR => {}
            _ => return Err(Failure::at(ChildStep::Handshake, Errno::last())),
        }
    }
}

/// A session of its own (`setsid`): no controlling terminal, and its own
/// process group.
#[cfg(target_os = "linux")]
fn start_session() -> Result<(), Failure> {
    // SAFETY: `setsid` is async-signal-safe.
    if unsafe { libc::setsid() } == -1 {
        return Err(Failure::at(ChildStep::Session, Errno::last()));
    }
    Ok(())
}

/// Joins a running container's namespaces, in the order given (the mount
/// namespace last). This process is single-threaded, which joining a mount
/// namespace requires.
#[cfg(target_os = "linux")]
fn join_namespaces(join: &super::plan::JoinSetup) -> Result<(), Failure> {
    for namespace in &join.namespaces {
        // SAFETY: `setns` is a plain system call; the descriptor was opened
        // before the fork.
        if unsafe { libc::setns(namespace.fd, namespace.flag) } == -1 {
            return Err(Failure {
                step: ChildStep::JoinNamespace,
                errno: Errno::last(),
                detail: namespace.index,
            });
        }
    }
    Ok(())
}

/// Inside the new namespaces: a session of its own (no controlling
/// terminal, its own process group), mounts that don't propagate to the
/// host, a `/proc` for the new PID namespace, and the hostname.
#[cfg(target_os = "linux")]
fn set_up_container(container: &super::plan::ContainerSetup) -> Result<(), Failure> {
    start_session()?;
    if container.private_mounts {
        // First, before any mount: on hosts where `/` is shared (systemd),
        // a mount made here would otherwise appear on the host too. Slave,
        // not private (as runc does): nothing propagates out, but the
        // host's unmounts still propagate in, so a device unmounted on the
        // host isn't kept busy by the container's copy of the mount.
        mount(
            None,
            c"/",
            None,
            libc::MS_REC | libc::MS_SLAVE,
            ChildStep::MountPrivate,
        )?;
    }
    if container.mount_proc {
        // Mounted by this process, so it shows the new PID namespace.
        mount(
            Some(c"proc"),
            c"/proc",
            Some(c"proc"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            ChildStep::MountProc,
        )?;
    }
    if let Some(hostname) = &container.hostname {
        let name = hostname.as_bytes();
        // SAFETY: `name` is valid for its length; `sethostname` only reads it.
        if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } == -1 {
            return Err(Failure::at(ChildStep::Hostname, Errno::last()));
        }
    }
    Ok(())
}

/// `mount(2)` with C strings that need no allocation.
#[cfg(target_os = "linux")]
fn mount(
    source: Option<&std::ffi::CStr>,
    target: &std::ffi::CStr,
    fstype: Option<&std::ffi::CStr>,
    flags: libc::c_ulong,
    step: ChildStep,
) -> Result<(), Failure> {
    let or_null = |s: Option<&std::ffi::CStr>| s.map_or(std::ptr::null(), std::ffi::CStr::as_ptr);
    // SAFETY: every pointer is NULL or a NUL-terminated string that outlives
    // the call; no data argument.
    let rc = unsafe {
        libc::mount(
            or_null(source),
            target.as_ptr(),
            or_null(fstype),
            flags,
            std::ptr::null(),
        )
    };
    if rc == -1 {
        return Err(Failure::at(step, Errno::last()));
    }
    Ok(())
}

/// Makes this process the leader of a new process group.
///
/// The parent makes the same call for the child (TLPI §34.2: with both
/// calling it, the order in which they run doesn't matter). On macOS the two
/// calls can collide, and this one then fails with EPERM although the
/// parent's has done the job, or is about to (seen about once in 1,000
/// parallel starts on macOS 27). So success is judged by the result: on
/// EPERM, check whether the group is already ours, and otherwise retry for
/// up to about 50 ms while the parent's call finishes.
fn start_process_group() -> Result<(), Failure> {
    const ATTEMPTS: u32 = 50;
    let pause = libc::timespec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    let mut last = Errno::EPERM;
    for _ in 0..ATTEMPTS {
        match setpgid(Pid::from_raw(0), Pid::from_raw(0)) {
            Ok(()) => return Ok(()),
            Err(Errno::EPERM) => {}
            Err(errno) => return Err(Failure::at(ChildStep::ProcessGroup, errno)),
        }
        if getpgrp() == getpid() {
            return Ok(());
        }
        // SAFETY: `nanosleep` is async-signal-safe; `pause` is valid and the
        // remaining time isn't needed.
        let rc = unsafe { libc::nanosleep(&raw const pause, std::ptr::null_mut()) };
        if rc == -1 && Errno::last() != Errno::EINTR {
            last = Errno::last();
        }
    }
    Err(Failure::at(ChildStep::ProcessGroup, last))
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
        let mut result = set_rlimit(limit.resource, limit.soft, limit.hard);
        if let (Err(Errno::EINVAL), Some(soft)) = (result, limit.fallback_soft) {
            result = set_rlimit(limit.resource, soft, limit.hard);
        }
        result.map_err(|errno| Failure {
            step: ChildStep::Rlimit,
            errno,
            detail: u64::try_from(index).unwrap_or(u64::MAX),
        })?;
    }
    Ok(())
}

fn set_rlimit(
    resource: super::plan::RawResource,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
) -> Result<(), Errno> {
    let value = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    // SAFETY: `value` is a valid `rlimit`; `setrlimit` is a direct system
    // call wrapper with no locks or allocation.
    if unsafe { libc::setrlimit(resource, &raw const value) } == -1 {
        return Err(Errno::last());
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
///
/// These rely on the 64-bit-only build (see `lib.rs`): on 32-bit Linux,
/// `SYS_setresuid` and friends are the old 16-bit calls.
///
/// **On Linux, raw system calls, not the C library's wrappers.** The kernel
/// keeps credentials per thread; glibc's wrappers make the change for the
/// whole process by signalling every other thread and waiting for each to
/// follow. After our raw `clone` (see `clone.rs`), glibc still lists the
/// parent's threads, which don't exist here: the wait never ends. (It hung,
/// in a test binary with other threads running.) This process has one
/// thread, so the raw call is the whole change.
fn switch_credentials(credentials: &Credentials) -> Result<(), Failure> {
    set_groups(&credentials.groups)?;
    set_gid(credentials.gid)?;
    set_uid(credentials.uid)?;
    verify_credentials(credentials)
}

/// The raw `setgroups` system call (see [`switch_credentials`]).
#[cfg(target_os = "linux")]
fn set_groups(groups: &[libc::gid_t]) -> Result<(), Failure> {
    // SAFETY: `groups` is a valid array of `len` group IDs for the duration
    // of the call; the kernel only reads it.
    let rc = unsafe { libc::syscall(libc::SYS_setgroups, groups.len(), groups.as_ptr()) };
    raw_result(rc).map_err(|errno| Failure::at(ChildStep::SetGroups, errno))
}

/// Raw `libc::setgroups`: nix doesn't offer it on macOS.
#[cfg(not(target_os = "linux"))]
fn set_groups(groups: &[libc::gid_t]) -> Result<(), Failure> {
    let count = libc::c_int::try_from(groups.len())
        .map_err(|_| Failure::at(ChildStep::SetGroups, Errno::EINVAL))?;
    // SAFETY: `groups` is a valid array of `count` group IDs for the duration
    // of the call; `setgroups` only reads it.
    let rc = unsafe { libc::setgroups(count, groups.as_ptr()) };
    if rc == -1 {
        return Err(Failure::at(ChildStep::SetGroups, Errno::last()));
    }
    Ok(())
}

/// The raw `setresgid` system call (see [`switch_credentials`]).
#[cfg(target_os = "linux")]
fn set_gid(gid: Gid) -> Result<(), Failure> {
    let gid = gid.as_raw();
    // SAFETY: plain integer arguments.
    let rc = unsafe { libc::syscall(libc::SYS_setresgid, gid, gid, gid) };
    raw_result(rc).map_err(|errno| Failure::at(ChildStep::SetGid, errno))
}

/// As root, `setgid` sets the real, effective and saved group IDs.
#[cfg(not(target_os = "linux"))]
fn set_gid(gid: Gid) -> Result<(), Failure> {
    nix::unistd::setgid(gid).map_err(|errno| Failure::at(ChildStep::SetGid, errno))
}

/// The raw `setresuid` system call (see [`switch_credentials`]).
#[cfg(target_os = "linux")]
fn set_uid(uid: Uid) -> Result<(), Failure> {
    let uid = uid.as_raw();
    // SAFETY: plain integer arguments.
    let rc = unsafe { libc::syscall(libc::SYS_setresuid, uid, uid, uid) };
    raw_result(rc).map_err(|errno| Failure::at(ChildStep::SetUid, errno))
}

/// As root, `setuid` sets the real, effective and saved user IDs.
#[cfg(not(target_os = "linux"))]
fn set_uid(uid: Uid) -> Result<(), Failure> {
    nix::unistd::setuid(uid).map_err(|errno| Failure::at(ChildStep::SetUid, errno))
}

/// A raw system call's return value: -1 and `errno` on failure.
#[cfg(target_os = "linux")]
fn raw_result(rc: libc::c_long) -> Result<(), Errno> {
    if rc == -1 { Err(Errno::last()) } else { Ok(()) }
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
    if !credentials.uid.is_root() && set_uid(Uid::from_raw(0)).is_ok() {
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
fn groups_match(_expected: &[libc::gid_t]) -> bool {
    true
}

/// On macOS the group list shares storage with the effective group, so check
/// that every requested group survived `setgid`.
#[cfg(not(target_os = "linux"))]
fn groups_match(expected: &[libc::gid_t]) -> bool {
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
    expected.iter().all(|gid| actual.contains(gid))
}

/// On Linux, makes the command die if shipyard dies, so it can't be left
/// running unsupervised. Set after the credential change, which clears it.
#[cfg(target_os = "linux")]
fn die_with_parent(plan: &ChildPlan) -> Result<(), Failure> {
    nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)
        .map_err(|errno| Failure::at(ChildStep::ParentDeathSignal, errno))?;
    // The parent may have died before the call above: then the signal will
    // never come, so check by hand. Not with `getppid`: inside a new PID
    // namespace it returns 0. Instead, the parent holds its end of the go
    // pipe open until `exec`; once every writer is gone, the pipe reports a
    // hang-up.
    let mut watch = libc::pollfd {
        fd: plan.handshake.read,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` is async-signal-safe; `watch` is one valid pollfd, and a
    // zero timeout never blocks.
    let rc = unsafe { libc::poll(&raw mut watch, 1, 0) };
    if rc == 1 && watch.revents & libc::POLLHUP != 0 {
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
            errno if means_not_found(errno) => not_found = failure,
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
    write_record(
        report,
        &encode(failure.step, failure.errno as i32, failure.detail),
    );
}

/// Writes a small record to a pipe in one `write`, retrying only if a
/// signal interrupted it. Small pipe writes are atomic: the reader gets all
/// of it or none. Best effort: if the write fails, there's no one to tell.
pub(super) fn write_record(pipe: RawFd, record: &[u8]) {
    loop {
        // SAFETY: `record` is valid for its length, and `write` is
        // async-signal-safe.
        let written = unsafe { libc::write(pipe, record.as_ptr().cast(), record.len()) };
        if written != -1 || Errno::last() != Errno::EINTR {
            return;
        }
    }
}
