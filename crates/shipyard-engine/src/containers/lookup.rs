//! Finding containers: every record, one by reference, one read again by
//! its ID ([`find`], [`read_one`]).

use std::fs;
#[cfg(test)]
use std::path::PathBuf;

#[cfg(any(test, target_os = "linux"))]
use super::RECORD;
use super::id;
#[cfg(target_os = "linux")]
use super::store::record_dir;
use super::store::{Store, Strict};
use super::{ContainerId, MATCHES_SHOWN, Record, Serial};
use crate::Engine;
use crate::error::{Error, ErrorKind, Result};

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
    // A container whose record can't be read (left out above) still has
    // the prefix: with another, it's more than one, never the other alone.
    let unread: Vec<ContainerId> = ids_in_store(engine)
        .into_iter()
        .filter(|id| id.as_str().starts_with(&prefix))
        .filter(|id| !by_prefix.iter().any(|record| record.id == *id))
        .collect();
    if !unread.is_empty() && by_prefix.len() + unread.len() > 1 {
        let labels = by_prefix
            .iter()
            .map(|record| label(record))
            .chain(
                unread
                    .iter()
                    .map(|id| format!("{} (its record unreadable)", id.short())),
            )
            .collect();
        return Err(many(reference, labels));
    }
    one(reference, &by_prefix)
}

/// The IDs the store has a directory for (a record, readable or not), with
/// no lock: none if it can't be listed (what was read decides then).
fn ids_in_store(engine: &Engine) -> Vec<ContainerId> {
    let Ok(dir) = engine.paths.containers() else {
        return Vec::new();
    };
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter_map(|name| ContainerId::parse(&name).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The one record of `matches`, or the error for none or many.
pub(super) fn one(reference: &str, matches: &[&Record]) -> Result<Record> {
    match matches {
        [] => Err(not_found(reference)),
        [record] => Ok((*record).clone()),
        _ => Err(many(
            reference,
            matches.iter().map(|record| label(record)).collect(),
        )),
    }
}

/// How a container is named among others: its short ID and its name.
fn label(record: &Record) -> String {
    format!("{} ({})", record.id.short(), record.name)
}

/// The error for a `reference` that names more than one container, each
/// in `labels` (the first [`MATCHES_SHOWN`] said, then how many more).
fn many(reference: &str, labels: Vec<String>) -> Error {
    let count = labels.len();
    let mut named: Vec<String> = labels.into_iter().take(MATCHES_SHOWN).collect();
    if count > MATCHES_SHOWN {
        named.push(format!("and {} more", count - MATCHES_SHOWN));
    }
    Error::new(
        ErrorKind::InvalidRequest,
        format!(
            "{reference:?} names more than one container: {}",
            named.join(", ")
        ),
    )
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
