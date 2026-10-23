//! Docker's `--tmpfs` and `--mount` (step 11.3e2), for the CLI and the
//! API alike: a tmpfs as a [`TmpfsMount`] (kept beside the record, in
//! `created.json`), a bind or a named volume as a `--volume` string (the
//! record's own field: no format change).

use std::path::{Path, PathBuf};

use shipyard_runtime::isolation::{Mount, MountFlag};

use crate::error::{Error, ErrorKind, Result};

/// The options of `mount(8)` a tmpfs takes as flags: each name, its flag,
/// and whether it sets the flag (`noexec`) or clears it (`exec`).
const FLAGS: [(&str, MountFlag, bool); 8] = [
    ("ro", MountFlag::ReadOnly, true),
    ("rw", MountFlag::ReadOnly, false),
    ("noexec", MountFlag::NoExec, true),
    ("exec", MountFlag::NoExec, false),
    ("nosuid", MountFlag::NoSuid, true),
    ("suid", MountFlag::NoSuid, false),
    ("nodev", MountFlag::NoDev, true),
    ("dev", MountFlag::NoDev, false),
];

/// A tmpfs's own options (its data, `mount(2)`'s last argument), each
/// value's rule: a size (digits, then `k`, `m` or `g`), an octal mode, a
/// number.
const DATA: [(&str, Value); 6] = [
    ("size", Value::Size),
    ("nr_blocks", Value::Size),
    ("nr_inodes", Value::Size),
    ("mode", Value::Octal),
    ("uid", Value::Number),
    ("gid", Value::Number),
];

/// What a tmpfs option's value may be.
#[derive(Debug, Clone, Copy)]
enum Value {
    /// Digits, then `k`, `m` or `g`, or none.
    Size,
    /// Octal digits, at most four.
    Octal,
    /// Digits.
    Number,
}

/// A tmpfs in a container, as Docker's `--tmpfs` gives it: where, and its
/// options as given (`size=64m`, `exec`, `mode=1770`), each checked; at
/// each start a fresh one, empty, `noexec,nosuid,nodev` unless its options
/// say otherwise (Docker's defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpfsMount {
    /// Where it's mounted in the container: absolute.
    destination: PathBuf,
    /// Its options, in order, as given, each checked.
    options: Vec<String>,
}

impl TmpfsMount {
    /// `--tmpfs PATH[:OPTIONS]`'s value: `/run`, `/run:size=64m,exec`.
    ///
    /// # Errors
    ///
    /// A path that isn't absolute, or an option Shipyard doesn't take
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn parse(spec: &str) -> Result<Self> {
        let (destination, options) = spec.split_once(':').unwrap_or((spec, ""));
        Self::new(
            destination,
            options
                .split(',')
                .filter(|option| !option.is_empty())
                .map(str::to_owned),
        )
    }

    /// A tmpfs at `destination` with `options`, each checked.
    ///
    /// # Errors
    ///
    /// As [`TmpfsMount::parse`].
    pub fn new(destination: &str, options: impl IntoIterator<Item = String>) -> Result<Self> {
        let destination = PathBuf::from(destination);
        if !destination.is_absolute() {
            return Err(invalid(format!(
                "a tmpfs's path must be absolute, not {:?}",
                destination.display().to_string()
            )));
        }
        let options: Vec<String> = options.into_iter().collect();
        for option in &options {
            check_option(option)?;
        }
        Ok(Self {
            destination,
            options,
        })
    }

    /// Where it's mounted in the container.
    #[must_use]
    pub fn destination(&self) -> &std::path::Path {
        &self.destination
    }

    /// Its options, as given.
    #[must_use]
    pub fn options(&self) -> &[String] {
        &self.options
    }

    /// Whether it's read-only: an `ro` its last `rw` doesn't undo.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.to_runtime().has(MountFlag::ReadOnly)
    }

    /// The runtime's mount of it: Docker's defaults (`noexec`, `nosuid`,
    /// `nodev`), then its options in order, the last of a pair winning.
    #[must_use]
    pub(crate) fn to_runtime(&self) -> Mount {
        let mut mount = Mount::tmpfs(&self.destination);
        mount.flags = vec![MountFlag::NoExec, MountFlag::NoSuid, MountFlag::NoDev];
        let mut data = Vec::new();
        for option in &self.options {
            if let Some(&(_, flag, on)) = FLAGS.iter().find(|(name, ..)| name == option) {
                mount.flags.retain(|&set| set != flag);
                if on {
                    mount.flags.push(flag);
                }
            } else {
                data.push(option.clone());
            }
        }
        if !data.is_empty() {
            mount.data = Some(data.join(","));
        }
        mount
    }

    /// Its options as Docker's `inspect` shows them (`HostConfig.Tmpfs`):
    /// comma-joined, as given.
    #[must_use]
    pub fn options_text(&self) -> String {
        self.options.join(",")
    }
}

/// What a Docker `--mount` asks for: a bind or a named volume, as the
/// `--volume` string that says the same; or a tmpfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerMount {
    /// `SOURCE:DEST[:ro]`, a `--volume`'s.
    Volume(String),
    /// A tmpfs.
    Tmpfs(TmpfsMount),
}

/// One `--mount`, as Docker's API gives it (`HostConfig.Mounts`): its
/// `type`, `source` and `target`, read-only or not, a tmpfs's size (bytes)
/// and mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MountRequest {
    /// `bind`, `volume` or `tmpfs`.
    pub kind: String,
    /// A bind's host path, or a volume's name.
    pub source: Option<String>,
    /// Where it's mounted in the container.
    pub target: String,
    /// Mounted read-only.
    pub read_only: bool,
    /// A tmpfs's size (`tmpfs-size`): bytes, or with `k`, `m` or `g`.
    pub tmpfs_size: Option<String>,
    /// A tmpfs's mode, octal (`tmpfs-mode`).
    pub tmpfs_mode: Option<String>,
    /// A bind's propagation (`bind-propagation`): only `rprivate` and
    /// `private` taken (a container's mounts never reach the host).
    pub propagation: Option<String>,
    /// No image's files copied into the volume (`volume-nocopy`, the
    /// API's `NoCopy`): its spec gets Docker's `nocopy` mode, a volume's
    /// only.
    pub no_copy: Option<bool>,
    /// A bind without the mounts below its source: refused.
    pub non_recursive: bool,
    /// A bind's missing source made (Docker's API's `CreateMountpoint`):
    /// [`source_to_make`](Self::source_to_make) says which.
    pub create_mountpoint: bool,
}

impl MountRequest {
    /// What no mount of its kind takes, refused.
    fn refuse_what_is_not_taken(&self) -> Result<()> {
        if let Some(propagation) = self
            .propagation
            .as_deref()
            .filter(|propagation| !matches!(*propagation, "" | "rprivate" | "private"))
        {
            return Err(invalid(format!(
                "a bind's propagation {propagation} isn't supported: a container's mounts \
                 never reach the host (rprivate)"
            )));
        }
        if self.no_copy == Some(true) && self.kind != "volume" {
            return Err(invalid(format!(
                "no-copy is a volume's, not a {}'s",
                self.kind
            )));
        }
        if self.non_recursive {
            return Err(invalid("a non-recursive bind isn't supported".to_owned()));
        }
        if self.create_mountpoint && self.kind != "bind" {
            return Err(invalid(format!(
                "making a missing source is a bind's, not a {}'s",
                self.kind
            )));
        }
        if self.target.is_empty() {
            return Err(invalid("a mount needs a target".to_owned()));
        }
        if self.kind != "tmpfs" && (self.tmpfs_size.is_some() || self.tmpfs_mode.is_some()) {
            return Err(invalid(format!(
                "tmpfs-size and tmpfs-mode are a tmpfs's, not a {}'s",
                self.kind
            )));
        }
        Ok(())
    }

    /// Its `--volume` modes, after the `:`: `ro`, `nocopy` (taken off at
    /// the create, which then leaves the place's volume unfilled).
    fn modes(&self) -> String {
        let modes: Vec<&str> = [
            self.read_only.then_some("ro"),
            (self.no_copy == Some(true)).then_some("nocopy"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if modes.is_empty() {
            String::new()
        } else {
            format!(":{}", modes.join(","))
        }
    }

    /// The bind's source to make before the create, if it asks for one
    /// (`create_mountpoint`; [`run::make_bind_source`](crate::run::make_bind_source)).
    #[must_use]
    pub fn source_to_make(&self) -> Option<&Path> {
        self.source
            .as_deref()
            .filter(|_| self.create_mountpoint && self.kind == "bind")
            .map(Path::new)
    }

    /// A mount of `kind` at `target`, nothing else set.
    #[must_use]
    pub fn new(kind: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            target: target.into(),
            ..Self::default()
        }
    }

    /// What it asks for.
    ///
    /// # Errors
    ///
    /// What isn't taken (another propagation, a non-recursive bind,
    /// no-copy or a source to make on a mount of another kind); another
    /// type; no target; a
    /// bind without an absolute source; an anonymous volume (no source)
    /// whose target isn't absolute; a source or
    /// target holding `:` (a `--volume` string couldn't say it); a tmpfs's
    /// size or mode that isn't one; a source for a tmpfs
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn resolve(&self) -> Result<DockerMount> {
        self.refuse_what_is_not_taken()?;
        match self.kind.as_str() {
            "tmpfs" => {
                if self
                    .source
                    .as_deref()
                    .is_some_and(|source| !source.is_empty())
                {
                    return Err(invalid("a tmpfs has no source".to_owned()));
                }
                let mut options = Vec::new();
                if self.read_only {
                    options.push("ro".to_owned());
                }
                if let Some(size) = &self.tmpfs_size {
                    options.push(format!("size={size}"));
                }
                if let Some(mode) = &self.tmpfs_mode {
                    options.push(format!("mode={mode}"));
                }
                Ok(DockerMount::Tmpfs(TmpfsMount::new(&self.target, options)?))
            }
            "bind" | "volume" => {
                let modes = self.modes();
                let Some(source) = self.source.as_deref().filter(|source| !source.is_empty())
                else {
                    if self.kind == "bind" {
                        return Err(invalid("a bind needs a source: the host path".to_owned()));
                    }
                    // A volume without a name: an anonymous one, made at
                    // the create (as `-v /path`).
                    if !self.target.starts_with('/') || self.target.contains(':') {
                        return Err(invalid(format!(
                            "an anonymous volume's target must be an absolute path without \
                             ':', not {:?}",
                            self.target
                        )));
                    }
                    return Ok(DockerMount::Volume(format!("{}{modes}", self.target)));
                };
                let named = shipyard_storage::volume::is_named_volume(source);
                if self.kind == "bind" && !source.starts_with('/') {
                    return Err(invalid(format!(
                        "a bind's source must be an absolute path, not {source:?}"
                    )));
                }
                if self.kind == "volume" && !named {
                    return Err(invalid(format!(
                        "a volume's source is its name, not a path ({source:?}): use \
                         type=bind for a host path"
                    )));
                }
                if source.contains(':') || self.target.contains(':') {
                    return Err(invalid(format!(
                        "a mount's source and target can't hold ':' ({source}, {})",
                        self.target
                    )));
                }
                Ok(DockerMount::Volume(format!(
                    "{source}:{}{modes}",
                    self.target
                )))
            }
            other => Err(invalid(format!(
                "a mount's type is bind, volume or tmpfs, not {other:?}"
            ))),
        }
    }
}

/// Docker's `--mount` value, `KEY=VALUE` pairs separated by commas: `type`
/// (`volume` by default), `source`/`src`, `target`/`destination`/`dst`,
/// `readonly`/`ro` (alone, or `=true|false|1|0`), `tmpfs-size`,
/// `tmpfs-mode`; `bind-propagation` of `rprivate` or `private` (the
/// container's mounts never reach the host anyway), `volume-nocopy` (the
/// image's files never copied into the volume) and `consistency`
/// (Docker ignores it on Linux) taken; anything else refused.
///
/// # Errors
///
/// A key not taken, or as [`MountRequest::resolve`]
/// ([`ErrorKind::InvalidRequest`]).
pub fn parse_mount(spec: &str) -> Result<DockerMount> {
    let mut asked = MountRequest::new("volume", "");
    for field in spec.split(',').filter(|field| !field.is_empty()) {
        let (key, value) = match field.split_once('=') {
            Some((key, value)) => (key.trim().to_ascii_lowercase(), Some(value.trim())),
            None => (field.trim().to_ascii_lowercase(), None),
        };
        let given = |what: &str| {
            value
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("--mount's {what} needs a value")))
        };
        match key.as_str() {
            "type" => asked.kind = given("type")?.to_ascii_lowercase(),
            "source" | "src" => asked.source = Some(given("source")?),
            "target" | "destination" | "dst" => asked.target = given("target")?,
            "readonly" | "ro" => {
                asked.read_only = match value {
                    None => true,
                    Some(text) => go_bool(text).ok_or_else(|| {
                        invalid(format!("--mount's {key} is true or false, not {text:?}"))
                    })?,
                };
            }
            "tmpfs-size" => asked.tmpfs_size = Some(given("tmpfs-size")?),
            "tmpfs-mode" => asked.tmpfs_mode = Some(given("tmpfs-mode")?),
            "bind-propagation" => asked.propagation = Some(given("bind-propagation")?),
            "volume-nocopy" => {
                asked.no_copy = Some(match value {
                    None => true,
                    Some(text) => go_bool(text).ok_or_else(|| {
                        invalid(format!("--mount's {key} is true or false, not {text:?}"))
                    })?,
                });
            }
            "consistency" => {}
            other => {
                return Err(invalid(format!("--mount's {other} isn't supported")));
            }
        }
    }
    asked.resolve()
}

/// Go's `strconv.ParseBool`: what Docker's `--mount` takes for a boolean.
fn go_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Whether `option` is one a tmpfs takes, its value checked.
fn check_option(option: &str) -> Result<()> {
    if FLAGS.iter().any(|(name, ..)| *name == option) {
        return Ok(());
    }
    let refused = || {
        invalid(format!(
            "the tmpfs option {option:?} isn't supported (ro, rw, exec, noexec, suid, nosuid, \
             dev, nodev, size, nr_inodes, mode, uid, gid)"
        ))
    };
    let (key, value) = option.split_once('=').ok_or_else(refused)?;
    let (_, rule) = DATA
        .iter()
        .find(|(name, _)| *name == key)
        .ok_or_else(refused)?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let fits = match rule {
        Value::Number => digits(value),
        Value::Octal => {
            // Leading zeros taken (`01777`), as the kernel takes them.
            let significant = value.trim_start_matches('0');
            !value.is_empty()
                && significant.len() <= 4
                && value.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
        }
        Value::Size => {
            let number = value
                .strip_suffix(['k', 'm', 'g', 'K', 'M', 'G'])
                .unwrap_or(value);
            digits(number)
        }
    };
    if fits {
        Ok(())
    } else {
        Err(invalid(format!(
            "the tmpfs option {option:?} has a value that isn't one"
        )))
    }
}

/// An [`ErrorKind::InvalidRequest`] saying `message`.
fn invalid(message: String) -> Error {
    Error::new(ErrorKind::InvalidRequest, message)
}

#[cfg(test)]
mod tests;
