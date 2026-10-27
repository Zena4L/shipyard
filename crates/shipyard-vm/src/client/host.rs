//! The VM's host process started for the CLI: spawned as a process of its own,
//! or kicked through launchd once `vm install` has loaded its agent (the host
//! keeps its own log short).

use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{HOST_ARG, VmError, VmPaths};

use super::{Status, status};

/// The most the lock file's PID is read as.
pub(super) const PID_CAP: u64 = 64;

/// The host as the CLI started it: a process of its own, or launchd's
/// job (once `shipyard vm install` has loaded it: then launchd holds
/// Docker's socket, and the one host must be its).
pub(super) enum Started {
    /// Spawned here.
    Spawned(std::process::Child),
    /// launchd's, kicked into starting.
    Launchd {
        /// How many times launchd had started it as it was kicked: a run
        /// past these is this start's (or a later one's).
        runs: Option<u64>,
        /// When it was last kicked.
        kicked: Instant,
        /// Whether this start wrote the start mark (its host takes it).
        marked: bool,
    },
}

/// How often a launchd start is kicked again while no host holds the lock
/// (launchd itself starts a job at most every 10 s).
const KICK_AGAIN: Duration = Duration::from_secs(2);

impl Started {
    /// How it ended, if it has: a spawned one's status; launchd's, a run
    /// since its kick that ended with an error (launchd's word on it).
    pub(super) fn ended(&mut self, paths: &VmPaths) -> Option<std::process::ExitStatus> {
        match self {
            Self::Spawned(host) => host.try_wait().ok().flatten(),
            Self::Launchd { runs, marked, .. } => {
                let job = crate::launchd::job(paths)?;
                // (A mark this start never wrote is no run of its.)
                let mark_taken = *marked && !paths.start_asked().exists();
                ended_in_launchd(*runs, job, mark_taken)
            }
        }
    }

    /// Whether to start it again (at `now`) once no host holds the lock: a
    /// spawned one that ended; launchd's every [`KICK_AGAIN`] (kicking it
    /// while it runs does nothing).
    pub(super) fn may_start_again(
        &self,
        ended: Option<std::process::ExitStatus>,
        now: Instant,
    ) -> bool {
        match self {
            Self::Spawned(_) => ended.is_some(),
            Self::Launchd { kicked, .. } => now.duration_since(*kicked) >= KICK_AGAIN,
        }
    }
}

/// A launchd start's end, from its job (`runs` when it was kicked): a run
/// since then, not running now, whose exit code launchd kept. With `runs`
/// unknown (`launchctl print` failed at the kick, F144), a run since is
/// one that took this start's mark (`mark_taken`: only a host removes it).
/// (One that ended well, a run at load that found nothing asked, isn't an
/// end: the start goes on.)
fn ended_in_launchd(
    runs: Option<u64>,
    job: crate::launchd::Job,
    mark_taken: bool,
) -> Option<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt as _;
    let since = runs.map_or(mark_taken, |runs| job.runs > runs);
    let code = job.last_exit.filter(|&code| code != 0)?;
    (since && !job.running)
        .then(|| std::process::ExitStatus::from_raw(i32::try_from(code & 0xff).unwrap_or(1) << 8))
}

/// Starts the host: through launchd if its agent for this VM is loaded
/// (`shipyard vm install`), else [`spawn_host`].
pub(super) fn start_host(paths: &VmPaths) -> Result<Started, VmError> {
    if crate::launchd::loaded(paths) {
        let runs = crate::launchd::job(paths).map(|job| job.runs);
        // A host runs already (booting): no mark, no kick, only the wait
        // (a mark it would never take would boot the VM at the next login).
        let marked = status(paths)? == Status::Stopped;
        if marked {
            // Its host boots only when asked (else it takes itself for
            // launchd's run at load): the mark says the CLI asked.
            let mark = paths.start_asked();
            std::fs::write(&mark, b"").map_err(|e| VmError::io(&mark, e))?;
            crate::launchd::kickstart(paths)?;
        }
        return Ok(Started::Launchd {
            runs,
            kicked: Instant::now(),
            marked,
        });
    }
    spawn_host(paths).map(Started::Spawned)
}

/// Starts the host here: this binary again, in a session of its own (a
/// closed terminal doesn't end it), in `/` (it holds no directory of the
/// caller's), with nothing the CLI inherited, its output to its log.
fn spawn_host(paths: &VmPaths) -> Result<std::process::Child, VmError> {
    let exe = std::env::current_exe().map_err(|e| VmError::Host(format!("this binary: {e}")))?;
    // (The host keeps its own log short: F106.)
    let log_path = paths.host_log();
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| VmError::io(&log_path, e))?;
    let err = log.try_clone().map_err(|e| VmError::io(&log_path, e))?;
    let bound = descriptor_bound();
    let mut command = Command::new(exe);
    command
        .arg(HOST_ARG)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err);
    // SAFETY: between fork and exec, `setsid` and `fcntl` only: both
    // async-signal-safe, no allocation, no lock. The CLI is single-threaded
    // here, and has used no Objective-C: the host process is a fresh exec.
    unsafe {
        command.pre_exec(move || {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            // Whatever the CLI inherited open (a caller's pipe) closes at
            // the exec: the host outlives the CLI, and would hold it.
            for fd in 3..bound {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|e| VmError::Host(format!("starting the VM's host: {e}")))
}

/// The numbers a descriptor may have here: the soft limit, bounded.
fn descriptor_bound() -> i32 {
    nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .map_or(1024, |(soft, _)| {
            i32::try_from(soft.min(65_536)).unwrap_or(1024)
        })
}

/// The last `count` lines of `path` (or why there are none).
pub(crate) fn last_lines(path: &std::path::Path, count: usize) -> String {
    std::fs::read_to_string(path).map_or_else(
        |error| format!("no log ({error})"),
        |text| {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(count)..].join(" / ")
        },
    )
}

#[cfg(test)]
mod tests;
