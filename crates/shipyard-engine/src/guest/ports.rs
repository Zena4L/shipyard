//! `__guest-ports` (step 10.3c1): what the guest's containers publish, as
//! a [`PortsAnswer`] on stdout, for the VM's host (through the agent, which
//! runs it as any command) to listen on the Mac.
//!
//! [`PortsAnswer`]: shipyard_runtime::attach::forward::PortsAnswer

use std::io::Write as _;
use std::sync::Arc;

/// Prints the answer; its exit code (1: it couldn't be had, said on
/// stderr).
pub(super) fn print() -> i32 {
    let engine = crate::Engine::new(crate::Paths::from_env(), Arc::new(Quiet));
    let answer = crate::containers::published(&engine).map_err(|e| e.to_string());
    let bytes = answer.and_then(|answer| answer.encode().map_err(|e| e.to_string()));
    let written = bytes.and_then(|bytes| {
        let mut out = std::io::stdout().lock();
        out.write_all(&bytes)
            .and_then(|()| out.flush())
            .map_err(|e| e.to_string())
    });
    match written {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "shipyard: the guest's ports: {why}");
            1
        }
    }
}

/// A reporter that says nothing: asked every 2 s, an unreadable
/// container's warning would fill the host's log.
struct Quiet;

impl crate::report::Reporter for Quiet {
    fn report(&self, _message: crate::report::Message) {}
}
