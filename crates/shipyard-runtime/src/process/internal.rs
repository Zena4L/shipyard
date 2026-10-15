//! What shipyard's internal processes share: `exec`'s joiner (`exec.rs`)
//! and `cp`'s helper (`helper.rs`). Each is shipyard's binary started again
//! from a sealed copy (`start_sealed`), with a hidden first argument and a
//! sealed setup (`internal_entry`), that joins a container's namespaces
//! (`split_namespaces`, `join`) before it does its one job.

use std::ffi::OsStr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

use nix::errno::Errno;

use super::child::{Failure, SETUP_FAILED};
use super::failure::ChildStep;
use super::setup::is_sealed_setup;
use crate::id_map::IdMaps;
use crate::isolation::Namespace;
use crate::{Error, Result};

/// Namespaces to join before the user namespace, and after it: `(index in
/// Namespace::ALL, descriptor)`.
pub(super) type SplitNamespaces = (Vec<(u64, RawFd)>, Vec<(u64, RawFd)>);

/// `joining`'s namespaces other than the user and PID ones, split: those
/// owned outside the container's user namespace (the host's, or a network
/// namespace given by path), joined first with the caller's own powers; and
/// the rest, joined once inside it (the mount namespace last). As
/// `(index in Namespace::ALL, descriptor)`.
pub(super) fn split_namespaces(joining: &super::join::Joining) -> Result<SplitNamespaces> {
    let index = |kind: Namespace| kind.index();
    // Owned outside the container's user namespace (the host's, or a
    // network namespace given by path): joined with the joiner's own powers,
    // before it enters the user namespace, where it has none over them.
    match &joining.user {
        Some(user) => {
            let own = inode(user.as_fd())?;
            let mut early = Vec::new();
            let mut late = Vec::new();
            for (kind, fd) in &joining.others {
                if owner_inode(fd.as_fd())? == own {
                    late.push((index(*kind), fd.as_raw_fd()));
                } else {
                    early.push((index(*kind), fd.as_raw_fd()));
                }
            }
            Ok((early, late))
        }
        None => Ok((
            Vec::new(),
            joining
                .others
                .iter()
                .map(|(kind, fd)| (index(*kind), fd.as_raw_fd()))
                .collect(),
        )),
    }
}

/// Whether the container's user namespace allows `setgroups` (or, without
/// one, shipyard's own does).
pub(super) fn setgroups_allowed(joining: &super::join::Joining) -> bool {
    match &joining.user {
        Some(_) => {
            read_proc_file(joining, "setgroups").is_some_and(|text| !text.starts_with("deny"))
        }
        None => super::user_namespace::setgroups_allowed(),
    }
}

/// The text of `name` in the joined process's `/proc` entry, if it can be
/// read.
fn read_proc_file(joining: &super::join::Joining, name: &str) -> Option<String> {
    let fd = nix::fcntl::openat(
        &joining.directory,
        name,
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .ok()?;
    std::io::read_to_string(std::fs::File::from(fd)).ok()
}

/// Starts `how`'s process from the sealed copy at `exe`; where this host
/// won't run it (a security module's policy on memfds), from shipyard's
/// binary file `host_exe` if given, with a warning, else `unsealed()`.
/// `what` names it in errors.
pub(super) fn start_sealed(
    exe: &OwnedFd,
    host_exe: Option<&OwnedFd>,
    how: &Internal<'_>,
    what: &str,
    unsealed: impl FnOnce() -> Error,
) -> Result<std::process::Child> {
    let failed =
        |err: &std::io::Error| Error::sys(format!("starting {what}"), super::io_errno(err));
    match start_internal(exe, how) {
        Ok(child) => Ok(child),
        Err(err) if err.raw_os_error() == Some(libc::EACCES) => match host_exe {
            Some(host_exe) => {
                tracing::warn!(
                    "this host refused to run a sealed copy of shipyard's binary for {what}; it \
                     runs the binary's file itself"
                );
                start_internal(host_exe, how).map_err(|err| failed(&err))
            }
            None => Err(unsealed()),
        },
        Err(err) => Err(failed(&err)),
    }
}

/// How to start shipyard's binary as one of its internal processes (exec's
/// joiner, a helper).
pub(super) struct Internal<'a> {
    /// Its name (`argv[0]`).
    pub(super) arg0: &'a str,
    /// The hidden first argument its entry point looks for.
    pub(super) entry: &'a std::ffi::CStr,
    /// Its sealed setup, whose number is its second argument.
    pub(super) setup: RawFd,
    /// The descriptors it inherits by number (their close-on-exec flag is
    /// cleared in the fork).
    pub(super) inherited: &'a [RawFd],
    /// Its standard input, output and error, each inherited if `None`.
    pub(super) stdio: [Option<std::os::fd::BorrowedFd<'a>>; 3],
}

/// Starts `binary` (open, run through `/proc/self/fd`) as `how` says, with
/// an empty environment.
pub(super) fn start_internal(
    binary: &OwnedFd,
    how: &Internal<'_>,
) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;
    let mut command = std::process::Command::new(format!("/proc/self/fd/{}", binary.as_raw_fd()));
    command
        .arg0(how.arg0)
        .arg(OsStr::from_bytes(how.entry.to_bytes()))
        .arg(how.setup.to_string())
        .env_clear();
    let [stdin, stdout, stderr] = how.stdio;
    if let Some(fd) = stdin {
        command.stdin(fd.try_clone_to_owned()?);
    }
    if let Some(fd) = stdout {
        command.stdout(fd.try_clone_to_owned()?);
    }
    if let Some(fd) = stderr {
        command.stderr(fd.try_clone_to_owned()?);
    }
    let inherited = how.inherited.to_vec();
    // SAFETY: the closure only calls `fcntl` (async-signal-safe) on
    // descriptor numbers, from a vector made before the fork.
    unsafe {
        command.pre_exec(move || {
            for &fd in &inherited {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Whether shipyard's binary file, open at `file`, may run in a container in
/// place of its sealed copy (one that can't be made or run): only if nobody
/// in the container could write it, or the container could change the
/// program run next (CVE-2019-5736). `maps` are the container's user
/// namespace's; `None` is none of its own, the host's IDs as they are (its
/// root is host root).
///
/// Capabilities in a user namespace (`CAP_DAC_OVERRIDE`, `CAP_FOWNER`) only
/// reach files whose owner is mapped into it (`user_namespaces(7)`): an owner
/// that isn't anyone inside, a group that may write that isn't either, and
/// no write for others leave the file out of reach. The container's
/// processes also keep this process's supplementary groups where it may not
/// drop them (`setgroups` denied, a single-ID map): a group of ours that may
/// write it puts it in reach too. A file on a read-only mount can't be
/// written through `/proc/PID/exe` at all. (An ACL granting write isn't
/// looked at.)
pub(super) fn fallback_allowed(file: std::os::fd::BorrowedFd<'_>, maps: Option<&IdMaps>) -> bool {
    let read_only = nix::sys::statvfs::fstatvfs(file)
        .is_ok_and(|fs| fs.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY));
    if read_only {
        return true;
    }
    let mut own_groups: Vec<u32> = nix::unistd::getgroups()
        .unwrap_or_default()
        .into_iter()
        .map(nix::unistd::Gid::as_raw)
        .collect();
    own_groups.push(nix::unistd::getegid().as_raw());
    nix::sys::stat::fstat(file).is_ok_and(|stat| {
        unreachable_from(stat.st_uid, stat.st_gid, stat.st_mode, maps, &own_groups)
    })
}

/// [`fallback_allowed`]'s decision, on the file's owner, group and mode, and
/// this process's own groups.
fn unreachable_from(
    uid: u32,
    gid: u32,
    mode: u32,
    maps: Option<&IdMaps>,
    own_groups: &[u32],
) -> bool {
    let Some(maps) = maps else {
        return false;
    };
    let group_writable = mode & libc::S_IWGRP != 0;
    let others_writable = mode & libc::S_IWOTH != 0;
    let group_inside = maps.maps_host_gid(gid) || own_groups.contains(&gid);
    !(others_writable || maps.maps_host_uid(uid) || group_writable && group_inside)
}

/// The ID maps of the user namespace being joined, from its process's
/// `/proc` entry: `None` without one of its own (the host's), if they can't
/// be read, or if that namespace isn't a child of shipyard's own (one the
/// container made inside its own since: its maps are in its parent's terms,
/// and every namespace between could reach the file too). Either way,
/// nothing is out of the container's reach.
pub(super) fn maps_of(joining: &super::join::Joining) -> Option<IdMaps> {
    use std::os::unix::fs::MetadataExt as _;
    let user = joining.user.as_ref()?;
    let own = std::fs::metadata("/proc/self/ns/user").ok()?.ino();
    if parent_inode(user.as_fd()).ok()? != own {
        return None;
    }
    IdMaps::from_proc(
        &read_proc_file(joining, "uid_map")?,
        &read_proc_file(joining, "gid_map")?,
    )
}

/// Refuses hosts where a credential change makes a process dumpable
/// (`fs.suid_dumpable` 1): the command would be exposed for a moment
/// between its credential calls and making itself non-dumpable again.
pub(crate) fn refuse_unsafe_dumpable(what: &'static str) -> Result<()> {
    match std::fs::read("/proc/sys/fs/suid_dumpable") {
        Ok(value) => dumpable_refusal(&value, what),
        Err(_) => Ok(()),
    }
}

/// [`refuse_unsafe_dumpable`]'s decision on the setting's text: only 1
/// (`SUID_DUMP_USER`) leaves the process dumpable after a credential change;
/// 0 and 2 (dumpable by root alone) don't.
fn dumpable_refusal(value: &[u8], what: &'static str) -> Result<()> {
    if value.trim_ascii() == b"1" {
        Err(Error::UnsafeDumpable { what })
    } else {
        Ok(())
    }
}

/// The inode of the namespace open at `fd`.
pub(super) fn inode(fd: std::os::fd::BorrowedFd<'_>) -> Result<u64> {
    nix::sys::stat::fstat(fd)
        .map(|stat| stat.st_ino)
        .map_err(|errno| Error::sys("reading a namespace of the container", errno))
}

/// The inode of the user namespace that owns the namespace open at `fd`
/// (`NS_GET_USERNS`, Linux 4.9).
fn owner_inode(fd: std::os::fd::BorrowedFd<'_>) -> Result<u64> {
    /// `NS_GET_USERNS`: `_IO(0xb7, 0x1)`.
    const NS_GET_USERNS: libc::c_ulong = 0xb701;
    related_inode(
        fd,
        NS_GET_USERNS,
        "finding who owns a namespace of the container",
    )
}

/// The inode of the user namespace that is the parent of the user
/// namespace open at `fd` (`NS_GET_PARENT`, Linux 4.9).
fn parent_inode(fd: std::os::fd::BorrowedFd<'_>) -> Result<u64> {
    /// `NS_GET_PARENT`: `_IO(0xb7, 0x2)`.
    const NS_GET_PARENT: libc::c_ulong = 0xb702;
    related_inode(
        fd,
        NS_GET_PARENT,
        "finding the parent of the container's user namespace",
    )
}

/// The inode of the namespace `request` (an `NS_GET_*` ioctl, which takes
/// no argument and returns a new descriptor) finds from the one at `fd`.
fn related_inode(
    fd: std::os::fd::BorrowedFd<'_>,
    request: libc::c_ulong,
    doing: &'static str,
) -> Result<u64> {
    #[allow(
        clippy::useless_conversion,
        reason = "the request is `ioctl`'s own type, which differs between C libraries"
    )]
    // SAFETY: an `NS_GET_*` request takes no argument and returns a new
    // descriptor, which is ours.
    let related = unsafe { libc::ioctl(fd.as_raw_fd(), request.try_into().unwrap_or(0)) };
    if related < 0 {
        return Err(Error::sys(doing, Errno::last()));
    }
    // SAFETY: just returned by the kernel, ours alone.
    let related = unsafe { OwnedFd::from_raw_fd(related) };
    inode(related.as_fd())
}

/// Whether this process was started as one of shipyard's internal
/// processes (its hidden first argument `entry`): if so, its sealed setup's
/// descriptor; `None` if not. A privileged start (setuid, file
/// capabilities), or one without a sealed setup, gets no further: it exits.
/// `label` names the process in what it says.
pub(super) fn internal_entry(entry: &std::ffi::CStr, label: &str) -> Option<RawFd> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::from_bytes(entry.to_bytes())) {
        return None;
    }
    // As for the init: a privileged start gets no further.
    if let Err(err) = super::refuse_privileged_start() {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("shipyard-{label}: {err}\n").as_bytes(),
        );
        std::process::exit(SETUP_FAILED);
    }
    if let Some(fd) = args
        .next()
        .and_then(|arg| arg.to_str()?.parse::<RawFd>().ok())
        && is_sealed_setup(fd)
    {
        return Some(fd);
    }
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        format!("shipyard: this is {label}'s internal entry point\n").as_bytes(),
    );
    std::process::exit(SETUP_FAILED);
}

/// Makes descriptors this process holds close-on-exec (again: an internal
/// process's, inherited by number across its own exec; or what a monitor
/// keeps from its children). A failure (one already closed) is ignored.
pub(crate) fn close_on_exec(fds: impl IntoIterator<Item = RawFd>) {
    for fd in fds {
        let _ = nix::fcntl::fcntl(
            // SAFETY: inherited across the exec; borrowed only for this call.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
    }
}

/// The namespaces in a setup's list: `(index in Namespace::ALL,
/// descriptor)`.
pub(super) fn decode_namespaces(list: &[(u64, RawFd)]) -> Vec<(Namespace, RawFd)> {
    list.iter()
        .filter_map(|&(index, fd)| {
            let kind = *Namespace::ALL.get(usize::try_from(index).ok()?)?;
            Some((kind, fd))
        })
        .collect()
}

/// `setns` into the namespace open at `fd`.
pub(super) fn join(fd: RawFd, kind: Namespace) -> std::result::Result<(), Failure> {
    // SAFETY: a plain system call on an inherited descriptor.
    if unsafe { libc::setns(fd, kind.clone_flag()) } == -1 {
        let index = usize::try_from(kind.index()).unwrap_or(usize::MAX);
        return Err(Failure::at(ChildStep::JoinNamespace, Errno::last()).detail(index));
    }
    Ok(())
}

/// One above the highest descriptor open in this (single-threaded)
/// process: the command's closing sweep goes that far without `close_range`.
/// Where `/proc` can't tell, the most this process may have open.
pub(super) fn fd_bound() -> RawFd {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .and_then(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<RawFd>().ok())
                .max()
        })
        .map_or_else(
            || {
                nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
                    .ok()
                    .and_then(|(_, hard)| RawFd::try_from(hard).ok())
                    .unwrap_or(RawFd::MAX)
            },
            |highest| highest.saturating_add(1),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dumpable_setting_of_1_is_refused() {
        assert!(matches!(
            dumpable_refusal(b"1\n", "exec"),
            Err(Error::UnsafeDumpable { what: "exec" })
        ));
        for safe in [&b"0\n"[..], b"2\n", b"2", b"", b"11\n"] {
            assert!(dumpable_refusal(safe, "exec").is_ok(), "{safe:?}");
        }
    }

    /// The binary's file may run in a container only where nobody inside
    /// could write it: its owner not mapped in, nor a group that may write,
    /// and no write for others. Without a user namespace, never.
    #[test]
    fn the_binarys_file_runs_only_out_of_the_containers_reach() {
        let rootless = IdMaps::from_proc("0 1000 1\n1 100000 65536\n", "0 1000 1\n").unwrap();
        let root_owned = (0, 0, 0o755);
        // A rootless user's: installed in their home, theirs.
        assert!(!unreachable_from(1000, 1000, 0o755, Some(&rootless), &[]));
        // Root's (/usr/local/bin): out of reach.
        assert!(unreachable_from(
            root_owned.0,
            root_owned.1,
            root_owned.2,
            Some(&rootless),
            &[]
        ));
        // Root's, but anyone may write it.
        assert!(!unreachable_from(0, 0, 0o757, Some(&rootless), &[]));
        // Root's, its group (mapped inside) may write it.
        assert!(!unreachable_from(0, 1000, 0o775, Some(&rootless), &[]));
        // ... a group not mapped inside may: still out of reach.
        assert!(unreachable_from(0, 50, 0o775, Some(&rootless), &[]));
        // ... unless it's one of ours, which the container's processes keep
        // where they can't drop them (a single-ID map).
        assert!(!unreachable_from(0, 50, 0o775, Some(&rootless), &[50]));
        assert!(unreachable_from(0, 50, 0o755, Some(&rootless), &[50]));
        // A root container without a user namespace: its root is host root.
        assert!(!unreachable_from(0, 0, 0o755, None, &[]));
        assert!(!unreachable_from(1000, 1000, 0o755, None, &[]));
    }
}
