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
    /// Where networks are kept, or why not (a relative
    /// `SHIPYARD_NETWORK_DIR`).
    networks: std::result::Result<PathBuf, String>,
    /// Where the events journal is kept, if given (`SHIPYARD_EVENTS_DIR`,
    /// or why not: a relative one); else beside the containers' records.
    events: Option<std::result::Result<PathBuf, String>>,
    /// The caller's runtime directory (`XDG_RUNTIME_DIR`), if it has one.
    runtime_dir: Option<PathBuf>,
    /// The temporary directory (`TMPDIR`, else `/tmp`).
    temp_dir: PathBuf,
    /// The settings a network's DNS server takes from its environment (for
    /// tests), as the caller's environment gives them.
    dns_env: Vec<(OsString, OsString)>,
    /// Where programs are found (`PATH`): pasta, for a run.
    program_path: Option<OsString>,
    /// Started through `sudo` (`SUDO_USER` set): a user's own session's
    /// fate at logout is theirs, not root's (`doctor`).
    started_by_sudo: bool,
    /// The variables that moved a store from its default place (F44): the
    /// boot unit, which runs with systemd's environment, won't see them.
    store_overrides: Vec<&'static str>,
    /// The Docker API's socket, if `SHIPYARD_API_SOCKET` moves it (for
    /// tests: each its own service); else the runtime root's.
    api_socket: Option<std::result::Result<PathBuf, String>>,
    /// A test build's pause in a create's fill (`SHIPYARD_TEST_FILL_PAUSE`:
    /// where to say it's there), for a test to kill it mid-fill. A release
    /// takes none.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "a fill is Linux's")
    )]
    fill_pause: Option<PathBuf>,
    /// A test build's pause in a start, its container made and not yet
    /// started (`SHIPYARD_TEST_START_PAUSE`: where to say it's there), for a
    /// test to end its init in F119's window. A release takes none.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "a start is Linux's")
    )]
    start_pause: Option<PathBuf>,
}

/// The variables that move a store (for [`Paths::store_overrides`]).
const STORE_VARIABLES: [&str; 4] = [
    "SHIPYARD_IMAGE_STORE",
    "SHIPYARD_CONTAINER_DIR",
    "SHIPYARD_NETWORK_DIR",
    "SHIPYARD_EVENTS_DIR",
];

/// Which of [`STORE_VARIABLES`] move a store from where it would be
/// without them (one set to its default place moves nothing), and
/// `XDG_DATA_HOME` for anyone but root, where it moves a store none of
/// them placed from `~/.local/share`.
fn store_overrides_from_env() -> Vec<&'static str> {
    let defaults = [
        default_store("images").ok(),
        default_store("containers").ok(),
        Some(PathBuf::from(DEFAULT_NETWORKS)),
        default_store("events").ok(),
    ];
    store_overrides(
        |name| std::env::var_os(name),
        nix::unistd::geteuid().is_root(),
        &defaults,
    )
}

/// [`store_overrides_from_env`]'s choice, given the environment's
/// variables (`value`), whether it's root, and each store's default place
/// in [`STORE_VARIABLES`]' order.
fn store_overrides(
    value: impl Fn(&str) -> Option<OsString>,
    root: bool,
    defaults: &[Option<PathBuf>; 4],
) -> Vec<&'static str> {
    let value = |name: &str| value(name).filter(|value| !value.is_empty());
    let mut overrides: Vec<&'static str> = STORE_VARIABLES
        .into_iter()
        .zip(defaults)
        .filter(|(name, default)| {
            value(name).is_some_and(|set| default.as_deref() != Some(Path::new(&set)))
        })
        .map(|(name, _)| name)
        .collect();
    let placed_by_data_home =
        !overrides.contains(&STORE_VARIABLES[0]) || !overrides.contains(&STORE_VARIABLES[1]);
    if !root
        && placed_by_data_home
        && let Some(data) = value("XDG_DATA_HOME")
        && value("HOME")
            .is_none_or(|home| Path::new(&home).join(".local/share") != Path::new(&data))
    {
        overrides.push("XDG_DATA_HOME");
    }
    overrides
}

/// Where networks are kept, unless `SHIPYARD_NETWORK_DIR` says.
const DEFAULT_NETWORKS: &str = "/var/lib/shipyard/networks";

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
    /// [`events`](Self::events)). The API's socket: `SHIPYARD_API_SOCKET`
    /// if set (for tests: each its own service), else the runtime root's
    /// (see [`api_socket`](Self::api_socket)).
    ///
    /// Which of these moved a store is kept for
    /// [`store_overrides`](Self::store_overrides) (F44).
    #[must_use]
    pub fn from_env() -> Self {
        let dns_env = dns_env_of(std::env::vars_os(), cfg!(debug_assertions));
        Self {
            image_store: image_store_from_env(),
            containers: containers_from_env(),
            runtime_root: runtime_root_from_env(),
            networks: std::env::var_os("SHIPYARD_NETWORK_DIR")
                .filter(|dir| !dir.is_empty())
                .map_or_else(
                    || Ok(PathBuf::from(DEFAULT_NETWORKS)),
                    |dir| absolute("network store", "SHIPYARD_NETWORK_DIR", PathBuf::from(dir)),
                ),
            events: std::env::var_os("SHIPYARD_EVENTS_DIR")
                .filter(|dir| !dir.is_empty())
                .map(|dir| absolute("events journal", "SHIPYARD_EVENTS_DIR", PathBuf::from(dir))),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            temp_dir: std::env::temp_dir(),
            dns_env,
            program_path: std::env::var_os("PATH"),
            started_by_sudo: std::env::var_os("SUDO_USER").is_some(),
            store_overrides: store_overrides_from_env(),
            api_socket: std::env::var_os("SHIPYARD_API_SOCKET")
                .filter(|path| !path.is_empty())
                .map(|path| absolute("API socket", "SHIPYARD_API_SOCKET", PathBuf::from(path))),
            fill_pause: std::env::var_os("SHIPYARD_TEST_FILL_PAUSE")
                .filter(|path| cfg!(debug_assertions) && !path.is_empty())
                .map(PathBuf::from),
            start_pause: std::env::var_os("SHIPYARD_TEST_START_PAUSE")
                .filter(|path| cfg!(debug_assertions) && !path.is_empty())
                .map(PathBuf::from),
        }
    }

    /// A test build's pause in a create's fill, if one was asked for
    /// (`SHIPYARD_TEST_FILL_PAUSE`): where the fill says it's there.
    #[must_use]
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "a fill is Linux's")
    )]
    pub(crate) fn fill_pause(&self) -> Option<&Path> {
        self.fill_pause.as_deref()
    }

    /// A test build's pause in a start (`SHIPYARD_TEST_START_PAUSE`): where
    /// the start says its container's ID, then waits for it to be removed.
    #[must_use]
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "a start is Linux's")
    )]
    pub(crate) fn start_pause(&self) -> Option<&Path> {
        self.start_pause.as_deref()
    }

    /// These places, with networks kept in `dir`.
    #[must_use]
    pub fn with_networks(mut self, dir: impl Into<PathBuf>) -> Self {
        self.networks = Ok(dir.into());
        self
    }

    /// Where networks are kept.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Environment`]: `SHIPYARD_NETWORK_DIR` isn't an absolute
    /// path.
    pub fn networks(&self) -> Result<&Path> {
        self.networks
            .as_deref()
            .map_err(|why| Error::new(ErrorKind::Environment, why.clone()))
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

    /// Whether this was started through `sudo` (`SUDO_USER` set).
    #[must_use]
    pub fn started_by_sudo(&self) -> bool {
        self.started_by_sudo
    }

    /// The environment variables that moved a store from its default place
    /// (`SHIPYARD_IMAGE_STORE`, `SHIPYARD_*_DIR`, a non-root user's
    /// `XDG_DATA_HOME`), as `from_env` found them: what a boot unit, with
    /// systemd's environment, won't see (F44).
    #[must_use]
    pub fn store_overrides(&self) -> &[&'static str] {
        &self.store_overrides
    }

    /// What a network's DNS server is given of the caller's environment.
    #[must_use]
    pub fn dns_env(&self) -> &[(OsString, OsString)] {
        &self.dns_env
    }

    /// These places, with the events journal kept in `dir`.
    #[must_use]
    pub fn with_events(mut self, dir: impl Into<PathBuf>) -> Self {
        self.events = Some(Ok(dir.into()));
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
            return dir
                .clone()
                .map_err(|why| Error::new(ErrorKind::Environment, why));
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

    /// These places, with the container store's place unknown (`why`), as a
    /// wrong environment leaves it.
    #[cfg(test)]
    pub(crate) fn without_containers(mut self, why: &str) -> Self {
        self.containers = Err(why.to_owned());
        self
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

    /// The Docker API's socket, `api.sock` in the [runtime's state
    /// root](Self::runtime_root): `/run/shipyard/api.sock` for root, else
    /// `$XDG_RUNTIME_DIR/shipyard/api.sock` (step 11.2a, D11.2-1);
    /// `SHIPYARD_API_SOCKET` (tests) wins.
    ///
    /// # Errors
    ///
    /// As [`runtime_root`](Self::runtime_root);
    /// [`ErrorKind::Environment`] for a `SHIPYARD_API_SOCKET` that isn't an
    /// absolute path.
    pub fn api_socket(&self) -> Result<PathBuf> {
        match &self.api_socket {
            Some(socket) => socket
                .clone()
                .map_err(|why| Error::new(ErrorKind::Environment, why)),
            None => Ok(self.runtime_root()?.join("api.sock")),
        }
    }

    /// Where the API's exec `exec_id` has its command's PID written once it
    /// runs (F117): `.exec-<id>.pid` in the runtime's state root, the
    /// caller's alone (no container's ID starts with a dot).
    ///
    /// # Errors
    ///
    /// As [`runtime_root`](Self::runtime_root).
    pub fn exec_pid_file(&self, exec_id: &str) -> Result<PathBuf> {
        Ok(self.runtime_root()?.join(format!(".exec-{exec_id}.pid")))
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

/// The DNS server's variables among `variables` (see [`DNS_ENV`]): in a
/// debug build (the tests') only (F61), so a release's DNS server never takes its
/// upstreams or its idle time from whoever started it.
fn dns_env_of(
    variables: impl Iterator<Item = (OsString, OsString)>,
    debug_build: bool,
) -> Vec<(OsString, OsString)> {
    if !debug_build {
        return Vec::new();
    }
    variables
        .filter(|(name, _)| DNS_ENV.iter().any(|kept| name.as_os_str() == *kept))
        .collect()
}

fn image_store_from_env() -> std::result::Result<PathBuf, String> {
    store_from_env("SHIPYARD_IMAGE_STORE", "image store", "images")
}

fn containers_from_env() -> std::result::Result<PathBuf, String> {
    store_from_env("SHIPYARD_CONTAINER_DIR", "container store", "containers")
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
fn store_from_env(variable: &str, what: &str, name: &str) -> std::result::Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
        return absolute(what, variable, PathBuf::from(dir));
    }
    default_store(name).and_then(|dir| absolute(what, "XDG_DATA_HOME or HOME", dir))
}

/// `dir`, the place of `what` (the image store, say) as `placed_by` (the
/// variables) gave it, if it's absolute: a store relative to wherever the
/// caller happens to be would be another in each directory (F61; the XDG
/// spec treats a relative `XDG_DATA_HOME` as invalid too).
fn absolute(what: &str, placed_by: &str, dir: PathBuf) -> std::result::Result<PathBuf, String> {
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(format!(
            "the {what} {} isn't an absolute path (from {placed_by})",
            dir.display()
        ))
    }
}

/// The store `name`'s place in Shipyard's data directory, where it is
/// without its variable: `/var/lib/shipyard` for root,
/// `$XDG_DATA_HOME/shipyard` or `~/.local/share/shipyard` for anyone else.
fn default_store(name: &str) -> std::result::Result<PathBuf, String> {
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

    /// A store variable moves a store unless it names its default place;
    /// `XDG_DATA_HOME` does for anyone but root, unless it's
    /// `~/.local/share` or both stores it places are placed by others.
    #[test]
    fn store_overrides_are_what_moved_a_store() {
        let home = "/home/u/.local/share/shipyard";
        let defaults = [
            Some(PathBuf::from(format!("{home}/images"))),
            Some(PathBuf::from(format!("{home}/containers"))),
            Some(PathBuf::from(DEFAULT_NETWORKS)),
            Some(PathBuf::from(format!("{home}/events"))),
        ];
        let given = |pairs: &[(&str, &str)], root: bool| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            store_overrides(
                |name| {
                    pairs
                        .iter()
                        .find(|(set, _)| set == name)
                        .map(|(_, value)| OsString::from(value))
                },
                root,
                &defaults,
            )
        };
        let none: &[&str] = &[];
        assert_eq!(given(&[("HOME", "/home/u")], false), none);
        assert_eq!(
            given(
                &[
                    ("SHIPYARD_CONTAINER_DIR", "/tmp/c"),
                    ("SHIPYARD_EVENTS_DIR", "")
                ],
                false
            ),
            ["SHIPYARD_CONTAINER_DIR"]
        );
        assert_eq!(
            given(&[("SHIPYARD_NETWORK_DIR", DEFAULT_NETWORKS)], true),
            none,
            "its default place moves nothing"
        );
        let data = [("HOME", "/home/u"), ("XDG_DATA_HOME", "/data")];
        assert_eq!(given(&data, false), ["XDG_DATA_HOME"]);
        assert_eq!(given(&data, true), none, "root's stores aren't its");
        assert_eq!(
            given(
                &[
                    ("HOME", "/home/u"),
                    ("XDG_DATA_HOME", "/home/u/.local/share")
                ],
                false
            ),
            none
        );
        assert_eq!(
            given(
                &[
                    ("XDG_DATA_HOME", "/data"),
                    ("SHIPYARD_IMAGE_STORE", "/i"),
                    ("SHIPYARD_CONTAINER_DIR", "/c"),
                ],
                false
            ),
            ["SHIPYARD_IMAGE_STORE", "SHIPYARD_CONTAINER_DIR"],
            "both stores it places are placed by others"
        );
    }

    #[test]
    fn networks_go_where_they_are_told() {
        let paths = Paths::from_env().with_networks("/elsewhere/networks");
        assert_eq!(paths.networks().unwrap(), Path::new("/elsewhere/networks"));
    }

    /// A store variable must give an absolute path (F61): a relative one is
    /// refused, as an `Environment` error naming it; an absolute one is
    /// taken (the control).
    #[test]
    fn a_relative_store_is_refused() {
        let refused = absolute("events journal", "SHIPYARD_EVENTS_DIR", "rel/events".into());
        assert!(
            refused
                .as_ref()
                .unwrap_err()
                .contains("isn't an absolute path (from SHIPYARD_EVENTS_DIR)"),
            "{refused:?}"
        );
        assert_eq!(
            absolute(
                "events journal",
                "SHIPYARD_EVENTS_DIR",
                "/abs/events".into()
            )
            .unwrap(),
            Path::new("/abs/events")
        );
        let paths = Paths {
            events: Some(refused),
            networks: absolute("network store", "SHIPYARD_NETWORK_DIR", "rel".into()),
            image_store: absolute("image store", "SHIPYARD_IMAGE_STORE", "rel".into()),
            ..Paths::from_env()
        };
        assert_eq!(paths.events().unwrap_err().kind(), ErrorKind::Environment);
        assert_eq!(paths.networks().unwrap_err().kind(), ErrorKind::Environment);
        assert_eq!(
            paths.image_store().unwrap_err().kind(),
            ErrorKind::Environment
        );
    }

    /// The DNS server's test variables are taken in a test build only
    /// (F61): a release collects none; a test build collects them, and
    /// nothing else.
    #[test]
    fn the_dns_test_variables_are_a_test_builds() {
        let given = || {
            [
                (DNS_ENV[0], "192.0.2.53"),
                (DNS_ENV[1], "5"),
                ("PATH", "/bin"),
            ]
            .into_iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        };
        assert!(dns_env_of(given(), false).is_empty(), "a release's");
        let taken = dns_env_of(given(), true);
        assert_eq!(taken.len(), 2, "{taken:?}");
        assert!(taken.iter().all(|(name, _)| name != "PATH"));
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
