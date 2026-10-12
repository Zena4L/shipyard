//! `cp`: copying files into and out of a container (stage 8, step 8.5a),
//! by runtime ID, with Docker's forms:
//!
//! ```text
//! cp ID:SRC HOST_DEST     cp ID:SRC -         (a tar stream on stdout)
//! cp HOST_SRC ID:DEST     cp - ID:DEST        (a tar stream on stdin)
//! ```
//!
//! **Who touches what.** Inside the container, only a runtime helper
//! (`shipyard_runtime::container::start_helper`): shipyard's own binary,
//! joined to the container's user and mount namespaces, where `/` is the
//! container's root and a planted symlink leads nowhere else. It writes the
//! source as a tar stream (`pack.rs`), or extracts one
//! (`shipyard_image::unpack_tar`). The host side only ever opens the user's
//! own paths, and extracts a container's stream with the same safe
//! unpacker, the files becoming the caller's.
//!
//! **Docker's path rules**, decided where the destination is (the host
//! here, the container in the helper):
//!
//! | source | destination | result |
//! |---|---|---|
//! | a file | missing | made, with the destination's name (its directory must exist; a trailing `/` is an error) |
//! | a file | a file | overwritten |
//! | a file | a directory | copied into it |
//! | a directory | missing | made, with the destination's name |
//! | a directory | a file | an error |
//! | a directory | a directory | copied into it (`SRC/.`: its contents) |
//!
//! **Owners:** copied in, files become the container's root's (`-a`: the
//! source's, where the container has those IDs); copied out, the caller's,
//! without setuid and setgid bits. `-L` follows a source that's a symlink.

#[cfg(target_os = "linux")]
mod helpers;
#[cfg(target_os = "linux")]
mod pack;

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use shipyard_runtime::container::{ContainerError, HelperFailure};

#[cfg(target_os = "linux")]
pub use pack::PackError;

/// One side of a `cp`: a container's path (`ID:PATH`), a host path, or `-`
/// (a tar stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// `ID:PATH`.
    Container {
        /// The container.
        id: String,
        /// The path in it (absolute or not: from its root either way).
        path: PathBuf,
    },
    /// A path on the host.
    Host(PathBuf),
    /// `-`: a tar stream.
    Stream,
}

impl Operand {
    /// Reads an operand as Docker does: `-` is a stream; `ID:PATH` a
    /// container's path, unless what's before the colon has a `/` in it (a
    /// host path with a colon must start with `/` or `./`).
    #[must_use]
    pub fn parse(text: &OsStr) -> Self {
        let bytes = text.as_bytes();
        if bytes == b"-" {
            return Self::Stream;
        }
        match bytes.iter().position(|&byte| byte == b':') {
            Some(colon) if colon > 0 && !bytes[..colon].contains(&b'/') => Self::Container {
                id: String::from_utf8_lossy(&bytes[..colon]).into_owned(),
                path: PathBuf::from(OsStr::from_bytes(&bytes[colon + 1..])),
            },
            _ => Self::Host(PathBuf::from(text)),
        }
    }
}

/// What a copy is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpOptions {
    /// `-a`: files copied in keep their owners (where the container has
    /// those IDs), instead of becoming root's.
    pub archive: bool,
    /// `-L`: a source that's a symlink is followed.
    pub follow_link: bool,
}

/// What a copy did that's worth saying.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpReport {
    /// Things left out or changed, as sentences.
    pub warnings: Vec<String>,
}

/// Why a copy failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CpError {
    /// The operands aren't one container side and one host side or stream.
    #[error("{0}")]
    Usage(&'static str),
    /// The container's side (no such container, the wrong state, not
    /// yours…).
    #[error(transparent)]
    Container(#[from] ContainerError),
    /// The source doesn't exist.
    #[error("no such file or directory: {0}")]
    NotFound(String),
    /// A directory, or a directory's contents, onto something that isn't one.
    #[error("cannot copy a directory onto a file: {0}")]
    DirectoryOntoFile(String),
    /// A directory where a file was to go, or the other way round: neither
    /// replaces the other.
    #[error("a directory and a file can't replace each other: {0}")]
    KindClash(String),
    /// The destination is a device, FIFO or socket (`/dev/null`, say): a
    /// copy never replaces one.
    #[error("{0} is a device, FIFO or socket: cp never replaces one")]
    Special(String),
    /// What must be a directory isn't.
    #[error("not a directory: {0}")]
    NotADirectory(String),
    /// The destination's directory doesn't exist.
    #[error("the destination's directory doesn't exist: {0}")]
    ParentMissing(String),
    /// A destination named as a directory (a trailing `/`) doesn't exist.
    #[error("the destination directory doesn't exist: {0}")]
    NoSuchDirectory(String),
    /// Extracting what was copied failed.
    #[error("{0}")]
    Unpack(#[from] shipyard_image::UnpackError),
    /// Writing what's copied failed.
    #[cfg(target_os = "linux")]
    #[error("{0}")]
    Pack(#[from] PackError),
    /// Something else in the container.
    #[error("in the container: {0}")]
    InContainer(HelperFailure),
    /// Something else on the host.
    #[error("{doing}: {error}")]
    Io {
        /// What was being done.
        doing: String,
        /// Why it failed.
        error: std::io::Error,
    },
}

/// A copy's direction and ends, read from its two operands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    /// Out of container `id`'s `path`.
    Out {
        /// The container.
        id: String,
        /// Its path.
        path: PathBuf,
        /// Where to: a host path, or `None` for a stream.
        to: Option<PathBuf>,
    },
    /// Into container `id`'s `path`.
    In {
        /// From: a host path, or `None` for a stream.
        from: Option<PathBuf>,
        /// The container.
        id: String,
        /// Its path.
        path: PathBuf,
    },
}

impl Transfer {
    /// The copy two operands ask for.
    ///
    /// # Errors
    ///
    /// [`CpError::Usage`]: both or neither are a container's, or both are
    /// streams; or an empty path.
    pub fn of(source: Operand, destination: Operand) -> Result<Self, CpError> {
        let empty = |path: &Path| path.as_os_str().is_empty();
        match (source, destination) {
            (Operand::Container { .. }, Operand::Container { .. }) => Err(CpError::Usage(
                "copying between two containers isn't supported: one side must be the host",
            )),
            (Operand::Container { id, path }, to) => {
                if empty(&path) {
                    return Err(CpError::Usage("the container's path is empty"));
                }
                let to = match to {
                    Operand::Host(path) if empty(&path) => {
                        return Err(CpError::Usage("the destination is empty"));
                    }
                    Operand::Host(path) => Some(path),
                    _ => None,
                };
                Ok(Self::Out { id, path, to })
            }
            (from, Operand::Container { id, path }) => {
                if empty(&path) {
                    return Err(CpError::Usage("the container's path is empty"));
                }
                let from = match from {
                    Operand::Host(path) if empty(&path) => {
                        return Err(CpError::Usage("the source is empty"));
                    }
                    Operand::Host(path) => Some(path),
                    _ => None,
                };
                Ok(Self::In { from, id, path })
            }
            _ => Err(CpError::Usage(
                "one side must be a container's path, as ID:PATH",
            )),
        }
    }
}

/// How a source path reads: its last name, whether only its contents are
/// meant (`SRC/.`, or the root), and whether a symlink there is followed.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the copies themselves are Linux's")
)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    name: Option<OsString>,
    contents: bool,
    follow_last: bool,
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the copies themselves are Linux's")
)]
impl Source {
    fn of(path: &Path, follow_link: bool) -> Self {
        let bytes = path.as_os_str().as_bytes();
        let contents = bytes.ends_with(b"/.") || bytes == b".";
        let name = (!contents)
            .then(|| path.file_name().map(OsStr::to_owned))
            .flatten();
        Self {
            // The root (`/`) has no name: its contents.
            contents: contents || name.is_none(),
            follow_last: follow_link || contents || bytes.ends_with(b"/"),
            name,
        }
    }
}

/// Whether a destination path names a directory (ends with `/` or `/.`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the copies themselves are Linux's")
)]
fn names_directory(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes.ends_with(b"/") || bytes.ends_with(b"/.")
}

/// The limits a copy's extraction runs under: no size or entry limits of
/// its own (a full disk stops it, as it stops `cp`), and a memory budget of
/// 1 GiB for what it keeps track of (some ten million entries).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the copies themselves are Linux's")
)]
fn limits() -> shipyard_image::Limits {
    shipyard_image::Limits {
        max_bytes: u64::MAX,
        tar: shipyard_image::tar_reader::TarLimits {
            max_headers: u64::MAX,
            ..Default::default()
        },
        min_free: Some(0),
        max_retained: 1024 * 1024 * 1024,
    }
}

/// Warnings for what an extraction left out or changed, and `sockets` left
/// out of the archive.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the copies themselves are Linux's")
)]
fn warnings(report: &shipyard_image::LayerReport, sockets: u64) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut add = |count: u64, what: &str| {
        if count > 0 {
            warnings.push(format!("{count} {what}"));
        }
    };
    add(report.devices_skipped, "device node(s) weren't copied");
    add(sockets, "socket(s) weren't copied");
    add(
        report.ids_collapsed,
        "file(s) owned by IDs the container doesn't have now belong to its root",
    );
    add(
        report.xattrs_refused + report.xattrs_not_applied,
        "extended attribute(s) weren't copied",
    );
    warnings
}

/// Registers `cp`'s helpers: **call it first thing in `main`** of a binary
/// that copies (after the runtime's entry points). Otherwise it returns at
/// once.
pub fn helper_entry_point() {
    #[cfg(target_os = "linux")]
    shipyard_runtime::container::helper_entry_point(&helpers::HELPERS);
}

/// Copies `path` out of container `id`, to the host path `to` or, if `None`,
/// as a tar stream to `stream`.
///
/// # Errors
///
/// A [`CpError`].
pub fn copy_out(
    root: &Path,
    id: &str,
    path: &Path,
    to: Option<&Path>,
    stream: &mut dyn Write,
    options: CpOptions,
) -> Result<CpReport, CpError> {
    #[cfg(target_os = "linux")]
    {
        helpers::copy_out(root, id, path, to, stream, options)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, id, path, to, stream, options);
        Err(ContainerError::LinuxOnly.into())
    }
}

/// Copies the host path `from` or, if `None`, a tar stream read from
/// `stream`, into container `id` at `path`.
///
/// # Errors
///
/// A [`CpError`].
pub fn copy_in(
    root: &Path,
    id: &str,
    from: Option<&Path>,
    stream: &mut dyn Read,
    path: &Path,
    options: CpOptions,
) -> Result<CpReport, CpError> {
    #[cfg(target_os = "linux")]
    {
        helpers::copy_in(root, id, from, stream, path, options)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, id, from, stream, path, options);
        Err(ContainerError::LinuxOnly.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Operand {
        Operand::parse(OsStr::new(text))
    }

    #[test]
    fn operands_read_as_dockers() {
        assert_eq!(parse("-"), Operand::Stream);
        assert_eq!(
            parse("web:/etc/hosts"),
            Operand::Container {
                id: "web".to_owned(),
                path: PathBuf::from("/etc/hosts")
            }
        );
        assert_eq!(
            parse("web:"),
            Operand::Container {
                id: "web".to_owned(),
                path: PathBuf::new()
            }
        );
        assert_eq!(parse("./a:b"), Operand::Host(PathBuf::from("./a:b")));
        assert_eq!(parse("/a:b"), Operand::Host(PathBuf::from("/a:b")));
        assert_eq!(parse(":x"), Operand::Host(PathBuf::from(":x")));
        assert_eq!(parse("plain"), Operand::Host(PathBuf::from("plain")));
    }

    #[test]
    fn one_side_must_be_a_containers() {
        let container = || parse("c:/x");
        assert!(matches!(
            Transfer::of(container(), container()),
            Err(CpError::Usage(_))
        ));
        assert!(matches!(
            Transfer::of(parse("a"), parse("b")),
            Err(CpError::Usage(_))
        ));
        assert!(matches!(
            Transfer::of(parse("-"), parse("-")),
            Err(CpError::Usage(_))
        ));
        assert!(matches!(
            Transfer::of(parse("c:"), parse("b")),
            Err(CpError::Usage(_))
        ));
        assert_eq!(
            Transfer::of(container(), parse("-")).unwrap(),
            Transfer::Out {
                id: "c".to_owned(),
                path: PathBuf::from("/x"),
                to: None
            }
        );
        assert_eq!(
            Transfer::of(parse("dir"), container()).unwrap(),
            Transfer::In {
                from: Some(PathBuf::from("dir")),
                id: "c".to_owned(),
                path: PathBuf::from("/x")
            }
        );
    }

    #[test]
    fn a_source_reads_as_dockers() {
        let source = |path: &str, follow| Source::of(Path::new(path), follow);
        assert_eq!(
            source("/data", false),
            Source {
                name: Some("data".into()),
                contents: false,
                follow_last: false
            }
        );
        assert!(source("/data", true).follow_last, "-L");
        assert!(source("/data/", false).follow_last, "a trailing slash");
        let contents = source("/data/.", false);
        assert!(contents.contents && contents.follow_last && contents.name.is_none());
        assert!(source("/", false).contents, "the root: its contents");
        assert!(names_directory(Path::new("out/")));
        assert!(names_directory(Path::new("out/.")));
        assert!(!names_directory(Path::new("out")));
    }
}
