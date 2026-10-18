//! End-to-end tests of the macOS VM (step 10.3a): the real `shipyard`
//! starting its host, booting the guest built by `scripts/guest/build.sh`,
//! running a container in it, stopping it in order, and booting it again.
//!
//! They run only where asked (`SHIPYARD_TEST_VM=required`, with the guest
//! in `target/guest` or `SHIPYARD_GUEST_DIR`): each boots a VM, and pulls an
//! image from Docker Hub in it. Elsewhere they say why and return. The
//! binary is signed first (scripts/macos/sign.sh): cargo's runner signs the
//! test binary, not the `shipyard` it runs.

#![cfg(target_os = "macos")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic in a test is a clear failure"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.20";

mod vm_support;
use vm_support::Vm;

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The milestone of 10.3a: the guest boots to its agent (within a bound:
/// H7), a container runs in it with its stdout, stderr and exit code
/// apart, input reaches it, the VM stops in order (the guest's own stop,
/// not a forced one), and a second boot finds the first's image (the data
/// disk kept, checked and mounted, not formatted again).
#[test]
fn the_guest_boots_runs_a_container_and_keeps_its_data() {
    let Some(vm) = Vm::new("milestone") else {
        return;
    };
    let began = Instant::now();
    let started = vm.shipyard(&["vm", "start"]);
    assert!(started.status.success(), "{started:?}");
    let booted = began.elapsed();
    assert!(booted < Duration::from_secs(30), "booted in {booted:?}");
    assert!(text(&vm.shipyard(&["vm", "status"]).stdout).starts_with("running"));

    let ran = vm.shipyard(&[
        "run",
        "--rm",
        IMAGE,
        "sh",
        "-c",
        "echo out; echo err >&2; exit 3",
    ]);
    assert_eq!(ran.status.code(), Some(3), "{ran:?}");
    assert_eq!(text(&ran.stdout), "out\n");
    assert!(text(&ran.stderr).ends_with("err\n"), "{ran:?}");

    let mut typed = vm
        .command(&["run", "--rm", "-i", IMAGE, "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    typed.stdin.take().unwrap().write_all(b"typed\n").unwrap();
    let typed = typed.wait_with_output().unwrap();
    assert!(typed.status.success(), "{typed:?}");
    assert_eq!(text(&typed.stdout), "typed\n");

    let stopped = vm.shipyard(&["vm", "stop"]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert_eq!(
        text(&vm.shipyard(&["vm", "status"]).stdout).lines().next(),
        Some("stopped")
    );
    let host_log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    assert!(
        !host_log.contains("stopped at once"),
        "a forced stop: {host_log}"
    );
    assert!(!host_log.contains("stopped by itself"), "{host_log}");
    assert!(
        vm.console()
            .contains("shipyard-guest: stopped; powering off")
    );

    let again = vm.shipyard(&["vm", "start"]);
    assert!(again.status.success(), "{again:?}: {}", vm.console());
    let images = vm.shipyard(&["images"]);
    assert!(text(&images.stdout).contains("alpine"), "{images:?}");
    assert_eq!(vm.console().matches("formatting /dev/vda").count(), 1);
}

/// Two first commands at once start one VM: both reach it, one host holds
/// the lock (the other's host finds it taken and leaves).
#[test]
fn two_first_commands_start_one_vm() {
    let Some(vm) = Vm::new("two") else {
        return;
    };
    let starts: Vec<_> = (0..2)
        .map(|_| {
            vm.command(&["vm", "start"])
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for start in starts {
        let output = start.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    assert_eq!(log.matches("booting the guest").count(), 1, "{log}");
}

/// 10.3b1: commands run in the guest through the plain CLI, as on Linux.
/// A directory under the home binds at its own path; a link in it out of
/// the home is refused before anything is sent; `-e KEY` takes the Mac's
/// value; a Ctrl-C (SIGINT to the CLI) reaches the container; a reader
/// that goes (`… | head -1`) ends the command quietly; and a warm run is
/// quick (H7).
#[test]
fn commands_are_forwarded_as_on_linux() {
    use std::io::BufRead as _;
    let Some(vm) = Vm::new("forward") else {
        return;
    };
    let started = vm.shipyard(&["vm", "start"]);
    assert!(started.status.success(), "{started:?}");
    let pulled = vm.shipyard(&["pull", "-q", IMAGE]);
    assert!(pulled.status.success(), "{pulled:?}");

    // A bind of the home's own files, and a link out of the home refused.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vm-forward-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note"), "from the mac\n").unwrap();
    let bind = format!("{}:/w", dir.display());
    let read = vm.shipyard(&["run", "--rm", "--volume", &bind, IMAGE, "cat", "/w/note"]);
    assert_eq!(text(&read.stdout), "from the mac\n", "{read:?}");
    // The bind keeps the home's mount flags (F108): no set-user-ID bits, no
    // device files, from the Mac (its line found: the control).
    let flags = vm.shipyard(&[
        "run",
        "--rm",
        "--volume",
        &bind,
        IMAGE,
        "sh",
        "-c",
        "grep ' /w ' /proc/self/mountinfo",
    ]);
    let line = text(&flags.stdout);
    assert!(line.contains(" /w "), "{flags:?}");
    assert!(line.contains("nosuid") && line.contains("nodev"), "{line}");
    // (`/usr` is on every Mac: the link leads somewhere here.)
    std::os::unix::fs::symlink("/usr", dir.join("escape")).unwrap();
    let escape = format!("{}:/x", dir.join("escape").display());
    // The guest's own state, which the Mac hasn't: refused, never sent.
    for bind in [escape.as_str(), "/var/lib/shipyard:/x", "/run/shipyard:/x"] {
        let refused = vm.shipyard(&["run", "--rm", "--volume", bind, IMAGE, "true"]);
        assert_eq!(refused.status.code(), Some(125), "{bind}: {refused:?}");
        assert!(
            text(&refused.stderr).contains("outside your home"),
            "{bind}: {refused:?}"
        );
    }

    // Ctrl-C: the CLI's SIGINT, passed on to the container (its trap says
    // it came).
    let mut interrupted = vm
        .command(&[
            "run",
            "--rm",
            "--name",
            "trapped",
            IMAGE,
            "sh",
            "-c",
            "trap 'exit 42' INT; echo ready; sleep 60 & wait",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(interrupted.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(interrupted.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    assert_eq!(interrupted.wait().unwrap().code(), Some(42));

    // A reader that goes: quiet, and a success.
    let mut many = vm
        .command(&["run", "--rm", IMAGE, "seq", "1", "200000"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    std::io::BufReader::new(many.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert_eq!(first, "1\n");
    let many = many.wait_with_output().unwrap();
    assert!(many.status.success(), "{many:?}");
    assert_eq!(text(&many.stderr), "");

    // H7: a warm run, start to end.
    let began = Instant::now();
    let warm = vm.shipyard(&["run", "--rm", IMAGE, "true"]);
    assert!(warm.status.success(), "{warm:?}");
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 10.3b1: what crosses to the guest is what the command names. `-e KEY`
/// takes the Mac's value, for the container only (`HTTPS_PROXY` isn't the
/// guest's shipyard's); a variable not named doesn't cross; one the
/// guest's own shipyard reads (`PATH`) is refused; a command reads its
/// input with `-i`, and without, leaves it unread.
#[test]
fn variables_and_input_cross_as_named() {
    let Some(vm) = Vm::new("named") else {
        return;
    };
    let started = vm.shipyard(&["vm", "start"]);
    assert!(started.status.success(), "{started:?}");
    // `-e HTTPS_PROXY` is the container's, never the guest's shipyard's: its
    // pull (the image isn't in this VM yet) doesn't go through a proxy
    // that isn't there (the guest's own port 9).
    let proxied = vm
        .command(&[
            "run",
            "--rm",
            "-e",
            "HTTPS_PROXY",
            SERVER_IMAGE,
            "sh",
            "-c",
            "echo $HTTPS_PROXY",
        ])
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(proxied.status.success(), "{proxied:?}");
    assert_eq!(text(&proxied.stdout), "http://127.0.0.1:9\n", "{proxied:?}");

    // `-e KEY` copies the Mac's value; nothing else of the Mac's crosses.
    let copied = vm
        .command(&[
            "run",
            "--rm",
            "-e",
            "FORWARD_TEST_TOKEN",
            IMAGE,
            "sh",
            "-c",
            "echo $FORWARD_TEST_TOKEN",
        ])
        .env("FORWARD_TEST_TOKEN", "xyz")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(text(&copied.stdout), "xyz\n", "{copied:?}");

    // A variable not named doesn't cross: the guest's shipyard never had
    // it to copy (the control: the same name, named, did above).
    let kept = vm
        .command(&[
            "run",
            "--rm",
            "-e",
            "FORWARD_TEST_OTHER",
            IMAGE,
            "sh",
            "-c",
            "echo ${FORWARD_TEST_OTHER:-absent}",
        ])
        .env("FORWARD_TEST_TOKEN", "xyz")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(text(&kept.stdout), "absent\n", "{kept:?}");
    let reserved = vm.shipyard(&["run", "--rm", "-e", "PATH", IMAGE, "true"]);
    assert_eq!(reserved.status.code(), Some(125), "{reserved:?}");

    // Input: read with -i; without, not read (the next reader still has it).
    let typed = |args: &[&str]| {
        let mut child = vm
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"typed\n").unwrap();
        child.wait_with_output().unwrap()
    };
    let with = typed(&["run", "--rm", "-i", IMAGE, "cat"]);
    assert_eq!(text(&with.stdout), "typed\n", "{with:?}");
    let without = typed(&["run", "--rm", IMAGE, "cat"]);
    assert_eq!(text(&without.stdout), "", "{without:?}");
}

/// The web server's image: busybox's own has `httpd` (Alpine's hasn't).
const SERVER_IMAGE: &str = "busybox:1.36";

/// A busybox web server in [`SERVER_IMAGE`], serving `hello-port` at `/` and 8 MiB
/// of [`big_body`]'s pattern at `/big`.
const SERVER: &str = "mkdir -p /www && echo hello-port > /www/index.html && \
                      yes 0123456789abcdef | head -c 8388608 > /www/big && \
                      exec httpd -f -p 80 -h /www";

/// What `/big` holds.
fn big_body() -> Vec<u8> {
    b"0123456789abcdef\n"
        .iter()
        .copied()
        .cycle()
        .take(8 << 20)
        .collect()
}

/// `GET path` from `address`: the body, if it answered.
fn fetch(address: &str, path: &str) -> Option<Vec<u8>> {
    fetch_after(address, path, Duration::ZERO)
}

/// [`fetch`], the request sent `pause` after the connection is made (a
/// slow client: what's forwarded mustn't take its silence for its end).
fn fetch_after(address: &str, path: &str, pause: Duration) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let address: std::net::SocketAddr = address.parse().unwrap();
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    std::thread::sleep(pause);
    write!(stream, "GET {path} HTTP/1.0\r\n\r\n").ok()?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).ok()?;
    let start = answer.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    Some(answer.split_off(start))
}

/// `fetch` until it answers, for at most `limit`: how long it took. The
/// host's log said if it doesn't.
fn answers_within(vm: &Vm, address: &str, limit: Duration) -> Duration {
    let began = Instant::now();
    loop {
        if fetch(address, "/").as_deref() == Some(b"hello-port\n".as_slice()) {
            return began.elapsed();
        }
        assert!(
            began.elapsed() < limit,
            "{address} didn't answer within {limit:?}; the host's log:\n{}\n{}",
            std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default(),
            text(&vm.shipyard(&["ps", "-a"]).stdout)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a connection to `address` is refused at once (nothing listens).
fn refused(address: &str) -> bool {
    let address: std::net::SocketAddr = address.parse().unwrap();
    std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
}

/// A port free on this Mac's loopback, both families, now (taken by the
/// system, then let go): the tests' ports never clash with a developer's.
fn free_port() -> u16 {
    loop {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        if std::net::TcpListener::bind(("::1", port)).is_ok() {
            return port;
        }
    }
}

/// `shipyard run -d --name NAME -p PUBLISH` of the web server.
fn serve(vm: &Vm, name: &str, publish: &str) {
    let ran = vm.shipyard(&[
        "run",
        "-d",
        "--name",
        name,
        "-p",
        publish,
        SERVER_IMAGE,
        "sh",
        "-c",
        SERVER,
    ]);
    assert!(ran.status.success(), "{ran:?}");
}

/// The milestone of 10.3c1: a published port answers on the Mac's
/// loopback, both families, within a bound of the `run -d` (H7: the 2 s
/// poll, woken as the command ends), byte for byte for 8 MiB; `vm status`
/// says it; one published on `127.0.0.1` only there; a removed
/// container's port is closed.
#[test]
fn published_ports_answer_on_the_macs_loopback() {
    let Some(vm) = Vm::new("ports") else {
        return;
    };
    assert!(vm.shipyard(&["pull", "-q", SERVER_IMAGE]).status.success());
    let (web, local) = (free_port(), free_port());
    let at = |host: &str, port: u16| format!("{host}:{port}");
    serve(&vm, "web", &format!("{web}:80"));
    let took = answers_within(&vm, &at("127.0.0.1", web), Duration::from_secs(5));
    eprintln!("forwarded {took:?} after `run -d` returned");
    answers_within(&vm, &at("[::1]", web), Duration::from_secs(1));
    let status = text(&vm.shipyard(&["vm", "status"]).stdout);
    assert!(
        status.contains(&format!("ports: 127.0.0.1:{web}, [::1]:{web}")),
        "{status}"
    );
    assert_eq!(fetch(&at("127.0.0.1", web), "/big"), Some(big_body()));
    let slowly = fetch_after(&at("127.0.0.1", web), "/", Duration::from_millis(500));
    assert_eq!(
        slowly.as_deref(),
        Some(b"hello-port\n".as_slice()),
        "a slow client"
    );

    // The agent forwards only a port a running container publishes: asked
    // straight through the host's socket (as any of this user's programs
    // could), the published one is taken, another (the guest's own) not.
    let asked = |port: u16| {
        use shipyard_runtime::attach::forward::{self, Connection, FORWARD_REQUEST, Request};
        let mut stream = std::os::unix::net::UnixStream::connect(vm.dir.join("vm.sock")).unwrap();
        let args = [FORWARD_REQUEST, "0.0.0.0", &port.to_string()].map(str::to_owned);
        let request = Request::new(args.to_vec(), "/".to_owned(), Vec::new());
        forward::send(&mut stream, &forward::hello(&request).unwrap()).unwrap();
        Connection::new(stream).next_frame().unwrap()
    };
    assert!(
        matches!(
            asked(web),
            Some(shipyard_runtime::attach::Frame::Welcome { .. })
        ),
        "the published port"
    );
    assert!(
        matches!(
            asked(53),
            Some(shipyard_runtime::attach::Frame::Refused(why))
                if why.contains("no running container publishes port 53")
        ),
        "the guest's own port"
    );

    // On `127.0.0.1` only: that one answers, `::1` isn't listened on.
    serve(&vm, "local", &format!("127.0.0.1:{local}:80"));
    answers_within(&vm, &at("127.0.0.1", local), Duration::from_secs(5));
    assert!(refused(&at("[::1]", local)));

    // Removed, its port is closed (it answered just before).
    answers_within(&vm, &at("127.0.0.1", web), Duration::from_secs(1));
    assert!(vm.shipyard(&["rm", "-f", "web"]).status.success());
    let began = Instant::now();
    while !refused(&at("127.0.0.1", web)) {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{web} still open"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = vm.shipyard(&["rm", "-f", "local"]);
}

/// A port another program on the Mac listens on (on every IPv4 address)
/// is said busy there, and that program keeps its connections (never
/// shadowed by a loopback listener of the VM's), while the port is
/// forwarded where it's free (`::1`); once that program is gone, on
/// `127.0.0.1` too.
#[test]
fn a_port_in_use_on_the_mac_is_said_and_left_alone() {
    let Some(vm) = Vm::new("busy") else {
        return;
    };
    let port = free_port();
    let theirs = std::net::TcpListener::bind(("0.0.0.0", port)).unwrap();
    assert!(vm.shipyard(&["pull", "-q", SERVER_IMAGE]).status.success());
    serve(&vm, "busy", &format!("{port}:80"));
    let busy = format!("not forwarded: 127.0.0.1:{port} (in use on this Mac)");
    let began = Instant::now();
    loop {
        let status = text(&vm.shipyard(&["vm", "status"]).stdout);
        if status.contains(&busy) {
            assert!(status.contains(&format!("ports: [::1]:{port}")), "{status}");
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "{status}");
        std::thread::sleep(Duration::from_millis(200));
    }
    answers_within(&vm, &format!("[::1]:{port}"), Duration::from_secs(1));
    // `localhost` still reaches the Mac's own program: the connection it
    // takes is this one (the VM host's probes, if any, are let pass).
    let client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mine = client.local_addr().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let began = Instant::now();
    let accepted = loop {
        match theirs.accept() {
            Ok((accepted, peer)) if peer == mine => break accepted,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    began.elapsed() < Duration::from_secs(5),
                    "the Mac's own program never got its connection"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("{error}"),
        }
    };
    // The client first, as a browser would: no TIME_WAIT on the port.
    drop(client);
    drop(accepted);
    drop(theirs);
    answers_within(&vm, &format!("127.0.0.1:{port}"), Duration::from_secs(35));
    let _ = vm.shipyard(&["rm", "-f", "busy"]);
}

/// `shipyard ARGS` to `vm`, its host (if it starts one) idle after
/// `idle` seconds.
fn with_idle(vm: &Vm, idle: &str, args: &[&str]) -> std::process::Output {
    vm.command(args)
        .env("SHIPYARD_VM_IDLE", idle)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Waits for `vm status`'s first line to be `stopped`, for at most `limit`.
fn stops_within(vm: &Vm, limit: Duration) {
    let began = Instant::now();
    while !text(&vm.shipyard(&["vm", "status"]).stdout).starts_with("stopped") {
        assert!(
            began.elapsed() < limit,
            "still running after {limit:?}; the host's log:\n{}",
            std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// D20: the VM stops by itself once idle (no command, nothing running),
/// in order (the host's log says so), and the next command boots it again
/// within a bound (H7: a cold start).
#[test]
fn an_idle_vm_stops_by_itself_and_starts_again() {
    let Some(vm) = Vm::new("idle") else {
        return;
    };
    assert!(with_idle(&vm, "3", &["vm", "start"]).status.success());
    stops_within(&vm, Duration::from_secs(30));
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    assert!(log.contains("idle for 3 s: stopping"), "{log}");
    let began = Instant::now();
    let cold = with_idle(&vm, "3", &["ps"]);
    let took = began.elapsed();
    assert!(cold.status.success(), "{cold:?}");
    eprintln!("a cold `ps` took {took:?}");
    assert!(took < Duration::from_secs(10), "a cold start took {took:?}");
}

/// A container running keeps the VM up past the limit (the host says so:
/// a line, not a wait that proves nothing); once it's gone, the VM stops.
#[test]
fn a_running_container_keeps_the_vm_up() {
    let Some(vm) = Vm::new("keptup") else {
        return;
    };
    assert!(with_idle(&vm, "3", &["pull", "-q", IMAGE]).status.success());
    let ran = with_idle(
        &vm,
        "3",
        &["run", "-d", "--name", "long", IMAGE, "sleep", "600"],
    );
    assert!(ran.status.success(), "{ran:?}");
    let began = Instant::now();
    loop {
        let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default();
        if log.contains("kept up: 1 active") {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(30), "{log}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(text(&vm.shipyard(&["vm", "status"]).stdout).starts_with("running"));
    assert!(with_idle(&vm, "3", &["rm", "-f", "long"]).status.success());
    stops_within(&vm, Duration::from_secs(30));
}

/// Amendment 7's race: commands come as the VM stops for being idle (a
/// 1 s limit, commands paced around it); every one of them runs, through
/// a stop and a new boot when it lands in one.
#[test]
fn commands_racing_the_idle_stop_all_run() {
    let Some(vm) = Vm::new("race") else {
        return;
    };
    for round in 0..20_u64 {
        let ran = with_idle(&vm, "1", &["ps"]);
        assert!(
            ran.status.success(),
            "round {round}: {ran:?}; the host's log:\n{}",
            std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
        );
        // Around the limit and the 2 s poll: before, at and past them.
        std::thread::sleep(Duration::from_millis(500 * (round % 7)));
    }
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default();
    assert!(
        log.contains("idle for 1 s: stopping"),
        "stops were raced: {log}"
    );
    // How often a command landed in a stop's window (refused, then sent
    // again): the race itself, which the pacing makes likely, not certain
    // (the client's side of it is `client`'s unit test, deterministic).
    eprintln!(
        "{} stops, {} commands refused as one began",
        log.matches("idle for 1 s: stopping").count(),
        log.matches("a client refused: stopping").count()
    );
}

/// `doctor` on a Mac checks the VM: this binary (signed) passes; a copy
/// signed without the virtualization entitlement fails on it, and says how
/// to sign it.
#[test]
fn doctor_checks_the_vm() {
    let Some(vm) = Vm::new("doctor") else {
        return;
    };
    let checked = vm.shipyard(&["doctor"]);
    let said = text(&checked.stdout);
    for line in [
        "ok    macos",
        "ok    entitlement",
        "ok    guest",
        "ok    vm",
    ] {
        assert!(said.contains(line), "{line}: {said}");
    }
    assert!(checked.status.success(), "{checked:?}");
    // Stopped, the VM isn't started to check its guest: it's said.
    assert!(said.contains("guest:     the VM isn't running"), "{said}");
    assert!(text(&vm.shipyard(&["vm", "status"]).stdout).starts_with("stopped"));

    // Running, the guest's own checks follow (FEATURES §10: the engine's
    // kernel needs, checked in the guest): cgroups, overlay, nftables.
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    let checked = vm.shipyard(&["doctor"]);
    let said = text(&checked.stdout);
    assert!(said.contains("guest:     Linux 6."), "{said}");
    for line in [
        "ok    cgroups",
        "ok    images",
        "ok    network",
        "ok    firewall",
    ] {
        assert!(said.contains(line), "{line}: {said}");
    }
    assert!(!said.contains("fail "), "{said}");
    assert!(checked.status.success(), "{checked:?}");
    assert!(vm.shipyard(&["vm", "stop"]).status.success());

    std::fs::create_dir_all(&vm.dir).unwrap();
    let copy = vm.dir.join("shipyard-unsigned");
    std::fs::copy(vm_support::SHIPYARD, &copy).unwrap();
    let resigned = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&copy)
        .output()
        .unwrap();
    assert!(resigned.status.success(), "{resigned:?}");
    let mut command = Command::new(&copy);
    command.arg("doctor").stdin(Stdio::null());
    for (name, value) in vm.env() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    let unsigned = command.output().unwrap();
    let said = text(&unsigned.stdout);
    assert!(said.contains("fail  entitlement"), "{said}");
    assert!(said.contains("scripts/macos/sign.sh"), "{said}");
    assert_eq!(unsigned.status.code(), Some(1), "{unsigned:?}");
}

/// The race, made to happen: a command sent while the host stops (its
/// socket held open for it, `SHIPYARD_VM_TEST_STOP_HOLD`) is refused
/// before it reaches the guest, waits for the stop, starts the VM again
/// and runs, once.
#[test]
fn a_command_sent_into_a_stop_runs_after_it() {
    let Some(vm) = Vm::new("intostop") else {
        return;
    };
    let started = vm
        .command(&["vm", "start"])
        .env("SHIPYARD_VM_TEST_STOP_HOLD", "3000")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    let mut stop = vm
        .command(&["vm", "stop"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let began = Instant::now();
    loop {
        let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default();
        if log.contains("SIGTERM: stopping the guest") {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "{log}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let ran = vm.shipyard(&["ps"]);
    assert!(ran.status.success(), "{ran:?}");
    assert!(stop.wait().unwrap().success());
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default();
    assert!(log.contains("a client refused: stopping"), "{log}");
    assert_eq!(log.matches("the guest is up").count(), 2, "{log}");
}

/// A container given the home can't reach the VM's own files in it: the
/// VM's directory (here under the home, as a user's is: its data disk, its
/// socket) is empty to it, and so is the guest (`target/guest`, when the
/// repository is under the home), while the home around them is there
/// (the bind works: the control).
#[test]
fn the_vms_own_files_are_hidden_from_a_home_bind() {
    let Some(vm) = Vm::new_in_home("hidden") else {
        return;
    };
    let home = std::fs::canonicalize(std::env::var_os("HOME").unwrap()).unwrap();
    let started = vm.shipyard(&["vm", "start"]);
    assert!(started.status.success(), "{started:?}");
    assert!(
        vm.dir.join("data.img").exists(),
        "the VM's disk, in the home"
    );
    let inside = |path: &Path| Path::new("/h").join(path.strip_prefix(&home).unwrap());
    let vm_dir = inside(&std::fs::canonicalize(&vm.dir).unwrap());
    let repo = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
    // Each place: there (a wrong path is no proof), and empty to it.
    let mut hidden = vec![vm_dir];
    if repo.starts_with(&home) {
        hidden.push(inside(&repo.join("target/guest")));
    }
    let script = hidden
        .iter()
        .map(|place| format!("test -d {0} && ls -A {0} | wc -l", place.display()))
        .chain(std::iter::once(
            "test -n \"$(ls -A /h)\" && echo bound".to_owned(),
        ))
        .collect::<Vec<_>>()
        .join("; ");
    let bind = format!("{}:/h", home.display());
    let ran = vm.shipyard(&["run", "--rm", "--volume", &bind, IMAGE, "sh", "-c", &script]);
    assert!(ran.status.success(), "{ran:?}");
    let said: Vec<String> = text(&ran.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect();
    let mut expected = vec!["0"; hidden.len()];
    expected.push("bound");
    assert_eq!(said, expected, "{ran:?}");
}
