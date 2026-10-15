//! A run's volumes and `/etc` files, bound into its root filesystem.

use std::path::{Path, PathBuf};

use shipyard_runtime::isolation::{Mount, Root};
use shipyard_storage::{VolumeHold, VolumeMount};

use crate::Engine;
use crate::error::{Context as _, Error, ErrorKind, Result};
use crate::network::{ETC_FILES, RunDir};

/// Binds the run's `/etc` files (`etc`) into `root`, before the volumes, so a
/// volume a user puts on one of them wins. An image's root takes them all
/// (one that's missing is made in the overlay's writable layer, which goes
/// with the run); a root filesystem of the user's own (`--rootfs`) only
/// those it has as regular files: Shipyard never writes into it. (A link,
/// like Debian's `resolv.conf` to systemd's, is followed inside the root to
/// bind on its target, which would be made if it's missing; and `etc`
/// itself is looked at without following a link.)
pub(super) fn bind_etc_files(root: &mut Root, etc: &RunDir) {
    let is_file = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
    };
    let etc_dir = root.path.join("etc");
    let has_etc = std::fs::symlink_metadata(&etc_dir).is_ok_and(|metadata| metadata.is_dir());
    for name in ETC_FILES {
        if root.overlay.is_none() && !(has_etc && is_file(&etc_dir.join(name))) {
            continue;
        }
        root.mounts
            .push(Mount::bind(etc.file(name), Path::new("/etc").join(name)));
    }
}

/// `source` made absolute, symlinks resolved, as UTF-8.
pub(super) fn canonical_source(source: &str) -> Result<String> {
    std::fs::canonicalize(source)
        .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
        .with_context(|| source.to_owned())?
        .into_os_string()
        .into_string()
        .map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                format!("{source}: a path that isn't UTF-8"),
            )
        })
}

/// Binds `volumes` into `root`: a host path as it is (resolved again, as
/// close to the run as can be), a named volume's directory (made or not:
/// [`hold_volumes`] makes it). Returns the named volumes' names.
pub(super) fn bind_volumes(
    engine: &Engine,
    root: &mut Root,
    volumes: Vec<VolumeMount>,
) -> Result<Vec<String>> {
    let mut named = Vec::new();
    let mut store = None;
    for mount in volumes {
        let source = if shipyard_storage::volume::is_named_volume(&mount.source) {
            let store = match &store {
                Some(store) => store,
                None => store.insert(crate::volumes::open(engine)?),
            };
            let data = store.data_path(&mount.source)?;
            named.push(mount.source);
            data
        } else {
            PathBuf::from(canonical_source(&mount.source)?)
        };
        let mut bind = Mount::bind(source, mount.destination);
        if mount.read_only {
            bind.flags
                .push(shipyard_runtime::isolation::MountFlag::ReadOnly);
        }
        root.mounts.push(bind);
    }
    Ok(named)
}

/// Makes the named volumes `names` if new, and holds them: nothing removes
/// a volume while its hold lives.
pub(super) fn hold_volumes(engine: &Engine, names: &[String]) -> Result<Vec<VolumeHold>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::volumes::open(engine)?;
    names
        .iter()
        .map(|name| {
            let held = store
                .hold(name)
                .with_context(|| format!("the volume {name}"))?;
            if held.created() {
                crate::events::emit_about(engine, crate::events::Kind::Volume, "create", name);
            }
            Ok(held)
        })
        .collect()
}

/// Holds the named volumes `names`, as [`hold_volumes`] does, without
/// making any: for a copy from a stopped container, whose volume may have
/// been removed since.
///
/// # Errors
///
/// A volume that doesn't exist ([`ErrorKind::NotFound`]), or can't be held.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "copies need Linux")
)]
pub(super) fn hold_existing_volumes(engine: &Engine, names: &[String]) -> Result<Vec<VolumeHold>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = crate::volumes::open(engine)?;
    names
        .iter()
        .map(|name| {
            store
                .hold_existing(name)
                .with_context(|| format!("the container's volume {name}"))
        })
        .collect()
}
