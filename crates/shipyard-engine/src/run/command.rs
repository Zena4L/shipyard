//! The command a run runs: who runs it, with what environment, where.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use shipyard_runtime::isolation::Isolation;
use shipyard_runtime::process::{Env, Rlimit, User};

use super::RunRequest;
use crate::error::{Context as _, Error, ErrorKind, Result, bail};
use crate::report::{Warning, WarningKind};

/// Adds an image's `Env` `entries` to `env`, as they are: one without a
/// value (`NAME`, not `NAME=value`) is skipped with a warning, and never
/// looked up on the host, as a `--env NAME` is (the image would read the
/// caller's secrets).
pub(super) fn add_image_env(
    env: &mut Env,
    entries: &[String],
    warnings: &mut Vec<Warning>,
) -> Result<()> {
    for entry in entries {
        if entry.contains('=') {
            // The image's fault, not the request's.
            env.push_entry(OsStr::new(entry))
                .map_err(|error| Error::wrap(ErrorKind::Image, error))
                .context("the image's Env")?;
        } else {
            warnings.push(Warning {
                kind: WarningKind::ImageEnvIgnored,
                message: format!("the image's Env entry `{entry}` has no value; ignored"),
            });
        }
    }
    Ok(())
}

/// `entry` as a `KEY=VALUE` (an `-e`): as given if it has an `=`; a bare
/// `KEY` with the caller's value, from `caller_env` (never this process's
/// environment), or `None` if the caller has none (skipped: Docker's
/// behavior). Shared by `run` and `exec`.
///
/// Error messages name the variable but never echo the value, which may be a
/// secret.
///
/// # Errors
///
/// [`crate::ErrorKind::InvalidRequest`]: a bare
/// name that's empty or has spaces (checked first: it would otherwise just
/// be "not found" and silently dropped).
pub(crate) fn resolved_env(
    entry: &OsStr,
    caller_env: &[(OsString, OsString)],
) -> Result<Option<OsString>> {
    let bytes = entry.as_bytes();
    if bytes.contains(&b'=') {
        return Ok(Some(entry.to_owned()));
    }
    if bytes.is_empty() {
        bail!(InvalidRequest, "invalid --env: the variable name is empty");
    }
    // Not named in the message: without `=`, a typo like `TOKEN secret`
    // would put the secret in the "name".
    if bytes.iter().any(u8::is_ascii_whitespace) {
        bail!(
            InvalidRequest,
            "invalid --env: names can't contain spaces (did you mean KEY=VALUE?)"
        );
    }
    // The caller's value, as the caller gave it: never this process's own.
    let Some(value) = caller_env
        .iter()
        .find(|(name, _)| name.as_os_str() == entry)
        .map(|(_, value)| value)
    else {
        // Not named here either: a value passed without `KEY=` (`-e "$TOKEN"`)
        // looks exactly like a name that isn't set.
        tracing::debug!("a bare --env name has no value from the caller; skipped");
        return Ok(None);
    };
    let mut set = entry.to_owned();
    set.push("=");
    set.push(value);
    Ok(Some(set))
}

/// Adds `entry` to `env`, resolved as [`resolved_env`] resolves it.
pub(super) fn add_env(
    env: &mut Env,
    entry: &OsStr,
    caller_env: &[(OsString, OsString)],
) -> Result<()> {
    let Some(set) = resolved_env(entry, caller_env)? else {
        return Ok(());
    };
    let key = OsStr::from_bytes(
        set.as_bytes()
            .split(|&b| b == b'=')
            .next()
            .unwrap_or_default(),
    );
    env.push_entry(&set)
        .with_context(|| format!("invalid --env \"{}\"", key.to_string_lossy()))
}

/// Removes repeated groups, keeping the first occurrence of each.
pub(super) fn dedup(groups: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    groups
        .into_iter()
        .filter(|group| seen.insert(group.clone()))
        .collect()
}

/// A later `--ulimit` for the same resource replaces an earlier one.
pub(super) fn last_wins(limits: Vec<Rlimit>) -> Vec<Rlimit> {
    let mut merged: Vec<Rlimit> = Vec::new();
    for limit in limits {
        match merged.iter_mut().find(|m| m.kind == limit.kind) {
            Some(existing) => *existing = limit,
            None => merged.push(limit),
        }
    }
    merged
}

/// Parses `USER` or `USER:GROUP`, each a name or a number.
///
/// # Errors
///
/// Why it's neither, for the caller to show.
pub fn parse_user(s: &str) -> Result<(String, Option<String>), String> {
    match s.split_once(':') {
        Some((user, group)) => Ok((parse_name_or_id(user)?, Some(parse_name_or_id(group)?))),
        None => Ok((parse_name_or_id(s)?, None)),
    }
}

/// A user or group: a number (checked as `parse_id` does), or a name to
/// look up in the container's `/etc/passwd` or `/etc/group`.
///
/// # Errors
///
/// Why it's neither, for the caller to show.
pub fn parse_name_or_id(s: &str) -> Result<String, String> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return parse_id(s).map(|_| s.to_owned());
    }
    // A sign or a space: a malformed number, more likely than a name.
    let malformed = s.is_empty()
        || s.starts_with(['-', '+'])
        || s.contains(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '/' | '\0'));
    if malformed {
        return Err(format!("\"{s}\" is neither a name nor a number"));
    }
    Ok(s.to_owned())
}

/// A strict numeric user or group ID: digits only, and never `u32::MAX`, which
/// the kernel reads as "leave unchanged".
fn parse_id(s: &str) -> Result<u32, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("\"{s}\" is not a valid numeric ID"));
    }
    match s.parse::<u32>() {
        Ok(u32::MAX) => Err(format!(
            "{} is reserved: the kernel reads it as \"leave unchanged\"",
            u32::MAX
        )),
        Ok(id) => Ok(id),
        Err(_) => Err(format!("\"{s}\" is too large for an ID")),
    }
}

impl RunRequest {
    /// The user to run as: an explicit `--user` wins; else the image's `User`
    /// (`name`, `uid`, or `uid:gid`); else root inside an isolated run, or the
    /// caller without isolation.
    pub(super) fn resolve_user(
        &self,
        isolation: Option<&Isolation>,
        image: Option<&shipyard_image::RuntimeConfig>,
    ) -> Result<User> {
        let spec = match &self.user {
            Some((name, group)) => Some((name.clone(), group.clone())),
            None => match image.and_then(|config| config.user.as_deref()) {
                Some(user) => {
                    // The image's own `User`: its fault, not the request's.
                    Some(parse_user(user).map_err(|e| Error::new(ErrorKind::Image, e))?)
                }
                None => None,
            },
        };
        let mut user = match spec {
            Some((name, group)) => {
                let mut user = User::named(name, group);
                user.names.memberships = isolation.is_some();
                user
            }
            None if isolation.is_some() => User::new(0, 0),
            None => User::current(),
        };
        user.names.additional_groups = dedup(self.group_add.clone());
        Ok(user)
    }

    /// The command to run, as Docker makes it: the entrypoint (the image's,
    /// or `--entrypoint`'s; none for `--entrypoint ""`), then the command
    /// given, or else the image's `Cmd` (unless `--entrypoint` replaced its
    /// entrypoint: then that goes too).
    pub(super) fn resolve_command(
        &self,
        image: Option<&shipyard_image::RuntimeConfig>,
    ) -> Result<Vec<OsString>> {
        let (entrypoint, arguments) = self.entrypoint_and_cmd(image);
        let command: Vec<OsString> = entrypoint.into_iter().chain(arguments).collect();
        if command.is_empty() {
            bail!(InvalidRequest, "the image sets no command, so give one");
        }
        Ok(command)
    }

    /// The entrypoint and the command apart, as
    /// [`resolve_command`](Self::resolve_command) joins them and `inspect`
    /// shows them (F101: one rule, so a view never differs from what a
    /// start runs).
    pub(crate) fn entrypoint_and_cmd(
        &self,
        image: Option<&shipyard_image::RuntimeConfig>,
    ) -> (Vec<OsString>, Vec<OsString>) {
        let Some(config) = image else {
            // Its own alone (a run without an image takes no entrypoint; a
            // view whose image config is gone shows what it was given).
            let entrypoint = self
                .entrypoint
                .iter()
                .filter(|given| !given.is_empty())
                .cloned()
                .collect();
            return (entrypoint, self.command.clone());
        };
        let owned = |list: &[String]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let (entrypoint, default) = match &self.entrypoint {
            // As Docker: no entrypoint, and the image's `Cmd` if nothing's
            // given.
            Some(given) if given.is_empty() => (Vec::new(), owned(&config.cmd)),
            Some(given) => (vec![given.clone()], Vec::new()),
            None => (owned(&config.entrypoint), owned(&config.cmd)),
        };
        let arguments = if self.command.is_empty() {
            default
        } else {
            self.command.clone()
        };
        (entrypoint, arguments)
    }

    /// The variables a start gives it, as `inspect` shows them: its image's
    /// (those with a value), then its own (which win), in order. No default
    /// `PATH` (Docker's `inspect` shows none either). Its own are a
    /// record's: resolved, `KEY=VALUE` each.
    pub(crate) fn shown_env(&self, image: Option<&shipyard_image::RuntimeConfig>) -> Vec<String> {
        let mut env = Env::new();
        let image = image.map_or(&[][..], |config| &config.env);
        for entry in image.iter().filter(|entry| entry.contains('=')) {
            let _ = env.push_entry(OsStr::new(entry));
        }
        for entry in &self.env {
            let _ = env.push_entry(entry);
        }
        env.iter()
            .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
            .collect()
    }

    /// The working directory: `--workdir`, else the image's `WorkingDir`,
    /// else `/`.
    pub(super) fn resolve_workdir(&self, image: Option<&shipyard_image::RuntimeConfig>) -> PathBuf {
        self.workdir
            .clone()
            .or_else(|| {
                image
                    .and_then(|config| config.working_dir.as_deref())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}
