//! `nf_tables` numbers (`linux/netfilter/nf_tables.h`, `nfnetlink.h`,
//! `nfnetlink_conntrack.h`), ours since macOS has none: the Linux test at the
//! end checks them against libc's wherever libc has them.
//!
//! Every 32-bit number `nf_tables` carries in an attribute is big-endian; the
//! values loaded into its registers (`ct state`, a `fib` result) are the
//! host's.

/// nfnetlink subsystems: the high byte of a message's type.
pub(crate) const NFNL_SUBSYS_CTNETLINK: u16 = 1;
pub(crate) const NFNL_SUBSYS_NFTABLES: u16 = 10;

/// `nf_tables` message types (the low byte).
pub(crate) const NFT_MSG_NEWTABLE: u16 = 0;
pub(crate) const NFT_MSG_GETTABLE: u16 = 1;
pub(crate) const NFT_MSG_DELTABLE: u16 = 2;
pub(crate) const NFT_MSG_NEWCHAIN: u16 = 3;
pub(crate) const NFT_MSG_GETCHAIN: u16 = 4;
pub(crate) const NFT_MSG_NEWRULE: u16 = 6;
pub(crate) const NFT_MSG_GETRULE: u16 = 7;
pub(crate) const NFT_MSG_NEWSET: u16 = 9;
pub(crate) const NFT_MSG_NEWSETELEM: u16 = 12;
pub(crate) const NFT_MSG_GETSETELEM: u16 = 13;
pub(crate) const NFT_MSG_DELSETELEM: u16 = 14;

/// Tables.
pub(crate) const NFTA_TABLE_NAME: u16 = 1;
pub(crate) const NFTA_TABLE_USERDATA: u16 = 6;

/// Chains.
pub(crate) const NFTA_CHAIN_TABLE: u16 = 1;
pub(crate) const NFTA_CHAIN_NAME: u16 = 3;
pub(crate) const NFTA_CHAIN_HOOK: u16 = 4;
pub(crate) const NFTA_CHAIN_POLICY: u16 = 5;
pub(crate) const NFTA_CHAIN_TYPE: u16 = 7;
pub(crate) const NFTA_HOOK_HOOKNUM: u16 = 1;
pub(crate) const NFTA_HOOK_PRIORITY: u16 = 2;

/// Rules and their expressions.
pub(crate) const NFTA_RULE_TABLE: u16 = 1;
pub(crate) const NFTA_RULE_CHAIN: u16 = 2;
pub(crate) const NFTA_RULE_EXPRESSIONS: u16 = 4;
pub(crate) const NFTA_LIST_ELEM: u16 = 1;
pub(crate) const NFTA_EXPR_NAME: u16 = 1;
pub(crate) const NFTA_EXPR_DATA: u16 = 2;

/// Sets and maps.
pub(crate) const NFTA_SET_TABLE: u16 = 1;
pub(crate) const NFTA_SET_NAME: u16 = 2;
pub(crate) const NFTA_SET_FLAGS: u16 = 3;
pub(crate) const NFTA_SET_KEY_TYPE: u16 = 4;
pub(crate) const NFTA_SET_KEY_LEN: u16 = 5;
pub(crate) const NFTA_SET_DATA_TYPE: u16 = 6;
pub(crate) const NFTA_SET_DATA_LEN: u16 = 7;
/// Every new set needs one, unique in its batch.
pub(crate) const NFTA_SET_ID: u16 = 10;
pub(crate) const NFTA_SET_USERDATA: u16 = 13;
pub(crate) const NFT_SET_INTERVAL: u32 = 0x4;
pub(crate) const NFT_SET_MAP: u32 = 0x8;

/// Set elements.
pub(crate) const NFTA_SET_ELEM_LIST_TABLE: u16 = 1;
pub(crate) const NFTA_SET_ELEM_LIST_SET: u16 = 2;
pub(crate) const NFTA_SET_ELEM_LIST_ELEMENTS: u16 = 3;
pub(crate) const NFTA_SET_ELEM_KEY: u16 = 1;
pub(crate) const NFTA_SET_ELEM_DATA: u16 = 2;
pub(crate) const NFTA_SET_ELEM_FLAGS: u16 = 3;
pub(crate) const NFT_SET_ELEM_INTERVAL_END: u32 = 0x1;

/// Data: a value or a verdict.
pub(crate) const NFTA_DATA_VALUE: u16 = 1;
pub(crate) const NFTA_DATA_VERDICT: u16 = 2;
pub(crate) const NFTA_VERDICT_CODE: u16 = 1;

/// Verdicts (`NF_DROP`, `NF_ACCEPT`).
pub(crate) const NF_DROP: i32 = 0;
pub(crate) const NF_ACCEPT: i32 = 1;

/// Registers: the verdict, and the 32-bit ones (`NFT_REG32_00` on).
pub(crate) const NFT_REG_VERDICT: u32 = 0;
pub(crate) const NFT_REG32_00: u32 = 8;

/// Expressions' attributes.
pub(crate) const NFTA_META_DREG: u16 = 1;
pub(crate) const NFTA_META_KEY: u16 = 2;
pub(crate) const NFTA_PAYLOAD_DREG: u16 = 1;
pub(crate) const NFTA_PAYLOAD_BASE: u16 = 2;
pub(crate) const NFTA_PAYLOAD_OFFSET: u16 = 3;
pub(crate) const NFTA_PAYLOAD_LEN: u16 = 4;
pub(crate) const NFTA_CMP_SREG: u16 = 1;
pub(crate) const NFTA_CMP_OP: u16 = 2;
pub(crate) const NFTA_CMP_DATA: u16 = 3;
pub(crate) const NFTA_BITWISE_SREG: u16 = 1;
pub(crate) const NFTA_BITWISE_DREG: u16 = 2;
pub(crate) const NFTA_BITWISE_LEN: u16 = 3;
pub(crate) const NFTA_BITWISE_MASK: u16 = 4;
pub(crate) const NFTA_BITWISE_XOR: u16 = 5;
pub(crate) const NFTA_LOOKUP_SET: u16 = 1;
pub(crate) const NFTA_LOOKUP_SREG: u16 = 2;
pub(crate) const NFTA_LOOKUP_DREG: u16 = 3;
pub(crate) const NFTA_LOOKUP_FLAGS: u16 = 5;
pub(crate) const NFT_LOOKUP_F_INV: u32 = 1;
pub(crate) const NFTA_IMMEDIATE_DREG: u16 = 1;
pub(crate) const NFTA_IMMEDIATE_DATA: u16 = 2;
pub(crate) const NFTA_NAT_TYPE: u16 = 1;
pub(crate) const NFTA_NAT_FAMILY: u16 = 2;
pub(crate) const NFTA_NAT_REG_ADDR_MIN: u16 = 3;
pub(crate) const NFTA_NAT_REG_PROTO_MIN: u16 = 5;
pub(crate) const NFTA_FIB_DREG: u16 = 1;
pub(crate) const NFTA_FIB_RESULT: u16 = 2;
pub(crate) const NFTA_FIB_FLAGS: u16 = 3;
pub(crate) const NFTA_CT_DREG: u16 = 1;
pub(crate) const NFTA_CT_KEY: u16 = 2;

/// Expressions' values.
pub(crate) const NFT_META_PROTOCOL: u32 = 1;
pub(crate) const NFT_META_NFPROTO: u32 = 15;
pub(crate) const NFT_META_L4PROTO: u32 = 16;
pub(crate) const NFT_META_IIFNAME: u32 = 6;
pub(crate) const NFT_META_OIFNAME: u32 = 7;
pub(crate) const NFT_PAYLOAD_LL_HEADER: u32 = 0;
pub(crate) const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
pub(crate) const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
pub(crate) const NFT_CMP_EQ: u32 = 0;
pub(crate) const NFT_CMP_NEQ: u32 = 1;
pub(crate) const NFT_NAT_DNAT: u32 = 1;
pub(crate) const NFT_FIB_RESULT_ADDRTYPE: u32 = 3;
pub(crate) const NFTA_FIB_F_DADDR: u32 = 2;
pub(crate) const NFT_CT_STATE: u32 = 0;
pub(crate) const NFT_CT_STATUS: u32 = 2;

/// A route type: the address is one of this host's (`RTN_LOCAL`).
pub(crate) const RTN_LOCAL: u32 = 2;
/// `ct state` bits: invalid, established and related.
pub(crate) const CT_STATE_INVALID: u32 = 1;
pub(crate) const CT_STATE_ESTABLISHED: u32 = 1 << 1;
pub(crate) const CT_STATE_RELATED: u32 = 1 << 2;
/// `ct status` bit: the connection's destination was NAT'd (`IPS_DST_NAT`).
pub(crate) const IPS_DST_NAT: u32 = 1 << 5;

/// Protocol families (`NFPROTO_*`) and netfilter hooks (`NF_INET_*`).
pub(crate) const NFPROTO_UNSPEC: u8 = 0;
pub(crate) const NFPROTO_INET: u8 = 1;
pub(crate) const NFPROTO_IPV4: u8 = 2;
pub(crate) const NFPROTO_ARP: u8 = 3;
pub(crate) const NFPROTO_NETDEV: u8 = 5;
pub(crate) const NFPROTO_BRIDGE: u8 = 7;
pub(crate) const NFPROTO_IPV6: u8 = 10;
pub(crate) const NF_INET_PRE_ROUTING: u32 = 0;
pub(crate) const NF_INET_LOCAL_IN: u32 = 1;
pub(crate) const NF_INET_FORWARD: u32 = 2;
pub(crate) const NF_INET_LOCAL_OUT: u32 = 3;
pub(crate) const NF_INET_POST_ROUTING: u32 = 4;
/// The bridge's first hook: a frame as it enters the bridge, from a port.
pub(crate) const NF_BR_PRE_ROUTING: u32 = 0;

/// Ethertypes (`ETH_P_*`), as `meta protocol` holds them.
pub(crate) const ETH_P_IP: u16 = 0x0800;
pub(crate) const ETH_P_ARP: u16 = 0x0806;
pub(crate) const ETH_P_IPV6: u16 = 0x86dd;

/// nft's own data types (`datatype.h`), recorded with a set so `nft list`
/// shows its keys properly; the kernel only keeps them. A concatenation's
/// type is its parts' types, 6 bits each.
pub(crate) const TYPE_NFPROTO: u32 = 2;
pub(crate) const TYPE_IPADDR: u32 = 7;
pub(crate) const TYPE_IP6ADDR: u32 = 8;
pub(crate) const TYPE_ETHERADDR: u32 = 9;
pub(crate) const TYPE_INET_PROTOCOL: u32 = 12;
pub(crate) const TYPE_INET_SERVICE: u32 = 13;
pub(crate) const TYPE_IFNAME: u32 = 41;
const TYPE_BITS: u32 = 6;

/// The type of the concatenation of `types`, as nft computes it.
pub(crate) fn concat_type(types: &[u32]) -> u32 {
    types.iter().fold(0, |all, &part| (all << TYPE_BITS) | part)
}

/// An interface name's size, with its NUL (`IFNAMSIZ`).
pub(crate) const IFNAMSIZ: usize = 16;

/// ctnetlink (`nfnetlink_conntrack.h`).
pub(crate) const IPCTNL_MSG_CT_GET: u16 = 1;
pub(crate) const IPCTNL_MSG_CT_DELETE: u16 = 2;
pub(crate) const CTA_TUPLE_ORIG: u16 = 1;
pub(crate) const CTA_TUPLE_REPLY: u16 = 2;
pub(crate) const CTA_ZONE: u16 = 18;
pub(crate) const CTA_TUPLE_IP: u16 = 1;
pub(crate) const CTA_IP_V4_SRC: u16 = 1;
#[cfg(test)]
pub(crate) const CTA_IP_V4_DST: u16 = 2;
pub(crate) const CTA_IP_V6_SRC: u16 = 3;
#[cfg(test)]
pub(crate) const CTA_IP_V6_DST: u16 = 4;

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    /// Where libc has them, they're the kernel's.
    #[test]
    fn they_are_the_kernels() {
        let pairs: &[(i64, i64)] = &[
            (
                NFNL_SUBSYS_CTNETLINK.into(),
                libc::NFNL_SUBSYS_CTNETLINK.into(),
            ),
            (
                NFNL_SUBSYS_NFTABLES.into(),
                libc::NFNL_SUBSYS_NFTABLES.into(),
            ),
            (NFT_MSG_NEWTABLE.into(), libc::NFT_MSG_NEWTABLE.into()),
            (NFT_MSG_GETTABLE.into(), libc::NFT_MSG_GETTABLE.into()),
            (NFT_MSG_DELTABLE.into(), libc::NFT_MSG_DELTABLE.into()),
            (NFT_MSG_NEWCHAIN.into(), libc::NFT_MSG_NEWCHAIN.into()),
            (NFT_MSG_GETCHAIN.into(), libc::NFT_MSG_GETCHAIN.into()),
            (NFT_MSG_NEWRULE.into(), libc::NFT_MSG_NEWRULE.into()),
            (NFT_MSG_GETRULE.into(), libc::NFT_MSG_GETRULE.into()),
            (NFT_MSG_NEWSET.into(), libc::NFT_MSG_NEWSET.into()),
            (NFT_MSG_NEWSETELEM.into(), libc::NFT_MSG_NEWSETELEM.into()),
            (NFT_MSG_GETSETELEM.into(), libc::NFT_MSG_GETSETELEM.into()),
            (NFT_MSG_DELSETELEM.into(), libc::NFT_MSG_DELSETELEM.into()),
            (NFT_SET_INTERVAL.into(), libc::NFT_SET_INTERVAL.into()),
            (NFT_SET_MAP.into(), libc::NFT_SET_MAP.into()),
            (
                NFT_SET_ELEM_INTERVAL_END.into(),
                libc::NFT_SET_ELEM_INTERVAL_END.into(),
            ),
            (NF_DROP.into(), libc::NF_DROP.into()),
            (NF_ACCEPT.into(), libc::NF_ACCEPT.into()),
            (NFT_REG_VERDICT.into(), libc::NFT_REG_VERDICT.into()),
            (NFT_REG32_00.into(), libc::NFT_REG32_00.into()),
            (NFT_LOOKUP_F_INV.into(), libc::NFT_LOOKUP_F_INV.into()),
            (NFT_META_NFPROTO.into(), libc::NFT_META_NFPROTO.into()),
            (NFT_META_L4PROTO.into(), libc::NFT_META_L4PROTO.into()),
            (NFT_META_IIFNAME.into(), libc::NFT_META_IIFNAME.into()),
            (NFT_META_PROTOCOL.into(), libc::NFT_META_PROTOCOL.into()),
            (
                NFT_PAYLOAD_LL_HEADER.into(),
                libc::NFT_PAYLOAD_LL_HEADER.into(),
            ),
            (NF_BR_PRE_ROUTING.into(), libc::NF_BR_PRE_ROUTING.into()),
            (ETH_P_IP.into(), libc::ETH_P_IP.into()),
            (ETH_P_ARP.into(), libc::ETH_P_ARP.into()),
            (ETH_P_IPV6.into(), libc::ETH_P_IPV6.into()),
            (NFT_META_OIFNAME.into(), libc::NFT_META_OIFNAME.into()),
            (
                NFT_PAYLOAD_NETWORK_HEADER.into(),
                libc::NFT_PAYLOAD_NETWORK_HEADER.into(),
            ),
            (
                NFT_PAYLOAD_TRANSPORT_HEADER.into(),
                libc::NFT_PAYLOAD_TRANSPORT_HEADER.into(),
            ),
            (NFT_CMP_EQ.into(), libc::NFT_CMP_EQ.into()),
            (NFT_CMP_NEQ.into(), libc::NFT_CMP_NEQ.into()),
            (NFT_NAT_DNAT.into(), libc::NFT_NAT_DNAT.into()),
            (NFT_CT_STATE.into(), libc::NFT_CT_STATE.into()),
            (NFT_CT_STATUS.into(), libc::NFT_CT_STATUS.into()),
            (RTN_LOCAL.into(), libc::RTN_LOCAL.into()),
            (NFPROTO_UNSPEC.into(), libc::NFPROTO_UNSPEC.into()),
            (NFPROTO_INET.into(), libc::NFPROTO_INET.into()),
            (NFPROTO_IPV4.into(), libc::NFPROTO_IPV4.into()),
            (NFPROTO_ARP.into(), libc::NFPROTO_ARP.into()),
            (NFPROTO_NETDEV.into(), libc::NFPROTO_NETDEV.into()),
            (NFPROTO_BRIDGE.into(), libc::NFPROTO_BRIDGE.into()),
            (NFPROTO_IPV6.into(), libc::NFPROTO_IPV6.into()),
            (NF_INET_PRE_ROUTING.into(), libc::NF_INET_PRE_ROUTING.into()),
            (NF_INET_LOCAL_IN.into(), libc::NF_INET_LOCAL_IN.into()),
            (NF_INET_FORWARD.into(), libc::NF_INET_FORWARD.into()),
            (NF_INET_LOCAL_OUT.into(), libc::NF_INET_LOCAL_OUT.into()),
            (
                NF_INET_POST_ROUTING.into(),
                libc::NF_INET_POST_ROUTING.into(),
            ),
            (
                i64::try_from(IFNAMSIZ).unwrap(),
                i64::try_from(libc::IFNAMSIZ).unwrap(),
            ),
        ];
        for (index, (ours, theirs)) in pairs.iter().enumerate() {
            assert_eq!(ours, theirs, "pair {index}");
        }
    }
}
