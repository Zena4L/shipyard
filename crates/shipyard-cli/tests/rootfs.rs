//! End-to-end tests of `shipyard run --rootfs`: a root filesystem of the
//! container's own, with its own `/proc`, `/sys` and `/dev` (Linux).
//!
//! The root filesystems are tiny, made from the host's own programs
//! (`shipyard_test_support::rootfs`). Switching to one needs root, so those
//! tests are `#[ignore]`d in a normal run (see `isolation.rs`); the usage
//! errors are checked as any user.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use shipyard_test_support::rootfs::{MARKER, RootFs};
use shipyard_test_support::{Run, Shipyard, run, under_rosetta};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// `shipyard run --rootfs ROOT FLAGS… sh -c SCRIPT`.
fn in_root(root: &RootFs, flags: &[&str], script: &str) -> Run {
    let args = [
        &["run", "--rootfs", root.arg()],
        flags,
        &["sh", "-c", script],
    ]
    .concat();
    run(SHIPYARD.command(&args))
}

/// Like `in_root`, and the script must succeed.
fn ok_in_root(root: &RootFs, flags: &[&str], script: &str) -> String {
    let run = in_root(root, flags, script);
    assert_eq!(run.code, Some(0), "{script}: {}", run.stderr);
    run.stdout
}

/// The mount points in a `/proc/PID/mountinfo` (field 5), decoded enough
/// for the paths used here.
fn mount_points(mountinfo: &str) -> Vec<String> {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|point| point.replace("\\040", " "))
        .collect()
}

/// The shell loop that prints the options of the mount at `/`, from inside
/// (the root filesystem has no `grep`).
const ROOT_MOUNT_OPTIONS: &str = r#"while read -r _ _ _ _ point options _; do if [ "$point" = / ]; then echo "$options"; fi; done < /proc/self/mountinfo"#;

/// A file system mounted on the host for one test; unmounted on drop (and
/// its directory removed, if it's empty).
struct HostMount {
    dir: PathBuf,
}

impl HostMount {
    /// A tmpfs with `options`, on a new directory under the temporary
    /// directory.
    fn tmpfs(options: &str) -> Self {
        Self::at(tempdir("tmpfs"), "tmpfs", options)
    }

    /// A file system of type `fstype` with `options` on `dir`, made if
    /// needed.
    fn at(dir: PathBuf, fstype: &str, options: &str) -> Self {
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("mount")
            .args(["-t", fstype, "-o", options, fstype])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(status.success(), "mounting {fstype} with {options}");
        Self { dir }
    }
}

impl Drop for HostMount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("-l").arg(&self.dir).status();
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn host_mount_points() -> Vec<String> {
    mount_points(&std::fs::read_to_string("/proc/self/mountinfo").unwrap())
}

/// A fresh, empty directory under the temporary directory.
fn tempdir(name: &str) -> PathBuf {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "shipyard-{name}-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- As any user -------------------------------------------------------------

#[test]
fn a_missing_root_filesystem_is_a_usage_error() {
    let run = run(SHIPYARD.command(&["run", "--rootfs", "/no/such/dir", "true"]));
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("--rootfs /no/such/dir"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_file_is_not_a_root_filesystem() {
    let run = run(SHIPYARD.command(&["run", "--rootfs", "/proc/self/status", "true"]));
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("isn't a directory"), "{}", run.stderr);
}

/// The host's own `/` can't be switched to; without `--rootfs` the
/// container already shares it.
#[test]
fn the_hosts_own_root_is_refused() {
    let run = run(SHIPYARD.command(&["run", "--rootfs", "/", "true"]));
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("the host's own /"), "{}", run.stderr);
}

#[test]
fn contradicting_flags_are_usage_errors() {
    for args in [
        &["run", "--no-image", "--read-only", "true"][..],
        &["run", "--no-isolation", "--rootfs", "/", "true"],
    ] {
        let run = run(SHIPYARD.command(args));
        assert_eq!(run.code, Some(125), "{args:?}: {}", run.stderr);
    }
}

// --- As root -----------------------------------------------------------------

/// The command sees the root filesystem, and nothing of the host's.
#[test]
#[ignore = "needs root"]
fn the_command_sees_the_root_filesystem() {
    let root = RootFs::minimal();
    let out = ok_in_root(
        &root,
        &[],
        "cat /marker; test -e /etc/passwd && echo host-files; ls /",
    );
    assert!(out.starts_with(MARKER), "{out}");
    assert!(!out.contains("host-files"), "{out}");
    for made in ["dev", "proc", "sys"] {
        assert!(out.lines().any(|line| line == made), "{made}: {out}");
    }
}

/// Only the container's own mounts are left inside: the host's tree is
/// detached, not just hidden under the new root.
#[test]
#[ignore = "needs root"]
fn no_host_mount_is_left_inside() {
    let root = RootFs::minimal();
    let points = mount_points(&ok_in_root(&root, &[], "cat /proc/self/mountinfo"));
    assert!(points.contains(&"/".to_owned()), "{points:?}");
    for point in &points {
        assert!(
            point == "/"
                || ["/proc", "/sys", "/dev"]
                    .iter()
                    .any(|top| point.starts_with(top)),
            "a mount from outside: {point} in {points:?}"
        );
    }
}

/// Nothing the container mounts appears on the host (in
/// `scripts/test-linux.sh`, `/` is shared, so a leak would show).
#[test]
#[ignore = "needs root"]
fn the_containers_mounts_stay_inside() {
    let root = RootFs::minimal();
    ok_in_root(&root, &[], "true");
    let leaked: Vec<String> = host_mount_points()
        .into_iter()
        .filter(|point| Path::new(point).starts_with(root.path()))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
}

/// The root filesystem's own submounts come along (the bind onto itself is
/// recursive).
#[test]
#[ignore = "needs root"]
fn the_root_filesystems_own_mounts_come_along() {
    let root = RootFs::minimal();
    let work = HostMount::at(root.host_path("/work"), "tmpfs", "size=1m");
    std::fs::write(work.dir.join("on-the-submount"), "yes\n").unwrap();
    assert_eq!(ok_in_root(&root, &[], "cat /work/on-the-submount"), "yes\n");
}

/// The working directory and the `PATH` lookup are the root filesystem's.
#[test]
#[ignore = "needs root"]
fn paths_resolve_inside_the_root_filesystem() {
    let root = RootFs::minimal();
    let script = root.host_path("/bin/only-inside");
    std::fs::write(&script, "#!/bin/sh\necho inside\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let inside = run(SHIPYARD.command(&["run", "--rootfs", root.arg(), "only-inside"]));
    assert_eq!(
        (inside.code, inside.stdout.as_str()),
        (Some(0), "inside\n"),
        "{}",
        inside.stderr
    );
    assert_eq!(ok_in_root(&root, &["-w", "/work"], "pwd"), "/work\n");
    // On the host, not in the root filesystem.
    let host_only = run(SHIPYARD.command(&["run", "--rootfs", root.arg(), "env"]));
    assert_eq!(host_only.code, Some(127), "{}", host_only.stderr);
}

/// `/proc` is the container's: only its own processes.
#[test]
#[ignore = "needs root"]
fn proc_shows_the_containers_processes() {
    let root = RootFs::minimal();
    let out = ok_in_root(&root, &[], "for p in /proc/[0-9]*; do echo $p; done");
    let pids: Vec<&str> = out.lines().collect();
    assert!(pids.contains(&"/proc/1"), "{out}");
    assert!(
        pids.len() <= 3,
        "the init, the shell (and a moment's child): {out}"
    );
}

/// `/sys` is there, read-only.
#[test]
#[ignore = "needs root"]
fn sys_is_read_only() {
    let root = RootFs::minimal();
    let run = in_root(&root, &[], "ls /sys/kernel >/dev/null && touch /sys/x");
    assert_ne!(run.code, Some(0));
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
}

/// `/dev` has the standard devices and links, and its own `pts`, `shm` and
/// `mqueue`.
#[test]
#[ignore = "needs root"]
fn dev_has_the_standard_devices() {
    let root = RootFs::minimal();
    let out = ok_in_root(
        &root,
        &[],
        "stat -c '%n %F %t:%T %a' /dev/null /dev/zero /dev/full /dev/random /dev/urandom /dev/tty",
    );
    assert_eq!(
        out,
        "/dev/null character special file 1:3 666\n\
         /dev/zero character special file 1:5 666\n\
         /dev/full character special file 1:7 666\n\
         /dev/random character special file 1:8 666\n\
         /dev/urandom character special file 1:9 666\n\
         /dev/tty character special file 5:0 666\n"
    );
    let out = ok_in_root(
        &root,
        &[],
        "readlink /dev/fd /dev/stdin /dev/stdout /dev/stderr /dev/ptmx",
    );
    assert_eq!(
        out,
        "/proc/self/fd\n/proc/self/fd/0\n/proc/self/fd/1\n/proc/self/fd/2\npts/ptmx\n"
    );
    // The devices work.
    let out = ok_in_root(
        &root,
        &[],
        "echo x > /dev/null && head -c 4 /dev/zero | wc -c; echo x > /dev/full || echo full",
    );
    assert_eq!(out, "4\nfull\n");
    // Their own file systems, and `/dev/shm` writable.
    let mountinfo = ok_in_root(&root, &[], "touch /dev/shm/x && cat /proc/self/mountinfo");
    for (point, fstype) in [
        ("/dev", "tmpfs"),
        ("/dev/pts", "devpts"),
        ("/dev/shm", "tmpfs"),
        ("/dev/mqueue", "mqueue"),
    ] {
        assert!(
            mountinfo.lines().any(|line| {
                let mut fields = line.split(' ');
                fields.nth(4) == Some(point)
                    && line
                        .split(" - ")
                        .nth(1)
                        .is_some_and(|rest| rest.starts_with(fstype))
            }),
            "{point} as {fstype}: {mountinfo}"
        );
    }
}

/// The masked paths read as empty; the read-only ones refuse writes.
#[test]
#[ignore = "needs root"]
fn masked_and_read_only_paths() {
    let root = RootFs::minimal();
    // Which of these exist depends on the kernel's configuration (no
    // `/proc/kcore` without CONFIG_PROC_KCORE): the ones the host has.
    let files: Vec<&str> = [
        "/proc/kcore",
        "/proc/keys",
        "/proc/timer_list",
        "/proc/interrupts",
    ]
    .into_iter()
    .filter(|path| Path::new(path).exists())
    .collect();
    assert!(!files.is_empty(), "no masked file to check on this kernel");
    for file in files {
        assert_eq!(
            ok_in_root(&root, &[], &format!("wc -c < {file}")),
            "0\n",
            "{file}"
        );
    }
    if Path::new("/sys/firmware").exists() {
        assert_eq!(ok_in_root(&root, &[], "ls -A /sys/firmware | wc -l"), "0\n");
    }
    for path in ["/proc/sysrq-trigger", "/proc/sys/kernel/domainname"] {
        let run = in_root(&root, &[], &format!("echo x > {path}"));
        assert_ne!(run.code, Some(0), "{path}");
        assert!(
            run.stderr.contains("Read-only file system"),
            "{path}: {}",
            run.stderr
        );
    }
}

/// `--read-only`: `/` refuses writes, the mounts on it don't.
#[test]
#[ignore = "needs root"]
fn a_read_only_root_filesystem() {
    let root = RootFs::minimal();
    let run = in_root(
        &root,
        &["--read-only"],
        "touch /dev/shm/x && echo shm-ok; touch /x",
    );
    assert_ne!(run.code, Some(0));
    assert_eq!(run.stdout, "shm-ok\n");
    assert!(
        run.stderr.contains("Read-only file system"),
        "{}",
        run.stderr
    );
}

/// Making the root read-only keeps the options it had: a read-only remount
/// given only "read-only" would drop `nosuid` and `nodev` (probe 8b).
#[test]
#[ignore = "needs root"]
fn read_only_keeps_the_mounts_other_options() {
    let tmpfs = HostMount::tmpfs("nosuid,nodev");
    let root = RootFs::minimal_in(&tmpfs.dir);
    let options = ok_in_root(&root, &["--read-only"], ROOT_MOUNT_OPTIONS);
    let options: Vec<&str> = options.trim().split(',').collect();
    for wanted in ["ro", "nosuid", "nodev"] {
        assert!(options.contains(&wanted), "{wanted}: {options:?}");
    }
}

/// A root filesystem whose `proc` is a symlink out of it: refused, and
/// nothing is mounted where it points (the runc CVE-2021-30465 class).
#[test]
#[ignore = "needs root"]
fn a_symlinked_mount_point_is_refused() {
    let root = RootFs::minimal();
    let outside = tempdir("outside");
    std::os::unix::fs::symlink(&outside, root.host_path("/proc")).unwrap();
    let run = in_root(&root, &[], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("/proc is a symbolic link"),
        "{}",
        run.stderr
    );
    // `--no-isolation` can't help with a root filesystem: no hint for it.
    assert!(!run.stderr.contains("--no-isolation"), "{}", run.stderr);
    // (Followed, the mount would land on `outside` in the container's mount
    // namespace only, so the host's mount table couldn't show it: the
    // refusal is what's checked.)
    let _ = std::fs::remove_dir(&outside);
}

/// Containers started together on a fresh root filesystem all start: the
/// ones that find `/proc` already made by another carry on.
#[test]
#[ignore = "needs root"]
fn containers_can_start_together_on_a_fresh_root_filesystem() {
    let root = RootFs::minimal();
    let runs: Vec<std::thread::JoinHandle<Run>> = (0..8)
        .map(|_| {
            let command = SHIPYARD.command(&["run", "--rootfs", root.arg(), "sh", "-c", "exit 3"]);
            std::thread::spawn(move || run(command))
        })
        .collect();
    for started in runs {
        let run = started.join().unwrap();
        assert_eq!(run.code, Some(3), "{}", run.stderr);
    }
}

/// The host's own mounts on the root filesystem's `proc`, `sys` and `dev`
/// (a chroot often has them) are detached in the container: only its own are
/// left, so unmounting those can't uncover the host's.
#[test]
#[ignore = "needs root"]
fn host_mounts_on_the_targets_are_detached_inside() {
    let root = RootFs::minimal();
    let mounts = [
        HostMount::at(root.host_path("/dev"), "tmpfs", "size=1m"),
        HostMount::at(root.host_path("/proc"), "proc", "defaults"),
        // A sysfs refuses to be mounted on another: this one must go first.
        HostMount::at(root.host_path("/sys"), "sysfs", "ro"),
    ];
    let points = mount_points(&ok_in_root(&root, &[], "cat /proc/self/mountinfo"));
    for target in ["/dev", "/proc", "/sys"] {
        let count = points.iter().filter(|point| *point == target).count();
        assert_eq!(count, 1, "{target}: {points:?}");
    }
    drop(mounts);
}

/// A root filesystem on a read-only file system, without `/proc`: the error
/// says to create it.
#[test]
#[ignore = "needs root"]
fn missing_targets_on_a_read_only_file_system_are_explained() {
    let tmpfs = HostMount::tmpfs("size=64m");
    let root = RootFs::minimal_in(&tmpfs.dir);
    let status = Command::new("mount")
        .args(["-o", "remount,ro"])
        .arg(&tmpfs.dir)
        .status()
        .unwrap();
    assert!(status.success());
    let run = in_root(&root, &[], "true");
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(run.stderr.contains("create /proc in it"), "{}", run.stderr);
}

/// A program on a `noexec` mount: the error says why it can't run.
#[test]
#[ignore = "needs root"]
fn a_noexec_root_filesystem_is_explained() {
    let tmpfs = HostMount::tmpfs("noexec");
    let root = RootFs::minimal_in(&tmpfs.dir);
    let run = in_root(&root, &[], "true");
    assert_eq!(run.code, Some(126), "{}", run.stderr);
    assert!(run.stderr.contains("mounted noexec"), "{}", run.stderr);
}

/// A program whose dynamic loader is missing from the root filesystem: the
/// error says the program is there.
#[test]
#[ignore = "needs root"]
fn a_missing_loader_is_explained() {
    // Under Rosetta (x86-64 emulation on Apple silicon), the kernel hands an
    // x86 program to the emulator, which reports the missing loader itself:
    // the exec succeeds, and there's nothing for shipyard to explain.
    if under_rosetta() {
        eprintln!("under Rosetta emulation, which runs the program anyway; skipping");
        return;
    }
    let root = RootFs::minimal();
    root.remove_loader();
    let run = in_root(&root, &[], "true");
    assert_eq!(run.code, Some(127), "{}", run.stderr);
    assert!(
        run.stderr.contains("interpreter or loader is missing"),
        "{}",
        run.stderr
    );
}
