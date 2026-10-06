//! Resolving a path inside a container's root filesystem, as the container
//! will see it (Linux).
//!
//! Before the pivot, the root filesystem is just a directory on the host,
//! and its content isn't shipyard's. Resolved naively, a symlink in it
//! (`/data` → `/etc`) or a `..` would lead out of it, onto the host: a mount
//! meant for the container would land on the host's `/etc` (probe 11; the
//! runc CVE-2021-30465 class). So paths are resolved the way the kernel will
//! once the root is switched: symlinks are followed *inside* the root, and
//! `..` stops at it.
//!
//! **`openat2(RESOLVE_IN_ROOT)`** does exactly that, in the kernel (Linux
//! 5.6+). Where it's missing (older kernels, seccomp profiles, x86-64
//! emulation: probe 12), a component-by-component walk does the same: each
//! name is opened with `O_NOFOLLOW` relative to its directory, and a symlink
//! is read and its target walked in turn, from the root if it's absolute.
//!
//! Missing components are made as they're met (a mount target must exist):
//! directories, and the last one as a directory or an empty file.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};

use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat, openat2, readlinkat};
use nix::sys::stat::{Mode, SFlag, fstat, mkdirat};

use super::io_errno;

/// The most symlinks one resolution follows, as the kernel's own limit
/// (`MAXSYMLINKS`): more is a loop.
const MAX_SYMLINKS: usize = 40;

/// What the last component must be, made if it's missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leaf {
    Directory,
    /// Anything but a directory (made as an empty file): a target for
    /// binding a file onto.
    File,
}

/// Opens `path` inside `root` (`O_PATH`), resolved as the container will
/// see it, making what's missing on the way.
pub(crate) fn resolve_in_root(root: &OwnedFd, path: &Path, leaf: Leaf) -> nix::Result<OwnedFd> {
    resolve(root, path, leaf, Method::Openat2First)
}

/// How to resolve: the kernel first, or only the walk (for tests: the walk
/// is what runs where `openat2` is missing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Openat2First,
    #[cfg_attr(not(test), allow(dead_code, reason = "tests force the walk"))]
    WalkOnly,
}

fn resolve(root: &OwnedFd, path: &Path, leaf: Leaf, method: Method) -> nix::Result<OwnedFd> {
    if method == Method::Openat2First {
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS);
        match openat2(root, relative(path), how) {
            Ok(fd) => return checked(fd, leaf),
            // Not there yet (the walk makes it), or no `openat2` here: the
            // walk. Anything else is a real answer.
            Err(Errno::ENOENT | Errno::ENOSYS | Errno::EPERM | Errno::EAGAIN | Errno::EXDEV) => {}
            Err(errno) => return Err(errno),
        }
    }
    walk(root, path, leaf)
}

/// `path` without its leading `/`: relative to the root's descriptor.
fn relative(path: &Path) -> &Path {
    path.strip_prefix("/").unwrap_or(path)
}

/// The walk: one component at a time, never following a symlink by path.
fn walk(root: &OwnedFd, path: &Path, leaf: Leaf) -> nix::Result<OwnedFd> {
    let mut pending: VecDeque<OsString> = names(path).collect();
    // The directories above the current one, for `..`.
    let mut parents: Vec<OwnedFd> = Vec::new();
    let mut current = root.try_clone().map_err(|e| io_errno(&e))?;
    let mut symlinks = 0;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            // At the root, `..` is the root.
            if let Some(parent) = parents.pop() {
                current = parent;
            }
            continue;
        }
        let last = pending.is_empty();
        let next = match open_nofollow(&current, &name) {
            Ok(next) => next,
            Err(Errno::ENOENT) => make(&current, &name, if last { leaf } else { Leaf::Directory })?,
            Err(errno) => return Err(errno),
        };
        let kind = file_type(&next)?;
        if kind == SFlag::S_IFLNK {
            symlinks += 1;
            if symlinks > MAX_SYMLINKS {
                return Err(Errno::ELOOP);
            }
            let target = readlinkat(&current, name.as_os_str())?;
            let target = Path::new(&target);
            if target.has_root() {
                parents.clear();
                current = root.try_clone().map_err(|e| io_errno(&e))?;
            }
            for (at, part) in names(target).enumerate() {
                pending.insert(at, part);
            }
            continue;
        }
        if !last && kind != SFlag::S_IFDIR {
            return Err(Errno::ENOTDIR);
        }
        parents.push(std::mem::replace(&mut current, next));
    }
    checked(current, leaf)
}

/// The names in `path`, with `..` kept (the walk handles it) and `/` and
/// `.` dropped.
fn names(path: &Path) -> impl Iterator<Item = OsString> + '_ {
    path.components().filter_map(|part| match part {
        Component::Normal(name) => Some(name.to_owned()),
        Component::ParentDir => Some(OsString::from("..")),
        Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
    })
}

fn open_nofollow(dir: &OwnedFd, name: &OsStr) -> nix::Result<OwnedFd> {
    openat(
        dir,
        name,
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

/// Makes `name` in `dir` as `leaf` (another process may have just made it:
/// fine), and opens it.
fn make(dir: &OwnedFd, name: &OsStr, leaf: Leaf) -> nix::Result<OwnedFd> {
    let made = match leaf {
        Leaf::Directory => mkdirat(dir, name, Mode::from_bits_truncate(0o755)),
        Leaf::File => openat(
            dir,
            name,
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o644),
        )
        .map(drop),
    };
    match made {
        Ok(()) | Err(Errno::EEXIST) => open_nofollow(dir, name),
        Err(errno) => Err(errno),
    }
}

/// `fd`, if it's the kind `leaf` asks for.
fn checked(fd: OwnedFd, leaf: Leaf) -> nix::Result<OwnedFd> {
    let is_directory = file_type(&fd)? == SFlag::S_IFDIR;
    match (leaf, is_directory) {
        (Leaf::Directory, false) => Err(Errno::ENOTDIR),
        (Leaf::File, true) => Err(Errno::EISDIR),
        _ => Ok(fd),
    }
}

fn file_type(fd: &OwnedFd) -> nix::Result<SFlag> {
    let stat = fstat(fd.as_fd())?;
    Ok(SFlag::from_bits_truncate(
        stat.st_mode & SFlag::S_IFMT.bits(),
    ))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;

    use super::*;

    /// A scratch root filesystem, removed on drop.
    struct Scratch {
        dir: PathBuf,
        fd: OwnedFd,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("shipyard-resolve-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("etc")).unwrap();
            let fd = nix::fcntl::open(
                &dir,
                OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            Self { dir, fd }
        }

        /// Where `fd` ends up, as a path inside the root ("" for the root).
        fn inside(&self, fd: &OwnedFd) -> String {
            let at = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
            let at = at.strip_prefix(&self.dir).unwrap_or_else(|_| {
                panic!("resolved out of the root: {}", at.display());
            });
            at.to_string_lossy().into_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const METHODS: [Method; 2] = [Method::Openat2First, Method::WalkOnly];

    fn resolved(root: &Scratch, path: &str, leaf: Leaf, method: Method) -> nix::Result<String> {
        resolve(&root.fd, Path::new(path), leaf, method).map(|fd| root.inside(&fd))
    }

    #[test]
    fn missing_directories_are_made() {
        for method in METHODS {
            let root = Scratch::new(&format!("make-{method:?}"));
            let got = resolved(&root, "/a/b/c", Leaf::Directory, method).unwrap();
            assert_eq!(got, "a/b/c", "{method:?}");
            assert!(root.dir.join("a/b/c").is_dir());
            let got = resolved(&root, "/a/file", Leaf::File, method).unwrap();
            assert_eq!(got, "a/file");
            assert!(root.dir.join("a/file").is_file());
        }
    }

    #[test]
    fn dot_dot_stops_at_the_root() {
        for method in METHODS {
            let root = Scratch::new(&format!("dotdot-{method:?}"));
            let got = resolved(&root, "/../../../etc/x", Leaf::Directory, method).unwrap();
            assert_eq!(got, "etc/x", "{method:?}");
        }
    }

    #[test]
    fn symlinks_resolve_inside_the_root() {
        for method in METHODS {
            let root = Scratch::new(&format!("links-{method:?}"));
            // Absolute: the host's /etc, if followed naively.
            std::os::unix::fs::symlink("/etc", root.dir.join("evil")).unwrap();
            // Relative, climbing out.
            std::os::unix::fs::symlink("../../../../etc", root.dir.join("etc/climb")).unwrap();
            std::os::unix::fs::symlink("etc", root.dir.join("rel")).unwrap();
            for (path, expected) in [
                ("/evil/made-by-test", "etc/made-by-test"),
                ("/etc/climb/also", "etc/also"),
                ("/rel/third", "etc/third"),
            ] {
                let got = resolved(&root, path, Leaf::Directory, method).unwrap();
                assert_eq!(got, expected, "{path} ({method:?})");
            }
            assert!(!Path::new("/etc/made-by-test").exists(), "made on the host");
        }
    }

    #[test]
    fn a_symlink_loop_is_an_error() {
        for method in METHODS {
            let root = Scratch::new(&format!("loop-{method:?}"));
            std::os::unix::fs::symlink("b", root.dir.join("a")).unwrap();
            std::os::unix::fs::symlink("a", root.dir.join("b")).unwrap();
            assert_eq!(
                resolved(&root, "/a/x", Leaf::Directory, method),
                Err(Errno::ELOOP),
                "{method:?}"
            );
        }
    }

    #[test]
    fn the_leaf_must_be_the_right_kind() {
        for method in METHODS {
            let root = Scratch::new(&format!("kind-{method:?}"));
            std::fs::write(root.dir.join("etc/file"), "").unwrap();
            assert_eq!(
                resolved(&root, "/etc/file", Leaf::Directory, method),
                Err(Errno::ENOTDIR)
            );
            assert_eq!(
                resolved(&root, "/etc/file/below", Leaf::Directory, method),
                Err(Errno::ENOTDIR)
            );
            assert_eq!(
                resolved(&root, "/etc", Leaf::File, method),
                Err(Errno::EISDIR)
            );
        }
    }
}
