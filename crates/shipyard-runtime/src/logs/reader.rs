//! Reading a container's logs (stage 8, D15; here since stage 10): the CRI
//! records its monitor wrote ([its logs](super)), as lines, with `--tail`,
//! `--since` and `--follow`.
//!
//! A log directory holds `log.json`, `container.log` (the newest) and
//! `container.log.1` … `.N-1` (older and older; [`writer`](super::writer)).
//! The files are read oldest first. A record the program's line was cut into (`P`, then
//! `F`) is joined back into the line; a line too long to hold is handed on
//! in pieces. A line that isn't a record (torn by a crash, or written by
//! something else) is skipped and counted, not fatal: the rest is still
//! there to read.
//!
//! **Following** waits for more (polling, every [`POLL`]). When
//! `container.log` is rotated (its name now another file), the file being
//! read is finished first, then found again among the older ones by its
//! inode, and the newer files are read after it: two rotations between
//! polls lose nothing.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::logs::cri::{Stream, parse_record};
use crate::logs::info::{InfoError, LogInfo};
use crate::logs::time::Timestamp;
use crate::logs::writer::{CURRENT, older};

/// The longest line handed on whole; longer ones go in pieces of at most
/// this size (`Entry::complete` false but the last).
pub const MAX_LINE: usize = 1024 * 1024;

/// The longest a record's line in a file can be: its text's most
/// ([`MAX_TEXT`](crate::logs::cri::MAX_TEXT)), and room for its
/// prefix. Anything longer isn't one of ours, and is skipped.
const MAX_RECORD_LINE: usize = crate::logs::cri::MAX_TEXT + 256;

/// How often a follower looks for more.
pub const POLL: Duration = Duration::from_millis(200);

/// What to read.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogsOptions {
    /// Only the last this many lines (of those since [`since`](Self::since)).
    pub tail: Option<usize>,
    /// Only lines from this time on.
    pub since: Option<Timestamp>,
    /// Then wait for more, until the container has ended and everything it
    /// wrote is read.
    pub follow: bool,
    /// Only the directory with this identity (the one the container was
    /// created with): another put at its path since is refused.
    pub identity: Option<crate::container::DirIdentity>,
    /// Asked between a follower's looks: `true` ends it (its reader has
    /// gone, which a write would only find once there's more to write).
    pub gone: Option<fn() -> bool>,
}

impl LogsOptions {
    /// Whether its [`gone`](Self::gone) hook says the reader has gone.
    #[must_use]
    pub fn reader_gone(&self) -> bool {
        self.gone.is_some_and(|gone| gone())
    }
}

/// A line of the program's output (or a piece of one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// When its first part was read from the program.
    pub time: Timestamp,
    /// Which output.
    pub stream: Stream,
    /// Its bytes, without the newline.
    pub text: Vec<u8>,
    /// Whether it's a line's end: false for a piece (a line longer than
    /// [`MAX_LINE`], or, when following, part of a line written before its
    /// end, a prompt say).
    pub complete: bool,
}

/// How a read went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadSummary {
    /// Lines that weren't records, skipped.
    pub skipped: usize,
}

/// Logs that can't be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LogsError {
    /// The directory's `log.json` is missing, malformed, or another format
    /// or version (a newer Shipyard's).
    #[error("the logs in {}: {source}", dir.display())]
    Info {
        /// The log directory.
        dir: PathBuf,
        /// What's wrong with `log.json`.
        #[source]
        source: InfoError,
    },
    /// A log file can't be opened or read, or isn't a regular file.
    #[error("{}: {error}", file.display())]
    File {
        /// The file.
        file: PathBuf,
        /// Why.
        error: std::io::Error,
    },
    /// Where the entries go failed (a closed pipe): reading stops.
    #[error(transparent)]
    Sink(std::io::Error),
    /// Its container can't be found, or was made without logs
    /// ([`of_runtime`]).
    #[error(transparent)]
    Container(#[from] crate::container::ContainerError),
    /// The directory isn't the one the container was created with: another
    /// was put at its path since.
    #[error("{} isn't the log directory its container was created with", dir.display())]
    Replaced {
        /// The log directory's path.
        dir: PathBuf,
    },
}

/// Reads the logs in `dir`, as `options` say, handing each line to `sink`.
/// With [`LogsOptions::follow`], waits for more until `finished` says the
/// container has ended and its output is all in (then reads once more).
///
/// # Errors
///
/// [`LogsError`]: the directory's `log.json`, a file, or `sink`.
pub fn read(
    dir: &Path,
    options: &LogsOptions,
    finished: impl FnMut() -> bool,
    mut sink: impl FnMut(Entry) -> std::io::Result<()>,
) -> Result<ReadSummary, LogsError> {
    read_then(dir, options, finished, None, &mut sink)
}

/// Told how many entries [`LogsOptions::tail`] kept, just before they're
/// handed on (before anything is): to hand on older ones first.
pub type BeforeTail<'a> = Box<dyn FnOnce(usize) -> Result<(), LogsError> + 'a>;

/// [`read`], with `before_tail` told how many entries the tail kept: to
/// hand on entries read elsewhere first (a container's older starts').
///
/// # Errors
///
/// As [`read`], and `before_tail`'s.
pub fn read_then<'a>(
    dir: &Path,
    options: &LogsOptions,
    mut finished: impl FnMut() -> bool,
    before_tail: Option<BeforeTail<'a>>,
    sink: &'a mut impl FnMut(Entry) -> std::io::Result<()>,
) -> Result<ReadSummary, LogsError> {
    let directory = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .map_err(|error| file_error(dir, error))?;
    if let Some(expected) = options.identity {
        let metadata = directory
            .metadata()
            .map_err(|error| file_error(dir, error))?;
        if (metadata.dev(), metadata.ino()) != (expected.dev, expected.ino) {
            return Err(LogsError::Replaced {
                dir: dir.to_owned(),
            });
        }
    }
    // A container still being created may not have written it yet: a
    // follower waits for it.
    let info = loop {
        match LogInfo::read_at(&directory) {
            // Its reader gone meanwhile: nothing to read, and no one to
            // tell (F78).
            Err(InfoError::Read(error))
                if options.follow
                    && error.kind() == std::io::ErrorKind::NotFound
                    && options.reader_gone() =>
            {
                return Ok(ReadSummary::default());
            }
            Err(InfoError::Read(error))
                if options.follow
                    && error.kind() == std::io::ErrorKind::NotFound
                    && !finished() =>
            {
                std::thread::sleep(POLL);
            }
            result => {
                break result.map_err(|source| LogsError::Info {
                    dir: dir.to_owned(),
                    source,
                })?;
            }
        }
    };
    let mut reader = Reader::new(dir, directory, *options, sink);
    reader.before_tail = before_tail;
    let (files, mut current) = reader.chain(info.settings.max_files)?;
    for mut file in files {
        reader.read_to_end(&mut file)?;
        reader.finished_file();
    }
    if let Some(current) = &mut current {
        reader.read_to_end(current)?;
    }
    // What's there so far (joined, and `--tail`'s lines), then each record
    // as it comes (partials too).
    reader.end()?;
    if !options.follow {
        return Ok(reader.stats);
    }
    reader.following = true;
    // `container.log` deleted (by hand): the writer puts a new one in its
    // place at its next rotation, followed from there.
    let mut current = match current {
        Some(current) => current,
        None => loop {
            let done = finished();
            if let Some(current) = reader.open(CURRENT)? {
                break current;
            }
            if done {
                return Ok(reader.stats);
            }
            std::thread::sleep(POLL);
        },
    };
    loop {
        // Asked before looking: if it says done, what's read next is all.
        let done = finished();
        reader.read_to_end(&mut current)?;
        if let Some(next) = reader.after_rotation(&mut current, info.settings.max_files)? {
            current = next;
            continue;
        }
        if done {
            reader.end()?;
            return Ok(reader.stats);
        }
        std::thread::sleep(POLL);
    }
}

/// A file being read: its name (for messages), its descriptor, and its
/// inode (to find it again after a rotation).
struct LogFile {
    name: PathBuf,
    file: File,
    inode: u64,
}

/// Bytes read but not yet a whole line.
#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
    /// The line so far is too long to be a record: skipped to its end.
    skipping: bool,
}

struct Reader<'a, S> {
    /// The directory's path, for messages.
    dir: PathBuf,
    /// The directory, open: every file is opened through it, never by a
    /// path (as the writer does).
    directory: File,
    options: LogsOptions,
    lines: Lines,
    /// Each stream's line so far, joined from partial records (not when
    /// following): stdout's, then stderr's.
    joined: [Option<Entry>; 2],
    /// The last [`LogsOptions::tail`] entries, until they're handed on.
    tail: Option<VecDeque<Entry>>,
    stats: ReadSummary,
    sink: &'a mut S,
    /// Reused for each read.
    buffer: Vec<u8>,
    /// Past what was there (`--follow`): records go on as they come, a
    /// partial one too, not joined into lines.
    following: bool,
    /// Told how many the tail kept, before they're handed on.
    before_tail: Option<BeforeTail<'a>>,
}

impl<'a, S: FnMut(Entry) -> std::io::Result<()>> Reader<'a, S> {
    fn new(dir: &Path, directory: File, options: LogsOptions, sink: &'a mut S) -> Self {
        Self {
            dir: dir.to_owned(),
            directory,
            options,
            lines: Lines::default(),
            joined: [None, None],
            tail: options.tail.map(|_| VecDeque::new()),
            stats: ReadSummary::default(),
            sink,
            buffer: Vec::new(),
            following: false,
            before_tail: None,
        }
    }

    /// The files to read, oldest first, and `container.log` apart. Listed
    /// again if `container.log` was rotated meanwhile (its inode changed).
    fn chain(&self, max_files: u32) -> Result<(Vec<LogFile>, Option<LogFile>), LogsError> {
        for _ in 0..5 {
            let before = self.inode_of(CURRENT);
            let mut files = Vec::new();
            for n in (1..max_files).rev() {
                if let Some(file) = self.open(&older(n))? {
                    files.push(file);
                }
            }
            let current = self.open(CURRENT)?;
            if before == current.as_ref().map(|file| file.inode) {
                return Ok((files, current));
            }
        }
        Err(file_error(
            &self.dir,
            std::io::Error::other("rotated too often to be listed"),
        ))
    }

    /// `name` in the directory, if it's there: a regular file, never through
    /// a link, never waited on (a FIFO planted there).
    fn open(&self, name: &str) -> Result<Option<LogFile>, LogsError> {
        let path = self.dir.join(name);
        let file = match nix::fcntl::openat(
            &self.directory,
            name,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_NONBLOCK
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(nix::errno::Errno::ENOENT) => return Ok(None),
            Err(errno) => return Err(file_error(&path, errno.into())),
        };
        let metadata = file.metadata().map_err(|error| file_error(&path, error))?;
        if !metadata.is_file() {
            return Err(file_error(
                &path,
                std::io::Error::other("not a regular file"),
            ));
        }
        Ok(Some(LogFile {
            name: path,
            file,
            inode: metadata.ino(),
        }))
    }

    fn inode_of(&self, name: &str) -> Option<u64> {
        nix::sys::stat::fstatat(
            &self.directory,
            name,
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .ok()
        .map(|stat| stat.st_ino)
    }

    /// Reads `file` from where it is to its end, a line at a time.
    fn read_to_end(&mut self, file: &mut LogFile) -> Result<(), LogsError> {
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.resize(64 * 1024, 0);
        let result = self.read_with(file, &mut buffer);
        self.buffer = buffer;
        result
    }

    fn read_with(&mut self, file: &mut LogFile, buffer: &mut [u8]) -> Result<(), LogsError> {
        loop {
            let read = match file.file.read(buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(file_error(&file.name, error)),
            };
            let mut bytes = buffer.get(..read).unwrap_or_default();
            while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
                let (piece, rest) = bytes.split_at(end);
                bytes = rest.get(1..).unwrap_or_default();
                let skipping = std::mem::take(&mut self.lines.skipping);
                let mut line = std::mem::take(&mut self.lines.partial);
                if skipping || line.len() + piece.len() > MAX_RECORD_LINE {
                    self.stats.skipped += 1;
                } else {
                    line.extend_from_slice(piece);
                    self.record(&line)?;
                }
                line.clear();
                self.lines.partial = line;
            }
            if self.lines.partial.len() + bytes.len() > MAX_RECORD_LINE {
                self.lines.skipping = true;
                self.lines.partial.clear();
            } else if !self.lines.skipping {
                self.lines.partial.extend_from_slice(bytes);
            }
        }
    }

    /// One line of a file.
    fn record(&mut self, line: &[u8]) -> Result<(), LogsError> {
        let Ok(record) = parse_record(line) else {
            self.stats.skipped += 1;
            return Ok(());
        };
        if self.following {
            // As it comes: a partial now (a prompt), the rest later.
            return self.emit(Entry {
                time: record.time,
                stream: record.stream,
                text: record.text.to_vec(),
                complete: !record.partial,
            });
        }
        let slot = record.stream.index();
        let mut joined = self.joined[slot].take();
        // Too long to hold with this part: what there is goes on as a
        // piece, the rest to follow.
        if joined
            .as_ref()
            .is_some_and(|entry| entry.text.len() + record.text.len() > MAX_LINE)
            && let Some(piece) = joined.take()
        {
            self.emit(piece)?;
        }
        let mut entry = joined.unwrap_or_else(|| Entry {
            time: record.time,
            stream: record.stream,
            text: Vec::new(),
            complete: false,
        });
        entry.text.extend_from_slice(record.text);
        if !record.partial {
            entry.complete = true;
            return self.emit(entry);
        }
        self.joined[slot] = Some(entry);
        Ok(())
    }

    /// Hands `entry` on, or keeps it for the tail.
    fn emit(&mut self, entry: Entry) -> Result<(), LogsError> {
        if self.options.since.is_some_and(|since| entry.time < since) {
            return Ok(());
        }
        if let (Some(tail), Some(keep)) = (&mut self.tail, self.options.tail) {
            tail.push_back(entry);
            while tail.len() > keep {
                tail.pop_front();
            }
            return Ok(());
        }
        (self.sink)(entry).map_err(LogsError::Sink)
    }

    /// Hands on the tail kept so far; from now on entries go straight on.
    fn flush_tail(&mut self) -> Result<(), LogsError> {
        let Some(kept) = self.tail.take() else {
            return Ok(());
        };
        if let Some(before_tail) = self.before_tail.take() {
            before_tail(kept.len())?;
        }
        for entry in kept {
            (self.sink)(entry).map_err(LogsError::Sink)?;
        }
        Ok(())
    }

    /// The end of what there is: lines still joining go as they are.
    fn end(&mut self) -> Result<(), LogsError> {
        for slot in 0..2 {
            if let Some(entry) = self.joined[slot].take() {
                self.emit(entry)?;
            }
        }
        self.flush_tail()
    }

    /// If `container.log` is no longer `current` (rotated), the file to go
    /// on with: having read `current` to its end, the one after it in the
    /// chain (found by inode; the oldest there is if it went out of the
    /// chain altogether). The files between are read on the way.
    fn after_rotation(
        &mut self,
        current: &mut LogFile,
        max_files: u32,
    ) -> Result<Option<LogFile>, LogsError> {
        if self.inode_of(CURRENT) == Some(current.inode) {
            return Ok(None);
        }
        // The new `container.log` first: between the rename and the new
        // file there's none yet, and then nothing is read (or the files in
        // between would be read again on the next poll).
        let Some(mut next) = self.open(CURRENT)? else {
            return Ok(None);
        };
        // Renamed, `current` takes no more: what the writer put in it just
        // before is read now, or never.
        self.read_to_end(current)?;
        self.finished_file();
        let position = (1..max_files).find(|&n| self.inode_of(&older(n)) == Some(current.inode));
        // From the next newer one: `.k-1` … `.1`, then `container.log`.
        let first = position.map_or(max_files.saturating_sub(1), |n| n.saturating_sub(1));
        for n in (1..=first).rev() {
            if let Some(mut file) = self.open(&older(n))? {
                // Rotated again while this looked: `next` is among them,
                // and read once, below.
                if file.inode == next.inode {
                    continue;
                }
                self.read_to_end(&mut file)?;
                self.finished_file();
            }
        }
        self.read_to_end(&mut next)?;
        Ok(Some(next))
    }

    /// A file read to its end that takes no more: a line it didn't end is
    /// torn (a crash), never continued in the next file.
    fn finished_file(&mut self) {
        if !self.lines.partial.is_empty() || self.lines.skipping {
            self.stats.skipped += 1;
        }
        self.lines = Lines::default();
    }
}

fn file_error(file: &Path, error: std::io::Error) -> LogsError {
    LogsError::File {
        file: file.to_owned(),
        error,
    }
}

/// Whose logs [`of_runtime`] reads.
#[derive(Debug, Clone, Copy)]
pub enum RuntimeLogs<'a> {
    /// A runtime container's (`shipyard-runtime`), by its ID.
    Container(&'a str),
    /// Those in a directory (a container deleted since): followed until
    /// the caller stops.
    Dir(&'a Path),
}

/// The logs `which` names, under the runtime's state directory `root`
/// (`shipyard-runtime logs`), as `options` say, each line to `sink`.
/// Following ends once the monitor has written the container's end (its
/// logs complete), or the container is deleted, or its ID names another's
/// since (deleted and made again: another monitor, whatever inode its
/// directory got).
///
/// # Errors
///
/// No such container, one made without logs, logs that can't be read, or
/// `sink` (its [`LogsError::Sink`]).
pub fn of_runtime(
    root: &Path,
    which: RuntimeLogs<'_>,
    options: &LogsOptions,
    sink: impl FnMut(Entry) -> std::io::Result<()>,
) -> Result<ReadSummary, LogsError> {
    use crate::container;
    let (dir, identity, instance) = match which {
        RuntimeLogs::Container(id) => {
            let logs = container::logs_of(root, id)?;
            (logs.dir, logs.identity, logs.instance)
        }
        RuntimeLogs::Dir(dir) => (dir.to_owned(), None, None),
    };
    // Only the directory the container was created with: not another put
    // at its path since.
    let options = LogsOptions {
        identity,
        ..*options
    };
    let finished = || {
        options.reader_gone()
            || match which {
                RuntimeLogs::Container(id) => container::logs_of(root, id).map_or(true, |logs| {
                    logs.finished
                        || logs.dir != dir
                        || logs.identity != identity
                        || logs.instance != instance
                }),
                RuntimeLogs::Dir(_) => false,
            }
    };
    read(&dir, &options, finished, sink)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Write as _;

    use crate::logs::cri::write_record;
    use crate::logs::info::{INFO_FILE, LogSettings};

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "shipyard-runtime-logs-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(
                path.join(INFO_FILE),
                serde_json_like(LogSettings {
                    max_size: 64 * 1024,
                    max_files: 3,
                }),
            )
            .unwrap();
            Self(path)
        }

        fn write(&self, name: &str, records: &[(u64, Stream, bool, &str)]) {
            let mut out = Vec::new();
            for &(secs, stream, partial, text) in records {
                let time = Timestamp::from_unix(i64::try_from(secs).unwrap(), 0).unwrap();
                write_record(&mut out, time, stream, partial, text.as_bytes());
            }
            std::fs::write(self.0.join(name), out).unwrap();
        }

        fn read(&self, options: LogsOptions) -> (Vec<String>, ReadSummary) {
            let mut lines = Vec::new();
            let read = read(
                &self.0,
                &options,
                || true,
                |entry| {
                    lines.push(format!(
                        "{} {}{}",
                        entry.stream.name(),
                        String::from_utf8_lossy(&entry.text),
                        if entry.complete { "" } else { "…" }
                    ));
                    Ok(())
                },
            )
            .unwrap();
            (lines, read)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn serde_json_like(settings: LogSettings) -> String {
        format!(
            r#"{{"format":"cri","version":1,"maxSize":{},"maxFiles":{}}}"#,
            settings.max_size, settings.max_files
        )
    }

    const OUT: Stream = Stream::Stdout;
    const ERR: Stream = Stream::Stderr;

    #[test]
    fn files_are_read_oldest_first_and_partials_joined() {
        let scratch = Scratch::new();
        scratch.write(&older(2), &[(1, OUT, false, "one")]);
        scratch.write(&older(1), &[(2, OUT, true, "tw"), (2, ERR, false, "oops")]);
        scratch.write(CURRENT, &[(3, OUT, false, "o"), (4, OUT, false, "three")]);
        let (lines, read) = scratch.read(LogsOptions::default());
        assert_eq!(
            lines,
            ["stdout one", "stderr oops", "stdout two", "stdout three"]
        );
        assert_eq!(read.skipped, 0);
    }

    #[test]
    fn tail_and_since_pick_the_lines() {
        let scratch = Scratch::new();
        scratch.write(
            CURRENT,
            &[
                (10, OUT, false, "a"),
                (20, OUT, false, "b"),
                (30, OUT, false, "c"),
                (40, OUT, false, "d"),
            ],
        );
        let tail = |n| LogsOptions {
            tail: Some(n),
            ..LogsOptions::default()
        };
        assert_eq!(scratch.read(tail(2)).0, ["stdout c", "stdout d"]);
        assert_eq!(scratch.read(tail(0)).0, Vec::<String>::new());
        assert_eq!(scratch.read(tail(9)).0.len(), 4);
        let since = LogsOptions {
            since: Timestamp::from_unix(20, 0),
            tail: Some(1),
            ..LogsOptions::default()
        };
        assert_eq!(scratch.read(since).0, ["stdout d"]);
        let since = LogsOptions {
            since: Timestamp::from_unix(25, 0),
            ..LogsOptions::default()
        };
        assert_eq!(scratch.read(since).0, ["stdout c", "stdout d"]);
    }

    /// A torn line (a crash in the middle of a write) or one that isn't a
    /// record is skipped and counted; the rest is read.
    #[test]
    fn what_isnt_a_record_is_skipped() {
        let scratch = Scratch::new();
        scratch.write(CURRENT, &[(1, OUT, false, "before")]);
        let mut text = std::fs::read(scratch.0.join(CURRENT)).unwrap();
        text.extend_from_slice(b"2026-10-10T06:16:19.0000");
        text.extend_from_slice(b"not a record at all\n");
        text.extend(vec![b'x'; MAX_RECORD_LINE * 3]);
        text.push(b'\n');
        let mut after = Vec::new();
        write_record(
            &mut after,
            Timestamp::from_unix(3, 0).unwrap(),
            OUT,
            false,
            b"after",
        );
        text.extend_from_slice(&after);
        std::fs::write(scratch.0.join(CURRENT), text).unwrap();
        let (lines, read) = scratch.read(LogsOptions::default());
        assert_eq!(lines, ["stdout before", "stdout after"]);
        assert_eq!(read.skipped, 2);
    }

    #[test]
    fn a_newer_version_is_refused() {
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join(INFO_FILE), r#"{"format":"cri","version":2}"#).unwrap();
        let result = read(&scratch.0, &LogsOptions::default(), || true, |_| Ok(()));
        assert!(matches!(
            result,
            Err(LogsError::Info {
                source: InfoError::UnknownVersion { found: 2 },
                ..
            })
        ));
    }

    /// A FIFO planted as a log file is refused, not waited on.
    #[test]
    fn a_file_that_isnt_regular_is_refused() {
        let scratch = Scratch::new();
        nix::unistd::mkfifo(&scratch.0.join(CURRENT), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let result = read(&scratch.0, &LogsOptions::default(), || true, |_| Ok(()));
        assert!(matches!(result, Err(LogsError::File { .. })), "{result:?}");
    }

    /// A record written to `container.log` after the follower last read it
    /// and just before it's rotated (between the follower's read and its
    /// look at the names) is read, from the old file, before the new one.
    #[test]
    fn a_record_just_before_a_rotation_is_read() {
        let scratch = Scratch::new();
        scratch.write(CURRENT, &[(1, OUT, false, "first")]);
        let mut lines = Vec::new();
        {
            let mut sink = |entry: Entry| {
                lines.push(String::from_utf8_lossy(&entry.text).into_owned());
                Ok(())
            };
            let directory = File::open(&scratch.0).unwrap();
            let options = LogsOptions {
                follow: true,
                ..LogsOptions::default()
            };
            let mut reader = Reader::new(&scratch.0, directory, options, &mut sink);
            reader.following = true;
            let mut current = reader.open(CURRENT).unwrap().unwrap();
            reader.read_to_end(&mut current).unwrap();
            // In the window: appended, then rotated.
            let mut late = Vec::new();
            write_record(
                &mut late,
                Timestamp::from_unix(2, 0).unwrap(),
                OUT,
                false,
                b"late",
            );
            std::fs::OpenOptions::new()
                .append(true)
                .open(scratch.0.join(CURRENT))
                .unwrap()
                .write_all(&late)
                .unwrap();
            std::fs::rename(scratch.0.join(CURRENT), scratch.0.join(older(1))).unwrap();
            std::fs::write(scratch.0.join(CURRENT), b"").unwrap();
            assert!(reader.after_rotation(&mut current, 3).unwrap().is_some());
        }
        assert_eq!(lines, ["first", "late"]);
    }

    /// Following logs whose `container.log` was deleted (by hand): what the
    /// older files hold, then the new `container.log` the writer puts in
    /// place at its next rotation, not an end at once.
    /// A follower waiting for a directory's `log.json` whose reader goes
    /// ends with nothing read, not an error (F78); one whose container ends
    /// without writing it still fails (the control).
    #[test]
    fn a_follower_waiting_for_its_logs_ends_when_its_reader_goes() {
        let scratch = Scratch::new();
        std::fs::remove_file(scratch.0.join(INFO_FILE)).unwrap();
        let follow = |gone| LogsOptions {
            follow: true,
            gone,
            ..LogsOptions::default()
        };
        // Its container "ends" at the third look: a follower that doesn't
        // ask after its reader fails then, where it should have ended at
        // its first.
        let read_with = |options: &LogsOptions| {
            let looks = Cell::new(0);
            read(
                &scratch.0,
                options,
                || {
                    looks.set(looks.get() + 1);
                    looks.get() > 2
                },
                |_| Ok(()),
            )
        };
        let summary = read_with(&follow(Some(|| true))).unwrap();
        assert_eq!(summary.skipped, 0);
        assert!(matches!(
            read_with(&follow(None)),
            Err(LogsError::Info { .. })
        ));
    }

    #[test]
    fn following_waits_for_a_deleted_current_file() {
        let scratch = Scratch::new();
        scratch.write(&older(1), &[(1, OUT, false, "old")]);
        let polls = Cell::new(0);
        let dir = scratch.0.clone();
        let mut lines = Vec::new();
        read(
            &scratch.0,
            &LogsOptions {
                follow: true,
                ..LogsOptions::default()
            },
            || {
                polls.set(polls.get() + 1);
                if polls.get() == 2 {
                    let mut new = Vec::new();
                    write_record(
                        &mut new,
                        Timestamp::from_unix(2, 0).unwrap(),
                        OUT,
                        false,
                        b"new",
                    );
                    std::fs::write(dir.join(CURRENT), new).unwrap();
                }
                polls.get() > 3
            },
            |entry| {
                lines.push(String::from_utf8_lossy(&entry.text).into_owned());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(lines, ["old", "new"]);
    }

    /// Following: lines as they come, then across two rotations between
    /// polls, nothing lost, until the container has finished.
    #[test]
    fn following_survives_rotations() {
        let scratch = Scratch::new();
        scratch.write(CURRENT, &[(1, OUT, false, "first")]);
        let polls = Cell::new(0);
        let dir = scratch.0.clone();
        let mut lines = Vec::new();
        read(
            &scratch.0,
            &LogsOptions {
                follow: true,
                ..LogsOptions::default()
            },
            || {
                polls.set(polls.get() + 1);
                match polls.get() {
                    1 => {
                        // Two rotations at once: "first"'s file is now .2.
                        std::fs::rename(dir.join(older(1)), dir.join(older(2))).ok();
                        std::fs::rename(dir.join(CURRENT), dir.join(older(2))).unwrap();
                        let mut middle = Vec::new();
                        write_record(
                            &mut middle,
                            Timestamp::from_unix(2, 0).unwrap(),
                            OUT,
                            false,
                            b"second",
                        );
                        std::fs::write(dir.join(older(1)), middle).unwrap();
                        let mut last = Vec::new();
                        write_record(
                            &mut last,
                            Timestamp::from_unix(3, 0).unwrap(),
                            OUT,
                            true,
                            b"pro",
                        );
                        std::fs::write(dir.join(CURRENT), last).unwrap();
                        false
                    }
                    2 => false,
                    _ => true,
                }
            },
            |entry| {
                lines.push((
                    String::from_utf8_lossy(&entry.text).into_owned(),
                    entry.complete,
                ));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            lines,
            [
                ("first".to_owned(), true),
                ("second".to_owned(), true),
                // When following, a partial goes on as it is (a prompt).
                ("pro".to_owned(), false),
            ]
        );
    }
}
