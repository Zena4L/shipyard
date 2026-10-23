//! Reading the journal: the past events, then the new ones as they're
//! written, across rotations.

use std::io::{Read as _, Seek as _};
use std::path::Path;
use std::time::SystemTime;

use super::{Event, FILE, Filters, POLL, ROTATED};
use crate::Engine;
use crate::error::{Context as _, Error, Result};

/// What [`read`] reads. (Built by its callers, field by field.)
#[derive(Debug, Clone, Default)]
pub struct EventsOptions {
    /// From this time (none: from now on, unless there's an `until`).
    pub since: Option<SystemTime>,
    /// Up to this time (none: followed, without end).
    pub until: Option<SystemTime>,
    /// Which events, between those times.
    pub filters: Filters,
    /// Asked between a follower's looks: `true` ends it (its reader has
    /// gone, which a write would only find at the next event).
    pub gone: Option<crate::logs::Gone>,
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
    options: &EventsOptions,
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
            || options.gone.as_ref().is_some_and(crate::logs::Gone::ask)
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
