//! Where the engine keeps things.
//!
//! [`Paths::from_env`] is the one place that reads the environment for
//! them, so every operation finds the same places, and a caller (a test, an
//! API with a store of its own) can give others.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

/// The engine's places on disk.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The image store (and the volumes in it), or why there's none.
    image_store: std::result::Result<PathBuf, String>,
    /// Where containers' records are kept, or why there's nowhere.
    containers: std::result::Result<PathBuf, String>,
    /// The runtime's state root (containers' state directories), or why
    /// there's none.
    runtime_root: std::result::Result<PathBuf, String>,
    /// Where networks are kept.
    networks: PathBuf,
    /// Where the events journal is kept, if given (`SHIPYARD_EVENTS_DIR`);
    /// else beside the containers' records.
    events: Option<PathBuf>,
    /// The caller's runtime directory (`XDG_RUNTIME_DIR`), if it has one.
    runtime_dir: Option<PathBuf>,
    /// The temporary directory (`TMPDIR`, else `/tmp`).
    temp_dir: PathBuf,
    /// The settings a network's DNS server takes from its environment (for
    /// tests), as the caller's environment gives them.
    dns_env: Vec<(OsString, OsString)>,
    /// Where programs are found (`PATH`): pasta, for a run.
    program_path: Option<OsString>,
}

impl Paths {
    /// The places the environment gives. The image store: an explicit
    /// `SHIPYARD_IMAGE_STORE` wins (useful where the default is on an
    /// overlay filesystem, which can't hold an overlay upperdir); otherwise
    /// `/var/lib/shipyard/images` for root, or
    /// `$XDG_DATA_HOME`/`~/.local/share/shipyard/images` for anyone else.
    /// Shared with the volume store.
    ///
    /// Containers' records: `SHIPYARD_CONTAINER_DIR` if set (for tests),
    /// else `containers` beside the image store's default place
    /// (`/var/lib/shipyard/containers` for root,
    /// `$XDG_DATA_HOME`/`~/.local/share/shipyard/containers` for anyone
    /// else).
    ///
    /// Networks: `SHIPYARD_NETWORK_DIR` if set (for tests), else
    /// `/var/lib/shipyard/networks`. The runtime directory is
    /// `XDG_RUNTIME_DIR`; the temporary one `TMPDIR`'s (else `/tmp`). A
    /// network's DNS server gets `SHIPYARD_DNS_UPSTREAM` and
    /// `SHIPYARD_DNS_IDLE`, for tests.
    ///
    /// The events journal: `SHIPYARD_EVENTS_DIR` if set, else beside the
    /// containers' records (`/var/lib/shipyard/events` for root: see
    /// [`events`](Self::events)).
    #[must_use]
    pub fn from_env() -> Self {
        let dns_env = std::env::vars_os()
            .filter(|(name, _)| DNS_ENV.iter().any(|kept| name.as_os_str() == *kept))
            .collect();
        Self {
            image_store: image_store_from_env(),
            containers: containers_from_env(),
            runtime_root: runtime_root_from_env(),
            networks: std::env::var_os("SHIPYARD_NETWORK_DIR")
                .filter(|dir| !dir.is_empty())
                .map_or_else(
                    || PathBuf::from("/var/lib/shipyard/networks"),
                    PathBuf::from,
                ),
            events: std::env::var_os("SHIPYARD_EVENTS_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            temp_dir: std::env::temp_dir(),
            dns_env,
            program_path: std::env::var_os("PATH"),
        }
    }

    /// These places, with networks kept in `dir`.
    #[must_use]
    pub fn with_networks(mut self, dir: impl Into<PathBuf>) -> Self {
        self.networks = dir.into();
        self
    }

    /// Where networks are kept.
    #[must_use]
    pub fn networks(&self) -> &Path {
        &self.networks
    }

    /// The caller's runtime directory, if it has one.
    #[must_use]
    pub fn runtime_dir(&self) -> Option<&Path> {
        self.runtime_dir.as_deref()
    }

    /// The temporary directory.
    #[must_use]
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    /// Where programs are found (`PATH`), if anywhere.
    #[must_use]
    pub fn program_path(&self) -> Option<&std::ffi::OsStr> {
        self.program_path.as_deref()
    }

    /// What a network's DNS server is given of the caller's environment.
    #[must_use]
    pub fn dns_env(&self) -> &[(OsString, OsString)] {
        &self.dns_env
    }

    /// These places, with the events journal kept in `dir`.
    #[must_use]
    pub fn with_events(mut self, dir: impl Into<PathBuf>) -> Self {
        self.events = Some(dir.into());
        self
    }

    /// Where the events journal is kept: as given, else beside the
    /// containers' records, so a store moved elsewhere (a test's) takes its
    /// journal with it: `events` beside a directory named `containers` (the
    /// default stores'), `NAME.events` beside any other (`/tmp/c`'s is
    /// `/tmp/c.events`, never one every store in `/tmp` would share).
    ///
    /// # Errors
    ///
    /// As [`containers`](Self::containers), when none was given.
    pub fn events(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.events {
            return Ok(dir.clone());
        }
        let containers = self.containers()?;
        Ok(match containers.file_name() {
            Some(name) if name == "containers" => containers.with_file_name("events"),
            Some(name) => {
                let mut events = name.to_owned();
                events.push(".events");
                containers.with_file_name(events)
            }
            None => containers.join("events"),
        })
    }

    /// These places, with containers' records kept in `dir`.
    #[must_use]
    pub fn with_containers(mut self, dir: impl Into<PathBuf>) -> Self {
        self.containers = Ok(dir.into());
        self
    }

    /// Where containers' records are kept.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Environment`]: there's nowhere to keep them (not root,
    /// and neither `XDG_DATA_HOME` nor `HOME` is set).
    pub fn containers(&self) -> Result<&Path> {
        self.containers
            .as_deref()
            .map_err(|why| Error::new(ErrorKind::Environment, why.clone()))
    }

    /// These places, with the runtime's state root at `dir`.
    #[must_use]
    pub fn with_runtime_root(mut self, dir: impl Into<PathBuf>) -> Self {
        self.runtime_root = Ok(dir.into());
        self
    }

    /// The runtime's state root: `/run/shipyard` for root, else
    /// `$XDG_RUNTIME_DIR/shipyard` (as `shipyard-runtime` chooses it).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Environment`]: there's none (`XDG_RUNTIME_DIR` unset,
    /// relative, or someone else's), or containers need Linux here.
    pub fn runtime_root(&self) -> Result<&Path> {
        self.runtime_root
            .as_deref()
            .map_err(|why| Error::new(ErrorKind::Environment, why.clone()))
    }

    /// These places, with the image store at `dir`.
    #[must_use]
    pub fn with_image_store(mut self, dir: impl Into<PathBuf>) -> Self {
        self.image_store = Ok(dir.into());
        self
    }

    /// The image store's directory.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Environment`]: there's nowhere to keep images (not root,
    /// and neither `XDG_DATA_HOME` nor `HOME` is set).
    pub fn image_store(&self) -> Result<&Path> {
        self.image_store
            .as_deref()
            .map_err(|why| Error::new(ErrorKind::Environment, why.clone()))
    }
}

/// The variables a network's DNS server reads (see `network::dns`).
const DNS_ENV: [&str; 2] = [
    crate::network::DNS_UPSTREAM_ENV,
    crate::network::DNS_IDLE_ENV,
];

fn image_store_from_env() -> std::result::Result<PathBuf, String> {
    store_from_env("SHIPYARD_IMAGE_STORE", "images")
}

fn containers_from_env() -> std::result::Result<PathBuf, String> {
    // Absolute only: a store relative to wherever the caller happens to
    // be would hold other containers in each directory.
    store_from_env("SHIPYARD_CONTAINER_DIR", "containers").and_then(|dir| {
        if dir.is_absolute() {
            Ok(dir)
        } else {
            Err(format!(
                "the container store {} isn't an absolute path (from SHIPYARD_CONTAINER_DIR, \
                 XDG_DATA_HOME or HOME)",
                dir.display()
            ))
        }
    })
}

fn runtime_root_from_env() -> std::result::Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt as _;
    if cfg!(not(target_os = "linux")) {
        return Err(shipyard_runtime::container::ContainerError::LinuxOnly.to_string());
    }
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let owner = runtime_dir
        .as_ref()
        .and_then(|dir| std::fs::metadata(dir).ok())
        .filter(std::fs::Metadata::is_dir)
        .map(|metadata| metadata.uid());
    let me = nix::unistd::geteuid();
    shipyard_runtime::container::choose_root(
        me.is_root(),
        runtime_dir.as_deref(),
        owner,
        me.as_raw(),
    )
    .map_err(|err| match err {
        // In `shipyard`'s words (the runtime's names its own `--root`).
        shipyard_runtime::container::ContainerError::NoStateRoot(why) => {
            format!("no directory for containers' state: {why} (a login session sets it)")
        }
        other => other.to_string(),
    })
}

/// A store's place: `variable` if it's set, else `name` in Shipyard's data
/// directory (`/var/lib/shipyard` for root, `$XDG_DATA_HOME/shipyard` or
/// `~/.local/share/shipyard` for anyone else).
fn store_from_env(variable: &str, name: &str) -> std::result::Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if nix::unistd::geteuid().is_root() {
        return Ok(Path::new("/var/lib/shipyard").join(name));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(data).join("shipyard").join(name));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            format!("neither XDG_DATA_HOME nor HOME is set, so there's nowhere to keep {name}")
        })?;
    Ok(PathBuf::from(home).join(".local/share/shipyard").join(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    #[test]
    fn a_given_store_wins() {
        let paths = Paths::from_env().with_image_store("/somewhere");
        assert_eq!(paths.image_store().unwrap(), Path::new("/somewhere"));
    }

    /// The journal beside the containers' store: `events` for a store
    /// named `containers`, else its own name's; as given, when it is.
    #[test]
    fn the_journal_goes_with_its_store() {
        let at = |dir: &str| Paths::from_env().with_containers(dir).events().unwrap();
        let home = "/home/u/.local/share/shipyard";
        assert_eq!(
            at(&format!("{home}/containers")),
            Path::new(home).join("events")
        );
        assert_eq!(at("/tmp/c"), Path::new("/tmp/c.events"));
        assert_eq!(
            Paths::from_env()
                .with_containers("/tmp/c")
                .with_events("/elsewhere")
                .events()
                .unwrap(),
            Path::new("/elsewhere")
        );
    }

    #[test]
    fn networks_go_where_they_are_told() {
        let paths = Paths::from_env().with_networks("/elsewhere/networks");
        assert_eq!(paths.networks(), Path::new("/elsewhere/networks"));
    }

    #[test]
    fn nowhere_to_keep_images_is_an_environment_error() {
        let paths = Paths {
            image_store: Err("no place".to_owned()),
            ..Paths::from_env()
        };
        let error = paths.image_store().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Environment);
        assert_eq!(error.to_string(), "no place");
    }
}
