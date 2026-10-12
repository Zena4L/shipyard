//! End-to-end tests of a run's cgroup (stage 5, step 2): a rootful isolated
//! run is born into `/sys/fs/cgroup/shipyard/run/<shipyard's pid>`, sees only
//! that cgroup (a cgroup namespace and a read-only `cgroup2` mount), and the
//! cgroup goes when the run ends (Linux, root, cgroup v2).
//!
//! The runtime's cgroups (`runtime/<id>`) are tested in `runtime.rs`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::io::{BufRead as _, BufReader};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use shipyard_test_support::rootfs::RootFs;
use std::os::unix::process::CommandExt as _;

use shipyard_test_support::{
    Rootless, Shipyard, child_of, delegated_cgroup, rootless, run, start_command,
};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// Where the host's cgroup v2 tree is.
const ROOT: &str = "/sys/fs/cgroup";

/// Root inside is host user 100000, and so on for 65536 IDs.
const MAP: &str = "0:100000:65536";

/// What the command reports about its cgroup, before it waits to be ended.
const LOOK_INSIDE: &str = "cat /proc/self/cgroup; \
    stat -f -c %T /sys/fs/cgroup; \
    mkdir /sys/fs/cgroup/child 2>&1; \
    if [ -e /sys/fs/cgroup/shipyard ]; then echo the host tree; else echo its own tree; fi; \
    echo procs $(cat /sys/fs/cgroup/cgroup.procs); \
    exec sleep 300";

/// The cgroup tree these tests need: v2, at the root. Fails, saying so,
/// where it isn't: these tests run only where cgroups are set up (CI's root
/// job, `scripts/test-linux.sh`).
fn require_cgroup_v2() {
    assert!(
        Path::new(ROOT).join("cgroup.controllers").exists(),
        "these tests need a cgroup v2 tree at {ROOT}"
    );
}

/// `shipyard run`'s cgroup, for the shipyard process `pid`.
fn run_cgroup(pid: libc::pid_t) -> PathBuf {
    Path::new(ROOT).join(format!("shipyard/run/{pid}"))
}

/// Starts `shipyard run ARGS… sh -c LOOK_INSIDE`, checks what the command
/// saw and what the host sees while it runs, then ends it and checks the
/// cgroup is gone.
fn a_run_has_its_own_cgroup(args: &[&str]) {
    let shipyard =
        start_command(SHIPYARD.command(&[&["run"], args, &["sh", "-c", LOOK_INSIDE]].concat()));
    let inside: Vec<String> = (0..5).map(|_| shipyard.next_line()).collect();
    assert_eq!(
        inside[0], "0::/",
        "{args:?}: the namespace's root: {inside:?}"
    );
    assert_eq!(inside[1], "cgroup2fs", "{args:?}: {inside:?}");
    assert!(
        inside[2].contains("Read-only file system"),
        "{args:?}: {inside:?}"
    );
    assert_eq!(inside[3], "its own tree", "{args:?}: {inside:?}");
    let procs: Vec<&str> = inside[4].split_whitespace().skip(1).collect();
    assert!(
        procs.contains(&"1"),
        "{args:?}: the init's in it: {inside:?}"
    );

    let cgroup = run_cgroup(shipyard.pid());
    let init = child_of(shipyard.pid());
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{init}/cgroup")).unwrap(),
        format!("0::/shipyard/run/{}\n", shipyard.pid()),
        "{args:?}"
    );
    let held = std::fs::read_to_string(cgroup.join("cgroup.procs")).unwrap();
    assert!(
        !held.lines().any(|pid| pid == shipyard.pid().to_string()),
        "{args:?}: shipyard itself stays out: {held}"
    );

    shipyard.signal(libc::SIGTERM);
    let _ = shipyard.finish();
    assert!(!cgroup.exists(), "{args:?}: {} is left", cgroup.display());
}

#[test]
#[ignore = "needs root"]
fn a_run_without_a_root_filesystem_has_its_own_cgroup() {
    require_cgroup_v2();
    a_run_has_its_own_cgroup(&[]);
}

#[test]
#[ignore = "needs root"]
fn a_run_on_a_root_filesystem_has_its_own_cgroup() {
    require_cgroup_v2();
    let root = RootFs::minimal();
    a_run_has_its_own_cgroup(&["--rootfs", root.arg()]);
}

/// In a user namespace too: the clean image makes the cgroup namespace (so
/// the user namespace owns it, and may mount it).
#[test]
#[ignore = "needs root"]
fn a_run_in_a_user_namespace_has_its_own_cgroup() {
    require_cgroup_v2();
    let root = RootFs::minimal();
    root.chown_all(100_000, 100_000);
    a_run_has_its_own_cgroup(&["--uidmap", MAP, "--rootfs", root.arg()]);
}

/// A run that ends by itself takes its cgroup along, whatever its exit.
#[test]
#[ignore = "needs root"]
fn a_finished_run_leaves_no_cgroup() {
    require_cgroup_v2();
    let shipyard = start_command(SHIPYARD.command(&["run", "sh", "-c", "echo up; exit 3"]));
    assert_eq!(shipyard.next_line(), "up");
    let cgroup = run_cgroup(shipyard.pid());
    let (code, _) = shipyard.finish();
    assert_eq!(code, 3);
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
}

/// A shipyard killed before it could clean up leaves its `run/<pid>`, maybe
/// with processes in it (with no PID namespace to end them): the next run
/// kills them, and the cgroup goes. One a live run holds (its lock) stays,
/// and so does what's in it, even if its PID isn't one this `/proc` shows: a
/// shipyard in another PID namespace sharing the tree.
#[test]
#[ignore = "needs root"]
fn the_next_run_sweeps_cgroups_of_dead_runs() {
    require_cgroup_v2();
    let dead = run_cgroup(gone_pid());
    let held = run_cgroup(gone_pid());
    std::fs::create_dir_all(&dead).unwrap();
    let lock = lock_new_cgroup(&held);
    // What a run killed outright left in its cgroup (no PID namespace ended
    // it), and what a live run has in its own.
    let in_cgroup = |cgroup: &Path| {
        let process = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), process.id().to_string()).unwrap();
        process
    };
    let mut left = in_cgroup(&dead);
    let mut living = in_cgroup(&held);

    let first = run(SHIPYARD.command(&["run", "true"]));
    // Killed by the sweep; its cgroup goes with the next one, once empty.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let ended = loop {
        if let Some(status) = left.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let second = run(SHIPYARD.command(&["run", "true"]));
    let (held_left, dead_left) = (held.exists(), dead.exists());
    let still_living = living.try_wait().unwrap().is_none();
    let _ = left.kill();
    let _ = left.wait();
    let _ = living.kill();
    let _ = living.wait();
    drop(lock);
    let _ = std::fs::remove_dir(&held);
    let _ = std::fs::remove_dir(&dead);
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert_eq!(
        ended.and_then(|status| std::os::unix::process::ExitStatusExt::signal(&status)),
        Some(libc::SIGKILL),
        "what a dead run left wasn't killed"
    );
    assert!(still_living, "a live run's process was killed");
    assert!(held_left, "a live run's cgroup was removed");
    assert!(!dead_left, "{} is left", dead.display());
}

/// Makes the cgroup at `path` and locks it, as a live run holds its own.
/// Other tests' runs sweep any unlocked one named after a dead PID, so it can
/// go between `mkdir` and `flock` (as shipyard's own claim allows for):
/// again, until the lock is on the directory that's at the path.
fn lock_new_cgroup(path: &Path) -> std::fs::File {
    use std::os::unix::fs::MetadataExt as _;
    for _ in 0..50 {
        let _ = std::fs::create_dir_all(path);
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        // SAFETY: `flock` on a descriptor this test owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            continue;
        }
        let (Ok(locked), Ok(there)) = (file.metadata(), std::fs::metadata(path)) else {
            continue;
        };
        if locked.ino() == there.ino() {
            return file;
        }
    }
    panic!("couldn't make and lock {}", path.display());
}

/// The PID of a process that has come and gone.
fn gone_pid() -> libc::pid_t {
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    libc::pid_t::try_from(gone.id()).unwrap()
}

// --- Limits (step 3) -------------------------------------------------------------

/// `shipyard run ARGS…`, which must succeed: its output.
fn ok(args: &[&str]) -> String {
    let run = run(SHIPYARD.command(&[&["run"], args].concat()));
    assert_eq!(run.code, Some(0), "{args:?}: {}", run.stderr);
    run.stdout
}

/// Docker's flags land in the container's own cgroup files, as cgroup v2
/// takes them, before anything runs.
#[test]
#[ignore = "needs root"]
fn limits_are_in_the_containers_cgroup() {
    require_cgroup_v2();
    let out = ok(&[
        "--memory",
        "64m",
        "--memory-reservation",
        "32m",
        "--cpus",
        "0.5",
        "--cpu-shares",
        "512",
        "--pids-limit",
        "50",
        "--cpuset-cpus",
        "0",
        "sh",
        "-c",
        "cd /sys/fs/cgroup && cat memory.max memory.low cpu.max cpu.weight pids.max \
         cpuset.cpus.effective",
    ]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        ["67108864", "33554432", "50000 100000", "20", "50", "0"],
        "{out}"
    );
}

/// Swap, as Docker means it: as much as memory unless `--memory-swap`
/// (memory plus swap) says otherwise; -1 is no limit.
#[test]
#[ignore = "needs root"]
fn swap_follows_dockers_rule() {
    require_cgroup_v2();
    let swap = |extra: &[&str]| {
        run(SHIPYARD.command(
            &[
                &["run", "--memory", "64m"],
                extra,
                &["cat", "/sys/fs/cgroup/memory.swap.max"],
            ]
            .concat(),
        ))
    };
    let explicit = swap(&["--memory-swap", "96m"]);
    if explicit.code == Some(125) && explicit.stderr.contains("doesn't account swap") {
        eprintln!("skipping: this kernel doesn't account swap");
        return;
    }
    assert_eq!(explicit.stdout, "33554432\n", "{}", explicit.stderr);
    for (extra, expected) in [
        (&[][..], "67108864\n"),
        (&["--memory-swap", "64m"], "0\n"),
        (&["--memory-swap", "-1"], "max\n"),
    ] {
        let given = swap(extra);
        assert_eq!(given.stdout, expected, "{extra:?}: {}", given.stderr);
    }
}

/// A command the OOM killer kills: 137, and said so.
#[test]
#[ignore = "needs root"]
fn the_oom_killer_is_reported() {
    require_cgroup_v2();
    // 32 MiB and no swap; command substitution keeps the whole output in
    // the shell's memory.
    let hog = "x=$(head -c 200m /dev/urandom); echo survived";
    let in_32m = |script: &str| {
        run(SHIPYARD.command(&[
            "run",
            "--memory",
            "32m",
            "--memory-swap",
            "32m",
            "sh",
            "-c",
            script,
        ]))
    };
    let killed = in_32m(hog);
    assert_eq!(killed.code, Some(137), "{}", killed.stderr);
    assert!(
        killed
            .stderr
            .contains("the command was killed: out of memory"),
        "{}",
        killed.stderr
    );
    // Another process the victim: the command goes on, and it's said.
    let lived = in_32m(&format!("sh -c '{hog}'; echo the command lives"));
    assert_eq!(lived.code, Some(0), "{}", lived.stderr);
    assert!(
        lived.stdout.contains("the command lives"),
        "{}",
        lived.stdout
    );
    assert!(
        lived
            .stderr
            .contains("a process in the container was killed: out of memory"),
        "{}",
        lived.stderr
    );
}

/// The process limit counts every process in the container, its init
/// included; one too low for the init to start the command says so.
#[test]
#[ignore = "needs root"]
fn the_process_limit_holds() {
    require_cgroup_v2();
    // The shell gives up at the first fork refused ("Cannot fork").
    let capped = run(SHIPYARD.command(&[
        "run",
        "--pids-limit",
        "10",
        "sh",
        "-c",
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do sleep 2 & done; wait",
    ]));
    assert_ne!(capped.code, Some(0), "all 12 started");
    assert!(capped.stderr.contains("fork"), "{}", capped.stderr);
    let refused = run(SHIPYARD.command(&["run", "--pids-limit", "1", "true"]));
    assert_eq!(refused.code, Some(125), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("process limit is 1"),
        "{}",
        refused.stderr
    );
}

/// Without root, a container's cgroup goes in the subtree delegated to the
/// user (step 4): born there, with the cgroup namespace, the read-only mount
/// and the limits, and gone after the run. Where nothing is delegated, a
/// limit is refused with the fix instead.
#[test]
fn rootless_cgroups_follow_delegation() {
    if !rootless(Rootless::Single) {
        return;
    }
    let Some(base) = delegated_cgroup() else {
        let refused = run(SHIPYARD.command(&["run", "--memory", "64m", "true"]));
        assert_eq!(refused.code, Some(125), "{}", refused.stderr);
        assert!(
            refused.stderr.contains("systemd-run --user --scope"),
            "{}",
            refused.stderr
        );
        return;
    };
    let shipyard = start_command(SHIPYARD.command(&[
        "run",
        "--memory",
        "64m",
        "--pids-limit",
        "50",
        "sh",
        "-c",
        "cat /proc/self/cgroup /sys/fs/cgroup/memory.max /sys/fs/cgroup/pids.max; \
         mkdir /sys/fs/cgroup/x 2>&1; exec sleep 300",
    ]));
    let inside: Vec<String> = (0..4).map(|_| shipyard.next_line()).collect();
    assert_eq!(inside[..3], ["0::/", "67108864", "50"], "{inside:?}");
    assert!(inside[3].contains("Read-only file system"), "{inside:?}");
    let cgroup = base.join(format!("shipyard/run/{}", shipyard.pid()));
    let init = child_of(shipyard.pid());
    let placed = std::fs::read_to_string(format!("/proc/{init}/cgroup")).unwrap();
    assert!(
        placed
            .trim_end()
            .ends_with(&format!("shipyard/run/{}", shipyard.pid())),
        "{placed}"
    );
    assert!(cgroup.exists(), "{}", cgroup.display());
    shipyard.signal(libc::SIGTERM);
    let _ = shipyard.finish();
    assert!(!cgroup.exists(), "{} is left", cgroup.display());
}

/// A limit whose controller wasn't delegated is refused, naming it.
#[test]
fn an_undelegated_controller_is_named() {
    if !rootless(Rootless::Single) {
        return;
    }
    let Some(base) = delegated_cgroup() else {
        eprintln!("skipping: nothing is delegated here");
        return;
    };
    let delegated = std::fs::read_to_string(base.join("cgroup.controllers")).unwrap();
    let Some((controller, flag)) = [("cpuset", ["--cpuset-cpus", "0"]), ("cpu", ["--cpus", "1"])]
        .into_iter()
        .find(|(controller, _)| !delegated.split_whitespace().any(|c| c == *controller))
    else {
        eprintln!("skipping: every controller is delegated here ({delegated})");
        return;
    };
    let refused = run(SHIPYARD.command(&[&["run"], &flag[..], &["true"]].concat()));
    assert_eq!(refused.code, Some(125), "{}", refused.stderr);
    assert!(
        refused
            .stderr
            .contains(&format!("the {controller} controller"))
            && refused.stderr.contains("delegated"),
        "{}",
        refused.stderr
    );
}

/// As root, the no-delegation path: shipyard run as `nobody` from a cgroup
/// root owns (as an SSH login's session scope is): a limit is refused, with
/// the fix.
#[test]
#[ignore = "needs root"]
fn without_delegation_a_limit_is_refused_with_the_fix() {
    require_cgroup_v2();
    let scratch = Path::new(ROOT).join(format!("shipyard-undelegated-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let procs = std::ffi::CString::new(
        scratch
            .join("cgroup.procs")
            .into_os_string()
            .into_encoded_bytes(),
    )
    .unwrap();
    let mut command = std::process::Command::new("setpriv");
    command
        .args(["--reuid", "65534", "--regid", "65534", "--clear-groups"])
        .arg(SHIPYARD.path())
        .args(["run", "--memory", "64m", "true"]);
    // SAFETY: open, write and close are async-signal-safe; this moves the
    // child itself ("0") into the root-owned cgroup before it drops root.
    unsafe {
        command.pre_exec(move || {
            let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let written = libc::write(fd, b"0".as_ptr().cast(), 1);
            let error = std::io::Error::last_os_error();
            libc::close(fd);
            if written == 1 { Ok(()) } else { Err(error) }
        });
    }
    let refused = run(command);
    let _ = std::fs::remove_dir(&scratch);
    assert_eq!(refused.code, Some(125), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("isn't delegated")
            && refused.stderr.contains("systemd-run --user --scope"),
        "{}",
        refused.stderr
    );
}

/// I/O limits take whole disks: a partition is refused, saying so.
#[test]
#[ignore = "needs root"]
fn a_partition_is_not_a_disk() {
    require_cgroup_v2();
    let partition = std::fs::read_dir("/sys/class/block")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("partition").exists())
        .map(|entry| Path::new("/dev").join(entry.file_name()))
        .find(|dev| dev.exists());
    let Some(partition) = partition else {
        eprintln!("skipping: no partition here");
        return;
    };
    let limit = format!("{}:1mb", partition.display());
    let refused = run(SHIPYARD.command(&["run", "--device-read-bps", &limit, "true"]));
    assert_eq!(refused.code, Some(125), "{}", refused.stderr);
    assert!(
        refused.stderr.contains("not a partition"),
        "{}",
        refused.stderr
    );
}

/// A command shipyard itself killed (its stop timeout ran out) isn't
/// blamed on the OOM killer, even though it struck in the container.
#[test]
#[ignore = "needs root"]
fn shipyards_own_kill_is_not_an_oom_kill() {
    require_cgroup_v2();
    let mut shipyard = SHIPYARD
        .command(&[
            "run",
            "--memory",
            "32m",
            "--memory-swap",
            "32m",
            "--stop-timeout",
            "1",
            "sh",
            "-c",
            // The inner shell asks to be the OOM killer's victim (raising
            // one's own score needs no privilege): left to the kernel, a
            // bash `sh` (Fedora's) is big enough to be picked itself.
            "sh -c 'echo 1000 >/proc/self/oom_score_adj; x=$(head -c 200m /dev/urandom)'; \
             trap '' TERM; echo ready; sleep 300",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(shipyard.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    if ready != "ready\n" {
        let output = shipyard.wait_with_output().unwrap();
        panic!(
            "no \"ready\" ({ready:?}); shipyard ended with {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // SAFETY: a signal to the shipyard this test started.
    unsafe { libc::kill(libc::pid_t::try_from(shipyard.id()).unwrap(), libc::SIGTERM) };
    let output = shipyard.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(137), "{stderr}");
    assert!(!stderr.contains("the command was killed"), "{stderr}");
    assert!(
        stderr.contains("a process in the container was killed: out of memory"),
        "{stderr}"
    );
}
