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
use std::os::fd::{AsFd, AsRawFd as _, FromRawFd as _, IntoRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use nix::errno::Errno;
use nix::mount::{MsFlags, mount};

use super::channel::{
    GAVE_UP_MARK, Report, ReportReader, START_MARK, decode_record, encode, encode_status,
    report_pipe,
};
use super::child::{
    Failure, SETUP_FAILED, command_main, report_failure, wait_for_go, write_record,
};
use super::failure::UnmappedId;
use super::failure::{ChildFailure, ChildStep, FailureContext, describe};
use super::plan::ChildPlan;
use super::setup::{HooksSetup, NetworkSetup, is_sealed_setup, read_setup};
use super::status::ExitStatus;
use super::{identity, rootfs, sysctl};
use crate::container::{State, Status};
use crate::hooks::{self, HookError, Phase};
use crate::isolation::IdMaps;
use crate::security::seccomp::SeccompError;
use crate::security::{Confinement, ConfinementError, SeccompFilter};

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
    // As for the CLI itself: a privileged start (setuid, file capabilities)
    // gets no further. Anyone can start this entry point by hand, before the
    // CLI's own check would run.
    if let Err(err) = super::refuse_privileged_start() {
        let _ = writeln!(std::io::stderr(), "shipyard-init: {err}");
        std::process::exit(125);
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

/// Marks close-on-exec again the descriptors the setup names that were
/// passed across the exec (the gate, the hooks pipe, an overlay to
/// attach): the command must not inherit them.
fn close_on_exec_again(setup: &super::setup::Setup) {
    let hooks_pipe = setup.hooks.as_ref().map(|hooks| hooks.pipe);
    let attach = setup
        .root
        .as_ref()
        .and_then(super::setup::RootSetup::attach);
    let terminal = setup.terminal.as_ref().map(|terminal| terminal.socket);
    for fd in setup
        .gate
        .into_iter()
        .chain(hooks_pipe)
        .chain(attach)
        .chain(terminal)
    {
        let _ = nix::fcntl::fcntl(
            // SAFETY: inherited across the exec; borrowed only for this call.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
    }
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
    close_on_exec_again(&setup);
    // The proof that this image started: without it, shipyard reports an
    // init that never ran (`Error::InitDidNotStart`).
    write_record(status, &[START_MARK]);

    // The kernel's last capability, read before any mount: `/proc` is still
    // the host's, and a bundle may mask `/proc/sys` in the container.
    let last_cap =
        cap_last_cap().unwrap_or_else(|errno| fail(Failure::at(ChildStep::CapLastCap, errno)));
    // Whether the user namespace lets the command set its groups, read
    // before `/proc` is the container's too.
    let setgroups_allowed = super::user_namespace::setgroups_allowed();
    // The container's state, if it stopped for hooks: `startContainer`'s
    // too.
    let state = set_up(&setup, report).unwrap_or_else(|failure| fail(failure));
    // A keyring of the container's own, not its caller's: the init joins
    // it, and the command inherits it.
    join_session_keyring()
        .unwrap_or_else(|errno| fail(Failure::at(ChildStep::SessionKeyring, errno)));
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
    // In a user namespace, an ID outside its maps doesn't exist: switching
    // to it would fail with a bare EINVAL.
    if let Some(maps) = setup
        .user_namespace
        .as_ref()
        .map(|namespace| &namespace.maps)
    {
        check_mapped(&spec.user, maps).unwrap_or_else(|failure| fail(failure));
    }
    // Where `setgroups` is denied, the command keeps the groups it has; it
    // can't be given others.
    if !setgroups_allowed && let Some(&gid) = spec.user.additional_gids.first() {
        let gid = usize::try_from(gid).unwrap_or(usize::MAX);
        fail(Failure::at(ChildStep::GroupsDenied, Errno::EPERM).detail(gid));
    }
    // What the command may do, for the user it turned out to be, on this
    // kernel.
    let confinement = Confinement::new(&setup.security, spec.user.uid, last_cap, kernel_version())
        .unwrap_or_else(|err| fail(confinement_failure(&err)));
    let (mut plan, mut context) = ChildPlan::for_container_command(
        &spec,
        setup.keep_ignored.clone(),
        setup.parent,
        confinement,
        setgroups_allowed,
    )
    .unwrap_or_else(|_| fail(Failure::at(ChildStep::ReadSetup, Errno::EINVAL)));
    context.namespace_root = setup
        .user_namespace
        .as_ref()
        .and_then(|namespace| namespace.maps.host_uid(0));
    // The command's terminal, made here from the container's own devpts,
    // its master relayed by shipyard (`-t`).
    let slave = setup.terminal.as_ref().map(|terminal| {
        make_terminal(terminal, spec.user.uid, spec.user.gid)
            .unwrap_or_else(|failure| fail(failure))
    });
    plan.terminal = slave.as_ref().map(std::os::fd::AsRawFd::as_raw_fd);
    // With an init, its command waits until the init has confined itself
    // (`confine_self`): nothing in the container runs beside an init that
    // still has root's power. (Made before the start gate, so a failure is
    // `create`'s.)
    let go = setup.init.then(|| {
        nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .unwrap_or_else(|errno| fail(Failure::at(ChildStep::Handshake, errno)))
    });
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
    if let (Some(hooks), Some(state)) = (&setup.hooks, state)
        && let Err(failure) = start_container(hooks, state, &plan, last_cap)
    {
        let read = setup.gate.is_none().then_some(report);
        start_container_failed(failure, read, hooks, &mut context);
    }

    let Some(go) = go else {
        // No PID namespace, so no init: this process becomes the command.
        // SAFETY: a single-threaded process; `plan` is the command's.
        unsafe { command_main(&plan, report) }
    };
    let command = start_command(&plan, report, go, last_cap)
        .unwrap_or_else(|errno| fail(Failure::at(ChildStep::InitFork, errno)));
    // The command has its terminal: the init keeps no part of it.
    drop(slave);
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

/// The command's pseudo-terminal (TLPI ch. 64): made from the container's
/// own devpts (its root filesystem's, or the one `own_terminals` mounts),
/// sized as the user's terminal (a new one is 0x0: probe T7), its slave
/// opened from the master, never by path (another devpts could be mounted
/// over it: T3), and given to the command's user, as runc does, so programs
/// that open their terminal by name can. Everything `O_NOCTTY`: this
/// process leads a session, and opening a terminal without it would make it
/// the init's (T2). The master goes to shipyard (`SCM_RIGHTS`, T6), which
/// says when it's relaying it: only then may the command write to it.
/// Returns the slave, for the command.
fn make_terminal(
    terminal: &super::setup::TerminalSetup,
    uid: u32,
    gid: u32,
) -> Result<OwnedFd, Failure> {
    use nix::fcntl::OFlag;
    let step =
        |detail: usize| move |errno| Failure::at(ChildStep::TerminalOpen, errno).detail(detail);
    let master = nix::fcntl::open(
        "/dev/pts/ptmx",
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(step(0))?;
    let unlock: libc::c_int = 0;
    // SAFETY: `TIOCSPTLCK` reads one int, which `unlock` is.
    Errno::result(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSPTLCK, &raw const unlock) })
        .map_err(step(1))?;
    super::terminal::set_window_size(master.as_raw_fd(), (terminal.rows, terminal.cols))
        .map_err(step(2))?;
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: `TIOCGPTPEER` takes the open flags and returns a new
    // descriptor, which nothing else owns.
    let slave = Errno::result(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, flags) })
        .map_err(step(3))?;
    // SAFETY: just returned by the kernel, ours alone.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    nix::unistd::fchown(
        &slave,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(step(4))?;
    // SAFETY: inherited across the exec (see `Setup::terminal`), and ours.
    let socket = unsafe { OwnedFd::from_raw_fd(terminal.socket) };
    let sent = |errno| Failure::at(ChildStep::TerminalSend, errno);
    super::terminal::send_fd(socket.as_fd(), master.as_fd()).map_err(sent)?;
    drop(master);
    // Shipyard's byte: the user's terminal is raw (if asked), and the
    // master relayed. End-of-file: shipyard is gone.
    let mut byte = [0_u8; 1];
    loop {
        match nix::unistd::read(&socket, &mut byte) {
            Ok(1) => return Ok(slave),
            Ok(_) => return Err(sent(Errno::EPIPE).detail(1)),
            Err(Errno::EINTR) => {}
            Err(errno) => return Err(sent(errno).detail(1)),
        }
    }
}

/// Without a root filesystem of its own, the container sees the host's
/// `/dev`: its `/dev/pts` would name the host's terminals, the user's among
/// them, which the container could open by path (and change, or read keys
/// from) though it was never given one (D16). A devpts instance of its own
/// over it shows only the container's, as a root filesystem's `/dev` does
/// (`rootfs.rs`); and its `ptmx` over `/dev/ptmx`, so programs that make a
/// terminal (`script`, `tmux`) make it there. Only in this mount namespace:
/// the host's are untouched.
fn own_terminals() -> Result<(), Errno> {
    let mount_devpts = |options: &str| {
        mount(
            Some("devpts"),
            "/dev/pts",
            Some("devpts"),
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            Some(options),
        )
    };
    // The `tty` group, where the user namespace maps it (probe T1: EINVAL
    // where it doesn't).
    match mount_devpts("newinstance,ptmxmode=0666,mode=0620,gid=5") {
        Err(Errno::EINVAL) => mount_devpts("newinstance,ptmxmode=0666,mode=0620")?,
        result => result?,
    }
    mount(
        Some("/dev/pts/ptmx"),
        "/dev/ptmx",
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
}

/// Forks the command, PID 2. It waits on the go pipe (read, write ends)
/// while this process confines itself (`confine_self`), then goes on with
/// `plan`. If the init can't confine itself, the command is killed before
/// it goes on. Returns its PID.
fn start_command(
    plan: &ChildPlan,
    report: RawFd,
    (go_read, go_write): (OwnedFd, OwnedFd),
    last_cap: u8,
) -> Result<libc::pid_t, Errno> {
    // SAFETY: this process is single-threaded (a fresh image that started
    // no threads), so the child may run ordinary code; it runs only
    // `command_main`, which follows the after-fork rules anyway.
    let command = match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Child => {
            drop(go_write);
            if let Err(failure) = wait_for_go(go_read.as_raw_fd()) {
                report_failure(report, &failure);
                // SAFETY: a forked child, which must not run the init's
                // exit handlers.
                unsafe { libc::_exit(SETUP_FAILED) }
            }
            drop(go_read);
            // SAFETY: right after `fork`, with every signal still blocked.
            unsafe { command_main(plan, report) }
        }
        nix::unistd::ForkResult::Parent { child } => child.as_raw(),
    };
    drop(go_read);
    let filter = plan
        .confinement
        .as_ref()
        .and_then(|confinement| confinement.seccomp.as_ref());
    let Err(unconfined) = confine_self(last_cap, filter) else {
        write_record(go_write.as_raw_fd(), &[0]);
        return Ok(command);
    };
    // The command, still waiting for the go byte, never goes on.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(command),
        nix::sys::signal::Signal::SIGKILL,
    );
    match unconfined {
        // An init with every capability must not stay: the reason goes to
        // the container's stderr (still the init's here).
        Unconfined::Capabilities => {
            let _ = writeln!(
                std::io::stderr(),
                "shipyard: the container's init couldn't drop its own capabilities; stopping"
            );
            std::process::exit(SETUP_FAILED);
        }
        // The kernel refused the filter (under x86-64 emulation, or an
        // action newer than it): the command would meet the same refusal.
        // Reported as its own failure would be, with the same explanation.
        Unconfined::Filter(failure) => report_failure(report, &failure),
    }
    Ok(command)
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
            // The command never stops for hooks: only the clean image does.
            Ok(Report::Running | Report::AtHooks) | Err(_) => return None,
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
                super::discard_pending(nix::sys::signal::Signal::SIGPIPE);
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

/// The container setup, in the new namespaces: a session of its own (no
/// controlling terminal, its own process group), mounts that don't
/// propagate out, its own root filesystem or a `/proc` for the new PID
/// namespace, its loopback interface, and the hostname. With hooks, it
/// stops once the mounts are made ([`stop_for_hooks`], with `report`), and
/// returns the state they got.
fn set_up(setup: &super::setup::Setup, report: RawFd) -> Result<Option<State>, Failure> {
    let at = |step| move |errno| Failure::at(step, errno);
    nix::unistd::setsid().map_err(at(ChildStep::Session))?;
    if setup.cgroup.namespace {
        // Here, not at the clone: this process is in the container's cgroup
        // by now (born there, or moved there before the go byte), so that's
        // the namespace's root, and `/proc/self/cgroup` says `/` (G10).
        nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWCGROUP)
            .map_err(at(ChildStep::CgroupNamespace))?;
    }
    if setup.network == NetworkSetup::Own {
        // A new network namespace has only `lo`, and it's down: programs
        // that talk to themselves over 127.0.0.1 need it up. Before the
        // hooks, so `createContainer` sees it too.
        bring_loopback_up().map_err(at(ChildStep::Loopback))?;
    }
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
    // Once the mounts are made: the hooks' state, if there are hooks.
    let stop = || {
        setup
            .hooks
            .as_ref()
            .map(|hooks| stop_for_hooks(report, hooks))
            .transpose()
    };
    let state = if let Some(root) = &setup.root {
        // `/proc`, `/sys` and `/dev` inside it (and the sysctls, before its
        // `/proc/sys` is made read-only), the hooks, then the switch.
        rootfs::enter(root, setup.cwd(), &setup.sysctls, setup.cgroup.mount, stop)?
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
        if setup.private_mounts {
            own_terminals().map_err(at(ChildStep::OwnTerminals))?;
        }
        if let Some(cgroup) = setup.cgroup.mount {
            mount_cgroup_over_the_hosts(cgroup).map_err(at(ChildStep::MountCgroup))?;
        }
        stop()?
    };
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
    Ok(state)
}

/// Brings up `lo` in this process's network namespace: a request over
/// rtnetlink (this process is single-threaded, and already inside, with its
/// capabilities there). A failure that isn't the kernel's refusal (no
/// answer, one that can't be read) is reported as EIO, for want of an errno.
fn bring_loopback_up() -> Result<(), nix::errno::Errno> {
    use shipyard_network::route::{LinkChange, Rtnetlink};
    /// The loopback interface is always the first a namespace gets (the
    /// kernel registers it first: `loopback_net_init`).
    const LOOPBACK: u32 = 1;
    let mut rtnetlink = Rtnetlink::new().map_err(|error| error.errno().unwrap_or(Errno::EIO))?;
    rtnetlink
        .change(LOOPBACK, LinkChange::Up(true))
        .map_err(|error| error.errno().unwrap_or(Errno::EIO))
}

/// Stops for the runtime's hooks: says so on the `report` pipe, then waits
/// on the hooks pipe for the container's state (`prestart` and
/// `createRuntime` ran), and runs `createContainer` with it (its PID now
/// the init's own, in the container's PID namespace), here: in the
/// container's namespaces, before the root is switched. End-of-file with no
/// state means a runtime hook failed (the runtime says which): exit, as a
/// setup that failed.
fn stop_for_hooks(report: RawFd, hooks: &HooksSetup) -> Result<State, Failure> {
    use std::io::Read as _;
    write_record(report, &encode(ChildStep::AtHooks, 0, 0));
    // SAFETY: inherited across the exec, and only read here, once.
    let mut pipe = unsafe { std::fs::File::from_raw_fd(hooks.pipe) };
    let mut state = Vec::new();
    // No deadline of its own: shipyard writes it once its hooks ran (each
    // bounded by its OCI `timeout`; one without may run as long as it
    // likes, as runc's), and end-of-file comes if shipyard dies.
    pipe.read_to_end(&mut state)
        .map_err(|err| Failure::at(ChildStep::Handshake, super::io_errno(&err)))?;
    drop(pipe);
    if state.is_empty() {
        std::process::exit(SETUP_FAILED);
    }
    let mut state: State = serde_json::from_slice(&state)
        .map_err(|_| Failure::at(ChildStep::Handshake, Errno::EPROTO))?;
    // The hooks from here on run in the container's namespaces, so their
    // state has the init's PID as the container sees it (1, with a PID
    // namespace of its own), as the spec says, not the host's.
    state.pid = i32::try_from(std::process::id()).ok();
    hooks::run(Phase::CreateContainer, &hooks.create_container, &state)
        .map_err(|err| hook_failure(ChildStep::CreateContainerHook, &err))?;
    Ok(state)
}

/// Runs `startContainer`, inside the container, with `state` (now
/// "created"), and says how it went.
///
/// Not from this process: its programs come from the container's root
/// filesystem, which the image controls, and this process still has every
/// capability (a rootful container's are root's on the host) and no
/// seccomp filter. A child of its own runs them, confined first as the
/// init is about to be ([`confine_self`]: every capability gone but
/// `CAP_KILL`, `no_new_privs`, the container's filter), and reports a
/// failure as a record on a pipe of its own.
fn start_container(
    hooks: &HooksSetup,
    state: State,
    plan: &ChildPlan,
    last_cap: u8,
) -> Result<(), ChildFailure> {
    let state = State {
        status: Status::Created,
        ..state
    };
    let at = |step| {
        move |errno| ChildFailure {
            step,
            errno,
            detail: 0,
        }
    };
    let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .map_err(at(ChildStep::StartContainerHook))?;
    // SAFETY: this process is single-threaded (a fresh image that started
    // no threads), so the child may run ordinary code; it exits with
    // `_exit`, never returning here.
    let runner = match unsafe { nix::unistd::fork() }.map_err(at(ChildStep::InitFork))? {
        nix::unistd::ForkResult::Child => {
            drop(read);
            let filter = plan
                .confinement
                .as_ref()
                .and_then(|confinement| confinement.seccomp.as_ref());
            let failure = match confine_self(last_cap, filter) {
                Err(Unconfined::Capabilities) => {
                    Some(Failure::at(ChildStep::HookRunnerCapabilities, Errno::EPERM))
                }
                Err(Unconfined::Filter(failure)) => Some(failure),
                Ok(()) => hooks::run(Phase::StartContainer, &hooks.start_container, &state)
                    .err()
                    .map(|err| hook_failure(ChildStep::StartContainerHook, &err)),
            };
            if let Some(failure) = &failure {
                report_failure(write.as_raw_fd(), failure);
            }
            // SAFETY: a forked child, which must not run the init's exit
            // handlers.
            unsafe { libc::_exit(if failure.is_some() { SETUP_FAILED } else { 0 }) }
        }
        nix::unistd::ForkResult::Parent { child } => child,
    };
    drop(write);
    let mut record = Vec::new();
    // No deadline of its own: the runner applies each hook's OCI `timeout`
    // (one without may run as long as it likes, as runc's), and a deadline
    // here would end hooks the spec lets run.
    let _ = std::io::Read::read_to_end(&mut std::fs::File::from(read), &mut record);
    let ended = nix::sys::wait::waitpid(runner, None);
    if let Some(failure) = decode_record(&record) {
        return Err(failure);
    }
    match ended {
        Ok(nix::sys::wait::WaitStatus::Exited(_, 0)) => Ok(()),
        // Ended without saying why (killed: its seccomp filter, say).
        _ => Err(at(ChildStep::StartContainerHook)(Errno::ECHILD)),
    }
}

/// Ends the container over `failure` of `startContainer`: reported on the
/// `report` pipe while shipyard reads it (`Some`: no start gate); past the
/// gate, nobody does any more, so it's said on the container's stderr, and
/// the exit (125) says the rest.
fn start_container_failed(
    failure: ChildFailure,
    report: Option<RawFd>,
    hooks: &HooksSetup,
    context: &mut FailureContext,
) -> ! {
    if let Some(report) = report {
        report_failure(report, &failure.into());
    } else {
        context
            .hooks
            .start_container
            .clone_from(&hooks.start_container);
        let _ = writeln!(
            std::io::stderr(),
            "shipyard: {}",
            describe(failure, context)
        );
    }
    std::process::exit(SETUP_FAILED);
}

/// A hook's failure, as the report record that carries it.
fn hook_failure(step: ChildStep, err: &HookError) -> Failure {
    let (errno, detail) = err.to_record();
    Failure::at(step, errno).raw_detail(detail)
}

/// The container's own cgroup at `/sys/fs/cgroup`, without a root
/// filesystem: over the host's tree, which the container would otherwise see
/// (its own mount namespace: the host's view stays).
///
/// Not right on it: there's one cgroup2 superblock, and the kernel refuses
/// to mount a superblock on its own mount's root (EBUSY). So an empty,
/// read-only tmpfs covers it first, as `/sys`'s host mounts are covered
/// (`rootfs.rs`). Not unmounted instead: in a user namespace the kernel
/// locks the host's mounts.
fn mount_cgroup_over_the_hosts(cgroup: crate::isolation::CgroupMount) -> nix::Result<()> {
    const AT: &str = "/sys/fs/cgroup";
    let cover = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC | MsFlags::MS_RDONLY;
    mount(
        Some("tmpfs"),
        AT,
        Some("tmpfs"),
        cover,
        Some("size=4k,mode=755"),
    )?;
    mount(
        Some("cgroup2"),
        AT,
        Some("cgroup2"),
        cgroup.mount_flags(),
        None::<&str>,
    )
}

/// `setdomainname(2)`: the NIS domain name, in the container's UTS
/// namespace. (nix has no wrapper for it.)
fn set_domainname(name: &str) -> nix::Result<()> {
    // SAFETY: `name` is valid for `name.len()` bytes; the kernel copies them.
    let rc = unsafe { libc::setdomainname(name.as_ptr().cast(), name.len()) };
    Errno::result(rc).map(drop)
}

/// Refuses a command whose user, group or a supplementary group isn't
/// mapped in the container's user namespace.
fn check_mapped(user: &super::spec::User, maps: &IdMaps) -> Result<(), Failure> {
    let unmapped = |which: UnmappedId, id| {
        Failure::at(ChildStep::UnmappedId, Errno::EINVAL).detail(which.detail(id))
    };
    if maps.host_uid(user.uid).is_none() {
        return Err(unmapped(UnmappedId::User, user.uid));
    }
    if maps.host_gid(user.gid).is_none() {
        return Err(unmapped(UnmappedId::Group, user.gid));
    }
    match user
        .additional_gids
        .iter()
        .find(|&&gid| maps.host_gid(gid).is_none())
    {
        Some(&gid) => Err(unmapped(UnmappedId::SupplementaryGroup, gid)),
        None => Ok(()),
    }
}

/// Gives the container a session keyring of its own, as runc does
/// (`keyrings(7)`): a new, empty, anonymous one, which the command inherits
/// across `fork` and `exec`. Otherwise it would share its caller's, and with
/// it whatever keys the caller's login session holds (Docker's seccomp
/// profile refuses the keyring calls, but a bundle without a profile, or
/// `seccomp=unconfined`, doesn't).
///
/// Two refusals leave nothing to share, so the container goes on:
/// - ENOSYS: a kernel built without keyrings;
/// - EPERM: an outer seccomp filter refusing `keyctl` (shipyard itself in
///   a Docker container). The container inherits that filter, and can't
///   reach the old keyring either (step 8's probe, K4).
///
/// # Errors
///
/// Any other: EDQUOT when the user already has as many keys as the kernel
/// allows (K5).
fn join_session_keyring() -> Result<(), Errno> {
    // SAFETY: `keyctl(KEYCTL_JOIN_SESSION_KEYRING, NULL)`: a NULL name asks
    // for a new anonymous keyring; the kernel reads nothing through it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            libc::KEYCTL_JOIN_SESSION_KEYRING,
            std::ptr::null::<libc::c_char>(),
        )
    };
    match Errno::result(rc) {
        Ok(_) | Err(Errno::ENOSYS | Errno::EPERM) => Ok(()),
        Err(errno) => Err(errno),
    }
}

/// `/proc/sys/kernel/cap_last_cap`: the number of this kernel's last
/// capability (40 on Linux 5.9 and later).
fn cap_last_cap() -> Result<u8, Errno> {
    let text = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .map_err(|err| super::io_errno(&err))?;
    text.trim().parse().map_err(|_| Errno::EINVAL)
}

/// This kernel's `(major, minor)` version, for a seccomp profile's
/// `minKernel` conditions. `(0, 0)` if `uname` doesn't say: rules that need
/// a newer kernel are then left out, and a filter can only get stricter.
fn kernel_version() -> (u32, u32) {
    crate::kernel::version(&crate::kernel::release()).unwrap_or_default()
}

/// A confinement this kernel can't apply, as a setup failure.
fn confinement_failure(err: &ConfinementError) -> Failure {
    match err {
        ConfinementError::UnknownToKernel(capability) => {
            Failure::at(ChildStep::CapabilityUnknown, Errno::EINVAL)
                .detail(usize::from(capability.number()))
        }
        ConfinementError::KernelTooNew(last) => {
            Failure::at(ChildStep::TooManyCapabilities, Errno::ERANGE).detail(usize::from(*last))
        }
        ConfinementError::Seccomp(SeccompError::TooLong(len)) => {
            Failure::at(ChildStep::SeccompCompile, Errno::E2BIG).detail(*len)
        }
        ConfinementError::Seccomp(_) => Failure::at(ChildStep::SeccompCompile, Errno::EINVAL),
    }
}

/// The init gives up its own power before its command goes on: every
/// capability but `CAP_KILL` (to signal a command running as another user,
/// TLPI §20.5), and `no_new_privs`. Without `CAP_SYS_PTRACE`, its
/// non-dumpable flag then keeps root inside out of `/proc/1` (step 1's
/// probe, P1). Then `filter`, the container's own seccomp filter, as
/// Docker's init (tini) runs under it: whatever reaches the init through
/// the namespace meets the same rules as the command. (Without
/// `no_new_privs`, installing it would need `CAP_SYS_ADMIN`; the init has
/// it set by then.)
///
/// # Errors
///
/// What couldn't be done.
fn confine_self(last_cap: u8, filter: Option<&SeccompFilter>) -> Result<(), Unconfined> {
    Confinement::for_init(last_cap)
        .map_err(|err| confinement_failure(&err))
        .and_then(|init| {
            super::child::confine_before_switch(&init)?;
            super::child::confine_after_switch(&init)
        })
        .map_err(|_| Unconfined::Capabilities)?;
    filter
        .map_or(Ok(()), super::child::install_filter)
        .map_err(Unconfined::Filter)
}

/// Why the init couldn't confine itself.
enum Unconfined {
    /// Its capabilities couldn't be dropped.
    Capabilities,
    /// The kernel refused the seccomp filter.
    Filter(Failure),
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
            // Refused (a seccomp profile without `rt_sigtimedwait`), or
            // impossible with a valid set. Spinning on it would burn a CPU
            // forever; ending the container is the lesser evil.
            -1 => give_up(status_pipe),
            // A forwarded signal: on to the command. (Never re-raised here:
            // PID 1 would drop it.) It can't fail but for a seccomp profile
            // refusing `kill` (the init keeps `CAP_KILL`, and an unreaped
            // command exists): then no signal would reach the command, and
            // stopping it would wait for the stop timeout's SIGKILL.
            other => {
                // SAFETY: `command` is our child.
                if unsafe { libc::kill(command, other) } == -1 {
                    give_up(status_pipe);
                }
            }
        }
    }
}

/// Ends the container when the init can't supervise its command, saying so
/// on the status pipe: its stderr is /dev/null by now, and its exit code
/// alone would pass for the command's. The kernel kills whatever is left in
/// the namespace.
fn give_up(status_pipe: RawFd) -> ! {
    write_record(status_pipe, &[GAVE_UP_MARK]);
    std::process::exit(SETUP_FAILED);
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
            // Nothing more right now.
            0 => return,
            // No children left.
            -1 if Errno::last() == Errno::ECHILD => return,
            // Refused (a seccomp profile without `wait4`): the command
            // could never be reaped, and the container would hang. Ending
            // it is the lesser evil.
            -1 => give_up(status_pipe),
            // An orphan that ended up with PID 1: reaped, nothing else.
            _ => {}
        }
    }
}
