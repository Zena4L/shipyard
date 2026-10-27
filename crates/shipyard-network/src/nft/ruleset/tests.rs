//! The ruleset's rules and elements, as built (the kernel's view of them
//! is `linux_tests`'s).

#![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]

use super::super::consts::{
    CT_STATE_INVALID, IPS_CONFIRMED, IPS_DST_NAT, NF_DROP, NFT_CMP_EQ, NFT_CMP_GTE, NFT_CMP_LTE,
    NFT_CMP_NEQ, NFT_CT_STATE, NFT_CT_STATUS, NFT_DYNSET_OP_ADD, NFT_META_IIFNAME,
    NFT_META_OIFNAME, NFT_NAT_DNAT, NFT_REJECT_ICMPX_ADMIN_PROHIBITED, NFT_REJECT_ICMPX_UNREACH,
    NFT_REJECT_TCP_RST,
};
use super::super::expr::Expr;
use super::elements::subnet_range;
use super::exprs::{
    address, address6, ct_bits, interface_in, ipv6, nfproto, not_to_loopback6, on_one_bridge,
    routing_closed,
};
use super::rules::{limit_rules, link_local_refused, metadata_rules};
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
    let table = || install(&[], &[], &ClosedRouting::default(), &[]);
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
    assert_eq!(forward.len(), 19);
    // What conntrack finds invalid on a bridge first, then replies, an
    // isolated network's to the metadata addresses, the limit, what stays on a
    // bridge, the internal networks, DNAT from an outside link dropped, DNAT,
    // the rest, then routing for others if closed.
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
    // A published port from an outside link (the macOS guest's NAT): dropped,
    // before DNAT is accepted (F108).
    assert_eq!(
        forward[4],
        [
            interface_in(NFT_META_IIFNAME, set::OUTSIDE, false),
            ct_bits(NFT_CT_STATUS, IPS_DST_NAT, true),
            vec![Expr::Verdict(NF_DROP)],
        ]
        .concat()
    );
    assert_eq!(
        forward[5],
        [
            ct_bits(NFT_CT_STATUS, IPS_DST_NAT, true),
            vec![Expr::Verdict(NF_ACCEPT)]
        ]
        .concat()
    );
    assert_eq!(forward[6].last(), Some(&Expr::Verdict(NF_DROP)));
    // Routing closed: between the host's own interfaces only.
    assert_eq!(
        forward[7],
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
        &["eth0".to_owned()],
    );
    let what: Vec<&str> = items.iter().map(|item| item.what.as_str()).collect();
    assert_eq!(what[0], "adding the table shipyard");
    assert_eq!(what[1], "adding the set bridges");
    assert_eq!(what[18], "adding the chain guard");
    assert!(what.contains(&"adding rule 19 of the chain forward"));
    assert!(what.contains(&"adding rule 9 of the chain input"));
    assert_eq!(
        what[what.len() - 6..],
        [
            "adding to the set ports",
            "adding to the set uplinks",
            "adding to the set closed",
            "adding to the set uplinks",
            "adding to the set closed",
            "adding to the set outside",
        ]
    );
    for item in &items {
        let bytes = item.message.finish(1, 1).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(message.kind >> 8, 10, "{}", item.what);
        assert_eq!(&message.payload[..4], &[FAMILY, 0, 0, 0], "{}", item.what);
    }
}
