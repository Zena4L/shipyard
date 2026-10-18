//! `--console-socket`: where a container's terminal (its pseudo-terminal's
//! master) is sent, as runc does.

use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// The most a socket's path can be, with its terminating NUL
/// (`sizeof(sun_path)`, TLPI §57.2).
const SUN_PATH_MAX: usize = 108;

/// Connects to the console socket at `path` (`--console-socket`), as the
/// caller: the container's init (`create`) or exec's joiner (`exec -t`)
/// sends a terminal there.
///
/// A path too long for a socket address is reached through its directory:
/// opened, then named as `/proc/self/fd/N/NAME`, which is short.
pub(crate) fn connect_console(path: &Path) -> anyhow::Result<UnixStream> {
    use anyhow::Context as _;
    let named = || format!("--console-socket {}", path.display());
    let metadata = std::fs::metadata(path).with_context(named)?;
    if !metadata.file_type().is_socket() {
        anyhow::bail!("{}: not a socket", named());
    }
    let connected = if path.as_os_str().len() < SUN_PATH_MAX {
        UnixStream::connect(path)
    } else {
        connect_through_directory(path)
    };
    connected.with_context(named)
}

/// Connects to the socket at the long path `path` through its directory.
#[cfg(target_os = "linux")]
fn connect_through_directory(path: &Path) -> std::io::Result<UnixStream> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return UnixStream::connect(path);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(parent)?;
    let mut short = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    short.push(name);
    UnixStream::connect(short)
}

/// Connects to the socket at `path` (no `/proc` to shorten it with here).
#[cfg(not(target_os = "linux"))]
fn connect_through_directory(path: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(path)
}
