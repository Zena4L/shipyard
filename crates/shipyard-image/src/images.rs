//! Turning a pulled image's layer blobs into the directories overlayfs
//! stacks.
//!
//! The store keeps each layer as a compressed tar blob, named by digest
//! ([`Store`]). A container's root filesystem needs each layer as a
//! directory, so each is extracted once, into `layers/<name>/diff` (the
//! name is the digest's hex, marked if the files are the caller's:
//! [`LayerOwners`]), and shared by every image and container that uses it.
//!
//! **Placing a layer is one `rename(2)`.** A layer is extracted into a
//! fresh `layers/.tmp-<name>-<pid>-<n>/diff`; only when it's complete and
//! verified is the temporary directory renamed to `layers/<name>`. A rename
//! within a directory is atomic, so `layers/<name>` existing *is* the mark
//! of a complete layer: there's no marker file (one inside the layer would
//! show in every container's `/`). If another process placed the same layer
//! first, the rename fails (`ENOTEMPTY`) and this copy is discarded.
//!
//! **Removing one is a rename too.** A layer to remove is first renamed to
//! `layers/.trash-…`, then emptied: a removal cut short (Ctrl-C, a file it
//! can't delete) leaves trash for the next garbage collection, never a
//! half-emptied layer under a name that says it's complete. The same goes
//! for a directory in an older layout, found where a layer should be.
//!
//! The data reaches the disk before the renames: one `syncfs(2)` for the
//! whole batch (cheaper than an `fsync` per file), then an `fsync` of
//! `layers/` for the renames themselves. A crash leaves at worst a
//! temporary directory, never a placed layer with missing contents.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::errno::Errno;
use nix::sys::stat::{Mode, SFlag, mkdirat};

use crate::digest::Digest;
use crate::fsat;
use crate::manifest::{ImageConfig, ImageManifest};
use crate::store::{DIFF, LayerOwners, Store, StoreError};
use crate::unpack::{self, LayerReport, LayerSpec, UnpackError, UnpackOptions};

/// An image's extracted layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    /// The layers' content directories (`layers/<name>/diff`), base first.
    pub directories: Vec<PathBuf>,
    /// What extracting the new ones did (layers already in the store add
    /// nothing).
    pub report: LayerReport,
}

/// Why extracting or removing layers failed.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// A layer blob couldn't be read from the store.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A layer couldn't be extracted.
    #[error("extracting layer {index} ({digest}): {source}")]
    Unpack {
        /// Which layer (0-based, base first).
        index: usize,
        /// Its digest.
        digest: Digest,
        /// Why it failed.
        source: UnpackError,
    },
    /// The config lists a different number of layers than the manifest.
    #[error("the image config lists {diff_ids} layer diff_ids for {layers} layers")]
    LayerCount {
        /// Layers in the manifest.
        layers: usize,
        /// `diff_ids` in the config.
        diff_ids: usize,
    },
    /// A name given to [`remove_layers`] isn't a layer's.
    #[error("`{0}` isn't the name of a layer")]
    NotALayer(String),
    /// A layer directory couldn't be made, placed or removed.
    #[error("{action} {name}: {source}")]
    Io {
        /// What was being done.
        action: &'static str,
        /// The layer (or directory) it was done to.
        name: String,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// An [`ExtractError::Io`] for `action` on `name`, from an errno.
fn failed(
    action: &'static str,
    name: impl std::fmt::Display,
) -> impl FnOnce(Errno) -> ExtractError {
    move |errno| ExtractError::Io {
        action,
        name: name.to_string(),
        source: errno.into(),
    }
}

/// The store's layer directory, opened.
fn layers_dir(store: &Store) -> Result<OwnedFd, ExtractError> {
    store.layers_dir().map_err(|source| ExtractError::Io {
        action: "opening",
        name: "the store's layer directory".to_owned(),
        source,
    })
}

/// The layers of `manifest`, as [`LayerSpec`]s: each layer's digest and
/// media type with its `diff_id` from `config`.
///
/// # Errors
///
/// [`ExtractError::LayerCount`] if the config and manifest disagree on how
/// many layers there are.
pub fn layer_specs(
    manifest: &ImageManifest,
    config: &ImageConfig,
) -> Result<Vec<LayerSpec>, ExtractError> {
    if manifest.layers.len() != config.diff_ids.len() {
        return Err(ExtractError::LayerCount {
            layers: manifest.layers.len(),
            diff_ids: config.diff_ids.len(),
        });
    }
    Ok(manifest
        .layers
        .iter()
        .zip(&config.diff_ids)
        .map(|(layer, diff_id)| LayerSpec {
            digest: layer.digest.clone(),
            media_type: layer.media_type.clone(),
            diff_id: diff_id.clone(),
        })
        .collect())
}

/// The content directories `layers` (base first) have, or will have once
/// extracted, with files owned as `owners` says.
#[must_use]
pub fn layer_directories(store: &Store, layers: &[LayerSpec], owners: LayerOwners) -> Vec<PathBuf> {
    layers
        .iter()
        .map(|layer| store.layer_diff(&layer.digest, owners))
        .collect()
}

/// Extracts every layer in `layers` (base first) the store doesn't have
/// extracted yet, and returns all their content directories.
///
/// # Errors
///
/// An [`ExtractError`]. Layers already placed stay; the temporary
/// directories of this call are removed.
pub fn extract_layers(
    store: &Store,
    layers: &[LayerSpec],
    options: &UnpackOptions,
) -> Result<Extracted, ExtractError> {
    let owners = options.ownership.owners();
    let layers_dir = layers_dir(store)?;
    let mut temporaries = Temporaries {
        dir: &layers_dir,
        names: Vec::new(),
    };
    let mut report = LayerReport::default();
    // Each layer's content directory, base first: the lower layers of the
    // next one.
    let mut diffs: Vec<OwnedFd> = Vec::with_capacity(layers.len());
    // Extracted here, to place once all are on disk: (temporary, final).
    let mut to_place: Vec<(OsString, String)> = Vec::new();

    for (index, layer) in layers.iter().enumerate() {
        let name = owners.name(&layer.digest);
        if let Some(diff) = placed(&layers_dir, &name)? {
            diffs.push(diff);
            continue;
        }
        let temporary = temporaries.create(&name)?;
        let diff = make_diff(&layers_dir, &temporary)?;
        let blob = store.open_blob(&layer.digest)?;
        let layer_report =
            unpack::unpack_layer(blob, layer, &diff, &diffs, options).map_err(|source| {
                ExtractError::Unpack {
                    index,
                    digest: layer.digest.clone(),
                    source,
                }
            })?;
        report.add(&layer_report);
        diffs.push(diff);
        to_place.push((temporary, name));
    }

    if !to_place.is_empty() {
        sync_filesystem(&layers_dir)?;
        for (temporary, name) in &to_place {
            match nix::fcntl::renameat(
                &layers_dir,
                temporary.as_os_str(),
                &layers_dir,
                name.as_str(),
            ) {
                Ok(()) => temporaries.placed(temporary),
                // Another process placed it first: keep theirs (it may be
                // mounted already), and this copy goes with the temporaries.
                Err(Errno::ENOTEMPTY | Errno::EEXIST) => {}
                Err(errno) => return Err(failed("placing the layer", name)(errno)),
            }
        }
        nix::unistd::fsync(&layers_dir)
            .map_err(failed("syncing", "the store's layer directory"))?;
    }

    Ok(Extracted {
        directories: layer_directories(store, layers, owners),
        report,
    })
}

/// Moves extracted layers aside to trash, by directory name (as garbage
/// collection lists them: a digest's hex, or a caller-owned layer's), and
/// returns the trash names, for [`remove_layers`] to delete later: renaming
/// is quick, so a collection does it alone, and deletes after, while runs
/// carry on. Leftovers (`.tmp-…`, `.trash-…`) are returned as they are. A
/// name that isn't one of these is refused, so nothing else in the store
/// can be reached through here.
///
/// # Errors
///
/// [`ExtractError::NotALayer`] for a bad name; [`ExtractError::Io`] if one
/// can't be renamed.
pub fn trash_layers(store: &Store, names: &[String]) -> Result<Vec<String>, ExtractError> {
    let layers_dir = layers_dir(store)?;
    let mut trash = Vec::with_capacity(names.len());
    for name in names {
        if fsat::is_leftover(name) {
            trash.push(name.clone());
        } else if LayerOwners::digest_hex(name).is_none() {
            return Err(ExtractError::NotALayer(name.clone()));
        } else if let Some(moved) = fsat::move_to_trash(&layers_dir, name)
            .map_err(failed("moving aside the layer", name))?
        {
            trash.push(moved.to_string_lossy().into_owned());
        }
    }
    Ok(trash)
}

/// Removes extracted layers by directory name: what [`trash_layers`]
/// moved aside (deleted as it is), or a layer (moved aside first, then
/// deleted).
///
/// # Errors
///
/// As [`trash_layers`]; and [`ExtractError::Io`] if one can't be deleted
/// (files owned by other IDs need the user namespace they were made in).
/// What was moved aside stays trash, for later.
pub fn remove_layers(store: &Store, names: &[String]) -> Result<(), ExtractError> {
    let layers_dir = layers_dir(store)?;
    for name in trash_layers(store, names)? {
        fsat::remove_tree_at(&layers_dir, OsStr::new(&name)).map_err(failed("removing", &name))?;
    }
    Ok(())
}

/// The content directory of a placed layer, if `layers/<name>` is one. A
/// directory there without `diff` (an older layout), or anything else, is
/// moved to trash and emptied, to be extracted again.
fn placed(layers_dir: &OwnedFd, name: &str) -> Result<Option<OwnedFd>, ExtractError> {
    let Some(stat) =
        fsat::stat_at(layers_dir, OsStr::new(name)).map_err(failed("reading the layer", name))?
    else {
        return Ok(None);
    };
    if fsat::kind(&stat) == SFlag::S_IFDIR {
        let layer = fsat::open_dir_at(layers_dir, OsStr::new(name))
            .map_err(failed("opening the layer", name))?;
        match fsat::open_dir_at(&layer, OsStr::new(DIFF)) {
            Ok(diff) => return Ok(Some(diff)),
            Err(Errno::ENOENT) => {}
            Err(errno) => return Err(failed("opening the layer", name)(errno)),
        }
    }
    tracing::debug!("re-extracting the layer {name}: not in the current layout");
    if let Some(trash) =
        fsat::move_to_trash(layers_dir, name).map_err(failed("moving aside the layer", name))?
    {
        // Best effort: what can't go now is garbage collection's.
        if let Err(errno) = fsat::remove_tree_at(layers_dir, &trash) {
            tracing::debug!("left {} for later: {errno}", trash.to_string_lossy());
        }
    }
    Ok(None)
}

/// Makes the content directory in a fresh temporary layer directory.
fn make_diff(layers_dir: &OwnedFd, temporary: &OsStr) -> Result<OwnedFd, ExtractError> {
    let shown = temporary.to_string_lossy();
    let layer = fsat::open_dir_at(layers_dir, temporary).map_err(failed("opening", &shown))?;
    mkdirat(&layer, DIFF, Mode::from_bits_truncate(0o755)).map_err(failed("preparing", &shown))?;
    fsat::open_dir_at(&layer, OsStr::new(DIFF)).map_err(failed("opening", &shown))
}

/// Writes what's been extracted to disk before it's placed (Linux).
#[cfg(target_os = "linux")]
fn sync_filesystem(dir: &OwnedFd) -> Result<(), ExtractError> {
    nix::unistd::syncfs(dir).map_err(failed("syncing", "the extracted layers"))
}

/// Elsewhere there's no `syncfs`; the store isn't mounted there anyway.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps, reason = "the Linux version's signature")]
fn sync_filesystem(_: &OwnedFd) -> Result<(), ExtractError> {
    Ok(())
}

/// The temporary layer directories of one call, removed when it ends
/// (unless placed): on success the copies another process beat, on failure
/// everything half-extracted.
struct Temporaries<'a> {
    dir: &'a OwnedFd,
    names: Vec<OsString>,
}

impl Temporaries<'_> {
    /// Makes a new temporary directory for the layer `name`.
    fn create(&mut self, name: &str) -> Result<OsString, ExtractError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        loop {
            let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
            let temporary = OsString::from(format!(".tmp-{name}-{}-{nonce}", std::process::id()));
            match mkdirat(
                self.dir,
                temporary.as_os_str(),
                Mode::from_bits_truncate(0o700),
            ) {
                Ok(()) => {
                    self.names.push(temporary.clone());
                    return Ok(temporary);
                }
                // Left by an earlier process with the same PID: next name.
                Err(Errno::EEXIST) => {}
                Err(errno) => {
                    return Err(failed("creating a temporary directory for", name)(errno));
                }
            }
        }
    }

    /// `name` was placed: no longer this call's to remove.
    fn placed(&mut self, name: &OsStr) {
        self.names.retain(|kept| kept != name);
    }
}

impl Drop for Temporaries<'_> {
    fn drop(&mut self) {
        for name in &self.names {
            if let Err(errno) = fsat::remove_tree_at(self.dir, name) {
                tracing::warn!(
                    "couldn't remove the temporary layer directory {}: {errno}",
                    name.to_string_lossy()
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::*;
    use crate::store::{DIFF, LayerOwners};
    use crate::unpack::tests::{Layer, Scratch, keep};

    /// A store under `scratch` holding `layers`' blobs, and their specs.
    fn store_with(scratch: &Scratch, layers: &[&Layer]) -> (Store, Vec<LayerSpec>) {
        let store = Store::open(scratch.0.join("store")).unwrap();
        let specs = layers
            .iter()
            .map(|layer| {
                let (blob, spec) = layer.blob();
                store.write_blob(&spec.digest, &mut &blob[..]).unwrap();
                spec
            })
            .collect();
        (store, specs)
    }

    fn names(store: &Store) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(store.root().join("layers"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn layers_are_placed_by_digest_with_their_contents_in_diff() {
        let scratch = Scratch::new("place");
        let mut base = Layer::default();
        base.dir("etc", 0o755).file("etc/os-release", b"base");
        let mut top = Layer::default();
        top.file("etc/os-release", b"top");
        let (store, specs) = store_with(&scratch, &[&base, &top]);
        let extracted = extract_layers(&store, &specs, &keep()).unwrap();
        assert_eq!(extracted.directories.len(), 2);
        assert_eq!(
            std::fs::read_to_string(extracted.directories[1].join("etc/os-release")).unwrap(),
            "top"
        );
        assert!(
            !extracted.directories[0]
                .join(".shipyard-extracted")
                .exists(),
            "no marker"
        );
        let mut expected: Vec<String> = specs
            .iter()
            .map(|spec| LayerOwners::Caller.name(&spec.digest))
            .collect();
        expected.sort();
        assert_eq!(names(&store), expected, "no temporaries left");

        // Again: nothing to do, the same directories.
        let again = extract_layers(&store, &specs, &keep()).unwrap();
        assert_eq!(again.directories, extracted.directories);
        assert_eq!(again.report, LayerReport::default());
    }

    #[test]
    fn a_layer_placed_by_another_process_is_kept() {
        let scratch = Scratch::new("race");
        let mut layer = Layer::default();
        layer.file("f", b"ours");
        let (store, specs) = store_with(&scratch, &[&layer]);
        // Another process won the race: its copy is there, and is in use.
        let theirs = store
            .root()
            .join("layers")
            .join(LayerOwners::Caller.name(&specs[0].digest));
        std::fs::create_dir_all(theirs.join(DIFF)).unwrap();
        std::fs::write(theirs.join(DIFF).join("f"), "theirs").unwrap();
        let extracted = extract_layers(&store, &specs, &keep()).unwrap();
        assert_eq!(
            std::fs::read_to_string(extracted.directories[0].join("f")).unwrap(),
            "theirs"
        );
        assert_eq!(names(&store), [LayerOwners::Caller.name(&specs[0].digest)]);
    }

    #[test]
    fn removing_a_layer_moves_it_aside_first() {
        let scratch = Scratch::new("trash");
        let mut layer = Layer::default();
        layer.dir("d", 0o755).file("d/f", b"");
        let (store, specs) = store_with(&scratch, &[&layer]);
        extract_layers(&store, &specs, &keep()).unwrap();
        let name = LayerOwners::Caller.name(&specs[0].digest);
        remove_layers(&store, &[name]).unwrap();
        assert!(names(&store).is_empty(), "{:?}", names(&store));
        // Trash left by a removal cut short is removable by its name.
        std::fs::create_dir_all(store.root().join("layers/.trash-x-1-0/diff")).unwrap();
        remove_layers(&store, &[".trash-x-1-0".to_owned()]).unwrap();
        assert!(names(&store).is_empty());
    }

    /// Makes `path` immutable (or not), so removing it fails; whether that
    /// worked here (`chattr` needs root on Linux).
    fn immutable(path: &Path, on: bool) -> bool {
        let (program, flag) = if cfg!(target_os = "macos") {
            ("chflags", if on { "uchg" } else { "nouchg" })
        } else {
            ("chattr", if on { "+i" } else { "-i" })
        };
        std::process::Command::new(program)
            .arg(flag)
            .arg(path)
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_removal_cut_short_leaves_trash_not_a_half_emptied_layer() {
        let scratch = Scratch::new("cut");
        let mut layer = Layer::default();
        layer.dir("d", 0o755).file("d/a", b"").file("d/stuck", b"");
        let (store, specs) = store_with(&scratch, &[&layer]);
        let extracted = extract_layers(&store, &specs, &keep()).unwrap();
        let stuck = extracted.directories[0].join("d/stuck");
        if !immutable(&stuck, true) {
            return;
        }
        let name = LayerOwners::Caller.name(&specs[0].digest);
        let result = remove_layers(&store, std::slice::from_ref(&name));
        let left = names(&store);
        // Undo it first, so the scratch directory can go whatever happens.
        let trash = store
            .root()
            .join("layers")
            .join(&left[0])
            .join("diff/d/stuck");
        immutable(&trash, false);
        assert!(result.is_err());
        assert_eq!(left.len(), 1);
        assert!(
            left[0].starts_with(".trash-"),
            "{left:?}: the layer's own name is gone"
        );
        // The next collection takes the trash.
        remove_layers(&store, &left).unwrap();
        assert!(names(&store).is_empty());
    }

    #[test]
    fn a_layer_in_an_older_layout_is_extracted_again() {
        let scratch = Scratch::new("old");
        let mut layer = Layer::default();
        layer.file("f", b"new");
        let (store, specs) = store_with(&scratch, &[&layer]);
        let old = store
            .root()
            .join("layers")
            .join(LayerOwners::Caller.name(&specs[0].digest));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("f"), "old").unwrap();
        let extracted = extract_layers(&store, &specs, &keep()).unwrap();
        assert_eq!(
            std::fs::read_to_string(extracted.directories[0].join("f")).unwrap(),
            "new"
        );
        assert_eq!(names(&store), [LayerOwners::Caller.name(&specs[0].digest)]);
    }

    #[test]
    fn a_failed_extraction_leaves_nothing_behind() {
        let scratch = Scratch::new("failed");
        let mut good = Layer::default();
        good.file("f", b"");
        let mut bad = Layer::default();
        bad.file("a", b"").file("a/b", b"");
        let (store, specs) = store_with(&scratch, &[&good, &bad]);
        assert!(extract_layers(&store, &specs, &keep()).is_err());
        assert!(names(&store).is_empty(), "{:?}", names(&store));
    }

    #[test]
    fn the_config_and_manifest_must_agree_on_the_layers() {
        let manifest = crate::manifest::ImageManifest {
            config: crate::manifest::Descriptor {
                media_type: String::new(),
                digest: Digest::sha256(b"c"),
                size: 1,
                platform: None,
            },
            layers: Vec::new(),
        };
        let config = ImageConfig::parse(
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":["sha256:0000000000000000000000000000000000000000000000000000000000000000"]}}"#,
        )
        .unwrap();
        assert!(matches!(
            layer_specs(&manifest, &config),
            Err(ExtractError::LayerCount {
                layers: 0,
                diff_ids: 1
            })
        ));
    }

    #[test]
    fn the_store_is_private() {
        let scratch = Scratch::new("private");
        let root = scratch.0.join("store");
        let store = Store::open(&root).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.root()), 0o700);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(Store::open(&root), Err(StoreError::NotPrivate(_))),
            "an open store is refused, not changed"
        );
        assert_eq!(mode(&root), 0o755);
    }

    #[test]
    fn only_layer_names_can_be_removed() {
        let scratch = Scratch::new("remove");
        let store = Store::open(scratch.0.join("store")).unwrap();
        for name in ["..", "../blobs", "index.json", "abc", "lost+found"] {
            assert!(matches!(
                remove_layers(&store, &[name.to_owned()]),
                Err(ExtractError::NotALayer(_))
            ));
        }
    }
}
