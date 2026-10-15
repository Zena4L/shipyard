//! A run's `--device` and `--device-cgroup-rule` (stage 5, step 7), as
//! Docker takes them: host devices given to the container, and kinds
//! of devices it may use.

use std::path::PathBuf;

use shipyard_runtime::devices::Access;
#[cfg(target_os = "linux")]
use shipyard_runtime::devices::DeviceRule;
#[cfg(target_os = "linux")]
use shipyard_runtime::devices::{DeviceKind, DeviceNode, Devices};

/// `--device HOST[:CONTAINER][:PERMS]`: the host's device (or every one
/// below a directory), at `CONTAINER` (its own path if none), for `PERMS`
/// (`rwm` if none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceArg {
    /// The host's device, or a directory of them.
    pub host: PathBuf,
    /// Where it goes in the container (`host`'s own path if `None`).
    pub container: Option<PathBuf>,
    /// What the container may do with it.
    pub access: Access,
}

/// Adds `--device`s and `--device-cgroup-rule`s to `devices`: an allow rule
/// for each device, then the rules. With a root filesystem of the
/// container's own (`with_root`), the devices' nodes are made in it too;
/// without one, the container's `/dev` is the host's, and they're used where
/// they are.
///
/// # Errors
///
/// A host path that isn't a device (or a directory with none), or a
/// container path asked for without a root filesystem.
#[cfg(target_os = "linux")]
pub(super) fn add(
    devices: &mut Devices,
    args: &[DeviceArg],
    rules: &[DeviceRule],
    with_root: bool,
) -> crate::Result<()> {
    for arg in args {
        for node in host_nodes(arg)? {
            devices.rules.push(node.rule(arg.access));
            if with_root {
                devices.nodes.push(node);
            } else if arg.container.is_some() {
                crate::error::bail!(
                    InvalidRequest,
                    "--device {}:{}: a path of its own in the container needs --rootfs (without \
                     one, the container's /dev is the host's, and the device is used where it is)",
                    arg.host.display(),
                    node.path.display()
                );
            }
        }
    }
    devices.rules.extend_from_slice(rules);
    devices.asked |= !args.is_empty() || !rules.is_empty();
    Ok(())
}

/// The devices `arg` names on the host: the one at its path (followed
/// through symlinks: `/dev/disk/by-id/…`), or every one below it if it's a
/// directory (`/dev/dri`), each with the path it gets in the container and
/// the host's mode and owner.
#[cfg(target_os = "linux")]
fn host_nodes(arg: &DeviceArg) -> crate::Result<Vec<DeviceNode>> {
    use crate::error::Context as _;
    let resolved = std::fs::canonicalize(&arg.host)
        .map_err(|error| crate::Error::wrap(crate::ErrorKind::InvalidRequest, error))
        .with_context(|| format!("--device {}", arg.host.display()))?;
    let at = arg.container.clone().unwrap_or_else(|| arg.host.clone());
    let metadata =
        std::fs::metadata(&resolved).with_context(|| format!("--device {}", arg.host.display()))?;
    if !metadata.is_dir() {
        return device_at(&resolved, &metadata, at)
            .map(|node| vec![node])
            .ok_or_else(|| {
                crate::Error::new(
                    crate::ErrorKind::InvalidRequest,
                    format!("--device {}: not a device", arg.host.display()),
                )
            });
    }
    let mut nodes = Vec::new();
    let mut dirs = vec![resolved.clone()];
    // As `find -xdev`: not into what's mounted below it (`/dev/pts`'s host
    // terminals, `/dev/shm`).
    let file_system = std::os::unix::fs::MetadataExt::dev(&metadata);
    while let Some(dir) = dirs.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("--device {}", dir.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            // Symlinks in it aren't followed: they'd lead out of it, or to a
            // device already listed.
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if std::os::unix::fs::MetadataExt::dev(&metadata) == file_system {
                    dirs.push(path);
                }
            } else if let Ok(relative) = path.strip_prefix(&resolved)
                && let Some(found) = device_at(&path, &metadata, at.join(relative))
            {
                nodes.push(found);
            }
        }
    }
    if nodes.is_empty() {
        crate::error::bail!(
            InvalidRequest,
            "--device {}: no devices in it",
            arg.host.display()
        );
    }
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(nodes)
}

/// The node at `host` (whose metadata is `metadata`) as one at `at` in the
/// container, if it's a device.
#[cfg(target_os = "linux")]
fn device_at(
    host: &std::path::Path,
    metadata: &std::fs::Metadata,
    at: PathBuf,
) -> Option<DeviceNode> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let kind = if metadata.file_type().is_char_device() {
        DeviceKind::Char
    } else if metadata.file_type().is_block_device() {
        DeviceKind::Block
    } else {
        return None;
    };
    let rdev = metadata.rdev();
    let mut node = DeviceNode::new(at, kind, libc::major(rdev), libc::minor(rdev));
    node.mode = metadata.mode() & 0o7777;
    node.uid = metadata.uid();
    node.gid = metadata.gid();
    node.source = (node.path != host).then(|| host.to_path_buf());
    Some(node)
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn arg(host: &str, container: Option<&str>, access: &str) -> DeviceArg {
        DeviceArg {
            host: host.into(),
            container: container.map(PathBuf::from),
            access: Access::parse(access).unwrap(),
        }
    }

    #[test]
    fn a_host_device_is_found_with_its_numbers() {
        let mut devices = Devices::docker_default();
        add(
            &mut devices,
            &[arg("/dev/null", Some("/dev/mine"), "r")],
            &[],
            true,
        )
        .unwrap();
        let node = devices.nodes.last().unwrap();
        assert_eq!(
            (node.kind, node.major, node.minor),
            (DeviceKind::Char, 1, 3)
        );
        assert_eq!(node.path, PathBuf::from("/dev/mine"));
        assert_eq!(node.source, Some(PathBuf::from("/dev/null")));
        assert!(devices.asked);
        // Not a device, and a path of its own without a root filesystem.
        assert!(add(&mut devices, &[arg("/etc/hostname", None, "r")], &[], true).is_err());
        assert!(
            add(
                &mut devices,
                &[arg("/dev/null", Some("/dev/x"), "r")],
                &[],
                false
            )
            .is_err()
        );
        // A directory: every device below it.
        let mut devices = Devices::docker_default();
        add(&mut devices, &[arg("/dev", Some("/host"), "r")], &[], true).unwrap();
        assert!(
            devices
                .nodes
                .iter()
                .any(|node| node.path == std::path::Path::new("/host/null"))
        );
    }
}
