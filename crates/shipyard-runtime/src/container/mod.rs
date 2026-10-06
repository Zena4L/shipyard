//! The OCI runtime lifecycle: `create`, `start`, `state`, `kill` and
//! `delete`, as runc implements them, each a separate call. Containers need
//! Linux: elsewhere every call is [`ContainerError::LinuxOnly`]. The
//! implementation is in `lifecycle.rs`.
//!
//! **A container outlives the call that created it.** Shipyard has no daemon:
//! `create` starts a small **monitor** process (this binary again, through
//! [`monitor_entry_point`]) that spawns the container and stays its parent.
//! The monitor records how the container ended, and the container dies with
//! it (the init's parent-death signal).
//!
//! **Created, then started.** The container's init sets everything up, then
//! waits at a **start gate**: it opens the state directory's `exec.fifo` for
//! writing, which blocks until `start` opens it for reading. Until then the
//! user's program hasn't run.
//!
//! **The state directory** is `ROOT/ID` (mode 0700): `state.json` (the
//! bundle, the init's PID and start time), `exec.fifo` (until started), and
//! `exit` (once it ended). Every call takes an exclusive `flock` on the
//! directory while it reads and changes it.
//!
//! **PIDs are checked, not trusted:** a PID is only the init's if
//! `/proc/PID/stat` still shows the start time recorded with it. A reused PID
//! is someone else's process.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::oci::BundleError;

#[cfg(not(target_os = "linux"))]
mod elsewhere;
#[cfg(target_os = "linux")]
mod lifecycle;

#[cfg(not(target_os = "linux"))]
pub use elsewhere::{create, delete, kill, monitor_entry_point, start, state};
#[cfg(target_os = "linux")]
pub use lifecycle::{create, delete, kill, monitor_entry_point, start, state};

/// Where the state directories go unless told otherwise (runc's is
/// `/run/runc`).
pub const DEFAULT_ROOT: &str = "/run/shipyard";

/// The version of the OCI runtime spec the state follows.
pub const OCI_VERSION: &str = "1.2.0";

/// A container `create` made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Created {
    /// Its init's PID (for `--pid-file`).
    pub pid: i32,
    /// What the bundle asked for that is ignored, one message each (as
    /// `shipyard run --bundle` prints them).
    pub warnings: Vec<String>,
}

/// Where a container is in its life (OCI's `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Status {
    /// `create` is still setting it up.
    Creating,
    /// Set up, waiting for `start`.
    Created,
    /// Its program runs.
    Running,
    /// It has ended.
    Stopped,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Creating => "creating",
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopped => "stopped",
        })
    }
}

/// A container's state, as OCI's `state` prints it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct State {
    /// The OCI runtime spec version followed ([`OCI_VERSION`]).
    pub oci_version: String,
    /// Its ID.
    pub id: String,
    /// Where it is in its life.
    pub status: Status,
    /// Its init's PID, while it's created or running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    /// The bundle it was created from (absolute).
    pub bundle: PathBuf,
    /// Its config's annotations.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

/// Why a lifecycle call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ContainerError {
    /// Not an ID a container can have.
    #[error(
        "invalid container ID \"{0}\": a letter or digit, then letters, digits, _ . + or -, \
         at most 255 in all"
    )]
    InvalidId(String),
    /// `create` with an ID that's taken.
    #[error("a container with ID {0} exists")]
    Exists(String),
    /// No container with that ID.
    #[error("no container with ID {0}")]
    NotFound(String),
    /// The container isn't where the call needs it to be.
    #[error("container {id} is {status}; {action} needs it {wanted}")]
    WrongStatus {
        /// Its ID.
        id: String,
        /// Where it is.
        status: Status,
        /// The call.
        action: &'static str,
        /// Where it must be for that.
        wanted: &'static str,
    },
    /// The bundle has no PID namespace. Without one, the container's
    /// processes can't all be found, signalled or cleaned up (that needs
    /// cgroups), and nobody is left to explain a program that can't run.
    #[error(
        "the bundle has no pid namespace: shipyard-runtime needs one (without it, the \
         container's processes can't all be found or stopped until cgroups arrive)"
    )]
    NoPidNamespace,
    /// `kill` with SIGSTOP: it can't be caught, so it would stop the
    /// container's init, which then passes nothing on, instead of its
    /// program.
    #[error(
        "SIGSTOP would stop container {0}'s init, not its program; send TSTP to stop the \
         program (pausing a whole container comes with cgroups)"
    )]
    StopSignal(String),
    /// Another call has held the container's lock too long (stopped, or
    /// stuck on a hung file system).
    #[error("container {0} is busy: another call has held its lock for over 10 seconds")]
    Busy(String),
    /// The state root could be changed by someone else.
    #[error("refusing the state root {}: {reason}", path.display())]
    UnsafeRoot {
        /// The root given.
        path: PathBuf,
        /// What's wrong with it.
        reason: &'static str,
    },
    /// Containers need a Linux kernel.
    #[error(
        "containers need a Linux kernel (current host: {})",
        std::env::consts::OS
    )]
    LinuxOnly,
    /// Creating containers needs root (as running isolated does).
    #[error("creating containers needs root (rootless support comes later)")]
    NeedsRoot,
    /// The bundle can't be run.
    #[error(transparent)]
    Bundle(#[from] BundleError),
    /// Setting the container up failed (the monitor's message).
    #[error("{0}")]
    Setup(String),
    /// A file or system call in the state directory failed.
    #[error("{doing}: {source}")]
    Io {
        /// What was being done.
        doing: String,
        /// Why it failed.
        source: std::io::Error,
    },
}

/// A lifecycle call's result.
pub type Result<T, E = ContainerError> = std::result::Result<T, E>;

/// Accepts IDs as runc does: a letter or digit first, then letters, digits
/// and `_ . + -`, at most 255 (one directory name). No `/`, `.` or `..`.
///
/// # Errors
///
/// [`ContainerError::InvalidId`].
pub fn validate_id(id: &str) -> Result<()> {
    let valid = id.len() <= 255
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ContainerError::InvalidId(id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        for good in ["a", "web-1", "db.v2", "x_y+z", "0abc"] {
            assert!(validate_id(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "-x",
            "_x",
            "a/b",
            "a b",
            "ä",
            &"x".repeat(256),
        ] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_state_is_ocis_json() {
        let state = State {
            oci_version: OCI_VERSION.into(),
            id: "c1".into(),
            status: Status::Created,
            pid: Some(42),
            bundle: "/b".into(),
            annotations: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
        };
        let json: serde_json::Value = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ociVersion": OCI_VERSION, "id": "c1", "status": "created",
                "pid": 42, "bundle": "/b", "annotations": {"k": "v"}
            })
        );
        let stopped = State {
            status: Status::Stopped,
            pid: None,
            annotations: BTreeMap::new(),
            ..state
        };
        let json = serde_json::to_value(&stopped).unwrap();
        assert!(json.get("pid").is_none() && json.get("annotations").is_none());
    }
}
