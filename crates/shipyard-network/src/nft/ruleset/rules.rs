//! The chains' rules: `guard`, `dstnat` and `output`, `forward`, `srcnat`
//! and `input`, and the groups they share (the connection limit, the
//! metadata drops).

use super::super::consts::{
    CT_STATE_ESTABLISHED, CT_STATE_INVALID, CT_STATE_RELATED, IPPROTO_ICMPV6, IPS_CONFIRMED,
    IPS_DST_NAT, NF_ACCEPT, NF_DROP, NFT_CMP_EQ, NFT_CMP_GTE, NFT_CMP_LTE, NFT_CMP_NEQ,
    NFT_CT_STATE, NFT_CT_STATUS, NFT_DYNSET_OP_ADD, NFT_META_IIFNAME, NFT_META_L4PROTO,
    NFT_META_OIFNAME, NFT_PAYLOAD_TRANSPORT_HEADER, NFT_REJECT_ICMPX_ADMIN_PROHIBITED,
    NFT_REJECT_ICMPX_UNREACH, NFT_REJECT_TCP_RST,
};
use super::super::expr::Expr;
use super::exprs::{
    address, address6, ct_bits, dnat_through, for_this_host, interface_in, ipv4, ipv6, loopback,
    not_from_loopback, not_to_loopback6, on_one_bridge, protocol_and_port, routing_closed,
};
use super::{
    CONNTRACK_LIMIT, LINK_LOCAL4, LINK_LOCAL6, METADATA4, METADATA6, ND_FIRST, ND_LAST, chain, reg,
    set,
};
use crate::ipnet::Family;

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
pub(super) fn limit_rules() -> [Vec<Expr>; 4] {
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
pub(super) fn link_local_refused() -> Vec<Expr> {
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
pub(super) fn metadata_rules() -> [Vec<Expr>; 5] {
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
fn forward_rules() -> [Vec<Expr>; 19] {
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
        // A published port reached from a link no one outside the host may
        // reach it from (the macOS guest's NAT link: F108), never accepted.
        [
            interface_in(NFT_META_IIFNAME, set::OUTSIDE, false),
            ct_bits(NFT_CT_STATUS, IPS_DST_NAT, true),
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
