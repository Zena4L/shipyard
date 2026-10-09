//! rtnetlink against the real kernel.
//!
//! The normal-user tests only read (this namespace's links, addresses and
//! routes). The root tests change things, each in a network namespace of its
//! own: a thread of the test's own unshares one (`unshare(CLONE_NEWNET)`
//! affects only the calling thread), so nothing they make is ever seen by the
//! host, another test, or the test harness's threads, and it all goes when the
//! thread ends.

use std::net::IpAddr;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

use nix::errno::Errno;
use nix::sched::{CloneFlags, unshare};

use super::{LinkChange, Route, RouteType, Rtnetlink};
use crate::ipnet::{Family, IpNet};
use crate::linux_test::{in_new_netns, reference_tool, this_netns};

/// The hairpin mode of the bridge port with `index`, as the kernel lists it
/// for the bridge family (`RTM_GETLINK` with `AF_BRIDGE`: one message per
/// port, with its settings in `IFLA_PROTINFO`). Read here, not from
/// /sys/class/net: sysfs shows the namespace it was mounted in, not this
/// thread's.
fn hairpin(index: u32) -> Option<bool> {
    use super::consts::{AF_BRIDGE, IFLA_BRPORT_MODE, IFLA_PROTINFO, RTM_GETLINK};
    use super::link::LinkHeader;
    use crate::netlink::{MessageBuilder, Socket};

    let mut message = MessageBuilder::new(RTM_GETLINK, 0);
    message.raw(
        &LinkHeader {
            family: AF_BRIDGE,
            ..LinkHeader::default()
        }
        .encode(),
    );
    let found = Socket::route()
        .unwrap()
        .dump("listing bridge ports", &message, |message| {
            let (header, attrs) = message.split(LinkHeader::LEN)?;
            if LinkHeader::decode(header)?.index != index {
                return Ok(None);
            }
            for attr in attrs {
                let attr = attr?;
                if attr.kind == IFLA_PROTINFO {
                    for setting in attr.nested() {
                        let setting = setting?;
                        if setting.kind == IFLA_BRPORT_MODE {
                            return Ok(Some(setting.u8()? != 0));
                        }
                    }
                }
            }
            Ok(None)
        })
        .unwrap();
    found.into_iter().next()
}

fn net(text: &str) -> IpNet {
    text.parse().unwrap()
}

#[test]
fn the_loopback_interface_is_listed() {
    let mut rtnl = Rtnetlink::new().unwrap();
    let links = rtnl.links().unwrap();
    let lo = links
        .iter()
        .find(|link| link.name == "lo")
        .expect("lo is listed");
    assert_eq!(lo.index, 1);
    assert_eq!(rtnl.link("lo").unwrap().as_ref(), Some(lo));
}

#[test]
fn a_missing_interface_is_none() {
    let mut rtnl = Rtnetlink::new().unwrap();
    assert_eq!(rtnl.link("shy-none0").unwrap(), None);
}

#[test]
fn addresses_and_routes_are_listed() {
    let mut rtnl = Rtnetlink::new().unwrap();
    // Whatever is there parses; an up loopback has 127.0.0.1/8.
    let addresses = rtnl.addresses(Some(1)).unwrap();
    assert!(addresses.iter().all(|address| address.index == 1));
    if rtnl.link("lo").unwrap().is_some_and(|lo| lo.is_up()) {
        assert!(
            addresses
                .iter()
                .any(|address| address.net == net("127.0.0.1/8")),
            "{addresses:?}"
        );
    }
    rtnl.routes(Family::V4).unwrap();
    rtnl.routes(Family::V6).unwrap();
    // An interface that isn't there has no addresses.
    assert_eq!(rtnl.addresses(Some(u32::MAX - 1)).unwrap(), []);
}

#[test]
fn a_bad_name_is_refused_before_asking_the_kernel() {
    let mut rtnl = Rtnetlink::new().unwrap();
    let error = rtnl.create_bridge("a-name-longer-than-15").unwrap_err();
    assert!(
        error.to_string().contains("longer than 15 bytes"),
        "{error}"
    );
}

#[test]
fn a_normal_user_cant_make_a_bridge() {
    if nix::unistd::geteuid().is_root() {
        return; // the root tests below cover making one
    }
    let error = Rtnetlink::new()
        .unwrap()
        .create_bridge("shy-test0")
        .unwrap_err();
    assert_eq!(error.errno(), Some(Errno::EPERM));
    assert_eq!(
        error.to_string(),
        format!("creating the bridge shy-test0: {}", Errno::EPERM.desc())
    );
}

#[test]
#[ignore = "needs root"]
fn a_bridge_is_made_brought_up_and_deleted() {
    in_new_netns(|| {
        let mut rtnl = Rtnetlink::new().unwrap();
        rtnl.create_bridge("shy-test0").unwrap();
        let bridge = rtnl.link("shy-test0").unwrap().unwrap();
        assert_eq!(bridge.kind.as_deref(), Some("bridge"));
        assert!(!bridge.is_up());
        // a second one with the same name is refused, with the kernel's errno
        let error = rtnl.create_bridge("shy-test0").unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EEXIST));
        rtnl.change(bridge.index, LinkChange::Up(true)).unwrap();
        assert!(rtnl.link("shy-test0").unwrap().unwrap().is_up());
        rtnl.change(bridge.index, LinkChange::Mtu(1400)).unwrap();
        assert_eq!(rtnl.link("shy-test0").unwrap().unwrap().mtu, Some(1400));
        rtnl.delete(bridge.index).unwrap();
        assert_eq!(rtnl.link("shy-test0").unwrap(), None);
    });
}

#[test]
#[ignore = "needs root"]
fn the_kernels_reason_comes_with_its_refusal() {
    in_new_netns(|| {
        let mut rtnl = Rtnetlink::new().unwrap();
        // An MTU the bridge can't take: the kernel explains (probe N9).
        rtnl.create_bridge("shy-test0").unwrap();
        let index = rtnl.link("shy-test0").unwrap().unwrap().index;
        let error = rtnl.change(index, LinkChange::Mtu(10)).unwrap_err();
        assert_eq!(error.errno(), Some(Errno::EINVAL));
        assert!(error.to_string().contains("(the kernel says: "), "{error}");
    });
}

#[test]
#[ignore = "needs root"]
fn a_veth_peer_is_born_in_another_namespace() {
    in_new_netns(|| {
        let container = this_netns();
        unshare(CloneFlags::CLONE_NEWNET).unwrap(); // "the host"
        let host = this_netns();
        let mut rtnl = Rtnetlink::new().unwrap();
        rtnl.create_veth("shy-v0", "eth0", Some(container.as_fd()), None)
            .unwrap();
        assert!(rtnl.link("shy-v0").unwrap().is_some());
        // The peer never existed here, under any name.
        assert_eq!(rtnl.link("eth0").unwrap(), None);
        assert_eq!(rtnl.links().unwrap().len(), 2, "lo and shy-v0 only");

        // A socket made inside sees it, and keeps acting inside once the
        // thread is back (probe N4).
        let mut inside = Rtnetlink::in_namespace(container.as_fd()).unwrap();
        assert_eq!(
            this_netns().metadata().unwrap().ino(),
            host.metadata().unwrap().ino()
        );
        let eth0 = inside.link("eth0").unwrap().expect("eth0 is inside");
        assert_eq!(eth0.kind.as_deref(), Some("veth"));
        inside.change(eth0.index, LinkChange::Up(true)).unwrap();
        inside
            .add_address(eth0.index, net("10.200.0.2/24"))
            .unwrap();
        // It landed inside, and only inside.
        assert!(
            inside
                .addresses(Some(eth0.index))
                .unwrap()
                .iter()
                .any(|a| a.net == net("10.200.0.2/24"))
        );
        assert_eq!(
            rtnl.addresses(None)
                .unwrap()
                .iter()
                .filter(|a| a.net == net("10.200.0.2/24"))
                .count(),
            0
        );

        // Deleting one end deletes the other.
        let host_end = rtnl.link("shy-v0").unwrap().unwrap();
        rtnl.delete(host_end.index).unwrap();
        assert_eq!(inside.link("eth0").unwrap(), None);
    });
}

#[test]
#[ignore = "needs root"]
fn a_link_moves_and_is_renamed() {
    in_new_netns(|| {
        let other = this_netns();
        unshare(CloneFlags::CLONE_NEWNET).unwrap();
        let mut rtnl = Rtnetlink::new().unwrap();
        rtnl.create_veth("shy-v1", "shy-v1p", None, None).unwrap();
        let peer = rtnl.link("shy-v1p").unwrap().unwrap();
        rtnl.change(peer.index, LinkChange::Name("shy-v1q"))
            .unwrap();
        let peer = rtnl.link("shy-v1q").unwrap().expect("renamed");
        rtnl.move_to(peer.index, other.as_fd(), None).unwrap();
        assert_eq!(rtnl.link("shy-v1q").unwrap(), None);
        let mut there = Rtnetlink::in_namespace(other.as_fd()).unwrap();
        assert!(there.link("shy-v1q").unwrap().is_some());
    });
}

#[test]
#[ignore = "needs root"]
fn a_port_joins_a_bridge_with_hairpin() {
    in_new_netns(|| {
        let mut rtnl = Rtnetlink::new().unwrap();
        rtnl.create_bridge("shy-br0").unwrap();
        rtnl.create_veth("shy-v2", "shy-v2p", None, None).unwrap();
        let bridge = rtnl.link("shy-br0").unwrap().unwrap().index;
        let port = rtnl.link("shy-v2").unwrap().unwrap().index;
        rtnl.change(port, LinkChange::Master(bridge)).unwrap();
        assert_eq!(rtnl.link("shy-v2").unwrap().unwrap().master, Some(bridge));
        assert_eq!(hairpin(port), Some(false));
        rtnl.set_hairpin(port, true).unwrap();
        assert_eq!(hairpin(port), Some(true));
        rtnl.change(port, LinkChange::Master(0)).unwrap();
        assert_eq!(rtnl.link("shy-v2").unwrap().unwrap().master, None);
    });
}

#[test]
#[ignore = "needs root"]
fn addresses_and_routes_are_added_and_read_back() {
    in_new_netns(|| {
        let mut rtnl = Rtnetlink::new().unwrap();
        rtnl.create_bridge("shy-br1").unwrap();
        let bridge = rtnl.link("shy-br1").unwrap().unwrap().index;
        rtnl.change(bridge, LinkChange::Up(true)).unwrap();
        rtnl.add_address(bridge, net("10.200.0.1/24")).unwrap();
        rtnl.add_address(bridge, net("fd00:200::1/64")).unwrap();
        let addresses = rtnl.addresses(Some(bridge)).unwrap();
        assert!(
            addresses.iter().any(|a| a.net == net("10.200.0.1/24")),
            "{addresses:?}"
        );
        assert!(
            addresses.iter().any(|a| a.net == net("fd00:200::1/64")),
            "{addresses:?}"
        );
        // the same address again is refused
        assert_eq!(
            rtnl.add_address(bridge, net("10.200.0.1/24"))
                .unwrap_err()
                .errno(),
            Some(Errno::EEXIST)
        );

        let gateway: IpAddr = "10.200.0.254".parse().unwrap();
        let through = Route {
            destination: net("10.201.0.0/16"),
            gateway: Some(gateway),
            index: Some(bridge),
            route_type: RouteType::Unicast,
        };
        rtnl.add_route(&through).unwrap();
        rtnl.add_route(&Route::default_via(gateway)).unwrap();
        let routes = rtnl.routes(Family::V4).unwrap();
        assert!(routes.contains(&through), "{routes:?}");
        assert!(
            routes
                .iter()
                .any(|r| r.destination == IpNet::any(Family::V4) && r.gateway == Some(gateway)),
            "{routes:?}"
        );
        // the kernel's own route for the bridge's subnet is there too
        assert!(
            routes
                .iter()
                .any(|r| r.destination == net("10.200.0.0/24") && r.gateway.is_none())
        );
    });
}

#[test]
#[ignore = "needs root"]
fn a_dump_longer_than_a_datagram_is_read_whole() {
    in_new_netns(|| {
        let mut rtnl = Rtnetlink::new().unwrap();
        // About 200 KB of link descriptions: more than one datagram (at most
        // 32 KiB) can hold, so the kernel sends them in several.
        for n in 0..150 {
            rtnl.create_bridge(&format!("shy-many{n}")).unwrap();
        }
        let links = rtnl.links().unwrap();
        assert_eq!(links.len(), 151);
        assert_eq!(
            links
                .iter()
                .filter(|link| link.name.starts_with("shy-many"))
                .count(),
            150
        );
    });
}

// ---- Differential: Shipyard's rtnetlink against `ip -j` (H5) ----

/// `ip -j ARGS…` in this thread's network namespace (a fork copies the
/// calling thread's), as JSON.
fn ip_json(args: &[&str]) -> serde_json::Value {
    let output = std::process::Command::new("ip")
        .arg("-j")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ip {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("ip's JSON")
}

/// What a link looks like to compare: name, kind, up, MTU, bridge, MAC.
type LinkView = (
    String,
    Option<String>,
    bool,
    Option<u32>,
    Option<String>,
    Option<String>,
);

fn mac_text(mac: &[u8]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Asserts that Shipyard's rtnetlink and `ip` see this namespace the same:
/// every link, every address, and every route of the main table.
fn ip_sees_the_same(rtnl: &mut Rtnetlink) {
    let links = rtnl.links().unwrap();
    same_links(&links, &ip_json(&["-d", "link", "show"]));
    same_addresses(rtnl, &links);
    same_routes(rtnl, &links);
}

/// The name of the link with `index`.
fn name_of(links: &[super::Link], index: u32) -> Option<String> {
    links
        .iter()
        .find(|link| link.index == index)
        .map(|link| link.name.clone())
}

/// Every link: name, kind, up, MTU, bridge, and an Ethernet link's MAC
/// (`ip` shows a tunnel's, `tunl0`'s say, as an IP address).
fn same_links(links: &[super::Link], ip_links: &serde_json::Value) {
    let ethernet: Vec<&str> = ip_links
        .as_array()
        .unwrap()
        .iter()
        .filter(|link| matches!(link["link_type"].as_str(), Some("ether" | "loopback")))
        .filter_map(|link| link["ifname"].as_str())
        .collect();
    let mut ours: Vec<LinkView> = links
        .iter()
        .map(|link| {
            (
                link.name.clone(),
                link.kind.clone(),
                link.flags & libc::IFF_UP as u32 != 0,
                link.mtu,
                link.master.and_then(|index| name_of(links, index)),
                link.mac
                    .as_deref()
                    .filter(|_| ethernet.contains(&link.name.as_str()))
                    .map(mac_text),
            )
        })
        .collect();
    let mut theirs: Vec<LinkView> = ip_links
        .as_array()
        .unwrap()
        .iter()
        .map(|link| {
            let text = |key: &str| link[key].as_str().map(str::to_owned);
            (
                text("ifname").unwrap(),
                link["linkinfo"]["info_kind"].as_str().map(str::to_owned),
                link["flags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|flag| flag == "UP"),
                link["mtu"].as_u64().map(|mtu| u32::try_from(mtu).unwrap()),
                text("master"),
                text("address").filter(|_| ethernet.contains(&text("ifname").unwrap().as_str())),
            )
        })
        .collect();
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs, "links");
}

/// Every address, by interface.
fn same_addresses(rtnl: &mut Rtnetlink, links: &[super::Link]) {
    let index_of = |name: &str| {
        links
            .iter()
            .find(|link| link.name == name)
            .map(|link| link.index)
            .unwrap()
    };
    let mut ours: Vec<(u32, IpNet)> = rtnl
        .addresses(None)
        .unwrap()
        .into_iter()
        .map(|address| (address.index, address.net))
        .collect();
    let mut theirs: Vec<(u32, IpNet)> = Vec::new();
    for link in ip_json(&["addr", "show"]).as_array().unwrap() {
        let index = index_of(link["ifname"].as_str().unwrap());
        for info in link["addr_info"].as_array().unwrap() {
            let text = format!(
                "{}/{}",
                info["local"].as_str().unwrap(),
                info["prefixlen"].as_u64().unwrap()
            );
            theirs.push((index, net(&text)));
        }
    }
    ours.sort_by_key(|(index, net)| (*index, net.to_string()));
    theirs.sort_by_key(|(index, net)| (*index, net.to_string()));
    assert_eq!(ours, theirs, "addresses");
}

/// Every route of the main table: destination, gateway, interface, type.
fn same_routes(rtnl: &mut Rtnetlink, links: &[super::Link]) {
    for (family, flag) in [(Family::V4, "-4"), (Family::V6, "-6")] {
        let mut ours: Vec<String> = rtnl
            .routes(family)
            .unwrap()
            .iter()
            .map(|route| {
                format!(
                    "{} via {:?} dev {:?} {:?}",
                    route.destination,
                    route.gateway,
                    route.index.and_then(|index| name_of(links, index)),
                    route.route_type
                )
            })
            .collect();
        let mut theirs: Vec<String> = ip_json(&[flag, "route", "show", "table", "main"])
            .as_array()
            .unwrap()
            .iter()
            .map(|route| ip_route_text(route, family))
            .collect();
        ours.sort();
        theirs.sort();
        assert_eq!(ours, theirs, "{family:?} routes");
    }
}

/// A route `ip -j` lists, in the form [`same_routes`] compares.
fn ip_route_text(route: &serde_json::Value, family: Family) -> String {
    let destination = match route["dst"].as_str().unwrap() {
        "default" => IpNet::any(family).to_string(),
        dst if dst.contains('/') => dst.to_owned(),
        host => format!("{host}/{}", if family == Family::V4 { 32 } else { 128 }),
    };
    let gateway: Option<IpAddr> = route["gateway"]
        .as_str()
        .map(|gateway| gateway.parse().unwrap());
    let kind = match route["type"].as_str() {
        None | Some("unicast") => RouteType::Unicast,
        Some("blackhole") => RouteType::Blackhole,
        Some("unreachable") => RouteType::Unreachable,
        Some("prohibit") => RouteType::Prohibit,
        Some(other) => panic!("a route type Shipyard doesn't read: {other}"),
    };
    format!(
        "{destination} via {gateway:?} dev {:?} {kind:?}",
        route["dev"].as_str().map(str::to_owned)
    )
}

/// What Shipyard's rtnetlink makes, `ip` reads back the same: a bridge with
/// an MTU, a veth pair with one end its port, IPv4 and IPv6 addresses, and
/// routes (through a gateway, default, blackhole).
#[test]
#[ignore = "needs root"]
fn ip_reads_what_shipyard_makes() {
    if !reference_tool("ip") {
        return;
    }
    in_new_netns(|| {
        let mut rtnl = Rtnetlink::new().unwrap();
        rtnl.create_bridge("shy-br9").unwrap();
        let bridge = rtnl.link("shy-br9").unwrap().unwrap().index;
        rtnl.change(bridge, LinkChange::Mtu(1400)).unwrap();
        rtnl.change(bridge, LinkChange::Address([0x02, 0x42, 0x0a, 0xc9, 0, 1]))
            .unwrap();
        rtnl.change(bridge, LinkChange::Up(true)).unwrap();
        rtnl.create_veth("shy-v9", "shy-v9p", None, None).unwrap();
        let port = rtnl.link("shy-v9").unwrap().unwrap().index;
        rtnl.change(port, LinkChange::Master(bridge)).unwrap();
        rtnl.change(port, LinkChange::Up(true)).unwrap();
        rtnl.add_address(bridge, net("10.201.0.1/24")).unwrap();
        rtnl.add_address(bridge, net("fd00:9::1/64")).unwrap();
        rtnl.add_route(&Route {
            destination: net("10.202.0.0/24"),
            gateway: Some("10.201.0.2".parse().unwrap()),
            index: None,
            route_type: RouteType::Unicast,
        })
        .unwrap();
        rtnl.add_route(&Route::default_via("10.201.0.254".parse().unwrap()))
            .unwrap();
        rtnl.add_route(&Route::default_via("fd00:9::fe".parse().unwrap()))
            .unwrap();
        rtnl.add_route(&Route {
            destination: net("10.203.0.0/16"),
            gateway: None,
            index: None,
            route_type: RouteType::Blackhole,
        })
        .unwrap();
        ip_sees_the_same(&mut rtnl);
    });
}

/// What `ip` makes, Shipyard's rtnetlink reads the same.
#[test]
#[ignore = "needs root"]
fn shipyard_reads_what_ip_makes() {
    if !reference_tool("ip") {
        return;
    }
    in_new_netns(|| {
        let ip = |args: &str| {
            let status = std::process::Command::new("ip")
                .args(args.split_whitespace())
                .status()
                .unwrap();
            assert!(status.success(), "ip {args}");
        };
        for args in [
            "link add shy-br8 type bridge",
            "link set shy-br8 mtu 1300 address 02:42:0a:c8:00:01 up",
            "link add shy-v8 type veth peer name shy-v8p",
            "link set shy-v8 master shy-br8 up",
            "addr add 10.208.0.1/24 dev shy-br8",
            "addr add fd00:8::1/64 dev shy-br8 nodad",
            "route add 10.209.0.0/24 via 10.208.0.2",
            "route add default via 10.208.0.254",
            "route add unreachable 10.210.0.0/16",
            "route add prohibit 10.211.0.0/16",
            "-6 route add default via fd00:8::fe",
        ] {
            ip(args);
        }
        let mut rtnl = Rtnetlink::new().unwrap();
        ip_sees_the_same(&mut rtnl);
    });
}
