//! Checks that run first thing in `main`.
//!
//! Closed standard descriptors need no check here: Rust's own startup code
//! reopens any closed fd 0, 1 or 2 on `/dev/null` before `main` runs (or aborts),
//! so a later `open` can never land on them (TLPI §4.3, §5.5).

use nix::unistd::{getegid, geteuid, getgid, getuid};

use crate::{Error, Result};

/// Refuses to run if shipyard was started with privileges its caller doesn't
/// have: a setuid or setgid bit, Linux file capabilities, or a security-module
/// transition.
///
/// Shipyard is never meant to be installed that way. If it were, any user could
/// run commands with elevated privileges, so we stop immediately.
pub fn refuse_privileged_start() -> Result<()> {
    match privileged_start() {
        Some(reason) => Err(Error::PrivilegedStart { reason }),
        None => Ok(()),
    }
}

/// Why this start is privileged, if it is.
fn privileged_start() -> Option<String> {
    let (real_uid, effective_uid) = (getuid(), geteuid());
    if real_uid != effective_uid {
        return Some(format!(
            "setuid bit: real uid {real_uid}, effective uid {effective_uid}"
        ));
    }
    let (real_gid, effective_gid) = (getgid(), getegid());
    if real_gid != effective_gid {
        return Some(format!(
            "setgid bit: real gid {real_gid}, effective gid {effective_gid}"
        ));
    }
    // Catches what the ID comparison can't, such as file capabilities, where
    // the real and effective IDs stay equal.
    kernel_flags_secure_exec().then(|| {
        "the kernel flagged this start as privileged (for example file capabilities)".to_owned()
    })
}

/// The kernel's own verdict: `AT_SECURE` is set for setuid, setgid and
/// file-capability execs alike.
#[cfg(target_os = "linux")]
fn kernel_flags_secure_exec() -> bool {
    // SAFETY: `getauxval` only reads the auxiliary vector the kernel passed at
    // exec; it has no preconditions and returns 0 for unknown entries.
    unsafe { libc::getauxval(libc::AT_SECURE) != 0 }
}

/// `issetugid` reports whether the process gained privileges at exec.
#[cfg(not(target_os = "linux"))]
fn kernel_flags_secure_exec() -> bool {
    // SAFETY: `issetugid` takes no arguments and only reads process state.
    unsafe { libc::issetugid() != 0 }
}
