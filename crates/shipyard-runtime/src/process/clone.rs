//! Starting a child in new namespaces: the raw `clone3` system call, falling
//! back to `clone` (`clone(2)`; TLPI ch. 28).
//!
//! Neither has a usable C library wrapper for this: glibc's `clone` wants a
//! function and a separate stack, and has no `clone3` at all. Called with no
//! stack, both behave like `fork`: the child carries on from the call, on a
//! copy of the parent's stack, and gets 0 back.
//!
//! **What the child must not do.** A raw `clone` skips the C library's fork
//! handling, so in the child some of its cached state still describes the
//! parent's thread. In particular `raise()` and anything `pthread_*` would
//! target the parent's thread. The child runs only `child.rs`, which uses
//! system calls with explicit process IDs, so this doesn't arise; keep it that
//! way.

use std::os::fd::RawFd;

use nix::errno::Errno;
use nix::unistd::Pid;

/// The kernel's `struct clone_args` (`<linux/sched.h>`, `CLONE_ARGS_SIZE_VER2`).
/// The C library doesn't declare it; its layout is part of the kernel's
/// stable ABI: eleven 64-bit fields.
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// `CLONE_INTO_CGROUP` (Linux 5.7): the child starts in the cgroup whose
/// directory `clone_args.cgroup` is. A 64-bit flag: `clone3` only.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// Which side of the clone we're on.
pub(crate) enum Cloned {
    /// The new process.
    Child,
    /// The parent, with the child's PID.
    Parent {
        /// The child.
        pid: Pid,
        /// Whether it started in the cgroup asked for. Not after the
        /// fallback to `clone`, which can't: the parent must move it.
        in_cgroup: bool,
    },
}

/// Starts a child, like `fork`, with the `CLONE_*` `flags` (namespace flags),
/// in the cgroup whose directory is open as `cgroup`, if given.
///
/// Tries `clone3` first, where the child starts in its cgroup (the probe's
/// G3). Falls back to `clone` when `clone3` isn't available:
/// ENOSYS on kernels before 5.3 and under Docker's default seccomp profile,
/// E2BIG if the kernel doesn't know this version of the arguments, and EPERM,
/// which older seccomp profiles returned for `clone3` alone. (If `clone` is
/// refused too, its EPERM is the real answer.) Then the child isn't in its
/// cgroup yet (`in_cgroup: false`).
///
/// # Errors
///
/// The errno of the call that failed, e.g. EPERM without privileges.
///
/// # Safety
///
/// As for `fork`: in the child, only async-signal-safe code may run until
/// `exec` or `_exit` (see `child.rs`), and the C library's per-thread state
/// must not be used (see the module docs). `flags` must not contain
/// `CLONE_VM`, `CLONE_THREAD` or other flags that share the parent's memory,
/// nor `CLONE_NEWTIME` (it can't go through the `clone` fallback).
pub(crate) unsafe fn clone_process(
    flags: libc::c_int,
    cgroup: Option<RawFd>,
) -> Result<Cloned, Errno> {
    // SAFETY: the caller's contract.
    match unsafe { clone3(flags, cgroup) } {
        Err(Errno::ENOSYS | Errno::E2BIG | Errno::EPERM) => {
            // SAFETY: the caller's contract.
            unsafe { clone_legacy(flags) }
        }
        result => result,
    }
}

/// `clone3` with no stack (so it behaves like `fork`).
///
/// # Safety
///
/// As for [`clone_process`].
unsafe fn clone3(flags: libc::c_int, cgroup: Option<RawFd>) -> Result<Cloned, Errno> {
    // Bit for bit: the flags are a bit set, not a number.
    let flags = u64::from(flags.cast_unsigned());
    let args = CloneArgs {
        flags: cgroup.map_or(flags, |_| flags | CLONE_INTO_CGROUP),
        exit_signal: u64::from(libc::SIGCHLD.cast_unsigned()),
        cgroup: cgroup.map_or(0, |fd| u64::from(fd.cast_unsigned())),
        ..CloneArgs::default()
    };
    // SAFETY: `args` is a valid `clone_args` of the size passed; no stack is
    // given, so the child continues here on a copy of this stack, like fork.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &raw const args,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    outcome(rc, cgroup.is_some())
}

/// The older `clone`, with no stack (so it behaves like `fork`).
///
/// # Safety
///
/// As for [`clone_process`].
unsafe fn clone_legacy(flags: libc::c_int) -> Result<Cloned, Errno> {
    // The low byte of `clone`'s flags is the signal sent to the parent when
    // the child exits.
    let flags_and_signal = libc::c_ulong::from((flags | libc::SIGCHLD).cast_unsigned());
    // Full-width null pointers, not `0`: through `...`, a 32-bit integer
    // leaves the upper half of the 64-bit argument undefined, and the kernel
    // reads all of it (a garbage "stack" would crash the child).
    let null = std::ptr::null_mut::<libc::c_void>();
    // SAFETY: no stack, so it behaves like `fork`. With every pointer argument
    // null, the per-architecture argument order of `clone` doesn't matter.
    let rc = unsafe { libc::syscall(libc::SYS_clone, flags_and_signal, null, null, null, null) };
    outcome(rc, false)
}

/// What a clone system call's return value means; `in_cgroup` says whether
/// the child was placed in a cgroup by the call.
fn outcome(rc: libc::c_long, in_cgroup: bool) -> Result<Cloned, Errno> {
    match rc {
        -1 => Err(Errno::last()),
        0 => Ok(Cloned::Child),
        pid => libc::pid_t::try_from(pid)
            .map(|pid| Cloned::Parent {
                pid: Pid::from_raw(pid),
                in_cgroup,
            })
            .map_err(|_| Errno::EOVERFLOW),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_args_match_the_kernels_layout() {
        // CLONE_ARGS_SIZE_VER2: eleven 64-bit fields, no padding.
        assert_eq!(std::mem::size_of::<CloneArgs>(), 88);
        assert_eq!(std::mem::align_of::<CloneArgs>(), 8);
        assert_eq!(std::mem::offset_of!(CloneArgs, exit_signal), 32);
        assert_eq!(std::mem::offset_of!(CloneArgs, cgroup), 80);
    }

    /// Runs `clone` (one of the two calls, without namespace flags or a
    /// cgroup): it must behave exactly like `fork`. It's called once; its
    /// child only calls `_exit`.
    #[allow(clippy::zombie_processes, reason = "reaped with waitpid below")]
    fn works_like_fork(clone: impl FnOnce() -> Result<Cloned, Errno>) {
        match clone().unwrap() {
            // SAFETY: `_exit` is async-signal-safe.
            Cloned::Child => unsafe { libc::_exit(7) },
            Cloned::Parent { pid, in_cgroup } => {
                assert!(!in_cgroup, "no cgroup was asked for");
                let status = nix::sys::wait::waitpid(pid, None).unwrap();
                assert_eq!(status, nix::sys::wait::WaitStatus::Exited(pid, 7));
            }
        }
    }

    #[test]
    fn clone3_without_namespace_flags_works_like_fork() {
        // Some environments have no clone3 at all (old kernels, seccomp,
        // emulators such as QEMU): that's what the fallback is for.
        // SAFETY: a probe that changes nothing: an invalid size fails at once.
        let probe = unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null::<CloneArgs>(), 0) };
        if probe == -1 && Errno::last() == Errno::ENOSYS {
            eprintln!("no clone3 here; only the fallback can be tested");
            return;
        }
        // SAFETY: the child only calls `_exit` (see `works_like_fork`).
        works_like_fork(|| unsafe { clone3(0, None) });
    }

    /// The fallback path, which normal hosts never take.
    #[test]
    fn the_clone_fallback_works_like_fork() {
        // SAFETY: the child only calls `_exit` (see `works_like_fork`).
        works_like_fork(|| unsafe { clone_legacy(0) });
    }

    #[test]
    fn the_whole_path_works_like_fork() {
        // SAFETY: the child only calls `_exit` (see `works_like_fork`).
        works_like_fork(|| unsafe { clone_process(0, None) });
    }
}
