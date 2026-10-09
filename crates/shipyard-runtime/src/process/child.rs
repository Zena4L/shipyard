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

#[cfg(target_os = "linux")]
use super::failure::NamespaceRootStep;
use super::failure::{ChildStep, means_not_found};
use super::plan::{ChildPlan, Credentials, FdSweep};
use super::signal::MAX_SIGNAL;
#[cfg(target_os = "linux")]
use crate::security::{Confinement, SeccompFilter};

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

    /// The same failure, saying which one of several (a path, a mount).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only Linux sets up containers")
    )]
    pub(super) fn detail(self, detail: usize) -> Self {
        Self {
            detail: u64::try_from(detail).unwrap_or(u64::MAX),
            ..self
        }
    }

    /// The same failure, with a detail of its own encoding (a hook's: see
    /// `HookError::to_record`).
    #[cfg(target_os = "linux")]
    pub(super) fn raw_detail(self, detail: u64) -> Self {
        Self { detail, ..self }
    }
}

/// A failure another process reported, to report on in turn.
#[cfg(target_os = "linux")]
impl From<super::failure::ChildFailure> for Failure {
    fn from(failure: super::failure::ChildFailure) -> Self {
        Self {
            step: failure.step,
            errno: failure.errno,
            detail: failure.detail,
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
/// Isolated, this process re-execs at once into the container's clean
/// image, which sets the container up and starts the command (see
/// `init.rs`).
fn set_up_and_exec(plan: &ChildPlan) -> Result<std::convert::Infallible, Failure> {
    if let Some(handshake) = &plan.handshake {
        // Our copy of the parent's end of the go pipe: closed at once, so
        // that the pipe reports when the *parent's* copy closes.
        // SAFETY: `close` is async-signal-safe; the descriptor is ours.
        unsafe { libc::close(handshake.parent_end) };
    }
    reset_signal_dispositions(&plan.keep_ignored)?;
    if let Some(handshake) = &plan.handshake {
        wait_for_go(handshake.read)?;
    }
    #[cfg(target_os = "linux")]
    if let Some(join) = &plan.join {
        // Already in the container's PID namespace (see `join.rs`).
        join_namespaces(join)?;
        // A session of its own, as an isolated command has.
        start_session()?;
        return prepare_and_exec(plan);
    }
    #[cfg(target_os = "linux")]
    if let Some(reexec) = &plan.reexec {
        // First: a credential change clears the parent-death signal.
        if reexec.namespace_root {
            become_namespace_root(setgroups_allowed_here())?;
        }
        // The init must die with shipyard too: then the kernel kills the
        // whole namespace. The signal survives the exec below.
        die_with_parent(plan)?;
        return Err(reexec_into_clean_image(plan, reexec));
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
    #[cfg(target_os = "linux")]
    if let Some(confinement) = &plan.confinement {
        if let Some(filter) = confinement.filter_before_setup() {
            install_filter(filter)?;
        }
        confine_before_switch(confinement)?;
    }
    if let Some(credentials) = &plan.credentials {
        switch_credentials(credentials)?;
    }
    #[cfg(target_os = "linux")]
    if let Some(confinement) = &plan.confinement {
        confine_after_switch(confinement)?;
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
    #[cfg(target_os = "linux")]
    if let Some(filter) = plan
        .confinement
        .as_ref()
        .and_then(Confinement::filter_before_exec)
    {
        install_filter(filter)?;
    }
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
pub(super) fn wait_for_go(pipe: RawFd) -> Result<(), Failure> {
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

/// Re-execs the shipyard binary as the container's clean image, which sets
/// the container up and starts the command (see `init.rs`). Only returns on
/// failure.
///
/// First every descriptor but the ones passed on is closed: anything
/// leaked by shipyard's own parent would otherwise be held by the
/// container's PID 1 for its whole life.
#[cfg(target_os = "linux")]
fn reexec_into_clean_image(plan: &ChildPlan, reexec: &super::plan::Reexec) -> Failure {
    let closed = reexec.close_ranges.iter().all(|&(first, last)| {
        // SAFETY: `close_range` only closes descriptors; the arguments are
        // plain integers.
        unsafe { libc::syscall(libc::SYS_close_range, first, last, 0_u32) == 0 }
    });
    if !closed {
        // No `close_range` (before Linux 5.9, or seccomp): the plan's list.
        let keep = |fd: RawFd| {
            reexec.passed.contains(&fd)
                || fd == reexec.exe
                || reexec
                    .host_exe
                    .as_ref()
                    .is_some_and(|(host, _)| fd == *host)
        };
        let close = |fd: RawFd| {
            if !keep(fd) {
                // SAFETY: `close` is async-signal-safe; a descriptor that
                // isn't open just gives EBADF.
                unsafe { libc::close(fd) };
            }
        };
        match &plan.inherited_fds {
            FdSweep::Listed(fds) => fds.iter().copied().for_each(close),
            FdSweep::Below(bound) => (3..*bound).for_each(close),
        }
    }
    for &fd in &reexec.passed {
        // SAFETY: `fcntl` is async-signal-safe; clearing close-on-exec only
        // lets the descriptor survive the exec.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } == -1 {
            return Failure::at(ChildStep::Reexec, Errno::last());
        }
    }
    // A pending SIGPIPE survives the exec, but the clean image's Rust
    // startup then ignores SIGPIPE, which discards it: say so, and the clean
    // image raises it again.
    let argv = if sigpipe_pending() {
        &reexec.argv_sigpipe
    } else {
        &reexec.argv
    };
    exec_binary(reexec.exe, &reexec.exe_path, argv, &reexec.env);
    // A sealed copy that won't run (a security module's policy on memfds,
    // say): shipyard's file.
    if let Some((host, host_path)) = &reexec.host_exe {
        exec_binary(*host, host_path, argv, &reexec.env);
    }
    Failure::at(ChildStep::Reexec, Errno::last())
}

/// Execs the binary open at `fd` (`path` is the same, through `/proc`);
/// returns only if that failed, with `errno` saying why.
#[cfg(target_os = "linux")]
fn exec_binary(
    fd: RawFd,
    path: &std::ffi::CStr,
    argv: &super::plan::CStringArray,
    env: &super::plan::CStringArray,
) {
    // SAFETY: `execveat` and `execve` are async-signal-safe system calls.
    // Every pointer passed is a NUL-terminated string or a null-terminated
    // array of them, prepared before the fork and alive until the exec (or
    // its failure).
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            fd,
            c"".as_ptr(),
            argv.as_ptr(),
            env.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
        // No `execveat` (before Linux 3.19, seccomp, some emulators): the
        // same file through `/proc` (the host's, still, at this point).
        libc::execve(path.as_ptr(), argv.as_ptr(), env.as_ptr());
    }
}

#[cfg(target_os = "linux")]
/// Whether a SIGPIPE is pending for this process (blocked, as every signal
/// is here).
fn sigpipe_pending() -> bool {
    // SAFETY: `sigemptyset` initializes the set before `sigpending` fills
    // it and `sigismember` reads it; all three are async-signal-safe and
    // touch only this stack variable.
    unsafe {
        let mut pending = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&raw mut pending);
        libc::sigpending(&raw mut pending) == 0
            && libc::sigismember(&raw const pending, libc::SIGPIPE) == 1
    }
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
    if let Some(groups) = &credentials.groups {
        set_groups(groups)?;
    }
    set_gid(credentials.gid)?;
    set_uid(credentials.uid)?;
    verify_credentials(credentials)
}

/// A container's command, before the switch of user (TLPI ch. 39; the order
/// is step 1's probe's): keep capabilities across the switch if the user
/// isn't root (C3), then drop from the bounding set, which needs
/// `CAP_SETPCAP` and so root (C1). Raw `prctl`: no C library state.
#[cfg(target_os = "linux")]
pub(super) fn confine_before_switch(confinement: &Confinement) -> Result<(), Failure> {
    if confinement.keep_caps {
        prctl(libc::PR_SET_KEEPCAPS, 1, 0)
            .map_err(|errno| Failure::at(ChildStep::KeepCaps, errno))?;
    }
    for &capability in &confinement.bounding_drop {
        prctl(libc::PR_CAPBSET_DROP, libc::c_ulong::from(capability), 0).map_err(|errno| {
            Failure::at(ChildStep::BoundingDrop, errno).detail(usize::from(capability))
        })?;
    }
    Ok(())
}

/// A container's command, after the switch of user: its effective,
/// permitted and inheritable sets (`capset`), its ambient ones (C4), and
/// `no_new_privs` (N1). Raw system calls, from data prepared before the fork.
#[cfg(target_os = "linux")]
pub(super) fn confine_after_switch(confinement: &Confinement) -> Result<(), Failure> {
    // `__user_cap_header_struct`: the version, and this process (0).
    let header: [u32; 2] = [LINUX_CAPABILITY_VERSION_3, 0];
    // SAFETY: `header` and `capset_data` are the kernel's
    // `__user_cap_header_struct` and two `__user_cap_data_struct`s (three
    // `u32`s each), which it only reads.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capset,
            header.as_ptr(),
            confinement.capset_data.as_ptr(),
        )
    };
    raw_result(rc).map_err(|errno| Failure::at(ChildStep::Capset, errno))?;
    for &capability in &confinement.ambient {
        prctl(
            libc::PR_CAP_AMBIENT,
            libc::c_ulong::from(libc::PR_CAP_AMBIENT_RAISE.unsigned_abs()),
            libc::c_ulong::from(capability),
        )
        .map_err(|errno| {
            Failure::at(ChildStep::AmbientRaise, errno).detail(usize::from(capability))
        })?;
    }
    if confinement.no_new_privileges {
        prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0)
            .map_err(|errno| Failure::at(ChildStep::NoNewPrivs, errno))?;
    }
    Ok(())
}

/// Installs a seccomp filter on this process (`seccomp(2)`,
/// `SECCOMP_SET_MODE_FILTER`): from here on, the kernel runs it on every
/// system call this process and everything it starts makes. Needs
/// `no_new_privs` or `CAP_SYS_ADMIN` (step 1's probe, S1). The raw system
/// call, on a program compiled before the fork.
#[cfg(target_os = "linux")]
pub(super) fn install_filter(filter: &SeccompFilter) -> Result<(), Failure> {
    // `Instruction` is the kernel's `struct sock_filter`, field for field.
    const _: () = assert!(
        size_of::<crate::security::seccomp::Instruction>() == size_of::<libc::sock_filter>()
    );
    let program = libc::sock_fprog {
        len: filter.len,
        filter: filter
            .program
            .as_ptr()
            .cast::<libc::sock_filter>()
            .cast_mut(),
    };
    // SAFETY: `program` points at `len` instructions, which the kernel only
    // reads (and copies) during the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            filter.flags,
            &raw const program,
        )
    };
    raw_result(rc).map_err(|errno| Failure::at(ChildStep::Seccomp, errno))
}

/// `_LINUX_CAPABILITY_VERSION_3`: `capset`'s 64-bit sets, in two halves.
#[cfg(target_os = "linux")]
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// The raw `prctl` system call with two arguments (no C library state).
#[cfg(target_os = "linux")]
fn prctl(option: libc::c_int, arg2: libc::c_ulong, arg3: libc::c_ulong) -> Result<(), Errno> {
    // SAFETY: `prctl` with integer arguments; the options used here read no
    // memory.
    raw_result(unsafe { libc::syscall(libc::SYS_prctl, option, arg2, arg3, 0, 0) })
}

/// Becomes root of the new user namespace (uid and gid 0 inside, no
/// supplementary groups), once the parent has written its maps.
///
/// Cloned by host root, this process is host uid 0, which the maps usually
/// leave out: inside, it's the overflow uid (65534). It has every
/// capability in the namespace, but `execve` recomputes capabilities from
/// the effective uid, and the clean image would start with none. As uid 0
/// inside, the exec keeps them all (`capabilities(7)`). Raw system calls, as
/// in [`switch_credentials`]; the failure's detail names the call.
///
/// Without `setgroups` (denied in the namespace: see
/// [`setgroups_allowed_here`]), the group list stays: the kernel would
/// refuse the call.
#[cfg(target_os = "linux")]
fn become_namespace_root(setgroups: bool) -> Result<(), Failure> {
    let failed = |call: NamespaceRootStep| {
        move |errno| Failure::at(ChildStep::NamespaceRoot, errno).detail(call.detail())
    };
    if setgroups {
        // SAFETY: plain integer arguments; no list (size 0, NULL).
        raw_result(unsafe {
            libc::syscall(libc::SYS_setgroups, 0, std::ptr::null::<libc::gid_t>())
        })
        .map_err(failed(NamespaceRootStep::Groups))?;
    }
    // SAFETY: plain integer arguments.
    raw_result(unsafe { libc::syscall(libc::SYS_setresgid, 0, 0, 0) })
        .map_err(failed(NamespaceRootStep::Gid))?;
    // SAFETY: plain integer arguments.
    raw_result(unsafe { libc::syscall(libc::SYS_setresuid, 0, 0, 0) })
        .map_err(failed(NamespaceRootStep::Uid))
}

/// Whether this process's user namespace allows `setgroups`
/// (`user_namespaces(7)`): its `/proc/self/setgroups` says `deny` when the
/// map writer denied it (a rootless caller mapping only its own IDs, or
/// `newgidmap` without a subordinate group range), and when an ancestor
/// namespace did: a namespace created inside one that denies it can't allow
/// it (`unshare -U -r`, say). Read, not inferred from the maps. Raw calls,
/// for after the clone; a kernel without the file (before 3.19) has no such
/// denial.
#[cfg(target_os = "linux")]
fn setgroups_allowed_here() -> bool {
    // SAFETY: a valid C string; `open` is async-signal-safe.
    let fd = unsafe {
        libc::open(
            c"/proc/self/setgroups".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd == -1 {
        return true;
    }
    // A failed read leaves it zeroed: allowed, and `setgroups` itself then
    // says whether it isn't.
    let mut text = [0u8; 8];
    // SAFETY: `text` is writable for its length; `read` and `close` are
    // async-signal-safe.
    unsafe {
        libc::read(fd, text.as_mut_ptr().cast(), text.len());
        libc::close(fd);
    }
    !text.starts_with(b"deny")
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
    let groups_ok = credentials.groups.as_deref().is_none_or(groups_match);
    if !ids_match(credentials) || !groups_ok {
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
    let Some(handshake) = &plan.handshake else {
        // The command a container's clean image starts has no handshake.
        // Without a PID namespace it's shipyard's child (the clean image
        // became it), and the credential change cleared the signal set
        // before the re-exec: shipyard may have died in between. Same
        // namespace, so `getppid` tells. With one, its parent is the init,
        // and if that dies the kernel ends the whole namespace.
        if plan
            .parent
            .is_some_and(|parent| nix::unistd::getppid().as_raw() != parent)
        {
            // SAFETY: see `ExitOnUnwind::drop`. Nobody is left to report to.
            unsafe { libc::_exit(SETUP_FAILED) }
        }
        return Ok(());
    };
    // The parent may have died before the call above: then the signal will
    // never come, so check by hand. Not with `getppid`: inside a new PID
    // namespace it returns 0. Instead, the parent holds its end of the go
    // pipe open until `exec`; once every writer is gone, the pipe reports a
    // hang-up.
    let mut watch = libc::pollfd {
        fd: handshake.read,
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

/// Starts the command a container's clean init forks (or becomes, without
/// a PID namespace): puts back the signal dispositions the init's own
/// startup changed (Rust ignores SIGPIPE), then sets the command up and
/// `exec`s it. Never returns; a failure goes to `report`.
///
/// # Safety
///
/// After `fork` in the clean init (a single-threaded process), or in that
/// process itself: `plan` must be the command's, built before the fork.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn command_main(plan: &ChildPlan, report: RawFd) -> ! {
    let _guard = ExitOnUnwind;
    let Err(failure) =
        reset_signal_dispositions(&plan.keep_ignored).and_then(|()| prepare_and_exec(plan));
    report_failure(report, &failure);
    // SAFETY: see `ExitOnUnwind::drop`.
    unsafe { libc::_exit(SETUP_FAILED) }
}

/// Writes the failure record. Best effort: if the write fails there is no one
/// to tell, and the parent falls back to the exit status.
pub(super) fn report_failure(report: RawFd, failure: &Failure) {
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
