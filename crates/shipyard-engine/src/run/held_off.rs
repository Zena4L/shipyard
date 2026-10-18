//! Ctrl-C and its kin held off while a container is made ([`HeldOff`]).

use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};

/// SIGINT, SIGTERM, SIGHUP and SIGQUIT held off in this thread while it
/// lives, then as they were (one sent meanwhile is delivered then). A
/// mask is inherited across `fork` and `exec`: every process started
/// meanwhile (the container's monitor, a network's DNS server) empties
/// its own before its `exec`.
#[derive(Debug)]
pub(super) struct HeldOff {
    before: Option<SigSet>,
}

impl HeldOff {
    /// Whether one of them was sent meanwhile (it waits, pending:
    /// `sigpending`).
    pub(super) fn asked_to_stop(&self) -> bool {
        if self.before.is_none() {
            return false;
        }
        // SAFETY: an all-zero `sigset_t` is a valid one to fill.
        let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `pending` is valid for writing.
        if unsafe { libc::sigpending(&raw mut pending) } != 0 {
            return false;
        }
        [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
            .into_iter()
            // SAFETY: `pending` was filled by `sigpending`.
            .any(|signal| unsafe { libc::sigismember(&raw const pending, signal) } == 1)
    }

    pub(super) fn new() -> Self {
        let mut set = SigSet::empty();
        for signal in [
            Signal::SIGINT,
            Signal::SIGTERM,
            Signal::SIGHUP,
            Signal::SIGQUIT,
        ] {
            set.add(signal);
        }
        let mut before = SigSet::empty();
        let held = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), Some(&mut before));
        Self {
            before: held.ok().map(|()| before),
        }
    }
}

impl Drop for HeldOff {
    fn drop(&mut self) {
        if let Some(before) = &self.before {
            let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(before), None);
        }
    }
}
