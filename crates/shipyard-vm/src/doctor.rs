//! What `shipyard doctor` checks on a Mac (step 10.3c2): what the VM needs
//! (macOS 13 or later on Apple Silicon, D10.3-4 and D29; this binary signed
//! with the virtualization entitlement; a guest, of this build), how it
//! stands (running or not, its idle stop, its ports on the Mac), and what
//! it shares (the home). The CLI prints them as `doctor`'s lines.

use std::path::Path;

use crate::client::{self, Status};
use crate::idle::IDLE_LIMIT;
use crate::ports::PortsFile;
use crate::{Guest, VmError, VmPaths};

/// How a check went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Works.
    Ok,
    /// Works, with something to know.
    Warn,
    /// The VM can't run, or not as it should.
    Fail,
}

/// One check: its level, its name (a word), what was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// How it went.
    pub level: Level,
    /// Its name.
    pub name: &'static str,
    /// What was found, and what to do if it failed.
    pub text: String,
}

impl Finding {
    fn new(level: Level, name: &'static str, text: impl Into<String>) -> Self {
        Self {
            level,
            name,
            text: text.into(),
        }
    }
}

/// The oldest macOS the VM runs on (D10.3-4).
pub const MACOS_FLOOR: u32 = 13;

/// The entitlement Virtualization.framework asks of a binary.
const ENTITLEMENT: &str = "com.apple.security.virtualization";

/// The checks, for the VM at `paths`, this build being `revision`. Places
/// that can't be had (`Err`) are one check, failed: those that need them
/// aren't made.
#[must_use]
pub fn checks(paths: Result<&VmPaths, &VmError>, revision: &str) -> Vec<Finding> {
    let mut found = vec![
        macos(os_version().as_deref(), std::env::consts::ARCH),
        entitlement(),
    ];
    match paths {
        Ok(paths) => found.extend([
            guest(&paths.guest, revision),
            vm(paths),
            ports(paths),
            docker(
                &paths.docker_socket(),
                crate::launchd::socket_there(&paths.docker_socket()),
                (
                    crate::launchd::program(paths).as_deref(),
                    crate::launchd::state(paths),
                ),
                crate::launchd::this_binary().ok().as_deref(),
            ),
            shares(),
            files(paths),
            disk(&paths.disk(), &paths.dir),
            rosetta(crate::machine::rosetta()),
        ]),
        Err(error) => found.extend([
            shares(),
            Finding::new(Level::Fail, "files", error.to_string()),
        ]),
    }
    if let Ok(paths) = paths
        && crate::launchd::installed_by_older(paths)
    {
        found.push(Finding::new(
            Level::Warn,
            "launchd",
            crate::launchd::OLDER_AGENT,
        ));
    }
    found
}

/// This Mac's Rosetta for Linux: amd64 images run in the VM through it
/// (step 13.3); without it, how to have it.
fn rosetta(rosetta: crate::machine::Rosetta) -> Finding {
    use crate::machine::Rosetta;
    match rosetta {
        Rosetta::Installed => Finding::new(
            Level::Ok,
            "rosetta",
            "installed: amd64 images run in the VM through it (slower than arm64 ones)",
        ),
        Rosetta::NotInstalled => Finding::new(
            Level::Warn,
            "rosetta",
            "not installed: amd64 images won't run (`softwareupdate --install-rosetta \
             --agree-to-license`, then `shipyard vm stop`)",
        ),
        Rosetta::NotSupported => Finding::new(
            Level::Warn,
            "rosetta",
            "not supported on this Mac: amd64 images won't run",
        ),
    }
}

/// The Mac's free space below which `doctor` warns: the guest's ext4 goes
/// read-only when its sparse image can't grow (F103).
const FREE_WARN: u64 = 10 << 30;

/// The Mac's free space below which `doctor` fails.
const FREE_FAIL: u64 = 2 << 30;

/// The data disk (`image`): what its sparse image takes on the Mac (it
/// gives freed space back at each stop: D13.2-3), and the Mac's free space
/// where it lives (`dir`'s volume).
fn disk(image: &Path, dir: &Path) -> Finding {
    use std::os::unix::fs::MetadataExt as _;
    let taken = std::fs::metadata(image).ok().map(|m| m.blocks() * 512);
    let free = nix::sys::statvfs::statvfs(dir)
        .ok()
        .map(|vfs| u64::from(vfs.blocks_available()) * vfs.fragment_size());
    disk_finding(taken, free)
}

/// [`disk`]'s finding from the image's bytes `taken` (none: no disk yet)
/// and the Mac's `free` bytes.
fn disk_finding(taken: Option<u64>, free: Option<u64>) -> Finding {
    // To a tenth of a GiB, in whole numbers (no float's rounding).
    let gib = |bytes: u64| {
        let tenths = u128::from(bytes) * 10 / (1 << 30);
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    };
    let taken = taken.map_or_else(
        || "not made yet".to_owned(),
        |bytes| format!("takes {}", gib(bytes)),
    );
    match free {
        None => Finding::new(
            Level::Warn,
            "disk",
            format!("{taken}; this Mac's free space can't be read"),
        ),
        Some(free) if free < FREE_FAIL => Finding::new(
            Level::Fail,
            "disk",
            format!(
                "{taken}; this Mac has {} free: the VM's disk can't grow, and its files go \
                 read-only (free some space; what `shipyard system prune` frees comes back at \
                 the VM's stop: `shipyard vm stop`)",
                gib(free)
            ),
        ),
        Some(free) if free < FREE_WARN => Finding::new(
            Level::Warn,
            "disk",
            format!("{taken}; this Mac has only {} free", gib(free)),
        ),
        Some(free) => Finding::new(
            Level::Ok,
            "disk",
            format!("{taken}; {} free on this Mac", gib(free)),
        ),
    }
}

/// Docker's socket (`socket`): launchd's any time, once `shipyard vm
/// install` has loaded its agent (`agent`: the program it runs), else only
/// while the VM runs. An agent whose socket isn't `there` (another host
/// took it, or the directory was emptied) is said, with the fix. An agent that runs another binary than this one
/// (`ours`: moved, or rebuilt elsewhere) is said, with the fix.
fn docker(
    socket: &Path,
    there: bool,
    (agent, state): (Option<&Path>, crate::launchd::AgentState),
    ours: Option<&Path>,
) -> Finding {
    let socket = socket.display();
    match agent {
        // (Docker's socket is then no one's: F152.)
        None if state == crate::launchd::AgentState::NotLoaded => Finding::new(
            Level::Warn,
            "docker",
            format!("{socket}: {}", crate::launchd::NOT_LOADED),
        ),
        None => Finding::new(
            Level::Ok,
            "docker",
            format!("{socket} while the VM runs (`shipyard vm install`: any time, by launchd)"),
        ),
        Some(_) if !there => Finding::new(
            Level::Warn,
            "docker",
            format!(
                "launchd's agent is loaded but {socket} isn't there: run `shipyard vm install` \
                 again"
            ),
        ),
        Some(agent)
            if let Some(folder) = crate::launchd::protected(
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .as_deref(),
                agent,
            ) =>
        {
            Finding::new(
                Level::Warn,
                "docker",
                format!(
                    "launchd starts {}, in your {folder} folder: macOS lets it run only with \
                     your permission. Put shipyard elsewhere, then `shipyard vm install` again",
                    agent.display()
                ),
            )
        }
        Some(agent) if ours.is_some_and(|ours| ours != agent) => Finding::new(
            Level::Warn,
            "docker",
            format!(
                "launchd starts {}, not this shipyard: run `shipyard vm install` again",
                agent.display()
            ),
        ),
        Some(_) => Finding::new(
            Level::Ok,
            "docker",
            format!("{socket} (launchd's: any time)"),
        ),
    }
}

/// This Mac's version, as `sw_vers` says it (`kern.osproductversion`).
fn os_version() -> Option<String> {
    crate::sysctl::string_named(c"kern.osproductversion")
}

/// Why the VM can't run on this Mac's macOS (`version`, `sw_vers`'s), if it
/// can't: older than [`MACOS_FLOOR`] (step 13.7: refused by name at the
/// VM's start, as `doctor` says it). One that can't be read isn't refused.
#[must_use]
pub fn floor_refusal(version: Option<&str>) -> Option<String> {
    let version = version?;
    (major(version)? < MACOS_FLOOR)
        .then(|| format!("macOS {version}: the VM needs macOS {MACOS_FLOOR} or later"))
}

/// [`floor_refusal`] for this Mac.
#[must_use]
pub fn this_macs_floor_refusal() -> Option<String> {
    floor_refusal(os_version().as_deref())
}

/// The major version of a macOS version (`13.6.1` → 13).
fn major(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

/// `version` and `arch` against what the VM needs.
fn macos(version: Option<&str>, arch: &str) -> Finding {
    let Some(version) = version else {
        return Finding::new(Level::Warn, "macos", "the version can't be read");
    };
    if arch != "aarch64" {
        return Finding::new(
            Level::Fail,
            "macos",
            format!("macOS {version} on {arch}: the VM needs Apple Silicon"),
        );
    }
    if let Some(refused) = floor_refusal(Some(version)) {
        return Finding::new(Level::Fail, "macos", refused);
    }
    match major(version) {
        Some(_) => Finding::new(
            Level::Ok,
            "macos",
            format!("macOS {version}, Apple Silicon"),
        ),
        None => Finding::new(
            Level::Warn,
            "macos",
            format!("{version:?} isn't a version this reads: not checked"),
        ),
    }
}

/// Whether this binary is signed with the virtualization entitlement.
fn entitlement() -> Finding {
    let Ok(exe) = std::env::current_exe() else {
        return Finding::new(Level::Warn, "entitlement", "this binary can't be found");
    };
    match codesign(&exe) {
        Ok((shown, signature)) => entitled(&shown, signer(&signature).as_deref(), &exe),
        Err(why) => Finding::new(
            Level::Warn,
            "entitlement",
            format!("codesign {why}: not checked"),
        ),
    }
}

/// How long `codesign` may take.
const CODESIGN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What `codesign` shows of `exe`: its entitlements (stdout) and its
/// signature (stderr: `-dvv`), within [`CODESIGN_LIMIT`] (a stalled one is
/// killed).
fn codesign(exe: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let shown = crate::child::output_within(
        std::process::Command::new("/usr/bin/codesign")
            .args(["-dvv", "--entitlements", "-", "--xml"])
            .arg(exe),
        CODESIGN_LIMIT,
    )
    .map_err(|e| format!("can't be run ({e})"))?
    .ok_or_else(|| format!("didn't answer within {} s", CODESIGN_LIMIT.as_secs()))?;
    Ok((shown.stdout, shown.stderr))
}

/// Who signed a binary, as `codesign -dvv` says it (`signature`): its first
/// authority, or `None` for an ad-hoc signature (or none read).
fn signer(signature: &[u8]) -> Option<String> {
    String::from_utf8_lossy(signature)
        .lines()
        .find_map(|line| line.strip_prefix("Authority="))
        .map(str::to_owned)
}

/// [`entitlement`]'s finding from `codesign`'s output for `exe` (`shown`,
/// its entitlements; `signer`, who signed it, `None` ad hoc): signed to run
/// a VM, and by an identity that's the same across rebuilds or not (step
/// 13.7, F140: macOS asks again for the guarded folders after each rebuild
/// of an ad-hoc one).
fn entitled(shown: &[u8], signer: Option<&str>, exe: &Path) -> Finding {
    let shown = String::from_utf8_lossy(shown);
    let key = format!("<key>{ENTITLEMENT}</key>");
    match shown.find(&key) {
        Some(at) if shown[at + key.len()..].trim_start().starts_with("<true/>") => {
            let how = signer.map_or_else(
                || {
                    "ad hoc: macOS asks again for your folders after each rebuild; \
                     scripts/macos/make-signing-identity.sh makes an identity that stays"
                        .to_owned()
                },
                |signer| format!("{signer}: one identity across rebuilds"),
            );
            Finding::new(
                Level::Ok,
                "entitlement",
                format!("signed to run a VM ({how})"),
            )
        }
        _ => Finding::new(
            Level::Fail,
            "entitlement",
            format!(
                "{} isn't signed to run a VM: scripts/macos/sign.sh {0}",
                exe.display()
            ),
        ),
    }
}

/// The guest at `dir`, and whether it's this build's.
fn guest(dir: &Path, revision: &str) -> Finding {
    match Guest::find(dir) {
        Ok(guest) => {
            let what = format!(
                "Alpine {}, kernel {}, shipyard {}",
                guest.manifest.alpine,
                guest.manifest.kernel.version,
                guest.manifest.shipyard.revision
            );
            if guest.manifest.shipyard.revision == revision {
                Finding::new(Level::Ok, "guest", what)
            } else {
                Finding::new(
                    Level::Warn,
                    "guest",
                    format!(
                        "{what}: another build than this shipyard's ({revision}); \
                         scripts/guest/build.sh makes it this one's"
                    ),
                )
            }
        }
        Err(error) => Finding::new(Level::Fail, "guest", error.to_string()),
    }
}

/// Whether the VM runs, and when it stops by itself: after the running
/// host's limit, or the one a start now would have (`SHIPYARD_VM_IDLE`).
fn vm(paths: &VmPaths) -> Finding {
    let running = !matches!(client::status(paths), Ok(Status::Stopped));
    let limit = running
        .then(|| PortsFile::read(&paths.ports()).ok().flatten())
        .flatten()
        .map_or_else(
            || {
                let setting = std::env::var("SHIPYARD_VM_IDLE").ok();
                crate::idle::idle_limit(setting.as_deref()).unwrap_or(IDLE_LIMIT)
            },
            |file| std::time::Duration::from_secs(file.idle_seconds),
        );
    let idle = format!("it stops after {} with nothing running", span(limit));
    match client::status(paths) {
        // Another build's guest booted (F173b): what to do said.
        Ok(Status::Running(Some(pid)))
            if let Some(another) = crate::booted::running_elsewhere(paths) =>
        {
            Finding::new(
                Level::Warn,
                "vm",
                format!("running (host PID {pid}), but {another}"),
            )
        }
        Ok(Status::Running(Some(pid))) => {
            Finding::new(Level::Ok, "vm", format!("running (host PID {pid}); {idle}"))
        }
        Ok(Status::Running(None)) => Finding::new(Level::Ok, "vm", format!("running; {idle}")),
        Ok(Status::Stopped) => Finding::new(
            Level::Ok,
            "vm",
            format!("stopped: it starts on first use, and {idle}"),
        ),
        Err(error) => Finding::new(Level::Warn, "vm", error.to_string()),
    }
}

/// `limit` as said: whole minutes, else seconds.
fn span(limit: std::time::Duration) -> String {
    match limit.as_secs() {
        seconds if seconds >= 60 && seconds % 60 == 0 => format!("{} min", seconds / 60),
        seconds => format!("{seconds} s"),
    }
}

/// The ports the VM forwards to this Mac, while it runs.
fn ports(paths: &VmPaths) -> Finding {
    const ONLY: &str = "published ports are reached on this Mac's loopback only";
    if client::status(paths).is_ok_and(|status| status == Status::Stopped) {
        return Finding::new(Level::Ok, "ports", ONLY);
    }
    match PortsFile::read(&paths.ports()) {
        Ok(file) => {
            let shadowed = file
                .as_ref()
                .map(crate::ports::shadowed)
                .unwrap_or_default();
            ports_found(file.as_ref(), &shadowed, ONLY)
        }
        Err(error) => Finding::new(Level::Warn, "ports", error.to_string()),
    }
}

/// [`ports`]'s finding from what the host wrote, with the forwarded ports
/// another program shadows (`shadowed`, F98; `only`: the rule said).
fn ports_found(
    file: Option<&PortsFile>,
    shadowed: &[(u16, shipyard_runtime::attach::forward::Protocol)],
    only: &str,
) -> Finding {
    let Some(file) = file else {
        return Finding::new(Level::Ok, "ports", only);
    };
    let mut not = Vec::new();
    for (address, why) in &file.unbound {
        not.push(format!("{address} ({why})"));
    }
    for (address, why) in &file.unbound_udp {
        not.push(format!("{address}/udp ({why})"));
    }
    if file.udp > 0 {
        not.push(format!("{} UDP (an older guest's)", file.udp));
    }
    let shadows: Vec<String> = shadowed
        .iter()
        .map(|(port, protocol)| format!("{port}{}", if protocol.is_tcp() { "" } else { "/udp" }))
        .collect();
    if not.is_empty() && shadows.is_empty() {
        return Finding::new(
            Level::Ok,
            "ports",
            format!(
                "{} forwarded; {only}",
                file.forwarded.len() + file.forwarded_udp.len()
            ),
        );
    }
    let mut said = Vec::new();
    if !not.is_empty() {
        said.push(format!("not forwarded: {}", not.join(", ")));
    }
    if !shadows.is_empty() {
        said.push(format!(
            "shadowed: {} (another program on this Mac listens on every address of it, yet \
             localhost reaches the container)",
            shadows.join(", ")
        ));
    }
    Finding::new(Level::Warn, "ports", format!("{}; {only}", said.join("; ")))
}

/// The places the VM shares, each at its own path: the home, and those
/// beside it (step 13.4).
fn shares() -> Finding {
    let Some(home) = std::env::var_os("HOME") else {
        return Finding::new(Level::Fail, "shares", "HOME isn't set: nothing to share");
    };
    match std::fs::canonicalize(&home) {
        Ok(home) => match crate::host::cmdline(&home.to_string_lossy(), &[], &[], false) {
            Ok(_) => {
                let shared: Vec<String> = crate::shares::all(&home)
                    .iter()
                    .map(|place| place.display().to_string())
                    .collect();
                Finding::new(
                    Level::Ok,
                    "shares",
                    format!("{} shared with the VM, read-write", shared.join(", ")),
                )
            }
            Err(error) => Finding::new(Level::Fail, "shares", error.to_string()),
        },
        Err(error) => Finding::new(
            Level::Fail,
            "shares",
            format!("{}: {error}", Path::new(&home).display()),
        ),
    }
}

/// The VM's directory: the user's own, if it's there yet.
fn files(paths: &VmPaths) -> Finding {
    match paths.check(nix::unistd::getuid().as_raw()) {
        Ok(()) => Finding::new(Level::Ok, "files", paths.dir.display().to_string()),
        Err(error) => Finding::new(Level::Fail, "files", error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The disk's line: what the image takes, the Mac's free space, a warning
    /// under 10 GiB free, a failure under 2 GiB.
    #[test]
    fn the_disk_is_said_with_the_macs_free_space() {
        let ok = disk_finding(Some(3 << 30), Some(100 << 30));
        assert_eq!(ok.level, Level::Ok);
        assert_eq!(ok.text, "takes 3.0 GiB; 100.0 GiB free on this Mac");
        assert_eq!(
            disk_finding(Some(1 << 30), Some(9 << 30)).level,
            Level::Warn
        );
        let full = disk_finding(None, Some(1 << 30));
        assert_eq!(full.level, Level::Fail);
        assert!(
            full.text
                .starts_with("not made yet; this Mac has 1.0 GiB free"),
            "{}",
            full.text
        );
        assert_eq!(disk_finding(Some(0), None).level, Level::Warn);
    }

    /// Docker's socket: while the VM runs without an agent; launchd's with
    /// one that runs this binary; a warning, with the fix, for one that
    /// runs another.
    #[test]
    fn dockers_socket_says_who_holds_it() {
        use crate::launchd::AgentState;
        let socket = Path::new("/vm/docker.sock");
        let ours = Path::new("/bin/shipyard");
        let none = docker(socket, false, (None, AgentState::NotInstalled), Some(ours));
        assert_eq!(none.level, Level::Ok);
        assert!(none.text.contains("while the VM runs"), "{}", none.text);
        let installed = docker(socket, true, (Some(ours), AgentState::Loaded), Some(ours));
        assert_eq!(installed.level, Level::Ok);
        assert!(installed.text.contains("launchd's"), "{}", installed.text);
        let moved = docker(
            socket,
            true,
            (Some(Path::new("/old/shipyard")), AgentState::Loaded),
            Some(ours),
        );
        assert_eq!(moved.level, Level::Warn);
        assert!(moved.text.contains("shipyard vm install"), "{}", moved.text);
        let lost = docker(socket, false, (Some(ours), AgentState::Loaded), Some(ours));
        assert_eq!(lost.level, Level::Warn);
        assert!(lost.text.contains("isn't there"), "{}", lost.text);
        let unloaded = docker(socket, false, (None, AgentState::NotLoaded), Some(ours));
        assert_eq!(unloaded.level, Level::Warn);
        assert!(
            unloaded.text.contains("installed but not loaded"),
            "{}",
            unloaded.text
        );
    }

    /// macOS 13 or later on Apple Silicon; an older one, or Intel, fails;
    /// an unread version warns.
    #[test]
    fn the_macos_floor_is_13_on_apple_silicon() {
        for (version, arch, level) in [
            ("13.0", "aarch64", Level::Ok),
            ("27.0", "aarch64", Level::Ok),
            ("12.7.1", "aarch64", Level::Fail),
            ("14.1", "x86_64", Level::Fail),
            ("junk", "aarch64", Level::Warn),
        ] {
            assert_eq!(macos(Some(version), arch).level, level, "{version} {arch}");
        }
        assert_eq!(macos(None, "aarch64").level, Level::Warn);
    }

    /// The entitlement is found only as `codesign` shows it granted.
    #[test]
    fn the_entitlement_is_read_from_codesigns_xml() {
        let exe = Path::new("/x/shipyard");
        let granted = format!(
            "<?xml version=\"1.0\"?><plist><dict><key>{ENTITLEMENT}</key>\n\t<true/></dict></plist>"
        );
        assert_eq!(entitled(granted.as_bytes(), None, exe).level, Level::Ok);
        let refused = format!("<plist><dict><key>{ENTITLEMENT}</key><false/></dict></plist>");
        assert_eq!(entitled(refused.as_bytes(), None, exe).level, Level::Fail);
        assert_eq!(entitled(b"", None, exe).level, Level::Fail);
        let other = b"<plist><dict><key>com.apple.security.app-sandbox</key><true/></dict></plist>";
        assert_eq!(entitled(other, None, exe).level, Level::Fail);
        // Who signed it said: an identity kept across rebuilds, or ad hoc.
        let by = entitled(granted.as_bytes(), Some("Shipyard Local Signing"), exe);
        assert!(
            by.text
                .contains("Shipyard Local Signing: one identity across rebuilds"),
            "{}",
            by.text
        );
        let ad_hoc = entitled(granted.as_bytes(), None, exe);
        assert!(
            ad_hoc.text.contains("make-signing-identity.sh"),
            "{}",
            ad_hoc.text
        );
    }

    /// The signer is `codesign -dvv`'s first authority; an ad-hoc
    /// signature has none.
    #[test]
    fn the_signer_is_codesigns_first_authority() {
        let signed = b"Executable=/x\nIdentifier=shipyard\nAuthority=Shipyard Local Signing\nAuthority=Other\n";
        assert_eq!(signer(signed).as_deref(), Some("Shipyard Local Signing"));
        assert_eq!(signer(b"Executable=/x\nSignature=adhoc\n"), None);
        assert_eq!(signer(b""), None);
    }

    /// A Mac older than the floor is refused by name; the floor and newer,
    /// or a version that can't be read, aren't.
    #[test]
    fn an_older_macos_is_refused_by_name() {
        let refused = floor_refusal(Some("12.7.4")).unwrap();
        assert!(
            refused.contains("macOS 12.7.4: the VM needs macOS 13 or later"),
            "{refused}"
        );
        for fine in [Some("13.0"), Some("27.0.1"), Some("x"), None] {
            assert_eq!(floor_refusal(fine), None, "{fine:?}");
        }
    }

    /// No guest fails; another build's warns; this build's is fine.
    #[test]
    fn a_guest_is_needed_and_this_builds_is_best() {
        let dir =
            std::env::temp_dir().join(format!("shipyard-doctor-guest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(guest(&dir, "abc").level, Level::Fail);
        std::fs::write(
            dir.join(crate::MANIFEST),
            r#"{"version": 1, "alpine": "3.24.2", "minirootfs_sha256": "9bf7",
                "packages": [], "kernel": {"version": "6.18.57-0-virt", "sha256": "1993"},
                "initramfs": {"sha256": "e1c3"},
                "shipyard": {"revision": "abc", "sha256": "209b"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("Image"), b"k").unwrap();
        std::fs::write(dir.join("initramfs.cpio.gz"), b"i").unwrap();
        assert_eq!(guest(&dir, "abc").level, Level::Ok);
        assert_eq!(guest(&dir, "def").level, Level::Warn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The idle limit as said: whole minutes, else seconds.
    #[test]
    fn the_idle_limit_is_said_plainly() {
        use std::time::Duration;
        assert_eq!(span(Duration::from_secs(3)), "3 s");
        assert_eq!(span(Duration::from_secs(300)), "5 min");
        assert_eq!(span(Duration::from_secs(90)), "90 s");
    }

    /// Ports that aren't forwarded warn, each with why; none, all's well.
    #[test]
    fn ports_not_forwarded_are_said() {
        use crate::ports::Unbound;
        use shipyard_runtime::attach::forward::Protocol::{Tcp, Udp};
        let only = "loopback only";
        assert_eq!(ports_found(None, &[], only).level, Level::Ok);
        let fine = PortsFile::new(
            &[
                (Tcp, "127.0.0.1:80".parse().unwrap()),
                (Udp, "127.0.0.1:53".parse().unwrap()),
            ],
            &[],
            0,
            IDLE_LIMIT,
        );
        let said = ports_found(Some(&fine), &[], only);
        assert_eq!(said.level, Level::Ok);
        assert!(said.text.starts_with("2 forwarded"), "{}", said.text);
        // A shadowed port alone warns, though everything's forwarded.
        let shadowed = ports_found(Some(&fine), &[(80, Tcp)], only);
        assert_eq!(shadowed.level, Level::Warn);
        assert!(
            shadowed.text.starts_with("shadowed: 80 ("),
            "{}",
            shadowed.text
        );
        let busy = PortsFile::new(
            &[],
            &[
                ((Tcp, "127.0.0.1:5432".parse().unwrap()), Unbound::InUse),
                ((Udp, "127.0.0.1:53".parse().unwrap()), Unbound::InUse),
            ],
            1,
            IDLE_LIMIT,
        );
        let said = ports_found(Some(&busy), &[(8080, Tcp)], only);
        assert_eq!(said.level, Level::Warn);
        assert!(
            said.text.contains(
                "not forwarded: 127.0.0.1:5432 (in use on this Mac), 127.0.0.1:53/udp (in use \
                 on this Mac), 1 UDP (an older guest's); shadowed: 8080 (another program on \
                 this Mac listens on every address of it, yet localhost reaches the container)"
            ),
            "{}",
            said.text
        );
    }
}
