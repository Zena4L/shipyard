//! The read every versioned JSON format of Shipyard's does (H2, F107): its
//! `version` peeked first, a newer one than this Shipyard reads refused
//! (never guessed at), then the whole read as its type. Formats with more
//! to it keep their own: a container's record (several versions, each its
//! own type), the runtime's `state.json` and its exit record (a missing
//! version means stage 8's), and `exec`'s snapshot (read before the rest
//! of its directory).

use serde::de::DeserializeOwned;

/// Why a versioned document can't be read. Exhaustive on purpose: its
/// callers (one per format) match every variant, so a new one is a build
/// error at each until it's handled.
#[derive(Debug)]
pub enum Error {
    /// It's of this version, newer than the newest read.
    Newer(u32),
    /// It isn't JSON of the type, or has no version.
    Unreadable(serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Newer(version) => write!(f, "version {version}, newer than this Shipyard reads"),
            Self::Unreadable(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Newer(_) => None,
            Self::Unreadable(error) => Some(error),
        }
    }
}

/// `bytes` as a `T`, if their `version` is at most `newest`.
///
/// # Errors
///
/// [`Error::Newer`] for a newer version (whatever else the bytes hold),
/// [`Error::Unreadable`] for anything that isn't a `T` with a version.
pub fn read<T: DeserializeOwned>(bytes: &[u8], newest: u32) -> Result<T, Error> {
    #[derive(serde::Deserialize)]
    struct Versioned {
        version: u32,
    }
    let Versioned { version } = serde_json::from_slice(bytes).map_err(Error::Unreadable)?;
    if version > newest {
        return Err(Error::Newer(version));
    }
    serde_json::from_slice(bytes).map_err(Error::Unreadable)
}

/// Why a versioned file can't be read ([`read_file`]).
#[derive(Debug)]
pub enum FileError {
    /// It can't be read: a symlink at its name, not a regular file, larger
    /// than its bound, or the system's error.
    Io(std::io::Error),
    /// It was read, and isn't one this reads ([`read`]'s error).
    Versioned(Error),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Versioned(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Versioned(error) => Some(error),
        }
    }
}

/// The file at `path` as a `T` ([`read`]), read as
/// [`fs::read_bounded`](crate::fs::read_bounded) reads (never through a
/// symlink at its name, never past `cap` bytes); `None` if there's none
/// (F120: the volumes' and the networks' `labels.json`).
///
/// # Errors
///
/// [`FileError::Io`] if it can't be read, [`FileError::Versioned`] if it
/// isn't a `T` of at most `newest`.
pub fn read_file<T: DeserializeOwned>(
    path: &std::path::Path,
    cap: u64,
    newest: u32,
) -> Result<Option<T>, FileError> {
    let bytes = match crate::fs::read_bounded(path, cap) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FileError::Io(error)),
    };
    read(&bytes, newest).map(Some).map_err(FileError::Versioned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        version: u32,
        name: String,
    }

    /// Read at or below the newest; a newer one refused by its version
    /// alone (its shape may be anything); no version, or not the type,
    /// unreadable.
    #[test]
    fn a_newer_version_is_refused_never_guessed_at() {
        let doc: Doc = read(br#"{"version":1,"name":"a"}"#, 2).unwrap();
        assert_eq!(
            doc,
            Doc {
                version: 1,
                name: "a".to_owned()
            }
        );
        assert!(read::<Doc>(br#"{"version":2,"name":"b"}"#, 2).is_ok());
        assert!(matches!(
            read::<Doc>(br#"{"version":3,"other":true}"#, 2),
            Err(Error::Newer(3))
        ));
        assert!(matches!(
            read::<Doc>(br#"{"name":"a"}"#, 2),
            Err(Error::Unreadable(_))
        ));
        assert!(matches!(
            read::<Doc>(br#"{"version":1,"name":"a","x":1}"#, 2),
            Err(Error::Unreadable(_))
        ));
        assert!(matches!(
            read::<Doc>(b"not json", 2),
            Err(Error::Unreadable(_))
        ));
    }

    /// A file: none if missing, read if there, each refusal its own kind
    /// (too big, a symlink: unreadable; a newer version).
    #[test]
    fn a_file_is_read_bounded_and_none_if_missing() {
        let dir = std::env::temp_dir().join(format!("versioned-{}", std::process::id()));
        // (What an aborted run with this PID left, gone first.)
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.json");
        assert!(read_file::<Doc>(&path, 64, 1).unwrap().is_none());
        std::fs::write(&path, br#"{"version":1,"name":"a"}"#).unwrap();
        assert_eq!(read_file::<Doc>(&path, 64, 1).unwrap().unwrap().name, "a");
        assert!(matches!(
            read_file::<Doc>(&path, 8, 1),
            Err(FileError::Io(_))
        ));
        std::fs::write(&path, br#"{"version":2,"name":"a"}"#).unwrap();
        assert!(matches!(
            read_file::<Doc>(&path, 64, 1),
            Err(FileError::Versioned(Error::Newer(2)))
        ));
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(
            read_file::<Doc>(&link, 64, 2),
            Err(FileError::Io(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
