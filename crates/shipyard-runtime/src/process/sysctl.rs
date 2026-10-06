//! Setting a container's sysctls (Linux; `sysctl(8)`, `proc(5)`).
//!
//! A sysctl is a file under `/proc/sys`: `kernel.shmmax` is
//! `/proc/sys/kernel/shmmax`, and writing the file sets it. The ones a
//! container may set are its IPC namespace's (`Isolation::sysctls` allows no
//! others): the kernel keeps a copy per namespace and picks the writer's, so
//! the host's are untouched.
//!
//! Written relative to a descriptor for the container's own `/proc`, with
//! the last component never followed through a symlink (`/proc/sys` has
//! none; anything else would be a surprise not to trust).

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::path::Path;

use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;

use super::child::Failure;
use super::failure::ChildStep;

/// Writes each `(key, value)` under the `/proc` open at `proc`, in order.
/// A failure's detail is the sysctl's index.
pub(crate) fn apply(proc: &OwnedFd, sysctls: &[(String, String)]) -> Result<(), Failure> {
    for (index, (key, value)) in sysctls.iter().enumerate() {
        set(proc, key, value)
            .map_err(|errno| Failure::at(ChildStep::Sysctl, errno).detail(index))?;
    }
    Ok(())
}

/// [`apply`], under the `/proc` at `path` (none to set: nothing opened).
pub(crate) fn apply_at(path: &Path, sysctls: &[(String, String)]) -> Result<(), Failure> {
    if sysctls.is_empty() {
        return Ok(());
    }
    let proc = nix::fcntl::open(
        path,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| Failure::at(ChildStep::Sysctl, errno))?;
    apply(&proc, sysctls)
}

fn set(proc: &OwnedFd, key: &str, value: &str) -> nix::Result<()> {
    let path = format!("sys/{}", key.replace('.', "/"));
    let file = openat(
        proc,
        path.as_str(),
        OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    // One write: the kernel parses a sysctl from a single write.
    std::fs::File::from(file)
        .write_all(value.as_bytes())
        .map_err(|e| super::io_errno(&e))
}
