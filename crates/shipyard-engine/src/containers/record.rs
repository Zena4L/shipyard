//! A container's record (`record.json`), and its versions.
//!
//! Version 1 is the first (9.2b); version 2 (9.4a) adds the image a
//! container was made from, `--rm`, and the ports its last start bound;
//! version 3 (9.5a) adds its restart policy (in its config), whether its
//! user stopped it, its restarts, the boot it last started in, and when a
//! restart waits to start it (and, 9.5b, its last restart's delay). Within
//! a version the shape is fixed (the config's restart policy and the delay
//! aside, which version 3 records may lack: see [`Config`]): a field added
//! later makes
//! a new version, and the reader keeps reading the older ones (the
//! ROADMAP's H2).
//! A record of a version above this Shipyard's is refused, never misread or
//! rewritten.

use std::io::Read as _;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::config::Config;
use super::id::{ContainerId, Serial};
use super::labels::Labels;
use super::names::Name;
use crate::error::{Context as _, Result, bail};

/// The record's version: what's written.
pub(crate) const VERSION: u32 = 3;

/// The most of a record read (and written): far more than any record
/// within its bounds (labels, names) takes.
pub(crate) const MAX_SIZE: u64 = 1024 * 1024;

/// What the engine keeps of a container, for as long as the container
/// exists: who it is, how to run it, and how it last ran. Read and
/// written through its versions only (`Record::parse`'s, `to_json`'s).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Record {
    /// Its ID, also its runtime state directory's name.
    pub id: ContainerId,
    /// Its serial.
    pub serial: Serial,
    /// Its name (also its name on its networks).
    pub name: Name,
    /// When it was made.
    pub created: SystemTime,
    /// How to run it, as asked.
    pub config: Config,
    /// Its labels.
    pub labels: Labels,
    /// When it last started (none: never).
    pub started: Option<SystemTime>,
    /// When it last ended, as far as its record knows (the runtime's exit
    /// record says it while it has one).
    pub finished: Option<SystemTime>,
    /// How it last ended.
    pub exit: Option<Exit>,
    /// The image it was made from, by its manifest's digest: every start
    /// runs that one, whatever its tag names since (none: no image, or a
    /// version 1 record).
    pub image_id: Option<String>,
    /// Removed once it ends (`--rm`).
    pub auto_remove: bool,
    /// The host ports its last start published.
    pub ports: Vec<BoundPort>,
    /// Stopped by its user (`stop`, `kill`, `restart`, `rm -f`) since it
    /// was last started: its restart policy leaves it so (Docker's
    /// "manually stopped"; `always` comes back when the host starts again).
    pub stopped_by_user: bool,
    /// The restarts its policy made since its user last started it.
    pub restart_count: u32,
    /// The boot its last start ran in (the kernel's boot ID): a start of
    /// another boot ended with it.
    pub boot_id: Option<String>,
    /// When its restart policy starts it again, while a restart waits.
    pub next_start: Option<SystemTime>,
    /// The delay before its last restart, in milliseconds (0: none yet):
    /// the next one's backoff doubles it.
    pub restart_delay_ms: u64,
}

/// A host port a start published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct BoundPort {
    /// The host address it's on (`0.0.0.0` or `::` for every one).
    pub host_ip: std::net::IpAddr,
    /// The host port.
    pub host_port: u16,
    /// The container's port.
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
}

impl BoundPort {
    /// A port published on `host_ip:host_port`, to the container's
    /// `container_port`, over `protocol` (`tcp` or `udp`).
    #[must_use]
    pub fn new(
        host_ip: std::net::IpAddr,
        host_port: u16,
        container_port: u16,
        protocol: &str,
    ) -> Self {
        Self {
            host_ip,
            host_port,
            container_port,
            protocol: protocol.to_owned(),
        }
    }
}

/// How a container ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Exit {
    /// Its exit code (128 + N for signal N), if known.
    pub code: Option<i32>,
    /// What the out-of-memory killer did, if it ended it.
    pub oom: Option<String>,
}

impl From<&shipyard_runtime::container::ExitRecord> for Exit {
    fn from(exit: &shipyard_runtime::container::ExitRecord) -> Self {
        Self {
            code: exit.code,
            oom: exit.oom.clone(),
        }
    }
}

/// Version 3, as it's written: every field, always (within a version the
/// shape is fixed).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V3 {
    version: u32,
    id: ContainerId,
    serial: Serial,
    name: Name,
    created: SystemTime,
    config: Config,
    labels: Labels,
    started: Option<SystemTime>,
    finished: Option<SystemTime>,
    exit: Option<Exit>,
    image_id: Option<String>,
    auto_remove: bool,
    ports: Vec<BoundPort>,
    stopped_by_user: bool,
    restart_count: u32,
    boot_id: Option<String>,
    next_start: Option<SystemTime>,
    /// Added before version 3 was released (9.5b): a version 3 record of
    /// 9.5a's builds has none (0).
    #[serde(default)]
    restart_delay_ms: u64,
}

/// Version 2 (9.4a), as it was written: read, never written again.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2 {
    /// Checked before (`parse`): here only so it's no unknown field.
    #[serde(rename = "version")]
    _version: u32,
    id: ContainerId,
    serial: Serial,
    name: Name,
    created: SystemTime,
    config: Config,
    labels: Labels,
    started: Option<SystemTime>,
    finished: Option<SystemTime>,
    exit: Option<Exit>,
    image_id: Option<String>,
    auto_remove: bool,
    ports: Vec<BoundPort>,
}

/// Version 1 (9.2b), as it was written: read, never written again.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    /// Checked before (`parse`): here only so it's no unknown field.
    #[serde(rename = "version")]
    _version: u32,
    id: ContainerId,
    serial: Serial,
    name: Name,
    created: SystemTime,
    config: Config,
    labels: Labels,
    started: Option<SystemTime>,
    finished: Option<SystemTime>,
    exit: Option<Exit>,
}

impl Record {
    /// A new container's record.
    pub(crate) fn new(
        id: ContainerId,
        serial: Serial,
        name: Name,
        config: Config,
        labels: Labels,
    ) -> Self {
        Self {
            id,
            serial,
            name,
            created: SystemTime::now(),
            config,
            labels,
            started: None,
            finished: None,
            exit: None,
            image_id: None,
            auto_remove: false,
            ports: Vec::new(),
            stopped_by_user: false,
            restart_count: 0,
            boot_id: None,
            next_start: None,
            restart_delay_ms: 0,
        }
    }

    /// The request a start runs: its config's, under its name.
    ///
    /// # Errors
    ///
    /// As [`Config::to_request`].
    pub fn to_request(&self) -> Result<crate::run::RunRequest> {
        self.config.to_request(&self.name)
    }

    /// What's written: version 3.
    ///
    /// # Errors
    ///
    /// It can't be written as JSON (a time before 1970), or it's bigger
    /// than [`MAX_SIZE`] ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest):
    /// too much asked of it).
    pub(crate) fn to_json(&self) -> Result<Vec<u8>> {
        let Self {
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id,
            auto_remove,
            ports,
            stopped_by_user,
            restart_count,
            boot_id,
            next_start,
            restart_delay_ms,
        } = self.clone();
        let json = serde_json::to_vec_pretty(&V3 {
            version: VERSION,
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id,
            auto_remove,
            ports,
            stopped_by_user,
            restart_count,
            boot_id,
            next_start,
            restart_delay_ms,
        })
        .context("the container's record")?;
        if json.len() as u64 > MAX_SIZE {
            bail!(
                InvalidRequest,
                "the container's record would take {} bytes: at most {MAX_SIZE}",
                json.len()
            );
        }
        Ok(json)
    }

    /// Reads a record of a version this Shipyard knows. Opened without
    /// following a symlink or waiting on a FIFO, and read up to
    /// [`MAX_SIZE`].
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Storage`](crate::ErrorKind::Storage): no version, a
    /// newer one, not a record, or it couldn't be read.
    pub(crate) fn read(path: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut bytes = Vec::new();
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .and_then(|file| file.take(MAX_SIZE + 1).read_to_end(&mut bytes))
            .with_context(|| format!("reading {}", path.display()))
            .map_err(|err| err.io_as(crate::ErrorKind::Storage))?;
        Self::parse(&bytes).with_context(|| path.display().to_string())
    }

    /// A record from `bytes` (see [`read`](Self::read)).
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Versioned {
            version: Option<serde_json::Value>,
        }
        if bytes.len() as u64 > MAX_SIZE {
            bail!(
                Storage,
                "a container's record of more than {MAX_SIZE} bytes"
            );
        }
        let version = serde_json::from_slice::<Versioned>(bytes)
            .context("not a container's record")?
            .version
            .and_then(|version| version.as_u64());
        match version {
            Some(1) => return Self::parse_v1(bytes),
            Some(2) => return Self::parse_v2(bytes),
            Some(3) => {}
            Some(found) if found > u64::from(VERSION) => bail!(
                Storage,
                "a container's record has version {found}, which this Shipyard doesn't know \
                 (it reads version {VERSION}): a newer Shipyard wrote it"
            ),
            _ => bail!(
                Storage,
                "not a container's record: it has no version Shipyard wrote"
            ),
        }
        let V3 {
            version: _,
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id,
            auto_remove,
            ports,
            stopped_by_user,
            restart_count,
            boot_id,
            next_start,
            restart_delay_ms,
        } = serde_json::from_slice(bytes).context("a container's record")?;
        Ok(Self {
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id,
            auto_remove,
            ports,
            stopped_by_user,
            restart_count,
            boot_id,
            next_start,
            restart_delay_ms,
        })
    }

    /// A version 2 record (9.4a's): no restart policy (`no`), never stopped
    /// by its user nor restarted, its boot unknown.
    fn parse_v2(bytes: &[u8]) -> Result<Self> {
        let V2 {
            _version: _,
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id,
            auto_remove,
            ports,
        } = serde_json::from_slice(bytes).context("a container's record (version 2)")?;
        Ok(Self {
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id,
            auto_remove,
            ports,
            stopped_by_user: false,
            restart_count: 0,
            boot_id: None,
            next_start: None,
            restart_delay_ms: 0,
        })
    }

    /// A version 1 record (9.2b's): no image ID (a start resolves its
    /// image's reference again), no `--rm`, no ports recorded.
    fn parse_v1(bytes: &[u8]) -> Result<Self> {
        let V1 {
            _version: _,
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
        } = serde_json::from_slice(bytes).context("a container's record (version 1)")?;
        Ok(Self {
            id,
            serial,
            name,
            created,
            config,
            labels,
            started,
            finished,
            exit,
            image_id: None,
            auto_remove: false,
            ports: Vec::new(),
            stopped_by_user: false,
            restart_count: 0,
            boot_id: None,
            next_start: None,
            restart_delay_ms: 0,
        })
    }
}
