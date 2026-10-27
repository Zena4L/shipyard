//! A root filesystem of the container's own (Linux; `pivot_root(2)`,
//! `mount_namespaces(7)`). Run by the container's clean image (`init.rs`),
//! in its new mount namespace, after `/` was made a slave.
//!
//! **Mounts first, then the pivot** (runc's order). Every mount is made in
//! one pass, in the order a config lists them (the spec's "MUST"): the
//! standard ones (`/proc`, `/sys`, `/dev` and those in it) wherever listed,
//! with Shipyard's own options, those not listed first (`Root::mount_order`),
//! and the container's own (`Root::mounts`: binds from the host and tmpfs)
//! between them, each at a destination resolved inside the root
//! (`resolve.rs`), never by a path that could lead out of it. Sysctls are
//! set after the pass, through the `/proc` made in it. `/proc` and `/sys`
//! are mounted while the host's tree is still there: under a user namespace (stage 4), the kernel only lets a
//! container mount `proc` or `sysfs` while a fully visible one is mounted
//! in its mount namespace, which stops being true once the host's tree is
//! gone.
//!
//! **Mount targets are never followed through symlinks.** The root
//! filesystem's content isn't shipyard's: a `proc` that is a symlink to
//! `/etc` would put the mount on the host's `/etc` (probe 11; runc's
//! CVE-2021-30465). So each target is opened with `O_NOFOLLOW` relative to
//! the root, and mounted on through its descriptor: `fchdir` to it, then
//! mount on `.`, so no path is resolved again. (Not `/proc/self/fd/N`:
//! that needs the host's `/proc` to be one that shows this process.)
//! Everything below `/dev` is
//! made in a fresh tmpfs, relative to its own descriptor: nothing of the
//! root filesystem's is involved.
//!
//! **Whatever is already mounted on a target is detached first** (in the
//! container's copy of the mount table only). A root filesystem prepared
//! as a chroot often has the host's `/dev` or `/proc` mounted in it: left
//! underneath the container's own, root inside could unmount the
//! container's and reach the host's devices and processes.
//!
//! **Then the pivot:** `pivot_root(".", ".")` stacks the old root on top of
//! the new one, and a lazy unmount detaches it, so the host's tree is
//! unreachable (probe 3). Masked and read-only paths come after: inside
//! the container, with no host left to escape to.

/// The root filesystem's directories the standard mounts go on. A
/// failure's detail is the number.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target {
    Proc = 0,
    Sys = 1,
    Dev = 2,
}

impl Target {
    const ALL: [Self; 3] = [Self::Proc, Self::Sys, Self::Dev];

    /// The target a failure's detail names.
    pub(crate) fn from_detail(detail: u64) -> Option<Self> {
        usize::try_from(detail)
            .ok()
            .and_then(|index| Self::ALL.get(index).copied())
    }

    /// Its name in the root filesystem.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Proc => "proc",
            Self::Sys => "sys",
            Self::Dev => "dev",
        }
    }
}

/// What's made in `/dev`. A failure's detail is the index.
pub(crate) const DEV_ENTRIES: [&str; 15] = [
    "/dev",
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/fd",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/ptmx",
    "/dev/pts",
    "/dev/shm",
    "/dev/mqueue",
];

/// Whether a mount at `destination` (as written) that resolved to
/// `target_place` was steered by a symlink into one of the standard mounts'
/// places in the root (at `root_place`): `/proc`, `/sys`, `/dev`, or one
/// nested in them (`/dev/pts`, `/sys/fs/cgroup`, …). Its place is in one
/// (or is one) and its destination, as written, isn't. Those are the
/// kernel's interfaces, or the standard mounts', made before or after: one
/// led there would land on them, or be hidden by them (F36). So
/// `/dev/ptmx`, which Shipyard links into `pts`, is refused as a mount's
/// destination, but `/dev/pts/x`, written there, isn't.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "mounts are made on Linux")
)]
pub(crate) fn steered(
    root_place: &std::path::Path,
    target_place: &std::path::Path,
    destination: &std::path::Path,
) -> bool {
    let Ok(inside) = target_place.strip_prefix(root_place) else {
        // Not under the root at all: it can't be told where it is.
        return true;
    };
    let written = destination.strip_prefix("/").unwrap_or(destination);
    crate::isolation::StandardMount::ORDER
        .into_iter()
        .map(|standard| {
            let place = standard.destination();
            place.strip_prefix('/').unwrap_or(place)
        })
        .any(|standard| inside.starts_with(standard) && !written.starts_with(standard))
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{clone_root_alone, enter, view};

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    use nix::errno::Errno;
    use nix::fcntl::{OFlag, openat};
    use nix::mount::{MntFlags, MsFlags, mount, umount2};
    use nix::sys::stat::{Mode, SFlag, fstatat, makedev, mkdirat, mknodat, umask};
    use nix::sys::statvfs::{FsFlags, statvfs};
    use nix::unistd::{fchdir, pivot_root, symlinkat};

    use super::{DEV_ENTRIES, Target, overlay};
    use crate::isolation::{CgroupMount, MountStep, StandardMount};
    use crate::process::child::Failure;
    use crate::process::failure::ChildStep;
    use crate::process::io_errno;
    use crate::process::resolve::{Leaf, resolve_in_root};
    use crate::process::setup::{DeviceSetup, MountSetup, RootSetup, SysSetup};
    use crate::process::sysctl;

    type Step<T = ()> = Result<T, Failure>;

    /// How many mounts stacked on one target are detached at most.
    const MAX_STACKED_MOUNTS: usize = 64;

    /// The flags of every mount here that holds no programs or devices.
    const NOSUID_NODEV_NOEXEC: MsFlags = MsFlags::MS_NOSUID
        .union(MsFlags::MS_NODEV)
        .union(MsFlags::MS_NOEXEC);

    /// Character devices every container gets: (name, major, minor). The
    /// same as runc's and Docker's.
    const DEVICES: [(&str, u64, u64); 6] = [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ];

    /// Symlinks in `/dev`: (name, target).
    const LINKS: [(&str, &str); 5] = [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ];

    /// Switches this process (and so the container) to `root`, setting
    /// `sysctls` on the way (before `/proc/sys` becomes read-only), and
    /// calling `before_pivot` once every mount is made, before the switch
    /// (the hooks' moment: `init.rs`). Returns what `before_pivot` did.
    pub(crate) fn enter<T>(
        root: &RootSetup,
        cwd: &Path,
        sysctls: &[(String, String)],
        cgroup: Option<CgroupMount>,
        before_pivot: impl FnOnce() -> Step<T>,
    ) -> Step<T> {
        // Device nodes and directories get exactly the modes given here.
        // Whatever runs in between (hooks), and after, gets the umask back.
        let umask_before = umask(Mode::empty());
        let (root_fd, cloned) = mount_root(root)?;
        // Where the root is, for where each mount lands (read once, when a
        // mount needs it).
        let mut root_place = None;
        // In the order given: a config's listed order (the spec's "MUST"),
        // the standard ones it doesn't list first (`Root::mount_order`).
        for step in &root.order {
            match *step {
                MountStep::Standard(standard) => {
                    mount_standard(&root_fd, root, standard, cgroup)?;
                }
                MountStep::Mount(index) => {
                    let (Some(mount), source) = (
                        root.mounts.get(index),
                        cloned.get(index).and_then(Option::as_ref),
                    ) else {
                        continue;
                    };
                    let root_place = match &root_place {
                        Some(place) => place,
                        None => root_place.insert(
                            place(&root_fd)
                                .map_err(|errno| Failure::at(ChildStep::MountTarget, errno))?,
                        ),
                    };
                    add_mount(&root_fd, root_place, mount, source)
                        .map_err(|(step, errno)| Failure::at(step, errno).detail(index))?;
                }
            }
        }
        if !sysctls.is_empty() {
            // Through the `/proc` mounted in the root filesystem.
            let proc = open_directory(&root_fd, Target::Proc.name())
                .map_err(|errno| Failure::at(ChildStep::Sysctl, errno))?;
            sysctl::apply(&proc, sysctls)?;
        }
        for (index, device) in root.devices.iter().enumerate() {
            make_device(&root_fd, device, root.user_namespace)
                .map_err(|errno| Failure::at(ChildStep::DeviceNode, errno).detail(index))?;
        }
        umask(umask_before);
        let before = before_pivot()?;
        umask(Mode::empty());
        pivot(&root_fd)?;
        for (index, path) in root.masked().enumerate() {
            mask(path).map_err(|errno| Failure::at(ChildStep::MaskPath, errno).detail(index))?;
        }
        for (index, path) in root.readonly().enumerate() {
            make_read_only(path)
                .map_err(|errno| Failure::at(ChildStep::ReadonlyPath, errno).detail(index))?;
        }
        // As Docker: a working directory the root filesystem doesn't have (an
        // image's `WorkingDir`, a `-w`) is made, root's, `rwxr-xr-x` (the
        // umask is 0 here). After the switch, so nothing here can lead to the
        // host; before a read-only root is. If it can't be made, changing to
        // it fails, saying so.
        {
            use std::os::unix::fs::DirBuilderExt as _;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(cwd);
        }
        if root.read_only {
            remount(Path::new("/"), MsFlags::MS_RDONLY)
                .map_err(|errno| Failure::at(ChildStep::ReadOnlyRoot, errno))?;
        }
        umask(umask_before);
        Ok(before)
    }

    /// A directory to attach a root real root made on: `root` in a small
    /// tmpfs on `/tmp`, in the container's mount namespace only (gone with
    /// the host's tree at the pivot). Opened `O_PATH`.
    ///
    /// It hides the host's `/tmp` from paths looked up after it: the bind
    /// sources are cloned before ([`clone_sources`]). The mount follows a
    /// `/tmp` that's a symlink, and so does the open after it.
    fn staging() -> Result<OwnedFd, Errno> {
        mount(
            Some("tmpfs"),
            "/tmp",
            Some("tmpfs"),
            NOSUID_NODEV_NOEXEC,
            Some("mode=700,size=64k"),
        )?;
        let tmp = nix::fcntl::open(
            "/tmp",
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        mkdirat(&tmp, "root", Mode::from_bits_truncate(0o700))?;
        openat(
            &tmp,
            "root",
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    }

    /// The root filesystem `root` describes, mounted: its overlay (or the
    /// one real root made, attached) bound onto itself. Its directory, and
    /// its bind sources cloned where that's needed (an attached overlay's:
    /// its staging hides the host's `/tmp`), for [`add_mount`].
    fn mount_root(root: &RootSetup) -> Step<(OwnedFd, Vec<Option<ClonedSource>>)> {
        // The bind sources, cloned before anything is mounted over the host's
        // paths (see `staging`), where the root needs it.
        let cloned;
        let root_fd = if let Some(mounted) = root.attach() {
            // Made by real root (`--uidmap`: the container's own root can't
            // reach the image store, nor the directory it's for): attached in
            // this mount namespace where the container's root can reach, a
            // private staging tmpfs; the mount's descriptor is the root's.
            // SAFETY: inherited across the exec for this; owned by nothing
            // else here.
            let mounted = unsafe { OwnedFd::from_raw_fd(mounted) };
            cloned = clone_sources(root, Which::Every)?;
            let fail = |errno| Failure::at(ChildStep::AttachOverlay, errno);
            let on = staging().map_err(fail)?;
            overlay::attach(&mounted, on.as_fd()).map_err(fail)?;
            mounted
        } else {
            // A source Shipyard made: by descriptor, never by its path (step
            // 11.7), before anything is mounted.
            cloned = clone_sources(root, Which::Made)?;
            // An image root: mounted here, inside the container's namespaces,
            // so a rootless overlay mount lands in the user namespace where
            // the kernel allows it.
            if let Some((lower, upper, work, userxattr)) = root.overlay() {
                mount_overlay(root.path(), &lower, upper, work, userxattr)?;
            }
            bind_root(root.path())?
        };
        Ok((root_fd, cloned))
    }

    /// The root filesystem `root` describes, as a start mounts it, for a
    /// helper reaching a stopped container's files (stage 9, step 9.4c2):
    /// its overlay (or the one real root made, attached), bound onto itself,
    /// and its binds (volumes) at their destinations, in their order. Not
    /// what only a running container has: no `/proc`, `/dev`, `/sys`, tmpfs
    /// mounts, devices or masks, and no pivot. Read-only if the container's
    /// root is (`--read-only`). Returns the root's directory.
    ///
    /// Runs in a mount namespace of the caller's own, made private: every
    /// mount goes with it.
    ///
    /// # Errors
    ///
    /// What it was doing, and why it failed.
    pub(crate) fn view(root: &RootSetup) -> Result<OwnedFd, (String, Errno)> {
        let (root_fd, cloned) = mount_root(root)
            .map_err(|failure| ("mounting the container's root".to_owned(), failure.errno()))?;
        let root_place =
            place(&root_fd).map_err(|errno| ("finding the container's root".to_owned(), errno))?;
        for step in &root.order {
            let MountStep::Mount(index) = *step else {
                continue;
            };
            // A bind (a volume, a host directory); a tmpfs is a running
            // container's only (Docker's `cp` leaves them out too).
            let Some(mount) = root
                .mounts
                .get(index)
                .filter(|mount| mount.source().is_some())
            else {
                continue;
            };
            add_mount(
                &root_fd,
                &root_place,
                mount,
                cloned.get(index).and_then(Option::as_ref),
            )
            .map_err(|(_, errno)| (format!("mounting {}", mount.destination().display()), errno))?;
        }
        if root.read_only {
            // As its start leaves it: the root read-only, its volumes as
            // they are (mounts of their own).
            let path = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
            remount(&path, MsFlags::MS_RDONLY)
                .map_err(|errno| ("making the container's root read-only".to_owned(), errno))?;
        }
        Ok(root_fd)
    }

    /// A bind mount's source, cloned as a detached mount tree
    /// (`open_tree(OPEN_TREE_CLONE)`), to attach by descriptor.
    pub(super) struct ClonedSource {
        tree: OwnedFd,
        is_dir: bool,
    }

    /// Which of a root's bind sources [`clone_sources`] clones.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Which {
        /// Every one (an attached root's: its staging hides the host's paths).
        Every,
        /// Those Shipyard made (`made_below`): the rest are bound by path.
        Made,
    }

    /// `root`'s bind sources (`which` of them), cloned while the host's
    /// paths are as they are (`None` for a tmpfs, or one not cloned). The
    /// clones are made with this process's credentials, as a bind by path
    /// would be. A made source is reached from its directory without
    /// following a symlink: one put on its path is refused (`ELOOP`).
    fn clone_sources(root: &RootSetup, which: Which) -> Step<Vec<Option<ClonedSource>>> {
        root.mounts
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let Some(source) = entry.source() else {
                    return Ok(None);
                };
                let cloned = match (entry.made_below(), which) {
                    (Some(dir), _) => clone_made(dir, source, entry.recursive),
                    (None, Which::Every) => clone_tree(source, entry.recursive),
                    (None, Which::Made) => return Ok(None),
                };
                cloned
                    .map(Some)
                    .map_err(|errno| Failure::at(ChildStep::Mount, errno).detail(index))
            })
            .collect()
    }

    /// A `source` below `dir` (one Shipyard made, or, in the macOS guest, one
    /// in a share: 13.8c), cloned by descriptor: `dir` opened as it is,
    /// canonical (a link at its end refused), then each component below it
    /// without following a symlink (`shipyard_sys::fs::open_below`), the
    /// last one a directory or a regular file (a link: `ELOOP`; anything
    /// else, `ENOTDIR`).
    fn clone_made(dir: &Path, source: &Path, recursive: bool) -> nix::Result<ClonedSource> {
        let below = source.strip_prefix(dir).map_err(|_| Errno::EINVAL)?;
        let dir = nix::fcntl::open(
            dir,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let below_dirs = |path: &Path| {
            shipyard_sys::fs::open_below(dir.as_fd(), path)
                .map_err(|error| Errno::from_raw(error.raw_os_error().unwrap_or(libc::EINVAL)))
        };
        let made = match (below.parent(), below.file_name()) {
            (Some(parent), Some(name)) => {
                let parent = below_dirs(parent)?;
                nix::fcntl::openat(
                    &parent,
                    name,
                    OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )?
            }
            // The directory itself (a whole share).
            _ => below_dirs(below)?,
        };
        let kind =
            SFlag::from_bits_truncate(nix::sys::stat::fstat(&made)?.st_mode & SFlag::S_IFMT.bits());
        // A directory or a file: never a link, nor a device, a FIFO or a
        // socket someone sharing its directory put in its place.
        if kind == SFlag::S_IFLNK {
            return Err(Errno::ELOOP);
        }
        if kind != SFlag::S_IFDIR && kind != SFlag::S_IFREG {
            return Err(Errno::ENOTDIR);
        }
        let recursive = if recursive { libc::AT_RECURSIVE } else { 0 };
        let tree = open_tree(made.as_raw_fd(), c"", libc::AT_EMPTY_PATH | recursive)?;
        Ok(ClonedSource {
            tree,
            is_dir: kind == SFlag::S_IFDIR,
        })
    }

    /// `open_tree(path, OPEN_TREE_CLONE)`, with what's mounted below it if
    /// `recursive`.
    fn clone_tree(path: &Path, recursive: bool) -> nix::Result<ClonedSource> {
        use std::os::unix::ffi::OsStrExt as _;
        let path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
        let recursive = if recursive { libc::AT_RECURSIVE } else { 0 };
        let tree = open_tree(libc::AT_FDCWD, &path, recursive)?;
        let is_dir =
            SFlag::from_bits_truncate(nix::sys::stat::fstat(&tree)?.st_mode & SFlag::S_IFMT.bits())
                == SFlag::S_IFDIR;
        Ok(ClonedSource { tree, is_dir })
    }

    /// Binds the root filesystem onto itself, recursively: `pivot_root`
    /// needs a mount point (probe 1), and the root filesystem's own
    /// submounts come along. Returns the bind's directory.
    fn bind_root(path: &Path) -> Step<OwnedFd> {
        let fail = |errno| Failure::at(ChildStep::BindRoot, errno);
        mount(
            Some(path),
            path,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(fail)?;
        // Opened after the mount, so it's the bind, not the directory under it.
        nix::fcntl::open(
            path,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(fail)
    }

    /// Mounts an overlay at `merged`: read-only `lower` layers (passed base
    /// first, stacked top-first for the kernel) under a writable `upper`,
    /// with `work` as the kernel's scratch area. `userxattr` is set for a
    /// rootless mount (a user namespace), where overlay keeps its state in
    /// `user.overlay.*`. Runs in the container's mount and user namespaces,
    /// so the kernel permits an unprivileged mount.
    ///
    /// With the new mount API where the kernel has `lowerdir+` (6.8), one
    /// call per layer: up to overlay's own limit (500). Elsewhere, one `mount(2)`,
    /// whose options are at most a page: the paths are made relative to
    /// the directory they share to fit more, and an image with more layers
    /// than fit is refused (`E2BIG`, the detail its layer count).
    ///
    /// Always `index=off` (a container's own upper layer is mounted again
    /// at each start, over layers that may have been extracted again since:
    /// with overlay's index on, a kernel's default on some, that mount would
    /// fail). That turns off the kernel's refusal of an upper directory
    /// already mounted: one mount at a time is the caller's to keep (the
    /// engine's area lock, which a start and a `cp` view both take). And a
    /// root overlay mounted read-only (its kernel's quiet fallback when the
    /// work directory can't be used) is refused (`EROFS`): what the
    /// container writes would be lost.
    fn mount_overlay(
        merged: &Path,
        lower: &[&Path],
        upper: &Path,
        work: &Path,
        userxattr: bool,
    ) -> Step {
        overlay::mount_overlay(merged, lower, upper, work, userxattr).map_err(|errno| {
            let detail = if errno == Errno::E2BIG {
                lower.len()
            } else {
                0
            };
            Failure::at(ChildStep::MountOverlay, errno).detail(detail)
        })?;
        let read_only = nix::sys::statvfs::statvfs(merged).is_ok_and(|stats| {
            stats
                .flags()
                .contains(nix::sys::statvfs::FsFlags::ST_RDONLY)
        });
        if read_only {
            return Err(Failure::at(ChildStep::MountOverlay, Errno::EROFS));
        }
        Ok(())
    }

    /// One of the mounts Shipyard makes in every container, with its own
    /// options: `/proc`, a read-only `/sys`, the container's cgroup at
    /// `/sys/fs/cgroup` (if asked), `/dev` with its nodes and links, and
    /// each mount in `/dev`.
    fn mount_standard(
        root: &OwnedFd,
        setup: &RootSetup,
        standard: StandardMount,
        cgroup: Option<CgroupMount>,
    ) -> Step {
        match standard {
            StandardMount::Proc => {
                let on = target(root, Target::Proc)?;
                mount_on(&on, "proc", "proc", NOSUID_NODEV_NOEXEC, None)
                    .map_err(|errno| Failure::at(ChildStep::MountProc, errno))
            }
            StandardMount::Sys => mount_sys(root, setup),
            StandardMount::Cgroup => match cgroup {
                Some(cgroup) => mount_cgroup(root, cgroup)
                    .map_err(|errno| Failure::at(ChildStep::MountCgroup, errno)),
                None => Ok(()),
            },
            StandardMount::Dev => {
                let on = target(root, Target::Dev)?;
                // Not `nodev`: its device nodes must open (probe 10).
                mount_on(
                    &on,
                    "tmpfs",
                    "tmpfs",
                    MsFlags::MS_NOSUID,
                    Some("mode=755,size=65536k"),
                )
                .map_err(|errno| Failure::at(ChildStep::MountDev, errno))?;
                fill_dev(root, setup)
            }
            StandardMount::Devpts => {
                // Its own instance of devpts (probe 5): the container's
                // terminals only. `ptmxmode` makes its `ptmx` usable (the
                // `/dev/ptmx` link). Its terminals' group, if any, is
                // `pts_gid` (see `RootSetup`).
                let options = match setup.pts_gid {
                    Some(gid) => format!("newinstance,ptmxmode=0666,mode=0620,gid={gid}"),
                    None => "newinstance,ptmxmode=0666,mode=0620".to_owned(),
                };
                mount_in_dev(
                    root,
                    "pts",
                    ("devpts", "devpts"),
                    MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
                    Some(&options),
                )
            }
            // Another container's, with its IPC namespace (Docker's
            // `--ipc container:`): cloned in its mount namespace.
            StandardMount::Shm if let Some(theirs) = setup.shm_mount_ns => {
                let fail = dev_failure("shm");
                let dev = open_directory(root, "dev").map_err(dev_failure("/dev"))?;
                let tree = clone_their_shm(theirs).map_err(fail)?;
                // This container's own rules, whatever the other made of its
                // mount (`suid`, `exec`, shared with its mounts): Docker's.
                confine_shared(&tree).map_err(fail)?;
                mkdirat(&dev, "shm", Mode::from_bits_truncate(0o755)).map_err(fail)?;
                let on = open_directory(&dev, "shm").map_err(fail)?;
                overlay::attach(&tree, on.as_fd()).map_err(fail)
            }
            StandardMount::Shm => mount_in_dev(
                root,
                "shm",
                ("shm", "tmpfs"),
                NOSUID_NODEV_NOEXEC,
                Some("mode=1777,size=65536k"),
            ),
            // POSIX message queues show the IPC namespace of whoever mounts
            // them (probe 6): only with the container's own.
            StandardMount::Mqueue if setup.mqueue => mount_in_dev(
                root,
                "mqueue",
                ("mqueue", "mqueue"),
                NOSUID_NODEV_NOEXEC,
                None,
            ),
            StandardMount::Mqueue => Ok(()),
        }
    }

    /// A read-only `/sys`.
    fn mount_sys(root: &OwnedFd, setup: &RootSetup) -> Step {
        // Read-only: it's the host's kernel and devices. Its network devices
        // are the ones of the network namespace it's mounted in: the
        // container's own, or the host's when the container shares them
        // (probe 7).
        let on = target(root, Target::Sys)?;
        let sys = match setup.sys {
            SysSetup::Own => match mount_on(
                &on,
                "sysfs",
                "sysfs",
                NOSUID_NODEV_NOEXEC | MsFlags::MS_RDONLY,
                None,
            ) {
                // In a user namespace, the kernel mounts a new sysfs only
                // where the host's is fully visible: inside another
                // container, part of it is covered (Docker's masked
                // paths), and it refuses. The host's, bound, as without a
                // network of its own.
                Err(Errno::EPERM) if setup.user_namespace => bind_host_sys(root, &on),
                mounted => mounted,
            },
            SysSetup::HostBind => bind_host_sys(root, &on),
        };
        sys.map_err(|errno| Failure::at(ChildStep::MountSys, errno))
    }

    /// In a user namespace, `sysfs` can only be mounted with a network
    /// namespace of the container's own (step 1's probe, U6): the host's
    /// `/sys` is bound instead, with the mounts below it, and every one made
    /// read-only, `nosuid`, `nodev` and `noexec`. Restrictions only add: the
    /// kernel locks a host mount's flags in a user namespace (U9).
    ///
    /// The mounts below it are the host's (its cgroup tree, `securityfs`,
    /// `debugfs`, …), which a fresh `sysfs` doesn't have. They can't be
    /// unmounted (the kernel locks them with the bind) or left out (it
    /// refuses a bind that would reveal what they cover), so each is
    /// covered with an empty, read-only tmpfs.
    fn bind_host_sys(root: &OwnedFd, on: &OwnedFd) -> nix::Result<()> {
        let flags = NOSUID_NODEV_NOEXEC | MsFlags::MS_RDONLY;
        bind(on, Leaf::Directory, Path::new("/sys"), true)?;
        // Opened again: `on` is the directory under the new mount.
        let mounted = open_directory(root, Target::Sys.name())?;
        add_restrictions(&mounted, true, flags, || {
            on_target(&mounted, Leaf::Directory, |at| remount(at, flags))
        })?;
        let mut below = mounts_below(&mounted)?;
        // Outermost first: covering one hides those inside it.
        below.sort_by_key(|point| point.components().count());
        let mut covered: Vec<PathBuf> = Vec::new();
        for point in below {
            if covered.iter().any(|outer| point.starts_with(outer)) {
                continue;
            }
            mount(
                Some("tmpfs"),
                &point,
                Some("tmpfs"),
                flags,
                Some("size=4k,mode=755"),
            )?;
            covered.push(point);
        }
        Ok(())
    }

    /// Opens `target` in the root filesystem: a directory, never through a
    /// symlink, with nothing mounted on it. Made if it's missing (probe 13).
    /// A symlink there gives ELOOP.
    fn target(root: &OwnedFd, target: Target) -> Step<OwnedFd> {
        let fail = |errno| Failure::at(ChildStep::RootTarget, errno).detail(target as usize);
        let name = target.name();
        let mut fd = match open_directory(root, name) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => {
                // EEXIST: another container on the same root filesystem
                // made it just now.
                match mkdirat(root, name, Mode::from_bits_truncate(0o755)) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(errno) => return Err(fail(errno)),
                }
                open_directory(root, name).map_err(fail)?
            }
            // With O_NOFOLLOW and O_DIRECTORY, a symlink gives ENOTDIR, like
            // a file: tell them apart, for the message.
            Err(errno) => {
                return Err(fail(if is_symlink(root, name) {
                    Errno::ELOOP
                } else {
                    errno
                }));
            }
        };
        // Detach what's mounted there, top first, until it's the root
        // filesystem's own directory: unmounting a directory that isn't a
        // mount's root gives EINVAL. Bounded, in case of a kernel surprise.
        for _ in 0..MAX_STACKED_MOUNTS {
            fchdir(fd.as_fd()).map_err(fail)?;
            match umount2(".", MntFlags::MNT_DETACH) {
                Err(Errno::EINVAL) => return Ok(fd),
                Err(errno) => return Err(fail(errno)),
                Ok(()) => fd = open_directory(root, name).map_err(fail)?,
            }
        }
        Err(fail(Errno::EBUSY))
    }

    /// The container's own cgroup at `<root>/sys/fs/cgroup`: `cgroup2`,
    /// mounted from inside its cgroup namespace, so it shows that cgroup as
    /// the root (the probe's G10). Here, before the pivot, with `/sys` just
    /// mounted: `add_mount` refuses targets in `/sys`. In a user namespace
    /// it goes on top of the empty cover `bind_host_sys` put there.
    fn mount_cgroup(root: &OwnedFd, cgroup: CgroupMount) -> nix::Result<()> {
        let sys = open_directory(root, "sys")?;
        let fs = open_directory(&sys, "fs")?;
        let on = open_directory(&fs, "cgroup")?;
        mount_on(&on, "cgroup2", "cgroup2", cgroup.mount_flags(), None)
    }

    /// Another container's `/dev/shm`, as a detached mount: a mount can be
    /// cloned only by a process in its mount namespace (the kernel's
    /// `check_mnt`; probe `foreign-shm.c`), so a child enters theirs (`ns`,
    /// inherited), opens `/dev/shm` there (its root's: never through a
    /// symlink), checks it's a tmpfs, clones it (`open_tree`) and hands it
    /// back. A detached mount belongs to no namespace: it's attached here.
    /// (This process is single-threaded, as `setns` into a mount namespace
    /// needs: the clean image.)
    fn clone_their_shm(ns: std::os::fd::RawFd) -> nix::Result<OwnedFd> {
        /// How long the helper may take, in seconds.
        const SHM_CLONE_WAIT_SECONDS: i64 = 30;
        use nix::sys::socket::{AddressFamily, MsgFlags, SockFlag, SockType, socketpair};
        use nix::unistd::ForkResult;
        let (ours, theirs) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )?;
        // SAFETY: this process is single-threaded (the clean image): the
        // child may do anything, and only `_exit`s.
        match unsafe { nix::unistd::fork() }? {
            ForkResult::Child => {
                drop(ours);
                let cloned = (|| -> nix::Result<OwnedFd> {
                    // SAFETY: `ns` is a descriptor this process inherited and
                    // holds open.
                    let ns = unsafe { std::os::fd::BorrowedFd::borrow_raw(ns) };
                    nix::sched::setns(ns, nix::sched::CloneFlags::CLONE_NEWNS)?;
                    // Its root and working directory are the namespace's
                    // root now: the other container's.
                    let root = nix::fcntl::open(
                        "/",
                        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )?;
                    let dev = open_directory(&root, "dev")?;
                    let shm = open_directory(&dev, "shm")?;
                    if nix::sys::statfs::fstatfs(&shm)?.filesystem_type()
                        != nix::sys::statfs::TMPFS_MAGIC
                    {
                        return Err(Errno::EXDEV);
                    }
                    clone_of(&shm)
                })();
                let code = match cloned {
                    Ok(tree) => {
                        let sent = shipyard_sys::fdpass::send(
                            theirs.as_fd(),
                            &[tree.as_fd()],
                            &[0],
                            MsgFlags::empty(),
                        );
                        i32::from(sent.is_err())
                    }
                    Err(errno) => {
                        let _ = nix::unistd::write(&theirs, &(errno as i32).to_ne_bytes());
                        1
                    }
                };
                // SAFETY: `_exit` ends the child at once.
                unsafe { libc::_exit(code) }
            }
            ForkResult::Parent { child } => {
                drop(theirs);
                // A helper stuck in the other's mount namespace (a lookup
                // that never ends) never holds this start for good.
                let _ = nix::sys::socket::setsockopt(
                    &ours,
                    nix::sys::socket::sockopt::ReceiveTimeout,
                    &<nix::sys::time::TimeVal as nix::sys::time::TimeValLike>::seconds(
                        SHM_CLONE_WAIT_SECONDS,
                    ),
                );
                let mut bytes = [0_u8; 4];
                let mut room = shipyard_sys::fdpass::Room::new(1);
                let received = shipyard_sys::fdpass::receive(
                    ours.as_fd(),
                    &mut bytes,
                    &mut room,
                    MsgFlags::MSG_CMSG_CLOEXEC,
                );
                if received.is_err() {
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                }
                let _ = nix::sys::wait::waitpid(child, None);
                let received = received?;
                match received.fds.into_iter().next() {
                    Some(tree) => Ok(tree),
                    None if received.bytes == 4 => Err(Errno::from_raw(i32::from_ne_bytes(bytes))),
                    None => Err(Errno::EIO),
                }
            }
        }
    }

    /// Makes the detached mount `tree` (another container's `/dev/shm`)
    /// `nosuid`, `nodev`, `noexec` and private: nothing the other container
    /// set on its own mount (a remount with `suid,exec`, a shared peer
    /// group) comes along. `mount_setattr(2)` (Linux 5.12).
    fn confine_shared(tree: &OwnedFd) -> nix::Result<()> {
        let attr = libc::mount_attr {
            attr_set: libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | libc::MOUNT_ATTR_NOEXEC,
            attr_clr: 0,
            propagation: libc::MS_PRIVATE,
            userns_fd: 0,
        };
        // SAFETY: the path is an empty NUL-terminated string and `attr` a
        // valid `mount_attr` of the size given; the kernel only reads both.
        let result = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                tree.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                &raw const attr,
                std::mem::size_of::<libc::mount_attr>(),
            )
        };
        Errno::result(result).map(drop)
    }

    /// A detached copy of the mount at `/` alone, nothing mounted on it
    /// (`open_tree(OPEN_TREE_CLONE)`, not recursive): a container's own
    /// filesystem, what its volumes cover shown as it is underneath
    /// (`export`, step 12.2b). Needs `CAP_SYS_ADMIN` over the mount
    /// namespace; the kernel refuses (`EINVAL`) a mount whose children are
    /// locked (they'd reveal what they cover to a namespace that may not
    /// see it).
    pub(crate) fn clone_root_alone() -> nix::Result<OwnedFd> {
        open_tree(libc::AT_FDCWD, c"/", 0)
    }

    /// `open_tree(OPEN_TREE_CLONE)` of the mount at `place` (an `O_PATH`
    /// descriptor): a detached copy of it.
    fn clone_of(place: &OwnedFd) -> nix::Result<OwnedFd> {
        open_tree(place.as_raw_fd(), c"", libc::AT_EMPTY_PATH)
    }

    /// `open_tree(at, path, OPEN_TREE_CLONE | flags)`, close-on-exec: a
    /// detached copy of the mount there.
    fn open_tree(
        at: std::os::fd::RawFd,
        path: &std::ffi::CStr,
        flags: libc::c_int,
    ) -> nix::Result<OwnedFd> {
        const OPEN_TREE_CLONE: libc::c_uint = 1;
        let flags = OPEN_TREE_CLONE | (libc::O_CLOEXEC | flags).cast_unsigned();
        // SAFETY: `open_tree` takes a directory descriptor (or `AT_FDCWD`),
        // a NUL-terminated path and flags, and returns a new descriptor or
        // -1.
        let fd = unsafe { libc::syscall(libc::SYS_open_tree, at, path.as_ptr(), flags) };
        let fd = i32::try_from(fd).map_err(|_| Errno::EBADF)?;
        if fd < 0 {
            return Err(Errno::last());
        }
        // SAFETY: a new descriptor the kernel just gave this process.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Opens the directory `name` in `dir` as a place to mount on: the
    /// directory itself, never through a symlink.
    fn open_directory(dir: &OwnedFd, name: &str) -> nix::Result<OwnedFd> {
        openat(
            dir,
            name,
            OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    }

    fn is_symlink(dir: &OwnedFd, name: &str) -> bool {
        fstatat(dir, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFLNK)
    }

    /// Mounts a file system on the directory open at `target`: changes to
    /// it and mounts on `.`, so nothing resolves its path again.
    fn mount_on(
        target: &OwnedFd,
        source: &str,
        fstype: &str,
        flags: MsFlags,
        data: Option<&str>,
    ) -> nix::Result<()> {
        fchdir(target.as_fd())?;
        mount(Some(source), ".", Some(fstype), flags, data)
    }

    /// Device nodes and symlinks, made relative to the new tmpfs (the
    /// mounts in it are steps of their own: `mount_standard`).
    fn fill_dev(root: &OwnedFd, setup: &RootSetup) -> Step {
        // Through the mount just made on it.
        let dev = open_directory(root, "dev").map_err(dev_failure("/dev"))?;
        for (name, major, minor) in DEVICES {
            let made = if setup.user_namespace {
                bind_host_device(&dev, name)
            } else {
                mknodat(
                    &dev,
                    name,
                    SFlag::S_IFCHR,
                    Mode::from_bits_truncate(0o666),
                    makedev(major, minor),
                )
            };
            made.map_err(dev_failure(name))?;
        }
        for (name, target) in LINKS {
            symlinkat(target, &dev, name).map_err(dev_failure(name))?;
        }
        Ok(())
    }

    /// In a user namespace, `mknod` is refused, and a node on a file system
    /// the namespace mounted wouldn't open anyway (U7): the host's
    /// `/dev/NAME` is bound onto an empty file instead, as runc does without
    /// root. The bind keeps the host mount's flags, so it opens.
    fn bind_host_device(dev: &OwnedFd, name: &str) -> nix::Result<()> {
        let file = openat(
            dev,
            name,
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o666),
        )?;
        bind(&file, Leaf::File, &Path::new("/dev").join(name), false)
    }

    /// [`mount_in`] the root's `/dev`, mounted already (the order's checked:
    /// `Root::check_mount_order`).
    fn mount_in_dev(
        root: &OwnedFd,
        name: &str,
        what: (&str, &str),
        flags: MsFlags,
        data: Option<&str>,
    ) -> Step {
        let dev = open_directory(root, "dev").map_err(dev_failure("/dev"))?;
        mount_in(&dev, name, what, flags, data)
    }

    /// Makes the directory `name` in `/dev` (a fresh tmpfs), and mounts
    /// `source` of type `fstype` on it.
    fn mount_in(
        dev: &OwnedFd,
        name: &str,
        (source, fstype): (&str, &str),
        flags: MsFlags,
        data: Option<&str>,
    ) -> Step {
        let fail = dev_failure(name);
        mkdirat(dev, name, Mode::from_bits_truncate(0o755)).map_err(fail)?;
        let on = open_directory(dev, name).map_err(fail)?;
        mount_on(&on, source, fstype, flags, data).map_err(fail)
    }

    /// A failure making `/dev/NAME` (or `/dev` itself), named by its index
    /// in [`DEV_ENTRIES`].
    fn dev_failure(name: &str) -> impl Fn(Errno) -> Failure + Copy {
        let entry = dev_entry(name);
        move |errno| Failure::at(ChildStep::MountDev, errno).detail(entry)
    }

    /// The index of `/dev/NAME` (or of `/dev` itself) in [`DEV_ENTRIES`].
    fn dev_entry(name: &str) -> usize {
        DEV_ENTRIES
            .iter()
            .position(|entry| entry.strip_prefix("/dev/") == Some(name) || *entry == name)
            .unwrap_or(0)
    }

    /// One of the container's own mounts: a bind from the host, or a tmpfs,
    /// at its destination resolved inside the root (made if missing).
    fn add_mount(
        root: &OwnedFd,
        root_place: &Path,
        entry: &MountSetup,
        cloned: Option<&ClonedSource>,
    ) -> Result<(), (ChildStep, Errno)> {
        // A file is bound onto a file, a directory onto a directory.
        let leaf = match (cloned, entry.source()) {
            (Some(cloned), _) if !cloned.is_dir => Leaf::File,
            (None, Some(source)) if !source.is_dir() => Leaf::File,
            _ => Leaf::Directory,
        };
        let resolve = || resolve_in_root(root, entry.destination(), leaf);
        let target = resolve().map_err(|errno| (ChildStep::MountTarget, errno))?;
        // The destination's text was checked (not in /proc or /sys), but a
        // symlink in the root filesystem can still lead there: what counts
        // is where the target is (runc's CVE-2019-19921).
        if on_kernel_interface(&target, entry.destination()) {
            return Err((ChildStep::MountTarget, Errno::EXDEV));
        }
        // Nor, by its place, into the root's `/proc`, `/sys` or `/dev` when
        // it isn't written there: one of those may not be mounted yet (a
        // config can list them after), or be a tmpfs, which the type
        // doesn't tell (F36).
        let target_place = place(&target).map_err(|errno| (ChildStep::MountTarget, errno))?;
        if super::steered(root_place, &target_place, entry.destination()) {
            return Err((ChildStep::MountTarget, Errno::EXDEV));
        }
        let restrictions = entry.restrictions()
            | if entry.read_only {
                MsFlags::MS_RDONLY
            } else {
                MsFlags::empty()
            };
        let Some(source) = entry.source() else {
            // A new tmpfs takes its flags as it's mounted.
            return on_target(&target, leaf, |at| {
                mount(
                    Some("tmpfs"),
                    at,
                    Some("tmpfs"),
                    restrictions,
                    entry.data.as_deref(),
                )
            })
            .map_err(|errno| (ChildStep::Mount, errno));
        };
        match cloned {
            // By descriptor, onto the target's.
            Some(cloned) => overlay::attach(&cloned.tree, target.as_fd()),
            None => bind(&target, leaf, source, entry.recursive),
        }
        .map_err(|errno| (ChildStep::Mount, errno))?;
        if restrictions.is_empty() {
            return Ok(());
        }
        // Resolved again: `target` is the directory under the new mount, and
        // the remount must reach the mount itself.
        let restrict = |errno| (ChildStep::MountRestrict, errno);
        let mounted = resolve().map_err(restrict)?;
        // A bind takes none of the flags given with it.
        add_restrictions(&mounted, entry.recursive, restrictions, || {
            on_target(&mounted, leaf, |at| remount(at, restrictions))
        })
        .map_err(restrict)
    }

    /// Makes the device node `device` in the root filesystem: with `mknod`,
    /// owned as it says; in a user namespace, where `mknod` is refused, the
    /// host's node bound in instead.
    fn make_device(root: &OwnedFd, device: &DeviceSetup, user_namespace: bool) -> nix::Result<()> {
        if user_namespace {
            bind_host_device_node(root, device)
        } else {
            mknod_device(root, device)
        }
    }

    /// The node's file type and device number.
    fn device_identity(device: &DeviceSetup) -> (SFlag, u64) {
        let kind = if device.block {
            SFlag::S_IFBLK
        } else {
            SFlag::S_IFCHR
        };
        (
            kind,
            makedev(u64::from(device.major), u64::from(device.minor)),
        )
    }

    /// Whether `stat` is the device `device` names.
    fn is_device(stat: &nix::sys::stat::FileStat, device: &DeviceSetup) -> bool {
        let (kind, number) = device_identity(device);
        stat.st_mode & SFlag::S_IFMT.bits() == kind.bits() && stat.st_rdev == number
    }

    /// Binds the host's node (`source`, which must be that very device:
    /// ENODEV otherwise) onto an empty file at the node's path. Before the
    /// pivot, the host's tree is still there.
    fn bind_host_device_node(root: &OwnedFd, device: &DeviceSetup) -> nix::Result<()> {
        let source = device.source();
        if !is_device(&nix::sys::stat::stat(source)?, device) {
            return Err(Errno::ENODEV);
        }
        let target = resolve_in_root(root, device.path(), Leaf::File)?;
        if on_kernel_interface(&target, device.path()) {
            return Err(Errno::EXDEV);
        }
        bind(&target, Leaf::File, source, false)
    }

    /// `mknod`s the node in its directory, resolved inside the root, and
    /// gives it its owner. One already there is fine if it's the same device,
    /// and gets the mode and owner asked for.
    fn mknod_device(root: &OwnedFd, device: &DeviceSetup) -> nix::Result<()> {
        let path = device.path();
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(Errno::EINVAL);
        };
        let dir = resolve_in_root(root, parent, Leaf::Directory)?;
        if on_kernel_interface(&dir, parent) {
            return Err(Errno::EXDEV);
        }
        let (kind, number) = device_identity(device);
        let mode = Mode::from_bits_truncate(device.mode);
        match mknodat(&dir, name, kind, mode, number) {
            // The same device there already (a persistent root filesystem's,
            // Shipyard's own `/dev/null`): it gets the mode and owner asked
            // for, as a new one would. (Checked a device, not a link, so the
            // mode can follow it: `fchmodat` takes no "don't follow" before
            // Linux 6.6.)
            Err(Errno::EEXIST) => {
                let there = fstatat(&dir, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)?;
                if !is_device(&there, device) {
                    return Err(Errno::EEXIST);
                }
                nix::sys::stat::fchmodat(
                    &dir,
                    name,
                    mode,
                    nix::sys::stat::FchmodatFlags::FollowSymlink,
                )?;
            }
            made => made?,
        }
        nix::unistd::fchownat(
            &dir,
            name,
            Some(nix::unistd::Uid::from_raw(device.uid)),
            Some(nix::unistd::Gid::from_raw(device.gid)),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
    }

    /// Whether what's open at `target`, which `destination` asked for, is a
    /// kernel interface nothing may be mounted on: in a `proc` or `sysfs`
    /// file system (the container's `/proc` or `/sys`, mounted already); or
    /// in its cgroup, `devpts` or `mqueue` one, unless `destination` names
    /// that place itself (`/dev/mqueue`, asked for, is the caller's
    /// choice). Anywhere else, a symlink in the root filesystem led there
    /// (runc's CVE-2019-19921).
    fn on_kernel_interface(target: &OwnedFd, destination: &Path) -> bool {
        use nix::sys::statfs::{
            CGROUP2_SUPER_MAGIC, DEVPTS_SUPER_MAGIC, PROC_SUPER_MAGIC, SYSFS_MAGIC, fstatfs,
        };
        // Refused if it can't be told: this is a guard.
        let Ok(fs) = fstatfs(target) else {
            return true;
        };
        let kind = fs.filesystem_type();
        if kind == PROC_SUPER_MAGIC || kind == SYSFS_MAGIC {
            return true;
        }
        let place = if kind == CGROUP2_SUPER_MAGIC {
            "/sys/fs/cgroup"
        } else if kind == DEVPTS_SUPER_MAGIC {
            "/dev/pts"
        } else if i128::from(kind.0) == MQUEUE_MAGIC {
            "/dev/mqueue"
        } else {
            return false;
        };
        !destination.starts_with(place)
    }

    /// `mqueue`'s file system magic (`MQUEUE_MAGIC`, `linux/magic.h`), which
    /// neither libc nor nix has. Compared as an `i128`: `statfs`' type is
    /// signed on glibc and unsigned on musl, and both fit.
    const MQUEUE_MAGIC: i128 = 0x1980_0202;

    /// Binds the host's `source` onto the target, with the mounts below it
    /// if `recursive` (`rbind`).
    fn bind(target: &OwnedFd, leaf: Leaf, source: &Path, recursive: bool) -> nix::Result<()> {
        let flags = if recursive {
            MsFlags::MS_BIND | MsFlags::MS_REC
        } else {
            MsFlags::MS_BIND
        };
        on_target(target, leaf, |at| {
            mount(Some(source), at, None::<&str>, flags, None::<&str>)
        })
    }

    /// Adds `restrictions` (read-only, `nosuid`, …) to the mount open at
    /// `mounted`, and with `recursive` to each mount below it as well: they
    /// came along with their own flags (probe 8). Other flags are kept.
    ///
    /// With `mount_setattr` where the kernel has it; otherwise by
    /// remounting: `remount_top` for the mount itself (the callers reach it
    /// differently), then the ones below it by path.
    fn add_restrictions(
        mounted: &OwnedFd,
        recursive: bool,
        restrictions: MsFlags,
        remount_top: impl FnOnce() -> nix::Result<()>,
    ) -> nix::Result<()> {
        match set_attributes(mounted, recursive, restrictions) {
            Err(Errno::ENOSYS) => {}
            done => return done,
        }
        remount_top()?;
        if recursive {
            for below in mounts_below(mounted)? {
                remount(&below, restrictions)?;
            }
        }
        Ok(())
    }

    /// Adds `restrictions` to the mount open at `mounted`, and with
    /// `recursive` to every mount below it (hidden ones too), leaving their
    /// other flags as they are: `mount_setattr(2)` (Linux 5.12) only sets
    /// what it's given, unlike a remount, which replaces the flags (probe
    /// 8b). ENOSYS on older kernels (and under x86-64 emulation).
    fn set_attributes(
        mounted: &OwnedFd,
        recursive: bool,
        restrictions: MsFlags,
    ) -> nix::Result<()> {
        let flags = libc::AT_EMPTY_PATH | if recursive { libc::AT_RECURSIVE } else { 0 };
        let attributes = [
            (MsFlags::MS_RDONLY, libc::MOUNT_ATTR_RDONLY),
            (MsFlags::MS_NOSUID, libc::MOUNT_ATTR_NOSUID),
            (MsFlags::MS_NODEV, libc::MOUNT_ATTR_NODEV),
            (MsFlags::MS_NOEXEC, libc::MOUNT_ATTR_NOEXEC),
        ]
        .into_iter()
        .filter(|(flag, _)| restrictions.contains(*flag))
        .fold(0, |set, (_, attribute)| set | attribute);
        let attr = libc::mount_attr {
            attr_set: attributes,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        // SAFETY: the path is an empty NUL-terminated string and `attr` a
        // valid `mount_attr` of the size given; the kernel only reads both.
        let result = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                mounted.as_raw_fd(),
                c"".as_ptr(),
                flags,
                &raw const attr,
                std::mem::size_of::<libc::mount_attr>(),
            )
        };
        Errno::result(result).map(drop)
    }

    /// Runs `act` on the path that reaches what's open at `target`: `.`
    /// after changing to a directory, `/proc/self/fd/N` for a file (which
    /// can't be changed to; the host's `/proc` is still there before the
    /// pivot).
    fn on_target(
        target: &OwnedFd,
        leaf: Leaf,
        act: impl FnOnce(&Path) -> nix::Result<()>,
    ) -> nix::Result<()> {
        match leaf {
            Leaf::Directory => {
                fchdir(target.as_fd())?;
                act(Path::new("."))
            }
            Leaf::File => act(&fd_path(target)),
        }
    }

    /// Where what's open at `fd` is: the kernel's own path to it, read from
    /// its `/proc/self/fd` link (the host's `/proc`, before the pivot).
    fn place(fd: &OwnedFd) -> nix::Result<PathBuf> {
        nix::fcntl::readlink(&fd_path(fd)).map(PathBuf::from)
    }

    /// `/proc/self/fd/N`: what's open at `fd`, as a path.
    fn fd_path(fd: &OwnedFd) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
    }

    /// The mount points strictly below the mount open at `mounted`, from
    /// `/proc/self/mountinfo` (this process's mount namespace).
    fn mounts_below(mounted: &OwnedFd) -> nix::Result<Vec<PathBuf>> {
        let top = std::fs::read_link(fd_path(mounted)).map_err(|e| io_errno(&e))?;
        let table = std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| io_errno(&e))?;
        Ok(table
            .lines()
            .filter_map(|line| line.split(' ').nth(4))
            .map(unescape_mount_point)
            .filter(|point| point.starts_with(&top) && *point != top)
            .collect())
    }

    /// A mount point as `mountinfo` writes it: space, tab, newline and
    /// backslash as octal escapes (`\040`).
    fn unescape_mount_point(field: &str) -> PathBuf {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while let Some(&byte) = bytes.get(at) {
            let octal = bytes
                .get(at + 1..at + 4)
                .filter(|digits| byte == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)))
                .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok());
            if let Some(decoded) = octal {
                out.push(decoded);
                at += 4;
            } else {
                out.push(byte);
                at += 1;
            }
        }
        PathBuf::from(OsString::from_vec(out))
    }

    /// Makes the root filesystem `/`, and detaches the old root.
    fn pivot(root: &OwnedFd) -> Step {
        fchdir(root.as_fd()).map_err(|errno| Failure::at(ChildStep::PivotRoot, errno))?;
        // The new root and the place for the old one are the same: the old
        // root ends up stacked on top of the new one, at `.` (probe 3), with
        // no directory needed in the root filesystem for it.
        pivot_root(".", ".").map_err(|errno| Failure::at(ChildStep::PivotRoot, errno))?;
        // Lazily: anything still using the old tree (nothing should) keeps it
        // alive, but nothing new can reach it.
        umount2(".", MntFlags::MNT_DETACH)
            .map_err(|errno| Failure::at(ChildStep::DetachOldRoot, errno))?;
        nix::unistd::chdir("/").map_err(|errno| Failure::at(ChildStep::PivotRoot, errno))
    }

    /// Hides `path`: `/dev/null` over a file, an empty read-only tmpfs over
    /// a directory (probe 9). A missing path is left alone.
    fn mask(path: &Path) -> nix::Result<()> {
        let stat = match nix::sys::stat::stat(path) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return Ok(()),
            Err(errno) => return Err(errno),
        };
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            mount(
                Some("tmpfs"),
                path,
                Some("tmpfs"),
                NOSUID_NODEV_NOEXEC | MsFlags::MS_RDONLY,
                Some("size=0"),
            )
        } else {
            mount(
                Some("/dev/null"),
                path,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
        }
    }

    /// Makes `path`, and every mount below it, read-only: a bind onto
    /// itself, so it's a mount of its own, then the restriction. A missing
    /// path is left alone.
    fn make_read_only(path: &Path) -> nix::Result<()> {
        match mount(
            Some(path),
            path,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        ) {
            Err(Errno::ENOENT) => return Ok(()),
            result => result?,
        }
        let mounted = nix::fcntl::open(path, OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty())?;
        add_restrictions(&mounted, true, MsFlags::MS_RDONLY, || {
            remount(path, MsFlags::MS_RDONLY)
        })
    }

    /// Remounts the mount at `path` with `added` (read-only, `nosuid`, …),
    /// keeping its other flags.
    ///
    /// A bind remount *replaces* the mount's flags: given only `MS_RDONLY`,
    /// it silently drops `nosuid`, `nodev` and `noexec` (probe 8b), which
    /// would make "read-only" less safe. So the current ones are read with
    /// `statvfs` and given again: read-only too, or restricting a bind
    /// would make a host mount that's read-only writable. (`relatime` is
    /// the kernel's default; `statvfs` can't show `strictatime`, and nix
    /// can't show `nosymfollow`, which only kernels 5.10 and 5.11 have
    /// without `mount_setattr`.) Submounts stay as they are (probe 8).
    fn remount(path: &Path, added: MsFlags) -> nix::Result<()> {
        let current = statvfs(path)?.flags();
        let kept = [
            (FsFlags::ST_RDONLY, MsFlags::MS_RDONLY),
            (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
            (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
            (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
            (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
            (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        ]
        .into_iter()
        .filter(|(set, _)| current.contains(*set))
        .fold(MsFlags::empty(), |flags, (_, flag)| flags | flag);
        mount(
            None::<&str>,
            path,
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | added | kept,
            None::<&str>,
        )
    }

    #[cfg(test)]
    mod tests {
        use std::path::Path;

        use super::super::DEV_ENTRIES;
        use super::{DEVICES, LINKS, dev_entry, unescape_mount_point};

        /// A failure names `/dev`'s entries by index: each thing `fill_dev`
        /// makes has its own entry in the table, and the table nothing else.
        #[test]
        fn every_dev_entry_is_named() {
            let made: Vec<&str> = std::iter::once("/dev")
                .chain(DEVICES.iter().map(|(name, ..)| *name))
                .chain(LINKS.iter().map(|(name, _)| *name))
                .chain(["pts", "shm", "mqueue"])
                .collect();
            let indexes: Vec<usize> = made.iter().map(|name| dev_entry(name)).collect();
            assert_eq!(
                indexes,
                (0..DEV_ENTRIES.len()).collect::<Vec<_>>(),
                "{made:?}"
            );
        }

        #[test]
        fn mount_points_are_unescaped() {
            assert_eq!(unescape_mount_point("/a\\040b\\011c"), Path::new("/a b\tc"));
            assert_eq!(
                unescape_mount_point("/back\\134slash"),
                Path::new("/back\\slash")
            );
            // Not an escape: left as it is.
            assert_eq!(unescape_mount_point("/x\\9y\\"), Path::new("/x\\9y\\"));
        }
    }
}

#[cfg(test)]
mod place_tests {
    use std::path::Path;

    use super::steered;

    /// Refused where a symlink led a mount into `/proc`, `/sys` or `/dev`;
    /// fine where it's written there, or elsewhere.
    #[test]
    fn a_mount_steered_into_a_standard_directory_is_told() {
        let root = Path::new("/run/shipyard/x/root");
        let at = |inside: &str| root.join(inside);
        for (place, destination, refused) in [
            ("sys/x", "/s/x", true),
            ("sys", "/s", true),
            ("proc/sys", "/data", true),
            ("dev/x", "/d/x", true),
            ("dev/shm", "/dev/shm", false),
            ("dev/x", "/dev/x", false),
            ("data", "/data", false),
            ("etc", "/evil", false),
            // A component, not a prefix of the name.
            ("sysx", "/s", false),
            ("devices", "/d", false),
            ("", "/", false),
            // Nested standard mounts: Shipyard's `/dev/ptmx` leads into
            // `pts`; written there, a mount is the caller's.
            ("dev/pts/ptmx", "/dev/ptmx", true),
            ("dev/shm/x", "/dev/foo", true),
            ("dev/pts/x", "/dev/pts/x", false),
            ("sys/fs/cgroup/x", "/c/x", true),
        ] {
            assert_eq!(
                steered(root, &at(place), Path::new(destination)),
                refused,
                "{destination} at {place}"
            );
        }
        // Outside the root: it can't be told, so it's refused.
        assert!(steered(root, Path::new("/etc"), Path::new("/etc")));
    }
}

/// Mounting an image's overlay, by the new mount API or the old one.
#[cfg(target_os = "linux")]
pub(crate) mod overlay {
    use std::ffi::{CStr, CString, OsString};
    use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Component, Path, PathBuf};

    use nix::errno::Errno;
    use nix::fcntl::OFlag;
    use nix::mount::{MsFlags, mount};

    // From the kernel's `linux/mount.h` (the `libc` crate has the system
    // call numbers, not these).
    const FSOPEN_CLOEXEC: libc::c_uint = 0x1;
    const FSCONFIG_SET_FLAG: libc::c_uint = 0;
    const FSCONFIG_SET_STRING: libc::c_uint = 1;
    const FSCONFIG_CMD_CREATE: libc::c_uint = 6;
    const FSMOUNT_CLOEXEC: libc::c_uint = 0x1;
    const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;
    const MOVE_MOUNT_T_EMPTY_PATH: libc::c_uint = 0x40;
    const MOUNT_ATTR_NODEV: libc::c_uint = 0x4;

    /// The most a `mount(2)` option string can be: a page (4 KiB, or 16 or
    /// 64 KiB on some arm64 kernels), its NUL included.
    fn mount_data_max() -> usize {
        nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
            .ok()
            .flatten()
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(4096)
    }

    /// Why the new mount API didn't mount it.
    pub(crate) enum NewApi {
        /// This kernel can't: no new mount API (Linux 5.2), or no
        /// `lowerdir+` (6.8). The old way it is.
        Unsupported,
        /// It refused (for a reason the old way would share).
        Failed(Errno),
    }

    /// Mounts an overlay at `merged`, as a container's root filesystem is
    /// (see `linux::mount_overlay`): with the new mount API where the kernel
    /// can, else with one `mount(2)`.
    ///
    /// # Errors
    ///
    /// The errno of the mount that failed.
    pub(crate) fn mount_overlay(
        merged: &Path,
        lower: &[&Path],
        upper: &Path,
        work: &Path,
        userxattr: bool,
    ) -> Result<(), Errno> {
        match mount_new(merged, lower, upper, work, userxattr) {
            Ok(()) => Ok(()),
            Err(NewApi::Unsupported) => mount_legacy(merged, lower, upper, work, userxattr),
            Err(NewApi::Failed(errno)) => Err(errno),
        }
    }

    /// Mounts the overlay with the new mount API ([`build`]), then
    /// `move_mount` onto `merged`.
    pub(super) fn mount_new(
        merged: &Path,
        lower: &[&Path],
        upper: &Path,
        work: &Path,
        userxattr: bool,
    ) -> Result<(), NewApi> {
        let mounted = build(lower, upper, work, userxattr)?;
        let target = CString::new(merged.as_os_str().as_bytes())
            .map_err(|_| NewApi::Failed(Errno::EINVAL))?;
        move_mount(&mounted, None, &target, 0).map_err(NewApi::Failed)
    }

    /// The overlay as a detached mount, not attached anywhere yet: `fsopen`,
    /// a `fsconfig` per option (each layer its own `lowerdir+`, top first),
    /// `fsmount`. Overlay reaches its layers with the credentials of whoever
    /// makes it: real root here can make one for a container whose root
    /// can't reach the image store ([`attach`]).
    pub(crate) fn build(
        lower: &[&Path],
        upper: &Path,
        work: &Path,
        userxattr: bool,
    ) -> Result<OwnedFd, NewApi> {
        // SAFETY: `fsopen` takes a NUL-terminated name and flags, and
        // returns a new descriptor or -1.
        let fs = unsafe { libc::syscall(libc::SYS_fsopen, c"overlay".as_ptr(), FSOPEN_CLOEXEC) };
        let fs = match owned(fs) {
            Ok(fs) => fs,
            Err(Errno::ENOSYS) => return Err(NewApi::Unsupported),
            Err(errno) => return Err(NewApi::Failed(errno)),
        };
        // EINVAL anywhere means the old way: a kernel without `lowerdir+`
        // refuses it (6.5 to 6.7), takes it into a legacy option string and
        // refuses that when it's a page (before 6.5), or refuses the result
        // at `FSCONFIG_CMD_CREATE`. A real problem (the upper directory on
        // overlayfs, say) gets the same answer from the old way.
        let unsupported = |errno| {
            if errno == Errno::EINVAL {
                NewApi::Unsupported
            } else {
                NewApi::Failed(errno)
            }
        };
        // Each path by a descriptor, as `/proc/self/fd/N`: `fsconfig` takes
        // at most 256 bytes a value, which a deep store's paths can pass.
        // (Opened here, so they're looked up with this process's
        // credentials, as overlay would.)
        let opened = |path: &Path| {
            nix::fcntl::open(
                path,
                OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(NewApi::Failed)
        };
        let set_dir = |key: &CStr, path: &Path| -> Result<(), NewApi> {
            let dir = opened(path)?;
            let value = format!("/proc/self/fd/{}", dir.as_raw_fd());
            set_string(&fs, key, value.as_bytes()).map_err(unsupported)
        };
        for layer in lower.iter().rev() {
            set_dir(c"lowerdir+", layer)?;
        }
        set_dir(c"upperdir", upper)?;
        set_dir(c"workdir", work)?;
        set_string(&fs, c"index", b"off").map_err(unsupported)?;
        if userxattr {
            fsconfig(&fs, FSCONFIG_SET_FLAG, Some(c"userxattr"), None).map_err(unsupported)?;
        }
        fsconfig(&fs, FSCONFIG_CMD_CREATE, None, None).map_err(unsupported)?;
        // SAFETY: `fsmount` takes the context's descriptor and flags, and
        // returns a new descriptor (the detached mount) or -1.
        // `nodev`: no device node in an image is ever opened through it.
        let mounted = unsafe {
            libc::syscall(
                libc::SYS_fsmount,
                fs.as_raw_fd(),
                FSMOUNT_CLOEXEC,
                MOUNT_ATTR_NODEV,
            )
        };
        owned(mounted).map_err(NewApi::Failed)
    }

    /// Attaches the detached mount `mounted` ([`build`], or a cloned tree)
    /// on what's open as `on` (an `O_PATH` descriptor): by descriptor, so no
    /// path is looked up again.
    ///
    /// # Errors
    ///
    /// The errno of `move_mount`.
    pub(crate) fn attach(mounted: &OwnedFd, on: BorrowedFd<'_>) -> Result<(), Errno> {
        move_mount(mounted, Some(on.as_raw_fd()), c"", MOVE_MOUNT_T_EMPTY_PATH)
    }

    /// `move_mount` of the detached mount `mounted` onto `target`, in the
    /// directory `at` (or the working directory), with `flags` on top of
    /// `MOVE_MOUNT_F_EMPTY_PATH`.
    fn move_mount(
        mounted: &OwnedFd,
        at: Option<std::os::fd::RawFd>,
        target: &CStr,
        flags: libc::c_uint,
    ) -> Result<(), Errno> {
        // SAFETY: `move_mount` takes the detached mount's descriptor with an
        // empty path (`MOVE_MOUNT_F_EMPTY_PATH`), and the target as a
        // directory descriptor and a NUL-terminated path (empty with
        // `MOVE_MOUNT_T_EMPTY_PATH`); it returns 0 or -1.
        let moved = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                mounted.as_raw_fd(),
                c"".as_ptr(),
                at.unwrap_or(libc::AT_FDCWD),
                target.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH | flags,
            )
        };
        if moved < 0 {
            return Err(Errno::last());
        }
        Ok(())
    }

    /// Mounts the overlay with one `mount(2)`: every path relative to the
    /// directory they all share (this process's working directory for the
    /// call, put back after), so as many layers as can fit in a page do.
    ///
    /// # Errors
    ///
    /// `E2BIG` if they don't fit; the errno of a step that failed.
    pub(super) fn mount_legacy(
        merged: &Path,
        lower: &[&Path],
        upper: &Path,
        work: &Path,
        userxattr: bool,
    ) -> Result<(), Errno> {
        let all: Vec<&Path> = lower.iter().copied().chain([upper, work]).collect();
        let base = shared_ancestor(&all);
        let options = legacy_options(&base, lower, upper, work, userxattr)?;
        let back = nix::fcntl::open(
            ".",
            nix::fcntl::OFlag::O_PATH
                | nix::fcntl::OFlag::O_DIRECTORY
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )?;
        nix::unistd::chdir(&base)?;
        let mounted = mount(
            Some("overlay"),
            merged,
            Some("overlay"),
            MsFlags::MS_NODEV,
            Some(options.as_os_str()),
        );
        // The mount's own error first: it's the one that says something.
        let restored = nix::unistd::fchdir(&back);
        mounted?;
        restored
    }

    /// The options of a legacy overlay mount, paths relative to `base`.
    ///
    /// # Errors
    ///
    /// `E2BIG` if they'd be over a page.
    pub(super) fn legacy_options(
        base: &Path,
        lower: &[&Path],
        upper: &Path,
        work: &Path,
        userxattr: bool,
    ) -> Result<OsString, Errno> {
        let relative = |path: &Path| -> OsString {
            match path.strip_prefix(base) {
                Ok(rest) if !rest.as_os_str().is_empty() => rest.as_os_str().to_owned(),
                _ => path.as_os_str().to_owned(),
            }
        };
        let mut options = OsString::from("lowerdir=");
        // Top first: the engine passes base first.
        for (index, dir) in lower.iter().rev().enumerate() {
            if index > 0 {
                options.push(":");
            }
            options.push(relative(dir));
        }
        options.push(",upperdir=");
        options.push(relative(upper));
        options.push(",workdir=");
        options.push(relative(work));
        options.push(",index=off");
        if userxattr {
            options.push(",userxattr");
        }
        if options.len() >= mount_data_max() {
            return Err(Errno::E2BIG);
        }
        Ok(options)
    }

    /// The deepest directory every one of `paths` is in (`/` at least).
    pub(super) fn shared_ancestor(paths: &[&Path]) -> PathBuf {
        let mut shared: Vec<Component<'_>> = match paths.first() {
            Some(first) => first.parent().unwrap_or(first).components().collect(),
            None => return PathBuf::from("/"),
        };
        for path in paths.iter().skip(1) {
            let parts: Vec<Component<'_>> = path.parent().unwrap_or(path).components().collect();
            let same = shared
                .iter()
                .zip(&parts)
                .take_while(|(a, b)| a == b)
                .count();
            shared.truncate(same);
        }
        let ancestor: PathBuf = shared.iter().collect();
        if ancestor.as_os_str().is_empty() {
            PathBuf::from("/")
        } else {
            ancestor
        }
    }

    /// A descriptor a system call returned (or its errno).
    fn owned(raw: libc::c_long) -> Result<OwnedFd, Errno> {
        let fd = i32::try_from(raw).map_err(|_| Errno::EBADF)?;
        if fd < 0 {
            return Err(Errno::last());
        }
        // SAFETY: a new descriptor the kernel just gave this process,
        // owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// `fsconfig(FSCONFIG_SET_STRING, key, value)`.
    fn set_string(fs: &OwnedFd, key: &CStr, value: &[u8]) -> Result<(), Errno> {
        let value = CString::new(value).map_err(|_| Errno::EINVAL)?;
        fsconfig(fs, FSCONFIG_SET_STRING, Some(key), Some(&value))
    }

    /// One `fsconfig` call on the context `fs`.
    fn fsconfig(
        fs: &OwnedFd,
        command: libc::c_uint,
        key: Option<&CStr>,
        value: Option<&CStr>,
    ) -> Result<(), Errno> {
        let pointer =
            |text: Option<&std::ffi::CStr>| text.map_or(std::ptr::null(), std::ffi::CStr::as_ptr);
        // SAFETY: `fsconfig` takes the context's descriptor, a command, and
        // a key and value that are NUL-terminated strings or null (as the
        // command wants), and an auxiliary number (0 here); it returns 0 or
        // -1.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                fs.as_raw_fd(),
                command,
                pointer(key),
                pointer(value),
                0,
            )
        };
        if rc < 0 { Err(Errno::last()) } else { Ok(()) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn legacy_options_are_relative_and_bounded() {
            let layers: Vec<PathBuf> = (0..3)
                .map(|i| PathBuf::from(format!("/store/layers/{i:064x}/diff")))
                .collect();
            let lower: Vec<&Path> = layers.iter().map(PathBuf::as_path).collect();
            let upper = Path::new("/store/containers/run-1/upper");
            let work = Path::new("/store/containers/run-1/work");
            let all: Vec<&Path> = lower.iter().copied().chain([upper, work]).collect();
            let base = shared_ancestor(&all);
            assert_eq!(base, Path::new("/store"));
            let options = legacy_options(&base, &lower, upper, work, true).unwrap();
            let text = options.to_str().unwrap();
            assert!(
                text.starts_with(&format!("lowerdir=layers/{:064x}/diff:", 2)),
                "top first: {text}"
            );
            assert!(text.ends_with(
                ",upperdir=containers/run-1/upper,workdir=containers/run-1/work,index=off,userxattr"
            ));
            // About 55 such layers fit in a page; 100 don't.
            let many: Vec<PathBuf> = (0..100)
                .map(|i| PathBuf::from(format!("/store/layers/{i:064x}/diff")))
                .collect();
            let many: Vec<&Path> = many.iter().map(PathBuf::as_path).collect();
            assert_eq!(
                legacy_options(&base, &many, upper, work, false),
                Err(Errno::E2BIG)
            );
            assert_eq!(
                shared_ancestor(&[Path::new("/a/x"), Path::new("/b/y")]),
                Path::new("/")
            );
        }
    }
}
