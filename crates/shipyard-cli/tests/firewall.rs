//! End-to-end tests of networks' firewall (Linux, root): the way out
//! (masquerade), published ports (`-p`), internal networks, networks kept
//! apart, and what a killed run leaves swept.
//!
//! Each test runs in a network namespace of its own (`in_new_netns`, "the
//! host"), with an [`Upstream`] standing in for the world, and networks in a
//! scratch directory: never the host's firewall or the CI runner's. A server
//! "in a container" is a listener on a thread joined to its network
//! namespace.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use shipyard_network::nft::Nftables;
use shipyard_network::route::{Route, Rtnetlink};
use shipyard_test_support::net::{NetworkDir, Server, Upstream, fetch, fetch_from, in_netns};
use shipyard_test_support::{Rootless, Shipyard, in_new_netns, is_root, rootless};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// The address of the container on `network` started `nth` (0 first),
/// from `network inspect`.
fn container_ip(networks: &NetworkDir, network: &str, nth: usize) -> Ipv4Addr {
    networks.inspect(network)["Containers"][nth]["IPAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
#[ignore = "needs root"]
fn root_runs_on_the_bridge_network_with_a_way_out() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let server = Server::start(upstream.netns(), 7000, "out");
        // No --network: the default bridge network.
        let (running, netns) = networks.sleeping(&[]);
        assert_eq!(
            fetch_from(&netns, SocketAddr::from((Upstream::ADDRESS, 7000))).as_deref(),
            Some("out")
        );
        // The world sees the host's address, not the container's.
        assert_eq!(
            server.peer().map(|peer| peer.ip()),
            Some(IpAddr::V4(Upstream::HOST))
        );
        assert_eq!(
            std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward").unwrap(),
            "1\n"
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn published_ports_reach_the_container_from_everywhere() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let (running, netns) =
            networks.sleeping(&["-p", "8080:80", "-p", "127.0.0.1:8081:81", "-p", "82"]);
        let _eighty = Server::start(&netns, 80, "eighty");
        let _local = Server::start(&netns, 81, "local");
        let _random = Server::start(&netns, 82, "random");
        // The host's own clients reach ports through 127.0.0.1: the bridge
        // routes from loopback (only a network that publishes does).
        assert_eq!(setting("ipv4/conf/shipyard0/route_localnet"), "1");
        // From the world, from this host (its own address, and loopback).
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((Upstream::HOST, 8080))).as_deref(),
            Some("eighty")
        );
        assert_eq!(
            fetch(SocketAddr::from((Upstream::HOST, 8080))).as_deref(),
            Some("eighty")
        );
        assert_eq!(
            fetch(SocketAddr::from((Ipv4Addr::LOCALHOST, 8080))).as_deref(),
            Some("eighty")
        );
        // Published on 127.0.0.1 only: this host reaches it, the world
        // doesn't.
        assert_eq!(
            fetch(SocketAddr::from((Ipv4Addr::LOCALHOST, 8081))).as_deref(),
            Some("local")
        );
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((Upstream::HOST, 8081))),
            None
        );
        // A free host port, shown by `network inspect`.
        let inspect = networks.run(&["network", "inspect", "bridge"]);
        let json: serde_json::Value = serde_json::from_str(&inspect.stdout).unwrap();
        let port = json[0]["Containers"][0]["Ports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|port| port["container_port"] == 82)
            .and_then(|port| port["host_port"].as_u64())
            .and_then(|port| u16::try_from(port).ok())
            .unwrap_or_else(|| panic!("{}", inspect.stdout));
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((Upstream::HOST, port))).as_deref(),
            Some("random")
        );
        // Hairpin: a container reaching a published port through the host,
        // another one and itself.
        let (other, other_netns) = networks.sleeping(&[]);
        assert_eq!(
            fetch_from(&other_netns, SocketAddr::from((Upstream::HOST, 8080))).as_deref(),
            Some("eighty")
        );
        assert_eq!(
            fetch_from(&netns, SocketAddr::from((Upstream::HOST, 8080))).as_deref(),
            Some("eighty")
        );
        other.stop();
        running.stop();
        // A port the kernel chose is printed.
        let chosen = networks.run(&["run", "-p", "83", "true"]);
        assert_eq!(chosen.code, Some(0), "{}", chosen.stderr);
        assert!(
            chosen.stderr.lines().any(|line| {
                line.strip_prefix("shipyard: published 0.0.0.0:")
                    .and_then(|rest| rest.strip_suffix(" -> 83/tcp"))
                    .is_some_and(|port| port.parse::<u16>().is_ok())
            }),
            "{}",
            chosen.stderr
        );
    });
}

#[test]
#[ignore = "needs root"]
fn a_published_port_is_held_while_its_run_lives() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let (running, _netns) = networks.sleeping(&["-p", "8090:80"]);
        let again = networks.run(&["run", "-p", "8090:80", "true"]);
        assert_eq!(again.code, Some(125), "{}", again.stderr);
        assert!(
            again
                .stderr
                .contains("port 8090/tcp on 0.0.0.0 is already in use"),
            "{}",
            again.stderr
        );
        // No other program can take it either.
        assert!(std::net::TcpListener::bind("0.0.0.0:8090").is_err());
        running.stop();
        std::net::TcpListener::bind("0.0.0.0:8090").unwrap();
    });
}

#[test]
#[ignore = "needs root"]
fn an_internal_network_has_no_way_out_and_publishes_nothing() {
    in_new_netns(|| {
        // The host doesn't route yet (a new namespace copies the host's
        // setting: set it).
        std::fs::write("/proc/sys/net/ipv4/ip_forward", "0").unwrap();
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let _server = Server::start(upstream.netns(), 7000, "out");
        let created = networks.run(&["network", "create", "--internal", "secret"]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        let bridge = networks.inspect("secret")["Bridge"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = networks.run(&["run", "--network", "secret", "true"]);
        assert_eq!(first.code, Some(0), "{}", first.stderr);
        // A container with a way out gets out (so the world is there)...
        let (open, open_netns) = networks.sleeping(&[]);
        assert_eq!(
            fetch_from(&open_netns, SocketAddr::from((Upstream::ADDRESS, 7000))).as_deref(),
            Some("out")
        );
        open.stop();
        // ...which turned the host's forwarding on, for every interface
        // but the internal network's bridge.
        assert_eq!(setting("ipv4/ip_forward"), "1");
        assert_eq!(setting(&format!("ipv4/conf/{bridge}/forwarding")), "0");
        // ...and one on the internal network doesn't.
        let (running, netns) = networks.sleeping(&["--network", "secret"]);
        assert_eq!(
            fetch_from(&netns, SocketAddr::from((Upstream::ADDRESS, 7000))),
            None
        );
        // Its bridge doesn't forward, even without the firewall's rules, and
        // nothing on it publishes: no routing from loopback either.
        assert_eq!(setting(&format!("ipv4/conf/{bridge}/forwarding")), "0");
        assert_eq!(setting(&format!("ipv4/conf/{bridge}/route_localnet")), "0");
        // One made now, on a host that forwards (a new interface forwards
        // too, by default), doesn't either.
        networks.create(&["--internal", "later"]);
        let later = networks.inspect("later")["Bridge"]
            .as_str()
            .unwrap()
            .to_owned();
        let on_later = networks.run(&["run", "--network", "later", "true"]);
        assert_eq!(on_later.code, Some(0), "{}", on_later.stderr);
        assert_eq!(setting(&format!("ipv4/conf/{later}/forwarding")), "0");
        // The host still reaches it.
        let _inside = Server::start(&netns, 80, "inside");
        let ip = container_ip(&networks, "secret", 0);
        assert_eq!(fetch(SocketAddr::from((ip, 80))).as_deref(), Some("inside"));
        let published = networks.run(&["run", "--network", "secret", "-p", "80", "true"]);
        assert_eq!(published.code, Some(125), "{}", published.stderr);
        assert!(
            published
                .stderr
                .contains("the network secret is internal: nothing on it can be reached"),
            "{}",
            published.stderr
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn only_published_ports_reach_a_container_from_elsewhere() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let created = networks.run(&["network", "create", "--subnet", "10.90.0.0/24", "other"]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        let (running, netns) = networks.sleeping(&["-p", "8070:80"]);
        let _inside = Server::start(&netns, 80, "inside");
        let ip = container_ip(&networks, "bridge", 0);
        // The host reaches it...
        assert_eq!(fetch(SocketAddr::from((ip, 80))).as_deref(), Some("inside"));
        // ...but not a container on another network,
        let (other, other_netns) = networks.sleeping(&["--network", "other"]);
        assert_eq!(fetch_from(&other_netns, SocketAddr::from((ip, 80))), None);
        // nor the world, even routed to it through the host.
        upstream.route_back();
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((ip, 80))),
            None
        );
        // Only through its published port.
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((Upstream::HOST, 8070))).as_deref(),
            Some("inside")
        );
        other.stop();
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_killed_runs_ports_and_connections_are_swept_by_the_next() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let (killed, _netns) = networks.sleeping(&["-p", "5353:53/udp", "-p", "8095:80"]);
        let ip = container_ip(&networks, "bridge", 0);
        // A UDP flow to its published port: the kernel remembers it.
        in_netns(upstream.netns(), || {
            let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            socket
                .send_to(b"x", SocketAddr::from((Upstream::HOST, 5353)))
                .unwrap();
        });
        let involving = || {
            shipyard_network::conntrack::connections(shipyard_network::Family::V4)
                .unwrap()
                .iter()
                .filter(|connection| connection.involves(IpAddr::V4(ip)))
                .count()
        };
        assert!(involving() >= 1);
        let mut nft = Nftables::new().unwrap();
        assert_eq!(nft.elements("ports").unwrap().len(), 2);
        killed.signal(libc::SIGKILL);
        let _ = killed.finish_status();
        // Left as they were, until the next run on the network sweeps them.
        assert_eq!(nft.elements("ports").unwrap().len(), 2);
        let next = networks.run(&["run", "true"]);
        assert_eq!(next.code, Some(0), "{}", next.stderr);
        assert!(nft.elements("ports").unwrap().is_empty());
        assert_eq!(involving(), 0);
        // And the ports are free again.
        let again = networks.run(&["run", "-p", "8095:80", "true"]);
        assert_eq!(again.code, Some(0), "{}", again.stderr);
    });
}

#[test]
#[ignore = "needs root"]
fn a_host_that_didnt_route_routes_only_for_its_containers() {
    in_new_netns(|| {
        // The test's namespace doesn't route (a new one copies the host's
        // setting: set it), with a second link: the world on one side
        // (Upstream), a LAN on the other.
        std::fs::write("/proc/sys/net/ipv4/ip_forward", "0").unwrap();
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let lan = Upstream::with("lan0", [192, 0, 3, 1], [192, 0, 3, 2]);
        let _lan_server = Server::start(lan.netns(), 7000, "lan");
        // The world, routed through this host to the LAN, and the LAN's
        // replies back (so only the host's routing can stop it).
        upstream.route_back();
        in_netns(lan.netns(), || {
            Rtnetlink::new()
                .unwrap()
                .add_route(&Route::default_via(IpAddr::V4(Ipv4Addr::new(192, 0, 3, 1))))
                .unwrap();
        });
        let (running, netns) = networks.sleeping(&[]);
        // Shipyard turned forwarding on: its container gets out...
        assert_eq!(
            fetch_from(
                &netns,
                SocketAddr::from((Ipv4Addr::new(192, 0, 3, 2), 7000))
            )
            .as_deref(),
            Some("lan")
        );
        // ...and the world isn't routed anywhere through this host.
        assert_eq!(
            fetch_from(
                upstream.netns(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 3, 2), 7000))
            ),
            None
        );
        // A link made after (another program's network: Docker's, a VPN's,
        // started later) is routed: it isn't one of the host's own
        // interfaces Shipyard closed routing between.
        let _out = Server::start(upstream.netns(), 7001, "out");
        let later = Upstream::with("later0", [192, 0, 4, 1], [192, 0, 4, 2]);
        in_netns(later.netns(), || {
            Rtnetlink::new()
                .unwrap()
                .add_route(&Route::default_via(IpAddr::V4(Ipv4Addr::new(192, 0, 4, 1))))
                .unwrap();
        });
        assert_eq!(
            fetch_from(later.netns(), SocketAddr::from((Upstream::ADDRESS, 7001))).as_deref(),
            Some("out")
        );
        // Both ways: the world reaches it too (as Docker's published ports).
        let _later_server = Server::start(later.netns(), 7002, "later");
        assert_eq!(
            fetch_from(
                upstream.netns(),
                SocketAddr::from((Ipv4Addr::new(192, 0, 4, 2), 7002))
            )
            .as_deref(),
            Some("later")
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_port_a_killed_run_left_on_another_network_is_reclaimed() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let upstream = Upstream::new();
        let created = networks.run(&["network", "create", "other"]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        let (killed, _netns) = networks.sleeping(&["-p", "8099:80"]);
        killed.signal(libc::SIGKILL);
        let _ = killed.finish_status();
        // Nobody ran on its network since: its port is still mapped, to an
        // address nothing holds. A run on another network takes it back.
        let (running, netns) = networks.sleeping(&["--network", "other", "-p", "8099:80"]);
        let _inside = Server::start(&netns, 80, "reclaimed");
        assert_eq!(
            fetch_from(upstream.netns(), SocketAddr::from((Upstream::HOST, 8099))).as_deref(),
            Some("reclaimed")
        );
        running.stop();
    });
}

#[test]
fn publishing_needs_a_network_of_the_containers_own() {
    if !is_root() && !rootless(Rootless::Single) {
        return;
    }
    let run = |flags: &[&str]| {
        shipyard_test_support::run(SHIPYARD.command(&[&["run"], flags, &["true"]].concat()))
    };
    let none = run(&["--network", "none", "-p", "80"]);
    assert_eq!(none.code, Some(125), "{}", none.stderr);
    assert!(
        none.stderr.contains("need a network the host reaches"),
        "{}",
        none.stderr
    );
    let host = run(&["--network", "host", "-p", "80"]);
    assert!(
        host.stderr
            .contains("the container's ports are the host's already"),
        "{}",
        host.stderr
    );
    let v6 = run(&["-p", "[::1]:8080:80"]);
    assert_eq!(v6.code, Some(125), "{}", v6.stderr);
    assert!(
        v6.stderr.contains("can't reach a container"),
        "{}",
        v6.stderr
    );
}

/// The kernel setting at `path` (under `/proc/sys`), as this thread's
/// network namespace has it.
fn setting(path: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/net/{path}"))
        .unwrap()
        .trim()
        .to_owned()
}

/// `nft ARGS` (the real one, made by nftables), in this thread's network
/// namespace: its stdout, or `None` where nft isn't installed (the test
/// skips; the test image and CI have it).
fn nft(args: &[&str]) -> Option<String> {
    let output = match std::process::Command::new("nft").args(args).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("nft isn't installed; skipping");
            return None;
        }
        Err(error) => panic!("running nft: {error}"),
    };
    assert!(
        output.status.success(),
        "nft {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).unwrap())
}

/// Another program's firewall that drops what the host forwards (Docker's
/// or ufw's `FORWARD`), and what comes in to it: a run is warned about
/// each, with the commands that let its containers through. Shipyard never
/// changes another program's rules.
#[test]
#[ignore = "needs root"]
fn another_firewalls_drop_policies_are_warned_about_with_their_fixes() {
    in_new_netns(|| {
        for command in [
            "add table ip filter",
            "add chain ip filter FORWARD { type filter hook forward priority 0; policy drop; }",
            "add chain ip filter INPUT { type filter hook input priority 0; policy drop; }",
        ] {
            if nft(&[command]).is_none() {
                return;
            }
        }
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["app"]);
        let bridge = networks.inspect("app")["Bridge"]
            .as_str()
            .unwrap()
            .to_owned();
        let run = networks.run(&["run", "--network", "app", "true"]);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        assert!(
            run.stderr.contains(&format!(
                "the firewall's chain ip filter FORWARD drops forwarded traffic by default, so \
                 containers on {bridge} may have no way out; to let them through: nft insert \
                 rule ip filter FORWARD iifname {bridge} accept"
            )),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr.contains(&format!(
                "the firewall's chain ip filter INPUT drops what comes in to the host by \
                 default, so containers on {bridge} may get no answer from the network's DNS \
                 server"
            )),
            "{}",
            run.stderr
        );
        assert!(
            run.stderr.contains(&format!(
                "iptables -I INPUT -i {bridge} -p udp --dport 53 -j ACCEPT"
            )),
            "{}",
            run.stderr
        );
        // Theirs, untouched.
        let theirs = nft(&["list", "table", "ip", "filter"]).unwrap();
        assert!(!theirs.contains(&bridge), "{theirs}");
        assert_eq!(theirs.matches("policy drop").count(), 2, "{theirs}");
    });
}

/// A UDP datagram from `from` (an address of the namespace `netns`) to
/// `to`, and whether a listener on `to` in `listener` got it.
fn datagram_arrives(
    netns: &std::fs::File,
    from: Ipv4Addr,
    listener: &std::fs::File,
    to: Ipv4Addr,
) -> bool {
    use std::net::UdpSocket;
    let receiver = in_netns(listener, || UdpSocket::bind((to, 9999))).unwrap();
    receiver
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    in_netns(netns, || {
        let sender = UdpSocket::bind((from, 0)).unwrap();
        // A few: the first may wait for the neighbour's hardware address.
        for _ in 0..3 {
            let _ = sender.send_to(b"hello", (to, 9999));
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    let mut buffer = [0_u8; 16];
    receiver.recv_from(&mut buffer).is_ok()
}

/// The hardware address Shipyard gives the container at `ip` (`02:42` and
/// its address).
fn mac_of(ip: Ipv4Addr) -> [u8; 6] {
    let [a, b, c, d] = ip.octets();
    [0x02, 0x42, a, b, c, d]
}

/// Whether a UDP datagram from `from` reaches `to` (a listener in
/// `listener`) when written raw onto `eth0` in `netns`, as root in a
/// container with `CAP_NET_RAW` can: an Ethernet frame from `src_mac` to
/// `to`'s hardware address, inside `vlan_tags` 802.1Q tags (VLAN 0).
fn raw_datagram_arrives(
    netns: &std::fs::File,
    src_mac: [u8; 6],
    from: Ipv4Addr,
    listener: &std::fs::File,
    to: Ipv4Addr,
    vlan_tags: usize,
) -> bool {
    use std::net::UdpSocket;
    // A port the kernel picks: a fixed one may be taken in a busy run.
    let receiver = in_netns(listener, || UdpSocket::bind((to, 0))).unwrap();
    let port = receiver.local_addr().unwrap().port();
    receiver
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let payload = b"hello";
    let udp_len = u16::try_from(8 + payload.len()).unwrap();
    let total = 20 + udp_len;
    let mut ip = vec![0x45, 0];
    ip.extend(total.to_be_bytes());
    ip.extend([0, 0, 0x40, 0, 64, 17, 0, 0]);
    ip.extend(from.octets());
    ip.extend(to.octets());
    let sum: u32 = ip
        .chunks(2)
        .map(|word| u32::from(u16::from_be_bytes([word[0], word[1]])))
        .sum();
    let folded = (sum & 0xffff) + (sum >> 16);
    let checksum = !u16::try_from((folded & 0xffff) + (folded >> 16)).unwrap();
    ip[10..12].copy_from_slice(&checksum.to_be_bytes());
    let mut frame = [&mac_of(to)[..], &src_mac[..]].concat();
    for _ in 0..vlan_tags {
        frame.extend([0x81, 0x00, 0x00, 0x00]);
    }
    frame.extend([0x08, 0x00]);
    frame.extend(ip);
    frame.extend(1234_u16.to_be_bytes());
    frame.extend(port.to_be_bytes());
    frame.extend(udp_len.to_be_bytes());
    frame.extend([0, 0]);
    frame.extend(payload);
    in_netns(netns, || {
        let index = Rtnetlink::new()
            .unwrap()
            .link("eth0")
            .unwrap()
            .unwrap()
            .index;
        // SAFETY: `socket` takes no pointers; the descriptor is owned at once.
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                i32::from(u16::try_from(libc::ETH_P_ALL).unwrap().to_be()),
            )
        };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        // SAFETY: `fd` is a new descriptor of ours.
        let socket = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        // SAFETY: an all-zero `sockaddr_ll` is valid; its fields are set below.
        let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        address.sll_family = u16::try_from(libc::AF_PACKET).unwrap();
        address.sll_ifindex = i32::try_from(index).unwrap();
        address.sll_halen = 6;
        address.sll_addr[..6].copy_from_slice(&mac_of(to));
        for _ in 0..3 {
            // SAFETY: `frame` and `address` are live and their lengths are
            // given; `socket` is open.
            let sent = unsafe {
                libc::sendto(
                    std::os::fd::AsRawFd::as_raw_fd(&socket),
                    frame.as_ptr().cast(),
                    frame.len(),
                    0,
                    (&raw const address).cast(),
                    u32::try_from(std::mem::size_of::<libc::sockaddr_ll>()).unwrap(),
                )
            };
            assert!(sent >= 0, "{}", std::io::Error::last_os_error());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    let mut buffer = [0_u8; 16];
    receiver.recv_from(&mut buffer).is_ok()
}

/// What only a protocol the bridge table knows may carry: a frame inside
/// VLAN tags (stacked, as the bridge takes only the outer one off) from a
/// container, with another's address, doesn't reach its neighbour, which
/// would take every tag off and believe it. Raw frames from its own
/// address do (the control: the frames are well made).
#[test]
#[ignore = "needs root"]
fn tagged_frames_dont_get_past_the_guard() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["--subnet", "10.92.0.0/24", "app"]);
        let (first, first_netns) = networks.sleeping(&["--network", "app"]);
        let (second, second_netns) = networks.sleeping(&["--network", "app"]);
        let (first_ip, second_ip) = (
            container_ip(&networks, "app", 0),
            container_ip(&networks, "app", 1),
        );
        let spoofed = Ipv4Addr::new(10, 92, 0, 200);
        let mac = mac_of(first_ip);
        let arrives = |from, tags| {
            raw_datagram_arrives(&first_netns, mac, from, &second_netns, second_ip, tags)
        };
        assert!(
            arrives(first_ip, 0),
            "a raw frame of its own didn't get through"
        );
        assert!(!arrives(spoofed, 0), "another address got through");
        assert!(
            !arrives(spoofed, 2),
            "another address got through, in VLAN tags"
        );
        first.stop();
        second.stop();
    });
}

/// A container sends only as itself on its network (the bridge table): a
/// datagram from another address it took (as root inside with
/// `CAP_NET_ADMIN`, or raw, with `CAP_NET_RAW`) never reaches its
/// neighbour, while one from its own address does. Both before and after
/// the table is flushed and rebuilt by the next run, from the leases.
#[test]
#[ignore = "needs root"]
fn a_container_sends_only_as_itself() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        networks.create(&["--subnet", "10.91.0.0/24", "app"]);
        let (first, first_netns) = networks.sleeping(&["--network", "app"]);
        let (second, second_netns) = networks.sleeping(&["--network", "app"]);
        let (first_ip, second_ip) = (
            container_ip(&networks, "app", 0),
            container_ip(&networks, "app", 1),
        );
        // The first container takes an address that isn't its own, as root
        // in it could.
        let spoofed = Ipv4Addr::new(10, 91, 0, 200);
        in_netns(&first_netns, || {
            let mut rt = Rtnetlink::new().unwrap();
            let eth0 = rt.link("eth0").unwrap().unwrap();
            rt.add_address(eth0.index, "10.91.0.200/24".parse().unwrap())
                .unwrap();
        });
        let check = || {
            assert!(
                datagram_arrives(&first_netns, first_ip, &second_netns, second_ip),
                "its own address didn't get through"
            );
            assert!(
                !datagram_arrives(&first_netns, spoofed, &second_netns, second_ip),
                "another address got through"
            );
        };
        check();
        // Flushed (an administrator's `nft flush ruleset`): the next run
        // rebuilds it from the live leases.
        let flushed = std::process::Command::new("nft")
            .args(["delete", "table", "bridge", "shipyard"])
            .status();
        if flushed.is_ok_and(|status| status.success()) {
            let run = networks.run(&["run", "--network", "app", "true"]);
            assert_eq!(run.code, Some(0), "{}", run.stderr);
            check();
        }
        // A container's port leaves the table with it: nothing builds up.
        let guarded_ports = || {
            nft(&["list", "set", "bridge", "shipyard", "ports"])
                .map(|set| set.matches("\"shy").count())
        };
        // (Where nft isn't installed, only the stops.)
        let looked = guarded_ports();
        if let Some(both) = looked {
            assert_eq!(both, 2);
        }
        first.stop();
        if looked.is_some() {
            assert_eq!(guarded_ports(), Some(1));
        }
        second.stop();
        if looked.is_some() {
            assert_eq!(guarded_ports(), Some(0));
        }
    });
}

/// The administrator's own `nft list ruleset` reads Shipyard's table, every
/// set, map and rule of it (an encoding nft couldn't decode would break
/// their tools), with a network of each kind and ports on both families.
#[test]
#[ignore = "needs root"]
fn nft_reads_shipyards_whole_table() {
    in_new_netns(|| {
        if nft(&["--version"]).is_none() {
            return;
        }
        let networks = NetworkDir::new(SHIPYARD);
        // The host's loopback up, for a port on 127.0.0.1.
        let _upstream = Upstream::new();
        networks.create(&["--ipv6", "dual"]);
        networks.create(&["--internal", "closed"]);
        let (running, _netns) = networks.sleeping(&[
            "--network",
            "dual",
            "-p",
            "8080:80",
            "-p",
            "127.0.0.1:5353:53/udp",
        ]);
        let table = nft(&["list", "table", "inet", "shipyard"]).unwrap();
        for part in [
            "chain guard",
            "chain dstnat",
            "chain output",
            "chain forward",
            "chain srcnat",
            "map ports",
            "map ports6",
            "masquerade",
            "dnat ip to",
            "dnat ip6 to",
            ": 10.",
        ] {
            assert!(table.contains(part), "no {part:?} in:\n{table}");
        }
        // A set of interface names lists its names, not `""` (nft reads
        // their byte order from the set; a pair of them is typed field by
        // field, and lists them either way).
        let bridges = table
            .split("set bridges {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or_default();
        assert!(bridges.contains("\"shy-"), "{bridges}");
        // And nothing it couldn't decode (nft prints `unknown` or `[invalid
        // type]` for what it can't read; `ct state invalid` is a rule's).
        assert!(
            !table.contains("unknown") && !table.contains("[invalid"),
            "{table}"
        );
        assert!(table.contains("ct state invalid drop"), "{table}");
        assert!(nft(&["list", "ruleset"]).is_some());
        // The bridge table too.
        let bridge = nft(&["list", "table", "bridge", "shipyard"]).unwrap();
        for part in [
            "chain ports_only",
            "set ports",
            "set macs",
            "set addresses",
            "set addresses6",
            "arp saddr ip",
            "icmpv6",
            "drop",
        ] {
            assert!(bridge.contains(part), "no {part:?} in:\n{bridge}");
        }
        assert!(
            !bridge.contains("unknown") && !bridge.contains("[invalid"),
            "{bridge}"
        );
        running.stop();
    });
}
