//! The links between containers, for the desktop UI's map (step 14.1a):
//! those compose declares (`depends_on`, which it writes in each container's
//! `com.docker.compose.depends_on` label: `service:condition:restart`, comma
//! separated) and those seen in the containers' socket tables, each with its
//! connections now; and where the map starts, each container's published
//! ports, each with its connections from outside the containers (step 14.3).
//!
//! An observed link is counted once, from its client's side, and only when
//! both ends are seen: a connected socket in one container's network
//! namespace whose remote is another's address (its lease), mirrored by a
//! socket in that other's namespace (the same two addresses, the other way
//! round). A container reaching another through a port it publishes on the
//! host is masqueraded on its way (the host's NAT: the target sees one of
//! the host's addresses), but keeps its source port where it can: the
//! target's socket on its container port, from a host address and that
//! port, confirms it. A UDP server's socket is never connected: a connected
//! UDP client is confirmed by the server bound on that port (weaker: said in
//! the docs). Each namespace is read once, however many containers share it
//! (`--network container:`), and never the host's own (`--network host`:
//! its table is everyone's). Only connections up count (`ESTAB`): one
//! closed lingers in `TIME-WAIT` for a minute.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};

use serde::Serialize;
use shipyard_network::observe::{Protocol, Socket, State};

use super::ContainerState;
use crate::Engine;
use crate::error::Result;

/// The format of [`Links`]: a newer one is the client's to refuse.
pub const LINKS_VERSION: u32 = 1;

/// The label compose writes a service's `depends_on` in.
pub const DEPENDS_ON_LABEL: &str = "com.docker.compose.depends_on";
/// The label of a container's compose project.
pub const PROJECT_LABEL: &str = "com.docker.compose.project";
/// The label of a container's compose service.
pub const SERVICE_LABEL: &str = "com.docker.compose.service";
/// The label compose sets on a one-off container (`compose run`).
pub const ONE_OFF_LABEL: &str = "com.docker.compose.oneoff";

/// The most `depends_on` entries read from one label: past it, ignored.
const MAX_DEPENDS: usize = 256;

/// Every container, how they're linked, and where the map starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Links {
    /// [`LINKS_VERSION`].
    pub version: u32,
    /// Every container.
    pub containers: Vec<Node>,
    /// The services a container declares it depends on that have no
    /// container now.
    pub missing: Vec<Missing>,
    /// The ports running containers publish on the host: where the map
    /// starts.
    pub entries: Vec<Entry>,
    /// The links, declared and observed.
    pub links: Vec<Link>,
    /// Whether observed links could be looked for, and why not.
    pub observed: Observed,
    /// The containers whose sockets weren't read, and why.
    pub unread: Vec<Unread>,
}

/// A container, as the map draws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Node {
    /// Its ID.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its ISO 6346 serial, as written (`SHPU 304812 2`).
    pub serial: String,
    /// Its memory limit (`memory.max`), in bytes, if it has one.
    pub memory_max: Option<u64>,
    /// When it last started (RFC 3339), if it has.
    pub started_at: Option<String>,
    /// Its compose project, if compose made it.
    pub project: Option<String>,
    /// Its compose service, if compose made it.
    pub service: Option<String>,
    /// Its state's word (`running`, `exited`, …).
    pub state: &'static str,
    /// What its health checks say, if it has them and is up.
    pub health: Option<crate::health::Status>,
    /// How it last ended, if it has.
    pub exit_code: Option<i32>,
    /// A one-off container of compose's (`compose run`).
    pub one_off: bool,
    /// The containers in its network namespace (`--network container:`),
    /// it among them, when there's more than one.
    pub shares_network_with: Vec<String>,
    /// Up: running, paused, or still stopping (Docker's "running").
    #[serde(skip)]
    pub(crate) up: bool,
    /// Ended, and not about to restart.
    #[serde(skip)]
    pub(crate) ended: bool,
}

/// A service declared as a dependency, with no container.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[non_exhaustive]
pub struct Missing {
    /// Its key, as links name it: `PROJECT/SERVICE`.
    pub id: String,
    /// Its project.
    pub project: String,
    /// Its service.
    pub service: String,
}

/// A port a container publishes on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Entry {
    /// The container's ID.
    pub container: String,
    /// The host address (unspecified: every one).
    pub host_ip: IpAddr,
    /// The host's port.
    pub host_port: u16,
    /// The container's port.
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
    /// Its TCP connections up now from outside the containers (the host's
    /// clients; on a Mac, its forwarded ports'): never another
    /// container's, through the host or not, nor the container's own.
    pub connections: u32,
}

/// What made a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    /// Compose's `depends_on`.
    Declared,
    /// Connections seen in the socket tables.
    Observed,
}

/// One container's link to another (or to a [`Missing`] service).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Link {
    /// The container that depends, or connects: its ID.
    pub from: String,
    /// The one it depends on, or connects to: its ID, or a [`Missing`]'s.
    pub to: String,
    /// `to` is a [`Missing`] service's key, not a container's ID.
    pub to_missing: bool,
    /// What made it.
    pub kind: LinkKind,
    /// The port it connects to (observed).
    pub port: Option<u16>,
    /// `tcp` or `udp` (observed).
    pub protocol: Option<&'static str>,
    /// The condition compose waits for (declared): `service_started`,
    /// `service_healthy` or `service_completed_successfully`.
    pub condition: Option<String>,
    /// Its connections up now (observed).
    pub connections: u32,
    /// Through a port the target publishes on the host (observed).
    pub via_host_port: bool,
    /// Its condition isn't met (declared): the target isn't up (or isn't
    /// healthy, or didn't end well, as the condition asks), or has no
    /// container.
    pub refused: bool,
}

/// Whether observed links could be looked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Observed {
    /// They could.
    pub available: bool,
    /// Why not, in a word a client can match: `rootless` (no shared
    /// networks to tell a peer by), `not_linux`, `no_runtime` (its state
    /// can't be found), `no_networks` (they can't be read), `own_namespace`
    /// (this service's own network namespace can't be read: a host-network
    /// container couldn't be told).
    pub cause: Option<&'static str>,
    /// Why not, or what was left out (a network whose leases can't be
    /// read), said.
    pub reason: Option<String>,
}

impl Observed {
    /// Available, nothing left out.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "the tables are read on Linux")
    )]
    fn available() -> Self {
        Self {
            available: true,
            cause: None,
            reason: None,
        }
    }

    /// Not available, for `cause`, said as `reason`.
    fn unavailable(cause: &'static str, reason: impl Into<String>) -> Self {
        Self {
            available: false,
            cause: Some(cause),
            reason: Some(reason.into()),
        }
    }
}

/// A container whose sockets weren't read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Unread {
    /// Its ID.
    pub container: String,
    /// Why.
    pub reason: String,
}

/// One entry of a `depends_on` label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Depends {
    pub(crate) service: String,
    pub(crate) condition: String,
}

/// The entries of a `depends_on` label (`service:condition:restart[:…]`,
/// comma separated, at most [`MAX_DEPENDS`]): an entry with no service is
/// skipped, one with no condition is `service_started` (compose's default).
pub(crate) fn parse_depends_on(label: &str) -> Vec<Depends> {
    label
        .split(',')
        .take(MAX_DEPENDS)
        .filter_map(|entry| {
            let mut parts = entry.trim().split(':');
            let service = parts.next()?.trim();
            if service.is_empty() {
                return None;
            }
            let condition = parts
                .next()
                .map(str::trim)
                .filter(|condition| !condition.is_empty())
                .unwrap_or("service_started");
            Some(Depends {
                service: service.to_owned(),
                condition: condition.to_owned(),
            })
        })
        .collect()
}

/// What [`declared`] needs of a container: its node, and its `depends_on`
/// label (a caller's: labels are never an image's, `labels.rs`).
#[derive(Debug, Clone)]
pub(crate) struct Declaring<'a> {
    pub(crate) node: &'a Node,
    pub(crate) depends_on: Option<&'a str>,
}

/// Whether `target` meets `condition`: up for `service_started` (and any
/// condition it doesn't know), up and healthy for `service_healthy` (one
/// without checks: up), ended with 0 (not about to restart) for
/// `service_completed_successfully`.
fn meets(target: &Node, condition: &str) -> bool {
    match condition {
        "service_healthy" => {
            target.up
                && target
                    .health
                    .is_none_or(|health| health == crate::health::Status::Healthy)
        }
        "service_completed_successfully" => target.ended && target.exit_code == Some(0),
        _ => target.up,
    }
}

/// The declared links of `containers`: each one's `depends_on`, to every
/// container of the service in its own project (never another project's,
/// never a one-off's), or to the service as [`Missing`] when it has none;
/// a service named twice is one link.
pub(crate) fn declared(containers: &[Declaring<'_>]) -> (Vec<Link>, Vec<Missing>) {
    let mut links = Vec::new();
    let mut missing = BTreeSet::new();
    for from in containers {
        let (Some(project), Some(label)) = (&from.node.project, from.depends_on) else {
            continue;
        };
        if from.node.one_off {
            continue;
        }
        let mut seen = BTreeSet::new();
        for depends in parse_depends_on(label) {
            if !seen.insert(depends.service.clone()) {
                continue;
            }
            let targets: Vec<&Node> = containers
                .iter()
                .map(|other| other.node)
                .filter(|other| {
                    !other.one_off
                        && other.project.as_ref() == Some(project)
                        && other.service.as_deref() == Some(depends.service.as_str())
                })
                .collect();
            let link = |to: String, to_missing: bool, refused: bool| Link {
                from: from.node.id.clone(),
                to,
                to_missing,
                kind: LinkKind::Declared,
                port: None,
                protocol: None,
                condition: Some(depends.condition.clone()),
                connections: 0,
                via_host_port: false,
                refused,
            };
            if targets.is_empty() {
                let key = Missing {
                    id: format!("{project}/{}", depends.service),
                    project: project.clone(),
                    service: depends.service.clone(),
                };
                links.push(link(key.id.clone(), true, true));
                missing.insert(key);
            }
            for target in targets {
                links.push(link(
                    target.id.clone(),
                    false,
                    !meets(target, &depends.condition),
                ));
            }
        }
    }
    (links, missing.into_iter().collect())
}

/// One network namespace's sockets, and the container it's named after.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
pub(crate) struct Namespace {
    pub(crate) owner: String,
    pub(crate) sockets: Vec<Socket>,
}

/// What [`observed`] knows of the host: whose each address is, the
/// host's own addresses (the networks' gateways), and the ports
/// containers publish on it.
#[derive(Debug, Clone, Default)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
pub(crate) struct Addresses {
    pub(crate) owners: BTreeMap<IpAddr, String>,
    pub(crate) host: BTreeSet<IpAddr>,
    pub(crate) published: Vec<Entry>,
}

/// Whether a socket read from a container's table is one links are made
/// of: a listener, a connection to a container or to the host, or one on a
/// published container port whatever its other end (a client anywhere: its
/// entry's count).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
pub(crate) fn kept(socket: &Socket, addresses: &Addresses) -> bool {
    if socket.is_listening() {
        return true;
    }
    if socket.state != State::Established {
        return false;
    }
    let ip = socket.remote.ip().to_canonical();
    addresses.owners.contains_key(&ip)
        || addresses.host.contains(&ip)
        || addresses
            .published
            .iter()
            .any(|entry| entry.container_port == socket.local.port())
}

/// An address with an IPv4-mapped IPv6 address as its IPv4 one.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

/// The namespaces' tables, indexed for [`observed`].
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
struct Tables<'a> {
    addresses: &'a Addresses,
    /// Each namespace's index, by its owner.
    namespace_of: HashMap<&'a str, usize>,
    /// Each connection up, by (protocol, its own end, the other), with its
    /// namespace.
    ends: HashMap<(Protocol, SocketAddr, SocketAddr), usize>,
    /// Each connection up whose other end is one of the host's addresses,
    /// by (protocol, its own port, the other's port), with its namespaces:
    /// a target's end of one the host's NAT carried in through a published
    /// port, and a container's own end of one it made to such a port.
    host_ends: HashMap<(Protocol, u16, u16), Vec<usize>>,
    /// Each namespace's ports waiting for others.
    listening: BTreeSet<(usize, Protocol, u16)>,
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
impl<'a> Tables<'a> {
    /// `namespaces` indexed, against `addresses`.
    fn of(namespaces: &'a [Namespace], addresses: &'a Addresses) -> Self {
        let mut tables = Self {
            addresses,
            namespace_of: HashMap::new(),
            ends: HashMap::new(),
            host_ends: HashMap::new(),
            listening: BTreeSet::new(),
        };
        for (index, namespace) in namespaces.iter().enumerate() {
            tables.namespace_of.insert(namespace.owner.as_str(), index);
            for socket in &namespace.sockets {
                let (local, remote) = (canonical(socket.local), canonical(socket.remote));
                if socket.is_listening() {
                    tables
                        .listening
                        .insert((index, socket.protocol, local.port()));
                } else if socket.state == State::Established {
                    tables.ends.insert((socket.protocol, local, remote), index);
                    if addresses.host.contains(&remote.ip()) {
                        tables
                            .host_ends
                            .entry((socket.protocol, local.port(), remote.port()))
                            .or_default()
                            .push(index);
                    }
                }
            }
        }
        tables
    }

    /// Whom the connection `(local, remote)` of namespace `index` reaches,
    /// on which of its ports, and whether through a port it publishes on
    /// the host: only when that one's namespace confirms it (its mirror
    /// socket; for UDP, its socket bound on the port).
    fn target(
        &self,
        index: usize,
        protocol: Protocol,
        (local, remote): (SocketAddr, SocketAddr),
    ) -> Option<(String, u16, bool)> {
        if let Some(owner) = self.addresses.owners.get(&remote.ip()) {
            let other = *self.namespace_of.get(owner.as_str())?;
            let confirmed = self.ends.get(&(protocol, remote, local)) == Some(&other)
                || (protocol == Protocol::Udp
                    && self.listening.contains(&(other, protocol, remote.port())));
            return (other != index && confirmed).then(|| (owner.clone(), remote.port(), false));
        }
        if !self.addresses.host.contains(&remote.ip()) {
            return None;
        }
        // Through a port published on the host: masqueraded on its way, the
        // target sees a host address, the source port kept.
        self.addresses.published.iter().find_map(|entry| {
            let on_it = entry.host_port == remote.port()
                && entry.protocol == protocol.as_str()
                && (entry.host_ip.is_unspecified() || entry.host_ip.to_canonical() == remote.ip());
            let other = *self.namespace_of.get(entry.container.as_str())?;
            let confirmed = self
                .host_ends
                .get(&(protocol, entry.container_port, local.port()))
                .is_some_and(|namespaces| namespaces.contains(&other))
                || (protocol == Protocol::Udp
                    && self
                        .listening
                        .contains(&(other, protocol, entry.container_port)));
            (on_it && other != index && confirmed)
                .then(|| (entry.container.clone(), entry.container_port, true))
        })
    }
}

/// The observed links between `namespaces`, as [`Links`] says them.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
pub(crate) fn observed(namespaces: &[Namespace], addresses: &Addresses) -> Vec<Link> {
    let tables = Tables::of(namespaces, addresses);
    let mut counted: BTreeMap<(String, String, u16, Protocol, bool), u32> = BTreeMap::new();
    for (index, namespace) in namespaces.iter().enumerate() {
        for socket in &namespace.sockets {
            if socket.state != State::Established {
                continue;
            }
            let (local, remote) = (canonical(socket.local), canonical(socket.remote));
            let Some((to, port, via_host_port)) =
                tables.target(index, socket.protocol, (local, remote))
            else {
                continue;
            };
            let Some(&other) = tables.namespace_of.get(to.as_str()) else {
                continue;
            };
            // Counted from the client's side: the server is the side whose
            // port it listens on, else the lower port's (else the first).
            // (One through a published port is a client's: its server's
            // side reaches no container.)
            let client = via_host_port
                || tables.listening.contains(&(other, socket.protocol, port))
                || (!tables
                    .listening
                    .contains(&(index, socket.protocol, local.port()))
                    && (port, other) < (local.port(), index));
            if client {
                *counted
                    .entry((
                        namespace.owner.clone(),
                        to,
                        port,
                        socket.protocol,
                        via_host_port,
                    ))
                    .or_default() += 1;
            }
        }
    }
    counted
        .into_iter()
        .map(
            |((from, to, port, protocol, via_host_port), connections)| Link {
                from,
                to,
                to_missing: false,
                kind: LinkKind::Observed,
                port: Some(port),
                protocol: Some(protocol.as_str()),
                condition: None,
                connections,
                via_host_port,
                refused: false,
            },
        )
        .collect()
}

/// Each of `addresses`' published entries' connections from outside the
/// containers (see [`Entry::connections`]), in their order: its
/// container's TCP sockets up on its container port whose other end is no
/// container's lease, not loopback (the container's own), and not a
/// container's connection the host's NAT carried in (the client's socket,
/// to a host address with the source port kept, says so). A container port
/// published on several host ports gives each its connections: which host
/// port an outside client came by, the container's table doesn't say.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the tables are read on Linux; tested everywhere")
)]
pub(crate) fn entry_connections(namespaces: &[Namespace], addresses: &Addresses) -> Vec<u32> {
    let tables = Tables::of(namespaces, addresses);
    addresses
        .published
        .iter()
        .map(|entry| {
            if entry.protocol != Protocol::Tcp.as_str() {
                return 0;
            }
            let Some(&index) = tables.namespace_of.get(entry.container.as_str()) else {
                return 0;
            };
            // A container's connection the host's NAT carried in, through
            // any host port publishing this container port: its client's
            // socket to that host port, from this source port.
            let through_host = |source: u16| {
                addresses.published.iter().any(|other| {
                    other.container == entry.container
                        && other.container_port == entry.container_port
                        && other.protocol == entry.protocol
                        && tables
                            .host_ends
                            .contains_key(&(Protocol::Tcp, source, other.host_port))
                })
            };
            let outside = namespaces[index].sockets.iter().filter(|socket| {
                let (local, remote) = (canonical(socket.local), canonical(socket.remote));
                socket.protocol == Protocol::Tcp
                    && socket.state == State::Established
                    && local.port() == entry.container_port
                    && !remote.ip().is_loopback()
                    && !addresses.owners.contains_key(&remote.ip())
                    && !(addresses.host.contains(&remote.ip()) && through_host(remote.port()))
            });
            u32::try_from(outside.count()).unwrap_or(u32::MAX)
        })
        .collect()
}

/// Every container, its links and where the map starts: see [`Links`].
/// Observed links need root's networks (each peer named by its lease):
/// without them they're said unavailable, and the rest stands.
///
/// # Errors
///
/// The container store can't be read.
pub fn links(engine: &Engine) -> Result<Links> {
    let summaries = super::summaries(engine, true)?;
    let mut nodes: Vec<Node> = summaries.iter().map(node).collect();
    let declaring: Vec<Declaring<'_>> = nodes
        .iter()
        .zip(&summaries)
        .map(|(node, summary)| Declaring {
            node,
            depends_on: summary
                .record
                .labels
                .as_map()
                .get(DEPENDS_ON_LABEL)
                .map(String::as_str),
        })
        .collect();
    let (mut links, missing) = declared(&declaring);
    let mut entries: Vec<Entry> = summaries
        .iter()
        .filter(|summary| {
            matches!(
                summary.state,
                ContainerState::Running { .. } | ContainerState::Paused
            )
        })
        .flat_map(|summary| {
            summary.record.ports.iter().map(|port| Entry {
                container: summary.record.id.as_str().to_owned(),
                host_ip: port.host_ip,
                host_port: port.host_port,
                container_port: port.container_port,
                protocol: port.protocol.clone(),
                connections: 0,
            })
        })
        .collect();
    let read = read_namespaces(engine, &summaries, &entries);
    links.extend(read.links);
    for (entry, connections) in entries.iter_mut().zip(&read.entry_connections) {
        entry.connections = *connections;
    }
    for node in &mut nodes {
        if let Some(shared) = read.shared.get(&node.id) {
            node.shares_network_with.clone_from(shared);
        }
    }
    Ok(Links {
        version: LINKS_VERSION,
        containers: nodes,
        missing,
        entries,
        links,
        observed: read.observed,
        unread: read.unread,
    })
}

/// A summary as the map's node.
fn node(summary: &super::Summary) -> Node {
    let labels = summary.record.labels.as_map();
    let exit_code = match &summary.state {
        ContainerState::Exited { code, .. } | ContainerState::Restarting { code, .. } => *code,
        _ => None,
    };
    Node {
        id: summary.record.id.as_str().to_owned(),
        name: summary.record.name.as_str().to_owned(),
        serial: summary.record.serial.to_string(),
        memory_max: summary.record.config.resources.memory_max,
        started_at: summary.record.started.map(crate::events::rfc3339),
        project: labels.get(PROJECT_LABEL).cloned(),
        service: labels.get(SERVICE_LABEL).cloned(),
        state: summary.state.word(),
        health: summary.health.as_ref().map(|health| health.status),
        exit_code,
        one_off: labels
            .get(ONE_OFF_LABEL)
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        shares_network_with: Vec::new(),
        up: matches!(
            summary.state,
            ContainerState::Running { .. } | ContainerState::Paused | ContainerState::Stopping
        ),
        ended: matches!(summary.state, ContainerState::Exited { .. }),
    }
}

/// What the tables say: the observed links, the namespaces containers
/// share, whether the links could be looked for, and what wasn't read.
struct Read {
    links: Vec<Link>,
    /// Each entry's connections from outside, in the entries' order.
    entry_connections: Vec<u32>,
    shared: BTreeMap<String, Vec<String>>,
    observed: Observed,
    unread: Vec<Unread>,
}

impl Read {
    /// Nothing read, observed as `observed` says.
    fn nothing(observed: Observed) -> Self {
        Self {
            links: Vec::new(),
            entry_connections: Vec::new(),
            shared: BTreeMap::new(),
            observed,
            unread: Vec::new(),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn read_namespaces(_: &Engine, _: &[super::Summary], _: &[Entry]) -> Read {
    Read::nothing(Observed::unavailable(
        "not_linux",
        "a container's sockets are read on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn read_namespaces(engine: &Engine, summaries: &[super::Summary], entries: &[Entry]) -> Read {
    linux::read_namespaces(engine, summaries, entries)
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests;
