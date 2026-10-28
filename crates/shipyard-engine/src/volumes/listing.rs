//! Each volume's contents for the desktop UI's Volumes section (step
//! 14.1b): its size (its drum's fill) and what's at its top. A volume is a
//! container's to fill, so both are bounded: its size walk stops past
//! [`WALK`]'s entries, depth or waiting directories, at a mount inside it,
//! or once the read's [`TIME`] is up, by descriptor and never through a
//! link (`fsat`), and says when it stopped (`complete: false`: "at least");
//! its listing reads at most [`SCANNED`] names of its top level and says the
//! first [`MAX_LISTED`]. What's measured is `_data`, what its containers
//! see (`system df` counts the volume's whole directory, its labels and
//! markers too: a few bytes more).

use serde::Serialize;
use shipyard_image::fsat::{self, Budget, EntryKind};

use crate::Engine;
use crate::error::{Context as _, Result};

/// The answer's version (`GET /shipyard/volumes`).
pub const VOLUMES_VERSION: u32 = 1;

/// The most entries of a volume's top level said.
pub(crate) const MAX_LISTED: usize = 200;

/// The most names of a volume's top level read.
pub(crate) const SCANNED: usize = 100_000;

/// The longest name said (in characters): longer ones are shortened, so a
/// volume's listing stays a few kilobytes whatever its names.
const LONGEST_NAME: usize = 120;

/// How far one volume's size walk goes.
pub(crate) const WALK: Budget = Budget {
    entries: 200_000,
    depth: 64,
    waiting: 20_000,
    one_filesystem: true,
    until: None,
};

/// How long one read of every volume may take: a volume not reached by
/// then is said not walked.
pub(crate) const TIME: std::time::Duration = std::time::Duration::from_secs(3);

/// Every volume's contents: see the module's doc.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct VolumeContents {
    /// [`VOLUMES_VERSION`].
    pub version: u32,
    /// Each volume, by name.
    pub volumes: Vec<VolumeContent>,
    /// What couldn't be read (a volume's labels, time or directory).
    pub warnings: Vec<String>,
}

/// One volume.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct VolumeContent {
    /// Its name.
    pub name: String,
    /// When it was made (RFC 3339), as Docker's list says it.
    pub created: String,
    /// The bytes of its files (a file linked twice counted once).
    pub size: u64,
    /// Whether its whole tree was walked: `false`, there's more (past the
    /// walk's bounds or the read's time, an unreadable directory, a mount
    /// inside).
    pub complete: bool,
    /// Its top level's entries, by name.
    pub files: Vec<VolumeFile>,
    /// How many more entries its top level has.
    pub more: usize,
    /// Whether its whole top level was read: `false`, `files` and `more`
    /// are of the first 100,000 names.
    pub listed_whole: bool,
}

/// An entry at a volume's top.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct VolumeFile {
    /// Its name (bytes that aren't UTF-8 replaced; past 120 characters,
    /// shortened with `…`).
    pub name: String,
    /// What it is.
    pub kind: EntryKind,
    /// Its own size.
    pub size: u64,
}

/// Every volume's contents, within 3 seconds.
///
/// # Errors
///
/// The volume store can't be read.
pub fn contents(engine: &Engine) -> Result<VolumeContents> {
    let listed = crate::docker::list_volumes(engine)?;
    let store = super::open(engine)?;
    let root = fsat::open_dir(store.root()).context("opening the volume store")?;
    let budget = Budget {
        until: Some(std::time::Instant::now() + TIME),
        ..WALK
    };
    let mut warnings = listed.warnings.unwrap_or_default();
    let volumes = listed
        .volumes
        .into_iter()
        .map(|volume| {
            let mut content = VolumeContent {
                name: volume.name,
                created: volume.created_at,
                size: 0,
                complete: false,
                files: Vec::new(),
                more: 0,
                listed_whole: false,
            };
            // Its directory, then its `_data`, by descriptor.
            let Ok(dir) = fsat::open_dir_at(&root, std::ffi::OsStr::new(&content.name)) else {
                warnings.push(format!(
                    "the volume {}'s directory can't be read",
                    content.name
                ));
                return content;
            };
            let data = std::ffi::OsStr::new("_data");
            let taken = fsat::tree_usage_within(&dir, data, &budget);
            content.size = taken.bytes;
            content.complete = taken.complete;
            match fsat::listing(&dir, data, MAX_LISTED, SCANNED) {
                Ok(listing) => {
                    content.files = listing.entries.iter().map(file).collect();
                    content.more = listing.more;
                    content.listed_whole = listing.complete;
                }
                Err(error) => {
                    warnings.push(format!(
                        "the volume {}'s files can't be listed: {error}",
                        content.name
                    ));
                }
            }
            content
        })
        .collect();
    Ok(VolumeContents {
        version: VOLUMES_VERSION,
        volumes,
        warnings,
    })
}

/// A listed entry as the answer says it.
fn file(entry: &fsat::Listed) -> VolumeFile {
    let name = entry.name.to_string_lossy();
    let name = if name.chars().count() > LONGEST_NAME {
        format!("{}…", name.chars().take(LONGEST_NAME).collect::<String>())
    } else {
        name.into_owned()
    };
    VolumeFile {
        name,
        kind: entry.kind,
        size: entry.size,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A name that isn't UTF-8, or that looks like markup, is said as text,
    /// a long one shortened; each kind said in its word; the answer says
    /// its version.
    #[test]
    fn a_volume_s_files_are_said_as_text() {
        use std::os::unix::ffi::OsStrExt as _;
        let listed = |name: &[u8], kind| {
            file(&fsat::Listed {
                name: std::ffi::OsStr::from_bytes(name).to_owned(),
                kind,
                size: 4096,
            })
        };
        assert_eq!(listed(b"caf\xe9", EntryKind::File).name, "caf\u{fffd}");
        assert_eq!(listed(b"<b>x</b>", EntryKind::File).name, "<b>x</b>");
        let long = listed(&[b'a'; 250], EntryKind::File).name;
        assert_eq!(long.chars().count(), LONGEST_NAME + 1);
        assert!(long.ends_with('…'));
        let kinds: Vec<serde_json::Value> = [
            EntryKind::File,
            EntryKind::Directory,
            EntryKind::Link,
            EntryKind::Other,
        ]
        .into_iter()
        .map(|kind| serde_json::to_value(listed(b"x", kind)).unwrap()["kind"].clone())
        .collect();
        assert_eq!(kinds, ["file", "directory", "link", "other"]);
        let answer = serde_json::to_value(VolumeContents {
            version: VOLUMES_VERSION,
            volumes: Vec::new(),
            warnings: Vec::new(),
        })
        .unwrap();
        assert_eq!(answer["version"], 1);
    }
}
