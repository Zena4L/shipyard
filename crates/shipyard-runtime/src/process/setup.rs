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

use crate::isolation::{Isolation, Mount, MountFlag, MountKind, Namespace, Root};
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
    /// More mounts, in order ([`Root::mounts`]).
    pub(crate) mounts: Vec<MountSetup>,
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
    fn new(root: &Root, mqueue: bool) -> Self {
        let bytes = |path: &PathBuf| path.as_os_str().as_bytes().to_vec();
        Self {
            path: bytes(&root.path),
            read_only: root.read_only,
            masked: root.masked_paths.iter().map(bytes).collect(),
            readonly: root.readonly_paths.iter().map(bytes).collect(),
            mqueue,
            mounts: root.mounts.iter().map(MountSetup::new).collect(),
        }
    }

    /// The directory on the host.
    pub(crate) fn path(&self) -> &Path {
        as_path(&self.path)
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
    /// The setup for running `spec` isolated as `isolation` says.
    pub(crate) fn new(
        spec: &ProcessSpec,
        keep_ignored: &[libc::c_int],
        isolation: &Isolation,
        gate: Option<RawFd>,
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
            root: isolation
                .root
                .as_ref()
                .map(|root| RootSetup::new(root, isolation.has(Namespace::Ipc))),
            gate,
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
        };
        let mut isolation = Isolation::standard();
        isolation.hostname = Some("box".into());
        let mut root = Root::new(PathBuf::from(OsString::from_vec(b"/srv/\xffroot".to_vec())));
        root.read_only = true;
        isolation.root = Some(root.clone());
        let setup = Setup::new(&spec, &[libc::SIGHUP], &isolation, Some(7));
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
