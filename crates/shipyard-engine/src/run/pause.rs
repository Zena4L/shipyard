//! `pause` and `unpause` (step 11.3d1): a running container's processes
//! frozen where they are, and thawed, through its cgroup's freezer
//! (`cgroup.freeze`, the runtime's `pause` and `resume`), as Docker's.

use crate::Engine;
use crate::containers::Record;
use crate::error::Result;

/// Freezes the container `reference` names: a running one becomes paused,
/// its processes stopped where they are until [`unpause`]. Under its
/// operation lock; its `pause` event said. Its record.
///
/// # Errors
///
/// No such container; one that isn't running, or is already paused
/// ([`ErrorKind::NotRunning`](crate::ErrorKind::NotRunning), Docker's
/// 409); one without a cgroup of its own, or a freeze that didn't happen
/// in time (the runtime's); off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn pause(engine: &Engine, reference: &str) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::frozen(engine, reference, true);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference);
        Err(crate::error::linux_only("containers"))
    }
}

/// Thaws the paused container `reference` names: it runs on from where it
/// was. Under its operation lock; its `unpause` event said. Its record.
///
/// # Errors
///
/// No such container; one that isn't paused
/// ([`ErrorKind::NotRunning`](crate::ErrorKind::NotRunning), Docker's
/// 409); a thaw that didn't happen in time (the runtime's); off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn unpause(engine: &Engine, reference: &str) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::frozen(engine, reference, false);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference);
        Err(crate::error::linux_only("containers"))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::Engine;
    use crate::containers::{self, ContainerState, Record};
    use crate::error::{Result, bail};

    /// Freezes (`freeze`) or thaws the container `reference` names, as
    /// [`super::pause`] and [`super::unpause`] say.
    pub(super) fn frozen(engine: &Engine, reference: &str, freeze: bool) -> Result<Record> {
        // Read again under the lock: a start, stop or rm meanwhile.
        let containers::Locked {
            record,
            operation: _operation,
        } = containers::lock_found(engine, reference, containers::OP_WAIT)?;
        let state = containers::state(engine, &record)?;
        let name = &record.name;
        match (freeze, &state) {
            (true, ContainerState::Running { .. }) | (false, ContainerState::Paused) => {}
            (true, ContainerState::Paused) => {
                bail!(NotRunning, "container {name} is already paused")
            }
            (false, ContainerState::Running { .. }) => {
                bail!(NotRunning, "container {name} is not paused")
            }
            (_, _) => bail!(NotRunning, "container {name} is not running"),
        }
        let root = engine.paths.runtime_root()?;
        // Ctrl-C and its kin wait until the freeze is reached or undone:
        // cut between the two, it would be left asked (F126).
        let _held = crate::run::HeldOff::new();
        if freeze {
            shipyard_runtime::container::pause(root, record.id.as_str())?;
        } else {
            shipyard_runtime::container::resume(root, record.id.as_str())?;
        }
        let action = if freeze { "pause" } else { "unpause" };
        crate::events::emit(engine, &crate::events::container(&record, action));
        Ok(record)
    }
}
