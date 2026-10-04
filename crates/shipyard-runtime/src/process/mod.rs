//! Process lifecycle: start a program the way a container runtime must,
//! supervise it, and report how it ended.
//!
//! This is POSIX-only code (TLPI ch. 6, 20–28), so it runs on macOS and Linux.
//! Later stages wrap the same pipeline in namespaces (stage 2) and reuse the
//! supervisor in the per-container monitor (stage 8).

mod channel;
mod child;
mod env;
mod failure;
mod plan;
mod rlimit;
mod signal;
mod spawn;
mod spec;
mod startup;
mod status;
mod stop;

pub use env::{DEFAULT_PATH, Env, EnvError};
pub use failure::{SpawnError, SpawnErrorKind};
pub use rlimit::{Rlimit, RlimitError, RlimitKind, RlimitValue};
pub use signal::{Signal, SignalError};
pub use spawn::{Child, die_from, run_foreground, spawn};
pub use spec::{MAX_GROUPS, ProcessSpec, SpecError, User};
pub use startup::refuse_privileged_start;
pub use status::{EXIT_CANNOT_EXECUTE, EXIT_ENGINE_ERROR, EXIT_NOT_FOUND, ExitStatus};
pub use stop::{DEFAULT_STOP_TIMEOUT_SECS, StopPolicy};
