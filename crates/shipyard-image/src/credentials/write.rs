//! A sign-in kept where `docker login` keeps it, and removed as `docker
//! logout` removes it (step 12.4, D12.4-1): Docker and Shipyard share one.
//!
//! The registry's helper (`credHelpers`, else `credsStore`) is asked to
//! `store` (the secret on its standard input alone) or `erase`; with none,
//! the credential goes in the config's `auths` itself (base64, not
//! encrypted: the caller warns, as Docker's CLI does). Either way, every
//! other `auths` key naming the same registry goes (`https://ghcr.io` and
//! `ghcr.io` are one registry: an old plain-text entry never outlives a
//! sign-in through a helper, or a sign-out).
//!
//! **The file.** Read whole and bounded, as JSON: a file that isn't is
//! refused, never overwritten. Every key Shipyard doesn't own is kept as it
//! was. Written whole or not at all (`shipyard_sys::fs::write_atomically`:
//! a temporary, synced, renamed), through a symlink to its target (a
//! dotfiles link stays a link), with the old file's mode and owner (`sudo
//! shipyard login` with the user's `HOME` doesn't hand the user's config to
//! root), `0600` for a new one in a new `0700` directory. A temporary a
//! killed write left (it may hold a secret) is removed by the next write.

use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value};

use super::{
    Answer, Credential, CredentialError, Credentials, config_bytes, real_path, registry_key,
    server_url, where_invalid,
};

/// How old a temporary of the config's must be to be taken for one a
/// killed write left.
const LEFT_BEHIND: Duration = Duration::from_mins(10);

/// Where a credential was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// By the helper `docker-credential-<name>`.
    Helper(String),
    /// In the config file itself, base64: readable by anyone who can read
    /// the file.
    File(PathBuf),
}

/// What a sign-out did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// Whether there was a sign-in to remove.
    pub removed: bool,
    /// Why the helper couldn't erase its own, if it couldn't (the file's
    /// were still removed): a warning, as Docker's CLI says it.
    pub helper_failed: Option<String>,
}

impl Credentials {
    /// Keeps `credential` for `registry` (as a user names it: `ghcr.io`,
    /// `https://index.docker.io/v1/`) where Docker's config says: its
    /// helper, else the file.
    ///
    /// # Errors
    ///
    /// No config place (no `HOME`); a config that can't be read or isn't
    /// Docker's; a helper that fails; the file not written; a registry's
    /// own token, which isn't something a sign-in keeps.
    pub fn keep(&self, registry: &str, credential: &Credential) -> Result<Kept, CredentialError> {
        let place = config_to_write(self.config_path()?)?;
        match self.keep_at(&place.path, registry, credential) {
            Ok(Kept::File(_)) => Ok(Kept::File(place.shown.clone())),
            kept => kept.map_err(|err| place.said(err)),
        }
    }

    /// [`keep`](Self::keep), at the config file `path` (see [`Place`]).
    fn keep_at(
        &self,
        path: &Path,
        registry: &str,
        credential: &Credential,
    ) -> Result<Kept, CredentialError> {
        let path = path.to_owned();
        let mut config = read_value(&path)?.unwrap_or_else(|| Value::Object(Map::new()));
        let registry = registry_key(registry);
        let server = server_url(&registry);
        let helper = helper_for(&config, &registry);
        let (username, secret) = match credential {
            Credential::Basic { username, password } => (username.as_str(), password.as_str()),
            Credential::IdentityToken(token) => ("<token>", token.as_str()),
            Credential::RegistryToken(_) => {
                return Err(config_error(
                    &path,
                    "a registry's own token isn't kept by a sign-in",
                ));
            }
        };
        let mut entry = Map::new();
        if let Some(helper) = &helper {
            let input =
                serde_json::json!({"ServerURL": server, "Username": username, "Secret": secret});
            self.run_helper(helper, "store", input.to_string().as_bytes())?;
        } else if let Credential::IdentityToken(token) = credential {
            entry.insert("identitytoken".to_owned(), Value::String(token.clone()));
        } else {
            let pair = format!("{username}:{secret}");
            entry.insert(
                "auth".to_owned(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(pair)),
            );
        }
        let auths = auths_of(&mut config, &path)?;
        auths.retain(|key, _| registry_key(key) != registry);
        auths.insert(server, Value::Object(entry));
        write_config(&path, &config)?;
        Ok(match helper {
            Some(helper) => Kept::Helper(helper),
            None => Kept::File(path),
        })
    }

    /// Removes the sign-in for `registry`: the helper's (erased), and every
    /// `auths` key naming it.
    ///
    /// # Errors
    ///
    /// As [`keep`](Self::keep)'s; a helper's failure is said, not an error.
    pub fn forget(&self, registry: &str) -> Result<Forgotten, CredentialError> {
        let place = config_to_write(self.config_path()?)?;
        self.forget_at(&place.path, registry)
            .map_err(|err| place.said(err))
    }

    /// [`forget`](Self::forget), at the config file `path` (see [`Place`]).
    fn forget_at(&self, path: &Path, registry: &str) -> Result<Forgotten, CredentialError> {
        let path = path.to_owned();
        let Some(mut config) = read_value(&path)? else {
            return Ok(Forgotten::default());
        };
        let registry = registry_key(registry);
        let mut forgotten = Forgotten::default();
        if let Some(helper) = helper_for(&config, &registry) {
            match self.run_helper(&helper, "erase", server_url(&registry).as_bytes()) {
                Ok(Answer::Done(_)) => forgotten.removed = true,
                Ok(Answer::NotFound) => {}
                Err(err) => forgotten.helper_failed = Some(err.to_string()),
            }
        }
        if let Some(auths) = config.get_mut("auths").and_then(Value::as_object_mut) {
            let before = auths.len();
            auths.retain(|key, _| registry_key(key) != registry);
            if auths.len() != before {
                forgotten.removed = true;
                write_config(&path, &config)?;
            }
        }
        Ok(forgotten)
    }

    /// The config file's place.
    fn config_path(&self) -> Result<&Path, CredentialError> {
        self.config
            .as_deref()
            .ok_or_else(|| CredentialError::Config {
                path: "~/.docker/config.json".to_owned(),
                reason: "there's no place for it (neither DOCKER_CONFIG nor HOME is set)"
                    .to_owned(),
            })
    }
}

/// Where a sign-in reads and writes its config.
struct Place {
    /// The path its reads and writes go through.
    path: PathBuf,
    /// The path said to its user (the real file's).
    shown: PathBuf,
    /// As root on Linux, its directory, open: `path` is through it
    /// (`/proc/self/fd/N/NAME`), so a directory swapped for a link
    /// meanwhile is never written through (F162).
    _dir: Option<std::fs::File>,
}

impl Place {
    /// `err`, naming the file as its user knows it.
    fn said(&self, err: CredentialError) -> CredentialError {
        let io = self.path.display().to_string();
        let shown = self.shown.display().to_string();
        let fix = |path: String| if path == io { shown.clone() } else { path };
        match err {
            CredentialError::Read { path, source } => CredentialError::Read {
                path: fix(path),
                source,
            },
            CredentialError::Config { path, reason } => CredentialError::Config {
                path: fix(path),
                reason,
            },
            CredentialError::Write { path, source } => CredentialError::Write {
                path: fix(path),
                source,
            },
            other => other,
        }
    }
}

/// The file a sign-in reads and writes for the config at `path`: what it
/// links to, if it's a link. As root, a link (the file's, or its
/// directory's) that leads to what its owner doesn't own is refused: root
/// running with another user's `HOME` (`sudo -E`) would otherwise rewrite,
/// or make, a file of root's (or anyone's) wherever that user's link points
/// (F162). Docker's CLI follows such a link; nothing else changes.
fn config_to_write(path: &Path) -> Result<Place, CredentialError> {
    let real = real_path(path);
    if !nix::unistd::geteuid().is_root() {
        return Ok(Place {
            path: real.clone(),
            shown: real,
            _dir: None,
        });
    }
    refuse_a_foreign_link(path, &real)?;
    pinned(path, real)
}

/// As root on Linux, [`config_to_write`]'s place through its directory,
/// opened once without following a last link and owned by whom the config
/// is (the link's owner, else the directory entry's; root, for one made
/// here): checked on the open directory, so a swap after the check (the
/// directory renamed away, a link put in its place) can't redirect the
/// write.
#[cfg(target_os = "linux")]
fn pinned(path: &Path, real: PathBuf) -> Result<Place, CredentialError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let refused = || CredentialError::ForeignLink {
        path: path.display().to_string(),
    };
    let write_error = |source: std::io::Error| CredentialError::Write {
        path: real.display().to_string(),
        source,
    };
    let (Some(dir), Some(name)) = (real.parent(), real.file_name()) else {
        return Ok(Place {
            path: real.clone(),
            shown: real,
            _dir: None,
        });
    };
    let lstat_owner = |place: &Path| std::fs::symlink_metadata(place).ok().map(|meta| meta.uid());
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    let named_dir = path.parent().unwrap_or(dir);
    let mut expected = if is_link {
        lstat_owner(path)
    } else {
        lstat_owner(named_dir)
    };
    let open = |at: &Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(at)
    };
    let opened = match std::fs::canonicalize(dir) {
        Ok(canonical) => open(&canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Made here, alone (never through a link: `mkdir` refuses what's
            // there), so root's.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(write_error)?;
            expected = Some(0);
            open(dir)
        }
        Err(error) => Err(error),
    };
    let dir_file = opened.map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            refused()
        } else {
            write_error(error)
        }
    })?;
    let owner = dir_file.metadata().map_err(write_error)?.uid();
    if expected.is_some_and(|expected| expected != owner) {
        return Err(refused());
    }
    let through = Path::new("/proc/self/fd")
        .join(std::os::fd::AsRawFd::as_raw_fd(&dir_file).to_string())
        .join(name);
    Ok(Place {
        path: through,
        shown: real,
        _dir: Some(dir_file),
    })
}

/// Elsewhere (no `/proc`), [`config_to_write`]'s place by its path alone.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps, reason = "the Linux one's signature")]
fn pinned(_path: &Path, real: PathBuf) -> Result<Place, CredentialError> {
    Ok(Place {
        path: real.clone(),
        shown: real,
        _dir: None,
    })
}

/// [`config_to_write`]'s rule, for root: `path`'s own link, else its
/// directory's, and what it leads to must have one owner.
fn refuse_a_foreign_link(path: &Path, real: &Path) -> Result<(), CredentialError> {
    let link_owner = |place: &Path| {
        std::fs::symlink_metadata(place)
            .ok()
            .filter(|meta| meta.file_type().is_symlink())
            .map(|meta| meta.uid())
    };
    let owner_of = |place: &Path| std::fs::metadata(place).ok().map(|meta| meta.uid());
    let (owner, target) = if let Some(owner) = link_owner(path) {
        // The file it leads to, or (none yet) the directory it'd be made in.
        let target = owner_of(real).or_else(|| real.parent().and_then(owner_of));
        (owner, target)
    } else if let Some(dir) = path.parent()
        && let Some(owner) = link_owner(dir)
    {
        (owner, owner_of(dir))
    } else {
        return Ok(());
    };
    if target.is_some_and(|target| target != owner) {
        return Err(CredentialError::ForeignLink {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

fn config_error(path: &Path, reason: &str) -> CredentialError {
    CredentialError::Config {
        path: path.display().to_string(),
        reason: reason.to_owned(),
    }
}

/// The config at `path` as a JSON object; `None` if there's none.
fn read_value(path: &Path) -> Result<Option<Value>, CredentialError> {
    let Some(bytes) = config_bytes(path)? else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Value::Object(Map::new())));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value @ Value::Object(_)) => Ok(Some(value)),
        Ok(_) => Err(config_error(path, "not a JSON object")),
        Err(err) => Err(where_invalid(path, &err)),
    }
}

/// The helper for `registry` the config names: its own, else the store.
fn helper_for(config: &Value, registry: &str) -> Option<String> {
    config
        .get("credHelpers")
        .and_then(Value::as_object)
        .and_then(|helpers| {
            helpers
                .iter()
                .find(|(key, _)| registry_key(key) == registry)
                .and_then(|(_, helper)| helper.as_str())
        })
        .or_else(|| config.get("credsStore").and_then(Value::as_str))
        .filter(|helper| !helper.is_empty())
        .map(str::to_owned)
}

/// The config's `auths`, made if it has none (or `null`).
fn auths_of<'c>(
    config: &'c mut Value,
    path: &Path,
) -> Result<&'c mut Map<String, Value>, CredentialError> {
    let Value::Object(object) = config else {
        return Err(config_error(path, "not a JSON object"));
    };
    let auths = object
        .entry("auths")
        .or_insert_with(|| Value::Object(Map::new()));
    if auths.is_null() {
        *auths = Value::Object(Map::new());
    }
    auths
        .as_object_mut()
        .ok_or_else(|| config_error(path, "its `auths` isn't an object"))
}

/// Writes `config` to `path` whole, keeping the old file's mode and owner.
fn write_config(path: &Path, config: &Value) -> Result<(), CredentialError> {
    let write_error = |source: std::io::Error| CredentialError::Write {
        path: path.display().to_string(),
        source,
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(write_error)?;
    }
    sweep_left_behind(dir, path);
    let old = std::fs::metadata(path).ok();
    let mode = old.as_ref().map_or(0o600, |old| old.mode() & 0o7777);
    let owner = old.as_ref().map(|old| (old.uid(), old.gid()));
    let mut bytes = serde_json::to_vec_pretty(config)
        .map_err(std::io::Error::other)
        .map_err(write_error)?;
    bytes.push(b'\n');
    let prepare = |file: &std::fs::File| -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        // Root keeps a user's file the user's (`sudo shipyard login` with
        // the user's `HOME`); anyone else writes it as their own, as Docker's
        // CLI does (a chown it may not make isn't a failed sign-in).
        if let Some((uid, gid)) = owner
            && nix::unistd::geteuid().is_root()
            && (uid, gid) != (0, 0)
        {
            std::os::unix::fs::fchown(file, Some(uid), Some(gid))?;
        }
        Ok(())
    };
    shipyard_sys::fs::write_atomically(
        path,
        &bytes,
        &shipyard_sys::fs::Write::new(mode).prepared_by(&prepare),
    )
    .map_err(|err| write_error(err.into()))
}

/// Removes the temporaries of `path` (`.NAME.PID.N.tmp`) older than
/// [`LEFT_BEHIND`]: what a killed write left, a secret perhaps in it.
fn sweep_left_behind(dir: &Path, path: &Path) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!(".{name}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > LEFT_BEHIND);
        let temporary = Path::new(file_name).extension() == Some("tmp".as_ref());
        if file_name.starts_with(&prefix) && temporary && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests;
