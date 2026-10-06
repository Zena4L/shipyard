//! Reading a small file a user names (a bundle's `config.json`, a seccomp
//! profile) without being stuck on one that isn't.
//!
//! The checks are made on the file as opened, not on its path beforehand: a
//! FIFO swapped in between would block the read forever. It's opened
//! non-blocking (a FIFO's open would otherwise wait for a writer) and read
//! with a limit (`/dev/zero` never ends, and a file may grow after the
//! check).

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Why a small file couldn't be read.
#[derive(Debug)]
pub(crate) enum SmallFileError {
    /// Opening or reading it failed.
    Io(std::io::Error),
    /// It isn't a regular file (a FIFO, a device, a directory).
    NotAFile,
    /// It's larger than the limit.
    TooLarge,
}

/// The contents of the regular file at `path`, at most `limit` bytes.
pub(crate) fn read_small_file(path: &Path, limit: u64) -> Result<Vec<u8>, SmallFileError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(SmallFileError::Io)?;
    if !file.metadata().map_err(SmallFileError::Io)?.is_file() {
        return Err(SmallFileError::NotAFile);
    }
    let mut contents = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut contents)
        .map_err(SmallFileError::Io)?;
    if u64::try_from(contents.len()).map_or(true, |len| len > limit) {
        return Err(SmallFileError::TooLarge);
    }
    Ok(contents)
}
