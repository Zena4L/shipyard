//! Registry credentials, read where Docker keeps them: `docker login`
//! signs in for Shipyard too.
//!
//! Docker's `config.json` (`$DOCKER_CONFIG/config.json`, or
//! `~/.docker/config.json`) says, per registry, where its credentials are:
//!
//! - `credHelpers`: a registry's own helper program;
//! - `credsStore`: the helper for every other registry (the macOS
//!   keychain's `osxkeychain`, Docker Desktop's `desktop`, `pass`, …);
//! - `auths`: credentials written into the file itself (`auth` is
//!   base64 of `user:password`; `identitytoken` an OAuth refresh token).
//!
//! A helper is a program, `docker-credential-<name>`, found on `PATH`. Asked
//! `get` with the registry's server URL on its standard input, it answers
//! `{"ServerURL": …, "Username": …, "Secret": …}` on its standard output,
//! or fails, saying why there (`credentials not found in native keychain`).
//! A Username of `<token>` makes the Secret an identity token.
//!
//! **What this never does.** Show a secret: [`Credential`]'s `Debug` hides
//! them, errors never carry them, and a helper's answer is never printed.
//! Run a program the config names by path: a helper's name is plain
//! letters, digits, `.`, `_` and `-`. Wait for a helper forever: it gets
//! [`HELPER_TIMEOUT`] (a keychain may ask the user first), timed with
//! `poll(2)` on its pipes, not a thread, then it's killed.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;

/// How long a credential helper may take to answer.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest config file read.
const MAX_CONFIG: u64 = 1024 * 1024;
/// The most a helper may write, on each of its outputs.
const MAX_HELPER_OUTPUT: usize = 64 * 1024;
/// The longest helper message shown in an error.
const MAX_MESSAGE: usize = 200;
/// Docker Hub's name in a credential store (Docker's legacy index URL).
const DOCKER_HUB_SERVER: &str = "https://index.docker.io/v1/";
/// The names Docker Hub's credentials can be keyed by.
const DOCKER_HUB_KEYS: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];

/// A credential for a registry.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A username and password (or access token used as one).
    Basic {
        /// The username.
        username: String,
        /// The password.
        password: String,
    },
    /// An `OAuth2` refresh token, exchanged at the token realm for an access
    /// token.
    IdentityToken(String),
}

impl std::fmt::Debug for Credential {
    /// The kind, and the username: never a secret.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { username, .. } => formatter
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::IdentityToken(_) => formatter.write_str("IdentityToken(<redacted>)"),
        }
    }
}

/// A credential, and where it came from (for messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundCredential {
    /// The credential.
    pub credential: Credential,
    /// The config file or the helper it came from.
    pub source: String,
}

/// Why credentials couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The config file couldn't be read.
    #[error("reading {path}: {source}")]
    Read {
        /// The config file.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// The config file isn't one.
    #[error("{path} isn't a Docker config file: {reason}")]
    Config {
        /// The config file.
        path: String,
        /// What's wrong with it.
        reason: String,
    },
    /// A credential helper failed, or can't be run.
    #[error("the credential helper docker-credential-{helper}: {reason}")]
    Helper {
        /// The helper's name.
        helper: String,
        /// What went wrong.
        reason: String,
    },
}

/// Where credentials are looked for.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The Docker config file; `None`: no credentials at all.
    config: Option<PathBuf>,
    /// Where helpers are, instead of `PATH` (tests).
    helper_dir: Option<PathBuf>,
    /// How long a helper may take.
    helper_timeout: Duration,
}

impl Credentials {
    /// Docker's: `$DOCKER_CONFIG/config.json`, else
    /// `$HOME/.docker/config.json`.
    #[must_use]
    pub fn docker() -> Self {
        let dir = std::env::var_os("DOCKER_CONFIG")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".docker"))
            });
        Self {
            config: dir.map(|dir| dir.join("config.json")),
            ..Self::none()
        }
    }

    /// None: every pull is anonymous.
    #[must_use]
    pub fn none() -> Self {
        Self {
            config: None,
            helper_dir: None,
            helper_timeout: HELPER_TIMEOUT,
        }
    }

    /// The ones in the Docker config file at `config`.
    #[must_use]
    pub fn at(config: impl Into<PathBuf>) -> Self {
        Self {
            config: Some(config.into()),
            ..Self::none()
        }
    }

    /// Helpers looked for in `dir`, not on `PATH`.
    #[must_use]
    pub fn with_helper_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.helper_dir = Some(dir.into());
        self
    }

    /// Helpers given `timeout` to answer.
    #[must_use]
    pub fn with_helper_timeout(mut self, timeout: Duration) -> Self {
        self.helper_timeout = timeout;
        self
    }

    /// The credential for `registry` (as an image reference names it:
    /// `docker.io` for Docker Hub), in Docker's order: the registry's own
    /// helper, else the default store, else the config's `auths`. `None`:
    /// pull anonymously.
    ///
    /// # Errors
    ///
    /// A config file that can't be read or parsed, or a helper that fails
    /// (other than finding nothing).
    pub fn lookup(&self, registry: &str) -> Result<Option<FoundCredential>, CredentialError> {
        let Some(path) = &self.config else {
            return Ok(None);
        };
        let Some(config) = read_config(path)? else {
            return Ok(None);
        };
        let registry = registry_key(registry);
        let helper = config
            .cred_helpers
            .iter()
            .find(|(key, _)| registry_key(key) == registry)
            .map(|(_, helper)| helper)
            .or(config.creds_store.as_ref())
            .filter(|helper| !helper.is_empty());
        if let Some(helper) = helper
            && let Some(credential) = self.ask_helper(helper, &server_url(&registry))?
        {
            return Ok(Some(FoundCredential {
                credential,
                source: format!("docker-credential-{helper}"),
            }));
        }
        let entry = config
            .auths
            .iter()
            .find(|(key, _)| registry_key(key) == registry)
            .map(|(_, entry)| entry);
        Ok(entry
            .map(|entry| entry.credential(path))
            .transpose()?
            .flatten()
            .map(|credential| FoundCredential {
                credential,
                source: path.display().to_string(),
            }))
    }

    /// Runs the helper `name` for `server`: its credential, or `None` if it
    /// has none.
    fn ask_helper(&self, name: &str, server: &str) -> Result<Option<Credential>, CredentialError> {
        let failed = |reason: String| CredentialError::Helper {
            helper: name.to_owned(),
            reason,
        };
        if !valid_helper_name(name) {
            return Err(failed(
                "not a helper name (letters, digits, `.`, `_` and `-` only)".to_owned(),
            ));
        }
        let program = format!("docker-credential-{name}");
        let program = match &self.helper_dir {
            Some(dir) => dir.join(program),
            None => PathBuf::from(program),
        };
        let spawn = || {
            Command::new(&program)
                .arg("get")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // A group of its own: a script's children go with it, if
                // it has to be killed.
                .process_group(0)
                .spawn()
        };
        // ETXTBSY: a helper just installed, whose writer's descriptor a
        // process forking at that moment still holds. Gone in a moment.
        let mut spawned = spawn();
        for _ in 0..3 {
            match &spawned {
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(50));
                    spawned = spawn();
                }
                _ => break,
            }
        }
        let mut child = spawned.map_err(|err| {
            failed(if err.kind() == std::io::ErrorKind::NotFound {
                "not installed (it isn't on PATH)".to_owned()
            } else {
                format!("can't be started: {err}")
            })
        })?;
        // Short: well under a pipe's buffer, so this can't block. A helper
        // that exits without reading it is no reason to fail here.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(server.as_bytes());
        }
        let (stdout, stderr, status) = collect(&mut child, self.helper_timeout).map_err(failed)?;
        if status.success() {
            return parse_answer(&stdout).map_err(failed);
        }
        // A failing helper says why on stdout (Docker's helpers do) or
        // stderr. Never anything that looks like an answer: one that printed
        // the credential and then failed must not show it.
        let shown =
            |line: &String| !line.starts_with('{') && !line.to_ascii_lowercase().contains("secret");
        let message = first_line(&stdout)
            .filter(shown)
            .or_else(|| first_line(&stderr).filter(shown));
        if message
            .as_deref()
            .is_some_and(|message| message.contains("credentials not found"))
        {
            return Ok(None);
        }
        Err(failed(
            message.unwrap_or_else(|| format!("it failed ({status})")),
        ))
    }
}

/// The parts of Docker's `config.json` read here.
///
/// No `Debug`: it holds secrets.
#[derive(Default, serde::Deserialize)]
struct DockerConfig {
    #[serde(default, deserialize_with = "null_is_empty")]
    auths: BTreeMap<String, AuthEntry>,
    #[serde(rename = "credsStore", default)]
    creds_store: Option<String>,
    #[serde(rename = "credHelpers", default, deserialize_with = "null_is_empty")]
    cred_helpers: BTreeMap<String, String>,
}

/// One registry's entry in `auths`.
///
/// No `Debug`: it holds secrets.
#[derive(Default, serde::Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    identitytoken: Option<String>,
}

impl AuthEntry {
    /// Its credential, if it holds one (a `credsStore` leaves `{}` here).
    fn credential(&self, path: &Path) -> Result<Option<Credential>, CredentialError> {
        if let Some(token) = self
            .identitytoken
            .as_ref()
            .filter(|token| !token.is_empty())
        {
            return Ok(Some(Credential::IdentityToken(token.clone())));
        }
        let Some(auth) = self.auth.as_ref().filter(|auth| !auth.is_empty()) else {
            return Ok(None);
        };
        let bad = || CredentialError::Config {
            path: path.display().to_string(),
            reason: "an `auth` that isn't base64 of `user:password`".to_owned(),
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth.trim())
            .map_err(|_| bad())?;
        let decoded = String::from_utf8(decoded).map_err(|_| bad())?;
        let (username, password) = decoded.split_once(':').ok_or_else(bad)?;
        Ok(Some(Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }))
    }
}

/// A map that may be `null` in the file.
fn null_is_empty<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: serde::Deserialize<'de>,
{
    use serde::Deserialize as _;
    Ok(Option::<BTreeMap<String, V>>::deserialize(deserializer)?.unwrap_or_default())
}

/// The config at `path`; `None` if there's none.
fn read_config(path: &Path) -> Result<Option<DockerConfig>, CredentialError> {
    let read_error = |source| CredentialError::Read {
        path: path.display().to_string(),
        source,
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(read_error(err)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    let invalid = |reason: String| CredentialError::Config {
        path: path.display().to_string(),
        reason,
    };
    if bytes.len() as u64 > MAX_CONFIG {
        return Err(invalid(format!("larger than {MAX_CONFIG} bytes")));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        // Where, never what: serde's message can quote a value, and the
        // values here are secrets.
        .map_err(|err| {
            invalid(format!(
                "not valid JSON at line {}, column {}",
                err.line(),
                err.column()
            ))
        })
}

/// The registry a config key names: no scheme, no path, lowercase; Docker
/// Hub's names as `docker.io`.
fn registry_key(key: &str) -> String {
    let key = key.trim().to_ascii_lowercase();
    let key = key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
        .unwrap_or(&key);
    let host = key.split('/').next().unwrap_or(key);
    if DOCKER_HUB_KEYS.contains(&host) {
        "docker.io".to_owned()
    } else {
        host.to_owned()
    }
}

/// What a helper is asked for `registry`: Docker Hub's legacy URL, or the
/// registry's own name.
fn server_url(registry: &str) -> String {
    if registry == "docker.io" {
        DOCKER_HUB_SERVER.to_owned()
    } else {
        registry.to_owned()
    }
}

/// Whether `name` can be a helper's name (and never a path).
fn valid_helper_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A helper's successful answer.
fn parse_answer(stdout: &[u8]) -> Result<Option<Credential>, String> {
    #[derive(serde::Deserialize)]
    struct Answer {
        #[serde(rename = "Username", default)]
        username: String,
        #[serde(rename = "Secret", default)]
        secret: String,
    }
    // Never the serde error: it can quote the input, the secret included.
    let answer: Answer = serde_json::from_slice(stdout)
        .map_err(|_| "its answer isn't the credential JSON helpers give".to_owned())?;
    Ok(match (answer.username.as_str(), answer.secret) {
        (_, secret) if secret.is_empty() => None,
        ("<token>", secret) => Some(Credential::IdentityToken(secret)),
        (username, password) => Some(Credential::Basic {
            username: username.to_owned(),
            password,
        }),
    })
}

/// The first non-empty line of `bytes`, cut to [`MAX_MESSAGE`].
fn first_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(MAX_MESSAGE).collect())
}

/// Everything `child` writes on stdout and stderr until it ends, and how it
/// ended; killed (its whole process group) and reaped if that takes over
/// `timeout`, or it writes too much. Reaped whatever happens.
fn collect(
    child: &mut Child,
    timeout: Duration,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus), String> {
    let deadline = Instant::now() + timeout;
    let late = || format!("didn't answer within {timeout:?}");
    let outputs = read_both(child, deadline, late);
    let ended = outputs.and_then(|(stdout, stderr)| {
        // The pipes closed: it's ending (or closed them and goes on: then
        // it's waited for, within the same deadline).
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok((stdout, stderr, status)),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => return Err(late()),
                Err(err) => return Err(format!("waiting for it: {err}")),
            }
        }
    });
    if ended.is_err() {
        kill(child);
    }
    ended
}

/// Kills `child`'s process group (it leads one) and reaps `child`.
fn kill(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads `child`'s stdout and stderr to their ends, both at once (`poll`),
/// by `deadline`.
fn read_both(
    child: &mut Child,
    deadline: Instant,
    late: impl Fn() -> String,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("its output isn't a pipe".to_owned());
    };
    let mut outputs = [Vec::new(), Vec::new()];
    let mut open = [true, true];
    let mut buffer = [0u8; 4096];
    while open.contains(&true) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(late());
        }
        let timeout = PollTimeout::try_from(left).unwrap_or(PollTimeout::MAX);
        // A closed pipe is polled for nothing (no events asked for).
        let wanted = |is_open: bool| {
            if is_open {
                PollFlags::POLLIN
            } else {
                PollFlags::empty()
            }
        };
        let mut fds = [
            PollFd::new(stdout.as_fd(), wanted(open[0])),
            PollFd::new(stderr.as_fd(), wanted(open[1])),
        ];
        match poll(&mut fds, timeout) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(errno) => return Err(format!("waiting for its answer: {errno}")),
        }
        let ready = fds.map(|fd| fd.revents().is_some_and(|events| !events.is_empty()));
        if open[0] && ready[0] {
            open[0] = read_some(&mut stdout, &mut buffer, &mut outputs[0])?;
        }
        if open[1] && ready[1] {
            open[1] = read_some(&mut stderr, &mut buffer, &mut outputs[1])?;
        }
    }
    let [out, err] = outputs;
    Ok((out, err))
}

/// One read from a ready pipe into `into`: whether it's still open.
fn read_some(
    pipe: &mut impl std::io::Read,
    buffer: &mut [u8],
    into: &mut Vec<u8>,
) -> Result<bool, String> {
    let read = pipe
        .read(buffer)
        .map_err(|err| format!("reading its answer: {err}"))?;
    if into.len() + read > MAX_HELPER_OUTPUT {
        return Err(format!("it wrote more than {MAX_HELPER_OUTPUT} bytes"));
    }
    into.extend_from_slice(&buffer[..read]);
    Ok(read > 0)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use super::*;

    /// A scratch directory with a config and helper scripts.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("shipyard-creds-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// Credentials from a config holding `json`, helpers from here.
        fn config(&self, json: &str) -> Credentials {
            let path = self.0.join("config.json");
            std::fs::write(&path, json).unwrap();
            Credentials::at(path).with_helper_dir(&self.0)
        }

        /// A helper `docker-credential-<name>` running `script` (sh).
        fn helper(&self, name: &str, script: &str) {
            use std::os::unix::fs::PermissionsExt as _;
            let path = self.0.join(format!("docker-credential-{name}"));
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn basic(username: &str, password: &str) -> Credential {
        Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    fn found(credentials: &Credentials, registry: &str) -> Option<Credential> {
        credentials
            .lookup(registry)
            .unwrap()
            .map(|found| found.credential)
    }

    #[test]
    fn auths_are_read_whatever_the_key_looks_like() {
        let scratch = Scratch::new();
        // `me:pw` and `ci:token`.
        let credentials = scratch.config(
            r#"{"auths":{
                "https://index.docker.io/v1/":{"auth":"bWU6cHc="},
                "https://GHCR.io":{"auth":"Y2k6dG9rZW4="},
                "acr.example":{"identitytoken":"refresh"},
                "empty.example":{}
            }}"#,
        );
        assert_eq!(found(&credentials, "docker.io"), Some(basic("me", "pw")));
        assert_eq!(found(&credentials, "ghcr.io"), Some(basic("ci", "token")));
        assert_eq!(
            found(&credentials, "acr.example"),
            Some(Credential::IdentityToken("refresh".to_owned()))
        );
        assert_eq!(found(&credentials, "empty.example"), None);
        assert_eq!(found(&credentials, "quay.io"), None);
    }

    #[test]
    fn no_config_is_no_credentials_and_a_broken_one_is_an_error() {
        let scratch = Scratch::new();
        let missing = Credentials::at(scratch.0.join("nothing.json"));
        assert_eq!(found(&missing, "docker.io"), None);
        assert!(Credentials::none().lookup("docker.io").unwrap().is_none());
        let broken = scratch.config("{not json");
        assert!(matches!(
            broken.lookup("docker.io"),
            Err(CredentialError::Config { .. })
        ));
        let bad_auth = scratch.config(r#"{"auths":{"docker.io":{"auth":"%%%"}}}"#);
        let err = bad_auth.lookup("docker.io").unwrap_err();
        assert!(err.to_string().contains("base64"), "{err}");
        let nulls = scratch.config(r#"{"auths":null,"credHelpers":null,"credsStore":null}"#);
        assert_eq!(found(&nulls, "docker.io"), None);
    }

    #[test]
    fn a_registrys_helper_comes_before_the_store_and_the_store_before_auths() {
        let scratch = Scratch::new();
        scratch.helper(
            "own",
            r#"read server; echo "{\"Username\":\"own\",\"Secret\":\"$server\"}""#,
        );
        scratch.helper("store", r#"echo '{"Username":"store","Secret":"s"}'"#);
        let credentials = scratch.config(
            r#"{"credHelpers":{"ghcr.io":"own"},"credsStore":"store",
                "auths":{"ghcr.io":{"auth":"bWU6cHc="},"quay.io":{"auth":"bWU6cHc="}}}"#,
        );
        assert_eq!(
            found(&credentials, "ghcr.io"),
            Some(basic("own", "ghcr.io"))
        );
        assert_eq!(found(&credentials, "quay.io"), Some(basic("store", "s")));
        // Docker Hub is asked by its legacy URL.
        let hub = scratch.config(r#"{"credsStore":"own"}"#);
        assert_eq!(
            found(&hub, "docker.io"),
            Some(basic("own", "https://index.docker.io/v1/"))
        );
    }

    #[test]
    fn what_helpers_answer() {
        let scratch = Scratch::new();
        scratch.helper(
            "token",
            r#"echo '{"Username":"<token>","Secret":"refresh"}'"#,
        );
        scratch.helper(
            "none",
            "echo 'credentials not found in native keychain'; exit 1",
        );
        scratch.helper(
            "broken",
            "echo 'the keychain is locked'; echo 'stderr detail' >&2; exit 1",
        );
        scratch.helper("garbage", "echo 'Secret: hunter2'");
        scratch.helper(
            "leaky",
            r#"echo '{"Username":"me","Secret":"hunter2"}'; echo 'crashed' >&2; exit 1"#,
        );
        let ask = |helper: &str| {
            scratch
                .config(&format!(
                    r#"{{"credsStore":"{helper}","auths":{{"r.example":{{"auth":"bWU6cHc="}}}}}}"#
                ))
                .lookup("r.example")
        };
        assert_eq!(
            ask("token").unwrap().unwrap().credential,
            Credential::IdentityToken("refresh".to_owned())
        );
        // Nothing in the store: the config's own entry still counts.
        let fallback = ask("none").unwrap().unwrap();
        assert_eq!(fallback.credential, basic("me", "pw"));
        assert!(fallback.source.ends_with("config.json"));
        let err = ask("broken").unwrap_err().to_string();
        assert!(err.contains("the keychain is locked"), "{err}");
        let err = ask("garbage").unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "a secret in an error: {err}");
        let err = ask("leaky").unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "a secret in an error: {err}");
        assert!(err.contains("crashed"), "{err}");
        let err = ask("missing").unwrap_err().to_string();
        assert!(err.contains("isn't on PATH"), "{err}");
    }

    #[test]
    fn a_helper_named_like_a_path_is_never_run() {
        let scratch = Scratch::new();
        let credentials = scratch.config(r#"{"credsStore":"../../bin/sh"}"#);
        let err = credentials.lookup("docker.io").unwrap_err().to_string();
        assert!(err.contains("not a helper name"), "{err}");
    }

    #[test]
    fn a_helper_that_hangs_or_floods_is_stopped() {
        let scratch = Scratch::new();
        scratch.helper("slow", "exec sleep 30");
        scratch.helper("flood", "exec yes");
        let started = Instant::now();
        let err = scratch
            .config(r#"{"credsStore":"slow"}"#)
            .with_helper_timeout(Duration::from_millis(300))
            .lookup("docker.io")
            .unwrap_err()
            .to_string();
        assert!(err.contains("didn't answer"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed, not waited for"
        );
        let err = scratch
            .config(r#"{"credsStore":"flood"}"#)
            .lookup("docker.io")
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than"), "{err}");
    }

    #[test]
    fn debug_hides_the_secrets() {
        let debug = format!(
            "{:?} {:?}",
            basic("me", "hunter2"),
            Credential::IdentityToken("refresh-secret".to_owned())
        );
        assert!(
            !debug.contains("hunter2") && !debug.contains("refresh-secret"),
            "{debug}"
        );
        assert!(debug.contains("me"));
    }
}
