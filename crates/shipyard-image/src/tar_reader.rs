//! A streaming reader for the tar archives image layers are made of.
//!
//! A layer comes from a registry, so every byte of it is untrusted. The
//! `tar` crate's iterator reads each PAX and GNU long-name header whole into
//! memory, with no limit (a 4 GiB "name" is a few kilobytes gzipped), and its
//! raw mode, which doesn't, also ignores the PAX size and IDs. So the layer
//! format is read here, block by block, with every limit checked *before*
//! the bytes it guards are read:
//!
//! - an archive is a sequence of 512-byte header blocks, each followed by
//!   the entry's data rounded up to a whole block, and ends with zero blocks;
//! - a header's fields are fixed-width: numbers in octal ASCII, or base-256
//!   (high bit set) when they don't fit, and a checksum of the block;
//! - extension entries change the *next* header: GNU `L`/`K` carry a long
//!   path or link target, POSIX `x` carries `key=value` records (a path, a
//!   size, IDs, extended attributes). They're capped at
//!   [`TarLimits::max_metadata`];
//! - POSIX global headers (`g`) are ignored, as Docker and containerd do; a
//!   global `path=` would otherwise rename every later entry.
//!
//! What a layer can't carry is refused, not skipped: sparse files (their
//! data isn't the file's contents), unknown entry types, data on entries
//! that have none (a directory or a link with a size would hide the bytes
//! after it from other tar readers), paths that climb with `..`.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Component, Path, PathBuf};

/// The size of a tar block.
const BLOCK: usize = 512;

/// Limits on what an archive may make the reader hold or walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TarLimits {
    /// The most extension bytes (long names, PAX records) read into memory
    /// for one entry, all its extension headers together.
    pub max_metadata: u64,
    /// The most headers (entries and extensions) read.
    pub max_headers: u64,
    /// The longest entry path or link target, in bytes.
    pub max_path: usize,
    /// The most components in an entry path.
    pub max_components: usize,
}

impl Default for TarLimits {
    fn default() -> Self {
        Self {
            max_metadata: 1024 * 1024,
            max_headers: 1_000_000,
            max_path: 4096,
            max_components: 256,
        }
    }
}

/// Why an archive couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum TarError {
    /// Reading the stream failed (including a limit on the stream itself).
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The archive isn't well-formed.
    #[error("malformed layer archive: {0}")]
    Malformed(String),
    /// The archive uses something a layer can't carry.
    #[error("unsupported in a layer: {0}")]
    Unsupported(String),
    /// An extension header is larger than the limit.
    #[error("a layer header extension of {size} bytes is over the {limit}-byte limit")]
    MetadataTooLarge {
        /// Its size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// The archive has more headers than the limit.
    #[error("the layer has more than {0} entries")]
    TooManyEntries(u64),
    /// A path climbs out of the layer, or is too long or too deep.
    #[error("the layer entry `{0}` has an unsafe path")]
    UnsafePath(String),
}

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A regular file, with data.
    File,
    /// A directory.
    Directory,
    /// A symbolic link; [`Entry::link`] is its target, as written.
    Symlink,
    /// A hard link to an earlier entry; [`Entry::link`] is that entry's path.
    Hardlink,
    /// A FIFO.
    Fifo,
    /// A character device.
    CharDevice,
    /// A block device.
    BlockDevice,
}

/// One entry's header, after its extensions are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What it is.
    pub kind: Kind,
    /// Its path under the layer root, cleaned: only normal components. Empty
    /// for the root itself (`./`).
    pub path: PathBuf,
    /// A link's target: raw for a symlink (resolved later, by the kernel,
    /// inside the container), cleaned like [`path`](Self::path) for a
    /// hardlink.
    pub link: Option<OsString>,
    /// The permission bits (`0o7777`).
    pub mode: u32,
    /// The owner.
    pub uid: u32,
    /// The group.
    pub gid: u32,
    /// The modification time: seconds, and nanoseconds.
    pub mtime: (i64, u32),
    /// The data's size (only a file has any).
    pub size: u64,
    /// Extended attributes (`SCHILY.xattr.*`), name and value.
    pub xattrs: Vec<(OsString, Vec<u8>)>,
}

/// Reads entries from a tar stream. The current entry's data is read from
/// the reader itself ([`Read`]); what's left of it is skipped by the next
/// call to [`next_entry`](Self::next_entry).
#[derive(Debug)]
pub struct TarReader<R> {
    inner: R,
    limits: TarLimits,
    /// Data bytes of the current entry not yet read.
    remaining: u64,
    /// Padding after the current entry's data.
    padding: u64,
    /// Headers read so far.
    headers: u64,
    /// Global PAX headers ignored.
    ignored_globals: u64,
    done: bool,
}

/// Extensions collected for the next entry.
#[derive(Default)]
struct Pending {
    path: Option<Vec<u8>>,
    link: Option<Vec<u8>>,
    size: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
    mtime: Option<(i64, u32)>,
    xattrs: Vec<(OsString, Vec<u8>)>,
}

impl<R: Read> TarReader<R> {
    /// A reader over `inner`.
    pub fn new(inner: R, limits: TarLimits) -> Self {
        Self {
            inner,
            limits,
            remaining: 0,
            padding: 0,
            headers: 0,
            ignored_globals: 0,
            done: false,
        }
    }

    /// How many POSIX global headers were ignored.
    pub fn ignored_globals(&self) -> u64 {
        self.ignored_globals
    }

    /// The underlying stream, positioned after the end-of-archive marker.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The next entry, or `None` at the end of the archive.
    ///
    /// # Errors
    ///
    /// A [`TarError`] for a stream failure, a malformed or unsupported
    /// header, or a limit.
    pub fn next_entry(&mut self) -> Result<Option<Entry>, TarError> {
        if self.done {
            return Ok(None);
        }
        self.skip_rest()?;
        let mut pending = Pending::default();
        // Every extension header of this entry together: one entry can't
        // gather more than the limit in many headers of under it each.
        let mut extensions: u64 = 0;
        loop {
            let Some(block) = self.read_header()? else {
                self.done = true;
                return Ok(None);
            };
            let type_flag = block[156];
            let size = number(&block[124..136], "size")?;
            match type_flag {
                b'x' | b'g' | b'L' | b'K' => {
                    extensions = extensions.saturating_add(size);
                    let data = self.read_extension(extensions, size)?;
                    match type_flag {
                        b'x' => apply_pax(&data, &mut pending)?,
                        b'g' => self.ignored_globals += 1,
                        b'L' => pending.path = Some(trim_nuls(data)),
                        _ => pending.link = Some(trim_nuls(data)),
                    }
                }
                _ => return self.entry(&block, type_flag, size, pending).map(Some),
            }
        }
    }

    /// Builds the entry from a header block and its extensions, and sets up
    /// its data for reading.
    fn entry(
        &mut self,
        block: &[u8; BLOCK],
        type_flag: u8,
        header_size: u64,
        pending: Pending,
    ) -> Result<Entry, TarError> {
        let raw_path = match pending.path {
            Some(path) => path,
            None => header_path(block),
        };
        let kind = match type_flag {
            // V7: no type, a trailing slash for a directory.
            0 if raw_path.last() == Some(&b'/') => Kind::Directory,
            b'0' | b'7' | 0 => Kind::File,
            b'1' => Kind::Hardlink,
            b'2' => Kind::Symlink,
            b'3' => Kind::CharDevice,
            b'4' => Kind::BlockDevice,
            b'5' => Kind::Directory,
            b'6' => Kind::Fifo,
            b'S' => return Err(TarError::Unsupported("a sparse file".to_owned())),
            other => {
                return Err(TarError::Unsupported(format!(
                    "the entry type `{}`",
                    char::from(other).escape_default()
                )));
            }
        };
        let size = pending.size.unwrap_or(header_size);
        if kind != Kind::File && size != 0 {
            // Other readers (Go's) read no data after these: a size would
            // hide what follows from them, and show it to us.
            return Err(TarError::Malformed(format!(
                "`{}` has {size} bytes of data but can't have any",
                String::from_utf8_lossy(&raw_path)
            )));
        }
        let path = clean_path(&raw_path, &self.limits)?;
        let link = match kind {
            Kind::Symlink | Kind::Hardlink => {
                let raw = match pending.link {
                    Some(link) => link,
                    None => field(&block[157..257]).to_vec(),
                };
                if raw.is_empty() || raw.len() > self.limits.max_path {
                    return Err(TarError::Malformed(format!(
                        "the link `{}` has an empty or too long target",
                        path.display()
                    )));
                }
                if kind == Kind::Hardlink {
                    Some(clean_path(&raw, &self.limits)?.into_os_string())
                } else {
                    Some(OsString::from_vec(raw))
                }
            }
            _ => None,
        };
        let uid = match pending.uid {
            Some(uid) => uid,
            None => id(&block[108..116], "uid")?,
        };
        let gid = match pending.gid {
            Some(gid) => gid,
            None => id(&block[116..124], "gid")?,
        };
        let mtime = match pending.mtime {
            Some(mtime) => mtime,
            None => (
                i64::try_from(number(&block[136..148], "mtime")?)
                    .map_err(|_| TarError::Malformed("mtime out of range".to_owned()))?,
                0,
            ),
        };
        let mode = u32::try_from(number(&block[100..108], "mode")? & 0o7777).unwrap_or(0);
        self.remaining = size;
        self.padding = padding(size);
        Ok(Entry {
            kind,
            path,
            link,
            mode,
            uid,
            gid,
            mtime,
            size,
            xattrs: pending.xattrs,
        })
    }

    /// Reads the next header block, checking its checksum; `None` at the end
    /// of the archive: a zero block (then another, or the end of the
    /// stream), or the stream ending between entries, as Go's reader takes
    /// it (a truncated layer still fails its `diff_id`).
    fn read_header(&mut self) -> Result<Option<[u8; BLOCK]>, TarError> {
        let mut block = [0u8; BLOCK];
        if !read_block(&mut self.inner, &mut block)? {
            return Ok(None);
        }
        if block.iter().all(|&b| b == 0) {
            // The end: a second zero block, or nothing, may follow.
            let mut second = [0u8; BLOCK];
            if read_block(&mut self.inner, &mut second)? && second.iter().any(|&b| b != 0) {
                return Err(TarError::Malformed(
                    "data after the end-of-archive marker".to_owned(),
                ));
            }
            return Ok(None);
        }
        self.headers += 1;
        if self.headers > self.limits.max_headers {
            return Err(TarError::TooManyEntries(self.limits.max_headers));
        }
        verify_checksum(&block)?;
        Ok(Some(block))
    }

    /// Reads an extension's `size` bytes of data, refusing them *before*
    /// reading if the entry's extensions come to over the limit (`total`).
    fn read_extension(&mut self, total: u64, size: u64) -> Result<Vec<u8>, TarError> {
        if total > self.limits.max_metadata {
            return Err(TarError::MetadataTooLarge {
                size: total,
                limit: self.limits.max_metadata,
            });
        }
        let length = usize::try_from(size).map_err(|_| TarError::MetadataTooLarge {
            size,
            limit: self.limits.max_metadata,
        })?;
        let mut data = vec![0u8; length];
        self.inner.read_exact(&mut data).map_err(early_end)?;
        skip(&mut self.inner, padding(size))?;
        Ok(data)
    }

    /// Skips what's left of the current entry's data, and its padding.
    fn skip_rest(&mut self) -> Result<(), TarError> {
        let rest = self.remaining.saturating_add(self.padding);
        self.remaining = 0;
        self.padding = 0;
        skip(&mut self.inner, rest)
    }
}

impl<R: Read> Read for TarReader<R> {
    /// Reads the current entry's data; 0 at its end.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = usize::try_from(self.remaining).map_or(buf.len(), |rest| rest.min(buf.len()));
        let read = self.inner.read(&mut buf[..want])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the archive ends inside an entry",
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// Reads one block; `false` if the stream ended before any of it.
fn read_block(reader: &mut impl Read, block: &mut [u8; BLOCK]) -> Result<bool, TarError> {
    let mut filled = 0;
    while filled < BLOCK {
        match reader.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(TarError::Malformed("a truncated header block".to_owned())),
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(true)
}

/// Skips `count` bytes of the stream.
fn skip(reader: &mut impl Read, count: u64) -> Result<(), TarError> {
    let skipped = io::copy(&mut reader.take(count), &mut io::sink())?;
    if skipped == count {
        Ok(())
    } else {
        Err(TarError::Malformed(
            "the archive ends inside an entry".to_owned(),
        ))
    }
}

/// The padding after `size` bytes of data, to a whole block.
fn padding(size: u64) -> u64 {
    (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64
}

/// Maps an early end of the stream to a format error.
fn early_end(err: io::Error) -> TarError {
    if err.kind() == io::ErrorKind::UnexpectedEof {
        TarError::Malformed("the archive ends inside an entry".to_owned())
    } else {
        TarError::Io(err)
    }
}

/// Checks the header checksum: the sum of the block's bytes with the
/// checksum field taken as spaces, unsigned (the standard) or signed (old
/// writers; Go accepts both).
fn verify_checksum(block: &[u8; BLOCK]) -> Result<(), TarError> {
    let stored = number(&block[148..156], "checksum")?;
    let spaces = 8 * u64::from(b' ');
    let outside = block[..148].iter().chain(&block[156..]);
    let unsigned: u64 = outside.clone().map(|&b| u64::from(b)).sum::<u64>() + spaces;
    #[allow(
        clippy::cast_possible_wrap,
        reason = "the signed sum reads each byte as an i8, as old tar writers did"
    )]
    let signed: i64 = outside.map(|&b| i64::from(b as i8)).sum::<i64>() + 8 * 32;
    if stored == unsigned || i64::try_from(stored).is_ok_and(|stored| stored == signed) {
        Ok(())
    } else {
        Err(TarError::Malformed(
            "a header checksum doesn't match".to_owned(),
        ))
    }
}

/// A NUL-terminated field's bytes.
fn field(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// An extension's data without the trailing NULs GNU tar adds.
fn trim_nuls(mut data: Vec<u8>) -> Vec<u8> {
    while data.last() == Some(&0) {
        data.pop();
    }
    data
}

/// A header's path: `prefix/name` for a POSIX (ustar) header, `name`
/// otherwise (GNU uses the prefix's bytes for other fields).
fn header_path(block: &[u8; BLOCK]) -> Vec<u8> {
    let name = field(&block[..100]);
    let ustar = &block[257..263] == b"ustar\0";
    let prefix = if ustar { field(&block[345..500]) } else { &[] };
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut path = prefix.to_vec();
        path.push(b'/');
        path.extend_from_slice(name);
        path
    }
}

/// A numeric field: octal ASCII (spaces and NULs around it), or base-256
/// when its first byte has the high bit set. Negative base-256 numbers are
/// refused: no field a layer needs is negative.
fn number(bytes: &[u8], what: &str) -> Result<u64, TarError> {
    let malformed = || TarError::Malformed(format!("the {what} field isn't a valid number"));
    match bytes.first() {
        Some(&first) if first & 0x80 != 0 => {
            if first & 0x40 != 0 {
                return Err(malformed());
            }
            let mut value: u64 = u64::from(first & 0x3f);
            for &byte in &bytes[1..] {
                value = value
                    .checked_mul(256)
                    .and_then(|v| v.checked_add(u64::from(byte)))
                    .ok_or_else(malformed)?;
            }
            Ok(value)
        }
        _ => {
            // Blanks (spaces, NULs) may surround the digits, never split
            // them: `"12\0 34"` is malformed, not 12 (as Go reads it).
            let start = bytes
                .iter()
                .position(|&b| b != b' ' && b != 0)
                .unwrap_or(bytes.len());
            let end = bytes[start..]
                .iter()
                .position(|&b| b == b' ' || b == 0)
                .map_or(bytes.len(), |at| start + at);
            if bytes[end..].iter().any(|&b| b != b' ' && b != 0) {
                return Err(malformed());
            }
            // An all-blank field is zero (some writers leave unused ones so).
            let mut value: u64 = 0;
            for &byte in &bytes[start..end] {
                if !(b'0'..=b'7').contains(&byte) {
                    return Err(malformed());
                }
                value = value
                    .checked_mul(8)
                    .and_then(|v| v.checked_add(u64::from(byte - b'0')))
                    .ok_or_else(malformed)?;
            }
            Ok(value)
        }
    }
}

/// A user or group ID field.
fn id(bytes: &[u8], what: &str) -> Result<u32, TarError> {
    id_value(number(bytes, what)?, what)
}

/// A user or group ID, in range: `u32::MAX` is refused, since to `chown` it
/// means "leave unchanged".
fn id_value(value: u64, what: &str) -> Result<u32, TarError> {
    u32::try_from(value)
        .ok()
        .filter(|&id| id != u32::MAX)
        .ok_or_else(|| TarError::Malformed(format!("the {what} {value} is out of range")))
}

/// Applies POSIX `x` records (`<length> <key>=<value>\n`, the length
/// counting the whole record) to the next entry. Records are split by their
/// length, not at newlines: a binary value (a file capability) may hold one.
fn apply_pax(data: &[u8], pending: &mut Pending) -> Result<(), TarError> {
    let malformed = |what: &str| TarError::Malformed(format!("a PAX header {what}"));
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| malformed("record has no length"))?;
        let length: usize = std::str::from_utf8(&rest[..space])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| malformed("record length isn't a number"))?;
        if length <= space + 1 || length > rest.len() || rest[length - 1] != b'\n' {
            return Err(malformed("record has a bad length"));
        }
        let record = &rest[space + 1..length - 1];
        rest = &rest[length..];
        let equals = record
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| malformed("record has no `=`"))?;
        let (key, value) = (&record[..equals], &record[equals + 1..]);
        apply_record(key, value, pending)?;
    }
    Ok(())
}

/// Applies one PAX record.
fn apply_record(key: &[u8], value: &[u8], pending: &mut Pending) -> Result<(), TarError> {
    let decimal = |what: &str| -> Result<u64, TarError> {
        std::str::from_utf8(value)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| TarError::Malformed(format!("the PAX {what} isn't a number")))
    };
    match key {
        b"path" => pending.path = Some(value.to_vec()),
        b"linkpath" => pending.link = Some(value.to_vec()),
        b"size" => pending.size = Some(decimal("size")?),
        b"uid" | b"gid" => {
            let what = if key == b"uid" { "uid" } else { "gid" };
            let id = id_value(decimal(what)?, what)?;
            if key == b"uid" {
                pending.uid = Some(id);
            } else {
                pending.gid = Some(id);
            }
        }
        b"mtime" => pending.mtime = Some(pax_time(value)?),
        _ if key.starts_with(b"GNU.sparse.") => {
            return Err(TarError::Unsupported("a sparse file".to_owned()));
        }
        _ => {
            if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                if name.is_empty() {
                    return Err(TarError::Malformed("an empty xattr name".to_owned()));
                }
                pending
                    .xattrs
                    .push((OsString::from_vec(name.to_vec()), value.to_vec()));
            }
            // Everything else (atime, ctime, user and group names, charset,
            // LIBARCHIVE.*, comments) changes nothing a layer needs.
        }
    }
    Ok(())
}

/// A PAX time: decimal seconds, possibly negative, with an optional
/// fraction (to nanoseconds; more digits are truncated).
fn pax_time(value: &[u8]) -> Result<(i64, u32), TarError> {
    let malformed = || TarError::Malformed("the PAX mtime isn't a time".to_owned());
    let text = std::str::from_utf8(value).map_err(|_| malformed())?;
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut seconds: i64 = whole.parse().map_err(|_| malformed())?;
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    let digits: String = fraction.chars().take(9).collect();
    let nanos = if digits.is_empty() {
        0
    } else {
        let scale = 10u32.pow(9 - u32::try_from(digits.len()).unwrap_or(9));
        digits.parse::<u32>().map_err(|_| malformed())? * scale
    };
    // `-1.5` is a second and a half before the epoch: -2 s + 0.5 s.
    if whole.starts_with('-') && nanos > 0 {
        seconds = seconds.checked_sub(1).ok_or_else(malformed)?;
        return Ok((seconds, 1_000_000_000 - nanos));
    }
    Ok((seconds, nanos))
}

/// Cleans an entry path: drops a leading `/`, `.` components and empty ones
/// (as Docker does), refuses `..`, and checks the length and depth limits.
/// The layer root itself cleans to an empty path.
pub fn clean_path(raw: &[u8], limits: &TarLimits) -> Result<PathBuf, TarError> {
    let shown = || String::from_utf8_lossy(raw).into_owned();
    if raw.len() > limits.max_path || raw.contains(&0) {
        return Err(TarError::UnsafePath(shown()));
    }
    let path = Path::new(OsStr::from_bytes(raw));
    let mut cleaned = PathBuf::new();
    let mut depth = 0;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                depth += 1;
                if depth > limits.max_components {
                    return Err(TarError::UnsafePath(shown()));
                }
                cleaned.push(name);
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(TarError::UnsafePath(shown()));
            }
        }
    }
    Ok(cleaned)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in a test is a clear failure"
)]
mod tests {
    use super::*;

    /// A header block with `name`, `type_flag` and `size`, checksummed.
    fn header(name: &[u8], type_flag: u8, size: u64) -> [u8; BLOCK] {
        let mut block = [0u8; BLOCK];
        block[..name.len()].copy_from_slice(name);
        block[100..107].copy_from_slice(b"0000644");
        block[108..115].copy_from_slice(b"0000000");
        block[116..123].copy_from_slice(b"0000000");
        block[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        block[136..147].copy_from_slice(b"00000000000");
        block[156] = type_flag;
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        checksum(&mut block);
        block
    }

    fn checksum(block: &mut [u8; BLOCK]) {
        block[148..156].copy_from_slice(b"        ");
        let sum: u64 = block.iter().map(|&b| u64::from(b)).sum();
        block[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    }

    /// An archive from blocks and data, padded, with the end marker.
    fn archive(parts: &[(&[u8; BLOCK], &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (header, data) in parts {
            bytes.extend_from_slice(*header);
            bytes.extend_from_slice(data);
            bytes.resize(
                bytes.len() + usize::try_from(padding(data.len() as u64)).unwrap(),
                0,
            );
        }
        bytes.extend_from_slice(&[0u8; 2 * BLOCK]);
        bytes
    }

    fn entries(bytes: &[u8], limits: TarLimits) -> Result<Vec<Entry>, TarError> {
        let mut reader = TarReader::new(bytes, limits);
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry()? {
            entries.push(entry);
        }
        Ok(entries)
    }

    /// A PAX record, length-prefixed.
    fn record(key: &str, value: &[u8]) -> Vec<u8> {
        let body = [key.as_bytes(), b"=", value, b"\n"].concat();
        // The length counts its own digits and the space.
        let mut length = body.len() + 2;
        while length.to_string().len() + 1 + body.len() != length {
            length += 1;
        }
        [format!("{length} ").as_bytes(), &body].concat()
    }

    #[test]
    fn a_plain_archive_reads() {
        let file = header(b"dir/file", b'0', 5);
        let dir = header(b"dir/", b'5', 0);
        let bytes = archive(&[(&dir, b""), (&file, b"hello")]);
        let mut reader = TarReader::new(&bytes[..], TarLimits::default());
        let first = reader.next_entry().unwrap().unwrap();
        assert_eq!(
            (first.kind, first.path.as_path()),
            (Kind::Directory, Path::new("dir"))
        );
        let second = reader.next_entry().unwrap().unwrap();
        assert_eq!(second.kind, Kind::File);
        let mut data = String::new();
        reader.read_to_string(&mut data).unwrap();
        assert_eq!(data, "hello");
        assert!(reader.next_entry().unwrap().is_none());
    }

    #[test]
    fn a_bad_checksum_is_refused() {
        let mut block = header(b"file", b'0', 0);
        block[0] = b'g';
        assert!(matches!(
            entries(&archive(&[(&block, b"")]), TarLimits::default()),
            Err(TarError::Malformed(_))
        ));
    }

    #[test]
    fn pax_records_apply_to_the_next_entry_even_binary_ones() {
        let capability = b"\x01\x00\x00\x02\n\x00";
        let records = [
            record("path", b"a/very/long/name"),
            record("uid", b"1000"),
            record("mtime", b"1700000000.5"),
            record("SCHILY.xattr.security.capability", capability),
        ]
        .concat();
        let pax = header(b"pax", b'x', records.len() as u64);
        let file = header(b"short", b'0', 0);
        let read = entries(
            &archive(&[(&pax, &records), (&file, b"")]),
            TarLimits::default(),
        )
        .unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].path, Path::new("a/very/long/name"));
        assert_eq!(read[0].uid, 1000);
        assert_eq!(read[0].mtime, (1_700_000_000, 500_000_000));
        assert_eq!(read[0].xattrs[0].1, capability);
    }

    #[test]
    fn an_oversized_extension_is_refused_before_it_is_read() {
        // The header claims 2 MiB; no data follows at all.
        let pax = header(b"pax", b'x', 2 * 1024 * 1024);
        let mut bytes = pax.to_vec();
        bytes.extend_from_slice(&[0u8; 2 * BLOCK]);
        assert!(matches!(
            entries(&bytes, TarLimits::default()),
            Err(TarError::MetadataTooLarge { .. })
        ));
    }

    #[test]
    fn global_headers_are_ignored_and_sparse_files_refused() {
        let records = record("path", b"renamed");
        let global = header(b"g", b'g', records.len() as u64);
        let file = header(b"file", b'0', 0);
        let read = entries(
            &archive(&[(&global, &records), (&file, b"")]),
            TarLimits::default(),
        )
        .unwrap();
        assert_eq!(
            read[0].path,
            Path::new("file"),
            "a global path renames nothing"
        );

        let sparse = header(b"sparse", b'S', 0);
        assert!(matches!(
            entries(&archive(&[(&sparse, b"")]), TarLimits::default()),
            Err(TarError::Unsupported(_))
        ));
        let records = record("GNU.sparse.map", b"0,10");
        let pax = header(b"pax", b'x', records.len() as u64);
        assert!(matches!(
            entries(
                &archive(&[(&pax, &records), (&file, b"")]),
                TarLimits::default()
            ),
            Err(TarError::Unsupported(_))
        ));
    }

    #[test]
    fn data_on_an_entry_that_has_none_is_refused() {
        let mut link = header(b"link", b'2', 4);
        link[157..163].copy_from_slice(b"target");
        checksum(&mut link);
        assert!(matches!(
            entries(&archive(&[(&link, b"hide")]), TarLimits::default()),
            Err(TarError::Malformed(_))
        ));
    }

    #[test]
    fn pax_times_before_the_epoch_and_blanks_inside_numbers() {
        assert_eq!(pax_time(b"-1.5").unwrap(), (-2, 500_000_000));
        assert_eq!(pax_time(b"-3").unwrap(), (-3, 0));
        assert_eq!(number(b" 0000644\0", "mode").unwrap(), 0o644);
        assert!(number(b"12\0 34", "mode").is_err());
    }

    #[test]
    fn many_extension_headers_count_together() {
        let records = record("comment", &vec![b'x'; 600 * 1024]);
        let pax = header(b"pax", b'x', records.len() as u64);
        let file = header(b"file", b'0', 0);
        let bytes = archive(&[(&pax, &records), (&pax, &records), (&file, b"")]);
        assert!(matches!(
            entries(&bytes, TarLimits::default()),
            Err(TarError::MetadataTooLarge { .. })
        ));
    }

    #[test]
    fn negative_and_overflowing_numbers_are_refused() {
        // Base-256, negative (the sign bit).
        assert!(number(&[0xff; 12], "mtime").is_err());
        // Base-256, positive.
        assert_eq!(
            number(&[0x80, 0, 0, 0, 0, 0, 0, 1, 0], "size").unwrap(),
            256
        );
        // Base-256, over 64 bits.
        assert!(number(&[0x80, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], "size").is_err());
        assert!(
            id(b"37777777777\0", "uid").is_err(),
            "u32::MAX means no change"
        );
    }

    #[test]
    fn paths_are_cleaned_and_bounded() {
        let limits = TarLimits::default();
        assert_eq!(
            clean_path(b"/abs/./x", &limits).unwrap(),
            Path::new("abs/x")
        );
        assert_eq!(clean_path(b"./", &limits).unwrap(), Path::new(""));
        assert!(clean_path(b"a/../../etc", &limits).is_err());
        let deep = "d/".repeat(300);
        assert!(clean_path(deep.as_bytes(), &limits).is_err());
        assert!(clean_path(&vec![b'a'; 5000], &limits).is_err());
    }

    #[test]
    fn the_entry_limit_holds() {
        let file = header(b"file", b'0', 0);
        let bytes = archive(&[(&file, b""), (&file, b""), (&file, b"")]);
        let limits = TarLimits {
            max_headers: 2,
            ..TarLimits::default()
        };
        assert!(matches!(
            entries(&bytes, limits),
            Err(TarError::TooManyEntries(2))
        ));
    }

    #[test]
    fn an_archive_without_its_end_is_refused() {
        let file = header(b"file", b'0', 10);
        let mut bytes = file.to_vec();
        bytes.extend_from_slice(b"short");
        let mut reader = TarReader::new(&bytes[..], TarLimits::default());
        reader.next_entry().unwrap();
        assert!(reader.next_entry().is_err());
    }
}

#[cfg(test)]
mod differential;
