//! End-to-end tests of a container's confinement (stage 4): its
//! capabilities, `no_new_privs` and seccomp filter (Linux).
//!
//! The command reports its own state from `/proc/self/status` (`CapBnd`,
//! `CapEff`, `CapAmb`, `NoNewPrivs`, `Seccomp`): what the kernel holds, not
//! what Shipyard meant. Running a container needs root, so those tests are
//! `#[ignore]`d in a normal run (see `isolation.rs`); the usage errors run
//! as any user.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::json;
use shipyard_runtime::security::{Capability, CapabilitySet};
use shipyard_test_support::bundle::Bundle;
use shipyard_test_support::{Run, Shipyard, killed_by, run, start_command, under_rosetta};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

/// Prints the command's status: its capability sets and `no_new_privs`
/// among it. (`cat`, which the bundles' small root filesystem has.)
const SHOW_STATUS: &str = "cat /proc/self/status";

/// `shipyard run ARGS… sh -c SCRIPT`.
fn shipyard_run(args: &[&str], script: &str) -> Run {
    run(SHIPYARD.command(&[&["run"], args, &["sh", "-c", script]].concat()))
}

/// `shipyard run ARGS…`.
fn run_args(args: &[&str]) -> Run {
    run(SHIPYARD.command(&[&["run"], args].concat()))
}

/// The value of `field` in `/proc/…/status` output.
fn field(status: &str, field: &str) -> u64 {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("no {field} in {status}"))
        .trim();
    u64::from_str_radix(value, 16).unwrap_or_else(|_| panic!("{field}: {value}"))
}

/// The command's own status, from a run that must succeed.
fn status_with(args: &[&str]) -> String {
    let run = shipyard_run(args, SHOW_STATUS);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    run.stdout
}

fn bits(names: &[&str]) -> u64 {
    names
        .iter()
        .map(|name| name.parse::<Capability>().unwrap())
        .collect::<CapabilitySet>()
        .bits()
}

// --- As any user: usage errors --------------------------------------------------

#[test]
fn security_flags_are_checked() {
    for (args, expected) in [
        (&["--cap-add", "FLY"][..], "unknown capability"),
        (
            &["--cap-add", "CHOWN", "--cap-drop", "CHOWN"],
            "both name CAP_CHOWN",
        ),
        (&["--security-opt", "apparmor=x"], "unknown security option"),
        (
            &["--security-opt", "seccomp=/no/such.json"],
            "seccomp profile /no/such.json",
        ),
        (
            &["--security-opt", "no-new-privileges=maybe"],
            "true or false",
        ),
        (
            &["--no-isolation", "--cap-add", "CHOWN"],
            "cannot be used with",
        ),
    ] {
        let run = shipyard_run(args, "true");
        assert_eq!(run.code, Some(125), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{args:?}: {}", run.stderr);
    }
}

// --- As root: what the command gets ------------------------------------------------

/// Docker's 14, nothing else, and `no_new_privs` on (Shipyard's default,
/// unlike Docker's).
#[test]
#[ignore = "needs root"]
fn the_default_is_dockers_set_and_no_new_privs() {
    let status = status_with(&[]);
    let default = CapabilitySet::DOCKER_DEFAULT.bits();
    assert_eq!(field(&status, "CapBnd"), default, "{status}");
    assert_eq!(field(&status, "CapEff"), default, "{status}");
    assert_eq!(field(&status, "CapPrm"), default, "{status}");
    assert_eq!(field(&status, "CapInh"), 0, "{status}");
    assert_eq!(field(&status, "CapAmb"), 0, "{status}");
    assert_eq!(field(&status, "NoNewPrivs"), 1, "{status}");
    let off = status_with(&["--security-opt", "no-new-privileges=false"]);
    assert_eq!(field(&off, "NoNewPrivs"), 0, "{off}");
}

#[test]
#[ignore = "needs root"]
fn cap_add_and_cap_drop_change_the_set() {
    let none = status_with(&["--cap-drop", "ALL"]);
    assert_eq!(field(&none, "CapBnd"), 0, "{none}");
    assert_eq!(field(&none, "CapEff"), 0, "{none}");
    let admin = status_with(&["--cap-add", "SYS_ADMIN", "--cap-drop", "CHOWN"]);
    let expected =
        (CapabilitySet::DOCKER_DEFAULT.bits() | bits(&["SYS_ADMIN"])) & !bits(&["CHOWN"]);
    assert_eq!(field(&admin, "CapEff"), expected, "{admin}");
    assert_eq!(field(&admin, "CapBnd"), expected, "{admin}");
    let only = status_with(&["--cap-drop", "ALL", "--cap-add", "net_bind_service"]);
    assert_eq!(
        field(&only, "CapEff"),
        bits(&["NET_BIND_SERVICE"]),
        "{only}"
    );
}

/// A non-root user has nothing effective, unless `--cap-add` gave it some:
/// those it keeps across `exec` (ambient, as Podman does).
#[test]
#[ignore = "needs root"]
fn a_non_root_user_gets_only_what_was_added() {
    let plain = status_with(&["--user", "1000"]);
    assert_eq!(field(&plain, "CapEff"), 0, "{plain}");
    assert_eq!(
        field(&plain, "CapBnd"),
        CapabilitySet::DOCKER_DEFAULT.bits(),
        "{plain}"
    );
    let added = status_with(&["--user", "1000", "--cap-add", "NET_BIND_SERVICE"]);
    let bind = bits(&["NET_BIND_SERVICE"]);
    for set in ["CapEff", "CapPrm", "CapInh", "CapAmb"] {
        assert_eq!(field(&added, set), bind, "{set}: {added}");
    }
}

/// A host directory with a setuid-root copy of `id`, removed on drop.
struct SetuidId {
    dir: PathBuf,
}

impl SetuidId {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("shipyard-setuid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::copy("/usr/bin/id", dir.join("id")).unwrap();
        std::fs::set_permissions(dir.join("id"), std::fs::Permissions::from_mode(0o4755)).unwrap();
        Self { dir }
    }

    fn path(&self) -> String {
        self.dir.join("id").display().to_string()
    }
}

impl Drop for SetuidId {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `no_new_privs` disarms setuid programs: a setuid-root `id` run by user
/// 1000 stays user 1000, unless it's turned off.
#[test]
#[ignore = "needs root"]
fn no_new_privs_disarms_setuid_programs() {
    let id = SetuidId::new();
    let script = format!("{} -u", id.path());
    let guarded = shipyard_run(&["--user", "1000"], &script);
    assert_eq!(guarded.stdout.trim(), "1000", "{}", guarded.stderr);
    let open = shipyard_run(
        &[
            "--user",
            "1000",
            "--security-opt",
            "no-new-privileges=false",
        ],
        &script,
    );
    assert_eq!(open.stdout.trim(), "0", "{}", open.stderr);
}

// --- As root: the init -------------------------------------------------------------

/// The init keeps only `CAP_KILL`, and `no_new_privs`, once its command runs.
#[test]
#[ignore = "needs root"]
fn the_init_keeps_only_cap_kill() {
    let run = shipyard_run(&[], "grep -E '^(CapEff|CapBnd|NoNewPrivs):' /proc/1/status");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let kill = bits(&["KILL"]);
    assert_eq!(field(&run.stdout, "CapEff"), kill, "{}", run.stdout);
    assert_eq!(field(&run.stdout, "CapBnd"), kill, "{}", run.stdout);
    assert_eq!(field(&run.stdout, "NoNewPrivs"), 1, "{}", run.stdout);
}

/// Root inside can't reach the init's descriptors (it isn't dumpable, and
/// `CAP_SYS_PTRACE` is gone: step 1's probe P1), unless given it. (Listing
/// `/proc/1/fd` is allowed: a non-dumpable process's directory belongs to
/// root, mode 0500. Following an entry is what needs the ptrace check.)
#[test]
#[ignore = "needs root"]
fn root_inside_cant_reach_the_inits_descriptors() {
    let refused = shipyard_run(&[], "ls -l /proc/1/fd/0");
    assert_ne!(refused.code, Some(0), "{}", refused.stdout);
    assert!(
        refused.stderr.contains("Permission denied"),
        "{}",
        refused.stderr
    );
    let allowed = shipyard_run(&["--cap-add", "SYS_PTRACE"], "readlink /proc/1/fd/0");
    assert_eq!(allowed.stdout.trim(), "/dev/null", "{}", allowed.stderr);
}

/// The init still passes signals to a command running as another user: the
/// `CAP_KILL` it keeps is what lets it.
#[test]
#[ignore = "needs root"]
fn the_init_still_forwards_signals_to_a_non_root_command() {
    let running = start_command(SHIPYARD.command(&[
        "run",
        "--user",
        "1000",
        "sh",
        "-c",
        "trap 'exit 9' TERM; echo ready; sleep 100 & wait",
    ]));
    assert_eq!(running.next_line(), "ready");
    running.signal(libc::SIGTERM);
    let (code, _) = running.finish();
    assert_eq!(code, 9, "the signal didn't reach the command");
    assert_ne!(code, killed_by(libc::SIGKILL));
}

// --- As root: bundles ----------------------------------------------------------------

/// `shipyard run --bundle` with `sh -c SCRIPT` and `change` made to the config.
fn bundle_run(script: &str, change: impl FnOnce(&mut serde_json::Value)) -> Run {
    let bundle = Bundle::new();
    let mut config = bundle.config();
    config["process"]["args"] = json!(["sh", "-c", script]);
    change(&mut config);
    bundle.write(&config);
    run(SHIPYARD.command(&["run", "--bundle", bundle.arg()]))
}

/// A bundle's sets are applied exactly, and its `noNewPrivileges`.
///
/// For root, the kernel's own rule shows: on `execve`, a process with UID 0
/// gets its whole bounding set back as permitted and effective (the
/// program's file capabilities count as all; `capabilities(7)`), unless
/// `no_new_privs` keeps it from gaining anything. So with `no_new_privs`
/// the exact sets hold; without, the bounding set is root's real limit.
#[test]
#[ignore = "needs root"]
fn a_bundles_capabilities_are_exact() {
    let with = |no_new_privileges: bool| {
        let run = bundle_run(SHOW_STATUS, |config| {
            config["process"]["capabilities"] = json!({
                "bounding": ["CAP_KILL", "CAP_CHOWN"],
                "effective": ["CAP_KILL"],
                "permitted": ["CAP_KILL"]
            });
            config["process"]["noNewPrivileges"] = json!(no_new_privileges);
        });
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        run.stdout
    };
    let both = bits(&["KILL", "CHOWN"]);
    let guarded = with(true);
    assert_eq!(field(&guarded, "CapBnd"), both, "{guarded}");
    assert_eq!(field(&guarded, "CapEff"), bits(&["KILL"]), "{guarded}");
    assert_eq!(field(&guarded, "NoNewPrivs"), 1, "{guarded}");
    let open = with(false);
    assert_eq!(field(&open, "CapBnd"), both, "{open}");
    assert_eq!(
        field(&open, "CapEff"),
        both,
        "root's exec restores the bounding set: {open}"
    );
    assert_eq!(field(&open, "NoNewPrivs"), 0, "{open}");
}

/// Without `process.capabilities`, none, with a warning (runc would give
/// all of root's).
#[test]
#[ignore = "needs root"]
fn a_bundle_without_capabilities_gets_none() {
    let run = bundle_run(SHOW_STATUS, |config| {
        config["process"]
            .as_object_mut()
            .unwrap()
            .remove("capabilities");
    });
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("no capabilities"), "{}", run.stderr);
    assert_eq!(field(&run.stdout, "CapBnd"), 0, "{}", run.stdout);
    assert_eq!(field(&run.stdout, "CapEff"), 0, "{}", run.stdout);
}

/// A bundle may mask `/proc/sys`: the kernel's last capability is read before
/// the container's mounts, so the run doesn't need it.
#[test]
#[ignore = "needs root"]
fn a_bundle_may_mask_proc_sys() {
    // Rosetta itself reads /proc/sys/vm/mmap_min_addr to start each x86
    // program inside, so a masked /proc/sys stops the emulator, not
    // Shipyard.
    if under_rosetta() {
        eprintln!("under Rosetta emulation, which needs /proc/sys itself; skipping");
        return;
    }
    let run = bundle_run("echo ran", |config| {
        config["linux"]["maskedPaths"] = json!(["/proc/sys"]);
    });
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "ran\n");
}

/// A host directory with a root-only file and a copy of `cat` holding a file
/// capability (`cap_dac_override+ep`, what `setcap` would write), removed on
/// drop.
struct FileCapability {
    dir: PathBuf,
}

impl FileCapability {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("shipyard-filecap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("secret"), "secret\n").unwrap();
        std::fs::set_permissions(dir.join("secret"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::copy("/bin/cat", dir.join("cat")).unwrap();
        // `vfs_cap_data`, revision 2: the magic word (revision 2, effective),
        // then permitted and inheritable, low halves, then high halves.
        let dac_override = 1_u32 << 1;
        let data: Vec<u8> = [0x0200_0001_u32, dac_override, 0, 0, 0]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let path = std::ffi::CString::new(dir.join("cat").display().to_string()).unwrap();
        // SAFETY: valid NUL-terminated strings and a buffer of `data.len()`.
        let rc = unsafe {
            libc::setxattr(
                path.as_ptr(),
                c"security.capability".as_ptr(),
                data.as_ptr().cast(),
                data.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "setxattr: {}", std::io::Error::last_os_error());
        Self { dir }
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }
}

impl Drop for FileCapability {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `no_new_privs` disarms file capabilities too: programs like Debian's
/// `ping` (`cap_net_raw+ep`) don't work for a non-root user by default.
/// Either turn it off, or give the user the capability: `--cap-add` raises
/// it ambient, which `no_new_privs` keeps.
#[test]
#[ignore = "needs root"]
fn no_new_privs_disarms_file_capabilities_too() {
    let files = FileCapability::new();
    let read_with = |args: &[&str], cat: &str| {
        shipyard_run(
            &[&["--user", "1000"], args].concat(),
            &format!("{cat} {}", files.path("secret")),
        )
    };
    let guarded = read_with(&[], &files.path("cat"));
    assert!(
        guarded.stderr.contains("Permission denied"),
        "{}",
        guarded.stdout
    );
    let open = read_with(
        &["--security-opt", "no-new-privileges=false"],
        &files.path("cat"),
    );
    assert_eq!(open.stdout, "secret\n", "{}", open.stderr);
    let ambient = read_with(&["--cap-add", "DAC_OVERRIDE"], "cat");
    assert_eq!(ambient.stdout, "secret\n", "{}", ambient.stderr);
}

// --- As root: seccomp ------------------------------------------------------------

/// `Seccomp` in `/proc/…/status`: 0 none, 2 a filter.
const FILTERED: u64 = 2;

/// Under Rosetta no filter can be installed: the tests that need one say so
/// and skip (the test support runs everything else unconfined there).
fn filters_work_here() -> bool {
    if under_rosetta() {
        eprintln!("under Rosetta emulation, where no seccomp filter can be installed; skipping");
    }
    !under_rosetta()
}

/// A seccomp profile in a file, removed on drop.
struct ProfileFile(PathBuf);

impl ProfileFile {
    fn new(profile: &serde_json::Value) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "shipyard-seccomp-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, profile.to_string()).unwrap();
        Self(path)
    }

    /// `seccomp=PATH`.
    fn option(&self) -> String {
        format!("seccomp={}", self.0.display())
    }
}

impl Drop for ProfileFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Docker's profile is on by default: a new user namespace is refused, and
/// `--cap-add SYS_ADMIN` allows it (the profile follows the capabilities);
/// `seccomp=unconfined` means no filter.
#[test]
#[ignore = "needs root"]
fn the_default_filter_is_dockers() {
    if !filters_work_here() {
        return;
    }
    let status = status_with(&[]);
    assert_eq!(field(&status, "Seccomp"), FILTERED, "{status}");
    let refused = shipyard_run(&[], "unshare -U true");
    assert_ne!(refused.code, Some(0), "{}", refused.stdout);
    assert!(
        refused.stderr.contains("Operation not permitted"),
        "{}",
        refused.stderr
    );
    let admin = shipyard_run(&["--cap-add", "SYS_ADMIN"], "unshare -U true");
    assert_eq!(admin.code, Some(0), "{}", admin.stderr);
    let off = status_with(&["--security-opt", "seccomp=unconfined"]);
    assert_eq!(field(&off, "Seccomp"), 0, "{off}");
}

/// A profile of one's own: an errno of its choosing, and a kill (SIGSYS).
#[test]
#[ignore = "needs root"]
fn a_profile_of_ones_own() {
    if !filters_work_here() {
        return;
    }
    let profile = |action: serde_json::Value| {
        let mut rule = json!({"names": ["mkdir", "mkdirat"]});
        rule.as_object_mut()
            .unwrap()
            .extend(action.as_object().unwrap().clone());
        ProfileFile::new(&json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [rule]}))
    };
    let enotdir = profile(json!({"action": "SCMP_ACT_ERRNO", "errnoRet": libc::ENOTDIR}));
    let run = shipyard_run(&["--security-opt", &enotdir.option()], "mkdir /tmp/x");
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stderr.contains("Not a directory"), "{}", run.stderr);

    let kill = profile(json!({"action": "SCMP_ACT_KILL_PROCESS"}));
    let run = run_args(&["--security-opt", &kill.option(), "mkdir", "/tmp/x"]);
    assert_eq!(run.code, Some(killed_by(libc::SIGSYS)), "{}", run.stderr);
}

/// Without `no_new_privs` the filter goes on first, while the process still
/// has `CAP_SYS_ADMIN`, and the switch of user and capabilities run under it.
#[test]
#[ignore = "needs root"]
fn without_no_new_privs_the_filter_goes_on_first() {
    if !filters_work_here() {
        return;
    }
    for user in ["0", "1000"] {
        let status = status_with(&["--user", user, "--security-opt", "no-new-privileges=false"]);
        assert_eq!(field(&status, "Seccomp"), FILTERED, "{status}");
        assert_eq!(field(&status, "NoNewPrivs"), 0, "{status}");
    }
}

/// Under Rosetta, a filter is refused with a hint (and the run fails: never
/// silently unfiltered).
#[test]
#[ignore = "needs root"]
fn under_emulation_a_filter_is_refused_with_a_hint() {
    if !under_rosetta() {
        eprintln!("not under Rosetta emulation, where filters work; skipping");
        return;
    }
    let profile = ProfileFile::new(&json!({"defaultAction": "SCMP_ACT_ALLOW"}));
    let run = run_args(&["--security-opt", &profile.option(), "true"]);
    assert_eq!(run.code, Some(125), "{}", run.stderr);
    assert!(
        run.stderr.contains("installing the seccomp filter")
            && run.stderr.contains("seccomp=unconfined"),
        "{}",
        run.stderr
    );
}

/// A bundle's `linux.seccomp` applies; without one, no filter (the spec,
/// runc).
#[test]
#[ignore = "needs root"]
fn a_bundles_seccomp_is_its_own() {
    let none = bundle_run(SHOW_STATUS, |_| {});
    assert_eq!(none.code, Some(0), "{}", none.stderr);
    assert_eq!(field(&none.stdout, "Seccomp"), 0, "{}", none.stdout);
    if !filters_work_here() {
        return;
    }
    let filtered = bundle_run(SHOW_STATUS, |config| {
        config["linux"]["seccomp"] = json!({"defaultAction": "SCMP_ACT_ALLOW"});
    });
    assert_eq!(filtered.code, Some(0), "{}", filtered.stderr);
    assert_eq!(
        field(&filtered.stdout, "Seccomp"),
        FILTERED,
        "{}",
        filtered.stdout
    );
    // Only the script changes directory: the bundle has `no_new_privs`, so
    // the filter goes on after Shipyard's own `chdir`. (Refusing `openat`
    // instead would stop the dynamic loader before the script starts.)
    let refused = bundle_run("cd /tmp", |config| {
        config["linux"]["seccomp"] = json!({
            "defaultAction": "SCMP_ACT_ALLOW",
            "syscalls": [{"names": ["chdir"], "action": "SCMP_ACT_ERRNO", "errnoRet": libc::ENOTDIR}]
        });
    });
    assert_ne!(refused.code, Some(0), "{}", refused.stdout);
    assert!(refused.stderr.contains("can't cd"), "{}", refused.stderr);
}
