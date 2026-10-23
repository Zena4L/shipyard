//! End-to-end tests of a container's network (Linux):
//!
//! - `--network none` and `host`: the container's own `/sys` and loopback
//!   interface, its `net.*` settings, its hostname;
//! - bridge networks (root): addresses and leases, the host reaching
//!   containers, a killed run's lease swept, `network rm` refusing a network
//!   in use, and the `/etc` files a root filesystem gets;
//! - a bundle's network namespace (made, joined by its path) and the host's
//!   devices moved into it (`netDevices`).
//!
//! Runs that make a namespace work as any user who can isolate (root, or
//! rootless). Tests that make interfaces need root, and make them in a
//! network namespace of the test's own (`in_new_netns`), with networks in a
//! scratch directory (`SHIPYARD_NETWORK_DIR`): never the host's.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::sync::mpsc;

use serde_json::json;
use shipyard_network::route::Rtnetlink;
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::net::NetworkDir;
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{
    Rootless, Run, Shipyard, in_new_netns, is_root, rootless, run, start_command,
};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `shipyard ARGS…`.
fn run_cli(args: &[&str]) -> Run {
    run(SHIPYARD.command(args))
}

/// `shipyard run FLAGS… sh -c SCRIPT`.
fn shipyard(flags: &[&str], script: &str) -> Run {
    run(SHIPYARD.command(
        &[
            shipyard_test_support::run_words(flags),
            flags,
            &["sh", "-c", script],
        ]
        .concat(),
    ))
}

/// Whether this user can run isolated commands here (root, or rootless
/// where the host allows it); says why not otherwise.
fn can_isolate() -> bool {
    is_root() || rootless(Rootless::Single)
}

/// The interfaces `/proc/net/dev` lists (the reader's network namespace).
fn interfaces(proc_net_dev: &str) -> Vec<String> {
    proc_net_dev
        .lines()
        .skip(2)
        .filter_map(|line| Some(line.split_once(':')?.0.trim().to_owned()))
        .collect()
}

#[test]
fn a_network_of_its_own_has_only_loopback() {
    if !can_isolate() {
        return;
    }
    let run = shipyard(&["--network", "none"], "cat /proc/net/dev");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(interfaces(&run.stdout), ["lo"]);
}

#[test]
fn the_hosts_network_with_host() {
    if !can_isolate() {
        return;
    }
    // The host's interfaces come and go (other containers'), so: the
    // container sees the ones the host had before and still has after.
    // (The default is a network of the container's own: the bridge network
    // for root, pasta without.)
    let before = interfaces(&std::fs::read_to_string("/proc/net/dev").unwrap());
    let run = shipyard(&["--network", "host"], "cat /proc/net/dev");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let after = interfaces(&std::fs::read_to_string("/proc/net/dev").unwrap());
    let seen = interfaces(&run.stdout);
    for name in before.iter().filter(|name| after.contains(name)) {
        assert!(seen.contains(name), "{name} not in {seen:?}");
    }
}

#[test]
fn its_sysfs_lists_its_own_loopback_and_it_is_up() {
    if !can_isolate() {
        return;
    }
    let root = RootFs::minimal();
    // `flags` 0x9: up (IFF_UP) and a loopback (IFF_LOOPBACK). Rootless too:
    // the user namespace owns the network namespace, so a sysfs of the
    // container's own can be mounted (probe N1).
    let run = shipyard(
        &["--network", "none", "--rootfs", root.arg()],
        "ls /sys/class/net; cat /sys/class/net/lo/flags; touch /sys/x",
    );
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert_eq!(run.stdout, "lo\n0x9\n");
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
}

#[test]
fn network_settings_with_a_network_of_its_own() {
    if !can_isolate() {
        return;
    }
    // Docker's defaults, and the user's own settings, which win.
    let run = shipyard(
        &[
            "--network",
            "none",
            "--sysctl",
            "net.ipv4.ip_forward=1",
            "--sysctl",
            "net.ipv4.ip_unprivileged_port_start=80",
        ],
        "cd /proc/sys/net/ipv4; cat ip_forward ip_unprivileged_port_start ping_group_range",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    // Every group from 0 to the top the container has: the host's whole
    // range as root, the top of the group map rootless.
    let top = if is_root() { "2147483647" } else { "" };
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", run.stdout);
    assert_eq!(lines[..2], ["1", "80"]);
    let (low, high) = lines[2].split_once('\t').expect("a range");
    assert_eq!(low, "0");
    if !top.is_empty() {
        assert_eq!(high, top);
    }
    let defaults = shipyard(
        &["--network", "none"],
        "cat /proc/sys/net/ipv4/ip_unprivileged_port_start",
    );
    assert_eq!(defaults.stdout, "0\n", "{}", defaults.stderr);
}

#[test]
fn network_settings_need_a_network_of_its_own() {
    let run = shipyard(
        &["--network", "host", "--sysctl", "net.ipv4.ip_forward=1"],
        "true",
    );
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("any --network but host gives it one"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_setting_only_the_host_has_is_explained() {
    if !can_isolate() {
        return;
    }
    // A `net.core` setting the host has and a network namespace doesn't
    // (one the whole machine shares). Which they are changes with the
    // kernel (6.x made some per namespace), so it's found here.
    let ours = shipyard(&["--network", "none"], "ls /proc/sys/net/core");
    assert_eq!(ours.code, Some(0), "{}", ours.stderr);
    let Some(host_only) = std::fs::read_dir("/proc/sys/net/core")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .find(|name| !ours.stdout.lines().any(|line| line == name))
    else {
        eprintln!("every net.core setting is a network namespace's own on this kernel");
        return;
    };
    let key = format!("net.core.{host_only}=1");
    let run = shipyard(&["--network", "none", "--sysctl", &key], "true");
    assert_eq!(run.code, Some(125), "{key}: {}", run.stderr);
    assert!(
        run.stderr
            .contains("a network namespace has no such setting"),
        "{key}: {}",
        run.stderr
    );
}

/// The interface names in this thread's network namespace.
fn links() -> Vec<String> {
    Rtnetlink::new()
        .unwrap()
        .links()
        .unwrap()
        .into_iter()
        .map(|link| link.name)
        .collect()
}

/// Waits (up to 5 s) for a TCP connection to `address` to be refused: the
/// address is a live host (its kernel answered), with nothing listening.
fn refused_at(address: &str) -> bool {
    let address: std::net::SocketAddr = address.parse().unwrap();
    (0..50).any(|_| {
        match std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(100))
        {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => true,
            _ => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }
    })
}

#[test]
fn bridge_networks_need_root() {
    if is_root() || !rootless(Rootless::Single) {
        return;
    }
    let run = shipyard(&["--network", "bridge"], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("need root for now"), "{}", run.stderr);
    // Listed, as Docker's: the built-ins rootless has (step 11.3c).
    let network = run_cli(&["network", "ls", "--format", "{{.Name}}"]);
    assert_eq!(network.code, Some(0), "{}", network.stderr);
    assert_eq!(network.stdout, "host\nnone\npasta\n");
}

#[test]
fn the_hostname_is_the_runs_id_with_a_network_of_its_own() {
    if !can_isolate() {
        return;
    }
    let run = shipyard(&["--network", "none"], "cat /proc/sys/kernel/hostname");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let name = run.stdout.trim();
    assert_eq!(name.len(), 12, "{name:?}");
    assert!(name.bytes().all(|b| b.is_ascii_hexdigit()), "{name:?}");
    // With the host's network, the host's name (the one its DNS knows).
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let shared = shipyard(&["--network", "host"], "cat /proc/sys/kernel/hostname");
    assert_eq!(shared.stdout, host, "{}", shared.stderr);
    let given = shipyard(
        &["--network", "none", "--hostname", "web-1"],
        "cat /proc/sys/kernel/hostname",
    );
    assert_eq!(given.stdout, "web-1\n", "{}", given.stderr);
}

#[test]
#[ignore = "needs root"]
fn a_container_on_the_default_network_has_its_address_and_default_route() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let root = RootFs::minimal();
        let run = networks.run(&[
            "run",
            "--network",
            "bridge",
            "--rootfs",
            root.arg(),
            "sh",
            "-c",
            "cat /sys/class/net/eth0/address /sys/class/net/eth0/flags; cat /proc/net/route",
        ]);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        let lines: Vec<&str> = run.stdout.lines().collect();
        // The first address of 10.88.0.0/16 after the gateway, and its MAC.
        assert_eq!(lines[0], "02:42:0a:58:00:02", "{}", run.stdout);
        // Up (`IFF_UP`, set as the run brings it up), not its `operstate`,
        // which the kernel sets a moment later (F85).
        let flags = u32::from_str_radix(lines[1].trim_start_matches("0x"), 16).unwrap();
        assert_eq!(flags & 0x1, 0x1, "IFF_UP: {}", lines[1]);
        // The default route, via 10.88.0.1 (little-endian hex in /proc).
        assert!(
            run.stdout
                .lines()
                .any(|line| line.starts_with("eth0\t00000000\t0100580A")),
            "{}",
            run.stdout
        );
        // The bridge stays; the run's veth went with it.
        let after = links();
        assert!(after.contains(&"shipyard0".to_owned()), "{after:?}");
        assert!(
            !after
                .iter()
                .any(|name| name.starts_with("shy") && name != "shipyard0"),
            "{after:?}"
        );
    });
}

#[test]
#[ignore = "needs root"]
fn the_host_reaches_containers_on_a_network() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let created = networks.run(&["network", "create", "--subnet", "10.90.0.0/24", "app"]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        let first = start_command(networks.command(&[
            "run",
            "--no-image",
            "--network",
            "app",
            "--ip",
            "10.90.0.50",
            "sleep",
            "30",
        ]));
        let second = start_command(networks.command(&[
            "run",
            "--no-image",
            "--network",
            "app",
            "--name",
            "api",
            "sleep",
            "30",
        ]));
        // Each container's kernel answers: nothing listens, so it refuses.
        assert!(
            refused_at("10.90.0.50:9"),
            "the first container doesn't answer"
        );
        assert!(
            refused_at("10.90.0.2:9"),
            "the second container doesn't answer"
        );
        let inspect = networks.run(&["network", "inspect", "--shipyard", "app"]);
        assert!(
            inspect.stdout.contains("\"10.90.0.50\""),
            "{}",
            inspect.stdout
        );
        // Taken: refused.
        let taken = networks.run(&[
            "run",
            "--no-image",
            "--network",
            "app",
            "--ip",
            "10.90.0.50",
            "true",
        ]);
        assert_eq!(taken.code, Some(125), "{}", taken.stderr);
        assert!(taken.stderr.contains("taken"), "{}", taken.stderr);
        // In use: can't be removed.
        let busy = networks.run(&["network", "rm", "app"]);
        assert_eq!(busy.code, Some(1), "{}", busy.stderr);
        // Named: by name; the other (a run with no record, unnamed) by its
        // run's ID.
        assert!(busy.stderr.contains("in use by "), "{}", busy.stderr);
        assert!(busy.stderr.contains("api"), "{}", busy.stderr);
        // A prune keeps it (in use isn't a failure), and removes the one no
        // container is on.
        let spare = networks.run(&["network", "create", "--subnet", "10.95.0.0/24", "spare"]);
        assert_eq!(spare.code, Some(0), "{}", spare.stderr);
        let pruned = networks.run(&["network", "prune", "-f"]);
        assert_eq!(pruned.code, Some(0), "{}", pruned.stderr);
        assert_eq!(
            pruned.stdout, "spare\nRemoved 1 network(s)\nKept 1 in use: app\n",
            "{}",
            pruned.stderr
        );
        first.signal(libc::SIGTERM);
        second.signal(libc::SIGTERM);
        let _ = (first.finish(), second.finish());
        let removed = networks.run(&["network", "rm", "app"]);
        assert_eq!(removed.code, Some(0), "{}", removed.stderr);
        assert!(
            !links().iter().any(|name| name.starts_with("shy-")),
            "{:?}",
            links()
        );
    });
}

#[test]
#[ignore = "needs root"]
fn a_killed_runs_address_is_swept_by_the_next() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let created = networks.run(&["network", "create", "--subnet", "10.91.0.0/24", "app"]);
        assert_eq!(created.code, Some(0), "{}", created.stderr);
        let killed = start_command(networks.command(&[
            "run",
            "--no-image",
            "--network",
            "app",
            "--ip",
            "10.91.0.9",
            "sleep",
            "30",
        ]));
        assert!(refused_at("10.91.0.9:9"));
        killed.signal(libc::SIGKILL);
        let _ = killed.finish_status();
        // The lease is still on disk, but its lock is gone: the next run
        // sweeps it, and may take the address.
        let next = networks.run(&[
            "run",
            "--no-image",
            "--network",
            "app",
            "--ip",
            "10.91.0.9",
            "true",
        ]);
        assert_eq!(next.code, Some(0), "{}", next.stderr);
        let inspect = networks.run(&["network", "inspect", "--shipyard", "app"]);
        assert!(
            inspect.stdout.contains("\"Containers\": []"),
            "{}",
            inspect.stdout
        );
        assert!(
            !links()
                .iter()
                .any(|name| name.starts_with("shy") && !name.starts_with("shy-")),
            "{:?}",
            links()
        );
    });
}

#[test]
#[ignore = "needs root"]
fn a_root_filesystem_gets_the_etc_files_it_has() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let root = RootFs::minimal();
        // The minimal root has no /etc: nothing bound, nothing written.
        let bare = networks.run(&[
            "run",
            "--network",
            "bridge",
            "--rootfs",
            root.arg(),
            "sh",
            "-c",
            "ls /etc",
        ]);
        assert_ne!(bare.code, Some(0), "{}", bare.stderr);
        assert!(!root.host_path("/etc").exists());
        // With them: the run's own, and DIR's stay as they were.
        std::fs::create_dir_all(root.host_path("/etc")).unwrap();
        for name in ["hosts", "hostname"] {
            std::fs::write(root.host_path(&format!("/etc/{name}")), "theirs\n").unwrap();
        }
        // A link is left alone: binding on it would make its target in DIR
        // (resolved inside DIR, where this one, absolute, is missing; the
        // host has it).
        std::os::unix::fs::symlink("/etc/passwd", root.host_path("/etc/resolv.conf")).unwrap();
        let run = networks.run(&[
            "run",
            "--network",
            "bridge",
            "--rootfs",
            root.arg(),
            "--add-host",
            "db:10.0.0.5",
            "sh",
            "-c",
            "cat /etc/hosts; echo --; cat /etc/hostname; echo --; cat /etc/resolv.conf 2>/dev/null; true",
        ]);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        let parts: Vec<&str> = run.stdout.split("--\n").collect();
        let hostname = parts[1].trim();
        assert_eq!(hostname.len(), 12, "{}", run.stdout);
        // Its address (the network's next, round robin) under its name.
        assert!(
            parts[0].lines().any(
                |line| line.starts_with("10.88.0.") && line.ends_with(&format!("\t{hostname}"))
            ),
            "{}",
            run.stdout
        );
        assert!(parts[0].contains("10.0.0.5\tdb\n"), "{}", run.stdout);
        // The link's target isn't there: nothing to read.
        assert_eq!(parts[2], "", "{}", run.stdout);
        for name in ["hosts", "hostname"] {
            assert_eq!(
                std::fs::read_to_string(root.host_path(&format!("/etc/{name}"))).unwrap(),
                "theirs\n"
            );
        }
        assert!(
            std::fs::symlink_metadata(root.host_path("/etc/resolv.conf"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!root.host_path("/etc/passwd").exists());
    });
}

#[test]
#[ignore = "needs root"]
fn the_etc_files_follow_dns_and_reach_a_mapped_root() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let root = RootFs::minimal();
        std::fs::create_dir_all(root.host_path("/etc")).unwrap();
        std::fs::write(root.host_path("/etc/hostname"), "theirs\n").unwrap();
        std::fs::write(root.host_path("/etc/resolv.conf"), "theirs\n").unwrap();
        // A regular resolv.conf is the run's, and follows --dns.
        let dns = networks.run(&[
            "run",
            "--network",
            "none",
            "--rootfs",
            root.arg(),
            "--dns",
            "10.0.0.53",
            "sh",
            "-c",
            "cat /etc/resolv.conf",
        ]);
        assert_eq!(dns.code, Some(0), "{}", dns.stderr);
        assert!(
            dns.stdout.contains("nameserver 10.0.0.53\n"),
            "{}",
            dns.stdout
        );
        // A root inside that isn't the host's reads them too: its binds are
        // made with its own credentials, through the run's directories.
        let mapped = networks.run(&[
            "run",
            "--uidmap",
            "0:100000:65536",
            "--network",
            "none",
            "--hostname",
            "mapped",
            "--rootfs",
            root.arg(),
            "sh",
            "-c",
            "cat /etc/hostname",
        ]);
        assert_eq!(mapped.code, Some(0), "{}", mapped.stderr);
        assert_eq!(mapped.stdout, "mapped\n");
        assert_eq!(
            std::fs::read_to_string(root.host_path("/etc/resolv.conf")).unwrap(),
            "theirs\n"
        );
    });
}

/// `shipyard run --bundle` with `config`, the bundle's root filesystem
/// written first.
fn run_bundle(bundle: &Bundle, config: &serde_json::Value) -> Run {
    bundle.write(config);
    run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]))
}

/// A bundle whose command is `sh -c script`, in a network namespace of its
/// own (`network`: an extra field for its `linux.namespaces` entry).
fn network_bundle(script: &str, network: &serde_json::Value) -> (Bundle, serde_json::Value) {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    let mut entry = json!({"type": "network"});
    if let (Some(entry), Some(extra)) = (entry.as_object_mut(), network.as_object()) {
        entry.extend(extra.clone());
    }
    config["linux"]["namespaces"]
        .as_array_mut()
        .unwrap()
        .push(entry);
    (bundle, config)
}

#[test]
#[ignore = "needs root"]
fn a_bundles_network_namespace_has_only_loopback_up() {
    let (bundle, config) =
        network_bundle("ls /sys/class/net; cat /sys/class/net/lo/flags", &json!({}));
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "lo\n0x9\n");
}

#[test]
#[ignore = "needs root"]
fn a_bundle_joins_a_network_namespace_by_its_path() {
    // A thread of the test holds a namespace with a bridge in it, until
    // told to let go.
    let (path_tx, path_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        in_new_netns(move || {
            Rtnetlink::new()
                .unwrap()
                .create_bridge("shy-joined0")
                .unwrap();
            let path = format!(
                "/proc/{}/task/{}/ns/net",
                std::process::id(),
                nix::unistd::gettid()
            );
            path_tx.send(path).unwrap();
            let _ = done_rx.recv();
        });
    });
    let Ok(path) = path_rx.recv() else {
        // The holder failed before it had a namespace: its panic says why.
        holder.join().unwrap();
        unreachable!("the holder sent no path, and didn't panic");
    };
    let (bundle, config) = network_bundle("ls /sys/class/net", &json!({"path": path}));
    let run = run_bundle(&bundle, &config);
    drop(done_tx);
    holder.join().unwrap();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(
        run.stdout.lines().any(|line| line == "shy-joined0"),
        "{}",
        run.stdout
    );
}

#[test]
#[ignore = "needs root"]
fn a_network_path_to_another_kind_of_namespace_is_refused() {
    let path = format!("/proc/{}/ns/uts", std::process::id());
    let (bundle, config) = network_bundle("true", &json!({"path": path}));
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("isn't a network namespace"),
        "{}",
        run.stderr
    );
    // Not a namespace at all (a FIFO there would have hung the run, a
    // device been opened): refused before it's used.
    let (bundle, config) = network_bundle("true", &json!({"path": "/etc/hostname"}));
    let file = run_bundle(&bundle, &config);
    assert_eq!(file.code, Some(125), "{}", file.stderr);
    assert!(
        file.stderr.contains("it isn't a namespace"),
        "{}",
        file.stderr
    );
}

#[test]
#[ignore = "needs root"]
fn the_hosts_devices_move_in_renamed() {
    in_new_netns(|| {
        // A veth pair in this test's own namespace: one end moves in.
        let mut rtnetlink = Rtnetlink::new().unwrap();
        rtnetlink
            .create_veth("shy-nd0", "shy-nd1", None, None)
            .unwrap();
        let (bundle, mut config) = network_bundle("ls /sys/class/net", &json!({}));
        config["linux"]["netDevices"] = json!({"shy-nd0": {"name": "eth5"}});
        let run = run_bundle(&bundle, &config);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        assert_eq!(run.stdout, "eth5\nlo\n");
        // The container's namespace is gone, and a veth with it: its other
        // end too (the kernel does it soon after, probe N8).
        // Teardown is the kernel's, after the namespace's last process: on a
        // loaded machine, it can take a while.
        let gone = (0..1000).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            rtnetlink.link("shy-nd1").unwrap().is_none()
        });
        assert!(gone, "the veth's other end is still there");

        config["linux"]["netDevices"] = json!({"shy-none0": {}});
        let missing = run_bundle(&bundle, &config);
        assert_eq!(missing.code, Some(125), "{}", missing.stderr);
        assert!(
            missing
                .stderr
                .contains("there's no such device on the host"),
            "{}",
            missing.stderr
        );

        // The second device fails (it isn't there; the map is sorted, so
        // it's second): the first is moved back, under its own name.
        rtnetlink
            .create_veth("shy-nd2", "shy-nd3", None, None)
            .unwrap();
        config["linux"]["netDevices"] = json!({"shy-nd2": {"name": "eth7"}, "shy-none1": {}});
        let failed = run_bundle(&bundle, &config);
        assert_eq!(failed.code, Some(125), "{}", failed.stderr);
        assert!(
            rtnetlink.link("shy-nd2").unwrap().is_some(),
            "the first device came back"
        );
    });
}

#[test]
#[ignore = "needs root"]
fn the_hosts_own_network_namespace_gets_no_settings_or_devices() {
    // Joining shipyard's own namespace gives the container nothing of its
    // own: a setting there would be the host's.
    let path = format!("/proc/{}/ns/net", std::process::id());
    let (bundle, mut config) = network_bundle("true", &json!({"path": path}));
    config["linux"]["sysctl"] = json!({"net.ipv4.ip_forward": "1"});
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("is shipyard's own"), "{}", run.stderr);
    // Joining it with nothing to change is allowed.
    config["linux"]["sysctl"] = json!({});
    let plain = run_bundle(&bundle, &config);
    assert_eq!(plain.code, Some(0), "{}", plain.stderr);
}

#[test]
fn moving_a_device_needs_root() {
    if is_root() || !rootless(Rootless::Single) {
        return;
    }
    // Any device of the host's but its loopback.
    let host = interfaces(&std::fs::read_to_string("/proc/net/dev").unwrap());
    let Some(device) = host.into_iter().find(|name| name != "lo") else {
        return;
    };
    let (bundle, mut config) = network_bundle("true", &json!({}));
    config["linux"]["netDevices"] = json!({ device: {} });
    let run = run_bundle(&bundle, &config);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("it needs root"), "{}", run.stderr);
}

/// Root without `CAP_NET_ADMIN` (inside a container started without it,
/// say) can't make the bridge network: the container gets the host's, and
/// the run says so. With it (the control), no such warning.
#[test]
#[ignore = "needs root"]
fn root_without_net_admin_is_told_it_gets_the_hosts_network() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let warned = "the container gets the host's network: the bridge network needs \
                      CAP_NET_ADMIN";
        // The same command, under `setpriv`, without the capability.
        let shipyard = networks.command(&["run", "--no-image", "true"]);
        let mut without = std::process::Command::new("setpriv");
        without
            .args([
                "--bounding-set",
                "-net_admin",
                "--inh-caps",
                "-net_admin",
                "--",
            ])
            .arg(shipyard.get_program())
            .args(shipyard.get_args())
            .envs(
                shipyard
                    .get_envs()
                    .filter_map(|(key, value)| Some((key, value?))),
            );
        let run = run(without);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        assert!(
            run.stderr.contains(warned) && run.stderr.contains("shipyard has no CAP_NET_ADMIN"),
            "{}",
            run.stderr
        );
        let with = networks.run(&["run", "--no-image", "true"]);
        assert_eq!(with.code, Some(0), "{}", with.stderr);
        assert!(!with.stderr.contains(warned), "{}", with.stderr);
    });
}
