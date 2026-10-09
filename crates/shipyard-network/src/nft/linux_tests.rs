//! `nf_tables` and conntrack against the real kernel, as root, each test in
//! a network namespace of its own (see `route/linux_tests.rs`): the
//! firewall changed is that namespace's, never the host's.

use std::net::{IpAddr, Ipv4Addr};

use nix::errno::Errno;

use super::consts::{NF_ACCEPT, NF_INET_FORWARD, NFPROTO_INET};
use super::objects::{delete_table, new_table};
use super::ruleset::{ClosedRouting, FAMILY, NetworkRules, PortRule, TABLE, set};
use super::{Nftables, Ruleset};
use crate::linux_test::in_new_netns;
use crate::netlink::Socket;
use crate::nft::consts::NFNL_SUBSYS_NFTABLES;
use crate::ports::{Protocol, Published};

fn bridge() -> NetworkRules {
    NetworkRules {
        bridge: "shipyard0".into(),
        subnet: "10.88.0.0/16".parse().unwrap(),
        subnet6: Some("fd00:88::/64".parse().unwrap()),
        internal: false,
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
        nft.install(&[bridge()], &[port(8080, None)], &ClosedRouting::default())
            .unwrap();
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Current);
        // Every base chain is there, accepting by default.
        let ours: Vec<_> = nft
            .chains()
            .unwrap()
            .into_iter()
            .filter(|chain| chain.family == NFPROTO_INET && chain.table == TABLE)
            .collect();
        assert_eq!(ours.len(), 5, "{ours:?}");
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
        nft.install(&[bridge()], &[], &ClosedRouting::default())
            .unwrap();
        assert_eq!(nft.ruleset().unwrap(), Ruleset::Current);
        assert!(nft.elements(set::PORTS).unwrap().is_empty());
    });
}

#[test]
#[ignore = "needs root"]
fn networks_and_ports_come_and_go() {
    in_new_netns(|| {
        let mut nft = Nftables::new().unwrap();
        nft.install(&[], &[], &ClosedRouting::default()).unwrap();
        let internal = NetworkRules {
            bridge: "shy-internal".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: true,
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
        nft.install(&[], &[], &ClosedRouting::default()).unwrap();
        // 10.88.0.0/16 ends where 10.89.0.0/24 starts.
        nft.add_network(&bridge()).unwrap();
        nft.add_network(&NetworkRules {
            bridge: "shy-next".into(),
            subnet: "10.89.0.0/24".parse().unwrap(),
            subnet6: None,
            internal: false,
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
        )
        .unwrap();
        assert_eq!(nft.elements(set::CLOSED).unwrap().len(), 1);
        assert_eq!(nft.elements(set::UPLINKS).unwrap().len(), 2);
    });
}
