//! `shipyard run`'s `--device` and `--device-cgroup-rule` (stage 5, step
//! 7), as Docker takes them: host devices given to the container, and kinds
//! of devices it may use.

use std::path::PathBuf;

use shipyard_runtime::devices::{Access, DeviceRule};
#[cfg(target_os = "linux")]
use shipyard_runtime::devices::{DeviceKind, DeviceNode, Devices};

/// `--device HOST[:CONTAINER][:PERMS]`: the host's device (or every one
/// below a directory), at `CONTAINER` (its own path if none), for `PERMS`
/// (`rwm` if none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceArg {
    host: PathBuf,
    container: Option<PathBuf>,
    access: Access,
}

/// Reads `--device`'s value as Docker does: with two parts, the second is
/// the permissions if it's made of `r`, `w` and `m`, the container path
/// otherwise.
pub(crate) fn parse_device(text: &str) -> Result<DeviceArg, String> {
    let parts: Vec<&str> = text.split(':').collect();
    let (host, container, access) = match parts.as_slice() {
        [host] => (*host, None, None),
        [host, perms] if Access::parse(perms).is_ok() => (*host, None, Some(*perms)),
        [host, container] => (*host, Some(*container), None),
        [host, container, perms] => (*host, Some(*container), Some(*perms)),
        _ => return Err(format!("{text:?} isn't HOST[:CONTAINER][:PERMS]")),
    };
    if !host.starts_with('/') {
        return Err(format!("{host:?}: the host's device is an absolute path"));
    }
    if let Some(container) = container.filter(|path| !path.starts_with('/')) {
        return Err(format!(
            "{container:?}: the path in the container is absolute"
        ));
    }
    let access = access
        .map(Access::parse)
        .transpose()
        .map_err(|err| err.to_string())?
        .unwrap_or(Access::ALL);
    Ok(DeviceArg {
        host: PathBuf::from(host),
        container: container.map(PathBuf::from),
        access,
    })
}

/// Reads `--device-cgroup-rule`'s value: `c 1:3 rwm`, `b *:* m`, …
pub(crate) fn parse_rule(text: &str) -> Result<DeviceRule, String> {
    text.parse()
        .map_err(|err: shipyard_runtime::devices::DeviceError| err.to_string())
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
pub(crate) fn add(
    devices: &mut Devices,
    args: &[DeviceArg],
    rules: &[DeviceRule],
    with_root: bool,
) -> anyhow::Result<()> {
    for arg in args {
        for node in host_nodes(arg)? {
            devices.rules.push(node.rule(arg.access));
            if with_root {
                devices.nodes.push(node);
            } else if arg.container.is_some() {
                anyhow::bail!(
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
fn host_nodes(arg: &DeviceArg) -> anyhow::Result<Vec<DeviceNode>> {
    use anyhow::Context as _;
    let resolved = std::fs::canonicalize(&arg.host)
        .with_context(|| format!("--device {}", arg.host.display()))?;
    let at = arg.container.clone().unwrap_or_else(|| arg.host.clone());
    let metadata =
        std::fs::metadata(&resolved).with_context(|| format!("--device {}", arg.host.display()))?;
    if !metadata.is_dir() {
        return device_at(&resolved, &metadata, at)
            .map(|node| vec![node])
            .ok_or_else(|| anyhow::anyhow!("--device {}: not a device", arg.host.display()));
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
        anyhow::bail!("--device {}: no devices in it", arg.host.display());
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

#[cfg(test)]
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
    fn device_flags_read_as_docker_reads_them() {
        assert_eq!(parse_device("/dev/sdb"), Ok(arg("/dev/sdb", None, "rwm")));
        assert_eq!(parse_device("/dev/sdb:r"), Ok(arg("/dev/sdb", None, "r")));
        assert_eq!(
            parse_device("/dev/sdb:/dev/xvdc"),
            Ok(arg("/dev/sdb", Some("/dev/xvdc"), "rwm"))
        );
        assert_eq!(
            parse_device("/dev/sdb:/dev/xvdc:rw"),
            Ok(arg("/dev/sdb", Some("/dev/xvdc"), "rw"))
        );
        for bad in [
            "sdb",
            "/dev/sdb:xvdc",
            "/dev/sdb:/dev/x:rx",
            "/a:/b:r:extra",
            "",
        ] {
            assert!(parse_device(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rules_read_as_docker_writes_them() {
        assert_eq!(
            parse_rule("c 1:3 rwm").unwrap().to_string(),
            "allow c 1:3 rwm"
        );
        assert!(parse_rule("c 1:3").is_err());
    }

    #[cfg(target_os = "linux")]
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
