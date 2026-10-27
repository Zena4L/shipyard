//! What the running VM booted (`booted.json`, v1, step 13.6a, F173b): the
//! host's PID and the build of the guest it booted, written once the guest
//! is up, with the guest directory and the home it serves (13.6b1, F106). A CLI of another build (an upgrade while the VM ran, kept up by
//! its containers or launchd) learns from it that the VM it talks to isn't
//! its own, and says so (`vm status`, `doctor`), before a command is
//! refused for it. Read only while that host runs: its PID the lock's (a
//! file a killed host left says nothing).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Build, VmError};

/// The file's version.
pub const BOOTED_VERSION: u32 = 1;

/// The most a `booted.json` may hold.
const BOOTED_CAP: u64 = 4096;

/// What a host booted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Booted {
    /// The format's version ([`BOOTED_VERSION`]).
    pub version: u32,
    /// The host's PID (as its lock file says it).
    pub host: u32,
    /// The guest's revision, as its manifest said.
    pub revision: String,
    /// Its `shipyard`'s sha256: dev builds' revisions all end `-dirty`.
    pub sha256: String,
    /// The guest directory it booted from (13.6b1; absent in a 13.6a
    /// host's file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<PathBuf>,
    /// The home it shares (13.6b1; absent in a 13.6a host's file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<PathBuf>,
}

impl Booted {
    /// What host `host` booted: the guest `build`.
    #[must_use]
    pub fn of(host: u32, build: &Build) -> Self {
        Self {
            version: BOOTED_VERSION,
            host,
            revision: build.revision.clone(),
            sha256: build.sha256.clone(),
            guest: None,
            home: None,
        }
    }

    /// With the guest directory it booted from and the home it shares,
    /// each as it's found canonical (as given if it can't be).
    #[must_use]
    pub fn serving(mut self, guest: &Path, home: Option<&std::ffi::OsStr>) -> Self {
        self.guest = Some(canonical(guest));
        self.home = home.map(|home| canonical(Path::new(home)));
        self
    }

    /// What it serves that isn't what a CLI with guest directory `guest`
    /// and home `home` would start: said, or `None` (a 13.6a host's file
    /// says neither, so nothing).
    #[must_use]
    pub fn serves_elsewhere(&self, guest: &Path, home: Option<&std::ffi::OsStr>) -> Option<String> {
        let (guest, home) = (
            canonical(guest),
            home.map(|home| canonical(Path::new(home))),
        );
        if let (Some(theirs), Some(ours)) = (&self.home, &home)
            && theirs != ours
        {
            return Some(format!(
                "the running VM shares {}, not this shipyard's home ({}): `shipyard vm stop`, and \
                 it starts again with this one",
                theirs.display(),
                ours.display()
            ));
        }
        match &self.guest {
            Some(theirs) if *theirs != guest => Some(format!(
                "the running VM booted the guest in {}, not this shipyard's ({}): `shipyard vm \
                 stop`, and it starts again with this one",
                theirs.display(),
                guest.display()
            )),
            _ => None,
        }
    }

    /// Written at `path`, whole.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`].
    pub fn write(&self, path: &Path) -> Result<(), VmError> {
        crate::state_file::write(path, self, shipyard_sys::fs::Durability::Atomic)
    }

    /// The one at `path`, if any.
    ///
    /// # Errors
    ///
    /// [`VmError::Io`]: unreadable, not this format, or a newer version.
    pub fn read(path: &Path) -> Result<Option<Self>, VmError> {
        crate::state_file::read(path, BOOTED_CAP, BOOTED_VERSION, "")
    }

    /// Whether it booted another build than `build` (the guest's directory
    /// now): its revision or its binary differ.
    #[must_use]
    pub fn differs(&self, build: &Build) -> bool {
        self.revision != build.revision || self.sha256 != build.sha256
    }
}

/// What a CLI says of a VM that booted another build's guest (`booted`)
/// than its guest directory's: what to do, `launchd` if `vm install`'s
/// agent starts it.
#[must_use]
pub fn another_build(booted: &Booted, launchd: bool) -> String {
    let then = if launchd {
        "`shipyard vm stop`, then `shipyard vm install` from this shipyard (launchd's agent \
         starts the one it was installed from)"
    } else {
        "`shipyard vm stop` (it starts again as this one's)"
    };
    format!(
        "the running VM is another build's (its guest: shipyard {}) than this shipyard's: {then}",
        booted.revision
    )
}

/// What a CLI says of a running VM whose host is from before 0.5 (it binds
/// no `host.sock`): it shares only the home, and is stopped by its PID.
pub const OLDER_HOST: &str = "the running VM is an older build's (from before Shipyard 0.5): \
     `shipyard vm stop`, and it starts again as this one's";

/// `path` canonical, as given if it can't be.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// What a CLI says of the running VM at `paths` if it isn't the one this
/// CLI would start: it shares another home, or booted from another guest
/// directory, or booted another build's guest than its directory holds now;
/// `None` if it's this one's, isn't running, or can't be told.
#[cfg(target_os = "macos")]
#[must_use]
pub fn running_elsewhere(paths: &crate::VmPaths) -> Option<String> {
    let crate::client::Status::Running(Some(host)) = crate::client::status(paths).ok()? else {
        return None;
    };
    let Some(booted) = Booted::read(&paths.booted()).ok()? else {
        // A host with no `host.sock` is from before 13.6b1 (this build's
        // binds it before it writes its PID): one that wrote no
        // `booted.json` either is another build's for sure.
        return (!paths.host_socket().exists()).then(|| OLDER_HOST.to_owned());
    };
    if booted.host != host {
        return None;
    }
    let home = std::env::var_os("HOME");
    if let Some(elsewhere) = booted.serves_elsewhere(&paths.guest, home.as_deref()) {
        return Some(elsewhere);
    }
    let guest = crate::Guest::find(&paths.guest).ok()?;
    booted
        .differs(&guest.manifest.shipyard)
        .then(|| another_build(&booted, crate::launchd::loaded(paths)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn build(revision: &str, sha256: &str) -> Build {
        Build {
            revision: revision.to_owned(),
            sha256: sha256.to_owned(),
        }
    }

    /// v1 round-trips; a newer version is refused; none is `None`.
    #[test]
    fn the_booted_file_is_versioned() {
        let dir = std::env::temp_dir().join(format!("shipyard-booted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("booted.json");
        assert_eq!(Booted::read(&path).unwrap(), None);
        let booted = Booted::of(42, &build("abc-dirty", "209b"));
        booted.write(&path).unwrap();
        assert_eq!(Booted::read(&path).unwrap(), Some(booted));
        std::fs::write(
            &path,
            r#"{"version":2,"host":42,"revision":"abc","sha256":"209b"}"#,
        )
        .unwrap();
        assert!(Booted::read(&path).is_err(), "a newer version");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A 13.6a host's file (no guest, no home) is read, and says nothing
    /// of either; this one's says another home or guest directory, and
    /// nothing for the same ones.
    #[test]
    fn another_home_or_guest_is_said() {
        let old: Booted =
            serde_json::from_str(r#"{"version":1,"host":7,"revision":"abc","sha256":"209b"}"#)
                .unwrap();
        assert_eq!(
            old.serves_elsewhere(Path::new("/g"), Some("/h".as_ref())),
            None
        );
        let here = std::env::temp_dir();
        let booted = Booted::of(7, &build("abc", "209b")).serving(&here, Some(here.as_os_str()));
        assert_eq!(booted.serves_elsewhere(&here, Some(here.as_os_str())), None);
        let other = Path::new("/nonexistent-shipyard-guest");
        let said = booted
            .serves_elsewhere(other, Some(here.as_os_str()))
            .unwrap();
        assert!(said.contains("booted the guest in"), "{said}");
        let said = booted
            .serves_elsewhere(&here, Some("/nonexistent-home".as_ref()))
            .unwrap();
        assert!(said.contains("not this shipyard's home"), "{said}");
    }

    /// Another revision, or the same revision with another binary (two dev
    /// builds), is another build; the same both is not.
    #[test]
    fn another_build_is_told_by_its_revision_or_its_binary() {
        let booted = Booted::of(1, &build("abc-dirty", "209b"));
        assert!(!booted.differs(&build("abc-dirty", "209b")));
        assert!(booted.differs(&build("abc-dirty", "77ff")));
        assert!(booted.differs(&build("def", "209b")));
        assert!(another_build(&booted, true).contains("`shipyard vm install`"));
        assert!(!another_build(&booted, false).contains("vm install"));
    }
}
