//! Starting a command inside a running container, as `exec` does (Linux;
//! stage 8, steps 8.4a and 8.4b): the joiner.
//!
//! **Why a process of its own.** Entering a container's PID namespace needs
//! `CAP_SYS_ADMIN` both where the caller is and in the user namespace that
//! owns it (`setns(2)`): a rootless caller has that only once it has joined
//! the container's user namespace, and nothing leaves a user namespace. So
//! shipyard starts a short-lived process, the **joiner**, which does:
//!
//! ```text
//! shipyard ── fork + exec of the sealed copy ──► joiner (host PID namespace)
//!   1. its OOM score and limits (inherited by what it starts)
//!   2. the namespaces the container's user namespace doesn't own (a
//!      network namespace given by path, say), with its host powers
//!   3. non-dumpable; the user namespace (non-dumpable again); the PID one
//!   4. with -t: the command's terminal, from the container's devpts
//!      (through the init's root), its master sent on
//!   5. clone3(CLONE_PARENT | CLONE_INTO_CGROUP) ──► the command
//!        shipyard's own child (CLONE_PARENT), PID n inside the container,
//!        born in its cgroup; it joins the rest itself (child.rs), mount last
//!   6. says the command's PID, and exits
//! ```
//!
//! **The joiner runs the sealed copy** of shipyard's binary (as the
//! container's init does, `spawn.rs`), never the file on disk: the command
//! is a clone of it, so its `/proc/self/exe` is that copy until its own
//! `exec`, and an image whose `/bin/sh` is `/proc/self/exe` can't reach the
//! host's binary (CVE-2019-5736). It's an ordinary single-threaded process:
//! it may allocate; only its clone, the command, keeps `child.rs`'s rules.
//!
//! **Hidden from the container until it runs.** The joiner makes itself
//! non-dumpable before it enters anything of the container's, and again
//! after the user namespace (a credential change resets it to
//! `fs.suid_dumpable`); the command does again after each credential call.
//! Non-dumpable, its `/proc/PID` is the host root's and ptrace is refused
//! (probe E4): the container can't read its memory, descriptors or
//! environment, nor trace it, before its `exec` (CVE-2016-9962). Hosts that
//! set `fs.suid_dumpable` to 1 are refused: there a credential call makes it
//! dumpable for a moment.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use nix::errno::Errno;
use nix::sys::signal::{SigSet, SigmaskHow, sigprocmask};
use serde::{Deserialize, Serialize};

use super::child::{Failure, SETUP_FAILED, apply_rlimits, child_main, make_undumpable};
use super::clone::{Cloned, clone_process};
use super::failure::ChildStep;
use super::init::{cap_last_cap, confinement_failure, kernel_version};
use super::internal::{
    Internal, close_on_exec, decode_namespaces, fd_bound, internal_entry, join, setgroups_allowed,
    split_namespaces, start_sealed,
};
use super::plan::{ChildPlan, ExecChild, Handshake};
use super::setup::{SpecSetup, read_sealed_json};
use crate::isolation::Namespace;
use crate::security::{Confinement, Security};
use crate::{Error, Result};

/// The hidden argument that makes the shipyard binary run as `exec`'s
/// joiner (see [`entry_point`]).
pub(crate) const EXEC_ENTRY_ARG: &std::ffi::CStr = c"__shipyard-exec";

/// The test hook (`SHIPYARD_TEST_EXEC_PAUSE`, milliseconds): the command
/// waits that long once it has joined the container (before its credential
/// switch), so a test can look at it from inside.
pub(crate) const PAUSE_VAR: &str = "SHIPYARD_TEST_EXEC_PAUSE";

/// What the joiner needs, as it receives it: in a sealed memfd (see
/// `setup.rs`), never in its arguments, which anyone can read
/// (`/proc/PID/cmdline` isn't guarded like the rest).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JoinerSetup {
    spec: SpecSetup,
    security: Security,
    keep_ignored: Vec<libc::c_int>,
    /// Namespaces to join before the user namespace (owned outside it),
    /// then the user and PID namespaces, then the rest (the command's, mount
    /// last): `(index in Namespace::ALL, descriptor)`.
    early: Vec<(u64, RawFd)>,
    user: Option<RawFd>,
    pid: Option<RawFd>,
    late: Vec<(u64, RawFd)>,
    /// The container's user namespace allows `setgroups` (or, without one,
    /// shipyard's own does).
    setgroups_allowed: bool,
    /// The container's cgroup, open, for `CLONE_INTO_CGROUP`.
    cgroup: Option<RawFd>,
    /// The report pipe's write end: the joiner's failures and the command's.
    report: RawFd,
    /// The go pipe's read end, for the command.
    go: RawFd,
    /// The joiner's record pipe: the command's PID.
    record: RawFd,
    /// A terminal for the command (`-t`): its socket, size, and whether
    /// the receiver answers (shipyard's relay) or not (a console socket);
    /// and the init's `/proc` directory, whose root holds the container's
    /// devpts.
    terminal: Option<(super::setup::TerminalSetup, RawFd)>,
    detach: bool,
    pause_ms: u32,
    /// Descriptors passed on to the command (see `passed.rs`), inherited.
    passing: Option<super::passed::Passing>,
}

impl JoinerSetup {
    /// Every descriptor the joiner inherits by number (they must survive
    /// its exec: their close-on-exec flag is cleared in the fork).
    fn passed(&self) -> Vec<RawFd> {
        self.early
            .iter()
            .chain(&self.late)
            .map(|&(_, fd)| fd)
            .chain(self.user)
            .chain(self.pid)
            .chain(self.cgroup)
            .chain(
                self.terminal
                    .into_iter()
                    .flat_map(|(terminal, dir)| [terminal.socket, dir]),
            )
            .chain([self.report, self.go, self.record])
            .chain(
                self.passing
                    .iter()
                    .flat_map(|passing| passing.sources.iter().copied()),
            )
            .collect()
    }
}

/// What the parent opened for the joiner: kept open until it has started.
pub(crate) struct JoinerFiles {
    pub(crate) setup: OwnedFd,
    /// The descriptors the setup names, and the setup's.
    passed: Vec<RawFd>,
}

/// The descriptors the joiner passes on, by number: the container's cgroup
/// (if it has one) and the three pipes' ends.
#[derive(Debug, Clone, Copy)]
pub(crate) struct JoinerPipes {
    /// The container's cgroup, open.
    pub(crate) cgroup: Option<RawFd>,
    /// The report pipe's write end.
    pub(crate) report: RawFd,
    /// The go pipe's read end.
    pub(crate) go: RawFd,
    /// The joiner's record pipe's write end.
    pub(crate) record: RawFd,
    /// With a terminal (`-t`): where to send its master, and how; and the
    /// container's init's `/proc` directory, to reach its devpts through.
    pub(crate) terminal: Option<(super::setup::TerminalSetup, RawFd)>,
}

/// The joiner's setup for running `spec` in `joining`'s namespaces,
/// confined by `security`.
///
/// # Errors
///
/// Supplementary groups in a user namespace that denies `setgroups`
/// ([`Error::GroupsDenied`]), or a namespace whose owner can't be read.
pub(crate) fn joiner_files(
    spec: &super::ProcessSpec,
    keep_ignored: &[libc::c_int],
    security: &Security,
    joining: &super::join::Joining,
    pipes: JoinerPipes,
    detach: bool,
    passing: Option<&super::passed::Passing>,
) -> Result<JoinerFiles> {
    let (early, late) = split_namespaces(joining)?;
    let setgroups_allowed = setgroups_allowed(joining);
    // Said here, plainly: the joiner could only report it as a bare step.
    if !setgroups_allowed && let Some(&gid) = spec.user.additional_gids.first() {
        return Err(Error::GroupsDenied { gid });
    }
    // Test builds only: a release never waits for an environment variable.
    let pause_ms = if cfg!(debug_assertions) {
        std::env::var(PAUSE_VAR)
            .ok()
            .and_then(|ms| ms.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let setup = JoinerSetup {
        spec: SpecSetup::new(spec),
        security: security.clone(),
        keep_ignored: keep_ignored.to_vec(),
        early,
        user: joining.user.as_ref().map(AsRawFd::as_raw_fd),
        pid: joining.pid.as_ref().map(AsRawFd::as_raw_fd),
        late,
        setgroups_allowed,
        cgroup: pipes.cgroup,
        report: pipes.report,
        go: pipes.go,
        record: pipes.record,
        terminal: pipes.terminal,
        detach,
        pause_ms,
        passing: passing.cloned(),
    };
    let mut passed = setup.passed();
    let fd = super::setup::sealed_json(&setup, c"shipyard-exec", "exec's setup")?;
    passed.push(fd.as_raw_fd());
    Ok(JoinerFiles { setup: fd, passed })
}

/// Starts the joiner for the container `joining` names: a sealed copy of
/// shipyard's binary (or, if this host won't make or run one, the binary's
/// file, only where nobody in the container could write it), with `files`
/// passed, and `stdio` (each inherited if `None`) as its standard
/// descriptors, which the command it clones keeps. Returns it. Every signal is blocked in the caller; the joiner blocks them again
/// itself (`std`'s spawn clears the mask in the child).
pub(crate) fn start_joiner(
    joining: &super::join::Joining,
    files: &JoinerFiles,
    stdio: [Option<RawFd>; 3],
) -> Result<std::process::Child> {
    // The binary's file itself (no sealed copy could be made, or run) would
    // let the container, once the command has exec'd, reach the file
    // shipyard is run from (CVE-2019-5736): only where nobody inside could
    // write it.
    let maps = super::internal::maps_of(joining);
    let (exe, host_exe) = super::spawn::own_binary(
        |file| super::internal::fallback_allowed(file, maps.as_ref()),
        |why| Error::Unsealed { what: "exec", why },
    )?;
    // SAFETY: the caller holds the command's ends open until the joiner is
    // started (see `ChildEnds`).
    let stdio = stdio.map(|fd| fd.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) }));
    let how = Internal {
        arg0: "shipyard-exec",
        entry: EXEC_ENTRY_ARG,
        setup: files.setup.as_raw_fd(),
        passed: &files.passed,
        // The command's, which it clones with them.
        stdio,
    };
    start_sealed(&exe, host_exe.as_ref(), &how, "exec's joiner", || {
        Error::Unsealed {
            what: "exec",
            why: crate::error::UNSEALED_POLICY,
        }
    })
}

/// What the joiner says once the command exists: its PID, and whether the
/// clone placed it in the container's cgroup.
const RECORD_LEN: usize = 8;

/// Reads the joiner's record, waiting up to `limit`: the command's PID and
/// whether it's in its cgroup. `None` if the joiner ended (or was killed)
/// without one.
pub(crate) fn read_record(
    pipe: &OwnedFd,
    limit: std::time::Duration,
) -> Option<(nix::unistd::Pid, bool)> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let deadline = std::time::Instant::now() + limit;
    let mut record = [0_u8; RECORD_LEN];
    let mut filled = 0;
    while filled < RECORD_LEN {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
        match poll(
            &mut fds,
            PollTimeout::try_from(left).unwrap_or(PollTimeout::MAX),
        ) {
            Ok(0) => return None,
            Ok(_) | Err(Errno::EINTR) => {}
            Err(_) => return None,
        }
        match nix::unistd::read(pipe, record.get_mut(filled..)?) {
            Ok(0) => return None,
            Ok(read) => filled += read,
            Err(Errno::EINTR | Errno::EAGAIN) => {}
            Err(_) => return None,
        }
    }
    let [a, b, c, d, placed, ..] = record;
    let pid = i32::from_ne_bytes([a, b, c, d]);
    (pid > 0).then(|| (nix::unistd::Pid::from_raw(pid), placed == 1))
}

/// Where the shipyard binary starts as `exec`'s joiner. Does nothing unless
/// this process was started that way (by [`start_joiner`], with the hidden
/// first argument); otherwise it never returns.
pub(crate) fn entry_point() {
    if let Some(fd) = internal_entry(EXEC_ENTRY_ARG, "exec") {
        run(fd);
    }
}

/// The joiner (see the module docs). Never returns.
fn run(setup_fd: RawFd) -> ! {
    // Every signal blocked: the command must start with them blocked (it
    // resets them itself, `child.rs`), and nothing should cut this short.
    let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::all()), None);
    let _ = nix::sys::prctl::set_name(c"shipyard-exec");
    let Ok(setup) = read_sealed_json::<JoinerSetup>(setup_fd) else {
        std::process::exit(SETUP_FAILED);
    };
    let report = setup.report;
    let fail = |failure: Failure| -> ! {
        super::child::report_failure(report, &failure);
        std::process::exit(SETUP_FAILED);
    };
    // Passed across the exec, so not close-on-exec: make them so again (the
    // command closes them for good anyway, `child.rs`).
    close_on_exec(setup.passed());
    // The passed descriptors' targets (3 and up), held before anything is
    // opened here (the terminal, say), and before the container's limits
    // apply (a low descriptor limit would refuse them). The command's
    // sweep closes them, and it puts the passed ones there.
    let passing = setup.passing.clone().unwrap_or_default();
    if !passing.sources.is_sorted() {
        fail(Failure::at(ChildStep::PassFds, Errno::EINVAL));
    }
    let _reserved = super::passed::reserve(passing.sources.len()).unwrap_or_else(|err| {
        fail(Failure::at(
            ChildStep::PassFds,
            err.errno().unwrap_or(Errno::EBUSY),
        ))
    });
    let spec = setup
        .spec
        .spec()
        .unwrap_or_else(|_| fail(Failure::at(ChildStep::ReadSetup, Errno::EINVAL)));
    let last_cap =
        cap_last_cap().unwrap_or_else(|errno| fail(Failure::at(ChildStep::CapLastCap, errno)));
    let confinement = Confinement::new(&setup.security, spec.user.uid, last_cap, kernel_version())
        .unwrap_or_else(|err| fail(confinement_failure(&err)));
    let namespaces = decode_namespaces;
    let exec = ExecChild {
        close_first: setup.record,
        keep: [report, setup.go, -1, -1],
        relayed: -1,
        fd_bound: fd_bound(),
        detach: setup.detach,
        pause_ms: setup.pause_ms,
    };
    let (mut plan, _) = ChildPlan::for_exec(
        &spec,
        setup.keep_ignored.clone(),
        // The parent's end of the go pipe isn't passed: nothing to close.
        Handshake {
            read: setup.go,
            parent_end: -1,
        },
        confinement,
        setup.setgroups_allowed,
        (super::join::join_setup(&namespaces(&setup.late)), exec),
        setup.passing.as_ref(),
    )
    .unwrap_or_else(|_| fail(Failure::at(ChildStep::ReadSetup, Errno::EINVAL)));

    // 1. With the host's powers, still: inherited by the command.
    set_limits(&mut plan, spec.oom_score_adj).unwrap_or_else(|failure| fail(failure));
    // 2. What the container's user namespace doesn't own.
    for (kind, fd) in namespaces(&setup.early) {
        join(fd, kind).unwrap_or_else(|failure| fail(failure));
    }
    // 3. Hidden from the container from here on; the user namespace (a
    //    credential change: hidden again), the PID namespace.
    make_undumpable().unwrap_or_else(|failure| fail(failure));
    if let Some(user) = setup.user {
        join(user, Namespace::User).unwrap_or_else(|failure| fail(failure));
        make_undumpable().unwrap_or_else(|failure| fail(failure));
    }
    if let Some(pid) = setup.pid {
        join(pid, Namespace::Pid).unwrap_or_else(|failure| fail(failure));
    }
    // 4. Its terminal (`-t`), from the container's own devpts, made here
    //    (ordinary code; the command may not allocate), now that this
    //    process may reach the container's init and give the slave away.
    let _slave = give_terminal(&setup, &mut plan, &spec).unwrap_or_else(|failure| fail(failure));
    // 5. The command: shipyard's child, inside, in the container's cgroup.
    // SAFETY: this process is single-threaded; the child runs only
    // `child_main`, which follows `child.rs`'s rules, with every signal
    // blocked.
    match unsafe { clone_process(libc::CLONE_PARENT, setup.cgroup) } {
        Ok(Cloned::Child) => {
            // SAFETY: the clone, with every signal blocked; `plan` and the
            // pipes were prepared above.
            unsafe { child_main(&plan, report) }
        }
        Ok(Cloned::Parent { pid, in_cgroup }) => {
            // 5. Its PID, for shipyard (it can't see it otherwise: it's
            //    its child, but not one it started).
            let mut record = [0_u8; RECORD_LEN];
            if let Some(head) = record.get_mut(..4) {
                head.copy_from_slice(&pid.as_raw().to_ne_bytes());
            }
            if let Some(placed) = record.get_mut(4) {
                *placed = u8::from(in_cgroup);
            }
            // SAFETY: the record pipe's write end, inherited; ours.
            let mut pipe = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(setup.record) });
            if std::io::Write::write_all(&mut pipe, &record).is_err() {
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                fail(Failure::at(ChildStep::ExecRecord, Errno::EPIPE));
            }
            std::process::exit(0);
        }
        Err(errno) => fail(Failure::at(ChildStep::ExecClone, errno)),
    }
}

/// The command's terminal (`-t`), into `plan`: its slave (returned: open
/// until the clone has it) and, with shipyard's relay, the socket the
/// command waits on for the relay's answer. The relay answers once it
/// relays the master, but it starts only once the command exists: the
/// command waits, not this process. (A console socket never answers.)
fn give_terminal(
    setup: &JoinerSetup,
    plan: &mut ChildPlan,
    spec: &super::ProcessSpec,
) -> std::result::Result<Option<OwnedFd>, Failure> {
    let Some((terminal, proc_dir)) = setup.terminal else {
        return Ok(None);
    };
    let unanswered = super::setup::TerminalSetup {
        socket: dup_kept(terminal.socket, terminal.acknowledged)?,
        acknowledged: false,
        ..terminal
    };
    let slave = joiner_terminal(proc_dir, &unanswered, spec.user.uid, spec.user.gid)?;
    plan.terminal = Some(slave.as_raw_fd());
    if let Some(exec) = &mut plan.exec {
        exec.keep[super::plan::KEEP_SLAVE] = slave.as_raw_fd();
        // More descriptors were opened for it: the sweep's bound again.
        exec.fd_bound = fd_bound();
        if terminal.acknowledged {
            exec.keep[super::plan::KEEP_RELAYED] = terminal.socket;
            exec.relayed = terminal.socket;
        }
    }
    Ok(Some(slave))
}

/// The socket the master is sent on: `socket` itself, or, when the command
/// keeps it to wait for the relay's answer (`acknowledged`), a copy (sending
/// the master closes the one it's given).
fn dup_kept(socket: RawFd, acknowledged: bool) -> std::result::Result<RawFd, Failure> {
    if !acknowledged {
        return Ok(socket);
    }
    // SAFETY: inherited across the exec; borrowed only for this call.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(socket) };
    nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(3))
        .map_err(|errno| Failure::at(ChildStep::TerminalSend, errno))
}

/// Sets this process's OOM score adjustment and the plan's limits, for the
/// command to inherit (a hard limit is raised with the host's powers, before
/// the user namespace), then clears the plan's: they're set.
fn set_limits(
    plan: &mut ChildPlan,
    oom_score_adj: Option<i32>,
) -> std::result::Result<(), Failure> {
    if let Some(adj) = oom_score_adj {
        std::fs::write("/proc/self/oom_score_adj", adj.to_string())
            .map_err(|err| Failure::at(ChildStep::OomScoreAdj, super::io_errno(&err)))?;
    }
    apply_rlimits(plan)?;
    plan.rlimits.clear();
    Ok(())
}

/// The command's pseudo-terminal, from the container's own devpts: reached
/// through the init's root (`/proc/<init>/root`, open from `proc_dir`), the
/// path walked a component at a time without following any link (the
/// container controls it, and a link could lead anywhere the walk reaches).
/// What's opened must then prove to be a pseudo-terminal's master
/// (`init::terminal_from`'s ioctls fail on anything else). Returns the
/// slave; the master has gone to `terminal`'s socket.
fn joiner_terminal(
    proc_dir: RawFd,
    terminal: &super::setup::TerminalSetup,
    uid: u32,
    gid: u32,
) -> std::result::Result<OwnedFd, Failure> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    let failed = |errno| Failure::at(ChildStep::TerminalOpen, errno).detail(0);
    let dir = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    // SAFETY: inherited across the exec; borrowed only for this call.
    let proc_dir = unsafe { std::os::fd::BorrowedFd::borrow_raw(proc_dir) };
    // (The one link followed: `root`, the kernel's own, to the init's root.)
    let root = openat(proc_dir, "root", dir, Mode::empty()).map_err(failed)?;
    let dev = openat(&root, "dev", dir | OFlag::O_NOFOLLOW, Mode::empty()).map_err(failed)?;
    let pts = openat(&dev, "pts", dir | OFlag::O_NOFOLLOW, Mode::empty()).map_err(failed)?;
    // Looked at before it's opened: opening a device runs its driver (a
    // watchdog arms, a serial line's signals change), and this process
    // isn't under the container's device rules. Only devpts's own
    // multiplexer (5:2) is opened.
    let leaf = openat(
        &pts,
        "ptmx",
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(failed)?;
    let not_ptmx = || Failure::at(ChildStep::TerminalOpen, Errno::ENODEV).detail(5);
    let stat = nix::sys::stat::fstat(&leaf).map_err(failed)?;
    let devpts = nix::sys::statfs::fstatfs(&leaf)
        .is_ok_and(|fs| fs.filesystem_type() == nix::sys::statfs::DEVPTS_SUPER_MAGIC);
    let is_char = stat.st_mode & libc::S_IFMT == libc::S_IFCHR;
    if !devpts || !is_char || libc::major(stat.st_rdev) != 5 || libc::minor(stat.st_rdev) != 2 {
        return Err(not_ptmx());
    }
    // The same file (its descriptor), now opened: not blocking, then made so.
    let master = nix::fcntl::open(
        format!("/proc/self/fd/{}", leaf.as_raw_fd()).as_str(),
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(failed)?;
    let flags = nix::fcntl::fcntl(&master, nix::fcntl::FcntlArg::F_GETFL).map_err(failed)?;
    nix::fcntl::fcntl(
        &master,
        nix::fcntl::FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) & !OFlag::O_NONBLOCK),
    )
    .map_err(failed)?;
    super::init::terminal_from(master, terminal, uid, gid)
}
