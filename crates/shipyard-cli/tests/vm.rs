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
use vm_support::BoundedOutput as _;
use vm_support::{Vm, text};

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
    let typed = vm_support::finish_within(typed, vm_support::COMMAND_LIMIT, "typed");
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
        let output = vm_support::finish_within(start, vm_support::COMMAND_LIMIT, "start");
        assert!(output.status.success(), "{output:?}");
    }
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    assert_eq!(log.matches("booting the guest").count(), 1, "{log}");
}

/// 10.3b1: commands run in the guest through the plain CLI, as on Linux.
/// `command` started with SIGINT as a terminal's Ctrl-C gives it: a test
/// run from a shell's background job (`… &`) inherits it ignored, and a
/// program started so rightly keeps ignoring it (11.5b2b's verify: the CLI
/// never saw the test's SIGINT, and the command ran its `sleep 60` out).
fn with_sigint(command: &mut Command) {
    // SAFETY: between fork and exec, `signal` only: async-signal-safe, no
    // allocation, no lock.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(command, || {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
}

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
            text(&refused.stderr)
                .contains("not shared: on a Mac, only your home and a few places beside it"),
            "{bind}: {refused:?}"
        );
    }

    // Ctrl-C: the CLI's SIGINT, passed on to the container (its trap says
    // it came).
    let mut trapped = vm.command(&[
        "run",
        "--rm",
        "--name",
        "trapped",
        IMAGE,
        "sh",
        "-c",
        "trap 'exit 42' INT; echo ready; sleep 60 & wait",
    ]);
    trapped
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    with_sigint(&mut trapped);
    let mut interrupted = trapped.spawn().unwrap();
    let mut out = std::io::BufReader::new(interrupted.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(interrupted.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let ended = vm_support::finish_within(interrupted, vm_support::COMMAND_LIMIT, "interrupted");
    assert_eq!(
        ended.status.code(),
        Some(42),
        "{}; the host's log:\n{}",
        text(&ended.stderr),
        std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
    );

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
    let many = vm_support::finish_within(many, vm_support::COMMAND_LIMIT, "many");
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

/// A build on the Mac: its context in the home (where a Mac keeps it
/// private: `0750`, `~/Desktop` `0700`) is read by the guest's builder, and
/// the image runs (the stage-12 close's Murphy review found every such build
/// refused). Needs the network in the guest: BuildKit's image and the base.
#[test]
fn a_build_in_the_home_runs() {
    let Some(vm) = Vm::new("build") else {
        return;
    };
    let context =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("vm-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&context);
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        "FROM busybox:1.37\nCOPY note /note\nCMD [\"cat\", \"/note\"]\n",
    )
    .unwrap();
    std::fs::write(context.join("note"), "built on the mac\n").unwrap();
    std::fs::set_permissions(
        &context,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let built = vm.shipyard(&["build", "-t", "made-here:1", context.to_str().unwrap()]);
    assert!(built.status.success(), "{built:?}");
    let ran = vm.shipyard(&["run", "--rm", "--pull", "never", "made-here:1"]);
    assert_eq!(text(&ran.stdout), "built on the mac\n", "{ran:?}");
    std::fs::remove_dir_all(&context).unwrap();
}

/// Stage 12's archives through the VM: `save -o` and `export -o` write a
/// file in the Mac's home, `load -i` and `import` read one, `save` to a
/// pipe and `load` from one stream through the forwarded command; what was
/// saved and loaded back runs, and so does what was exported and imported.
#[test]
fn archives_cross_as_files_in_the_home() {
    let Some(vm) = Vm::new("archives") else {
        return;
    };
    let pulled = vm.shipyard(&["pull", "-q", IMAGE]);
    assert!(pulled.status.success(), "{pulled:?}");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vm-archives-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let saved = dir.join("image.tar");
    let save = vm.shipyard(&["save", "-o", saved.to_str().unwrap(), IMAGE]);
    assert!(save.status.success(), "{save:?}");
    assert!(
        std::fs::metadata(&saved).unwrap().len() > 1 << 20,
        "a whole image"
    );
    let removed = vm.shipyard(&["rmi", IMAGE]);
    assert!(removed.status.success(), "{removed:?}");
    let load = vm.shipyard(&["load", "-i", saved.to_str().unwrap()]);
    assert!(load.status.success(), "{load:?}");
    assert!(text(&load.stdout).contains(IMAGE), "{load:?}");
    let ran = vm.shipyard(&["run", "--rm", "--pull", "never", IMAGE, "echo", "loaded"]);
    assert_eq!(text(&ran.stdout), "loaded\n", "{ran:?}");

    // To a pipe and back from one: the same bytes as a stream.
    let streamed = vm
        .command(&["save", IMAGE])
        .stdin(Stdio::null())
        .bounded_output();
    assert!(streamed.status.success(), "{streamed:?}");
    let mut loading = vm
        .command(&["load", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    loading
        .stdin
        .take()
        .unwrap()
        .write_all(&streamed.stdout)
        .unwrap();
    let loaded = vm_support::finish_within(loading, vm_support::COMMAND_LIMIT, "loading");
    assert!(loaded.status.success(), "{loaded:?}");

    // A container's files out, and in again as an image of their own.
    let made = vm.shipyard(&["create", "--name", "exported", IMAGE, "true"]);
    assert!(made.status.success(), "{made:?}");
    let exported = dir.join("files.tar");
    let export = vm.shipyard(&["export", "-o", exported.to_str().unwrap(), "exported"]);
    assert!(export.status.success(), "{export:?}");
    let import = vm.shipyard(&["import", exported.to_str().unwrap(), "imported:1"]);
    assert!(import.status.success(), "{import:?}");
    let release = vm.shipyard(&["run", "--rm", "imported:1", "cat", "/etc/alpine-release"]);
    assert!(release.status.success(), "{release:?}");
    assert!(text(&release.stdout).starts_with("3.20"), "{release:?}");
    let history = vm.shipyard(&["history", "-q", "imported:1"]);
    assert_eq!(text(&history.stdout).lines().count(), 1, "{history:?}");
    let _ = vm.shipyard(&["rm", "exported"]);
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
        .bounded_output();
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
        .bounded_output();
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
        .bounded_output();
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
        vm_support::finish_within(child, vm_support::COMMAND_LIMIT, "child")
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
                if why.contains("no running container publishes tcp port 53")
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
        .bounded_output()
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
        // F108: no command (this check's own process is one) holds the
        // agent's socket: PID 1 keeps it, the agent took it close-on-exec.
        "ok    agent port",
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
    let unsigned = command.bounded_output();
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
        .bounded_output();
    assert!(started.status.success(), "{started:?}");
    let stop = vm
        .command(&["vm", "stop"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let began = Instant::now();
    loop {
        let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default();
        // (`vm stop` asks on the host's own socket: 13.6b1.)
        if log.contains("asked over its own socket: stopping the guest") {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "{log}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let ran = vm.shipyard(&["ps"]);
    assert!(ran.status.success(), "{ran:?}");
    assert!(
        vm_support::finish_within(stop, vm_support::COMMAND_LIMIT, "vm stop")
            .status
            .success()
    );
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default();
    assert!(log.contains("a client refused: stopping"), "{log}");
    assert_eq!(log.matches("the guest is up").count(), 2, "{log}");
}

/// `vm`'s data disk, once kept: a first boot's (`data.img.new`) is renamed
/// once the host's own watch hears the guest ready, a moment after the CLI
/// does (13.8c). Within 10 s.
fn kept_disk(vm: &Vm) -> PathBuf {
    let disk = vm.dir.join("data.img");
    let began = Instant::now();
    while !disk.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "the VM's disk never kept"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    disk
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
    kept_disk(&vm);
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

/// Step 13.2: a guest whose clock went wrong (as a Mac's sleep leaves it:
/// 13.1's probe froze it across a pause) is set right by the host's own
/// asks, with no command forwarded. A request stamped an hour behind sets
/// it back (the console says so: the control); the host's next ask, within
/// seconds, sets it to the Mac's again.
#[test]
fn a_wrong_clock_is_set_right_by_the_host() {
    use shipyard_runtime::attach::forward::{self, Connection, Request};
    let Some(vm) = Vm::new("clock") else {
        return;
    };
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    // Past the agent's first minute: before it, an old stamp is a request
    // that waited for a restarting agent, which never sets the clock back
    // (13.5b, F108); after, it's the guest's clock ahead, followed. (A wait
    // for a rule of the clock's, not for something to happen.)
    std::thread::sleep(Duration::from_secs(62));
    // The console's clock lines (`the clock was OURS, the Mac's THEIRS: set
    // to the Mac's`), as (ours, theirs).
    let settings = |console: &str| -> Vec<(i64, i64)> {
        console
            .lines()
            .filter_map(|line| {
                let rest = line.split("the clock was ").nth(1)?;
                let (ours, rest) = rest.split_once(", the Mac's ")?;
                let (theirs, _) = rest.split_once(':')?;
                Some((ours.trim().parse().ok()?, theirs.trim().parse().ok()?))
            })
            .collect()
    };
    let before = settings(&vm.console()).len();
    let mut stream = std::os::unix::net::UnixStream::connect(vm.dir.join("vm.sock")).unwrap();
    let mut request = Request::new(vec!["version".to_owned()], "/".to_owned(), Vec::new());
    request.time = forward::clock().map(|now| now - 3600);
    forward::send(&mut stream, &forward::hello(&request).unwrap()).unwrap();
    let mut connection = Connection::new(stream);
    while let Ok(Some(frame)) = connection.next_frame() {
        if matches!(frame, shipyard_runtime::attach::Frame::Exited(_)) {
            break;
        }
    }
    // Set back an hour (the control), then set forward to the Mac's again by
    // the host's own poll (which may have done it already).
    let began = Instant::now();
    loop {
        let seen = settings(&vm.console());
        let new = seen.get(before..).unwrap_or_default();
        let back = new.iter().position(|(ours, theirs)| theirs + 3000 < *ours);
        let forward = back.and_then(|at| {
            new[at + 1..]
                .iter()
                .find(|(ours, theirs)| *theirs > ours + 3000)
        });
        if forward.is_some() {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "set back {back:?}, then never set right: {}",
            vm.console()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Step 13.2 (F103): what the guest frees goes back to the Mac at its stop.
/// A 512 MiB layer imported grows the disk's image (the control); removed,
/// the VM stopped, the image is back near its size before.
#[test]
fn a_stop_gives_freed_space_back() {
    use std::os::unix::fs::MetadataExt as _;
    let Some(vm) = Vm::new_in_home("trim") else {
        return;
    };
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    let disk = kept_disk(&vm);
    let taken = || std::fs::metadata(&disk).unwrap().blocks() * 512;
    let before = taken();
    let scratch = vm_support::Scratch::in_home("trim");
    let random = scratch.0.join("random");
    let made = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "head -c 536870912 /dev/urandom > '{}' && tar -cf '{}.tar' -C '{}' random",
            random.display(),
            random.display(),
            scratch.0.display()
        ))
        .status()
        .unwrap();
    assert!(made.success());
    let tar = format!("{}.tar", random.display());
    let imported = vm.shipyard(&["import", &tar, "trim-test:1"]);
    assert!(imported.status.success(), "{imported:?}");
    let grown = taken();
    assert!(
        grown > before + (400 << 20),
        "the image grew by {} bytes",
        grown - before
    );
    assert!(vm.shipyard(&["rmi", "trim-test:1"]).status.success());
    assert!(vm.shipyard(&["vm", "stop"]).status.success());
    let after = taken();
    assert!(
        after < before + (100 << 20),
        "before {before}, grown {grown}, after the stop {after}: {}",
        vm.console()
    );
    assert!(
        vm.console()
            .contains("free bytes of /var/lib/shipyard given back to the Mac")
    );
}

/// Step 13.2 (D13.2-1): `vm set`'s size is taken at the next start (the
/// host's log says which, and from where), and `default` puts it back.
#[test]
fn the_vm_takes_its_size_from_vm_set() {
    let Some(vm) = Vm::new("size") else {
        return;
    };
    let set = vm.shipyard(&["vm", "set", "--memory", "5g", "--cpus", "2"]);
    assert!(set.status.success(), "{set:?}");
    assert!(
        text(&set.stdout).starts_with("size: 5 GiB (vm set), 2 CPUs (vm set)"),
        "{set:?}"
    );
    assert!(vm.shipyard(&["vm", "start"]).status.success());
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    assert!(log.contains("2 CPUs (vm set), 5120 MiB (vm set)"), "{log}");
    let running = vm.shipyard(&["vm", "set", "--memory", "default"]);
    assert!(
        text(&running.stdout).contains("it takes this at its next start"),
        "{running:?}"
    );
    assert!(
        text(&vm.shipyard(&["vm", "status"]).stdout)
            .contains("(this Mac's default), 2 CPUs (vm set)")
    );
    let refused = vm.shipyard(&["vm", "set", "--memory", "1g"]);
    assert!(!refused.status.success(), "{refused:?}");
}

/// Alpine's static amd64 busybox, pinned: what the Rosetta test runs.
const AMD64_BUSYBOX: (&str, &str) = (
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/x86_64/busybox-static-1.37.0-r31.apk",
    "b0d6cfc585d7fdd12df359cc5d38d5e45894e9e9e78c4059eb41577d022ec734",
);

/// Alpine's static arm64 busybox, pinned: an image of the VM's own platform
/// that no registry's pull is needed for (the file sharing tests').
const ARM64_BUSYBOX: (&str, &str) = (
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/busybox-static-1.37.0-r31.apk",
    "965777e06b94bf11981d5f4ecdfcd577879f4b0dda544294d1dd3f72b217bc75",
);

/// Step 13.3 (D13-2): an amd64 image runs in the VM through Rosetta. Made
/// from Alpine's static amd64 busybox (pinned by its sha256, fetched by the
/// Mac and kept under `target/`), imported as `linux/amd64`: it runs, under
/// the default seccomp profile, and says it's `x86_64`; `images` lists it,
/// `image inspect` says `amd64`, and the guest's `doctor` says Rosetta is
/// registered. Skipped (said) where this Mac has no Rosetta.
#[test]
fn an_amd64_image_runs_through_rosetta() {
    let Some(vm) = Vm::new_in_home("rosetta") else {
        return;
    };
    let doctor = text(&vm.shipyard(&["doctor"]).stdout);
    if !doctor.contains("ok    rosetta") {
        assert!(
            std::env::var("SHIPYARD_TEST_ROSETTA").as_deref() != Ok("required"),
            "Rosetta is required here (SHIPYARD_TEST_ROSETTA): {doctor}"
        );
        eprintln!("skipping: this Mac has no Rosetta for Linux: {doctor}");
        return;
    }
    let scratch = vm_support::Scratch::in_home("rosetta");
    let tar = busybox_root(&scratch, AMD64_BUSYBOX);
    let imported = vm.shipyard(&[
        "import",
        "--platform",
        "linux/amd64",
        "--change",
        r#"CMD ["/bin/busybox", "uname", "-m"]"#,
        tar.to_str().unwrap(),
        "amd64-busybox:1",
    ]);
    assert!(imported.status.success(), "{imported:?}");
    let ran = vm.shipyard(&["run", "--rm", "amd64-busybox:1"]);
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(text(&ran.stdout), "x86_64\n");
    // Its own platform asked for: the same image, no pull.
    let asked = vm.shipyard(&[
        "run",
        "--rm",
        "--platform",
        "linux/amd64",
        "amd64-busybox:1",
        "/bin/busybox",
        "echo",
        "asked",
    ]);
    assert_eq!(text(&asked.stdout), "asked\n", "{asked:?}");
    let images = text(
        &vm.shipyard(&["images", "--format", "{{.Repository}}"])
            .stdout,
    );
    assert!(images.contains("amd64-busybox"), "{images}");
    let arch = vm.shipyard(&[
        "image",
        "inspect",
        "--format",
        "{{.Architecture}}",
        "amd64-busybox:1",
    ]);
    assert_eq!(text(&arch.stdout), "amd64\n", "{arch:?}");
    let guest = text(&vm.shipyard(&["doctor"]).stdout);
    assert!(
        guest.contains("registered: amd64 images run through it"),
        "{guest}"
    );
}

/// A root filesystem of one of Alpine's static busyboxes alone
/// (`bin/busybox`, the applets the tests use links to it), as a tar in `scratch`: the
/// busybox (`pinned`: its URL and sha256) fetched once by the Mac and kept
/// under `target/`.
fn busybox_root(scratch: &vm_support::Scratch, pinned: (&str, &str)) -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixtures = repo.join("target/vm-test-fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    // Named by its sum too: the arm64 and amd64 packages have one name.
    let apk = fixtures.join(format!(
        "{}-{}",
        &pinned.1[..12],
        pinned.0.rsplit('/').next().unwrap()
    ));
    let sum = |path: &Path| {
        let out = Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
            .unwrap();
        text(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    if !apk.is_file() || sum(&apk) != pinned.1 {
        let fetched = Command::new("curl")
            .args(["-fsS", "--max-time", "120", "-o"])
            .arg(&apk)
            .arg(pinned.0)
            .status()
            .unwrap();
        assert!(
            fetched.success(),
            "fetching {} (a newer release replaced it? pin the new one)",
            pinned.0
        );
    }
    assert_eq!(sum(&apk), pinned.1, "the pinned busybox");
    // A root filesystem of it alone: bin/busybox, and its applets' links.
    let root = scratch.0.join("root");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let unpacked = Command::new("tar")
        .arg("-xzf")
        .arg(&apk)
        .arg("-C")
        .arg(&scratch.0)
        .arg("bin/busybox.static")
        .status()
        .unwrap();
    assert!(unpacked.success());
    std::fs::rename(
        scratch.0.join("bin/busybox.static"),
        root.join("bin/busybox"),
    )
    .unwrap();
    for applet in [
        "sh", "cat", "cut", "ip", "ln", "ls", "nc", "rmdir", "true", "wc",
    ] {
        std::os::unix::fs::symlink("busybox", root.join("bin").join(applet)).unwrap();
    }
    let tar = scratch.0.join("root.tar");
    assert!(
        Command::new("tar")
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(&root)
            .arg(".")
            .status()
            .unwrap()
            .success()
    );
    tar
}

/// The arm64 busybox imported into `vm`'s store as `IMAGE`'s stand-in:
/// `share-busybox:1`, its name returned.
fn local_busybox(vm: &Vm) -> &'static str {
    let scratch = vm_support::Scratch::in_home("busybox");
    let tar = busybox_root(&scratch, ARM64_BUSYBOX);
    let imported = vm.shipyard(&["import", tar.to_str().unwrap(), "share-busybox:1"]);
    assert!(imported.status.success(), "{imported:?}");
    "share-busybox:1"
}

/// Step 13.4 (D13.4-1): `/tmp` is the Mac's. A file the Mac wrote there is
/// read in a container that binds it by `/tmp`'s path (the control: the
/// bind is the Mac's, not an empty one the VM made), and one the container
/// writes is on the Mac.
#[test]
fn a_bind_of_tmp_is_the_macs() {
    let Some(vm) = Vm::new("tmp") else {
        return;
    };
    let image = local_busybox(&vm);
    let scratch = vm_support::Scratch::in_tmp("tmp-bind");
    std::fs::write(scratch.0.join("mac.txt"), "from the Mac\n").unwrap();
    let bind = format!("{}:/w", scratch.0.display());
    let ran = vm.shipyard(&[
        "run",
        "--rm",
        "--volume",
        &bind,
        image,
        "sh",
        "-c",
        "cat /w/mac.txt && echo from the VM > /w/vm.txt",
    ]);
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(text(&ran.stdout), "from the Mac\n");
    assert_eq!(
        std::fs::read_to_string(scratch.0.join("vm.txt")).unwrap(),
        "from the VM\n"
    );
}

/// Step 13.4 (F143): Docker's API refuses a bind outside what the Mac
/// shares, as Docker Desktop does ("mounts denied"), and one a link in the
/// home takes to the VM's own files; a bind in the home is made (the
/// control: the same request, another source).
#[test]
fn a_bind_outside_the_shares_is_refused() {
    use shipyard_test_support::api;
    let Some(vm) = Vm::new("denied") else {
        return;
    };
    let image = local_busybox(&vm);
    let scratch = vm_support::Scratch::in_home("denied");
    std::os::unix::fs::symlink("/var/lib/shipyard", scratch.0.join("guest-state")).unwrap();
    let socket = vm.docker_socket();
    api::wait_for(&socket, Duration::from_secs(60));
    let create = |source: &Path| {
        let body = serde_json::json!({
            "Image": image,
            "Cmd": ["true"],
            "HostConfig": { "Binds": [format!("{}:/w", source.display())] },
        });
        api::post(&socket, "/v1.43/containers/create", Some(&body)).unwrap()
    };
    let made = create(&scratch.0);
    assert_eq!(made.status, 201, "{}", made.text());
    for refused in [
        Path::new("/opt/shipyard-denied-test"),
        &scratch.0.join("guest-state"),
        &scratch.0.join("guest-state/new"),
    ] {
        let answer = create(refused);
        assert_eq!(
            answer.status,
            400,
            "{}: {}",
            refused.display(),
            answer.text()
        );
        assert!(
            answer.text().contains("mounts denied: the path"),
            "{}",
            answer.text()
        );
    }
    // Judged again at a start: a directory in the home that a container
    // which binds the home made a link to the VM's own files since the
    // create is refused, not bound. (Made through the guest, as such a
    // container would: one the Mac made is seen by the guest only once its
    // cache of the old directory lapses.)
    let swapped = scratch.0.join("swapped");
    std::fs::create_dir(&swapped).unwrap();
    let bind = format!("{}:/w", swapped.display());
    let name = format!("swap-{}", std::process::id());
    let created = vm.shipyard(&["create", "--name", &name, "--volume", &bind, image, "true"]);
    assert!(created.status.success(), "{created:?}");
    let ran = vm.shipyard(&["start", "-a", &name]);
    assert!(ran.status.success(), "the control: {ran:?}");
    let home_bind = format!("{}:/d", scratch.0.display());
    let swap = vm.shipyard(&[
        "run",
        "--rm",
        "--volume",
        &home_bind,
        image,
        "sh",
        "-c",
        "rmdir /d/swapped && ln -s /var/lib/shipyard /d/swapped",
    ]);
    assert!(swap.status.success(), "{swap:?}");
    let refused = vm.shipyard(&["start", "-a", &name]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        text(&refused.stderr).contains("mounts denied"),
        "{refused:?}"
    );
    assert!(vm.shipyard(&["rm", &name]).status.success());
}

/// Step 13.4: the VM's own places stay hidden in every share. A VM whose
/// directory is under `/tmp` (as these tests' are): a container binding
/// `/tmp` sees that directory empty while the rest of `/tmp` is there (the
/// control: a file the Mac wrote beside it), and a bind of the directory
/// itself, or of it named in another case, is refused.
#[test]
fn the_vms_own_places_stay_hidden() {
    let Some(vm) = Vm::new("hide") else {
        return;
    };
    let image = local_busybox(&vm);
    let scratch = vm_support::Scratch::in_tmp("hide");
    std::fs::write(scratch.0.join("beside.txt"), "x").unwrap();
    let real = std::fs::canonicalize(&vm.dir).unwrap();
    let inside = |path: &Path| {
        Path::new("/t").join(
            path.strip_prefix("/private/tmp")
                .or_else(|_| path.strip_prefix("/tmp"))
                .unwrap(),
        )
    };
    // Its name in another case too: the Mac's file system finds it by
    // either (the hiding must hold for both).
    let other_case = PathBuf::from(
        inside(&real)
            .to_str()
            .unwrap()
            .replace("shipyard-vm-test", "SHIPYARD-VM-TEST"),
    );
    let script = format!(
        "test -d {0} && ls -A {0} | wc -l; test -d {2} && ls -A {2} | wc -l; \
         test -f {1} && echo beside",
        inside(&real).display(),
        inside(&scratch.0.join("beside.txt")).display(),
        other_case.display()
    );
    let ran = vm.shipyard(&[
        "run", "--rm", "--volume", "/tmp:/t", image, "sh", "-c", &script,
    ]);
    assert!(ran.status.success(), "{ran:?}");
    let said: Vec<String> = text(&ran.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect();
    assert_eq!(said, ["0", "0", "beside"], "{ran:?}");
    let shouted = PathBuf::from(
        real.to_str()
            .unwrap()
            .replace("shipyard-vm-test", "SHIPYARD-VM-TEST"),
    );
    for own in [real.clone(), real.join("data.img"), shouted] {
        let bind = format!("{}:/v", own.display());
        let refused = vm.shipyard(&["run", "--rm", "--volume", &bind, image, "true"]);
        assert!(!refused.status.success(), "{}: {refused:?}", own.display());
        assert!(
            text(&refused.stderr).contains("mounts denied"),
            "{}: {refused:?}",
            own.display()
        );
    }
}

/// Step 13.4 (D13.4-2, H7): small files written through a bind of the home
/// stay within a bound: 2,000 in under 2 s (13.1 measured 10,000 in 2.45 s,
/// so ~0.5 s; four times that), timed in the guest by its uptime.
#[test]
fn small_files_on_a_share_stay_within_a_bound() {
    let Some(vm) = Vm::new("files") else {
        return;
    };
    let image = local_busybox(&vm);
    let scratch = vm_support::Scratch::in_home("files");
    let bind = format!("{}:/w", scratch.0.display());
    let script = "s=$(cut -d' ' -f1 /proc/uptime); i=0; \
                  while [ $i -lt 2000 ]; do echo x > /w/f$i; i=$((i+1)); done; \
                  e=$(cut -d' ' -f1 /proc/uptime); echo $s $e";
    let ran = vm.shipyard(&["run", "--rm", "--volume", &bind, image, "sh", "-c", script]);
    assert!(ran.status.success(), "{ran:?}");
    let out = text(&ran.stdout);
    let times: Vec<f64> = out
        .split_whitespace()
        .map(|time| time.parse().unwrap())
        .collect();
    assert_eq!(times.len(), 2, "{out}");
    let took = times[1] - times[0];
    eprintln!("2,000 small files through the share: {took:.2} s");
    assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 2000);
    assert!(took < 2.0, "2,000 small files took {took:.2} s");
}

/// Step 13.5a (F96): a UDP port a container publishes answers on the Mac's
/// loopback (an echo: `nc -u` with `cat`), `vm status` says it's forwarded,
/// and a datagram's round trip is quick once it is (H7: under a second).
#[test]
fn a_udp_port_answers_on_the_macs_loopback() {
    let Some(vm) = Vm::new("udp") else {
        return;
    };
    let image = local_busybox(&vm);
    let port = free_port();
    let publish = format!("{port}:7777/udp");
    let ran = vm.shipyard(&[
        "run", "-d", "--name", "echo-udp", "-p", &publish, image, "nc", "-lu", "-p", "7777", "-e",
        "/bin/cat",
    ]);
    assert!(ran.status.success(), "{ran:?}");
    let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut reply = [0u8; 64];
    // Each datagram numbered, and read until its own answer comes (an
    // earlier one's may still be on its way).
    let mut asked = 0u32;
    let mut ask = |wait: Duration| -> Option<Duration> {
        asked += 1;
        let sent = format!("ping {asked}\n");
        let at = Instant::now();
        client.send_to(sent.as_bytes(), address).unwrap();
        while at.elapsed() < wait {
            if let Ok(got) = client.recv(&mut reply)
                && reply[..got] == *sent.as_bytes()
            {
                return Some(at.elapsed());
            }
        }
        None
    };
    // Forwarded within the poll (2 s): asked until it answers.
    let began = Instant::now();
    while ask(Duration::from_millis(500)).is_none() {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "no answer: {}",
            text(&vm.shipyard(&["vm", "status"]).stdout)
        );
    }
    // H7: once forwarded, a round trip within a second.
    let took = ask(Duration::from_secs(5)).expect("an answer once forwarded");
    assert!(took < Duration::from_secs(1), "{took:?}");
    let status = text(&vm.shipyard(&["vm", "status"]).stdout);
    assert!(
        status.contains(&format!("127.0.0.1:{port}/udp")),
        "{status}"
    );
    assert!(vm.shipyard(&["rm", "-f", "echo-udp"]).status.success());
}

/// Step 13.5a (F97): a TCP port is reached through the container's own
/// address (its lease's), not the guest's DNAT, on both of the Mac's
/// loopbacks: an echo (`nc -lk` with `cat`) answers each, byte for byte, and
/// the agent never says it dialled through the guest's own address.
#[test]
fn a_tcp_port_is_reached_at_the_containers_own_address() {
    use std::io::{Read as _, Write as _};
    let Some(vm) = Vm::new("tcpdial") else {
        return;
    };
    let image = local_busybox(&vm);
    let port = free_port();
    let publish = format!("{port}:80");
    let ran = vm.shipyard(&[
        "run", "-d", "--name", "echo-tcp", "-p", &publish, image, "nc", "-lk", "-p", "80", "-e",
        "/bin/cat",
    ]);
    assert!(ran.status.success(), "{ran:?}");
    for ip in [
        std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
        std::net::Ipv6Addr::LOCALHOST.into(),
    ] {
        let address = std::net::SocketAddr::new(ip, port);
        let began = Instant::now();
        let mut stream = loop {
            if let Ok(stream) =
                std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
            {
                break stream;
            }
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "{address} never forwarded: {}",
                text(&vm.shipyard(&["vm", "status"]).stdout)
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(b"echoed\n").unwrap();
        let mut echoed = [0u8; 7];
        stream.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"echoed\n", "{address}");
    }
    // Dialled at the container: the agent never fell back to the guest's
    // DNAT, which it says on the console the first time it does.
    let console = vm.console();
    assert!(
        !console.contains("dialled through the guest's own address"),
        "{console}"
    );
    assert!(vm.shipyard(&["rm", "-f", "echo-tcp"]).status.success());
}

/// Step 13.5b (F108): a published port doesn't answer on the VM's own
/// address on the Mac's virtual network (`eth0`'s, which the Mac and other
/// VMs on it reach), while `localhost` reaches it, and a host-network
/// container is reached on that address (the controls).
#[test]
fn a_published_port_doesnt_answer_on_the_vms_own_address() {
    use std::io::{Read as _, Write as _};
    let Some(vm) = Vm::new("outside") else {
        return;
    };
    let image = local_busybox(&vm);
    let port = free_port();
    let publish = format!("{port}:80");
    let ran = vm.shipyard(&[
        "run", "-d", "--name", "echo-out", "-p", &publish, image, "nc", "-lk", "-p", "80", "-e",
        "/bin/cat",
    ]);
    assert!(ran.status.success(), "{ran:?}");
    // The VM's own address, as a container on its network sees it.
    let shown = vm.shipyard(&[
        "run",
        "--rm",
        "--network",
        "host",
        image,
        "ip",
        "-4",
        "-o",
        "addr",
        "show",
        "eth0",
    ]);
    assert!(shown.status.success(), "{shown:?}");
    let ip: std::net::Ipv4Addr = text(&shown.stdout)
        .split_whitespace()
        .skip_while(|word| *word != "inet")
        .nth(1)
        .and_then(|cidr| cidr.split('/').next()?.parse().ok())
        .unwrap_or_else(|| panic!("{shown:?}"));
    let echoed = |address: std::net::SocketAddr| -> bool {
        let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))
        else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        let mut echoed = [0u8; 3];
        stream.write_all(b"hi\n").is_ok()
            && stream.read_exact(&mut echoed).is_ok()
            && &echoed == b"hi\n"
    };
    // The control: forwarded within the poll.
    let local = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let began = Instant::now();
    while !echoed(local) {
        assert!(began.elapsed() < Duration::from_secs(20), "never forwarded");
        std::thread::sleep(Duration::from_millis(200));
    }
    // The control on that address: a container on the VM's own network
    // (`--network host`, not a published port: F175) is reached there, so
    // the Mac does reach the address.
    let other = free_port();
    let listened = vm.shipyard(&[
        "run",
        "-d",
        "--name",
        "echo-host",
        "--network",
        "host",
        image,
        "nc",
        "-lk",
        "-p",
        &other.to_string(),
        "-e",
        "/bin/cat",
    ]);
    assert!(listened.status.success(), "{listened:?}");
    let began = Instant::now();
    while !echoed(std::net::SocketAddr::from((ip, other))) {
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "the VM's address {ip} is unreachable from the Mac: no proof"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !echoed(std::net::SocketAddr::from((ip, port))),
        "the VM's own address {ip} answers port {port}"
    );
    assert!(
        vm.shipyard(&["rm", "-f", "echo-out", "echo-host"])
            .status
            .success()
    );
}

/// Step 13.5b (F98): a Mac program that binds every address of a port the
/// VM forwards, after it's forwarded (with `SO_REUSEADDR`, as most servers
/// do), is said by `vm status`: `localhost` still reaches the container.
#[test]
fn a_mac_program_on_a_forwarded_port_is_said() {
    let Some(vm) = Vm::new("shadow") else {
        return;
    };
    let image = local_busybox(&vm);
    let port = free_port();
    let publish = format!("{port}:80");
    let ran = vm.shipyard(&[
        "run",
        "-d",
        "--name",
        "echo-shadow",
        "-p",
        &publish,
        image,
        "nc",
        "-lk",
        "-p",
        "80",
        "-e",
        "/bin/cat",
    ]);
    assert!(ran.status.success(), "{ran:?}");
    let forwarded = format!("127.0.0.1:{port}");
    let began = Instant::now();
    while !text(&vm.shipyard(&["vm", "status"]).stdout).contains(&forwarded) {
        assert!(began.elapsed() < Duration::from_secs(20), "never forwarded");
        std::thread::sleep(Duration::from_millis(200));
    }
    let shadow = format!("shadowed: {port}:");
    assert!(
        !text(&vm.shipyard(&["vm", "status"]).stdout).contains(&shadow),
        "the control"
    );
    // std's listener sets `SO_REUSEADDR`.
    let theirs = std::net::TcpListener::bind(("0.0.0.0", port)).unwrap();
    let status = text(&vm.shipyard(&["vm", "status"]).stdout);
    assert!(status.contains(&shadow), "{status}");
    drop(theirs);
    assert!(vm.shipyard(&["rm", "-f", "echo-shadow"]).status.success());
}

/// Step 13.6a (F94): a command that reads no input still gets the Mac's
/// signals while its input is stuck. Input written to `run -i`'s stdin
/// until it stops moving fills every buffer on the way (the control: it
/// stops, and its writer is blocked when the SIGINT is sent), and the
/// SIGINT, on the command's control connection, reaches its trap anyway.
#[test]
fn a_signal_reaches_a_command_that_reads_no_input() {
    use std::sync::atomic::Ordering::SeqCst;
    let Some(vm) = Vm::new("stuck") else {
        return;
    };
    let image = local_busybox(&vm);
    let mut command = vm.command(&[
        "run",
        "--rm",
        "-i",
        image,
        "sh",
        "-c",
        // (The local image links few applets: `sleep` is busybox's.)
        "trap 'exit 42' INT; echo ready; busybox sleep 600 & wait",
    ]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    with_sigint(&mut command);
    let mut child = command.spawn().unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut out, &mut line).unwrap();
    assert_eq!(line, "ready\n");
    let written = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut stdin = child.stdin.take().unwrap();
    let theirs = std::sync::Arc::clone(&written);
    std::thread::spawn(move || {
        let chunk = vec![b'x'; 16 * 1024];
        // (Up to 1 GiB: a reader that takes it all fails the test below.)
        for _ in 0..65_536 {
            if stdin.write_all(&chunk).is_err() {
                return;
            }
            theirs.fetch_add(chunk.len(), SeqCst);
        }
    });
    // Stuck: nothing more taken for 3 s.
    let began = Instant::now();
    let (mut last, mut since) = (0, Instant::now());
    while since.elapsed() < Duration::from_secs(3) {
        assert!(
            began.elapsed() < Duration::from_secs(90),
            "the input never got stuck ({} bytes taken): the test proves nothing",
            written.load(SeqCst)
        );
        std::thread::sleep(Duration::from_millis(250));
        let now = written.load(SeqCst);
        if now != last {
            (last, since) = (now, Instant::now());
        }
    }
    eprintln!("({last} bytes taken before the input stuck)");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let began = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if began.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!(
                "the SIGINT never reached the command; the host's log:\n{}",
                std::fs::read_to_string(vm.dir.join("host.log")).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut out, &mut rest).unwrap();
    assert_eq!(
        status.code(),
        Some(42),
        "stdout after ready: {rest:?}; stderr: {stderr}"
    );
}

/// Step 13.6a (F173b): the host keeps what it booted (`booted.json`: its
/// PID, the guest's build); `vm status` and `doctor` say when the running
/// VM booted another build's guest than the guest directory's (here: the
/// file rewritten to say so), and nothing for this one's (the control).
#[test]
fn another_builds_vm_is_said() {
    let Some(vm) = Vm::new("booted") else {
        return;
    };
    let started = vm.shipyard(&["vm", "start"]);
    assert!(started.status.success(), "{started:?}");
    let path = vm.dir.join("booted.json");
    let booted: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(vm.guest().join("manifest.json")).unwrap()).unwrap();
    assert_eq!(booted["revision"], manifest["shipyard"]["revision"]);
    assert_eq!(booted["sha256"], manifest["shipyard"]["sha256"]);
    let said = "the running VM is another build's (its guest: shipyard older)";
    assert!(!text(&vm.shipyard(&["vm", "status"]).stdout).contains("another build's"));
    let mut older = booted.clone();
    older["revision"] = "older".into();
    std::fs::write(&path, older.to_string()).unwrap();
    let status = text(&vm.shipyard(&["vm", "status"]).stdout);
    assert!(status.contains(said), "{status}");
    let doctor = vm.shipyard(&["doctor"]);
    assert!(text(&doctor.stdout).contains(said), "{doctor:?}");
}

/// Step 13.6b1 (F93, F106): `vm stop` asks the host on its own socket, never
/// signals a PID, and a stop sent while the VM starts succeeds: taken once
/// the guest is up (its disk check never cut), the guest stopped in order.
/// The control: a stop of a VM that's up is said the same way.
#[test]
fn a_stop_during_a_start_succeeds() {
    let Some(vm) = Vm::new("stop-start") else {
        return;
    };
    let mut starting = vm.command(&["vm", "start"]);
    starting
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let starting = starting.spawn().unwrap();
    // The host's own socket is there from its lock on, booting or not.
    let began = Instant::now();
    while !vm.dir.join("host.sock").exists() {
        assert!(began.elapsed() < Duration::from_secs(30), "no host.sock");
        std::thread::sleep(Duration::from_millis(20));
    }
    let stopped = vm.shipyard(&["vm", "stop"]);
    assert!(stopped.status.success(), "{stopped:?}");
    // The stop came later, so it wins: the start says it was stopped (or it
    // had finished first), and starts no other VM.
    let started = vm_support::finish_within(starting, vm_support::COMMAND_LIMIT, "starting");
    assert!(
        started.status.success() || text(&started.stderr).contains("the VM is stopping"),
        "the start: {started:?}"
    );
    let status = vm.shipyard(&["vm", "status"]);
    assert!(text(&status.stdout).starts_with("stopped"), "{status:?}");
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    assert!(log.contains("asked to stop over its own socket"), "{log}");
    assert!(
        !log.contains("stopped at once"),
        "the stop was forced:\n{log}"
    );
    // Before the guest started, or once it was up, in order: never midway.
    let before = log.contains("stopped before its guest started");
    assert!(
        before || vm.console().contains("stopped; powering off"),
        "{log}\n{}\nthe start: {started:?}",
        vm.console()
    );

    let up = vm.shipyard(&["vm", "start"]);
    assert!(up.status.success(), "{up:?}");
    let before = std::fs::read_to_string(vm.dir.join("host.log"))
        .unwrap()
        .len();
    let stopped = vm.shipyard(&["vm", "stop"]);
    assert!(stopped.status.success(), "{stopped:?}");
    // (Only what this stop wrote: the first half's lines say much the same.)
    let log = std::fs::read_to_string(vm.dir.join("host.log")).unwrap();
    let this_stop = log.get(before..).unwrap_or_default();
    assert!(
        this_stop.contains("asked over its own socket: stopping the guest\n"),
        "{this_stop}"
    );
}

/// A child that ends within its limit is read whole; one past it is
/// killed and fails its test (F132: no VM test waits for good).
#[test]
fn a_child_past_its_limit_is_killed() {
    let echo = Command::new("/bin/echo")
        .arg("said")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let said = vm_support::finish_within(echo, std::time::Duration::from_secs(10), "echo");
    assert_eq!(text(&said.stdout), "said\n");
    let sleep = Command::new("/bin/sleep")
        .arg("30")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    let failed = std::panic::catch_unwind(|| {
        vm_support::finish_within(sleep, std::time::Duration::from_millis(300), "sleep")
    });
    assert!(failed.is_err(), "it wasn't failed");
    assert!(
        began.elapsed() < std::time::Duration::from_secs(10),
        "it wasn't killed"
    );
}
