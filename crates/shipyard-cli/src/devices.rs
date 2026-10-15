//! `shipyard run`'s `--device` and `--device-cgroup-rule` (stage 5, step
//! 7): their syntax, as Docker takes them (the engine's `run::DeviceArg` is
//! what they come to).

use std::path::PathBuf;

use shipyard_engine::run::DeviceArg;
use shipyard_runtime::devices::{Access, DeviceRule};

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
}
