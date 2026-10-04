//! Helpers shared by the end-to-end test files.

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

/// The longest any one shipyard run in a test may take.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);

/// The exit code for a death by `signal`.
pub(crate) fn killed_by(signal: libc::c_int) -> i32 {
    128 + signal
}

/// Blocks `signal` in the calling process. For `pre_exec` hooks: only
/// async-signal-safe calls.
pub(crate) fn block(signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `sigset_t` operations and `sigprocmask` are async-signal-safe,
    // and `set` is valid storage.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, signal);
        if libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Ignores `signal` in the calling process, as `nohup` does for SIGHUP. For
/// `pre_exec` hooks: only async-signal-safe calls.
pub(crate) fn ignore(signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `signal` is async-signal-safe.
    if unsafe { libc::signal(signal, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A `shipyard` command, started in a new session with no controlling
/// terminal, so it behaves the same whether the tests run from a terminal or
/// in CI.
pub(crate) fn shipyard(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shipyard"));
    command.args(args);
    // SAFETY: `setsid` is async-signal-safe, as `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}
