//! `update` (stage 9, step 9.5b; limits in step 11.3d1): a container's
//! settings changed without making it again, Docker's `update`: its
//! restart policy (`--restart`: a crash loop's restarts end with `--restart
//! no`, the container kept), and its resource limits (`--memory`,
//! `--cpus`, `--pids-limit`, …), live while it runs and kept for its next
//! starts.

use shipyard_runtime::resources::DockerLimits;

use crate::Engine;
use crate::containers::{Record, RestartPolicy};
use crate::error::{Result, bail};

/// What [`update`] changes (`None`: left as it is).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpdateRequest {
    /// Its restart policy.
    pub restart: Option<RestartPolicy>,
    /// Its resource limits: those given (Docker's, each `None` not given).
    pub limits: Option<DockerLimits>,
}

impl UpdateRequest {
    /// A request that changes the restart policy to `policy`.
    #[must_use]
    pub fn restart(policy: RestartPolicy) -> Self {
        Self {
            restart: Some(policy),
            limits: None,
        }
    }

    /// This request, with `limits` changed too (none, if they ask nothing:
    /// Docker's zeros are "not set").
    #[must_use]
    pub fn with_limits(mut self, limits: DockerLimits) -> Self {
        self.limits = limits.asks_anything().then_some(limits);
        self
    }
}

/// Changes the container `reference` names as `request` says, under its
/// operation lock (after a `start`, `stop` or `rm` of it that's under way).
/// A restart that waits finds the new policy as it next looks: `no` ends
/// it. Its record as changed.
///
/// # Errors
///
/// Nothing asked; a restart policy for a container removed as it ends
/// (`--rm`, Docker's rule); limits that can't be (a value the kernel
/// wouldn't take, a device's rate, a process limit lifted)
/// ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest)); no
/// such container; a limit the running container's kernel refused (the
/// runtime's: nothing kept); its operation lock held too long
/// ([`ErrorKind::Busy`](crate::ErrorKind::Busy)); off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn update(engine: &Engine, reference: &str, request: &UpdateRequest) -> Result<Record> {
    if request.restart.is_none() && request.limits.is_none() {
        bail!(
            InvalidRequest,
            "you must provide one or more flags when using this command"
        );
    }
    if let Some(limits) = &request.limits {
        refuse_unsupported_limits(limits)?;
    }
    #[cfg(target_os = "linux")]
    return linux::update(engine, reference, request);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, request);
        Err(crate::error::linux_only("containers"))
    }
}

/// What a running container's limits can't change: a device's rates
/// (Docker's `update` takes none), and a process or CPU limit lifted (`-1`:
/// the record keeps none for "no limit" against the default one, F125).
fn refuse_unsupported_limits(limits: &DockerLimits) -> Result<()> {
    let devices = [
        ("--device-read-bps", &limits.device_read_bps),
        ("--device-write-bps", &limits.device_write_bps),
        ("--device-read-iops", &limits.device_read_iops),
        ("--device-write-iops", &limits.device_write_iops),
    ];
    if let Some((flag, _)) = devices.iter().find(|(_, rates)| !rates.is_empty()) {
        bail!(
            InvalidRequest,
            "update takes no device rates ({flag}): they're given when a container is made"
        );
    }
    if limits.pids_limit.is_some_and(|limit| limit < 0) {
        bail!(
            InvalidRequest,
            "update can't lift a process limit yet (a limit of -1): give a number"
        );
    }
    if limits.cpu_quota.is_some_and(|quota| quota < 0) {
        bail!(
            InvalidRequest,
            "update can't lift a CPU limit yet (a quota of -1): give a number"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use shipyard_runtime::cgroup::{Controller, Tree};
    use shipyard_runtime::resources::{DockerLimits, Resources};

    use super::UpdateRequest;
    use crate::Engine;
    use crate::containers::{self, ContainerState, Record, RestartPolicy};
    use crate::error::{Error, ErrorKind, Result, bail};

    pub(super) fn update(
        engine: &Engine,
        reference: &str,
        request: &UpdateRequest,
    ) -> Result<Record> {
        // Read again under the lock: renamed, changed or removed meanwhile.
        let containers::Locked {
            record,
            operation: _operation,
        } = containers::lock_found(engine, reference, containers::OP_WAIT)?;
        if let Some(policy) = request.restart
            && record.auto_remove
            && policy != RestartPolicy::No
        {
            bail!(
                InvalidRequest,
                "container {} is removed as it ends (--rm): it can't be started again \
                 (--restart {policy})",
                record.name
            );
        }
        let up = matches!(
            containers::state(engine, &record)?,
            ContainerState::Running { .. } | ContainerState::Paused
        );
        let limits = match &request.limits {
            Some(limits) => Some(Limited::of(&record, limits)?),
            None => None,
        };
        if let Some(limits) = &limits
            && !up
        {
            // Its next start must be able to apply them: refused now, never
            // a container that can't start (rootless without delegation).
            placeable(&limits.both)?;
        }
        // The record first (what its next starts apply), then its cgroup:
        // a refusal there puts the record back, and the limits the kernel
        // took meanwhile, so neither is left saying what the other doesn't.
        let before = record.config.clone();
        let updated = containers::update(engine, &record.id, |record| {
            if let Some(policy) = request.restart {
                record.config.restart = policy;
            }
            if let Some(limits) = &limits {
                record.config.resources = containers::CgroupLimits::from(&limits.both);
            }
        })?;
        if up
            && let Some(limits) = &limits
            && let Err(error) = limits.applied(engine, &record)
        {
            // Its record put back too; if that can't be written, it says
            // limits the kernel refused, and every start applies them: said.
            if let Err(undo) = containers::update(engine, &record.id, |record| {
                record.config.restart = before.restart;
                record.config.resources = before.resources.clone();
            }) {
                engine.warn(
                    crate::report::WarningKind::LeftForLater,
                    format!(
                        "container {}'s record keeps the refused limits (it couldn't be put \
                         back: {undo:#}): its next start applies them; run `update` again",
                        record.id.short()
                    ),
                );
            }
            // Its cgroup was put back by the runtime: as it was before, each
            // file as the kernel had it (11.7).
            return Err(error);
        }
        let mut event = crate::events::container(&updated, "update");
        if let Some(policy) = request.restart {
            event = event.with("restartPolicy", policy);
        }
        crate::events::emit(engine, &event);
        Ok(updated)
    }

    /// A container's limits before an update and after it, and what its
    /// cgroup is to be told now.
    struct Limited {
        /// What it keeps now: what it kept with those given over them.
        both: Resources,
        /// What its cgroup is told while it runs: those given, with
        /// `cpu.max` whole (a period given alone keeps its quota) and a
        /// default swap following its memory.
        live: Resources,
    }

    impl Limited {
        /// `record`'s limits with `limits` over them, checked.
        fn of(record: &Record, limits: &DockerLimits) -> Result<Self> {
            let invalid = |why: shipyard_runtime::resources::ResourceError| {
                Error::new(ErrorKind::InvalidRequest, why.to_string())
            };
            let kept = record.config.resources.to_resources();
            let given = limits
                .resources_as_given(kept.memory_max)
                .map_err(invalid)?;
            let both = kept.overlaid(&given);
            both.validate().map_err(invalid)?;
            let mut live = given.clone();
            live.cpu_max = given.cpu_max.and(both.cpu_max);
            if given.swap_max.is_none() && both.swap_max != kept.swap_max {
                live.swap_max = both.swap_max;
                live.swap_is_default = both.swap_is_default;
            }
            Ok(Self { both, live })
        }

        /// The new limits written in `record`'s cgroup; what was skipped
        /// said.
        fn applied(&self, engine: &Engine, record: &Record) -> Result<()> {
            let root = engine.paths.runtime_root()?;
            let skipped =
                shipyard_runtime::container::update(root, record.id.as_str(), &self.live)?;
            for why in skipped {
                engine.warn(crate::report::WarningKind::Runtime, why);
            }
            Ok(())
        }
    }

    /// Refuses limits a start couldn't write here: no cgroup tree this
    /// user can make one in, or a controller it doesn't offer (what a start
    /// would refuse: `LimitsNeedACgroup`).
    fn placeable(limits: &Resources) -> Result<()> {
        if limits.is_empty() {
            return Ok(());
        }
        let tree = Tree::find().map_err(|why| {
            Error::new(
                ErrorKind::Environment,
                format!("its limits need a cgroup of its own, and none can be made here: {why}"),
            )
        })?;
        let wanted = [
            (
                Controller::Memory,
                "memory",
                limits.memory_max.is_some()
                    || limits.memory_low.is_some()
                    || limits.swap_max.is_some(),
            ),
            (
                Controller::Cpu,
                "cpu",
                limits.cpu_max.is_some() || limits.cpu_weight.is_some(),
            ),
            (
                Controller::Cpuset,
                "cpuset",
                limits.cpuset_cpus.is_some() || limits.cpuset_mems.is_some(),
            ),
            (Controller::Pids, "pids", limits.pids_max.is_some()),
            (
                Controller::Io,
                "io",
                limits.io_weight.is_some() || !limits.io_max.is_empty(),
            ),
        ];
        if let Some((_, name, _)) = wanted
            .iter()
            .find(|(controller, _, asked)| *asked && !tree.offers(*controller))
        {
            bail!(
                Environment,
                "its limits need the {name} controller, which isn't delegated here: \
                 a start couldn't apply them"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    /// What an update can't do is refused before anything is looked at,
    /// by kind: nothing asked (Docker's zeros are nothing), a device's
    /// rate, a process or CPU limit lifted.
    #[test]
    fn what_an_update_can_t_do_is_refused() {
        let (engine, _said, _store) = crate::testing::engine_on_a_new_store("update-refusals");
        let refused = |request: &UpdateRequest| {
            update(&engine, "web", request)
                .map(drop)
                .unwrap_err()
                .kind()
        };
        assert_eq!(
            refused(&UpdateRequest::default()),
            ErrorKind::InvalidRequest
        );
        let mut zeros = DockerLimits::default();
        zeros.memory = Some(0);
        zeros.pids_limit = Some(0);
        zeros.cpus = Some("0".to_owned());
        let zeros = UpdateRequest::default().with_limits(zeros);
        assert!(zeros.limits.is_none(), "zeros ask nothing");
        assert_eq!(refused(&zeros), ErrorKind::InvalidRequest);
        let limited = |change: fn(&mut DockerLimits)| {
            let mut limits = DockerLimits::default();
            change(&mut limits);
            UpdateRequest::default().with_limits(limits)
        };
        for request in [
            limited(|limits| limits.pids_limit = Some(-1)),
            limited(|limits| limits.cpu_quota = Some(-1)),
            limited(|limits| {
                limits.device_read_bps = vec![(std::path::PathBuf::from("/dev/sda"), 1)];
            }),
        ] {
            assert_eq!(refused(&request), ErrorKind::InvalidRequest, "{request:?}");
        }
        // One that asks something gets past them (to the container's
        // lookup: none here, or not this platform's).
        let asked = limited(|limits| limits.memory = Some(64 << 20));
        assert_ne!(refused(&asked), ErrorKind::InvalidRequest);
    }
}
