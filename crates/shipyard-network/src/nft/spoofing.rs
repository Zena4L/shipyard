//! Shipyard's second table, `bridge shipyard`: each container sends only
//! as itself.
//!
//! A container's root has `CAP_NET_RAW` (Docker's default set), so it can
//! write any frame it likes: from another container's addresses (IP or
//! hardware), or an ARP reply saying the gateway is at its own. Its
//! neighbours on the bridge would believe it, and so would the network's DNS
//! server. The `inet` table can't stop it: a frame from one port of a bridge
//! to another is switched, never routed, and a routed one is seen as coming
//! from the bridge, not from the container's port. A `bridge` table sees
//! every frame as it enters the bridge, from the port (`meta iifname`: the
//! host's end of the container's veth pair).
//!
//! | set           | holds, for each container's port                        |
//! |---------------|---------------------------------------------------------|
//! | `ports`       | its name: frames from other ports aren't checked        |
//! | `macs`        | `port . hardware address`                               |
//! | `addresses`   | `port . hardware address . IPv4 address`                |
//! | `addresses6`  | `port . hardware address . IPv6 address`                |
//!
//! The chain `ports_only` (prerouting, before anything) lets through, from
//! a container's port: IPv4 from its address and hardware address; ARP
//! whose sender (both hardware fields, and the address) is it; IPv6 from
//! its address or a link-local one (with its hardware address), except a
//! neighbour advertisement for an address that isn't its own (IPv6's ARP
//! reply). Anything else from it is dropped: other protocols too, VLAN
//! tags among them (the bridge takes only an outer tag off, and the frame
//! inside could be another's IPv4 or ARP, believed by whoever takes the
//! rest off). A link-local IPv6 address isn't tied to the
//! container, nor are advertisements for one: carried forward.
//!
//! A port that isn't in `ports` is left alone. That's what keeps the table
//! safe to add (it never stops traffic it doesn't know): a container whose
//! elements are missing (a kernel without bridge filtering, a table flushed
//! and not rebuilt yet) can still send as others, as before this table.
//!
//! The table is its own, apart from `inet shipyard`, and installed in a
//! batch of its own: a kernel without bridge filtering refuses it, and the
//! `inet` table must not go with it. It carries its version as user data
//! too.

use std::net::{Ipv4Addr, Ipv6Addr};

use super::consts::{
    ETH_P_ARP, ETH_P_IP, ETH_P_IPV6, IFNAMSIZ, IPPROTO_ICMPV6, NF_ACCEPT, NF_BR_PRE_ROUTING,
    NF_DROP, NFPROTO_BRIDGE, NFT_CMP_EQ, NFT_CMP_NEQ, NFT_META_IIFNAME, NFT_META_L4PROTO,
    NFT_META_PROTOCOL, NFT_PAYLOAD_LL_HEADER, NFT_PAYLOAD_NETWORK_HEADER,
    NFT_PAYLOAD_TRANSPORT_HEADER, NFT_REG32_00, TYPE_ETHERADDR, TYPE_IFNAME, TYPE_IP6ADDR,
    TYPE_IPADDR, concat_type,
};
use super::expr::Expr;
use super::objects::{
    BaseChain, Element, SetSpec, add_elements, new_chain, new_rule, new_set, new_table,
};
use super::ruleset::ifname_key;
use crate::netlink::BatchItem;

/// The table's name (in its own family: no clash with `inet shipyard`).
pub(crate) const TABLE: &str = "shipyard";
/// Its family.
pub(crate) const FAMILY: u8 = NFPROTO_BRIDGE;
/// Its user data: what version of these rules it holds.
pub(crate) const VERSION: &[u8] = b"shipyard bridge ruleset 1";
/// The chain's name.
const CHAIN: &str = "ports_only";

/// The sets' names.
mod set {
    pub(super) const PORTS: &str = "ports";
    pub(super) const MACS: &str = "macs";
    pub(super) const ADDRESSES: &str = "addresses";
    pub(super) const ADDRESSES6: &str = "addresses6";
}

/// A neighbour advertisement's `ICMPv6` type.
const ND_NEIGHBOR_ADVERT: u8 = 136;

/// A container's port on a bridge, and who it may send as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortGuard {
    /// The host's end of its veth pair.
    pub port: String,
    /// Its hardware address.
    pub mac: [u8; 6],
    /// Its IPv4 address.
    pub ip: Option<Ipv4Addr>,
    /// Its IPv6 address.
    pub ip6: Option<Ipv6Addr>,
}

/// The 32-bit register `n`.
const fn reg(n: u32) -> u32 {
    NFT_REG32_00 + n
}

/// Register 4: where a hardware address goes, after the port's name (16
/// bytes: registers 0 to 3). It takes two (6 bytes, padded to 8).
const MAC_REG: u32 = 4;
/// Register 6: where an address goes, after the hardware address.
const ADDRESS_REG: u32 = 6;

fn sets() -> [SetSpec; 4] {
    let ifname_len = u32::try_from(IFNAMSIZ).unwrap_or(16);
    [
        SetSpec {
            name: set::PORTS,
            flags: 0,
            key: (TYPE_IFNAME, ifname_len),
            data: None,
        },
        SetSpec {
            name: set::MACS,
            flags: 0,
            key: (concat_type(&[TYPE_IFNAME, TYPE_ETHERADDR]), ifname_len + 8),
            data: None,
        },
        SetSpec {
            name: set::ADDRESSES,
            flags: 0,
            key: (
                concat_type(&[TYPE_IFNAME, TYPE_ETHERADDR, TYPE_IPADDR]),
                ifname_len + 8 + 4,
            ),
            data: None,
        },
        SetSpec {
            name: set::ADDRESSES6,
            flags: 0,
            key: (
                concat_type(&[TYPE_IFNAME, TYPE_ETHERADDR, TYPE_IP6ADDR]),
                ifname_len + 8 + 16,
            ),
            data: None,
        },
    ]
}

fn chain() -> BaseChain {
    BaseChain {
        name: CHAIN,
        kind: "filter",
        hook: NF_BR_PRE_ROUTING,
        priority: -300,
        policy: NF_ACCEPT,
    }
}

/// `meta protocol` (the frame's ethertype) is `ethertype`.
fn protocol(ethertype: u16) -> Vec<Expr> {
    vec![
        Expr::Meta {
            key: NFT_META_PROTOCOL,
            dreg: reg(0),
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: ethertype.to_be_bytes().to_vec(),
        },
    ]
}

/// The frame's port into registers 0 to 3, and its source hardware address
/// (`ether saddr`) into [`MAC_REG`].
fn port_and_mac() -> Vec<Expr> {
    vec![
        Expr::Meta {
            key: NFT_META_IIFNAME,
            dreg: reg(0),
        },
        Expr::Payload {
            base: NFT_PAYLOAD_LL_HEADER,
            offset: 6,
            len: 6,
            dreg: reg(MAC_REG),
        },
    ]
}

/// Loads `len` bytes at `offset` of `base` into [`ADDRESS_REG`].
fn address(base: u32, offset: u32, len: u32) -> Expr {
    Expr::Payload {
        base,
        offset,
        len,
        dreg: reg(ADDRESS_REG),
    }
}

/// Looks registers 0 on up in `set` (with `invert`, a hit ends the rule).
fn lookup(set: &str, invert: bool) -> Expr {
    Expr::Lookup {
        set: set.to_owned(),
        sreg: reg(0),
        dreg: None,
        invert,
    }
}

/// The two bytes at `offset` of `base` are a link-local IPv6 address's
/// start (`fe80::/10`); with `not`, they aren't.
fn link_local(base: u32, offset: u32, not: bool) -> Vec<Expr> {
    vec![
        Expr::Payload {
            base,
            offset,
            len: 2,
            dreg: reg(0),
        },
        Expr::Bitwise {
            sreg: reg(0),
            dreg: reg(0),
            mask: vec![0xff, 0xc0],
            xor: vec![0, 0],
        },
        Expr::Cmp {
            sreg: reg(0),
            op: if not { NFT_CMP_NEQ } else { NFT_CMP_EQ },
            data: vec![0xfe, 0x80],
        },
    ]
}

/// The chain's rules, in order (see the module).
pub(crate) fn rules() -> Vec<Vec<Expr>> {
    let accept = Expr::Verdict(NF_ACCEPT);
    let drop = Expr::Verdict(NF_DROP);
    // IPv4: from its address, with its hardware address.
    let mut ipv4 = protocol(ETH_P_IP);
    ipv4.extend(port_and_mac());
    ipv4.extend([
        address(NFT_PAYLOAD_NETWORK_HEADER, 12, 4),
        lookup(set::ADDRESSES, false),
        accept.clone(),
    ]);
    // ARP: the frame's sender and the ARP sender (hardware address, then
    // address) are it.
    let mut arp = protocol(ETH_P_ARP);
    arp.extend(port_and_mac());
    arp.extend([
        address(NFT_PAYLOAD_NETWORK_HEADER, 14, 4),
        lookup(set::ADDRESSES, false),
        Expr::Payload {
            base: NFT_PAYLOAD_NETWORK_HEADER,
            offset: 8,
            len: 6,
            dreg: reg(MAC_REG),
        },
        lookup(set::ADDRESSES, false),
        accept.clone(),
    ]);
    // A neighbour advertisement for a (not link-local) address that isn't
    // its own: someone else's, announced as at this container.
    let mut advert = protocol(ETH_P_IPV6);
    advert.extend([
        Expr::Meta {
            key: NFT_META_L4PROTO,
            dreg: reg(0),
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: vec![IPPROTO_ICMPV6],
        },
        Expr::Payload {
            base: NFT_PAYLOAD_TRANSPORT_HEADER,
            offset: 0,
            len: 1,
            dreg: reg(0),
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: vec![ND_NEIGHBOR_ADVERT],
        },
    ]);
    advert.extend(link_local(NFT_PAYLOAD_TRANSPORT_HEADER, 8, true));
    advert.extend(port_and_mac());
    advert.extend([
        address(NFT_PAYLOAD_TRANSPORT_HEADER, 8, 16),
        lookup(set::ADDRESSES6, true),
        drop.clone(),
    ]);
    // IPv6: from its address, or a link-local one, with its hardware
    // address.
    let mut ipv6 = protocol(ETH_P_IPV6);
    ipv6.extend(port_and_mac());
    ipv6.extend([
        address(NFT_PAYLOAD_NETWORK_HEADER, 8, 16),
        lookup(set::ADDRESSES6, false),
        accept.clone(),
    ]);
    let mut ipv6_link_local = protocol(ETH_P_IPV6);
    ipv6_link_local.extend(link_local(NFT_PAYLOAD_NETWORK_HEADER, 8, false));
    ipv6_link_local.extend(port_and_mac());
    ipv6_link_local.extend([lookup(set::MACS, false), accept.clone()]);
    // Anything else of these from it isn't it.
    let refused = |ethertype| [protocol(ethertype), vec![drop.clone()]].concat();
    vec![
        // Not a container's port: not this table's.
        vec![
            Expr::Meta {
                key: NFT_META_IIFNAME,
                dreg: reg(0),
            },
            lookup(set::PORTS, true),
            accept,
        ],
        ipv4,
        refused(ETH_P_IP),
        arp,
        refused(ETH_P_ARP),
        advert,
        ipv6,
        ipv6_link_local,
        refused(ETH_P_IPV6),
        // Any other protocol: dropped. One this table doesn't read could
        // carry anything; a VLAN tag (the bridge takes only the outer one
        // off) could carry an IPv4 or ARP frame from another's address,
        // which its neighbour would believe once it took the tags off.
        vec![drop],
    ]
}

/// The batch that makes the whole table (on one that isn't there: the
/// caller deletes an old one in the same batch), with `guards`.
pub(crate) fn install(guards: &[PortGuard]) -> Vec<BatchItem> {
    let mut items = vec![new_table(FAMILY, TABLE, VERSION)];
    items.extend(
        sets()
            .iter()
            .zip(1..)
            .map(|(spec, id)| new_set(FAMILY, TABLE, spec, id)),
    );
    items.push(new_chain(FAMILY, TABLE, &chain()));
    for (number, exprs) in rules().iter().enumerate() {
        items.push(new_rule(FAMILY, TABLE, CHAIN, number + 1, exprs));
    }
    for guard in guards {
        items.extend(guard_items(guard));
    }
    items
}

/// The elements `guard` puts in the sets, by set. `ports` comes last: the
/// port is checked only once it may send as itself.
pub(crate) fn guard_elements(guard: &PortGuard) -> Vec<(&'static str, Vec<Element>)> {
    let port = ifname_key(&guard.port);
    let mut mac = guard.mac.to_vec();
    // Padded to whole registers, as the kernel loads it.
    mac.resize(8, 0);
    let port_mac = [port.clone(), mac].concat();
    let mut elements = vec![(set::MACS, vec![Element::key(port_mac.clone())])];
    if let Some(ip) = guard.ip {
        elements.push((
            set::ADDRESSES,
            vec![Element::key([port_mac.as_slice(), &ip.octets()].concat())],
        ));
    }
    if let Some(ip6) = guard.ip6 {
        elements.push((
            set::ADDRESSES6,
            vec![Element::key([port_mac.as_slice(), &ip6.octets()].concat())],
        ));
    }
    elements.push((set::PORTS, vec![Element::key(port)]));
    elements
}

/// Adds `guard`'s elements (those there already are left).
pub(crate) fn guard_items(guard: &PortGuard) -> Vec<BatchItem> {
    guard_elements(guard)
        .iter()
        .map(|(set, elements)| add_elements(FAMILY, TABLE, set, elements, false))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;

    fn guard() -> PortGuard {
        PortGuard {
            port: "shyabcdef12".to_owned(),
            mac: [0x02, 0x42, 10, 0, 0, 2],
            ip: Some(Ipv4Addr::new(10, 0, 0, 2)),
            ip6: Some("fd00::2".parse().unwrap()),
        }
    }

    /// Each element's key is as long as its set's: the port's name padded
    /// to 16 bytes, the hardware address to 8, then the address.
    #[test]
    fn elements_fit_their_sets() {
        let specs = sets();
        for (name, elements) in guard_elements(&guard()) {
            let spec = specs.iter().find(|spec| spec.name == name).unwrap();
            for element in elements {
                assert_eq!(element.key.len(), spec.key.1 as usize, "{name}");
            }
        }
        let addresses = guard_elements(&guard());
        let ipv4 = &addresses
            .iter()
            .find(|(name, _)| *name == set::ADDRESSES)
            .unwrap()
            .1[0]
            .key;
        assert_eq!(&ipv4[..11], b"shyabcdef12");
        assert_eq!(&ipv4[16..24], &[0x02, 0x42, 10, 0, 0, 2, 0, 0]);
        assert_eq!(&ipv4[24..], &[10, 0, 0, 2]);
    }

    /// A port is checked only once it may send as itself: `ports` is the
    /// last set it goes in. One with no IPv6 has no IPv6 element.
    #[test]
    fn a_port_is_checked_last() {
        let elements = guard_elements(&guard());
        assert_eq!(elements.last().unwrap().0, set::PORTS);
        let v4_only = PortGuard {
            ip6: None,
            ..guard()
        };
        assert!(
            !guard_elements(&v4_only)
                .iter()
                .any(|(name, _)| *name == set::ADDRESSES6)
        );
    }

    /// The first rule leaves other ports alone, and the last drops what no
    /// rule let through: the chain checks only containers' ports, and
    /// stops what isn't them. Every other rule that lets a frame through
    /// asks for one of the three protocols the table reads (no other
    /// protocol, a VLAN tag say, gets through).
    #[test]
    fn only_containers_ports_are_checked_and_the_rest_dropped() {
        let rules = rules();
        for rule in &rules[1..] {
            if rule.last() == Some(&Expr::Verdict(NF_ACCEPT)) {
                assert!(
                    [ETH_P_IP, ETH_P_ARP, ETH_P_IPV6]
                        .iter()
                        .any(|&ethertype| rule.starts_with(&protocol(ethertype))),
                    "{rule:?}"
                );
            }
        }
        assert!(matches!(
            rules[0].as_slice(),
            [
                Expr::Meta { .. },
                Expr::Lookup { invert: true, .. },
                Expr::Verdict(NF_ACCEPT)
            ]
        ));
        assert!(matches!(
            rules.last().unwrap().as_slice(),
            [Expr::Verdict(NF_DROP)]
        ));
    }

    /// The table's batch makes it, its sets, its chain and every rule, and
    /// a guard's elements.
    #[test]
    fn the_install_batch_is_whole() {
        let items = install(&[guard()]);
        assert_eq!(
            items.len(),
            1 + sets().len() + 1 + rules().len() + guard_elements(&guard()).len()
        );
    }
}
