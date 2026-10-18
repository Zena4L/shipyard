//! Attaching to a container by name (stage 9, step 9.4c1): the runtime's
//! client ([`shipyard_runtime::attach::client`]) to the container a
//! reference names, its end race settled by its record.

pub use shipyard_runtime::attach::client::{AttachError, AttachOptions, Ended, Settled};
#[cfg(target_os = "linux")]
pub(crate) use shipyard_runtime::attach::client::{Welcomed, attach, welcomed};

/// What an attachment comes to ([`client::settle`](shipyard_runtime::attach::client::settle)),
/// as the engine's [`Result`](crate::Result).
///
/// # Errors
///
/// As the runtime's `settle`.
#[cfg(target_os = "linux")]
pub(crate) fn settle(
    attached: Result<Result<Ended, AttachError>, shipyard_runtime::container::ContainerError>,
    ended_code: impl FnOnce() -> Option<i32>,
) -> crate::Result<Settled> {
    Ok(shipyard_runtime::attach::client::settle(
        attached, ended_code,
    )?)
}

/// Attaches to the container `reference` names (stage 9, step 9.4c1):
/// what [`settle`](shipyard_runtime::attach::client::settle) says, the end
/// race settled by its record once its end is (its `running` file free: its
/// clean-up has recorded it).
///
/// # Errors
///
/// No such container, or one that ended as this connected and was removed
/// at its end (`--rm`), its code with it
/// ([`ErrorKind::NotFound`](crate::ErrorKind::NotFound)); one that isn't up
/// ([`ErrorKind::NotRunning`](crate::ErrorKind::NotRunning)); as
/// [`settle`](shipyard_runtime::attach::client::settle).
pub fn to_container(
    engine: &crate::Engine,
    reference: &str,
    options: &AttachOptions,
) -> crate::Result<Settled> {
    #[cfg(target_os = "linux")]
    {
        use crate::containers::{self, Needs};
        use shipyard_runtime::container::{self as runtime, ContainerError};
        let record = containers::find(engine, reference)?;
        containers::require(engine, &record, Needs::Attachable, "attach to it")?;
        let root = engine.paths.runtime_root()?;
        let attached = match runtime::attach_stream(root, record.id.as_str()) {
            // Ended (and its state gone, or stopped) since it was seen up:
            // the end race, as a monitor that has let go.
            Err(ContainerError::NotFound(_) | ContainerError::WrongStatus { .. }) => {
                Err(ContainerError::MonitorGone(record.id.as_str().to_owned()))
            }
            connected => connected.map(|stream| attach(stream, options)),
        };
        settle(attached, || recorded_end(engine, &record)).map_err(|error| {
            if containers::read_one(engine, &record.id).is_err() {
                crate::Error::new(
                    crate::ErrorKind::NotFound,
                    format!(
                        "container {} ended as this attached, and was removed (--rm): its exit \
                         code went with it",
                        record.name
                    ),
                )
            } else {
                error
            }
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, options);
        Err(crate::error::linux_only("containers"))
    }
}

/// Container `record`'s exit code, once it has ended and its end is
/// settled (waited for up to [`SETTLE_WAIT`](crate::containers::SETTLE_WAIT)):
/// `None` if it's still up (what
/// went wrong wasn't its end: a monitor refusing a client, say), if its
/// end isn't settled by then, or if it's gone (`--rm`).
#[cfg(target_os = "linux")]
pub(crate) fn recorded_end(
    engine: &crate::Engine,
    record: &crate::containers::Record,
) -> Option<i32> {
    use crate::containers::{Needs, unmet};
    use shipyard_runtime::container as runtime;
    let root = engine.paths.runtime_root().ok()?;
    // Still there to attach to: not ended. (Stopping, its init gone and its
    // monitor cleaning up, is the end coming.)
    if runtime::peek_status(root, record.id.as_str())
        .is_ok_and(|status| unmet(status, Needs::Attachable).is_none())
    {
        return None;
    }
    let settled = crate::containers::settled(engine, &record.id, crate::containers::SETTLE_WAIT)
        .ok()
        .flatten()?;
    Some(settled.exit?.code_or_unknown())
}
