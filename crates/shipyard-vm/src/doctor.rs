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
            home(),
            files(paths),
        ]),
        Err(error) => found.extend([
            home(),
            Finding::new(Level::Fail, "files", error.to_string()),
        ]),
    }
    found
}

/// This Mac's version, as `sw_vers` says it (`kern.osproductversion`).
fn os_version() -> Option<String> {
    let name = c"kern.osproductversion";
    let mut buffer = [0_u8; 64];
    let mut len = buffer.len();
    // SAFETY: `name` is a C string; `buffer` and `len` are valid, `len` its
    // size, which the call updates to what it wrote; no new value is set.
    let done = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if done != 0 {
        return None;
    }
    let text = buffer.get(..len)?;
    let text = text.split(|&byte| byte == 0).next()?;
    String::from_utf8(text.to_vec()).ok()
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
    match major(version) {
        Some(major) if major >= MACOS_FLOOR => Finding::new(
            Level::Ok,
            "macos",
            format!("macOS {version}, Apple Silicon"),
        ),
        Some(_) => Finding::new(
            Level::Fail,
            "macos",
            format!("macOS {version}: the VM needs macOS {MACOS_FLOOR} or later"),
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
        Ok(shown) => entitled(&shown, &exe),
        Err(why) => Finding::new(
            Level::Warn,
            "entitlement",
            format!("codesign {why}: not checked"),
        ),
    }
}

/// How long `codesign` may take.
const CODESIGN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What `codesign` shows of `exe`'s entitlements, within
/// [`CODESIGN_LIMIT`] (a stalled one is killed). Its output is small: it
/// fits in the pipe while it's waited for.
fn codesign(exe: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut child = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("can't be run ({e})"))?;
    let began = std::time::Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if began.elapsed() > CODESIGN_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "didn't answer within {} s",
                CODESIGN_LIMIT.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut shown = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut shown).map_err(|e| e.to_string())?;
    }
    Ok(shown)
}

/// [`entitlement`]'s finding from `codesign`'s output for `exe`.
fn entitled(shown: &[u8], exe: &Path) -> Finding {
    let shown = String::from_utf8_lossy(shown);
    let key = format!("<key>{ENTITLEMENT}</key>");
    match shown.find(&key) {
        Some(at) if shown[at + key.len()..].trim_start().starts_with("<true/>") => {
            Finding::new(Level::Ok, "entitlement", "signed to run a VM")
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
        Ok(file) => ports_found(file.as_ref(), ONLY),
        Err(error) => Finding::new(Level::Warn, "ports", error.to_string()),
    }
}

/// [`ports`]'s finding from what the host wrote (`only`: the rule said).
fn ports_found(file: Option<&PortsFile>, only: &str) -> Finding {
    let Some(file) = file else {
        return Finding::new(Level::Ok, "ports", only);
    };
    let mut not = Vec::new();
    for (address, why) in &file.unbound {
        not.push(format!("{address} ({why})"));
    }
    if file.udp > 0 {
        not.push(format!("{} UDP (only TCP is, for now)", file.udp));
    }
    if not.is_empty() {
        return Finding::new(
            Level::Ok,
            "ports",
            format!("{} forwarded; {only}", file.forwarded.len()),
        );
    }
    Finding::new(
        Level::Warn,
        "ports",
        format!("not forwarded: {}; {only}", not.join(", ")),
    )
}

/// The home the VM shares, at its own path.
fn home() -> Finding {
    let Some(home) = std::env::var_os("HOME") else {
        return Finding::new(Level::Fail, "home", "HOME isn't set: nothing to share");
    };
    match std::fs::canonicalize(&home) {
        Ok(home) => match crate::host::cmdline(&home.to_string_lossy(), &[]) {
            Ok(_) => Finding::new(
                Level::Ok,
                "home",
                format!("{} is shared with the VM, read-write", home.display()),
            ),
            Err(error) => Finding::new(Level::Fail, "home", error.to_string()),
        },
        Err(error) => Finding::new(
            Level::Fail,
            "home",
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
        assert_eq!(entitled(granted.as_bytes(), exe).level, Level::Ok);
        let refused = format!("<plist><dict><key>{ENTITLEMENT}</key><false/></dict></plist>");
        assert_eq!(entitled(refused.as_bytes(), exe).level, Level::Fail);
        assert_eq!(entitled(b"", exe).level, Level::Fail);
        let other = b"<plist><dict><key>com.apple.security.app-sandbox</key><true/></dict></plist>";
        assert_eq!(entitled(other, exe).level, Level::Fail);
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
        let only = "loopback only";
        assert_eq!(ports_found(None, only).level, Level::Ok);
        let fine = PortsFile::new(
            vec!["127.0.0.1:80".parse().unwrap()],
            Vec::new(),
            0,
            IDLE_LIMIT,
        );
        assert_eq!(ports_found(Some(&fine), only).level, Level::Ok);
        let busy = PortsFile::new(
            Vec::new(),
            vec![("127.0.0.1:5432".parse().unwrap(), Unbound::InUse)],
            1,
            IDLE_LIMIT,
        );
        let said = ports_found(Some(&busy), only);
        assert_eq!(said.level, Level::Warn);
        assert!(
            said.text
                .contains("127.0.0.1:5432 (in use on this Mac), 1 UDP"),
            "{}",
            said.text
        );
    }
}
