//! End-to-end tests of networks' DNS servers (Linux, root): containers on a
//! network made with `shipyard network create` find each other by name,
//! alias, short ID and run ID; other names go to the upstream resolvers (but not from
//! an internal network); a name is one running container's; the server
//! runs once per network, as nobody, in a cgroup of its own, and stops when
//! its network is idle or removed; its worker, killed, is started again;
//! and only those networks' containers are told about it.
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
            let taken = networks
                .run(&[&["run", "--no-image", "--network", "app"], flags, &["true"]].concat());
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
        let again = networks.run(&[
            "run",
            "--no-image",
            "--network",
            "app",
            "--name",
            "db",
            "true",
        ]);
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
            let mut command =
                networks.command(&["run", "--no-image", "--network", "app", "sleep", "30"]);
            command.env("SHIPYARD_DNS_UPSTREAM", Upstream::ADDRESS.to_string());
            start_command(command)
        };
        let (first, second) = (start(), start());
        let _ = (container_netns(first.pid()), container_netns(second.pid()));
        let servers = networks.dns_servers();
        assert_eq!(servers.len(), 1, "{servers:?}");
        let server = servers[0];
        // Its watchdog and its worker alike.
        let workers = networks.wait_for_dns_workers(1, Duration::from_secs(5));
        assert_eq!(workers.len(), 1, "{workers:?}");
        for process in [server, workers[0]] {
            assert_eq!(status(process, "Uid:"), "65534\t65534\t65534\t65534");
            assert_eq!(status(process, "Gid:"), "65534\t65534\t65534\t65534");
            assert_eq!(status(process, "Groups:"), "");
            assert_eq!(status(process, "CapPrm:"), "0000000000000000");
            assert_eq!(status(process, "CapEff:"), "0000000000000000");
            assert_eq!(status(process, "NoNewPrivs:"), "1");
        }
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

/// The cgroup `pid` is in (`/proc/PID/cgroup`'s v2 line), as a directory.
fn cgroup_of(pid: libc::pid_t) -> std::path::PathBuf {
    let line = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = line
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap_or_else(|| panic!("no cgroup v2 line for {pid}: {line}"));
    std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))
}

/// A server is in a cgroup of its own (its watchdog and its worker), named
/// in its network's directory, with its limits; it goes with its network.
#[test]
#[ignore = "needs root"]
fn a_server_has_a_cgroup_of_its_own_till_its_network_goes() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        let (running, _) =
            networks.sleeping_with(&[("SHIPYARD_DNS_IDLE", "3600")], &["--network", "app"]);
        let servers = networks.dns_servers();
        let workers = networks.wait_for_dns_workers(1, Duration::from_secs(5));
        assert_eq!((servers.len(), workers.len()), (1, 1));
        let name = std::fs::read_to_string(networks.path().join("app/dns.cgroup")).unwrap();
        assert!(name.starts_with("app-"), "{name}");
        let cgroup = cgroup_of(servers[0]);
        assert!(
            cgroup.ends_with(std::path::Path::new("shipyard/dns").join(&name)),
            "{}",
            cgroup.display()
        );
        assert_eq!(cgroup_of(workers[0]), cgroup);
        let read = |file: &str| std::fs::read_to_string(cgroup.join(file)).unwrap();
        assert_eq!(read("memory.max").trim(), (64u64 << 20).to_string());
        // The server's most threads, its watchdog, and a few to spare.
        let tasks = shipyard_network::dns::server::MOST_TASKS + 5;
        assert_eq!(read("pids.max").trim(), tasks.to_string());

        running.stop();
        let removed = networks.run(&["network", "rm", "app"]);
        assert_eq!(removed.code, Some(0), "{}", removed.stderr);
        assert!(
            networks
                .wait_for_dns_servers(0, Duration::from_secs(10))
                .is_empty()
        );
        assert!(!cgroup.exists(), "{} is left", cgroup.display());
    });
}

/// A server's worker that's killed is started again by its watchdog (the
/// server's lock held meanwhile: the same server), names resolve again, and
/// the log says it; killed again at once, the next start waits twice as
/// long. The watchdog stopped, its worker goes with it: no server is left
/// unwatched.
#[test]
#[ignore = "needs root"]
fn a_killed_servers_worker_is_started_again() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        let (db, netns) = networks.sleeping(&["--network", "app", "--name", "db"]);
        let (db_ip, _) = container(&networks, "app", "db");
        let gateway = networks.gateway("app");
        let server = networks.dns_servers();
        assert_eq!(server.len(), 1);
        for (round, wait) in [(1, "100 ms"), (2, "200 ms")] {
            let workers = networks.wait_for_dns_workers(1, Duration::from_secs(5));
            assert_eq!(workers.len(), 1, "round {round}: {workers:?}");
            // SAFETY: `kill` takes plain values.
            assert_eq!(unsafe { libc::kill(workers[0], libc::SIGKILL) }, 0);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let again = loop {
                let now = networks.dns_workers();
                if now.len() == 1 && now[0] != workers[0] {
                    break now[0];
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "round {round}: no new worker"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(networks.dns_servers(), server, "round {round}");
            assert_eq!(status(again, "Uid:"), "65534\t65534\t65534\t65534");
            let named = resolve(&netns, gateway, "db", rtype::A).unwrap();
            assert_eq!(addresses(&named), [db_ip], "round {round}");
            let log = std::fs::read_to_string(networks.path().join("app/dns.log")).unwrap();
            assert!(
                log.contains(&format!(
                    "its worker was killed (SIGKILL): started again in {wait}"
                )),
                "round {round}: {log}"
            );
        }
        assert_eq!(networks.inspect("app")["DNSRunning"], true);
        // SAFETY: `kill` takes plain values.
        assert_eq!(unsafe { libc::kill(server[0], libc::SIGTERM) }, 0);
        // Gone is its lock let go too: a process's command line empties
        // (the kernel's `exit_mm`) before its files close (`exit_files`),
        // so the processes can look gone while the lock is still held.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !networks.dns_workers().is_empty()
            || !networks.dns_servers().is_empty()
            || networks.inspect("app")["DNSRunning"] != false
        {
            assert!(
                std::time::Instant::now() < deadline,
                "its worker outlived it, or its lock is still held"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        db.stop();
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
            "--no-image",
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

/// What `dig` (BIND's resolver library: another implementation of the
/// protocol) makes of the server's answer to `NAME TYPE`, asked from
/// `netns`: the status, whether it's authoritative, and the answers' data.
fn dig(netns: &std::fs::File, server: Ipv4Addr, args: &[&str]) -> (String, bool, Vec<String>) {
    let output = shipyard_test_support::net::in_netns(netns, || {
        std::process::Command::new("dig")
            .arg(format!("@{server}"))
            .args(["+tries=1", "+time=2", "+noall", "+comments", "+answer"])
            .args(args)
            .output()
    })
    .expect("running dig");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    // What dig says when it can't parse a message.
    for trouble in ["malformed", "bad packet", ";; WARNING", "FORMERR"] {
        assert!(!text.contains(trouble), "dig {args:?}:\n{text}");
    }
    let status = text
        .split("status: ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_else(|| panic!("no status from dig {args:?}:\n{text}"))
        .to_owned();
    let authoritative = text
        .lines()
        .find_map(|line| line.strip_prefix(";; flags: "))
        .is_some_and(|flags| {
            flags
                .split(';')
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .any(|flag| flag == "aa")
        });
    let answers = text
        .lines()
        .filter(|line| !line.starts_with(';') && !line.trim().is_empty())
        .map(|line| {
            line.split_whitespace()
                .last()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    (status, authoritative, answers)
}

/// `dig` reads the server's answers as Shipyard means them (H5, H8): a
/// container's name and alias, any case, authoritative; another type for
/// it, empty but no error; its address's name; over TCP too; another name
/// from upstream, not authoritative; a refusal from an internal network.
#[test]
#[ignore = "needs root"]
fn dig_reads_the_servers_answers_as_meant() {
    if !shipyard_test_support::reference_tool("dig") {
        return;
    }
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["app"]);
        networks.create(&["--internal", "quiet"]);
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
        let (db_ip, _) = container(&networks, "app", "db");
        let db_ip = db_ip.to_string();
        assert_eq!(
            dig(&netns, server, &["db", "A"]),
            ("NOERROR".to_owned(), true, vec![db_ip.clone()])
        );
        for args in [
            &["DATABASE", "A"][..],
            &["+tcp", "database", "A"],
            &["db.", "A"],
        ] {
            assert_eq!(
                dig(&netns, server, args),
                ("NOERROR".to_owned(), true, vec![db_ip.clone()]),
                "{args:?}"
            );
        }
        assert_eq!(
            dig(&netns, server, &["db", "AAAA"]),
            ("NOERROR".to_owned(), true, vec![])
        );
        let reverse = dig(&netns, server, &["-x", &db_ip]);
        assert_eq!(
            reverse,
            ("NOERROR".to_owned(), true, vec!["db.".to_owned()])
        );
        let (status, authoritative, answers) = dig(&netns, server, &["example.com", "A"]);
        assert_eq!(
            (status.as_str(), authoritative, answers),
            ("NOERROR", false, vec![WORLD.to_string()])
        );

        let (lonely, quiet_netns) = networks.sleeping(&["--network", "quiet", "--name", "lonely"]);
        let quiet = networks.gateway("quiet");
        let (status, _, answers) = dig(&quiet_netns, quiet, &["example.com", "A"]);
        assert_eq!((status.as_str(), answers.len()), ("REFUSED", 0));
        db.stop();
        client.stop();
        lonely.stop();
    });
}

/// A running container renamed (stage 9, step 9.4b2): it answers to its
/// short ID too (9.7a); within a second or two its new name is its
/// address's on the network, and its old one is
/// nobody's (the world's to answer). A name another container on the
/// network answers to (a foreground run's) is refused, and nothing changes.
/// Started again, its lease has the new name from the start.
#[test]
#[ignore = "needs root"]
fn a_renamed_container_answers_to_its_new_name() {
    in_new_netns(|| {
        use std::os::unix::fs::PermissionsExt as _;
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _world = FakeResolver::start(upstream.netns(), WORLD);
        networks.create(&["app"]);
        let (_registry, image) = shipyard_test_support::registry::fixture();
        let scratch = shipyard_test_support::registry::Scratch::new();
        let containers = scratch.0.join("containers");
        std::fs::create_dir_all(&containers).unwrap();
        std::fs::set_permissions(&containers, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shipyard = |args: &[&str]| {
            let mut command = networks.command(args);
            command
                .env("SHIPYARD_IMAGE_STORE", scratch.store())
                .env("SHIPYARD_CONTAINER_DIR", &containers)
                .env("SHIPYARD_DNS_UPSTREAM", Upstream::ADDRESS.to_string());
            shipyard_test_support::run(command)
        };
        let made = shipyard(&[
            "run",
            "-d",
            "--name",
            "web",
            "--network",
            "app",
            &image,
            "sleep",
            "300",
        ]);
        assert_eq!(made.code, Some(0), "{}", made.stderr);
        let (db, _) = networks.sleeping(&["--network", "app", "--name", "db"]);
        let (client, netns) = networks.sleeping(&["--network", "app"]);
        let server = networks.gateway("app");
        let web = networks.address("app", "web");
        let answer = |name: &str| resolve(&netns, server, name, rtype::A).unwrap();
        assert_eq!(addresses(&answer("web")), [web]);
        // Its short ID (its hostname, as `ps` shows it) is a name of its
        // too (stage 9, step 9.7a), in any case.
        let short = &made.stdout.trim()[..12];
        assert_eq!(addresses(&answer(short)), [web]);
        assert_eq!(addresses(&answer(&short.to_uppercase())), [web]);
        let refused = shipyard(&["rename", "web", "DB"]);
        assert_eq!(refused.code, Some(1), "{}", refused.stderr);
        assert!(refused.stderr.contains("taken"), "{}", refused.stderr);
        assert_eq!(addresses(&answer("web")), [web], "unchanged");
        let named = |reference: &str| {
            let shown = shipyard(&["inspect", "--shipyard", reference]);
            serde_json::from_str::<serde_json::Value>(&shown.stdout).unwrap()[0]["name"].clone()
        };
        assert_eq!(named("web"), "web", "its record unchanged");
        let renamed = shipyard(&["rename", "web", "site"]);
        assert_eq!(renamed.code, Some(0), "{}", renamed.stderr);
        assert_eq!(renamed.stdout, "");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while addresses(&answer("site")) != [web] {
            assert!(std::time::Instant::now() < deadline, "never site");
            std::thread::sleep(Duration::from_millis(100));
        }
        let old = answer("web");
        assert_eq!(addresses(&old), [WORLD]);
        assert!(!old.header.authoritative());
        assert_eq!(named("site"), "site");
        // Started again: its new lease has the new name.
        for step in [&["stop", "-t", "1", "site"][..], &["start", "site"]] {
            let done = shipyard(step);
            assert_eq!(done.code, Some(0), "{step:?}: {}", done.stderr);
        }
        let again = networks.address("app", "site");
        assert_eq!(addresses(&answer("site")), [again]);
        let removed = shipyard(&["rm", "-f", "site"]);
        assert_eq!(removed.code, Some(0), "{}", removed.stderr);
        client.stop();
        db.stop();
    });
}
