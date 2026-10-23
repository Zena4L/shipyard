//! A run's place on a bridge network: its lease (an address, held while it
//! lives), the host ports it publishes, its veth pair once connected, and
//! what's undone when it ends.

#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use super::firewall::{
    ensure_bridge_table, ensure_firewall, open_routing, warn_about_other_firewalls,
};
#[cfg(target_os = "linux")]
use super::{Joined, NetworkPlan, networks, open_network};
#[cfg(target_os = "linux")]
use crate::Engine;
#[cfg(target_os = "linux")]
use crate::error::{Context as _, Result, bail};
use crate::report::{Message, Reporter, WarningKind, warn};

/// Why `-p` can't go on `config`'s network: an internal one, or an IPv6
/// host address on a network without IPv6.
#[cfg(target_os = "linux")]
pub(super) fn check_ports(
    config: &shipyard_network::store::NetworkConfig,
    ports: &[shipyard_network::ports::PortRequest],
) -> Result<()> {
    let name = &config.name;
    if config.internal && !ports.is_empty() {
        bail!(
            InvalidRequest,
            "--publish/--publish-all: the network {name} is internal: nothing on it can be \
             reached from outside"
        );
    }
    if config.subnet6.is_none()
        && let Some(ip) = ports
            .iter()
            .find_map(|port| port.host_ip.filter(std::net::IpAddr::is_ipv6))
    {
        bail!(
            InvalidRequest,
            "--publish: {ip} is an IPv6 address, and the network {name} has no IPv6 (make one \
             with `shipyard network create --ipv6`)"
        );
    }
    Ok(())
}

/// Sweeps `network`'s leases of runs that are gone, and what they left:
/// their published ports, their veth pair (the kernel deletes one with its
/// namespace, but not at once), and the connections the kernel remembers for
/// their address. What can't be cleaned up is a warning: the address is
/// free all the same.
#[cfg(target_os = "linux")]
pub(super) fn sweep_gone(
    network: &shipyard_network::store::LockedNetwork,
    nft: &mut shipyard_network::nft::Nftables,
    reporter: &dyn Reporter,
) -> Result<()> {
    for gone in network.sweep()? {
        release_lease(
            network.config(),
            &gone,
            nft,
            true,
            "a container that's gone",
            reporter,
        );
    }
    Ok(())
}

/// Lets go of a detached container's lease on the network `name`, `hold`
/// (its locked lease file, as its monitor kept it): under the network's
/// lock, the hold is closed (its lock goes with it), then the network's
/// leases of runs that are gone are swept, this one among them (its ports
/// unpublished, its veth deleted, its connections forgotten, its file
/// removed). If the network can't be locked (it's gone, or busy), the hold
/// is closed all the same: the next run on it sweeps the lease.
///
/// # Errors
///
/// The network couldn't be locked, or swept.
#[cfg(target_os = "linux")]
pub(crate) fn release_lease_hold(
    engine: &Engine,
    name: &str,
    hold: std::os::fd::OwnedFd,
) -> Result<()> {
    let locked = networks(engine).and_then(|networks| Ok(networks.lock(name)?));
    drop(hold);
    let locked = locked?;
    let mut nft = shipyard_network::nft::Nftables::new()?;
    sweep_gone(&locked, &mut nft, engine.reporter().as_ref())
}

/// Publishes `ports`. The ports maps are every network's: a key can be
/// taken by a run that's gone (swept on another network, or a firewall
/// rebuilt from its lease as it was released); then it's reclaimed. A key
/// taken by a live run is an error (it can't be: the host port is held).
#[cfg(target_os = "linux")]
fn publish(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    ports: &[shipyard_network::nft::PortRule],
) -> Result<()> {
    match nft.publish(ports) {
        Err(error) if error.errno() == Some(nix::errno::Errno::EEXIST) => {}
        other => return Ok(other?),
    }
    let live = networks.live_port_rules()?;
    let mut stale = Vec::new();
    let mut todo = Vec::new();
    for (port, now) in ports.iter().zip(nft.published(ports)?) {
        match now {
            // Published already (the rest of the request failed).
            Some(now) if now == *port => {}
            Some(now) if live.contains(&now) => {
                bail!(
                    InUse,
                    "port {} is published already, by another container",
                    port.published
                );
            }
            Some(now) => {
                stale.push(now);
                todo.push(*port);
            }
            None => todo.push(*port),
        }
    }
    nft.unpublish(&stale)?;
    Ok(nft.publish(&todo)?)
}

/// Undoes what a run's `lease` on `config`'s network made, in order: its
/// published ports (so no new connection reaches it), its veth pair (if
/// `connected`, and only as a port of the network's bridge: a device of its
/// name elsewhere is another run's), and the connections the kernel
/// remembers for its address. `whose` names the run in warnings.
#[cfg(target_os = "linux")]
pub(super) fn release_lease(
    config: &shipyard_network::store::NetworkConfig,
    lease: &shipyard_network::store::Lease,
    nft: &mut shipyard_network::nft::Nftables,
    connected: bool,
    whose: &str,
    reporter: &dyn Reporter,
) {
    let left = |message: String| warn(reporter, WarningKind::LeftForLater, message);
    let ports = lease.port_rules();
    if !ports.is_empty()
        && let Err(error) = nft.unpublish(&ports)
    {
        left(format!("unpublishing the ports of {whose}: {error}"));
    }
    // Only where the bridge table is (a kernel without bridge filtering
    // has none).
    if matches!(
        nft.bridge_ruleset(),
        Ok(shipyard_network::nft::Ruleset::Current
            | shipyard_network::nft::Ruleset::Outdated
            | shipyard_network::nft::Ruleset::Newer)
    ) && let Err(error) = nft.unguard(&lease.port_guard())
    {
        left(format!("forgetting the port of {whose}: {error}"));
    }
    if connected && let Err(error) = shipyard_network::attach::disconnect(config, lease) {
        left(format!(
            "removing the veth {} of {whose}: {error}",
            lease.host_veth
        ));
    }
    for ip in lease.addresses() {
        if let Err(error) = shipyard_network::conntrack::flush_address(ip) {
            left(format!("forgetting the connections of {whose}: {error}"));
        }
    }
}

/// A run's place on a network: its lease (its address, held while the run
/// lives), the host ports it publishes (held too), and once it's connected,
/// its veth pair. Released when the run ends (on drop).
#[cfg(target_os = "linux")]
pub(super) struct Attachment {
    pub(super) config: shipyard_network::store::NetworkConfig,
    lease: Option<shipyard_network::store::HeldLease>,
    /// Which of the container's default routes go through it.
    pub(super) routes: shipyard_network::attach::DefaultRoutes,
    /// Whether its veth pair was made: only then is the device of its name
    /// this run's to delete (another run's could have the same name).
    connected: std::cell::Cell<bool>,
    /// The host ports it publishes, held: no other program can take them.
    held: Vec<shipyard_network::ports::HeldPort>,
    /// Where what its end can't undo is reported.
    reporter: Arc<dyn Reporter>,
}

#[cfg(target_os = "linux")]
impl Attachment {
    /// The host ports it publishes, as a container's record keeps them: one
    /// on every address is on `0.0.0.0`, and on `::` too if it's held on
    /// IPv6 as well and its lease has an IPv6 address (only then is it
    /// forwarded to the container: `Lease::port_rules`).
    pub(crate) fn bound(&self) -> impl Iterator<Item = crate::containers::BoundPort> {
        let has_ipv6 = self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.lease().ip6.is_some());
        self.held.iter().flat_map(move |port| {
            let ipv6 = (has_ipv6 && port.also_ipv6())
                .then(|| bound(&port.published, std::net::Ipv6Addr::UNSPECIFIED.into()));
            std::iter::once(bound(
                &port.published,
                std::net::Ipv4Addr::UNSPECIFIED.into(),
            ))
            .chain(ipv6)
        })
    }
}

/// A published port as a container's record keeps it, on its host address,
/// else on `every` (an unspecified address).
#[cfg(target_os = "linux")]
pub(crate) fn bound(
    port: &shipyard_network::ports::Published,
    every: std::net::IpAddr,
) -> crate::containers::BoundPort {
    crate::containers::BoundPort::new(
        port.host_ip.unwrap_or(every),
        port.host_port,
        port.container_port,
        port.protocol.name(),
    )
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("config", &self.config)
            .field("lease", &self.lease)
            .field("routes", &self.routes)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

/// Whose a lease is: what of a run [`Attachment::take`] needs (a start's
/// plan has it; a `network connect` reads it from the container's leases
/// and record).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
pub(super) struct TakeFor<'a> {
    /// The run's ID (each start's own).
    pub(super) run: &'a super::RunId,
    /// The container's hostname.
    pub(super) hostname: &'a str,
    /// Its name, its first name on the network.
    pub(super) name: Option<&'a str>,
    /// Its ID, a container's (a run with no record has none).
    pub(super) container: Option<&'a crate::containers::ContainerId>,
}

#[cfg(target_os = "linux")]
impl<'a> TakeFor<'a> {
    /// What `plan`'s run is.
    fn of(plan: &'a NetworkPlan) -> Self {
        Self {
            run: &plan.run,
            hostname: &plan.hostname,
            name: plan.name.as_deref(),
            container: plan.container.as_ref(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Attachment {
    /// Takes an address on `joined` for `whose` container, its `n`th
    /// network (0 first: its interface is `eth{n}`), publishing `ports` (the network
    /// with the IPv4 default route has them all; `routes` says which go
    /// through this one): the host's ports are held first; then, under the
    /// network's lock (its alone: a run never holds two networks' locks,
    /// so two can't wait for each other), the leases of runs that are
    /// gone are swept (with what they left), the network is made ready
    /// (its bridge, the firewall), the address and ports are taken, and
    /// the network's DNS server is started if it has one and it isn't
    /// running (a server that can't start fails the run).
    pub(super) fn take(
        engine: &Engine,
        whose: &TakeFor<'_>,
        joined: &Joined,
        n: usize,
        routes: shipyard_network::attach::DefaultRoutes,
        ports: &[shipyard_network::ports::PortRequest],
    ) -> Result<Attachment> {
        use shipyard_network::store::LeaseRequest;
        use shipyard_network::{attach, nft::Nftables};
        let name = &joined.name;
        let networks = networks(engine)?;
        let config = open_network(engine, &networks, name)?;
        check_ports(&config, ports)?;
        let held = shipyard_network::ports::hold(ports, engine.paths().runtime_root().ok())?;
        let network = networks.lock(name)?;
        let mut nft = Nftables::new()?;
        sweep_gone(&network, &mut nft, engine.reporter().as_ref())?;
        let config = network.config().clone();
        // Again: it may have been made again since it was looked at.
        check_ports(&config, ports)?;
        attach::ensure_bridge(&config).with_context(|| format!("the network {name}"))?;
        ensure_firewall(&networks, &mut nft, &config, engine.reporter().as_ref())
            .with_context(|| format!("the network {name}'s firewall rules"))?;
        let guarded = ensure_bridge_table(&networks, &mut nft, engine.reporter().as_ref());
        if !config.internal {
            open_routing(&networks, &mut nft, &config)?;
        }
        let lease = network.allocate(&LeaseRequest {
            run: whose.run.as_str().to_owned(),
            hostname: whose.hostname.to_owned(),
            host_veth: whose.run.host_veth(n),
            ip: joined.ip,
            ip6: joined.ip6,
            mac: joined.mac,
            ports: held.iter().map(|port| port.published).collect(),
            name: whose.name.map(str::to_owned),
            aliases: joined.aliases.clone(),
            interface: shipyard_network::store::interface_name(n),
            container: whose.container.map(|id| id.as_str().to_owned()),
        })?;
        // From here, dropping it undoes it all.
        let attachment = Attachment {
            config,
            lease: Some(lease),
            routes,
            connected: std::cell::Cell::new(false),
            held,
            reporter: Arc::clone(engine.reporter()),
        };
        if let Some(lease) = &attachment.lease {
            if !lease.lease().ports.is_empty() {
                attach::allow_local_clients(&attachment.config)?;
            }
            publish(&networks, &mut nft, &lease.lease().port_rules())?;
            announce(ports, &lease.lease().ports, engine.reporter().as_ref());
            // Before its veth exists: from its first frame, it sends only as
            // itself.
            if guarded && let Err(error) = nft.guard(&lease.lease().port_guard()) {
                engine.warn(
                    WarningKind::AntiSpoofing,
                    format!("the container can send as another on the network {name}: {error}"),
                );
            }
        }
        let has_dns = attachment.config.has_dns();
        if has_dns {
            // Under the network's lock, with the lease held: see `ensure`.
            super::dns::ensure(engine.paths(), &networks, &attachment.config)?;
        }
        drop(network);
        // Out of the network's lock: other runs needn't wait for it.
        warn_about_other_firewalls(&mut nft, &attachment.config, engine.reporter().as_ref());
        Ok(attachment)
    }

    /// The container's addresses: its IPv4 one, and its IPv6 one if the
    /// network has IPv6.
    pub(super) fn addresses(&self) -> Vec<std::net::IpAddr> {
        self.lease
            .as_ref()
            .map_or_else(Vec::new, |lease| lease.lease().addresses().collect())
    }

    /// Connects the container whose network namespace is `netns`: the
    /// runtime's network hook.
    pub(super) fn connect(&self, netns: std::os::fd::BorrowedFd<'_>) -> Result<(), String> {
        let Some(lease) = &self.lease else {
            return Err("the run's lease was given back already".to_owned());
        };
        connect_to(&self.config, lease.lease(), netns, self.routes)?;
        self.connected.set(true);
        Ok(())
    }

    /// Its lease's lock (for a monitor to hold), while it has its lease.
    pub(super) fn lease_hold(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.lease
            .as_ref()
            .map(shipyard_network::store::HeldLease::hold)
    }

    /// Adds its lease and its ports' sockets to `holds`: what a detached
    /// container's monitor keeps (see [`into_connect`](Self::into_connect)).
    pub(super) fn holds<'a>(&'a self, holds: &mut crate::run::holds::Holds<'a>) {
        use crate::run::holds::HoldKind;
        if let Some(lease) = &self.lease {
            holds.add(
                HoldKind::Lease {
                    network: self.config.name.clone(),
                },
                lease.hold(),
            );
        }
        for port in &self.held {
            for socket in port.sockets() {
                holds.add(HoldKind::Port, socket);
            }
        }
    }

    /// Gives the lease and ports up to whoever holds copies of
    /// [`holds`](Self::holds) (this process's are closed, never unlocked:
    /// nothing is released), keeping what connecting the container needs.
    pub(super) fn into_connect(mut self) -> Option<Connect> {
        let lease = self.lease.take()?;
        let connect = Connect {
            config: self.config.clone(),
            lease: lease.lease().clone(),
            routes: self.routes,
        };
        lease.give_up();
        Some(connect)
    }
}

/// What connecting a detached container to one of its networks needs, once
/// its lease is held by its monitor: the network, the lease, its routes.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct Connect {
    config: shipyard_network::store::NetworkConfig,
    lease: shipyard_network::store::Lease,
    routes: shipyard_network::attach::DefaultRoutes,
}

#[cfg(target_os = "linux")]
impl Connect {
    /// Connects the container whose network namespace is `netns`, as
    /// [`Attachment::connect`] does.
    pub(crate) fn connect(&self, netns: std::os::fd::BorrowedFd<'_>) -> Result<(), String> {
        connect_to(&self.config, &self.lease, netns, self.routes)
    }
}

/// Connects the container whose network namespace is `netns` to `config`'s
/// network, with `lease`: what a run's network hook and a detached
/// container's [`Connect`] both do.
#[cfg(target_os = "linux")]
fn connect_to(
    config: &shipyard_network::store::NetworkConfig,
    lease: &shipyard_network::store::Lease,
    netns: std::os::fd::BorrowedFd<'_>,
    routes: shipyard_network::attach::DefaultRoutes,
) -> Result<(), String> {
    shipyard_network::attach::connect(config, lease, netns, routes).map_err(|error| {
        format!(
            "connecting the container to the network {}: {error}",
            config.name
        )
    })
}

/// Takes `plan`'s bridge networks, `joined`, in order (see
/// [`Attachment::take`]). The container's default route, and the ports it
/// publishes (`ports`), go through [`route_network`]. If one can't be
/// taken, those taken already are given back.
#[cfg(target_os = "linux")]
pub(super) fn take_all(
    engine: &Engine,
    plan: &NetworkPlan,
    joined: &[Joined],
    ports: &[shipyard_network::ports::PortRequest],
) -> Result<Vec<Attachment>> {
    let store = networks(engine)?;
    let configs: Vec<Option<shipyard_network::store::NetworkConfig>> = joined
        .iter()
        .map(|network| store.get(&network.name).ok().flatten())
        .collect();
    let internal: Vec<bool> = configs
        .iter()
        .map(|config| config.as_ref().is_some_and(|config| config.internal))
        .collect();
    let ipv6: Vec<bool> = configs
        .iter()
        .map(|config| {
            config
                .as_ref()
                .is_some_and(|config| config.subnet6.is_some())
        })
        .collect();
    let route = route_network(&internal);
    let route6 = route_network6(&internal, &ipv6);
    joined
        .iter()
        .enumerate()
        .map(|(n, network)| {
            let routes = shipyard_network::attach::DefaultRoutes {
                ipv4: n == route,
                ipv6: route6 == Some(n),
            };
            let ports = if routes.ipv4 { ports } else { &[] };
            Attachment::take(engine, &TakeFor::of(plan), network, n, routes, ports)
        })
        .collect()
}

/// Which of a container's networks (whether each is `internal`, in order)
/// its default route goes through: the first that isn't internal (an
/// internal network leads nowhere), else the first.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn route_network(internal: &[bool]) -> usize {
    internal.iter().position(|internal| !internal).unwrap_or(0)
}

/// Which network its IPv6 default route goes through, as
/// [`route_network`] chooses, among those with IPv6 (`ipv6`): none if
/// none has.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
fn route_network6(internal: &[bool], ipv6: &[bool]) -> Option<usize> {
    let with_ipv6 = || (0..ipv6.len()).filter(|&n| ipv6[n]);
    with_ipv6()
        .find(|&n| !internal.get(n).copied().unwrap_or(false))
        .or_else(|| with_ipv6().next())
}

/// The resolver for a container on `networks` (in order): the DNS server
/// of the first that has one and a way out, else of the first that has one
/// (as Docker's, the default network has none; a server forwards for a
/// container with a way out elsewhere); `None` for the host's resolvers.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
pub(crate) fn resolver(
    networks: &[&shipyard_network::store::NetworkConfig],
) -> Option<std::net::IpAddr> {
    let with_dns = || networks.iter().filter(|config| config.has_dns());
    with_dns()
        .find(|config| !config.internal)
        .or_else(|| with_dns().next())
        .map(|config| config.gateway)
}

/// When the run ends: what its lease made is undone ([`release_lease`]),
/// then its address is given back, then its host ports.
#[cfg(target_os = "linux")]
impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            match shipyard_network::nft::Nftables::new() {
                Ok(mut nft) => {
                    release_lease(
                        &self.config,
                        lease.lease(),
                        &mut nft,
                        self.connected.get(),
                        "the container",
                        self.reporter.as_ref(),
                    );
                }
                Err(error) => {
                    warn(
                        self.reporter.as_ref(),
                        WarningKind::LeftForLater,
                        format!("cleaning up the container's network: {error}"),
                    );
                }
            }
            lease.release();
        }
    }
}

/// Reports, as notices, the ports of `published` whose host port the kernel chose (in
/// `requests`, those without one): the user has no other way to know them.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "networks are Linux's")
)]
pub(super) fn announce(
    requests: &[shipyard_network::ports::PortRequest],
    published: &[shipyard_network::ports::Published],
    reporter: &dyn Reporter,
) {
    for (request, published) in requests.iter().zip(published) {
        if request.host_port.is_none() {
            reporter.report(Message::Notice(format!("published {published}")));
        }
    }
}

/// The ports a run publishes: `-p`'s (`ports`), then with `-P`
/// (`publish_all`), the image's `exposed` ports that `-p` doesn't already
/// publish, on free host ports, as Docker's. One that can't be read is
/// skipped, with a warning.
pub(super) fn port_requests(
    ports: &[shipyard_network::ports::PortRequest],
    publish_all: bool,
    exposed: &[String],
    reporter: &dyn Reporter,
) -> Vec<shipyard_network::ports::PortRequest> {
    let mut requests = ports.to_vec();
    if publish_all {
        for key in exposed {
            match shipyard_network::ports::parse_exposed(key) {
                Some(request)
                    if !ports.iter().any(|given| {
                        given.container_port == request.container_port
                            && given.protocol == request.protocol
                    }) =>
                {
                    requests.push(request);
                }
                Some(_) => {}
                None => warn(
                    reporter,
                    WarningKind::ExposedPortUnreadable,
                    format!("--publish-all: the image's exposed port {key:?} can't be read"),
                ),
            }
        }
    }
    requests
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn ipv6s_route_goes_through_a_network_with_ipv6() {
        // The first with IPv6 and a way out...
        assert_eq!(
            route_network6(&[false, true, false], &[false, true, true]),
            Some(2)
        );
        // ...else the first with IPv6...
        assert_eq!(route_network6(&[false, true], &[false, true]), Some(1));
        // ...and none without.
        assert_eq!(route_network6(&[false, false], &[false, false]), None);
        // IPv4's own, beside: the first network with a way out.
        assert_eq!(route_network(&[false, true, false]), 0);
    }

    #[test]
    fn publish_all_adds_the_exposed_ports_p_doesnt_publish() {
        use shipyard_network::ports::{Protocol, parse_publish};
        let given = parse_publish("8080:80").unwrap();
        let exposed = ["80/tcp", "80/udp", "443", "nonsense"].map(String::from);
        let ports = |requests: Vec<shipyard_network::ports::PortRequest>| -> Vec<_> {
            requests
                .iter()
                .map(|request| (request.host_port, request.container_port, request.protocol))
                .collect()
        };
        let reporter = crate::report::Collect::default();
        // Without -P, -p's alone.
        assert_eq!(
            ports(port_requests(&given, false, &exposed, &reporter)),
            [(Some(8080), 80, Protocol::Tcp)]
        );
        // With it, the rest on free ports: not 80/tcp again (-p has it),
        // and a key that can't be read skipped.
        assert_eq!(
            ports(port_requests(&given, true, &exposed, &reporter)),
            [
                (Some(8080), 80, Protocol::Tcp),
                (None, 80, Protocol::Udp),
                (None, 443, Protocol::Tcp),
            ]
        );
        let warned: Vec<_> = reporter
            .warnings()
            .iter()
            .map(|warning| warning.kind)
            .collect();
        assert_eq!(warned, [WarningKind::ExposedPortUnreadable], "\"nonsense\"");
    }

    fn config(name: &str, internal: bool, gateway: &str) -> shipyard_network::store::NetworkConfig {
        shipyard_network::store::NetworkConfig {
            name: name.into(),
            subnet: "10.0.0.0/8".parse().unwrap(),
            gateway: gateway.parse().unwrap(),
            bridge: shipyard_network::store::bridge_name(name),
            internal,
            created: 0,
            subnet6: None,
            gateway6: None,
            isolate_host: false,
            conntrack_limit: false,
        }
    }

    #[test]
    fn the_route_goes_through_the_first_network_with_a_way_out() {
        assert_eq!(route_network(&[false, false]), 0);
        assert_eq!(route_network(&[true, false, false]), 1);
        assert_eq!(route_network(&[true, true]), 0);
    }

    #[test]
    fn the_resolver_is_the_first_server_with_a_way_out() {
        let bridge = config("bridge", false, "10.88.0.1");
        let quiet = config("quiet", true, "10.89.0.1");
        let out = config("out", false, "10.90.0.1");
        let ip = |text: &str| Some(text.parse().unwrap());
        assert_eq!(resolver(&[&quiet, &out]), ip("10.90.0.1"));
        // The default network has no server; an internal one's forwards for
        // a container with a way out elsewhere.
        assert_eq!(resolver(&[&bridge, &quiet]), ip("10.89.0.1"));
        assert_eq!(resolver(&[&bridge]), None);
    }
}
