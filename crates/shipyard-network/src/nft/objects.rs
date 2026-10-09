//! `nf_tables` objects as requests: tables, chains, rules, sets and their
//! elements, each a [`BatchItem`] (`nf_tables` changes only come in batches).
//!
//! A *table* holds chains and sets, in an address family (`inet`: IPv4 and
//! IPv6). A *base chain* is attached to a netfilter hook (where the kernel
//! hands it packets: prerouting, forward, …) at a priority (lower runs
//! first), with a type (`filter`, or `nat`, whose rules see a connection's
//! first packet only) and a policy for packets no rule decided. A *set*
//! holds keys; a *map* gives each key a value.

use super::consts::{
    NFNL_SUBSYS_NFTABLES, NFT_MSG_DELSETELEM, NFT_MSG_DELTABLE, NFT_MSG_NEWCHAIN, NFT_MSG_NEWRULE,
    NFT_MSG_NEWSET, NFT_MSG_NEWSETELEM, NFT_MSG_NEWTABLE, NFT_SET_ELEM_INTERVAL_END, NFT_SET_MAP,
    NFTA_CHAIN_HOOK, NFTA_CHAIN_NAME, NFTA_CHAIN_POLICY, NFTA_CHAIN_TABLE, NFTA_CHAIN_TYPE,
    NFTA_DATA_VALUE, NFTA_HOOK_HOOKNUM, NFTA_HOOK_PRIORITY, NFTA_LIST_ELEM, NFTA_RULE_CHAIN,
    NFTA_RULE_EXPRESSIONS, NFTA_RULE_TABLE, NFTA_SET_DATA_LEN, NFTA_SET_DATA_TYPE,
    NFTA_SET_ELEM_DATA, NFTA_SET_ELEM_FLAGS, NFTA_SET_ELEM_KEY, NFTA_SET_ELEM_LIST_ELEMENTS,
    NFTA_SET_ELEM_LIST_SET, NFTA_SET_ELEM_LIST_TABLE, NFTA_SET_FLAGS, NFTA_SET_ID,
    NFTA_SET_KEY_LEN, NFTA_SET_KEY_TYPE, NFTA_SET_NAME, NFTA_SET_TABLE, NFTA_TABLE_NAME,
    NFTA_TABLE_USERDATA,
};
use super::expr::Expr;
use crate::netlink::{BatchItem, MessageBuilder, flag, nfnl_message};

/// A message of `nf_tables` type `msg` about `family`.
pub(crate) fn message(msg: u16, flags: u16, family: u8) -> MessageBuilder {
    nfnl_message(NFNL_SUBSYS_NFTABLES, msg, flags, family)
}

/// A base chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseChain {
    /// Its name.
    pub(crate) name: &'static str,
    /// `filter` or `nat`.
    pub(crate) kind: &'static str,
    /// The netfilter hook (`NF_INET_*`).
    pub(crate) hook: u32,
    /// Its priority at the hook.
    pub(crate) priority: i32,
    /// What happens to a packet no rule decided (`NF_ACCEPT`, `NF_DROP`).
    pub(crate) policy: i32,
}

/// A set or a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetSpec {
    /// Its name.
    pub(crate) name: &'static str,
    /// `NFT_SET_*` flags (`MAP` is added for a map).
    pub(crate) flags: u32,
    /// Its key's type (nft's, for `nft list`) and length.
    pub(crate) key: (u32, u32),
    /// A map's value's type and length.
    pub(crate) data: Option<(u32, u32)>,
}

/// A set element: a key, a map's value, and whether it ends an interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Its key.
    pub key: Vec<u8>,
    /// A map's value.
    pub data: Option<Vec<u8>>,
    /// It ends an interval.
    pub interval_end: bool,
}

impl Element {
    /// A set's key.
    pub(crate) fn key(key: Vec<u8>) -> Self {
        Element {
            key,
            data: None,
            interval_end: false,
        }
    }
}

/// Creates table `name` (or leaves it, if it's there), with `userdata`.
pub(crate) fn new_table(family: u8, name: &str, userdata: &[u8]) -> BatchItem {
    let mut message = message(NFT_MSG_NEWTABLE, flag::CREATE, family);
    message
        .attr_str(NFTA_TABLE_NAME, name)
        .attr(NFTA_TABLE_USERDATA, userdata);
    BatchItem {
        what: format!("adding the table {name}"),
        message,
    }
}

/// Deletes table `name` with everything in it.
pub(crate) fn delete_table(family: u8, name: &str) -> BatchItem {
    let mut message = message(NFT_MSG_DELTABLE, 0, family);
    message.attr_str(NFTA_TABLE_NAME, name);
    BatchItem {
        what: format!("deleting the table {name}"),
        message,
    }
}

/// Creates `chain` in `table`.
pub(crate) fn new_chain(family: u8, table: &str, chain: &BaseChain) -> BatchItem {
    let mut message = message(NFT_MSG_NEWCHAIN, flag::CREATE, family);
    message
        .attr_str(NFTA_CHAIN_TABLE, table)
        .attr_str(NFTA_CHAIN_NAME, chain.name)
        .nest(NFTA_CHAIN_HOOK, |hook| {
            hook.attr_be32(NFTA_HOOK_HOOKNUM, chain.hook)
                .attr(NFTA_HOOK_PRIORITY, &chain.priority.to_be_bytes());
        })
        .attr_str(NFTA_CHAIN_TYPE, chain.kind)
        .attr(NFTA_CHAIN_POLICY, &chain.policy.to_be_bytes());
    BatchItem {
        what: format!("adding the chain {}", chain.name),
        message,
    }
}

/// Appends a rule of `exprs` to `chain`; `number` names it in errors.
pub(crate) fn new_rule(
    family: u8,
    table: &str,
    chain: &str,
    number: usize,
    exprs: &[Expr],
) -> BatchItem {
    let mut message = message(NFT_MSG_NEWRULE, flag::CREATE | flag::APPEND, family);
    message
        .attr_str(NFTA_RULE_TABLE, table)
        .attr_str(NFTA_RULE_CHAIN, chain)
        .nest(NFTA_RULE_EXPRESSIONS, |list| {
            for expr in exprs {
                expr.encode(list);
            }
        });
    BatchItem {
        what: format!("adding rule {number} of the chain {chain}"),
        message,
    }
}

/// Creates `set` in `table`; `id` is unique in the batch.
pub(crate) fn new_set(family: u8, table: &str, set: &SetSpec, id: u32) -> BatchItem {
    let mut message = message(NFT_MSG_NEWSET, flag::CREATE, family);
    let flags = set.flags | if set.data.is_some() { NFT_SET_MAP } else { 0 };
    message
        .attr_str(NFTA_SET_TABLE, table)
        .attr_str(NFTA_SET_NAME, set.name)
        .attr_be32(NFTA_SET_FLAGS, flags)
        .attr_be32(NFTA_SET_KEY_TYPE, set.key.0)
        .attr_be32(NFTA_SET_KEY_LEN, set.key.1);
    if let Some((kind, len)) = set.data {
        message
            .attr_be32(NFTA_SET_DATA_TYPE, kind)
            .attr_be32(NFTA_SET_DATA_LEN, len);
    }
    message.attr_be32(NFTA_SET_ID, id);
    BatchItem {
        what: format!("adding the set {}", set.name),
        message,
    }
}

/// Adds `elements` to `set` (with `exclusive`, one that's there already is
/// an error, EEXIST; without, it's left as it is).
pub(crate) fn add_elements(
    family: u8,
    table: &str,
    set: &str,
    elements: &[Element],
    exclusive: bool,
) -> BatchItem {
    let flags = flag::CREATE | if exclusive { flag::EXCL } else { 0 };
    BatchItem {
        what: format!("adding to the set {set}"),
        message: elements_message(NFT_MSG_NEWSETELEM, flags, family, table, set, elements),
    }
}

/// Deletes `elements` from `set` (one that isn't there is an error,
/// ENOENT).
pub(crate) fn delete_elements(
    family: u8,
    table: &str,
    set: &str,
    elements: &[Element],
) -> BatchItem {
    BatchItem {
        what: format!("removing from the set {set}"),
        message: elements_message(NFT_MSG_DELSETELEM, 0, family, table, set, elements),
    }
}

fn elements_message(
    msg: u16,
    flags: u16,
    family: u8,
    table: &str,
    set: &str,
    elements: &[Element],
) -> MessageBuilder {
    let mut message = message(msg, flags, family);
    message
        .attr_str(NFTA_SET_ELEM_LIST_TABLE, table)
        .attr_str(NFTA_SET_ELEM_LIST_SET, set)
        .nest(NFTA_SET_ELEM_LIST_ELEMENTS, |list| {
            for element in elements {
                list.nest(NFTA_LIST_ELEM, |item| {
                    item.nest(NFTA_SET_ELEM_KEY, |key| {
                        key.attr(NFTA_DATA_VALUE, &element.key);
                    });
                    if let Some(data) = &element.data {
                        item.nest(NFTA_SET_ELEM_DATA, |value| {
                            value.attr(NFTA_DATA_VALUE, data);
                        });
                    }
                    if element.interval_end {
                        item.attr_be32(NFTA_SET_ELEM_FLAGS, NFT_SET_ELEM_INTERVAL_END);
                    }
                });
            }
        });
    message
}
