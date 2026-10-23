//! Descriptors passed on to a container's process (stage 8, step 8.5b):
//! runc's `--preserve-fds N`, and systemd's socket activation.
//!
//! **What a process inherits.** Across `fork` a child gets a copy of every
//! descriptor; across `exec` it keeps those not marked close-on-exec (TLPI
//! §27.4). Shipyard marks or closes everything but 0, 1 and 2 on the way
//! to a container's command, so nothing of the host's leaks in by accident;
//! these are the ones it hands on on purpose.
//!
//! **Socket activation** (`sd_listen_fds(3)`): a service manager opens the
//! listening sockets, starts the program with them at 3, 4, … and says so
//! in its environment: `LISTEN_FDS` (how many), `LISTEN_PID` (whose they
//! are: a variable inherited by a child that isn't meant to take them, a
//! shell's, say, must be ignored), and `LISTEN_FDNAMES`. A runtime between
//! the two takes them if they're its own, and passes them on, saying so
//! again for the program's own PID.
//!
//! **The layout** is runc's: the listening sockets first (3 and up), then
//! the `--preserve-fds` ones after them.

use std::ffi::OsString;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt as _;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The most descriptors passed on.
pub(crate) const MAX_PASSED: usize = 1024;

/// The first descriptor passed on (after stdin, stdout and stderr).
pub(crate) const FIRST_PASSED: RawFd = 3;

/// Descriptors kept free below the soft limit beside the passed ones and
/// their copies, for what a spawn holds of its own meanwhile: a container's
/// monitor alone has some twenty (its pipes, the log writer, the attach
/// listener, the gate's FIFOs, the report and go pipes, the sealed binary).
pub(crate) const SLACK: usize = 48;

/// Descriptors to pass on, in the order they're to be numbered from 3.
#[derive(Debug, Default)]
pub struct PassedFds {
    /// The descriptors (this process's copies, close-on-exec, numbered
    /// above the targets).
    pub(crate) fds: Vec<OwnedFd>,
    /// The first `count` of them are socket activation's.
    pub(crate) listen: Option<Listen>,
    /// The targets themselves (3 and up), held, close-on-exec, so that
    /// nothing this process opens before passing them on takes one of
    /// their numbers.
    pub(crate) _reserved: Vec<OwnedFd>,
}

/// Socket activation's part of what's passed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listen {
    /// How many of the passed descriptors are listening sockets.
    pub count: usize,
    /// Their names (`LISTEN_FDNAMES`), as given.
    pub names: Option<OsString>,
}

/// What a process that starts the command needs to know of the passed
/// descriptors: their numbers in it (in target order), and socket
/// activation's part. Plain data: it crosses `exec` in a setup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PassedPlan {
    /// The descriptors' numbers, in target order (3, 4, …).
    pub(crate) sources: Vec<RawFd>,
    /// How many of them are listening sockets, and their names.
    pub(crate) listen: Option<(usize, Option<Vec<u8>>)>,
}

impl PassedPlan {
    /// The first descriptor above the targets: where the command's sweep
    /// of what it mustn't keep starts.
    pub(crate) fn above(&self) -> RawFd {
        above(self.sources.len())
    }

    /// The environment socket activation adds, but `LISTEN_PID`: `(name,
    /// value)` byte strings.
    pub(crate) fn listen_env(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some((count, names)) = &self.listen else {
            return Vec::new();
        };
        let mut env = vec![(b"LISTEN_FDS".to_vec(), count.to_string().into_bytes())];
        if let Some(names) = names {
            env.push((b"LISTEN_FDNAMES".to_vec(), names.clone()));
        }
        env
    }
}

/// The names socket activation's environment uses: the command's own are
/// replaced by what's passed.
pub(crate) const LISTEN_NAMES: [&[u8]; 3] = [b"LISTEN_FDS", b"LISTEN_PID", b"LISTEN_FDNAMES"];

/// One above the targets for `count` descriptors.
fn above(count: usize) -> RawFd {
    FIRST_PASSED.saturating_add(RawFd::try_from(count).unwrap_or(RawFd::MAX))
}

/// The target of the `i`th passed descriptor (from 0): 3, 4, …
fn target(i: usize) -> RawFd {
    above(i)
}

impl PassedFds {
    /// What this process was given to pass on: socket activation's
    /// descriptors, if `LISTEN_PID` names this process, then `preserve`
    /// more (`--preserve-fds`). The originals (3 and up) stay open,
    /// close-on-exec (no child inherits them), as `_reserved`;
    /// the copies are above them.
    ///
    /// # Errors
    ///
    /// One of them isn't open ([`Error::FdNotOpen`]), or there are too many
    /// ([`Error::TooManyFds`]).
    pub fn from_caller(preserve: usize) -> Result<Self> {
        let listen = listen_from(
            std::env::var_os("LISTEN_PID").as_deref(),
            std::env::var_os("LISTEN_FDS").as_deref(),
            std::env::var_os("LISTEN_FDNAMES"),
            std::process::id(),
        );
        let count = listen
            .as_ref()
            .map_or(0, |listen| listen.count)
            .saturating_add(preserve);
        let (fds, reserved) = take(count)?;
        Ok(Self {
            fds,
            listen,
            _reserved: reserved,
        })
    }

    /// Nothing to pass on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }

    /// How many there are: the descriptors 3 up to `3 + len` are theirs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fds.len()
    }

    /// What a process that starts the command needs to know of them.
    pub(crate) fn plan(&self) -> PassedPlan {
        PassedPlan {
            sources: self
                .fds
                .iter()
                .map(std::os::fd::AsRawFd::as_raw_fd)
                .collect(),
            listen: self
                .listen
                .as_ref()
                .map(|listen| (listen.count, listen.names.clone().map(OsString::into_vec))),
        }
    }
}

/// Socket activation's descriptors, if `pid` and `fds` (`LISTEN_PID` and
/// `LISTEN_FDS`) say this process (`own`) has any: `sd_listen_fds`'s rules.
fn listen_from(
    pid: Option<&std::ffi::OsStr>,
    fds: Option<&std::ffi::OsStr>,
    names: Option<OsString>,
    own: u32,
) -> Option<Listen> {
    let count = shipyard_sys::listen::passed(
        own,
        pid.and_then(std::ffi::OsStr::to_str),
        fds.and_then(std::ffi::OsStr::to_str),
    );
    (count > 0).then_some(Listen { count, names })
}

/// Takes descriptors 3 to 3+`count`-1: copies (close-on-exec, above them
/// all), and the originals, made close-on-exec.
fn take(count: usize) -> Result<(Vec<OwnedFd>, Vec<OwnedFd>)> {
    let max = most_passable();
    if count > max {
        return Err(Error::TooManyFds { count, max });
    }
    let targets = (0..count).map(target);
    // All of them checked first.
    for fd in targets.clone() {
        // SAFETY: F_GETFD only reads the descriptor's flags.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            return Err(Error::FdNotOpen {
                fd,
                last: above(count).saturating_sub(1),
            });
        }
    }
    let mut copies = Vec::with_capacity(count);
    let mut originals = Vec::with_capacity(count);
    for fd in targets {
        // SAFETY: `fd` is open (checked above) and this process's: the
        // copy is new, and ours.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, above(count)) };
        if copy == -1 {
            return Err(Error::sys(
                "copying a descriptor to pass on",
                nix::errno::Errno::last(),
            ));
        }
        // SAFETY: just made by `fcntl`, ours alone.
        copies.push(unsafe { OwnedFd::from_raw_fd(copy) });
    }
    // Every copy made: the originals are this process's from here on (the
    // caller gave them to pass on). Close-on-exec, so no child inherits
    // them; F_SETFD on an open descriptor can't fail.
    for fd in (0..count).map(target) {
        // SAFETY: F_SETFD only changes the descriptor's flags.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        // SAFETY: open (checked above), given to this process to pass on.
        originals.push(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    Ok((copies, originals))
}

/// The most descriptors that can be passed on here: the originals and their
/// copies must fit below the soft limit (where `F_DUPFD` stops), with room
/// to spare.
fn most_passable() -> usize {
    let soft = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .ok()
        .and_then(|(soft, _)| usize::try_from(soft).ok())
        .unwrap_or(usize::MAX);
    MAX_PASSED.min(soft.saturating_sub(SLACK.saturating_add(3)) / 2)
}

/// Descriptors passed across an exec for a command, this process's own
/// now, with their targets (3 and up) held ([`reserve`]): what the process
/// opens from then on can't take one of their numbers. The one way the
/// monitor, the init and the joiner take them. Dropped once the command
/// has its own: a socket the command closes is then closed.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the monitor, init and joiner are Linux's")
)]
pub(crate) struct Held {
    _sources: Vec<OwnedFd>,
    _reserved: Vec<OwnedFd>,
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the monitor, init and joiner are Linux's")
)]
impl Held {
    /// `sources`, held, and their targets reserved. The targets must be
    /// free (see [`reserve`]).
    ///
    /// # Errors
    ///
    /// A target couldn't be held.
    pub(crate) fn take(sources: Vec<OwnedFd>) -> Result<Self> {
        let reserved = reserve(sources.len())?;
        Ok(Self {
            _sources: sources,
            _reserved: reserved,
        })
    }
}

/// The descriptors `sources` names, as this process's own.
///
/// # Safety
///
/// Each is open, this process's, passed to it for this, and owned by
/// nothing else in it (taken once).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the monitor, init and joiner are Linux's")
)]
pub(crate) unsafe fn adopt(sources: &[std::os::fd::RawFd]) -> Vec<OwnedFd> {
    use std::os::fd::FromRawFd as _;
    sources
        .iter()
        // SAFETY: the caller's promise.
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect()
}

/// Holds descriptors 3 to 3+`count`-1 with `/dev/null`, close-on-exec, so
/// that nothing this process opens takes their numbers before the command
/// is placed there (see `child::place_passed`). They must be free: what's
/// passed on, and everything else this process was given, is above them.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the monitor, init and joiner are Linux's")
)]
pub(crate) fn reserve(count: usize) -> Result<Vec<OwnedFd>> {
    // 0, 1 and 2 are taken and the targets free: they're the lowest free
    // numbers, so each open lands on the next one.
    (0..count)
        .map(|i| {
            let target = target(i);
            let null = nix::fcntl::open(
                "/dev/null",
                nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )
            .map_err(|errno| Error::sys("opening /dev/null to hold a descriptor number", errno))?;
            if null.as_raw_fd() != target {
                let what = std::fs::read_link(format!("/proc/self/fd/{target}")).map_or_else(
                    |_| "something".to_owned(),
                    |path| path.display().to_string(),
                );
                return Err(Error::sys(
                    format!("holding descriptor {target} for a passed one (it's {what}'s)"),
                    nix::errno::Errno::EBUSY,
                ));
            }
            Ok(null)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn activation_is_only_for_the_process_it_names() {
        let ours = |pid: &str, fds: &str| {
            listen_from(Some(OsStr::new(pid)), Some(OsStr::new(fds)), None, 42)
        };
        assert_eq!(ours("42", "2").map(|l| l.count), Some(2));
        assert_eq!(ours("41", "2"), None, "another process's");
        assert_eq!(ours("42", "0"), None, "none");
        assert_eq!(ours("42", "two"), None, "not a number");
        assert_eq!(ours(" 42", "2"), None);
        assert_eq!(listen_from(None, Some(OsStr::new("2")), None, 42), None);
        assert_eq!(listen_from(Some(OsStr::new("42")), None, None, 42), None);
        let named = listen_from(
            Some(OsStr::new("42")),
            Some(OsStr::new("1")),
            Some(OsString::from("http")),
            42,
        );
        assert_eq!(named.unwrap().names.as_deref(), Some(OsStr::new("http")));
    }

    #[test]
    fn the_environment_says_how_many_and_their_names() {
        let passed = PassedFds {
            listen: Some(Listen {
                count: 2,
                names: Some(OsString::from("a:b")),
            }),
            ..PassedFds::default()
        };
        assert_eq!(
            passed.plan().listen_env(),
            [
                (b"LISTEN_FDS".to_vec(), b"2".to_vec()),
                (b"LISTEN_FDNAMES".to_vec(), b"a:b".to_vec()),
            ]
        );
        assert!(PassedPlan::default().listen_env().is_empty());
        assert_eq!(PassedPlan::default().above(), 3);
    }

    #[test]
    fn too_many_are_refused() {
        assert!(matches!(
            take(MAX_PASSED + 1),
            Err(Error::TooManyFds { count, .. }) if count == MAX_PASSED + 1
        ));
        assert!(most_passable() <= MAX_PASSED);
    }
}
