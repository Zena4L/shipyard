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

use super::child::{
    SETUP_FAILED, close_from, confine_after_switch, confine_before_switch, make_undumpable,
};
use super::internal::{
    Internal, close_on_exec, decode_namespaces, fd_bound, internal_entry, join, setgroups_allowed,
    split_namespaces, start_sealed,
};
use super::setup::read_sealed_json;
use crate::container::{Helper, HelperFailure, HelperIo, RunningHelper};
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
    /// Namespaces owned outside the container's user namespace, joined
    /// first; the user namespace; then the rest, the mount one last:
    /// `(index in Namespace::ALL, descriptor)`.
    early: Vec<(u64, RawFd)>,
    user: Option<RawFd>,
    late: Vec<(u64, RawFd)>,
    setgroups_allowed: bool,
    /// The report pipe's write end.
    report: RawFd,
    /// The PID of the process that started it, which it dies with.
    starter: i32,
    pause_ms: u32,
}

impl HelperSetup {
    /// Every descriptor it inherits by number.
    fn passed(&self) -> Vec<RawFd> {
        self.early
            .iter()
            .chain(&self.late)
            .map(|&(_, fd)| fd)
            .chain(self.user)
            .chain([self.report])
            .collect()
    }
}

/// Starts helper function `name` with `args`, in `joining`'s namespaces
/// (which must include a mount namespace), its standard input and output
/// `stdin` and `stdout`.
pub(crate) fn launch(
    joining: &super::join::Joining,
    name: &str,
    args: &[u8],
    stdin: &OwnedFd,
    stdout: &OwnedFd,
) -> Result<RunningHelper> {
    super::internal::refuse_unsafe_dumpable("cp")?;
    let (early, late) = split_namespaces(joining)?;
    let (report_read, report_write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
        .map_err(|errno| Error::sys("making the helper's report pipe", errno))?;
    let pause_ms = if cfg!(debug_assertions) {
        std::env::var(PAUSE_VAR)
            .ok()
            .and_then(|ms| ms.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let setup = HelperSetup {
        name: name.to_owned(),
        args: args.to_vec(),
        early,
        user: joining.user.as_ref().map(AsRawFd::as_raw_fd),
        late,
        setgroups_allowed: setgroups_allowed(joining),
        report: report_write.as_raw_fd(),
        starter: nix::unistd::getpid().as_raw(),
        pause_ms,
    };
    let mut passed = setup.passed();
    let setup_fd = super::setup::sealed_json(&setup, c"shipyard-helper", "the helper's setup")?;
    passed.push(setup_fd.as_raw_fd());
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
        passed: &passed,
        stdio: [
            Some(stdin.as_fd()),
            Some(stdout.as_fd()),
            Some(null.as_fd()),
        ],
    };
    let child = start_sealed(&exe, host_exe.as_ref(), &how, "the helper", refused)?;
    Ok(RunningHelper {
        child: Some(child),
        report: Some(report_read),
    })
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
    close_on_exec(setup.passed().into_iter().chain([setup_fd]));
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
    let outcome = enter(&setup, helpers).and_then(|(helper, io)| (helper.run)(&io));
    let Err(failure) = outcome else {
        std::process::exit(0);
    };
    say(report, &failure);
    std::process::exit(1);
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
    //    last.
    hide()?;
    for (kind, fd) in decode_namespaces(&setup.early) {
        joined(kind, fd)?;
    }
    let maps = match setup.user {
        Some(user) => {
            joined(Namespace::User, user)?;
            hide()?;
            // Inside it now: the first column is the IDs as it sees them.
            Some((id_map("uid_map")?, id_map("gid_map")?))
        }
        None => None,
    };
    for (kind, fd) in decode_namespaces(&setup.late) {
        joined(kind, fd)?;
    }
    nix::unistd::chdir("/").map_err(setting_up("going to its root"))?;

    // 3. Root inside; dies with its starter; only its capabilities.
    if setup.user.is_some() {
        become_root(setup.setgroups_allowed)?;
        hide()?;
    }
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
    close_all_but([setup.report, own_fds.as_raw_fd()], bound)?;
    if setup.pause_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(u64::from(setup.pause_ms)));
    }
    let root = open(
        "/",
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(setting_up("opening the container's root"))?;
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
        own_fds,
    };
    Ok((helper, io))
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

/// Closes every descriptor from 3 up but `keep`.
fn close_all_but(mut keep: [RawFd; 2], bound: RawFd) -> std::result::Result<(), HelperFailure> {
    let closing =
        |failure: super::child::Failure| setup_failure(failure.errno(), "closing the host's files");
    keep.sort_unstable();
    let mut start = 3_u32;
    for fd in keep {
        let fd = u32::try_from(fd).unwrap_or(0);
        if fd < start {
            continue;
        }
        close_from(start, fd.saturating_sub(1), bound).map_err(closing)?;
        start = fd.saturating_add(1);
    }
    close_from(start, u32::MAX, bound).map_err(closing)
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

    #[test]
    fn an_id_map_reads_as_its_inside_ranges() {
        let map = "         0     100000      65536\n     65536       1000          1\n";
        assert_eq!(id_ranges(map), [(0, 65536), (65536, 1)]);
        assert_eq!(id_ranges("garbage\n0 1\n"), []);
    }
}
