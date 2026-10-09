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
use std::path::Path;
use std::time::{Duration, Instant};

use libc::c_int;
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::stat::Mode;
use nix::unistd::Pid;

use super::signals::{Arrival, Signals, Sources};
use super::spawn::{Child, Placement, SpawnOptions, spawn};
use super::spec::ProcessSpec;
use super::status::ExitStatus;
use super::stop::StopPolicy;
use crate::hooks::{ContainerHooks, Phase};
use crate::isolation::{Isolation, Join, Namespace};
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

/// A caller's network setup for a container: called with the container
/// ([`NetworkTarget`]) before anything in it runs (see
/// [`Supervisor::supervise_with_network`]); its error message is reported
/// as [`Error::Network`].
pub type NetworkHook<'a> = &'a mut dyn FnMut(NetworkTarget<'_>) -> Result<(), String>;

/// The container a [`NetworkHook`] sets up: its init's process ID (in
/// shipyard's PID namespace: a helper like pasta joins its namespaces by
/// it), and a descriptor of its network namespace.
#[derive(Debug, Clone, Copy)]
pub struct NetworkTarget<'a> {
    /// The container's init.
    pub pid: libc::pid_t,
    /// Its network namespace.
    pub netns: std::os::fd::BorrowedFd<'a>,
}

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
    ///   shell's job control doesn't extend to it. The container is set up
    ///   by this program's binary, re-exec'd (with a PID namespace, it then
    ///   stays as the container's init): `main` must call
    ///   [`init_entry_point`](super::init_entry_point) first.
    ///
    /// # Errors
    ///
    /// Invalid input, isolation on macOS, without root or without the init
    /// entry point, a container init that failed to start
    /// ([`Error::InitDidNotStart`]), or a failure to start the command (see
    /// [`Error::exit_code`]).
    pub fn supervise(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        isolation: Option<&Isolation>,
    ) -> Result<ExitStatus> {
        let placement = isolation.map_or(Placement::Host, |isolation| {
            Placement::New(Box::new(isolation.clone()))
        });
        self.run(spec, stop, placement, None)
    }

    /// Like [`supervise`](Self::supervise), with `network` called once the
    /// container's namespaces exist and before anything in it runs: with a
    /// descriptor of its network namespace, to give it interfaces and
    /// addresses from outside (as an engine does). Every signal is held
    /// while it runs, so it must finish in bounded time. An error from it
    /// stops the container before it starts ([`Error::Network`]).
    ///
    /// # Errors
    ///
    /// As [`supervise`](Self::supervise), and the hook's own; and
    /// [`Error::Network`] if `isolation` doesn't make a network namespace
    /// (the hook would configure the host's, or one the container joins).
    pub fn supervise_with_network(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        isolation: &Isolation,
        network: NetworkHook<'_>,
    ) -> Result<ExitStatus> {
        // The hook configures the namespace it's given: never the host's, or
        // one the container joins (someone else's).
        if !isolation.makes_network_namespace() {
            return Err(Error::Network(
                "a network hook needs a network namespace made for the container".to_owned(),
            ));
        }
        let placement = Placement::New(Box::new(isolation.clone()));
        self.run(spec, stop, placement, Some(network))
    }

    /// Like [`supervise`](Self::supervise), but the command starts inside
    /// the namespaces of a process that's already running (`join.target`,
    /// normally a container's init), as `exec` into a container does
    /// (Linux).
    ///
    /// The command is shipyard's own child, supervised exactly like an
    /// isolated one: a session of its own, signals forwarded, the same stop
    /// policy. Inside the container it's an ordinary process (not PID 1);
    /// its parent is outside, so `getppid()` there is 0. When it exits,
    /// whatever is left in its process group is killed, as for
    /// [`supervise`](Self::supervise); processes that left the group belong
    /// to the container's init. If the container ends, the kernel kills the
    /// command (SIGKILL, so 137).
    ///
    /// # Errors
    ///
    /// An invalid `join`, Linux only, root only ([`Error::JoinNeedsRoot`], or
    /// [`Error::JoinRefused`] as root), no such process
    /// ([`Error::NoSuchProcess`]), a container that is exiting
    /// ([`Error::ContainerExiting`]), a `/proc` of another PID namespace
    /// ([`Error::ForeignProc`]), or a failure to start the command.
    pub fn supervise_joined(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        join: &Join,
    ) -> Result<ExitStatus> {
        self.run(spec, stop, Placement::Join(join.clone()), None)
    }

    fn run(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        placement: Placement,
        network: Option<NetworkHook<'_>>,
    ) -> Result<ExitStatus> {
        stop.validate()?;
        match &placement {
            Placement::Host => {}
            Placement::New(isolation) => check_isolation(isolation)?,
            Placement::Join(join) => check_join(join)?,
        }
        let terminal = Terminal::open();
        // With a session of its own, the command never shares the terminal's
        // group, whether or not shipyard has one.
        let own_session = placement.has_own_session();
        let job_terminal = terminal.as_ref().filter(|_| !own_session);
        tracing::debug!(
            terminal = terminal.is_some(),
            placement = ?placement,
            "{}",
            if job_terminal.is_some() {
                "sharing shipyard's process group with the command"
            } else {
                "giving the command its own process group"
            }
        );
        let spec = with_terminal_env(spec, terminal.is_some(), std::env::var_os("TERM"));
        let spec = resolve_names(spec, &placement)?;
        #[cfg(target_os = "linux")]
        let cgroup = make_run_cgroup(&placement)?;
        #[cfg(target_os = "linux")]
        let hooks = placement
            .isolation()
            .and_then(|isolation| isolation.hooks.clone());
        #[cfg(not(target_os = "linux"))]
        let hooks = None;
        let options = SpawnOptions {
            placement,
            new_process_group: job_terminal.is_none(),
            keep_ignored: self
                .signals
                .ignored_at_startup()
                .iter()
                .copied()
                .filter(|signal| KEEP_IGNORED.contains(signal))
                .collect(),
            gate: None,
            #[cfg(target_os = "linux")]
            cgroup: cgroup.as_ref().map(|run| run.cgroup().clone()),
        };
        let started = |pid: Pid| self.signals.set_child(pid.as_raw());
        // Whether shipyard itself killed the command (its stop timeout): then
        // a SIGKILL isn't the OOM killer's.
        #[cfg_attr(
            not(target_os = "linux"),
            allow(unused_assignments, reason = "only Linux reports OOM kills")
        )]
        let mut killed_by_us = false;
        let result = spawn(&spec, &options, network, started).and_then(|mut child| {
            let result = self.watch(&mut child, stop, job_terminal, own_session, hooks.as_ref());
            if result.is_err() {
                // Never leave the command running unsupervised.
                child.kill_all();
                reap_killed(&mut child);
            }
            result.map(|ended| {
                killed_by_us = ended.killed_by_us;
                ended.status
            })
        });
        #[cfg(target_os = "linux")]
        kill_leftovers(&mut self.inherited);
        #[cfg(target_os = "linux")]
        if let Some(run) = cgroup {
            if let Ok(status) = &result {
                report_oom(run.cgroup(), *status, killed_by_us);
            }
            if let Err(err) = run.cgroup().remove(CGROUP_REMOVAL) {
                tracing::warn!("{err}");
            }
        }
        self.signals.clear_child();
        // The container is gone, whether it ran or its setup failed: as the
        // spec has it, `poststop` follows either way.
        if let Some(hooks) = &hooks {
            run_warning(hooks, Phase::Poststop, None);
        }
        result
    }

    /// The event loop. Returns once the command has exited and been reaped.
    /// Runs `hooks`' `poststart` once the command's `exec` resolved.
    fn watch(
        &mut self,
        child: &mut Child,
        stop: StopPolicy,
        terminal: Option<&Terminal>,
        own_session: bool,
        hooks: Option<&ContainerHooks>,
    ) -> Result<Ended> {
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
            let resolving = child.report_fd().is_some();
            child.check_exec()?;
            if resolving
                && child.report_fd().is_none()
                && let Some(hooks) = hooks
            {
                // The command runs: failures are only warnings now.
                run_warning(hooks, Phase::Poststart, Some(child.pid().as_raw()));
            }
            if let Some(status) = reap_children(child, &mut self.inherited)? {
                return Ok(Ended {
                    status,
                    killed_by_us: timers.killed_at.is_some(),
                });
            }
            if terminal.is_some() {
                follow_stops(child, &self.signals, continued, timers.requested);
            } else if continued && own_session {
                // Resumed (`fg` after Ctrl-Z). A command in a session of its
                // own got no SIGCONT from the shell: if it had stopped itself,
                // only this resumes it. (One that hadn't just gets a harmless
                // SIGCONT.)
                child.continue_all();
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

/// Rejects a join that can't work here, before anything starts.
fn check_join(join: &Join) -> Result<()> {
    join.validate()?;
    if !cfg!(target_os = "linux") {
        return Err(Error::LinuxOnly {
            feature: "joining namespaces",
            os: std::env::consts::OS,
        });
    }
    // Joining namespaces needs CAP_SYS_ADMIN, as creating them does.
    if !nix::unistd::geteuid().is_root() {
        return Err(Error::JoinNeedsRoot);
    }
    Ok(())
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
    // Every isolated run re-execs this binary as the container's clean
    // image (see `init.rs`).
    #[cfg(target_os = "linux")]
    if !super::has_init_entry_point() {
        return Err(Error::NoInitEntryPoint);
    }
    if let Some(root) = &isolation.root {
        // Here, for a clear message: inside, a missing directory would only
        // be "binding the root filesystem onto itself: No such file".
        let reason = match std::fs::metadata(&root.path) {
            Ok(metadata) if metadata.is_dir() => None,
            Ok(_) => Some("isn't a directory".to_owned()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Some("doesn't exist".to_owned())
            }
            Err(err) => Some(err.to_string()),
        };
        if let Some(reason) = reason {
            return Err(Error::RootFilesystem {
                path: root.path.clone(),
                reason,
            });
        }
    }
    // Creating namespaces needs CAP_SYS_ADMIN: root has it, and anyone has
    // it inside a user namespace of their own (rootless). Checked here for a
    // clear message; a clone or mount failing with EPERM says the same.
    if !nix::unistd::geteuid().is_root() && !isolation.has(Namespace::User) {
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

/// How the command ended.
struct Ended {
    status: ExitStatus,
    /// Shipyard killed it: the stop timeout ran out.
    killed_by_us: bool,
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
/// Resolves user and group names where the command runs: for an isolated
/// run that's in the container, so the container's init does it (`init.rs`);
/// for a run on the host, here, in the host's `/etc`. A joined command's
/// names would need the container's `/etc`, read safely through its root:
/// not yet, so they're refused.
fn resolve_names<'a>(
    spec: Cow<'a, ProcessSpec>,
    placement: &Placement,
) -> Result<Cow<'a, ProcessSpec>> {
    let etc = match placement {
        Placement::New(_) => return Ok(spec),
        Placement::Host => Some(Path::new("/etc")),
        Placement::Join(_) if !spec.user.names.is_empty() => {
            return Err(Error::NamesWhenJoining);
        }
        Placement::Join(_) => None,
    };
    let mut spec = spec.into_owned();
    super::identity::resolve(&mut spec, etc)?;
    Ok(Cow::Owned(spec))
}

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
    if !super::proc_is_ours() {
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

/// How long the end of a run waits for its cgroup to empty: a killed
/// process goes at once, unless it's stuck in the kernel.
#[cfg(target_os = "linux")]
const CGROUP_REMOVAL: Duration = Duration::from_secs(5);

/// The cgroup an isolated run gets: `run/<shipyard's pid>`, made here and
/// held (locked) until the run ends, with the run's limits written in before
/// anything runs in it. Below this process's part of the tree: root's, or a
/// user's delegated subtree.
///
/// Without limits, `None` where none can be made (no delegation, cgroup v1, a
/// read-only tree): the run goes on without one. With limits, that's an
/// error: they'd be silently missing. So is real root failing to claim one
/// in a tree it found.
#[cfg(target_os = "linux")]
fn make_run_cgroup(placement: &Placement) -> Result<Option<crate::cgroup::RunCgroup>> {
    use crate::cgroup;
    let Some(isolation) = placement.isolation() else {
        return Ok(None);
    };
    let resources = &isolation.resources;
    let limited = !resources.is_empty();
    let tree = match cgroup::Tree::find() {
        Ok(tree) => tree,
        Err(why) if limited => {
            return Err(Error::LimitsNeedACgroup {
                why: why.to_string().into(),
            });
        }
        Err(why) => {
            tracing::debug!("running without a cgroup of its own: {why}");
            warn_devices_without_cgroup(isolation, &why);
            return Ok(None);
        }
    };
    // Runs killed before they could clean up left theirs.
    tree.sweep_stale_runs();
    let run = match tree.claim_run(std::process::id()) {
        Ok(run) => run,
        Err(err) if limited => {
            return Err(Error::LimitsNeedACgroup {
                why: err.to_string().into(),
            });
        }
        // Root on a tree it can use always gets one: a claim that fails
        // there (`run/<pid>` taken, a cgroup holding processes in the way)
        // is a fault, and going on would drop the device rules unwarned.
        Err(err) if cgroup::is_host_root() => return Err(err.into()),
        Err(err) => {
            tracing::debug!("running without a cgroup of its own: {err}");
            warn_devices_without_cgroup(isolation, &err);
            return Ok(None);
        }
    };
    // Limits, then the device program, before anything runs in it.
    let confined = run.cgroup().limit(resources).and_then(|skipped| {
        let devices = cgroup::enforce_devices(run.cgroup(), &isolation.devices)?;
        Ok(skipped.into_iter().chain(devices))
    });
    match confined {
        Ok(warnings) => {
            for warning in warnings {
                tracing::warn!("{warning}");
            }
            Ok(Some(run))
        }
        Err(err) => {
            let _ = run.cgroup().remove(CGROUP_REMOVAL);
            Err(err.into())
        }
    }
}

/// Warns that `isolation`'s device rules, if asked for, can't be enforced:
/// the container has no cgroup, for `why`.
#[cfg(target_os = "linux")]
fn warn_devices_without_cgroup(isolation: &Isolation, why: &impl std::fmt::Display) {
    for warning in crate::cgroup::devices_without_cgroup(isolation, why) {
        tracing::warn!("{warning}");
    }
}

/// Runs `hooks`' `phase` (for the init `pid`), each failure a warning.
fn run_warning(hooks: &ContainerHooks, phase: Phase, pid: Option<i32>) {
    for warning in hooks.run_all(phase, pid) {
        tracing::warn!("{warning}");
    }
}

/// Says whether the OOM killer struck in the container: the command itself
/// (it died of SIGKILL, a kill was counted, and shipyard didn't send it), or
/// another process in it.
#[cfg(target_os = "linux")]
fn report_oom(cgroup: &crate::cgroup::Cgroup, status: ExitStatus, killed_by_us: bool) {
    let events = cgroup.oom_events();
    let kills = events.kills;
    if kills == 0 {
        return;
    }
    let cause = events.cause();
    if status.killed_by_sigkill() && !killed_by_us {
        tracing::warn!("the command was killed: {cause}");
    } else if kills == 1 {
        tracing::warn!("a process in the container was killed: {cause}");
    } else {
        tracing::warn!("{kills} processes in the container were killed: {cause}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn isolation_is_linux_only() {
        assert!(matches!(
            check_isolation(&Isolation::standard()),
            Err(Error::LinuxOnly { .. })
        ));
    }

    /// This test binary never calls `init_entry_point` (nothing in this
    /// crate's tests does), so it can't be a container's init.
    #[cfg(target_os = "linux")]
    #[test]
    fn isolation_needs_the_init_entry_point() {
        assert!(matches!(
            check_isolation(&Isolation::standard()),
            Err(Error::NoInitEntryPoint)
        ));
    }

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
            oom_score_adj: None,
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
