//! Every helper's entry point, in the one order a binary must call them
//! ([`entry_points`]): each of Shipyard's helpers is the running binary,
//! re-executed, and takes over first thing in `main`, before anything starts
//! a thread or opens a descriptor.

use crate::{cp, doctor, guest, image, network, run};

/// What a binary gives [`entry_points`]: by name, so two hooks of one shape
/// can't be swapped.
#[derive(Debug)]
pub struct Hooks<L, R> {
    /// The storage helper's logging (its process has no `main` of its own).
    pub init_logging: L,
    /// Pasta's launcher's guard: why it may not start, if it may not.
    pub refuse_privileged_start: R,
}

/// Runs every helper's entry point, in order: call it first thing in
/// `main`. A helper takes over and never returns; for any other start, this
/// returns at once. The order matters:
///
/// 1. a container's init (it re-execs this binary to become a clean
///    process);
/// 2. `__cleanup`, a detached container's exit command, which takes the
///    descriptors it inherits before anything here opens one; then
///    `__health`, the monitor and `cp`'s helper in a container;
/// 3. the storage helper (a rootless caller's layer work, in a user
///    namespace of its own: making one needs a single-threaded process),
///    with [`Hooks::init_logging`] for its own log;
/// 4. a network's DNS server;
/// 5. `doctor`'s probe ([`doctor::probe_entry_point`]: it makes a user
///    namespace, single-threaded too);
/// 6. pasta's launcher (a rootless run's network: it joins the container's
///    user namespace), refused as [`Hooks::refuse_privileged_start`] says.
///    Its pasta's death while the container runs is said by
///    [`network::pasta_lost`];
/// 7. the macOS guest's init, its agent, its ports' answer and its checks ([`guest`]:
///    only ever run in Shipyard's own VM).
pub fn entry_points<L, R>(hooks: Hooks<L, R>)
where
    L: FnOnce(),
    R: FnOnce() -> std::result::Result<(), String>,
{
    let Hooks {
        init_logging,
        refuse_privileged_start,
    } = hooks;
    shipyard_runtime::process::init_entry_point();
    run::cleanup_entry_point();
    run::health_entry_point();
    shipyard_runtime::container::monitor_entry_point();
    cp::helper_entry_point();
    image::storage_helper_entry_point(init_logging);
    network::dns_entry_point();
    doctor::probe_entry_point();
    shipyard_network::pasta::entry_point(refuse_privileged_start, network::pasta_lost);
    guest::init_entry_point();
    guest::agent_entry_point();
    guest::ports_entry_point();
    guest::doctor_entry_point();
}
