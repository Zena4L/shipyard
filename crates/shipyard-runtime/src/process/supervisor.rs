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
//! starts can be killed together. An isolated command never gets the host
//! terminal (stage 8): it gets pipes, or with `-t` a pseudo-terminal of its
//! own (made inside the container), whose session it leads.
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
use super::stdio::{DRAIN_WAIT, Relay, Stdio};
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
    /// The command's standard descriptors (see [`set_stdio`](Self::set_stdio)).
    stdio: Stdio,
    /// Called once the command's `exec` has succeeded, during a run of
    /// [`supervise_exec`](Self::supervise_exec), with its PID (as the host
    /// sees it).
    running: Option<Box<dyn FnOnce(u32)>>,
    /// Descriptors to pass on to the next command (see
    /// [`pass`](Self::pass)): closed once it has its own.
    passed: Option<super::passed::PassedFds>,
    /// Whether this process was a child subreaper before it was installed:
    /// put back when it's dropped (F29: a program that embeds the runtime
    /// and lives on mustn't stay one).
    #[cfg(target_os = "linux")]
    subreaper_before: bool,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if !self.subreaper_before {
            let _ = nix::sys::prctl::set_child_subreaper(false);
        }
    }
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}

impl Supervisor {
    /// Passes `passed` on to the next command started (`--preserve-fds`,
    /// socket activation: see `passed.rs`), at 3 and up. This keeps them
    /// until then, and closes them once it has its own.
    pub fn pass(&mut self, passed: super::passed::PassedFds) {
        self.passed = Some(passed);
    }

    /// The descriptors to pass on to the next command, taken: held by the
    /// caller until it's spawned, then dropped.
    fn take_passed(&mut self) -> Option<super::passed::PassedFds> {
        self.passed.take().filter(|passed| !passed.is_empty())
    }

    /// Installs the signal handlers (see [`Supervisor`]) and, on Linux, makes
    /// shipyard a *child subreaper*: processes the command leaves behind are
    /// re-parented to shipyard instead of init, so it can clean them up.
    /// Dropped, it lets both go (the handlers, and the subreaper setting as
    /// it found it).
    ///
    /// **Install it on the thread that starts the commands:** the death
    /// signal each child is given (`PR_SET_PDEATHSIG`) is its forking
    /// thread's, so a child of a thread that ends before it would be
    /// killed. And it counts on Rust's start having set SIGPIPE to be
    /// ignored (a write to a gone reader is an error, not a death): a
    /// program that restores SIGPIPE's default must block it around a
    /// supervised run.
    pub fn install() -> Result<Self> {
        Self::installed_with(Signals::install()?)
    }

    /// [`install`](Self::install), with `signals` already claimed.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::unnecessary_wraps, reason = "its fallible steps are Linux's")
    )]
    fn installed_with(signals: Signals) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let subreaper_before = nix::sys::prctl::get_child_subreaper().unwrap_or(false);
        #[cfg(target_os = "linux")]
        nix::sys::prctl::set_child_subreaper(true)
            .map_err(|e| Error::sys("becoming a child subreaper", e))?;
        #[cfg(target_os = "linux")]
        let inherited = direct_children().unwrap_or_default();
        #[cfg(not(target_os = "linux"))]
        let inherited = Vec::new();
        Ok(Self {
            signals,
            inherited,
            stdio: Stdio::default(),
            running: None,
            passed: None,
            #[cfg(target_os = "linux")]
            subreaper_before,
        })
    }

    /// What the commands supervised from now on get as their standard
    /// input, output and error: shipyard's own by default. A
    /// [`Relay`](super::Stream::Relay) stream is a pipe, relayed to or from
    /// shipyard's own descriptor for as long as the command runs, and then
    /// until its last output is written out (see `stdio.rs`), for a joined
    /// command too (`exec`'s, where one of shipyard's is a terminal: see
    /// `Stdio::sparing_terminals`).
    pub fn set_stdio(&mut self, stdio: Stdio) {
        self.stdio = stdio;
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
    /// ([`Error::InitDidNotStart`]), the relay's descriptors that can't be
    /// opened (see [`set_stdio`](Self::set_stdio); [`Error::LinuxOnly`]
    /// elsewhere), or a failure to start the command (see
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
        self.run(spec, stop, placement, None, RunExtras::default())
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
        self.run(spec, stop, placement, Some(network), RunExtras::default())
    }

    /// Like [`supervise`](Self::supervise), but the command starts inside
    /// the namespaces of a process that's already running (`join.target`,
    /// normally a container's init) (Linux).
    ///
    /// The primitive under `exec`: `shipyard-runtime exec` goes through
    /// `supervise_exec`, which adds the container's confinement from its
    /// snapshot, its `running` check, and what's said when the command never
    /// ran (`Error::ExecHeldUp`). This joins and supervises, nothing more.
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
    /// An invalid `join`, Linux only, not this user's ([`Error::JoinNotYours`], or
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
        self.run(
            spec,
            stop,
            Placement::Join(join.clone()),
            None,
            RunExtras::default(),
        )
    }

    /// `exec`: like [`supervise_joined`](Self::supervise_joined), with the
    /// command's environment exactly as `spec` has it (no `TERM` added), and
    /// `running` called with its PID once its `exec` has succeeded (the
    /// container's lock is released then, and its PID file written: see
    /// `container::exec`). With a terminal: relayed
    /// as [`set_stdio`](Self::set_stdio)'s says, or, with `console` (an OCI
    /// console socket, open, and the terminal's size), its master sent there
    /// and nothing relayed.
    ///
    /// # Errors
    ///
    /// As [`supervise_joined`](Self::supervise_joined).
    pub fn supervise_exec(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        join: &Join,
        console: Option<(std::os::fd::RawFd, super::terminal::Size)>,
        running: impl FnOnce(u32) + 'static,
    ) -> Result<ExitStatus> {
        self.running = Some(Box::new(running));
        let result = self.run(
            spec,
            stop,
            Placement::Join(join.clone()),
            None,
            RunExtras {
                exact_env: true,
                console,
            },
        );
        // Not called (the command never ran): dropped now.
        self.running = None;
        result
    }

    /// `exec -d`: starts `spec` in `join`'s namespaces and returns its PID
    /// once its `exec` has succeeded, leaving it running: it outlives
    /// shipyard (no parent-death signal), and whoever inherits it reaps it
    /// (the caller, while it lives: it's the caller's child).
    ///
    /// # Errors
    ///
    /// As [`supervise_joined`](Self::supervise_joined): the command never
    /// ran (it's been reaped).
    pub fn start_detached(
        &mut self,
        spec: &ProcessSpec,
        join: &Join,
        console: Option<(std::os::fd::RawFd, super::terminal::Size)>,
    ) -> Result<u32> {
        // Taken first: a refusal below lets go of them too.
        let held = self.take_passed();
        check_join(join)?;
        let placement = Placement::Join(join.clone());
        check_stdio(&placement, self.stdio)?;
        // Nobody relays once this returns: `/dev/null`, not a relay, in
        // place of a terminal (see `Stdio::sparing_terminals`, which never
        // asks for one here; this guards other callers).
        if [self.stdio.stdin, self.stdio.stdout, self.stdio.stderr].contains(&super::Stream::Relay)
        {
            return Err(Error::sys(
                "a detached command's standard descriptors can't be relayed",
                Errno::EINVAL,
            ));
        }
        let (child_ends, _) = super::stdio::prepare(self.stdio, true)?;
        let spec = resolve_names(Cow::Borrowed(spec), &placement)?;
        let options = SpawnOptions {
            stdio: child_ends.targets(),
            placement,
            keep_ignored: self.keep_ignored(),
            #[cfg(target_os = "linux")]
            exec: super::spawn::ExecSpawn { detach: true },
            // Its terminal, if it has one, goes to the console socket.
            terminal: console.map(|(fd, size)| super::stdio::ChildTerminal::Console { fd, size }),
            takes_terminal: console.is_some(),
            passing: held.as_ref().map(super::passed::PassedFds::plan),
            ..SpawnOptions::default()
        };
        let spawned = spawn(&spec, &options, None, |_| {});
        // The command has its own copies now (or there's no command).
        drop((held, child_ends));
        let mut child = spawned?;
        let by = Instant::now() + EXEC_WAIT;
        loop {
            if let Err(err) = child.check_exec().and_then(|()| match child.report_fd() {
                Some(_) => exec_not_held_up(&child, by),
                None => Ok(()),
            }) {
                child.kill_all();
                reap_killed(&mut child);
                return Err(err);
            }
            if child.report_fd().is_none() {
                // Run, unless it was killed before its exec (by the container,
                // or with it).
                if let Ok(
                    nix::sys::wait::WaitStatus::Exited(..)
                    | nix::sys::wait::WaitStatus::Signaled(..),
                ) =
                    nix::sys::wait::waitpid(child.pid(), Some(nix::sys::wait::WaitPidFlag::WNOHANG))
                {
                    return Err(Error::ExecHeldUp(
                        "it ended before it ran (killed by the container, or with it)",
                    ));
                }
                return Ok(child.pid().as_raw().cast_unsigned());
            }
            if let Some(report) = child.report_fd() {
                let mut fds = [PollFd::new(report, PollFlags::POLLIN)];
                let _ = poll(&mut fds, PollTimeout::from(100_u16));
            }
        }
    }

    fn run(
        &mut self,
        spec: &ProcessSpec,
        stop: StopPolicy,
        placement: Placement,
        network: Option<NetworkHook<'_>>,
        extras: RunExtras,
    ) -> Result<ExitStatus> {
        stop.validate()?;
        match &placement {
            Placement::Host => {}
            Placement::New(isolation) => check_isolation(isolation)?,
            Placement::Join(join) => check_join(join)?,
        }
        check_stdio(&placement, self.stdio)?;
        let terminal = Terminal::open();
        // With a session of its own, the command never shares the terminal's
        // group, whether or not shipyard has one. A command with a
        // pseudo-terminal starts one to take it, isolated or not.
        let pty = self.stdio.terminal.is_some();
        let own_session = placement.has_own_session() || pty;
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
        let spec = self.with_term_unless(spec, terminal.is_some(), extras.exact_env);
        let spec = resolve_names(spec, &placement)?;
        // Before the run's cgroup: a failure here leaves nothing to remove.
        let (child_ends, mut relay) =
            super::stdio::prepare(self.stdio, !matches!(placement, Placement::Host))?;
        #[cfg(target_os = "linux")]
        let cgroup = make_run_cgroup(&placement)?;
        let hooks = container_hooks(&placement);
        let held = self.take_passed();
        let options = SpawnOptions {
            stdio: child_ends.targets(),
            terminal: extras
                .console
                .map(|(fd, size)| super::stdio::ChildTerminal::Console { fd, size })
                .or(child_ends.terminal()),
            takes_terminal: pty || extras.console.is_some(),
            placement,
            // A command taking a terminal starts its own session: it mustn't
            // be made a group leader first.
            new_process_group: job_terminal.is_none() && !pty,
            keep_ignored: self.keep_ignored(),
            gate: None,
            #[cfg(target_os = "linux")]
            cgroup: cgroup.as_ref().map(|run| run.cgroup().clone()),
            #[cfg(target_os = "linux")]
            exec: super::spawn::ExecSpawn { detach: false },
            passing: held.as_ref().map(super::passed::PassedFds::plan),
        };
        let started = |pid: Pid| self.signals.set_child(pid.as_raw());
        // Whether shipyard itself killed the command (its stop timeout): then
        // a SIGKILL isn't the OOM killer's.
        #[cfg_attr(
            not(target_os = "linux"),
            allow(unused_assignments, reason = "only Linux reports OOM kills")
        )]
        let mut killed_by_us = false;
        // Whether the run ended on a stop request (or an error): then its last
        // output gets only the drain's time, not whatever a stalled terminal
        // would take.
        let mut stop_requested = false;
        let spawned = spawn(&spec, &options, network, started);
        // The child has its copies (or there's none): with ours closed, its
        // stdin ends when the relay closes it, and so do passed sockets.
        drop((child_ends, held));
        let result = spawned.and_then(|mut child| {
            let result = self.watch(
                &mut child,
                stop,
                job_terminal,
                own_session,
                hooks.as_ref(),
                relay.as_mut(),
            );
            if result.is_err() {
                // Never leave the command running unsupervised.
                child.kill_all();
                reap_killed(&mut child);
            }
            result.map(|ended| {
                killed_by_us = ended.killed_by_us;
                stop_requested = ended.stop_requested;
                ended.status
            })
        });
        #[cfg(target_os = "linux")]
        kill_leftovers(&mut self.inherited);
        // Whichever way the run ended (an init's failure message comes this
        // way too): the last output goes out, the user's input stays theirs.
        if let Some(relay) = &mut relay {
            self.drain(relay, stop_requested || result.is_err());
            // The user's terminal is theirs again now, not after the
            // cleanup below (a few seconds, at worst).
            relay.release_terminal();
        }
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
        mut relay: Option<&mut Relay>,
    ) -> Result<Ended> {
        let mut timers = StopTimers::default();
        // `exec`'s command must run (exec) by then: until it does, it holds
        // the container's lock.
        let exec_by = self.running.is_some().then(|| Instant::now() + EXEC_WAIT);
        loop {
            let wake = match (
                timers.next_wake(),
                exec_by.filter(|_| child.report_fd().is_some()),
            ) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            self.wait_for_event(child, wake, relay.as_deref())?;
            let mut continued = false;
            // With a pseudo-terminal relayed, the user's terminal's own
            // signals are for it, not the command: a resize resizes it (the
            // kernel tells its foreground job), a hang-up hangs it up. A
            // `kill -HUP` while the terminal is there is forwarded as ever.
            let pty = relay.as_deref().is_some_and(Relay::has_terminal);
            for arrival in self.signals.drain() {
                match arrival.signal {
                    // Whoever sent it, it's a stop request.
                    libc::SIGTERM => timers.request(child, stop),
                    libc::SIGCONT => continued = true,
                    libc::SIGWINCH if pty => {
                        if let Some(relay) = relay.as_deref() {
                            relay.resize();
                        }
                    }
                    // The user's terminal hung up (its shell passes the
                    // kernel's SIGHUP on to its jobs): the terminal says so,
                    // the sender can't.
                    libc::SIGHUP if pty && super::terminal::host_hung_up() => {
                        if let Some(relay) = relay.as_deref_mut() {
                            relay.hang_up();
                        }
                    }
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
            if let Some(relay) = relay.as_deref_mut() {
                relay.pump();
            }
            // The exec report comes before reaping: a failed exec must be
            // reported as such, not as an ordinary exit.
            let resolving = child.report_fd().is_some();
            child.check_exec()?;
            if let Some(by) = exec_by
                && child.report_fd().is_some()
            {
                exec_not_held_up(child, by)?;
            }
            if resolving && child.report_fd().is_none() {
                if let Some(running) = self.running.take() {
                    running(u32::try_from(child.pid().as_raw()).unwrap_or(0));
                }
                if let Some(hooks) = hooks {
                    // The command runs: failures are only warnings now.
                    run_warning(hooks, Phase::Poststart, Some(child.pid().as_raw()));
                }
            }
            if let Some(status) = reap_children(child, &mut self.inherited)? {
                return Ok(Ended {
                    status,
                    killed_by_us: timers.killed_at.is_some(),
                    stop_requested: timers.requested,
                });
            }
            // After reaping: a run whose command has ended (or whose
            // container died before its terminal was relayed) never stops
            // for the terminal.
            if let Some(relay) = relay.as_deref_mut() {
                self.stop_for_the_terminal(relay, timers.requested);
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

    /// The signals the command keeps ignoring: those shipyard was told to
    /// ignore (`nohup`, a script's `&`) that matter to it.
    fn keep_ignored(&self) -> Vec<c_int> {
        self.signals
            .ignored_at_startup()
            .iter()
            .copied()
            .filter(|signal| KEEP_IGNORED.contains(signal))
            .collect()
    }

    /// [`with_term`](Self::with_term), unless the spec's environment is to
    /// be kept exactly (`exec`).
    fn with_term_unless<'a>(
        &self,
        spec: &'a ProcessSpec,
        shipyard_has_one: bool,
        exact: bool,
    ) -> Cow<'a, ProcessSpec> {
        if exact {
            Cow::Borrowed(spec)
        } else {
            self.with_term(spec, shipyard_has_one)
        }
    }

    /// The command's `TERM`, where it has a terminal: shipyard's own if it
    /// writes to that directly (`shipyard_has_one`), or its pseudo-terminal
    /// (the host's `TERM`, else `xterm`). A command whose output is relayed
    /// has none, so no `TERM` invites it to send a terminal's escapes into a
    /// pipe.
    fn with_term<'a>(&self, spec: &'a ProcessSpec, shipyard_has_one: bool) -> Cow<'a, ProcessSpec> {
        let pty = self.stdio.terminal.is_some();
        let has_terminal = pty || (shipyard_has_one && self.stdio.stdout == super::Stream::Inherit);
        let host_term = std::env::var_os("TERM").or_else(|| pty.then(|| "xterm".into()));
        with_terminal_env(spec, has_terminal, host_term)
    }

    /// Passes on the command's last output once it has ended (see
    /// `stdio.rs`): the user's input is no longer read, everything read is
    /// written out, and the pipes are waited on for end-of-file for up to
    /// [`DRAIN_WAIT`]. Signals still count: a stop request (SIGTERM,
    /// SIGINT, SIGHUP, SIGQUIT) throws away what the terminal hasn't taken,
    /// so a stalled terminal (Ctrl-S, a frozen connection) can't keep
    /// shipyard forever; job control stops shipyard as in the loop. If the
    /// run already ended `stopping` (on a stop request, or an error), what's
    /// left once the wait is over is thrown away too.
    fn drain(&mut self, relay: &mut Relay, stopping: bool) {
        relay.end_input();
        let deadline = Instant::now() + DRAIN_WAIT;
        loop {
            relay.pump();
            if Instant::now() >= deadline {
                if stopping {
                    relay.abandon();
                }
                relay.stop_waiting();
            }
            if relay.outputs_done() {
                return;
            }
            let told_to_stop = self.signals.drain().into_iter().any(|arrival| {
                matches!(
                    arrival.signal,
                    libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGQUIT
                )
            });
            if told_to_stop {
                tracing::debug!("told to stop while passing on the last output; dropping the rest");
                relay.abandon();
                return;
            }
            if relay.stop_needed().is_some() {
                self.stop_for_the_terminal(relay, false);
                continue;
            }
            let mut fds = vec![PollFd::new(self.signals.wake_fd(), PollFlags::POLLIN)];
            fds.extend(relay.poll_fds());
            let left = deadline.saturating_duration_since(Instant::now());
            let timeout = if left.is_zero() {
                PollTimeout::NONE
            } else {
                poll_timeout(left.min(Duration::from_millis(100)))
            };
            match poll(&mut fds, timeout) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(errno) => {
                    tracing::debug!(%errno, "waiting on the command's last output");
                    return;
                }
            }
        }
    }

    /// Stops shipyard for job control when the relay needs the terminal
    /// from the background (to read it, or to write to it with `tostop`),
    /// as the kernel would stop a program doing that itself; `fg` resumes
    /// it. Never during a stop request (`stopping`), nor where nothing could
    /// resume it: the relay then gives up on the terminal instead (input
    /// ends, output is discarded).
    fn stop_for_the_terminal(&self, relay: &mut Relay, stopping: bool) {
        let Some(signal) = relay.stop_needed() else {
            return;
        };
        // `exec`'s command, not yet started: it holds the container's lock
        // until then, and a stopped shipyard would hold it for as long as
        // the job sits in the background. Its terminal is relayed without
        // raw mode or input instead (`exec -t … &`).
        let exec_pending = self.running.is_some();
        if stopping || exec_pending || !under_job_control() {
            relay.give_up_held_back();
            if !stopping {
                // At once: a terminal given up on is relayed now, and its
                // command waits for that. (Not during a stop request: the
                // command's input would end before the stop signal reaches
                // it, and it would act on that end-of-file.)
                relay.pump();
            }
            return;
        }
        tracing::debug!(signal, "waiting for the terminal: stopping until resumed");
        // Stops shipyard right here, until its shell sends SIGCONT (`fg`);
        // the user's terminal is put back meanwhile if it's raw.
        relay.stop_self(signal);
        // That SIGCONT is handled here: left pending, it would continue
        // the command, which never stopped.
        self.signals.forget(libc::SIGCONT);
    }

    /// Sleeps until a signal arrives, the report pipe has something, the
    /// relay has something to move (see `stdio.rs`), or `wake_at` passes.
    fn wait_for_event(
        &self,
        child: &Child,
        wake_at: Option<Instant>,
        relay: Option<&Relay>,
    ) -> Result<()> {
        // The report pipe only until `exec` resolves; the relay's
        // descriptors while it has something to move.
        let mut fds = vec![PollFd::new(self.signals.wake_fd(), PollFlags::POLLIN)];
        fds.extend(
            child
                .report_fd()
                .map(|report| PollFd::new(report, PollFlags::POLLIN)),
        );
        if let Some(relay) = relay {
            fds.extend(relay.poll_fds());
        }
        let now = Instant::now();
        let until = wake_at.map_or(SAFETY_NET, |at| {
            at.saturating_duration_since(now).min(SAFETY_NET)
        });
        let timeout = poll_timeout(until);
        match poll(&mut fds, timeout) {
            // EINTR just means a signal arrived: the loop handles it next.
            Ok(_) | Err(Errno::EINTR) => Ok(()),
            Err(e) => Err(Error::sys("waiting for signals and the command", e)),
        }
    }
}

/// How long `exec`'s command may take to run (exec) once started: it takes
/// milliseconds, but until then `exec` holds the container's lock.
const EXEC_WAIT: Duration = Duration::from_secs(10);

/// Fails `exec`'s command if something holds it up before it runs: stopped
/// (the container may signal it: it's in its PID namespace), or not run by
/// `by` (its cgroup frozen from inside, say). It would hold the container's
/// lock meanwhile.
fn exec_not_held_up(child: &Child, by: Instant) -> Result<()> {
    if child.stopped().is_some() {
        return Err(Error::ExecHeldUp("it was stopped before it ran"));
    }
    if Instant::now() >= by {
        return Err(Error::ExecHeldUp("it didn't run in time (frozen?)"));
    }
    Ok(())
}

/// What a run does beyond the usual (see [`Supervisor::supervise_exec`]).
#[derive(Debug, Default, Clone, Copy)]
struct RunExtras {
    /// The command's environment exactly as its spec has it: no `TERM`
    /// added.
    exact_env: bool,
    /// `exec`'s terminal goes to this OCI console socket (open, and the
    /// terminal's size), not to a relay.
    console: Option<(std::os::fd::RawFd, super::terminal::Size)>,
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
    // Without root, the kernel decides: joining a container whose user
    // namespace is this user's works (exec, rootless), anything else is
    // refused when its namespaces are opened.
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
    /// It ended after a stop request (SIGTERM, then the stop signal).
    stop_requested: bool,
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

/// With a terminal the command writes to directly, or a pseudo-terminal of
/// its own (`-t`), pass the host's `TERM` through, or `xterm` for a
/// pseudo-terminal when the host has none (unless the user set one), so
/// full-screen programs like `top` or `vim` know how to draw. Not when its
/// output is only relayed: it has no terminal then.
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

/// Refuses standard descriptors the placement can't have: a terminal of the
/// command's own needs a mount namespace of its own, whose devpts it's made
/// in (a joined command's, `exec -t`, too).
fn check_stdio(placement: &Placement, stdio: super::Stdio) -> Result<()> {
    match placement {
        Placement::Join(join) if stdio.terminal.is_some() && !join.has(Namespace::Mount) => {
            Err(Error::TerminalNeedsMount)
        }
        // The container's devpts would be mounted over the host's.
        #[cfg(target_os = "linux")]
        Placement::New(isolation)
            if stdio.terminal.is_some() && !isolation.has(Namespace::Mount) =>
        {
            Err(Error::TerminalNeedsMount)
        }
        _ => Ok(()),
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

/// An isolated run's hooks (OCI bundles'), to run around it.
fn container_hooks(placement: &Placement) -> Option<ContainerHooks> {
    #[cfg(target_os = "linux")]
    return placement
        .isolation()
        .and_then(|isolation| isolation.hooks.clone());
    #[cfg(not(target_os = "linux"))]
    {
        let _ = placement;
        None
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

    /// F29: a supervisor dropped puts the subreaper setting back as it
    /// found it (off stays off; on stays on). The setting is the
    /// process's: changed only while this test holds the signals' one
    /// claim (a [`Signals`] of its own, which no supervisor or other test
    /// can install meanwhile), and tried by `install`'s own steps.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dropped_supervisor_puts_the_subreaper_back() {
        for before in [false, true] {
            let claim = claimed();
            nix::sys::prctl::set_child_subreaper(before).unwrap();
            let supervisor = Supervisor::installed_with(claim).unwrap();
            assert!(nix::sys::prctl::get_child_subreaper().unwrap());
            drop(supervisor);
            assert_eq!(nix::sys::prctl::get_child_subreaper().unwrap(), before);
            nix::sys::prctl::set_child_subreaper(false).unwrap();
        }
    }

    /// The signals' one claim, waited for while another test has it.
    #[cfg(target_os = "linux")]
    fn claimed() -> Signals {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match Signals::install() {
                Ok(signals) => return signals,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("no signals: {error}"),
            }
        }
    }

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
    fn a_joined_command_may_have_its_descriptors_relayed() {
        let join = Placement::Join(Join::standard(1));
        assert!(check_stdio(&join, crate::process::Stdio::default()).is_ok());
        // In place of a terminal (`exec` without `-t`).
        let relayed = crate::process::Stdio {
            stdout: crate::process::Stream::Relay,
            ..crate::process::Stdio::default()
        };
        assert!(check_stdio(&join, relayed).is_ok());
        assert!(check_stdio(&Placement::Host, relayed).is_ok());
        // A terminal of its own (`exec -t`): made in its mount namespace.
        let terminal = crate::process::Stdio {
            terminal: Some(crate::process::Pty::default()),
            ..crate::process::Stdio::default()
        };
        assert!(check_stdio(&join, terminal).is_ok());
        let mut without_mounts = Join::standard(1);
        without_mounts.namespaces = vec![crate::isolation::Namespace::Uts];
        assert!(matches!(
            check_stdio(&Placement::Join(without_mounts), terminal),
            Err(Error::TerminalNeedsMount)
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
