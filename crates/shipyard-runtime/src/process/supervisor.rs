//! The supervisor: runs one command and stays with it until it's gone.
//!
//! One event loop (`poll`) watches two things: the signal wake pipe and, until
//! `exec` resolves, the child's report pipe. On each wakeup it forwards
//! signals, handles a stop request, and reaps children (TLPI ch. 26, §63.2).
//!
//! **Process groups.** With a controlling terminal, the command shares
//! shipyard's process group: Ctrl-C, Ctrl-Z and hangup reach both from the
//! kernel, and the shell's job control works on the pair. Without one (CI,
//! services, scripts), the command gets its own group, so everything it
//! starts can be killed together. Handing the host terminal to the command is
//! deliberately not done: containers get their own pseudo-terminal in stage 8.
//!
//! **Which signals are forwarded.** In its own group, the command can only
//! get a signal sent to shipyard if shipyard forwards it, so everything is
//! forwarded (except what the command sent shipyard itself). In a shared
//! group, a signal from the terminal has already reached the command too, so
//! forwarding it would deliver it twice; see [`should_forward`].

use std::borrow::Cow;
use std::ffi::OsStr;
#[cfg(not(target_os = "linux"))]
use std::os::fd::AsRawFd;
#[cfg(not(target_os = "linux"))]
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use libc::c_int;
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;
use nix::unistd::Pid;

use super::signals::{Arrival, Signals, Sources};
use super::spawn::{Child, SpawnOptions, spawn};
use super::spec::ProcessSpec;
use super::status::ExitStatus;
use super::stop::StopPolicy;
use crate::isolation::Isolation;
use crate::{Error, Result};

/// When to first warn that a command killed with SIGKILL still hasn't exited, and how
/// often to repeat. A process stuck in the kernel (uninterruptible "D" state,
/// often a hung network filesystem) can't die until the kernel lets go.
const STUCK_FIRST_WARNING: Duration = Duration::from_secs(5);
const STUCK_REPEAT_WARNING: Duration = Duration::from_secs(30);
/// The longest the loop sleeps without checking on its children, even with
/// nothing to wait for. SIGCHLD should always wake it; this is the backstop
/// in case one is ever lost.
const SAFETY_NET: Duration = Duration::from_secs(1);
/// Signals ignored at startup that the command keeps ignoring: the ones
/// `nohup` (HUP) and a script's `&` (INT, QUIT) set up to protect a job.
const KEEP_IGNORED: [c_int; 3] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT];

/// Supervises commands. Owns this process's signal handlers, so only one can
/// exist per process; create it before starting anything.
///
/// It reaps every child of this process, and on Linux it also becomes the
/// parent of every process the command leaves behind and kills them when the
/// command exits. Children the process already had when the supervisor was
/// installed are left running (but reaped when they exit). Don't start other
/// children while supervising.
pub struct Supervisor {
    signals: Signals,
    /// Children shipyard already had before it supervised anything: `exec`
    /// keeps a process's children, so `helper & exec shipyard run app`
    /// makes `helper` one. They aren't the command's, and are left alone.
    /// A PID leaves the list when it's reaped (and could be reused).
    inherited: Vec<libc::pid_t>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}

impl Supervisor {
    /// Installs the signal handlers (see [`Supervisor`]) and, on Linux, makes
    /// shipyard a *child subreaper*: processes the command leaves behind are
    /// re-parented to shipyard instead of init, so it can clean them up.
    pub fn install() -> Result<Self> {
        let signals = Signals::install()?;
        #[cfg(target_os = "linux")]
        nix::sys::prctl::set_child_subreaper(true)
            .map_err(|e| Error::sys("becoming a child subreaper", e))?;
        #[cfg(target_os = "linux")]
        let inherited = direct_children().unwrap_or_default();
        #[cfg(not(target_os = "linux"))]
        let inherited = Vec::new();
        Ok(Self { signals, inherited })
    }

    /// Runs the command and supervises it until it has exited and everything
    /// it left behind has been cleaned up. Returns how it ended.
    ///
    /// - Signals another process sends shipyard are forwarded to the command.
    /// - SIGTERM is a stop request, like `docker stop`: the command gets
    ///   `stop.signal` (and SIGCONT, in case it's stopped), then SIGKILL once
    ///   `stop.timeout` has passed.
    /// - With `isolation` (Linux), the command starts in new namespaces and a
    ///   session of its own. It's then never in the terminal's process group,
    ///   so signals from the terminal are forwarded like any other, and the
    ///   shell's job control doesn't extend to it.
    ///
    /// # Errors
    ///
    /// Invalid input, isolation on macOS or without root, or a failure to
    /// start the command (see [`Error::exit_code`]).
    pub fn supervise(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        isolation: Option<&Isolation>,
    ) -> Result<ExitStatus> {
        stop.validate()?;
        if let Some(isolation) = isolation {
            check_isolation(isolation)?;
        }
        let terminal = Terminal::open();
        // With isolation the command has a session of its own, so it never
        // shares the terminal's group, whether or not shipyard has one.
        let job_terminal = terminal.as_ref().filter(|_| isolation.is_none());
        tracing::debug!(
            terminal = terminal.is_some(),
            isolated = isolation.is_some(),
            "{}",
            if job_terminal.is_some() {
                "sharing shipyard's process group with the command"
            } else {
                "giving the command its own process group"
            }
        );
        let spec = with_terminal_env(spec, terminal.is_some(), std::env::var_os("TERM"));
        let options = SpawnOptions {
            isolation: isolation.cloned(),
            new_process_group: job_terminal.is_none(),
            keep_ignored: self
                .signals
                .ignored_at_startup()
                .iter()
                .copied()
                .filter(|signal| KEEP_IGNORED.contains(signal))
                .collect(),
        };
        let started = |pid: Pid| self.signals.set_child(pid.as_raw());
        let result = spawn(&spec, &options, started).and_then(|mut child| {
            let result = self.watch(&mut child, stop, job_terminal);
            if result.is_err() {
                // Never leave the command running unsupervised.
                child.kill_all();
                reap_killed(&mut child);
            }
            result
        });
        #[cfg(target_os = "linux")]
        kill_leftovers(&mut self.inherited);
        self.signals.clear_child();
        result
    }

    /// The event loop. Returns once the command has exited and been reaped.
    fn watch(
        &mut self,
        child: &mut Child,
        stop: StopPolicy,
        terminal: Option<&Terminal>,
    ) -> Result<ExitStatus> {
        let mut timers = StopTimers::default();
        loop {
            self.wait_for_event(child, timers.next_wake())?;
            let mut continued = false;
            for arrival in self.signals.drain() {
                match arrival.signal {
                    // Whoever sent it, it's a stop request.
                    libc::SIGTERM => timers.request(child, stop),
                    libc::SIGCONT => continued = true,
                    _ if should_forward(arrival, Group::of(terminal)) => {
                        tracing::debug!(
                            signal = arrival.signal,
                            "forwarding a signal to the command"
                        );
                        child.signal(arrival.signal);
                    }
                    _ => {}
                }
            }
            timers.tick(child, stop, Instant::now());
            // The exec report comes before reaping: a failed exec must be
            // reported as such, not as an ordinary exit.
            child.check_exec()?;
            if let Some(status) = reap_children(child, &mut self.inherited)? {
                return Ok(status);
            }
            if terminal.is_some() {
                follow_stops(child, &self.signals, continued, timers.requested);
            }
        }
    }

    /// Sleeps until a signal arrives, the report pipe has something, or
    /// `wake_at` passes.
    fn wait_for_event(&self, child: &Child, wake_at: Option<Instant>) -> Result<()> {
        // The report pipe only until `exec` resolves; the unused slot just
        // repeats the wake pipe and isn't passed to `poll`.
        let wake = self.signals.wake_fd();
        let report = child.report_fd();
        let mut fds = [
            PollFd::new(wake, PollFlags::POLLIN),
            PollFd::new(report.unwrap_or(wake), PollFlags::POLLIN),
        ];
        let watched = if report.is_some() { 2 } else { 1 };
        let now = Instant::now();
        let until = wake_at.map_or(SAFETY_NET, |at| {
            at.saturating_duration_since(now).min(SAFETY_NET)
        });
        let timeout = poll_timeout(until);
        match poll(&mut fds[..watched], timeout) {
            // EINTR just means a signal arrived: the loop handles it next.
            Ok(_) | Err(Errno::EINTR) => Ok(()),
            Err(e) => Err(Error::sys("waiting for signals and the command", e)),
        }
    }
}

/// Rejects an isolation that can't work here, before anything starts.
fn check_isolation(isolation: &Isolation) -> Result<()> {
    isolation.validate()?;
    if !cfg!(target_os = "linux") {
        return Err(Error::LinuxOnly {
            feature: "namespace isolation",
            os: std::env::consts::OS,
        });
    }
    // Creating namespaces needs CAP_SYS_ADMIN. Checked here for a clear
    // message; a clone or mount failing with EPERM says the same thing.
    if !nix::unistd::geteuid().is_root() {
        return Err(Error::IsolationNeedsRoot);
    }
    Ok(())
}

/// Keeps shipyard's state in step with the command's, so the shell's job
/// control sees the pair as one job. Only with a terminal: without one,
/// nobody does job control.
///
/// Ctrl-Z needs nothing from us: the terminal stops the whole group. But a
/// command can stop just itself (`kill -STOP $$`, a debugger). Shipyard would
/// keep running, the shell would never see a stopped job, and the terminal
/// would look frozen. So:
///
/// - **The command stopped, shipyard didn't:** shipyard stops itself, so the
///   shell shows the job as stopped. Always with SIGSTOP: SIGTSTP and friends
///   can be ignored, or discarded in an orphaned process group, and then
///   shipyard wouldn't stop.
/// - **Shipyard was continued, the command is still stopped:** continue it
///   too (as sudo does). That covers `kill -CONT <shipyard>`.
///
/// After Ctrl-Z both are stopped and both are continued together, so neither
/// rule fires: the command is running again by the time shipyard looks.
///
/// Shipyard never stops itself when nothing could resume it:
/// - **During a stop request:** a stopped shipyard couldn't SIGKILL the
///   command when `--stop-timeout` runs out. (SIGKILL works on a stopped
///   command, so it's simply left stopped.)
/// - **In an orphaned process group** (no job-control shell above it: the
///   shell exited, or shipyard runs under `ssh -t cmd`, `script`, an IDE):
///   nobody would ever send SIGCONT. The kernel discards terminal stop
///   signals there for the same reason.
fn follow_stops(child: &Child, signals: &Signals, continued: bool, stopping: bool) {
    let Some(signal) = child.stopped() else {
        return;
    };
    if continued {
        child.signal(libc::SIGCONT);
        return;
    }
    if stopping || !under_job_control() {
        tracing::trace!(
            signal,
            "the command is stopped; nothing would resume shipyard"
        );
        return;
    }
    tracing::debug!(signal, "the command stopped itself; stopping too");
    // SAFETY: `kill` has no memory-safety preconditions. This stops
    // shipyard right here, until something sends it SIGCONT.
    unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };
    tracing::debug!("shipyard was continued");
    // That SIGCONT is handled here: left pending, it would read as a new
    // "continued" on the next pass and undo a fresh stop of the command.
    signals.forget(libc::SIGCONT);
    if child.stopped().is_some() {
        child.signal(libc::SIGCONT);
    }
}

/// Whether a job-control shell could resume shipyard's process group: its
/// parent is in the same session but another group (POSIX's definition of a
/// group that isn't orphaned, applied to shipyard's own parent).
fn under_job_control() -> bool {
    use nix::unistd::{getpgid, getpgrp, getppid, getsid};
    let parent = getppid();
    let same_session = matches!(
        (getsid(Some(parent)), getsid(None)),
        (Ok(theirs), Ok(ours)) if theirs == ours
    );
    same_session && getpgid(Some(parent)).is_ok_and(|group| group != getpgrp())
}

/// How the command's process group relates to shipyard's, when a signal is
/// handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// The command has its own group (no terminal).
    Own,
    /// The command shares shipyard's group. `foreground` says whether that
    /// group is the terminal's foreground group, the one the terminal sends
    /// Ctrl-C to. Only macOS needs to know (see [`Terminal::foreground`]).
    Shared { foreground: bool },
}

impl Group {
    fn of(terminal: Option<&Terminal>) -> Self {
        terminal.map_or(Self::Own, |tty| Self::Shared {
            foreground: tty.foreground(),
        })
    }
}

/// Whether to pass `arrival` on to the command.
///
/// - Never what only the command itself sent: it would get it straight back.
/// - In its own group, everything else: nothing reaches it otherwise.
/// - In a shared group, only what another process sent shipyard. A signal
///   from the terminal has reached the command already. Linux marks those as
///   [`Sources::KERNEL`]. macOS can't tell them apart from a `kill`, so there,
///   the signals a terminal sends are not forwarded while shipyard is in the
///   foreground. The cost: on macOS, `kill -INT <shipyard>` from elsewhere
///   doesn't reach a foreground command.
fn should_forward(arrival: Arrival, group: Group) -> bool {
    let Arrival { signal, sources } = arrival;
    match group {
        Group::Own => sources.contains(Sources::OTHER) || sources.contains(Sources::KERNEL),
        Group::Shared { foreground } => {
            sources.contains(Sources::OTHER) && !(foreground && can_come_from_a_terminal(signal))
        }
    }
}

/// The signals a terminal sends its foreground group (or, for hangup, its
/// session). Ctrl-Z's SIGTSTP is never caught, so it isn't listed.
fn can_come_from_a_terminal(signal: c_int) -> bool {
    #[cfg(not(target_os = "linux"))]
    if signal == libc::SIGINFO {
        return true;
    }
    [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGWINCH].contains(&signal)
}

/// Shipyard's controlling terminal, kept open while a command runs.
struct Terminal {
    /// Kept open on macOS to ask for the foreground group; Linux never
    /// needs to ask.
    #[cfg(not(target_os = "linux"))]
    fd: OwnedFd,
}

impl Terminal {
    /// The controlling terminal, if there is one. Opening `/dev/tty` is the
    /// reliable test: checking whether fds 0–2 are terminals misses
    /// `cmd </dev/null >log 2>&1` run from a terminal. Non-blocking, so a
    /// serial line waiting for a carrier can't hang the open.
    fn open() -> Option<Self> {
        open(
            "/dev/tty",
            OFlag::O_RDONLY | OFlag::O_NOCTTY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .ok()
        .map(|fd| {
            #[cfg(target_os = "linux")]
            drop(fd);
            Self {
                #[cfg(not(target_os = "linux"))]
                fd,
            }
        })
    }

    /// Whether shipyard's group is the terminal's foreground group. A
    /// terminal that hung up has no foreground; it counts as one here, since
    /// the hangup signal itself went to the whole group.
    #[cfg(not(target_os = "linux"))]
    fn foreground(&self) -> bool {
        // SAFETY: `tcgetpgrp` only reads the terminal's state; the descriptor
        // is open for as long as `self` lives.
        let foreground = unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) };
        foreground == -1 || foreground == nix::unistd::getpgrp().as_raw()
    }

    /// Linux marks terminal signals as coming from the kernel, so it never
    /// needs to ask.
    #[cfg(target_os = "linux")]
    #[allow(clippy::unused_self, reason = "same signature as on macOS")]
    fn foreground(&self) -> bool {
        false
    }
}

/// The graceful-stop sequence: stop signal, then SIGKILL after the timeout,
/// then periodic warnings if the command still doesn't exit.
#[derive(Debug, Default)]
struct StopTimers {
    /// Whether a stop was requested.
    requested: bool,
    /// When to SIGKILL, once a stop was requested.
    kill_at: Option<Instant>,
    /// When the SIGKILL was sent.
    killed_at: Option<Instant>,
    /// When to next warn that the killed command still hasn't exited.
    next_warning: Option<Instant>,
}

impl StopTimers {
    /// The next moment the loop must wake up for, if any.
    fn next_wake(&self) -> Option<Instant> {
        [self.kill_at, self.next_warning]
            .into_iter()
            .flatten()
            .min()
    }

    /// Asks the command to stop. Repeated requests don't restart the timer.
    fn request(&mut self, child: &Child, stop: StopPolicy) {
        if self.requested {
            return;
        }
        self.requested = true;
        tracing::info!(signal = %stop.signal, timeout = ?stop.timeout, "stopping the command");
        child.signal(stop.signal.as_raw());
        // A stopped process can't act on the stop signal until it's
        // continued, nor can the processes it would pass it on to.
        child.continue_all();
        // An absurd timeout overflows; that just means "wait forever".
        self.kill_at = Instant::now().checked_add(stop.timeout);
    }

    /// Kills the command if its time is up, and warns if it's stuck.
    fn tick(&mut self, child: &Child, stop: StopPolicy, now: Instant) {
        if self.kill_at.is_some_and(|at| now >= at) {
            tracing::info!(timeout = ?stop.timeout, "command didn't stop in time; killing it");
            child.kill_all();
            self.kill_at = None;
            self.killed_at = Some(now);
            self.next_warning = now.checked_add(STUCK_FIRST_WARNING);
        }
        if let (Some(killed), Some(warn_at)) = (self.killed_at, self.next_warning)
            && now >= warn_at
        {
            tracing::warn!(
                pid = child.pid().as_raw(),
                waited = ?now.duration_since(killed),
                "the command was killed but hasn't exited; it may be stuck in the kernel \
                 (for example on a hung network filesystem)"
            );
            self.next_warning = now.checked_add(STUCK_REPEAT_WARNING);
        }
    }
}

/// A poll timeout covering `remaining`, rounded *up* to whole milliseconds:
/// rounding down would make 0.9 ms a zero timeout, and the loop would spin
/// until the deadline instead of sleeping.
fn poll_timeout(remaining: Duration) -> PollTimeout {
    let millis = remaining.as_nanos().div_ceil(1_000_000);
    i32::try_from(millis)
        .ok()
        .and_then(|ms| PollTimeout::try_from(ms).ok())
        .unwrap_or(PollTimeout::MAX)
}

/// Waits for a command that was just sent SIGKILL to exit, then reaps it.
/// Polled rather than a blocking `waitpid`, so a command stuck in the kernel
/// (uninterruptible "D" state) gets the same periodic warnings as in the
/// event loop instead of a silent hang.
fn reap_killed(child: &mut Child) {
    let started = Instant::now();
    let mut warn_at = started.checked_add(STUCK_FIRST_WARNING);
    while !has_exited(child.pid()) {
        let now = Instant::now();
        if warn_at.is_some_and(|at| now >= at) {
            tracing::warn!(
                pid = child.pid().as_raw(),
                waited = ?now.duration_since(started),
                "the command was killed but hasn't exited; it may be stuck in the kernel"
            );
            warn_at = now.checked_add(STUCK_REPEAT_WARNING);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.reap();
}

/// Reaps every child that has exited. Returns the command's status once it
/// has exited.
///
/// Children are *peeked* at first (`WNOWAIT`), so the command is still an
/// unreaped zombie when its process group is killed: until it's reaped, its
/// process and group IDs can't be reused by an unrelated process.
///
/// Other children are reaped quietly, and dropped from `inherited`.
fn reap_children(
    child: &mut Child,
    inherited: &mut Vec<libc::pid_t>,
) -> Result<Option<ExitStatus>> {
    loop {
        let pid = match peek_exited()? {
            Peek::Exited(pid) => pid,
            // macOS can report a stopped child ahead of an exited one: ask
            // about the command directly. (Another exited child stays a
            // zombie until the next check.)
            Peek::Stopped if has_exited(child.pid()) => child.pid(),
            Peek::Nothing | Peek::NoChild | Peek::Stopped => return Ok(None),
        };
        if pid == child.pid() {
            // Anything still running in its group goes with it.
            child.kill_all();
            return child.reap().map(Some);
        }
        reap_quietly(pid);
        inherited.retain(|&other| other != pid.as_raw());
    }
}

/// What a peek at the children found.
enum Peek {
    Nothing,
    /// No such child (ECHILD): none at all, or that one is already reaped.
    NoChild,
    Exited(Pid),
    /// Only macOS: a *stopped* child, reported although only exits were
    /// asked for.
    Stopped,
}

/// The PID of an exited child, without reaping it. Raw `libc::waitid`: nix
/// doesn't provide it on macOS.
///
/// Only real exits count. macOS's `waitid(WEXITED)` also reports a child
/// that is merely *stopped* (`CLD_STOPPED`), which POSIX doesn't allow;
/// taken as an exit, it made shipyard kill a command that someone had
/// paused with `kill -STOP`.
fn peek_exited() -> Result<Peek> {
    peek(libc::P_ALL, 0)
}

/// Whether child `pid` has exited (without reaping it), or is no longer
/// a child at all.
fn has_exited(pid: Pid) -> bool {
    let Ok(id) = libc::id_t::try_from(pid.as_raw()) else {
        return true;
    };
    matches!(peek(libc::P_PID, id), Ok(Peek::Exited(_) | Peek::NoChild))
}

/// `waitid(WEXITED | WNOHANG | WNOWAIT)` for `which`/`id`.
fn peek(which: libc::idtype_t, id: libc::id_t) -> Result<Peek> {
    loop {
        // SAFETY: a zeroed `siginfo_t` is valid; `si_pid` stays 0 if there's
        // nothing to report (POSIX leaves that case unspecified otherwise).
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is valid, writable storage for the result.
        let rc = unsafe {
            libc::waitid(
                which,
                id,
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            match Errno::last() {
                Errno::EINTR => continue,
                Errno::ECHILD => return Ok(Peek::NoChild),
                errno => return Err(Error::sys("checking for exited children", errno)),
            }
        }
        // SAFETY: `waitid` filled `info` for a child event (or left it zeroed).
        let pid = unsafe { info.si_pid() };
        return Ok(match info.si_code {
            _ if pid == 0 => Peek::Nothing,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED => {
                Peek::Exited(Pid::from_raw(pid))
            }
            _ => Peek::Stopped,
        });
    }
}

/// Reaps one exited child that isn't the command.
fn reap_quietly(pid: Pid) {
    let mut raw = 0;
    // SAFETY: `raw` is valid storage; WNOHANG never blocks.
    unsafe { libc::waitpid(pid.as_raw(), &raw mut raw, libc::WNOHANG) };
}

/// With a terminal, pass the host's `TERM` through (unless the user set one),
/// so full-screen programs like `top` or `vim` know how to draw.
fn with_terminal_env(
    spec: &ProcessSpec,
    terminal: bool,
    host_term: Option<std::ffi::OsString>,
) -> Cow<'_, ProcessSpec> {
    let term = OsStr::new("TERM");
    if !terminal || spec.env.get(term).is_some() {
        return Cow::Borrowed(spec);
    }
    let Some(value) = host_term else {
        return Cow::Borrowed(spec);
    };
    let mut with_term = spec.clone();
    match with_term.env.set(term, &value) {
        Ok(()) => Cow::Owned(with_term),
        Err(_) => Cow::Borrowed(spec),
    }
}

/// Linux: kills and reaps every process left behind. As a subreaper,
/// shipyard has inherited them all (even ones that escaped with `setsid`).
/// Children it had before supervising (`inherited`) are left running.
/// Repeats until none are left, since dying ones re-parent their own
/// children to shipyard; gives up after a short deadline and says so.
#[cfg(target_os = "linux")]
fn kill_leftovers(inherited: &mut Vec<libc::pid_t>) {
    const DEADLINE: Duration = Duration::from_secs(2);
    let started = Instant::now();
    loop {
        reap_exited_children(inherited);
        let Some(children) = direct_children() else {
            tracing::warn!(
                "/proc belongs to another PID namespace; \
                 processes left behind by the command were not cleaned up"
            );
            return;
        };
        let leftovers: Vec<libc::pid_t> = children
            .into_iter()
            .filter(|pid| !inherited.contains(pid))
            .collect();
        if leftovers.is_empty() {
            return;
        }
        if started.elapsed() >= DEADLINE {
            tracing::warn!(
                count = leftovers.len(),
                "processes left behind by the command didn't exit in time"
            );
            return;
        }
        for pid in &leftovers {
            // SAFETY: `kill` has no memory-safety preconditions. These are
            // our unreaped children, so their PIDs can't have been reused.
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reaps every child that has exited, dropping each from `inherited`.
#[cfg(target_os = "linux")]
fn reap_exited_children(inherited: &mut Vec<libc::pid_t>) {
    loop {
        let mut raw = 0;
        // SAFETY: `raw` is valid storage; WNOHANG never blocks.
        match unsafe { libc::waitpid(-1, &raw mut raw, libc::WNOHANG) } {
            -1 if Errno::last() == Errno::EINTR => {}
            -1 | 0 => return, // none left, or none exited yet
            pid => inherited.retain(|&other| other != pid),
        }
    }
}

/// shipyard's direct children, from `/proc/self/task/*/children` (one file per
/// thread), or by scanning `/proc/*/stat` for our PID if that file is missing
/// (kernels built without `CONFIG_PROC_CHILDREN`).
///
/// `None` if `/proc` belongs to another PID namespace (mounted for an
/// ancestor, as after `unshare -p -f` without `--mount-proc`): its PIDs
/// would name different processes than `kill` does here.
#[cfg(target_os = "linux")]
fn direct_children() -> Option<Vec<libc::pid_t>> {
    let ours = std::fs::read_link("/proc/self")
        .ok()
        .and_then(|link| link.to_str()?.parse::<u32>().ok())
        == Some(std::process::id());
    if !ours {
        return None;
    }
    let lists: Vec<String> = std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .collect();
    if lists.is_empty() {
        // No `children` file could be read: fall back to the slow scan. (An
        // empty file just means no children, and needs no scan.)
        return Some(children_by_scanning_proc());
    }
    Some(
        lists
            .iter()
            .flat_map(|list| list.split_whitespace().filter_map(|pid| pid.parse().ok()))
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn children_by_scanning_proc() -> Vec<libc::pid_t> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // Format: "pid (comm) state ppid ...". The name can contain
            // spaces and parentheses, so parse from the last ')'.
            let after_name = stat.get(stat.rfind(')')?.checked_add(2)?..)?;
            let ppid: u32 = after_name.split_whitespace().nth(1)?.parse().ok()?;
            (ppid == me).then_some(pid)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_timeouts_round_up() {
        assert_eq!(
            poll_timeout(Duration::from_micros(900)),
            PollTimeout::try_from(1).unwrap()
        );
        assert_eq!(
            poll_timeout(Duration::from_millis(5)),
            PollTimeout::try_from(5).unwrap()
        );
        assert_eq!(poll_timeout(Duration::ZERO), PollTimeout::ZERO);
        assert_eq!(poll_timeout(Duration::MAX), PollTimeout::MAX);
    }

    #[test]
    #[allow(clippy::zombie_processes, reason = "reaped by hand with waitpid below")]
    fn waitid_peeks_without_reaping() {
        // Checks the platform's `waitid(WNOWAIT)`, which macOS implements
        // less commonly used: the same child must show up until reaped.
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let peek = || {
            // SAFETY: zeroed `siginfo_t` is valid storage.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let id = libc::id_t::try_from(pid).unwrap();
            // SAFETY: as in `peek_exited`; this waits for this child only.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    id,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            assert_eq!(rc, 0);
            // SAFETY: filled in by `waitid`.
            unsafe { info.si_pid() }
        };
        assert_eq!(peek(), pid);
        assert_eq!(peek(), pid, "a peek must not reap");
        let mut raw = 0;
        // SAFETY: reaping the child this test started.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut raw, 0) }, pid);
        assert_eq!(
            ExitStatus::from_wait_status(raw),
            Some(ExitStatus::Exited(3))
        );
    }

    #[test]
    fn term_is_passed_through_only_with_a_terminal() {
        let mut spec = ProcessSpec {
            args: vec!["true".into()],
            env: crate::process::Env::new(),
            cwd: "/".into(),
            user: crate::process::User::current(),
            rlimits: Vec::new(),
        };
        let term = |spec: &ProcessSpec, terminal, host: Option<&str>| {
            with_terminal_env(spec, terminal, host.map(Into::into))
                .env
                .get(OsStr::new("TERM"))
                .map(ToOwned::to_owned)
        };
        assert_eq!(term(&spec, false, Some("xterm")), None);
        assert_eq!(term(&spec, true, None), None);
        assert_eq!(term(&spec, true, Some("xterm")), Some("xterm".into()));
        spec.env
            .set(OsStr::new("TERM"), OsStr::new("dumb"))
            .unwrap();
        assert_eq!(
            term(&spec, true, Some("xterm")),
            Some("dumb".into()),
            "the user's TERM wins"
        );
    }

    fn arrival(signal: c_int, sources: Sources) -> Arrival {
        Arrival { signal, sources }
    }

    #[test]
    fn in_its_own_group_everything_but_its_own_signals_is_forwarded() {
        let own = Group::Own;
        assert!(should_forward(arrival(libc::SIGINT, Sources::OTHER), own));
        assert!(should_forward(arrival(libc::SIGHUP, Sources::KERNEL), own));
        assert!(!should_forward(
            arrival(libc::SIGUSR1, Sources::COMMAND),
            own
        ));
        assert!(should_forward(
            arrival(libc::SIGUSR1, Sources::COMMAND.union(Sources::OTHER)),
            own
        ));
    }

    #[test]
    fn in_a_shared_group_terminal_signals_are_not_delivered_twice() {
        let background = Group::Shared { foreground: false };
        let foreground = Group::Shared { foreground: true };
        // Linux: the kernel's Ctrl-C already reached the command.
        assert!(!should_forward(
            arrival(libc::SIGINT, Sources::KERNEL),
            background
        ));
        // A `kill` from another process is forwarded...
        assert!(should_forward(
            arrival(libc::SIGINT, Sources::OTHER),
            background
        ));
        assert!(should_forward(
            arrival(libc::SIGUSR1, Sources::OTHER),
            foreground
        ));
        // ...unless (macOS) it may have been the terminal's own.
        assert!(!should_forward(
            arrival(libc::SIGINT, Sources::OTHER),
            foreground
        ));
        assert!(!should_forward(
            arrival(libc::SIGWINCH, Sources::OTHER),
            foreground
        ));
        assert!(!should_forward(
            arrival(libc::SIGUSR1, Sources::COMMAND),
            background
        ));
    }
}
