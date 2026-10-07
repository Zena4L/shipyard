//! A root filesystem of the container's own (Linux; `pivot_root(2)`,
//! `mount_namespaces(7)`). Run by the container's clean image (`init.rs`),
//! in its new mount namespace, after `/` was made a slave.
//!
//! **Mounts first, then the pivot** (runc's order). `/proc`, `/sys` and
//! `/dev` are mounted into the root filesystem while the host's tree is
//! still there: under a user namespace (stage 4), the kernel only lets a
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
//! **Then the container's own mounts** (`Root::mounts`: binds from the
//! host and tmpfs), each at a destination resolved inside the root
//! (`resolve.rs`), never by a path that could lead out of it.
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

#[cfg(target_os = "linux")]
pub(crate) use linux::enter;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    use nix::errno::Errno;
    use nix::fcntl::{OFlag, openat};
    use nix::mount::{MntFlags, MsFlags, mount, umount2};
    use nix::sys::stat::{Mode, SFlag, fstatat, makedev, mkdirat, mknodat, umask};
    use nix::sys::statvfs::{FsFlags, statvfs};
    use nix::unistd::{fchdir, pivot_root, symlinkat};

    use super::{DEV_ENTRIES, Target};
    use crate::isolation::CgroupMount;
    use crate::process::child::Failure;
    use crate::process::failure::ChildStep;
    use crate::process::io_errno;
    use crate::process::resolve::{Leaf, resolve_in_root};
    use crate::process::setup::{DeviceSetup, MountSetup, RootSetup};
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
        sysctls: &[(String, String)],
        cgroup: Option<CgroupMount>,
        before_pivot: impl FnOnce() -> Step<T>,
    ) -> Step<T> {
        // Device nodes and directories get exactly the modes given here.
        // Whatever runs in between (hooks), and after, gets the umask back.
        let umask_before = umask(Mode::empty());
        let root_fd = bind_root(root.path())?;
        mount_standard(&root_fd, root, cgroup)?;
        if !sysctls.is_empty() {
            // Through the `/proc` just mounted in the root filesystem.
            let proc = open_directory(&root_fd, Target::Proc.name())
                .map_err(|errno| Failure::at(ChildStep::Sysctl, errno))?;
            sysctl::apply(&proc, sysctls)?;
        }
        for (index, mount) in root.mounts.iter().enumerate() {
            add_mount(&root_fd, mount)
                .map_err(|(step, errno)| Failure::at(step, errno).detail(index))?;
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
        if root.read_only {
            remount(Path::new("/"), MsFlags::MS_RDONLY)
                .map_err(|errno| Failure::at(ChildStep::ReadOnlyRoot, errno))?;
        }
        umask(umask_before);
        Ok(before)
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

    /// `/proc`, a read-only `/sys` (with the container's cgroup at
    /// `/sys/fs/cgroup`, if asked), and `/dev` with what's in it.
    fn mount_standard(root: &OwnedFd, setup: &RootSetup, cgroup: Option<CgroupMount>) -> Step {
        let on = target(root, Target::Proc)?;
        mount_on(&on, "proc", "proc", NOSUID_NODEV_NOEXEC, None)
            .map_err(|errno| Failure::at(ChildStep::MountProc, errno))?;
        // Read-only: it's the host's kernel and devices. Until the container
        // has its own network namespace (stage 7), it lists the host's
        // network devices (probe 7).
        let on = target(root, Target::Sys)?;
        let sys = if setup.user_namespace {
            bind_host_sys(root, &on)
        } else {
            mount_on(
                &on,
                "sysfs",
                "sysfs",
                NOSUID_NODEV_NOEXEC | MsFlags::MS_RDONLY,
                None,
            )
        };
        sys.map_err(|errno| Failure::at(ChildStep::MountSys, errno))?;
        if let Some(cgroup) = cgroup {
            mount_cgroup(root, cgroup)
                .map_err(|errno| Failure::at(ChildStep::MountCgroup, errno))?;
        }
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

    /// Device nodes, symlinks, `/dev/pts`, `/dev/shm` and `/dev/mqueue`,
    /// made relative to the new tmpfs.
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
        // Its own instance of devpts (probe 5): the container's terminals
        // only. `ptmxmode` makes its `ptmx` usable (the `/dev/ptmx` link).
        // Its terminals' group, if any, is `pts_gid` (see `RootSetup`).
        let pts_options = match setup.pts_gid {
            Some(gid) => format!("newinstance,ptmxmode=0666,mode=0620,gid={gid}"),
            None => "newinstance,ptmxmode=0666,mode=0620".to_owned(),
        };
        mount_in(
            &dev,
            "pts",
            ("devpts", "devpts"),
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            Some(&pts_options),
        )?;
        mount_in(
            &dev,
            "shm",
            ("shm", "tmpfs"),
            NOSUID_NODEV_NOEXEC,
            Some("mode=1777,size=65536k"),
        )?;
        // POSIX message queues show the IPC namespace of whoever mounts them
        // (probe 6): only with the container's own.
        if setup.mqueue {
            mount_in(
                &dev,
                "mqueue",
                ("mqueue", "mqueue"),
                NOSUID_NODEV_NOEXEC,
                None,
            )?;
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
    fn add_mount(root: &OwnedFd, entry: &MountSetup) -> Result<(), (ChildStep, Errno)> {
        // A file is bound onto a file, a directory onto a directory.
        let leaf = match entry.source() {
            Some(source) if !source.is_dir() => Leaf::File,
            _ => Leaf::Directory,
        };
        let resolve = || resolve_in_root(root, entry.destination(), leaf);
        let target = resolve().map_err(|errno| (ChildStep::MountTarget, errno))?;
        // The destination's text was checked (not in /proc or /sys), but a
        // symlink in the root filesystem can still lead there: what counts
        // is where the target is (runc's CVE-2019-19921).
        if on_kernel_interface(&target) {
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
        bind(&target, leaf, source, entry.recursive).map_err(|errno| (ChildStep::Mount, errno))?;
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
        if on_kernel_interface(&target) {
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
        if on_kernel_interface(&dir) {
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

    /// Whether what's open at `target` is in a `proc` or `sysfs` file
    /// system: the container's `/proc` or `/sys`, mounted already.
    fn on_kernel_interface(target: &OwnedFd) -> bool {
        use nix::sys::statfs::{PROC_SUPER_MAGIC, SYSFS_MAGIC, fstatfs};
        fstatfs(target).is_ok_and(|fs| {
            let kind = fs.filesystem_type();
            kind == PROC_SUPER_MAGIC || kind == SYSFS_MAGIC
        })
    }

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
