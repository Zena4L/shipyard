//! A container's device rules, enforced (stage 5, step 7): the program
//! `devices/program.rs` makes, loaded into the kernel and attached to the
//! container's cgroup with the `bpf(2)` system call (Linux).
//!
//! - **`BPF_PROG_LOAD`** hands the kernel the instructions. Its verifier
//!   checks them (every path ends, nothing is read before it's written, only
//!   the context's three words are read), and it returns a descriptor for the
//!   program. If it refuses, a second try asks for the verifier's log, which
//!   says why.
//! - **`BPF_PROG_ATTACH`** puts it on the cgroup's directory, as a
//!   `BPF_CGROUP_DEVICE` program. With `BPF_F_ALLOW_MULTI`, programs attached
//!   above (an outer runtime's) still run too: a device must pass every one.
//!
//! The cgroup keeps the program alive: both descriptors are closed after the
//! attach (they're close-on-exec from the start, as `bpf(2)` makes them).
//! Loading one needs `CAP_BPF` (or `CAP_SYS_ADMIN`) in the host's user
//! namespace (probe G12 found a user namespace's root allowed on one
//! kernel; the test machine's refuses it). Before Linux 5.11 its memory also
//! counts against `RLIMIT_MEMLOCK`, which root lifts for the load.

use std::os::fd::{AsRawFd, FromRawFd as _, OwnedFd};

use nix::errno::Errno;

use super::{Cgroup, CgroupError};
use crate::devices::Devices;

/// `bpf(2)` commands.
const BPF_PROG_LOAD: libc::c_int = 5;
const BPF_PROG_ATTACH: libc::c_int = 8;
/// The program type: a cgroup's device program.
const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
/// The attach type, for the same.
const BPF_CGROUP_DEVICE: u32 = 6;
/// Programs attached to the cgroup's ancestors run too.
const BPF_F_ALLOW_MULTI: u32 = 2;

/// How much of the verifier's log is kept when it refuses a program: room
/// for the whole of the longest program's (before Linux 6.4 a log too long
/// keeps its start, and the reason is at its end).
const LOG_SIZE: usize = 4 * 1024 * 1024;

/// `union bpf_attr`, as `BPF_PROG_LOAD` reads it (up to the fields used:
/// the kernel takes a shorter struct, and the rest as zero).
#[repr(C)]
#[derive(Debug, Default)]
struct ProgramLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

/// `union bpf_attr`, as `BPF_PROG_ATTACH` reads it.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgramAttach {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: u32,
}

impl Cgroup {
    /// Lets the processes in it use only the devices `devices`' rules allow:
    /// their program, loaded and attached. Before anything runs in it.
    ///
    /// # Errors
    ///
    /// The kernel refused to load or attach it (see [`CgroupError::Devices`];
    /// EPERM without the privilege to).
    pub fn restrict_devices(&self, devices: &Devices) -> Result<(), CgroupError> {
        let fail = |doing, errno, log| CgroupError::Devices {
            doing,
            path: self.path().to_path_buf(),
            errno,
            log,
        };
        let program = crate::devices::program(devices)?;
        let loaded = match load(&program) {
            // Before Linux 5.11, a program's memory counts against the
            // loader's RLIMIT_MEMLOCK, often 64 KiB: EPERM even for root.
            // Real root may lift it; only for the load, then it's put back
            // (the container inherits this process's limits).
            Err((Errno::EPERM, _)) if is_host_root() => with_memlock_lifted(|| load(&program)),
            loaded => loaded,
        }
        .map_err(|(errno, log)| fail("loading the device program", errno, log))?;
        let dir = self.open_dir()?;
        attach(&dir, &loaded)
            .map_err(|errno| fail("attaching the device program", errno, String::new()))
    }
}

/// Puts `devices` on `cgroup` ([`Cgroup::restrict_devices`]). Where this
/// process isn't real root (a rootless container), whatever the kernel's
/// refusal (EPERM without `CAP_BPF`; ENOSYS or EINVAL without BPF, or under
/// a seccomp profile that answers so), it goes without, as Podman does
/// rootless: a user namespace can't make device nodes anyway, and the
/// host's it can reach keep the host's permissions. Returns the warning
/// that's then due, if `devices` were asked for.
///
/// # Errors
///
/// Any refusal as real root: running without the program would let the
/// container use what it shouldn't.
pub(crate) fn enforce_devices(
    cgroup: &Cgroup,
    devices: &Devices,
) -> Result<Option<String>, CgroupError> {
    match cgroup.restrict_devices(devices) {
        Ok(()) => Ok(None),
        Err(CgroupError::Devices { errno, .. }) if !is_host_root() => {
            let why = if errno == Errno::EPERM {
                "loading a device program needs CAP_BPF and CAP_NET_ADMIN (or CAP_SYS_ADMIN) in the \
                 host's user namespace".to_owned()
            } else {
                format!("the kernel refused the device program: {}", errno.desc())
            };
            Ok(unenforced_devices(devices, &why))
        }
        Err(err) => Err(err),
    }
}

/// The warning for `devices` that can't be enforced, because of `why`: only
/// if they were asked for (the defaults going without is how a rootless
/// container is, and a container with no cgroup).
pub(crate) fn unenforced_devices(devices: &Devices, why: &str) -> Option<String> {
    unenforced_warning(devices, why, is_host_root())
}

/// [`unenforced_devices`]' warning, for a caller that is real root (`root`)
/// or not: what still limits the container differs.
fn unenforced_warning(devices: &Devices, why: &str, root: bool) -> Option<String> {
    let left = if root {
        "nothing but its capabilities limits which devices the container can use"
    } else {
        "the container can still use only what your user can"
    };
    devices
        .asked
        .then(|| format!("the device rules can't be enforced here ({why}); {left}"))
}

/// The warning for `devices` when the container has no cgroup, for `why`
/// (see [`unenforced_devices`]).
pub(crate) fn devices_without_cgroup(
    devices: &Devices,
    why: &impl std::fmt::Display,
) -> Option<String> {
    unenforced_devices(
        devices,
        &format!("the container has no cgroup of its own: {why}"),
    )
}

/// Whether this process is real root: euid 0 in the host's user namespace,
/// whose maps are the identity over every ID. (Root of another namespace
/// has no `CAP_BPF` where it counts.)
pub(crate) fn is_host_root() -> bool {
    nix::unistd::geteuid().is_root()
        && std::fs::read_to_string("/proc/self/uid_map")
            .is_ok_and(|map| map.split_whitespace().collect::<Vec<_>>() == ["0", "0", "4294967295"])
}

/// Loads `program` as a cgroup device program. On a refusal, the errno and
/// the verifier's log (from a second try that asks for it).
fn load(program: &[crate::devices::Instruction]) -> Result<OwnedFd, (Errno, String)> {
    let license = c"MIT OR Apache-2.0";
    let mut name = [0_u8; 16];
    name[..12].copy_from_slice(b"shipyard_dev");
    let mut attr = ProgramLoad {
        prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
        insn_cnt: u32::try_from(program.len()).map_err(|_| (Errno::E2BIG, String::new()))?,
        insns: program.as_ptr() as u64,
        license: license.as_ptr() as u64,
        prog_name: name,
        expected_attach_type: BPF_CGROUP_DEVICE,
        ..ProgramLoad::default()
    };
    let errno = match load_program(&mut attr) {
        Ok(fd) => return Ok(fd),
        // Not allowed: the log wouldn't say more.
        Err(errno @ Errno::EPERM) => return Err((errno, String::new())),
        Err(errno) => errno,
    };
    let mut log = vec![0_u8; LOG_SIZE];
    attr.log_level = 1;
    attr.log_size = u32::try_from(LOG_SIZE).unwrap_or(u32::MAX);
    attr.log_buf = log.as_mut_ptr() as u64;
    // The same load again: a refusal that doesn't happen twice (ENOMEM)
    // isn't one.
    if let Ok(loaded) = load_program(&mut attr) {
        return Ok(loaded);
    }
    let end = log.iter().position(|&byte| byte == 0).unwrap_or(log.len());
    let text = String::from_utf8_lossy(log.get(..end).unwrap_or_default());
    Err((errno, refusal(errno, &text)))
}

/// What a refused load says, from the verifier's `log`: its last lines,
/// where it stopped. Without a log, an EINVAL or ENOSYS means the kernel
/// didn't get as far as the verifier: it has no such programs.
fn refusal(errno: Errno, log: &str) -> String {
    if log.trim().is_empty() && matches!(errno, Errno::EINVAL | Errno::ENOSYS) {
        return NO_DEVICE_PROGRAMS.to_owned();
    }
    let tail: Vec<&str> = log.lines().rev().take(4).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    tail.join("; ")
}

/// What an EINVAL or ENOSYS without a verifier's log means.
const NO_DEVICE_PROGRAMS: &str = "this kernel can't run cgroup device programs: it needs \
                                  CONFIG_BPF_SYSCALL and CONFIG_CGROUP_BPF";

/// Runs `load` with this process's `RLIMIT_MEMLOCK` lifted, and puts it back
/// after. If it can't be lifted, `load` isn't tried again: the EPERM stands.
fn with_memlock_lifted(
    load: impl FnOnce() -> Result<OwnedFd, (Errno, String)>,
) -> Result<OwnedFd, (Errno, String)> {
    use nix::sys::resource::{RLIM_INFINITY, Resource, getrlimit, setrlimit};
    let refused = || (Errno::EPERM, String::new());
    let (soft, hard) = getrlimit(Resource::RLIMIT_MEMLOCK).map_err(|_| refused())?;
    setrlimit(Resource::RLIMIT_MEMLOCK, RLIM_INFINITY, RLIM_INFINITY).map_err(|_| refused())?;
    let loaded = load();
    let _ = setrlimit(Resource::RLIMIT_MEMLOCK, soft, hard);
    loaded
}

/// Attaches the program `loaded` to the cgroup open at `dir`.
fn attach(dir: &OwnedFd, loaded: &OwnedFd) -> Result<(), Errno> {
    let fd = |fd: &OwnedFd| u32::try_from(fd.as_raw_fd()).map_err(|_| Errno::EBADF);
    let mut attr = ProgramAttach {
        target_fd: fd(dir)?,
        attach_bpf_fd: fd(loaded)?,
        attach_type: BPF_CGROUP_DEVICE,
        attach_flags: BPF_F_ALLOW_MULTI,
        replace_bpf_fd: 0,
    };
    bpf(BPF_PROG_ATTACH, &mut attr).map(drop)
}

/// The `bpf(2)` call `command` with `attr`: what it returned (a new
/// descriptor for `BPF_PROG_LOAD`, 0 for `BPF_PROG_ATTACH`).
fn bpf<T>(command: libc::c_int, attr: &mut T) -> Result<libc::c_long, Errno> {
    // SAFETY: `attr` is a valid `bpf_attr` prefix of the size given, and
    // the pointers inside it point at buffers the caller keeps alive for the
    // call. glibc and musl have no wrapper: the raw call.
    let returned = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            command,
            std::ptr::from_mut(attr),
            std::mem::size_of::<T>(),
        )
    };
    Errno::result(returned)
}

/// `BPF_PROG_LOAD` with `attr`: the program's new descriptor.
fn load_program(attr: &mut ProgramLoad) -> Result<OwnedFd, Errno> {
    // The verifier gives up with EAGAIN when a signal is pending for the
    // thread (a child's SIGCHLD, say). Returning to user space delivers it,
    // so the load is simply tried again, as libbpf does.
    let mut tries = 1;
    let fd = loop {
        match bpf(BPF_PROG_LOAD, attr) {
            Err(Errno::EAGAIN) if tries < LOAD_TRIES => tries += 1,
            result => break result?,
        }
    };
    let fd = libc::c_int::try_from(fd).map_err(|_| Errno::EBADF)?;
    // SAFETY: `bpf(2)` just returned this descriptor; nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// How many times a load the verifier gave up on (EAGAIN) is tried: as
/// libbpf.
const LOAD_TRIES: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    /// Only rules asked for are warned about, and the warning says what still
    /// limits the container: the user's own rights, or for root nothing but
    /// its capabilities.
    #[test]
    fn the_unenforced_warning_says_what_still_limits() {
        let mut devices = Devices::docker_default();
        assert_eq!(unenforced_warning(&devices, "why", false), None);
        devices.asked = true;
        let rootless = unenforced_warning(&devices, "why", false).unwrap();
        assert!(rootless.ends_with("only what your user can"), "{rootless}");
        let root = unenforced_warning(&devices, "why", true).unwrap();
        assert!(
            root.contains("(why)")
                && root.ends_with("its capabilities limits which devices the container can use"),
            "{root}"
        );
    }

    /// The structs are the kernel's prefixes: these offsets are its
    /// (`include/uapi/linux/bpf.h`).
    #[test]
    fn attributes_have_the_kernels_layout() {
        assert_eq!(std::mem::offset_of!(ProgramLoad, insns), 8);
        assert_eq!(std::mem::offset_of!(ProgramLoad, license), 16);
        assert_eq!(std::mem::offset_of!(ProgramLoad, log_level), 24);
        assert_eq!(std::mem::offset_of!(ProgramLoad, log_buf), 32);
        assert_eq!(std::mem::offset_of!(ProgramLoad, kern_version), 40);
        assert_eq!(std::mem::offset_of!(ProgramLoad, prog_name), 48);
        assert_eq!(std::mem::offset_of!(ProgramLoad, expected_attach_type), 68);
        assert_eq!(std::mem::size_of::<ProgramLoad>(), 72);
        assert_eq!(std::mem::offset_of!(ProgramAttach, attach_flags), 12);
        assert_eq!(std::mem::size_of::<ProgramAttach>(), 20);
    }

    #[test]
    fn a_refusal_says_why() {
        assert_eq!(refusal(Errno::ENOSYS, ""), NO_DEVICE_PROGRAMS);
        assert_eq!(refusal(Errno::EINVAL, "\n"), NO_DEVICE_PROGRAMS);
        assert_eq!(
            refusal(
                Errno::EINVAL,
                "0: (b4) r0 = 0\n1: ok\n2: x\n3: y\nunreachable insn 4\n"
            ),
            "1: ok; 2: x; 3: y; unreachable insn 4"
        );
        assert_eq!(refusal(Errno::EPERM, ""), "");
    }

    /// The memlock limit is lifted for the load, and put back exactly.
    #[test]
    #[ignore = "needs root"]
    fn the_memlock_limit_is_lifted_and_put_back() {
        use nix::sys::resource::{RLIM_INFINITY, Resource, getrlimit, setrlimit};
        if !is_host_root() {
            return;
        }
        let before = getrlimit(Resource::RLIMIT_MEMLOCK).unwrap();
        setrlimit(Resource::RLIMIT_MEMLOCK, 4096, 8192).unwrap();
        let inside = with_memlock_lifted(|| {
            assert_eq!(
                getrlimit(Resource::RLIMIT_MEMLOCK).unwrap(),
                (RLIM_INFINITY, RLIM_INFINITY)
            );
            Err((Errno::EPERM, String::new()))
        });
        assert!(inside.is_err());
        assert_eq!(getrlimit(Resource::RLIMIT_MEMLOCK).unwrap(), (4096, 8192));
        setrlimit(Resource::RLIMIT_MEMLOCK, before.0, before.1).unwrap();
    }

    /// The kernel's verifier takes what the generator makes: Docker's
    /// defaults, an empty list, and a long one.
    #[test]
    #[ignore = "needs root"]
    fn the_verifier_accepts_the_programs() {
        if !is_host_root() {
            return;
        }
        let mut lists = vec![Devices::docker_default()];
        let mut empty = Devices::docker_default();
        empty.rules.clear();
        lists.push(empty);
        let mut long = Devices::docker_default();
        long.rules = (0..crate::devices::MAX_RULES)
            .map(|n| {
                let rule: crate::devices::DeviceRule = format!("c {n}:* rw").parse().unwrap();
                if n % 2 == 0 {
                    rule
                } else {
                    crate::devices::DeviceRule {
                        allow: false,
                        ..rule
                    }
                }
            })
            .collect();
        lists.push(long);
        for devices in lists {
            let program = crate::devices::program(&devices).unwrap();
            if let Err((errno, log)) = load(&program) {
                panic!("{} rules refused: {errno}: {log}", devices.rules.len());
            }
        }
    }
}
