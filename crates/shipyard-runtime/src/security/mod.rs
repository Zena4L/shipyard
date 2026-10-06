//! A container's confinement (stage 4): its capabilities, `no_new_privs`,
//! and the system calls it may make (seccomp).

mod capability;
mod confinement;
pub mod seccomp;
mod syscalls;

pub use capability::{Capability, CapabilityError, CapabilitySet};
pub use confinement::{Capabilities, CapabilitySets, Security};
#[cfg(target_os = "linux")]
pub(crate) use confinement::{Confinement, ConfinementError, SeccompFilter};
pub use seccomp::Seccomp;
