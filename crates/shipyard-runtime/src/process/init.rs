//! The container's init: PID 1 of a new PID namespace (Linux;
//! `pid_namespaces(7)`).
//!
//! The kernel treats a namespace's PID 1 specially: a signal it has no
//! handler for is dropped, even SIGTERM and SIGINT. So the command can't be
//! PID 1 itself (a plain `sleep` would ignore Ctrl-C). Instead this init is
//! PID 1 and the command is PID 2, with ordinary signal rules. The init:
//!
//! - **blocks every signal and waits for them with `sigwaitinfo`.** Blocked
//!   signals are queued even for PID 1 (checked on a real kernel: see
//!   `docs/stages/02-namespaces.md`);
//! - **forwards each one to the command;**
//! - **reaps every child**, including orphans re-parented to it, so no
//!   zombies pile up;
//! - **when the command exits,** hands its exact wait status to shipyard
//!   over the status pipe (PID 1 can't die "by" a signal, so its own exit
//!   status couldn't say "killed by SIGINT") and exits. The kernel then kills
//!   whatever is left in the namespace.
//!
//! **A clean process.** The init starts as a copy of shipyard (the `clone`),
//! which carries shipyard's command line (`-e TOKEN=…` included, readable by
//! every process in the container through `/proc/1/cmdline`), its
//! environment and its memory. So once the command is started, the init
//! re-execs the shipyard binary through the hidden [`entry_point`]: a fresh
//! image with a minimal command line, an empty environment and `/dev/null`
//! for stdio. Only if that exec fails does it go on as the copy.
//!
//! Up to the exec, this runs in the process that `clone` created, so the
//! rules of `child.rs` apply: no allocation, no panics, no locks, only
//! async-signal-safe calls.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;

use nix::errno::Errno;

use super::channel::{StartMark, encode_status};
use super::child::{Failure, SETUP_FAILED, prepare_and_exec, write_record};
use super::clone::{Cloned, clone_process};
use super::failure::ChildStep;
use super::plan::{ChildPlan, FdSweep, InitSetup};
use super::status::ExitStatus;

/// How many pending signals [`pass_on_pending`] passes on at most: more
/// than every standard signal, and a few queued real-time ones.
const PENDING_LIMIT: u32 = 64;

/// The command's PID inside the new namespace: the init's first child.
const COMMAND_PID: libc::pid_t = 2;

/// Becomes the container's init: starts the command (PID 2), which goes on
/// to set itself up and `exec`, and then supervises it until it exits. Only
/// returns on failure, in the command or if the command couldn't be started.
pub(super) fn run(plan: &ChildPlan, init: &InitSetup) -> Result<std::convert::Infallible, Failure> {
    // Our raw `clone` rather than the C library's `fork`, whose fork handlers
    // would run on the stale per-thread state a raw clone leaves behind
    // (see `clone.rs`). Without namespace flags it behaves like `fork`.
    // SAFETY: both sides only run code that follows this module's rules.
    match unsafe { clone_process(0) } {
        Err(errno) => Err(Failure::at(ChildStep::InitFork, errno)),
        Ok(Cloned::Child) => prepare_and_exec(plan),
        Ok(Cloned::Parent(command)) => {
            // Everything inherited except the status pipe and the binary: the
            // report pipe's write end (so shipyard sees end-of-file when the
            // command execs), the go pipe, and anything leaked by shipyard's
            // own parent, which the container's PID 1 would otherwise pin for
            // its whole life.
            close_all_but([init.status_write, init.exe], &plan.inherited_fds);
            // Don't keep shipyard's working directory busy for the
            // container's life (an unmount of it would fail).
            // SAFETY: `chdir` is async-signal-safe; the path is a literal.
            unsafe { libc::chdir(c"/".as_ptr()) };
            // The clean image is told the command is PID 2; it always is in a
            // fresh namespace, but don't rely on it unchecked.
            if command.as_raw() == COMMAND_PID {
                reexec(command.as_raw(), init);
            }
            // Only if the exec failed: carry on as the copy.
            wait_and_forward(command.as_raw(), init.status_write, StartMark::Copy)
        }
    }
}

/// Re-execs the shipyard binary as the clean init (see the module docs).
/// Returns only if the exec failed.
fn reexec(command: libc::pid_t, init: &InitSetup) {
    pass_on_pending(command);
    // stdio: the terminal or pipes it inherited stay with the command, so a
    // reader sees end-of-file when the command closes its end. (Kept as they
    // are if /dev/null can't be opened: Rust's startup needs 0-2 open.)
    // SAFETY: `open`, `dup2`, `close`, `fcntl`, `execveat` and `execve` are
    // async-signal-safe system calls. Every pointer passed is a
    // NUL-terminated string or a null-terminated array of them, prepared
    // before the fork and alive until the exec (or its failure).
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..=2 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
        // The status pipe must survive the exec; it's close-on-exec.
        libc::fcntl(init.status_write, libc::F_SETFD, 0);
        libc::syscall(
            libc::SYS_execveat,
            init.exe,
            c"".as_ptr(),
            init.argv.as_ptr(),
            init.env.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
        // No `execveat` (before Linux 3.19, seccomp, some emulators): the
        // same file through the container's own `/proc`.
        libc::execve(
            init.exe_path.as_ptr(),
            init.argv.as_ptr(),
            init.env.as_ptr(),
        );
    }
}

/// Sends the command whatever signals are already pending for the init,
/// before the exec: the new image's startup would lose some of them (Rust's
/// sets SIGPIPE to ignored, and making a signal ignored discards it even
/// while it's blocked). SIGCHLD stays pending: the new image reaps.
///
/// At most [`PENDING_LIMIT`] of them: a process inside that keeps
/// signalling PID 1 mustn't keep the init here instead of starting over
/// clean. (What's left stays pending, and the new image forwards it.)
fn pass_on_pending(command: libc::pid_t) {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigfillset` and `sigdelset` initialise and edit `set`;
    // `sigtimedwait` with a zero timeout only takes a pending signal, and
    // `kill` sends it on. All are async-signal-safe.
    unsafe {
        libc::sigfillset(set.as_mut_ptr());
        libc::sigdelset(set.as_mut_ptr(), libc::SIGCHLD);
        let set = set.assume_init();
        let now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        for _ in 0..PENDING_LIMIT {
            let signal = libc::sigtimedwait(&raw const set, std::ptr::null_mut(), &raw const now);
            if signal <= 0 {
                return;
            }
            libc::kill(command, signal);
        }
    }
}

/// Where the shipyard binary starts as a container's re-exec'd init. Does
/// nothing unless this process was started that way (by `init.rs`, with
/// the hidden first argument); otherwise it never returns.
///
/// A fresh process: everything about it was set up before the exec. Every
/// signal is still blocked (the mask survives `exec`), pending ones are kept,
/// and it inherited exactly the status pipe.
/// (Ignored signals stay ignored across `exec`, as the forked init had
/// them: a SIGHUP ignored under `nohup`, say.)
pub(crate) fn entry_point() {
    let mut args = std::env::args_os().skip(1);
    let entry = std::ffi::OsStr::from_bytes(super::INIT_ENTRY_ARG.to_bytes());
    if args.next().as_deref() != Some(entry) {
        return;
    }
    let status_pipe: Option<RawFd> = args.next().and_then(|a| a.to_str()?.parse().ok());
    let command: Option<libc::pid_t> = args.next().and_then(|a| a.to_str()?.parse().ok());
    // SAFETY: `getpid` has no preconditions.
    let is_pid_1 = unsafe { libc::getpid() } == 1;
    match (status_pipe, command) {
        (Some(status_pipe), Some(command)) if is_pid_1 => {
            // Exec'd through a descriptor, the process is named after its
            // number; give it a name that says what it is (`ps`, `top`).
            // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most
            // 16 bytes, which this is.
            unsafe { libc::prctl(libc::PR_SET_NAME, c"shipyard-init".as_ptr()) };
            // Rust's startup set SIGPIPE to ignored. A signal sent while
            // it's ignored and blocked is still queued on Linux (checked),
            // but POSIX lets a system discard it: back to the default. It
            // stays blocked. (Ones pending at the exec were passed on
            // before it: see `pass_on_pending`.)
            // SAFETY: changing a disposition has no memory-safety conditions.
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            wait_and_forward(command, status_pipe, StartMark::Clean)
        }
        _ => {
            // Started by hand, or by a broken caller.
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"shipyard: this is the container init's internal entry point\n",
            );
            std::process::exit(125);
        }
    }
}

/// The init's loop: wait for any signal, reap on SIGCHLD, forward the rest.
/// First it tells shipyard it runs, and whether it's the clean image.
fn wait_and_forward(command: libc::pid_t, status_pipe: RawFd, mark: StartMark) -> ! {
    // Not dumpable: `/proc/1` (its descriptors, its memory) is then only for
    // processes with CAP_SYS_PTRACE, as runc does for its init. Root inside
    // has that until capabilities are dropped (stage 4).
    // SAFETY: PR_SET_DUMPABLE takes a plain integer.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) };
    write_record(status_pipe, &[mark as u8]);
    let mut all = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigfillset` initialises the set; `sigprocmask` is
    // async-signal-safe. Every signal is blocked already (since before the
    // clone); this makes sure of it.
    let all = unsafe {
        libc::sigfillset(all.as_mut_ptr());
        libc::sigprocmask(libc::SIG_SETMASK, all.as_ptr(), std::ptr::null_mut());
        all.assume_init()
    };
    loop {
        // SAFETY: `sigwaitinfo` is async-signal-safe; `all` is initialised and
        // no siginfo is asked for.
        let signal = unsafe { libc::sigwaitinfo(&raw const all, std::ptr::null_mut()) };
        match signal {
            libc::SIGCHLD => reap(command, status_pipe),
            -1 if Errno::last() == Errno::EINTR => {}
            // Can't happen with a valid set. Spinning on it would burn a
            // CPU forever; ending the container is the lesser evil.
            // SAFETY: see `child.rs`'s `ExitOnUnwind`.
            -1 => unsafe { libc::_exit(SETUP_FAILED) },
            // A forwarded signal: on to the command. (Never re-raised here:
            // PID 1 would drop it.)
            // SAFETY: `kill` is async-signal-safe; `command` is our child.
            other => unsafe {
                libc::kill(command, other);
            },
        }
    }
}

/// Reaps every child that has exited (signals coalesce: one SIGCHLD can
/// stand for many). When it's the command, reports its status and exits.
fn reap(command: libc::pid_t, status_pipe: RawFd) {
    loop {
        let mut raw = 0;
        // SAFETY: `waitpid` is async-signal-safe; `raw` is valid storage.
        let pid = unsafe { libc::waitpid(-1, &raw mut raw, libc::WNOHANG) };
        if pid == command {
            send_status(status_pipe, raw);
            // The init's own exit code mirrors the command's: shipyard only
            // falls back to it when the status pipe said nothing. Without
            // WUNTRACED, `waitpid` reports only exits and deaths, which always
            // decode; 1 just makes the impossible case a failure.
            let code = ExitStatus::from_wait_status(raw).map_or(1, ExitStatus::code);
            // SAFETY: `_exit` is async-signal-safe. The kernel then kills
            // everything left in the namespace.
            unsafe { libc::_exit(libc::c_int::from(code)) }
        }
        match pid {
            -1 if Errno::last() == Errno::EINTR => {}
            // Nothing more right now (0), or no children left (ECHILD).
            0 | -1 => return,
            // An orphan that ended up with PID 1: reaped, nothing else.
            _ => {}
        }
    }
}

/// Sends the command's raw wait status to shipyard. Best effort: if
/// shipyard is gone, there's no one to tell.
fn send_status(status_pipe: RawFd, raw: libc::c_int) {
    write_record(status_pipe, &encode_status(raw));
}

/// Closes every descriptor from 3 up except the two in `keep`.
fn close_all_but(keep: [RawFd; 2], sweep: &FdSweep) {
    if close_ranges_around(keep) {
        return;
    }
    let close = |fd: RawFd| {
        if !keep.contains(&fd) {
            // SAFETY: `close` is async-signal-safe; an fd that isn't open
            // just gives EBADF.
            unsafe { libc::close(fd) };
        }
    };
    match sweep {
        FdSweep::Listed(fds) => fds.iter().copied().for_each(close),
        FdSweep::Below(bound) => (3..*bound).for_each(close),
    }
}

/// `close_range` over everything from 3 up but `keep`. False if the call
/// isn't available (old kernel, seccomp), so the caller falls back.
fn close_ranges_around(keep: [RawFd; 2]) -> bool {
    let [first, second] = keep;
    let (Ok(first), Ok(second)) = (u32::try_from(first), u32::try_from(second)) else {
        return false;
    };
    ranges_around(first.min(second), first.max(second))
        .into_iter()
        .flatten()
        .all(|(first, last)| {
            // SAFETY: `close_range` only closes descriptors; the arguments are
            // plain integers.
            let rc = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0_u32) };
            rc == 0
        })
}

/// The descriptor ranges from 3 to the maximum that leave out `low` and
/// `high` (`low <= high`; they may be the same).
fn ranges_around(low: u32, high: u32) -> [Option<(u32, u32)>; 3] {
    // From `first` to just before `end`, if that's not empty (and not below 3).
    let span = |first: u32, end: u32| {
        let first = first.max(3);
        end.checked_sub(1)
            .filter(|&last| last >= first)
            .map(|last| (first, last))
    };
    let after = |fd: u32| fd.checked_add(1);
    [
        span(3, low),
        after(low).and_then(|first| span(first, high)),
        after(high).map(|first| (first.max(3), u32::MAX)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_leave_out_only_the_kept_descriptors() {
        assert_eq!(
            ranges_around(5, 10),
            [Some((3, 4)), Some((6, 9)), Some((11, u32::MAX))]
        );
        // Next to each other, and next to fd 3: no empty ranges.
        assert_eq!(ranges_around(3, 4), [None, None, Some((5, u32::MAX))]);
        // The same descriptor twice.
        assert_eq!(
            ranges_around(7, 7),
            [Some((3, 6)), None, Some((8, u32::MAX))]
        );
        assert_eq!(
            ranges_around(4, u32::MAX),
            [Some((3, 3)), Some((5, u32::MAX - 1)), None]
        );
    }
}
