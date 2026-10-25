//! Docker's filters, for its lists (step 11.2, moved here in 11.3c so the
//! API's `filters` and the CLI's `-f KEY=VALUE` read them alike, H1).
//!
//! What's asked for is a [`Given`]: a list of values per key, from Docker's
//! JSON (`{"label": ["app=web"]}`, or the older `{"label": {"app=web":
//! true}}`) or from `KEY=VALUE` pairs. Each list's filters then check
//! their keys: any key not read is refused, never ignored (a filter
//! ignored lists what wasn't asked for). An object passes if, for every
//! key, one of its values matches (every one, for `label`: Docker's rule).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{ANONYMOUS_VOLUME, ContainerSummary, ImageSummary, NetworkResource};
use crate::error::{Error, ErrorKind, Result};

/// The states `status` may name (Docker's).
const STATES: [&str; 7] = [
    "created",
    "restarting",
    "running",
    "removing",
    "paused",
    "exited",
    "dead",
];

/// What `health` may name (Docker's, [`ContainerSummary::health`]'s).
const HEALTHS: [&str; 4] = ["starting", "healthy", "unhealthy", "none"];

/// What a `name` filter can't hold: Docker reads it as a regular
/// expression, and Shipyard reads only text, `^`, `$` and `.` (any one
/// character), so the rest is refused, never matched as text.
const NAME_PATTERN: [char; 11] = ['*', '+', '?', '[', ']', '(', ')', '{', '}', '|', '\\'];

/// The filters asked for: each key's values, in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Given(BTreeMap<String, Vec<String>>);

/// One key's values, in either of Docker's forms.
#[derive(Deserialize)]
#[serde(untagged)]
enum Values {
    List(Vec<String>),
    Set(BTreeMap<String, bool>),
}

impl Given {
    /// Docker's filters JSON `text` (none if it's absent or empty).
    ///
    /// # Errors
    ///
    /// Text that isn't Docker's JSON ([`ErrorKind::InvalidRequest`]).
    pub fn from_json(text: Option<&str>) -> Result<Self> {
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return Ok(Self::default());
        };
        let given: BTreeMap<String, Values> = serde_json::from_str(text)
            .map_err(|error| invalid(format!("invalid filters: {error}")))?;
        Ok(Self(
            given
                .into_iter()
                .map(|(key, values)| {
                    let values = match values {
                        Values::List(values) => values,
                        Values::Set(set) => set
                            .into_iter()
                            .filter_map(|(value, on)| on.then_some(value))
                            .collect(),
                    };
                    (key, values)
                })
                .collect(),
        ))
    }

    /// Whether no filter was given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.values().all(Vec::is_empty)
    }

    /// `KEY=VALUE` pairs, as the CLI's `-f` takes them (a key repeated
    /// adds a value).
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut given: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (key, value) in pairs {
            given.entry(key).or_default().push(value);
        }
        Self(given)
    }

    /// Every value as `KEY=VALUE`, in order (for a filter read elsewhere:
    /// the events').
    #[must_use]
    pub fn flattened(self) -> Vec<String> {
        self.0
            .into_iter()
            .flat_map(|(key, values)| {
                values
                    .into_iter()
                    .map(move |value| format!("{key}={value}"))
            })
            .collect()
    }

    /// Each key's values, checked by `check` (an error refuses them all).
    fn checked(
        self,
        mut check: impl FnMut(&str, &[String]) -> Result<()>,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        for (key, values) in &self.0 {
            check(key, values)?;
        }
        Ok(self.0)
    }
}

/// A refusal of what was asked for.
fn invalid(message: String) -> Error {
    Error::new(ErrorKind::InvalidRequest, message)
}

/// A key no list of this kind reads.
fn unsupported(key: &str, supported: &str) -> Error {
    invalid(format!(
        "the filter '{key}' isn't supported yet ({supported})"
    ))
}

/// The first of `values` `ok` refuses, as Docker words it.
fn each_value(key: &str, values: &[String], ok: impl Fn(&str) -> bool) -> Result<()> {
    match values.iter().find(|value| !ok(value)) {
        Some(bad) => Err(invalid(format!("invalid filter '{key}={bad}'"))),
        None => Ok(()),
    }
}

/// A container's list filters: `id`, `name`, `label`, `status`,
/// `ancestor`, `exited`, `health`, `network`, `volume`, `before` and
/// `since`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContainerFilters(BTreeMap<String, Vec<String>>);

/// What a container filter reads beyond its summary.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Facts<'a> {
    /// Its exit code, if it has ended and it's known.
    pub(crate) exit_code: Option<i32>,
    /// The manifest digests the `ancestor`s asked for name in the store
    /// (an image named another way than the container's was).
    pub(crate) ancestors: &'a [String],
}

impl ContainerFilters {
    /// The filters `given` asks for.
    ///
    /// # Errors
    ///
    /// A key not read, or a value that can't be one of its key's
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn parse(given: Given) -> Result<Self> {
        given
            .checked(|key, values| match key {
                "id" | "label" | "ancestor" | "network" | "volume" => Ok(()),
                "name" => each_value(key, values, |value| !value.contains(NAME_PATTERN)),
                "before" | "since" => each_value(key, values, |value| !value.is_empty()),
                "status" => each_value(key, values, |value| STATES.contains(&value)),
                "health" => each_value(key, values, |value| HEALTHS.contains(&value)),
                "exited" => each_value(key, values, |value| value.parse::<i32>().is_ok()),
                _ => Err(unsupported(
                    key,
                    "id, name, label, status, ancestor, exited, health, network, volume, \
                     before, since",
                )),
            })
            .map(Self)
    }

    /// The images `ancestor` names, as asked.
    pub(crate) fn ancestors(&self) -> impl Iterator<Item = &str> {
        self.0
            .get("ancestor")
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// Whether every container is to be looked at, not only those up: a
    /// `status` or an `exited` asked for may name ended ones (Docker's).
    #[must_use]
    pub fn wants_all(&self) -> bool {
        self.0.contains_key("status") || self.0.contains_key("exited")
    }

    /// The containers `before` and `since` name, by key.
    pub(crate) fn bounds(&self) -> impl Iterator<Item = (&str, &str)> {
        ["before", "since"].into_iter().flat_map(|key| {
            self.0
                .get(key)
                .into_iter()
                .flatten()
                .map(move |value| (key, value.as_str()))
        })
    }

    /// Whether `summary`, with its `facts`, passes every key's filter but
    /// `before` and `since` (the caller's: they name other containers).
    pub(crate) fn pass(&self, summary: &ContainerSummary, facts: Facts<'_>) -> bool {
        self.0.iter().all(|(key, values)| {
            values.is_empty()
                || matches!(key.as_str(), "before" | "since")
                || matched(key, values, |value| match key.as_str() {
                    "id" => summary.id.starts_with(value.as_str()),
                    "name" => summary.names.iter().any(|name| name_matches(name, value)),
                    "label" => label_matches(&summary.labels, value),
                    "status" => summary.state == value,
                    "ancestor" => {
                        ancestor_matches(summary, value)
                            || facts.ancestors.contains(&summary.image_id)
                    }
                    "exited" => {
                        summary.state == "exited"
                            && facts.exit_code.map(|code| code.to_string()).as_ref() == Some(value)
                    }
                    "health" => summary.health == value,
                    "network" => {
                        summary
                            .network_settings
                            .networks
                            .iter()
                            .any(|(name, endpoint)| {
                                name == value
                                    || (!endpoint.network_id.is_empty()
                                        && endpoint.network_id.starts_with(value.as_str()))
                            })
                    }
                    "volume" => summary
                        .mounts
                        .iter()
                        .any(|mount| mount.name == *value || mount.destination == *value),
                    _ => false,
                })
        })
    }
}

/// Whether `summary` runs the image `wanted` names: as it was given (a
/// tag left out is `latest`), or by its ID (whole, or a start of it with or
/// without `sha256:`).
fn ancestor_matches(summary: &ContainerSummary, wanted: &str) -> bool {
    let with_latest = |name: &str| {
        let last = name.rsplit('/').next().unwrap_or(name);
        if last.contains(':') || last.contains('@') {
            name.to_owned()
        } else {
            format!("{name}:latest")
        }
    };
    if !summary.image.is_empty() && with_latest(&summary.image) == with_latest(wanted) {
        return true;
    }
    let id = summary
        .image_id
        .strip_prefix("sha256:")
        .unwrap_or(&summary.image_id);
    let wanted = wanted.strip_prefix("sha256:").unwrap_or(wanted);
    !id.is_empty() && wanted.len() >= 4 && id.starts_with(wanted)
}

/// Docker matches a name filter as a regular expression over `/name`:
/// here, what it contains, with `^` and `$` anchoring it and `.` any one
/// character (the rest of a regular expression is refused at parse).
fn name_matches(name: &str, pattern: &str) -> bool {
    let bare = name.strip_prefix('/').unwrap_or(name);
    let (start, rest) = match pattern.strip_prefix('^') {
        Some(rest) => (true, rest),
        None => (false, pattern),
    };
    let (end, wanted) = match rest.strip_suffix('$') {
        Some(wanted) => (true, wanted),
        None => (false, rest),
    };
    let wanted: Vec<char> = wanted.chars().collect();
    let at = |candidate: &[char], from: usize| {
        candidate
            .get(from..from + wanted.len())
            .is_some_and(|part| {
                part.iter()
                    .zip(&wanted)
                    .all(|(have, want)| *want == '.' || have == want)
            })
    };
    // A pattern anchored at the start may name the `/` or not.
    [name, bare].iter().any(|candidate| {
        let candidate: Vec<char> = candidate.chars().collect();
        let Some(last) = candidate.len().checked_sub(wanted.len()) else {
            return false;
        };
        match (start, end) {
            (true, true) => candidate.len() == wanted.len() && at(&candidate, 0),
            (true, false) => at(&candidate, 0),
            (false, true) => at(&candidate, last),
            (false, false) => (0..=last).any(|from| at(&candidate, from)),
        }
    })
}

/// `KEY` (it has the label) or `KEY=VALUE` (it has it, so set).
fn label_matches(labels: &BTreeMap<String, String>, filter: &str) -> bool {
    match filter.split_once('=') {
        Some((key, value)) => labels.get(key).is_some_and(|set| set == value),
        None => labels.contains_key(filter),
    }
}

/// An image list's filters: `reference` (a pattern, `*` and `?` within a
/// path part, over the name with its tag or without), `label`, `dangling`
/// (Shipyard keeps no image without a name: `true` passes none).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImageFilters(BTreeMap<String, Vec<String>>);

impl ImageFilters {
    /// The filters `given` asks for.
    ///
    /// # Errors
    ///
    /// A key not read, a `reference` with a character class, or a
    /// `dangling` that isn't a boolean ([`ErrorKind::InvalidRequest`]).
    pub fn parse(given: Given) -> Result<Self> {
        given
            .checked(|key, values| match key {
                "label" => Ok(()),
                // Go's character classes and escapes aren't read: a
                // pattern with one is refused, never matched as text.
                "reference" => match values.iter().find(|value| value.contains(['[', '\\'])) {
                    Some(bad) => Err(invalid(format!(
                        "invalid filter 'reference={bad}': `[…]` and `\\` aren't supported \
                         (use `*` and `?`)"
                    ))),
                    None => Ok(()),
                },
                "dangling" => each_value(key, values, |value| boolean(value).is_some()),
                _ => Err(unsupported(key, "reference, label, dangling")),
            })
            .map(Self)
    }

    /// Whether `image` passes every key's filter.
    #[must_use]
    pub fn pass(&self, image: &ImageSummary) -> bool {
        let no_labels = BTreeMap::new();
        let labels = image.labels.as_ref().unwrap_or(&no_labels);
        self.passes(&image.repo_tags, &image.repo_digests, labels)
    }

    /// Whether an image of these tags, digests and labels passes.
    fn passes(
        &self,
        repo_tags: &[String],
        repo_digests: &[String],
        labels: &BTreeMap<String, String>,
    ) -> bool {
        self.0.iter().all(|(key, values)| {
            values.is_empty()
                || matched(key, values, |value| match key.as_str() {
                    "reference" => repo_tags.iter().any(|tag| {
                        let name = tag
                            .rsplit_once(':')
                            .filter(|(_, tag)| !tag.contains('/'))
                            .map_or(tag.as_str(), |(name, _)| name);
                        glob(value, tag) || glob(value, name)
                    }),
                    "label" => label_matches(labels, value),
                    // Dangling: no name at all, tag or digest (an image
                    // only a container keeps isn't listed, so: none).
                    "dangling" => {
                        let dangling = repo_tags.is_empty() && repo_digests.is_empty();
                        boolean(value) == Some(dangling)
                    }
                    _ => false,
                })
        })
    }
}

/// A volume list's filters: `name` (what it contains), `label`,
/// `dangling` (no container's record mounts it), `driver` (`local`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VolumeFilters(BTreeMap<String, Vec<String>>);

impl VolumeFilters {
    /// The filters `given` asks for.
    ///
    /// # Errors
    ///
    /// A key not read, or a `dangling` that isn't a boolean
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn parse(given: Given) -> Result<Self> {
        given
            .checked(|key, values| match key {
                "name" | "label" | "driver" => Ok(()),
                "dangling" => each_value(key, values, |value| boolean(value).is_some()),
                _ => Err(unsupported(key, "name, label, dangling, driver")),
            })
            .map(Self)
    }

    /// Whether `dangling` was asked: only then is it worth reading which
    /// volumes containers use.
    #[must_use]
    pub fn wants_mounted(&self) -> bool {
        self.0.contains_key("dangling")
    }

    /// Whether `volume`, used (`mounted`) or not, passes every key's
    /// filter.
    #[must_use]
    pub fn pass(&self, volume: &super::Volume, mounted: bool) -> bool {
        let no_labels = BTreeMap::new();
        let labels = volume.labels.as_ref().unwrap_or(&no_labels);
        let name = volume.name.as_str();
        self.0.iter().all(|(key, values)| {
            values.is_empty()
                || matched(key, values, |value| match key.as_str() {
                    "name" => name.contains(value.as_str()),
                    "label" => label_matches(labels, value),
                    "driver" => value == "local",
                    "dangling" => boolean(value) == Some(!mounted),
                    _ => false,
                })
        })
    }
}

/// A network list's filters: `name` (what it contains), `id` (its start),
/// `label`, `driver`, `type` (`builtin` or `custom`), `dangling` (no
/// container up on it, and not pre-defined: Docker's `filterNetworkByUse`,
/// F120).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetworkFilters(BTreeMap<String, Vec<String>>);

impl NetworkFilters {
    /// The filters `given` asks for.
    ///
    /// # Errors
    ///
    /// A key not read (`scope` among them), a `type` that isn't `builtin`
    /// or `custom`, or a `dangling` that isn't a boolean
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn parse(given: Given) -> Result<Self> {
        given
            .checked(|key, values| match key {
                "name" | "id" | "label" | "driver" => Ok(()),
                "type" => each_value(key, values, |value| ["builtin", "custom"].contains(&value)),
                "dangling" => each_value(key, values, |value| boolean(value).is_some()),
                _ => Err(unsupported(key, "name, id, label, driver, type, dangling")),
            })
            .map(Self)
    }

    /// Whether `dangling` was asked: only then is it worth reading which
    /// networks containers that are up use.
    #[must_use]
    pub fn wants_in_use(&self) -> bool {
        self.0.contains_key("dangling")
    }

    /// Whether `network`, a container up on it (`in_use`) or not, passes
    /// every key's filter.
    #[must_use]
    pub fn pass(&self, network: &NetworkResource, in_use: bool) -> bool {
        self.passes(
            [&network.name, &network.id, &network.driver],
            &network.labels,
            in_use,
        )
    }

    /// Whether a network of this name, ID and driver, `labels` and use,
    /// passes.
    fn passes(
        &self,
        [name, id, driver]: [&str; 3],
        labels: &BTreeMap<String, String>,
        in_use: bool,
    ) -> bool {
        self.0.iter().all(|(key, values)| {
            values.is_empty()
                || matched(key, values, |value| match key.as_str() {
                    "name" => name.contains(value.as_str()),
                    "id" => id.starts_with(value.as_str()),
                    "label" => label_matches(labels, value),
                    "driver" => driver == value,
                    // The default `bridge` is built-in, as Docker counts it.
                    "type" => super::is_predefined(name) == (value == "builtin"),
                    // Docker's: a pre-defined network is never dangling.
                    "dangling" => boolean(value) == Some(!super::is_predefined(name) && !in_use),
                    _ => false,
                })
        })
    }
}

/// A prune's filters: `all` (every unused volume; without it, as Docker's
/// API since 1.42, only anonymous ones), `label` and `label!`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneFilters {
    /// Every unused volume, not only anonymous ones.
    pub all: bool,
    labels: Vec<String>,
    not_labels: Vec<String>,
}

impl PruneFilters {
    /// The filters `given` asks for.
    ///
    /// # Errors
    ///
    /// A key not read, or an `all` that isn't a boolean
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn parse(given: Given) -> Result<Self> {
        let mut filters = Self::default();
        for (key, values) in given.0 {
            match key.as_str() {
                "all" => {
                    for value in &values {
                        filters.all |= boolean(value)
                            .ok_or_else(|| invalid(format!("invalid filter 'all={value}'")))?;
                    }
                }
                "label" => filters.labels = values,
                "label!" => filters.not_labels = values,
                _ => return Err(unsupported(&key, "all, label, label!")),
            }
        }
        Ok(filters)
    }

    /// Whether an object of `labels` is to be pruned (unused: the engine's
    /// to decide).
    #[must_use]
    pub fn wants(&self, labels: &BTreeMap<String, String>) -> bool {
        (self.all || labels.contains_key(ANONYMOUS_VOLUME))
            && self.labels.iter().all(|label| label_matches(labels, label))
            && !self
                .not_labels
                .iter()
                .any(|label| label_matches(labels, label))
    }
}

/// What every prune's filters share: `until` (made before; the latest
/// given, as Docker keeps one), `label` and `label!`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Selection {
    /// Unix seconds: only what was made before.
    until: Option<i64>,
    labels: Vec<String>,
    not_labels: Vec<String>,
}

impl Selection {
    /// Reads `key`'s `values` if it's one of the shared keys, at `now`
    /// (Unix seconds, for an `until` given as a duration): whether it was.
    fn read(&mut self, key: &str, values: Vec<String>, now: i64) -> Result<bool> {
        match key {
            "until" => {
                for value in &values {
                    self.until = Some(until(value, now)?);
                }
            }
            "label" => self.labels = values,
            "label!" => self.not_labels = values,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Whether an object made at `created` (Unix seconds), of `labels`,
    /// passes.
    fn wants(&self, created: i64, labels: &BTreeMap<String, String>) -> bool {
        // Docker's: kept only if made after it.
        self.until.is_none_or(|until| created <= until) && self.labels_pass(labels)
    }

    /// Whether `labels` pass the label filters alone.
    fn labels_pass(&self, labels: &BTreeMap<String, String>) -> bool {
        self.labels.iter().all(|label| label_matches(labels, label))
            && !self
                .not_labels
                .iter()
                .any(|label| label_matches(labels, label))
    }
}

/// `container prune`'s filters: `until`, `label`, `label!`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContainerPruneFilters(Selection);

impl ContainerPruneFilters {
    /// The filters `given` asks for, an `until` duration counted back from
    /// `now` (Unix seconds).
    ///
    /// # Errors
    ///
    /// A key not read, or an `until` that's neither a time nor a duration
    /// ([`ErrorKind::InvalidRequest`]).
    pub fn parse(given: Given, now: i64) -> Result<Self> {
        let mut kept = Selection::default();
        for (key, values) in given.0 {
            if !kept.read(&key, values, now)? {
                return Err(unsupported(&key, "until, label, label!"));
            }
        }
        Ok(Self(kept))
    }

    /// Whether `summary` (a stopped container: the engine's to decide) is
    /// to be pruned.
    #[must_use]
    pub fn wants(&self, summary: &ContainerSummary) -> bool {
        self.0.wants(summary.created, &summary.labels)
    }

    /// Whether an object made at `created` (Unix seconds), of `labels`,
    /// is to be pruned: `system prune`'s networks.
    #[must_use]
    pub fn wants_made(&self, created: i64, labels: &BTreeMap<String, String>) -> bool {
        self.0.wants(created, labels)
    }

    /// Whether an object of `labels` passes the label filters alone
    /// (`system prune`'s volumes, which `until` is refused with).
    #[must_use]
    pub fn wants_labels(&self, labels: &BTreeMap<String, String>) -> bool {
        self.0.labels_pass(labels)
    }

    /// Whether `until` was asked for.
    #[must_use]
    pub fn has_until(&self) -> bool {
        self.0.until.is_some()
    }
}

/// `image prune`'s filters: `dangling` (`true`, the default: only images
/// with no tag; `false`: every unused one, `image prune -a`), `until`,
/// `label`, `label!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePruneFilters {
    /// Only dangling images (no name), not every unused one.
    pub dangling: bool,
    kept: Selection,
}

impl Default for ImagePruneFilters {
    fn default() -> Self {
        Self {
            dangling: true,
            kept: Selection::default(),
        }
    }
}

impl ImagePruneFilters {
    /// The filters `given` asks for, at `now`, as
    /// [`ContainerPruneFilters::parse`].
    ///
    /// # Errors
    ///
    /// As [`ContainerPruneFilters::parse`]; a `dangling` that isn't a
    /// boolean.
    pub fn parse(given: Given, now: i64) -> Result<Self> {
        let mut filters = Self::default();
        for (key, values) in given.0 {
            if key == "dangling" {
                for value in &values {
                    filters.dangling = boolean(value)
                        .ok_or_else(|| invalid(format!("invalid filter 'dangling={value}'")))?;
                }
            } else if !filters.kept.read(&key, values, now)? {
                return Err(unsupported(&key, "dangling, until, label, label!"));
            }
        }
        Ok(filters)
    }

    /// Whether `image` (an unused one: the engine's to decide) is to be
    /// pruned.
    #[must_use]
    pub fn wants(&self, image: &ImageSummary) -> bool {
        // Docker's prune: no tag (a digest isn't one: every image here
        // has its digest's name).
        let dangling = image.repo_tags.is_empty();
        (dangling || !self.dangling)
            && self.kept.wants(
                image.created,
                image.labels.as_ref().unwrap_or(&BTreeMap::new()),
            )
    }
}

/// Docker's `until`: a time (Unix seconds, RFC 3339: [`super::parse_time`])
/// or a Go duration before `now` (`10m`, `1h30m`, `1.5h`), as Unix seconds.
fn until(value: &str, now: i64) -> Result<i64> {
    let refused = || invalid(format!("invalid filter 'until={value}'"));
    if let Some(time) = super::parse_time(value) {
        return Ok(time.secs());
    }
    let seconds = go_duration(value).ok_or_else(refused)?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a duration's whole seconds, bounded by go_duration"
    )]
    Ok(now.saturating_sub(seconds as i64))
}

/// A Go duration (`time.ParseDuration`) in seconds: numbers, each with its
/// unit (`ns`, `us`, `µs`, `ms`, `s`, `m`, `h`); `None` if it isn't one,
/// or it's past a century. The one grammar of Docker's durations: filters'
/// `until`, and `logs --since` (11.6b's clean-code review).
#[must_use]
pub fn go_duration(text: &str) -> Option<f64> {
    const UNITS: [(&str, f64); 8] = [
        ("ns", 1e-9),
        ("us", 1e-6),
        ("µs", 1e-6),
        ("μs", 1e-6),
        ("ms", 1e-3),
        ("s", 1.0),
        ("m", 60.0),
        ("h", 3600.0),
    ];
    let mut rest = text.strip_prefix('+').unwrap_or(text);
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    let mut seconds = 0.0;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .filter(|&end| end > 0)?;
        let number: f64 = rest[..number_end].parse().ok()?;
        rest = &rest[number_end..];
        let (unit, scale) = UNITS
            .iter()
            .filter(|(unit, _)| rest.starts_with(unit))
            .max_by_key(|(unit, _)| unit.len())?;
        seconds += number * scale;
        rest = &rest[unit.len()..];
    }
    (seconds <= 100.0 * 365.0 * 86_400.0).then_some(seconds)
}

/// Docker's booleans in a filter.
fn boolean(value: &str) -> Option<bool> {
    match value {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// Whether `values` (one key's) match, by `each`: all of them for
/// `label` (an object with every label asked for, as Docker's), one of
/// them for any other key.
fn matched(key: &str, values: &[String], each: impl Fn(&String) -> bool) -> bool {
    if key == "label" {
        values.iter().all(each)
    } else {
        values.iter().any(each)
    }
}

/// Whether `text` matches `pattern` as Go's `path.Match` does it: `*` any
/// run of characters but `/`, `?` any one but `/`; anything else itself.
fn glob(pattern: &str, text: &str) -> bool {
    let (pattern, text): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), text.chars().collect());
    // The classic two-pointer walk, the last `*` remembered to retry from.
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&wanted) if (wanted == '?' && text[t] != '/') || wanted == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((at, from)) if text[from] != '/' => {
                    p = at + 1;
                    t = from + 1;
                    star = Some((at, from + 1));
                }
                _ => return false,
            },
        }
    }
    pattern[p..].iter().all(|&rest| rest == '*')
}

#[cfg(test)]
mod tests;
