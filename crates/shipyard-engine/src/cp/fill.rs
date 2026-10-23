//! Copy-up (step 11.4a2b, D11.4a2b-1): a volume still empty at a
//! container's create gets what its image has at its place, as Docker's
//! `copyExistingContents`. A helper (`volume-fill`) in a view of the
//! container's root alone (its layers, no mounts, its user namespace) is
//! handed the volume's directory (`O_PATH`) as its standard input; it packs the
//! place's contents and unpacks them into that directory, by descriptor
//! (cp's packer and unpacker, their checks), owners and modes kept. The
//! directory then takes the place's own owner and mode (Docker's
//! `copyOwnership`).

use std::ffi::OsString;
use std::io::{BufReader, BufWriter, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::{Mode, fchmod, fstat};
use serde::{Deserialize, Serialize};
use shipyard_image::{CopyOptions, IdRanges, Ownership, unpack_tar};
use shipyard_runtime::container::{HelperFailure, HelperIo, RootView, start_helper_in_view};

use super::CpError;
use super::helpers::{answer, errno_of, is_directory, kind, open_asked, pack_failure, pipe};
use super::pack::Packer;

/// `volume-fill`'s arguments.
#[derive(Debug, Serialize, Deserialize)]
struct FillArgs {
    /// The volume's place, in the container.
    path: Vec<u8>,
    /// A fill left unfinished: finished, whatever's in the volume.
    refill: bool,
}

/// The most of the helper's answer read: its warnings, a few lines.
const MAX_SAID: u64 = 64 * 1024;

/// Fills the volume directory `data` with what `view`'s root has at
/// `place` (nothing if it has no directory there, or `data` isn't empty
/// by then, unless it's a `refill` of a fill left unfinished), on the
/// create's own thread: the helper dies with it, and it waits for the
/// helper.
///
/// # Errors
///
/// The directory can't be opened, the helper can't start, or the copy
/// failed (a file the unpacker refuses, a full disk): what was copied
/// stays. Else, what the copy left out or changed (its warnings).
pub(crate) fn fill(
    view: &RootView,
    place: &Path,
    data: &Path,
    refill: bool,
) -> Result<Vec<String>, CpError> {
    let io = |doing: String| {
        move |errno: Errno| CpError::Io {
            doing,
            error: std::io::Error::from(errno),
        }
    };
    // `O_PATH`: a volume filled before may be the image's owner's, which
    // the host's user can't read (rootless); the helper reopens it, root
    // over its owner in the container's user namespace.
    let dir = open(
        data,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(io(format!("opening {}", data.display())))?;
    let args = serde_json::to_vec(&FillArgs {
        path: place.as_os_str().as_bytes().to_vec(),
        refill,
    })
    .map_err(|error| CpError::Io {
        doing: "encoding the helper's arguments".to_owned(),
        error: std::io::Error::other(error),
    })?;
    let (read, write) = pipe()?;
    let helper = start_helper_in_view(view, "volume-fill", &args, &dir, &write)?;
    drop((write, dir));
    answer(read, helper, MAX_SAID, "reading what the fill said")
}

// --- Inside the container's root (the helper's function)

/// `volume-fill`: the place's contents copied into the volume directory
/// on standard input, if the place is a directory and the volume is
/// still empty; then the volume takes the place's owner and mode.
pub(super) fn volume_fill(io: &HelperIo) -> Result<(), HelperFailure> {
    let args: FillArgs = serde_json::from_slice(&io.args).map_err(|_| {
        HelperFailure::new(kind::OTHER, Errno::EINVAL, "reading the fill's arguments")
    })?;
    let path = PathBuf::from(OsString::from_vec(args.path));
    // Resolved inside the root, as the runtime binds the volume there.
    let source = match open_asked(io, &path, true) {
        Ok(fd) if is_directory(&fd) => Some(fd),
        Ok(_) => None,
        Err(failure) if failure.kind == kind::NOT_FOUND => None,
        Err(failure) => return Err(failure),
    };
    // Given as `O_PATH`: opened here, where its owner is in reach.
    let volume = io
        .reopen(&io.stdin, OFlag::O_RDONLY | OFlag::O_DIRECTORY)
        .map_err(|errno| HelperFailure::new(kind::OTHER, errno, "opening the volume"))?;
    let warnings = match source {
        // Again, here: another container may have written to it since.
        Some(source) if args.refill || is_empty(&volume)? => {
            let report = copy(io, &source, &volume)?;
            take_owner_and_mode(&source, &volume)?;
            super::warnings(&report, 0)
        }
        _ => Vec::new(),
    };
    let said = serde_json::to_vec(&warnings)
        .map_err(|_| HelperFailure::new(kind::OTHER, Errno::EINVAL, "saying what was filled"))?;
    (&io.stdout).write_all(&said).map_err(|error| {
        HelperFailure::new(kind::OTHER, errno_of(&error), "saying what was filled")
    })
}

/// Whether the directory `dir` has nothing in it.
fn is_empty(dir: &OwnedFd) -> Result<bool, HelperFailure> {
    let listing = || {
        let opened = openat(
            dir,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let mut entries = nix::dir::Dir::from_fd(opened)?;
        let mut names = entries.iter();
        Ok(!names.any(|entry| {
            entry.is_err()
                || entry.is_ok_and(|entry| !matches!(entry.file_name().to_bytes(), b"." | b".."))
        }))
    };
    listing().map_err(|errno| HelperFailure::new(kind::OTHER, errno, "reading the volume"))
}

/// `source`'s contents packed on a thread, and unpacked into `volume` as
/// they come: the image's owners kept (within the user namespace's IDs).
fn copy(
    io: &HelperIo,
    source: &OwnedFd,
    volume: &OwnedFd,
) -> Result<shipyard_image::LayerReport, HelperFailure> {
    let (reader, writer) = std::io::pipe()
        .map_err(|error| HelperFailure::new(kind::OTHER, errno_of(&error), "making a pipe"))?;
    std::thread::scope(|scope| {
        let packing = scope.spawn(move || {
            let reopen = |fd: &OwnedFd, flags: OFlag| io.reopen(fd, flags);
            let mut out = BufWriter::new(writer);
            let mut packer = Packer::new(&mut out, &reopen).refusing_link_copies();
            packer
                .add_contents(source)
                .and_then(|()| packer.finish().map(drop))?;
            out.flush().map_err(super::PackError::Write)
        });
        let options = CopyOptions {
            ownership: Ownership::Apply {
                uids: IdRanges(io.uids.clone()),
                gids: IdRanges(io.gids.clone()),
                shift: None,
            },
            owner: None,
            // cp's, but for the free space it leaves: a fill isn't asked
            // for, so it keeps the image store's reserve.
            limits: shipyard_image::Limits {
                min_free: None,
                ..super::limits()
            },
            top: None,
        };
        let mut input = BufReader::new(reader);
        let unpacked = unpack_tar(&mut input, volume, &options);
        // What follows the archive's end (tar's padding), read so the
        // packer's last writes still find a reader.
        let _ = std::io::copy(&mut input, &mut std::io::sink());
        drop(input);
        let packed = packing.join().map_err(|_| {
            HelperFailure::new(kind::OTHER, Errno::EINVAL, "packing the image's files")
        })?;
        // The packer's failure first: the unpacker then saw the stream
        // end short, which says less.
        packed.map_err(|error| pack_failure(&error))?;
        unpacked.map_err(|error| HelperFailure::plain(kind::OTHER, error.to_string()))
    })
}

/// `volume` takes `source`'s owner and mode (its permission bits, setgid
/// and sticky ones too), as Docker's `copyOwnership` gives a filled
/// volume its place's.
fn take_owner_and_mode(source: &OwnedFd, volume: &OwnedFd) -> Result<(), HelperFailure> {
    let stat = fstat(source)
        .map_err(|errno| HelperFailure::new(kind::OTHER, errno, "reading the place's owner"))?;
    nix::unistd::fchown(
        volume,
        Some(nix::unistd::Uid::from_raw(stat.st_uid)),
        Some(nix::unistd::Gid::from_raw(stat.st_gid)),
    )
    .map_err(|errno| HelperFailure::new(kind::OTHER, errno, "giving the volume its owner"))?;
    fchmod(volume, Mode::from_bits_truncate(stat.st_mode & 0o7777))
        .map_err(|errno| HelperFailure::new(kind::OTHER, errno, "giving the volume its mode"))
}
