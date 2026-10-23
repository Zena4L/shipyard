//! What a container's create settled beside its record (step 11.3b1): the
//! ports it was asked to expose (`run --expose`, the API's `ExposedPorts`),
//! whether it gets the default process limit (F62, D11.3b-2), and (v2,
//! step 11.3e2) its tmpfs mounts. Record
//! v3's config is frozen, and a field of its own would need a new record
//! version that older binaries refuse (F67's rule: a file beside it).
//! Written once, inside the container's directory before it's placed, so
//! a container appears whole with it; read at each start (`-P`, the
//! limit, the tmpfs mounts: one that can't be read refuses the start) and
//! by `inspect`; and (v3, steps 11.4a2a-b) the anonymous volumes it made,
//! read by its removal (`rm -v`, `--rm`), and the bind sources each start
//! makes if they're missing (the API's, as Docker's daemon makes them). A container without it (made before 11.3b1)
//! settled nothing of these.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ContainerId;
use super::store::record_dir;
use crate::Engine;
use crate::error::{ErrorKind, Result, bail};

/// The file, in the container's directory.
const CREATED: &str = "created.json";

/// The most ports one container may expose: `-P` publishes each on its
/// own host port, and a monitor holds a socket for each.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a container's create is Linux's")
)]
pub(crate) const MAX_EXPOSED: usize = 1024;

/// What a container's create settled beside its record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Created {
    /// The ports it exposes beside its image's (keys like `80/tcp`).
    pub exposed: Vec<String>,
    /// Whether its starts give it the default process limit
    /// ([`DEFAULT_PIDS_MAX`](crate::run::DEFAULT_PIDS_MAX)) where the `pids`
    /// controller reaches its cgroup: it asked none, nor `-1`.
    pub default_pids: bool,
    /// Its tmpfs mounts, fresh at each start.
    pub tmpfs: Vec<crate::run::TmpfsMount>,
    /// The anonymous volumes its create made for it (`-v /path`, the
    /// image's `VOLUME`s): `rm -v` and `--rm` take them.
    pub anonymous: Vec<String>,
    /// Bind sources made if they're missing, at the create and at each
    /// start (the API's `Binds` and `CreateMountpoint`, Docker's).
    pub make_sources: Vec<String>,
    /// The container whose network it joins at each start (`--network
    /// container:NAME`), by ID: a rename never re-points it.
    pub network_from: Option<ContainerId>,
    /// The container whose IPC namespace (and `/dev/shm`) it joins at each
    /// start (`--ipc container:NAME`), by ID.
    pub ipc_from: Option<ContainerId>,
    /// Its input closed once its first attached client's ends (Docker's
    /// `StdinOnce`), at each start: kept here, not in the API's memory, so
    /// a service that ends between the create and the start keeps it
    /// (F116).
    pub stdin_once: bool,
    /// Which of `make_sources` the create made, and below what (v6, step
    /// 11.7): each start makes and binds them again by descriptor.
    pub made_sources: Vec<crate::run::MadeSource>,
}

/// The file's form (v6:
/// `{"version":6,"exposed":["80/tcp"],"default_pids":true,"tmpfs":[{"destination":"/run","options":["size=64m"]}],"anonymous":["<64 hex>"],"make_sources":["/srv/data"],"network_from":"<64 hex>","ipc_from":"<64 hex>","stdin_once":true,"made_sources":[{"source":"/srv/data","dir":"/srv","below":"data"}]}`;
/// v5 has no `made_sources`, v4 no `stdin_once`, v3 no `network_from` or `ipc_from`, v2 no
/// `anonymous` or `make_sources`, v1 no `tmpfs` either: none). Each written at the
/// oldest version that holds it, so an older build still reads what it
/// can.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatedFile {
    version: u32,
    exposed: Vec<String>,
    default_pids: bool,
    #[serde(default)]
    tmpfs: Vec<TmpfsFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    anonymous: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    make_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ipc_from: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    stdin_once: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    made_sources: Vec<MadeFile>,
}

/// A made bind source, as the file keeps it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MadeFile {
    source: String,
    dir: String,
    below: String,
}

/// A tmpfs mount, as the file keeps it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TmpfsFile {
    destination: String,
    options: Vec<String>,
}

impl CreatedFile {
    /// The version written.
    const VERSION: u32 = 6;
    /// The most read: [`MAX_EXPOSED`] keys of at most `65535/udp`'s
    /// length, quoted, with room.
    const MAX_FILE: u64 = 64 * 1024;
}

/// What a container exposes: its image's `exposed` ports, then those it
/// was `asked` (`--expose`, kept as `80/tcp`) the image doesn't have,
/// however the image writes them (`80` is `80/tcp`): what `-P` publishes
/// and `inspect` shows, each port once.
pub(crate) fn exposed_ports(exposed: &[String], asked: &[String]) -> Vec<String> {
    let mut ports = exposed.to_vec();
    let mut have: Vec<String> = exposed
        .iter()
        .filter_map(|key| shipyard_network::ports::exposed_key(key))
        .collect();
    for port in asked {
        let key = shipyard_network::ports::exposed_key(port).unwrap_or_else(|| port.clone());
        if !have.contains(&key) {
            have.push(key);
            ports.push(port.clone());
        }
    }
    ports
}

/// Writes `created` into `dir`, a new container's directory not yet
/// placed. Nothing settled: no file.
///
/// # Errors
///
/// It couldn't be written, or synced (a new container's directory must
/// hold it on the disk before it's placed).
pub(crate) fn write_created(dir: &Path, created: &Created) -> Result<()> {
    if *created == Created::default() {
        return Ok(());
    }
    let tmpfs: Vec<TmpfsFile> = created
        .tmpfs
        .iter()
        .map(|mount| {
            Ok(TmpfsFile {
                destination: mount
                    .destination()
                    .to_str()
                    .ok_or_else(|| {
                        crate::Error::new(ErrorKind::InvalidRequest, "a tmpfs's path must be UTF-8")
                    })?
                    .to_owned(),
                options: mount.options().to_vec(),
            })
        })
        .collect::<Result<_>>()?;
    let json = serde_json::to_vec(&CreatedFile {
        // The oldest that holds it: v1 (11.3b1's) with ports and the
        // process limit only, v2 (11.3e2's) with tmpfs too, v3 (11.4a2's)
        // with anonymous volumes and bind sources, v4 (11.4b2b's) with
        // another container's namespaces joined, v5 (11.6a1's) with its
        // input closed once, v6 (11.7's) with the bind sources it made.
        version: if !created.made_sources.is_empty() {
            CreatedFile::VERSION
        } else if created.stdin_once {
            5
        } else if created.network_from.is_some() || created.ipc_from.is_some() {
            4
        } else if !created.anonymous.is_empty() || !created.make_sources.is_empty() {
            3
        } else if !tmpfs.is_empty() {
            2
        } else {
            1
        },
        exposed: created.exposed.clone(),
        default_pids: created.default_pids,
        tmpfs,
        anonymous: created.anonymous.clone(),
        make_sources: created.make_sources.clone(),
        network_from: created
            .network_from
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        ipc_from: created.ipc_from.as_ref().map(|id| id.as_str().to_owned()),
        stdin_once: created.stdin_once,
        made_sources: created
            .made_sources
            .iter()
            .map(|made| {
                let text = |path: &Path| {
                    path.to_str().map(str::to_owned).ok_or_else(|| {
                        crate::Error::new(
                            ErrorKind::InvalidRequest,
                            format!("{}: a bind's source must be UTF-8", path.display()),
                        )
                    })
                };
                Ok(MadeFile {
                    source: text(&made.source)?,
                    dir: text(&made.dir)?,
                    below: text(&made.below)?,
                })
            })
            .collect::<Result<_>>()?,
    })
    .map_err(|error| crate::Error::new(ErrorKind::Storage, error.to_string()))?;
    // Never more than a start reads back: refused at the create.
    if u64::try_from(json.len()).unwrap_or(u64::MAX) > CreatedFile::MAX_FILE {
        bail!(
            InvalidRequest,
            "too many tmpfs mounts, exposed ports and volumes ({} bytes to keep, at most {})",
            json.len(),
            CreatedFile::MAX_FILE
        );
    }
    if let Some(unsynced) = crate::fs::write_atomically(dir, CREATED, &json, 0o600)?.unsynced() {
        return Err(unsynced);
    }
    Ok(())
}

/// What container `id`'s create settled: nothing for one made before
/// 11.3b1 (no file).
///
/// # Errors
///
/// [`ErrorKind::Storage`]: the file is there but can't be read, isn't
/// what it should be, or is a newer version's.
pub(crate) fn created(engine: &Engine, id: &ContainerId) -> Result<Created> {
    read_created(&record_dir(engine, id)?)
}

/// The container whose network container `id` joins, if any: none if its
/// create can't be read (its starts say why).
pub(crate) fn network_from(engine: &Engine, id: &ContainerId) -> Option<ContainerId> {
    created(engine, id)
        .ok()
        .and_then(|created| created.network_from)
}

/// What `dir`'s `created.json` says, as [`created`].
fn read_created(dir: &Path) -> Result<Created> {
    let path = dir.join(CREATED);
    let bytes = match shipyard_sys::fs::read_bounded(&path, CreatedFile::MAX_FILE) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Created::default());
        }
        Err(error) => {
            return Err(crate::Error::from(error)
                .context(format!("reading {}", path.display()))
                .io_as(ErrorKind::Storage));
        }
    };
    // Another container's ID, checked (an edited file names none).
    let container_id = |id: Option<String>| {
        id.map(|id| {
            ContainerId::parse(&id).map_err(|_| {
                crate::Error::new(
                    ErrorKind::Storage,
                    format!("{}: {id:?} isn't a container's ID", path.display()),
                )
            })
        })
        .transpose()
    };
    match shipyard_sys::versioned::read::<CreatedFile>(&bytes, CreatedFile::VERSION) {
        Ok(file) => Ok(Created {
            exposed: file.exposed,
            default_pids: file.default_pids,
            // Checked again: a file edited by hand never reaches `mount(2)`
            // with what a create would refuse.
            tmpfs: file
                .tmpfs
                .into_iter()
                .map(|mount| crate::run::TmpfsMount::new(&mount.destination, mount.options))
                .collect::<Result<_>>()
                .map_err(|error| {
                    crate::Error::new(ErrorKind::Storage, format!("{}: {error}", path.display()))
                })?,
            anonymous: match file
                .anonymous
                .iter()
                .find(|name| !shipyard_storage::volume::is_named_volume(name))
            {
                Some(name) => bail!(
                    Storage,
                    "{}: {name:?} isn't a volume's name",
                    path.display()
                ),
                None => file.anonymous,
            },
            make_sources: match file
                .make_sources
                .iter()
                .find(|source| !source.starts_with('/'))
            {
                Some(source) => {
                    bail!(
                        Storage,
                        "{}: {source:?} isn't an absolute path",
                        path.display()
                    )
                }
                None => file.make_sources,
            },
            network_from: container_id(file.network_from)?,
            ipc_from: container_id(file.ipc_from)?,
            stdin_once: file.stdin_once,
            // Checked again: an edited file never makes, nor binds, a path
            // a create wouldn't have.
            made_sources: file
                .made_sources
                .into_iter()
                .map(|made| {
                    crate::run::MadeSource::checked(
                        made.source.into(),
                        made.dir.into(),
                        made.below.into(),
                    )
                    .ok_or_else(|| {
                        crate::Error::new(
                            ErrorKind::Storage,
                            format!("{}: a made bind source that isn't plain", path.display()),
                        )
                    })
                })
                .collect::<Result<_>>()?,
        }),
        Err(error) => bail!(Storage, "{}: {error}", path.display()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// An image's `80` and an asked `80/tcp` are one port; a UDP one of
    /// the same number isn't.
    #[test]
    fn a_port_is_exposed_once_however_it_is_written() {
        let image = ["80".to_owned(), "53/udp".to_owned()];
        let asked = [
            "80/tcp".to_owned(),
            "53/tcp".to_owned(),
            "53/udp".to_owned(),
        ];
        assert_eq!(exposed_ports(&image, &asked), ["80", "53/udp", "53/tcp"]);
    }

    /// v3 written and read back with its anonymous volumes and bind
    /// sources to make (either alone is v3 too), v2 with its
    /// tmpfs mounts alone, v1 with neither (each the oldest that holds it);
    /// v4 with the network it joins, v5 with its input closed once (older
    /// versions read: [`older_versions_read_as_none_of_what_they_lack`]); a
    /// newer version, a field it doesn't know, a tmpfs option a create
    /// would refuse or a volume's name that isn't one (an edited file) is
    /// refused, never guessed at (H2).
    #[test]
    fn the_file_is_versioned() {
        let dir = std::env::temp_dir().join(format!("created-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let made = Created {
            exposed: vec!["80/tcp".to_owned()],
            default_pids: true,
            tmpfs: vec![crate::run::TmpfsMount::parse("/run:size=64m,exec").unwrap()],
            anonymous: vec!["a".repeat(64)],
            make_sources: vec!["/srv/data".to_owned()],
            network_from: None,
            ipc_from: None,
            stdin_once: false,
            made_sources: Vec::new(),
        };
        write_created(&dir, &made).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(written.starts_with(r#"{"version":3,"#), "{written}");
        assert_eq!(read_created(&dir).unwrap(), made);
        let joining = Created {
            network_from: ContainerId::parse(&"b".repeat(64)).ok(),
            ipc_from: ContainerId::parse(&"c".repeat(64)).ok(),
            ..made.clone()
        };
        write_created(&dir, &joining).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(
            written.starts_with(r#"{"version":4,"#),
            "a network joined: v4, {written}"
        );
        assert_eq!(read_created(&dir).unwrap(), joining);
        // Its input closed once (F116): v5, alone or with the rest.
        let once = Created {
            stdin_once: true,
            ..Created::default()
        };
        write_created(&dir, &once).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(
            written.starts_with(r#"{"version":5,"#) && written.contains(r#""stdin_once":true"#),
            "input closed once: v5, {written}"
        );
        assert_eq!(read_created(&dir).unwrap(), once);
        let sources_only = Created {
            anonymous: Vec::new(),
            tmpfs: Vec::new(),
            ..made.clone()
        };
        write_created(&dir, &sources_only).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(
            written.starts_with(r#"{"version":3,"#),
            "bind sources alone: v3, {written}"
        );
        assert_eq!(read_created(&dir).unwrap(), sources_only);
        let tmpfs_only = Created {
            anonymous: Vec::new(),
            make_sources: Vec::new(),
            ..made.clone()
        };
        write_created(&dir, &tmpfs_only).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(
            written.starts_with(r#"{"version":2,"#) && !written.contains("anonymous"),
            "no anonymous volumes: v2, {written}"
        );
        let ports_only = Created {
            tmpfs: Vec::new(),
            ..tmpfs_only.clone()
        };
        write_created(&dir, &ports_only).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(
            written.starts_with(r#"{"version":1,"#),
            "no tmpfs: v1, {written}"
        );
        assert_eq!(read_created(&dir).unwrap(), ports_only);
        let huge = Created {
            tmpfs: (0..400)
                .map(|n| {
                    crate::run::TmpfsMount::new(&format!("/{}{n}", "d".repeat(200)), Vec::new())
                        .unwrap()
                })
                .collect(),
            ..made.clone()
        };
        assert_eq!(
            write_created(&dir, &huge).unwrap_err().kind(),
            ErrorKind::InvalidRequest,
            "more than a start reads back"
        );
        write_created(&dir, &made).unwrap();
        assert_eq!(read_created(&dir).unwrap(), made);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Older versions read as none of what they lack: 11.3b1's v1, 11.3e2's
    /// v2, 11.4b2b's v4 (no `stdin_once`: its input as made) (H2).
    #[test]
    fn older_versions_read_as_none_of_what_they_lack() {
        let dir = std::env::temp_dir().join(format!("created-older-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |text: &str| std::fs::write(dir.join(CREATED), text).unwrap();
        let ports_only = Created {
            exposed: vec!["80/tcp".to_owned()],
            default_pids: true,
            ..Created::default()
        };
        write(r#"{"version":1,"exposed":["80/tcp"],"default_pids":true}"#);
        assert_eq!(read_created(&dir).unwrap(), ports_only);
        write(
            r#"{"version":2,"exposed":["80/tcp"],"default_pids":true,"tmpfs":[{"destination":"/run","options":["size=64m","exec"]}]}"#,
        );
        assert_eq!(
            read_created(&dir).unwrap(),
            Created {
                tmpfs: vec![crate::run::TmpfsMount::parse("/run:size=64m,exec").unwrap()],
                ..ports_only
            }
        );
        write(&format!(
            r#"{{"version":4,"exposed":[],"default_pids":false,"network_from":"{}"}}"#,
            "b".repeat(64)
        ));
        assert_eq!(
            read_created(&dir).unwrap(),
            Created {
                network_from: ContainerId::parse(&"b".repeat(64)).ok(),
                ..Created::default()
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bind sources a create made (11.7): v6, read back as they were;
    /// one whose path isn't plain (an edited file) refused.
    #[test]
    fn made_sources_are_v6() {
        let dir = std::env::temp_dir().join(format!("created-made-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let made_here = Created {
            make_sources: vec!["/srv/data/x".to_owned()],
            made_sources: vec![
                crate::run::MadeSource::checked(
                    "/srv/data/x".into(),
                    "/srv".into(),
                    "data/x".into(),
                )
                .unwrap(),
            ],
            ..Created::default()
        };
        write_created(&dir, &made_here).unwrap();
        let written = std::fs::read_to_string(dir.join(CREATED)).unwrap();
        assert!(
            written.starts_with(r#"{"version":6,"#)
                && written.contains(
                    r#""made_sources":[{"source":"/srv/data/x","dir":"/srv","below":"data/x"}]"#
                ),
            "made sources: v6, {written}"
        );
        assert_eq!(read_created(&dir).unwrap(), made_here);
        for below in ["../etc", "/etc", ""] {
            let edited = written.replace(r#""below":"data/x""#, &format!(r#""below":"{below}""#));
            std::fs::write(dir.join(CREATED), edited).unwrap();
            assert_eq!(
                read_created(&dir).unwrap_err().kind(),
                ErrorKind::Storage,
                "{below:?}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A newer version, a field it doesn't know, a tmpfs option a create
    /// would refuse, a volume's name or a container's ID that isn't one
    /// (an edited file): refused, never guessed at (H2).
    #[test]
    fn what_a_create_wouldn_t_write_is_refused() {
        let dir = std::env::temp_dir().join(format!("created-refused-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |text: &str| std::fs::write(dir.join(CREATED), text).unwrap();
        for refused in [
            r#"{"version":7,"exposed":[],"default_pids":false}"#,
            r#"{"version":4,"exposed":[],"default_pids":false,"network_from":"../x"}"#,
            r#"{"version":4,"exposed":[],"default_pids":false,"ipc_from":"x"}"#,
            r#"{"version":3,"exposed":[],"default_pids":false,"anonymous":["../x"]}"#,
            r#"{"version":3,"exposed":[],"default_pids":false,"make_sources":["rel"]}"#,
            r#"{"version":1,"exposed":[],"default_pids":false,"more":1}"#,
            r#"{"version":2,"exposed":[],"default_pids":false,"tmpfs":[{"destination":"/run","options":["size=1m,mode=777"]}]}"#,
            r#"{"version":2,"exposed":[],"default_pids":false,"tmpfs":[{"destination":"run","options":[]}]}"#,
        ] {
            write(refused);
            assert_eq!(
                read_created(&dir).unwrap_err().kind(),
                ErrorKind::Storage,
                "{refused}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
