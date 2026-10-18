//! Renaming a container (stage 9, step 9.4b2): its record, and, while it
//! runs on bridge networks, its name there (its DNS name) at once.
//!
//! A running container's name lives in two places: its record, and the
//! lease each of its bridge networks gave it (the networks' DNS servers
//! answer from the leases). Under the container's operation lock (no start,
//! stop or removal of it meanwhile):
//!
//! 1. the name is checked free among the containers (the store's lock,
//!    briefly: before any network's lock is taken, so none is held long);
//! 2. if it's up, the networks where it has a live lease are locked, in
//!    name order (two renames may wait for each other, never deadlock), and
//!    the name checked free on each (names there ignore case, as DNS does);
//! 3. the record is renamed (checked again: the store's lock decides between
//!    two renames to one name);
//! 4. each lease is rewritten in place (`shipyard_network`'s
//!    [`rename`](shipyard_network::store::LockedNetwork::rename)).
//!
//! A Ctrl-C waits from step 3 to the end, so the record and the leases
//! agree. A lease that can't be rewritten is said, not undone; renaming it
//! to the same name again rewrites what's left. Lock order: the operation
//! lock, the networks' (by name), the container store's.

use crate::Engine;
use crate::containers::Record;
use crate::error::Result;

/// Renames the container `reference` names `new`: its record, and its name
/// on the networks it's on (its DNS name, within a second).
///
/// # Errors
///
/// - [`ErrorKind::NotFound`](crate::ErrorKind::NotFound): no such
///   container (or one renamed meanwhile, found by its old name).
/// - [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a
///   name that can't be one (`--name`'s rule: a DNS label), or its name
///   already (everywhere).
/// - [`ErrorKind::Exists`](crate::ErrorKind::Exists): another container's
///   name, or one another container answers to on one of its networks.
/// - [`ErrorKind::Busy`](crate::ErrorKind::Busy): another command holds it,
///   or a store's lock is held too long.
/// - [`ErrorKind::Storage`](crate::ErrorKind::Storage): the container store
///   can't be read or written.
pub fn rename(engine: &Engine, reference: &str, new: &str) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::rename(engine, reference, new);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, new);
        Err(crate::error::linux_only("containers"))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use shipyard_network::store::{LockedNetwork, Networks};
    use shipyard_runtime::container::{self as runtime, Status};

    use crate::Engine;
    use crate::containers::{self, Name, Record};
    use crate::error::{ErrorKind, Result, bail};
    use crate::report::WarningKind;
    use crate::run::held_off::HeldOff;

    pub(super) fn rename(engine: &Engine, reference: &str, new: &str) -> Result<Record> {
        // `--name`'s rule (a DNS label, so its networks can answer to it),
        // within the record's.
        shipyard_network::dns::parse_name(new)
            .map_err(|why| crate::error::Error::new(ErrorKind::InvalidRequest, why))?;
        let new = Name::parse(new)?;
        let found = containers::find(engine, reference)?;
        // Again, by its ID, now nothing else works on it (as `take`). Found
        // by its name, it must still have it: another rename went first.
        let containers::Locked {
            record,
            operation: _operation,
        } = containers::lock_again(engine, &found.id, containers::OP_WAIT)?;
        if found.name.as_str() == reference && record.name != found.name {
            bail!(
                NotFound,
                "no container {reference:?}: it was renamed {}",
                record.name
            );
        }
        containers::check_name(engine, &record.id, &new)?;
        let (networks, by_name) = leased(engine, &record)?;
        let id = record.id.as_str();
        let old = record.name.as_str();
        // Its leases by its ID; by its name only if all are older than that.
        let found_by = by_name.then_some(old);
        let stale: Vec<&LockedNetwork> = networks
            .iter()
            .filter(|network| {
                network
                    .name_of(id, found_by)
                    .ok()
                    .flatten()
                    .is_some_and(|there| there != new.as_str())
            })
            .collect();
        if record.name == new && stale.is_empty() {
            bail!(
                InvalidRequest,
                "container {} is named {new} already",
                record.id.short()
            );
        }
        for network in &networks {
            network.check_rename(id, found_by, new.as_str())?;
        }
        // From here, a Ctrl-C waits: the record and its leases agree.
        let _held = HeldOff::new();
        let renamed = if record.name == new {
            record.clone()
        } else {
            let renamed = containers::rename(engine, &record.id, &new)?;
            crate::events::emit(
                engine,
                &crate::events::container(&renamed, "rename").with("oldName", old),
            );
            renamed
        };
        for network in &networks {
            if let Err(error) = network.rename(id, found_by, new.as_str()) {
                engine.warn(
                    WarningKind::LeftForLater,
                    format!(
                        "container {new}'s name on the network {} may not have changed: \
                         {error} (rename it {new} again, or start it again)",
                        network.config().name
                    ),
                );
            }
        }
        Ok(renamed)
    }

    /// The bridge networks where container `record` has a live lease,
    /// locked, in name order, and whether its leases are found by its name
    /// (all made before leases knew their container: none says its ID).
    /// None for one that isn't up, or a caller that isn't the host's root
    /// (only it has bridge networks). A network whose leases can't be read
    /// is said, and left out.
    fn leased(engine: &Engine, record: &Record) -> Result<(Vec<LockedNetwork>, bool)> {
        let runtime_root = engine.paths.runtime_root()?;
        let up = runtime::peek_status(runtime_root, record.id.as_str())
            .is_ok_and(|status| status.is_some_and(Status::is_live));
        if !up {
            return Ok((Vec::new(), false));
        }
        let leases = crate::network::container_leases(engine, record)?;
        let by_name = leases.iter().all(|(_, lease)| lease.container.is_none());
        let mut names: Vec<String> = leases.into_iter().map(|(network, _)| network).collect();
        if names.is_empty() {
            return Ok((Vec::new(), false));
        }
        let networks: Networks = crate::network::networks(engine)?;
        // In name order: every rename takes them so.
        names.sort();
        names.dedup();
        let locked = names
            .iter()
            .map(|name| networks.lock(name).map_err(Into::into))
            .collect::<Result<_>>()?;
        Ok((locked, by_name))
    }
}
