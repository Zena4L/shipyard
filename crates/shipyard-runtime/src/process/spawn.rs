//! Starting a process: `fork` + `exec` (TLPI ch. 24–28).
//!
//! [`spawn`] forks and returns at once; the supervisor then watches the
//! child's report pipe and its exit inside one event loop. Spawn only from the
//! main thread: Linux's parent-death signal fires when the *thread* that
//! forked exits, and on macOS a fork in another thread could inherit the
//! report pipe.

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, sigprocmask};
use nix::sys::signal::{Signal as NixSignal, sigaction};
use nix::unistd::{ForkResult, Pid, fork, getpgrp, setpgid};

use super::channel::{
    InitReport, Report, ReportReader, StartMark, go_pipe, read_init_report, report_pipe, send_go,
    status_pipe,
};
use super::child::child_main;
use super::failure::{FailureContext, describe};
use super::plan::{ChildPlan, Handshake, InitFds};
use super::signal::Signal;
use super::spec::ProcessSpec;
use super::status::ExitStatus;
use crate::isolation::{Isolation, Join, Namespace};
use crate::{Error, Result};

/// How to start the process.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnOptions {
    /// Put the command in its own process group, so it and everything it
    /// starts can be signalled together. Only without a terminal: on a
    /// terminal, a separate group would be stopped the moment it reads input.
    pub(crate) new_process_group: bool,
    /// Signals the command keeps ignoring instead of having reset to their
    /// defaults: the ones `nohup` or a script's `&` asked shipyard to ignore.
    pub(crate) keep_ignored: Vec<libc::c_int>,
    /// Which namespaces the command starts in.
    pub(crate) placement: Placement,
}

/// Which namespaces the command starts in.
#[derive(Debug, Clone, Default)]
pub(crate) enum Placement {
    /// Shipyard's own.
    #[default]
    Host,
    /// New ones (Linux).
    New(Isolation),
    /// A running process's (Linux; see `join.rs`).
    Join(Join),
}

impl Placement {
    /// The new namespaces, if that's where the command starts.
    pub(crate) fn isolation(&self) -> Option<&Isolation> {
        match self {
            Self::New(isolation) => Some(isolation),
            Self::Host | Self::Join(_) => None,
        }
    }

    /// Whether the command starts a session of its own (isolated or joined).
    /// It then has its own process group too, so `new_process_group`
    /// doesn't apply, and it never shares the terminal's group.
    pub(crate) fn has_own_session(&self) -> bool {
        !matches!(self, Self::Host)
    }
}

/// A started process.
#[derive(Debug)]
pub(crate) struct Child {
    pid: Pid,
    /// The command's own process group, if it has one.
    group: Option<Pid>,
    /// Until `exec` succeeds or fails.
    report: Option<ReportReader>,
    /// The parent's end of the go pipe, held open until `exec` resolves: the
    /// child checks it to know the parent is alive (see `channel.rs`).
    go: Option<OwnedFd>,
    /// With a container init: where it sends the command's exit status (the
    /// init itself is the process we reap).
    status: Option<OwnedFd>,
    context: FailureContext,
    reaped: bool,
}

/// Starts the process `spec` describes and returns without waiting.
///
/// Everything that can fail happens before `fork`: once the child exists,
/// this function can't return an error and leave it unsupervised.
///
/// `started` is called with the new PID while every signal is still blocked,
/// so a signal handler can never see the command running without knowing its
/// PID.
pub(crate) fn spawn(
    spec: &ProcessSpec,
    options: &SpawnOptions,
    started: impl FnOnce(Pid),
) -> Result<Child> {
    let (report_read, report_write) = report_pipe()?;
    let report = ReportReader::new(report_read)?;
    let (go_read, go_write) = go_pipe()?;
    let handshake = Handshake {
        read: go_read.as_raw_fd(),
        parent_end: go_write.as_raw_fd(),
    };
    // For the container's init, which only a new PID namespace needs (see
    // `init.rs`): the status pipe, and the binary it re-execs itself as.
    let needs_init = options
        .placement
        .isolation()
        .is_some_and(|isolation| isolation.has(Namespace::Pid));
    let status = needs_init.then(status_pipe).transpose()?;
    let init_exe = needs_init.then(open_own_binary).transpose()?;
    let init_fds = status
        .as_ref()
        .zip(init_exe.as_ref())
        .map(|((_, status_write), exe)| InitFds {
            status_write: status_write.as_raw_fd(),
            exe: exe.as_raw_fd(),
        });
    #[cfg(target_os = "linux")]
    let joining = match &options.placement {
        Placement::Join(join) => Some(super::join::Joining::open(join)?),
        Placement::Host | Placement::New(_) => None,
    };
    // Last, so its list of open descriptors includes the pipes above.
    let (plan, context) = ChildPlan::new(
        spec,
        options,
        handshake,
        init_fds,
        #[cfg(target_os = "linux")]
        joining.as_ref().map(super::join::Joining::setup),
    )?;

    // Block every signal across `fork`: the supervisor's handler must never
    // run in the child before the child resets it.
    let mask = BlockAllSignals::new()?;
    // SAFETY: the child branch only runs `child_main`, which is written to be
    // safe after `fork` (no allocation, locks or panics; see child.rs).
    let pid = match unsafe {
        launch(
            options,
            #[cfg(target_os = "linux")]
            joining.as_ref(),
        )
    }? {
        Launched::Child => {
            // SAFETY: this is the child, right after `fork`/`clone`, with all
            // signals blocked; `plan` and the pipes were prepared above.
            unsafe { child_main(&plan, report_write.as_raw_fd()) }
        }
        Launched::Parent(pid) => pid,
    };
    started(pid);
    drop(mask);
    drop(report_write);
    drop(go_read);
    // Only the init writes to it; with our copy closed, it reads as empty
    // once the init is gone.
    let status = status.map(|(read, _write)| read);
    let group = if options.placement.has_own_session() {
        // The child starts its own session: it leads the new group.
        Some(pid)
    } else {
        options.new_process_group.then(|| {
            // The child does this too; doing it here as well closes the
            // window before the child gets to it. Errors mean the child
            // already did it, or already exec'd or exited: all fine.
            let _ = setpgid(pid, pid);
            pid
        })
    };
    // Everything the child waits for is done.
    send_go(&go_write);
    tracing::debug!(pid = pid.as_raw(), program = %context.program.display(), "started");
    Ok(Child {
        pid,
        group,
        report: Some(report),
        go: Some(go_write),
        status,
        context,
        reaped: false,
    })
}

/// The command's result, from what the container's init sent (`report`)
/// and how the init itself ended (`init`).
///
/// The init's own exit code always mirrors the command's (128 + N for a
/// signal), so a status record is only believed when it agrees with it:
/// anything in the container that can reach the pipe could forge one. The
/// record adds what the exit code can't say, whether the command died by a
/// signal.
fn init_outcome(report: InitReport, init: ExitStatus) -> Result<ExitStatus> {
    match report {
        InitReport::Finished(mark, raw) => {
            warn_if_copy(mark);
            let command =
                ExitStatus::from_wait_status(raw).filter(|command| command.code() == init.code());
            if command.is_none() {
                tracing::warn!(
                    %init,
                    "the container's init reported a status that doesn't match how it \
                     exited; using its own exit status"
                );
            }
            Ok(command.unwrap_or(init))
        }
        // Killed before the command ended (the stop timeout's SIGKILL).
        InitReport::Started(mark) => {
            warn_if_copy(mark);
            Ok(init)
        }
        InitReport::Unexpected => {
            tracing::warn!(
                "unexpected data on the container's status pipe (written from inside the \
                 container?); using the init's own exit status"
            );
            Ok(init)
        }
        // Exited, never having run its loop: the re-exec'd image failed to
        // start. (Killed before it could say so is an ordinary end, say by
        // the stop timeout.)
        InitReport::Nothing => match init {
            ExitStatus::Exited(code) => Err(Error::InitDidNotStart { code }),
            _ => Ok(init),
        },
    }
}

fn warn_if_copy(mark: StartMark) {
    if mark == StartMark::Copy {
        tracing::warn!(
            "the container's init couldn't re-exec itself, so it ran as a copy of shipyard \
             and held its command line and environment"
        );
    }
}

/// The running shipyard binary, for the container's init to re-exec (see
/// `init.rs`). Opened by descriptor, before the clone: inside the container
/// `/proc` is remounted, and the file on disk may be replaced by an upgrade.
fn open_own_binary() -> Result<OwnedFd> {
    nix::fcntl::open(
        "/proc/self/exe",
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|e| Error::sys("opening shipyard's own binary for the container init", e))
}

/// Which side of the `fork` or `clone` this is.
enum Launched {
    Child,
    Parent(Pid),
}

/// Starts the child: with `clone` into new namespaces if isolated, with
/// `fork` otherwise.
///
/// # Safety
///
/// As for `fork`: the child may only run async-signal-safe code.
unsafe fn launch(
    options: &SpawnOptions,
    #[cfg(target_os = "linux")] joining: Option<&super::join::Joining>,
) -> Result<Launched> {
    #[cfg(target_os = "linux")]
    if let Some(joining) = joining {
        // SAFETY: the caller's contract.
        return match unsafe { joining.fork() }? {
            ForkResult::Child => Ok(Launched::Child),
            ForkResult::Parent { child } => Ok(Launched::Parent(child)),
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(isolation) = options.placement.isolation() {
        use super::clone::{Cloned, clone_process};
        // SAFETY: the caller's contract; the flags are namespace flags only
        // (`Isolation::validate` refuses CLONE_NEWTIME).
        return match unsafe { clone_process(isolation.clone_flags()) } {
            Ok(Cloned::Child) => Ok(Launched::Child),
            Ok(Cloned::Parent(pid)) => Ok(Launched::Parent(pid)),
            // Root and still refused: a container without privileges, or a
            // security policy.
            Err(Errno::EPERM) if nix::unistd::geteuid().is_root() => Err(Error::IsolationRefused),
            Err(Errno::EPERM) => Err(Error::IsolationNeedsRoot),
            // The kernel's limit on namespaces is reached (or set to 0).
            Err(Errno::ENOSPC) => Err(Error::sys(
                "starting the command in new namespaces (clone): the system's limit on \
                 namespaces is reached (see /proc/sys/user/max_*_namespaces)",
                Errno::ENOSPC,
            )),
            Err(errno) => Err(Error::sys(
                "starting the command in new namespaces (clone)",
                errno,
            )),
        };
    }
    #[cfg(not(target_os = "linux"))]
    if options.placement.has_own_session() {
        return Err(Error::LinuxOnly {
            feature: "namespace isolation",
            os: std::env::consts::OS,
        });
    }
    // SAFETY: the caller's contract.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => Ok(Launched::Child),
        Ok(ForkResult::Parent { child }) => Ok(Launched::Parent(child)),
        Err(errno) => Err(fork_error(errno)),
    }
}

/// A failed `fork`, explained.
pub(super) fn fork_error(errno: Errno) -> Error {
    Error::sys(
        if errno == Errno::EAGAIN {
            "starting the command: too many processes"
        } else {
            "starting the command (fork)"
        },
        errno,
    )
}

impl Child {
    pub(crate) fn pid(&self) -> Pid {
        self.pid
    }

    /// The report pipe, while `exec` hasn't resolved yet.
    pub(crate) fn report_fd(&self) -> Option<BorrowedFd<'_>> {
        self.report.as_ref().map(ReportReader::fd)
    }

    /// Reads the report pipe without blocking, if `exec` hasn't resolved yet.
    /// A failure becomes the error that explains it; the caller still has to
    /// reap the child.
    pub(crate) fn check_exec(&mut self) -> Result<()> {
        let Some(report) = self.report.as_mut() else {
            return Ok(());
        };
        match report.read()? {
            Report::Pending => Ok(()),
            Report::Running => {
                self.report = None;
                self.go = None;
                Ok(())
            }
            Report::Failed(failure) => {
                self.report = None;
                self.go = None;
                Err(describe(failure, &self.context).into())
            }
        }
    }

    /// Sends `signal` to the command. Safe from PID reuse: an unreaped child
    /// keeps its PID, and after reaping nothing is sent.
    pub(crate) fn signal(&self, signal: libc::c_int) {
        if self.reaped {
            return;
        }
        // SAFETY: `kill` has no memory-safety preconditions. Raw libc, since
        // nix's `Signal` can't express Linux real-time signals.
        unsafe { libc::kill(self.pid.as_raw(), signal) };
    }

    /// The signal that stopped the command, if it's stopped right now.
    ///
    /// Peeks with `waitid(WSTOPPED | WNOWAIT)`: nothing is consumed, and
    /// without `WEXITED` an exited command is never reaped here. Both Linux
    /// and macOS report only a *current* stop: once SIGCONT resumed the
    /// command, this returns `None` again.
    pub(crate) fn stopped(&self) -> Option<libc::c_int> {
        if self.reaped {
            return None;
        }
        stop_signal_of(self.pid)
    }

    /// Continues the command and, if it has its own group, everything in it:
    /// a stopped process can't act on a stop request.
    pub(crate) fn continue_all(&self) {
        self.signal(libc::SIGCONT);
        if let Some(group) = self.own_group() {
            // SAFETY: `killpg` has no memory-safety preconditions.
            unsafe { libc::killpg(group.as_raw(), libc::SIGCONT) };
        }
    }

    /// Kills the command and, if it has its own group, everything in it.
    pub(crate) fn kill_all(&self) {
        if self.reaped {
            return;
        }
        // The command itself first: right after `fork` its group may not
        // exist yet, and then `killpg` alone would miss it.
        self.signal(libc::SIGKILL);
        if let Some(group) = self.own_group() {
            // SAFETY: `killpg` has no memory-safety preconditions. ESRCH
            // (an empty group) is harmless.
            unsafe { libc::killpg(group.as_raw(), libc::SIGKILL) };
        }
    }

    /// The command's own process group, while it's safe to signal: not yet
    /// reaped (an unreaped command keeps the group ID from being reused),
    /// and never shipyard's own group.
    fn own_group(&self) -> Option<Pid> {
        self.group
            .filter(|group| !self.reaped && *group != getpgrp())
    }

    /// Reaps the command, which must have exited, and returns how it ended.
    ///
    /// Isolated, the process reaped is the container's init, and the
    /// command's own status comes over the status pipe. If the init sent
    /// none (it was killed, by the stop timeout's SIGKILL, or failed before
    /// starting the command), its own status is the answer.
    pub(crate) fn reap(&mut self) -> Result<ExitStatus> {
        let reaped = wait_pid(self.pid).inspect_err(|_| {
            // ECHILD and the like: the PID is no longer ours to signal.
            self.reaped = true;
        })?;
        self.reaped = true;
        let status = match self.status.take() {
            Some(pipe) => init_outcome(read_init_report(&pipe), reaped)?,
            None => reaped,
        };
        tracing::debug!(pid = self.pid.as_raw(), %status, "exited");
        Ok(status)
    }
}

/// The signal that stopped child `pid`, if it's stopped now. Raw
/// `libc::waitid`: nix doesn't provide it on macOS.
fn stop_signal_of(pid: Pid) -> Option<libc::c_int> {
    let id = libc::id_t::try_from(pid.as_raw()).ok()?;
    loop {
        // SAFETY: a zeroed `siginfo_t` is valid; it stays zeroed if no child
        // matches.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is valid, writable storage for the result.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            if Errno::last() == Errno::EINTR {
                continue;
            }
            return None;
        }
        // SAFETY: `waitid` filled `info` (or left it zeroed: `si_pid` 0).
        let (reported, code, status) = unsafe { (info.si_pid(), info.si_code, info.si_status()) };
        return (reported == pid.as_raw() && code == libc::CLD_STOPPED).then_some(status);
    }
}

/// `waitpid` for one child, retried on EINTR, decoded with the raw macros.
fn wait_pid(pid: Pid) -> Result<ExitStatus> {
    loop {
        let mut raw = 0;
        // SAFETY: `raw` is a valid place for the status; `pid` is our child.
        let rc = unsafe { libc::waitpid(pid.as_raw(), &raw mut raw, 0) };
        if rc == -1 {
            match Errno::last() {
                Errno::EINTR => continue,
                errno => return Err(Error::sys("waiting for the command to exit", errno)),
            }
        }
        // Without WUNTRACED, waitpid only reports exits and deaths by signal.
        if let Some(status) = ExitStatus::from_wait_status(raw) {
            return Ok(status);
        }
    }
}

/// Dies from `signal` the default way, as if it had been delivered to us.
///
/// When the command was killed by Ctrl-C, a shell running shipyard in a loop
/// only stops the loop if shipyard *also* appears to have died from SIGINT
/// (an exit code of 130 isn't enough). Returns only if the signal didn't kill
/// the process; callers then exit with `128 + signal`.
///
/// Does nothing for a signal this process ignores (its parent asked for
/// that, as `nohup` does), or one whose default action isn't to terminate:
/// raising SIGTSTP, for example, would stop shipyard instead.
pub fn die_from(signal: Signal) {
    let Ok(signal) = NixSignal::try_from(signal.as_raw()) else {
        return;
    };
    if !terminates_by_default(signal) || is_ignored(signal) {
        return;
    }
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    // SAFETY: installing SIG_DFL runs none of our code in a signal context.
    let _ = unsafe { sigaction(signal, &default) };
    let mut only = SigSet::empty();
    only.add(signal);
    let _ = sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&only), None);
    let _ = nix::sys::signal::raise(signal);
}

/// Whether the default action for `signal` ends the process.
fn terminates_by_default(signal: NixSignal) -> bool {
    use NixSignal::{SIGCHLD, SIGCONT, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH};
    #[cfg(not(target_os = "linux"))]
    if signal == NixSignal::SIGINFO {
        return false;
    }
    ![
        SIGCHLD, SIGCONT, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH,
    ]
    .contains(&signal)
}

/// Whether this process currently ignores `signal`.
fn is_ignored(signal: NixSignal) -> bool {
    // SAFETY: a zeroed `sigaction` is valid storage; a NULL new action only
    // reads the current one.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let rc = unsafe { libc::sigaction(signal as libc::c_int, std::ptr::null(), &raw mut current) };
    rc == 0 && current.sa_sigaction == libc::SIG_IGN
}

/// Blocks every signal until dropped, then restores the previous mask.
struct BlockAllSignals {
    previous: SigSet,
}

impl BlockAllSignals {
    fn new() -> Result<Self> {
        let mut previous = SigSet::empty();
        sigprocmask(
            SigmaskHow::SIG_SETMASK,
            Some(&SigSet::all()),
            Some(&mut previous),
        )
        .map_err(|e| Error::sys("blocking signals around fork", e))?;
        Ok(Self { previous })
    }
}

impl Drop for BlockAllSignals {
    fn drop(&mut self) {
        let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&self.previous), None);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn stop_peeks_see_only_a_current_stop_and_consume_nothing() {
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(libc::pid_t::try_from(sleeper.id()).unwrap());
        let until = |want: Option<libc::c_int>| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while stop_signal_of(pid) != want {
                assert!(Instant::now() < deadline, "never became {want:?}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        assert_eq!(stop_signal_of(pid), None, "running");
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
        until(Some(libc::SIGSTOP));
        assert_eq!(
            stop_signal_of(pid),
            Some(libc::SIGSTOP),
            "a peek consumes nothing"
        );
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).unwrap();
        until(None);
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    /// Without a new PID namespace there's no init: the command is the
    /// process we started, a child of this one, and its status comes
    /// straight from it.
    ///
    /// While another thread keeps starting and ending threads, as busy
    /// programs do: a clone taken while it holds the C library's thread-list
    /// lock gives the child that lock, held forever, and glibc's credential
    /// wrappers take it (see `child.rs`'s `switch_credentials`). Many spawns,
    /// to hit that moment.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root"]
    fn isolation_without_a_pid_namespace_starts_no_init() {
        // Rosetta (x86-64 emulation on Apple silicon) deadlocks inside its own
        // handling of `clone` while another thread creates threads: the
        // process wedges on an emulator lock, beyond even SIGTERM. That's
        // the emulator, not shipyard (native runs never hang), and real
        // x86-64 kernels have no such lock. Skip there.
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        if maps.contains("/mnt/rv/[rosetta]") {
            eprintln!("under Rosetta emulation, which deadlocks on this stress; skipping");
            return;
        }
        let isolation = Isolation {
            namespaces: vec![Namespace::Uts],
            hostname: Some("box".to_owned()),
        };
        let script = format!(
            "test \"$(hostname)\" = box && test $PPID = {} && exit 7",
            std::process::id()
        );
        let spec = ProcessSpec {
            args: ["sh", "-c", &script].map(Into::into).to_vec(),
            env: crate::process::Env::new(),
            cwd: "/".into(),
            user: crate::process::User::current(),
            rlimits: Vec::new(),
        };
        let options = SpawnOptions {
            placement: Placement::New(isolation),
            ..SpawnOptions::default()
        };
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn = std::thread::spawn({
            let done = std::sync::Arc::clone(&done);
            move || {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::spawn(|| {}).join().unwrap();
                }
            }
        });
        for _ in 0..50 {
            let mut child = spawn(&spec, &options, |_| {}).unwrap();
            assert!(child.status.is_none(), "no status pipe without an init");
            // A hang would be the bug: fail instead of waiting forever.
            let (reaped, watch) = std::sync::mpsc::channel::<()>();
            let pid = child.pid();
            let watchdog = std::thread::spawn(move || {
                let hung = matches!(
                    watch.recv_timeout(Duration::from_secs(20)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                );
                if hung {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                }
                hung
            });
            let status = child.reap().unwrap();
            drop(reaped);
            assert!(!watchdog.join().unwrap(), "the child hung before exec");
            assert_eq!(status, ExitStatus::Exited(7));
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        churn.join().unwrap();
    }

    #[test]
    fn only_terminating_signals_are_re_raised() {
        assert!(terminates_by_default(NixSignal::SIGINT));
        assert!(terminates_by_default(NixSignal::SIGTERM));
        assert!(!terminates_by_default(NixSignal::SIGTSTP));
        assert!(!terminates_by_default(NixSignal::SIGSTOP));
        assert!(!terminates_by_default(NixSignal::SIGCHLD));
    }
}
