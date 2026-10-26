//! The service's own socket, when systemd didn't pass one (D11.2-1): bound
//! in a directory that's the caller's alone, made the caller's alone (mode
//! 0600) from the start, held by a lock for as long as it's served, and
//! removed when the service ends.
//!
//! A second service is refused, never let to take the first's socket: the
//! lock says one of this crate's own serves it; a socket that answers says
//! something else does (systemd's, listening for its service).

use std::fs::File;
use std::io;
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// The lock beside the socket a service holds while it serves it.
const LOCK: &str = "api.lock";

/// Why the socket couldn't be bound.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BindError {
    /// The socket's path has no directory.
    #[error("{0}: a socket's path needs a directory")]
    NoDirectory(PathBuf),
    /// Its directory isn't the caller's alone.
    #[error("{path}: {reason}: the socket's directory must be yours alone")]
    Directory {
        /// The directory.
        path: PathBuf,
        /// Why it isn't.
        reason: &'static str,
    },
    /// Something already serves it: another service, or systemd's socket.
    #[error(
        "{0} is served already (another `shipyard system service`, or systemd's shipyard.socket)"
    )]
    Served(PathBuf),
    /// Something that isn't a socket is in its place: left as it is.
    #[error("{0} is there, and isn't a socket: it's left as it is")]
    NotASocket(PathBuf),
    /// A system call failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// Why it failed.
        #[source]
        source: io::Error,
    },
}

/// A socket this service bound: removed when it's dropped, while its lock
/// is still held (no other service can have bound it since), and only if
/// it's still the one it bound (its device and inode: systemd may have made
/// its own there since, for its socket unit).
#[derive(Debug)]
pub struct Bound {
    /// The socket and its device and inode, removed at the drop (`None`:
    /// left, as a killed service leaves it).
    path: Option<(PathBuf, (u64, u64))>,
    /// Its lock, held while it serves by its name.
    lock: Option<File>,
}

impl Bound {
    /// Removes the socket's name now (if it's still the one bound), then
    /// lets go of its lock: no client can connect by it any more, and one
    /// that finds it gone starts a service of its own, which binds at once
    /// (F149: the service's idle end; this one only serves what it had
    /// queued).
    pub fn unlink(&mut self) {
        if let Some((path, bound)) = self.path.take()
            && identity(&path).is_ok_and(|now| now == bound)
        {
            let _ = std::fs::remove_file(path);
        }
        self.lock = None;
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        self.unlink();
    }
}

/// A file's device and inode, as it is at `path` (not followed).
fn identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Binds the socket at `path`, listening: its directory made (0700) and
/// checked, its lock taken, a socket left by a service that ended removed.
///
/// Call it while the process is one thread: the socket is made with the
/// process's file mode mask narrowed (`umask` is the process's), so that
/// it's never anyone else's to connect to, even for a moment.
///
/// # Errors
///
/// See [`BindError`].
pub fn bind(path: &Path) -> Result<(UnixListener, Bound), BindError> {
    let io = |what: String| move |source| BindError::Io { what, source };
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| BindError::NoDirectory(path.to_owned()))?;
    let unsafe_because =
        shipyard_sys::fs::private_dir(dir).map_err(io(format!("making {}", dir.display())))?;
    if let Some(reason) = unsafe_because {
        return Err(BindError::Directory {
            path: dir.to_owned(),
            reason,
        });
    }
    let lock = shipyard_sys::fs::open_lock_file(&dir.join(LOCK))
        .map_err(io(format!("opening {}", dir.join(LOCK).display())))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(BindError::Served(path.to_owned())),
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(io(format!("locking {}", dir.join(LOCK).display()))(error));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            // Another program's socket, if it answers (systemd's: a
            // connection starts its service, which idles out).
            if UnixStream::connect(path).is_ok() {
                return Err(BindError::Served(path.to_owned()));
            }
            // A service's that ended without removing it (killed).
            std::fs::remove_file(path).map_err(io(format!("removing {}", path.display())))?;
        }
        Ok(_) => return Err(BindError::NotASocket(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(format!("checking {}", path.display()))(error)),
    }
    let mask = nix::sys::stat::Mode::from_bits_truncate(0o177);
    let before = nix::sys::stat::umask(mask);
    let bound = UnixListener::bind(path);
    nix::sys::stat::umask(before);
    let listener = bound.map_err(io(format!("binding {}", path.display())))?;
    let bound = identity(path).map_err(io(format!("checking {}", path.display())))?;
    Ok((
        listener,
        Bound {
            path: Some((path.to_owned(), bound)),
            lock: Some(lock),
        },
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// One test at a time: `bind` narrows the process's file mode mask for
    /// a moment (the service is one thread then), and a directory another
    /// test makes meanwhile would lose its search bits.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-api-socket-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        dir.join("run")
    }

    /// Bound in a directory of its own (made 0700), the socket the caller's
    /// alone; removed when it's dropped.
    #[test]
    fn the_socket_is_the_caller_s_alone_and_removed_after() {
        let _one = one_at_a_time();
        let dir = scratch();
        let path = dir.join("api.sock");
        let (_listener, bound) = bind(&path).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert!(UnixStream::connect(&path).is_ok());
        drop(bound);
        assert!(!path.exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// A second service is refused while the first serves; a socket left by
    /// one that ended is taken; a socket something answers on is left; a
    /// file that isn't a socket is left.
    #[test]
    fn a_served_socket_is_never_taken() {
        let _one = one_at_a_time();
        let dir = scratch();
        let path = dir.join("api.sock");
        let first = bind(&path).unwrap();
        assert!(matches!(bind(&path), Err(BindError::Served(_))));
        // The first ended without removing it (killed): its lock is gone.
        let (listener, mut bound) = first;
        drop(listener);
        bound.path = None;
        drop(bound);
        assert!(path.exists(), "left behind");
        let (_listener, again) = bind(&path).unwrap();
        drop(again);
        // Replaced while it served (systemd's socket unit made its own
        // there): left at the end.
        let (_listener, bound) = bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replaced = UnixListener::bind(&path).unwrap();
        drop(bound);
        assert!(path.exists(), "another's socket is left");
        drop(replaced);
        std::fs::remove_file(&path).unwrap();
        // Something else answers there: it isn't taken.
        let other = UnixListener::bind(&path).unwrap();
        assert!(matches!(bind(&path), Err(BindError::Served(_))));
        drop(other);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"mine").unwrap();
        assert!(matches!(bind(&path), Err(BindError::NotASocket(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"mine");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// A directory others can write to is refused.
    #[test]
    fn a_directory_others_can_write_to_is_refused() {
        let _one = one_at_a_time();
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            bind(&dir.join("api.sock")),
            Err(BindError::Directory { .. })
        ));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
