//! `update` (stage 9, step 9.5b): a container's settings changed without
//! making it again. For now its restart policy (`--restart`), Docker's
//! `update --restart`: a crash loop's restarts end with `--restart no`,
//! the container kept.

use crate::Engine;
use crate::containers::{Record, RestartPolicy};
use crate::error::Result;

/// What [`update`] changes (`None`: left as it is).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpdateRequest {
    /// Its restart policy.
    pub restart: Option<RestartPolicy>,
}

impl UpdateRequest {
    /// A request that changes the restart policy to `policy`.
    #[must_use]
    pub fn restart(policy: RestartPolicy) -> Self {
        Self {
            restart: Some(policy),
        }
    }
}

/// Changes the container `reference` names as `request` says, under its
/// operation lock (after a `start`, `stop` or `rm` of it that's under way).
/// A restart that waits finds the new policy as it next looks: `no` ends
/// it. Its record as changed.
///
/// # Errors
///
/// No such container; a restart policy for a container removed as it ends
/// (`--rm`) ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest),
/// Docker's rule); its operation lock held too long
/// ([`ErrorKind::Busy`](crate::ErrorKind::Busy)); off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn update(engine: &Engine, reference: &str, request: &UpdateRequest) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::update(engine, reference, request);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, request);
        Err(crate::error::linux_only("containers"))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::UpdateRequest;
    use crate::Engine;
    use crate::containers::{self, Record, RestartPolicy};
    use crate::error::{Result, bail};
    use crate::run::detached::linux::OP_WAIT;

    pub(super) fn update(
        engine: &Engine,
        reference: &str,
        request: &UpdateRequest,
    ) -> Result<Record> {
        let found = containers::find(engine, reference)?;
        let _operation = containers::operation_lock(engine, &found.id, OP_WAIT)?;
        // Again, under the lock: renamed, changed or removed meanwhile.
        let record = containers::read_one(engine, &found.id)?;
        let Some(policy) = request.restart else {
            return Ok(record);
        };
        if record.auto_remove && policy != RestartPolicy::No {
            bail!(
                InvalidRequest,
                "container {} is removed as it ends (--rm): it can't be started again \
                 (--restart {policy})",
                record.name
            );
        }
        let updated =
            containers::update(engine, &record.id, |record| record.config.restart = policy)?;
        crate::events::emit(
            engine,
            &crate::events::container(&updated, "update").with("restartPolicy", policy),
        );
        Ok(updated)
    }
}
