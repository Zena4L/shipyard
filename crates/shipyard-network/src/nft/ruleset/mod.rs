//! Shipyard's firewall: the table `inet shipyard`, ours alone.
//!
//! Its rules never change. What changes, as networks and containers come and
//! go, is the contents of its sets and maps (and, as packets pass, of the
//! dynamic sets `conns4` and `conns6`, which the kernel fills):
//!
//! | set           | holds                                                        |
//! |---------------|--------------------------------------------------------------|
//! | `bridges`     | every network's bridge                                       |
//! | `same_bridge` | `bridge . bridge` for each: traffic within one network       |
//! | `internal`    | the bridges of networks with no way out (`--internal`)       |
//! | `masquerade4` | the subnets of networks with a way out                       |
//! | `masquerade6` | their IPv6 prefixes                                          |
//! | `ports`       | `protocol . port` → `container address . port` (any address) |
//! | `ports_addr`  | `address . protocol . port` → the same (one host address)    |
//! | `ports6`, `ports6_addr` | the same for IPv6                                  |
//! | `closed`      | `ipv4`, `ipv6`: those Shipyard turned forwarding on for      |
//! | `uplinks`     | the host's own interfaces when it did                        |
//! | `outside`     | links no published port is reached from (the macOS VM's NAT) |
//! | `isolated`    | the bridges of networks kept from the host (`--isolate-host`) |
//! | `dns_allow`   | `bridge . gateway . protocol . 53`: what those may still reach |
//! | `limited`     | the bridges of networks with a connection limit              |
//! | `conns4`, `conns6` | a container address each, counting its connections (dynamic) |
//!
//! The chains:
//!
//! - `guard` (prerouting, before anything): a packet for 127.0.0.0/8, or
//!   from it, that didn't come in on `lo` is dropped. Our bridges have
//!   `route_localnet` on (so the host's own clients can reach a port
//!   published on 127.0.0.1), which would otherwise let a container reach the
//!   host's loopback services, or pass for one of them (CVE-2020-8558); and
//!   the DNAT of a port published on 127.0.0.1 happens before the kernel's
//!   own check, so a neighbour could reach it otherwise.
//! - `dstnat` (prerouting) and `output` (the host's own packets): a packet
//!   for one of the host's addresses, whose protocol and port (and address)
//!   are in a ports map, goes to the container's instead (DNAT).
//! - `forward` (what the host routes): what conntrack finds invalid, into
//!   or out of a container's bridge, is dropped; replies pass; so does what stays on
//!   one bridge (a host with `br_netfilter` hands switched frames to this
//!   hook too); nothing goes into or out of an internal network; what was
//!   DNAT'd (a published port) passes, unless it came in on a link in
//!   `outside` (the macOS guest's `eth0`: only the Mac reaches its ports,
//!   over vsock, never another VM on its network); anything else for a container's bridge
//!   is dropped, so neither another network nor the outside reaches a
//!   container, except through a published port. And if the host didn't
//!   route before Shipyard turned forwarding on (`closed`), it doesn't start
//!   routing for others: what goes from one of the interfaces it had then
//!   (`uplinks`) to another is dropped. Interfaces that came later (another
//!   program's bridges, a VPN) are left to their programs, which turned
//!   forwarding on for themselves. Before anything is accepted: an isolated
//!   network's connections to the cloud's metadata addresses are dropped,
//!   and a limited network's new ones counted (see `input`).
//! - `input` (what's for the host itself): from a limited network, a new
//!   connection is counted per source address, and refused over
//!   [`CONNTRACK_LIMIT`] (one from an IPv6 link-local source refused
//!   outright); from an isolated network, only replies, its DNS server and
//!   IPv6's neighbour discovery pass.
//! - `srcnat` (postrouting): a container's packet leaving by another
//!   interface takes the host's address (masquerade): its way out. One
//!   DNAT'd back onto a bridge (a container reaching a published port, maybe
//!   its own) does too, so the reply comes back the same way; so does one
//!   from 127.0.0.0/8 (the host's own client of a port published there).
//!
//! The table is `inet`, so a rule that matches interfaces or connection
//! states holds for both families. Those that match addresses, and the sets
//! of addresses, are written once per family. IPv6 needs no guard: the
//! kernel never routes `::1` off `lo`, and has no `route_localnet` for it.
//!
//! The table carries its version as user data: an older or damaged one (a
//! `nft flush ruleset`) is rebuilt whole, from the networks and leases on
//! disk.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::consts::{
    IFNAMSIZ, NF_ACCEPT, NF_INET_FORWARD, NF_INET_LOCAL_IN, NF_INET_LOCAL_OUT,
    NF_INET_POST_ROUTING, NF_INET_PRE_ROUTING, NFPROTO_INET, NFPROTO_IPV4, NFPROTO_IPV6,
    NFT_REG32_00, NFT_SET_EVAL, NFT_SET_INTERVAL, TYPE_IFNAME, TYPE_INET_PROTOCOL,
    TYPE_INET_SERVICE, TYPE_IP6ADDR, TYPE_IPADDR, TYPE_NFPROTO, concat_type,
};
use super::objects::{
    BaseChain, Element, SetSpec, add_elements, new_chain, new_rule, new_set, new_table,
};
use crate::ipnet::{Family, IpNet};
use crate::netlink::BatchItem;
use crate::ports::Published;

mod elements;
mod exprs;
mod rules;
#[cfg(test)]
mod tests;

// The Linux client's, and (`ifname_key`) `spoofing`'s.
#[cfg_attr(
    not(target_os = "linux"),
    allow(unused_imports, reason = "the client is Linux's")
)]
pub(crate) use elements::{
    add_network, by_map, fit_ports, ifname_key, network_elements, port_element, port_items,
    port_rule,
};
pub(crate) use rules::rules;

/// The table's name.
pub const TABLE: &str = "shipyard";
/// Its family: IPv4 and IPv6 (`inet`).
pub(crate) const FAMILY: u8 = NFPROTO_INET;
/// Its user data: what version of these rules it holds.
// (6: `masquerade` became `masquerade4`: `masquerade` is a keyword of
// nft's language, so `nft -f` couldn't read back a listing of the table,
// and a host saving and restoring its firewall with nft would fail on it.
// 7: the `input` chain and the metadata drops, for networks made with
// `--isolate-host`, stage 9, step 9.7c1. 8: the connection limit, for
// networks made with `--conntrack-limit`, step 9.7c2. 9: the `outside` set
// and its drop of what was DNAT'd from it, the macOS guest's NAT link, step
// 13.5b, F108.)
pub(crate) const VERSION: &[u8] = b"shipyard ruleset 9";

/// The most connections a container on a network made with
/// `--conntrack-limit` may have open, of those it opened (counted per
/// source address: an IPv6 one apart from the IPv4 one). The next new one
/// is refused. A busy server opens few (its clients open theirs); a
/// container opening thousands is a crawler, or a runaway: the host's
/// whole table is often 65,536 to 262,144.
pub const CONNTRACK_LIMIT: u32 = 8192;
/// The most elements one request adds or removes: each is answered alone,
/// and a batch must fit the socket's buffers.
const ELEMENTS_PER_REQUEST: usize = 256;
/// The most bytes of requests the table's install batch carries with the
/// ports in it: well inside a netlink socket's default send buffer (about
/// 208 KiB; a larger batch is refused whole, `EMSGSIZE`).
const ONE_BATCH: usize = 64 * 1024;

/// The chains' names.
pub(crate) mod chain {
    pub(crate) const GUARD: &str = "guard";
    pub(crate) const DSTNAT: &str = "dstnat";
    pub(crate) const OUTPUT: &str = "output";
    pub(crate) const FORWARD: &str = "forward";
    pub(crate) const SRCNAT: &str = "srcnat";
    pub(crate) const INPUT: &str = "input";
}

/// The sets' names.
pub(crate) mod set {
    pub(crate) const BRIDGES: &str = "bridges";
    pub(crate) const SAME_BRIDGE: &str = "same_bridge";
    pub(crate) const INTERNAL: &str = "internal";
    pub(crate) const CLOSED: &str = "closed";
    pub(crate) const UPLINKS: &str = "uplinks";
    pub(crate) const OUTSIDE: &str = "outside";
    pub(crate) const MASQUERADE: &str = "masquerade4";
    pub(crate) const MASQUERADE6: &str = "masquerade6";
    pub(crate) const PORTS: &str = "ports";
    pub(crate) const PORTS_ADDR: &str = "ports_addr";
    pub(crate) const PORTS6: &str = "ports6";
    pub(crate) const PORTS6_ADDR: &str = "ports6_addr";
    pub(crate) const ISOLATED: &str = "isolated";
    pub(crate) const DNS_ALLOW: &str = "dns_allow";
    pub(crate) const LIMITED: &str = "limited";
    pub(crate) const CONNS4: &str = "conns4";
    pub(crate) const CONNS6: &str = "conns6";
}

/// A network, as the firewall sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRules {
    /// Its bridge.
    pub bridge: String,
    /// Its subnet.
    pub subnet: IpNet,
    /// Its IPv6 prefix, if it has IPv6.
    pub subnet6: Option<IpNet>,
    /// No way out.
    pub internal: bool,
    /// Its containers kept from the host itself, and from the cloud's
    /// metadata addresses (`network create --isolate-host`).
    pub isolate_host: bool,
    /// Its DNS server's address (its gateway), if it has one: what an
    /// isolated network's containers may still reach on the host.
    pub dns: Option<Ipv4Addr>,
    /// Its containers' connections limited (`network create
    /// --conntrack-limit`, [`CONNTRACK_LIMIT`]).
    pub conntrack_limit: bool,
}

impl From<&crate::store::NetworkConfig> for NetworkRules {
    fn from(config: &crate::store::NetworkConfig) -> Self {
        NetworkRules {
            bridge: config.bridge.clone(),
            subnet: config.subnet,
            subnet6: config.subnet6,
            internal: config.internal,
            isolate_host: config.isolate_host,
            conntrack_limit: config.conntrack_limit,
            dns: match config.gateway {
                IpAddr::V4(gateway) if config.has_dns() => Some(gateway),
                _ => None,
            },
        }
    }
}

/// IPv4's link-local block (169.254.0.0/16), kept from an isolated
/// network's containers: it holds most clouds' metadata address
/// (169.254.169.254: AWS's, Azure's, Google's, …) and AWS's DNS and time
/// servers (169.254.169.253, 169.254.169.123).
const LINK_LOCAL4: [u8; 2] = [169, 254];
/// IPv6's link-local prefix (`fe80::/10`).
const LINK_LOCAL6: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0);
/// Alibaba Cloud's metadata address, outside the link-local block.
const METADATA4: Ipv4Addr = Ipv4Addr::new(100, 100, 100, 200);
/// The IPv6 metadata addresses: AWS's (`fd00:ec2::254`), AWS's DNS
/// (`fd00:ec2::23`) and Google's (`fd20:ce::254`).
const METADATA6: [Ipv6Addr; 3] = [
    Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x23),
    Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254),
];
/// `ICMPv6`'s neighbour solicitation and advertisement (RFC 4861: 135 and
/// 136): what an isolated network's IPv6 still needs of the host, to find
/// its gateway's hardware address and be found. (Router solicitations, 133,
/// aren't: a container's IPv6 is set, not learnt from the host.)
const ND_FIRST: u8 = 135;
/// See [`ND_FIRST`].
const ND_LAST: u8 = 136;

/// A published port, and the container address it goes to (of the host
/// address's family, for a port on one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRule {
    /// What's published.
    pub published: Published,
    /// The container's address.
    pub container: IpAddr,
}

/// The 32-bit register `n`.
const fn reg(n: u32) -> u32 {
    NFT_REG32_00 + n
}

pub(super) fn sets() -> [SetSpec; 17] {
    let ifname_len = u32::try_from(IFNAMSIZ).unwrap_or(16);
    let ifname = (TYPE_IFNAME, ifname_len);
    let [isolated, dns_allow, limited, conns4, conns6] = option_sets();
    let target = Some((concat_type(&[TYPE_IPADDR, TYPE_INET_SERVICE]), 8));
    // An IPv6 address takes four registers: 16 bytes, then the port's 4.
    let target6 = Some((concat_type(&[TYPE_IP6ADDR, TYPE_INET_SERVICE]), 20));
    [
        SetSpec {
            name: set::BRIDGES,
            flags: 0,
            key: ifname,
            data: None,
        },
        SetSpec {
            name: set::SAME_BRIDGE,
            flags: 0,
            key: (concat_type(&[TYPE_IFNAME, TYPE_IFNAME]), 2 * ifname_len),
            data: None,
        },
        SetSpec {
            name: set::CLOSED,
            flags: 0,
            key: (TYPE_NFPROTO, 1),
            data: None,
        },
        SetSpec {
            name: set::UPLINKS,
            flags: 0,
            key: ifname,
            data: None,
        },
        SetSpec {
            name: set::OUTSIDE,
            flags: 0,
            key: ifname,
            data: None,
        },
        SetSpec {
            name: set::INTERNAL,
            flags: 0,
            key: ifname,
            data: None,
        },
        SetSpec {
            name: set::MASQUERADE,
            flags: NFT_SET_INTERVAL,
            key: (TYPE_IPADDR, 4),
            data: None,
        },
        SetSpec {
            name: set::PORTS,
            flags: 0,
            key: (concat_type(&[TYPE_INET_PROTOCOL, TYPE_INET_SERVICE]), 8),
            data: target,
        },
        SetSpec {
            name: set::PORTS_ADDR,
            flags: 0,
            key: (
                concat_type(&[TYPE_IPADDR, TYPE_INET_PROTOCOL, TYPE_INET_SERVICE]),
                12,
            ),
            data: target,
        },
        SetSpec {
            name: set::MASQUERADE6,
            flags: NFT_SET_INTERVAL,
            key: (TYPE_IP6ADDR, 16),
            data: None,
        },
        SetSpec {
            name: set::PORTS6,
            flags: 0,
            key: (concat_type(&[TYPE_INET_PROTOCOL, TYPE_INET_SERVICE]), 8),
            data: target6,
        },
        SetSpec {
            name: set::PORTS6_ADDR,
            flags: 0,
            key: (
                concat_type(&[TYPE_IP6ADDR, TYPE_INET_PROTOCOL, TYPE_INET_SERVICE]),
                24,
            ),
            data: target6,
        },
        isolated,
        dns_allow,
        limited,
        conns4,
        conns6,
    ]
}

/// The sets of a network's options: `--isolate-host`'s and
/// `--conntrack-limit`'s.
fn option_sets() -> [SetSpec; 5] {
    let ifname_len = u32::try_from(IFNAMSIZ).unwrap_or(16);
    let ifname = (TYPE_IFNAME, ifname_len);
    [
        SetSpec {
            name: set::ISOLATED,
            flags: 0,
            key: ifname,
            data: None,
        },
        // The bridge (16 bytes), the gateway, the protocol, the port: each
        // in whole registers.
        SetSpec {
            name: set::DNS_ALLOW,
            flags: 0,
            key: (
                concat_type(&[
                    TYPE_IFNAME,
                    TYPE_IPADDR,
                    TYPE_INET_PROTOCOL,
                    TYPE_INET_SERVICE,
                ]),
                ifname_len + 12,
            ),
            data: None,
        },
        SetSpec {
            name: set::LIMITED,
            flags: 0,
            key: ifname,
            data: None,
        },
        // Filled by the limit's rules, one element per source, each with its
        // count.
        SetSpec {
            name: set::CONNS4,
            flags: NFT_SET_EVAL,
            key: (TYPE_IPADDR, 4),
            data: None,
        },
        SetSpec {
            name: set::CONNS6,
            flags: NFT_SET_EVAL,
            key: (TYPE_IP6ADDR, 16),
            data: None,
        },
    ]
}

pub(super) fn chains() -> [BaseChain; 6] {
    [
        BaseChain {
            name: chain::GUARD,
            kind: "filter",
            hook: NF_INET_PRE_ROUTING,
            priority: -300,
            policy: NF_ACCEPT,
        },
        BaseChain {
            name: chain::DSTNAT,
            kind: "nat",
            hook: NF_INET_PRE_ROUTING,
            priority: -100,
            policy: NF_ACCEPT,
        },
        BaseChain {
            name: chain::OUTPUT,
            kind: "nat",
            hook: NF_INET_LOCAL_OUT,
            priority: -100,
            policy: NF_ACCEPT,
        },
        BaseChain {
            name: chain::FORWARD,
            kind: "filter",
            hook: NF_INET_FORWARD,
            priority: 0,
            policy: NF_ACCEPT,
        },
        BaseChain {
            name: chain::SRCNAT,
            kind: "nat",
            hook: NF_INET_POST_ROUTING,
            priority: 100,
            policy: NF_ACCEPT,
        },
        // What's for the host itself, from an isolated network.
        BaseChain {
            name: chain::INPUT,
            kind: "filter",
            hook: NF_INET_LOCAL_IN,
            priority: 0,
            policy: NF_ACCEPT,
        },
    ]
}

/// Where Shipyard closed the host's routing to all but its containers (see
/// the module): the families it turned forwarding on for, and the
/// interfaces the host had then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosedRouting {
    /// The families.
    pub families: Vec<Family>,
    /// The host's own interfaces, at the time.
    pub interfaces: Vec<String>,
}

/// The batch that creates the whole table, filled with `networks` and
/// `ports`, with routing `closed`, and with the links no published port is
/// reached from (`outside`: none on a Linux host; the macOS guest's NAT
/// link) (on a table that isn't there: the caller deletes an old one in the
/// same batch).
pub(crate) fn install(
    networks: &[NetworkRules],
    ports: &[PortRule],
    closed: &ClosedRouting,
    outside: &[String],
) -> Vec<BatchItem> {
    let mut items = vec![new_table(FAMILY, TABLE, VERSION)];
    items.extend(
        sets()
            .iter()
            .zip(1..)
            .map(|(spec, id)| new_set(FAMILY, TABLE, spec, id)),
    );
    items.extend(chains().iter().map(|chain| new_chain(FAMILY, TABLE, chain)));
    let mut count = std::collections::HashMap::<&str, usize>::new();
    for (chain, exprs) in rules() {
        let number = count.entry(chain).or_default();
        *number += 1;
        items.push(new_rule(FAMILY, TABLE, chain, *number, &exprs));
    }
    for network in networks {
        items.extend(add_network(network));
    }
    items.extend(port_items(ports, false));
    for &family in &closed.families {
        items.extend(close_routing(family, &closed.interfaces));
    }
    if !outside.is_empty() {
        let links: Vec<Element> = outside
            .iter()
            .map(|name| Element::key(ifname_key(name)))
            .collect();
        items.push(add_elements(FAMILY, TABLE, set::OUTSIDE, &links, false));
    }
    items
}

/// Closes routing in `family` to all but the containers, between the
/// host's `interfaces` (see the module).
pub(crate) fn close_routing(family: Family, interfaces: &[String]) -> Vec<BatchItem> {
    let nfproto = match family {
        Family::V4 => NFPROTO_IPV4,
        Family::V6 => NFPROTO_IPV6,
    };
    let mut items = Vec::new();
    // The interfaces first: never a family closed with no interfaces in the
    // set, which would close nothing.
    if !interfaces.is_empty() {
        let uplinks: Vec<Element> = interfaces
            .iter()
            .map(|name| Element::key(ifname_key(name)))
            .collect();
        items.push(add_elements(FAMILY, TABLE, set::UPLINKS, &uplinks, false));
    }
    items.push(add_elements(
        FAMILY,
        TABLE,
        set::CLOSED,
        &[Element::key(vec![nfproto])],
        false,
    ));
    items
}
