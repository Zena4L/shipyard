//! Process lifecycle: start a program the way a container runtime must,
//! supervise it, and report how it ended.
//!
//! This is POSIX-only code (TLPI ch. 6, 20–28), so it runs on macOS and Linux.
//! Later stages wrap the same pipeline in namespaces (stage 2) and reuse the
//! supervisor in the per-container monitor (stage 8).

mod startup;
mod status;

pub use startup::{ensure_std_fds_open, refuse_setid_execution};
pub use status::{EXIT_CANNOT_EXECUTE, EXIT_ENGINE_ERROR, EXIT_NOT_FOUND, ExitStatus};
