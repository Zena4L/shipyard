//! Where the VM keeps its files: one directory of the user's own (the data
//! disk, the logs, the lock and the socket), and the guest's files (the
//! kernel, the initramfs and their manifest).

use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::VmError;

/// The longest path a Unix socket can be bound at on macOS (`sun_path` is
/// 104 bytes, with its NUL).
pub const MAX_SOCKET_PATH: usize = 103;

/// The VM's places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPaths {
    /// The VM's own directory: the user's, mode 0700.
    pub dir: PathBuf,
    /// The guest's files ([`crate::Manifest`] and what it names).
    pub guest: PathBuf,
    /// Whether [`guest`](Self::guest) was found beside this binary (a
    /// release's or Homebrew's keg), not given nor the VM directory's: a
    /// place that moves with the binary, never written down for later
    /// (`vm install`'s agent finds it again from the binary it runs: F178).
    pub guest_beside: bool,
}

/// The guest's files when `SHIPYARD_GUEST_DIR` doesn't say (13.6b2, F106):
/// the VM directory's `guest` if a guest is there; else the `guest`
/// directory beside the binary's own (`exe`, canonical:
/// `target/debug/shipyard` finds `target/guest`, where `build.sh` puts it;
/// a release's `bin/shipyard` its `guest`, where its tarball and Homebrew
/// put it: F178), only if it and the directory it's in are the user's or
/// root's and no one else may write them (a `shipyard` run from `/tmp/x`
/// mustn't boot a guest another user put in `/tmp/guest`, with the home
/// shared: 13.6b2's review; root's install under `/usr/local` or `/opt` is
/// root's, who could replace the binary as well); else the VM directory's,
/// whose absence is then said.
#[must_use]
pub(crate) fn guest_dir(vm_dir: &Path, exe: Option<&Path>) -> PathBuf {
    let kept = vm_dir.join("guest");
    let has_guest = |dir: &Path| dir.join(crate::MANIFEST).is_file();
    if has_guest(&kept) {
        return kept;
    }
    let me = nix::unistd::geteuid().as_raw();
    let trusted = |path: &Path, dir: bool| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            (if dir {
                metadata.is_dir()
            } else {
                metadata.is_file()
            }) && trusted_place(metadata.uid(), metadata.mode(), me)
        })
    };
    // Its files too (no link among them): a guest's directory of root's
    // holding another user's kernel is that user's guest.
    let trusted_files = |dir: &Path| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .all(|path| path.is_ok_and(|path| trusted(&path, false)))
        })
    };
    exe.and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|built| trusted(built, true))
        .map(|built| built.join("guest"))
        .filter(|beside| has_guest(beside) && trusted(beside, true) && trusted_files(beside))
        .unwrap_or(kept)
}

/// Whether a directory (or one of its files) owned by `owner`, with `mode`,
/// may hold the guest a user `me` boots: theirs or root's, and no one else
/// may write to it.
fn trusted_place(owner: u32, mode: u32, me: u32) -> bool {
    (owner == me || owner == 0) && mode & 0o022 == 0
}

impl VmPaths {
    /// The places, from the environment: `SHIPYARD_VM_DIR` (else
    /// `~/Library/Application Support/Shipyard/vm`) and `SHIPYARD_GUEST_DIR`
    /// (else its `guest` directory).
    ///
    /// # Errors
    ///
    /// [`VmError::Place`]: no home to place it in, or a relative path.
    pub fn from_env() -> Result<Self, VmError> {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        let dir = if let Some(dir) = var("SHIPYARD_VM_DIR") {
            PathBuf::from(dir)
        } else {
            let home = var("HOME").ok_or_else(|| {
                VmError::Place("HOME isn't set: no place for the VM's files".to_owned())
            })?;
            Self::default_dir(Path::new(&home))
        };
        let (guest, guest_beside) = var("SHIPYARD_GUEST_DIR").map_or_else(
            || {
                let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok();
                let found = guest_dir(&dir, exe.as_deref());
                let beside = found != dir.join("guest");
                (found, beside)
            },
            |given| (PathBuf::from(given), false),
        );
        let mut paths = Self::at(dir, guest)?;
        paths.guest_beside = guest_beside;
        Ok(paths)
    }

    /// The user's VM directory when none is set: in `home`'s Application
    /// Support, as a Mac app's.
    #[must_use]
    pub fn default_dir(home: &Path) -> PathBuf {
        home.join("Library/Application Support/Shipyard/vm")
    }

    /// The places at `dir` and `guest`, which must be absolute.
    ///
    /// # Errors
    ///
    /// [`VmError::Place`] for a relative path.
    pub fn at(dir: PathBuf, guest: PathBuf) -> Result<Self, VmError> {
        for (name, path) in [("the VM's directory", &dir), ("the guest's files", &guest)] {
            if !path.is_absolute() {
                return Err(VmError::Place(format!(
                    "{name} must be an absolute path: {}",
                    path.display()
                )));
            }
        }
        Ok(Self {
            dir,
            guest,
            guest_beside: false,
        })
    }

    /// The host's lock: held for as long as it runs (its PID written in it).
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.dir.join("vm.lock")
    }

    /// The host's socket, which the CLI connects to.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.dir.join("vm.sock")
    }

    /// Docker's API, for Docker's clients on the Mac (step 11.5b1): the
    /// longest of the host's sockets' paths, the one checked against a
    /// socket's address.
    #[must_use]
    pub fn docker_socket(&self) -> PathBuf {
        self.dir.join("docker.sock")
    }

    /// `vm install`'s mark that the run at load it causes is its own, never a
    /// login's (13.6b2: it boots no VM).
    #[must_use]
    pub fn just_installed(&self) -> PathBuf {
        self.dir.join("just-installed")
    }

    /// The CLI's mark that it asked launchd's agent to start the host (no
    /// connection waits for it then: step 11.5b2a).
    #[must_use]
    pub fn start_asked(&self) -> PathBuf {
        self.dir.join("start-asked")
    }

    /// The guest's data disk (a sparse file).
    #[must_use]
    pub fn disk(&self) -> PathBuf {
        self.dir.join("data.img")
    }

    /// A new data disk, kept as [`disk`](Self::disk) once a guest has
    /// booted with it.
    #[must_use]
    pub fn new_disk(&self) -> PathBuf {
        self.dir.join("data.img.new")
    }

    /// The guest's console, as it boots and runs.
    #[must_use]
    pub fn console_log(&self) -> PathBuf {
        self.dir.join("console.log")
    }

    /// The host process's own log.
    #[must_use]
    pub fn host_log(&self) -> PathBuf {
        self.dir.join("host.log")
    }

    /// The host's own socket (step 13.6b1): where it stands, and a stop
    /// asked of it, from its lock to its end, booting or not.
    #[must_use]
    pub fn host_socket(&self) -> PathBuf {
        self.dir.join("host.sock")
    }

    /// How many containers would come back if the VM started again
    /// ([`crate::comes_back::ComesBack`]), kept across its stops.
    #[must_use]
    pub fn comes_back(&self) -> PathBuf {
        self.dir.join("comes-back.json")
    }

    /// What the running host booted ([`crate::booted::Booted`]).
    #[must_use]
    pub fn booted(&self) -> PathBuf {
        self.dir.join("booted.json")
    }

    /// What the host forwards ([`crate::ports::PortsFile`]).
    #[must_use]
    pub fn ports(&self) -> PathBuf {
        self.dir.join("ports.json")
    }

    /// The network card's address, kept from boot to boot (`network.json`).
    #[must_use]
    pub fn network(&self) -> PathBuf {
        self.dir.join("network.json")
    }

    /// The VM's size, as `vm set` asked for it (`vm.json`).
    #[must_use]
    pub fn settings(&self) -> PathBuf {
        self.dir.join("vm.json")
    }

    /// Makes the VM's directory if it isn't there, and checks it's the
    /// user's own: a directory (not a link to one), owned by `uid`, that no
    /// one else can enter. Its socket's path must fit a socket's.
    ///
    /// # Errors
    ///
    /// [`VmError::Place`] for each refusal; [`VmError::Io`] if it can't be
    /// made or read.
    pub fn prepare(&self, uid: u32) -> Result<(), VmError> {
        self.check_socket()?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|error| VmError::io(&self.dir, error))?;
        let metadata =
            std::fs::symlink_metadata(&self.dir).map_err(|error| VmError::io(&self.dir, error))?;
        checked_dir(&self.dir, &metadata, uid)
    }

    /// [`VmPaths::prepare`]'s checks, without making the directory (`doctor`
    /// only looks): fine if it isn't there yet.
    ///
    /// # Errors
    ///
    /// [`VmError::Place`] or [`VmError::Io`], as [`VmPaths::prepare`].
    pub fn check(&self, uid: u32) -> Result<(), VmError> {
        self.check_socket()?;
        match std::fs::symlink_metadata(&self.dir) {
            Ok(metadata) => checked_dir(&self.dir, &metadata, uid),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(VmError::io(&self.dir, error)),
        }
    }

    /// The host's sockets' paths fit a socket's address: the longest
    /// ([`docker_socket`](Self::docker_socket)) checked, so the host never
    /// boots the guest and then can't serve one.
    fn check_socket(&self) -> Result<(), VmError> {
        let socket = self.docker_socket();
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(VmError::Place(format!(
                "the VM's socket path is {} bytes, more than a socket's {MAX_SOCKET_PATH}: \
                 set SHIPYARD_VM_DIR to a shorter directory ({})",
                socket.as_os_str().len(),
                socket.display()
            )));
        }
        Ok(())
    }
}

/// Whether `metadata` (of `dir`, not followed) is a directory of `uid`'s
/// that only it can enter.
fn checked_dir(dir: &Path, metadata: &std::fs::Metadata, uid: u32) -> Result<(), VmError> {
    let refused = |why: &str| {
        Err(VmError::Place(format!(
            "{} {why}: the VM's directory must be your own, mode 0700",
            dir.display()
        )))
    };
    if !metadata.file_type().is_dir() {
        return refused("isn't a directory (a link is refused)");
    }
    if metadata.uid() != uid {
        return refused("belongs to someone else");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return refused("can be entered by others");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The VM directory's guest first; else the one beside the binary's
    /// directory; else the VM directory's (to be said missing).
    #[test]
    fn the_guest_is_found_beside_the_binary() {
        let root = std::env::temp_dir().join(format!("shipyard-guest-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (vm, built) = (root.join("vm"), root.join("target"));
        let exe = built.join("debug/shipyard");
        std::fs::create_dir_all(vm.join("guest")).unwrap();
        std::fs::create_dir_all(built.join("guest")).unwrap();
        for dir in [&built, &built.join("guest")] {
            std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            vm.join("guest"),
            "neither has one"
        );
        std::fs::write(built.join("guest").join(crate::MANIFEST), b"{}").unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            built.join("guest"),
            "beside the binary"
        );
        assert_eq!(guest_dir(&vm, None), vm.join("guest"), "no binary known");
        // One others may write to (or the directory it's in): not taken.
        let shared: std::fs::Permissions = std::os::unix::fs::PermissionsExt::from_mode(0o777);
        std::fs::set_permissions(&built, shared.clone()).unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            vm.join("guest"),
            "its directory shared"
        );
        std::fs::set_permissions(&built, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(built.join("guest"), shared).unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            vm.join("guest"),
            "the guest's shared"
        );
        std::fs::set_permissions(
            built.join("guest"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            built.join("guest"),
            "theirs alone again"
        );
        // Its files too: a link among them, or one others may write to,
        // isn't taken (another's kernel in a directory of the user's).
        let link = built.join("guest").join("Image");
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            vm.join("guest"),
            "a link inside"
        );
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"kernel").unwrap();
        std::fs::set_permissions(&link, std::os::unix::fs::PermissionsExt::from_mode(0o666))
            .unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            vm.join("guest"),
            "a file others write"
        );
        std::fs::set_permissions(&link, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            built.join("guest"),
            "its files theirs alone"
        );
        std::fs::write(vm.join("guest").join(crate::MANIFEST), b"{}").unwrap();
        assert_eq!(
            guest_dir(&vm, Some(&exe)),
            vm.join("guest"),
            "the VM's own first"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// F178: a release installed by root (`/usr/local`, `/opt`) keeps its
    /// guest in root's directories: taken, as the user's own are; another
    /// user's, or one a group or others may write to, never.
    #[test]
    fn a_guest_beside_is_the_user_s_or_root_s() {
        let me = 501;
        assert!(trusted_place(me, 0o40755, me), "the user's");
        assert!(trusted_place(0, 0o40755, me), "root's");
        assert!(trusted_place(me, 0o40700, me), "the user's, private");
        assert!(!trusted_place(502, 0o40755, me), "another user's");
        assert!(
            !trusted_place(0, 0o40775, me),
            "root's, its group may write"
        );
        assert!(!trusted_place(me, 0o40757, me), "anyone may write");
        assert!(
            !trusted_place(0, 0o41777, me),
            "a sticky world-writable one"
        );
    }

    /// `doctor`'s look at the directory: none yet is fine; one others may
    /// enter, or a path too long for the socket, isn't; nothing is made.
    #[test]
    fn a_directory_is_checked_without_being_made() {
        use std::os::unix::fs::PermissionsExt as _;
        let uid = nix::unistd::getuid().as_raw();
        let base = std::env::temp_dir().join(format!("sy-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = VmPaths::at(base.join("vm"), base.join("guest")).unwrap();
        paths.check(uid).unwrap();
        assert!(!paths.dir.exists(), "made by a look");
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(paths.check(uid), Err(VmError::Place(_))));
        std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        paths.check(uid).unwrap();
        let long = VmPaths::at(base.join("x".repeat(120)), base.join("guest")).unwrap();
        assert!(matches!(long.check(uid), Err(VmError::Place(_))));
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shipyard-vm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The places are absolute; relative ones are refused.
    #[test]
    fn relative_places_are_refused() {
        assert!(VmPaths::at("/a".into(), "/b".into()).is_ok());
        assert!(matches!(
            VmPaths::at("a".into(), "/b".into()),
            Err(VmError::Place(_))
        ));
        assert!(matches!(
            VmPaths::at("/a".into(), "b".into()),
            Err(VmError::Place(_))
        ));
    }

    /// The directory is made 0700 and accepted; one others can enter, a
    /// link to one, or one of another user's, is refused; a socket path
    /// too long for a socket is refused before anything is made.
    #[test]
    fn the_directory_must_be_the_users_own() {
        let uid = nix::unistd::getuid().as_raw();
        let dir = scratch("prepare");
        let paths = VmPaths::at(dir.join("vm"), dir.join("guest")).unwrap();
        paths.prepare(uid).unwrap();
        assert_eq!(
            std::fs::metadata(&paths.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(matches!(paths.prepare(uid + 1), Err(VmError::Place(_))));
        std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(paths.prepare(uid), Err(VmError::Place(_))));
        std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = VmPaths::at(dir.join("link"), dir.join("guest")).unwrap();
        std::os::unix::fs::symlink(&paths.dir, &link.dir).unwrap();
        assert!(matches!(link.prepare(uid), Err(VmError::Place(_))));
        let long = VmPaths::at(dir.join("x".repeat(120)), dir.join("guest")).unwrap();
        assert!(matches!(long.prepare(uid), Err(VmError::Place(_))));
        assert!(!long.dir.exists(), "nothing made");
        // At the bound: the longest socket's path is what's measured (one
        // byte past it refused, though `vm.sock`'s would fit).
        let at_bound = |extra: usize| {
            let base = dir.join("b").as_os_str().len() + "/docker.sock".len();
            let name = "y".repeat(MAX_SOCKET_PATH + extra - base + 1);
            VmPaths::at(dir.join(name), dir.join("guest")).unwrap()
        };
        let fits = at_bound(0);
        assert_eq!(fits.docker_socket().as_os_str().len(), MAX_SOCKET_PATH);
        assert!(fits.prepare(uid).is_ok());
        let over = at_bound(1);
        assert!(over.socket().as_os_str().len() < MAX_SOCKET_PATH);
        assert!(matches!(over.prepare(uid), Err(VmError::Place(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
