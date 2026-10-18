//! The macOS guest's own processes (step 10.3, D29): on a Mac, Shipyard
//! runs containers in one small Linux VM whose whole root is an initramfs
//! (D10.3-2), and this binary is the guest's init and its agent.
//!
//! - `__guest-init` is PID 1 (`init`): it moves the root into a tmpfs (the
//!   initramfs's own root can't be `pivot_root`ed away from, which every
//!   container's start does), mounts what the engine needs, loads the
//!   kernel's modules, prepares the data disk (`disk`) and the shared
//!   home, settles containers left from the last boot, then runs the agent
//!   and powers off when told to stop.
//! - `__shipyard-agent` (`agent`) answers the Mac over vsock: each
//!   connection a command to run (`shipyard ARGS`, as the Mac CLI was
//!   given), its output and exit code sent back; or a connection to a port
//!   a container publishes, forwarded from the Mac's (step 10.3c1).
//! - `__guest-ports` (`ports`) prints what the containers publish, for the
//!   VM's host to listen on the Mac: a process of its own, so a failure
//!   costs one answer, never the agent and its sessions.
//!
//! - `__guest-doctor` (`doctor`) prints the guest's own `doctor` checks,
//!   for a Mac's `doctor` to show.
//!
//! All are entry points ([`crate::entry_points`]): they take over a
//! process started for them, and return at once for any other.

#[cfg(target_os = "linux")]
mod agent;
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the guest is Linux; its rules are tested everywhere"
    )
)]
mod disk;
#[cfg(target_os = "linux")]
mod doctor;
#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod ports;

/// The argument that makes this binary the guest's init.
#[cfg(target_os = "linux")]
const INIT_ARG: &str = "__guest-init";

/// The argument that makes this binary the guest's agent.
#[cfg(target_os = "linux")]
const AGENT_ARG: &str = "__shipyard-agent";

/// The guest's init, if this process was started as it (PID 1 in the macOS
/// guest): never returns then.
pub fn init_entry_point() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(INIT_ARG.as_ref()) {
        init::run();
    }
}

/// The guest's agent, if this process was started as it: never returns
/// then.
pub fn agent_entry_point() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(AGENT_ARG.as_ref()) {
        agent::run();
    }
}

/// The guest's published ports, printed, if this process was started for
/// them ([`shipyard_runtime::attach::forward::PORTS_REQUEST`]): never
/// returns then.
pub fn ports_entry_point() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref()
        == Some(shipyard_runtime::attach::forward::PORTS_REQUEST.as_ref())
    {
        std::process::exit(ports::print());
    }
}

/// The guest's own checks, printed, if this process was started for them
/// ([`shipyard_runtime::attach::forward::DOCTOR_REQUEST`]): never returns
/// then.
pub fn doctor_entry_point() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref()
        == Some(shipyard_runtime::attach::forward::DOCTOR_REQUEST.as_ref())
    {
        std::process::exit(doctor::print());
    }
}
