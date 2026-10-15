//! Putting a container on a network: the network's bridge on the host, and
//! a veth pair from it into the container's network namespace.
//!
//! ```text
//!   host                                   container (its network namespace)
//!   shipyard0 (10.88.0.1/16, the gateway)
//!     └─ shy1a2b3c4d ══════ veth ══════════ eth0 (10.88.0.2/16)
//!                                           default via 10.88.0.1
//! ```
//!
//! A bridge is a switch in the kernel: every veth end attached to it is a
//! port, and frames go between ports by hardware address. The bridge has an
//! address of its own on the network, the gateway, so the host is on it too.
//! The container's end is born in its namespace already named `eth0` (or
//! `eth{n}` on its n-th network; one request, `IFLA_NET_NS_FD` on the peer):
//! it never exists on the host.

use std::net::IpAddr;
use std::os::fd::BorrowedFd;

use nix::errno::Errno;
use thiserror::Error;

use crate::ipnet::{Family, IpNet};
use crate::route::{LinkChange, Route, Rtnetlink, RtnetlinkError};
use crate::store::{Lease, NetworkConfig};

#[cfg(test)]
use crate::store::{CONTAINER_INTERFACE, interface_name};

/// What went wrong putting a container on a network.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AttachError {
    /// A request the kernel refused.
    #[error(transparent)]
    Netlink(#[from] RtnetlinkError),
    /// A device that's in the way.
    #[error("{0}")]
    Device(String),
    /// A kernel setting that couldn't be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

/// Makes `config`'s bridge ready: made if it isn't there (after a reboot,
/// or the first time), with the gateway's address, and up. An internal
/// network's bridge doesn't forward (IPv4): what a container sends past it
/// stays on it even without the firewall's rules (after a `nft flush
/// ruleset`, say).
pub fn ensure_bridge(config: &NetworkConfig) -> Result<(), AttachError> {
    let mut host = Rtnetlink::new()?;
    let bridge = if let Some(link) = host.link(&config.bridge)? {
        link
    } else {
        match host.create_bridge(&config.bridge) {
            // Made meanwhile, by `network create` or another run.
            Err(error) if error.errno() == Some(Errno::EEXIST) => {}
            other => other?,
        }
        host.link(&config.bridge)?.ok_or_else(|| {
            AttachError::Device(format!(
                "the bridge {} went as soon as it was made",
                config.bridge
            ))
        })?
    };
    if bridge.kind.as_deref() != Some("bridge") {
        return Err(AttachError::Device(format!(
            "the network {} needs the device {} for its bridge, and it's there already, but \
             isn't a bridge",
            config.name, config.bridge
        )));
    }
    let gateway = config.gateway_net();
    let has_gateway = host
        .addresses(Some(bridge.index))?
        .iter()
        .any(|address| address.net == gateway);
    if !has_gateway {
        match host.add_address(bridge.index, gateway) {
            // Added since the listing, by another run on another network.
            Err(error) if error.errno() == Some(Errno::EEXIST) => {}
            other => other?,
        }
    }
    // The bridge's IPv6 settings, before it's up. Never a router
    // advertisement from a container: one would make it the host's IPv6
    // router (CVE-2020-10749's kind).
    if let Some(gateway6) = config.gateway6_net() {
        // A host may have IPv6 off on new interfaces (`default`); the
        // bridge has it on.
        set_ipv6(&config.bridge, "disable_ipv6", "0")?;
        set_ipv6(&config.bridge, "accept_ra", "0")?;
        // No duplicate address detection on it: the host solicits a
        // container's address from the bridge's link-local one, for a packet
        // it forwards, and while that's "tentative" (a second or two after
        // the bridge is made), it can't, and connections to a container's
        // published port time out.
        set_ipv6(&config.bridge, "accept_dad", "0")?;
        let has_gateway6 = host
            .addresses(Some(bridge.index))?
            .iter()
            .any(|address| address.net == gateway6);
        if !has_gateway6 {
            match host.add_address(bridge.index, gateway6) {
                Err(error) if error.errno() == Some(Errno::EEXIST) => {}
                other => other?,
            }
        }
    } else {
        // A network without IPv6 has none: not even a link-local address, or
        // its containers' router advertisements.
        set_ipv6(&config.bridge, "disable_ipv6", "1")?;
    }
    // A hardware address of its own, set: otherwise a bridge takes its
    // ports' lowest, and changes it when that port goes (a container
    // leaving), and the other containers' ARP entries for the gateway are
    // wrong for seconds. (Docker sets one too.) It's `02:42` and the
    // gateway's address, as the containers' are theirs.
    let mac = crate::store::mac_for(config.gateway);
    if bridge.mac.as_deref() != Some(&mac[..]) {
        host.change(bridge.index, LinkChange::Address(mac))?;
    }
    if !bridge.is_up() {
        host.change(bridge.index, LinkChange::Up(true))?;
    }
    if config.internal {
        stop_forwarding(&config.bridge)?;
    }
    Ok(())
}

/// Lets the host's own clients reach ports published on `config`'s network
/// through 127.0.0.1 (`route_localnet`): the kernel only routes packets from
/// 127.0.0.0/8 out of an interface that allows it. Only for a network that
/// publishes ports: it's what lets a container reach the host's loopback
/// the other way, which the firewall's guard stops (CVE-2020-8558), so a
/// bridge has it only where it's needed.
pub fn allow_local_clients(config: &NetworkConfig) -> Result<(), AttachError> {
    // The name was checked as an interface's: no `/`, no `..`.
    write_setting(
        &format!("/proc/sys/net/ipv4/conf/{}/route_localnet", config.bridge),
        "1",
    )
}

/// Turns IPv4 forwarding off for what comes in on `bridge` (an internal
/// network's). Turning the host's forwarding on turns every interface's on,
/// so it's done again after (see `turn_forwarding_on`'s callers).
pub fn stop_forwarding(bridge: &str) -> Result<(), AttachError> {
    write_setting(&format!("/proc/sys/net/ipv4/conf/{bridge}/forwarding"), "0")
}

/// Where `family`'s forwarding is set, in this network namespace.
fn forwarding_setting(family: Family) -> &'static str {
    match family {
        Family::V4 => "/proc/sys/net/ipv4/ip_forward",
        Family::V6 => "/proc/sys/net/ipv6/conf/all/forwarding",
    }
}

/// Whether this network namespace forwards `family`: `None` for IPv6 on a
/// kernel without it (`ipv6.disable=1`).
pub fn forwarding(family: Family) -> Result<Option<bool>, AttachError> {
    let path = forwarding_setting(family);
    match std::fs::read_to_string(path) {
        Ok(now) => Ok(Some(now.trim() == "1")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && family == Family::V6 => {
            Ok(None)
        }
        Err(source) => Err(AttachError::Io {
            context: format!("reading {path}"),
            source,
        }),
    }
}

/// Turns `family`'s forwarding on in this network namespace (Docker does
/// the same: a container's packets only get out if the host routes them).
/// The caller closes routing to all but containers first ([`forwarding`]
/// says whether it was off), so the host is never an open router, even if
/// this run goes no further.
///
/// For IPv6, the interfaces whose `accept_ra` is 1 are set to 2 first, and
/// returned: a host that forwards stops taking router advertisements on an
/// interface with 1, and one that learns its routes from them (SLAAC)
/// would lose its IPv6 default route; with 2, it keeps taking them. So do
/// interfaces made later (the `default` entry, from 1 to 2 too; Shipyard's
/// bridges and containers take none, whatever it says). 0 (none taken) and
/// 2 are left as they are.
pub fn turn_forwarding_on(family: Family) -> Result<Vec<String>, AttachError> {
    let mut kept = Vec::new();
    if family == Family::V6 {
        const CONF: &str = "/proc/sys/net/ipv6/conf";
        let interfaces = std::fs::read_dir(CONF).map_err(|source| AttachError::Io {
            context: format!("reading {CONF}"),
            source,
        })?;
        for entry in interfaces.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !learns_routes(&name) {
                continue;
            }
            let accept_ra = entry.path().join("accept_ra");
            if std::fs::read_to_string(&accept_ra).is_ok_and(|value| value.trim() == "1") {
                write_setting(&accept_ra.to_string_lossy(), "2")?;
                kept.push(name);
            }
        }
        kept.sort();
    }
    write_setting(forwarding_setting(family), "1")?;
    Ok(kept)
}

/// Whether the interface `name` (under `/proc/sys/net/ipv6/conf`) is one
/// whose `accept_ra` is the host's: not the entry for all of them, not
/// loopback, not Shipyard's bridges or its containers' veths (`default`,
/// for interfaces made later, is).
fn learns_routes(name: &str) -> bool {
    !matches!(name, "all" | "lo") && !is_shipyards(name)
}

/// Whether the interface `name` is one of Shipyard's: a bridge, or a
/// container's veth.
fn is_shipyards(name: &str) -> bool {
    name.starts_with(crate::store::DEVICE_PREFIX) || name == crate::store::DEFAULT_BRIDGE
}

/// The host's own interfaces, now: every one but loopback and Shipyard's.
/// What routing is closed between when Shipyard turns forwarding on (see
/// the module `nft`).
pub fn host_interfaces() -> Result<Vec<String>, AttachError> {
    let mut names: Vec<String> = Rtnetlink::new()?
        .links()?
        .into_iter()
        .map(|link| link.name)
        .filter(|name| name != "lo" && !is_shipyards(name))
        .collect();
    names.sort();
    Ok(names)
}

/// The IPv6 settings of the container's `interface` (in the calling
/// thread's network namespace), before it's up: with IPv6 (`on`), no
/// duplicate address detection (its addresses are the network's to give,
/// never another's; and one being checked is "tentative" for a second or
/// two, while connections time out), and no router advertisements or
/// addresses made from them (another container's would route its traffic);
/// without, none at all.
fn container_ipv6(interface: &str, on: bool) -> Result<(), AttachError> {
    if !on {
        return set_ipv6(interface, "disable_ipv6", "1");
    }
    set_ipv6(interface, "disable_ipv6", "0")?;
    set_ipv6(interface, "accept_dad", "0")?;
    set_ipv6(interface, "accept_ra", "0")?;
    set_ipv6(interface, "autoconf", "0")
}

/// Sets `interface`'s IPv6 setting `name` (in the calling thread's network
/// namespace), if the kernel has IPv6.
fn set_ipv6(interface: &str, name: &str, value: &str) -> Result<(), AttachError> {
    // The interface's name was checked as one's: no `/`, no `..`.
    let path = format!("/proc/sys/net/ipv6/conf/{interface}/{name}");
    if std::path::Path::new(&path).exists() {
        write_setting(&path, value)?;
    }
    Ok(())
}

/// Runs `f` on a thread of its own in the network namespace `netns` (a
/// thread's namespace is its own: the caller's never changes), for what
/// only works from inside, such as its `/proc/sys/net` settings.
fn in_netns<T: Send>(
    netns: BorrowedFd<'_>,
    f: impl FnOnce() -> Result<T, AttachError> + Send,
) -> Result<T, AttachError> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                nix::sched::setns(netns, nix::sched::CloneFlags::CLONE_NEWNET).map_err(
                    |errno| AttachError::Io {
                        context: "joining the container's network namespace".to_owned(),
                        source: std::io::Error::from(errno),
                    },
                )?;
                f()
            })
            .map_err(|source| AttachError::Io {
                context: "starting a thread for the container's network".to_owned(),
                source,
            })?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// The interface a sysctl key is one interface's setting of
/// (`net.ipv4.conf.IF.SETTING`, `net.ipv6.conf.IF.SETTING`, or `neigh`'s:
/// `net.ipv4.neigh.IF.SETTING`), if it is,
/// for an interface a container's network makes: not `all`, `default` or
/// `lo` (those are there from the start, and its init sets them). Each part
/// plain (letters, digits, `_` and `-`): never a path of another kind.
#[must_use]
pub fn interface_of_sysctl(key: &str) -> Option<&str> {
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    };
    match key.split('.').collect::<Vec<_>>().as_slice() {
        ["net", "ipv4" | "ipv6", "conf" | "neigh", interface, setting]
            if plain(interface)
                && plain(setting)
                && !matches!(*interface, "all" | "default" | "lo") =>
        {
            Some(interface)
        }
        _ => None,
    }
}

/// The interfaces the default routes of the network namespace `netns` go
/// through, by name: IPv4's, then IPv6's (a container's way out, as its
/// start set it, and whatever changed it since).
///
/// # Errors
///
/// Its links or routes can't be read.
#[cfg(target_os = "linux")]
pub fn default_route_interfaces(
    netns: BorrowedFd<'_>,
) -> Result<(Option<String>, Option<String>), AttachError> {
    use crate::ipnet::Family;
    let mut inside = Rtnetlink::in_namespace(netns)?;
    let links = inside.links()?;
    let mut through = |family: Family| -> Result<Option<String>, AttachError> {
        Ok(inside
            .routes(family)?
            .into_iter()
            .find(|route| route.destination.prefix() == 0)
            .and_then(|route| route.index)
            .and_then(|index| links.iter().find(|link| link.index == index))
            .map(|link| link.name.clone()))
    };
    Ok((through(Family::V4)?, through(Family::V6)?))
}

/// Sets `settings` (sysctl keys, each an interface's: see
/// [`interface_of_sysctl`], and their values) in the network namespace
/// `netns`, once its interfaces are there (a container's own settings,
/// which its init couldn't make before its networks were).
///
/// # Errors
///
/// A key that isn't an interface's, a setting the kernel doesn't have, or
/// one it refuses.
#[cfg(target_os = "linux")]
pub fn set_interface_sysctls(
    netns: BorrowedFd<'_>,
    settings: &[(String, String)],
) -> Result<(), AttachError> {
    for (key, _) in settings {
        if interface_of_sysctl(key).is_none() {
            return Err(AttachError::Io {
                context: format!("--sysctl {key}"),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "not an interface's setting",
                ),
            });
        }
    }
    in_netns(netns, || {
        for (key, value) in settings {
            write_setting(&format!("/proc/sys/{}", key.replace('.', "/")), value)?;
        }
        Ok(())
    })
}

/// Writes `value` to the kernel setting at `path`.
fn write_setting(path: &str, value: &str) -> Result<(), AttachError> {
    std::fs::write(path, value).map_err(|source| AttachError::Io {
        context: format!("writing {path}"),
        source,
    })
}

/// Which of a container's default routes go through a network: a
/// container has one of each family it has, each through one of its
/// networks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DefaultRoutes {
    /// IPv4's.
    pub ipv4: bool,
    /// IPv6's (for a network, and a lease, with IPv6).
    pub ipv6: bool,
}

/// Connects the container whose network namespace is `netns` to `config`'s
/// network, with `lease`'s address: a veth pair from the bridge, and in the
/// container, the lease's interface (`eth0`, …) up with the address (and
/// with it, a route to the network's subnet), and the default `routes`
/// through the gateway. If any of it fails, the pair is deleted again.
pub fn connect(
    config: &NetworkConfig,
    lease: &Lease,
    netns: BorrowedFd<'_>,
    routes: DefaultRoutes,
) -> Result<(), AttachError> {
    let mut host = Rtnetlink::new()?;
    let bridge = host.link(&config.bridge)?.ok_or_else(|| {
        AttachError::Device(format!(
            "the network {}'s bridge {} isn't there",
            config.name, config.bridge
        ))
    })?;
    host.create_veth(
        &lease.host_veth,
        &lease.interface,
        Some(netns),
        Some(lease.mac),
    )?;
    let connected = (|| {
        let veth = host.link(&lease.host_veth)?.ok_or_else(|| {
            AttachError::Device(format!(
                "the veth {} went as soon as it was made",
                lease.host_veth
            ))
        })?;
        // The same MTU at both ends and on the bridge, or large frames are
        // lost one way.
        if let Some(mtu) = bridge.mtu {
            host.change(veth.index, LinkChange::Mtu(mtu))?;
        }
        host.change(veth.index, LinkChange::Master(bridge.index))?;
        // A container reaching itself through the host (a published port)
        // needs its frames sent back out of the port they came in on.
        host.set_hairpin(veth.index, true)?;
        host.change(veth.index, LinkChange::Up(true))?;
        let mut inside = Rtnetlink::in_namespace(netns)?;
        let interface = inside.link(&lease.interface)?.ok_or_else(|| {
            AttachError::Device(format!(
                "the container's {} isn't in its namespace",
                lease.interface
            ))
        })?;
        if let Some(mtu) = bridge.mtu {
            inside.change(interface.index, LinkChange::Mtu(mtu))?;
        }
        // Its IPv6 settings, from inside (before it's up).
        let ipv6 = lease.ip6.is_some() && config.subnet6.is_some();
        in_netns(netns, || container_ipv6(&lease.interface, ipv6))?;
        inside.add_address(interface.index, config.host_net(lease.ip))?;
        if let (Some(ip6), Some(subnet6)) = (lease.ip6, config.subnet6) {
            let net = IpNet::new(IpAddr::V6(ip6), subnet6.prefix())
                .map_err(|error| AttachError::Device(error.to_string()))?;
            inside.add_address(interface.index, net)?;
        }
        inside.change(interface.index, LinkChange::Up(true))?;
        if routes.ipv4 {
            inside.add_route(&Route::default_via(config.gateway))?;
        }
        if routes.ipv6
            && let Some(gateway6) = lease.ip6.and(config.gateway6)
        {
            inside.add_route(&Route::default_via(IpAddr::V6(gateway6)))?;
        }
        Ok(())
    })();
    if connected.is_err() {
        // Best effort: the error that matters is the one that stopped it.
        // The pair is this call's own (made above), wherever it got to.
        if let Ok(Some(link)) = host.link(&lease.host_veth) {
            let _ = delete_if_present(&mut host, link.index);
        }
    }
    connected
}

/// Deletes `lease`'s veth pair (both ends go together), if it's still there
/// as a port of `config`'s bridge: after its run, or a run that's gone (the
/// kernel deletes it with the container's namespace too, but not at once:
/// probe N8). A device of that name elsewhere is someone else's (another
/// run's, whose name is the same by chance), and is left alone.
pub fn disconnect(config: &NetworkConfig, lease: &Lease) -> Result<(), AttachError> {
    let mut host = Rtnetlink::new()?;
    let Some(bridge) = host.link(&config.bridge)? else {
        return Ok(());
    };
    match host.link(&lease.host_veth)? {
        Some(link) if link.master == Some(bridge.index) => delete_if_present(&mut host, link.index),
        _ => Ok(()),
    }
}

/// Deletes `config`'s bridge, if it's there and is one.
pub fn delete_bridge(config: &NetworkConfig) -> Result<(), AttachError> {
    let mut host = Rtnetlink::new()?;
    match host.link(&config.bridge)? {
        Some(link) if link.kind.as_deref() == Some("bridge") => {
            delete_if_present(&mut host, link.index)
        }
        _ => Ok(()),
    }
}

/// Deletes the device `index`; one that's gone since it was looked up is
/// fine (the kernel deletes a veth with its namespace, in its own time).
fn delete_if_present(host: &mut Rtnetlink, index: u32) -> Result<(), AttachError> {
    match host.delete(index) {
        Err(error) if error.errno() == Some(Errno::ENODEV) => Ok(()),
        other => Ok(other?),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    //! Against the real kernel, as root, each in a network namespace of the
    //! test's own (see `route/linux_tests.rs`): never the host's.

    use std::os::fd::AsFd as _;

    use nix::sched::{CloneFlags, unshare};

    use super::*;

    use crate::linux_test::{in_new_netns, this_netns};
    use crate::store::{bridge_name, mac_for};

    /// Both default routes.
    const BOTH: DefaultRoutes = DefaultRoutes {
        ipv4: true,
        ipv6: true,
    };

    fn config() -> NetworkConfig {
        NetworkConfig {
            name: "app".into(),
            subnet: "10.92.0.0/24".parse().unwrap(),
            gateway: "10.92.0.1".parse().unwrap(),
            bridge: bridge_name("app"),
            internal: false,
            created: 0,
            subnet6: None,
            gateway6: None,
            isolate_host: false,
            conntrack_limit: false,
        }
    }

    fn lease(ip: &str) -> Lease {
        let ip = ip.parse().unwrap();
        Lease {
            ip,
            mac: mac_for(ip),
            run: "a1b2c3d4e5f6".into(),
            hostname: "a1b2c3d4e5f6".into(),
            host_veth: "shya1b2c3d4".into(),
            ports: Vec::new(),
            name: None,
            aliases: Vec::new(),
            interface: CONTAINER_INTERFACE.into(),
            ip6: None,
            container: None,
        }
    }

    #[test]
    fn only_the_hosts_own_interfaces_keep_taking_router_advertisements() {
        for host in ["eth0", "wlan0", "enp3s0", "docker0", "default"] {
            assert!(learns_routes(host), "{host}");
        }
        for not in [
            "all",
            "lo",
            "shipyard0",
            "shy-0123456789",
            "shya1b2c3d4",
            "shya1b2c3d4e51",
        ] {
            assert!(!learns_routes(not), "{not}");
        }
    }

    #[test]
    #[ignore = "needs root"]
    fn a_container_is_connected_and_a_failed_connect_leaves_nothing() {
        in_new_netns(|| {
            let container = this_netns();
            unshare(CloneFlags::CLONE_NEWNET).unwrap(); // "the host"
            let config = config();
            ensure_bridge(&config).unwrap();
            // Twice is fine: it's there, as it should be.
            ensure_bridge(&config).unwrap();
            let lease = lease("10.92.0.2");
            connect(&config, &lease, container.as_fd(), BOTH).unwrap();
            let mut host = Rtnetlink::new().unwrap();
            let veth = host.link(&lease.host_veth).unwrap().unwrap();
            let bridge = host.link(&config.bridge).unwrap().unwrap();
            assert_eq!(veth.master, Some(bridge.index));
            // The bridge keeps its own address with a port (one that isn't
            // set takes its first port's).
            let gateway_mac = crate::store::mac_for(config.gateway);
            assert_eq!(bridge.mac.as_deref(), Some(&gateway_mac[..]));
            let mut inside = Rtnetlink::in_namespace(container.as_fd()).unwrap();
            let eth0 = inside.link(CONTAINER_INTERFACE).unwrap().unwrap();
            assert_eq!(eth0.mac.as_deref(), Some(&lease.mac[..]));
            disconnect(&config, &lease).unwrap();
            assert_eq!(host.link(&lease.host_veth).unwrap(), None);
            let bridge = host.link(&config.bridge).unwrap().unwrap();
            assert_eq!(bridge.mac.as_deref(), Some(&gateway_mac[..]));
            // Gone: a second disconnect is fine.
            disconnect(&config, &lease).unwrap();

            // A gateway off the subnet: the default route is refused, after
            // the pair is made. Nothing of it is left, on either side.
            let unreachable = NetworkConfig {
                gateway: "10.93.0.1".parse().unwrap(),
                ..config.clone()
            };
            let error = connect(&unreachable, &lease, container.as_fd(), BOTH).unwrap_err();
            assert!(
                matches!(&error, AttachError::Netlink(e) if e.errno() == Some(Errno::ENETUNREACH)),
                "{error}"
            );
            assert_eq!(host.link(&lease.host_veth).unwrap(), None);
            let mut inside = Rtnetlink::in_namespace(container.as_fd()).unwrap();
            assert_eq!(inside.link(CONTAINER_INTERFACE).unwrap(), None);
            delete_bridge(&config).unwrap();
            assert_eq!(host.link(&config.bridge).unwrap(), None);
        });
    }

    #[test]
    #[ignore = "needs root"]
    fn a_second_network_is_another_interface_without_the_default_route() {
        use crate::Family;
        in_new_netns(|| {
            let container = this_netns();
            unshare(CloneFlags::CLONE_NEWNET).unwrap(); // "the host"
            let front = config();
            let back = NetworkConfig {
                name: "back".into(),
                subnet: "10.94.0.0/24".parse().unwrap(),
                gateway: "10.94.0.1".parse().unwrap(),
                bridge: bridge_name("back"),
                ..config()
            };
            ensure_bridge(&front).unwrap();
            ensure_bridge(&back).unwrap();
            let first = lease("10.92.0.2");
            let second = Lease {
                host_veth: "shya1b2c3d4e51".into(),
                interface: interface_name(1),
                ..lease("10.94.0.2")
            };
            connect(&front, &first, container.as_fd(), BOTH).unwrap();
            connect(&back, &second, container.as_fd(), DefaultRoutes::default()).unwrap();
            let host_has = |name: &str| Rtnetlink::new().unwrap().link(name).unwrap().is_some();
            let mut inside = Rtnetlink::in_namespace(container.as_fd()).unwrap();
            let eth1 = inside.link("eth1").unwrap().unwrap();
            assert!(eth1.is_up());
            let routes = inside.routes(Family::V4).unwrap();
            let defaults: Vec<_> = routes
                .iter()
                .filter(|route| route.destination.prefix() == 0)
                .collect();
            assert_eq!(defaults.len(), 1, "{routes:?}");
            assert_eq!(defaults[0].gateway, Some(front.gateway));
            // Each network's subnet through its own interface.
            assert!(routes.iter().any(|route| {
                route.destination == back.subnet && route.index == Some(eth1.index)
            }));
            // Each is deleted from its own bridge only.
            disconnect(&back, &first).unwrap();
            assert!(host_has(&first.host_veth));
            disconnect(&front, &first).unwrap();
            disconnect(&back, &second).unwrap();
            assert!(!host_has(&first.host_veth));
            assert!(!host_has(&second.host_veth));
            delete_bridge(&front).unwrap();
            delete_bridge(&back).unwrap();
        });
    }
}

#[cfg(test)]
mod sysctl_tests {
    use super::*;

    /// An interface's setting is `net.ipv4.conf.IF.X` or `net.ipv6.conf.IF.X`
    /// for an interface a network makes; `all`, `default` and `lo` are the
    /// init's, and anything else (another path) isn't one.
    #[test]
    fn interface_sysctls_are_one_interfaces() {
        assert_eq!(
            interface_of_sysctl("net.ipv4.conf.eth1.rp_filter"),
            Some("eth1")
        );
        assert_eq!(
            interface_of_sysctl("net.ipv6.conf.eth0.accept_ra"),
            Some("eth0")
        );
        assert_eq!(
            interface_of_sysctl("net.ipv4.neigh.eth1.gc_stale_time"),
            Some("eth1")
        );
        for not in [
            "net.ipv4.conf.all.rp_filter",
            "net.ipv4.conf.default.rp_filter",
            "net.ipv4.conf.lo.rp_filter",
            "net.ipv4.ip_forward",
            "net.ipv4.conf.eth0",
            "net.ipv4.conf.eth0.a.b",
            "net.ipv4.conf..rp_filter",
            "net.ipv4.conf.eth0/../x.y",
            "kernel.conf.eth0.x",
        ] {
            assert_eq!(interface_of_sysctl(not), None, "{not}");
        }
    }
}
