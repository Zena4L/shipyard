//! What would come back if the VM started again (`comes-back.json`, v1,
//! step 13.6b2, F106): how many containers the guest last said a restart
//! would start (their restart policies'). The host keeps it current while
//! it runs (each poll's v3 ports answer, and once more at its stop), and it
//! outlives the VM: a host launchd runs at login with `--at-login` boots the
//! VM only if it's above 0.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::VmError;

/// The file's version.
pub const COMES_BACK_VERSION: u32 = 1;

/// The most a `comes-back.json` may hold.
const COMES_BACK_CAP: u64 = 1024;

/// The variable `vm install --at-login` sets in launchd's agent: its host,
/// run at login, starts the VM when containers would come back.
pub const AT_LOGIN_VAR: &str = "SHIPYARD_VM_AT_LOGIN";

/// [`AT_LOGIN_VAR`]'s value that asks for the login start.
pub const AT_LOGIN_ON: &str = "1";

/// "N container(s) would come back", as the host's log and `vm status` say it.
#[must_use]
pub fn would_come_back(containers: u32) -> String {
    match containers {
        1 => "1 container would come back".to_owned(),
        n => format!("{n} containers would come back"),
    }
}

/// How many containers would come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComesBack {
    /// The format's version ([`COMES_BACK_VERSION`]).
    pub version: u32,
    /// How many.
    pub containers: u32,
}

impl ComesBack {
    /// `containers` coming back.
    #[must_use]
    pub fn of(containers: u32) -> Self {
        Self {
            version: COMES_BACK_VERSION,
            containers,
        }
    }

    /// Written at `path`, whole and synced (it outlives the VM, and a Mac's
    /// crash).
    ///
    /// # Errors
    ///
    /// [`VmError::Io`].
    pub fn write(&self, path: &Path) -> Result<(), VmError> {
        crate::state_file::write(path, self, shipyard_sys::fs::Durability::Synced)
    }

    /// The one at `path`, if any.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`]: unreadable, not this format, or a newer version.
    pub fn read(path: &Path) -> Result<Option<Self>, VmError> {
        crate::state_file::read(path, COMES_BACK_CAP, COMES_BACK_VERSION, "")
    }
}

/// Whether the host was installed to start the VM at login (`value`:
/// [`AT_LOGIN_VAR`]'s, as launchd gives it).
#[must_use]
pub fn at_login(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value == AT_LOGIN_ON)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// v1 round-trips; none is `None`; a newer version is refused.
    #[test]
    fn the_count_is_versioned() {
        let dir = std::env::temp_dir().join(format!("shipyard-comes-back-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("comes-back.json");
        assert_eq!(ComesBack::read(&path).unwrap(), None);
        ComesBack::of(2).write(&path).unwrap();
        assert_eq!(ComesBack::read(&path).unwrap(), Some(ComesBack::of(2)));
        std::fs::write(&path, r#"{"version":2,"containers":1}"#).unwrap();
        assert!(ComesBack::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Only `1` asks for the login start.
    #[test]
    fn the_login_start_is_asked_by_one() {
        assert!(at_login(Some("1".as_ref())));
        for not in [None, Some(""), Some("0"), Some("yes")] {
            assert!(!at_login(not.map(AsRef::as_ref)), "{not:?}");
        }
    }
}
