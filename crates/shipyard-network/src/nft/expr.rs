//! `nf_tables` expressions: the instructions of a rule.
//!
//! A rule is a little program the kernel runs on each packet: expressions
//! that load parts of it (its interface, a header field, its connection's
//! state) into registers, compare them, look them up in sets, and end in a
//! verdict or a NAT. An expression that doesn't match (a comparison that's
//! false, a lookup that misses) ends the rule, and the packet goes on to the
//! next one.
//!
//! Each is sent as `NFTA_LIST_ELEM { NFTA_EXPR_NAME name, NFTA_EXPR_DATA {…} }`.

use super::consts::{
    NFT_CONNLIMIT_F_INV, NFTA_CONNLIMIT_COUNT, NFTA_CONNLIMIT_FLAGS, NFTA_DYNSET_EXPR,
    NFTA_DYNSET_OP, NFTA_DYNSET_SET_NAME, NFTA_DYNSET_SREG_KEY, NFTA_REJECT_ICMP_CODE,
    NFTA_REJECT_TYPE,
};
use super::consts::{
    NFT_LOOKUP_F_INV, NFT_REG_VERDICT, NFTA_BITWISE_DREG, NFTA_BITWISE_LEN, NFTA_BITWISE_MASK,
    NFTA_BITWISE_SREG, NFTA_BITWISE_XOR, NFTA_CMP_DATA, NFTA_CMP_OP, NFTA_CMP_SREG, NFTA_CT_DREG,
    NFTA_CT_KEY, NFTA_DATA_VALUE, NFTA_DATA_VERDICT, NFTA_EXPR_DATA, NFTA_EXPR_NAME, NFTA_FIB_DREG,
    NFTA_FIB_FLAGS, NFTA_FIB_RESULT, NFTA_IMMEDIATE_DATA, NFTA_IMMEDIATE_DREG, NFTA_LIST_ELEM,
    NFTA_LOOKUP_DREG, NFTA_LOOKUP_FLAGS, NFTA_LOOKUP_SET, NFTA_LOOKUP_SREG, NFTA_META_DREG,
    NFTA_META_KEY, NFTA_NAT_FAMILY, NFTA_NAT_REG_ADDR_MIN, NFTA_NAT_REG_PROTO_MIN, NFTA_NAT_TYPE,
    NFTA_PAYLOAD_BASE, NFTA_PAYLOAD_DREG, NFTA_PAYLOAD_LEN, NFTA_PAYLOAD_OFFSET, NFTA_VERDICT_CODE,
};
use crate::netlink::MessageBuilder;

/// One expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expr {
    /// Loads a property of the packet (`key`: `NFT_META_*`) into `dreg`.
    Meta { key: u32, dreg: u32 },
    /// Loads `len` bytes at `offset` of a header (`base`) into `dreg`.
    Payload {
        base: u32,
        offset: u32,
        len: u32,
        dreg: u32,
    },
    /// Compares `sreg` with `data` (`op`: `NFT_CMP_*`).
    Cmp { sreg: u32, op: u32, data: Vec<u8> },
    /// `dreg = (sreg & mask) ^ xor`, over `mask.len()` bytes.
    Bitwise {
        sreg: u32,
        dreg: u32,
        mask: Vec<u8>,
        xor: Vec<u8>,
    },
    /// Looks `sreg` (and on) up in `set`: a miss (or with `invert`, a hit)
    /// ends the rule; in a map, the value goes to `dreg`.
    Lookup {
        set: String,
        sreg: u32,
        dreg: Option<u32>,
        invert: bool,
    },
    /// Asks the routing table about the packet: `result` (`NFT_FIB_RESULT_*`)
    /// for its `flags`' address, into `dreg`.
    Fib { flags: u32, result: u32, dreg: u32 },
    /// Loads a property of the packet's connection (`key`: `NFT_CT_*`).
    Ct { key: u32, dreg: u32 },
    /// Ends the rule with a verdict (`NF_ACCEPT`, `NF_DROP`).
    Verdict(i32),
    /// Changes the destination (`kind`: `NFT_NAT_DNAT`) to the address in
    /// `reg_addr` and the port in `reg_proto`.
    Nat {
        kind: u32,
        family: u32,
        reg_addr: u32,
        reg_proto: u32,
    },
    /// Changes the source to the outgoing interface's address.
    Masquerade,
    /// Finds `sreg`'s key in the dynamic `set`, adding it if it isn't
    /// there (`op`: `NFT_DYNSET_OP_ADD`), and runs `expr` as that
    /// element's own (each element keeps its own state: a count). Where
    /// `expr` doesn't match, nor does the rule.
    Dynset {
        set: String,
        op: u32,
        sreg: u32,
        expr: Box<Expr>,
    },
    /// The connections counted for an element (in a `dynset`): matches
    /// while there are at most `count`; with `invert`, once there are more.
    Connlimit { count: u32, invert: bool },
    /// Answers the packet with an error (`kind`: `NFT_REJECT_*`; `code`: an
    /// ICMP one's code, none for a TCP reset) and drops it: the sender
    /// learns at once.
    Reject { kind: u32, code: Option<u8> },
}

impl Expr {
    /// The expression's name, as the kernel knows it.
    fn name(&self) -> &'static str {
        match self {
            Expr::Meta { .. } => "meta",
            Expr::Payload { .. } => "payload",
            Expr::Cmp { .. } => "cmp",
            Expr::Bitwise { .. } => "bitwise",
            Expr::Lookup { .. } => "lookup",
            Expr::Fib { .. } => "fib",
            Expr::Ct { .. } => "ct",
            Expr::Verdict(_) => "immediate",
            Expr::Nat { .. } => "nat",
            Expr::Masquerade => "masq",
            Expr::Dynset { .. } => "dynset",
            Expr::Connlimit { .. } => "connlimit",
            Expr::Reject { .. } => "reject",
        }
    }

    /// Appends the expression to a rule's `NFTA_RULE_EXPRESSIONS`.
    pub(crate) fn encode(&self, message: &mut MessageBuilder) {
        message.nest(NFTA_LIST_ELEM, |message| self.encode_named(message));
    }

    /// Its name and data, without the list's wrapper (as a `dynset`
    /// nests its one expression).
    fn encode_named(&self, message: &mut MessageBuilder) {
        message.attr_str(NFTA_EXPR_NAME, self.name());
        message.nest(NFTA_EXPR_DATA, |data| self.encode_data(data));
    }

    fn encode_data(&self, data: &mut MessageBuilder) {
        match self {
            Expr::Meta { key, dreg } => {
                data.attr_be32(NFTA_META_KEY, *key)
                    .attr_be32(NFTA_META_DREG, *dreg);
            }
            Expr::Payload {
                base,
                offset,
                len,
                dreg,
            } => {
                data.attr_be32(NFTA_PAYLOAD_DREG, *dreg)
                    .attr_be32(NFTA_PAYLOAD_BASE, *base)
                    .attr_be32(NFTA_PAYLOAD_OFFSET, *offset)
                    .attr_be32(NFTA_PAYLOAD_LEN, *len);
            }
            Expr::Cmp {
                sreg,
                op,
                data: value,
            } => {
                data.attr_be32(NFTA_CMP_SREG, *sreg)
                    .attr_be32(NFTA_CMP_OP, *op)
                    .nest(NFTA_CMP_DATA, |nested| {
                        nested.attr(NFTA_DATA_VALUE, value);
                    });
            }
            Expr::Bitwise {
                sreg,
                dreg,
                mask,
                xor,
            } => encode_bitwise(data, *sreg, *dreg, mask, xor),
            Expr::Lookup {
                set,
                sreg,
                dreg,
                invert,
            } => {
                data.attr_str(NFTA_LOOKUP_SET, set)
                    .attr_be32(NFTA_LOOKUP_SREG, *sreg);
                if let Some(dreg) = dreg {
                    data.attr_be32(NFTA_LOOKUP_DREG, *dreg);
                }
                if *invert {
                    data.attr_be32(NFTA_LOOKUP_FLAGS, NFT_LOOKUP_F_INV);
                }
            }
            Expr::Fib {
                flags,
                result,
                dreg,
            } => {
                data.attr_be32(NFTA_FIB_DREG, *dreg)
                    .attr_be32(NFTA_FIB_RESULT, *result)
                    .attr_be32(NFTA_FIB_FLAGS, *flags);
            }
            Expr::Ct { key, dreg } => {
                data.attr_be32(NFTA_CT_KEY, *key)
                    .attr_be32(NFTA_CT_DREG, *dreg);
            }
            Expr::Verdict(code) => {
                data.attr_be32(NFTA_IMMEDIATE_DREG, NFT_REG_VERDICT).nest(
                    NFTA_IMMEDIATE_DATA,
                    |value| {
                        value.nest(NFTA_DATA_VERDICT, |verdict| {
                            verdict.attr(NFTA_VERDICT_CODE, &code.to_be_bytes());
                        });
                    },
                );
            }
            Expr::Nat {
                kind,
                family,
                reg_addr,
                reg_proto,
            } => {
                data.attr_be32(NFTA_NAT_TYPE, *kind)
                    .attr_be32(NFTA_NAT_FAMILY, *family)
                    .attr_be32(NFTA_NAT_REG_ADDR_MIN, *reg_addr)
                    .attr_be32(NFTA_NAT_REG_PROTO_MIN, *reg_proto);
            }
            Expr::Masquerade => {}
            Expr::Dynset {
                set,
                op,
                sreg,
                expr,
            } => encode_dynset(data, set, *op, *sreg, expr),
            Expr::Connlimit { count, invert } => encode_connlimit(data, *count, *invert),
            Expr::Reject { kind, code } => encode_reject(data, *kind, *code),
        }
    }
}

/// `bitwise`'s data: `(sreg & mask) ^ xor` into `dreg`, over the mask's
/// length.
fn encode_bitwise(data: &mut MessageBuilder, sreg: u32, dreg: u32, mask: &[u8], xor: &[u8]) {
    let len = u32::try_from(mask.len()).unwrap_or(u32::MAX);
    data.attr_be32(NFTA_BITWISE_SREG, sreg)
        .attr_be32(NFTA_BITWISE_DREG, dreg)
        .attr_be32(NFTA_BITWISE_LEN, len)
        .nest(NFTA_BITWISE_MASK, |nested| {
            nested.attr(NFTA_DATA_VALUE, mask);
        })
        .nest(NFTA_BITWISE_XOR, |nested| {
            nested.attr(NFTA_DATA_VALUE, xor);
        });
}

/// `dynset`'s data: its set, operation and key register, and its one
/// expression nested whole (no list around it).
fn encode_dynset(data: &mut MessageBuilder, set: &str, op: u32, sreg: u32, expr: &Expr) {
    data.attr_str(NFTA_DYNSET_SET_NAME, set)
        .attr_be32(NFTA_DYNSET_OP, op)
        .attr_be32(NFTA_DYNSET_SREG_KEY, sreg)
        .nest(NFTA_DYNSET_EXPR, |nested| expr.encode_named(nested));
}

/// `reject`'s data: its kind, and an ICMP one's code.
fn encode_reject(data: &mut MessageBuilder, kind: u32, code: Option<u8>) {
    data.attr_be32(NFTA_REJECT_TYPE, kind);
    if let Some(code) = code {
        data.attr(NFTA_REJECT_ICMP_CODE, &[code]);
    }
}

/// `connlimit`'s data: the limit, and "over" as its flag.
fn encode_connlimit(data: &mut MessageBuilder, count: u32, invert: bool) {
    data.attr_be32(NFTA_CONNLIMIT_COUNT, count).attr_be32(
        NFTA_CONNLIMIT_FLAGS,
        if invert { NFT_CONNLIMIT_F_INV } else { 0 },
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;
    use crate::netlink::message::{Attrs, Messages};
    use crate::nft::consts::{NF_DROP, NFT_CMP_EQ, NFT_META_IIFNAME, NFT_REG32_00};

    /// `expr` encoded alone, as the attributes of its `NFTA_EXPR_DATA`, with
    /// its name.
    fn encoded(expr: &Expr) -> (String, Vec<(u16, Vec<u8>)>) {
        let mut message = MessageBuilder::new(0, 0);
        expr.encode(&mut message);
        let bytes = message.finish(1, 1).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        let element = Attrs::new(message.payload).next().unwrap().unwrap();
        assert_eq!(element.kind, NFTA_LIST_ELEM);
        assert!(element.nested);
        let mut name = String::new();
        let mut data = Vec::new();
        for attr in element.nested().flatten() {
            match attr.kind {
                NFTA_EXPR_NAME => name = attr.string().unwrap().to_owned(),
                NFTA_EXPR_DATA => {
                    data = attr
                        .nested()
                        .flatten()
                        .map(|inner| (inner.kind, inner.value.to_vec()))
                        .collect();
                }
                _ => panic!("unexpected attribute {}", attr.kind),
            }
        }
        (name, data)
    }

    #[test]
    fn numbers_are_big_endian() {
        let (name, data) = encoded(&Expr::Meta {
            key: NFT_META_IIFNAME,
            dreg: NFT_REG32_00,
        });
        assert_eq!(name, "meta");
        assert_eq!(
            data,
            [
                (NFTA_META_KEY, vec![0, 0, 0, 6]),
                (NFTA_META_DREG, vec![0, 0, 0, 8])
            ]
        );
    }

    #[test]
    fn a_comparison_nests_its_value() {
        let (name, data) = encoded(&Expr::Cmp {
            sreg: NFT_REG32_00,
            op: NFT_CMP_EQ,
            data: vec![2],
        });
        assert_eq!(name, "cmp");
        assert_eq!(data[2].0, NFTA_CMP_DATA);
        let nested: Vec<_> = Attrs::new(&data[2].1).flatten().collect();
        assert_eq!(
            (nested[0].kind, nested[0].value),
            (NFTA_DATA_VALUE, &[2][..])
        );
    }

    #[test]
    fn a_verdict_is_an_immediate_into_the_verdict_register() {
        let (name, data) = encoded(&Expr::Verdict(NF_DROP));
        assert_eq!(name, "immediate");
        assert_eq!(data[0], (NFTA_IMMEDIATE_DREG, vec![0, 0, 0, 0]));
        let value = Attrs::new(&data[1].1).next().unwrap().unwrap();
        assert_eq!(value.kind, NFTA_DATA_VERDICT);
        let code = value.nested().next().unwrap().unwrap();
        assert_eq!(
            (code.kind, code.value),
            (NFTA_VERDICT_CODE, &[0, 0, 0, 0][..])
        );
    }

    #[test]
    fn an_inverted_lookup_says_so_and_a_set_lookup_has_no_destination() {
        let (_, data) = encoded(&Expr::Lookup {
            set: "bridges".into(),
            sreg: NFT_REG32_00,
            dreg: None,
            invert: true,
        });
        let kinds: Vec<u16> = data.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            [NFTA_LOOKUP_SET, NFTA_LOOKUP_SREG, NFTA_LOOKUP_FLAGS]
        );
        assert_eq!(data[0].1, b"bridges\0");
    }

    /// A `dynset` nests its expression's name and data directly (one
    /// expression, no list around it); `connlimit`'s "over" is its flag.
    #[test]
    fn a_dynset_nests_its_count_whole() {
        use crate::nft::consts::NFT_DYNSET_OP_ADD;
        let (name, data) = encoded(&Expr::Dynset {
            set: "conns4".into(),
            op: NFT_DYNSET_OP_ADD,
            sreg: NFT_REG32_00,
            expr: Box::new(Expr::Connlimit {
                count: 8192,
                invert: true,
            }),
        });
        assert_eq!(name, "dynset");
        assert_eq!(
            data[..3],
            [
                (NFTA_DYNSET_SET_NAME, b"conns4\0".to_vec()),
                (NFTA_DYNSET_OP, vec![0, 0, 0, 0]),
                (NFTA_DYNSET_SREG_KEY, vec![0, 0, 0, 8]),
            ]
        );
        assert_eq!(data[3].0, NFTA_DYNSET_EXPR);
        let nested: Vec<_> = Attrs::new(&data[3].1).flatten().collect();
        assert_eq!(
            (nested[0].kind, nested[0].value),
            (NFTA_EXPR_NAME, &b"connlimit\0"[..])
        );
        assert_eq!(nested[1].kind, NFTA_EXPR_DATA);
        let count: Vec<_> = nested[1]
            .nested()
            .flatten()
            .map(|attr| (attr.kind, attr.value.to_vec()))
            .collect();
        assert_eq!(
            count,
            [
                (NFTA_CONNLIMIT_COUNT, 8192_u32.to_be_bytes().to_vec()),
                (NFTA_CONNLIMIT_FLAGS, vec![0, 0, 0, 1]),
            ]
        );
    }

    #[test]
    fn a_reset_has_no_code() {
        let (_, data) = encoded(&Expr::Reject {
            kind: 1,
            code: None,
        });
        assert_eq!(data, [(NFTA_REJECT_TYPE, vec![0, 0, 0, 1])]);
    }

    #[test]
    fn a_reject_says_its_kind_and_one_byte_code() {
        let (name, data) = encoded(&Expr::Reject {
            kind: 2,
            code: Some(3),
        });
        assert_eq!(name, "reject");
        assert_eq!(
            data,
            [
                (NFTA_REJECT_TYPE, vec![0, 0, 0, 2]),
                (NFTA_REJECT_ICMP_CODE, vec![3])
            ]
        );
    }

    #[test]
    fn masquerade_has_no_data() {
        assert_eq!(encoded(&Expr::Masquerade), ("masq".to_owned(), vec![]));
    }
}
