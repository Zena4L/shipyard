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
use crate::linux_test::{in_new_netns, this_netns};

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
