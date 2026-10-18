//! `__guest-doctor` (stage 10's close): the guest's own checks, the
//! engine's as on a Linux host (its probe helper included: this same
//! binary), as a [`GuestDoctor`] on stdout, for the Mac's `doctor` to show
//! beside the VM's.
//!
//! [`GuestDoctor`]: crate::doctor::GuestDoctor

use std::io::Write as _;
use std::sync::Arc;

/// Prints the checks; its exit code (1: they couldn't be written, said on
/// stderr). A failed check is in what's printed, not in the exit code.
pub(super) fn print() -> i32 {
    let engine = crate::Engine::new(crate::Paths::from_env(), Arc::new(Quiet));
    let report = crate::doctor::checks(&engine);
    let written = crate::doctor::GuestDoctor::of(&report)
        .encode()
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)
                .and_then(|()| out.flush())
                .map_err(|e| e.to_string())
        });
    match written {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "shipyard: the guest's checks: {why}");
            1
        }
    }
}

/// A reporter that says nothing: what the checks find is in them.
struct Quiet;

impl crate::report::Reporter for Quiet {
    fn report(&self, _message: crate::report::Message) {}
}
