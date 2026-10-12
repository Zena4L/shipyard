//! End-to-end tests of rootless containers (stage 4, step 5): an isolated
//! `shipyard run` by a user without root (Linux).
//!
//! These run as a **normal user**, not root: in the normal-user pass of
//! `scripts/test-linux.sh` (as `tester`, whose image has the helpers and a
//! subordinate range) and in CI's Linux job. Where rootless can't work (as
//! root, user namespaces disabled, no helpers), each test says why and
//! returns, except with `SHIPYARD_TEST_ROOTLESS=required`, where that fails.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::Command;

use shipyard_test_support::rootfs::RootFs;
use shipyard_test_support::{Rootless, Run, Shipyard, for_this_kernel, killed_by, rootless, run};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `shipyard run ARGS… sh -c SCRIPT`.
fn shipyard_run(args: &[&str], script: &str) -> Run {
    run(SHIPYARD.command(&[&["run"], args, &["sh", "-c", script]].concat()))
}

/// `shipyard run ARGS…`, as given.
fn run_plain(args: &[&str]) -> Run {
    run(SHIPYARD.command(&[&["run"], args].concat()))
}

/// The same, which must succeed: its output.
fn ok(args: &[&str], script: &str) -> String {
    let run = shipyard_run(args, script);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    run.stdout
}

/// The same, with a `PATH` that has no `newuidmap`: the single-ID map.
fn without_helpers(args: &[&str], script: &str) -> Run {
    // The host's network: pasta is on the PATH taken away.
    let mut command =
        SHIPYARD.command(&[&["run", "--network", "host"], args, &["sh", "-c", script]].concat());
    command.env("PATH", "/nonexistent");
    run(command)
}

/// The caller's user and group.
fn me() -> (u32, u32) {
    // SAFETY: `getuid` and `getgid` have no preconditions.
    unsafe { (libc::getuid(), libc::getgid()) }
}

/// This user's first subordinate uid, read from `/etc/subuid` here rather
/// than through the runtime's parser: the test checks that parser's work.
fn first_subordinate_uid() -> u32 {
    first_subordinate("/etc/subuid")
}

/// This user's first subordinate gid (`/etc/subgid`: its range needn't
/// start where the uids' does).
fn first_subordinate_gid() -> u32 {
    first_subordinate("/etc/subgid")
}

fn first_subordinate(file: &str) -> u32 {
    let name = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let uid = me().0.to_string();
    std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let owner = fields.next()?;
            (owner == name.trim() || owner == uid).then(|| fields.next()?.parse().ok())?
        })
        .unwrap_or_else(|| panic!("a range in {file}"))
}

/// A world-writable directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("shipyard-rootless-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// With the helpers and a subordinate range: 0 is the caller, then 65536
/// IDs from the range; the command is root inside.
#[test]
fn the_full_map_puts_you_at_root() {
    if !rootless(Rootless::Full) {
        return;
    }
    let (uid, gid) = me();
    let first = first_subordinate_uid();
    let out = ok(
        &[],
        "cat /proc/self/uid_map /proc/self/gid_map; id -u; id -g",
    );
    let words: Vec<u32> = out.split_whitespace().map(|w| w.parse().unwrap()).collect();
    assert_eq!(&words[..3], [0, uid, 1], "{out}");
    assert_eq!(&words[3..5], [1, first], "{out}");
    assert_eq!(&words[6..9], [0, gid, 1], "{out}");
    assert_eq!(&words[words.len() - 2..], [0, 0], "root inside: {out}");
}

/// Root inside is the caller outside; `--user 1000` is a subordinate ID.
#[test]
fn files_belong_to_you_and_your_range() {
    if !rootless(Rootless::Full) {
        return;
    }
    let scratch = Scratch::new("owned");
    let (as_root, as_1000) = (scratch.file("root"), scratch.file("user"));
    ok(&[], &format!("touch {as_root}"));
    ok(&["--user", "1000:1000"], &format!("touch {as_1000}"));
    let owner = |path: &str| std::fs::metadata(path).unwrap().uid();
    assert_eq!(owner(&as_root), me().0);
    assert_eq!(owner(&as_1000), first_subordinate_uid() + 999);
}

/// Without the helpers: only the caller's IDs, `setgroups` denied, a warning
/// saying so, and other users refused by name.
#[test]
fn without_helpers_only_root_inside_is_mapped() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = without_helpers(&[], "cat /proc/self/uid_map /proc/self/setgroups; id -u");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("only root inside the container is mapped"),
        "{}",
        run.stderr
    );
    let words: Vec<&str> = run.stdout.split_whitespace().collect();
    assert_eq!(
        words,
        ["0", &me().0.to_string(), "1", "deny", "0"],
        "{}",
        run.stdout
    );
    let other = without_helpers(&["--user", "1000"], "true");
    assert_eq!(other.code, Some(125), "{}", other.stderr);
    assert!(
        other.stderr.contains("user 1000 isn't mapped"),
        "{}",
        other.stderr
    );
    // Group 0 is mapped, but the group list can't change: refused, not
    // silently left out.
    let groups = without_helpers(&["--group-add", "0"], "true");
    assert_eq!(groups.code, Some(125), "{}", groups.stderr);
    assert!(
        groups.stderr.contains("supplementary group 0 can't be set")
            && groups.stderr.contains("denies setgroups"),
        "{}",
        groups.stderr
    );
}

/// `shipyard run ARGS…` in a user namespace of the caller's own, which
/// denies `setgroups`, as do the namespaces made inside it: as its root
/// (`unshare -U -r`), or as the caller (`map`: `--map-current-user`).
fn in_a_namespace_denying_setgroups(map: &str, args: &[&str]) -> Run {
    let mut command = Command::new("unshare");
    command
        .args(["--user", map, SHIPYARD.path()])
        .args(for_this_kernel(&[&["run"], args].concat()));
    run(command)
}

/// Inside a namespace that denies `setgroups` (inherited: shipyard didn't
/// write it), shipyard is root and runs as root does, but never calls
/// `setgroups`: the command keeps its groups, and asking for others is
/// refused, isolated or not, with a user namespace of its own or not.
#[test]
fn an_inherited_setgroups_deny_is_respected() {
    if !rootless(Rootless::Single) {
        return;
    }
    // The outer namespace is `unshare`'s, not Shipyard's: where AppArmor
    // restricts user namespaces (Ubuntu's default), only Shipyard's own
    // binaries are allowed them, and this can't be set up.
    let unshare = Command::new("unshare")
        .args(["--user", "--map-root-user", "true"])
        .stderr(std::process::Stdio::null())
        .status();
    if !unshare.is_ok_and(|status| status.success()) {
        eprintln!("skipping: `unshare` can't map a user namespace here (AppArmor's restriction)");
        return;
    }
    // `--uidmap` too: a user namespace made inside inherits the denial, and
    // its root must be become without `setgroups`.
    for isolation in [&[][..], &["--no-isolation"], &["--uidmap", "0:0:1"]] {
        let run =
            in_a_namespace_denying_setgroups("--map-root-user", &[isolation, &["true"]].concat());
        assert_eq!(run.code, Some(0), "{isolation:?}: {}", run.stderr);
        let groups = in_a_namespace_denying_setgroups(
            "--map-root-user",
            &[isolation, &["--group-add", "0", "true"]].concat(),
        );
        assert_eq!(groups.code, Some(125), "{isolation:?}: {}", groups.stderr);
        assert!(
            groups.stderr.contains("supplementary group 0 can't be set"),
            "{isolation:?}: {}",
            groups.stderr
        );
    }
    // Not root there: adding groups needs root, which is said first.
    let caller = in_a_namespace_denying_setgroups(
        "--map-current-user",
        &["--no-isolation", "--group-add", "0", "true"],
    );
    assert_eq!(caller.code, Some(125), "{}", caller.stderr);
    assert!(caller.stderr.contains("needs root"), "{}", caller.stderr);
}

/// A rootless container gets a session keyring of its own too: the caller's
/// login keys stay out of reach.
#[test]
fn a_rootless_container_has_its_own_session_keyring() {
    if !rootless(Rootless::Single) {
        return;
    }
    let print = shipyard_test_support::print_session_keyring();
    let run = run_plain(&[&["--security-opt", "seccomp=unconfined"][..], &print].concat());
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let inside: i64 = run.stdout.trim().parse().unwrap();
    assert!(inside > 0, "{inside}");
    assert_ne!(inside, shipyard_test_support::session_keyring());
}

/// A root filesystem the caller owns is root's inside: writable.
#[test]
fn your_root_filesystem_is_roots_inside() {
    if !rootless(Rootless::Single) {
        return;
    }
    let root = RootFs::minimal();
    let out = ok(
        &["--rootfs", root.arg(), "--hostname", "box"],
        "touch /made-inside && echo written; cat /proc/sys/kernel/hostname; \
         echo x > /dev/null && echo null-ok; touch /sys/x 2>&1; true",
    );
    for expected in ["written", "box", "null-ok", "Read-only file system"] {
        assert!(out.contains(expected), "{expected}: {out}");
    }
    let made = std::fs::metadata(root.host_path("/made-inside")).unwrap();
    assert_eq!(made.uid(), me().0);
}

/// Docker's capabilities (inside the namespace), `no_new_privs`, and the
/// seccomp filter, as rootful.
#[test]
fn confinement_holds_rootless() {
    if !rootless(Rootless::Single) {
        return;
    }
    let status = ok(&[], "cat /proc/self/status");
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .unwrap()
            .trim()
            .to_owned()
    };
    assert_eq!(
        u64::from_str_radix(&field("CapEff"), 16).unwrap(),
        shipyard_runtime::security::CapabilitySet::DOCKER_DEFAULT.bits()
    );
    assert_eq!(field("NoNewPrivs"), "1");
    if !shipyard_test_support::under_rosetta() {
        assert_eq!(field("Seccomp"), "2");
    }
    // Inside, root's capabilities don't reach the host's files.
    let shadow = shipyard_run(&[], "cat /etc/shadow");
    assert_ne!(shadow.code, Some(0));
}

#[test]
fn exit_codes_and_signals_come_through() {
    if !rootless(Rootless::Single) {
        return;
    }
    assert_eq!(shipyard_run(&[], "exit 7").code, Some(7));
    assert_eq!(
        shipyard_run(&[], "kill -TERM $$").code,
        Some(killed_by(libc::SIGTERM))
    );
}

/// The maps are chosen for a rootless caller: `--uidmap` is root's.
/// `--no-isolation` still runs as the caller.
#[test]
fn uidmap_is_roots_and_no_isolation_is_you() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = shipyard_run(&["--uidmap", "0:100000:65536"], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("need root"), "{}", run.stderr);
    let plain = run_plain(&["--no-isolation", "id", "-u"]);
    assert_eq!(plain.stdout.trim(), me().0.to_string());
}

/// A directory of stand-in `newuidmap`/`newgidmap` running `script`, put
/// first on `PATH` for one run. Removed on drop.
struct FakeHelpers(Scratch);

impl FakeHelpers {
    fn new(name: &str, script: &str) -> Self {
        let dir = Scratch::new(name);
        for helper in ["newuidmap", "newgidmap"] {
            let path = dir.0.join(helper);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self(dir)
    }

    fn run(&self, script: &str) -> Run {
        let mut command = SHIPYARD.command(&["run", "sh", "-c", script]);
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", self.0.0.display()));
        run(command)
    }
}

/// A helper stuck (on a hung file system, say) runs while every signal is
/// blocked: it's stopped after a deadline, and the run fails saying so.
#[test]
fn a_hung_helper_is_stopped() {
    if !rootless(Rootless::Full) {
        return;
    }
    let hung = FakeHelpers::new("hung", "sleep 600");
    let started = std::time::Instant::now();
    let run = hung.run("true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("didn't finish within"),
        "{}",
        run.stderr
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

/// A helper that writes more than a pipe holds before it fails isn't taken
/// for a hung one: its stderr is read while it runs. The message keeps the
/// start of it.
#[test]
fn a_chatty_helper_isnt_taken_for_hung() {
    if !rootless(Rootless::Full) {
        return;
    }
    let chatty = FakeHelpers::new(
        "chatty",
        "head -c 200000 /dev/zero | tr '\\0' x >&2; exit 1",
    );
    let started = std::time::Instant::now();
    let run = chatty.run("true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("xxxx") && run.stderr.contains("must be setuid root"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.len() < 10_000, "{} bytes", run.stderr.len());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// A helper that fails (not setuid, here a stand-in that only complains):
/// its words, and what to check.
#[test]
fn a_failing_helper_says_what_to_check() {
    if !rootless(Rootless::Full) {
        return;
    }
    let failing = FakeHelpers::new("failing", "echo 'write to uid_map failed' >&2; exit 1");
    let run = failing.run("true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("write to uid_map failed")
            && run.stderr.contains("must be setuid root"),
        "{}",
        run.stderr
    );
}

// --- OCI bundles ---------------------------------------------------------------

/// `shipyard run --bundle BUNDLE` with `script` and `change` made to the
/// config.
fn bundle_run(script: &str, change: impl FnOnce(&mut serde_json::Value)) -> Run {
    let bundle = shipyard_test_support::bundle::Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = serde_json::json!(["sh", "-c", script]);
    change(&mut config);
    bundle.write(&config);
    run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]))
}

/// Adds a user namespace and `uid`/`gid` mappings to a config.
/// Gives the bundle a user namespace with the uid `mappings`, and the same
/// for gids: the caller's uid is its gid there, and its first subordinate
/// uid its first subordinate gid (a user's gid needn't be its uid, nor its
/// ranges start together).
fn with_mappings(config: &mut serde_json::Value, mappings: &serde_json::Value) {
    config["linux"]["namespaces"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"type": "user"}));
    config["linux"]["uidMappings"] = mappings.clone();
    let mut gids = mappings.clone();
    for mapping in gids.as_array_mut().unwrap() {
        let host = mapping["hostID"].as_u64().unwrap();
        if host == u64::from(me().0) {
            mapping["hostID"] = me().1.into();
        } else if host != 0 && host == u64::from(first_subordinate_uid()) {
            mapping["hostID"] = first_subordinate_gid().into();
        }
    }
    config["linux"]["gidMappings"] = gids;
}

/// A bundle without a user namespace gets one, rootless: root inside is you.
#[test]
fn a_bundle_gets_a_user_namespace_rootless() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = bundle_run(
        "stat -L -c %u /proc/self; head -1 /proc/self/uid_map",
        |_| {},
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("one was added"), "{}", run.stderr);
    let words: Vec<&str> = run.stdout.split_whitespace().collect();
    assert_eq!(
        words,
        ["0", "0", &me().0.to_string(), "1"],
        "{}",
        run.stdout
    );
}

/// A bundle's own mappings: the caller's own ID alone (written directly),
/// or with the subordinate range (through the helpers).
#[test]
fn a_bundles_own_mappings_rootless() {
    if !rootless(Rootless::Full) {
        return;
    }
    let (uid, _) = me();
    let single = serde_json::json!([{"containerID": 0, "hostID": uid, "size": 1}]);
    let run = bundle_run("cat /proc/self/uid_map", |config| {
        with_mappings(config, &single);
    });
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(!run.stderr.contains("one was added"), "{}", run.stderr);
    assert_eq!(run.stdout.split_whitespace().count(), 3, "{}", run.stdout);

    let full = serde_json::json!([
        {"containerID": 0, "hostID": uid, "size": 1},
        {"containerID": 1, "hostID": first_subordinate_uid(), "size": 65536}
    ]);
    let run = bundle_run("cat /proc/self/uid_map", |config| {
        with_mappings(config, &full);
    });
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout.split_whitespace().count(), 6, "{}", run.stdout);

    // IDs that aren't the caller's: the helpers refuse.
    let others = serde_json::json!([{"containerID": 0, "hostID": 0, "size": 65536}]);
    let run = bundle_run("true", |config| {
        with_mappings(config, &others);
    });
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("newuidmap"), "{}", run.stderr);
}

/// The bundle's user must exist in its namespace.
#[test]
fn a_bundles_unmapped_user_is_refused() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = bundle_run("true", |config| {
        config["process"]["user"] = serde_json::json!({"uid": 1000, "gid": 1000});
        let single = serde_json::json!([{"containerID": 0, "hostID": me().0, "size": 1}]);
        with_mappings(config, &single);
    });
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("user 1000 isn't mapped"),
        "{}",
        run.stderr
    );
}

/// Rootless, the command's OOM score can only go up: lowering it needs
/// `CAP_SYS_RESOURCE`, which says so.
#[test]
fn a_rootless_command_can_only_raise_its_oom_score() {
    if !rootless(Rootless::Single) {
        return;
    }
    let raised = ok(&["--oom-score-adj", "1000"], "cat /proc/self/oom_score_adj");
    assert_eq!(raised, "1000\n");
    let lowered = shipyard_run(&["--oom-score-adj", "-1000"], "true");
    assert_eq!(lowered.code, Some(125), "{}", lowered.stderr);
    assert!(
        lowered.stderr.contains("CAP_SYS_RESOURCE"),
        "{}",
        lowered.stderr
    );
}

// --- Standard input and output (stage 8, step 8.2a) ---

/// `shipyard run ARGS… sh -c SCRIPT`, with `input` on its stdin.
fn run_fed(args: &[&str], script: &str, input: &[u8]) -> Run {
    shipyard_test_support::run_with_input(
        SHIPYARD.command(&[&["run"], args, &["sh", "-c", script]].concat()),
        Some(input),
    )
}

/// With `-i`, piped input reaches the command (as it is: a pipe can't be
/// used against a terminal, so it isn't relayed).
#[test]
fn with_i_piped_input_reaches_the_command() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = run_fed(&["-i"], "cat", b"hello\n");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "hello\n");
}

/// Without `-i`, an isolated command's stdin is empty (D17, as Docker's):
/// what was meant for something else isn't taken.
#[test]
fn without_i_stdin_is_empty() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = run_fed(&[], "read x; echo \"read $? [$x]\"", b"hello\n");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "read 1 []\n");
}

/// H7: on a terminal, relaying the command's output adds little to a run:
/// an isolated `true` through `script` (a terminal), against the same run
/// with its streams sent elsewhere (nothing relayed). The best of a few
/// runs each, so a slow moment doesn't decide it.
#[test]
fn relaying_adds_little_to_a_run() {
    if !rootless(Rootless::Single) {
        return;
    }
    let args = shipyard_test_support::for_this_kernel(&["run", "--network", "none", "true"]);
    let shipyard = format!("{} {}", SHIPYARD.path(), args.join(" "));
    let best = |command: &str| {
        (0..5)
            .map(|_| {
                let started = std::time::Instant::now();
                let mut script = Command::new("script");
                script.args(["-qec", command, "/dev/null"]);
                let run = run(script);
                assert_eq!(run.code, Some(0), "{command}: {}{}", run.stdout, run.stderr);
                started.elapsed()
            })
            .min()
            .expect("five runs")
    };
    let relayed = best(&shipyard);
    let plain = best(&format!("{shipyard} </dev/null >/dev/null 2>&1"));
    assert!(
        relayed < plain + std::time::Duration::from_millis(100),
        "relayed {relayed:?}, plain {plain:?}"
    );
}

/// `-t` alone, with input and output piped (Docker allows it): the command
/// still gets a terminal of its own, sized 24 by 80 since there's no
/// terminal to copy (a new one would be 0 by 0), its output relayed.
#[test]
fn t_alone_with_piped_input_gives_a_terminal() {
    if !rootless(Rootless::Single) {
        return;
    }
    let run = run_fed(&["-t"], "tty; stty size", b"");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "/dev/pts/0\r\n24 80\r\n");
}

/// `shipyard run -t … >> file` appends: the relay writes through the file
/// as it was opened, not one opened again (at offset 0, without
/// `O_APPEND`), and the init's messages and the terminal's output share it.
#[test]
fn t_appends_to_its_output_file() {
    if !rootless(Rootless::Single) {
        return;
    }
    let path = std::env::temp_dir().join(format!("shipyard-t-append-{}", std::process::id()));
    std::fs::write(&path, "header\n").expect("writing the file");
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opening the file");
    let output = SHIPYARD
        .command(&["run", "-t", "--network", "none", "echo", "body"])
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .output()
        .expect("running shipyard");
    let written = std::fs::read_to_string(&path).expect("reading the file");
    std::fs::remove_file(&path).expect("removing the file");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(written, "header\nbody\r\n");
}

/// H7: a terminal of the container's own adds little to a run (`-t` against
/// none, piped, the best of a few runs each).
#[test]
fn a_terminal_adds_little_to_a_run() {
    if !rootless(Rootless::Single) {
        return;
    }
    let best = |args: &[&str]| {
        (0..5)
            .map(|_| {
                let started = std::time::Instant::now();
                let run = run_fed(args, "true", b"");
                assert_eq!(run.code, Some(0), "{}", run.stderr);
                started.elapsed()
            })
            .min()
            .expect("five runs")
    };
    let with = best(&["-t", "--network", "none"]);
    let without = best(&["--network", "none"]);
    assert!(
        with < without + std::time::Duration::from_millis(100),
        "with -t {with:?}, without {without:?}"
    );
}
