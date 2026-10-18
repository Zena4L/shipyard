//! Finding containers: every record, one by reference, one read again by
//! its ID ([`find`], [`read_one`]).

use std::fs;
#[cfg(test)]
use std::path::PathBuf;

use super::id;
#[cfg(target_os = "linux")]
use super::store::record_dir;
use super::store::{Store, Strict};
#[cfg(any(test, target_os = "linux"))]
use super::{ContainerId, RECORD};
use super::{MATCHES_SHOWN, Record, Serial};
use crate::Engine;
use crate::error::{Error, ErrorKind, Result, bail};

/// Every container's record, oldest first. One that can't be read is
/// reported ([`WarningKind::RecordUnreadable`]) and left out. No store yet
/// is none (it isn't made).
///
/// [`WarningKind::RecordUnreadable`]: crate::report::WarningKind::RecordUnreadable
///
/// # Errors
///
/// [`ErrorKind::Storage`]: the store can't be used.
/// [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn list(engine: &Engine) -> Result<Vec<Record>> {
    match Store::open(engine, false)? {
        Some(store) => store.read_all(engine, Strict::No),
        None => Ok(Vec::new()),
    }
}

/// The container `reference` names: in order, its full ID, its name, its
/// serial written with its owner code (`SHPU 304812 2`, the check digit
/// optional), or a prefix of its ID that no other container's has.
///
/// # Errors
///
/// - [`ErrorKind::NotFound`]: there's no such container.
/// - [`ErrorKind::InvalidRequest`]: more than one (a prefix of several IDs,
///   or a name or serial two records have: some are named), or a serial
///   with the wrong check digit.
/// - [`ErrorKind::Storage`]: the store can't be used.
/// - [`ErrorKind::Environment`]: there's nowhere to keep records.
pub fn find(engine: &Engine, reference: &str) -> Result<Record> {
    let records = list(engine)?;
    let by_id: Vec<&Record> = records
        .iter()
        .filter(|record| record.id.as_str() == reference)
        .collect();
    if !by_id.is_empty() {
        return one(reference, &by_id);
    }
    let by_name: Vec<&Record> = records
        .iter()
        .filter(|record| record.name.as_str() == reference)
        .collect();
    if !by_name.is_empty() {
        return one(reference, &by_name);
    }
    if let Some(serial) = Serial::parse(reference)? {
        let by_serial: Vec<&Record> = records
            .iter()
            .filter(|record| record.serial == serial)
            .collect();
        return one(reference, &by_serial);
    }
    if reference.is_empty() || !reference.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(not_found(reference));
    }
    let prefix = reference.to_ascii_lowercase();
    let by_prefix: Vec<&Record> = records
        .iter()
        .filter(|record| record.id.as_str().starts_with(&prefix))
        .collect();
    one(reference, &by_prefix)
}

/// The one record of `matches`, or the error for none or many.
pub(super) fn one(reference: &str, matches: &[&Record]) -> Result<Record> {
    match matches {
        [] => Err(not_found(reference)),
        [record] => Ok((*record).clone()),
        _ => {
            let mut named: Vec<String> = matches
                .iter()
                .take(MATCHES_SHOWN)
                .map(|record| format!("{} ({})", record.id.short(), record.name))
                .collect();
            if matches.len() > MATCHES_SHOWN {
                named.push(format!("and {} more", matches.len() - MATCHES_SHOWN));
            }
            bail!(
                InvalidRequest,
                "{reference:?} names more than one container: {}",
                named.join(", ")
            )
        }
    }
}

pub(super) fn not_found(reference: &str) -> Error {
    Error::new(ErrorKind::NotFound, format!("no container {reference:?}"))
}

/// Container `id`'s record, read again by its path (no other record
/// read): for a caller that holds its locks, or one that only looks (a
/// record is replaced whole, by a rename: never read half written).
///
/// # Errors
///
/// [`ErrorKind::NotFound`] if it's gone (removed: its directory renamed
/// away); else [`Record::read`]'s.
#[cfg(target_os = "linux")]
pub(crate) fn read_one(engine: &Engine, id: &ContainerId) -> Result<Record> {
    match Record::read(&record_dir(engine, id)?.join(RECORD)) {
        Err(error)
            if error
                .find::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Err(not_found(id.as_str()))
        }
        read => read,
    }
}

/// Whether container `id` (as text: an image store's area names it so) has
/// a directory in this store, a record or one being written. Read with no
/// lock: a container appears and goes whole.
pub(crate) fn exists(engine: &Engine, id: &str) -> bool {
    id::is_id(id)
        && engine
            .paths
            .containers()
            .is_ok_and(|store| fs::symlink_metadata(store.join(id)).is_ok())
}

/// Where container `id`'s record is in `store` (for tests).
#[cfg(test)]
pub(super) fn record_path(store: &std::path::Path, id: &ContainerId) -> PathBuf {
    store.join(id.as_str()).join(RECORD)
}
