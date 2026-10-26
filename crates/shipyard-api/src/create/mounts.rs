//! A create body's mounts (`HostConfig.Binds`, `Mounts`, `Tmpfs`) and the
//! anonymous volumes its `Volumes` asks for, as the run's own flags.

use std::path::PathBuf;

use shipyard_engine::run::RunRequest;

use crate::errors::ApiError;

use super::body::CreateBody;
use super::{body, field};

/// One of `HostConfig.Mounts`, as `--mount` reads it (the engine's
/// [`MountRequest`](shipyard_engine::run::MountRequest), its checks
/// shared): what Shipyard can't do refused (a volume's driver, labels or
/// subpath, a tmpfs's own `Options`, and what `MountRequest` refuses);
/// with a bind's source to make, if it asks for one (`CreateMountpoint`).
pub(super) fn mounted(
    mount: &body::Mount,
) -> Result<(shipyard_engine::run::DockerMount, Option<PathBuf>), ApiError> {
    let refuse = |what: &str| Err(field("Mounts", &format!("{what} isn't supported")));
    if let Some(volume) = &mount.volume_options
        && (volume
            .labels
            .as_ref()
            .is_some_and(|labels| !labels.is_empty())
            || volume
                .driver_config
                .as_ref()
                .is_some_and(|driver| !driver.is_null())
            || volume
                .subpath
                .as_deref()
                .is_some_and(|subpath| !subpath.is_empty()))
    {
        return refuse("a volume's labels, driver or subpath");
    }
    let mut asked = shipyard_engine::run::MountRequest::new(
        mount
            .kind
            .as_deref()
            .unwrap_or("volume")
            .to_ascii_lowercase(),
        mount.target.clone().unwrap_or_default(),
    );
    asked.source.clone_from(&mount.source);
    asked.read_only = mount.read_only.unwrap_or(false);
    if let Some(bind) = &mount.bind_options {
        asked.propagation.clone_from(&bind.propagation);
        asked.non_recursive = bind.non_recursive.unwrap_or(false);
        asked.create_mountpoint = bind.create_mountpoint.unwrap_or(false);
    }
    asked.no_copy = mount
        .volume_options
        .as_ref()
        .and_then(|volume| volume.no_copy);
    if let Some(tmpfs) = &mount.tmpfs_options {
        if tmpfs
            .options
            .as_ref()
            .is_some_and(|options| !options.is_empty())
        {
            return refuse("a tmpfs's own options (TmpfsOptions.Options)");
        }
        asked.tmpfs_size = tmpfs
            .size_bytes
            .filter(|&size| size != 0)
            .map(|size| size.to_string());
        asked.tmpfs_mode = tmpfs.mode.filter(|&mode| mode != 0).map(go_mode);
    }
    let resolved = asked
        .resolve()
        .map_err(|error| field("Mounts", &error.to_string()))?;
    Ok((resolved, asked.source_to_make().map(PathBuf::from)))
}

/// Go's `os.FileMode` as an octal `mode`: its permission bits, and its
/// set-user-ID, set-group-ID and sticky bits (Go's own, high up) where
/// `chmod(2)` has them; its other bits (the type) left out, as the
/// kernel's `&07777` leaves them in Docker's daemon.
fn go_mode(mode: u32) -> String {
    const SETUID: u32 = 1 << 23;
    const SETGID: u32 = 1 << 22;
    const STICKY: u32 = 1 << 20;
    let mut octal = mode & 0o7777;
    for (go, unix) in [(SETUID, 0o4000), (SETGID, 0o2000), (STICKY, 0o1000)] {
        if mode & go != 0 {
            octal |= unix;
        }
    }
    format!("{octal:o}")
}

/// `Volumes` (Docker's `-v /path`): each place no bind, mount or tmpfs
/// covers is an anonymous volume, made at the create (`/path` among the
/// request's volumes).
pub(super) fn the_anonymous_volumes(
    body: &CreateBody,
    request: &mut RunRequest,
) -> Result<(), ApiError> {
    for path in body.volumes.iter().flatten().map(|(path, _)| path) {
        if !path.starts_with('/') || path.contains(':') {
            return Err(field(
                "Volumes",
                &format!("{path:?} isn't an absolute path without ':'"),
            ));
        }
        if !shipyard_engine::run::covers(
            &request.volume,
            &request.tmpfs,
            std::path::Path::new(path),
        ) {
            request.volume.push(path.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
