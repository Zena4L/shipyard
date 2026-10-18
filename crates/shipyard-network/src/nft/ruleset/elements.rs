//! What goes in the sets and maps: a network's elements, a published
//! port's, and the requests that add them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::super::consts::IFNAMSIZ;
use super::super::objects::{Element, add_elements};
use super::{ELEMENTS_PER_REQUEST, FAMILY, NetworkRules, ONE_BATCH, PortRule, TABLE, set};
use crate::ipnet::IpNet;
use crate::netlink::BatchItem;
use crate::ports::Published;

/// The elements `network` puts in the sets.
pub(crate) fn network_elements(network: &NetworkRules) -> Vec<(&'static str, Vec<Element>)> {
    let name = ifname_key(&network.bridge);
    let bridge = Element::key(name.clone());
    let same = Element::key([name.clone(), name].concat());
    let mut elements = vec![
        (set::BRIDGES, vec![bridge.clone()]),
        (set::SAME_BRIDGE, vec![same]),
    ];
    if network.isolate_host {
        elements.push((set::ISOLATED, vec![bridge.clone()]));
        if let Some(dns) = network.dns {
            elements.push((set::DNS_ALLOW, dns_allow(&network.bridge, dns)));
        }
    }
    if network.conntrack_limit {
        elements.push((set::LIMITED, vec![bridge.clone()]));
    }
    if network.internal {
        elements.push((set::INTERNAL, vec![bridge]));
    } else {
        elements.push((set::MASQUERADE, subnet_range(network.subnet)));
        if let Some(subnet6) = network.subnet6 {
            elements.push((set::MASQUERADE6, subnet_range(subnet6)));
        }
    }
    elements
}

/// `dns_allow`'s elements for a network's bridge `bridge` and DNS server
/// `dns`: port 53, over UDP and TCP (each field in whole registers, as
/// the kernel loads them).
fn dns_allow(bridge: &str, dns: Ipv4Addr) -> Vec<Element> {
    [crate::ports::Protocol::Udp, crate::ports::Protocol::Tcp]
        .into_iter()
        .map(|protocol| {
            Element::key(
                [
                    ifname_key(bridge),
                    dns.octets().to_vec(),
                    vec![protocol.number(), 0, 0, 0],
                    vec![0, 53, 0, 0],
                ]
                .concat(),
            )
        })
        .collect()
}

/// Adds `network`'s elements (those there already are left).
pub(crate) fn add_network(network: &NetworkRules) -> Vec<BatchItem> {
    network_elements(network)
        .iter()
        .map(|(set, elements)| add_elements(FAMILY, TABLE, set, elements, false))
        .collect()
}

/// Adds `ports`' elements, a request per map (and per
/// [`ELEMENTS_PER_REQUEST`]); with `exclusive`, one that's there already (a
/// port another run publishes) is an error.
pub(crate) fn port_items(ports: &[PortRule], exclusive: bool) -> Vec<BatchItem> {
    by_map(ports)
        .into_iter()
        .flat_map(|(set, elements)| {
            elements
                .chunks(ELEMENTS_PER_REQUEST)
                .map(|chunk| add_elements(FAMILY, TABLE, set, chunk, exclusive))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The table's install batch `table` with the `ports` items in it, if they
/// fit one batch ([`ONE_BATCH`]); else `table` alone, and the ports for
/// batches of their own (returned).
pub(crate) fn fit_ports(
    mut table: Vec<BatchItem>,
    ports: Vec<BatchItem>,
) -> (Vec<BatchItem>, Vec<BatchItem>) {
    let size: usize = table
        .iter()
        .chain(&ports)
        .map(|item| item.message.encoded_len())
        .sum();
    if size <= ONE_BATCH {
        table.extend(ports);
        (table, Vec::new())
    } else {
        (table, ports)
    }
}

/// `ports`' elements, by map (one whose families don't match is none).
pub(crate) fn by_map(ports: &[PortRule]) -> Vec<(&'static str, Vec<Element>)> {
    let mut maps: Vec<(&'static str, Vec<Element>)> = Vec::new();
    for (set, element) in ports.iter().filter_map(port_element) {
        match maps.iter_mut().find(|(name, _)| *name == set) {
            Some((_, elements)) => elements.push(element),
            None => maps.push((set, vec![element])),
        }
    }
    maps
}

/// The port rule a ports map's element says: the inverse of
/// [`port_element`]; `None` for one that isn't one of ours.
pub(crate) fn port_rule(set: &str, element: &Element) -> Option<PortRule> {
    use crate::ports::Protocol;
    let key = element.key.as_slice();
    let (host_ip, rest): (Option<IpAddr>, &[u8]) = match set {
        set::PORTS | set::PORTS6 => (None, key),
        set::PORTS_ADDR => {
            let (ip, rest) = key.split_first_chunk::<4>()?;
            (Some(IpAddr::V4(Ipv4Addr::from(*ip))), rest)
        }
        set::PORTS6_ADDR => {
            let (ip, rest) = key.split_first_chunk::<16>()?;
            (Some(IpAddr::V6(Ipv6Addr::from(*ip))), rest)
        }
        _ => return None,
    };
    let [protocol, _, _, _, high, low, _, _] = <[u8; 8]>::try_from(rest).ok()?;
    let protocol = match protocol {
        6 => Protocol::Tcp,
        17 => Protocol::Udp,
        _ => return None,
    };
    let data = element.data.as_deref()?;
    let (container, port): (IpAddr, &[u8]) = if matches!(set, set::PORTS6 | set::PORTS6_ADDR) {
        let (ip, port) = data.split_first_chunk::<16>()?;
        (IpAddr::V6(Ipv6Addr::from(*ip)), port)
    } else {
        let (ip, port) = data.split_first_chunk::<4>()?;
        (IpAddr::V4(Ipv4Addr::from(*ip)), port)
    };
    let [port_high, port_low, _, _] = <[u8; 4]>::try_from(port).ok()?;
    Some(PortRule {
        published: Published {
            host_ip,
            host_port: u16::from_be_bytes([high, low]),
            container_port: u16::from_be_bytes([port_high, port_low]),
            protocol,
        },
        container,
    })
}

/// An interface name as a set key: its bytes, padded with NULs to
/// `IFNAMSIZ` (as `meta iifname` loads it).
pub(crate) fn ifname_key(name: &str) -> Vec<u8> {
    let mut key = name.as_bytes().to_vec();
    key.resize(IFNAMSIZ, 0);
    key
}

/// `subnet` as an interval: its first address, and the end (one past its
/// last), flagged as the end.
pub(crate) fn subnet_range(subnet: IpNet) -> Vec<Element> {
    let network = subnet.network();
    let (start, end) = match network.addr() {
        IpAddr::V4(start) => {
            let last = subnet.broadcast().map_or(u32::from(start), u32::from);
            (
                start.octets().to_vec(),
                last.checked_add(1).map(|end| end.to_be_bytes().to_vec()),
            )
        }
        IpAddr::V6(start) => {
            let size = subnet.size();
            let end = u128::from(start)
                .checked_add(size)
                .filter(|_| size != u128::MAX);
            (
                start.octets().to_vec(),
                end.map(|end| end.to_be_bytes().to_vec()),
            )
        }
    };
    let mut elements = vec![Element::key(start)];
    // The end of the last address is past the address space: nothing to
    // say.
    if let Some(end) = end {
        elements.push(Element {
            key: end,
            data: None,
            interval_end: true,
        });
    }
    elements
}

/// `port`'s map and element; `None` if its addresses' families differ (a
/// port on an IPv6 host address to an IPv4 container). Each field of a
/// concatenation takes whole 32-bit registers, padded with zeros, as the
/// kernel loads it: the protocol (one byte, first), a port (two bytes,
/// network order, first), an address (four, or sixteen).
pub(crate) fn port_element(port: &PortRule) -> Option<(&'static str, Element)> {
    let published = port.published;
    let protocol = [published.protocol.number(), 0, 0, 0];
    let [high, low] = published.host_port.to_be_bytes();
    let host_port = [high, low, 0, 0];
    let [high, low] = published.container_port.to_be_bytes();
    let (container, v6) = match port.container {
        IpAddr::V4(ip) => (ip.octets().to_vec(), false),
        IpAddr::V6(ip) => (ip.octets().to_vec(), true),
    };
    let mut data = container;
    data.extend([high, low, 0, 0]);
    let element = |key| Element {
        key,
        data: Some(data.clone()),
        interval_end: false,
    };
    Some(match published.host_ip {
        Some(IpAddr::V4(ip)) if !v6 => (
            set::PORTS_ADDR,
            element([ip.octets().as_slice(), &protocol, &host_port].concat()),
        ),
        Some(IpAddr::V6(ip)) if v6 => (
            set::PORTS6_ADDR,
            element([ip.octets().as_slice(), &protocol, &host_port].concat()),
        ),
        Some(_) => return None,
        None => (
            if v6 { set::PORTS6 } else { set::PORTS },
            element([protocol, host_port].concat()),
        ),
    })
}
