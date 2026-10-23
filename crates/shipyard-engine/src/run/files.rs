//! A container's files for `cp`: live through a helper, or a stopped one's
//! root mounted ([`container_files`]); that root view also serves a
//! create's fill of its volumes (`run::volumes::fill_volumes`).

use shipyard_runtime::container as runtime;
use shipyard_runtime::container::Status;

use super::take::{Took, take_or_leave};
use crate::Engine;
use crate::containers::{self, Record};
use crate::error::{Context as _, Error, ErrorKind, Result, bail};

/// How a copy reaches a container's files ([`container_files`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Files<'a> {
    /// It's up: by its runtime state root and ID (its monitor holds its
    /// files; a helper joins its namespaces).
    Live {
        /// The runtime's state root.
        root: &'a std::path::Path,
        /// The runtime's container ID.
        id: &'a str,
    },
    /// It isn't: its root as a start would mount it, held (its operation
    /// lock, its writable layer, its volumes) while this lasts.
    Stopped(&'a shipyard_runtime::container::RootView),
}

/// Calls `copy` with the files of the container `reference` names (stage 9,
/// step 9.4c2): through the container if it's up; else its root as a start
/// would mount it (its writable layer over its image, its volumes), with
/// the container held meanwhile, so a start waits (on its operation lock)
/// and nothing else mounts that layer. A created container that never
/// started gets its writable layer made here, as its first start would.
///
/// # Errors
///
/// No such container ([`ErrorKind::NotFound`],
/// also one its `--rm` removed meanwhile); one being worked on
/// ([`ErrorKind::Busy`]); what its root needs
/// (its image gone from the store, a volume in use); `copy`'s.
pub(crate) fn container_files<T>(
    engine: &Engine,
    reference: &str,
    mut copy: impl FnMut(Files<'_>) -> Result<T>,
) -> Result<T> {
    for _ in 0..3 {
        match take_or_leave(engine, reference)? {
            Took::Up(record) => {
                // Held for the copy: its helper may outlive the
                // container's init (it isn't in its PID namespace), and
                // no start may mount the same writable layer under it
                // meanwhile.
                let _operation =
                    containers::operation_lock(engine, &record.id, containers::OP_WAIT)?;
                let root = engine.paths.runtime_root()?;
                let id = record.id.as_str();
                let up = runtime::peek_status(root, id)?.is_some_and(Status::is_live);
                if up {
                    match copy(Files::Live { root, id }) {
                        // Ended before its helper could join it
                        // (nothing copied yet: the helper comes first).
                        Err(error) if never_joined(&error) => {}
                        copied => return copied,
                    }
                }
                // Ending: its end let go of (its `running` file free:
                // the clean-up done) before it's taken again, stopped.
                // Else, its init gone and not yet reaped, it still reads
                // as running, again and again.
                containers::wait_settled(engine, &record.id, containers::SETTLE_WAIT);
            }
            Took::Taken(taken) => {
                let (view, held) = stopped_view(engine, &taken.record)?;
                let copied = copy(Files::Stopped(&view));
                // Its layer and volumes let go, then the container.
                drop(held);
                drop(taken);
                return copied;
            }
            Took::Gone(record) => bail!(
                NotFound,
                "container {} was removed as it ended (--rm)",
                record.name
            ),
        }
    }
    bail!(
        Busy,
        "container {reference} kept starting and ending while it was being copied: try again"
    )
}

/// Whether `error` is a helper refused for a container that ended before
/// the helper joined it: stopped, or its runtime state gone already
/// (`NotFound`: its clean-up done, F84). Nothing was copied then: the
/// helper comes first.
fn never_joined(error: &Error) -> bool {
    matches!(
        error.find::<runtime::ContainerError>(),
        Some(
            runtime::ContainerError::WrongStatus { .. }
                | runtime::ContainerError::HelperNeedsMount(_)
                | runtime::ContainerError::NotFound(_)
        )
    )
}

/// What a stopped container's root view holds: its writable layer, its
/// volumes.
pub(super) struct ViewHeld {
    _area: Option<crate::image::RunArea>,
    /// Its volumes, held.
    pub(super) volumes: Vec<shipyard_storage::VolumeHold>,
}

/// Container `record`'s root as its start would mount it, for a copy (or
/// a create's fill):
/// its image's layers under its own writable layer (taken, and made if
/// it has none yet: a created container's first), or its `--rootfs`
/// directory; its volumes bound and held (made only for a container
/// never started, as its first start would: one removed since is said);
/// its user namespace's maps, chosen as a start chooses them.
/// Nothing else of a start's: no network, devices or security profile
/// is looked at, the image isn't pinned again, the record isn't written.
pub(super) fn stopped_view(
    engine: &Engine,
    record: &Record,
) -> Result<(runtime::RootView, ViewHeld)> {
    let request = record.to_request()?;
    let is_root = nix::unistd::geteuid().is_root();
    let (id_maps, _) =
        request.user_namespace(is_root, shipyard_runtime::process::rootless_id_maps)?;
    let (mut root, area) = match (&request.image, &request.rootfs) {
        (Some(reference), _) => {
            let Some(image_id) = &record.image_id else {
                bail!(
                    InvalidRequest,
                    "container {} was made by an older Shipyard: start it once, then copy",
                    record.name
                );
            };
            let owner = containers::require_store_id(engine)?;
            // Real root's maps (`--uidmap`): its layer given to the
            // container's root, as a start gives it.
            let maps = id_maps.as_ref().filter(|_| is_root);
            let prepared = crate::image::prepare_owned(
                engine,
                record.id.as_str(),
                &owner,
                reference,
                image_id,
                maps,
            )?;
            let mut root = shipyard_runtime::isolation::Root::new(prepared.root);
            root.overlay = Some(prepared.overlay);
            (root, Some(prepared.area))
        }
        (None, Some(dir)) => {
            let path = std::fs::canonicalize(dir)
                .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
                .with_context(|| format!("its --rootfs {}", dir.display()))?;
            (shipyard_runtime::isolation::Root::new(path), None)
        }
        (None, None) => bail!(
            Unsupported,
            "container {} has no root filesystem of its own to copy",
            record.name
        ),
    };
    root.read_only = request.read_only;
    let mounts = request
        .volume_mounts()
        .with_context(|| format!("container {}'s volumes", record.name))?;
    // What its create made, made again and bound as its starts do (11.7).
    let mut made = Vec::new();
    for source in crate::containers::created(engine, &record.id)?.made_sources {
        made.extend(source.made_for_this_start()?);
    }
    let named = crate::run::volumes::bind_volumes(engine, &mut root, mounts, &made)?;
    // Never started: its volumes made, as its first start would make
    // them (Docker's `create` does). Else one missing was removed since.
    let volumes = if record.started.is_none() {
        crate::run::volumes::hold_volumes(engine, &named)?
    } else {
        crate::run::volumes::hold_existing_volumes(engine, &named)?
    };
    Ok((
        runtime::RootView { root, id_maps },
        ViewHeld {
            _area: area,
            volumes,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy whose container ended before its helper joined it is tried
    /// again (then through its stopped view): stopped, or its state gone
    /// (F84); anything else is the copy's own failure (the control).
    #[test]
    fn a_container_gone_before_its_helper_joined_is_tried_again() {
        let error = |source: runtime::ContainerError| Error::from(source);
        assert!(never_joined(&error(runtime::ContainerError::NotFound(
            "c1".to_owned()
        ))));
        assert!(never_joined(&error(runtime::ContainerError::WrongStatus {
            id: "c1".to_owned(),
            status: Status::Stopped,
            action: "copy",
            wanted: "running",
        })));
        assert!(!never_joined(&Error::new(
            ErrorKind::Io,
            "a copy that failed"
        )));
        assert!(
            !never_joined(&error(runtime::ContainerError::MonitorTooOld(
                "c1".to_owned()
            ))),
            "another of the runtime's refusals"
        );
    }
}
