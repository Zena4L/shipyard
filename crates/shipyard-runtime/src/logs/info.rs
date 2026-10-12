//! `log.json`: what Shipyard needs to know of a log directory besides its
//! records (D15), written before the first record.
//!
//! The first on-disk format of Shipyard's with a version (H2): `version`
//! says which layout the directory has, and a reader refuses one it doesn't
//! know rather than misreading it. A later version adds fields (older readers
//! refuse it) or changes the layout; this one is 1.

use std::os::fd::AsFd;

use serde::{Deserialize, Serialize};

/// The file's name, in the log directory.
pub const INFO_FILE: &str = "log.json";

/// The format's name in `log.json`.
pub const FORMAT: &str = "cri";

/// The layout this code writes and reads.
pub const VERSION: u32 = 1;

/// How the logs are kept: rotated by size and count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogSettings {
    /// The most bytes `container.log` holds before it's rotated (one record
    /// past it at most).
    pub max_size: u64,
    /// How many files are kept, `container.log` among them.
    pub max_files: u32,
}

/// Settings outside what Shipyard keeps.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("log rotation: {0}")]
pub struct SettingsError(pub String);

impl LogSettings {
    /// The fewest files: a reader needs one to have moved on to when it
    /// finishes another (kubelet's rule too).
    pub const MIN_FILES: u32 = 2;
    /// The most files.
    pub const MAX_FILES: u32 = 100;
    /// The smallest file: several records of the most a record holds.
    pub const MIN_SIZE: u64 = 64 * 1024;
    /// The largest file.
    pub const MAX_SIZE: u64 = 1024 * 1024 * 1024;

    /// These settings, if Shipyard keeps logs that way.
    ///
    /// # Errors
    ///
    /// [`SettingsError`]: too few or many files, or files too small or large.
    pub fn validate(self) -> Result<Self, SettingsError> {
        if !(Self::MIN_FILES..=Self::MAX_FILES).contains(&self.max_files) {
            return Err(SettingsError(format!(
                "between {} and {} files are kept",
                Self::MIN_FILES,
                Self::MAX_FILES
            )));
        }
        if !(Self::MIN_SIZE..=Self::MAX_SIZE).contains(&self.max_size) {
            return Err(SettingsError(format!(
                "a file holds between {} KiB and {} GiB",
                Self::MIN_SIZE / 1024,
                Self::MAX_SIZE / (1024 * 1024 * 1024)
            )));
        }
        Ok(self)
    }
}

impl Default for LogSettings {
    /// 10 MiB × 5: containerd's and kubelet's defaults.
    fn default() -> Self {
        Self {
            max_size: 10 * 1024 * 1024,
            max_files: 5,
        }
    }
}

/// `log.json`'s content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogInfo {
    /// Always [`FORMAT`].
    pub format: String,
    /// The layout's version ([`VERSION`]).
    pub version: u32,
    /// How the files are rotated.
    #[serde(flatten)]
    pub settings: LogSettings,
}

/// A log directory that can't be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InfoError {
    /// No `log.json`, or it can't be read.
    #[error("reading {INFO_FILE}: {0}")]
    Read(#[source] std::io::Error),
    /// It isn't `log.json` as any version writes it.
    #[error("{INFO_FILE} is malformed: {0}")]
    Malformed(String),
    /// Another format than CRI.
    #[error("{INFO_FILE} names a log format this Shipyard doesn't know: {0:?}")]
    UnknownFormat(String),
    /// A version this Shipyard doesn't know (a newer one's).
    #[error(
        "{INFO_FILE} has version {found}, which this Shipyard doesn't know (it reads version \
         {VERSION}): a newer Shipyard wrote these logs"
    )]
    UnknownVersion {
        /// The version found.
        found: u32,
    },
}

impl LogInfo {
    /// The current version's, with `settings`.
    #[must_use]
    pub fn new(settings: LogSettings) -> Self {
        Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            settings,
        }
    }

    /// Reads `log.json` in the directory open at `dir`.
    ///
    /// # Errors
    ///
    /// [`InfoError`]: unreadable, malformed, or another format or version.
    pub fn read_at(dir: impl AsFd) -> Result<Self, InfoError> {
        use std::io::Read as _;
        let fd = nix::fcntl::openat(
            dir,
            INFO_FILE,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|errno| InfoError::Read(errno.into()))?;
        let mut text = Vec::new();
        // A few dozen bytes; more is not `log.json`.
        std::fs::File::from(fd)
            .take(64 * 1024)
            .read_to_end(&mut text)
            .map_err(InfoError::Read)?;
        Self::from_json(&text)
    }

    /// Reads `log.json`'s content: the format and version first, so that a
    /// newer layout is refused as such, not as malformed.
    ///
    /// # Errors
    ///
    /// As [`read_at`](Self::read_at).
    pub fn from_json(text: &[u8]) -> Result<Self, InfoError> {
        #[derive(Deserialize)]
        struct Header {
            format: String,
            version: u32,
        }
        let header: Header =
            serde_json::from_slice(text).map_err(|err| InfoError::Malformed(err.to_string()))?;
        if header.format != FORMAT {
            return Err(InfoError::UnknownFormat(header.format));
        }
        if header.version != VERSION {
            return Err(InfoError::UnknownVersion {
                found: header.version,
            });
        }
        serde_json::from_slice(text).map_err(|err| InfoError::Malformed(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_1_is_read() {
        let info = LogInfo::from_json(
            br#"{"format": "cri", "version": 1, "maxSize": 4096, "maxFiles": 2}"#,
        )
        .unwrap();
        assert_eq!(
            info.settings,
            LogSettings {
                max_size: 4096,
                max_files: 2
            }
        );
        let written = serde_json::to_vec(&LogInfo::new(LogSettings::default())).unwrap();
        assert_eq!(
            LogInfo::from_json(&written).unwrap(),
            LogInfo::new(LogSettings::default())
        );
    }

    #[test]
    fn settings_are_bounded() {
        assert!(LogSettings::default().validate().is_ok());
        for (max_size, max_files) in [
            (64 * 1024, 1),
            (64 * 1024, 101),
            (64 * 1024 - 1, 2),
            (1024 * 1024 * 1024 + 1, 2),
        ] {
            let settings = LogSettings {
                max_size,
                max_files,
            };
            assert!(settings.validate().is_err(), "{settings:?}");
        }
    }

    #[test]
    fn another_version_or_format_is_refused_as_such() {
        // A newer layout may have fields of its own: refused for its version,
        // before they're read.
        assert!(matches!(
            LogInfo::from_json(br#"{"format": "cri", "version": 2, "something": []}"#),
            Err(InfoError::UnknownVersion { found: 2 })
        ));
        assert!(matches!(
            LogInfo::from_json(br#"{"format": "json-file", "version": 1}"#),
            Err(InfoError::UnknownFormat(_))
        ));
        assert!(matches!(
            LogInfo::from_json(br#"{"format": "cri", "version": 1}"#),
            Err(InfoError::Malformed(_))
        ));
        assert!(matches!(
            LogInfo::from_json(b"not json"),
            Err(InfoError::Malformed(_))
        ));
    }
}
