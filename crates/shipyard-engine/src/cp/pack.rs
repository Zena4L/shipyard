//! Writing a tree as a tar stream, by descriptor (`cp`'s; Linux).
//!
//! The same code runs in two places: inside a container, in the helper
//! (copying out of it), and on the host (copying into it). Either way the
//! tree may be changing under it, and inside a container it's hostile:
//!
//! - **Nothing is reached by path.** Each entry is opened `O_PATH |
//!   O_NOFOLLOW` relative to its directory's descriptor, and looked at with
//!   `fstat`: a symlink is recorded as one, never followed.
//! - **Only regular files and directories are opened**, by reopening that
//!   same `O_PATH` descriptor: a device swapped in is never opened (opening
//!   runs its driver), a FIFO never blocks it. They're written as headers
//!   only.
//! - **A file is written at the size its header says**, cut short or padded
//!   with zeros if it changes meanwhile: the stream stays well-formed.
//! - **Pseudo-filesystems aren't entered** (`/proc`, `/sys`, devpts,
//!   cgroupfs and the like): what they hold isn't files to copy. Volumes and
//!   tmpfs are.
//! - **Bounded:** at most [`MAX_DEPTH`] directories deep (each level holds
//!   one descriptor), at most [`MAX_NAMES`] names in one directory, and
//!   [`MAX_LINKS`] files with more than one name remembered (any more are
//!   written as copies, or refused by a fill's packer: an image can't
//!   multiply its data in a volume): a container can't make the copy hold
//!   unbounded memory (the helper isn't in its cgroup).
//!
//! No extended attributes are written: no file capability travels with a
//! copy.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, openat, readlinkat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat};

/// The deepest directory written, below the source.
pub(crate) const MAX_DEPTH: usize = 256;

/// The most names one directory may hold (its names are sorted in memory).
pub(crate) const MAX_NAMES: usize = 1_000_000;

/// The most files with more than one name whose first name is remembered.
const MAX_LINKS: usize = 1_000_000;

/// The filesystems whose directories aren't entered: the kernel's views
/// (`statfs(2)`'s magic numbers).
const PSEUDO_FILESYSTEMS: [u64; 16] = [
    0x9fa0,      // proc
    0x6265_6572, // sysfs
    0x1cd1,      // devpts
    0x0027_e0eb, // cgroup
    0x6367_7270, // cgroup2
    0x1980_0202, // mqueue
    0x6462_6720, // debugfs
    0x7472_6163, // tracefs
    0x7363_6673, // securityfs
    0xcafe_4a11, // bpf
    0x6265_6570, // configfs
    0x6573_5543, // fusectl
    0x6165_676c, // pstore
    0xde5e_81e4, // efivarfs
    0xf97c_ff8c, // selinuxfs
    0x4249_4e4d, // binfmt_misc
];

/// Why a tree couldn't be written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackError {
    /// Reading the tree failed.
    #[error("{doing} {path}: {source}")]
    Read {
        /// What was being done.
        doing: &'static str,
        /// To what (as named in the archive).
        path: String,
        /// Why.
        source: io::Error,
    },
    /// The tree is deeper than a copy goes (256 directories).
    #[error("`{0}` is more than {MAX_DEPTH} directories deep")]
    TooDeep(String),
    /// A directory holds more names than a copy takes (a million).
    #[error("`{0}` holds more than {MAX_NAMES} names")]
    TooMany(String),
    /// Writing the archive failed (its reader went away, say).
    #[error("writing the archive: {0}")]
    Write(io::Error),
    /// More files with several names than a fill remembers (a million):
    /// each further name would be a copy of its data (11.7: an image could
    /// turn a small layer into a volume that fills the disk).
    #[error("`{0}` is past {MAX_LINKS} files with more than one name")]
    TooManyLinks(String),
}

/// Reopens an `O_PATH` descriptor with real flags, as the same file.
pub(crate) type Reopen<'r> = &'r dyn Fn(&OwnedFd, OFlag) -> nix::Result<OwnedFd>;

/// Writes entries to a tar stream. One that failed never ends its stream
/// with the end-of-archive marker (the `tar` crate writes one when a builder
/// is dropped): its reader must see it cut off, never a whole archive.
pub(crate) struct Packer<'r, W: Write> {
    builder: tar::Builder<Stoppable<W>>,
    reopen: Reopen<'r>,
    /// Files with more than one link, by identity: the first one's name.
    links: HashMap<(u64, u64), PathBuf>,
    /// Sockets left out (a socket can't be archived).
    pub(crate) sockets_skipped: u64,
    /// Whether a file with several names past [`MAX_LINKS`] is refused
    /// rather than written as a copy.
    refuse_link_copies: bool,
    /// How many such files it remembers ([`MAX_LINKS`]; fewer in a test).
    max_links: usize,
    /// The mount its tree is on, if it keeps to it ([`Packer::within_mount`]).
    own_mount: Option<u64>,
    /// Whether a directory's name ends with `/` ([`Packer::naming_directories`]).
    directory_slash: bool,
}

impl<'r, W: Write> Packer<'r, W> {
    pub(crate) fn new(out: W, reopen: Reopen<'r>) -> Self {
        Self {
            builder: tar::Builder::new(Stoppable {
                inner: out,
                stopped: false,
            }),
            reopen,
            links: HashMap::new(),
            sockets_skipped: 0,
            refuse_link_copies: false,
            max_links: MAX_LINKS,
            own_mount: None,
            directory_slash: false,
        }
    }

    /// One that names a directory with a trailing `/` (`bin/`), as Docker's
    /// `export` and GNU tar write them (`export`'s, step 12.2b: checked
    /// against Docker 29's).
    pub(crate) fn naming_directories(mut self) -> Self {
        self.directory_slash = true;
        self
    }

    /// `name` as a directory's entry is named.
    fn directory_name(&self, name: &Path) -> PathBuf {
        if self.directory_slash && !name.as_os_str().is_empty() {
            let mut named = name.as_os_str().to_owned();
            named.push("/");
            PathBuf::from(named)
        } else {
            name.to_owned()
        }
    }

    /// One that keeps to the mount `root` is on (`export`'s walk where a
    /// running container's root can't be cloned alone, step 12.2b): a mount
    /// point under it is written as itself (a directory, an empty file),
    /// never what's mounted there. Told by mount ID, never `st_dev`
    /// (overlayfs gives a file its layer's device).
    pub(crate) fn within_mount(mut self, root: &OwnedFd) -> Result<Self, PackError> {
        let id = shipyard_sys::fs::mount_id(root).map_err(|source| PackError::Read {
            doing: "reading the mount of",
            path: "/".to_owned(),
            source,
        })?;
        self.own_mount = Some(id);
        Ok(self)
    }

    /// One that refuses a file with several names past [`MAX_LINKS`] rather
    /// than write a copy of it: a fill's, whose tree an image chose (a
    /// `cp`'s copies are the user's own asking, as `tar`'s are).
    pub(crate) fn refusing_link_copies(mut self) -> Self {
        self.refuse_link_copies = true;
        self
    }

    /// Adds what `fd` is (open `O_PATH`, a symlink itself if it is one) as
    /// `name`, and, a directory, everything under it.
    pub(crate) fn add(&mut self, fd: &OwnedFd, name: &Path) -> Result<(), PackError> {
        let added = self.entry(fd, name, 0);
        self.stop_if_failed(added)
    }

    /// Adds what's under the directory `fd` (open `O_PATH`), named from it.
    pub(crate) fn add_contents(&mut self, fd: &OwnedFd) -> Result<(), PackError> {
        let added = self.children(fd, Path::new(""), 0);
        self.stop_if_failed(added)
    }

    /// Ends the archive and returns its writer.
    pub(crate) fn finish(self) -> Result<W, PackError> {
        self.builder
            .into_inner()
            .map(|stoppable| stoppable.inner)
            .map_err(PackError::Write)
    }

    /// `result`, and nothing more written if it's a failure.
    fn stop_if_failed(&mut self, result: Result<(), PackError>) -> Result<(), PackError> {
        if result.is_err() {
            self.builder.get_mut().stopped = true;
        }
        result
    }

    fn entry(&mut self, fd: &OwnedFd, name: &Path, depth: usize) -> Result<(), PackError> {
        let stat = fstat(fd).map_err(reading("reading", name))?;
        let kind = SFlag::from_bits_truncate(stat.st_mode & SFlag::S_IFMT.bits());
        let mut header = header_of(&stat);
        if let Some(own) = self.own_mount {
            let id = shipyard_sys::fs::mount_id(fd).map_err(|source| PackError::Read {
                doing: "reading the mount of",
                path: name.display().to_string(),
                source,
            })?;
            if id != own {
                // Another mount (a volume, a bind, /proc): only its place.
                if kind == SFlag::S_IFDIR {
                    header.set_entry_type(tar::EntryType::Directory);
                    let named = self.directory_name(name);
                    return self.append(&mut header, &named, io::empty());
                }
                header.set_entry_type(tar::EntryType::Regular);
                return self.append(&mut header, name, io::empty());
            }
        }
        if kind == SFlag::S_IFDIR {
            header.set_entry_type(tar::EntryType::Directory);
            let named = self.directory_name(name);
            self.append(&mut header, &named, io::empty())?;
            return self.children(fd, name, depth);
        }
        if kind == SFlag::S_IFREG {
            return self.file(fd, &stat, header, name);
        }
        if kind == SFlag::S_IFLNK {
            let target = readlinkat(fd, "").map_err(reading("reading the link", name))?;
            header.set_entry_type(tar::EntryType::Symlink);
            return self
                .builder
                .append_link(&mut header, name, target)
                .map_err(PackError::Write);
        }
        let (kind, device) = match kind {
            SFlag::S_IFIFO => (tar::EntryType::Fifo, false),
            SFlag::S_IFCHR => (tar::EntryType::Char, true),
            SFlag::S_IFBLK => (tar::EntryType::Block, true),
            _ => {
                self.sockets_skipped += 1;
                return Ok(());
            }
        };
        header.set_entry_type(kind);
        if device {
            let number = |n: u64| {
                u32::try_from(n).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
            };
            let set = number(nix::sys::stat::major(stat.st_rdev))
                .and_then(|major| header.set_device_major(major))
                .and_then(|()| number(nix::sys::stat::minor(stat.st_rdev)))
                .and_then(|minor| header.set_device_minor(minor));
            set.map_err(|source| PackError::Read {
                doing: "recording the device numbers of",
                path: name.display().to_string(),
                source,
            })?;
        }
        self.append(&mut header, name, io::empty())
    }

    /// A regular file: a link to one already written, or its data, read
    /// through the same file reopened.
    fn file(
        &mut self,
        fd: &OwnedFd,
        stat: &FileStat,
        mut header: tar::Header,
        name: &Path,
    ) -> Result<(), PackError> {
        let identity = (stat.st_dev, stat.st_ino);
        if stat.st_nlink > 1 {
            if let Some(first) = self.links.get(&identity) {
                header.set_entry_type(tar::EntryType::Link);
                let first = first.clone();
                return self
                    .builder
                    .append_link(&mut header, name, first)
                    .map_err(PackError::Write);
            }
            if self.links.len() < self.max_links {
                self.links.insert(identity, name.to_owned());
            } else if self.refuse_link_copies {
                return Err(PackError::TooManyLinks(name.display().to_string()));
            }
        }
        // Not blocking on the open (a lease a container holds), but read
        // as usual (a network filesystem may say "again" otherwise).
        let file = (self.reopen)(fd, OFlag::O_RDONLY | OFlag::O_NOCTTY | OFlag::O_NONBLOCK)
            .and_then(|file| {
                nix::fcntl::fcntl(&file, nix::fcntl::FcntlArg::F_SETFL(OFlag::O_RDONLY))?;
                Ok(file)
            })
            .map_err(reading("opening", name))?;
        let size = u64::try_from(stat.st_size).unwrap_or(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(size);
        self.append(
            &mut header,
            name,
            Exact::new(std::fs::File::from(file), size, name),
        )
    }

    /// Everything in the directory `fd`, in name order.
    fn children(&mut self, fd: &OwnedFd, name: &Path, depth: usize) -> Result<(), PackError> {
        if is_pseudo(fd) {
            return Ok(());
        }
        if depth >= MAX_DEPTH {
            return Err(PackError::TooDeep(name.display().to_string()));
        }
        let names = {
            let dir = (self.reopen)(fd, OFlag::O_RDONLY | OFlag::O_DIRECTORY)
                .map_err(reading("opening", name))?;
            let mut dir = nix::dir::Dir::from_fd(dir).map_err(reading("listing", name))?;
            let mut names: Vec<OsString> = Vec::new();
            for entry in dir.iter() {
                let entry = entry.map_err(reading("listing", name))?;
                let bytes = entry.file_name().to_bytes();
                if bytes != b"." && bytes != b".." {
                    if names.len() >= MAX_NAMES {
                        return Err(PackError::TooMany(name.display().to_string()));
                    }
                    names.push(std::ffi::OsStr::from_bytes(bytes).to_owned());
                }
            }
            names.sort_unstable();
            names
        };
        for child in names {
            let path = name.join(&child);
            let opened = openat(
                fd,
                child.as_os_str(),
                OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            );
            match opened {
                Ok(opened) => self.entry(&opened, &path, depth + 1)?,
                // Gone since it was listed: as tar does, left out.
                Err(Errno::ENOENT) => {}
                Err(errno) => return Err(reading("opening", &path)(errno)),
            }
        }
        Ok(())
    }

    fn append(
        &mut self,
        header: &mut tar::Header,
        name: &Path,
        data: impl Read,
    ) -> Result<(), PackError> {
        self.builder.append_data(header, name, data).map_err(|err| {
            match err
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<ReadFailed>())
            {
                Some(failed) => PackError::Read {
                    doing: "reading",
                    path: failed.path.clone(),
                    source: io::Error::from_raw_os_error(failed.errno),
                },
                None => PackError::Write(err),
            }
        })
    }
}

/// A header with `stat`'s mode, owner and time.
fn header_of(stat: &FileStat) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    #[allow(
        clippy::useless_conversion,
        reason = "mode_t's width differs between C libraries"
    )]
    header.set_mode(u32::from(stat.st_mode) & 0o7777);
    header.set_uid(u64::from(stat.st_uid));
    header.set_gid(u64::from(stat.st_gid));
    header.set_mtime(u64::try_from(stat.st_mtime).unwrap_or(0));
    header.set_size(0);
    header
}

/// A writer that can be stopped: then it writes nothing more.
pub(crate) struct Stoppable<W> {
    inner: W,
    stopped: bool,
}

impl<W: Write> Write for Stoppable<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.stopped {
            return Err(io::Error::other("the archive was stopped"));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Whether the directory `fd` is on one of the kernel's pseudo-filesystems.
fn is_pseudo(fd: &OwnedFd) -> bool {
    nix::sys::statfs::fstatfs(fd).is_ok_and(|fs| {
        #[allow(
            clippy::useless_conversion,
            reason = "the filesystem type's width differs between C libraries"
        )]
        let magic = u64::try_from(fs.filesystem_type().0).unwrap_or(0);
        PSEUDO_FILESYSTEMS.contains(&magic)
    })
}

/// An errno while `doing` something to `path`, as a [`PackError`].
fn reading<'p>(doing: &'static str, path: &'p Path) -> impl FnOnce(Errno) -> PackError + 'p {
    move |errno| PackError::Read {
        doing,
        path: path.display().to_string(),
        source: io::Error::from(errno),
    }
}

/// A read of a file's data failed (carried through the archive writer's
/// `io::Error` to tell it from a failed write).
#[derive(Debug, thiserror::Error)]
#[error("reading {path}")]
struct ReadFailed {
    path: String,
    errno: i32,
}

/// Exactly `size` bytes of a file: what it has, then zeros if it shrank.
struct Exact {
    file: std::fs::File,
    left: u64,
    ended: bool,
    path: PathBuf,
}

impl Exact {
    fn new(file: std::fs::File, size: u64, path: &Path) -> Self {
        Self {
            file,
            left: size,
            ended: false,
            path: path.to_owned(),
        }
    }
}

impl Read for Exact {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        let buf = &mut buf[..want];
        let read = if self.ended {
            0
        } else {
            loop {
                match self.file.read(buf) {
                    Ok(read) => break read,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        return Err(io::Error::other(ReadFailed {
                            path: self.path.display().to_string(),
                            errno: err.raw_os_error().unwrap_or(libc::EIO),
                        }));
                    }
                }
            }
        };
        let read = if read == 0 {
            // Shrunk: zeros for the rest.
            self.ended = true;
            buf.fill(0);
            want
        } else {
            read
        };
        self.left -= read as u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// A scratch directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("shipyard-pack-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    use super::super::helpers::host_reopen as reopen;

    fn path_fd(path: &Path) -> OwnedFd {
        nix::fcntl::open(
            path,
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    /// An archive's entries, as (name, kind, size, link) rows.
    fn listing(archive: &[u8]) -> Vec<(String, tar::EntryType, u64, Option<String>)> {
        let mut reader = tar::Archive::new(archive);
        reader
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let header = entry.header();
                (
                    entry.path().unwrap().display().to_string(),
                    header.entry_type(),
                    header.size().unwrap(),
                    entry
                        .link_name()
                        .unwrap()
                        .map(|link| link.display().to_string()),
                )
            })
            .collect()
    }

    fn pack(path: &Path, name: &str) -> Vec<u8> {
        let mut packer = Packer::new(Vec::new(), &reopen);
        packer.add(&path_fd(path), Path::new(name)).unwrap();
        packer.finish().unwrap()
    }

    #[test]
    fn a_tree_is_written_without_following_links() {
        let scratch = Scratch::new("tree");
        let tree = scratch.0.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/file"), "data").unwrap();
        std::fs::hard_link(tree.join("sub/file"), tree.join("again")).unwrap();
        std::os::unix::fs::symlink("/etc", tree.join("etc")).unwrap();
        nix::unistd::mkfifo(&tree.join("fifo"), Mode::from_bits_truncate(0o600)).unwrap();
        let rows = listing(&pack(&tree, "copy"));
        let names: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "copy",
                "copy/again",
                "copy/etc",
                "copy/fifo",
                "copy/sub",
                "copy/sub/file"
            ]
        );
        assert_eq!(rows[1].1, tar::EntryType::Regular);
        assert_eq!(rows[1].2, 4);
        assert_eq!(rows[2].1, tar::EntryType::Symlink, "never followed");
        assert_eq!(rows[2].3.as_deref(), Some("/etc"));
        assert_eq!(rows[3].1, tar::EntryType::Fifo, "a header, never opened");
        assert_eq!(
            rows[5].1,
            tar::EntryType::Link,
            "the second name of one file"
        );
        assert_eq!(rows[5].3.as_deref(), Some("copy/again"));
    }

    /// Past the files with several names it remembers, a packer writes the
    /// next one as a copy, but a fill's refuses it (its kind): an image
    /// can't make one file's data many times over in a volume (11.7).
    #[test]
    fn a_fill_refuses_link_copies_past_its_limit() {
        let scratch = Scratch::new("many-links");
        for name in ["a", "b", "c"] {
            std::fs::write(scratch.0.join(name), "data").unwrap();
            std::fs::hard_link(scratch.0.join(name), scratch.0.join(format!("{name}2"))).unwrap();
        }
        let packed = |refusing: bool| {
            let mut packer = Packer::new(Vec::new(), &reopen);
            if refusing {
                packer = packer.refusing_link_copies();
            }
            packer.max_links = 2;
            packer
                .add_contents(&path_fd(&scratch.0))
                .and_then(|()| packer.finish())
        };
        let copied = listing(&packed(false).unwrap());
        let links = copied
            .iter()
            .filter(|row| row.1 == tar::EntryType::Link)
            .count();
        assert_eq!((copied.len(), links), (6, 2), "{copied:?}");
        assert!(
            matches!(packed(true), Err(PackError::TooManyLinks(_))),
            "a fill's packer past its links"
        );
    }

    #[test]
    fn a_directorys_contents_are_named_from_it() {
        let scratch = Scratch::new("contents");
        std::fs::write(scratch.0.join("a"), "x").unwrap();
        let mut packer = Packer::new(Vec::new(), &reopen);
        packer.add_contents(&path_fd(&scratch.0)).unwrap();
        let rows = listing(&packer.finish().unwrap());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "a");
    }

    #[test]
    fn a_file_that_changes_keeps_its_headers_size() {
        let scratch = Scratch::new("exact");
        let path = scratch.0.join("f");
        std::fs::write(&path, "0123456789").unwrap();
        let mut short = Exact::new(std::fs::File::open(&path).unwrap(), 4, &path);
        let mut out = Vec::new();
        short.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"0123", "cut short");
        let mut long = Exact::new(std::fs::File::open(&path).unwrap(), 14, &path);
        let mut out = Vec::new();
        long.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"0123456789\0\0\0\0", "padded");
    }

    #[test]
    fn pseudo_filesystems_are_not_entered() {
        let rows = listing(&pack(Path::new("/proc"), "proc"));
        assert_eq!(rows.len(), 1, "the directory only: {rows:?}");
        assert_eq!(rows[0].1, tar::EntryType::Directory);
    }

    #[test]
    fn a_tree_too_deep_is_refused() {
        let scratch = Scratch::new("deep");
        let mut path = scratch.0.join("d");
        for _ in 0..=MAX_DEPTH {
            path.push("d");
        }
        std::fs::create_dir_all(&path).unwrap();
        let mut out = Vec::new();
        let mut packer = Packer::new(&mut out, &reopen);
        let result = packer.add(&path_fd(&scratch.0.join("d")), Path::new("d"));
        assert!(matches!(result, Err(PackError::TooDeep(_))), "{result:?}");
        drop(packer);
        // Cut off, never ended as if whole.
        assert!(!out.is_empty());
        assert!(
            out.len() < 1024 || out[out.len() - 1024..].iter().any(|&b| b != 0),
            "the end-of-archive marker was written"
        );
    }

    #[test]
    fn a_closed_reader_is_a_write_error() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let scratch = Scratch::new("closed");
        let mut packer = Packer::new(Closed, &reopen);
        let result = packer.add(&path_fd(&scratch.0), Path::new("x"));
        assert!(matches!(result, Err(PackError::Write(_))), "{result:?}");
    }
}
