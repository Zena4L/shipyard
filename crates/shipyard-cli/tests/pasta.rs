//! End-to-end tests of rootless networking with pasta (Linux).
//!
//! The tests that need a world to reach run as root, each in a network
//! namespace of its own (`in_new_netns`, "the host") with an [`Upstream`]
//! that's its way out, and start `shipyard` as an unprivileged user
//! ([`TestUser`]): a rootless container, with a network the test controls.
//! A server "in the container" is a listener on a thread joined to its
//! network namespace (root may join a rootless container's). The rest run
//! as any user that can isolate.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::fs::File;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};

use shipyard_test_support::net::{Server, TestUser, Upstream, container_netns, fetch, fetch_from};
use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{
    Rootless, Running, Shipyard, in_new_netns, is_root, rootless, run, start_command,
};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

fn at(ip: Ipv4Addr, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(ip, port))
}

/// A rootless run, started by root as `user`: `shipyard run FLAGS… sleep
/// 30`, its stderr in `runtime/stderr`, and its container's network
/// namespace once it's connected.
fn sleeping(user: &TestUser, runtime: &Path, flags: &[&str]) -> (Running, File) {
    let mut command = user.command(
        SHIPYARD.command(&[&["run"], flags, &["sleep", "30"]].concat()),
        runtime,
    );
    command.stderr(File::create(runtime.join("stderr")).unwrap());
    let running = start_command(command);
    let netns = container_netns(running.pid());
    (running, netns)
}

/// A scratch runtime directory, removed on drop (a failed test's too).
struct Runtime(PathBuf);

impl std::ops::Deref for Runtime {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn runtime() -> Runtime {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    Runtime(std::env::temp_dir().join(format!(
        "shipyard-pasta-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    )))
}

/// The world, as root sees it, and a user to be rootless as; `None` (the
/// test skips) without one.
fn world() -> Option<(TestUser, Upstream)> {
    let Some(user) = TestUser::find() else {
        eprintln!("no unprivileged user to run rootless containers as; skipping");
        return None;
    };
    let upstream = Upstream::new();
    upstream.route_default();
    Some((user, upstream))
}

#[test]
#[ignore = "needs root"]
fn a_rootless_container_has_a_way_out_through_pasta() {
    in_new_netns(|| {
        let Some((user, upstream)) = world() else {
            return;
        };
        let server = Server::start(upstream.netns(), 7000, "out");
        let runtime = runtime();
        // No --network: rootless's default.
        let (running, netns) = sleeping(&user, &runtime, &[]);
        assert_eq!(
            fetch_from(&netns, at(Upstream::ADDRESS, 7000)).as_deref(),
            Some("out")
        );
        // pasta made the connection again from the host: the host's address.
        assert_eq!(
            server.peer().map(|peer| peer.ip()),
            Some(IpAddr::V4(Upstream::HOST))
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_rootless_container_publishes_ports_through_pasta() {
    in_new_netns(|| {
        let Some((user, upstream)) = world() else {
            return;
        };
        let runtime = runtime();
        let (running, netns) = sleeping(
            &user,
            &runtime,
            &["-p", "8080:80", "-p", "127.0.0.1:8081:81", "-p", "82"],
        );
        let _eighty = Server::start(&netns, 80, "eighty");
        let _local = Server::start(&netns, 81, "local");
        let _random = Server::start(&netns, 82, "random");
        assert_eq!(
            fetch_from(upstream.netns(), at(Upstream::HOST, 8080)).as_deref(),
            Some("eighty")
        );
        assert_eq!(
            fetch(at(Ipv4Addr::LOCALHOST, 8081)).as_deref(),
            Some("local")
        );
        assert_eq!(fetch_from(upstream.netns(), at(Upstream::HOST, 8081)), None);
        // The free port is printed: "shipyard: published 0.0.0.0:N -> 82/tcp".
        let stderr = std::fs::read_to_string(runtime.join("stderr")).unwrap();
        let port: u16 = stderr
            .lines()
            .find_map(|line| line.strip_prefix("shipyard: published 0.0.0.0:"))
            .and_then(|rest| rest.strip_suffix(" -> 82/tcp"))
            .unwrap_or_else(|| panic!("{stderr}"))
            .parse()
            .unwrap();
        assert_eq!(
            fetch_from(upstream.netns(), at(Upstream::HOST, port)).as_deref(),
            Some("random")
        );
        // Ports it was told aren't printed.
        assert!(!stderr.contains("-> 80/tcp"), "{stderr}");
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_rootless_container_has_ipv6_through_pasta_when_the_host_has() {
    in_new_netns(|| {
        let Some((user, upstream)) = world() else {
            return;
        };
        let server = Server::start(upstream.netns(), 7000, "out");
        let runtime = runtime();
        let (running, netns) = sleeping(&user, &runtime, &["-p", "8080:80"]);
        let at6 = |ip, port| SocketAddr::new(IpAddr::V6(ip), port);
        assert_eq!(
            fetch_from(&netns, at6(Upstream::ADDRESS6, 7000)).as_deref(),
            Some("out")
        );
        assert_eq!(
            server.peer().map(|peer| peer.ip()),
            Some(IpAddr::V6(Upstream::HOST6))
        );
        // Its published port answers on IPv6 too.
        let _eighty = Server::start(&netns, 80, "eighty");
        assert_eq!(
            fetch_from(upstream.netns(), at6(Upstream::HOST6, 8080)).as_deref(),
            Some("eighty")
        );
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn a_rootless_container_never_reaches_the_hosts_loopback() {
    in_new_netns(|| {
        let Some((user, _upstream)) = world() else {
            return;
        };
        // A service only the host itself should reach.
        let host_only = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = host_only.local_addr().unwrap().port();
        let runtime = runtime();
        let (running, netns) = sleeping(&user, &runtime, &[]);
        // Not through the gateway's address (pasta's default maps it to the
        // host's loopback), nor through its own (pasta's default forwards
        // the host's ports in).
        assert_eq!(fetch_from(&netns, at(Upstream::ADDRESS, port)), None);
        assert_eq!(fetch_from(&netns, at(Ipv4Addr::LOCALHOST, port)), None);
        running.stop();
    });
}

#[test]
#[ignore = "needs root"]
fn pasta_goes_with_a_killed_cli() {
    in_new_netns(|| {
        let Some((user, _upstream)) = world() else {
            return;
        };
        let runtime = runtime();
        let (running, _netns) = sleeping(&user, &runtime, &[]);
        let runs = runtime.join("shipyard/runs");
        let pid_file = std::fs::read_dir(&runs)
            .unwrap()
            .flatten()
            .map(|entry| entry.path().join("pasta.pid"))
            .find(|path| path.exists())
            .unwrap_or_else(|| panic!("no pasta.pid in {}", runs.display()));
        let pasta: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(Path::new(&format!("/proc/{pasta}")).exists());
        // Out of the terminal's reach: a process group of its own, not
        // shipyard's (whose Ctrl-C is the container's to handle).
        let group = |pid: i32| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            // After the command's name (in parentheses): state, ppid, pgrp.
            let after = &stat[stat.rfind(')').unwrap() + 2..];
            after.split(' ').nth(2).unwrap().to_owned()
        };
        assert_ne!(group(pasta), group(running.pid()));
        running.signal(libc::SIGKILL);
        let _ = running.finish_status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while Path::new(&format!("/proc/{pasta}")).exists()
            && std::fs::read_to_string(format!("/proc/{pasta}/comm"))
                .is_ok_and(|comm| comm.trim() == "pasta" || comm.trim() == "passt")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "pasta outlived shipyard"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}

#[test]
#[ignore = "needs root"]
fn a_rootless_containers_resolver_is_pastas() {
    in_new_netns(|| {
        let Some((user, _upstream)) = world() else {
            return;
        };
        let root = RootFs::minimal();
        std::fs::create_dir_all(root.host_path("/etc")).unwrap();
        std::fs::write(root.host_path("/etc/resolv.conf"), "theirs\n").unwrap();
        // The user's own, as a rootless container's root must be.
        let owned = std::process::Command::new("chown")
            .args(["-R", &format!("{}:{}", user.uid, user.gid), root.arg()])
            .status()
            .unwrap();
        assert!(owned.success());
        let runtime = runtime();
        let resolv = run(user.command(
            SHIPYARD.command(&["run", "--rootfs", root.arg(), "cat", "/etc/resolv.conf"]),
            &runtime,
        ));
        assert_eq!(resolv.code, Some(0), "{}", resolv.stderr);
        assert!(
            resolv.stdout.contains("nameserver 169.254.1.1\n"),
            "{}",
            resolv.stdout
        );
    });
}

#[test]
fn without_pasta_a_rootless_run_says_how_to_do_without() {
    if is_root() || !rootless(Rootless::Single) {
        return;
    }
    let without = |flags: &[&str]| {
        let mut command = SHIPYARD.command(&[&["run"], flags, &["true"]].concat());
        command.env("PATH", "/nonexistent");
        run(command)
    };
    let refused = without(&[]);
    assert_eq!(refused.code, Some(125), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("install the passt package"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("--network host"),
        "{}",
        refused.stderr
    );
    // Without a network of its own, it doesn't need it.
    let host = without(&["--network", "host"]);
    assert_eq!(host.code, Some(0), "{}", host.stderr);
}

#[test]
fn a_rootless_containers_network_is_its_own_through_pasta() {
    if is_root() || !rootless(Rootless::Single) {
        return;
    }
    let listed = run(SHIPYARD.command(&["run", "cat", "/proc/net/dev"]));
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let interfaces: Vec<&str> = listed
        .stdout
        .lines()
        .skip(2)
        .filter_map(|line| Some(line.split_once(':')?.0.trim()))
        .collect();
    assert!(interfaces.contains(&"lo"), "{interfaces:?}");
    assert_eq!(interfaces.len(), 2, "lo and pasta's: {interfaces:?}");
}

/// pasta goes with the run: the run ends at once, not after a wait for a
/// pasta the supervisor reaped already (the second it once took).
#[test]
#[ignore = "needs root"]
fn a_rootless_run_ends_as_soon_as_its_command_does() {
    in_new_netns(|| {
        let Some((user, _upstream)) = world() else {
            return;
        };
        let runtime = runtime();
        let took = |network: &str| {
            let started = std::time::Instant::now();
            let ran = run(user.command(
                SHIPYARD.command(&["run", "--network", network, "true"]),
                &runtime,
            ));
            assert_eq!(ran.code, Some(0), "{}", ran.stderr);
            started.elapsed()
        };
        let without = took("none");
        let with = took("pasta");
        assert!(
            with < without + std::time::Duration::from_millis(600),
            "with pasta {with:?}, without {without:?}"
        );
    });
}
