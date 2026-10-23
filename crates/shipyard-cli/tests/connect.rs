//! End-to-end tests of `network connect` and `disconnect` (stage 9, step
//! 9.7b2; Linux, root): a running container joins a network at once (found
//! by name there, held after the command has gone) and leaves it; its record
//! keeps both, so a restart joins what it's on now; the network that carries
//! its way out needs `--force`; a stopped container's change waits for its
//! start; and its interfaces' own settings are set once they're made.
//!
//! Each test runs in a network namespace of its own, with networks and
//! stores in scratch directories, and an image from the fixture registry.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use shipyard_network::dns::message::rtype;
use shipyard_test_support::net::{NetworkDir, Upstream, addresses, resolve};
use shipyard_test_support::registry::{Scratch, Serving, fixture};
use shipyard_test_support::{Run, Shipyard, in_new_netns};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// A test's networks, its stores and the fixture image.
struct Setup {
    networks: NetworkDir,
    scratch: Scratch,
    image: String,
    _registry: Serving,
}

impl Setup {
    fn new(networks: &[&str]) -> Self {
        // The namespace's loopback, up (a host's is): the registry serves
        // on it.
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        let dir = NetworkDir::new(SHIPYARD);
        for name in networks {
            dir.create(&[name]);
        }
        let (registry, image) = fixture();
        let scratch = Scratch::new();
        let containers = scratch.0.join("containers");
        std::fs::create_dir_all(&containers).unwrap();
        std::fs::set_permissions(&containers, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            networks: dir,
            scratch,
            image,
            _registry: registry,
        }
    }

    /// `shipyard ARGS`, on these stores and networks.
    fn shipyard(&self, args: &[&str]) -> Run {
        let mut command = self.networks.command(args);
        command
            .env("SHIPYARD_IMAGE_STORE", self.scratch.store())
            .env("SHIPYARD_CONTAINER_DIR", self.scratch.0.join("containers"))
            .env("SHIPYARD_DNS_UPSTREAM", Upstream::ADDRESS.to_string());
        shipyard_test_support::run(command)
    }

    /// `shipyard ARGS`, which must succeed.
    fn ok(&self, args: &[&str]) -> Run {
        let run = self.shipyard(args);
        assert_eq!(run.code, Some(0), "{args:?}: {}", run.stderr);
        run
    }

    /// `run -d --name NAME FLAGS… IMAGE sleep 300`: its ID.
    fn detached(&self, name: &str, flags: &[&str]) -> String {
        let args = [
            &["run", "-d", "--name", name],
            flags,
            &[&self.image, "sleep", "300"],
        ]
        .concat();
        self.ok(&args).stdout.trim().to_owned()
    }

    /// The names of the containers on `network` now, as `network inspect`
    /// shows them.
    fn on(&self, network: &str) -> Vec<String> {
        self.networks.inspect(network)["Containers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|lease| lease["Name"].as_str().map(str::to_owned))
            .collect()
    }

    /// The bridge networks container `name`'s record joins.
    fn recorded(&self, name: &str) -> Vec<String> {
        let shown: serde_json::Value =
            serde_json::from_str(&self.ok(&["inspect", "--shipyard", name]).stdout).unwrap();
        shown[0]["config"]["network"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|network| network["bridges"].as_array().cloned().unwrap_or_default())
            .filter_map(|bridge| bridge["name"].as_str().map(str::to_owned))
            .collect()
    }
}

/// Whether any process has the file that was at `path` open (removed:
/// its descriptors' links say `PATH (deleted)`).
fn held_anywhere(path: &std::path::Path) -> bool {
    let deleted = format!("{} (deleted)", path.display());
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|process| std::fs::read_dir(process.path().join("fd")).ok())
        .flatten()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .any(|target| target.to_string_lossy() == deleted)
}

/// Waits (up to 15 s) for `found`, naming `what` if it never comes.
fn eventually(what: &str, mut found: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !found() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A running container joins a network at once: a peer there finds it by
/// name, the network lists it, and its lease outlives the command (the
/// network can't be removed); it leaves at once, and the network is free.
#[test]
#[ignore = "needs root"]
fn a_running_container_joins_and_leaves_a_network() {
    in_new_netns(|| {
        let setup = Setup::new(&["front", "back"]);
        let id = setup.detached("web", &["--network", "front"]);
        let (peer, netns) = setup.networks.sleeping(&["--network", "back"]);
        let gateway = setup.networks.gateway("back");
        let connected = setup.ok(&["network", "connect", "back", "web"]);
        assert_eq!(connected.stdout, "");
        assert_eq!(
            setup
                .on("back")
                .iter()
                .filter(|name| *name == "web")
                .count(),
            1
        );
        assert_eq!(
            setup.networks.lease("back", "web")["Container"],
            id.as_str(),
            "its lease names it"
        );
        let address = setup.networks.address("back", "web");
        // Its interface there, inside it, up.
        let state = setup.ok(&[
            "exec",
            "web",
            "sh",
            "-c",
            "read state < /sys/class/net/eth1/operstate; echo $state",
        ]);
        assert_eq!(state.stdout.trim(), "up");
        let answer = resolve(&netns, gateway, "web", rtype::A).unwrap();
        assert_eq!(addresses(&answer), [address]);
        // Held by its monitor, the command gone.
        let busy = setup.shipyard(&["network", "rm", "back"]);
        assert_eq!(busy.code, Some(1), "{}", busy.stderr);
        assert!(busy.stderr.contains("web"), "{}", busy.stderr);
        assert_eq!(setup.recorded("web"), ["front", "back"]);
        // Twice: refused.
        let again = setup.shipyard(&["network", "connect", "back", "web"]);
        assert_eq!(again.code, Some(1), "{}", again.stderr);
        assert!(again.stderr.contains("already"), "{}", again.stderr);
        // Leaving: gone from the network and its names at once.
        setup.ok(&["network", "disconnect", "back", "web"]);
        assert!(!setup.on("back").contains(&"web".to_owned()));
        eventually("its name forgotten", || {
            resolve(&netns, gateway, "web", rtype::A)
                .is_some_and(|answer| addresses(&answer) != [address])
        });
        assert_eq!(setup.recorded("web"), ["front"]);
        // Its monitor let go of its copy too: no process holds the removed
        // lease's file.
        let lease_file = setup
            .networks
            .path()
            .join("back/leases")
            .join(address.to_string());
        assert!(!lease_file.exists(), "its lease's file removed at once");
        eventually("the removed lease let go of", || {
            !held_anywhere(&lease_file)
        });
        peer.stop();
        setup.ok(&["network", "rm", "back"]);
        setup.ok(&["rm", "-f", "web"]);
    });
}

/// Its record keeps what it's on: a restart joins it again; the network
/// that carries its way out needs `--force`; its only network can't be
/// left; a stopped container's connect waits for its start.
#[test]
#[ignore = "needs root"]
fn the_record_keeps_it_and_the_way_out_needs_force() {
    in_new_netns(|| {
        let setup = Setup::new(&["front", "back", "spare"]);
        setup.detached("web", &["--network", "front"]);
        setup.ok(&["network", "connect", "back", "web"]);
        setup.ok(&["restart", "-t", "1", "web"]);
        assert!(setup.on("back").contains(&"web".to_owned()), "joined again");
        let refused = setup.shipyard(&["network", "disconnect", "front", "web"]);
        assert_eq!(refused.code, Some(1), "{}", refused.stderr);
        assert!(refused.stderr.contains("--force"), "{}", refused.stderr);
        let forced = setup.ok(&["network", "disconnect", "--force", "front", "web"]);
        assert!(
            forced.stderr.contains("no IPv4 default route now"),
            "{}",
            forced.stderr
        );
        assert!(!setup.on("front").contains(&"web".to_owned()));
        let only = setup.shipyard(&["network", "disconnect", "back", "web"]);
        assert_eq!(only.code, Some(1), "{}", only.stderr);
        assert!(only.stderr.contains("only network"), "{}", only.stderr);
        // Stopped: its record only, then its start.
        setup.ok(&["stop", "-t", "1", "web"]);
        setup.ok(&["network", "connect", "spare", "web"]);
        assert!(
            !setup.on("spare").contains(&"web".to_owned()),
            "not before its start"
        );
        setup.ok(&["start", "web"]);
        assert!(setup.on("spare").contains(&"web".to_owned()));
        assert!(setup.on("back").contains(&"web".to_owned()));
        // Not a bridge network; not a container's: refused.
        let host = setup.shipyard(&["network", "connect", "host", "web"]);
        assert_eq!(host.code, Some(1), "{}", host.stderr);
        let nobody = setup.shipyard(&["network", "connect", "back", "nobody"]);
        assert_eq!(nobody.code, Some(1), "{}", nobody.stderr);
        // An address the network can't give: refused now, not at a start.
        let outside = setup.shipyard(&["network", "connect", "--ip", "192.0.2.9", "front", "web"]);
        assert_eq!(outside.code, Some(1), "{}", outside.stderr);
        assert!(outside.stderr.contains("subnet"), "{}", outside.stderr);
        setup.ok(&["rm", "-f", "web"]);
    });
}

/// A network a stopped container names doesn't keep it (Docker's rule,
/// D11.4a-2): `network rm` removes it and `prune` takes it, while one a
/// running container is on is kept (its control). The stopped container's
/// next start fails, naming the network (Docker's says it's not found).
#[test]
#[ignore = "needs root"]
fn a_stopped_containers_network_goes_as_docker_s() {
    in_new_netns(|| {
        let setup = Setup::new(&["front", "back"]);
        setup.detached("web", &["--network", "front", "--network", "back"]);
        // Up: kept.
        let busy = setup.shipyard(&["network", "rm", "back"]);
        assert_eq!(busy.code, Some(1), "{}", busy.stderr);
        assert!(
            busy.stderr.contains("the network back is in use by web"),
            "{}",
            busy.stderr
        );
        // Waiting to restart (its policy's backoff): as good as up, kept.
        let image = setup.image.clone();
        setup.ok(&[
            "run",
            "-d",
            "--name",
            "again",
            "--network",
            "front",
            "--restart",
            "always",
            &image,
            "sh",
            "-c",
            "exit 1",
        ]);
        shipyard_test_support::eventually(Duration::from_secs(10), "it waits to restart", || {
            setup
                .ok(&["inspect", "-f", "{{.State.Restarting}}", "again"])
                .stdout
                .trim()
                == "true"
        });
        let waiting = setup.shipyard(&["network", "rm", "front"]);
        assert_eq!(waiting.code, Some(1), "{}", waiting.stderr);
        assert!(
            waiting.stderr.contains("in use by again"),
            "{}",
            waiting.stderr
        );
        setup.ok(&["rm", "-f", "again"]);
        setup.ok(&["stop", "-t", "1", "web"]);
        setup.ok(&["network", "rm", "back"]);
        let pruned = setup.ok(&["network", "prune", "-f"]);
        assert!(
            pruned.stdout.lines().any(|line| line == "front"),
            "{}",
            pruned.stdout
        );
        let start = setup.shipyard(&["start", "web"]);
        assert_eq!(start.code, Some(1), "{}", start.stderr);
        assert!(
            start.stderr.contains("there's no network named "),
            "{}",
            start.stderr
        );
        setup.ok(&["rm", "web"]);
    });
}

/// `port --sockets` (stage 9, step 9.7c1): what a running container
/// listens on, and its connections, each peer by its container's name; as
/// JSON too; none for a container that isn't running.
#[test]
#[ignore = "needs root"]
fn port_sockets_shows_its_listeners_and_peers() {
    in_new_netns(|| {
        use std::io::{Read as _, Write as _};
        let setup = Setup::new(&["app"]);
        setup.detached("web", &["--network", "app"]);
        setup.detached("client", &["--network", "app"]);
        let netns_of = |name: &str| {
            let shown: serde_json::Value =
                serde_json::from_str(&setup.ok(&["inspect", "--shipyard", name]).stdout).unwrap();
            let pid = shown[0]["state"]["pid"].as_i64().unwrap();
            std::fs::File::open(format!("/proc/{pid}/ns/net")).unwrap()
        };
        let web = setup.networks.address("app", "web");
        // Docker's `inspect` says its address there, from its live lease
        // (step 11.1).
        let docker: serde_json::Value =
            serde_json::from_str(&setup.ok(&["inspect", "web"]).stdout).unwrap();
        let endpoint = &docker[0]["NetworkSettings"]["Networks"]["app"];
        assert_eq!(endpoint["IPAddress"], web.to_string(), "{docker}");
        assert!(endpoint["IPPrefixLen"].as_u64().unwrap() > 0, "{docker}");
        assert_eq!(docker[0]["HostConfig"]["NetworkMode"], "app");
        // A listener in web's namespace, and client's connection to it,
        // kept open.
        let listener = shipyard_test_support::net::in_netns(&netns_of("web"), || {
            std::net::TcpListener::bind("0.0.0.0:7000").unwrap()
        });
        let mut connection = shipyard_test_support::net::in_netns(&netns_of("client"), || {
            std::net::TcpStream::connect((web, 7000)).unwrap()
        });
        let (mut accepted, _) = listener.accept().unwrap();
        connection.write_all(b"x").unwrap();
        let mut byte = [0];
        accepted.read_exact(&mut byte).unwrap();
        let table = setup.ok(&["port", "--sockets", "web"]).stdout;
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("PROTO"), "{table}");
        assert!(
            lines[1].contains("0.0.0.0:7000") && lines[1].contains("LISTEN"),
            "listening first: {table}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("ESTAB") && line.trim_end().ends_with("client")),
            "{table}"
        );
        let json: serde_json::Value = serde_json::from_str(
            &setup
                .ok(&["port", "--sockets", "--format", "json", "web"])
                .stdout,
        )
        .unwrap();
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .any(|sock| sock["peer"] == "client" && sock["state"] == "ESTAB"),
            "{json}"
        );
        setup.ok(&["stop", "-t", "1", "web"]);
        let stopped = setup.shipyard(&["port", "--sockets", "web"]);
        assert_eq!(stopped.code, Some(1), "{}", stopped.stderr);
        assert!(
            stopped.stderr.contains("is not running"),
            "{}",
            stopped.stderr
        );
        drop((connection, accepted));
        setup.ok(&["rm", "-f", "web", "client"]);
    });
}

/// Its interfaces' own settings (`--sysctl net.ipv4.conf.eth1.*`) are set
/// once its networks have made them; one for an interface it doesn't get is
/// refused.
#[test]
#[ignore = "needs root"]
fn an_interfaces_settings_are_set_once_it_is_made() {
    in_new_netns(|| {
        let setup = Setup::new(&["front", "back"]);
        setup.detached(
            "web",
            &[
                "--network",
                "front",
                "--network",
                "back",
                "--sysctl",
                "net.ipv4.conf.eth1.rp_filter=2",
            ],
        );
        let read = setup.ok(&[
            "exec",
            "web",
            "sh",
            "-c",
            "read value < /proc/sys/net/ipv4/conf/eth1/rp_filter; echo $value",
        ]);
        assert_eq!(read.stdout.trim(), "2");
        let refused = setup.shipyard(&[
            "run",
            "-d",
            "--name",
            "nope",
            "--network",
            "front",
            "--sysctl",
            "net.ipv4.conf.eth3.rp_filter=2",
            &setup.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.code, Some(125), "{}", refused.stderr);
        assert!(
            refused.stderr.contains("no interface eth3"),
            "{}",
            refused.stderr
        );
        // Its second network left: its next start passes over eth1's
        // setting (said), never refused.
        setup.ok(&["network", "disconnect", "back", "web"]);
        let restarted = setup.ok(&["restart", "-t", "1", "web"]);
        assert!(restarted.stderr.contains("eth1"), "{}", restarted.stderr);
        setup.ok(&["rm", "-f", "web"]);
        let _ = setup.shipyard(&["rm", "-f", "nope"]);
    });
}

/// A live step that fails after the record was written puts the record
/// back (stage 9, step 9.9a's gap tests): a connect to an address another
/// container's lease holds (in the subnet, so it passes the checks made
/// before the record, and fails at the lease) leaves web's record and the
/// network as they were; a disconnect that can't take the network's lock
/// (held here past the wait) leaves web on it, its record saying so. Each
/// then succeeds once the obstacle is gone.
#[test]
#[ignore = "needs root"]
fn a_failed_live_step_puts_the_record_back() {
    in_new_netns(|| {
        let setup = Setup::new(&["front", "back"]);
        setup.detached("web", &["--network", "front"]);
        setup.detached("db", &["--network", "back"]);
        let taken = setup.networks.address("back", "db").to_string();
        assert_eq!(setup.recorded("web"), ["front"]);
        let refused = setup.shipyard(&["network", "connect", "--ip", &taken, "back", "web"]);
        assert_eq!(refused.code, Some(1), "{}", refused.stderr);
        assert!(refused.stderr.contains("is taken"), "{}", refused.stderr);
        assert_eq!(setup.recorded("web"), ["front"], "its record put back");
        assert!(!setup.on("back").contains(&"web".to_owned()));
        // The control: the same connect, without the taken address.
        setup.ok(&["network", "connect", "back", "web"]);
        assert_eq!(setup.recorded("web"), ["front", "back"]);
        assert!(setup.on("back").contains(&"web".to_owned()));
        // The network's lock held (as a command changing it would) for
        // longer than a disconnect waits: its record is written, then the
        // lease can't be taken away.
        let lock = std::fs::File::open(setup.networks.path().join("back/lock")).unwrap();
        lock.lock().unwrap();
        let busy = setup.shipyard(&["network", "disconnect", "back", "web"]);
        lock.unlock().unwrap();
        drop(lock);
        assert_eq!(busy.code, Some(1), "{}", busy.stderr);
        assert!(busy.stderr.contains("is busy"), "{}", busy.stderr);
        assert_eq!(
            setup.recorded("web"),
            ["front", "back"],
            "its record put back"
        );
        assert!(setup.on("back").contains(&"web".to_owned()));
        // The control: the same disconnect, the lock free.
        setup.ok(&["network", "disconnect", "back", "web"]);
        assert_eq!(setup.recorded("web"), ["front"]);
        assert!(!setup.on("back").contains(&"web".to_owned()));
        setup.ok(&["rm", "-f", "web", "db"]);
    });
}
