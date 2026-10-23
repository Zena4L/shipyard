//! The VM directory's versioned JSON files (`ports.json`, `network.json`):
//! written whole, the user's alone, read with a bound, a newer version
//! refused (H2).

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use shipyard_sys::fs::Durability;

use crate::VmError;

/// Writes `value` at `path`, whole: `durability` says what a crash may
/// leave (see `shipyard_sys::fs::Durability`).
pub(crate) fn write<T: Serialize>(
    path: &Path,
    value: &T,
    durability: Durability,
) -> Result<(), VmError> {
    let bytes = serde_json::to_vec(value).map_err(|e| VmError::io(path, e.into()))?;
    let how = shipyard_sys::fs::Write::new(0o600).durability(durability);
    shipyard_sys::fs::write_atomically(path, &bytes, &how)
        .map_err(|e| VmError::io(path, std::io::Error::from(e)))
}

/// The file at `path` as a `T` of at most version `newest`, read up to
/// `cap` bytes: `None` if there's none. `hint` follows what's said of one
/// that can't be read (what the user may do about it).
pub(crate) fn read<T: DeserializeOwned>(
    path: &Path,
    cap: u64,
    newest: u32,
    hint: &str,
) -> Result<Option<T>, VmError> {
    let bytes = match shipyard_sys::fs::read_bounded(path, cap) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(VmError::io(path, error)),
    };
    shipyard_sys::versioned::read(&bytes, newest)
        .map(Some)
        .map_err(|error| {
            let why = match error {
                shipyard_sys::versioned::Error::Newer(version) => {
                    format!("version {version}, newer than this shipyard's {newest}")
                }
                shipyard_sys::versioned::Error::Unreadable(e) => e.to_string(),
            };
            VmError::io(path, std::io::Error::other(format!("{why}{hint}")))
        })
}
