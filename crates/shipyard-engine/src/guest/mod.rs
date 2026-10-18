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

/// The file PID 1 makes once the guest is ready for commands (its data
/// disk checked, repaired if need be, and mounted; the home shared): until
/// then, the agent, started first, refuses every request at once (D10.4-3:
/// a long repair never runs against a bound, nor is cut short by one).
#[cfg(target_os = "linux")]
const READY: &str = "/run/shipyard-guest-ready";

/// What a request is told while the guest isn't ready.
#[cfg(target_os = "linux")]
const NOT_READY: &str = "the VM is still starting: it's checking its data disk, or repairing it \
                         after an unclean stop (its console log says how far); try again shortly";

/// The argument that makes this binary the guest's agent.
#[cfg(target_os = "linux")]
const AGENT_ARG: &str = "__shipyard-agent";

/// The guest's init, if this process was started as it (PID 1 in the macOS
/// guest): never returns then. Anywhere else (a Linux host, a container),
/// refused: it would move the host's root into memory and remove the rest.
pub fn init_entry_point() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(INIT_ARG.as_ref()) {
        refuse_outside_the_guest(INIT_ARG, true);
        init::run();
    }
}

/// The guest's agent, if this process was started as it: never returns
/// then. Refused outside the guest, as its init is.
pub fn agent_entry_point() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(AGENT_ARG.as_ref()) {
        refuse_outside_the_guest(AGENT_ARG, false);
        agent::run();
    }
}

/// Ends this process (exit code 125, the engine's error) unless it's in Shipyard's own VM
/// ([`in_the_guest`]), as its PID 1 if `as_init`.
#[cfg(target_os = "linux")]
fn refuse_outside_the_guest(what: &str, as_init: bool) {
    let pid = std::process::id();
    // The guest's PID 1 starts before anything is mounted: no `/proc` yet
    // (it mounts its own later). Only PID 1 looks there for itself; anyone
    // else without `/proc` is refused.
    let cmdline = std::fs::read_to_string("/proc/cmdline")
        .ok()
        .or_else(|| {
            (as_init && pid == 1)
                .then(kernel_cmdline_unmounted)
                .flatten()
        })
        .unwrap_or_default();
    if !in_the_guest(pid, &cmdline, as_init) {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "shipyard: {what} runs only in Shipyard's own macOS VM: refused"
        );
        std::process::exit(i32::from(shipyard_runtime::process::EXIT_ENGINE_ERROR));
    }
}

/// The kernel's command line read by PID 1 before `/proc` is mounted: a
/// `proc` mounted at `/proc` for the read, and unmounted after (the init
/// mounts its own where it wants it).
#[cfg(target_os = "linux")]
fn kernel_cmdline_unmounted() -> Option<String> {
    use nix::mount::{MsFlags, mount, umount};
    std::fs::create_dir_all("/proc").ok()?;
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .ok()?;
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok();
    let _ = umount("/proc");
    cmdline
}

/// Whether a process `pid` under a kernel booted with `cmdline` is in
/// Shipyard's own VM: its kernel was given the shared home
/// ([`HOME_KEY`](shipyard_runtime::attach::forward::HOME_KEY), absolute),
/// which no other kernel is; and it's PID 1 if `as_init`.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the guest is Linux; its rule is tested everywhere"
    )
)]
fn in_the_guest(pid: u32, cmdline: &str, as_init: bool) -> bool {
    let booted_by_shipyard = cmdline.split_ascii_whitespace().any(|word| {
        word.strip_prefix(shipyard_runtime::attach::forward::HOME_KEY)
            .is_some_and(|home| home.starts_with('/'))
    });
    booted_by_shipyard && (!as_init || pid == 1)
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

#[cfg(test)]
mod tests {
    use super::in_the_guest;

    /// Only a kernel Shipyard booted (the shared home on its command line)
    /// is the guest's; its init is PID 1 too.
    #[test]
    fn only_shipyards_vm_is_the_guest() {
        let guest = "console=hvc0 shipyard.home=/Users/me";
        assert!(in_the_guest(1, guest, true));
        assert!(in_the_guest(40, guest, false), "the agent: any PID");
        assert!(!in_the_guest(40, guest, true), "an init that isn't PID 1");
        for host in [
            "BOOT_IMAGE=/vmlinuz root=/dev/sda1 ro quiet",
            "",
            "shipyard.home=Users/me",
            "not.shipyard.home=/Users/me",
        ] {
            assert!(!in_the_guest(1, host, true), "{host:?}");
            assert!(!in_the_guest(1, host, false), "{host:?}");
        }
    }
}
