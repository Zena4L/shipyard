//! `cp`'s two sides (Linux): the host's, and the helper functions that run
//! inside the container (`cp-out`, `cp-in`).

use std::ffi::OsString;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, SFlag, fstat};
use serde::{Deserialize, Serialize};
use shipyard_image::{CopyOptions, IdRanges, LayerReport, Ownership, Top, UnpackError, unpack_tar};
use shipyard_runtime::container::{
    ContainerError, Helper, HelperFailure, HelperIo, RunningHelper, start_helper,
    start_helper_in_view,
};

use super::pack::{PackError, Packer};
use super::{CpError, CpOptions, CpReport, Source, limits, names_directory, warnings};
use crate::run::Files;

/// The helpers `cp` registers.
pub(super) const HELPERS: [Helper; 4] = [
    Helper {
        name: "cp-out",
        run: cp_out,
    },
    Helper {
        name: "cp-in",
        run: cp_in,
    },
    Helper {
        name: "cp-stat",
        run: cp_stat,
    },
    Helper {
        name: "volume-fill",
        run: super::fill::volume_fill,
    },
];

/// What a helper's failure means, in `cp`'s terms ([`HelperFailure::kind`]).
pub(super) mod kind {
    pub(in crate::cp) const NOT_FOUND: u16 = 1;
    pub(in crate::cp) const DIRECTORY_ONTO_FILE: u16 = 2;
    pub(in crate::cp) const NOT_A_DIRECTORY: u16 = 3;
    pub(in crate::cp) const PARENT_MISSING: u16 = 4;
    pub(in crate::cp) const NO_SUCH_DIRECTORY: u16 = 5;
    pub(in crate::cp) const KIND_CLASH: u16 = 6;
    pub(in crate::cp) const SPECIAL: u16 = 8;
    pub(in crate::cp) const OTHER: u16 = 7;
}

/// `cp-out`'s arguments.
#[derive(Debug, Serialize, Deserialize)]
struct OutArgs {
    /// The source, in the container.
    path: Vec<u8>,
    follow_last: bool,
    /// Its name in the stream; `None`: only its contents (a directory's),
    /// named from it.
    name: Option<Vec<u8>>,
}

/// `cp-in`'s arguments.
#[derive(Debug, Serialize, Deserialize)]
struct InArgs {
    /// The destination, in the container.
    path: Vec<u8>,
    /// What the stream holds: a host source's name and kind, or `None` for
    /// a stream given as is (extracted into the destination directory).
    source: Option<SourceKind>,
    /// `-a`: the stream's owners, not root.
    archive: bool,
}

/// `cp-stat`'s arguments.
#[derive(Debug, Serialize, Deserialize)]
struct StatArgs {
    /// The path, in the container.
    path: Vec<u8>,
}

/// The most of `cp-stat`'s answer read: a JSON line of a few hundred bytes
/// (a link's target is at most `PATH_MAX`).
const MAX_STAT: u64 = 64 * 1024;

/// A host source, as the helper needs to know it.
#[derive(Debug, Serialize, Deserialize)]
struct SourceKind {
    /// Its name in the stream (none: its contents).
    name: Option<Vec<u8>>,
    directory: bool,
}

/// The most of what follows a stream's end-of-archive marker that `cp-in`
/// reads and drops (tar pads its archives to 10 KiB): what its sender
/// writes after it then never meets a closed pipe.
const MAX_TRAILER: u64 = 1024 * 1024;

// --- The host's side

/// See [`super::copy_out`].
pub(super) fn copy_out(
    files: Files<'_>,
    path: &Path,
    to: Option<&Path>,
    stream: &mut dyn Write,
    options: CpOptions,
) -> Result<CpReport, CpError> {
    let source = Source::of(path, options.follow_link);
    let args = OutArgs {
        path: path.as_os_str().as_bytes().to_vec(),
        follow_last: source.follow_last,
        name: source.name.as_ref().map(|name| name.as_bytes().to_vec()),
    };
    // The container first: nothing is made on the host for a copy that
    // can't start.
    let (read, helper) = start(files, "cp-out", &args, None)?;
    let Some(to) = to else {
        // A stream: the helper's, passed on as it is.
        let copied = std::io::copy(&mut std::fs::File::from(read), stream);
        return match (copied, helper.wait()) {
            (Ok(_), Ok(())) => Ok(CpReport::default()),
            // This side couldn't write (a full disk, a closed pipe): the
            // helper only saw its reader go.
            (Err(error), Err(ended)) if helper_lost_reader(&ended) => Err(CpError::Io {
                doing: "writing the archive".to_owned(),
                error,
            }),
            (_, Err(ended)) => Err(failure(ended)),
            (Err(error), Ok(())) => Err(CpError::Io {
                doing: "writing the archive".to_owned(),
                error,
            }),
        };
    };
    let Destination { dir, top, made } = destination_on_host(to, &source)?;
    let options = CopyOptions {
        ownership: Ownership::Keep,
        owner: None,
        limits: limits(),
        top,
    };
    // (The read end is closed when it returns: a helper still writing
    // stops, with a broken pipe.)
    let unpacked = unpack_tar(BufReader::new(std::fs::File::from(read)), &dir, &options);
    let result = match (unpacked, helper.wait()) {
        (Ok(report), Ok(())) => Ok(CpReport {
            warnings: warnings(&report, 0),
        }),
        // The extraction stopped first: the helper only saw it go.
        (Err(err), Err(ended)) if helper_lost_reader(&ended) => Err(unpack_error(err, to)),
        // The helper's failure explains the stream's end.
        (_, Err(ended)) => Err(failure(ended)),
        (Err(err), Ok(())) => Err(unpack_error(err, to)),
    };
    if result.is_err()
        && let Some(made) = made
    {
        // Made for a copy that failed: gone again, if nothing's in it.
        let _ = std::fs::remove_dir(made);
    }
    result
}

/// Whether a helper failed only because its reader went away.
fn helper_lost_reader(err: &ContainerError) -> bool {
    matches!(err, ContainerError::HelperFailed(failure) if failure.errno() == Some(Errno::EPIPE))
}

/// Where a copy extracts: the directory, the one name the stream must hold
/// (renamed), and the directory made for it, if one was.
struct Destination<P> {
    dir: OwnedFd,
    top: Option<Top>,
    made: Option<P>,
}

/// Where a copy out extracts on the host, for `source` and destination
/// `to` (Docker's rules, the module docs' table).
fn destination_on_host(to: &Path, source: &Source) -> Result<Destination<PathBuf>, CpError> {
    let shown = || to.display().to_string();
    let opening = |doing: &str| {
        let doing = format!("{doing} {}", to.display());
        move |errno: Errno| CpError::Io {
            doing,
            error: std::io::Error::from(errno),
        }
    };
    let directory = |path: &Path| {
        open(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    };
    let parent = || match to.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
        _ => PathBuf::from("."),
    };
    let name = || to.file_name().map(std::ffi::OsStr::to_owned);
    let kept = |rename: Option<OsString>, directory: bool| {
        source.name.as_ref().map(|name| Top {
            name: name.clone(),
            rename,
            directory,
        })
    };
    let at = |dir: OwnedFd, top: Option<Top>| Destination {
        dir,
        top,
        made: None,
    };
    match std::fs::metadata(to) {
        Ok(meta) if meta.is_dir() => {
            let dir = directory(to).map_err(opening("opening"))?;
            Ok(at(dir, kept(None, false)))
        }
        Ok(meta) if !meta.is_file() => Err(CpError::Special(shown())),
        Ok(_) if source.contents => Err(CpError::DirectoryOntoFile(shown())),
        Ok(_) => {
            let dir = directory(&parent()).map_err(opening("opening the directory of"))?;
            Ok(at(dir, kept(name(), false)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let parent = parent();
            let dir = directory(&parent).map_err(|errno| match errno {
                Errno::ENOENT => CpError::ParentMissing(shown()),
                Errno::ENOTDIR => CpError::NotADirectory(parent.display().to_string()),
                errno => opening("opening the directory of")(errno),
            })?;
            if source.contents {
                let name = name().ok_or_else(|| CpError::NotADirectory(shown()))?;
                let made = make_directory(&dir, &name).map_err(opening("making"))?;
                return Ok(Destination {
                    dir: made,
                    top: None,
                    made: Some(to.to_owned()),
                });
            }
            Ok(at(dir, kept(name(), names_directory(to))))
        }
        Err(error) => Err(CpError::Io {
            doing: format!("reading {}", to.display()),
            error,
        }),
    }
}

/// Makes directory `name` in `parent` for a directory's contents, mode 0755
/// whatever the umask (a directory's own mode isn't in the stream), and
/// opens it.
fn make_directory(parent: &OwnedFd, name: &std::ffi::OsStr) -> nix::Result<OwnedFd> {
    nix::sys::stat::mkdirat(parent, name, Mode::from_bits_truncate(0o755))?;
    let made = nix::fcntl::openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    nix::sys::stat::fchmod(&made, Mode::from_bits_truncate(0o755))?;
    Ok(made)
}

/// See [`super::copy_in`].
pub(super) fn copy_in(
    files: Files<'_>,
    from: Option<&Path>,
    stream: &mut dyn Read,
    path: &Path,
    options: CpOptions,
) -> Result<CpReport, CpError> {
    let source = from
        .map(|from| open_source(from, options.follow_link))
        .transpose()?;
    let args = InArgs {
        path: path.as_os_str().as_bytes().to_vec(),
        source: source.as_ref().map(|(_, kind)| SourceKind {
            name: kind.name.clone(),
            directory: kind.directory,
        }),
        archive: options.archive,
    };
    let (input, output) = pipe()?;
    let (report, helper) = start(files, "cp-in", &args, Some(input))?;
    let mut writer = BufWriter::new(std::fs::File::from(output));
    let mut sockets = 0;
    let sent = match &source {
        Some((fd, kind)) => {
            let mut packer = Packer::new(&mut writer, &host_reopen);
            let packed = match &kind.name {
                Some(name) => packer.add(fd, Path::new(std::ffi::OsStr::from_bytes(name))),
                None => packer.add_contents(fd),
            };
            sockets = packer.sockets_skipped;
            packed
                .and_then(|()| packer.finish().map(drop))
                .map_err(CpError::from)
        }
        None => std::io::copy(stream, &mut writer)
            .map(drop)
            .map_err(|error| CpError::Io {
                doing: "reading the archive".to_owned(),
                error,
            }),
    };
    let flushed = writer.into_inner().map(drop).map_err(|err| CpError::Io {
        doing: "sending the archive".to_owned(),
        error: err.into_error(),
    });
    let mut said = Vec::new();
    let _ = std::fs::File::from(report).read_to_end(&mut said);
    match (sent.and(flushed), helper.wait()) {
        (Ok(()), Ok(())) => {}
        // A stream it stopped reading shows here only as a broken pipe:
        // what it says explains it.
        (Err(err), Err(ended)) if send_hit_closed_pipe(&err) => return Err(failure(ended)),
        // This side stopped first (its source unreadable): the helper
        // only saw the stream end early.
        (Err(err), _) => return Err(err),
        (Ok(()), Err(ended)) => return Err(failure(ended)),
    }
    let report: LayerReport = serde_json::from_slice(&said).unwrap_or_default();
    Ok(CpReport {
        warnings: warnings(&report, sockets),
    })
}

/// Opens a host source (`O_PATH`, not followed unless `-L` or the path
/// says so), and says what it is.
fn open_source(from: &Path, follow_link: bool) -> Result<(OwnedFd, SourceKind), CpError> {
    let source = Source::of(from, follow_link);
    let mut flags = OFlag::O_PATH | OFlag::O_CLOEXEC;
    if !source.follow_last {
        flags |= OFlag::O_NOFOLLOW;
    }
    let fd = open(from, flags, Mode::empty()).map_err(|errno| match errno {
        Errno::ENOENT => CpError::NotFound(from.display().to_string()),
        errno => CpError::Io {
            doing: format!("opening {}", from.display()),
            error: std::io::Error::from(errno),
        },
    })?;
    let directory = is_directory(&fd);
    if source.contents && !directory {
        return Err(CpError::NotADirectory(from.display().to_string()));
    }
    let kind = SourceKind {
        name: source.name.as_ref().map(|name| name.as_bytes().to_vec()),
        directory,
    };
    Ok((fd, kind))
}

/// Whether sending failed only because the helper stopped reading.
fn send_hit_closed_pipe(err: &CpError) -> bool {
    match err {
        CpError::Pack(PackError::Write(error)) | CpError::Io { error, .. } => {
            error.kind() == std::io::ErrorKind::BrokenPipe
        }
        _ => false,
    }
}

/// See [`super::stat`].
pub(super) fn stat(files: Files<'_>, path: &Path) -> Result<super::PathStat, CpError> {
    let args = StatArgs {
        path: path.as_os_str().as_bytes().to_vec(),
    };
    let (read, helper) = start(files, "cp-stat", &args, None)?;
    answer(read, helper, MAX_STAT, "reading the path's stat")
}

/// A helper's answer: the JSON it writes on `read` (at most `max`
/// bytes), once it has ended well.
pub(super) fn answer<T: serde::de::DeserializeOwned>(
    read: OwnedFd,
    helper: RunningHelper,
    max: u64,
    doing: &str,
) -> Result<T, CpError> {
    let mut said = Vec::new();
    let read_said = std::fs::File::from(read).take(max).read_to_end(&mut said);
    helper.wait().map_err(failure)?;
    let unreadable = |error| CpError::Io {
        doing: doing.to_owned(),
        error,
    };
    read_said.map_err(unreadable)?;
    serde_json::from_slice(&said).map_err(|error| unreadable(std::io::Error::other(error)))
}

/// Starts helper `name` in the container's `files`, with `args`, its standard input
/// `stdin` (`/dev/null` if `None`) and its output on a pipe: the pipe's
/// read end, and the helper.
fn start(
    files: Files<'_>,
    name: &str,
    args: &impl Serialize,
    stdin: Option<OwnedFd>,
) -> Result<(OwnedFd, RunningHelper), CpError> {
    let args = serde_json::to_vec(args).map_err(|err| CpError::Io {
        doing: "encoding the helper's arguments".to_owned(),
        error: std::io::Error::other(err),
    })?;
    let stdin = match stdin {
        Some(fd) => fd,
        None => open(
            "/dev/null",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| CpError::Io {
            doing: "opening /dev/null".to_owned(),
            error: std::io::Error::from(errno),
        })?,
    };
    let (read, write) = pipe()?;
    let helper = match files {
        Files::Live { root, id } => {
            start_helper(root, id, name, "copying files", &args, &stdin, &write)?
        }
        Files::Stopped(view) => start_helper_in_view(view, name, &args, &stdin, &write)?,
    };
    Ok((read, helper))
}

pub(super) fn pipe() -> Result<(OwnedFd, OwnedFd), CpError> {
    nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(|errno| CpError::Io {
        doing: "making a pipe".to_owned(),
        error: std::io::Error::from(errno),
    })
}

/// Reopens `fd` through this process's own `/proc` (on the host).
pub(super) fn host_reopen(fd: &OwnedFd, flags: OFlag) -> nix::Result<OwnedFd> {
    open(
        format!("/proc/self/fd/{}", fd.as_raw_fd()).as_str(),
        flags | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

pub(super) fn is_directory(fd: &OwnedFd) -> bool {
    fstat(fd).is_ok_and(|stat| stat.st_mode & SFlag::S_IFMT.bits() == SFlag::S_IFDIR.bits())
}

/// A helper's end as a [`CpError`]: its failure in `cp`'s terms.
fn failure(err: ContainerError) -> CpError {
    let ContainerError::HelperFailed(failure) = err else {
        return CpError::Container(err);
    };
    let detail = failure.detail.clone();
    match failure.kind {
        kind::NOT_FOUND => CpError::NotFound(detail),
        kind::DIRECTORY_ONTO_FILE => CpError::DirectoryOntoFile(detail),
        kind::NOT_A_DIRECTORY => CpError::NotADirectory(detail),
        kind::PARENT_MISSING => CpError::ParentMissing(detail),
        kind::NO_SUCH_DIRECTORY => CpError::NoSuchDirectory(detail),
        kind::KIND_CLASH => CpError::KindClash(detail),
        kind::SPECIAL => CpError::Special(detail),
        _ => CpError::InContainer(failure),
    }
}

/// What an extraction's failure means in `cp`'s terms, for destination
/// `dest`, if it has one: a [`kind`].
fn cp_kind(err: &UnpackError, dest: &Path) -> Option<u16> {
    match err {
        UnpackError::KindClash(_) => Some(kind::KIND_CLASH),
        UnpackError::Special(_) => Some(kind::SPECIAL),
        UnpackError::NotADirectory(_) if names_directory(dest) => Some(kind::NO_SUCH_DIRECTORY),
        _ => None,
    }
}

/// An extraction's failure on the host, in `cp`'s terms where it has them.
fn unpack_error(err: UnpackError, to: &Path) -> CpError {
    let shown = to.display().to_string();
    match cp_kind(&err, to) {
        Some(kind::KIND_CLASH) => CpError::KindClash(shown),
        Some(kind::SPECIAL) => CpError::Special(shown),
        Some(_) => CpError::NoSuchDirectory(shown),
        None => CpError::Unpack(err),
    }
}

/// The errno of an I/O error (none if it has none).
pub(super) fn errno_of(err: &std::io::Error) -> Errno {
    Errno::from_raw(err.raw_os_error().unwrap_or(0))
}

// --- Inside the container (the helper's functions)

/// The container's `path`, as a helper opens what it's asked for (its
/// last link followed if `follow_last`): not there is `NOT_FOUND`.
pub(super) fn open_asked(
    io: &HelperIo,
    path: &Path,
    follow_last: bool,
) -> Result<OwnedFd, HelperFailure> {
    io.open(path, follow_last).map_err(|errno| {
        let shown = path.display().to_string();
        match errno {
            Errno::ENOENT => HelperFailure::plain(kind::NOT_FOUND, shown),
            errno => HelperFailure::new(kind::OTHER, errno, format!("opening {shown}")),
        }
    })
}

/// What the container's `path` is, for
/// [`in_scope`](super::scope::in_scope): a link's target, or `None`.
fn link_at(io: &HelperIo, path: &Path) -> Result<Option<PathBuf>, Errno> {
    let fd = io.open(path, false)?;
    let stat = fstat(&fd)?;
    if stat.st_mode & SFlag::S_IFMT.bits() != SFlag::S_IFLNK.bits() {
        return Ok(None);
    }
    nix::fcntl::readlinkat(&fd, "").map(|target| Some(PathBuf::from(target)))
}

/// Writes what the container's path is (never what a link at it leads to)
/// as JSON on standard output: a [`super::PathStat`].
fn cp_stat(io: &HelperIo) -> Result<(), HelperFailure> {
    let args: StatArgs = serde_json::from_slice(&io.args)
        .map_err(|_| HelperFailure::new(kind::OTHER, Errno::EINVAL, "reading cp's arguments"))?;
    let path = PathBuf::from(OsString::from_vec(args.path));
    let shown = path.display().to_string();
    let fd = open_asked(io, &path, false)?;
    let stat = fstat(&fd).map_err(|errno| HelperFailure::new(kind::OTHER, errno, shown.clone()))?;
    let symlink = stat.st_mode & SFlag::S_IFMT.bits() == SFlag::S_IFLNK.bits();
    let link_target = if symlink {
        super::scope::in_scope(&path, |path| link_at(io, path))
            .map_err(|errno| HelperFailure::new(kind::OTHER, errno, shown.clone()))?
            .display()
            .to_string()
    } else {
        String::new()
    };
    let said = super::PathStat {
        name: path.file_name().map_or_else(
            || "/".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
        size: u64::try_from(stat.st_size).unwrap_or(0),
        mode: stat.st_mode,
        mtime_secs: stat.st_mtime,
        mtime_nanos: u32::try_from(stat.st_mtime_nsec).unwrap_or(0),
        link_target,
    };
    let json = serde_json::to_vec(&said)
        .map_err(|_| HelperFailure::new(kind::OTHER, Errno::EINVAL, "writing the stat"))?;
    (&io.stdout)
        .write_all(&json)
        .map_err(|error| HelperFailure::new(kind::OTHER, errno_of(&error), "writing the stat"))
}

/// Writes the container's source as a tar stream on standard output.
fn cp_out(io: &HelperIo) -> Result<(), HelperFailure> {
    let args: OutArgs = serde_json::from_slice(&io.args)
        .map_err(|_| HelperFailure::new(kind::OTHER, Errno::EINVAL, "reading cp's arguments"))?;
    let path = PathBuf::from(OsString::from_vec(args.path));
    let shown = path.display().to_string();
    let fd = open_asked(io, &path, args.follow_last)?;
    let reopen = |fd: &OwnedFd, flags: OFlag| io.reopen(fd, flags);
    let mut out = BufWriter::new(&io.stdout);
    let mut packer = Packer::new(&mut out, &reopen);
    let packed = match &args.name {
        Some(name) => packer.add(&fd, Path::new(std::ffi::OsStr::from_bytes(name))),
        None if is_directory(&fd) => packer.add_contents(&fd),
        None => return Err(HelperFailure::plain(kind::NOT_A_DIRECTORY, shown)),
    };
    packed
        .and_then(|()| packer.finish().map(drop))
        .map_err(|err| pack_failure(&err))?;
    out.flush()
        .map_err(|err| pack_failure(&PackError::Write(err)))
}

/// A writer's failure, from the helper: a broken pipe as one (its reader
/// went away), anything else said in full.
pub(super) fn pack_failure(err: &PackError) -> HelperFailure {
    match &err {
        PackError::Write(source) if source.kind() == std::io::ErrorKind::BrokenPipe => {
            HelperFailure::new(kind::OTHER, Errno::EPIPE, "writing the archive")
        }
        _ => HelperFailure::plain(kind::OTHER, err.to_string()),
    }
}

/// Extracts the tar stream on standard input into the container's
/// destination, and says what it did on standard output.
fn cp_in(io: &HelperIo) -> Result<(), HelperFailure> {
    let args: InArgs = serde_json::from_slice(&io.args)
        .map_err(|_| HelperFailure::new(kind::OTHER, Errno::EINVAL, "reading cp's arguments"))?;
    let path = PathBuf::from(OsString::from_vec(args.path));
    let Destination { dir, top, made } = destination_inside(io, &path, args.source.as_ref())?;
    let options = CopyOptions {
        ownership: Ownership::Apply {
            uids: IdRanges(io.uids.clone()),
            gids: IdRanges(io.gids.clone()),
            shift: None,
        },
        owner: (!args.archive).then_some((0, 0)),
        limits: limits(),
        top,
    };
    let mut input = BufReader::new(&io.stdin);
    let unpacked = unpack_tar(&mut input, &dir, &options);
    let report = unpacked.map_err(|err| {
        if let Some(made) = &made {
            let _ = nix::unistd::unlinkat(
                &made.0,
                made.1.as_os_str(),
                nix::unistd::UnlinkatFlags::RemoveDir,
            );
        }
        match (cp_kind(&err, &path), err) {
            (Some(kind), _) => HelperFailure::plain(kind, path.display().to_string()),
            (None, UnpackError::Io { context, source }) => {
                HelperFailure::new(kind::OTHER, errno_of(&source), context)
            }
            (None, err) => HelperFailure::plain(kind::OTHER, err.to_string()),
        }
    })?;
    // What follows the end-of-archive marker (tar's padding), read so that
    // its sender's last writes still find a reader.
    let _ = std::io::copy(&mut input.take(MAX_TRAILER), &mut std::io::sink());
    let said = serde_json::to_vec(&report)
        .map_err(|_| HelperFailure::new(kind::OTHER, Errno::EINVAL, "encoding what was copied"))?;
    (&io.stdout)
        .write_all(&said)
        .map_err(|err| HelperFailure::new(kind::OTHER, errno_of(&err), "saying what was copied"))
}

/// Where a copy in extracts inside the container (Docker's rules, the
/// module docs' table). A directory made for a directory's contents is
/// named by its directory and name, to remove if the copy fails.
fn destination_inside(
    io: &HelperIo,
    path: &Path,
    source: Option<&SourceKind>,
) -> Result<Destination<(OwnedFd, OsString)>, HelperFailure> {
    let shown = || path.display().to_string();
    let opening =
        |errno: Errno| HelperFailure::new(kind::OTHER, errno, format!("opening {}", shown()));
    let source_name = source
        .and_then(|source| source.name.as_ref())
        .map(|name| OsString::from_vec(name.clone()));
    let top = |rename: Option<OsString>, directory: bool| {
        source_name.clone().map(|name| Top {
            name,
            rename,
            directory,
        })
    };
    let at = |dir: OwnedFd, top: Option<Top>| Destination {
        dir,
        top,
        made: None,
    };
    let name = || path.file_name().map(std::ffi::OsStr::to_owned);
    match io.open(path, true) {
        Ok(fd) if is_directory(&fd) => Ok(at(fd, top(None, false))),
        Ok(_) => match source {
            // A file onto a file: into its directory, as its name.
            Some(kind) if !kind.directory && source_name.is_some() => {
                let parent = path.parent().unwrap_or(Path::new("/"));
                let dir = io.open(parent, true).map_err(opening)?;
                Ok(at(dir, top(name(), false)))
            }
            Some(_) => Err(HelperFailure::plain(kind::DIRECTORY_ONTO_FILE, shown())),
            // A stream is extracted into a directory.
            None => Err(HelperFailure::plain(kind::NOT_A_DIRECTORY, shown())),
        },
        Err(Errno::ENOENT) => {
            let Some(kind) = source else {
                return Err(HelperFailure::plain(kind::NOT_FOUND, shown()));
            };
            let parent = path.parent().unwrap_or(Path::new("/"));
            let dir = match io.open(parent, true) {
                Ok(dir) if is_directory(&dir) => dir,
                Ok(_) => {
                    return Err(HelperFailure::plain(
                        kind::NOT_A_DIRECTORY,
                        parent.display().to_string(),
                    ));
                }
                Err(Errno::ENOENT) => {
                    return Err(HelperFailure::plain(kind::PARENT_MISSING, shown()));
                }
                Err(errno) => return Err(opening(errno)),
            };
            if names_directory(path) && !kind.directory {
                return Err(HelperFailure::plain(kind::NO_SUCH_DIRECTORY, shown()));
            }
            let Some(name) = name() else {
                return Err(HelperFailure::plain(kind::NOT_A_DIRECTORY, shown()));
            };
            if source_name.is_none() {
                // A directory's contents: into a directory made for them.
                let dir = io
                    .reopen(&dir, OFlag::O_RDONLY | OFlag::O_DIRECTORY)
                    .map_err(opening)?;
                let made = make_directory(&dir, &name).map_err(|errno| {
                    HelperFailure::new(kind::OTHER, errno, format!("making {}", shown()))
                })?;
                return Ok(Destination {
                    dir: made,
                    top: None,
                    made: Some((dir, name)),
                });
            }
            Ok(at(dir, top(Some(name), kind.directory)))
        }
        Err(errno) => Err(opening(errno)),
    }
}
