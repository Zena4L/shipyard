//! The expressions the rules are made of: matches on a packet's family,
//! interfaces, addresses, ports and connection, each a short program of
//! `nf_tables` expressions.

use std::net::Ipv6Addr;

use super::super::consts::{
    NFPROTO_IPV4, NFPROTO_IPV6, NFT_CMP_EQ, NFT_CMP_NEQ, NFT_FIB_RESULT_ADDRTYPE, NFT_META_IIFNAME,
    NFT_META_L4PROTO, NFT_META_NFPROTO, NFT_META_OIFNAME, NFT_NAT_DNAT, NFT_PAYLOAD_NETWORK_HEADER,
    NFT_PAYLOAD_TRANSPORT_HEADER, NFTA_FIB_F_DADDR, RTN_LOCAL,
};
use super::super::expr::Expr;
use super::{ifname_key, reg, set};
use crate::ipnet::Family;

/// `meta nfproto ipv4`.
pub(super) fn ipv4() -> Vec<Expr> {
    nfproto(NFPROTO_IPV4)
}

/// `meta nfproto ipv6`.
pub(super) fn ipv6() -> Vec<Expr> {
    nfproto(NFPROTO_IPV6)
}

/// `meta nfproto` is `family`.
pub(super) fn nfproto(family: u8) -> Vec<Expr> {
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
pub(super) fn interface_in(key: u32, set: &str, invert: bool) -> Vec<Expr> {
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
pub(super) fn address(offset: u32, dreg: u32) -> Expr {
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
pub(super) fn address6(offset: u32, dreg: u32) -> Expr {
    network_header(offset, 16, dreg)
}

/// `ip6 daddr != ::1`. The host's own client of `[::1]:PORT` isn't DNAT'd:
/// a packet from `::1` can't leave `lo` (a container drops it, and the
/// client would wait), so it meets the held port, and is refused at once.
pub(super) fn not_to_loopback6() -> Vec<Expr> {
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
pub(super) fn loopback(offset: u32) -> Vec<Expr> {
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
pub(super) fn ct_bits(key: u32, bits: u32, set: bool) -> Vec<Expr> {
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
pub(super) fn for_this_host() -> Vec<Expr> {
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
pub(super) fn protocol_and_port(proto: u32) -> [Expr; 2] {
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
pub(super) fn dnat_through(set: &str, family: Family) -> [Expr; 2] {
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

/// `iifname != "lo"`.
pub(super) fn not_from_loopback() -> Vec<Expr> {
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
pub(super) fn on_one_bridge() -> Vec<Expr> {
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
pub(super) fn routing_closed() -> Vec<Expr> {
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
