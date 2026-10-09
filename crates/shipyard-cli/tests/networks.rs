//! End-to-end tests of containers on several networks (Linux, root): an
//! interface on each, in order, one default route, no routing between them,
//! names across the networks a container shares with others (and not across
//! those it doesn't, nor the default network's), the long form's settings,
//! published ports on the network the route goes through, and what a killed
//! run leaves on each.
//!
//! As in `dns.rs`: each test in a network namespace of its own, networks in
//! a scratch directory, queries and connections made from the containers'
//! network namespaces by the test itself, and a fake upstream resolver, so
//! a name that isn't the network's gets a known answer (never a timeout that
//! would pass for "not found").

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::fs::File;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsFd as _;

use shipyard_network::Family;
use shipyard_network::dns::message::{Name, RData, rtype};
use shipyard_network::route::Rtnetlink;
use shipyard_test_support::net::{
    FakeResolver, NetworkDir, Server, Upstream, addresses, container_netns_on, fetch_from,
    in_netns, resolve,
};
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Shipyard, in_new_netns, start_command};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// What the fake upstream resolver answers every name with.
const WORLD: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

/// The default routes' gateways in the network namespace `netns`.
fn default_routes(netns: &File) -> Vec<Option<IpAddr>> {
    Rtnetlink::in_namespace(netns.as_fd())
        .unwrap()
        .routes(Family::V4)
        .unwrap()
        .into_iter()
        .filter(|route| route.destination.prefix() == 0)
        .map(|route| route.gateway)
        .collect()
}

/// The `A` records for `name` from the DNS server at `server`, asked from
/// `netns`: they must come back.
fn asked(netns: &File, server: Ipv4Addr, name: &str) -> Vec<Ipv4Addr> {
    let answer = resolve(netns, server, name, rtype::A)
        .unwrap_or_else(|| panic!("no answer for {name} from {server}"));
    addresses(&answer)
}

#[test]
#[ignore = "needs root"]
fn a_container_on_two_networks_reaches_and_names_each_ones_containers() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["front"]);
        networks.create(&["back"]);
        let (web, web_netns) = networks.sleeping(&["--network", "front", "--name", "web"]);
        let (db, db_netns) = networks.sleeping(&["--network", "back", "--name", "db"]);
        let (app, app_netns) =
            networks.sleeping(&["--network", "front", "--network", "back", "--name", "app"]);
        // An interface on each, in order, and the route through the first.
        let app_front = networks.lease("front", "app");
        let app_back = networks.lease("back", "app");
        assert_eq!(app_front["Interface"], "eth0");
        assert_eq!(app_back["Interface"], "eth1");
        assert_eq!(app_front["Run"], app_back["Run"]);
        let front = networks.gateway("front");
        let back = networks.gateway("back");
        assert_eq!(default_routes(&app_netns), [Some(IpAddr::V4(front))]);
        // It routes nothing between them.
        let forwarding = in_netns(&app_netns, || {
            std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward").unwrap()
        });
        assert_eq!(forwarding, "0\n");

        // app finds both, each on its own network, and reaches both.
        let web_ip = networks.address("front", "web");
        let db_ip = networks.address("back", "db");
        assert_eq!(asked(&app_netns, front, "web"), [web_ip]);
        assert_eq!(asked(&app_netns, front, "db"), [db_ip]);
        let [a, b, c, d] = db_ip.octets();
        let ptr = resolve(
            &app_netns,
            front,
            &format!("{d}.{c}.{b}.{a}.in-addr.arpa"),
            rtype::PTR,
        )
        .unwrap();
        assert_eq!(
            ptr.answers.first().map(|answer| &answer.data),
            Some(&RData::Ptr(Name::from_text("db").unwrap()))
        );
        let _server = Server::start(&db_netns, 7000, "db");
        let at_db = SocketAddr::from((db_ip, 7000));
        assert_eq!(fetch_from(&app_netns, at_db).as_deref(), Some("db"));
        // db finds app, by its address on back.
        let app_back_ip: Ipv4Addr = app_back["IPAddress"].as_str().unwrap().parse().unwrap();
        assert_eq!(asked(&db_netns, back, "app"), [app_back_ip]);

        // web shares nothing with db: its name is the world's, and there's
        // no way there.
        assert_eq!(asked(&web_netns, front, "db"), [WORLD]);
        assert_eq!(fetch_from(&web_netns, at_db), None);
        assert_eq!(asked(&db_netns, back, "web"), [WORLD]);
        for running in [app, web, db] {
            running.stop();
        }
    });
}

#[test]
#[ignore = "needs root"]
fn the_long_form_gives_a_network_its_own_settings_and_bridge_has_no_names() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["--subnet", "10.95.0.0/24", "back"]);
        let (on_bridge, _) = networks.sleeping(&["--network", "bridge", "--name", "other"]);
        let (app, app_netns) = networks.sleeping(&[
            "--network",
            "bridge",
            "--network",
            "name=back,ip=10.95.0.50,alias=cache",
            "--name",
            "app",
        ]);
        let on_back = networks.lease("back", "app");
        assert_eq!(on_back["IPAddress"], "10.95.0.50");
        assert_eq!(on_back["Aliases"], serde_json::json!(["cache"]));
        assert_eq!(networks.lease("bridge", "app")["Interface"], "eth0");
        // `cache` is app's name on back.
        let (peer, peer_netns) = networks.sleeping(&["--network", "back"]);
        let back = networks.gateway("back");
        assert_eq!(
            asked(&peer_netns, back, "cache"),
            [Ipv4Addr::new(10, 95, 0, 50)]
        );
        // app is on bridge too, but bridge has no names: back's server
        // doesn't give them (the world answers).
        assert_eq!(asked(&app_netns, back, "other"), [WORLD]);
        for running in [peer, app, on_bridge] {
            running.stop();
        }
    });
}

#[test]
#[ignore = "needs root"]
fn an_internal_first_network_leaves_the_route_ports_and_resolver_to_the_next() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["--internal", "quiet"]);
        networks.create(&["out"]);
        let (app, netns) = networks.sleeping(&[
            "--network",
            "quiet",
            "--network",
            "out",
            "--name",
            "app",
            "-p",
            "8080:80",
        ]);
        let out = networks.gateway("out");
        assert_eq!(default_routes(&netns), [Some(IpAddr::V4(out))]);
        // The ports are on out (eth1), where the route is.
        let on_out = networks.lease("out", "app");
        assert_eq!(on_out["Interface"], "eth1");
        assert_eq!(on_out["Ports"][0]["host_port"], 8080);
        assert_eq!(
            networks.lease("quiet", "app")["Ports"],
            serde_json::json!([])
        );
        app.stop();
        // Its resolver is out's server (it has a way out for other names).
        let root = RootFs::minimal();
        std::fs::create_dir_all(root.host_path("/etc")).unwrap();
        std::fs::write(root.host_path("/etc/resolv.conf"), "theirs\n").unwrap();
        let run = networks.run(&[
            "run",
            "--rootfs",
            root.arg(),
            "--network",
            "quiet",
            "--network",
            "out",
            "cat",
            "/etc/resolv.conf",
        ]);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        assert!(
            run.stdout.contains(&format!("nameserver {out}\n")),
            "{}",
            run.stdout
        );
    });
}

#[test]
#[ignore = "needs root"]
fn an_internal_networks_server_forwards_for_a_container_with_a_way_out() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["--internal", "quiet"]);
        let (app, netns) = networks.sleeping(&["--network", "bridge", "--network", "quiet"]);
        // On bridge too, it can reach the world's names through quiet's
        // server, as a container on quiet alone can't.
        let quiet = networks.gateway("quiet");
        assert_eq!(asked(&netns, quiet, "example.com"), [WORLD]);
        let (alone, alone_netns) = networks.sleeping(&["--network", "quiet"]);
        let refused = resolve(&alone_netns, quiet, "example.com", rtype::A).unwrap();
        assert!(addresses(&refused).is_empty());
        alone.stop();
        app.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_killed_runs_leases_are_swept_from_each_network() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["a"]);
        networks.create(&["b"]);
        let (killed, _) = networks.sleeping(&["--network", "a", "--network", "b"]);
        let leases = |network: &str| {
            std::fs::read_dir(networks.path().join(network).join("leases"))
                .unwrap()
                .count()
        };
        assert_eq!((leases("a"), leases("b")), (1, 1));
        killed.signal(libc::SIGKILL);
        let _ = killed.finish_status();
        // Each network's next run sweeps it from that network.
        for network in ["a", "b"] {
            let next = networks.run(&["run", "--network", network, "true"]);
            assert_eq!(next.code, Some(0), "{}", next.stderr);
            assert_eq!(leases(network), 0, "{network}");
        }
    });
}

/// Two runs joining the same networks in opposite orders, at once: neither
/// waits for the other forever (a run never holds two networks' locks). A
/// lock held across networks would only show when the two interleave, so
/// this can miss it; the design rules it out.
#[test]
#[ignore = "needs root"]
fn runs_joining_networks_in_opposite_orders_both_start() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["a"]);
        networks.create(&["b"]);
        let start = |first: &str, second: &str| {
            start_command(networks.command(&[
                "run",
                "--network",
                first,
                "--network",
                second,
                "sleep",
                "30",
            ]))
        };
        let (one, two) = (start("a", "b"), start("b", "a"));
        let _ = (
            container_netns_on(one.pid(), 2),
            container_netns_on(two.pid(), 2),
        );
        one.stop();
        two.stop();
    });
}

#[test]
fn host_none_and_pasta_join_nothing_else() {
    for mode in ["host", "none", "pasta"] {
        let refused = shipyard_test_support::run(SHIPYARD.command(&[
            "run",
            "--network",
            mode,
            "--network",
            "bridge",
            "true",
        ]));
        assert_eq!(refused.code, Some(125), "{mode}");
        assert!(
            refused.stderr.contains("can't be combined"),
            "{mode}: {}",
            refused.stderr
        );
    }
}
