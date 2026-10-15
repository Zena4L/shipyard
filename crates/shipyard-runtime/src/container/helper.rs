//! Helpers: shipyard's own code run inside a container's filesystem, as
//! `cp` needs (stage 8, step 8.5a).
//!
//! A helper is a short-lived process of shipyard's binary that joins a
//! running container's user and mount namespaces and then runs one function
//! the binary registered
//! ([`helper_entry_point`](super::helper_entry_point)). Everything it does
//! to a path happens there, where `/` is the container's root: a symlink the
//! container plants can only lead somewhere in the container, whenever it's
//! swapped (CVE-2018-15664's class). The host side never opens a path the
//! container controls.
//!
//! What the helper is, and isn't (see `process/helper.rs`):
//!
//! - **shipyard's binary**, the sealed copy, never one of the container's
//!   (CVE-2019-14271 had Docker's helper load the container's libraries);
//!   it executes nothing;
//! - **root in the container's user namespace**, with only the capabilities
//!   that read and write files ([`CapabilitySet`](crate::security::CapabilitySet)'s
//!   helper set), `no_new_privs`, non-dumpable;
//! - **in neither the container's PID namespace nor its cgroup**: nothing in
//!   a container with a PID namespace of its own can see or signal it, and a
//!   paused container's files can still be copied;
//! - **holding only its standard input, output and error**, the pipe its
//!   failure goes back on, and its own `/proc/self/fd` (the host's `/proc`,
//!   which shows only itself); it dies with the process that started it.

use std::os::fd::OwnedFd;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A function a helper can run, by name.
#[derive(Debug, Clone, Copy)]
pub struct Helper {
    /// Its name, as [`start_helper`](super::start_helper) gives it.
    pub name: &'static str,
    /// What it does, given what it was started with.
    pub run: fn(&HelperIo) -> Result<(), HelperFailure>,
}

/// What a helper's function is given: inside the container.
#[derive(Debug)]
pub struct HelperIo {
    /// The bytes [`start_helper`](super::start_helper) was given for it.
    pub args: Vec<u8>,
    /// Its standard input, as the caller gave it.
    pub stdin: std::fs::File,
    /// Its standard output, as the caller gave it.
    pub stdout: std::fs::File,
    /// The user IDs files can be owned by in the container, as `(first,
    /// count)` (all of them without a user namespace).
    pub uids: Vec<(u32, u32)>,
    /// The group IDs, likewise.
    pub gids: Vec<(u32, u32)>,
    /// The container's root, open (`O_PATH`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "paths are opened in it on Linux only")
    )]
    pub(crate) root: OwnedFd,
    /// This process's `/proc/self/fd`, opened before it joined the
    /// container (whose `/proc` doesn't show it).
    pub(crate) own_fds: OwnedFd,
}

impl HelperIo {
    /// Opens what's at `path` in the container (`O_PATH`), resolved as the
    /// container sees it: a symlink on the way stays inside its root. Nothing
    /// is made. The last component, if a symlink, is followed if
    /// `follow_last`, or opened itself.
    ///
    /// # Errors
    ///
    /// The errno: `ENOENT` for something missing, `ELOOP` for too many links.
    pub fn open(&self, path: &Path, follow_last: bool) -> nix::Result<OwnedFd> {
        #[cfg(target_os = "linux")]
        {
            crate::process::open_in_root(&self.root, path, follow_last)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, follow_last);
            Err(nix::errno::Errno::ENOSYS)
        }
    }

    /// Reopens `fd` (an `O_PATH` descriptor) with `flags`: the same file,
    /// whatever its path now leads to. (The container's `/proc` doesn't show
    /// this process: this goes through the host's, opened before the join.)
    ///
    /// # Errors
    ///
    /// The errno of the open.
    pub fn reopen(
        &self,
        fd: &impl std::os::fd::AsRawFd,
        flags: nix::fcntl::OFlag,
    ) -> nix::Result<OwnedFd> {
        nix::fcntl::openat(
            &self.own_fds,
            fd.as_raw_fd().to_string().as_str(),
            flags | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
    }
}

/// Why a helper failed, as it says so: a kind its function defines (0 is the
/// helper's own setup, [`HelperFailure::SETUP`]), the errno if one applies
/// (0 if none), and what it was doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{}", self.message())]
pub struct HelperFailure {
    /// What went wrong, in the helper function's terms.
    pub kind: u16,
    /// The errno, or 0.
    pub errno: i32,
    /// What it was doing, and on what.
    pub detail: String,
}

impl HelperFailure {
    /// The kind of a failure setting the helper up (joining, its
    /// credentials, its confinement).
    pub const SETUP: u16 = 0;

    /// The longest detail sent back, in bytes: with its JSON escapes, the
    /// report stays well within one pipe page.
    const MAX_DETAIL: usize = 1024;

    /// A failure of `kind`, with `errno`, while doing `detail`. Control
    /// characters in the detail (a container's file names can hold any)
    /// are escaped, so it can't drive the terminal it's shown on.
    #[must_use]
    pub fn new(kind: u16, errno: nix::errno::Errno, detail: impl Into<String>) -> Self {
        let mut shown = String::new();
        for c in detail.into().chars() {
            if shown.len() >= Self::MAX_DETAIL {
                shown.push('…');
                break;
            }
            if c.is_control() {
                shown.extend(c.escape_default());
            } else {
                shown.push(c);
            }
        }
        Self {
            kind,
            errno: errno as i32,
            detail: shown,
        }
    }

    /// A failure of `kind` with no errno.
    #[must_use]
    pub fn plain(kind: u16, detail: impl Into<String>) -> Self {
        Self::new(kind, nix::errno::Errno::UnknownErrno, detail)
    }

    /// The errno, if one applies.
    #[must_use]
    pub fn errno(&self) -> Option<nix::errno::Errno> {
        (self.errno != 0).then(|| nix::errno::Errno::from_raw(self.errno))
    }

    fn message(&self) -> String {
        match self.errno() {
            Some(errno) => format!("{}: {}", self.detail, errno.desc()),
            None => self.detail.clone(),
        }
    }
}

/// A container's root filesystem as its start mounts it, for a helper that
/// reaches a stopped container's files
/// ([`start_helper_in_view`](super::start_helper_in_view); stage 9, step
/// 9.4c2).
#[derive(Debug, Clone)]
pub struct RootView {
    /// Its root: the directory, the image's overlay, and its mounts
    /// ([`Root::mounts`](crate::isolation::Root::mounts)): binds (volumes,
    /// host directories) are mounted; a tmpfs, a running container's only,
    /// is skipped.
    pub root: crate::isolation::Root,
    /// Its user namespace's maps, if it has one: the helper makes a user
    /// namespace with them, so files have the owners they have in the
    /// container.
    pub id_maps: Option<crate::isolation::IdMaps>,
}

/// A helper, running (see [`start_helper`](super::start_helper)). Dropped
/// before [`wait`](Self::wait), it's killed and reaped.
#[derive(Debug)]
pub struct RunningHelper {
    pub(crate) child: Option<std::process::Child>,
    pub(crate) report: Option<OwnedFd>,
}

impl RunningHelper {
    /// Waits for it to end. Close your ends of its standard input and
    /// output first, or it may wait for you.
    ///
    /// # Errors
    ///
    /// What it said went wrong
    /// ([`ContainerError::HelperFailed`](super::ContainerError::HelperFailed)),
    /// or how it ended without saying
    /// ([`ContainerError::HelperEnded`](super::ContainerError::HelperEnded)).
    pub fn wait(mut self) -> super::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // Read first: it ends at the helper's end (it holds the only write
        // end), and a helper never waits on a report nobody reads.
        let mut said = Vec::new();
        if let Some(report) = self.report.take() {
            let _ = std::io::Read::read_to_end(&mut std::fs::File::from(report), &mut said);
        }
        let status = child.wait().map_err(|error| super::ContainerError::Io {
            doing: "waiting for the helper".to_owned(),
            error,
        })?;
        if !said.is_empty() {
            return Err(match serde_json::from_slice::<HelperFailure>(&said) {
                Ok(failure) => super::ContainerError::HelperFailed(failure),
                Err(_) => {
                    super::ContainerError::HelperEnded("its report couldn't be read".to_owned())
                }
            });
        }
        if status.success() {
            return Ok(());
        }
        Err(super::ContainerError::HelperEnded(
            match std::os::unix::process::ExitStatusExt::signal(&status) {
                Some(signal) => format!("killed by signal {signal}"),
                None => format!("exit status {}", status.code().unwrap_or(-1)),
            },
        ))
    }
}

impl Drop for RunningHelper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_reads_with_its_errno() {
        let failure = HelperFailure::new(3, nix::errno::Errno::ENOENT, "opening /data");
        assert_eq!(failure.errno(), Some(nix::errno::Errno::ENOENT));
        assert!(failure.to_string().starts_with("opening /data: "));
        let none = HelperFailure::new(3, nix::errno::Errno::UnknownErrno, "no errno");
        assert_eq!(none.errno(), None);
        assert_eq!(none.to_string(), "no errno");
    }

    #[test]
    fn a_failures_detail_is_bounded_and_escaped() {
        for long in [
            "é".repeat(4096),
            "\u{1}".repeat(4096),
            "\u{1b}[2J".repeat(1024),
        ] {
            let failure = HelperFailure::new(1, nix::errno::Errno::EIO, long);
            assert!(!failure.detail.chars().any(char::is_control), "{failure:?}");
            let json = serde_json::to_vec(&failure).unwrap();
            assert!(
                json.len() < 4096,
                "fits a pipe's atomic write: {}",
                json.len()
            );
        }
        let escaped = HelperFailure::plain(1, "a\u{1b}[31mb\nc");
        assert_eq!(escaped.detail, "a\\u{1b}[31mb\\nc");
        assert_eq!(escaped.errno(), None);
    }
}
