//! A rootless run's network: pasta, started once its container exists.

use std::path::PathBuf;
use std::sync::Arc;

use super::HOST_INSTEAD;
use super::attach::announce;
use crate::report::{Event, Reporter};

/// What a pasta run starts once its container exists.
pub(super) struct PastaPlan {
    /// pasta itself.
    pub(super) program: PathBuf,
    /// What it can do.
    pub(super) features: shipyard_network::pasta::Features,
    /// The host ports it publishes, held (free ones chosen, and taken ones
    /// refused, before anything was made) until just before the container
    /// is started ([`RunNetwork::release_ports`](super::RunNetwork::release_ports)): not until pasta binds
    /// them, since the container's first process, forked before pasta
    /// starts, holds its own copies of them until it execs.
    pub(super) held: std::cell::RefCell<Vec<shipyard_network::ports::HeldPort>>,
    /// What it publishes.
    pub(super) ports: Vec<shipyard_network::ports::Published>,
    /// The ports as asked (to tell the kernel-chosen ones).
    pub(super) requests: Vec<shipyard_network::ports::PortRequest>,
    /// Where its files go (the run's directory).
    pub(super) dir: PathBuf,
    /// The network wasn't asked for: a failure says how to do without.
    pub(super) defaulted: bool,
    /// pasta, once started (by the hook): killed when the run ends.
    pub(super) running: std::cell::RefCell<Option<shipyard_network::pasta::Pasta>>,
    /// Where its hint and the ports it publishes are reported.
    pub(super) reporter: Arc<dyn Reporter>,
}

impl std::fmt::Debug for PastaPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PastaPlan")
            .field("program", &self.program)
            .field("ports", &self.ports)
            .field("dir", &self.dir)
            .field("defaulted", &self.defaulted)
            .finish_non_exhaustive()
    }
}

impl PastaPlan {
    /// Starts pasta for the container whose init is `pid` (the hook).
    pub(super) fn start(&self, pid: libc::pid_t) -> Result<(), String> {
        let pasta = shipyard_network::pasta::Pasta::start(
            &self.program,
            self.features,
            pid,
            &self.ports,
            &self.dir,
        )
        .map_err(|error| self.failed(&error))?;
        *self.running.borrow_mut() = Some(pasta);
        announce(&self.requests, &self.ports, self.reporter.as_ref());
        Ok(())
    }

    /// pasta's failure, said; with a hint (`--network host`) if the
    /// network wasn't asked for.
    fn failed(&self, error: &shipyard_network::pasta::PastaError) -> String {
        if self.defaulted {
            self.reporter.report(Event::Hint(HOST_INSTEAD.to_owned()));
        }
        error.to_string()
    }

    /// Starts pasta for a detached container, following its `init`: pasta
    /// runs while the init does, whatever becomes of this process. The
    /// held ports are let go just before (pasta binds them: a detached
    /// container's init came from an exec'd monitor, so never had copies).
    /// Its launcher holds `hold` (a lock) until pasta has ended.
    pub(super) fn start_detached(
        &self,
        init: &shipyard_network::pasta::InitToFollow<'_>,
        hold: Option<std::os::fd::BorrowedFd<'_>>,
    ) -> Result<(), String> {
        self.held.borrow_mut().clear();
        let monitors = monitors_cgroup_procs(init.pid);
        shipyard_network::pasta::Pasta::start_detached_in(
            &self.program,
            self.features,
            init,
            &self.ports,
            &self.dir,
            hold,
            monitors.as_ref().map(std::os::fd::AsFd::as_fd),
        )
        .map_err(|error| self.failed(&error))?;
        announce(&self.requests, &self.ports, self.reporter.as_ref());
        Ok(())
    }
}

/// The `cgroup.procs` of the cgroup the container's monitor runs in (its
/// init's parent, `shipyard/monitors/<id>` where there's a cgroup tree):
/// pasta's launcher joins it, so pasta lives in the monitor's cgroup, not in
/// the scope that started it (a login session's, which a shutdown or a
/// logout ends first). `None` if the parent isn't the monitor (the init
/// adopted) or its cgroup can't be opened: pasta then stays where it starts.
fn monitors_cgroup_procs(init: i32) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let stat = std::fs::read_to_string(format!("/proc/{init}/stat")).ok()?;
    let parent: i32 = stat
        .rsplit(')')
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let cmdline = std::fs::read(format!("/proc/{parent}/cmdline")).ok()?;
    // The argument that makes the binary a monitor (the runtime's
    // `MONITOR_ARG`).
    if !cmdline
        .split(|&byte| byte == 0)
        .any(|arg| arg == b"__shipyard-monitor")
    {
        return None;
    }
    let cgroups = std::fs::read_to_string(format!("/proc/{parent}/cgroup")).ok()?;
    let relative = cgroups.lines().find_map(|line| line.strip_prefix("0::"))?;
    let procs = std::path::Path::new(shipyard_runtime::cgroup::ROOT)
        .join(relative.trim_start_matches('/'))
        .join("cgroup.procs");
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(procs)
        .ok()
}
