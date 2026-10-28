//! The observed half of [`super::links`], read on Linux: each running
//! container's network namespace found by its init, each read once.

use std::collections::BTreeMap;

use shipyard_network::observe::{self, ObserveError, Socket};

use super::{Addresses, Entry, Namespace, Observed, Read, Unread};
use crate::Engine;
use crate::containers::{ContainerState, Summary};

/// Why observed links can't be looked for without root's networks.
const NO_LEASES: &str = "rootless containers have no shared network (each its own pasta): \
                         the other end of a connection can't be told";

/// The containers in one network namespace, each with its init (its PID
/// and start time), in the order a read tries them.
type Members = Vec<(String, i32, u64)>;

/// The observed links of `summaries`' running containers: see
/// [`super::Links`].
pub(super) fn read_namespaces(engine: &Engine, summaries: &[Summary], entries: &[Entry]) -> Read {
    let found = match crate::network::lease_owners(engine) {
        Ok(found) => found,
        Err(error) if error.kind() == crate::ErrorKind::NeedsRoot => {
            return Read::nothing(Observed::unavailable("rootless", NO_LEASES));
        }
        Err(error) => {
            return Read::nothing(Observed::unavailable(
                "no_networks",
                format!("the networks can't be read: {error}"),
            ));
        }
    };
    let Ok(own) = observe::own_namespace() else {
        // A host-network container's table would pass for its own.
        return Read::nothing(Observed::unavailable(
            "own_namespace",
            "this service's own network namespace can't be read",
        ));
    };
    let mut read = Read::nothing(Observed::available());
    if !found.left_out.is_empty() {
        read.observed.reason = Some(found.left_out.join("; "));
    }
    let mut host = found.gateways;
    // The host's own addresses too (its uplink's, a LAN's): a client there
    // reaches a published port unmasqueraded, and a container that dials
    // one reaches the host.
    host.extend(own_addresses());
    let addresses = Addresses {
        owners: found.owners,
        host,
        published: entries.to_vec(),
    };
    let namespaces = find_namespaces(engine, summaries, own, &mut read.unread);
    let read_ones = read_tables(namespaces, &addresses, &mut read);
    read.links = super::observed(&read_ones, &addresses);
    read.entry_connections = super::entry_connections(&read_ones, &addresses);
    read
}

/// Each running container's network namespace (by its inode), with the
/// containers in it. One on the host's own network (`own`), or whose init
/// can't be found, or ended as its namespace was looked up, is put in
/// `unread`.
fn find_namespaces(
    engine: &Engine,
    summaries: &[Summary],
    own: u64,
    unread: &mut Vec<Unread>,
) -> BTreeMap<u64, Members> {
    let mut namespaces: BTreeMap<u64, Members> = BTreeMap::new();
    for summary in summaries {
        if !matches!(
            summary.state,
            ContainerState::Running { .. } | ContainerState::Paused | ContainerState::Stopping
        ) {
            continue;
        }
        let id = summary.record.id.as_str().to_owned();
        let left_out = |reason: String| Unread {
            container: id.clone(),
            reason,
        };
        let Some((pid, started)) = crate::containers::init_of(engine, &id) else {
            unread.push(left_out("its init can't be found (it ended?)".to_owned()));
            continue;
        };
        // The namespace looked up by a PID not held: still the init's after,
        // or it was another process's.
        let namespace = observe::namespace(pid).and_then(|namespace| {
            if shipyard_runtime::container::is_process(pid, started) {
                Ok(namespace)
            } else {
                Err(ObserveError::Gone(pid))
            }
        });
        match namespace {
            Ok(namespace) if namespace == own => unread.push(left_out(
                "on the host's network: its sockets are the host's".to_owned(),
            )),
            Ok(namespace) => namespaces
                .entry(namespace)
                .or_default()
                .push((id, pid, started)),
            Err(error) => unread.push(left_out(reason(&error))),
        }
    }
    namespaces
}

/// Each namespace's table, read once, kept to what links are made of
/// (listeners, connections to a container or the host, and those on a
/// published container port): through the
/// init of the container whose lease it holds (a joiner has none), else of
/// the next member if that one has ended. One that can't be read is put in
/// `read.unread`; those shared are said in `read.shared`.
fn read_tables(
    namespaces: BTreeMap<u64, Members>,
    addresses: &Addresses,
    read: &mut Read,
) -> Vec<Namespace> {
    let keep = |socket: &Socket| super::kept(socket, addresses);
    let mut tables = Vec::new();
    for mut members in namespaces.into_values() {
        let leased = |id: &String| addresses.owners.values().any(|owner| owner == id);
        members.sort_by_key(|(id, _, _)| !leased(id));
        let ids: Vec<String> = members.iter().map(|(id, _, _)| id.clone()).collect();
        let mut last = None;
        let sockets = members.iter().find_map(|(_, pid, started)| {
            crate::containers::init_sockets(*pid, *started, keep)
                .map_err(|error| last = Some(error))
                .ok()
        });
        let Some(sockets) = sockets else {
            let why = last.map_or_else(
                || "nothing to read it through".to_owned(),
                |error| reason(&error),
            );
            read.unread.extend(ids.into_iter().map(|container| Unread {
                container,
                reason: why.clone(),
            }));
            continue;
        };
        if ids.len() > 1 {
            for id in &ids {
                read.shared.insert(id.clone(), ids.clone());
            }
        }
        tables.push(Namespace {
            owner: ids[0].clone(),
            sockets,
        });
    }
    tables
}

/// The host's own addresses (this process's network namespace's: the API's
/// service runs in the host's), loopback aside; none if they can't be listed
/// (the gateways alone then).
fn own_addresses() -> Vec<std::net::IpAddr> {
    shipyard_network::route::Rtnetlink::new()
        .and_then(|mut route| route.addresses(None))
        .map(|found| {
            found
                .into_iter()
                .map(|address| address.net.addr().to_canonical())
                .filter(|ip| !ip.is_loopback())
                .collect()
        })
        .unwrap_or_default()
}

/// Why a namespace couldn't be read, as [`Unread`] says it.
fn reason(error: &ObserveError) -> String {
    match error {
        ObserveError::Gone(_) => "it ended as it was read".to_owned(),
        ObserveError::Hidden(_) => "its /proc entry is hidden from this user (hidepid)".to_owned(),
        other => format!("its sockets can't be read: {other}"),
    }
}
