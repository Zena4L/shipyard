//! End-to-end containers by name (stage 9, step 9.4a): `create`, `start`,
//! `run -d`, `ps`, `inspect` and `rm`, through the real `shipyard` binary,
//! with an image from the fixture registry.
//!
//! Each test has stores of its own (images, containers, networks and, for a
//! normal user, its runtime directory), and its containers have no network
//! (the engine's own tests cover networks). Linux only; a host where an
//! image can't run (rootless, without overlay) says why and returns.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use shipyard_test_support::registry::{Scratch, Serving, fixture, image_runs_here};
use shipyard_test_support::{Shipyard, is_root};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// One test's stores and fixture image.
struct Stores {
    scratch: Scratch,
    image: String,
    _registry: Serving,
}

impl Stores {
    /// Its stores, if an image runs here (else it says why).
    fn new() -> Option<Self> {
        let scratch = Scratch::new();
        for dir in ["containers", "networks", "xdg"] {
            std::fs::create_dir_all(scratch.0.join(dir)).unwrap();
        }
        for dir in ["containers", "xdg"] {
            std::fs::set_permissions(scratch.0.join(dir), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let (registry, image) = fixture();
        let stores = Self {
            scratch,
            image,
            _registry: registry,
        };
        let probe = stores.shipyard(&[
            "run",
            "--rm",
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            "exit 0",
        ]);
        image_runs_here(&probe).then_some(stores)
    }

    /// `shipyard ARGS`, on these stores.
    fn shipyard(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running shipyard")
    }

    /// `shipyard ARGS`, which must succeed: its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.shipyard(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// `shipyard ARGS`, not yet run, on these stores.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(SHIPYARD.path());
        command
            .args(args)
            .env("SHIPYARD_IMAGE_STORE", self.scratch.store())
            .env("SHIPYARD_CONTAINER_DIR", self.scratch.0.join("containers"))
            .env("SHIPYARD_NETWORK_DIR", self.scratch.0.join("networks"));
        if !is_root() {
            command.env("XDG_RUNTIME_DIR", self.scratch.0.join("xdg"));
        }
        command
    }

    /// `logs -f REFERENCE`, running in the background: it, and the next
    /// line it prints (waited for, up to 20 s).
    fn follow(&self, reference: &str) -> (std::process::Child, impl Fn() -> String) {
        use std::io::BufRead as _;
        let mut follow = self
            .command(&["logs", "-f", reference])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let followed = std::io::BufReader::new(follow.stdout.take().unwrap());
        let (lines, read) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in followed.lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let next_line = move || read.recv_timeout(Duration::from_secs(20)).unwrap();
        (follow, next_line)
    }

    /// `attach REFERENCE`, typing `typed`, then (once `answered` returns)
    /// the detach keys: what it printed, and how it ended.
    fn attach_and_type(&self, reference: &str, typed: &[u8], answered: impl FnOnce()) -> Output {
        use std::io::Write as _;
        use std::process::Stdio;
        let mut attach = self
            .command(&["attach", reference])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = attach.stdin.take().unwrap();
        input.write_all(typed).unwrap();
        answered();
        input.write_all(b"\x10\x11").unwrap();
        drop(input);
        attach.wait_with_output().unwrap()
    }

    /// `inspect REFERENCE`'s one object.
    fn inspect(&self, reference: &str) -> serde_json::Value {
        let shown: serde_json::Value =
            serde_json::from_str(&self.ok(&["inspect", reference])).unwrap();
        shown[0].clone()
    }
}

/// Waits (up to 20 s) for `found`, naming `what` if it never comes.
fn eventually(what: &str, mut found: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !found() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A container is made, started, listed, looked at and removed by its name,
/// its serial and its ID's prefix, each finding the same one; a running one
/// isn't removed without `-f`.
#[test]
fn a_container_is_found_by_name_serial_and_prefix() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "create",
            "--name",
            "web",
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    assert_eq!(id.len(), 64, "the full ID: {id}");
    let listed = stores.ok(&["ps", "-a"]);
    assert!(
        listed.contains("Created") && listed.contains("web"),
        "{listed}"
    );
    assert!(
        !stores.ok(&["ps"]).contains("web"),
        "not up: not without -a"
    );
    let shown = stores.inspect("web");
    assert_eq!(shown["id"], id.as_str());
    assert_eq!(shown["state"]["status"], "created");
    let serial = shown["serial"].as_str().unwrap().to_owned();
    assert_eq!(stores.inspect(&serial)["id"], id.as_str(), "by serial");
    assert_eq!(stores.inspect(&id[..8])["name"], "web", "by prefix");
    assert_eq!(
        stores.ok(&["start", &id[..8]]).trim(),
        &id[..8],
        "said as given"
    );
    let up: serde_json::Value =
        serde_json::from_str(&stores.ok(&["ps", "--format", "json"])).unwrap();
    assert_eq!(up[0]["state"], "running", "{up}");
    assert_eq!(stores.ok(&["ps", "-q"]).trim(), &id[..12]);
    let refused = stores.shipyard(&["rm", "web"]);
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is running"),
        "{refused:?}"
    );
    stores.ok(&["rm", "-f", &serial]);
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "");
    let gone = stores.shipyard(&["inspect", "web"]);
    assert_eq!(gone.status.code(), Some(125), "{gone:?}");
    assert_eq!(String::from_utf8_lossy(&gone.stdout).trim(), "[]");
}

/// `run -d` prints the full ID; the container's end shows in `ps -a`;
/// `rm` of several goes on past one that isn't there, and says so.
#[test]
fn run_d_prints_the_id_and_rm_goes_on() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "run",
            "-d",
            "--name",
            "once",
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            "exit 7",
        ])
        .trim()
        .to_owned();
    assert!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    eventually("Exited (7)", || {
        stores.ok(&["ps", "-a"]).contains("Exited (7)")
    });
    let removed = stores.shipyard(&["rm", "nothing-here", "once"]);
    assert_eq!(removed.status.code(), Some(125), "{removed:?}");
    assert_eq!(String::from_utf8_lossy(&removed.stdout).trim(), "once");
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "");
}

/// `rmi` refuses the image a container was made from, naming it; once the
/// container is removed, it goes.
#[test]
fn rmi_refuses_a_containers_image() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores
        .ok(&[
            "create",
            "--network",
            "none",
            &stores.image,
            "sh",
            "-c",
            "exit 0",
        ])
        .trim()
        .to_owned();
    let refused = stores.shipyard(&["rmi", &stores.image]);
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&id[..12]),
        "{refused:?}"
    );
    stores.ok(&["rm", &id]);
    stores.ok(&["rmi", &stores.image]);
}

/// `create` and `run -d` refuse what only a foreground run can have.
#[test]
fn containers_refuse_what_only_a_foreground_run_has() {
    let Some(stores) = Stores::new() else {
        return;
    };
    // A real bundle: refused for having no record, not for being missing.
    let bundle = shipyard_test_support::bundle::Bundle::new();
    for (args, why) in [
        (
            &["create", "--no-isolation", "true"][..],
            "a container needs isolation",
        ),
        (
            &["create", "--no-image", "sh"],
            "a container needs a root filesystem of its own",
        ),
        (
            &["run", "-d", "--bundle", bundle.arg()],
            "a container from a bundle has no record",
        ),
        (
            &["run", "-d", "--preserve-fds", "1", &stores.image],
            "only a foreground run can",
        ),
    ] {
        let refused = stores.shipyard(args);
        assert_eq!(refused.status.code(), Some(125), "{args:?}: {refused:?}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(why), "{args:?}: {stderr}");
    }
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "nothing made");
}

/// `stop`, `restart`, `kill` and `wait` by name: each prints the reference
/// (`wait`, the exit code); `kill` of one not running exits 125; several
/// references go on past one that fails.
#[test]
fn stop_restart_kill_and_wait_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert_eq!(stores.ok(&["stop", "-t", "5", "web"]).trim(), "web");
    assert_eq!(stores.ok(&["wait", "web"]).trim(), "143");
    let not_running = stores.shipyard(&["kill", "web"]);
    assert_eq!(not_running.status.code(), Some(125), "{not_running:?}");
    assert!(
        String::from_utf8_lossy(&not_running.stderr).contains("is not running"),
        "{not_running:?}"
    );
    assert_eq!(stores.ok(&["restart", "web"]).trim(), "web");
    assert!(stores.ok(&["ps"]).contains("web"), "up again");
    assert_eq!(stores.ok(&["kill", "-s", "TERM", "web"]).trim(), "web");
    assert_eq!(stores.ok(&["wait", "web"]).trim(), "143");
    let several = stores.shipyard(&["stop", "nothing-here", "web"]);
    assert_eq!(several.status.code(), Some(125), "{several:?}");
    assert_eq!(String::from_utf8_lossy(&several.stdout).trim(), "web");
    stores.ok(&["rm", "web"]);
}

/// `port` shows what a running container publishes, by name: every
/// binding (each address family on a network with IPv6), or one port's host
/// side; nothing published there exits 1, as Docker's; a stopped container
/// publishes nothing. `ps` shows both families too, and `network inspect`
/// names the container by its ID. As root, in a network namespace of the
/// test's own.
#[test]
fn port_shows_what_a_container_publishes() {
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        // The namespace's loopback, up (a host's is): `127.0.0.1` can be
        // bound.
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        let id = stores
            .ok(&[
                "run",
                "-d",
                "--name",
                "web",
                "--network",
                "app",
                "-p",
                "80",
                "-p",
                "127.0.0.1::53/udp",
                &stores.image,
                "sleep",
                "300",
            ])
            .trim()
            .to_owned();
        // In Docker's order: by the container's port.
        let all = stores.ok(&["port", "web"]);
        let lines: Vec<&str> = all.lines().collect();
        assert!(lines[0].starts_with("53/udp -> 127.0.0.1:"), "{all}");
        let host_port = lines[1]
            .strip_prefix("80/tcp -> 0.0.0.0:")
            .unwrap_or_else(|| panic!("{all}"));
        // Its IPv6 side, where the kernel has IPv6.
        let ipv6 = std::net::TcpListener::bind("[::]:0").is_ok();
        if ipv6 {
            assert_eq!(lines.len(), 3, "{all}");
            assert_eq!(lines[2], format!("80/tcp -> [::]:{host_port}"), "{all}");
            assert_eq!(
                stores.ok(&["port", "web", "80"]),
                format!("0.0.0.0:{host_port}\n[::]:{host_port}\n")
            );
        } else {
            assert_eq!(lines.len(), 2, "{all}");
        }
        // The same number over UDP is another port: none published.
        let udp = stores.shipyard(&["port", "web", "80/udp"]);
        assert_eq!(udp.status.code(), Some(1), "{udp:?}");
        let none = stores.shipyard(&["port", "web", "81"]);
        assert_eq!(none.status.code(), Some(1), "{none:?}");
        assert_eq!(
            String::from_utf8_lossy(&none.stderr).trim(),
            "shipyard: no public port '81/tcp' published for web"
        );
        let listed = stores.ok(&["ps"]);
        assert!(
            listed.contains(&format!("0.0.0.0:{host_port}->80/tcp")),
            "{listed}"
        );
        if ipv6 {
            assert!(
                listed.contains(&format!(
                    "0.0.0.0:{host_port}->80/tcp, [::]:{host_port}->80/tcp"
                )),
                "{listed}"
            );
        }
        let networks: serde_json::Value =
            serde_json::from_str(&stores.ok(&["network", "inspect", "app"])).unwrap();
        assert_eq!(networks[0]["Containers"][0]["Container"], id.as_str());
        assert_eq!(networks[0]["Containers"][0]["Name"], "web");
        stores.ok(&["stop", "-t", "1", "web"]);
        assert_eq!(stores.ok(&["port", "web"]), "");
        let stopped = stores.shipyard(&["port", "web", "80"]);
        assert_eq!(stopped.status.code(), Some(1), "{stopped:?}");
        assert_eq!(
            String::from_utf8_lossy(&stopped.stderr).trim(),
            "shipyard: no public port '80/tcp' published for web"
        );
        stores.ok(&["rm", "web"]);
    });
}

/// A health check by its flags: `ps` says `(healthy)`, `ps --format json`
/// and `inspect` say it too.
#[test]
fn health_shows_in_ps_and_inspect() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = stores.ok(&[
        "run",
        "-d",
        "--name",
        "checked",
        "--network",
        "none",
        "--health-cmd",
        "true",
        "--health-interval",
        "300ms",
        &stores.image,
        "sleep",
        "300",
    ]);
    assert!(!id.trim().is_empty());
    eventually("(healthy) in ps", || {
        stores
            .ok(&["ps"])
            .lines()
            .any(|line| line.contains("(healthy)"))
    });
    let listed: serde_json::Value =
        serde_json::from_str(&stores.ok(&["ps", "--format", "json"])).unwrap();
    assert_eq!(listed[0]["health"], "healthy", "{listed}");
    let shown = stores.inspect("checked");
    assert_eq!(shown["state"]["health"]["status"], "healthy", "{shown}");
    assert_eq!(
        shown["state"]["health"]["log"][0]["exit_code"], 0,
        "{shown}"
    );
    assert_eq!(shown["config"]["health"]["interval_ms"], 300, "{shown}");
    stores.ok(&["rm", "-f", "checked"]);
}

/// `--no-healthcheck` with a `--health-*` flag is refused; tuning flags
/// alone tune the image's check, and with an image without one, they're
/// said to be ignored.
#[test]
fn health_flags_and_no_healthcheck() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let refused = stores.shipyard(&[
        "run",
        "--no-healthcheck",
        "--health-cmd",
        "true",
        &stores.image,
        "true",
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--no-healthcheck"),
        "{refused:?}"
    );
    let tuned = stores.shipyard(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--health-interval",
        "1s",
        &stores.image,
        "sh",
        "-c",
        "exit 0",
    ]);
    assert!(tuned.status.success(), "{tuned:?}");
    assert!(
        String::from_utf8_lossy(&tuned.stderr).contains("there's no health check to tune"),
        "{tuned:?}"
    );
    // `--no-healthcheck` alone: nothing to say.
    let off = stores.shipyard(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--no-healthcheck",
        &stores.image,
        "sh",
        "-c",
        "exit 0",
    ]);
    assert!(off.status.success(), "{off:?}");
    assert!(
        !String::from_utf8_lossy(&off.stderr).contains("health"),
        "{off:?}"
    );
}

/// `rename`: the record renamed (found by its new name, not its old one),
/// nothing printed; another container's name, its own name, or a name that
/// can't be one: refused (125), nothing changed.
#[test]
fn rename_gives_a_container_a_new_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    for name in ["web", "db"] {
        stores.ok(&[
            "create",
            "--name",
            name,
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ]);
    }
    assert_eq!(stores.ok(&["rename", "web", "site"]), "");
    assert_eq!(stores.inspect("site")["name"], "site");
    assert_eq!(
        stores.shipyard(&["inspect", "web"]).status.code(),
        Some(125)
    );
    for (new, why) in [
        ("db", "is taken"),
        ("site", "is named site already"),
        ("no/slash", "can't be a container's name"),
        ("my.site", "can't be a container's name"),
    ] {
        let output = stores.shipyard(&["rename", "site", new]);
        assert_eq!(output.status.code(), Some(125), "{new}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(why),
            "{new}: {output:?}"
        );
    }
    assert_eq!(stores.inspect("site")["name"], "site");
    stores.ok(&["rm", "site", "db"]);
}

/// `shipyard logs` ends as its readers do (F70): 125 when stderr's reader
/// goes while stdout's is read.
#[test]
fn logs_end_as_their_readers_do_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "both",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "n=0; while [ $n -lt 20000 ]; do echo out $n; echo err $n >&2; n=$((n+1)); done",
    ]);
    assert_eq!(stores.ok(&["wait", "both"]).trim(), "0");
    shipyard_test_support::pipes::logs_end_as_their_readers_do(
        || stores.command(&["logs", "both"]),
        125,
    );
}

/// `logs`, `exec`, `cp` and `attach` by name: a stopped container's logs,
/// and its files copied out and in; exec's exit codes (the command's, 127
/// for one that isn't there); a running container's file copied out;
/// attaching to a container that isn't up exits 125.
#[test]
fn logs_exec_cp_and_attach_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "once",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo said-once",
    ]);
    assert_eq!(stores.ok(&["wait", "once"]).trim(), "0");
    assert_eq!(stores.ok(&["logs", "once"]), "said-once\n");
    let attach = stores.shipyard(&["attach", "once"]);
    assert_eq!(attach.status.code(), Some(125), "{attach:?}");
    // A stopped container's files: out, and in (its next start sees them).
    let marker = stores.scratch.0.join("stopped-marker");
    stores.ok(&["cp", "once:/marker", marker.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "shipyard test root filesystem\n"
    );
    let missing = stores.shipyard(&["cp", "once:/no/such", marker.to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(125), "{missing:?}");
    stores.ok(&["cp", marker.to_str().unwrap(), "once:/copied"]);
    let back = stores.scratch.0.join("copied-back");
    stores.ok(&["cp", "once:/copied", back.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(&back).unwrap(),
        "shipyard test root filesystem\n"
    );
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        &stores.image,
        "sleep",
        "300",
    ]);
    let ran = stores.shipyard(&["exec", "web", "sh", "-c", "exit 5"]);
    assert_eq!(ran.status.code(), Some(5), "{ran:?}");
    let missing = stores.shipyard(&["exec", "web", "/no/such/command"]);
    assert_eq!(missing.status.code(), Some(127), "{missing:?}");
    let copied = stores.scratch.0.join("marker-copy");
    stores.ok(&["cp", "web:/marker", copied.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "shipyard test root filesystem\n"
    );
    stores.ok(&["rm", "-f", "web", "once"]);
}

/// `logs -f` ends, quietly, once its reader has gone (`logs -f web | head
/// -n1`), though the container runs on and writes nothing more: between
/// its looks it asks whether stdout's reader is there (F78).
#[test]
fn logs_follow_ends_when_its_reader_goes() {
    use std::io::BufRead as _;
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "quiet",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo first; exec sleep 300",
    ]);
    let mut follow = stores
        .command(&["logs", "-f", "quiet"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Its first line read (the control: it follows), then its reader gone.
    let mut stdout = std::io::BufReader::new(follow.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "first\n");
    drop(stdout);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = follow.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = follow.kill();
            panic!("it followed with no reader");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(follow.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(status.success(), "{status:?}: {stderr}");
    assert_eq!(stderr, "");
    // It ended for its reader, not its container's end.
    assert!(stores.ok(&["ps"]).contains("Up"));
    stores.ok(&["rm", "-f", "quiet"]);
}

/// Stage 9's milestone (v0.2), through the `shipyard` binary, on one
/// container by its name: `run -d` returns while the container runs on;
/// `logs -f` follows it; `exec`, `attach` (typing reaches it, the keys
/// detach) and `cp` reach it; its process killed from outside, its restart
/// policy starts it again (`die` then `start` in the events, its logs
/// showing both starts); `system recover` leaves a running one as it is;
/// `stop` and `rm` leave nothing. Rootless or root, as the tests run. (A
/// reboot is the engine's tests' and the stage's manual check's: here the
/// runtime's state would have to be faked behind its back.)
#[test]
fn the_v0_2_milestone_a_named_container_end_to_end() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let started = Instant::now();
    stores.ok(&[
        "run",
        "-d",
        "-i",
        "--name",
        "web",
        "--restart",
        "on-failure:3",
        "--network",
        "none",
        &stores.image,
        "sh",
        "-c",
        "echo ready; while read line; do echo got-$line; done; sleep 300",
    ]);
    // `run -d` has returned: the CLI is gone, the container runs on.
    assert!(started.elapsed() < Duration::from_secs(20));
    eventually("web up", || stores.ok(&["ps"]).contains("Up"));
    let (mut follow, next_line) = stores.follow("web");
    assert_eq!(next_line(), "ready");
    // exec: a command beside it, its exit code its own.
    let ran = stores.shipyard(&["exec", "web", "sh", "-c", "echo $((40 + 2)); exit 4"]);
    assert_eq!(ran.status.code(), Some(4), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "42\n");
    // attach: what's typed reaches it; Ctrl-P Ctrl-Q leaves it running.
    let attached = stores.attach_and_type("web", b"one\n", || {
        assert_eq!(next_line(), "got-one");
    });
    assert_eq!(attached.status.code(), Some(0), "{attached:?}");
    assert!(
        String::from_utf8_lossy(&attached.stdout).contains("got-one"),
        "{attached:?}"
    );
    // cp: in, then back out, the same.
    let file = stores.scratch.0.join("milestone-in");
    std::fs::write(&file, "carried\n").unwrap();
    stores.ok(&["cp", file.to_str().unwrap(), "web:/carried"]);
    let back = stores.scratch.0.join("milestone-out");
    stores.ok(&["cp", "web:/carried", back.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&back).unwrap(), "carried\n");
    // Its process killed from outside: the policy starts it again.
    let pid = stores.inspect("web")["state"]["pid"].as_i64().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    // The follow ends with the start it followed.
    assert!(follow.wait().unwrap().success());
    eventually("restarted once", || {
        let shown = stores.inspect("web");
        shown["restart_count"] == 1 && shown["state"]["running"] == true
    });
    // As Docker's: a restart by its policy is a `die`, then a `start`.
    let events = stores.ok(&["events", "--since", "1h", "--until", "0s"]);
    let actions: Vec<&str> = events
        .lines()
        .filter(|line| line.contains("container") && line.contains("name=web"))
        .filter_map(|line| line.split(' ').nth(2))
        .collect();
    let died = actions.iter().position(|&action| action == "die");
    assert!(
        died.is_some_and(|died| actions[died..].contains(&"start")),
        "{events}"
    );
    eventually("both starts in its logs", || {
        stores.ok(&["logs", "web"]).matches("ready").count() == 2
    });
    // Recovery leaves a running container as it is.
    stores.ok(&["system", "recover"]);
    assert_eq!(stores.inspect("web")["restart_count"], 1);
    assert!(stores.ok(&["ps"]).contains("Up"));
    // Stopped by its user, it stays stopped; removed, nothing is left.
    stores.ok(&["stop", "-t", "1", "web"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(stores.inspect("web")["state"]["running"], false);
    stores.ok(&["rm", "web"]);
    assert!(
        stores.ok(&["ps", "-a"]).lines().count() <= 1,
        "nothing left"
    );
    let records = std::fs::read_dir(stores.scratch.0.join("containers"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("record.json").exists())
        .count();
    assert_eq!(records, 0);
}

/// A container recorded by an earlier Shipyard (a version 1 record, from
/// 9.2a, before images' IDs, ports and `--rm` were kept) is listed, looked
/// at, recovered around and removed by today's: records are read in every
/// version they were written in (H2).
#[test]
fn a_container_an_earlier_version_recorded_is_managed() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let dir = stores.scratch.0.join("containers").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("record.json"),
        format!(
            r#"{{
        "version": 1,
        "id": "{id}",
        "serial": 304812,
        "name": "brave_tugboat",
        "created": {{"secs_since_epoch": 1760000000, "nanos_since_epoch": 0}},
        "config": {{
            "interactive": false, "tty": false, "env": ["A=1"], "workdir": null,
            "user": null, "group_add": [], "ulimit": [], "oom_score_adj": null,
            "device": [], "device_cgroup_rule": [], "stop_signal": "SIGTERM",
            "stop_timeout": 10, "no_isolation": false, "hostname": null,
            "domainname": null, "sysctl": [], "network": ["none"],
            "network_alias": [], "ip": null, "ip6": null, "mac_address": null,
            "add_host": [], "dns": [], "dns_search": [], "dns_option": [],
            "publish": [], "publish_all": false, "rootfs": null, "image": "{image}",
            "entrypoint": null, "pull": "missing", "read_only": false,
            "cap_add": [], "cap_drop": [], "security_opt": [],
            "uidmap": [], "gidmap": [], "bundle": null,
            "resources": {{"swap_is_default": false, "cpu_max": false, "io_max": [],
                          "unified": {{}}}},
            "volume": [], "command": ["sh", "-c", "exit 0"]
        }},
        "labels": {{"tier": "web"}},
        "started": null,
        "finished": null,
        "exit": null
    }}"#,
            image = stores.image
        ),
    )
    .unwrap();
    let listed = stores.ok(&["ps", "-a"]);
    assert!(listed.contains("brave_tugboat"), "{listed}");
    let shown = stores.inspect("brave_tugboat");
    assert_eq!(shown["labels"]["tier"], "web");
    assert_eq!(shown["state"]["status"], "created");
    stores.ok(&["system", "recover"]);
    // Started by today's Shipyard, from what it recorded then.
    stores.ok(&["start", "brave_tugboat"]);
    assert_eq!(stores.ok(&["wait", "brave_tugboat"]).trim(), "0");
    stores.ok(&["rm", "brave_tugboat"]);
    assert!(!dir.exists(), "its record removed");
}

/// A container stage 8's `shipyard-runtime` made, still running when today's
/// Shipyard is installed (stage 9, step 9.9a, H2). Stage 8 had no records,
/// and its containers share today's state root: `ps`, `system recover` and
/// (rootless: root's state root is shared with other tests) `system stop`
/// leave it alone, and today's `shipyard-runtime` still reads its state
/// (`state.json` without a version), its end (a line of text) and deletes
/// it. Its monitor and init are stand-ins, as stage 8 recorded them; a real
/// stage 8 monitor was checked by hand (the stage notes).
#[test]
fn a_container_stage_8s_runtime_made_is_left_to_it() {
    const RUNTIME: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard-runtime"));
    let Some(stores) = Stores::new() else {
        return;
    };
    let root = if is_root() {
        std::path::PathBuf::from("/run/shipyard")
    } else {
        stores.scratch.0.join("xdg/shipyard")
    };
    let start_time = |pid: u32| -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after_name = stat.rsplit(')').next().unwrap();
        after_name
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap()
    };
    let mut monitor = Command::new("sleep").arg("300").spawn().unwrap();
    let mut init = Command::new("sleep").arg("300").spawn().unwrap();
    let id = format!("stage8-{}", std::process::id());
    let dir = root.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // As stage 8 wrote it: no version.
    let record = serde_json::json!({
        "bundle": "/nonexistent",
        "monitor": [monitor.id(), start_time(monitor.id())],
        "init": [init.id(), start_time(init.id())],
        "annotations": {},
    });
    std::fs::write(dir.join("state.json"), record.to_string()).unwrap();
    let runtime = |args: &[&str]| -> Output {
        Command::new(RUNTIME.path())
            .arg("--root")
            .arg(&root)
            .args(args)
            .output()
            .unwrap()
    };
    let status = || -> String {
        let state = runtime(&["state", &id]);
        assert!(state.status.success(), "{state:?}");
        let state: serde_json::Value = serde_json::from_slice(&state.stdout).unwrap();
        state["status"].as_str().unwrap().to_owned()
    };
    assert_eq!(status(), "running");
    // Today's engine: none of its containers, so none of its business.
    let listed = stores.ok(&["ps", "-a"]);
    assert!(!listed.contains(&id), "{listed}");
    stores.ok(&["system", "recover"]);
    if !is_root() {
        stores.ok(&["system", "stop"]);
        stores.ok(&["system", "recover"]);
    }
    assert!(dir.join("state.json").exists(), "its state kept");
    assert_eq!(status(), "running", "left running");
    // Its end, as stage 8's monitor recorded it.
    init.kill().unwrap();
    init.wait().unwrap();
    std::fs::write(dir.join("exit"), "exited (exit code 0)").unwrap();
    assert_eq!(status(), "stopped");
    let deleted = runtime(&["delete", &id]);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(!dir.exists());
    let _ = monitor.kill();
    let _ = monitor.wait();
}

/// A container's monitor runs in a cgroup of its own (`shipyard/monitors/<id>`),
/// out of its caller's (a login session's, a unit's), so a shutdown that ends
/// those doesn't end it before `system stop` reaches it (stage 9, step 9.9b).
/// Its init stays in its own cgroup; a policy restart's monitor joins the same
/// one; `rm` removes it. Root, or a user with a delegated cgroup tree.
#[test]
fn a_monitor_lives_in_a_cgroup_of_its_own() {
    if !is_root() && shipyard_test_support::delegated_cgroup().is_none() {
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    // `/proc/PID/cgroup`'s v2 line, and a process's parent.
    let cgroup_of = |pid: i64| -> String {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap()
            .to_owned()
    };
    let parent_of = |pid: i64| -> i64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after_name = stat.rsplit(')').next().unwrap();
        after_name
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    };
    let monitor_of = |name: &str| -> (i64, i64) {
        let init = stores.inspect(name)["state"]["pid"].as_i64().unwrap();
        let monitor = parent_of(init);
        let cmdline = std::fs::read(format!("/proc/{monitor}/cmdline")).unwrap();
        assert!(
            String::from_utf8_lossy(&cmdline).contains("__shipyard-monitor"),
            "{monitor} isn't a monitor"
        );
        (init, monitor)
    };
    let id = stores
        .ok(&[
            "run",
            "-d",
            "--name",
            "watched",
            "--restart",
            "on-failure",
            "--network",
            "none",
            &stores.image,
            "sleep",
            "300",
        ])
        .trim()
        .to_owned();
    let own = cgroup_of(i64::from(std::process::id()));
    let leaf = format!("shipyard/monitors/{id}");
    assert!(!own.ends_with(&leaf), "the test's own cgroup: {own}");
    let (init, monitor) = monitor_of("watched");
    let monitors = cgroup_of(monitor);
    assert!(monitors.ends_with(&leaf), "{monitors}");
    assert!(
        cgroup_of(init).ends_with(&format!("shipyard/runtime/{id}")),
        "{}",
        cgroup_of(init)
    );
    // A policy restart (its end from outside): its new monitor, there too.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(init).unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    eventually("restarted", || {
        let shown = stores.inspect("watched");
        shown["restart_count"] == 1 && shown["state"]["status"] == "running"
    });
    let (_, restarted) = monitor_of("watched");
    assert_ne!(restarted, monitor);
    assert_eq!(cgroup_of(restarted), monitors);
    stores.ok(&["rm", "-f", "watched"]);
    let path = std::path::Path::new("/sys/fs/cgroup").join(monitors.trim_start_matches('/'));
    assert!(!path.exists(), "{} left", path.display());
}

/// A container publishing a range of ports runs: its monitor holds every
/// socket (two a port, on a network with IPv6). One publishing more than a
/// monitor holds is refused before anything is made, saying why. As root, in
/// a network namespace of the test's own.
#[test]
fn a_range_of_ports_is_published_and_too_many_are_refused_first() {
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        stores.ok(&[
            "run",
            "-d",
            "--name",
            "ranged",
            "--network",
            "app",
            "-p",
            "20000-20099:20000-20099",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(stores.ok(&["port", "ranged"]).lines().count(), 200);
        stores.ok(&["rm", "-f", "ranged"]);
        let refused = stores.shipyard(&[
            "run",
            "-d",
            "--name",
            "too-many",
            "--network",
            "app",
            // 1,100 ports (a range has at most 1,024): two sockets each,
            // past what a monitor holds.
            "-p",
            "21000-21999:21000-21999",
            "-p",
            "22000-22099:22000-22099",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.status.code(), Some(125), "{refused:?}");
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(
            said.contains("sockets, which a container's monitor holds"),
            "{said}"
        );
        assert!(
            !stores.ok(&["ps", "-a"]).contains("too-many"),
            "nothing made"
        );
    });
}

/// A container whose published ports fit in a monitor's holds only without
/// its other holds (its lease, run directory, writable area, `running`) is
/// refused before anything is made: 1,050 ports on both families are 2,100
/// sockets, every hold a monitor takes, with nothing left for the rest. Its
/// descriptor limit raised as far as it goes, so the monitor's own bound is
/// the one met. 100 ports run (the control). As root, in a network
/// namespace of the test's own.
#[test]
fn ports_that_leave_no_room_for_the_other_holds_are_refused_first() {
    use std::os::unix::process::CommandExt as _;
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        use nix::sys::resource::{Resource, getrlimit, setrlimit};
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        let raised = |args: &[&str]| {
            let mut command = stores.command(args);
            // SAFETY: `getrlimit` and `setrlimit` are async-signal-safe
            // system calls; nothing is allocated.
            unsafe {
                command.pre_exec(|| {
                    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
                    setrlimit(Resource::RLIMIT_NOFILE, hard, hard)?;
                    Ok(())
                });
            }
            command.output().expect("running shipyard")
        };
        let ran = raised(&[
            "run",
            "-d",
            "--name",
            "fits",
            "--network",
            "app",
            "-p",
            "20000-20099:20000-20099",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert!(ran.status.success(), "{ran:?}");
        assert_eq!(stores.ok(&["port", "fits"]).lines().count(), 200);
        stores.ok(&["rm", "-f", "fits"]);
        let refused = raised(&[
            "run",
            "-d",
            "--name",
            "no-room",
            "--network",
            "app",
            // 1,050 ports: 2,100 sockets, and the other holds on top.
            "-p",
            "21000-22023:21000-22023",
            "-p",
            "23000-23025:23000-23025",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.status.code(), Some(125), "{refused:?}");
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(
            said.contains("sockets, which a container's monitor holds with up to"),
            "{said}"
        );
        assert!(
            !stores.ok(&["ps", "-a"]).contains("no-room"),
            "nothing made"
        );
    });
}

/// Under a hard descriptor limit of 1,024, a run asks only for the holds
/// its monitor can hand its exit command (each hold's copy sits above it
/// while they're placed: 478 here), where its soft limit alone would let
/// 960 by: 300 ports on both families (605 holds) are refused before
/// anything is made, saying the bound; 200 (405) run (the control). As
/// root, in a network namespace of the test's own.
#[test]
fn ports_are_bound_by_the_room_to_hand_them_on() {
    use std::os::unix::process::CommandExt as _;
    if !is_root() {
        eprintln!("skipping: a network of its own needs root");
        return;
    }
    let Some(stores) = Stores::new() else {
        return;
    };
    shipyard_test_support::in_new_netns(move || {
        use nix::sys::resource::{Resource, setrlimit};
        use shipyard_network::route::{LinkChange, Rtnetlink};
        let mut host = Rtnetlink::new().unwrap();
        let lo = host.link("lo").unwrap().unwrap().index;
        host.change(lo, LinkChange::Up(true)).unwrap();
        stores.ok(&["network", "create", "--ipv6", "app"]);
        let limited = |args: &[&str]| {
            let mut command = stores.command(args);
            // SAFETY: `setrlimit` is an async-signal-safe system call;
            // nothing is allocated.
            unsafe {
                command.pre_exec(|| {
                    setrlimit(Resource::RLIMIT_NOFILE, 1_024, 1_024)?;
                    Ok(())
                });
            }
            command.output().expect("running shipyard")
        };
        let ran = limited(&[
            "run",
            "-d",
            "--name",
            "fits",
            "--network",
            "app",
            "-p",
            "20000-20199:20000-20199",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert!(ran.status.success(), "{ran:?}");
        stores.ok(&["rm", "-f", "fits"]);
        let refused = limited(&[
            "run",
            "-d",
            "--name",
            "no-room",
            "--network",
            "app",
            "-p",
            "21000-21299:21000-21299",
            &stores.image,
            "sleep",
            "300",
        ]);
        assert_eq!(refused.status.code(), Some(125), "{refused:?}");
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(said.contains("at most 478 in all here"), "{said}");
        assert!(
            !stores.ok(&["ps", "-a"]).contains("no-room"),
            "nothing made"
        );
    });
}

/// A volume a container mounts is kept while the container is, running or
/// stopped (Docker's): `volume rm` refuses it naming the container (125),
/// `volume prune` keeps it as in use; once the container is removed,
/// `volume rm` removes it.
#[test]
fn a_stopped_containers_volume_is_kept_until_it_is_removed() {
    let Some(stores) = Stores::new() else {
        return;
    };
    stores.ok(&[
        "run",
        "-d",
        "--name",
        "web",
        "--network",
        "none",
        "--volume",
        "data:/data",
        &stores.image,
        "sleep",
        "300",
    ]);
    stores.ok(&["stop", "-t", "1", "web"]);
    let refused = stores.shipyard(&["volume", "rm", "data"]);
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("the volume data is in use by web"), "{said}");
    let pruned = stores.ok(&["volume", "prune", "-f"]);
    assert!(pruned.contains("Kept 1 in use: data"), "{pruned}");
    assert!(stores.ok(&["volume", "ls"]).contains("data"), "pruned");
    stores.ok(&["rm", "web"]);
    stores.ok(&["volume", "rm", "data"]);
    assert!(!stores.ok(&["volume", "ls"]).contains("data"), "kept");
}

/// A foreground run of an image is a container (Docker's): kept once it
/// ends, its exit code `run`'s, its logs and a new start there; `--rm`
/// leaves nothing; a caller without a state directory for containers runs
/// it here instead, saying so.
#[test]
fn a_foreground_run_is_a_container_by_name() {
    let Some(stores) = Stores::new() else {
        return;
    };
    let image = stores.image.clone();
    let ran = stores.shipyard(&[
        "run",
        "--name",
        "fg",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "echo once; exit 3",
    ]);
    assert_eq!(ran.status.code(), Some(3), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "once\n");
    assert!(stores.ok(&["ps", "-a"]).contains("fg"));
    assert_eq!(stores.ok(&["logs", "fg"]), "once\n");
    let again = stores.shipyard(&["start", "-a", "fg"]);
    assert_eq!(again.status.code(), Some(3), "{again:?}");
    assert_eq!(stores.ok(&["logs", "fg"]), "once\nonce\n");
    // Kept, it keeps its image: `rmi` refuses it until it's removed.
    let refused = stores.shipyard(&["rmi", &image]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("used by container"),
        "{refused:?}"
    );
    stores.ok(&["rm", "fg"]);
    let removed = stores.shipyard(&[
        "run",
        "--rm",
        "--sig-proxy=false",
        "--network",
        "none",
        &image,
        "sh",
        "-c",
        "echo gone",
    ]);
    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "--rm left it");
    if !shipyard_test_support::is_root() {
        let here = Command::new(SHIPYARD.path())
            .args(["run", "--network", "none", &image, "sh", "-c", "echo here"])
            .env("SHIPYARD_IMAGE_STORE", stores.scratch.store())
            .env(
                "SHIPYARD_CONTAINER_DIR",
                stores.scratch.0.join("containers"),
            )
            .env("SHIPYARD_NETWORK_DIR", stores.scratch.0.join("networks"))
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .expect("running shipyard");
        assert!(here.status.success(), "{here:?}");
        assert!(
            String::from_utf8_lossy(&here.stderr).contains("this run isn't a container"),
            "{here:?}"
        );
        assert_eq!(stores.ok(&["ps", "-a", "-q"]), "", "no record without one");
    }
}
