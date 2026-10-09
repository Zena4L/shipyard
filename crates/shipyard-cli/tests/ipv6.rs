//! End-to-end tests of IPv6 (Linux, root): networks made with `--ipv6`
//! give containers an IPv6 address beside their IPv4 one, a way out
//! through the host's IPv6 address (NAT66), published ports on both
//! families, AAAA answers, and the host's routing kept closed to all but
//! its containers.
//!
//! As the other network tests: each in a network namespace of its own,
//! with an [`Upstream`] (dual-stack: `2001:db8::1` the host's side,
//! `2001:db8::2` the far side), networks in a scratch directory.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::os::fd::AsFd as _;

use shipyard_network::IpNet;
use shipyard_network::dns::message::{RData, rtype};
use shipyard_network::route::{Route, Rtnetlink};
use shipyard_test_support::net::{
    NetworkDir, Server, Upstream, fetch, fetch_from, in_netns, resolve,
};
use shipyard_test_support::{Shipyard, in_new_netns};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// The IPv6 address of the container named `container` on `network`.
fn address6(networks: &NetworkDir, network: &str, container: &str) -> Ipv6Addr {
    networks.lease(network, container)["IPv6Address"]
        .as_str()
        .unwrap_or_else(|| panic!("{container} has no IPv6 address on {network}"))
        .parse()
        .unwrap()
}

/// The kernel setting at `path` (under `/proc/sys`), as this thread's
/// network namespace has it.
fn setting(path: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/{path}"))
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
#[ignore = "needs root"]
fn containers_on_an_ipv6_network_talk_over_it_and_the_host_reaches_them() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["--ipv6", "app"]);
        let config = networks.inspect("app");
        let subnet6: IpNet = config["Subnet6"].as_str().unwrap().parse().unwrap();
        assert_eq!(subnet6.prefix(), 64);
        assert!(matches!(subnet6.addr(), IpAddr::V6(ip) if ip.octets()[0] == 0xfd));
        let (one, one_netns) = networks.sleeping(&["--network", "app", "--name", "one"]);
        let (two, two_netns) = networks.sleeping(&["--network", "app", "--name", "two"]);
        let two6 = address6(&networks, "app", "two");
        assert!(subnet6.contains(IpAddr::V6(two6)));
        let _server = Server::start(&two_netns, 7000, "two");
        assert_eq!(
            fetch_from(&one_netns, SocketAddr::from((two6, 7000))).as_deref(),
            Some("two")
        );
        assert_eq!(
            fetch(SocketAddr::from((two6, 7000))).as_deref(),
            Some("two")
        );
        // Its name has its IPv6 address too.
        let gateway = networks.gateway("app");
        let aaaa = resolve(&one_netns, gateway, "two", rtype::AAAA).unwrap();
        assert_eq!(
            aaaa.answers.first().map(|answer| &answer.data),
            Some(&RData::Aaaa(two6))
        );
        // The container routes nothing, on IPv6 either, and takes no router
        // advertisement (another container's would route its traffic).
        let inside = |path: &'static str| in_netns(&two_netns, move || setting(path));
        assert_eq!(inside("net/ipv6/conf/all/forwarding"), "0");
        assert_eq!(inside("net/ipv6/conf/eth0/accept_ra"), "0");
        assert_eq!(inside("net/ipv6/conf/eth0/autoconf"), "0");
        // Nor does the host, from its bridge.
        let bridge = config["Bridge"].as_str().unwrap().to_owned();
        assert_eq!(setting(&format!("net/ipv6/conf/{bridge}/accept_ra")), "0");
        one.stop();
        two.stop();

        // A network without IPv6 has none at all: no link-local address
        // either, nor router advertisements.
        networks.create(&["plain"]);
        let (three, three_netns) = networks.sleeping(&["--network", "plain"]);
        let disabled = in_netns(&three_netns, || setting("net/ipv6/conf/eth0/disable_ipv6"));
        assert_eq!(disabled, "1");
        let plain = networks.inspect("plain")["Bridge"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(setting(&format!("net/ipv6/conf/{plain}/disable_ipv6")), "1");
        three.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn the_way_out_is_the_hosts_ipv6_address_and_router_advertisements_still_count() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let server = Server::start(upstream.netns(), 7000, "out");
        // The uplink takes router advertisements, and the host doesn't
        // forward (a new namespace's IPv6 settings are the defaults).
        std::fs::write("/proc/sys/net/ipv6/conf/up0/accept_ra", "1").unwrap();
        assert_eq!(setting("net/ipv6/conf/all/forwarding"), "0");
        networks.create(&["--ipv6", "app"]);
        let (running, netns) = networks.sleeping(&["--network", "app"]);
        assert_eq!(
            fetch_from(&netns, SocketAddr::from((Upstream::ADDRESS6, 7000))).as_deref(),
            Some("out")
        );
        // The world sees the host's address (NAT66).
        assert_eq!(
            server.peer().map(|peer| peer.ip()),
            Some(IpAddr::V6(Upstream::HOST6))
        );
        // Forwarding is on, and the uplink keeps taking advertisements.
        assert_eq!(setting("net/ipv6/conf/all/forwarding"), "1");
        assert_eq!(setting("net/ipv6/conf/up0/accept_ra"), "2");
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn ipv6_routing_stays_closed_to_all_but_containers() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        // A LAN on another link, with IPv6 of its own.
        let lan = Upstream::with("lan0", [192, 0, 3, 1], [192, 0, 3, 2]);
        let lan_host: Ipv6Addr = "2001:db8:3::1".parse().unwrap();
        let lan_far: Ipv6Addr = "2001:db8:3::2".parse().unwrap();
        let mut host = Rtnetlink::new().unwrap();
        let link = host.link("lan0").unwrap().unwrap();
        host.add_address(link.index, IpNet::new(IpAddr::V6(lan_host), 64).unwrap())
            .unwrap();
        let mut far = Rtnetlink::in_namespace(lan.netns().as_fd()).unwrap();
        let eth0 = far.link("eth0").unwrap().unwrap();
        far.add_address(eth0.index, IpNet::new(IpAddr::V6(lan_far), 64).unwrap())
            .unwrap();
        far.add_route(&Route::default_via(IpAddr::V6(lan_host)))
            .unwrap();
        let _lan_server = Server::start(lan.netns(), 7000, "lan");
        // The world, routed through this host to the LAN.
        upstream.route_back();
        networks.create(&["--ipv6", "app"]);
        let (running, netns) = networks.sleeping(&["--network", "app"]);
        // Shipyard turned IPv6 forwarding on: its container gets out...
        assert_eq!(
            fetch_from(&netns, SocketAddr::from((lan_far, 7000))).as_deref(),
            Some("lan")
        );
        // ...and the world isn't routed anywhere through this host.
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((lan_far, 7000))),
            None
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn ports_are_published_on_both_families() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        networks.create(&["--ipv6", "app"]);
        let (running, netns) = networks.sleeping(&[
            "--network",
            "app",
            "-p",
            "8080:80",
            "-p",
            "[2001:db8::1]:8081:81",
            "-p",
            "[::]:8082:82",
        ]);
        let _eighty = Server::start(&netns, 80, "eighty");
        let _one = Server::start(&netns, 81, "one");
        let _two = Server::start(&netns, 82, "two");
        let from_upstream = |address| fetch_from(upstream.netns(), address);
        assert_eq!(
            from_upstream(SocketAddr::from((Upstream::HOST6, 8080))).as_deref(),
            Some("eighty")
        );
        assert_eq!(
            from_upstream(SocketAddr::from((Upstream::HOST, 8080))).as_deref(),
            Some("eighty")
        );
        assert_eq!(
            from_upstream(SocketAddr::from((Upstream::HOST6, 8081))).as_deref(),
            Some("one")
        );
        // The host's own clients too.
        assert_eq!(
            fetch(SocketAddr::from((Upstream::HOST6, 8080))).as_deref(),
            Some("eighty")
        );
        // `[::]` is every address, as no address is.
        assert_eq!(
            from_upstream(SocketAddr::from((Upstream::HOST6, 8082))).as_deref(),
            Some("two")
        );
        assert_eq!(
            from_upstream(SocketAddr::from((Upstream::HOST, 8082))).as_deref(),
            Some("two")
        );
        // The host's client of [::1]: refused at once (never DNAT'd to a
        // container, which would drop a packet from ::1, leaving it waiting).
        let started = std::time::Instant::now();
        assert_eq!(fetch(SocketAddr::from((Ipv6Addr::LOCALHOST, 8080))), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // On its IPv6 address only.
        assert_eq!(
            from_upstream(SocketAddr::from((Upstream::HOST, 8081))),
            None
        );
        running.stop();

        // ::1 can't be published on; an IPv6 address needs a network with
        // IPv6.
        let loopback = networks.run(&["run", "--network", "app", "-p", "[::1]:8082:80", "true"]);
        assert_eq!(loopback.code, Some(125));
        assert!(
            loopback.stderr.contains("can't reach a container"),
            "{}",
            loopback.stderr
        );
        networks.create(&["plain"]);
        let plain = networks.run(&[
            "run",
            "--network",
            "plain",
            "-p",
            "[2001:db8::1]:8083:80",
            "true",
        ]);
        assert_eq!(plain.code, Some(125));
        assert!(plain.stderr.contains("has no IPv6"), "{}", plain.stderr);
    });
}

#[test]
#[ignore = "needs root"]
fn an_internal_ipv6_network_has_no_way_out() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _server = Server::start(upstream.netns(), 7000, "out");
        networks.create(&["--ipv6", "--internal", "quiet"]);
        let (running, netns) = networks.sleeping(&["--network", "quiet", "--name", "q"]);
        // It has IPv6, and the host reaches it...
        let ip6 = address6(&networks, "quiet", "q");
        let _inside = Server::start(&netns, 7001, "inside");
        assert_eq!(
            fetch(SocketAddr::from((ip6, 7001))).as_deref(),
            Some("inside")
        );
        // ...but it doesn't get out.
        assert_eq!(
            fetch_from(&netns, SocketAddr::from((Upstream::ADDRESS6, 7000))),
            None
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn addresses_and_prefixes_can_be_given() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&[
            "--subnet",
            "10.97.0.0/24",
            "--subnet",
            "fd00:97::/64",
            "--gateway",
            "fd00:97::fe",
            "given",
        ]);
        let config = networks.inspect("given");
        assert_eq!(config["Subnet6"], "fd00:97::/64");
        assert_eq!(config["Gateway6"], "fd00:97::fe");
        let (running, _) =
            networks.sleeping(&["--network", "given", "--name", "c", "--ip6", "fd00:97::50"]);
        assert_eq!(
            address6(&networks, "given", "c"),
            "fd00:97::50".parse::<Ipv6Addr>().unwrap()
        );
        running.stop();
        // The default network is IPv4 only.
        let (on_bridge, _) = networks.sleeping(&["--network", "bridge", "--name", "b"]);
        assert!(networks.lease("bridge", "b")["IPv6Address"].is_null());
        on_bridge.stop();
    });
}

/// A container on an IPv4-only network first and an IPv6 one after: its
/// IPv4 default route goes through the first, its IPv6 one through the
/// second (the one that has IPv6), so it has a way out on both.
#[test]
#[ignore = "needs root"]
fn each_familys_default_route_goes_through_a_network_that_has_it() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["--ipv6", "app"]);
        let (running, netns) = networks.sleeping(&["--network", "bridge", "--network", "app"]);
        let gateways = |family| -> Vec<Option<IpAddr>> {
            Rtnetlink::in_namespace(netns.as_fd())
                .unwrap()
                .routes(family)
                .unwrap()
                .into_iter()
                .filter(|route| route.destination.prefix() == 0)
                .map(|route| route.gateway)
                .collect()
        };
        let gateway6: Ipv6Addr = networks.inspect("app")["Gateway6"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            gateways(shipyard_network::Family::V4),
            [Some(IpAddr::V4(networks.gateway("bridge")))]
        );
        assert_eq!(
            gateways(shipyard_network::Family::V6),
            [Some(IpAddr::V6(gateway6))]
        );
        running.stop();
    });
}

/// A container sends only as itself over IPv6 too (the bridge table): a
/// datagram from another address of the network's prefix it took never
/// reaches its neighbour, while one from its own address does.
#[test]
#[ignore = "needs root"]
fn a_container_sends_only_as_itself_over_ipv6() {
    use std::net::UdpSocket;
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["--ipv6", "app"]);
        let (one, one_netns) = networks.sleeping(&["--network", "app", "--name", "one"]);
        let (two, two_netns) = networks.sleeping(&["--network", "app", "--name", "two"]);
        let (own, theirs) = (
            address6(&networks, "app", "one"),
            address6(&networks, "app", "two"),
        );
        let mut octets = own.octets();
        octets[15] = 0xee;
        let spoofed = Ipv6Addr::from(octets);
        in_netns(&one_netns, || {
            let mut rt = Rtnetlink::new().unwrap();
            let eth0 = rt.link("eth0").unwrap().unwrap();
            rt.add_address(eth0.index, IpNet::new(IpAddr::V6(spoofed), 64).unwrap())
                .unwrap();
        });
        // No duplicate address detection in the container: usable at once.
        let arrives = |from: Ipv6Addr| {
            let receiver = in_netns(&two_netns, || UdpSocket::bind((theirs, 9999))).unwrap();
            receiver
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            in_netns(&one_netns, || {
                let sender = UdpSocket::bind((from, 0)).unwrap();
                for _ in 0..3 {
                    let _ = sender.send_to(b"hello", (theirs, 9999));
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
            receiver.recv_from(&mut [0_u8; 16]).is_ok()
        };
        assert!(arrives(own), "its own address didn't get through");
        assert!(!arrives(spoofed), "another address got through");
        one.stop();
        two.stop();
    });
}
