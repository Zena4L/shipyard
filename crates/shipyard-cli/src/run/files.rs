//! Docker's environment and label files (`--env-file`, `--label-file`):
//! a `KEY=VALUE` per line, read as they are (no quotes, no `$`: not a
//! shell's), `#` lines and blank ones skipped. A bare `KEY` is an
//! environment file's variable copied from the caller's shell (as a bare
//! `-e KEY`), a label file's label with an empty value. A value is never
//! said in an error: a file's line is named by its number.

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

/// The most read of one file.
const MAX_FILE: u64 = 1024 * 1024;

/// Why a file couldn't be taken.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum FileError {
    /// It couldn't be read (the reason).
    #[error("{path}: {reason}")]
    Unreadable {
        /// The file.
        path: String,
        /// Why.
        reason: String,
    },
    /// It's past [`MAX_FILE`].
    #[error("{path}: more than {} MiB", MAX_FILE / 1024 / 1024)]
    TooBig {
        /// The file.
        path: String,
    },
    /// A line with no name before its `=`.
    #[error("{path}, line {line}: no name before the =")]
    NoName {
        /// The file.
        path: String,
        /// The line's number, from 1.
        line: usize,
    },
    /// A name with white space in it (Docker's refusal).
    #[error("{path}, line {line}: a name with white space in it")]
    Spaced {
        /// The file.
        path: String,
        /// The line's number, from 1.
        line: usize,
    },
    /// A NUL byte in a line: no variable or label can hold one (the
    /// kernel's strings end at it).
    #[error("{path}, line {line}: a NUL byte")]
    Nul {
        /// The file.
        path: String,
        /// The line's number, from 1.
        line: usize,
    },
    /// A label file's line that isn't text.
    #[error("{path}, line {line}: not UTF-8")]
    NotText {
        /// The file.
        path: String,
        /// The line's number, from 1.
        line: usize,
    },
}

/// The variables of the environment file `path`, in order: `KEY=VALUE`,
/// or a bare `KEY` (the caller's value, as a bare `-e KEY`).
///
/// # Errors
///
/// A [`FileError`].
pub(crate) fn env_file(path: &Path) -> Result<Vec<OsString>, FileError> {
    entries(path, &read(path)?)
        .map(|entry| entry.map(|(_, bytes)| OsString::from_vec(bytes)))
        .collect()
}

/// The labels of the label file `path`, in order: `(KEY, VALUE)`, a bare
/// `KEY`'s value empty.
///
/// # Errors
///
/// A [`FileError`].
pub(crate) fn label_file(path: &Path) -> Result<Vec<(String, String)>, FileError> {
    entries(path, &read(path)?)
        .map(|entry| {
            let (line, bytes) = entry?;
            let text = String::from_utf8(bytes).map_err(|_| FileError::NotText {
                path: path.display().to_string(),
                line,
            })?;
            Ok(match text.split_once('=') {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                None => (text, String::new()),
            })
        })
        .collect()
}

/// `path` read whole, up to [`MAX_FILE`]. Not `shipyard_sys`'s bounded
/// read, which refuses a pipe: a file of the caller's own may be one
/// (`--env-file <(…)`, `/dev/stdin`), and blocks only the caller.
fn read(path: &Path) -> Result<Vec<u8>, FileError> {
    let unreadable = |error: std::io::Error| FileError::Unreadable {
        path: path.display().to_string(),
        reason: error.to_string(),
    };
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_FILE + 1).read_to_end(&mut bytes))
        .map_err(unreadable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE {
        return Err(FileError::TooBig {
            path: path.display().to_string(),
        });
    }
    Ok(bytes)
}

/// The entries of `bytes` (the file `path`'s), each with its line's
/// number: its leading white space and a CRLF's `\r` taken off, a
/// first line's UTF-8 byte order mark too (Docker's); blank and `#`
/// lines skipped. The name (before any `=`) is checked.
fn entries<'a>(
    path: &'a Path,
    bytes: &'a [u8],
) -> impl Iterator<Item = Result<(usize, Vec<u8>), FileError>> + 'a {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    bytes
        .split(|&byte| byte == b'\n')
        .enumerate()
        .filter_map(move |(index, line)| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let start = line
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())
                .unwrap_or(line.len());
            let line = line.get(start..).unwrap_or_default();
            if line.is_empty() || line.starts_with(b"#") {
                return None;
            }
            let number = index + 1;
            let name = line.split(|&byte| byte == b'=').next().unwrap_or_default();
            let refused = if line.contains(&0) {
                Some(FileError::Nul {
                    path: path.display().to_string(),
                    line: number,
                })
            } else if name.is_empty() {
                Some(FileError::NoName {
                    path: path.display().to_string(),
                    line: number,
                })
            } else if name.iter().any(u8::is_ascii_whitespace) {
                Some(FileError::Spaced {
                    path: path.display().to_string(),
                    line: number,
                })
            } else {
                None
            };
            Some(refused.map_or_else(|| Ok((number, line.to_vec())), Err))
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    use shipyard_test_support::registry::Scratch;

    /// A file in a fresh directory, holding `bytes`.
    fn file(bytes: &[u8]) -> (Scratch, std::path::PathBuf) {
        let dir = Scratch::new();
        let path = dir.0.join("f");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    /// Docker's format: values as they are (spaces, `=`, quotes kept),
    /// bare keys, comments, blank lines, leading space, CRLF, a BOM.
    #[test]
    fn an_env_file_is_read_as_docker_reads_it() {
        let (_dir, path) = file(
            b"\xef\xbb\xbfA=1\n# a comment\n\n  B=two \r\nC='q'=x\nHOME\n\t# indented comment\n",
        );
        assert_eq!(
            env_file(&path).unwrap(),
            ["A=1", "B=two ", "C='q'=x", "HOME"].map(OsString::from)
        );
    }

    /// A name with white space, or none, is refused by its line's number,
    /// the value never said.
    #[test]
    fn bad_lines_are_refused_by_number() {
        let (_dir, path) = file(b"A=1\nMY KEY=secret\n");
        let error = env_file(&path).unwrap_err();
        assert!(
            matches!(error, FileError::Spaced { line: 2, .. }),
            "{error}"
        );
        assert!(!error.to_string().contains("secret"));
        let (_dir, path) = file(b"A=1\nB=x\0y\n");
        assert!(matches!(
            env_file(&path).unwrap_err(),
            FileError::Nul { line: 2, .. }
        ));
        let (_dir, path) = file(b"=secret\n");
        assert!(matches!(
            env_file(&path).unwrap_err(),
            FileError::NoName { line: 1, .. }
        ));
    }

    /// A missing file, and one past the bound, are refused.
    #[test]
    fn a_missing_or_huge_file_is_refused() {
        let (dir, _) = file(b"");
        assert!(matches!(
            env_file(&dir.0.join("none")).unwrap_err(),
            FileError::Unreadable { .. }
        ));
        let (_dir, path) = file(&vec![b'a'; usize::try_from(MAX_FILE).unwrap() + 1]);
        assert!(matches!(
            env_file(&path).unwrap_err(),
            FileError::TooBig { .. }
        ));
    }

    /// A label file: `KEY=VALUE`, and a bare `KEY` with an empty value.
    #[test]
    fn a_label_file_is_read_as_docker_reads_it() {
        let (_dir, path) = file(b"tier=web\n# c\nempty\nk=a=b\n");
        assert_eq!(
            label_file(&path).unwrap(),
            [
                ("tier".to_owned(), "web".to_owned()),
                ("empty".to_owned(), String::new()),
                ("k".to_owned(), "a=b".to_owned()),
            ]
        );
        let (_dir, path) = file(b"k=\xff\n");
        assert!(matches!(
            label_file(&path).unwrap_err(),
            FileError::NotText { line: 1, .. }
        ));
    }
}
