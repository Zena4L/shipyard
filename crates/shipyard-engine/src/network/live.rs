//! A running container's networks changed while it runs (stage 9, step
//! 9.7b2): what `network connect` and `disconnect` do to a network (the
//! container's side, its record and its monitor, is `run::connect`'s).
//!
//! **Connecting** takes a lease on the network for the container's run (its
//! run's ID, read from its other leases: its veths are named after it), as
//! a start does ([`Attachment::take`]: the bridge, the firewall, the
//! address, the DNS server), with no default route and no ports (those stay
//! with the network its start gave them), and makes its interface in the
//! container's network namespace (`eth{n}`, the first number free). The
//! caller then gives the lease's lock to the container's monitor
//! ([`Connected::hold`]), and lets go of its own ([`Connected::given`]).
//!
//! **Disconnecting** removes the container's lease from the network: its
//! file unlinked (its lock is its monitor's, which a process can't take
//! from another: TLPI §55.2), then what it made undone ([`release_lease`]:
//! its veth, its rules, the connections the kernel remembers).

#![cfg(target_os = "linux")]

use std::os::fd::BorrowedFd;

use shipyard_network::store::Lease;

use super::attach::{Attachment, TakeFor, release_lease};
use super::{Joined, RunId, networks};
use crate::Engine;
use crate::containers::Record;
use crate::error::{ErrorKind, Result, bail};
use crate::report::WarningKind;

/// The networks container `record` has a live lease on now, each with the
/// lease: those that say its ID. Only if none does (all made before leases
/// knew their container), those of its name that say no container (as
/// `rename` finds them): never a lease of a run with no record that has its
/// name, once its own say its ID. Root's networks only: none otherwise.
///
/// # Errors
///
/// The networks can't be listed.
pub(crate) fn container_leases(engine: &Engine, record: &Record) -> Result<Vec<(String, Lease)>> {
    let networks = match networks(engine) {
        Ok(networks) => networks,
        Err(error) if error.kind() == ErrorKind::NeedsRoot => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    for config in networks.list()? {
        match networks.live_leases(&config.name) {
            Ok(leases) => found.extend(
                leases
                    .into_iter()
                    .filter(|lease| is_its(lease, record))
                    .map(|lease| (config.name.clone(), lease)),
            ),
            Err(error) => engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "the network {}'s leases can't be read ({error}): a container on it isn't \
                     seen there",
                    config.name
                ),
            ),
        }
    }
    Ok(by_id_first(found, record))
}

/// Whether `lease` may be container `record`'s: it says its ID, or no
/// container and its name.
fn is_its(lease: &Lease, record: &Record) -> bool {
    lease.container.as_deref() == Some(record.id.as_str())
        || (lease.container.is_none() && lease.name.as_deref() == Some(record.name.as_str()))
}

/// `found` (leases that may be container `record`'s), only those that say
/// its ID if any does.
fn by_id_first(found: Vec<(String, Lease)>, record: &Record) -> Vec<(String, Lease)> {
    let by_id = |lease: &Lease| lease.container.as_deref() == Some(record.id.as_str());
    if found.iter().any(|(_, lease)| by_id(lease)) {
        found
            .into_iter()
            .filter(|(_, lease)| by_id(lease))
            .collect()
    } else {
        found
    }
}

/// The name container `record`'s leases may be found by on a network, as
/// well as its ID ([`container_leases`]' `leases`): its name only while
/// all its leases were made before leases knew their container; else
/// none (by its ID alone).
fn lease_name<'a>(leases: &[(String, Lease)], record: &'a Record) -> Option<&'a str> {
    (!leases.is_empty() && leases.iter().all(|(_, lease)| lease.container.is_none()))
        .then_some(record.name.as_str())
}

/// Whose each address on the networks is now (their live leases: a
/// container's ID), the networks' gateways (the host's own addresses on
/// them), and the networks whose leases couldn't be read (their addresses
/// unknown, said).
#[derive(Debug, Default)]
pub(crate) struct LeaseOwners {
    pub(crate) owners: std::collections::BTreeMap<std::net::IpAddr, String>,
    pub(crate) gateways: std::collections::BTreeSet<std::net::IpAddr>,
    pub(crate) left_out: Vec<String>,
}

/// [`LeaseOwners`] of root's networks, every address as its IPv4 one
/// where it's IPv4-mapped.
///
/// # Errors
///
/// As [`networks`] (not root: [`ErrorKind::NeedsRoot`]);
/// the networks' list can't be read.
pub(crate) fn lease_owners(engine: &Engine) -> Result<LeaseOwners> {
    let networks = networks(engine)?;
    let mut found = LeaseOwners::default();
    for config in networks.list()? {
        found.gateways.insert(config.gateway.to_canonical());
        if let Some(gateway6) = config.gateway6 {
            found.gateways.insert(std::net::IpAddr::V6(gateway6));
        }
        let leases = match networks.live_leases(&config.name) {
            Ok(leases) => leases,
            Err(error) => {
                found.left_out.push(format!(
                    "network {}'s leases can't be read: {error}",
                    config.name
                ));
                continue;
            }
        };
        for lease in leases {
            let Some(container) = lease.container.clone() else {
                continue;
            };
            for ip in lease.addresses() {
                found.owners.insert(ip.to_canonical(), container.clone());
            }
        }
    }
    Ok(found)
}

/// The first interface number not in `leases`' (their `eth{n}`), below
/// `most`.
pub(crate) fn next_interface(leases: &[(String, Lease)], most: usize) -> Option<usize> {
    (0..most).find(|n| {
        let name = shipyard_network::store::interface_name(*n);
        !leases.iter().any(|(_, lease)| lease.interface == name)
    })
}

/// What a container joining a network while it runs is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Joining<'a> {
    /// Its record.
    pub(crate) record: &'a Record,
    /// Its run's ID (from its other leases).
    pub(crate) run: &'a RunId,
    /// Its hostname.
    pub(crate) hostname: &'a str,
    /// Its interface's number on the network: `eth{n}`.
    pub(crate) n: usize,
    /// Its network namespace.
    pub(crate) netns: BorrowedFd<'a>,
    /// Its interface settings for that interface (`--sysctl
    /// net.ipv4.conf.eth{n}.*`).
    pub(crate) sysctls: &'a [(String, String)],
}

/// A network joined while the container runs: its lease still this
/// process's until it's given to the container's monitor.
#[derive(Debug)]
pub(crate) struct Connected(Attachment);

impl Connected {
    /// The lease's lock, for the container's monitor to hold.
    pub(crate) fn hold(&self) -> Option<BorrowedFd<'_>> {
        self.0.lease_hold()
    }

    /// The monitor holds the lease now: this process's copy is closed
    /// (never unlocked), and nothing is undone.
    pub(crate) fn given(self) {
        let _ = self.0.into_connect();
    }
}

/// Joins `joining` to `joined` (the network, its settings there): its
/// lease taken, its interface made in its namespace, its interface
/// settings set. If anything fails, what was done is undone (the
/// [`Attachment`]'s drop).
///
/// # Errors
///
/// As a start's taking a network, connecting it, or setting its
/// interface's settings.
pub(crate) fn connect_live(
    engine: &Engine,
    joining: &Joining<'_>,
    joined: &Joined,
) -> Result<Connected> {
    let whose = TakeFor {
        run: joining.run,
        hostname: joining.hostname,
        name: Some(joining.record.name.as_str()),
        container: Some(&joining.record.id),
    };
    let attachment = Attachment::take(
        engine,
        &whose,
        joined,
        joining.n,
        shipyard_network::attach::DefaultRoutes::default(),
        &[],
    )?;
    attachment
        .connect(joining.netns)
        .map_err(|message| crate::error::Error::new(ErrorKind::Network, message))?;
    if !joining.sysctls.is_empty() {
        shipyard_network::attach::set_interface_sysctls(joining.netns, joining.sysctls).map_err(
            |error| {
                crate::error::Error::new(
                    ErrorKind::InvalidRequest,
                    format!("the container's interface settings: {error}"),
                )
            },
        )?;
    }
    Ok(Connected(attachment))
}

/// Takes container `record`'s live lease on the network `name` away: what
/// it made undone first (its veth, its rules, its connections), then its
/// file removed (its monitor's lock left to it). In that order, under the
/// network's lock: a crash between the two leaves a lease whose veth is
/// gone, which a disconnect again (its record still says the network)
/// finishes; never a veth with an address no lease keeps (and the next
/// container would be given). `false`: it had none there.
///
/// # Errors
///
/// The network can't be locked, its leases read, or the lease's file
/// removed. (What can't be undone is said, and left: a disconnect again
/// tries again.)
pub(crate) fn disconnect_live(engine: &Engine, record: &Record, name: &str) -> Result<bool> {
    // By its ID, unless all its leases are older than that (its own leases
    // change only under its operation lock, which the caller holds).
    let by_name = lease_name(&container_leases(engine, record)?, record);
    let networks = networks(engine)?;
    let network = networks.lock(name)?;
    let Some(lease) = network.live_lease(record.id.as_str(), by_name)? else {
        return Ok(false);
    };
    let mut nft = shipyard_network::nft::Nftables::new()?;
    release_lease(
        network.config(),
        &lease,
        &mut nft,
        true,
        &format!("container {}", record.name),
        engine.reporter().as_ref(),
    );
    Ok(network.remove_live(record.id.as_str(), by_name)?.is_some())
}

/// The interfaces a container's default routes go through (in its
/// network namespace `netns`): IPv4's, then IPv6's.
///
/// # Errors
///
/// Its routes can't be read.
pub(crate) fn way_out(netns: BorrowedFd<'_>) -> Result<(Option<String>, Option<String>)> {
    shipyard_network::attach::default_route_interfaces(netns).map_err(|error| {
        crate::error::Error::new(
            ErrorKind::Network,
            format!("the container's routes: {error}"),
        )
    })
}

/// Fails unless `name` is a network made with `network create` or the
/// default one: not `host`, `none` or `pasta`.
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`] for those.
pub(crate) fn check_bridge(name: &str) -> Result<()> {
    if super::RESERVED.contains(&name) {
        bail!(
            InvalidRequest,
            "{name} isn't a network a container connects to: only bridge networks are"
        );
    }
    Ok(())
}
