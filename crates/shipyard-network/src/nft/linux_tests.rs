//! `nf_tables` and conntrack against the real kernel, as root, each test in
//! a network namespace of its own (see `route/linux_tests.rs`): the
//! firewall changed is that namespace's, never the host's.

use std::net::{IpAddr, Ipv4Addr};

use nix::errno::Errno;

use super::consts::{NF_ACCEPT, NF_INET_FORWARD, NFPROTO_INET};
use super::objects::{delete_table, new_table};
use super::ruleset::{ClosedRouting, FAMILY, NetworkRules, PortRule, TABLE, set};
use super::{Nftables, Ruleset};
use crate::linux_test::{in_new_netns, reference_tool};
use crate::netlink::Socket;
use crate::nft::consts::NFNL_SUBSYS_NFTABLES;
use crate::ports::{Protocol, Published};

fn bridge() -> NetworkRules {
    NetworkRules {
        bridge: "shipyard0".into(),
        subnet: "10.88.0.0/16".parse().unwrap(),
        subnet6: Some("fd00:88::/64".parse().unwrap()),
        internal: false,
        isolate_host: false,
        conntrack_limit: false,
        dns: None,
    }
}

fn port(host_port: u16, host_ip: Option<IpAddr>) -> PortRule {
    PortRule {
        published: Published {
            host_ip,
            host_port,
            container_port: 80,
            protocol: Protocol::Tcp,
        },
        container: IpAddr::V4(Ipv4Addr::new(10, 88, 0, 2)),
    }
}

fn port6(host_port: u16, host_ip: Option<IpAddr>) -> PortRule {
    PortRule {
        container: "fd00:88::2".parse().unwrap(),
        ..port(host_port, host_ip)
    }
}

#[test]
#[ignore = "needs root"]
fn the_ruleset_installs_is_current_and_is_rebuilt() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Missing);
        nft.install(
            &[bridge()],
            &[port(8080, None)],
            &ClosedRouting::default(),
            &[],
        )
        .unwrap();
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Current);
        // Every base chain is there, accepting by default.
        let ours: Vec<_> = nft
            .chains()
            .unwrap()
            .into_iter()
            .filter(|chain| chain.family == NFPROTO_INET && chain.table == TABLE)
            .collect();
        assert_eq!(ours.len(), 6, "{ours:?}");
        assert!(ours.iter().all(super::Chain::is_shipyards));
        let forward = ours.iter().find(|chain| chain.name == "forward").unwrap();
        assert_eq!(forward.hook, Some(NF_INET_FORWARD));
        assert_eq!(forward.policy, Some(u32::try_from(NF_ACCEPT).unwrap()));
        assert_eq!(nft.elements(set::BRIDGES).unwrap().len(), 1);
        assert_eq!(nft.elements(set::MASQUERADE).unwrap().len(), 2);
        assert_eq!(nft.elements(set::MASQUERADE6).unwrap().len(), 2);
        assert_eq!(nft.elements(set::PORTS).unwrap().len(), 1);

        // An older table (other user data) is found out of date, and
        // replaced whole: the elements are what's given now.
        let mut socket = Socket::netfilter().unwrap();
        socket
            .batch(
                "making an outdated table",
                NFNL_SUBSYS_NFTABLES,
                &[
                    delete_table(FAMILY, TABLE),
                    new_table(FAMILY, TABLE, b"old"),
                ],
            )
            .unwrap();
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Outdated);
        nft.install(&[bridge()], &[], &ClosedRouting::default(), &[])
            .unwrap();
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Current);
        assert!(nft.elements(set::PORTS).unwrap().is_empty());
    });
}

/// Ports as many as fit the table's own batch go in it, which the kernel
/// takes whole; more go in batches of their own: all are there either way.
#[test]
#[ignore = "needs root"]
fn many_ports_install_with_the_table_or_after_it() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        for (count, one_batch) in [(1000, true), (4000, false)] {
            let ports: Vec<PortRule> = (0..count).map(|n| port(1000 + n, None)).collect();
            // (Without the second round's `delete_table`: a few bytes.)
            let table = super::ruleset::install(&[bridge()], &[], &ClosedRouting::default(), &[]);
            let (_, more) =
                super::ruleset::fit_ports(table, super::ruleset::port_items(&ports, false));
            assert_eq!(more.is_empty(), one_batch, "{count} ports");
            nft.install(&[bridge()], &ports, &ClosedRouting::default(), &[])
                .unwrap();
            assert_eq!(nft.elements(set::PORTS).unwrap().len(), usize::from(count));
        }
    });
}

/// A port the kernel refuses (one key, two containers) doesn't take the
/// table down with it: the table is installed alone, and the error is the
/// port's.
#[test]
#[ignore = "needs root"]
fn a_refused_port_leaves_the_table_installed() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        let clash = PortRule {
            container: IpAddr::V4(Ipv4Addr::new(10, 88, 0, 3)),
            ..port(8080, None)
        };
        let refused = nft.install(
            &[bridge()],
            &[port(8080, None), clash],
            &ClosedRouting::default(),
            &[],
        );
        assert!(refused.is_err());
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Current);
        assert_eq!(nft.elements(set::BRIDGES).unwrap().len(), 1);
    });
}

#[test]
#[ignore = "needs root"]
fn networks_and_ports_come_and_go() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        nft.install(&[], &[], &ClosedRouting::default(), &[])
            .unwrap();
        let internal = NetworkRules {
            bridge: "shy-internal".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: true,
            isolate_host: false,
            conntrack_limit: false,
            dns: None,
        };
        nft.add_network(&bridge()).unwrap();
        nft.add_network(&internal).unwrap();
        // Twice is fine.
        nft.add_network(&bridge()).unwrap();
        assert_eq!(nft.elements(set::BRIDGES).unwrap().len(), 2);
        assert_eq!(nft.elements(set::INTERNAL).unwrap().len(), 1);

        nft.publish(&[
            port(8080, None),
            port(8080, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        ])
        .unwrap();
        // A port that's published already is refused, and nothing of the
        // batch is applied.
        let error = nft
            .publish(&[port(9090, None), port(8080, None)])
            .unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EEXIST), "{error}");
        assert_eq!(nft.elements(set::PORTS).unwrap().len(), 1);
        assert_eq!(nft.elements(set::PORTS_ADDR).unwrap().len(), 1);
        let listed = &nft.elements(set::PORTS).unwrap()[0];
        assert_eq!(listed.key, [6, 0, 0, 0, 0x1f, 0x90, 0, 0]);

        // Another run's element at the same key isn't ours to take away.
        let mut theirs = port(8080, None);
        theirs.container = IpAddr::V4(Ipv4Addr::new(10, 88, 0, 9));
        nft.unpublish(&[theirs]).unwrap();
        assert_eq!(nft.elements(set::PORTS).unwrap().len(), 1);
        assert_eq!(nft.published(&[theirs]).unwrap(), [Some(port(8080, None))]);
        nft.unpublish(&[
            port(8080, None),
            port(8080, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        ])
        .unwrap();
        // Gone already: fine.
        nft.unpublish(&[port(8080, None)]).unwrap();
        assert!(nft.elements(set::PORTS).unwrap().is_empty());

        nft.remove_network(&internal).unwrap();
        nft.remove_network(&internal).unwrap();
        assert_eq!(nft.elements(set::BRIDGES).unwrap().len(), 1);
        assert!(nft.elements(set::INTERNAL).unwrap().is_empty());
    });
}

#[test]
#[ignore = "needs root"]
fn ipv6_ports_are_published_and_unpublished() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        nft.install(
            &[bridge()],
            &[],
            &ClosedRouting {
                families: vec![crate::Family::V6],
                interfaces: vec!["eth0".into()],
            },
            &[],
        )
        .unwrap();
        assert_eq!(nft.elements(set::CLOSED).unwrap().len(), 1);
        let ports = [
            port6(8080, None),
            port6(8081, Some("2001:db8::1".parse().unwrap())),
            port(8080, None),
        ];
        nft.publish(&ports).unwrap();
        assert_eq!(nft.elements(set::PORTS6).unwrap().len(), 1);
        assert_eq!(nft.elements(set::PORTS6_ADDR).unwrap().len(), 1);
        assert_eq!(nft.elements(set::PORTS).unwrap().len(), 1);
        assert_eq!(
            nft.published(&ports).unwrap(),
            ports.iter().map(|port| Some(*port)).collect::<Vec<_>>()
        );
        nft.unpublish(&ports).unwrap();
        assert!(nft.elements(set::PORTS6).unwrap().is_empty());
        assert!(nft.elements(set::PORTS6_ADDR).unwrap().is_empty());
    });
}

#[test]
#[ignore = "needs root"]
fn adjacent_subnets_and_closed_routing() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        nft.install(&[], &[], &ClosedRouting::default(), &[])
            .unwrap();
        // 10.88.0.0/16 ends where 10.89.0.0/24 starts.
        nft.add_network(&bridge()).unwrap();
        nft.add_network(&NetworkRules {
            bridge: "shy-next".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: false,
            isolate_host: false,
            conntrack_limit: false,
            dns: None,
        })
        .unwrap();
        assert_eq!(nft.elements(set::MASQUERADE).unwrap().len(), 4);
        assert!(nft.elements(set::CLOSED).unwrap().is_empty());
        let uplinks = ["eth0".to_owned(), "wlan0".to_owned()];
        nft.close_routing(crate::Family::V4, &uplinks).unwrap();
        nft.close_routing(crate::Family::V4, &uplinks).unwrap();
        assert_eq!(nft.elements(set::CLOSED).unwrap().len(), 1);
        assert_eq!(nft.elements(set::UPLINKS).unwrap().len(), 2);
        // A rebuild keeps it closed when asked.
        nft.install(
            &[bridge()],
            &[],
            &ClosedRouting {
                families: vec![crate::Family::V4],
                interfaces: uplinks.to_vec(),
            },
            &[],
        )
        .unwrap();
        assert_eq!(nft.elements(set::CLOSED).unwrap().len(), 1);
        assert_eq!(nft.elements(set::UPLINKS).unwrap().len(), 2);
    });
}

// ---- Differential: Shipyard's nf_tables against `nft` (H5) ----

/// `nft ARGS…` in this thread's network namespace, with `input` on its
/// stdin: its output.
fn nft(args: &[&str], input: &str) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("nft")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "nft {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A ruleset with every kind of thing in it: a network with IPv6, an
/// internal one, IPv4 and IPv6 ports (one on an address), closed routing.
fn install_everything(nft: &mut Nftables) -> Vec<PortRule> {
    let ports = vec![
        port(8080, None),
        port(5353, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        port6(8443, None),
    ];
    nft.install(
        &[
            bridge(),
            NetworkRules {
                bridge: "shy-internal".into(),
                subnet: "10.90.0.0/24".parse().unwrap(),
                subnet6: None,
                internal: true,
                isolate_host: false,
                conntrack_limit: false,
                dns: None,
            },
            // Kept from the host, but for its DNS server.
            NetworkRules {
                bridge: "shy-isolated".into(),
                subnet: "10.91.0.0/24".parse().unwrap(),
                subnet6: None,
                internal: false,
                isolate_host: true,
                // And limited.
                conntrack_limit: true,
                dns: Some(Ipv4Addr::new(10, 91, 0, 1)),
            },
        ],
        &ports,
        &ClosedRouting {
            families: vec![crate::Family::V4, crate::Family::V6],
            interfaces: vec!["eth0".to_owned()],
        },
        &[],
    )
    .unwrap();
    ports
}

/// `nft` reads Shipyard's table as Shipyard meant it, and builds it again
/// from its own listing to the same thing.
///
/// - Nothing in it is beyond nft (`unknown`, `[invalid`).
/// - The key rules read as intended: the way out, published ports, internal
///   networks, closed routing, the loopback guard.
/// - nft's listing is a fixed point: listed, rebuilt from that text, listed
///   again, the same; each chain has as many rules; and Shipyard reads the
///   rebuilt table's sets and ports as its own.
///
/// Not byte for byte: nft encodes some rules differently but equivalently
/// (seen with nft 1.1.3: `ip daddr 127.0.0.0/8` compares one byte where
/// Shipyard masks four, and nft puts the protocol check right before the
/// address), so the rules are compared as nft prints them.
#[test]
#[ignore = "needs root"]
fn nft_reads_and_rebuilds_shipyards_table() {
    if !reference_tool("nft") {
        return;
    }
    in_new_netns(|| {
        let mut ours = Nftables::new().unwrap();
        let ports = install_everything(&mut ours);
        let listed = nft(&["list", "table", "inet", "shipyard"], "");
        assert!(!listed.contains("unknown"), "{listed}");
        assert!(!listed.contains("[invalid"), "{listed}");
        for rule in [
            "iifname != \"lo\" ip daddr 127.0.0.0/8 drop",
            "iifname @bridges ct state invalid drop",
            "ct state established,related accept",
            "iifname . oifname @same_bridge accept",
            "iifname @internal drop",
            "oifname @internal drop",
            "ct status dnat accept",
            "oifname @bridges drop",
            "meta nfproto @closed iifname @uplinks oifname @uplinks drop",
            "ip saddr @masquerade4 oifname != @bridges masquerade",
            "ip6 saddr @masquerade6 oifname != @bridges masquerade",
            "iifname @isolated drop",
            "iifname . ip daddr . meta l4proto . th dport @dns_allow accept",
            "iifname @isolated ip daddr 169.254.0.0/16 drop",
            "ip daddr 100.100.100.200 drop",
            "ip6 daddr fd00:ec2::254 drop",
            "ip6 daddr fd00:ec2::23 drop",
            "ip6 daddr fd20:ce::254 drop",
            // The isolated network's DNS server, as nft reads the element.
            "\"shy-isolated\" . 10.91.0.1 . udp . 53",
            "\"shy-isolated\" . 10.91.0.1 . tcp . 53",
            // The limit, and its counting sets, as nft reads them.
            "add @conns4 { ip saddr ct count over 8192 } reject with tcp reset",
            "add @conns4 { ip saddr ct count over 8192 } reject with icmpx admin-prohibited",
            "add @conns6 { ip6 saddr ct count over 8192 } reject with tcp reset",
            "add @conns6 { ip6 saddr ct count over 8192 } reject with icmpx admin-prohibited",
            "ip6 saddr fe80::/10 reject with icmpx admin-prohibited",
            "size 65536",
            "flags dynamic",
        ] {
            assert!(listed.contains(rule), "no `{rule}` in:\n{listed}");
        }
        let per_chain = |chains: Vec<String>| {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for chain in chains {
                match counts.iter_mut().find(|(name, _)| *name == chain) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((chain, 1)),
                }
            }
            counts
        };
        let rules = per_chain(ours.rule_chains().unwrap());
        // A set's elements, in no particular order (nft adds them in its
        // own). Without an interval set's closing element at 0 ("nothing
        // from 0 up to the first interval"): Shipyard's table holds one, nft
        // 1.1 doesn't write it, older nft does, and the kernel takes either.
        let held = |nft: &mut Nftables, name: &str| {
            let mut keys: Vec<_> = nft
                .elements(name)
                .unwrap()
                .into_iter()
                .filter(|element| {
                    !(element.interval_end && element.key.iter().all(|&byte| byte == 0))
                })
                .map(|element| (element.key, element.data, element.interval_end))
                .collect();
            keys.sort();
            keys
        };
        let elements: Vec<_> = [
            set::BRIDGES,
            set::MASQUERADE,
            set::MASQUERADE6,
            set::PORTS,
            set::ISOLATED,
            set::DNS_ALLOW,
            set::LIMITED,
        ]
        .into_iter()
        .map(|name| (name, held(&mut ours, name)))
        .collect();

        nft(&["delete", "table", "inet", "shipyard"], "");
        nft(&["-f", "-"], &listed);
        let again = nft(&["list", "table", "inet", "shipyard"], "");
        assert_eq!(listed, again, "nft's listing isn't a fixed point");
        // From here on, the table is nft's.
        let mut rebuilt = ours;
        assert_eq!(per_chain(rebuilt.rule_chains().unwrap()), rules);
        for (name, before) in elements {
            assert_eq!(held(&mut rebuilt, name), before, "{name}");
        }
        assert!(
            rebuilt
                .published(&ports)
                .unwrap()
                .iter()
                .all(Option::is_some)
        );
    });
}

/// The name nft gives a key or value type (`NFTA_SET_KEY_TYPE`), each part
/// of a concatenation in order.
fn type_names(id: u32) -> Vec<&'static str> {
    use super::consts::TYPE_BITS;
    let mut parts = Vec::new();
    let mut id = id;
    while id != 0 {
        parts.push(match id & ((1 << TYPE_BITS) - 1) {
            2 => "nf_proto",
            7 => "ipv4_addr",
            8 => "ipv6_addr",
            12 => "inet_proto",
            13 => "inet_service",
            41 => "ifname",
            other => panic!("a type the test doesn't name: {other}"),
        });
        id >>= TYPE_BITS;
    }
    parts.reverse();
    parts
}

/// A type as `nft -j` writes it: a name, or a list of names.
fn json_type(names: &[&str]) -> serde_json::Value {
    match names {
        [one] => (*one).into(),
        many => many
            .iter()
            .map(|name| serde_json::Value::from(*name))
            .collect(),
    }
}

/// A type as `nft -j` wrote it, in the list form: nftables before 1.1 (Ubuntu
/// 24.04's 1.0.9) writes a map's concatenated value type as one string,
/// `"ipv4_addr . inet_service"`; later ones as a list of names.
fn their_type(value: &serde_json::Value) -> serde_json::Value {
    match value.as_str() {
        Some(names) if names.contains(" . ") => {
            names.split(" . ").map(serde_json::Value::from).collect()
        }
        _ => value.clone(),
    }
}

/// `nft -j` describes Shipyard's base chains and sets as Shipyard made
/// them (`ruleset::chains` and `ruleset::sets`): each chain's type, hook,
/// priority and policy; each set's key type, a map's value type, and
/// whether it's an interval set.
#[test]
#[ignore = "needs root"]
fn nft_json_describes_shipyards_chains_and_sets() {
    if !reference_tool("nft") {
        return;
    }
    in_new_netns(|| {
        let mut ours = Nftables::new().unwrap();
        install_everything(&mut ours);
        let listed = nft(&["-j", "list", "table", "inet", "shipyard"], "");
        let json: serde_json::Value = serde_json::from_str(&listed).unwrap();
        let objects = json["nftables"].as_array().unwrap();
        let find = |kind: &str, name: &str| {
            objects
                .iter()
                .filter_map(|object| object.get(kind))
                .find(|their| their["name"] == name)
                .unwrap_or_else(|| panic!("nft lists no {kind} {name}"))
        };
        for chain in super::ruleset::chains() {
            let theirs = find("chain", chain.name);
            let hook = match chain.hook {
                0 => "prerouting",
                1 => "input",
                2 => "forward",
                3 => "output",
                4 => "postrouting",
                other => panic!("hook {other}"),
            };
            let policy = if chain.policy == NF_ACCEPT {
                "accept"
            } else {
                "drop"
            };
            assert_eq!(
                (
                    theirs["type"].as_str(),
                    theirs["hook"].as_str(),
                    theirs["prio"].as_i64(),
                    theirs["policy"].as_str()
                ),
                (
                    Some(chain.kind),
                    Some(hook),
                    Some(i64::from(chain.priority)),
                    Some(policy)
                ),
                "{}",
                chain.name
            );
        }
        for set in super::ruleset::sets() {
            let theirs = match set.data {
                Some(_) => find("map", set.name),
                None => find("set", set.name),
            };
            assert_eq!(
                their_type(&theirs["type"]),
                json_type(&type_names(set.key.0)),
                "{}",
                set.name
            );
            if let Some((data, _)) = set.data {
                assert_eq!(
                    their_type(&theirs["map"]),
                    json_type(&type_names(data)),
                    "{}",
                    set.name
                );
            }
            let interval = theirs["flags"]
                .as_array()
                .is_some_and(|flags| flags.iter().any(|flag| flag == "interval"));
            assert_eq!(
                interval,
                set.flags & super::consts::NFT_SET_INTERVAL != 0,
                "{}",
                set.name
            );
        }
    });
}
