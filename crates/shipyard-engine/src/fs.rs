//! Writing files that must survive a crash whole or not at all.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Context as _, Error, ErrorKind, Result, bail};

/// Writes `bytes` as `dir/name` (mode `mode`), so that a reader sees the old
/// file or the new one, never part of one, and so that the new one is on
/// the disk once this returns: a temporary file of its own (`O_EXCL`), its
/// data synced (`fsync`), renamed over `name` (atomic, TLPI §18.4), then
/// the directory synced, which makes the rename itself durable (TLPI
/// §13.3). The temporary file is removed if anything fails.
///
/// # Errors
///
/// Any step failed (as [`ErrorKind::Io`], with what
/// was being written).
pub(crate) fn write_atomically(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let temporary = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, dir.join(name))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.with_context(|| format!("writing {}", dir.join(name).display()))?;
    sync_directory(dir)
}

/// Syncs directory `dir`: what was made, renamed or removed in it is on the
/// disk once this returns. A file system that can't sync a directory at
/// all (`EINVAL`: 9p and some FUSE ones) has nothing more to give: that's
/// done too.
///
/// # Errors
///
/// It couldn't be opened or synced.
pub(crate) fn sync_directory(dir: &Path) -> Result<()> {
    match File::open(dir).and_then(|dir| dir.sync_all()) {
        Err(err) if err.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        synced => synced.with_context(|| format!("syncing {}", dir.display())),
    }
}

/// Opens `path`, a lock file (made if it's missing, the caller's alone, a
/// symlink there refused), to `flock`.
///
/// # Errors
///
/// It couldn't be opened.
pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// An exclusive `flock` on the lock file `path`, held until dropped,
/// waited for up to `wait` (a holder stopped or stuck mustn't block every
/// other caller for good).
///
/// # Errors
///
/// [`ErrorKind::Busy`]: still held after `wait` (`what` says whose lock).
/// Otherwise it couldn't be opened or locked.
pub(crate) fn lock_waiting(
    path: &Path,
    wait: Duration,
    what: &str,
) -> Result<nix::fcntl::Flock<File>> {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    let mut file = open_lock_file(path)?;
    let deadline = Instant::now() + wait;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => return Ok(locked),
            Err((unlocked, Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                file = unlocked;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err((_, Errno::EWOULDBLOCK)) => bail!(
                Busy,
                "{what} is busy: another command has held its lock for over {} seconds",
                wait.as_secs()
            ),
            Err((_, errno)) => {
                return Err(Error::from(errno).context(format!("locking {}", path.display())));
            }
        }
    }
}

/// Checks that `dir` is a directory of the caller's own that nobody else
/// can change: a real one (not a symlink to one), owned by the caller, not
/// writable by its group or others. `what` names it in the error.
///
/// # Errors
///
/// [`ErrorKind::Storage`], saying what's wrong; or it couldn't be looked at.
pub(crate) fn check_private_dir(dir: &Path, what: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata =
        std::fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    let why = if !metadata.is_dir() {
        "not a directory"
    } else if metadata.uid() != nix::unistd::geteuid().as_raw() {
        "someone else's"
    } else if metadata.mode() & 0o022 != 0 {
        "others can write in it"
    } else {
        return Ok(());
    };
    Err(Error::new(
        ErrorKind::Storage,
        format!("refusing {what} {}: {why}", dir.display()),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_file_is_replaced_whole_with_its_mode() {
        let dir = std::env::temp_dir().join(format!("shipyard-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_atomically(&dir, "f", b"one", 0o600).unwrap();
        write_atomically(&dir, "f", b"two", 0o600).unwrap();
        assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"two");
        let mode = std::fs::metadata(dir.join("f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Nothing left over.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // A failure leaves no temporary file: here, the target is a directory.
        std::fs::create_dir(dir.join("d")).unwrap();
        assert!(write_atomically(&dir, "d", b"x", 0o600).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
