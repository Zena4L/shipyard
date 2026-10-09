//! End-to-end tests of networks' DNS servers (Linux, root): containers on a
//! network made with `shipyard network create` find each other by name,
//! alias and run ID; other names go to the upstream resolvers (but not from
//! an internal network); a name is one running container's; the server
//! runs once per network, as nobody, and stops when its network is idle or
//! removed; and only those networks' containers are told about it.
//!
//! Each test runs in a network namespace of its own (`in_new_netns`, "the
//! host"), with networks in a scratch directory. Queries are sent from a
//! container's network namespace by the test itself (a thread joined to
//! it), with Shipyard's own DNS messages: no resolver has to be in the
//! container.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::net::Ipv4Addr;
use std::time::Duration;

use shipyard_network::dns::message::{Name, RData, rcode, rtype};
use shipyard_network::route::Rtnetlink;
use shipyard_test_support::net::{
    FakeResolver, NetworkDir, Upstream, addresses, container_netns, resolve, resolve_tcp,
};
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Shipyard, in_new_netns, start_command};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// What the fake upstream resolver answers every name with.
const WORLD: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

/// The container named `container` on the network `name`: its address and
/// its run's ID.
fn container(networks: &NetworkDir, name: &str, container: &str) -> (Ipv4Addr, String) {
    let lease = networks.lease(name, container);
    (
        lease["IPAddress"].as_str().unwrap().parse().unwrap(),
        lease["Run"].as_str().unwrap().to_owned(),
    )
}

/// The lines of `/proc/PID/status` that start with `field`.
fn status(pid: libc::pid_t, field: &str) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .unwrap_or_else(|| panic!("no {field} in {pid}'s status"))
        .trim()
        .to_owned()
}

#[test]
#[ignore = "needs root"]
fn containers_find_each_other_by_name_alias_and_run_id() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["app"]);
        let (db, _) = networks.sleeping(&[
            "--network",
            "app",
            "--name",
            "db",
            "--network-alias",
            "database",
        ]);
        let (client, netns) = networks.sleeping(&["--network", "app"]);
        let server = networks.gateway("app");
        let (db_ip, db_run) = container(&networks, "app", "db");
        for name in ["db", "DB", "database", &db_run] {
            let answer = resolve(&netns, server, name, rtype::A)
                .unwrap_or_else(|| panic!("no answer for {name}"));
            assert_eq!(addresses(&answer), [db_ip], "{name}");
            assert!(answer.header.authoritative(), "{name}");
        }
        // Another type for a container's name: no records, and no error
        // that would send a resolver upstream.
        let aaaa = resolve(&netns, server, "db", rtype::AAAA).unwrap();
        assert_eq!(aaaa.header.rcode(), rcode::NOERROR);
        assert!(aaaa.answers.is_empty());
        // Its address's name.
        let [a, b, c, d] = db_ip.octets();
        let ptr = resolve(
            &netns,
            server,
            &format!("{d}.{c}.{b}.{a}.in-addr.arpa"),
            rtype::PTR,
        )
        .unwrap();
        assert_eq!(
            ptr.answers.first().map(|answer| &answer.data),
            Some(&RData::Ptr(Name::from_text("db").unwrap()))
        );
        // Over TCP too.
        let tcp = resolve_tcp(&netns, server, "database", rtype::A).unwrap();
        assert_eq!(addresses(&tcp), [db_ip]);
        // Once it's gone, its names are nobody's: the world's to answer.
        db.stop();
        let gone = resolve(&netns, server, "db", rtype::A).unwrap();
        assert_eq!(addresses(&gone), [WORLD]);
        assert!(!gone.header.authoritative());
        client.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn other_names_go_upstream_but_not_from_an_internal_network() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["app"]);
        networks.create(&["--internal", "quiet"]);
        let (outgoing, netns) = networks.sleeping(&["--network", "app"]);
        let app = networks.gateway("app");
        let answer = resolve(&netns, app, "example.com", rtype::A).unwrap();
        assert_eq!(addresses(&answer), [WORLD]);
        let answer = resolve_tcp(&netns, app, "example.com", rtype::A);
        // Over TCP, the fake resolver isn't listening: the server says it
        // failed (and doesn't hang).
        assert_eq!(
            answer.map(|answer| answer.header.rcode()),
            Some(rcode::SERVFAIL)
        );

        let (lonely, quiet_netns) = networks.sleeping(&["--network", "quiet", "--name", "lonely"]);
        let quiet = networks.gateway("quiet");
        let refused = resolve(&quiet_netns, quiet, "example.com", rtype::A).unwrap();
        assert_eq!(refused.header.rcode(), rcode::REFUSED);
        assert!(refused.answers.is_empty());
        // Its own names still answer.
        let (lonely_ip, _) = container(&networks, "quiet", "lonely");
        let own = resolve(&quiet_netns, quiet, "lonely", rtype::A).unwrap();
        assert_eq!(addresses(&own), [lonely_ip]);
        // A server answers its own network alone: from another, nothing
        // listens there (its sockets are bound to its bridge).
        assert!(resolve(&netns, quiet, "lonely", rtype::A).is_none());
        outgoing.stop();
        lonely.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_name_is_one_running_containers_at_a_time() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        let (db, _) = networks.sleeping(&["--network", "app", "--name", "db"]);
        for flags in [
            &["--name", "db"][..],
            &["--name", "web", "--network-alias", "DB"][..],
        ] {
            let taken = networks.run(&[&["run", "--network", "app"], flags, &["true"]].concat());
            assert_eq!(taken.code, Some(125), "{flags:?}");
            assert!(
                taken
                    .stderr
                    .to_lowercase()
                    .contains("the name db is taken on the network app"),
                "{}",
                taken.stderr
            );
        }
        db.stop();
        let again = networks.run(&["run", "--network", "app", "--name", "db", "true"]);
        assert_eq!(again.code, Some(0), "{}", again.stderr);
    });
}

/// A container killed outright leaves its lease until the next run sweeps
/// it, but its lock goes at once: within a second or so (the server reads
/// the leases again at least that often, however often it's asked), its
/// names are nobody's, before any sweep.
#[test]
#[ignore = "needs root"]
fn a_killed_containers_name_goes_within_a_second() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["app"]);
        let (db, _) = networks.sleeping(&["--network", "app", "--name", "db"]);
        let (client, netns) = networks.sleeping(&["--network", "app"]);
        let server = networks.gateway("app");
        let (db_ip, _) = container(&networks, "app", "db");
        // Asked once: the server has read the leases.
        let named = resolve(&netns, server, "db", rtype::A).unwrap();
        assert_eq!(addresses(&named), [db_ip]);
        db.signal(libc::SIGKILL);
        let _ = db.finish_status();
        std::thread::sleep(Duration::from_millis(1500));
        let gone = resolve(&netns, server, "db", rtype::A).unwrap();
        assert_eq!(
            addresses(&gone),
            [WORLD],
            "still answered for a dead container"
        );
        client.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn one_server_runs_as_nobody_until_its_network_is_idle() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        // Two runs at once: one server.
        let start = || {
            let mut command = networks.command(&["run", "--network", "app", "sleep", "30"]);
            command.env("SHIPYARD_DNS_UPSTREAM", Upstream::ADDRESS.to_string());
            start_command(command)
        };
        let (first, second) = (start(), start());
        let _ = (container_netns(first.pid()), container_netns(second.pid()));
        let servers = networks.dns_servers();
        assert_eq!(servers.len(), 1, "{servers:?}");
        let server = servers[0];
        assert_eq!(status(server, "Uid:"), "65534\t65534\t65534\t65534");
        assert_eq!(status(server, "Gid:"), "65534\t65534\t65534\t65534");
        assert_eq!(status(server, "Groups:"), "");
        assert_eq!(status(server, "CapPrm:"), "0000000000000000");
        assert_eq!(status(server, "CapEff:"), "0000000000000000");
        assert_eq!(status(server, "NoNewPrivs:"), "1");
        assert_eq!(networks.inspect("app")["DNSRunning"], true);

        // With a container left, it stays (well past its idle time).
        first.stop();
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(networks.dns_servers(), [server]);
        // With none, it stops.
        second.stop();
        assert!(
            networks
                .wait_for_dns_servers(0, Duration::from_secs(10))
                .is_empty()
        );
        assert_eq!(networks.inspect("app")["DNSRunning"], false);

        // The next run starts another.
        let (again, netns) = networks.sleeping(&["--network", "app", "--name", "again"]);
        let (ip, _) = container(&networks, "app", "again");
        let answer = resolve(&netns, networks.gateway("app"), "again", rtype::A).unwrap();
        assert_eq!(addresses(&answer), [ip]);
        again.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_removed_networks_server_stops() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        let (running, _) =
            networks.sleeping_with(&[("SHIPYARD_DNS_IDLE", "3600")], &["--network", "app"]);
        running.stop();
        // Idle, but not for long enough to stop.
        assert_eq!(networks.dns_servers().len(), 1);
        let removed = networks.run(&["network", "rm", "app"]);
        assert_eq!(removed.code, Some(0), "{}", removed.stderr);
        assert!(
            networks
                .wait_for_dns_servers(0, Duration::from_secs(10))
                .is_empty()
        );
    });
}

#[test]
#[ignore = "needs root"]
fn a_bridge_made_again_gets_a_server_of_its_own() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        // A server that stays (its network idle for long enough to delete
        // the bridge under it).
        let idle = [("SHIPYARD_DNS_IDLE", "3600")];
        let (first, _) = networks.sleeping_with(&idle, &["--network", "app"]);
        first.stop();
        let old = networks.dns_servers();
        assert_eq!(old.len(), 1);
        let bridge = networks.inspect("app")["Bridge"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut host = Rtnetlink::new().unwrap();
        let index = host.link(&bridge).unwrap().unwrap().index;
        host.delete(index).unwrap();
        // The next run makes the bridge again at once, before the old
        // server has noticed: it waits for it to go, and starts another.
        let (again, netns) =
            networks.sleeping_with(&idle, &["--network", "app", "--name", "again"]);
        let (ip, _) = container(&networks, "app", "again");
        let answer = resolve(&netns, networks.gateway("app"), "again", rtype::A).unwrap();
        assert_eq!(addresses(&answer), [ip]);
        let new = networks.dns_servers();
        assert_eq!(new.len(), 1);
        assert_ne!(new, old);
        again.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn only_a_created_networks_containers_are_told_about_its_server() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        let root = RootFs::minimal();
        std::fs::create_dir_all(root.host_path("/etc")).unwrap();
        std::fs::write(root.host_path("/etc/resolv.conf"), "theirs\n").unwrap();
        let resolv_conf = |flags: &[&str]| {
            let run = networks.run(
                &[
                    &["run", "--rootfs", root.arg()],
                    flags,
                    &["cat", "/etc/resolv.conf"],
                ]
                .concat(),
            );
            assert_eq!(run.code, Some(0), "{}", run.stderr);
            run.stdout
        };
        let app = networks.gateway("app");
        let on_app = resolv_conf(&["--network", "app"]);
        assert!(on_app.contains(&format!("nameserver {app}\n")), "{on_app}");
        // --dns names others instead.
        let named = resolv_conf(&["--network", "app", "--dns", "10.0.0.53"]);
        assert!(named.contains("nameserver 10.0.0.53\n"), "{named}");
        assert!(!named.contains(&app.to_string()), "{named}");
        // The default network has no names, and no server.
        let on_bridge = resolv_conf(&["--network", "bridge", "--name", "box"]);
        let bridge = networks.gateway("bridge");
        assert!(!on_bridge.contains(&bridge.to_string()), "{on_bridge}");
        assert_eq!(networks.inspect("bridge")["DNSRunning"], false);
        let alias = networks.run(&[
            "run",
            "--network",
            "bridge",
            "--network-alias",
            "box",
            "true",
        ]);
        assert_eq!(alias.code, Some(125));
        assert!(
            alias.stderr.contains("alias= need a network made with"),
            "{}",
            alias.stderr
        );
    });
}
