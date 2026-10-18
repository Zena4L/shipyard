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
            Path::new(&home).join("Library/Application Support/Shipyard/vm")
        };
        let guest = var("SHIPYARD_GUEST_DIR").map_or_else(|| dir.join("guest"), PathBuf::from);
        Self::at(dir, guest)
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
        Ok(Self { dir, guest })
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

    /// What the host forwards ([`crate::ports::PortsFile`]).
    #[must_use]
    pub fn ports(&self) -> PathBuf {
        self.dir.join("ports.json")
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

    /// The socket's path fits a socket's address.
    fn check_socket(&self) -> Result<(), VmError> {
        let socket = self.socket();
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
