//! A helper: shipyard's own code run inside a container's filesystem (Linux;
//! stage 8, step 8.5a; the public side is `container/helper.rs`).
//!
//! ```text
//! shipyard ── fork + exec of the sealed copy ──► helper (host PID namespace)
//!   1. from its own /proc, while it's the host's: the descriptor bound, the
//!      kernel's last capability, its /proc/self/fd
//!   2. non-dumpable; the namespaces the container's user namespace doesn't
//!      own; the user namespace (non-dumpable again; the ID maps read); the
//!      mount namespace: `/` and the working directory become the
//!      container's root
//!   3. root inside (non-dumpable again: a credential change resets it),
//!      killed if its starter dies, only the helper capabilities,
//!      `no_new_privs`
//!   4. every descriptor closed but 0, 1, 2, its report pipe and its own
//!      /proc/self/fd (the host's /proc, which shows only this process)
//!   5. the registered function; a failure (or a panic) goes back on the
//!      report pipe
//! ```
//!
//! **Why `/proc` first.** The helper doesn't join the container's PID
//! namespace, and once in its mount namespace, `/proc` is the container's:
//! it has no `/proc/self` for this process. What it needs from there is read
//! before, and `/proc/self/fd` kept open to reopen descriptors by (an
//! `O_PATH` one, checked with `fstat`, is reopened as the same file, never
//! by its path again).
//!
//! **Non-dumpable throughout**, as exec's joiner (TLPI §22.1; probe E4):
//! made so before anything of the container's is joined, again after the
//! user namespace and after the credential switch (each resets it to
//! `fs.suid_dumpable`; a host where that's 1 is refused). Its `/proc/PID` is
//! then root's, and ptrace refused to anyone without `CAP_SYS_PTRACE` over
//! the host's user namespace.
//!
//! **A stopped container** (stage 9, step 9.4c2,
//! [`launch_in_view`]) has no namespaces to join: the helper makes its own
//! instead. With the container's ID maps, a user namespace (its starter
//! writes the maps, told on a pipe, as for a container; then root in it);
//! then a mount namespace, made private, where the container's root is
//! mounted as a start mounts it (`rootfs::view`: the overlay, the volumes).
//! Its mounts end with it. Then steps 3 to 5, as above (root in its user
//! namespace already).
//!
//! **What can reach it.** Nothing in a container with a PID namespace of
//! its own: the helper isn't in it. A container that shares the host's (or
//! another) PID namespace sees it, and its processes running as the same
//! user (root inside, after the switch) can signal it, stopping a copy
//! (Ctrl-C ends it); ptrace and its `/proc` entry stay refused.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};

use super::child::{SETUP_FAILED, confine_after_switch, confine_before_switch, make_undumpable};
use super::internal::{
    Internal, close_on_exec, decode_namespaces, fd_bound, internal_entry, join, setgroups_allowed,
    split_namespaces, start_sealed,
};
use super::setup::read_sealed_json;
use crate::container::{Helper, HelperFailure, HelperIo, HelperRoot, RunningHelper};
use crate::isolation::Namespace;
use crate::security::Confinement;
use crate::{Error, Result};

/// The hidden argument that makes the shipyard binary run as a helper.
const HELPER_ENTRY_ARG: &std::ffi::CStr = c"__shipyard-helper";

/// The test hook (`SHIPYARD_TEST_HELPER_PAUSE`, milliseconds): the helper
/// waits that long once it's set up, before its function runs, so a test can
/// look at it. Debug builds only.
const PAUSE_VAR: &str = "SHIPYARD_TEST_HELPER_PAUSE";

/// What a helper needs, in a sealed memfd (never in its arguments, which
/// anyone can read).
#[derive(Debug, Serialize, Deserialize)]
struct HelperSetup {
    /// The function to run.
    name: String,
    /// What it's given.
    args: Vec<u8>,
    /// How it reaches the container's files.
    how: How,
    /// The report pipe's write end.
    report: RawFd,
    /// The PID of the process that started it, which it dies with.
    starter: i32,
    pause_ms: u32,
}

/// How a helper reaches a container's files.
#[derive(Debug, Serialize, Deserialize)]
enum How {
    /// A container that's up: its namespaces, joined ([`launch`]).
    Join {
        /// Namespaces owned outside the container's user namespace, joined
        /// first; the user namespace; then the rest, the mount one last:
        /// `(index in Namespace::ALL, descriptor)`.
        early: Vec<(u64, RawFd)>,
        user: Option<RawFd>,
        late: Vec<(u64, RawFd)>,
        setgroups_allowed: bool,
        /// Whether its root is to be the container's root mount alone, a
        /// clone with nothing mounted on it (`export`), made before its
        /// capabilities go.
        clone_root: bool,
    },
    /// One that isn't: namespaces of its own, its root mounted there
    /// ([`launch_in_view`]).
    Mount(Box<ViewSetup>),
}

/// A stopped container's root, for a helper to mount in namespaces of its
/// own ([`launch_in_view`]).
#[derive(Debug, Serialize, Deserialize)]
struct ViewSetup {
    /// The root, as a start mounts it.
    root: super::setup::RootSetup,
    /// With a user namespace of its own: the write end of the pipe it says
    /// it's made on, and the read end of the one its starter answers on once
    /// the maps are written (closed unanswered: they weren't).
    user: Option<(RawFd, RawFd)>,
}

impl HelperSetup {
    /// Every descriptor it inherits by number.
    fn inherited(&self) -> Vec<RawFd> {
        let mut inherited = vec![self.report];
        match &self.how {
            How::Join {
                early, user, late, ..
            } => {
                inherited.extend(early.iter().chain(late).map(|&(_, fd)| fd));
                inherited.extend(*user);
            }
            How::Mount(view) => {
                inherited.extend(view.root.attach());
                inherited.extend(view.user.into_iter().flat_map(|(ready, go)| [ready, go]));
            }
        }
        inherited
    }
}

/// Starts helper function `name` with `args`, in `joining`'s namespaces
/// (which must include a mount namespace), its standard input and output
/// `stdin` and `stdout`; its root the container's root mount alone if
/// `clone_root`.
pub(crate) fn launch(
    joining: &super::join::Joining,
    name: &str,
    args: &[u8],
    stdin: &OwnedFd,
    stdout: &OwnedFd,
    clone_root: bool,
) -> Result<RunningHelper> {
    super::internal::refuse_unsafe_dumpable("cp")?;
    let (early, late) = split_namespaces(joining)?;
    let how = How::Join {
        early,
        user: joining.user.as_ref().map(AsRawFd::as_raw_fd),
        late,
        setgroups_allowed: setgroups_allowed(joining),
        clone_root,
    };
    Ok(spawn(name, args, how, stdin, stdout)?.0)
}

/// Starts a helper running function `name` with `args`, reaching the
/// container `how` says, its standard input and output `stdin` and
/// `stdout`: the helper, and its PID.
fn spawn(
    name: &str,
    args: &[u8],
    how: How,
    stdin: &OwnedFd,
    stdout: &OwnedFd,
) -> Result<(RunningHelper, u32)> {
    let (report_read, report_write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
        .map_err(|errno| Error::sys("making the helper's report pipe", errno))?;
    let setup = HelperSetup {
        name: name.to_owned(),
        args: args.to_vec(),
        how,
        report: report_write.as_raw_fd(),
        starter: nix::unistd::getpid().as_raw(),
        pause_ms: pause_ms(),
    };
    let child = start(&setup, stdin, stdout)?;
    let pid = child.id();
    // Only the helper's copy of the write end left: its end of file is the
    // helper's end.
    drop(report_write);
    Ok((
        RunningHelper {
            child: Some(child),
            report: Some(report_read),
        },
        pid,
    ))
}

/// How long a helper waits before its function (a test's hook).
fn pause_ms() -> u32 {
    if cfg!(debug_assertions) {
        std::env::var(PAUSE_VAR)
            .ok()
            .and_then(|ms| ms.parse().ok())
            .unwrap_or(0)
    } else {
        0
    }
}

/// Starts the helper `setup` describes, its standard input and output
/// `stdin` and `stdout`.
fn start(setup: &HelperSetup, stdin: &OwnedFd, stdout: &OwnedFd) -> Result<std::process::Child> {
    let mut inherited = setup.inherited();
    let setup_fd = super::setup::sealed_json(setup, c"shipyard-helper", "the helper's setup")?;
    inherited.push(setup_fd.as_raw_fd());
    // Its standard error: nowhere (a file it opens must never land on 2).
    let null = open("/dev/null", OFlag::O_RDWR | OFlag::O_CLOEXEC, Mode::empty())
        .map_err(|errno| Error::sys("opening /dev/null for the helper", errno))?;
    // Where no sealed copy can be made or run: the file itself, whoever may
    // write it. The helper runs no code of the container's, and nothing in
    // it can reach the helper's /proc entry (non-dumpable).
    let refused = || Error::sys("starting the helper", Errno::EACCES);
    let (exe, host_exe) = super::spawn::own_binary(|_| true, |_| refused())?;
    let how = Internal {
        arg0: "shipyard-helper",
        entry: HELPER_ENTRY_ARG,
        setup: setup_fd.as_raw_fd(),
        inherited: &inherited,
        stdio: [
            Some(stdin.as_fd()),
            Some(stdout.as_fd()),
            Some(null.as_fd()),
        ],
    };
    start_sealed(&exe, host_exe.as_ref(), &how, "the helper", refused)
}

/// What a helper with a user namespace of its own waits for: its maps
/// written by its starter ([`launch_in_view`]).
pub(crate) struct Handshake {
    /// The helper's PID, whose maps are written.
    pub(crate) pid: u32,
    /// The maps.
    pub(crate) maps: crate::isolation::IdMaps,
    /// Where it says it has made its user namespace (a byte).
    pub(crate) ready: OwnedFd,
    /// Where it's told its maps are written (a byte; closed unanswered:
    /// they weren't).
    pub(crate) go: OwnedFd,
}

/// Starts helper function `name` with `args` (as [`launch`]) in container
/// root `view`, mounted in namespaces of the helper's own (see the module
/// docs), its standard input and output `stdin` and `stdout`. With ID maps,
/// the helper then waits for them: the [`Handshake`] to complete.
pub(crate) fn launch_in_view(
    view: &crate::container::RootView,
    name: &str,
    args: &[u8],
    stdin: &OwnedFd,
    stdout: &OwnedFd,
) -> Result<(RunningHelper, Option<Handshake>)> {
    super::internal::refuse_unsafe_dumpable("cp")?;
    // Real root mapping IDs: its overlay made here, as for an init.
    let prebuilt = super::spawn::prebuilt_root_overlay(&view.root, view.id_maps.as_ref())?;
    let root = super::setup::RootSetup::for_view(
        &view.root,
        view.id_maps.as_ref(),
        prebuilt.as_ref().map(AsRawFd::as_raw_fd),
    );
    let pipe = || {
        nix::unistd::pipe2(OFlag::O_CLOEXEC)
            .map_err(|errno| Error::sys("making the helper's pipes", errno))
    };
    // (read, write) each: ready written there, read here; go the other way.
    let sync = match &view.id_maps {
        Some(maps) => Some((maps.clone(), pipe()?, pipe()?)),
        None => None,
    };
    let how = How::Mount(Box::new(ViewSetup {
        root,
        user: sync
            .as_ref()
            .map(|(_, (_, ready), (go, _))| (ready.as_raw_fd(), go.as_raw_fd())),
    }));
    let (helper, pid) = spawn(name, args, how, stdin, stdout)?;
    // The helper has its own copies now.
    drop(prebuilt);
    Ok((
        helper,
        sync.map(|(maps, (ready, _), (_, go))| Handshake {
            pid,
            maps,
            ready,
            go,
        }),
    ))
}

/// Waits up to `wait` for a byte on `pipe`: `EPIPE` at its end of file (its
/// writer gone), `ETIMEDOUT` once the time is up.
pub(crate) fn wait_for_byte(
    pipe: &OwnedFd,
    wait: std::time::Duration,
) -> std::result::Result<(), Errno> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let deadline = std::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(Errno::ETIMEDOUT);
        }
        let timeout = PollTimeout::try_from(left).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(errno) => return Err(errno),
        }
        let mut byte = [0_u8];
        return match nix::unistd::read(pipe, &mut byte) {
            Ok(1) => Ok(()),
            Ok(_) => Err(Errno::EPIPE),
            Err(Errno::EINTR) => continue,
            Err(errno) => Err(errno),
        };
    }
}

/// Where the shipyard binary starts as a helper, running one of `helpers`.
/// Does nothing unless this process was started that way (by [`launch`]);
/// otherwise it never returns.
pub(crate) fn entry_point(helpers: &[Helper]) {
    if let Some(fd) = internal_entry(HELPER_ENTRY_ARG, "helper") {
        run(fd, helpers);
    }
}

/// The helper (see the module docs). Never returns.
fn run(setup_fd: RawFd, helpers: &[Helper]) -> ! {
    let _ = nix::sys::prctl::set_name(c"shipyard-helper");
    let Ok(setup) = read_sealed_json::<HelperSetup>(setup_fd) else {
        std::process::exit(SETUP_FAILED);
    };
    close_on_exec(setup.inherited().into_iter().chain([setup_fd]));
    // Its standard error goes nowhere: a panic is reported as a failure.
    let report = setup.report;
    std::panic::set_hook(Box::new(move |panic| {
        let failure = HelperFailure::new(
            HelperFailure::SETUP,
            Errno::UnknownErrno,
            format!("the helper panicked: {panic}"),
        );
        say(report, &failure);
    }));
    // Its own creation modes: what it makes gets its mode set anyway, but
    // never one the caller's umask narrowed.
    nix::sys::stat::umask(Mode::from_bits_truncate(0o022));
    let outcome = enter(&setup, helpers).and_then(|(helper, io)| {
        (helper.run)(&io)?;
        Ok(io.note.into_inner().ok().flatten())
    });
    match outcome {
        Ok(None) => std::process::exit(0),
        // A success with something to say: its note, as a failure of the
        // note's kind, which the starter reads as a success.
        Ok(Some(note)) => {
            say(
                report,
                &HelperFailure::new(HelperFailure::NOTE, Errno::UnknownErrno, note),
            );
            std::process::exit(0);
        }
        Err(failure) => {
            say(report, &failure);
            std::process::exit(1);
        }
    }
}

/// Sends `failure` back on the report pipe (once: the process then ends).
fn say(report: RawFd, failure: &HelperFailure) {
    if let Ok(json) = serde_json::to_vec(failure) {
        // SAFETY: the report pipe's write end, inherited; borrowed for one
        // write, as the process ends.
        let pipe = unsafe { std::os::fd::BorrowedFd::borrow_raw(report) };
        let _ = nix::unistd::write(pipe, &json);
    }
}

/// A setup step's failure, saying what it was doing.
fn setting_up(doing: &str) -> impl FnOnce(Errno) -> HelperFailure + '_ {
    move |errno| HelperFailure::new(HelperFailure::SETUP, errno, doing)
}

/// A setup step's failure: `errno`, while `doing`.
fn setup_failure(errno: Errno, doing: &str) -> HelperFailure {
    HelperFailure::new(HelperFailure::SETUP, errno, doing)
}

/// Steps 1 to 4 (see the module docs): the helper to run, and what it's
/// given.
fn enter<'h>(
    setup: &HelperSetup,
    helpers: &'h [Helper],
) -> std::result::Result<(&'h Helper, HelperIo), HelperFailure> {
    let helper = helpers
        .iter()
        .find(|helper| helper.name == setup.name)
        .ok_or_else(|| setup_failure(Errno::ENOENT, &format!("no helper named {}", setup.name)))?;

    // 1. From /proc/self while it's this process's.
    let last_cap =
        super::init::cap_last_cap().map_err(setting_up("reading the kernel's last capability"))?;
    let confinement = Confinement::for_helper(last_cap)
        .map_err(|_| setup_failure(Errno::EINVAL, "preparing the helper's capabilities"))?;
    let own_fds = open(
        "/proc/self/fd",
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(setting_up("opening /proc/self/fd"))?;
    let bound = fd_bound();

    // 2. Hidden from here on; the container's namespaces, the mount one
    //    last; or, for a stopped container, its own, its root mounted there.
    hide()?;
    let (maps, mounted, root_is) = match &setup.how {
        How::Mount(view) => {
            let (maps, root) = made(view)?;
            (maps, Some(root), HelperRoot::AsMounted)
        }
        How::Join {
            early,
            user,
            late,
            setgroups_allowed,
            clone_root,
        } => {
            let maps = joined_all(early, *user, late)?;
            // 3. (begun) Root inside the user namespace it joined.
            if user.is_some() {
                become_root(*setgroups_allowed)?;
                hide()?;
            }
            // The clone while it may still mount (its capabilities go in
            // step 3); refused, the container's root as it is, said.
            if *clone_root {
                match super::rootfs::clone_root_alone() {
                    Ok(alone) => (maps, Some(alone), HelperRoot::Alone),
                    // Locked children, no right to mount, a filter or an
                    // emulator without the new mount API, an LSM's refusal.
                    Err(errno @ (Errno::EINVAL | Errno::EPERM | Errno::ENOSYS | Errno::EACCES)) => {
                        (maps, None, HelperRoot::CloneRefused(errno as i32))
                    }
                    Err(errno) => {
                        return Err(setup_failure(errno, "cloning the container's root"));
                    }
                }
            } else {
                (maps, None, HelperRoot::AsMounted)
            }
        }
    };

    // 3. Dies with its starter; only its capabilities.
    // (After the credential calls: a change of credentials clears it.)
    nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)
        .map_err(setting_up("tying itself to its starter"))?;
    if nix::unistd::getppid().as_raw() != setup.starter {
        return Err(setup_failure(Errno::ESRCH, "its starter ended"));
    }
    confine_before_switch(&confinement)
        .and_then(|()| confine_after_switch(&confinement))
        .map_err(|failure| setup_failure(failure.errno(), "dropping its capabilities"))?;

    // 4. Nothing of the host's but its standard streams, the report pipe
    //    and its own /proc/self/fd.
    let mut keep = vec![setup.report, own_fds.as_raw_fd()];
    keep.extend(mounted.as_ref().map(AsRawFd::as_raw_fd));
    close_host_files(keep, bound)?;
    if setup.pause_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(u64::from(setup.pause_ms)));
    }
    let root = match mounted {
        Some(root) => root,
        None => open(
            "/",
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(setting_up("opening the container's root"))?,
    };
    let all = vec![(0, u32::MAX)];
    let (uids, gids) = maps.unwrap_or_else(|| (all.clone(), all));
    let io = HelperIo {
        args: setup.args.clone(),
        // SAFETY: its standard input and output, given by its starter; the
        // function owns them now.
        stdin: unsafe { std::fs::File::from_raw_fd(0) },
        // SAFETY: as above.
        stdout: unsafe { std::fs::File::from_raw_fd(1) },
        uids,
        gids,
        root,
        root_is,
        own_fds,
        note: std::sync::Mutex::new(None),
    };
    Ok((helper, io))
}

/// Joins the container's namespaces (the mount one last) and goes to its
/// root: the ID maps of its user namespace, as seen from inside, if it
/// joined one.
fn joined_all(
    early: &[(u64, RawFd)],
    user: Option<RawFd>,
    late: &[(u64, RawFd)],
) -> std::result::Result<Option<IdRanges>, HelperFailure> {
    for (kind, fd) in decode_namespaces(early) {
        joined(kind, fd)?;
    }
    let maps = match user {
        Some(user) => {
            joined(Namespace::User, user)?;
            hide()?;
            // Inside it now: the first column is the IDs as it sees them.
            Some((id_map("uid_map")?, id_map("gid_map")?))
        }
        None => None,
    };
    for (kind, fd) in decode_namespaces(late) {
        joined(kind, fd)?;
    }
    nix::unistd::chdir("/").map_err(setting_up("going to its root"))?;
    Ok(maps)
}

/// A user's and a group's ID ranges, as `(first, count)` inside.
type IdRanges = (Vec<(u32, u32)>, Vec<(u32, u32)>);

/// A stopped container's namespaces, made: a user namespace with its maps
/// (its starter writes them), root in it; a mount namespace, private; its
/// root mounted there. The maps as seen inside, and the root's directory.
fn made(view: &ViewSetup) -> std::result::Result<(Option<IdRanges>, OwnedFd), HelperFailure> {
    use nix::sched::{CloneFlags, unshare};
    let maps = match view.user {
        Some((ready, go)) => {
            unshare(CloneFlags::CLONE_NEWUSER)
                .map_err(|errno| refused(errno, "making a user namespace"))?;
            // Dumpable while its starter maps it: a non-dumpable process's
            // `/proc/PID/uid_map` is root's, and a `newuidmap` that isn't
            // setuid root (Fedora's: file capabilities, run as the caller)
            // can't open it. Nothing else is in this namespace, just made
            // for a stopped container's view; hidden again once root in it.
            nix::sys::prctl::set_dumpable(true)
                .map_err(setting_up("letting its ID maps be written"))?;
            // SAFETY: inherited for this; each used once, then the process
            // ends with them.
            let (ready, go) = unsafe { (OwnedFd::from_raw_fd(ready), OwnedFd::from_raw_fd(go)) };
            nix::unistd::write(&ready, &[1]).map_err(setting_up("telling its starter"))?;
            drop(ready);
            let mut byte = [0_u8];
            match nix::unistd::read(&go, &mut byte) {
                Ok(1) => {}
                Ok(_) => return Err(setup_failure(Errno::EPIPE, "waiting for its ID maps")),
                Err(errno) => return Err(setup_failure(errno, "waiting for its ID maps")),
            }
            become_root(super::user_namespace::setgroups_allowed())?;
            // Again: a credential change resets it.
            hide()?;
            Some((id_map("uid_map")?, id_map("gid_map")?))
        }
        None => None,
    };
    unshare(CloneFlags::CLONE_NEWNS).map_err(|errno| refused(errno, "making a mount namespace"))?;
    // Private: nothing mounted here reaches the host, nor the other way.
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        nix::mount::MsFlags::MS_REC | nix::mount::MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(setting_up("making its mounts private"))?;
    let root =
        super::rootfs::view(&view.root).map_err(|(doing, errno)| setup_failure(errno, &doing))?;
    Ok((maps, root))
}

/// A namespace not made, `doing`: why, where the host's policy says (a
/// security module, `user.max_user_namespaces`).
fn refused(errno: Errno, doing: &str) -> HelperFailure {
    if matches!(errno, Errno::EPERM | Errno::EACCES | Errno::ENOSPC) {
        let why = super::user_namespace::userns_refusal();
        setup_failure(errno, &format!("{doing} ({why})"))
    } else {
        setup_failure(errno, doing)
    }
}

/// Joins the namespace of `kind` open at `fd`.
fn joined(kind: Namespace, fd: RawFd) -> std::result::Result<(), HelperFailure> {
    join(fd, kind).map_err(|failure| {
        setup_failure(
            failure.errno(),
            &format!("joining the container's {} namespace", kind.proc_name()),
        )
    })
}

/// Non-dumpable (again).
fn hide() -> std::result::Result<(), HelperFailure> {
    make_undumpable()
        .map_err(|failure| setup_failure(failure.errno(), "making itself non-dumpable"))
}

/// One of this process's ID maps (`uid_map`, `gid_map`), as it sees it.
fn id_map(name: &str) -> std::result::Result<Vec<(u32, u32)>, HelperFailure> {
    std::fs::read_to_string(format!("/proc/self/{name}")).map_or_else(
        |err| Err(setup_failure(super::io_errno(&err), "reading its ID maps")),
        |text| Ok(id_ranges(&text)),
    )
}

/// Root in the user namespace it joined: no groups (where it may), then
/// group and user 0.
fn become_root(setgroups_allowed: bool) -> std::result::Result<(), HelperFailure> {
    use nix::unistd::{Gid, Uid, setgroups, setresgid, setresuid};
    if setgroups_allowed {
        setgroups(&[]).map_err(setting_up("clearing its groups"))?;
    }
    let (gid, uid) = (Gid::from_raw(0), Uid::from_raw(0));
    setresgid(gid, gid, gid).map_err(setting_up("becoming root's group in the container"))?;
    setresuid(uid, uid, uid).map_err(setting_up("becoming root in the container"))
}

/// Closes every descriptor from 3 up but `keep` (the joiner's sweep,
/// [`super::child::close_all_but`]).
fn close_host_files(mut keep: Vec<RawFd>, bound: RawFd) -> std::result::Result<(), HelperFailure> {
    keep.sort_unstable();
    let kept = keep.into_iter().map(|fd| u32::try_from(fd).unwrap_or(0));
    super::child::close_all_but(kept, bound)
        .map_err(|failure| setup_failure(failure.errno(), "closing the host's files"))
}

/// The ranges in an ID map (`/proc/PID/uid_map`), as `(first, count)` on
/// the reader's side (its first column).
fn id_ranges(map: &str) -> Vec<(u32, u32)> {
    map.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<u32>);
            let first = fields.next()?.ok()?;
            let _outside = fields.next()?.ok()?;
            let count = fields.next()?.ok()?;
            Some((first, count))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The starter's wait for a byte: there, its writer gone, or nothing
    /// before the time is up.
    #[test]
    fn a_byte_is_waited_for_and_never_forever() {
        use std::time::Duration;
        let pipe = || nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
        let (read, write) = pipe();
        nix::unistd::write(&write, &[1]).unwrap();
        assert_eq!(wait_for_byte(&read, Duration::from_secs(1)), Ok(()));
        drop(write);
        assert_eq!(
            wait_for_byte(&read, Duration::from_secs(1)),
            Err(Errno::EPIPE)
        );
        let (read, _write) = pipe();
        let began = std::time::Instant::now();
        assert_eq!(
            wait_for_byte(&read, Duration::from_millis(20)),
            Err(Errno::ETIMEDOUT)
        );
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    /// A helper that mounts a stopped container's root inherits its report
    /// pipe, the overlay real root made (to attach) and its ready and go
    /// pipes; one that joins, its namespaces.
    #[test]
    fn a_helper_inherits_what_it_reaches_with() {
        let mut root = crate::isolation::Root::new("/var/lib/c/merged");
        root.overlay = Some(crate::isolation::Overlay::new(
            vec!["/l".into()],
            "/u".into(),
            "/w".into(),
        ));
        let maps = crate::isolation::IdMaps::single((1000, 1000));
        let setup = |how| HelperSetup {
            name: "cp-out".to_owned(),
            args: Vec::new(),
            how,
            report: 3,
            starter: 1,
            pause_ms: 0,
        };
        let view = |attach, user| {
            How::Mount(Box::new(ViewSetup {
                root: super::super::setup::RootSetup::for_view(&root, Some(&maps), attach),
                user,
            }))
        };
        let mut inherited = setup(view(Some(7), Some((8, 9)))).inherited();
        inherited.sort_unstable();
        assert_eq!(inherited, [3, 7, 8, 9]);
        assert_eq!(setup(view(None, None)).inherited(), [3]);
        let joined = How::Join {
            early: vec![(0, 4)],
            user: Some(5),
            late: vec![(1, 6)],
            setgroups_allowed: true,
            clone_root: false,
        };
        let mut inherited = setup(joined).inherited();
        inherited.sort_unstable();
        assert_eq!(inherited, [3, 4, 5, 6]);
    }

    #[test]
    fn an_id_map_reads_as_its_inside_ranges() {
        let map = "         0     100000      65536\n     65536       1000          1\n";
        assert_eq!(id_ranges(map), [(0, 65536), (65536, 1)]);
        assert_eq!(id_ranges("garbage\n0 1\n"), []);
    }
}
