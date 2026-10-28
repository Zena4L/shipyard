use std::net::{IpAddr, SocketAddr};

use shipyard_network::observe::{Protocol, Socket, State};

use super::*;

fn at(address: &str) -> SocketAddr {
    address.parse().unwrap()
}

fn socket(protocol: Protocol, local: &str, remote: &str, state: State) -> Socket {
    Socket {
        protocol,
        local: at(local),
        remote: at(remote),
        state,
        uid: 0,
        inode: 1,
    }
}

fn up(local: &str, remote: &str) -> Socket {
    socket(Protocol::Tcp, local, remote, State::Established)
}

fn listening(local: &str) -> Socket {
    socket(Protocol::Tcp, local, "0.0.0.0:0", State::Listen)
}

fn namespace(owner: &str, sockets: Vec<Socket>) -> Namespace {
    Namespace {
        owner: owner.to_owned(),
        sockets,
    }
}

/// `a` at 10.0.0.2, `b` at 10.0.0.3, the host's gateway 10.0.0.1.
fn addresses() -> Addresses {
    Addresses {
        owners: [("10.0.0.2", "a"), ("10.0.0.3", "b")]
            .into_iter()
            .map(|(ip, owner)| (ip.parse::<IpAddr>().unwrap(), owner.to_owned()))
            .collect(),
        host: std::iter::once("10.0.0.1".parse().unwrap()).collect(),
        published: Vec::new(),
    }
}

fn node(id: &str, project: Option<&str>, service: &str, state: &'static str) -> Node {
    Node {
        id: id.to_owned(),
        name: id.to_owned(),
        serial: "SHPU 000001 0".to_owned(),
        memory_max: None,
        started_at: None,
        project: project.map(str::to_owned),
        service: Some(service.to_owned()),
        state,
        health: None,
        exit_code: None,
        one_off: false,
        shares_network_with: Vec::new(),
        up: matches!(state, "running" | "paused" | "stopping"),
        ended: state == "exited",
    }
}

/// Compose's label read leniently: its default condition, an entry with
/// no service skipped, a fourth field ignored, at most 256 entries.
#[test]
fn a_depends_on_label_is_read_as_compose_writes_it() {
    let read = |label| {
        parse_depends_on(label)
            .into_iter()
            .map(|depends| (depends.service, depends.condition))
            .collect::<Vec<_>>()
    };
    assert!(read("").is_empty());
    assert_eq!(
        read("db:service_healthy:true,cache, :x,job:service_completed_successfully:false:true"),
        [
            ("db".to_owned(), "service_healthy".to_owned()),
            ("cache".to_owned(), "service_started".to_owned()),
            (
                "job".to_owned(),
                "service_completed_successfully".to_owned()
            ),
        ]
    );
    let many = vec!["s:service_started:false"; 1000].join(",");
    assert_eq!(read(&many).len(), MAX_DEPENDS);
}

/// A declared link goes to every container of the service in the same
/// project (never another's, never a one-off's); refused as its
/// condition says; to a missing service, refused, when none has one.
#[test]
fn declared_links_follow_compose_s_rules() {
    let web = node("web", Some("p"), "web", "running");
    let api1 = node("api1", Some("p"), "api", "running");
    let mut api2 = node("api2", Some("p"), "api", "running");
    api2.health = Some(crate::health::Status::Unhealthy);
    let other_project = node("api3", Some("q"), "api", "running");
    let mut one_off = node("api4", Some("p"), "api", "running");
    one_off.one_off = true;
    let mut job = node("job", Some("p"), "job", "exited");
    job.exit_code = Some(0);
    let mut failed = node("failed", Some("p"), "failed", "exited");
    failed.exit_code = Some(1);
    let label = "api:service_healthy:false,job:service_completed_successfully:false,\
                 failed:service_completed_successfully:false,gone:service_started:false";
    let all = [&web, &api1, &api2, &other_project, &one_off, &job, &failed];
    let declaring: Vec<Declaring<'_>> = all
        .iter()
        .map(|node| Declaring {
            node,
            depends_on: (node.id == "web").then_some(label),
        })
        .collect();
    let (links, missing) = declared(&declaring);
    let said: Vec<(&str, &str, bool)> = links
        .iter()
        .map(|link| (link.from.as_str(), link.to.as_str(), link.refused))
        .collect();
    assert_eq!(
        said,
        [
            ("web", "api1", false),
            ("web", "api2", true),
            ("web", "job", false),
            ("web", "failed", true),
            ("web", "p/gone", true),
        ]
    );
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].service, "gone");
    assert!(links.iter().all(|link| link.kind == LinkKind::Declared));
    assert_eq!(
        links.iter().map(|link| link.to_missing).collect::<Vec<_>>(),
        [false, false, false, false, true]
    );
}

/// `service_started` is met by a target up (stopping still counts) and
/// refused by one stopped; `service_healthy` by one up without checks;
/// `service_completed_successfully` never by one about to restart, whatever
/// its last exit; a service named twice is one link.
#[test]
fn a_declared_condition_is_met_as_compose_waits_for_it() {
    let refused = |target: &Node, condition: &str| {
        let from = node("web", Some("p"), "web", "running");
        let label = format!(
            "{0}:{condition}:false,{0}:{condition}:false",
            target.service.as_deref().unwrap()
        );
        let (links, _) = declared(&[
            Declaring {
                node: &from,
                depends_on: Some(&label),
            },
            Declaring {
                node: target,
                depends_on: None,
            },
        ]);
        assert_eq!(links.len(), 1, "named twice, one link: {links:?}");
        links[0].refused
    };
    assert!(!refused(
        &node("db", Some("p"), "db", "running"),
        "service_started"
    ));
    assert!(!refused(
        &node("db", Some("p"), "db", "stopping"),
        "service_started"
    ));
    assert!(refused(
        &node("db", Some("p"), "db", "exited"),
        "service_started"
    ));
    assert!(!refused(
        &node("db", Some("p"), "db", "running"),
        "service_healthy"
    ));
    let mut restarting = node("job", Some("p"), "job", "restarting");
    restarting.exit_code = Some(0);
    assert!(refused(&restarting, "service_completed_successfully"));
}

/// A connection up between two containers is one link, from the client to
/// the port the server listens on, counted once (the server's side of it
/// counts nothing); without the server's mirror socket, or to an address
/// that's no container's, it's no link; a closed connection lingering
/// (`TIME-WAIT`) isn't counted.
#[test]
fn an_observed_link_is_counted_once_from_its_client() {
    let a = namespace("a", vec![up("10.0.0.2:40000", "10.0.0.3:8080")]);
    let b = namespace(
        "b",
        vec![
            listening("0.0.0.0:8080"),
            up("10.0.0.3:8080", "10.0.0.2:40000"),
        ],
    );
    let links = observed(&[a.clone(), b], &addresses());
    assert_eq!(links.len(), 1, "{links:?}");
    let link = &links[0];
    assert_eq!(
        (
            link.from.as_str(),
            link.to.as_str(),
            link.port,
            link.connections
        ),
        ("a", "b", Some(8080), 1)
    );
    assert_eq!(link.protocol, Some("tcp"));
    // No mirror: B never saw it (another's address, or gone).
    let lonely = namespace("b", vec![listening("0.0.0.0:8080")]);
    assert!(observed(&[a.clone(), lonely], &addresses()).is_empty());
    // An address that's no container's.
    let outside = namespace("a", vec![up("10.0.0.2:40000", "203.0.113.7:8080")]);
    assert!(observed(&[outside], &addresses()).is_empty());
    // Closed: not counted, though both still list it.
    let lingering = |local, remote| socket(Protocol::Tcp, local, remote, State::TimeWait);
    let gone = [
        namespace("a", vec![lingering("10.0.0.2:40000", "10.0.0.3:8080")]),
        namespace(
            "b",
            vec![
                listening("0.0.0.0:8080"),
                lingering("10.0.0.3:8080", "10.0.0.2:40000"),
            ],
        ),
    ];
    assert!(observed(&gone, &addresses()).is_empty());
}

/// A server whose listener has closed (`nc -l`, a reload) keeps its links
/// while its connections are up: the lower port is the server's.
#[test]
fn a_link_outlives_its_listener() {
    let tables = [
        namespace("a", vec![up("10.0.0.2:40000", "10.0.0.3:8080")]),
        namespace("b", vec![up("10.0.0.3:8080", "10.0.0.2:40000")]),
    ];
    let links = observed(&tables, &addresses());
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!((links[0].from.as_str(), links[0].to.as_str()), ("a", "b"));
}

/// Through a port the target publishes on the host (its remote the host's
/// address): masqueraded by the host's NAT on its way, the target sees a
/// host address and the client's source port; a link to the container
/// port, said so, only with that socket; an outside service on that port
/// is none, nor is a container reaching its own published port.
#[test]
fn a_link_through_a_published_port_is_confirmed_by_its_target() {
    let mut addresses = addresses();
    addresses.published.push(Entry {
        container: "b".to_owned(),
        host_ip: "0.0.0.0".parse().unwrap(),
        host_port: 18080,
        container_port: 80,
        protocol: "tcp".to_owned(),
        connections: 0,
    });
    let a = namespace("a", vec![up("10.0.0.2:40000", "10.0.0.1:18080")]);
    let b = namespace(
        "b",
        vec![listening("0.0.0.0:80"), up("10.0.0.3:80", "10.0.0.1:40000")],
    );
    let links = observed(&[a.clone(), b], &addresses);
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(
        (links[0].from.as_str(), links[0].to.as_str(), links[0].port),
        ("a", "b", Some(80))
    );
    assert!(links[0].via_host_port);
    // Its source port remapped by the NAT (another connection held it):
    // not confirmed, never a guess.
    let remapped = namespace(
        "b",
        vec![listening("0.0.0.0:80"), up("10.0.0.3:80", "10.0.0.1:40001")],
    );
    assert!(observed(&[a.clone(), remapped], &addresses).is_empty());
    let unseen = namespace("b", vec![listening("0.0.0.0:80")]);
    // Another container's socket that looks the part (its own port 80, from
    // the host and that source port) confirms nothing of b's.
    let c = namespace(
        "c",
        vec![listening("0.0.0.0:80"), up("10.0.0.4:80", "10.0.0.1:40000")],
    );
    assert!(
        observed(&[a.clone(), unseen.clone(), c], &addresses).is_empty(),
        "another's socket"
    );
    assert!(observed(&[a, unseen], &addresses).is_empty(), "no mirror");
    let outside = namespace("a", vec![up("10.0.0.2:40000", "203.0.113.7:18080")]);
    assert!(
        observed(&[outside], &addresses).is_empty(),
        "an outside service"
    );
    let itself = namespace(
        "b",
        vec![
            listening("0.0.0.0:80"),
            up("10.0.0.3:40000", "10.0.0.1:18080"),
            up("10.0.0.3:80", "10.0.0.1:40000"),
        ],
    );
    assert!(observed(&[itself], &addresses).is_empty(), "its own port");
}

/// A published port's connections are the host's clients': two from
/// outside (the host through its NAT, a Mac's forwarded port) counted;
/// never another container's (directly, or through the host with its
/// source port kept), the container's own (loopback), a `TIME_WAIT`, a
/// listener, another port, or a UDP entry.
#[test]
fn a_published_port_s_connections_are_the_host_s() {
    let mut addresses = addresses();
    let entry = |port: u16, protocol: &str| Entry {
        container: "b".to_owned(),
        host_ip: "0.0.0.0".parse().unwrap(),
        host_port: port,
        container_port: 80,
        protocol: protocol.to_owned(),
        connections: 0,
    };
    addresses.published = vec![
        entry(18080, "tcp"),
        entry(18081, "udp"),
        entry(19090, "tcp"),
    ];
    let b = namespace(
        "b",
        vec![
            listening("0.0.0.0:80"),
            // The host's clients (one by its gateway, one from elsewhere).
            up("10.0.0.3:80", "10.0.0.1:51000"),
            up("10.0.0.3:80", "192.0.2.9:52000"),
            // a, directly; a through the host's NAT (its client socket below).
            up("10.0.0.3:80", "10.0.0.2:41000"),
            up("10.0.0.3:80", "10.0.0.1:40000"),
            up("10.0.0.3:80", "10.0.0.1:42000"),
            // Its own; ended; another port.
            up("127.0.0.1:80", "127.0.0.1:53000"),
            socket(
                Protocol::Tcp,
                "10.0.0.3:80",
                "10.0.0.1:54000",
                State::TimeWait,
            ),
            up("10.0.0.3:81", "10.0.0.1:55000"),
        ],
    );
    let a = namespace(
        "a",
        vec![
            up("10.0.0.2:41000", "10.0.0.3:80"),
            up("10.0.0.2:40000", "10.0.0.1:18080"),
            // a again, through the other host port publishing port 80.
            up("10.0.0.2:42000", "10.0.0.1:19090"),
        ],
    );
    // Each host port publishing port 80 says its outside connections; a's
    // through either is never one.
    assert_eq!(entry_connections(&[a, b.clone()], &addresses), [2, 0, 2]);
    // Without a's sockets, the NAT'd ones are the host's like any other.
    assert_eq!(entry_connections(&[b], &addresses), [4, 0, 4]);
}

/// A container's table is kept to what links are made of: a client on a
/// LAN (no lease, no gateway, unmasqueraded through a published port) is
/// kept on a published container port; elsewhere, not.
#[test]
fn a_lan_client_of_a_published_port_is_kept() {
    let mut addresses = addresses();
    addresses.published.push(Entry {
        container: "b".to_owned(),
        host_ip: "0.0.0.0".parse().unwrap(),
        host_port: 18080,
        container_port: 80,
        protocol: "tcp".to_owned(),
        connections: 0,
    });
    assert!(kept(&up("10.0.0.3:80", "192.168.1.20:51000"), &addresses));
    assert!(
        !kept(&up("10.0.0.3:81", "192.168.1.20:51000"), &addresses),
        "another port"
    );
    assert!(
        kept(&up("10.0.0.3:5000", "10.0.0.2:41000"), &addresses),
        "a container"
    );
    assert!(
        kept(&up("10.0.0.3:5000", "10.0.0.1:41000"), &addresses),
        "the host"
    );
    assert!(kept(&listening("0.0.0.0:9000"), &addresses));
    assert!(
        !kept(
            &socket(
                Protocol::Tcp,
                "10.0.0.3:80",
                "192.168.1.20:51000",
                State::TimeWait
            ),
            &addresses
        ),
        "ended"
    );
}

/// A UDP server's socket is never connected: a connected client's
/// datagrams to a container bound on that port are a link; to a port no
/// one is bound on, none.
#[test]
fn a_udp_link_is_confirmed_by_its_server_s_port() {
    let udp = |local, remote, state| socket(Protocol::Udp, local, remote, state);
    let a = namespace(
        "a",
        vec![udp("10.0.0.2:40000", "10.0.0.3:53", State::Established)],
    );
    let b = namespace("b", vec![udp("0.0.0.0:53", "0.0.0.0:0", State::Close)]);
    let links = observed(&[a.clone(), b], &addresses());
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(
        (links[0].to.as_str(), links[0].port, links[0].protocol),
        ("b", Some(53), Some("udp"))
    );
    let elsewhere = namespace("b", vec![udp("0.0.0.0:5353", "0.0.0.0:0", State::Close)]);
    assert!(observed(&[a, elsewhere], &addresses()).is_empty());
}

/// Many connections to one peer are one link with their count; reading
/// 100 namespaces of 1,000 sockets each stays well within a poll (H7).
#[test]
fn connections_are_aggregated_within_a_budget() {
    let clients: Vec<Socket> = (0..10_000u16)
        .map(|n| up(&format!("10.0.0.2:{}", 20_000 + n), "10.0.0.3:8080"))
        .collect();
    let mut server = vec![listening("0.0.0.0:8080")];
    server.extend((0..10_000u16).map(|n| up("10.0.0.3:8080", &format!("10.0.0.2:{}", 20_000 + n))));
    let links = observed(
        &[namespace("a", clients), namespace("b", server)],
        &addresses(),
    );
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].connections, 10_000);
    // 100 namespaces, each a client of the next.
    let mut owners = Addresses::default();
    let tables: Vec<Namespace> = (0..100u16)
        .map(|n| {
            let me = format!("10.1.{}.{}", n / 200, n % 200 + 1);
            let next = format!("10.1.{}.{}", (n + 1) / 200, (n + 1) % 200 + 1);
            owners.owners.insert(me.parse().unwrap(), format!("c{n}"));
            let mut sockets = vec![listening("0.0.0.0:5000")];
            for port in 0..500u16 {
                sockets.push(up(
                    &format!("{me}:{}", 30_000 + port),
                    &format!("{next}:5000"),
                ));
                sockets.push(up(
                    &format!("{me}:5000"),
                    &format!(
                        "10.1.{}.{}:{}",
                        n.saturating_sub(1) / 200,
                        n.saturating_sub(1) % 200 + 1,
                        30_000 + port
                    ),
                ));
            }
            namespace(&format!("c{n}"), sockets)
        })
        .collect();
    let began = std::time::Instant::now();
    let links = observed(&tables, &owners);
    let took = began.elapsed();
    assert_eq!(links.len(), 99, "each to the next");
    assert!(
        took < std::time::Duration::from_secs(2),
        "100,000 sockets took {took:?}"
    );
}

/// The answer says its version, and its kinds as words.
#[test]
fn the_answer_says_its_version() {
    let links = Links {
        version: LINKS_VERSION,
        containers: Vec::new(),
        missing: Vec::new(),
        entries: Vec::new(),
        links: vec![Link {
            from: "a".to_owned(),
            to: "b".to_owned(),
            to_missing: false,
            kind: LinkKind::Observed,
            port: Some(1),
            protocol: Some("tcp"),
            condition: None,
            connections: 1,
            via_host_port: false,
            refused: false,
        }],
        observed: Observed::available(),
        unread: Vec::new(),
    };
    let json = serde_json::to_value(&links).unwrap();
    assert_eq!(json["version"], 1);
    assert_eq!(json["links"][0]["kind"], "observed");
}
