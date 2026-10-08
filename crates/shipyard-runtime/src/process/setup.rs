//! What the container's clean image needs to set the container up and start
//! the command (Linux; see `init.rs`).
//!
//! The cloned child re-execs the shipyard binary at once (runc-style), so
//! everything else happens in that fresh process, in ordinary code. It gets
//! its instructions as JSON in a **sealed memfd**: an anonymous in-memory
//! file, written and sealed read-only before the clone. A pipe written before
//! the clone would block past its 64 KiB buffer (an environment can be
//! bigger), and nothing could read it yet.
//!
//! Arguments, environment and paths travel as bytes, not strings: on Unix
//! none of them has to be UTF-8. The spec is validated again on arrival
//! (`ChildPlan::new` validates everything).

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::sys::memfd::{MFdFlags, memfd_create};
use serde::{Deserialize, Serialize};

use super::env::Env;
use super::io_errno;
use super::rlimit::{Rlimit, RlimitValue};
use super::spec::{Names, ProcessSpec, User};
use nix::mount::MsFlags;

use crate::hooks::Hook;
use crate::isolation::{
    CgroupMount, IdMaps, Isolation, Mount, MountFlag, MountKind, Namespace, Root,
};
use crate::{Error, Result};

/// The container's setup, as the clean image receives it.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Setup {
    args: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    cwd: Vec<u8>,
    uid: u32,
    gid: u32,
    umask: Option<u32>,
    additional_gids: Vec<u32>,
    names: NamesSetup,
    /// `(name, soft, hard)`; `None` is unlimited.
    rlimits: Vec<(String, Option<u64>, Option<u64>)>,
    oom_score_adj: Option<i32>,
    /// Signals the command keeps ignoring (see `SpawnOptions::keep_ignored`).
    pub(crate) keep_ignored: Vec<libc::c_int>,
    /// Change `/`'s propagation first: a new mount namespace.
    pub(crate) private_mounts: bool,
    /// Mount a fresh `/proc`: new PID and mount namespaces.
    pub(crate) mount_proc: bool,
    pub(crate) hostname: Option<String>,
    pub(crate) domainname: Option<String>,
    /// `(key, value)`, in order (see `sysctl.rs`).
    pub(crate) sysctls: Vec<(String, String)>,
    /// Be the container's init (a new PID namespace), and start the command
    /// as PID 2. Without one, the clean image becomes the command.
    pub(crate) init: bool,
    /// Shipyard's PID, the command's parent when there's no init.
    pub(crate) parent: Option<libc::pid_t>,
    /// A root filesystem of the container's own (see `rootfs.rs`).
    pub(crate) root: Option<RootSetup>,
    /// The start gate's descriptor (`SpawnOptions::gate`), inherited: wait
    /// for it to be opened for reading before starting the command.
    pub(crate) gate: Option<RawFd>,
    /// The command's capabilities and `no_new_privs` ([`Isolation::security`]).
    pub(crate) security: crate::security::Security,
    /// The user namespace, if there's one.
    pub(crate) user_namespace: Option<UserNamespaceSetup>,
    /// Its cgroup namespace and mount.
    pub(crate) cgroup: CgroupSetup,
    /// Hooks to stop for while setting up, if there are any.
    pub(crate) hooks: Option<HooksSetup>,
}

/// The hooks part of the setup: where to wait for the runtime's, and the
/// clean image's own.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HooksSetup {
    /// The hooks pipe's read end (see `channel.rs`), inherited.
    pub(crate) pipe: RawFd,
    /// Run before `pivot_root`, once the runtime's are done.
    pub(crate) create_container: Vec<Hook>,
    /// Run before the command, after the start gate.
    pub(crate) start_container: Vec<Hook>,
}

/// The container's view of its cgroup, as the clean image makes it.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CgroupSetup {
    /// Make a cgroup namespace (the clean image is in its cgroup by then).
    pub(crate) namespace: bool,
    /// Mount the container's cgroup at `/sys/fs/cgroup`: only when it has a
    /// cgroup of its own, and a mount namespace to mount it in.
    pub(crate) mount: Option<CgroupMount>,
}

/// The container's user namespace, as the clean image needs it.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UserNamespaceSetup {
    /// Its ID maps ([`Isolation::id_maps`]): the command's IDs must be in
    /// them.
    pub(crate) maps: IdMaps,
}

/// The root filesystem part of the setup ([`Root`], as bytes).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RootSetup {
    path: Vec<u8>,
    pub(crate) read_only: bool,
    masked: Vec<Vec<u8>>,
    readonly: Vec<Vec<u8>>,
    /// Mount `/dev/mqueue`: a new IPC namespace, whose queues it shows.
    pub(crate) mqueue: bool,
    /// In a user namespace: `/dev`'s nodes and `/sys` are binds of the
    /// host's (`mknod` and a new `sysfs` are refused there).
    pub(crate) user_namespace: bool,
    /// The group `/dev/pts`'s terminals belong to: `tty` (5), unless a user
    /// namespace leaves it unmapped (the mount would be refused).
    pub(crate) pts_gid: Option<u32>,
    /// More mounts, in order ([`Root::mounts`]).
    pub(crate) mounts: Vec<MountSetup>,
    /// Device nodes to make, in order (`Isolation::devices`).
    pub(crate) devices: Vec<DeviceSetup>,
    /// If set, mount this overlay at `path` before binding (an image root).
    pub(crate) overlay: Option<OverlaySetup>,
    /// An overlay real root made already (`--uidmap`: see `spawn.rs`), to
    /// attach instead: the detached mount's descriptor, inherited.
    attach: Option<RawFd>,
}

/// The overlay layers to mount as the root, serialized for the init.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OverlaySetup {
    /// Read-only layer directories, base first (as bytes, like other paths).
    pub(crate) lower: Vec<Vec<u8>>,
    /// The writable upper directory.
    pub(crate) upper: Vec<u8>,
    /// The overlay work directory.
    pub(crate) work: Vec<u8>,
    /// Whether to mount with `userxattr` (rootless, in a user namespace).
    pub(crate) userxattr: bool,
}

/// A device node to make ([`DeviceNode`](crate::devices::DeviceNode)), its
/// path as bytes.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DeviceSetup {
    path: Vec<u8>,
    /// Where it is on the host, for a bind; `None`: at `path`.
    source: Option<Vec<u8>>,
    pub(crate) block: bool,
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl DeviceSetup {
    fn new(node: &crate::devices::DeviceNode) -> Self {
        Self {
            path: node.path.as_os_str().as_bytes().to_vec(),
            source: node
                .source
                .as_ref()
                .map(|source| source.as_os_str().as_bytes().to_vec()),
            block: node.kind == crate::devices::DeviceKind::Block,
            major: node.major,
            minor: node.minor,
            mode: node.mode,
            uid: node.uid,
            gid: node.gid,
        }
    }

    /// Where, inside the container.
    pub(crate) fn path(&self) -> &Path {
        as_path(&self.path)
    }

    /// Where it is on the host: what a user namespace binds in.
    pub(crate) fn source(&self) -> &Path {
        as_path(self.source.as_deref().unwrap_or(&self.path))
    }
}

/// One of [`Root::mounts`], as bytes.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MountSetup {
    destination: Vec<u8>,
    /// The host path of a bind; `None` for a tmpfs.
    source: Option<Vec<u8>>,
    /// A bind with the mounts below its source.
    pub(crate) recursive: bool,
    pub(crate) read_only: bool,
    /// `nosuid`, `nodev` and `noexec`, as `MsFlags` bits.
    restrictions: u64,
    pub(crate) data: Option<String>,
}

impl MountSetup {
    fn new(mount: &Mount) -> Self {
        let (source, recursive) = match &mount.kind {
            MountKind::Bind { source, recursive } => {
                (Some(source.as_os_str().as_bytes().to_vec()), *recursive)
            }
            MountKind::Tmpfs => (None, false),
        };
        let restrictions = [
            (MountFlag::NoSuid, MsFlags::MS_NOSUID),
            (MountFlag::NoDev, MsFlags::MS_NODEV),
            (MountFlag::NoExec, MsFlags::MS_NOEXEC),
        ]
        .into_iter()
        .filter(|(flag, _)| mount.has(*flag))
        .fold(MsFlags::empty(), |all, (_, bit)| all | bit);
        Self {
            destination: mount.destination.as_os_str().as_bytes().to_vec(),
            source,
            recursive,
            read_only: mount.has(MountFlag::ReadOnly),
            restrictions: restrictions.bits(),
            data: mount.data.clone(),
        }
    }

    /// Where it appears, inside the container.
    pub(crate) fn destination(&self) -> &Path {
        as_path(&self.destination)
    }

    /// The host path bound, if it's a bind.
    pub(crate) fn source(&self) -> Option<&Path> {
        self.source.as_deref().map(as_path)
    }

    /// `nosuid`, `nodev` and `noexec`, as asked.
    pub(crate) fn restrictions(&self) -> MsFlags {
        MsFlags::from_bits_truncate(self.restrictions)
    }
}

impl RootSetup {
    fn new(
        root: &Root,
        mqueue: bool,
        id_maps: Option<&IdMaps>,
        devices: &[crate::devices::DeviceNode],
        attach: Option<RawFd>,
    ) -> Self {
        /// The `tty` group in most distributions.
        const TTY: u32 = 5;
        let bytes = |path: &PathBuf| path.as_os_str().as_bytes().to_vec();
        Self {
            path: bytes(&root.path),
            read_only: root.read_only,
            masked: root.masked_paths.iter().map(bytes).collect(),
            readonly: root.readonly_paths.iter().map(bytes).collect(),
            mqueue,
            user_namespace: id_maps.is_some(),
            pts_gid: match id_maps {
                Some(maps) => maps.host_gid(TTY).map(|_| TTY),
                None => Some(TTY),
            },
            mounts: root.mounts.iter().map(MountSetup::new).collect(),
            devices: devices.iter().map(DeviceSetup::new).collect(),
            attach,
            overlay: root
                .overlay
                .as_ref()
                .filter(|_| attach.is_none())
                .map(|overlay| OverlaySetup {
                    lower: overlay.lower.iter().map(bytes).collect(),
                    upper: bytes(&overlay.upper),
                    work: bytes(&overlay.work),
                    // Rootless (a user namespace) needs userxattr; real root
                    // mounts without it.
                    userxattr: id_maps.is_some(),
                }),
        }
    }

    /// The directory on the host.
    pub(crate) fn path(&self) -> &Path {
        as_path(&self.path)
    }

    /// The overlay real root made, to attach (the mount's descriptor), if it
    /// did.
    pub(crate) fn attach(&self) -> Option<RawFd> {
        self.attach
    }

    /// The overlay to mount at [`path`](Self::path) before binding, if the
    /// root is an image: the lower layers (base first), upper, work, and
    /// whether to use `userxattr` (rootless).
    pub(crate) fn overlay(&self) -> Option<(Vec<&Path>, &Path, &Path, bool)> {
        self.overlay.as_ref().map(|overlay| {
            (
                overlay.lower.iter().map(|p| as_path(p)).collect(),
                as_path(&overlay.upper),
                as_path(&overlay.work),
                overlay.userxattr,
            )
        })
    }

    /// The paths to mask, inside the container.
    pub(crate) fn masked(&self) -> impl Iterator<Item = &Path> {
        self.masked.iter().map(|path| as_path(path))
    }

    /// The paths to make read-only, inside the container.
    pub(crate) fn readonly(&self) -> impl Iterator<Item = &Path> {
        self.readonly.iter().map(|path| as_path(path))
    }
}

/// [`Names`], to be resolved in the container.
#[derive(Debug, Serialize, Deserialize)]
struct NamesSetup {
    user: Option<String>,
    group: Option<String>,
    additional_groups: Vec<String>,
    memberships: bool,
}

impl From<&Names> for NamesSetup {
    fn from(names: &Names) -> Self {
        Self {
            user: names.user.clone(),
            group: names.group.clone(),
            additional_groups: names.additional_groups.clone(),
            memberships: names.memberships,
        }
    }
}

impl From<&NamesSetup> for Names {
    fn from(names: &NamesSetup) -> Self {
        Self {
            user: names.user.clone(),
            group: names.group.clone(),
            additional_groups: names.additional_groups.clone(),
            memberships: names.memberships,
        }
    }
}

fn as_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

impl Setup {
    /// The command's working directory (inside the container's root, if
    /// it has one).
    pub(crate) fn cwd(&self) -> &Path {
        as_path(&self.cwd)
    }

    /// The setup for running `spec` isolated as `isolation` says, in a
    /// cgroup of its own if `own_cgroup`, stopping for hooks on `hooks_pipe`
    /// if it's given.
    pub(crate) fn new(
        spec: &ProcessSpec,
        keep_ignored: &[libc::c_int],
        isolation: &Isolation,
        gate: Option<RawFd>,
        own_cgroup: bool,
        hooks_pipe: Option<RawFd>,
        attach: Option<RawFd>,
    ) -> Self {
        let bytes = |s: &OsStr| s.as_bytes().to_vec();
        Self {
            args: spec.args.iter().map(|arg| bytes(arg)).collect(),
            env: spec.env.iter().map(|(k, v)| (bytes(k), bytes(v))).collect(),
            cwd: bytes(spec.cwd.as_os_str()),
            uid: spec.user.uid,
            gid: spec.user.gid,
            umask: spec.user.umask,
            additional_gids: spec.user.additional_gids.clone(),
            names: NamesSetup::from(&spec.user.names),
            oom_score_adj: spec.oom_score_adj,
            rlimits: spec
                .rlimits
                .iter()
                .map(|limit| {
                    (
                        limit.kind.name().to_owned(),
                        finite(limit.soft),
                        finite(limit.hard),
                    )
                })
                .collect(),
            keep_ignored: keep_ignored.to_vec(),
            private_mounts: isolation.has(Namespace::Mount),
            mount_proc: isolation.has(Namespace::Pid) && isolation.has(Namespace::Mount),
            hostname: isolation.hostname.clone(),
            domainname: isolation.domainname.clone(),
            sysctls: isolation.sysctls.clone(),
            init: isolation.has(Namespace::Pid),
            parent: (!isolation.has(Namespace::Pid)).then(|| nix::unistd::getpid().as_raw()),
            root: isolation.root.as_ref().map(|root| {
                RootSetup::new(
                    root,
                    isolation.has(Namespace::Ipc),
                    isolation.id_maps.as_ref(),
                    &isolation.devices.nodes,
                    attach,
                )
            }),
            gate,
            security: isolation.security.clone(),
            user_namespace: isolation
                .id_maps
                .as_ref()
                .map(|maps| UserNamespaceSetup { maps: maps.clone() }),
            cgroup: CgroupSetup {
                namespace: isolation.has(Namespace::Cgroup),
                mount: isolation
                    .cgroup_mount
                    .filter(|_| own_cgroup && isolation.has(Namespace::Mount)),
            },
            hooks: hooks_pipe.map(|pipe| {
                let hooks = isolation
                    .hooks
                    .as_ref()
                    .map(|container| container.hooks.clone())
                    .unwrap_or_default();
                HooksSetup {
                    pipe,
                    create_container: hooks.create_container,
                    start_container: hooks.start_container,
                }
            }),
        }
    }

    /// The command's spec, rebuilt (and its environment validated).
    pub(crate) fn spec(&self) -> Result<ProcessSpec> {
        let mut env = Env::new();
        for (key, value) in &self.env {
            env.set(OsStr::from_bytes(key), OsStr::from_bytes(value))
                // Validated when shipyard built it: only a corrupt setup gets here.
                .map_err(|e| {
                    Error::sys(
                        format!("the container's setup: {e}"),
                        nix::errno::Errno::EINVAL,
                    )
                })?;
        }
        let rlimits = self
            .rlimits
            .iter()
            .map(|(name, soft, hard)| {
                Rlimit::new(
                    name.parse()?,
                    unlimited_if_none(*soft),
                    unlimited_if_none(*hard),
                )
            })
            .collect::<std::result::Result<_, _>>()
            .map_err(super::spec::SpecError::from)?;
        Ok(ProcessSpec {
            args: self.args.iter().cloned().map(OsString::from_vec).collect(),
            env,
            cwd: PathBuf::from(OsString::from_vec(self.cwd.clone())),
            user: User {
                uid: self.uid,
                gid: self.gid,
                umask: self.umask,
                additional_gids: self.additional_gids.clone(),
                names: Names::from(&self.names),
            },
            rlimits,
            oom_score_adj: self.oom_score_adj,
        })
    }

    /// Writes the setup to a new memfd, sealed so nothing can change it.
    pub(crate) fn to_memfd(&self) -> Result<OwnedFd> {
        let json = serde_json::to_vec(self).map_err(|e| {
            Error::sys(
                format!("encoding the container's setup: {e}"),
                nix::errno::Errno::EINVAL,
            )
        })?;
        let fd = memfd_create(
            c"shipyard-setup",
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        )
        .map_err(|e| Error::sys("creating the container's setup file (memfd)", e))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&json)
            .map_err(|e| Error::sys("writing the container's setup", io_errno(&e)))?;
        let fd = OwnedFd::from(file);
        fcntl(
            &fd,
            FcntlArg::F_ADD_SEALS(SealFlag::from_bits_retain(SEALS)),
        )
        .map_err(|e| Error::sys("sealing the container's setup", e))?;
        Ok(fd)
    }
}

/// The seals `to_memfd` adds: no writes, no resizing, no other seals.
const SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

/// Whether `fd` is a setup as `to_memfd` leaves it: a memfd with every
/// seal. The hidden entry point checks it before trusting its arguments, so
/// started by hand (with any other descriptor) it touches nothing.
pub(crate) fn is_sealed_setup(fd: RawFd) -> bool {
    // SAFETY: `F_GET_SEALS` only reads the flags. Raw `fcntl` because `fd`
    // is any number from the command line: one that isn't open gives EBADF
    // (a `BorrowedFd` would require it to be open), anything but a memfd
    // EINVAL.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    seals != -1 && seals & SEALS == SEALS
}

/// Reads the setup from the memfd at `fd` (inherited across the exec), and
/// closes it. A setup that doesn't decode is EINVAL. (Not a method: `Setup`
/// derives `Deserialize`, and the descriptor handling here is `unsafe`.)
pub(crate) fn read_setup(fd: RawFd) -> nix::Result<Setup> {
    // SAFETY: the descriptor was passed by number across the exec; this
    // process owns it, and nothing else in it uses it.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    // The offset is shared with shipyard's copy, which left it at the end.
    file.seek(SeekFrom::Start(0)).map_err(|e| io_errno(&e))?;
    let mut json = Vec::new();
    file.read_to_end(&mut json).map_err(|e| io_errno(&e))?;
    serde_json::from_slice(&json).map_err(|_| nix::errno::Errno::EINVAL)
}

fn finite(value: RlimitValue) -> Option<u64> {
    match value {
        RlimitValue::Finite(n) => Some(n),
        RlimitValue::Unlimited => None,
    }
}

fn unlimited_if_none(value: Option<u64>) -> RlimitValue {
    value.map_or(RlimitValue::Unlimited, RlimitValue::Finite)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::RlimitKind;

    #[test]
    fn a_setup_survives_the_memfd_byte_for_byte() {
        let mut env = Env::new();
        // Not UTF-8, and large: past a pipe's 64 KiB buffer.
        env.set(OsStr::from_bytes(b"BYTES"), OsStr::from_bytes(b"\xff\xfe"))
            .unwrap();
        let big = "x".repeat(200 * 1024);
        env.set("BIG".as_ref(), big.as_ref()).unwrap();
        let spec = ProcessSpec {
            args: vec![OsString::from_vec(b"/bin/\xffprog".to_vec()), "arg".into()],
            env,
            cwd: PathBuf::from(OsString::from_vec(b"/w\xff".to_vec())),
            user: User {
                uid: 1000,
                gid: 1000,
                umask: Some(0o027),
                additional_gids: vec![5, 6],
                names: Names {
                    user: Some("alice".into()),
                    group: None,
                    additional_groups: vec!["wheel".into()],
                    memberships: true,
                },
            },
            rlimits: vec![
                // Unlimited is allowed for `core` everywhere (not for
                // `nofile` on Linux).
                Rlimit::new(
                    RlimitKind::Core,
                    RlimitValue::Finite(64),
                    RlimitValue::Unlimited,
                )
                .unwrap(),
            ],
            oom_score_adj: None,
        };
        let mut isolation = Isolation::standard();
        isolation.hostname = Some("box".into());
        let mut root = Root::new(PathBuf::from(OsString::from_vec(b"/srv/\xffroot".to_vec())));
        root.read_only = true;
        isolation.root = Some(root.clone());
        let mut hooks = crate::hooks::Hooks::default();
        hooks.start_container.push(Hook::new("/bin/start"));
        hooks.poststop.push(Hook::new("/bin/runtime-only"));
        isolation.hooks = Some(crate::hooks::ContainerHooks::new(
            hooks,
            "c1",
            "/b",
            std::collections::BTreeMap::new(),
        ));
        let setup = Setup::new(
            &spec,
            &[libc::SIGHUP],
            &isolation,
            Some(7),
            true,
            Some(9),
            None,
        );
        let fd = setup.to_memfd().unwrap();
        // Sealed: no more writes.
        assert!(nix::unistd::write(&fd, b"x").is_err());
        assert!(is_sealed_setup(std::os::fd::AsRawFd::as_raw_fd(&fd)));
        let fd = std::os::fd::IntoRawFd::into_raw_fd(fd);
        let back = read_setup(fd).unwrap();
        assert_eq!(back.spec().unwrap(), spec);
        assert_eq!(back.keep_ignored, [libc::SIGHUP]);
        assert_eq!(back.hostname.as_deref(), Some("box"));
        assert!(back.init && back.mount_proc && back.private_mounts);
        assert_eq!(back.gate, Some(7));
        let back_hooks = back.hooks.unwrap();
        assert_eq!(back_hooks.pipe, 9);
        assert!(back_hooks.create_container.is_empty());
        assert_eq!(back_hooks.start_container, [Hook::new("/bin/start")]);
        let back_root = back.root.unwrap();
        assert_eq!(back_root.path(), root.path);
        assert!(back_root.read_only && back_root.mqueue);
        assert!(
            back_root
                .masked()
                .eq(root.masked_paths.iter().map(PathBuf::as_path))
        );
        assert!(
            back_root
                .readonly()
                .eq(root.readonly_paths.iter().map(PathBuf::as_path))
        );
    }

    #[test]
    fn only_a_sealed_memfd_passes_as_a_setup() {
        let (read, _write) = nix::unistd::pipe().unwrap();
        assert!(!is_sealed_setup(std::os::fd::AsRawFd::as_raw_fd(&read)));
        assert!(!is_sealed_setup(-1));
        let unsealed = memfd_create(c"unsealed", MFdFlags::MFD_ALLOW_SEALING).unwrap();
        assert!(!is_sealed_setup(std::os::fd::AsRawFd::as_raw_fd(&unsealed)));
    }
}
