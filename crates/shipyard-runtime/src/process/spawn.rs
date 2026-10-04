//! Starting a process (`fork` + `exec`, TLPI ch. 24–28) and waiting for it.
//!
//! Spawning is split into steps so the supervisor (stage 1, step 4) can put
//! its own event loop between them:
//!
//! 1. [`spawn`] forks and returns at once.
//! 2. [`Child::await_exec`] learns whether `exec` succeeded.
//! 3. [`Child::wait`] waits for the command to end.
//!
//! [`run_foreground`] does all three, `system()`-style.
//!
//! Spawn only from the main thread of a single-threaded process. Linux's
//! parent-death signal fires when the *thread* that forked exits, and on macOS
//! a fork in another thread could inherit the report pipe.

use std::os::fd::{AsRawFd, OwnedFd};

use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, sigprocmask};
use nix::sys::signal::{Signal as NixSignal, sigaction};
use nix::unistd::{ForkResult, Pid, fork};

use super::channel::{read_report, report_pipe};
use super::child::child_main;
use super::failure::{FailureContext, describe};
use super::plan::ChildPlan;
use super::signal::Signal;
use super::spec::ProcessSpec;
use super::status::ExitStatus;
use crate::{Error, Result};

/// A started process that hasn't been waited for yet.
///
/// Dropping it without calling [`Child::wait`] leaves a zombie process.
#[derive(Debug)]
#[must_use = "a child that is never waited for stays behind as a zombie"]
pub struct Child {
    pid: Pid,
    /// The read end of the report pipe, until [`Child::await_exec`] uses it.
    report: Option<OwnedFd>,
    context: FailureContext,
}

/// Starts the process `spec` describes and returns without waiting.
///
/// The spec is validated first. Everything the child needs is prepared here,
/// in the parent, because after `fork` the child can't safely allocate.
pub fn spawn(spec: &ProcessSpec) -> Result<Child> {
    let (plan, context) = ChildPlan::new(spec)?;
    let (report_read, report_write) = report_pipe()?;

    // Block every signal across `fork`: a handler installed by the caller
    // must never run in the child before the child resets it.
    let mask = BlockAllSignals::new()?;
    // SAFETY: the child branch only runs `child_main`, which is written to be
    // safe after `fork` (no allocation, locks or panics; see child.rs).
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // SAFETY: this is the child, right after `fork`, with all signals
            // blocked; `plan` and the pipe were prepared above.
            unsafe { child_main(&plan, report_write.as_raw_fd()) }
        }
        Ok(ForkResult::Parent { child }) => {
            drop(mask);
            drop(report_write);
            tracing::debug!(pid = child.as_raw(), program = %context.program.display(), "started");
            Ok(Child {
                pid: child,
                report: Some(report_read),
                context,
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
    /// The process ID.
    #[must_use]
    pub fn id(&self) -> i32 {
        self.pid.as_raw()
    }

    /// Waits until the command is running, or fails with the reason it never
    /// ran. A child that failed is reaped here, so there's nothing left to
    /// wait for.
    pub fn await_exec(&mut self) -> Result<()> {
        let Some(report) = self.report.take() else {
            return Ok(());
        };
        match read_report(&report) {
            Ok(None) => Ok(()),
            Ok(Some(failure)) => {
                // The child exits right after reporting; reap it so it doesn't
                // linger as a zombie.
                let _ = wait_pid(self.pid);
                Err(describe(failure, &self.context).into())
            }
            Err(err) => {
                // We can't tell whether the command started, so it must not
                // keep running unsupervised. It's unreaped, so the PID is
                // still ours to signal.
                let _ = nix::sys::signal::kill(self.pid, NixSignal::SIGKILL);
                let _ = wait_pid(self.pid);
                Err(err)
            }
        }
    }

    /// Waits for the command to end.
    pub fn wait(self) -> Result<ExitStatus> {
        let status = wait_pid(self.pid)?;
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

/// Runs the command in the foreground and waits for it, like POSIX `system()`
/// (TLPI §27.7):
///
/// - Ctrl-C and Ctrl-\ go to the whole foreground process group, which the
///   command shares with shipyard. The command gets them directly, so
///   shipyard ignores SIGINT and SIGQUIT while waiting, and reports how the
///   command handled them.
/// - SIGCHLD is set to its default while waiting. An inherited "ignore" would
///   make the kernel discard the command's exit status.
///
/// Signals sent to shipyard's own PID (rather than the process group) aren't
/// forwarded yet; the supervisor in the next step will.
pub fn run_foreground(spec: &ProcessSpec) -> Result<ExitStatus> {
    let mut dispositions = Dispositions::default();
    dispositions.set(NixSignal::SIGINT, SigHandler::SigIgn)?;
    dispositions.set(NixSignal::SIGQUIT, SigHandler::SigIgn)?;
    dispositions.set(NixSignal::SIGCHLD, SigHandler::SigDfl)?;

    let mut child = spawn(spec)?;
    child.await_exec()?;
    child.wait()
}

/// Dies from `signal` the default way, as if it had been delivered to us.
///
/// When the command was killed by Ctrl-C, a shell running shipyard in a loop
/// only stops the loop if shipyard *also* appears to have died from SIGINT
/// (an exit code of 130 isn't enough). Returns only if the signal didn't kill
/// the process; callers then exit with `128 + signal`.
pub fn die_from(signal: Signal) {
    let Ok(signal) = NixSignal::try_from(signal.as_raw()) else {
        return;
    };
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    // SAFETY: installing SIG_DFL runs none of our code in a signal context.
    let _ = unsafe { sigaction(signal, &default) };
    let mut only = SigSet::empty();
    only.add(signal);
    let _ = sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&only), None);
    let _ = nix::sys::signal::raise(signal);
}

/// Saves signal dispositions as they're changed, and restores them on drop.
#[derive(Default)]
struct Dispositions {
    saved: Vec<(NixSignal, SigAction)>,
}

impl Dispositions {
    fn set(&mut self, signal: NixSignal, handler: SigHandler) -> Result<()> {
        // Flags 0 also clears SA_NOCLDWAIT, which would auto-reap children.
        let action = SigAction::new(handler, SaFlags::empty(), SigSet::empty());
        // SAFETY: only SIG_IGN and SIG_DFL are installed here, so no code of
        // ours ever runs as a signal handler.
        let old = unsafe { sigaction(signal, &action) }
            .map_err(|e| Error::sys(format!("changing how {signal} is handled"), e))?;
        self.saved.push((signal, old));
        Ok(())
    }
}

impl Drop for Dispositions {
    fn drop(&mut self) {
        for (signal, old) in self.saved.drain(..).rev() {
            // SAFETY: restores what `sigaction` returned earlier for this signal.
            let _ = unsafe { sigaction(signal, &old) };
        }
    }
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
