//! What `nf_tables` lists, read back: a table's user data, chains, and set
//! elements. Bounded like every parser here (the fuzz target `netlink`
//! feeds these too).

/// `NF_DROP` as a chain's (unsigned) policy.
const NF_DROP_POLICY: u32 = 0;

use super::consts::{
    NF_INET_FORWARD, NF_INET_LOCAL_IN, NFPROTO_ARP, NFPROTO_BRIDGE, NFPROTO_INET, NFPROTO_IPV4,
    NFPROTO_IPV6, NFPROTO_NETDEV, NFT_SET_ELEM_INTERVAL_END, NFTA_CHAIN_HOOK, NFTA_CHAIN_NAME,
    NFTA_CHAIN_POLICY, NFTA_CHAIN_TABLE, NFTA_CHAIN_TYPE, NFTA_DATA_VALUE, NFTA_HOOK_HOOKNUM,
    NFTA_LIST_ELEM, NFTA_SET_ELEM_DATA, NFTA_SET_ELEM_FLAGS, NFTA_SET_ELEM_KEY,
    NFTA_SET_ELEM_LIST_ELEMENTS, NFTA_TABLE_USERDATA,
};
use super::objects::Element;
use crate::netlink::message::{Attr, Attrs};
use crate::netlink::{Message, NFGENMSG_LEN, ParseError};

/// A message's family (from its `nfgenmsg`) and attributes.
fn split<'a>(message: &Message<'a>) -> Result<(u8, Attrs<'a>), ParseError> {
    let (header, attrs) = message.split(NFGENMSG_LEN)?;
    let family = header
        .first()
        .copied()
        .ok_or(ParseError::Missing("family"))?;
    Ok((family, attrs))
}

/// A table's user data (empty if it has none).
pub(crate) fn table_userdata(message: &Message<'_>) -> Result<Vec<u8>, ParseError> {
    let (_, attrs) = split(message)?;
    for attr in attrs {
        let attr = attr?;
        if attr.kind == NFTA_TABLE_USERDATA {
            return Ok(attr.value.to_vec());
        }
    }
    Ok(Vec::new())
}

/// A chain, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    /// Its family (`NFPROTO_*`: 1 inet, 2 ip, 10 ip6, …).
    pub family: u8,
    /// Its table.
    pub table: String,
    /// Its name.
    pub name: String,
    /// Its type (`filter`, `nat`, …), for a base chain.
    pub kind: Option<String>,
    /// Its hook (`NF_INET_*`), for a base chain.
    pub hook: Option<u32>,
    /// Its policy (`NF_ACCEPT` 1, `NF_DROP` 0), for a base chain.
    pub policy: Option<u32>,
}

impl Chain {
    /// Its family's name, as nft writes it.
    #[must_use]
    pub fn family_name(&self) -> &'static str {
        match self.family {
            NFPROTO_INET => "inet",
            NFPROTO_IPV4 => "ip",
            NFPROTO_ARP => "arp",
            NFPROTO_NETDEV => "netdev",
            NFPROTO_BRIDGE => "bridge",
            NFPROTO_IPV6 => "ip6",
            _ => "unknown",
        }
    }

    /// Whether it drops what the host forwards unless a rule lets it
    /// through: a base chain on the forward hook whose policy is drop.
    #[must_use]
    pub fn drops_forwarded(&self) -> bool {
        self.hook == Some(NF_INET_FORWARD) && self.policy == Some(NF_DROP_POLICY)
    }

    /// Whether it drops what comes in to the host unless a rule lets it
    /// through: a base chain on the input hook whose policy is drop.
    #[must_use]
    pub fn drops_input(&self) -> bool {
        self.hook == Some(NF_INET_LOCAL_IN) && self.policy == Some(NF_DROP_POLICY)
    }

    /// Whether it's one of Shipyard's own.
    #[must_use]
    pub fn is_shipyards(&self) -> bool {
        self.family == super::ruleset::FAMILY && self.table == super::ruleset::TABLE
    }
}

/// A chain from a `NEWCHAIN` message.
pub(crate) fn chain(message: &Message<'_>) -> Result<Chain, ParseError> {
    let (family, attrs) = split(message)?;
    let mut chain = Chain {
        family,
        table: String::new(),
        name: String::new(),
        kind: None,
        hook: None,
        policy: None,
    };
    for attr in attrs {
        let attr = attr?;
        match attr.kind {
            NFTA_CHAIN_TABLE => chain.table = lossy(&attr)?,
            NFTA_CHAIN_NAME => chain.name = lossy(&attr)?,
            NFTA_CHAIN_TYPE => chain.kind = Some(lossy(&attr)?),
            NFTA_CHAIN_POLICY => chain.policy = Some(attr.be32()?),
            NFTA_CHAIN_HOOK => {
                for inner in attr.nested() {
                    let inner = inner?;
                    if inner.kind == NFTA_HOOK_HOOKNUM {
                        chain.hook = Some(inner.be32()?);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(chain)
}

/// A name the kernel holds, as text (names from other programs needn't be
/// UTF-8).
fn lossy(attr: &Attr<'_>) -> Result<String, ParseError> {
    Ok(String::from_utf8_lossy(attr.c_bytes()?).into_owned())
}

/// The elements in a `NEWSETELEM` message.
pub(crate) fn elements(message: &Message<'_>) -> Result<Vec<Element>, ParseError> {
    let (_, attrs) = split(message)?;
    let mut found = Vec::new();
    for attr in attrs {
        let attr = attr?;
        if attr.kind != NFTA_SET_ELEM_LIST_ELEMENTS {
            continue;
        }
        for item in attr.nested() {
            let item = item?;
            if item.kind != NFTA_LIST_ELEM {
                continue;
            }
            let mut element = Element {
                key: Vec::new(),
                data: None,
                interval_end: false,
            };
            for field in item.nested() {
                let field = field?;
                match field.kind {
                    NFTA_SET_ELEM_KEY => element.key = value(&field)?,
                    NFTA_SET_ELEM_DATA => element.data = Some(value(&field)?),
                    NFTA_SET_ELEM_FLAGS => {
                        element.interval_end = field.be32()? & NFT_SET_ELEM_INTERVAL_END != 0;
                    }
                    _ => {}
                }
            }
            found.push(element);
        }
    }
    Ok(found)
}

/// The `NFTA_DATA_VALUE` inside `attr`.
fn value(attr: &Attr<'_>) -> Result<Vec<u8>, ParseError> {
    for inner in attr.nested() {
        let inner = inner?;
        if inner.kind == NFTA_DATA_VALUE {
            return Ok(inner.value.to_vec());
        }
    }
    Err(ParseError::Missing("value"))
}

/// The chain of the rule in a `NEWRULE` message (the differential tests
/// against `nft` count rules per chain).
#[cfg(test)]
pub(crate) fn rule_chain(message: &Message<'_>) -> Result<String, ParseError> {
    use super::consts::NFTA_RULE_CHAIN;
    let (_, attrs) = split(message)?;
    for attr in attrs {
        let attr = attr?;
        if attr.kind == NFTA_RULE_CHAIN {
            return lossy(&attr);
        }
    }
    Err(ParseError::Missing("chain"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::netlink::message::Messages;
    use crate::nft::consts::{NFT_MSG_NEWCHAIN, NFT_MSG_NEWTABLE};
    use crate::nft::objects::{BaseChain, Element, add_elements, message, new_chain, new_table};

    /// What the kernel lists looks like what was sent: these are read back
    /// from our own requests.
    fn parsed(bytes: &[u8]) -> Message<'_> {
        Messages::new(bytes).next().unwrap().unwrap()
    }

    #[test]
    fn a_tables_userdata() {
        let bytes = new_table(1, "shipyard", b"v1")
            .message
            .finish(1, 0)
            .unwrap();
        assert_eq!(table_userdata(&parsed(&bytes)).unwrap(), b"v1");
        let bytes = message(NFT_MSG_NEWTABLE, 0, 1).finish(1, 0).unwrap();
        assert!(table_userdata(&parsed(&bytes)).unwrap().is_empty());
    }

    #[test]
    fn a_base_chain() {
        let bytes = new_chain(
            2,
            "filter",
            &BaseChain {
                name: "FORWARD",
                kind: "filter",
                hook: 2,
                priority: 0,
                policy: 0,
            },
        )
        .message
        .finish(1, 0)
        .unwrap();
        assert_eq!(
            chain(&parsed(&bytes)).unwrap(),
            Chain {
                family: 2,
                table: "filter".into(),
                name: "FORWARD".into(),
                kind: Some("filter".into()),
                hook: Some(2),
                policy: Some(0),
            }
        );
        // A regular chain has no hook.
        let bytes = message(NFT_MSG_NEWCHAIN, 0, 2).finish(1, 0).unwrap();
        assert_eq!(chain(&parsed(&bytes)).unwrap().hook, None);
    }

    #[test]
    fn set_elements() {
        let elements = [
            Element::key(vec![1, 2, 3, 4]),
            Element {
                key: vec![5],
                data: Some(vec![6]),
                interval_end: true,
            },
        ];
        let bytes = add_elements(1, "t", "s", &elements, false)
            .message
            .finish(1, 0)
            .unwrap();
        assert_eq!(
            super::elements(&parsed(&bytes)).unwrap(),
            [
                Element {
                    key: vec![1, 2, 3, 4],
                    data: None,
                    interval_end: false
                },
                Element {
                    key: vec![5],
                    data: Some(vec![6]),
                    interval_end: true
                }
            ]
        );
    }
}
