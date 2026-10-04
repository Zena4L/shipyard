//! Starting a process: `fork` + `exec` (TLPI ch. 24–28).
//!
//! [`spawn`] forks and returns at once; the supervisor then watches the
//! child's report pipe and its exit inside one event loop. Spawn only from the
//! main thread: Linux's parent-death signal fires when the *thread* that
//! forked exits, and on macOS a fork in another thread could inherit the
//! report pipe.

use std::os::fd::{AsRawFd, BorrowedFd};

use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, sigprocmask};
use nix::sys::signal::{Signal as NixSignal, sigaction};
use nix::unistd::{ForkResult, Pid, fork, getpgrp, setpgid};

use super::channel::{Report, ReportReader, report_pipe};
use super::child::child_main;
use super::failure::{FailureContext, describe};
use super::plan::ChildPlan;
use super::signal::Signal;
use super::spec::ProcessSpec;
use super::status::ExitStatus;
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
}

/// A started process.
#[derive(Debug)]
pub(crate) struct Child {
    pid: Pid,
    /// The command's own process group, if it has one.
    group: Option<Pid>,
    /// Until `exec` succeeds or fails.
    report: Option<ReportReader>,
    context: FailureContext,
    reaped: bool,
}

/// Starts the process `spec` describes and returns without waiting.
///
/// Everything that can fail happens before `fork`: once the child exists,
/// this function can't return an error and leave it unsupervised.
pub(crate) fn spawn(spec: &ProcessSpec, options: &SpawnOptions) -> Result<Child> {
    let (report_read, report_write) = report_pipe()?;
    let report = ReportReader::new(report_read)?;
    // Last, so its list of open descriptors includes the pipe above.
    let (plan, context) = ChildPlan::new(spec, options)?;

    // Block every signal across `fork`: the supervisor's handler must never
    // run in the child before the child resets it.
    let mask = BlockAllSignals::new()?;
    // SAFETY: the child branch only runs `child_main`, which is written to be
    // safe after `fork` (no allocation, locks or panics; see child.rs).
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // SAFETY: this is the child, right after `fork`, with all signals
            // blocked; `plan` and the pipe were prepared above.
            unsafe { child_main(&plan, report_write.as_raw_fd()) }
        }
        Ok(ForkResult::Parent { child: pid }) => {
            drop(mask);
            drop(report_write);
            let group = options.new_process_group.then(|| {
                // The child does this too; doing it here as well closes the
                // window before the child gets to it. Errors mean the child
                // already did it, or already exec'd or exited: all fine.
                let _ = setpgid(pid, pid);
                pid
            });
            tracing::debug!(pid = pid.as_raw(), program = %context.program.display(), "started");
            Ok(Child {
                pid,
                group,
                report: Some(report),
                context,
                reaped: false,
            })
        }
        Err(errno) => Err(Error::sys(
            if errno == Errno::EAGAIN {
                "starting the command: too many processes"
            } else {
                "starting the command (fork)"
            },
            errno,
        )),
    }
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
                Ok(())
            }
            Report::Failed(failure) => {
                self.report = None;
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

    /// Kills the command and, if it has its own group, everything in it.
    pub(crate) fn kill_all(&self) {
        if self.reaped {
            return;
        }
        // The command itself first: right after `fork` its group may not
        // exist yet, and then `killpg` alone would miss it.
        self.signal(libc::SIGKILL);
        if let Some(group) = self.group
            // Never our own group: that would kill shipyard itself.
            && group != getpgrp()
        {
            // SAFETY: `killpg` has no memory-safety preconditions. ESRCH
            // (an empty group) is harmless.
            unsafe { libc::killpg(group.as_raw(), libc::SIGKILL) };
        }
    }

    /// Reaps the command, which must have exited, and returns how it ended.
    pub(crate) fn reap(&mut self) -> Result<ExitStatus> {
        let status = wait_pid(self.pid).inspect_err(|_| {
            // ECHILD and the like: the PID is no longer ours to signal.
            self.reaped = true;
        })?;
        self.reaped = true;
        tracing::debug!(pid = self.pid.as_raw(), %status, "exited");
        Ok(status)
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
    use super::*;

    #[test]
    fn only_terminating_signals_are_re_raised() {
        assert!(terminates_by_default(NixSignal::SIGINT));
        assert!(terminates_by_default(NixSignal::SIGTERM));
        assert!(!terminates_by_default(NixSignal::SIGTSTP));
        assert!(!terminates_by_default(NixSignal::SIGSTOP));
        assert!(!terminates_by_default(NixSignal::SIGCHLD));
    }
}
