//! Writing a container's output as CRI records, rotated by size and count:
//! the monitor's half of the logs.
//!
//! The directory holds `log.json` (written first, [`super::info`]), then
//! `container.log`, the newest, and `container.log.1` … `.N-1`, older and
//! older. Everything is opened through the directory's descriptor, never a
//! path, and never through a symbolic link.
//!
//! **One record, one `write`** to a file opened `O_APPEND` (TLPI §5.1): each
//! write lands whole at the end. One that doesn't is finished, as
//! `write_all` would: a write cut short only says the rest didn't fit, and
//! the next one says why (`EFBIG`, `ENOSPC`). A record that can't be
//! finished is cut off again (`ftruncate` back by what of it was written,
//! from the file's own length) and lost; only a kill in the middle of a
//! write that spans pages can leave a torn line behind, which a reader skips.
//!
//! **Only a file-size limit rotates early** (a caller's `ulimit -f`, TLPI
//! §36.3: `EFBIG`, `SIGXFSZ` ignored): in a new file, the record fits again.
//! A full disk (`ENOSPC`) doesn't: rotating would drop the oldest file to
//! make room for an almost empty one, history lost for a few bytes. The
//! record is counted dropped, and the files are left as they are.
//!
//! **Rotation is renames** (TLPI §18.4): before a record that would take
//! `container.log` past its size, `.N-2` becomes `.N-1` (replacing it: the
//! oldest goes), … `container.log` becomes `.1`, and a new `container.log`
//! is opened. Each rename is atomic; a reader that has a file open keeps
//! reading it through the renames.

use std::io::Write as _;
use std::os::fd::{AsFd, OwnedFd};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::cri::{MAX_TEXT, Stream, write_record};
use super::info::{INFO_FILE, LogInfo, LogSettings};
use super::time::Timestamp;

/// The newest file's name.
pub const CURRENT: &str = "container.log";

/// The next newest file, while a rotation makes it: made before anything is
/// renamed, so a rotation that can't make it (a full disk) moves nothing.
const NEXT: &str = ".container.log.next";

/// The name of the `n`th older file (`n` from 1).
#[must_use]
pub fn older(n: u32) -> String {
    format!("{CURRENT}.{n}")
}

/// A container's logs, open for writing.
#[derive(Debug)]
pub struct LogWriter {
    dir: OwnedFd,
    settings: LogSettings,
    file: std::fs::File,
    /// Bytes in `file`.
    size: u64,
    /// How far past `max_size` it may grow before rotation is tried again,
    /// after a try failed (0 once one works): another `max_size` each time,
    /// not every record.
    deferred: u64,
    /// The logs are gone (their directory deleted): nothing written lands
    /// anywhere anyone can read, so nothing more is written.
    gone: bool,
    /// Each stream's line so far, not ended by a newline yet (at most
    /// [`MAX_TEXT`]): stdout's, then stderr's.
    pending: [Vec<u8>; 2],
    /// One record, formatted.
    record: Vec<u8>,
    /// Records that couldn't be written (a full disk, a file-size limit even
    /// in a new file, or logs whose directory is gone).
    dropped: u64,
    /// Whether each stream's last record was partial: its line must still
    /// be ended (`finish`), or a reader waits for the rest forever.
    last_partial: [bool; 2],
}

impl LogWriter {
    /// Starts the logs in the directory open at `dir` (empty: made for
    /// them): `log.json`, then an empty `container.log`. `settings` are
    /// [validated](LogSettings::validate).
    ///
    /// # Errors
    ///
    /// Settings out of bounds ([`std::io::ErrorKind::InvalidInput`]), or
    /// either file can't be written.
    pub fn create(dir: OwnedFd, settings: LogSettings) -> std::io::Result<Self> {
        let settings = settings
            .validate()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        let info = serde_json::to_vec(&LogInfo::new(settings))?;
        let temporary = format!(".{INFO_FILE}.tmp");
        let mut file = create_at(&dir, &temporary, OFlag::O_EXCL)?;
        file.write_all(&info)?;
        file.sync_all()?;
        // Linked, not renamed into place: a link never replaces a file, so
        // logs already there (another container's) are left alone.
        let linked = nix::unistd::linkat(
            &dir,
            temporary.as_str(),
            &dir,
            INFO_FILE,
            nix::fcntl::AtFlags::empty(),
        );
        let _ = nix::unistd::unlinkat(
            &dir,
            temporary.as_str(),
            nix::unistd::UnlinkatFlags::NoRemoveDir,
        );
        linked?;
        let file = create_at(&dir, CURRENT, OFlag::O_EXCL)?;
        Ok(Self {
            dir,
            settings,
            file,
            size: 0,
            deferred: 0,
            gone: false,
            pending: [Vec::new(), Vec::new()],
            record: Vec::with_capacity(MAX_TEXT + 64),
            dropped: 0,
            last_partial: [false, false],
        })
    }

    /// How many records were lost to write errors.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Takes `bytes` the program wrote on `stream`: a record for each line
    /// they end, and one for each [`MAX_TEXT`] of a line that's longer.
    pub fn push(&mut self, stream: Stream, mut bytes: &[u8]) {
        let slot = stream.index();
        while !bytes.is_empty() {
            let pending = self.pending.get(slot).map_or(0, Vec::len);
            let room = MAX_TEXT - pending;
            if let Some(end) = bytes.iter().take(room).position(|&byte| byte == b'\n') {
                let (line, rest) = bytes.split_at(end);
                self.extend(slot, line);
                self.emit(stream, false);
                bytes = rest.get(1..).unwrap_or_default();
            } else {
                let (part, rest) = bytes.split_at(room.min(bytes.len()));
                self.extend(slot, part);
                bytes = rest;
                if self.pending.get(slot).map_or(0, Vec::len) == MAX_TEXT {
                    self.emit(stream, true);
                }
            }
        }
    }

    /// `stream` ended: a line it didn't end is written as a full one
    /// (containerd's way: a reader joining partials then never waits for
    /// a rest that can't come).
    pub fn finish(&mut self, stream: Stream) {
        // Its last record partial (written when it went quiet) and nothing
        // since: an empty one ends the line.
        if self.has_pending(stream) || self.last_partial[stream.index()] {
            self.emit(stream, false);
        }
    }

    /// Writes what `stream` has of a line so far as a partial record, so it
    /// can be read before the line ends (a prompt): for a stream that's been
    /// quiet a while.
    pub fn flush(&mut self, stream: Stream) {
        if self.has_pending(stream) {
            self.emit(stream, true);
        }
    }

    /// Whether `stream` has part of a line not written yet.
    #[must_use]
    pub fn has_pending(&self, stream: Stream) -> bool {
        self.pending
            .get(stream.index())
            .is_some_and(|line| !line.is_empty())
    }

    fn extend(&mut self, slot: usize, bytes: &[u8]) {
        if let Some(pending) = self.pending.get_mut(slot) {
            pending.extend_from_slice(bytes);
        }
    }

    /// Writes `stream`'s pending text as one record.
    fn emit(&mut self, stream: Stream, partial: bool) {
        let mut text = std::mem::take(&mut self.pending[stream.index()]);
        self.record.clear();
        write_record(&mut self.record, Timestamp::now(), stream, partial, &text);
        text.clear();
        self.pending[stream.index()] = text;
        self.last_partial[stream.index()] = partial;
        let length = u64::try_from(self.record.len()).unwrap_or(u64::MAX);
        let limit = self.settings.max_size.saturating_add(self.deferred);
        if self.size > 0 && self.size.saturating_add(length) > limit {
            self.rotate_or_wait();
        }
        if self.gone {
            self.dropped += 1;
            return;
        }
        match self.append_whole() {
            Ok(()) => return,
            // A file-size limit below `max_size`: in a new file, it fits
            // again.
            Err(Errno::EFBIG) if self.size > 0 => {
                self.rotate_or_wait();
                if self.append_whole().is_ok() {
                    return;
                }
            }
            Err(_) => {}
        }
        self.dropped += 1;
    }

    /// Writes the formatted record whole, or not at all: what of it a write
    /// cut short left is cut off again, so the file ends with a whole record.
    /// The error is why it couldn't be finished.
    fn append_whole(&mut self) -> Result<(), Errno> {
        let mut written = 0;
        let failed = loop {
            match self
                .file
                .write(self.record.get(written..).unwrap_or_default())
            {
                Ok(0) => break Errno::EIO,
                Ok(n) => {
                    written += n;
                    if written >= self.record.len() {
                        let length = u64::try_from(written).unwrap_or(u64::MAX);
                        self.size = self.size.saturating_add(length);
                        return Ok(());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => break err.raw_os_error().map_or(Errno::EIO, Errno::from_raw),
            }
        };
        if written > 0 {
            // Back by what this record wrote, from the file's own length:
            // `size` may not be it (the file truncated by hand since).
            let length = nix::sys::stat::fstat(&self.file)
                .ok()
                .and_then(|stat| u64::try_from(stat.st_size).ok());
            let whole = length
                .map(|length| length.saturating_sub(u64::try_from(written).unwrap_or(u64::MAX)));
            match whole {
                Some(whole) if self.file.set_len(whole).is_ok() => self.size = whole,
                // Can't be cut back (an append-only file, say): ended, so the
                // next record isn't glued to it (a reader skips the torn
                // line, and would skip that record with it).
                _ => {
                    let ended = self.file.write(b"\n").is_ok_and(|n| n == 1);
                    self.size = length.unwrap_or(self.size) + u64::from(ended);
                }
            }
        }
        Err(failed)
    }

    /// Rotates; if it can't, writes on in the file it has, and tries again
    /// after another `max_size` (not at every record). Unless the file it
    /// has is gone with its directory: then nothing more is written, rather
    /// than filling the disk where nobody can see it.
    fn rotate_or_wait(&mut self) {
        match self.rotate() {
            Ok(()) => self.deferred = 0,
            Err(err) => {
                let unlinked =
                    nix::sys::stat::fstat(&self.file).is_ok_and(|file| file.st_nlink == 0);
                if unlinked {
                    tracing::debug!(%err, "the logs are gone: writing no more");
                    self.gone = true;
                } else {
                    tracing::debug!(%err, "rotating the logs; writing on in the same file");
                    // The next try once it has grown by another `max_size`.
                    self.deferred = self.size;
                }
            }
        }
    }

    /// Makes the next file, shifts the others down one (the oldest
    /// replaced), and puts the new one in place as `container.log`. A step
    /// that fails is undone as far as it can be: `container.log` is always
    /// there, and the older files keep their order.
    fn rotate(&mut self) -> std::io::Result<()> {
        let next = create_at(&self.dir, NEXT, OFlag::O_EXCL)?;
        let forget_next = |dir: &OwnedFd| {
            let _ = nix::unistd::unlinkat(dir, NEXT, nix::unistd::UnlinkatFlags::NoRemoveDir);
        };
        for n in (1..self.settings.max_files.saturating_sub(1)).rev() {
            match nix::fcntl::renameat(
                &self.dir,
                older(n).as_str(),
                &self.dir,
                older(n + 1).as_str(),
            ) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(errno) => {
                    forget_next(&self.dir);
                    return Err(errno.into());
                }
            }
        }
        // `container.log` deleted (by hand, say): the new file takes its
        // place all the same, and the deleted one stops growing.
        let moved = match nix::fcntl::renameat(&self.dir, CURRENT, &self.dir, older(1).as_str()) {
            Ok(()) => true,
            Err(Errno::ENOENT) => false,
            Err(errno) => {
                forget_next(&self.dir);
                return Err(errno.into());
            }
        };
        if let Err(errno) = nix::fcntl::renameat(&self.dir, NEXT, &self.dir, CURRENT) {
            if moved {
                let _ = nix::fcntl::renameat(&self.dir, older(1).as_str(), &self.dir, CURRENT);
            }
            forget_next(&self.dir);
            return Err(errno.into());
        }
        self.file = next;
        self.size = 0;
        Ok(())
    }
}

/// Creates `name` in `dir` for appending, 0600 whatever the umask, never
/// through a symbolic link.
fn create_at(dir: impl AsFd, name: &str, extra: OFlag) -> std::io::Result<std::fs::File> {
    let fd = nix::fcntl::openat(
        dir,
        name,
        OFlag::O_WRONLY
            | OFlag::O_APPEND
            | OFlag::O_CREAT
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | extra,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?;
    // The mode given is filtered by the umask: set it, so it's exactly this.
    nix::sys::stat::fchmod(&fd, Mode::S_IRUSR | Mode::S_IWUSR)?;
    Ok(std::fs::File::from(fd))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::super::cri::parse_record;
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "shipyard-logs-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn writer(&self, settings: LogSettings) -> LogWriter {
            let dir = nix::fcntl::open(
                &self.0,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            LogWriter::create(dir, settings).unwrap()
        }

        /// The records in `name`: (stream, partial, text).
        fn records(&self, name: &str) -> Vec<(Stream, bool, Vec<u8>)> {
            read_records(&self.0.join(name))
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read_records(path: &Path) -> Vec<(Stream, bool, Vec<u8>)> {
        let text = std::fs::read(path).unwrap();
        assert!(
            text.is_empty() || text.ends_with(b"\n"),
            "whole records only"
        );
        text.split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let record = parse_record(line).unwrap();
                (record.stream, record.partial, record.text.to_vec())
            })
            .collect()
    }

    const BIG: LogSettings = LogSettings {
        max_size: 1 << 30,
        max_files: 2,
    };

    #[test]
    fn lines_become_records_each_on_its_stream() {
        let scratch = Scratch::new();
        let mut logs = scratch.writer(BIG);
        logs.push(Stream::Stdout, b"one\ntw");
        logs.push(Stream::Stderr, b"oops\n");
        logs.push(Stream::Stdout, b"o\n\nthr");
        // Quiet a while: what there is goes out as a partial.
        logs.flush(Stream::Stdout);
        logs.push(Stream::Stdout, b"ee");
        logs.finish(Stream::Stdout);
        logs.finish(Stream::Stderr);
        assert_eq!(
            scratch.records(CURRENT),
            [
                (Stream::Stdout, false, b"one".to_vec()),
                (Stream::Stderr, false, b"oops".to_vec()),
                (Stream::Stdout, false, b"two".to_vec()),
                (Stream::Stdout, false, b"".to_vec()),
                (Stream::Stdout, true, b"thr".to_vec()),
                // Never ended, but nothing more can come: full.
                (Stream::Stdout, false, b"ee".to_vec()),
            ]
        );
        assert_eq!(scratch.files(), [CURRENT, INFO_FILE]);
    }

    /// A line longer than a record holds is cut: `P` records of
    /// [`MAX_TEXT`], then the rest as `F`. Nothing is held beyond that.
    #[test]
    fn a_long_line_is_cut_into_partials() {
        let scratch = Scratch::new();
        let mut logs = scratch.writer(BIG);
        let line = vec![b'x'; MAX_TEXT * 2 + 10];
        // In pieces that don't line up with the cuts.
        for chunk in line.chunks(1000) {
            logs.push(Stream::Stdout, chunk);
        }
        assert!(logs.pending[0].len() <= MAX_TEXT);
        logs.push(Stream::Stdout, b"\n");
        let records = scratch.records(CURRENT);
        let shape: Vec<(bool, usize)> = records
            .iter()
            .map(|(_, partial, text)| (*partial, text.len()))
            .collect();
        // 16 KiB: containerd's figure, which CRI readers expect.
        assert_eq!(shape, [(true, 16 * 1024), (true, 16 * 1024), (false, 10)]);
        // A line exactly a record's size ends in a record of its own.
        logs.push(Stream::Stderr, &vec![b'y'; MAX_TEXT]);
        logs.push(Stream::Stderr, b"\n");
        let records = scratch.records(CURRENT);
        let tail: Vec<(bool, usize)> = records[3..]
            .iter()
            .map(|(_, partial, text)| (*partial, text.len()))
            .collect();
        assert_eq!(tail, [(true, MAX_TEXT), (false, 0)]);
    }

    /// The files never hold more than their size (plus one record), nor
    /// are there more than their count; every record is in exactly one, in
    /// order, oldest in the highest number.
    #[test]
    fn rotation_keeps_the_bound_and_the_order() {
        let scratch = Scratch::new();
        let settings = LogSettings {
            max_size: LogSettings::MIN_SIZE,
            max_files: 3,
        };
        let mut logs = scratch.writer(settings);
        for n in 0..4000 {
            logs.push(
                Stream::Stdout,
                format!("line {n:04} {}\n", "z".repeat(40)).as_bytes(),
            );
        }
        assert_eq!(scratch.files(), [CURRENT, &older(1), &older(2), INFO_FILE]);
        let mut kept = Vec::new();
        for name in [older(2), older(1), CURRENT.to_owned()] {
            let size = std::fs::metadata(scratch.0.join(&name)).unwrap().len();
            assert!(size <= settings.max_size, "{name}: {size}");
            kept.extend(scratch.records(&name).into_iter().map(|(_, _, text)| text));
        }
        let numbers: Vec<u32> = kept
            .iter()
            .map(|text| std::str::from_utf8(&text[5..9]).unwrap().parse().unwrap())
            .collect();
        assert_eq!(*numbers.last().unwrap(), 3999);
        assert!(
            numbers.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "in order, none lost between the files"
        );
        assert_eq!(logs.dropped(), 0);
    }

    /// `container.log` deleted while the container writes (by hand, say):
    /// the next rotation puts a new one in its place, and the deleted one,
    /// which nobody can read, grows no more than a file's size.
    #[test]
    fn a_deleted_current_file_is_replaced_at_the_next_rotation() {
        let scratch = Scratch::new();
        let settings = LogSettings {
            max_size: LogSettings::MIN_SIZE,
            max_files: 3,
        };
        let mut logs = scratch.writer(settings);
        let line = format!("{}\n", "z".repeat(60));
        logs.push(Stream::Stdout, line.as_bytes());
        std::fs::remove_file(scratch.0.join(CURRENT)).unwrap();
        for _ in 0..2000 {
            logs.push(Stream::Stdout, line.as_bytes());
        }
        assert!(
            scratch.files().contains(&CURRENT.to_owned()),
            "{:?}",
            scratch.files()
        );
        let unlinked = nix::sys::stat::fstat(&logs.file).unwrap().st_nlink == 0;
        assert!(!unlinked, "still writing to the deleted file");
        assert_eq!(logs.dropped(), 0);
    }

    /// The whole directory deleted: rotation can't help, so nothing more is
    /// written (counted as dropped) rather than filling the disk out of
    /// sight.
    #[test]
    fn deleted_logs_stop_being_written() {
        let scratch = Scratch::new();
        let settings = LogSettings {
            max_size: LogSettings::MIN_SIZE,
            max_files: 3,
        };
        let mut logs = scratch.writer(settings);
        let line = format!("{}\n", "z".repeat(60));
        logs.push(Stream::Stdout, line.as_bytes());
        std::fs::remove_dir_all(&scratch.0).unwrap();
        for _ in 0..4000 {
            logs.push(Stream::Stdout, line.as_bytes());
        }
        let written = nix::sys::stat::fstat(&logs.file).unwrap().st_size;
        assert!(
            u64::try_from(written).unwrap() <= settings.max_size + 128,
            "{written} bytes out of sight"
        );
        assert!(logs.dropped() > 0);
    }

    /// A rotation that can't make its new file (here a directory is in
    /// its way: a full disk, in life) moves nothing: `container.log` stays,
    /// the older files are left alone, nothing is lost, and the next try
    /// comes a file's size later, not at every record.
    #[test]
    fn a_rotation_that_cant_happen_moves_nothing() {
        let scratch = Scratch::new();
        let settings = LogSettings {
            max_size: LogSettings::MIN_SIZE,
            max_files: 3,
        };
        let mut logs = scratch.writer(settings);
        std::fs::create_dir(scratch.0.join(NEXT)).unwrap();
        for n in 0..3000 {
            logs.push(
                Stream::Stdout,
                format!("line {n:04} {}\n", "z".repeat(40)).as_bytes(),
            );
        }
        assert_eq!(logs.dropped(), 0);
        assert_eq!(scratch.files(), [NEXT, CURRENT, INFO_FILE]);
        let kept = scratch.records(CURRENT);
        assert_eq!(kept.len(), 3000, "all of it, in the file it had");
        // Once it can, it does.
        std::fs::remove_dir(scratch.0.join(NEXT)).unwrap();
        for _ in 0..2000 {
            logs.push(Stream::Stdout, &[b'y'; 60]);
            logs.push(Stream::Stdout, b"\n");
        }
        assert!(scratch.files().contains(&older(1)));
        assert!(!scratch.files().contains(&NEXT.to_owned()));
    }

    /// A full disk (`ENOSPC`, as `/dev/full` gives) loses the record and
    /// moves nothing: rotating would drop the oldest file to make room for a
    /// few bytes.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_full_disk_drops_and_keeps_the_files() {
        let scratch = Scratch::new();
        let mut logs = scratch.writer(LogSettings {
            max_size: LogSettings::MIN_SIZE,
            max_files: 3,
        });
        logs.push(Stream::Stdout, b"kept\n");
        let before = scratch.records(CURRENT);
        logs.file = std::fs::File::options()
            .append(true)
            .open("/dev/full")
            .unwrap();
        logs.push(Stream::Stdout, b"lost\n");
        assert_eq!(logs.dropped(), 1);
        assert_eq!(scratch.files(), [CURRENT, INFO_FILE], "nothing rotated");
        assert_eq!(scratch.records(CURRENT), before);
    }

    /// A line written as a partial (when it went quiet) and never ended is
    /// ended at the stream's end, by an empty full record.
    #[test]
    fn a_partial_last_record_is_ended() {
        let scratch = Scratch::new();
        let mut logs = scratch.writer(BIG);
        logs.push(Stream::Stdout, b"prompt> ");
        logs.flush(Stream::Stdout);
        logs.finish(Stream::Stdout);
        logs.finish(Stream::Stderr);
        assert_eq!(
            scratch.records(CURRENT),
            [
                (Stream::Stdout, true, b"prompt> ".to_vec()),
                (Stream::Stdout, false, Vec::new()),
            ]
        );
    }

    #[test]
    fn settings_outside_the_bounds_are_refused() {
        let scratch = Scratch::new();
        let dir = nix::fcntl::open(
            &scratch.0,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        let one = LogSettings {
            max_size: LogSettings::MIN_SIZE,
            max_files: 1,
        };
        assert!(LogWriter::create(dir, one).is_err());
        assert!(scratch.files().is_empty(), "nothing written");
    }

    /// The files are the owner's alone whatever the umask, `log.json` is
    /// there before any record, and nothing is written through a link.
    #[test]
    fn files_are_private_and_never_followed() {
        let scratch = Scratch::new();
        // SAFETY: `umask` has no preconditions; restored below.
        let old = unsafe { libc::umask(0) };
        let mut logs = scratch.writer(BIG);
        // SAFETY: as above.
        unsafe { libc::umask(old) };
        logs.push(Stream::Stdout, b"x\n");
        for name in [CURRENT, INFO_FILE] {
            let mode = std::fs::metadata(scratch.0.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        assert!(
            LogInfo::read_at(
                nix::fcntl::open(
                    &scratch.0,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                    Mode::empty()
                )
                .unwrap()
            )
            .is_ok()
        );
        // A link planted where the next file goes is refused, not followed.
        let target = scratch.0.join("elsewhere");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, scratch.0.join("planted")).unwrap();
        let dir = nix::fcntl::open(
            &scratch.0,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        assert!(create_at(&dir, "planted", OFlag::empty()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"");
    }

    /// A directory that already has logs isn't started again (`O_EXCL`):
    /// two containers never share one.
    #[test]
    fn logs_are_never_started_over_others() {
        let scratch = Scratch::new();
        let _first = scratch.writer(BIG);
        let dir = nix::fcntl::open(
            &scratch.0,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )
        .unwrap();
        assert!(LogWriter::create(dir, BIG).is_err());
        assert_eq!(
            scratch.files(),
            [CURRENT, INFO_FILE],
            "nothing of the second left"
        );
    }
}
