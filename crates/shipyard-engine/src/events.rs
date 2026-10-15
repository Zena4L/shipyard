//! The events journal (stage 9, step 9.6a): what happened to containers,
//! images, volumes and networks, for `shipyard events` (Docker's).
//!
//! There's no daemon to hear about everything, so every process that
//! changes something appends what it did to one journal per store: a
//! command, a container's clean-up as it ends, a restart that waits. Each
//! event is one line of JSON, written with one `write` to a file opened
//! with `O_APPEND`: the kernel puts each such write whole at the file's end,
//! whoever else appends (TLPI §5.1), and a line is kept small (at most
//! [`MAX_LINE`]) so it's one write. The journal is rotated by size into one
//! older file: appenders hold a shared `flock` on `events.lock` while they
//! write, and the one that rotates takes it exclusively, without waiting
//! (busy: the next appender rotates). Each line carries its format's
//! version (the ROADMAP's H2): a reader skips the ones it doesn't know.
//!
//! An event's time is when it happened, not when it was written: a
//! container's end is written by its clean-up with the time its monitor saw
//! it, and one nothing recorded with the time recovery gives it. Readers
//! keep the journal's order and filter by time.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::Engine;
use crate::error::{Context as _, Error, Result};

/// The journal's line format version: what's written.
pub(crate) const VERSION: u32 = 1;
/// The longest line written: one `write` puts it whole.
pub const MAX_LINE: usize = 4096;
/// The journal's file.
const FILE: &str = "events.log";
/// Its one older file, after a rotation.
const ROTATED: &str = "events.log.1";
/// The lock appenders share and a rotation takes alone.
const LOCK: &str = "events.lock";
/// The size past which the journal is rotated.
const ROTATE_AT: u64 = 1024 * 1024;
/// How often a follower looks for new events.
const POLL: Duration = Duration::from_millis(200);

/// What an event is about (Docker's types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Kind {
    /// A container.
    Container,
    /// An image.
    Image,
    /// A named volume.
    Volume,
    /// A network.
    Network,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Container => "container",
            Self::Image => "image",
            Self::Volume => "volume",
            Self::Network => "network",
        })
    }
}

impl std::str::FromStr for Kind {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        Ok(match text {
            "container" => Self::Container,
            "image" => Self::Image,
            "volume" => Self::Volume,
            "network" => Self::Network,
            _ => crate::error::bail!(
                InvalidRequest,
                "event type {text:?}: give container, image, volume or network"
            ),
        })
    }
}

/// One event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Event {
    /// When it happened.
    pub time: SystemTime,
    /// What it's about.
    pub kind: Kind,
    /// What happened (`start`, `die`, `pull`, …: Docker's words).
    pub action: String,
    /// Its subject's ID (a container's, an image's digest, a volume's or a
    /// network's name).
    pub id: String,
    /// More about it (`name`, `image`, `exitCode`, `signal`, …).
    pub attributes: BTreeMap<String, String>,
}

impl Event {
    /// An event about `kind` `id`: `action`, now.
    #[must_use]
    pub fn new(kind: Kind, action: &str, id: &str) -> Self {
        Self {
            time: SystemTime::now(),
            kind,
            action: action.to_owned(),
            id: id.to_owned(),
            attributes: BTreeMap::new(),
        }
    }

    /// This event, with `key` said to be `value`.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        self.attributes.insert(key.to_owned(), value.to_string());
        self
    }

    /// This event, as having happened at `time`.
    #[must_use]
    pub fn at(mut self, time: SystemTime) -> Self {
        self.time = time;
        self
    }

    /// Its line, as written (without the newline): under [`MAX_LINE`]
    /// bytes with it. Its ID and action are cut to [`MAX_ID`] and
    /// [`MAX_ACTION`] bytes (even escaped, they fit), then its attributes'
    /// values, the longest first, until it fits.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line = V1 {
            v: VERSION,
            time: rfc3339(self.time),
            kind: self.kind,
            action: cut(&self.action, MAX_ACTION).to_owned(),
            id: cut(&self.id, MAX_ID).to_owned(),
            attributes: self.attributes.clone(),
        };
        loop {
            let text = serde_json::to_string(&line).unwrap_or_default();
            if text.len() < MAX_LINE {
                return text;
            }
            let Some(longest) = line
                .attributes
                .values_mut()
                .max_by_key(|value| value.len())
                .filter(|value| !value.is_empty())
            else {
                // Can't be (the rest is bounded); it's written as it is.
                return text;
            };
            let half = cut(longest, longest.len() / 2).len();
            longest.truncate(half);
        }
    }

    /// The event a line says, if it's one this Shipyard reads (its
    /// version known, its time readable). Parsed once, as version 1: a line
    /// of another version says another `v`, or has fields version 1 doesn't
    /// (refused: `deny_unknown_fields`), and is skipped either way.
    #[must_use]
    pub(crate) fn from_line(line: &str) -> Option<Self> {
        let read: V1 = serde_json::from_str(line).ok()?;
        if read.v != VERSION {
            return None;
        }
        let time = shipyard_runtime::logs::time::Timestamp::parse(&read.time).ok()?;
        Some(Self {
            time: system_time(time)?,
            kind: read.kind,
            action: read.action,
            id: read.id,
            attributes: read.attributes,
        })
    }
}

/// The longest ID a line keeps, in bytes.
pub const MAX_ID: usize = 512;
/// The longest action a line keeps, in bytes.
pub const MAX_ACTION: usize = 64;

/// `text`'s first `budget` bytes at most, ending on a character.
fn cut(text: &str, budget: usize) -> &str {
    let mut end = budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Version 1 of a line, as it's written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1 {
    v: u32,
    time: String,
    #[serde(rename = "type")]
    kind: Kind,
    action: String,
    id: String,
    attributes: BTreeMap<String, String>,
}

/// `time` in RFC 3339, UTC, to the nanosecond (as the journal writes it).
#[must_use]
pub fn rfc3339(time: SystemTime) -> String {
    let since = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    shipyard_runtime::logs::time::Timestamp::from_unix(
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        since.subsec_nanos(),
    )
    .map(|stamp| stamp.to_string())
    .unwrap_or_default()
}

/// `time` as the system's (none before 1970).
#[must_use]
pub fn system_time(time: shipyard_runtime::logs::time::Timestamp) -> Option<SystemTime> {
    let since = Duration::new(u64::try_from(time.secs()).ok()?, time.nanos());
    Some(SystemTime::UNIX_EPOCH + since)
}

/// Writes that `kind` `id` had `action` done to it, now.
pub(crate) fn emit_about(engine: &Engine, kind: Kind, action: &str, id: &str) {
    emit(engine, &Event::new(kind, action, id));
}

/// Writes that a prune of `kind`s removed `removed` things.
pub(crate) fn emit_pruned(engine: &Engine, kind: Kind, removed: usize) {
    emit(
        engine,
        &Event::new(kind, "prune", "").with("removed", removed),
    );
}

/// An event about the container `record` is: `action`, now, with its
/// name and image.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn container(record: &crate::containers::Record, action: &str) -> Event {
    let event =
        Event::new(Kind::Container, action, record.id.as_str()).with("name", record.name.as_str());
    match &record.config.image {
        Some(image) => event.with("image", image),
        None => event,
    }
}

/// Writes the end of the container `record` is (its exit, if it has one):
/// at the time its monitor saw it (else now), an OOM kill first, as
/// Docker says it, then `die` with its exit code.
#[cfg(target_os = "linux")]
pub(crate) fn container_ended(engine: &Engine, record: &crate::containers::Record) {
    let Some(exit) = &record.exit else {
        return;
    };
    let at = record.finished.unwrap_or_else(SystemTime::now);
    if exit.oom.is_some() {
        emit(engine, &container(record, "oom").at(at));
    }
    let mut died = container(record, "die").at(at);
    if let Some(code) = exit.code {
        died = died.with("exitCode", code);
    }
    emit(engine, &died);
}

/// Appends `event` to `engine`'s journal. Never fails what it's said
/// about: a journal that can't be written is warned about (once a
/// process), and the event is lost.
pub(crate) fn emit(engine: &Engine, event: &Event) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if let Err(error) = append(engine, event)
        && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        engine.warn(
            crate::report::WarningKind::LeftForLater,
            format!("the events journal couldn't be written: {error:#}"),
        );
    }
}

/// [`emit`]'s work: the line appended under the shared lock, then the
/// journal rotated if it's grown past [`ROTATE_AT`] (only if the lock can
/// be had alone at once).
fn append(engine: &Engine, event: &Event) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = made_journal_dir(engine)?;
    let mut line = event.to_line();
    line.push('\n');
    let lock = crate::fs::open_lock_file(&dir.join(LOCK))?;
    let shared = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockShared)
        .map_err(|(_, errno)| errno)
        .context("locking the events journal")?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(FILE))
        .context("opening the events journal")?;
    // One write: the line whole at the end, whoever else appends.
    file.write_all(line.as_bytes())
        .context("writing the events journal")?;
    let size = file.metadata().map_or(0, |meta| meta.len());
    drop(file);
    if size > ROTATE_AT {
        let lock = shared
            .unlock()
            .map_err(|(_, errno)| errno)
            .context("unlocking the events journal")?;
        // Alone, or not now: the next appender rotates.
        if let Ok(_alone) =
            nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
            && std::fs::metadata(dir.join(FILE)).is_ok_and(|meta| meta.len() > ROTATE_AT)
        {
            std::fs::rename(dir.join(FILE), dir.join(ROTATED))
                .context("rotating the events journal")?;
        }
    }
    Ok(())
}

/// The journal's directory, made if it isn't there (private, as the
/// stores are), for an appender.
fn made_journal_dir(engine: &Engine) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = engine.paths.events()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("making {}", dir.display()))?;
    crate::fs::check_private_dir(&dir, "the events journal's directory")?;
    Ok(dir)
}

/// Which events [`read`] gives: those of every key given (a key given
/// several times: any of its values), between `since` and `until`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Filters {
    /// Their types.
    pub kinds: Vec<Kind>,
    /// Their actions.
    pub actions: Vec<String>,
    /// About these containers (an ID's start, or a name).
    pub containers: Vec<String>,
    /// About these images (as a container's `image` too).
    pub images: Vec<String>,
    /// About these volumes.
    pub volumes: Vec<String>,
    /// About these networks.
    pub networks: Vec<String>,
}

impl Filters {
    /// Filters from `KEY=VALUE`s (`type`, `event`, `container`, `image`,
    /// `volume`, `network`).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): a key it doesn't know,
    /// no `=`, or no value.
    pub fn parse(given: &[String]) -> Result<Self> {
        let mut filters = Self::default();
        for filter in given {
            let Some((key, value)) = filter.split_once('=') else {
                crate::error::bail!(InvalidRequest, "filter {filter:?}: give KEY=VALUE");
            };
            if value.is_empty() {
                crate::error::bail!(InvalidRequest, "filter {filter:?}: give a value");
            }
            let value = value.to_owned();
            match key {
                "type" => filters.kinds.push(value.parse()?),
                "event" => filters.actions.push(value),
                "container" => filters.containers.push(value),
                "image" => filters.images.push(value),
                "volume" => filters.volumes.push(value),
                "network" => filters.networks.push(value),
                _ => crate::error::bail!(
                    InvalidRequest,
                    "filter {key:?}: give type, event, container, image, volume or network"
                ),
            }
        }
        Ok(filters)
    }

    /// Whether `event` is a network's, about a container these filters
    /// want (`connect`, `disconnect`: its `container` attribute, its ID or
    /// a start of it), as Docker's `--filter container=` takes them.
    fn about_its_networks(&self, event: &Event) -> bool {
        event.kind == Kind::Network
            && event.attributes.get("container").is_some_and(|id| {
                self.containers
                    .iter()
                    .any(|want| id.starts_with(want.as_str()))
            })
    }

    /// Whether `event` passes.
    #[must_use]
    pub fn pass(&self, event: &Event) -> bool {
        // A container by its name or its ID's start; an image by its
        // reference however it's written (`alpine` is
        // `docker.io/library/alpine:latest`); the rest by name.
        let about = |kind: Kind, wanted: &[String]| {
            wanted.is_empty()
                || (event.kind == kind
                    && wanted.iter().any(|want| {
                        event.id == *want
                            || (kind == Kind::Container && event.id.starts_with(want.as_str()))
                            || (kind == Kind::Image && same_image(&event.id, want))
                            || event.attributes.get("name") == Some(want)
                    }))
        };
        (self.kinds.is_empty() || self.kinds.contains(&event.kind))
            && (self.actions.is_empty() || self.actions.contains(&event.action))
            && (about(Kind::Container, &self.containers) || self.about_its_networks(event))
            && (about(Kind::Image, &self.images)
                || self.images.iter().any(|want| {
                    event
                        .attributes
                        .get("image")
                        .is_some_and(|image| same_image(image, want))
                }))
            && about(Kind::Volume, &self.volumes)
            && about(Kind::Network, &self.networks)
    }
}

/// Whether two image references name the same image (each as written,
/// or both read the same way).
fn same_image(one: &str, other: &str) -> bool {
    use shipyard_image::ImageReference;
    one == other
        || ImageReference::parse(one)
            .ok()
            .zip(ImageReference::parse(other).ok())
            .is_some_and(|(one, other)| one.to_string() == other.to_string())
}

/// What [`read`] reads. (Built by its callers, field by field.)
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// From this time (none: from now on, unless there's an `until`).
    pub since: Option<SystemTime>,
    /// Up to this time (none: followed, without end).
    pub until: Option<SystemTime>,
    /// Which events, between those times.
    pub filters: Filters,
    /// Asked between a follower's looks: `true` ends it (its reader has
    /// gone, which a write would only find at the next event).
    pub gone: Option<fn() -> bool>,
}

/// Gives `sink` the journal's events, as `options` say: the past ones from
/// `since` (the older file, then the current one; with only an `until`,
/// all of them, as Docker's), then (without `until`, or until it's past)
/// the new ones as they're written, across rotations.
///
/// A follower that misses three rotations between two looks (3 MiB of events
/// in 200 ms) misses the oldest of them.
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): `until`
/// before `since`. The journal can't be read; `sink`'s error (a closed
/// pipe), which ends it.
pub fn read(
    engine: &Engine,
    options: &Options,
    mut sink: impl FnMut(Event) -> std::io::Result<()>,
) -> Result<()> {
    if let (Some(since), Some(until)) = (options.since, options.until)
        && until < since
    {
        crate::error::bail!(InvalidRequest, "--until is before --since");
    }
    let since = options
        .since
        .or(options.until.map(|_| SystemTime::UNIX_EPOCH));
    let dir = engine.paths.events()?;
    let (file, rotated) = (dir.join(FILE), dir.join(ROTATED));
    let mut give = |event: Event| -> Result<()> {
        let after = since.is_none_or(|since| event.time >= since);
        let before = options.until.is_none_or(|until| event.time <= until);
        if after && before && options.filters.pass(&event) {
            sink(event).context("writing the events")?;
        }
        Ok(())
    };
    let mut current = match since {
        Some(_) => past(&file, &rotated, &mut give)?,
        None => Tail::open(&file, true)?,
    };
    if options
        .until
        .is_some_and(|until| until <= SystemTime::now())
    {
        return Ok(());
    }
    loop {
        // Moved (rotated) since it was opened: it's whole (appenders write
        // only to the file at the path), so read to its end, then the new.
        let moved = !current.at(&file);
        current.drain(&mut give)?;
        if moved {
            // Rotated twice: the older file is one this never had.
            if current.file.is_some() {
                let mut middle = Tail::open(&rotated, false)?;
                if middle.file.is_some() && middle.inode != current.inode {
                    middle.drain(&mut give)?;
                }
            }
            current = Tail::open(&file, false)?;
            continue;
        }
        if options.until.is_some_and(|until| SystemTime::now() > until)
            || options.gone.is_some_and(|gone| gone())
        {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
}

/// The past: the older file, then the current one, each read to its end,
/// opened so that a rotation meanwhile has neither read twice nor skipped
/// (the current one opened, then the older; again if the current one moved
/// in between). The current one, for following.
fn past(file: &Path, rotated: &Path, give: &mut impl FnMut(Event) -> Result<()>) -> Result<Tail> {
    loop {
        let mut current = Tail::open(file, false)?;
        let mut older = Tail::open(rotated, false)?;
        if !current.at(file) {
            continue;
        }
        older.drain(give)?;
        current.drain(give)?;
        return Ok(current);
    }
}

/// The journal's file being read: what's been read of it, and its inode
/// (a rotation renames it away: the same open file, another inode at the
/// path).
struct Tail {
    file: Option<std::fs::File>,
    inode: u64,
    partial: Vec<u8>,
}

impl Tail {
    /// `path` opened (from its end with `at_end`); one not there yet is
    /// read as empty, and found once it is.
    fn open(path: &Path, at_end: bool) -> Result<Self> {
        use std::os::unix::fs::MetadataExt as _;
        match std::fs::File::open(path) {
            Ok(mut file) => {
                if at_end {
                    file.seek(std::io::SeekFrom::End(0))
                        .context("reading the events journal")?;
                }
                let inode = file.metadata().map_or(0, |meta| meta.ino());
                Ok(Self {
                    file: Some(file),
                    inode,
                    partial: Vec::new(),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                file: None,
                inode: 0,
                partial: Vec::new(),
            }),
            Err(error) => Err(Error::from(error).context(format!("reading {}", path.display()))),
        }
    }

    /// Every whole line there is now, as events (lines it can't read are
    /// skipped); a line still being written is kept for later.
    fn drain(&mut self, give: &mut impl FnMut(Event) -> Result<()>) -> Result<()> {
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        let mut chunk = Vec::new();
        file.read_to_end(&mut chunk)
            .context("reading the events journal")?;
        self.partial.extend_from_slice(&chunk);
        // Line by line from an offset, what's read dropped once at the end
        // (dropped line by line, the rest would move each time: quadratic).
        let mut start = 0;
        let mut given = Ok(());
        while let Some(end) = self.partial[start..].iter().position(|&byte| byte == b'\n') {
            let line = &self.partial[start..start + end];
            start += end + 1;
            if let Some(event) = std::str::from_utf8(line)
                .ok()
                .and_then(|line| Event::from_line(line.trim_end()))
            {
                given = give(event);
                if given.is_err() {
                    break;
                }
            }
        }
        self.partial.drain(..start);
        given
    }

    /// Whether this is still the file at `path` (none there, for one that
    /// found none).
    fn at(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        match std::fs::metadata(path) {
            Ok(meta) => self.file.is_some() && meta.ino() == self.inode,
            Err(_) => self.file.is_none(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn event() -> Event {
        Event::new(Kind::Container, "die", "0123abcd")
            .with("name", "web")
            .with("exitCode", 3)
            .at(SystemTime::UNIX_EPOCH + Duration::new(1_760_000_000, 123_456_789))
    }

    /// A line round trips, and a pinned version 1 line reads as written.
    #[test]
    fn a_line_reads_back_and_version_1_is_pinned() {
        let line = event().to_line();
        assert_eq!(Event::from_line(&line), Some(event()));
        let pinned = r#"{"v":1,"time":"2025-10-09T08:53:20.123456789Z","type":"container","action":"die","id":"0123abcd","attributes":{"exitCode":"3","name":"web"}}"#;
        assert_eq!(line, pinned);
        assert_eq!(Event::from_line(pinned), Some(event()));
        // Another version, or not an event: skipped.
        assert_eq!(
            Event::from_line(&pinned.replace(r#""v":1"#, r#""v":2"#)),
            None
        );
        assert_eq!(Event::from_line("{"), None);
        assert_eq!(Event::from_line(r#"{"v":1}"#), None);
    }

    /// A line never grows past [`MAX_LINE`]: the longest values are cut,
    /// and an ID or action past its bound (on a character, in bytes); what's
    /// written still reads.
    #[test]
    fn a_line_is_kept_short() {
        let big = event().with("note", "é".repeat(10_000));
        let line = big.to_line();
        assert!(line.len() < MAX_LINE, "{}", line.len());
        let read = Event::from_line(&line).unwrap();
        assert_eq!(read.attributes["name"], "web");
        assert!(read.attributes["note"].starts_with('é'));
        // Escaped six bytes each, at their bounds: still a line that reads.
        let worst = Event::new(
            Kind::Volume,
            &"\u{1}".repeat(1_000),
            &"é\u{1}".repeat(3_000),
        );
        let line = worst.to_line();
        assert!(line.len() < MAX_LINE, "{}", line.len());
        let read = Event::from_line(&line).unwrap();
        assert!(read.id.len() <= MAX_ID && read.id.starts_with('é'));
        assert_eq!(read.action.len(), MAX_ACTION);
    }

    /// An engine whose journal is in a new directory of its own.
    fn journal(name: &str) -> (Engine, PathBuf) {
        let (engine, _, parent) = crate::testing::engine_on_a_new_store(name);
        let dir = parent.join("events");
        let paths = engine.paths().clone().with_events(&dir);
        (
            Engine::new(paths, std::sync::Arc::clone(engine.reporter())),
            dir,
        )
    }

    /// From now on, for a while: a follower that misses an event ends,
    /// failing its test, rather than waiting for ever.
    fn for_a_while() -> Options {
        let now = SystemTime::now();
        Options {
            since: Some(now),
            until: Some(now + Duration::from_secs(30)),
            ..Options::default()
        }
    }

    /// Every event `engine`'s journal has, oldest first.
    fn everything(engine: &Engine) -> Vec<Event> {
        let options = Options {
            since: Some(SystemTime::UNIX_EPOCH),
            until: Some(SystemTime::now()),
            ..Options::default()
        };
        let mut read = Vec::new();
        super::read(engine, &options, |event| {
            read.push(event);
            Ok(())
        })
        .unwrap();
        read
    }

    /// Many processes' appends at once (threads here: the same `write`s)
    /// give whole lines, each once, across a rotation; the journal is
    /// private.
    #[test]
    fn concurrent_appends_stay_whole_across_a_rotation() {
        use std::os::unix::fs::PermissionsExt as _;
        let (engine, dir) = journal("events-concurrent");
        let note = "x".repeat(100);
        std::thread::scope(|scope| {
            for thread in 0..16 {
                let (engine, note) = (&engine, &note);
                scope.spawn(move || {
                    for n in 0..500 {
                        let event = Event::new(Kind::Container, "start", &format!("{thread}"))
                            .with("n", n)
                            .with("note", note);
                        emit(engine, &event);
                    }
                });
            }
        });
        assert!(dir.join(ROTATED).exists(), "rotated past {ROTATE_AT}");
        let read = everything(&engine);
        assert_eq!(read.len(), 16 * 500);
        for thread in 0..16 {
            let ns: Vec<String> = read
                .iter()
                .filter(|event| event.id == thread.to_string())
                .map(|event| event.attributes["n"].clone())
                .collect();
            let wanted: Vec<String> = (0..500).map(|n: i32| n.to_string()).collect();
            assert_eq!(ns, wanted, "thread {thread}'s events, in its order");
        }
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(ROTATED)), 0o600);
        assert_eq!(mode(&dir.join(FILE)), 0o600);
    }

    /// A follower started before a rotation reads every event after it
    /// started, the old file's to its end and the new one's, while several
    /// processes (threads here) write at once.
    #[test]
    fn a_follower_reads_across_a_rotation() {
        let (engine, dir) = journal("events-follow");
        emit(&engine, &Event::new(Kind::Image, "pull", "before"));
        let (writers, each) = (8, 1_000);
        let note = "y".repeat(100);
        let read = std::thread::scope(|scope| {
            let follower = scope.spawn(|| {
                let mut read = Vec::new();
                let done = super::read(&engine, &for_a_while(), |event| {
                    read.push(event.id);
                    if read.len() == writers * each {
                        // Ends it, as a closed pipe does.
                        return Err(std::io::ErrorKind::BrokenPipe.into());
                    }
                    Ok(())
                });
                assert!(done.is_err(), "it ended before reading them all");
                read
            });
            // Begun: the follower has opened the file at its end.
            std::thread::sleep(POLL * 2);
            for writer in 0..writers {
                let (engine, note) = (&engine, &note);
                scope.spawn(move || {
                    for n in 0..each {
                        let id = format!("{writer}-{n}");
                        emit(
                            engine,
                            &Event::new(Kind::Image, "pull", &id).with("note", note),
                        );
                    }
                });
            }
            follower.join().unwrap()
        });
        assert!(dir.join(ROTATED).exists());
        for writer in 0..writers {
            let mine: Vec<&String> = read
                .iter()
                .filter(|id| id.starts_with(&format!("{writer}-")))
                .collect();
            let wanted: Vec<String> = (0..each).map(|n| format!("{writer}-{n}")).collect();
            assert_eq!(mine, wanted.iter().collect::<Vec<_>>(), "writer {writer}");
        }
    }

    /// Two rotations between a follower's looks: the file in the middle
    /// (rotated in, then out) is read too, in its place.
    #[test]
    fn a_follower_reads_a_file_rotated_twice_between_looks() {
        let (engine, dir) = journal("events-twice");
        emit(&engine, &Event::new(Kind::Volume, "create", "before"));
        let line = |id: &str| format!("{}\n", Event::new(Kind::Volume, "create", id).to_line());
        let (seen, follows) = std::sync::mpsc::channel();
        let read = std::thread::scope(|scope| {
            let engine = &engine;
            let follower = scope.spawn(move || {
                let mut read = Vec::new();
                let _ = super::read(engine, &for_a_while(), |event| {
                    // Its first `ready` says it follows; the rest are the
                    // test's.
                    if event.id == "ready" {
                        let _ = seen.send(());
                        return Ok(());
                    }
                    read.push(event.id);
                    if read.len() == 3 {
                        return Err(std::io::ErrorKind::BrokenPipe.into());
                    }
                    Ok(())
                });
                read
            });
            // Begun: `ready` events until the follower has read one (it
            // reads new ones, so it has the file open at its end by then).
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                emit(engine, &Event::new(Kind::Volume, "create", "ready"));
                if follows.recv_timeout(POLL).is_ok() {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "never followed");
            }
            // As appenders and two rotations would leave it, at once.
            let append = |text: String| {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(dir.join(FILE))
                    .unwrap();
                file.write_all(text.as_bytes()).unwrap();
            };
            append(line("first"));
            std::fs::rename(dir.join(FILE), dir.join(ROTATED)).unwrap();
            append(line("middle"));
            std::fs::rename(dir.join(FILE), dir.join(ROTATED)).unwrap();
            append(line("last"));
            follower.join().unwrap()
        });
        assert_eq!(read, ["first", "middle", "last"]);
    }

    /// `since` and `until` bound it by each event's own time (a die is
    /// written with its exit's time, which may be before what's ahead of it
    /// in the file); filters narrow it.
    #[test]
    fn reading_bounds_by_the_events_own_times() {
        let (engine, _) = journal("events-times");
        let at = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        for (id, secs) in [("a", 100), ("c", 300), ("b", 200)] {
            emit(
                &engine,
                &Event::new(Kind::Volume, "create", id).at(at(secs)),
            );
        }
        emit(
            &engine,
            &Event::new(Kind::Network, "create", "n").at(at(250)),
        );
        let mut read = Vec::new();
        let options = Options {
            since: Some(at(150)),
            until: Some(at(300)),
            filters: Filters::parse(&["type=volume".to_owned()]).unwrap(),
            ..Options::default()
        };
        super::read(&engine, &options, |event| {
            read.push(event.id);
            Ok(())
        })
        .unwrap();
        assert_eq!(read, ["c", "b"], "in the journal's order");
        // `until` alone: from the start (Docker's); before `since`: refused.
        let mut read = Vec::new();
        let options = Options {
            until: Some(at(150)),
            ..Options::default()
        };
        super::read(&engine, &options, |event| {
            read.push(event.id);
            Ok(())
        })
        .unwrap();
        assert_eq!(read, ["a"]);
        let backwards = Options {
            since: Some(at(300)),
            until: Some(at(100)),
            ..Options::default()
        };
        let error = super::read(&engine, &backwards, |_| Ok(())).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest);
    }

    /// Budget (H7): an event adds well under a millisecond to the command
    /// that writes it (tens of microseconds; the best of many short rounds,
    /// so a machine busy with other work still has a quiet one).
    #[test]
    fn an_emit_is_cheap() {
        let (engine, _) = journal("events-budget");
        let event = Event::new(Kind::Container, "start", "0123abcd").with("name", "web");
        let best = (0..20)
            .map(|_| {
                let started = std::time::Instant::now();
                for _ in 0..25 {
                    emit(&engine, &event);
                }
                started.elapsed() / 25
            })
            .min()
            .unwrap();
        assert!(best < Duration::from_millis(1), "{best:?} an event");
    }

    /// Filters as Docker's: each key's values any, every key given.
    #[test]
    fn filters_narrow_as_dockers() {
        let given = |filters: &[&str]| {
            Filters::parse(&filters.iter().map(|&f| f.to_owned()).collect::<Vec<_>>()).unwrap()
        };
        let die = event().with("image", "alpine");
        assert!(given(&[]).pass(&die));
        assert!(given(&["type=container", "event=die"]).pass(&die));
        assert!(given(&["event=start", "event=die"]).pass(&die));
        assert!(!given(&["event=start"]).pass(&die));
        assert!(given(&["container=web"]).pass(&die));
        assert!(given(&["container=0123"]).pass(&die));
        assert!(!given(&["container=db"]).pass(&die));
        assert!(given(&["image=alpine"]).pass(&die));
        assert!(!given(&["type=image"]).pass(&die));
        assert!(!given(&["volume=data"]).pass(&die));
        // Only a container's ID by its start: other names whole.
        let volume = Event::new(Kind::Volume, "create", "one");
        assert!(given(&["volume=one"]).pass(&volume));
        assert!(!given(&["volume=on"]).pass(&volume));
        assert!(!given(&["image=alp"]).pass(&die));
        // An image however its reference is written.
        let pull = Event::new(Kind::Image, "pull", "docker.io/library/alpine:latest");
        assert!(given(&["image=alpine"]).pass(&pull));
        assert!(given(&["image=alpine:latest"]).pass(&pull));
        assert!(!given(&["image=alpine:3.20"]).pass(&pull));
        assert!(given(&["image=docker.io/library/alpine"]).pass(&die));
        // A network's connect is the container's too, by its ID or a
        // start of it, and the network's by its name.
        let connect = Event::new(Kind::Network, "connect", "app")
            .with("name", "app")
            .with("container", "0123456789ab");
        assert!(given(&["container=0123"]).pass(&connect));
        assert!(given(&["network=app"]).pass(&connect));
        assert!(!given(&["container=db"]).pass(&connect));
        assert!(!given(&["container=0123", "network=other"]).pass(&connect));
        for bad in [["nope=1"], ["type=thing"], ["noequals"], ["volume="]] {
            let error = Filters::parse(&bad.map(str::to_owned)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{bad:?}");
        }
    }
}
