//! The container's clean image: it sets the container up, then becomes its
//! init, PID 1 of a new PID namespace (Linux; `pid_namespaces(7)`).
//!
//! **Re-exec first** (runc-style). The cloned child re-execs the shipyard
//! binary at once, through the hidden [`entry_point`], while it still sees
//! the host's files. So this is a fresh, single-threaded process: none of
//! shipyard's command line, environment or memory, and ordinary Rust (it
//! can allocate and report). It gets its instructions in a sealed memfd
//! (`setup.rs`), and does the container setup itself: a session, mount
//! propagation, `/proc`, the hostname.
//!
//! **Then the init.** The kernel treats a namespace's PID 1 specially: a
//! signal it has no handler for is dropped, even SIGTERM and SIGINT. So the
//! command can't be PID 1 itself (a plain `sleep` would ignore Ctrl-C).
//! Instead this process is PID 1 and forks the command as PID 2, with
//! ordinary signal rules. Then it:
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
//! Without a PID namespace there's no init: after the setup, this process
//! becomes the command itself.

use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd as _, IntoRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use nix::errno::Errno;
use nix::mount::{MsFlags, mount};

use super::channel::{Report, ReportReader, START_MARK, encode_status, report_pipe};
use super::child::{Failure, SETUP_FAILED, command_main, report_failure, write_record};
use super::failure::{ChildStep, FailureContext, describe};
use super::plan::ChildPlan;
use super::setup::{is_sealed_setup, read_setup};
use super::status::ExitStatus;
use super::{identity, rootfs, sysctl};

/// Where the shipyard binary starts as a container's clean image. Does
/// nothing unless this process was started that way (by `child.rs`, with
/// the hidden first argument); otherwise it never returns.
///
/// Everything else was set up before the exec: every signal is still
/// blocked (the mask survives `exec`), pending ones are kept, and the
/// process inherited exactly the setup, status and report descriptors.
/// (Ignored signals stay ignored across `exec`: a SIGHUP ignored under
/// `nohup`, say.)
pub(crate) fn entry_point() {
    let mut args = std::env::args_os().skip(1);
    let entry = std::ffi::OsStr::from_bytes(super::INIT_ENTRY_ARG.to_bytes());
    if args.next().as_deref() != Some(entry) {
        return;
    }
    let mut fd = || args.next().and_then(|a| a.to_str()?.parse::<RawFd>().ok());
    if let (Some(setup), Some(status), Some(report)) = (fd(), fd(), fd())
        && is_sealed_setup(setup)
    {
        let sigpipe = std::ffi::OsStr::from_bytes(super::SIGPIPE_PENDING_ARG.to_bytes());
        let sigpipe_was_pending = args.next().as_deref() == Some(sigpipe);
        run(setup, status, report, sigpipe_was_pending);
    }
    // Started by hand, or by a broken caller.
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        b"shipyard: this is the container init's internal entry point\n",
    );
    std::process::exit(125);
}

/// Sets the container up, then runs the command: as the init's child (PID
/// 2), or as this very process without a PID namespace.
fn run(setup_fd: RawFd, status: RawFd, report: RawFd, sigpipe_was_pending: bool) -> ! {
    // Not dumpable, from the start: `/proc/1` (its descriptors, among them
    // the pipes to shipyard, and its memory) is then only for processes
    // with CAP_SYS_PTRACE, as runc does for its init, and that's true
    // before the command runs, not only once this process waits for it.
    // (Root inside has that capability until stage 4 drops it.) The
    // command's exec makes it dumpable again, as any program.
    let _ = nix::sys::prctl::set_dumpable(false);
    // Exec'd through a descriptor, the process is named after the number or
    // the binary; give it a name that says what it is (`ps`, `top`), from
    // the setup on, while a runtime container waits at its gate too.
    let _ = nix::sys::prctl::set_name(c"shipyard-init");
    if sigpipe_was_pending {
        // Discarded by Rust's startup (it ignores SIGPIPE): raised again.
        // Every signal is still blocked, and a blocked signal is queued even
        // while it's ignored (checked), so it's forwarded to the command
        // like any other.
        let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGPIPE);
    }
    // Passed across the exec, so not close-on-exec: make them so again,
    // or the command would inherit them.
    for fd in [status, report] {
        let _ = nix::fcntl::fcntl(
            // SAFETY: inherited across the exec; borrowed only for this call.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
    }
    report_panics_to(report);
    let fail = |failure: Failure| -> ! {
        report_failure(report, &failure);
        std::process::exit(SETUP_FAILED);
    };
    let setup =
        read_setup(setup_fd).unwrap_or_else(|errno| fail(Failure::at(ChildStep::ReadSetup, errno)));
    if let Some(gate) = setup.gate {
        // Passed across the exec like the others: close-on-exec again.
        let _ = nix::fcntl::fcntl(
            // SAFETY: inherited across the exec; borrowed only for this call.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(gate) },
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
    }
    // The proof that this image started: without it, shipyard reports an
    // init that never ran (`Error::InitDidNotStart`).
    write_record(status, &[START_MARK]);

    set_up(&setup).unwrap_or_else(|failure| fail(failure));
    // The command's plan is built here, inside the container: its PATH
    // lookup, credentials and limits. Shipyard built the same plan from the
    // same spec before the clone, so only a corrupt setup fails here.
    let mut spec = setup
        .spec()
        .unwrap_or_else(|_| fail(Failure::at(ChildStep::ReadSetup, Errno::EINVAL)));
    // User and group names, in the container's own /etc (its root
    // filesystem's, or the host's without one).
    identity::resolve(&mut spec, Some(Path::new("/etc"))).unwrap_or_else(|err| {
        fail(Failure::at(ChildStep::UserLookup, err.errno).detail(err.which.detail()))
    });
    let (plan, context) =
        ChildPlan::for_container_command(&spec, setup.keep_ignored.clone(), setup.parent)
            .unwrap_or_else(|_| fail(Failure::at(ChildStep::ReadSetup, Errno::EINVAL)));
    // With a start gate (`shipyard-runtime create`): everything that can
    // fail before the command has been reported by now. Closing the report
    // pipe says "set up"; then wait for `start`. Nobody reads that pipe
    // afterwards: the command reports to the init instead, which explains
    // a failure on the container's stderr.
    let (report, command_report) = match setup.gate {
        Some(gate) => {
            PANIC_REPORT.store(-1, Ordering::Relaxed);
            let _ = nix::unistd::close(report);
            wait_at_gate(gate);
            match report_pipe() {
                Ok((read, write)) => (write.into_raw_fd(), Some(read)),
                Err(_) => (-1, None),
            }
        }
        None => (report, None),
    };

    if !setup.init {
        // No PID namespace, so no init: this process becomes the command.
        // SAFETY: a single-threaded process; `plan` is the command's.
        unsafe { command_main(&plan, report) }
    }
    // SAFETY: this process is single-threaded (a fresh image that started
    // no threads), so the child may run ordinary code; it runs only
    // `command_main`, which follows the after-fork rules anyway.
    let command = match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // SAFETY: right after `fork`, with every signal still blocked.
            unsafe { command_main(&plan, report) }
        }
        Ok(nix::unistd::ForkResult::Parent { child }) => child.as_raw(),
        Err(errno) => fail(Failure::at(ChildStep::InitFork, errno)),
    };
    let (report, exec_failure) = match command_report {
        Some(read) => {
            // Our copy closed first, or the pipe never reaches end-of-file.
            let _ = nix::unistd::close(report);
            // Closed already: `become_init` must not close it (or a reused
            // descriptor number) again.
            (-1, explain_failure(read, &context))
        }
        None => (report, None),
    };
    become_init(command, status, report, exec_failure)
}

/// Where a panic in this process is reported (-1: nowhere, once the report
/// pipe is the command's alone).
static PANIC_REPORT: AtomicI32 = AtomicI32::new(-1);

/// Makes a panic here (a bug: nothing in the setup should panic) a setup
/// failure that shipyard reports as such. Otherwise, with the start mark
/// already sent, its exit would pass for the command's: exit code 101, or
/// SIGABRT in release builds (`panic = "abort"`). The default hook still
/// prints the panic. Once nobody reads the report (after the start gate,
/// or once the command runs), the exit code alone says it: 125, a setup
/// failure, never the command's.
fn report_panics_to(report: RawFd) {
    PANIC_REPORT.store(report, Ordering::Relaxed);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = PANIC_REPORT.swap(-1, Ordering::Relaxed);
        if report >= 0 {
            report_failure(
                report,
                &Failure::at(ChildStep::InitPanicked, Errno::ENOTRECOVERABLE),
            );
        }
        default(info);
        std::process::exit(SETUP_FAILED);
    }));
}

/// Waits until the command execs (end-of-file on its report pipe) or
/// reports a failure, and then writes the failure, explained, to the
/// container's stderr: after `start` nobody else is left to tell. Before
/// the init's stdio becomes `/dev/null`. Returns the exit code the failure
/// stands for (127 not found, 126 can't execute, 125 setup), which the
/// command's own exit (always 125) doesn't say.
fn explain_failure(read: std::os::fd::OwnedFd, context: &FailureContext) -> Option<u8> {
    let Ok(mut reader) = ReportReader::new(read) else {
        return None;
    };
    loop {
        match reader.read() {
            Ok(Report::Pending) => {
                let mut fds = [nix::poll::PollFd::new(
                    reader.fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                let _ = nix::poll::poll(&mut fds, nix::poll::PollTimeout::NONE);
            }
            Ok(Report::Failed(failure)) => {
                let error = describe(failure, context);
                let _ = writeln!(std::io::stderr(), "shipyard: {error}");
                return Some(error.exit_code());
            }
            Ok(Report::Running) | Err(_) => return None,
        }
    }
}

/// Blocks until `start` has taken a byte from the start gate (a FIFO, open
/// `O_PATH` at `gate`). Opened for writing through `/proc/self/fd`, which
/// reaches the FIFO even from inside the container's root. Every signal but
/// SIGKILL is blocked meanwhile, so it stays pending until the command
/// runs. If the gate can't be used, the container can't start: exit (the
/// report pipe is closed already; the exit status says).
///
/// Opening returns once a reader has opened the FIFO, even one that has
/// gone since (a `start` killed in between). So the byte is written, the
/// write end kept open until the reader leaves, and then the pipe asked
/// whether the byte is still there: taken, `start` happened; still there
/// (or no reader left to write to), the gate is waited at again.
fn wait_at_gate(gate: RawFd) {
    let path = format!("/proc/self/fd/{gate}");
    loop {
        let opened = nix::fcntl::open(
            path.as_str(),
            nix::fcntl::OFlag::O_WRONLY | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        );
        let Ok(fifo) = opened else {
            std::process::exit(SETUP_FAILED);
        };
        match nix::unistd::write(&fifo, b"0") {
            Ok(1) => {}
            Err(Errno::EPIPE) => {
                // Our own SIGPIPE: blocked, so it's pending even though it's
                // ignored, and would be forwarded to the command later.
                discard_pending(nix::sys::signal::Signal::SIGPIPE);
                continue;
            }
            _ => std::process::exit(SETUP_FAILED),
        }
        wait_for_no_reader(&fifo);
        if unread(&fifo) == Some(0) {
            break;
        }
    }
    let _ = nix::unistd::close(gate);
}

/// Waits until nobody has the pipe whose write end is `fifo` open for
/// reading (POLLERR on a write end).
fn wait_for_no_reader(fifo: &impl AsFd) {
    let mut fds = [nix::poll::PollFd::new(
        fifo.as_fd(),
        nix::poll::PollFlags::empty(),
    )];
    let _ = nix::poll::poll(&mut fds, nix::poll::PollTimeout::NONE);
}

/// How many bytes are waiting in the pipe `fifo` is an end of (`FIONREAD`).
fn unread(fifo: &impl AsFd) -> Option<libc::c_int> {
    let mut count: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int, to `count`.
    let asked = unsafe { libc::ioctl(fifo.as_fd().as_raw_fd(), libc::FIONREAD, &raw mut count) };
    (asked == 0).then_some(count)
}

/// Takes `signal` off this process's pending signals, if it's there.
fn discard_pending(signal: nix::sys::signal::Signal) {
    let mut set = nix::sys::signal::SigSet::empty();
    set.add(signal);
    let now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid signal set and timeout; no siginfo wanted. The zero
    // timeout makes `sigtimedwait` only look.
    unsafe { libc::sigtimedwait(set.as_ref(), std::ptr::null_mut(), &raw const now) };
}

/// The container setup, in the new namespaces: a session of its own (no
/// controlling terminal, its own process group), mounts that don't
/// propagate out, its own root filesystem or a `/proc` for the new PID
/// namespace, and the hostname.
fn set_up(setup: &super::setup::Setup) -> Result<(), Failure> {
    let at = |step| move |errno| Failure::at(step, errno);
    nix::unistd::setsid().map_err(at(ChildStep::Session))?;
    if setup.private_mounts {
        // First, before any mount: on hosts where `/` is shared (systemd),
        // a mount made here would otherwise appear on the host too. Slave,
        // not private (as runc does): nothing propagates out, but the
        // host's unmounts still propagate in, so a device unmounted on the
        // host isn't kept busy by the container's copy of the mount.
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_SLAVE,
            None::<&str>,
        )
        .map_err(at(ChildStep::MountPrivate))?;
    }
    if let Some(root) = &setup.root {
        // `/proc`, `/sys` and `/dev` inside it (and the sysctls, before its
        // `/proc/sys` is made read-only), then the switch.
        rootfs::enter(root, &setup.sysctls)?;
    } else {
        if setup.mount_proc {
            // Mounted by this process, so it shows the new PID namespace.
            mount(
                Some("proc"),
                "/proc",
                Some("proc"),
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
                None::<&str>,
            )
            .map_err(at(ChildStep::MountProc))?;
        }
        sysctl::apply_at(Path::new("/proc"), &setup.sysctls)?;
    }
    if let Some(hostname) = &setup.hostname {
        nix::unistd::sethostname(hostname).map_err(at(ChildStep::Hostname))?;
    }
    if let Some(domainname) = &setup.domainname {
        set_domainname(domainname).map_err(at(ChildStep::Domainname))?;
    }
    // Don't keep shipyard's working directory busy for the container's
    // life (an unmount of it would fail). The command changes to its own.
    // (With a root filesystem, already done by the switch.)
    let _ = std::env::set_current_dir("/");
    Ok(())
}

/// `setdomainname(2)`: the NIS domain name, in the container's UTS
/// namespace. (nix has no wrapper for it.)
fn set_domainname(name: &str) -> nix::Result<()> {
    // SAFETY: `name` is valid for `name.len()` bytes; the kernel copies them.
    let rc = unsafe { libc::setdomainname(name.as_ptr().cast(), name.len()) };
    Errno::result(rc).map(drop)
}

/// Becomes the container's init, once the command (PID `command`) runs.
///
/// `exec_failure`: the exit code to report for the command if it couldn't
/// be executed (see `explain_failure`).
fn become_init(command: libc::pid_t, status: RawFd, report: RawFd, exec_failure: Option<u8>) -> ! {
    // The command holds the report pipe now: shipyard sees end-of-file when
    // it execs, or its failure record.
    PANIC_REPORT.store(-1, Ordering::Relaxed);
    let _ = nix::unistd::close(report);
    // stdio: the terminal or pipes it inherited stay with the command, so a
    // reader sees end-of-file when the command closes its end. (Kept as they
    // are if /dev/null can't be opened.)
    if let Ok(null) = nix::fcntl::open(
        "/dev/null",
        nix::fcntl::OFlag::O_RDWR,
        nix::sys::stat::Mode::empty(),
    ) {
        for fd in 0..=2 {
            // SAFETY: `dup2` onto the standard descriptors, which this
            // process owns; nothing in it uses them any more.
            unsafe { libc::dup2(null.as_raw_fd(), fd) };
        }
    }
    // Rust's startup set SIGPIPE to ignored. A signal sent while it's
    // ignored and blocked is still queued on Linux (checked), but POSIX lets
    // a system discard it: back to the default. It stays blocked.
    // SAFETY: changing a disposition has no memory-safety conditions.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    wait_and_forward(command, status, exec_failure)
}

/// The init's loop: wait for any signal, reap on SIGCHLD, forward the rest.
fn wait_and_forward(command: libc::pid_t, status_pipe: RawFd, exec_failure: Option<u8>) -> ! {
    let mut all = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigfillset` initialises the set. Every signal is blocked
    // already (since before the clone); this makes sure of it.
    let all = unsafe {
        libc::sigfillset(all.as_mut_ptr());
        libc::sigprocmask(libc::SIG_SETMASK, all.as_ptr(), std::ptr::null_mut());
        all.assume_init()
    };
    loop {
        // SAFETY: `all` is initialised and no siginfo is asked for.
        let signal = unsafe { libc::sigwaitinfo(&raw const all, std::ptr::null_mut()) };
        match signal {
            libc::SIGCHLD => reap(command, status_pipe, exec_failure),
            -1 if Errno::last() == Errno::EINTR => {}
            // Can't happen with a valid set. Spinning on it would burn a
            // CPU forever; ending the container is the lesser evil.
            -1 => std::process::exit(SETUP_FAILED),
            // A forwarded signal: on to the command. (Never re-raised here:
            // PID 1 would drop it.)
            // SAFETY: `command` is our child.
            other => unsafe {
                libc::kill(command, other);
            },
        }
    }
}

/// Reaps every child that has exited (signals coalesce: one SIGCHLD can
/// stand for many). When it's the command, reports its status and exits.
fn reap(command: libc::pid_t, status_pipe: RawFd, exec_failure: Option<u8>) {
    loop {
        let mut raw = 0;
        // SAFETY: `raw` is valid storage.
        let pid = unsafe { libc::waitpid(-1, &raw mut raw, libc::WNOHANG) };
        if pid == command {
            // A command that couldn't be executed exited 125 ("setup
            // failed"); its exit is the failure's code (as a wait status).
            if let Some(code) = exec_failure {
                raw = libc::c_int::from(code) << 8;
            }
            write_record(status_pipe, &encode_status(raw));
            // The init's own exit code mirrors the command's: shipyard checks
            // the record against it. Without WUNTRACED, `waitpid` reports
            // only exits and deaths, which always decode; 1 just makes the
            // impossible case a failure.
            let code = ExitStatus::from_wait_status(raw).map_or(1, ExitStatus::code);
            // `_exit`: nothing to flush, and the kernel then kills
            // everything left in the namespace.
            // SAFETY: ends the process at once.
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
