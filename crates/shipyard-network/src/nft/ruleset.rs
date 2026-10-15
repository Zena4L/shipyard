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
//!   DNAT'd (a published port) passes; anything else for a container's bridge
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
    CT_STATE_ESTABLISHED, CT_STATE_INVALID, CT_STATE_RELATED, IFNAMSIZ, IPPROTO_ICMPV6,
    IPS_CONFIRMED, IPS_DST_NAT, NF_ACCEPT, NF_DROP, NF_INET_FORWARD, NF_INET_LOCAL_IN,
    NF_INET_LOCAL_OUT, NF_INET_POST_ROUTING, NF_INET_PRE_ROUTING, NFPROTO_INET, NFPROTO_IPV4,
    NFPROTO_IPV6, NFT_CMP_EQ, NFT_CMP_GTE, NFT_CMP_LTE, NFT_CMP_NEQ, NFT_CT_STATE, NFT_CT_STATUS,
    NFT_DYNSET_OP_ADD, NFT_FIB_RESULT_ADDRTYPE, NFT_META_IIFNAME, NFT_META_L4PROTO,
    NFT_META_NFPROTO, NFT_META_OIFNAME, NFT_NAT_DNAT, NFT_PAYLOAD_NETWORK_HEADER,
    NFT_PAYLOAD_TRANSPORT_HEADER, NFT_REG32_00, NFT_REJECT_ICMPX_ADMIN_PROHIBITED,
    NFT_REJECT_ICMPX_UNREACH, NFT_REJECT_TCP_RST, NFT_SET_EVAL, NFT_SET_INTERVAL, NFTA_FIB_F_DADDR,
    RTN_LOCAL, TYPE_IFNAME, TYPE_INET_PROTOCOL, TYPE_INET_SERVICE, TYPE_IP6ADDR, TYPE_IPADDR,
    TYPE_NFPROTO, concat_type,
};
use super::expr::Expr;
use super::objects::{
    BaseChain, Element, SetSpec, add_elements, new_chain, new_rule, new_set, new_table,
};
use crate::ipnet::{Family, IpNet};
use crate::netlink::BatchItem;
use crate::ports::Published;

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
// networks made with `--conntrack-limit`, step 9.7c2.)
pub(crate) const VERSION: &[u8] = b"shipyard ruleset 8";

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

pub(super) fn sets() -> [SetSpec; 16] {
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

/// `meta nfproto ipv4`.
fn ipv4() -> Vec<Expr> {
    nfproto(NFPROTO_IPV4)
}

/// `meta nfproto ipv6`.
fn ipv6() -> Vec<Expr> {
    nfproto(NFPROTO_IPV6)
}

/// `meta nfproto` is `family`.
fn nfproto(family: u8) -> Vec<Expr> {
    vec![
        Expr::Meta {
            key: NFT_META_NFPROTO,
            dreg: reg(0),
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: vec![family],
        },
    ]
}

/// `iifname` (or `oifname`, with `key`) in (or with `invert`, not in) `set`.
fn interface_in(key: u32, set: &str, invert: bool) -> Vec<Expr> {
    vec![
        Expr::Meta { key, dreg: reg(0) },
        Expr::Lookup {
            set: set.to_owned(),
            sreg: reg(0),
            dreg: None,
            invert,
        },
    ]
}

/// The IPv4 source (`offset` 12) or destination (16) address.
fn address(offset: u32, dreg: u32) -> Expr {
    network_header(offset, 4, dreg)
}

/// `len` bytes of the network header from `offset`, into `dreg` (and the
/// registers after it).
fn network_header(offset: u32, len: u32, dreg: u32) -> Expr {
    Expr::Payload {
        base: NFT_PAYLOAD_NETWORK_HEADER,
        offset,
        len,
        dreg,
    }
}

/// The IPv6 source (`offset` 8) or destination (24) address, into four
/// registers from `dreg`.
fn address6(offset: u32, dreg: u32) -> Expr {
    network_header(offset, 16, dreg)
}

/// `ip6 daddr != ::1`. The host's own client of `[::1]:PORT` isn't DNAT'd:
/// a packet from `::1` can't leave `lo` (a container drops it, and the
/// client would wait), so it meets the held port, and is refused at once.
fn not_to_loopback6() -> Vec<Expr> {
    vec![
        address6(24, reg(0)),
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_NEQ,
            data: Ipv6Addr::LOCALHOST.octets().to_vec(),
        },
    ]
}

/// `ip saddr` (or `daddr`) in 127.0.0.0/8.
fn loopback(offset: u32) -> Vec<Expr> {
    vec![
        address(offset, reg(0)),
        Expr::Bitwise {
            sreg: reg(0),
            dreg: reg(0),
            mask: vec![255, 0, 0, 0],
            xor: vec![0; 4],
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: vec![127, 0, 0, 0],
        },
    ]
}

/// `ct KEY & bits` is non-zero (`set`) or zero.
fn ct_bits(key: u32, bits: u32, set: bool) -> Vec<Expr> {
    vec![
        Expr::Ct { key, dreg: reg(0) },
        Expr::Bitwise {
            sreg: reg(0),
            dreg: reg(0),
            mask: bits.to_ne_bytes().to_vec(),
            xor: vec![0; 4],
        },
        Expr::Cmp {
            sreg: reg(0),
            op: if set { NFT_CMP_NEQ } else { NFT_CMP_EQ },
            data: vec![0; 4],
        },
    ]
}

/// `fib daddr type local`: the packet is for one of this host's addresses.
fn for_this_host() -> Vec<Expr> {
    vec![
        Expr::Fib {
            flags: NFTA_FIB_F_DADDR,
            result: NFT_FIB_RESULT_ADDRTYPE,
            dreg: reg(0),
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: RTN_LOCAL.to_ne_bytes().to_vec(),
        },
    ]
}

/// `meta l4proto` into `proto` and `th dport` into the register after it.
fn protocol_and_port(proto: u32) -> [Expr; 2] {
    [
        Expr::Meta {
            key: NFT_META_L4PROTO,
            dreg: reg(proto),
        },
        Expr::Payload {
            base: NFT_PAYLOAD_TRANSPORT_HEADER,
            offset: 2,
            len: 2,
            dreg: reg(proto + 1),
        },
    ]
}

/// Looks the key starting in register 0 up in the ports map `set`, and
/// DNATs (in `family`) to the address and port it maps to: the address from
/// register 0, the port after it (register 1 for IPv4, 4 for IPv6).
fn dnat_through(set: &str, family: Family) -> [Expr; 2] {
    let port = match family {
        Family::V4 => reg(1),
        Family::V6 => reg(4),
    };
    [
        Expr::Lookup {
            set: set.to_owned(),
            sreg: reg(0),
            dreg: Some(reg(0)),
            invert: false,
        },
        Expr::Nat {
            kind: NFT_NAT_DNAT,
            family: u32::from(match family {
                Family::V4 => NFPROTO_IPV4,
                Family::V6 => NFPROTO_IPV6,
            }),
            reg_addr: reg(0),
            reg_proto: port,
        },
    ]
}

/// The DNAT rules: by address, protocol and port, then by protocol and port
/// alone; for IPv4, then for IPv6.
fn dnat_rules() -> [Vec<Expr>; 4] {
    let mut by_address = [ipv4(), for_this_host()].concat();
    by_address.push(address(16, reg(0)));
    by_address.extend(protocol_and_port(1));
    by_address.extend(dnat_through(set::PORTS_ADDR, Family::V4));
    let mut by_port = [ipv4(), for_this_host()].concat();
    by_port.extend(protocol_and_port(0));
    by_port.extend(dnat_through(set::PORTS, Family::V4));
    // The address takes registers 0 to 3.
    let mut by_address6 = [ipv6(), not_to_loopback6(), for_this_host()].concat();
    by_address6.push(address6(24, reg(0)));
    by_address6.extend(protocol_and_port(4));
    by_address6.extend(dnat_through(set::PORTS6_ADDR, Family::V6));
    let mut by_port6 = [ipv6(), not_to_loopback6(), for_this_host()].concat();
    by_port6.extend(protocol_and_port(0));
    by_port6.extend(dnat_through(set::PORTS6, Family::V6));
    [by_address, by_port, by_address6, by_port6]
}

/// `iifname != "lo"`.
fn not_from_loopback() -> Vec<Expr> {
    vec![
        Expr::Meta {
            key: NFT_META_IIFNAME,
            dreg: reg(0),
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_NEQ,
            data: ifname_key("lo"),
        },
    ]
}

/// `iifname . oifname @same_bridge`: the packet stays on one bridge.
fn on_one_bridge() -> Vec<Expr> {
    vec![
        Expr::Meta {
            key: NFT_META_IIFNAME,
            dreg: reg(0),
        },
        // The next 16 bytes: four registers on.
        Expr::Meta {
            key: NFT_META_OIFNAME,
            dreg: reg(4),
        },
        Expr::Lookup {
            set: set::SAME_BRIDGE.to_owned(),
            sreg: reg(0),
            dreg: None,
            invert: false,
        },
    ]
}

/// `meta nfproto @closed`: the host routes only for its containers.
fn routing_closed() -> Vec<Expr> {
    vec![
        Expr::Meta {
            key: NFT_META_NFPROTO,
            dreg: reg(0),
        },
        Expr::Lookup {
            set: set::CLOSED.to_owned(),
            sreg: reg(0),
            dreg: None,
            invert: false,
        },
    ]
}

/// Every chain's rules, in order.
pub(crate) fn rules() -> Vec<(&'static str, Vec<Expr>)> {
    let dnat = dnat_rules();
    let mut rules: Vec<(&'static str, Vec<Expr>)> = guard_rules()
        .into_iter()
        .map(|rule| (chain::GUARD, rule))
        .collect();
    rules.extend(dnat.iter().map(|rule| (chain::DSTNAT, rule.clone())));
    rules.extend(dnat.into_iter().map(|rule| (chain::OUTPUT, rule)));
    rules.extend(
        forward_rules()
            .into_iter()
            .map(|rule| (chain::FORWARD, rule)),
    );
    rules.extend(srcnat_rules().into_iter().map(|rule| (chain::SRCNAT, rule)));
    rules.extend(input_rules().into_iter().map(|rule| (chain::INPUT, rule)));
    rules
}

/// `input`'s: from an isolated network's bridge, to the host itself, only
/// replies (what the host opened), its DNS server on the gateway, and IPv6's
/// neighbour discovery; nothing else.
fn input_rules() -> [Vec<Expr>; 9] {
    let [tcp4, other4, tcp6, other6] = limit_rules();
    let [protocol, port] = protocol_and_port(5);
    let dns = vec![
        Expr::Meta {
            key: NFT_META_IIFNAME,
            dreg: reg(0),
        },
        // The bridge's name takes four registers.
        address(16, reg(4)),
        protocol,
        port,
        Expr::Lookup {
            set: set::DNS_ALLOW.to_owned(),
            sreg: reg(0),
            dreg: None,
            invert: false,
        },
    ];
    let neighbours = vec![
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
            op: NFT_CMP_GTE,
            data: vec![ND_FIRST],
        },
        Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_LTE,
            data: vec![ND_LAST],
        },
    ];
    [
        // Counted first: what a container opens to the host counts too.
        link_local_refused(),
        tcp4,
        other4,
        tcp6,
        other6,
        [
            from_isolated(),
            ct_bits(NFT_CT_STATE, CT_STATE_ESTABLISHED | CT_STATE_RELATED, true),
            vec![Expr::Verdict(NF_ACCEPT)],
        ]
        .concat(),
        [ipv4(), dns, vec![Expr::Verdict(NF_ACCEPT)]].concat(),
        [
            ipv6(),
            from_isolated(),
            neighbours,
            vec![Expr::Verdict(NF_ACCEPT)],
        ]
        .concat(),
        [from_isolated(), vec![Expr::Verdict(NF_DROP)]].concat(),
    ]
}

/// The connection limit (in `forward` and `input`): a connection from a
/// limited network, on its first packet (not yet confirmed: a later packet
/// of one unanswered still has `ct state new`, and isn't counted again),
/// adds its source to `conns4` (or `conns6`), whose element counts its
/// connections (`nf_conncount`). Over [`CONNTRACK_LIMIT`], it's refused: a
/// TCP one with a reset (`connect` fails, `ECONNREFUSED`, every time), any
/// other with an ICMP "administratively prohibited" (which the kernel
/// rate-limits). Refused, it's never confirmed: the table keeps the limit.
/// IPv4's TCP and others, then IPv6's.
fn limit_rules() -> [Vec<Expr>; 4] {
    let limit = |family: Vec<Expr>, source: Expr, set: &str, tcp: bool| {
        let refuse = if tcp {
            Expr::Reject {
                kind: NFT_REJECT_TCP_RST,
                code: None,
            }
        } else {
            Expr::Reject {
                kind: NFT_REJECT_ICMPX_UNREACH,
                code: Some(NFT_REJECT_ICMPX_ADMIN_PROHIBITED),
            }
        };
        [
            family,
            interface_in(NFT_META_IIFNAME, set::LIMITED, false),
            unconfirmed(),
            vec![
                Expr::Meta {
                    key: NFT_META_L4PROTO,
                    dreg: reg(0),
                },
                Expr::Cmp {
                    sreg: reg(0),
                    op: if tcp { NFT_CMP_EQ } else { NFT_CMP_NEQ },
                    data: vec![crate::ports::Protocol::Tcp.number()],
                },
                source,
                Expr::Dynset {
                    set: set.to_owned(),
                    op: NFT_DYNSET_OP_ADD,
                    sreg: reg(0),
                    expr: Box::new(Expr::Connlimit {
                        count: CONNTRACK_LIMIT,
                        invert: true,
                    }),
                },
                refuse,
            ],
        ]
        .concat()
    };
    [
        limit(ipv4(), address(12, reg(0)), set::CONNS4, true),
        limit(ipv4(), address(12, reg(0)), set::CONNS4, false),
        limit(ipv6(), address6(8, reg(0)), set::CONNS6, true),
        limit(ipv6(), address6(8, reg(0)), set::CONNS6, false),
    ]
}

/// `ct status & confirmed == 0`: a connection's first packet. (A packet
/// with no connection, `untracked` like neighbour discovery, has no status:
/// the rule doesn't match.)
fn unconfirmed() -> Vec<Expr> {
    ct_bits(NFT_CT_STATUS, IPS_CONFIRMED, false)
}

/// `input`'s first: a new connection to the host from a limited network,
/// from an IPv6 link-local source, is refused. The bridge table lets any
/// link-local source through (with the container's hardware address), so
/// counted, each made-up one would be a budget of its own, and enough of
/// them would fill `conns6`. Neighbour discovery is untracked, and passes.
fn link_local_refused() -> Vec<Expr> {
    [
        ipv6(),
        interface_in(NFT_META_IIFNAME, set::LIMITED, false),
        unconfirmed(),
        // `ip6 saddr fe80::/10`, the whole address loaded (as nft reads it).
        vec![
            address6(8, reg(0)),
            Expr::Bitwise {
                sreg: reg(0),
                dreg: reg(0),
                mask: [[0xff, 0xc0].as_slice(), &[0; 14]].concat(),
                xor: vec![0; 16],
            },
            Expr::Cmp {
                sreg: reg(0),
                op: NFT_CMP_EQ,
                data: LINK_LOCAL6.octets().to_vec(),
            },
        ],
        vec![Expr::Reject {
            kind: NFT_REJECT_ICMPX_UNREACH,
            code: Some(NFT_REJECT_ICMPX_ADMIN_PROHIBITED),
        }],
    ]
    .concat()
}

/// `iifname @isolated`: from an isolated network's bridge.
fn from_isolated() -> Vec<Expr> {
    interface_in(NFT_META_IIFNAME, set::ISOLATED, false)
}

/// `forward`'s metadata drops: from an isolated network, nothing to the
/// cloud's metadata addresses (IPv4's link-local block and Alibaba's, then
/// IPv6's).
fn metadata_rules() -> [Vec<Expr>; 5] {
    let to = |family: Vec<Expr>, daddr: Expr, data: Vec<u8>| {
        [
            family,
            from_isolated(),
            vec![
                daddr,
                Expr::Cmp {
                    sreg: reg(0),
                    op: NFT_CMP_EQ,
                    data,
                },
                Expr::Verdict(NF_DROP),
            ],
        ]
        .concat()
    };
    let link_local = [
        ipv4(),
        from_isolated(),
        vec![
            address(16, reg(0)),
            Expr::Bitwise {
                sreg: reg(0),
                dreg: reg(0),
                mask: vec![0xff, 0xff, 0, 0],
                xor: vec![0; 4],
            },
            Expr::Cmp {
                sreg: reg(0),
                op: NFT_CMP_EQ,
                data: [LINK_LOCAL4, [0, 0]].concat(),
            },
            Expr::Verdict(NF_DROP),
        ],
    ]
    .concat();
    let [aws, aws_dns, google] =
        METADATA6.map(|metadata| to(ipv6(), address6(24, reg(0)), metadata.octets().to_vec()));
    [
        link_local,
        to(ipv4(), address(16, reg(0)), METADATA4.octets().to_vec()),
        aws,
        aws_dns,
        google,
    ]
}

/// `guard`'s: nothing for or from 127.0.0.0/8 but on `lo`.
fn guard_rules() -> [Vec<Expr>; 2] {
    let drop = || Expr::Verdict(NF_DROP);
    [
        [ipv4(), not_from_loopback(), loopback(16), vec![drop()]].concat(),
        [ipv4(), not_from_loopback(), loopback(12), vec![drop()]].concat(),
    ]
}

/// `forward`'s (see the module).
fn forward_rules() -> [Vec<Expr>; 18] {
    let [link_local, alibaba, aws, aws_dns, google] = metadata_rules();
    let [tcp4, other4, tcp6, other6] = limit_rules();
    let drop = || Expr::Verdict(NF_DROP);
    let accept = || Expr::Verdict(NF_ACCEPT);
    let invalid = || ct_bits(NFT_CT_STATE, CT_STATE_INVALID, true);
    [
        // What conntrack can't place (out of any connection's window, an
        // error about no connection), into or out of a container's bridge:
        // dropped, as Kubernetes does. Other programs' traffic is theirs.
        [
            interface_in(NFT_META_IIFNAME, set::BRIDGES, false),
            invalid(),
            vec![drop()],
        ]
        .concat(),
        [
            interface_in(NFT_META_OIFNAME, set::BRIDGES, false),
            invalid(),
            vec![drop()],
        ]
        .concat(),
        [
            ct_bits(NFT_CT_STATE, CT_STATE_ESTABLISHED | CT_STATE_RELATED, true),
            vec![accept()],
        ]
        .concat(),
        // New connections from an isolated network to the metadata
        // addresses: before anything accepts them.
        link_local,
        alibaba,
        aws,
        aws_dns,
        google,
        // New connections from a limited network, counted: before anything
        // accepts them (after the metadata drops: one dropped anyway isn't
        // counted).
        tcp4,
        other4,
        tcp6,
        other6,
        [on_one_bridge(), vec![accept()]].concat(),
        [
            interface_in(NFT_META_IIFNAME, set::INTERNAL, false),
            vec![drop()],
        ]
        .concat(),
        [
            interface_in(NFT_META_OIFNAME, set::INTERNAL, false),
            vec![drop()],
        ]
        .concat(),
        [ct_bits(NFT_CT_STATUS, IPS_DST_NAT, true), vec![accept()]].concat(),
        [
            interface_in(NFT_META_OIFNAME, set::BRIDGES, false),
            vec![drop()],
        ]
        .concat(),
        [
            routing_closed(),
            interface_in(NFT_META_IIFNAME, set::UPLINKS, false),
            interface_in(NFT_META_OIFNAME, set::UPLINKS, false),
            vec![drop()],
        ]
        .concat(),
    ]
}

/// `srcnat`'s: the way out, hairpin, and local clients of 127.0.0.1; then
/// the way out and hairpin for IPv6.
fn srcnat_rules() -> [Vec<Expr>; 5] {
    let from_a_network = || {
        vec![
            address(12, reg(0)),
            Expr::Lookup {
                set: set::MASQUERADE.to_owned(),
                sreg: reg(0),
                dreg: None,
                invert: false,
            },
        ]
    };
    let from_a_network6 = || {
        vec![
            address6(8, reg(0)),
            Expr::Lookup {
                set: set::MASQUERADE6.to_owned(),
                sreg: reg(0),
                dreg: None,
                invert: false,
            },
        ]
    };
    [
        [
            ipv4(),
            from_a_network(),
            interface_in(NFT_META_OIFNAME, set::BRIDGES, true),
            vec![Expr::Masquerade],
        ]
        .concat(),
        [
            ipv4(),
            interface_in(NFT_META_OIFNAME, set::BRIDGES, false),
            ct_bits(NFT_CT_STATUS, IPS_DST_NAT, true),
            from_a_network(),
            vec![Expr::Masquerade],
        ]
        .concat(),
        [
            ipv4(),
            interface_in(NFT_META_OIFNAME, set::BRIDGES, false),
            loopback(12),
            vec![Expr::Masquerade],
        ]
        .concat(),
        [
            ipv6(),
            from_a_network6(),
            interface_in(NFT_META_OIFNAME, set::BRIDGES, true),
            vec![Expr::Masquerade],
        ]
        .concat(),
        [
            ipv6(),
            interface_in(NFT_META_OIFNAME, set::BRIDGES, false),
            ct_bits(NFT_CT_STATUS, IPS_DST_NAT, true),
            from_a_network6(),
            vec![Expr::Masquerade],
        ]
        .concat(),
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
/// `ports`, and with routing `closed` (on a table that isn't there: the
/// caller deletes an old one in the same batch).
pub(crate) fn install(
    networks: &[NetworkRules],
    ports: &[PortRule],
    closed: &ClosedRouting,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;
    use crate::netlink::message::Messages;
    use crate::ports::Protocol;

    fn rule(published: Published) -> PortRule {
        PortRule {
            published,
            container: IpAddr::V4(Ipv4Addr::new(10, 88, 0, 2)),
        }
    }

    fn rule6(published: Published) -> PortRule {
        PortRule {
            published,
            container: "fd00:1::2".parse().unwrap(),
        }
    }

    #[test]
    fn a_port_element_is_padded_as_registers_are() {
        let (set, element) = port_element(&rule(Published {
            host_ip: None,
            host_port: 8080,
            container_port: 80,
            protocol: Protocol::Tcp,
        }))
        .unwrap();
        assert_eq!(set, set::PORTS);
        // tcp (6), then 8080 = 0x1f90.
        assert_eq!(element.key, [6, 0, 0, 0, 0x1f, 0x90, 0, 0]);
        assert_eq!(element.data.unwrap(), [10, 88, 0, 2, 0, 80, 0, 0]);
        let (set, element) = port_element(&rule(Published {
            host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            host_port: 53,
            container_port: 5353,
            protocol: Protocol::Udp,
        }))
        .unwrap();
        assert_eq!(set, set::PORTS_ADDR);
        assert_eq!(element.key, [127, 0, 0, 1, 17, 0, 0, 0, 0, 53, 0, 0]);
        assert_eq!(element.data.unwrap(), [10, 88, 0, 2, 0x14, 0xe9, 0, 0]);
        // IPv6: the address takes 16 bytes (four registers), in the key and
        // in the data.
        let any = Published {
            host_ip: None,
            host_port: 8080,
            container_port: 80,
            protocol: Protocol::Tcp,
        };
        let (set, element) = port_element(&rule6(any)).unwrap();
        assert_eq!(set, set::PORTS6);
        assert_eq!(element.key, [6, 0, 0, 0, 0x1f, 0x90, 0, 0]);
        let container: Ipv6Addr = "fd00:1::2".parse().unwrap();
        assert_eq!(
            element.data.unwrap(),
            [container.octets().as_slice(), &[0, 80, 0, 0]].concat()
        );
        let host: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let (set, element) = port_element(&rule6(Published {
            host_ip: Some(IpAddr::V6(host)),
            ..any
        }))
        .unwrap();
        assert_eq!(set, set::PORTS6_ADDR);
        assert_eq!(
            element.key,
            [host.octets().as_slice(), &[6, 0, 0, 0, 0x1f, 0x90, 0, 0]].concat()
        );
        // The families must match.
        assert_eq!(
            port_element(&rule(Published {
                host_ip: Some(IpAddr::V6(host)),
                ..any
            })),
            None
        );
        assert_eq!(
            port_element(&rule6(Published {
                host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                ..any
            })),
            None
        );
    }

    /// The ports go in the table's own batch while they fit one; past that,
    /// in batches of their own.
    #[test]
    fn ports_go_in_the_tables_batch_while_they_fit() {
        let ports = |count: u16| -> Vec<PortRule> {
            (0..count)
                .map(|n| {
                    rule(Published {
                        host_ip: None,
                        host_port: 1000 + n,
                        container_port: 80,
                        protocol: Protocol::Tcp,
                    })
                })
                .collect()
        };
        let table = || install(&[], &[], &ClosedRouting::default());
        let tables = table().len();
        let few = port_items(&ports(3), false);
        let (batch, more) = fit_ports(table(), few.clone());
        assert_eq!(batch.len(), tables + few.len());
        assert!(more.is_empty());
        let many = port_items(&ports(4000), false);
        let size: usize = many.iter().map(|item| item.message.encoded_len()).sum();
        assert!(size > ONE_BATCH, "{size}");
        let (batch, more) = fit_ports(table(), many.clone());
        assert_eq!(batch.len(), tables);
        assert_eq!(more.len(), many.len());
    }

    #[test]
    fn a_ports_element_reads_back_as_its_rule() {
        for published in [
            Published {
                host_ip: None,
                host_port: 8080,
                container_port: 80,
                protocol: Protocol::Tcp,
            },
            Published {
                host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                host_port: 53,
                container_port: 5353,
                protocol: Protocol::Udp,
            },
        ] {
            let (set, element) = port_element(&rule(published)).unwrap();
            assert_eq!(port_rule(set, &element), Some(rule(published)));
        }
        for published in [
            Published {
                host_ip: None,
                host_port: 8080,
                container_port: 80,
                protocol: Protocol::Tcp,
            },
            Published {
                host_ip: Some("2001:db8::1".parse().unwrap()),
                host_port: 53,
                container_port: 5353,
                protocol: Protocol::Udp,
            },
        ] {
            let (set, element) = port_element(&rule6(published)).unwrap();
            assert_eq!(port_rule(set, &element), Some(rule6(published)));
        }
        assert_eq!(port_rule(set::BRIDGES, &Element::key(vec![0; 16])), None);
        // Several ports, one request per map.
        let both = [
            rule(Published {
                host_ip: None,
                host_port: 1,
                container_port: 1,
                protocol: Protocol::Tcp,
            }),
            rule(Published {
                host_ip: None,
                host_port: 2,
                container_port: 2,
                protocol: Protocol::Tcp,
            }),
        ];
        assert_eq!(port_items(&both, true).len(), 1);
    }

    #[test]
    fn a_subnet_is_an_interval_ending_past_its_last_address() {
        let elements = subnet_range("10.88.0.0/16".parse().unwrap());
        assert_eq!(elements[0], Element::key(vec![10, 88, 0, 0]));
        assert_eq!(
            elements[1],
            Element {
                key: vec![10, 89, 0, 0],
                data: None,
                interval_end: true
            }
        );
    }

    #[test]
    fn an_ipv6_prefix_is_an_interval_too() {
        let elements = subnet_range("fd00:1:2:3::/64".parse().unwrap());
        let start: Ipv6Addr = "fd00:1:2:3::".parse().unwrap();
        let end: Ipv6Addr = "fd00:1:2:4::".parse().unwrap();
        assert_eq!(elements[0], Element::key(start.octets().to_vec()));
        assert_eq!(elements[1].key, end.octets());
        assert!(elements[1].interval_end);
    }

    #[test]
    fn an_interface_key_is_padded_to_ifnamsiz() {
        let key = ifname_key("shipyard0");
        assert_eq!(key.len(), 16);
        assert_eq!(&key[..10], b"shipyard0\0");
    }

    #[test]
    fn internal_networks_have_no_way_out() {
        let internal = NetworkRules {
            bridge: "shy-x".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: Some("fd00:1::/64".parse().unwrap()),
            internal: true,
            isolate_host: false,
            conntrack_limit: false,
            dns: None,
        };
        let sets: Vec<&str> = network_elements(&internal)
            .iter()
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(sets, [set::BRIDGES, set::SAME_BRIDGE, set::INTERNAL]);
        let open = NetworkRules {
            internal: false,
            isolate_host: false,
            conntrack_limit: false,
            dns: None,
            ..internal
        };
        let sets: Vec<&str> = network_elements(&open).iter().map(|(s, _)| *s).collect();
        assert_eq!(
            sets,
            [
                set::BRIDGES,
                set::SAME_BRIDGE,
                set::MASQUERADE,
                set::MASQUERADE6
            ]
        );
    }

    #[test]
    fn ipv6_is_dnated_and_masqueraded_through_its_own_maps() {
        let rules = rules();
        let in_chain = |name: &str| -> Vec<&Vec<Expr>> {
            rules
                .iter()
                .filter(|(chain, _)| *chain == name)
                .map(|(_, exprs)| exprs)
                .collect()
        };
        // dstnat and output: IPv4 by address and by port, then IPv6's.
        for chain in [chain::DSTNAT, chain::OUTPUT] {
            let dnat = in_chain(chain);
            assert_eq!(dnat.len(), 4);
            assert_eq!(dnat[2][..2], ipv6()[..]);
            // Never for ::1.
            for rule in &dnat[2..] {
                assert_eq!(rule[2..4], not_to_loopback6()[..]);
            }
            assert!(dnat[2].contains(&address6(24, reg(0))));
            assert!(dnat[2].contains(&Expr::Nat {
                kind: NFT_NAT_DNAT,
                family: u32::from(NFPROTO_IPV6),
                reg_addr: reg(0),
                reg_proto: reg(4),
            }));
            assert!(dnat[3].contains(&Expr::Lookup {
                set: set::PORTS6.into(),
                sreg: reg(0),
                dreg: Some(reg(0)),
                invert: false,
            }));
        }
        let srcnat = in_chain(chain::SRCNAT);
        assert_eq!(srcnat.len(), 5);
        for rule in &srcnat[3..] {
            assert_eq!(rule[..2], ipv6()[..]);
            assert!(rule.contains(&address6(8, reg(0))));
            assert_eq!(rule.last(), Some(&Expr::Masquerade));
        }
    }

    /// The rules that keep containers in (the end-to-end tests can't send a
    /// packet for 127.0.0.0/8 across a bridge: the kernel keeps it local).
    #[test]
    fn the_guard_drops_what_a_bridge_sends_to_loopback() {
        let rules = rules();
        let guard: Vec<&Vec<Expr>> = rules
            .iter()
            .filter(|(chain, _)| *chain == "guard")
            .map(|(_, exprs)| exprs)
            .collect();
        assert_eq!(guard.len(), 2);
        // For 127/8 (offset 16), then from it (12): from anywhere but lo.
        for (rule, offset) in guard.iter().zip([16, 12]) {
            assert!(rule.contains(&Expr::Cmp {
                sreg: reg(0),
                op: NFT_CMP_NEQ,
                data: ifname_key("lo")
            }));
            assert!(rule.contains(&address(offset, reg(0))));
            assert!(rule.contains(&Expr::Cmp {
                sreg: reg(0),
                op: NFT_CMP_EQ,
                data: vec![127, 0, 0, 0]
            }));
            assert_eq!(rule.last(), Some(&Expr::Verdict(NF_DROP)));
        }
        // And it comes before NAT: -300.
        assert_eq!(chains()[0].priority, -300);
    }

    /// A connection from a limited network's bridge, on its first packet,
    /// adds its source to the dynamic set, whose element counts it; over the
    /// limit, a TCP one is reset and any other refused with an ICMP. Only
    /// limited networks' bridges are in `limited`.
    #[test]
    fn a_limited_network_counts_each_containers_new_connections() {
        let rules = limit_rules();
        let cases = [
            (NFPROTO_IPV4, address(12, reg(0)), set::CONNS4, true),
            (NFPROTO_IPV4, address(12, reg(0)), set::CONNS4, false),
            (NFPROTO_IPV6, address6(8, reg(0)), set::CONNS6, true),
            (NFPROTO_IPV6, address6(8, reg(0)), set::CONNS6, false),
        ];
        for (rule, (family, source, conns, tcp)) in rules.iter().zip(cases) {
            assert_eq!(rule[..2], nfproto(family)[..]);
            assert!(rule.contains(&Expr::Lookup {
                set: set::LIMITED.into(),
                sreg: reg(0),
                dreg: None,
                invert: false,
            }));
            // Its first packet only: `ct status & confirmed == 0`.
            let first = ct_bits(NFT_CT_STATUS, IPS_CONFIRMED, false);
            assert!(rule.windows(first.len()).any(|at| at == first));
            assert!(rule.contains(&Expr::Cmp {
                sreg: reg(0),
                op: if tcp { NFT_CMP_EQ } else { NFT_CMP_NEQ },
                data: vec![6],
            }));
            let refuse = if tcp {
                Expr::Reject {
                    kind: NFT_REJECT_TCP_RST,
                    code: None,
                }
            } else {
                Expr::Reject {
                    kind: NFT_REJECT_ICMPX_UNREACH,
                    code: Some(NFT_REJECT_ICMPX_ADMIN_PROHIBITED),
                }
            };
            assert_eq!(
                rule[rule.len() - 3..],
                [
                    source,
                    Expr::Dynset {
                        set: conns.into(),
                        op: NFT_DYNSET_OP_ADD,
                        sreg: reg(0),
                        expr: Box::new(Expr::Connlimit {
                            count: 8192,
                            invert: true,
                        }),
                    },
                    refuse,
                ]
            );
        }
        // A link-local source's new connection to the host is refused,
        // uncounted.
        let refused = link_local_refused();
        assert_eq!(refused[..2], nfproto(NFPROTO_IPV6)[..]);
        let first = ct_bits(NFT_CT_STATUS, IPS_CONFIRMED, false);
        assert!(refused.windows(first.len()).any(|at| at == first));
        assert!(refused.contains(&Expr::Cmp {
            sreg: reg(0),
            op: NFT_CMP_EQ,
            data: LINK_LOCAL6.octets().to_vec(),
        }));
        assert!(
            !refused
                .iter()
                .any(|expr| matches!(expr, Expr::Dynset { .. }))
        );
        // The counting sets are dynamic: the kernel fills them.
        let dynamic: Vec<&str> = sets()
            .iter()
            .filter(|spec| spec.flags & NFT_SET_EVAL != 0)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(dynamic, [set::CONNS4, set::CONNS6]);
        let network = |conntrack_limit| NetworkRules {
            bridge: "shy-app".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: false,
            isolate_host: false,
            dns: None,
            conntrack_limit,
        };
        let limited: Vec<_> = network_elements(&network(true))
            .into_iter()
            .filter(|(set, _)| *set == set::LIMITED)
            .collect();
        assert_eq!(
            limited,
            [(set::LIMITED, vec![Element::key(ifname_key("shy-app"))])]
        );
        assert!(
            network_elements(&network(false))
                .iter()
                .all(|(set, _)| *set != set::LIMITED)
        );
    }

    /// `input`: from an isolated network, replies, its DNS server (IPv4),
    /// IPv6's neighbour discovery, then nothing; its elements: its bridge,
    /// and its gateway's port 53 over UDP and TCP if it has names.
    #[test]
    fn an_isolated_network_reaches_the_host_only_for_its_dns() {
        let input: Vec<Vec<Expr>> = rules()
            .into_iter()
            .filter(|(chain, _)| *chain == chain::INPUT)
            .map(|(_, exprs)| exprs)
            .collect();
        assert_eq!(input.len(), 9);
        // The limit first (see the test before).
        assert_eq!(input[0], link_local_refused());
        assert_eq!(input[1..5], limit_rules());
        let input = &input[5..];
        assert_eq!(input[0].last(), Some(&Expr::Verdict(NF_ACCEPT)));
        assert!(input[1].contains(&Expr::Lookup {
            set: set::DNS_ALLOW.into(),
            sreg: reg(0),
            dreg: None,
            invert: false,
        }));
        assert_eq!(input[1].last(), Some(&Expr::Verdict(NF_ACCEPT)));
        assert_eq!(input[2].last(), Some(&Expr::Verdict(NF_ACCEPT)));
        // Neighbour solicitations and advertisements only (135 and 136).
        for (op, kind) in [(NFT_CMP_GTE, 135), (NFT_CMP_LTE, 136)] {
            assert!(input[2].contains(&Expr::Cmp {
                sreg: reg(0),
                op,
                data: vec![kind],
            }));
        }
        assert_eq!(
            input[3],
            [
                interface_in(NFT_META_IIFNAME, set::ISOLATED, false),
                vec![Expr::Verdict(NF_DROP)]
            ]
            .concat()
        );
        let isolated = |dns| NetworkRules {
            bridge: "shy-app".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: false,
            isolate_host: true,
            conntrack_limit: false,
            dns,
        };
        let with_dns = network_elements(&isolated(Some(Ipv4Addr::new(10, 89, 0, 1))));
        let allowed: Vec<&Element> = with_dns
            .iter()
            .filter(|(set, _)| *set == set::DNS_ALLOW)
            .flat_map(|(_, elements)| elements)
            .collect();
        // The bridge, the gateway, then UDP's and TCP's port 53, each field
        // in whole registers.
        let key = |protocol: u8| {
            let mut key = ifname_key("shy-app");
            key.extend([10, 89, 0, 1, protocol, 0, 0, 0, 0, 53, 0, 0]);
            key
        };
        let keys: Vec<&Vec<u8>> = allowed.iter().map(|element| &element.key).collect();
        assert_eq!(keys, [&key(17), &key(6)]);
        assert!(allowed.iter().all(|element| element.data.is_none()));
        assert!(with_dns.iter().any(|(set, _)| *set == set::ISOLATED));
        // No names: kept from the host whole.
        let without = network_elements(&isolated(None));
        assert!(without.iter().all(|(set, _)| *set != set::DNS_ALLOW));
        // Not isolated: none of it.
        let mut open = isolated(Some(Ipv4Addr::new(10, 89, 0, 1)));
        open.isolate_host = false;
        assert!(
            network_elements(&open)
                .iter()
                .all(|(set, _)| *set != set::ISOLATED && *set != set::DNS_ALLOW)
        );
    }

    #[test]
    fn only_dnated_traffic_reaches_a_bridge_and_internal_networks_are_closed() {
        let forward: Vec<Vec<Expr>> = rules()
            .into_iter()
            .filter(|(chain, _)| *chain == "forward")
            .map(|(_, exprs)| exprs)
            .collect();
        assert_eq!(forward.len(), 18);
        // What conntrack finds invalid on a bridge first, then replies, an
        // isolated network's to the metadata addresses, the limit, what stays on a
        // bridge, the internal networks, DNAT, the rest, then routing for
        // others if closed.
        let invalid = ct_bits(NFT_CT_STATE, CT_STATE_INVALID, true);
        for (rule, key) in forward[..2]
            .iter()
            .zip([NFT_META_IIFNAME, NFT_META_OIFNAME])
        {
            assert_eq!(
                *rule,
                [
                    interface_in(key, set::BRIDGES, false),
                    invalid.clone(),
                    vec![Expr::Verdict(NF_DROP)]
                ]
                .concat()
            );
        }
        assert_eq!(forward[2].last(), Some(&Expr::Verdict(NF_ACCEPT)));
        assert_eq!(forward[3..8], metadata_rules());
        assert_eq!(forward[8..12], limit_rules());
        let forward = &forward[11..];
        assert_eq!(
            forward[1],
            [on_one_bridge(), vec![Expr::Verdict(NF_ACCEPT)]].concat()
        );
        assert_eq!(
            forward[2][1],
            Expr::Lookup {
                set: set::INTERNAL.into(),
                sreg: reg(0),
                dreg: None,
                invert: false
            }
        );
        assert_eq!(forward[4].last(), Some(&Expr::Verdict(NF_ACCEPT)));
        assert_eq!(forward[5].last(), Some(&Expr::Verdict(NF_DROP)));
        // Routing closed: between the host's own interfaces only.
        assert_eq!(
            forward[6],
            [
                routing_closed(),
                interface_in(NFT_META_IIFNAME, set::UPLINKS, false),
                interface_in(NFT_META_OIFNAME, set::UPLINKS, false),
                vec![Expr::Verdict(NF_DROP)],
            ]
            .concat()
        );
    }

    #[test]
    fn the_table_is_built_in_order_and_every_item_encodes() {
        let items = install(
            &[NetworkRules {
                bridge: "shipyard0".into(),
                subnet: "10.88.0.0/16".parse().unwrap(),
                subnet6: None,
                internal: false,
                isolate_host: false,
                conntrack_limit: false,
                dns: None,
            }],
            &[rule(Published {
                host_ip: None,
                host_port: 8080,
                container_port: 80,
                protocol: Protocol::Tcp,
            })],
            &ClosedRouting {
                families: vec![Family::V4, Family::V6],
                interfaces: vec!["eth0".into()],
            },
        );
        let what: Vec<&str> = items.iter().map(|item| item.what.as_str()).collect();
        assert_eq!(what[0], "adding the table shipyard");
        assert_eq!(what[1], "adding the set bridges");
        assert_eq!(what[17], "adding the chain guard");
        assert!(what.contains(&"adding rule 18 of the chain forward"));
        assert!(what.contains(&"adding rule 9 of the chain input"));
        assert_eq!(
            what[what.len() - 5..],
            [
                "adding to the set ports",
                "adding to the set uplinks",
                "adding to the set closed",
                "adding to the set uplinks",
                "adding to the set closed",
            ]
        );
        for item in &items {
            let bytes = item.message.finish(1, 1).unwrap();
            let message = Messages::new(&bytes).next().unwrap().unwrap();
            assert_eq!(message.kind >> 8, 10, "{}", item.what);
            assert_eq!(&message.payload[..4], &[FAMILY, 0, 0, 0], "{}", item.what);
        }
    }
}
