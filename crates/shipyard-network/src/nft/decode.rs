//! What `nf_tables` lists, read back: a table's user data, chains, set
//! elements, and what another program's rules let through. Bounded like every parser here (the fuzz target `netlink`
//! feeds these too).

/// `NF_DROP` as a chain's (unsigned) policy.
const NF_DROP_POLICY: u32 = 0;

use super::consts::{
    NF_ACCEPT, NF_DROP, NF_INET_FORWARD, NF_INET_LOCAL_IN, NFPROTO_ARP, NFPROTO_BRIDGE,
    NFPROTO_INET, NFPROTO_IPV4, NFPROTO_IPV6, NFPROTO_NETDEV, NFT_CMP_EQ, NFT_GOTO, NFT_JUMP,
    NFT_META_IIFNAME, NFT_META_OIFNAME, NFT_SET_ELEM_INTERVAL_END, NFTA_BITWISE_DREG,
    NFTA_BITWISE_SREG, NFTA_CHAIN_HOOK, NFTA_CHAIN_NAME, NFTA_CHAIN_POLICY, NFTA_CHAIN_TABLE,
    NFTA_CHAIN_TYPE, NFTA_CMP_DATA, NFTA_CMP_OP, NFTA_CMP_SREG, NFTA_CT_DREG, NFTA_DATA_VALUE,
    NFTA_DATA_VERDICT, NFTA_EXPR_DATA, NFTA_EXPR_NAME, NFTA_HOOK_HOOKNUM, NFTA_IMMEDIATE_DATA,
    NFTA_LIST_ELEM, NFTA_MATCH_NAME, NFTA_META_DREG, NFTA_META_KEY, NFTA_PAYLOAD_DREG,
    NFTA_RULE_CHAIN, NFTA_RULE_EXPRESSIONS, NFTA_RULE_TABLE, NFTA_SET_ELEM_DATA,
    NFTA_SET_ELEM_FLAGS, NFTA_SET_ELEM_KEY, NFTA_SET_ELEM_LIST_ELEMENTS, NFTA_TABLE_USERDATA,
    NFTA_TARGET_NAME, NFTA_VERDICT_CHAIN, NFTA_VERDICT_CODE,
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

    /// Whether it's firewalld's: its `inet firewalld` table, or another of
    /// its own (`firewalld_policy_drop`, which it holds while it reloads).
    #[must_use]
    pub fn is_firewallds(&self) -> bool {
        self.family == NFPROTO_INET && self.table.starts_with(FIREWALLD)
    }
}

/// firewalld's table (in the `inet` family).
pub const FIREWALLD: &str = "firewalld";

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

/// A rule, as listed: what's needed to tell what another program's rule
/// lets through (its interfaces, whether it has other conditions, and
/// where it sends a packet), not the whole of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rule {
    /// Its family (`NFPROTO_*`).
    pub family: u8,
    /// Its table.
    pub table: String,
    /// Its chain.
    pub chain: String,
    /// The interface it takes packets from (`iifname`, iptables' `-i`), as
    /// compared: a name ending in NUL, or a prefix (a wildcard:
    /// `"shipyard*"`, iptables' `shipyard+`).
    pub iifname: Option<Vec<u8>>,
    /// The interface they leave by (`oifname`, `-o`), the same way.
    pub oifname: Option<Vec<u8>>,
    /// Whether it looks at the connection (`ct state`, `ct status`,
    /// iptables' `conntrack` or `state` match).
    pub ct: bool,
    /// Whether it has any other condition (a protocol, a port, an address,
    /// a set, a negation, an expression this doesn't know): it lets through
    /// less than its interfaces alone say.
    pub narrower: bool,
    /// Whether it rejects (`reject`, iptables' `REJECT`).
    pub rejects: bool,
    /// Its verdict, if it ends in one (an `immediate` verdict: not a
    /// `reject`, which is an expression of its own).
    pub verdict: Option<Verdict>,
}

/// Where a rule sends a packet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// Accepted (by this chain: other tables' chains still see it).
    Accept,
    /// Dropped, for good.
    Drop,
    /// On to the chain named, then back.
    Jump(String),
    /// On to the chain named, for good.
    Goto(String),
    /// Any other (`continue`, `return`, a queue): its code.
    Other(i32),
}

impl Rule {
    /// Whether it takes packets from the interface `name` (or from any).
    #[must_use]
    pub fn from_interface(&self, name: &str) -> bool {
        self.iifname
            .as_deref()
            .is_none_or(|value| names_interface(value, name))
    }

    /// Whether it lets packets leave by the interface `name` (or by any).
    #[must_use]
    pub fn to_interface(&self, name: &str) -> bool {
        self.oifname
            .as_deref()
            .is_none_or(|value| names_interface(value, name))
    }

    /// Whether it accepts (`NF_ACCEPT`).
    #[must_use]
    pub fn accepts(&self) -> bool {
        self.verdict == Some(Verdict::Accept)
    }

    /// The chain it jumps or goes to, if it does.
    #[must_use]
    pub fn goes_to(&self) -> Option<&str> {
        match &self.verdict {
            Some(Verdict::Jump(chain) | Verdict::Goto(chain)) => Some(chain),
            _ => None,
        }
    }

    /// Whether it has no condition at all: every packet that reaches it
    /// meets its verdict.
    #[must_use]
    pub fn unconditional(&self) -> bool {
        self.iifname.is_none() && self.oifname.is_none() && !self.ct && !self.narrower
    }

    /// Whether it ends every packet that reaches it: an unconditional drop
    /// or reject (iptables-services' last `-j REJECT`).
    #[must_use]
    pub fn ends_all(&self) -> bool {
        self.unconditional() && (self.rejects || self.verdict == Some(Verdict::Drop))
    }
}

/// Whether `value`, as an `iifname`/`oifname` comparison holds it, matches
/// the interface `name`: up to its NUL, exactly; without one, as a prefix
/// (a wildcard), unless it fills a whole name.
fn names_interface(value: &[u8], name: &str) -> bool {
    match value.iter().position(|&b| b == 0) {
        Some(end) => value.get(..end) == Some(name.as_bytes()),
        None if value.len() < IFNAMSIZ => name.as_bytes().starts_with(value),
        None => value == name.as_bytes(),
    }
}

/// An interface name's room, with its NUL (`IFNAMSIZ`).
const IFNAMSIZ: usize = 16;

/// What a rule's expression left in a register, for a later `cmp` on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loaded {
    Iifname,
    Oifname,
    Ct,
    Other,
}

/// The rule in a `NEWRULE` message.
pub(crate) fn rule(message: &Message<'_>) -> Result<Rule, ParseError> {
    let (family, attrs) = split(message)?;
    let mut found = Rule {
        family,
        ..Rule::default()
    };
    // Which register holds what: a `meta` loads an interface's name into
    // one, a `cmp` then compares it.
    let mut registers: Vec<(u32, Loaded)> = Vec::new();
    for attr in attrs {
        let attr = attr?;
        match attr.kind {
            NFTA_RULE_TABLE => found.table = lossy(&attr)?,
            NFTA_RULE_CHAIN => found.chain = lossy(&attr)?,
            NFTA_RULE_EXPRESSIONS => {
                for item in attr.nested() {
                    expression(&item?, &mut found, &mut registers)?;
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

/// One expression of a rule, into `rule`.
fn expression(
    item: &Attr<'_>,
    rule: &mut Rule,
    registers: &mut Vec<(u32, Loaded)>,
) -> Result<(), ParseError> {
    let mut name = None;
    let mut data = None;
    for field in item.nested() {
        let field = field?;
        match field.kind {
            NFTA_EXPR_NAME => name = Some(field.c_bytes()?),
            NFTA_EXPR_DATA => data = Some(field),
            _ => {}
        }
    }
    let data = data.as_ref();
    match name {
        Some(b"meta") => {
            let [dreg, key] = fields(data, [NFTA_META_DREG, NFTA_META_KEY])?;
            let what = match key.map(|key| key.be32()).transpose()? {
                Some(NFT_META_IIFNAME) => Loaded::Iifname,
                Some(NFT_META_OIFNAME) => Loaded::Oifname,
                _ => Loaded::Other,
            };
            // A `meta` that sets (no destination register) loads nothing.
            load(registers, dreg.as_ref(), what)?;
        }
        Some(b"ct") => {
            let [dreg] = fields(data, [NFTA_CT_DREG])?;
            load(registers, dreg.as_ref(), Loaded::Ct)?;
        }
        Some(b"payload") => {
            let [dreg] = fields(data, [NFTA_PAYLOAD_DREG])?;
            load(registers, dreg.as_ref(), Loaded::Other)?;
        }
        Some(b"bitwise") => {
            // Masks a register into another: what it holds stays the same
            // kind (`ct state established,related` is a mask, then a
            // comparison).
            let [sreg, dreg] = fields(data, [NFTA_BITWISE_SREG, NFTA_BITWISE_DREG])?;
            let what = held(registers, sreg.as_ref())?;
            load(registers, dreg.as_ref(), what)?;
        }
        Some(b"cmp") => {
            let [sreg, op, value_attr] = fields(data, [NFTA_CMP_SREG, NFTA_CMP_OP, NFTA_CMP_DATA])?;
            let equal = op.map(|op| op.be32()).transpose()? == Some(NFT_CMP_EQ);
            let what = held(registers, sreg.as_ref())?;
            match (what, equal, value_attr) {
                (Loaded::Iifname, true, Some(value_attr)) => {
                    rule.iifname = Some(value(&value_attr)?);
                }
                (Loaded::Oifname, true, Some(value_attr)) => {
                    rule.oifname = Some(value(&value_attr)?);
                }
                (Loaded::Ct, _, _) => rule.ct = true,
                _ => rule.narrower = true,
            }
        }
        Some(b"immediate") => {
            let [immediate] = fields(data, [NFTA_IMMEDIATE_DATA])?;
            for inner in immediate.iter().flat_map(Attr::nested) {
                let inner = inner?;
                if inner.kind == NFTA_DATA_VERDICT {
                    rule.verdict = Some(verdict(&inner)?);
                }
            }
        }
        Some(b"reject") => rule.rejects = true,
        // iptables' own matches and targets, as iptables-nft gives them.
        Some(b"match") => {
            let [xt] = fields(data, [NFTA_MATCH_NAME])?;
            match xt.map(|xt| xt.c_bytes()).transpose()? {
                Some(b"conntrack" | b"state") => rule.ct = true,
                _ => rule.narrower = true,
            }
        }
        Some(b"target") => {
            let [xt] = fields(data, [NFTA_TARGET_NAME])?;
            if xt.map(|xt| xt.c_bytes()).transpose()? == Some(b"REJECT") {
                rule.rejects = true;
            }
        }
        // What only counts or logs.
        Some(b"counter" | b"log") => {}
        _ => rule.narrower = true,
    }
    Ok(())
}

/// The attributes of kinds `wanted` in an expression's `data`, in that
/// order (`None` for one it hasn't).
fn fields<'a, const N: usize>(
    data: Option<&Attr<'a>>,
    wanted: [u16; N],
) -> Result<[Option<Attr<'a>>; N], ParseError> {
    let mut found = [const { None }; N];
    for field in data.into_iter().flat_map(Attr::nested) {
        let field = field?;
        if let Some(slot) = wanted
            .iter()
            .position(|&kind| kind == field.kind)
            .and_then(|at| found.get_mut(at))
        {
            *slot = Some(field);
        }
    }
    Ok(found)
}

/// Notes that `register` (an expression's destination, if it has one) now
/// holds `what`.
fn load(
    registers: &mut Vec<(u32, Loaded)>,
    register: Option<&Attr<'_>>,
    what: Loaded,
) -> Result<(), ParseError> {
    if let Some(register) = register {
        let register = register.be32()?;
        registers.retain(|&(held, _)| held != register);
        registers.push((register, what));
    }
    Ok(())
}

/// What `register` (an expression's source) holds.
fn held(registers: &[(u32, Loaded)], register: Option<&Attr<'_>>) -> Result<Loaded, ParseError> {
    let source = register.map(Attr::be32).transpose()?;
    Ok(registers
        .iter()
        .find(|&&(held, _)| Some(held) == source)
        .map_or(Loaded::Other, |&(_, what)| what))
}

/// A verdict: its code and, for a jump or goto, its chain.
fn verdict(attr: &Attr<'_>) -> Result<Verdict, ParseError> {
    let mut code = None;
    let mut chain = None;
    for field in attr.nested() {
        let field = field?;
        match field.kind {
            NFTA_VERDICT_CODE => code = Some(field.be32()?.cast_signed()),
            NFTA_VERDICT_CHAIN => chain = Some(lossy(&field)?),
            _ => {}
        }
    }
    let code = code.ok_or(ParseError::Missing("verdict code"))?;
    Ok(match (code, chain) {
        (NF_ACCEPT, _) => Verdict::Accept,
        (NF_DROP, _) => Verdict::Drop,
        (NFT_JUMP, Some(chain)) => Verdict::Jump(chain),
        (NFT_GOTO, Some(chain)) => Verdict::Goto(chain),
        (code, _) => Verdict::Other(code),
    })
}

/// The chain of the rule in a `NEWRULE` message (the differential tests
/// against `nft` count rules per chain).
#[cfg(test)]
pub(crate) fn rule_chain(message: &Message<'_>) -> Result<String, ParseError> {
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
    use crate::netlink::message::{MessageBuilder, Messages};
    use crate::nft::consts::{NFT_CMP_NEQ, NFT_MSG_NEWCHAIN, NFT_MSG_NEWTABLE};
    use crate::nft::objects::{BaseChain, Element, add_elements, message, new_chain, new_table};

    /// What the kernel lists looks like what was sent: these are read back
    /// from our own requests.
    fn parsed(bytes: &[u8]) -> Message<'_> {
        Messages::new(bytes).next().unwrap().unwrap()
    }

    /// One expression of a rule, as the kernel lists it.
    enum Expr<'a> {
        /// `meta` loading `key` into `register`.
        Meta(u32, u32),
        /// `ct` loading into a register.
        Ct(u32),
        /// `payload` loading into a register.
        Payload(u32),
        /// `bitwise` from a register into another.
        Bitwise(u32, u32),
        /// `cmp` of a register (with an operation) with a value.
        Cmp(u32, u32, &'a [u8]),
        /// An `immediate` verdict: its code and chain.
        Verdict(i32, Option<&'a str>),
        /// An expression of no data (`reject`, `counter`).
        Named(&'a str),
        /// iptables' own: `match` or `target`, and its name.
        Xt(&'a str, &'a str),
    }

    impl Expr<'_> {
        fn name(&self) -> &str {
            match self {
                Expr::Meta(..) => "meta",
                Expr::Ct(_) => "ct",
                Expr::Payload(_) => "payload",
                Expr::Bitwise(..) => "bitwise",
                Expr::Cmp(..) => "cmp",
                Expr::Verdict(..) => "immediate",
                Expr::Named(name) | Expr::Xt(name, _) => name,
            }
        }

        fn data(&self, data: &mut MessageBuilder) {
            use crate::nft::consts::NFTA_IMMEDIATE_DREG;
            match *self {
                Expr::Meta(key, dreg) => {
                    data.attr_be32(NFTA_META_KEY, key)
                        .attr_be32(NFTA_META_DREG, dreg);
                }
                Expr::Ct(dreg) => {
                    data.attr_be32(NFTA_CT_DREG, dreg);
                }
                Expr::Payload(dreg) => {
                    data.attr_be32(NFTA_PAYLOAD_DREG, dreg);
                }
                Expr::Bitwise(sreg, dreg) => {
                    data.attr_be32(NFTA_BITWISE_SREG, sreg)
                        .attr_be32(NFTA_BITWISE_DREG, dreg);
                }
                Expr::Cmp(sreg, op, value) => {
                    data.attr_be32(NFTA_CMP_SREG, sreg)
                        .attr_be32(NFTA_CMP_OP, op)
                        .nest(NFTA_CMP_DATA, |data| {
                            data.attr(NFTA_DATA_VALUE, value);
                        });
                }
                Expr::Verdict(code, chain) => {
                    data.attr_be32(NFTA_IMMEDIATE_DREG, 0)
                        .nest(NFTA_IMMEDIATE_DATA, |immediate| {
                            immediate.nest(NFTA_DATA_VERDICT, |verdict| {
                                verdict.attr_be32(NFTA_VERDICT_CODE, code.cast_unsigned());
                                if let Some(chain) = chain {
                                    verdict.attr_str(NFTA_VERDICT_CHAIN, chain);
                                }
                            });
                        });
                }
                Expr::Named(_) => {}
                Expr::Xt(_, xt) => {
                    data.attr_str(NFTA_MATCH_NAME, xt);
                }
            }
        }
    }

    /// A `NEWRULE` message for firewalld's `filter_FORWARD_POLICIES`, of
    /// `exprs`.
    fn rule_message(exprs: &[Expr<'_>]) -> Vec<u8> {
        use crate::nft::consts::NFT_MSG_NEWRULE;
        let mut built = message(NFT_MSG_NEWRULE, 0, 1);
        built
            .attr_str(NFTA_RULE_TABLE, "firewalld")
            .attr_str(NFTA_RULE_CHAIN, "filter_FORWARD_POLICIES")
            .nest(NFTA_RULE_EXPRESSIONS, |list| {
                for expr in exprs {
                    list.nest(NFTA_LIST_ELEM, |item| {
                        item.attr_str(NFTA_EXPR_NAME, expr.name())
                            .nest(NFTA_EXPR_DATA, |data| expr.data(data));
                    });
                }
            });
        built.finish(1, 0).unwrap()
    }

    fn decoded(exprs: &[Expr<'_>]) -> Rule {
        rule(&parsed(&rule_message(exprs))).unwrap()
    }

    /// An interface's name as nft compares it (NUL-padded to 16).
    const SHIPYARD0: &[u8; 16] = b"shipyard0\0\0\0\0\0\0\0";

    #[test]
    fn a_rules_interfaces_and_jump() {
        // `iifname "shipyard0" oifname "eth0" jump filter_FWD_trusted`.
        let rule = decoded(&[
            Expr::Meta(NFT_META_IIFNAME, 1),
            Expr::Cmp(1, NFT_CMP_EQ, SHIPYARD0),
            Expr::Meta(NFT_META_OIFNAME, 1),
            Expr::Cmp(1, NFT_CMP_EQ, b"eth0\0"),
            Expr::Verdict(NFT_JUMP, Some("filter_FWD_trusted")),
        ]);
        assert_eq!(rule.table, "firewalld");
        assert_eq!(rule.chain, "filter_FORWARD_POLICIES");
        assert!(rule.from_interface("shipyard0"));
        assert!(!rule.from_interface("shipyard"), "the whole name");
        assert!(!rule.from_interface("eth0"), "the other direction");
        assert!(rule.to_interface("eth0"));
        assert!(!rule.to_interface("shipyard0"));
        assert_eq!(rule.goes_to(), Some("filter_FWD_trusted"));
        assert!(!rule.accepts());
        assert!(!rule.ct && !rule.narrower && !rule.unconditional());
    }

    #[test]
    fn a_rules_wildcard_state_and_other_conditions() {
        // `-i shipyard+` (no NUL: a prefix), `ct state established,related`
        // (a mask, then a comparison), then `accept`.
        let rule = decoded(&[
            Expr::Meta(NFT_META_IIFNAME, 1),
            Expr::Cmp(1, NFT_CMP_EQ, b"shipyard"),
            Expr::Ct(1),
            Expr::Bitwise(1, 1),
            Expr::Cmp(1, NFT_CMP_NEQ, &[0; 4]),
            Expr::Named("counter"),
            Expr::Verdict(NF_ACCEPT, None),
        ]);
        assert!(rule.from_interface("shipyard0") && rule.from_interface("shipyard1"));
        assert!(!rule.from_interface("eth0"));
        assert!(rule.to_interface("anything"), "no output named");
        assert!(rule.ct && !rule.narrower);
        assert!(rule.accepts());
        // A port, a negated name, or an expression this doesn't know: it
        // lets less through than its interfaces say.
        for narrower in [
            [Expr::Payload(1), Expr::Cmp(1, NFT_CMP_EQ, &[0, 53])],
            [
                Expr::Meta(NFT_META_IIFNAME, 1),
                Expr::Cmp(1, NFT_CMP_NEQ, SHIPYARD0),
            ],
            [Expr::Named("limit"), Expr::Named("counter")],
        ] {
            let rule = decoded(&narrower);
            assert!(rule.narrower && rule.iifname.is_none());
        }
        // iptables' conntrack match is the connection's state; another
        // match narrows.
        assert!(decoded(&[Expr::Xt("match", "conntrack")]).ct);
        assert!(decoded(&[Expr::Xt("match", "multiport")]).narrower);
    }

    #[test]
    fn a_rule_that_ends_everything() {
        assert!(decoded(&[Expr::Named("reject")]).ends_all());
        assert!(decoded(&[Expr::Xt("target", "REJECT")]).ends_all());
        assert!(decoded(&[Expr::Named("counter"), Expr::Verdict(NF_DROP, None)]).ends_all());
        // Not when it's for some traffic only, nor an accept.
        let some = decoded(&[
            Expr::Meta(NFT_META_OIFNAME, 1),
            Expr::Cmp(1, NFT_CMP_EQ, SHIPYARD0),
            Expr::Named("reject"),
        ]);
        assert!(!some.ends_all());
        assert!(!decoded(&[Expr::Verdict(NF_ACCEPT, None)]).ends_all());
        assert_eq!(
            decoded(&[Expr::Verdict(NFT_GOTO, Some("x"))]).verdict,
            Some(Verdict::Goto("x".to_owned()))
        );
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
