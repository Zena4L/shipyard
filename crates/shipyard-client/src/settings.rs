//! The desktop app's own settings (step 14.2): whether it starts the Mac's
//! VM when it opens (D14.2-2), and the `shipyard` binary and VM directory
//! it uses (a GUI app gets launchd's environment, not the user's shell's:
//! no `PATH` beyond the system's, no `SHIPYARD_*`). `desktop.json`, version 1
//! from its first write (H2), written whole (`shipyard_sys`), the user's
//! alone (0600, in a 0700 directory). A newer one is never overwritten: the
//! app runs on the defaults and says so.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ClientError, Result};

/// The settings file's version.
pub const SETTINGS_VERSION: u32 = 1;

/// The most a settings file is read as.
const MAX_SETTINGS: u64 = 64 * 1024;

/// The app's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Start Shipyard's VM when the app opens (else a Start button).
    pub start_vm_on_open: bool,
    /// The `shipyard` binary the app runs, if one was chosen.
    pub shipyard: Option<PathBuf>,
    /// The VM's directory, if not the default one.
    pub vm_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            start_vm_on_open: true,
            shipyard: None,
            vm_dir: None,
        }
    }
}

/// Version 1 of the file, as it's written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    version: u32,
    start_vm_on_open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shipyard: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vm_dir: Option<PathBuf>,
}

/// The settings read, and whether they may be written back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The settings (the defaults if the file can't be used).
    pub settings: Settings,
    /// False when the file is a newer app's: never written over.
    pub writable: bool,
    /// What was wrong with the file, if anything.
    pub said: Option<String>,
}

/// Where the settings live: the Mac's `~/Library/Application
/// Support/Shipyard/desktop.json`, Linux's `$XDG_CONFIG_HOME/shipyard/`
/// (else `~/.config/shipyard/`).
///
/// # Errors
///
/// [`ClientError::Settings`] without a home directory.
pub fn path() -> Result<PathBuf> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| ClientError::Settings("no home directory ($HOME)".to_owned()))
    };
    if cfg!(target_os = "macos") {
        return Ok(home()?.join("Library/Application Support/Shipyard/desktop.json"));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map_or_else(|| home().map(|home| home.join(".config")), Ok)?;
    Ok(config.join("shipyard").join("desktop.json"))
}

/// The settings at `path`: the defaults where there's no file; the
/// defaults, said, where it can't be read (a corrupt one is written over at
/// the next save) or is a newer app's (never written over).
#[must_use]
pub fn load(path: &Path) -> Loaded {
    let bytes = match shipyard_sys::fs::read_bounded(path, MAX_SETTINGS) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                settings: Settings::default(),
                writable: true,
                said: None,
            };
        }
        Err(error) => {
            return Loaded {
                settings: Settings::default(),
                writable: true,
                said: Some(format!("{} can't be read: {error}", path.display())),
            };
        }
    };
    match shipyard_sys::versioned::read::<V1>(&bytes, SETTINGS_VERSION) {
        Ok(v1) => Loaded {
            settings: Settings {
                start_vm_on_open: v1.start_vm_on_open,
                shipyard: v1.shipyard,
                vm_dir: v1.vm_dir,
            },
            writable: true,
            said: None,
        },
        Err(shipyard_sys::versioned::Error::Newer(found)) => Loaded {
            settings: Settings::default(),
            writable: false,
            said: Some(format!(
                "{} is version {found}, a newer app's: its defaults used, the file left as it is",
                path.display()
            )),
        },
        Err(error) => Loaded {
            settings: Settings::default(),
            writable: true,
            said: Some(format!("{} can't be read: {error}", path.display())),
        },
    }
}

/// Writes `settings` to `path`, whole, the user's alone (its directory made
/// 0700 if it isn't there).
///
/// # Errors
///
/// [`ClientError::Settings`] if it can't be written.
pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let fail = |error: &dyn std::fmt::Display| {
        ClientError::Settings(format!("writing {}: {error}", path.display()))
    };
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|error| fail(&error))?;
    }
    let v1 = V1 {
        version: SETTINGS_VERSION,
        start_vm_on_open: settings.start_vm_on_open,
        shipyard: settings.shipyard.clone(),
        vm_dir: settings.vm_dir.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&v1).map_err(|error| fail(&error))?;
    shipyard_sys::fs::write_atomically(path, &bytes, &shipyard_sys::fs::Write::new(0o600))
        .map_err(|error| fail(&error))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-client-settings-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Saved and read back as they were, the user's alone (the file 0600,
    /// its new directory 0700); no file is the defaults.
    #[test]
    fn settings_are_kept_the_users_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("kept");
        let path = dir.join("Shipyard").join("desktop.json");
        assert_eq!(load(&path).settings, Settings::default());
        let settings = Settings {
            start_vm_on_open: false,
            shipyard: Some(PathBuf::from("/opt/shipyard/bin/shipyard")),
            vm_dir: None,
        };
        save(&path, &settings).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, settings);
        assert!(loaded.writable && loaded.said.is_none());
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A newer app's file is read as the defaults and never written over;
    /// a corrupt one is said, and may be.
    #[test]
    fn a_newer_file_is_never_written_over() {
        let dir = scratch("newer");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desktop.json");
        std::fs::write(
            &path,
            r#"{"version":2,"start_vm_on_open":false,"theme":"x"}"#,
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, Settings::default());
        assert!(!loaded.writable);
        assert!(loaded.said.is_some());
        std::fs::write(&path, b"{not json").unwrap();
        let corrupt = load(&path);
        assert!(corrupt.writable && corrupt.said.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
