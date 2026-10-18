//! A network's DNS server: this binary, re-executed as `shipyard
//! __shipyard-dns`, by the first run on the network that finds none running
//! (Linux). See `shipyard_network::dns` for what it answers, and
//! `shipyard_network::dns::server` for how it lives and stops.
//!
//! The starter (a run, under the network's lock, its lease held) takes the
//! network's `dns.lock`; if it can't, a server holds it, and there's
//! nothing to do. Otherwise the server is started with:
//!
//! - the lock, already held: it's passed on as an open descriptor, so it's
//!   never free between the run's check and the server's start, and held as
//!   long as the server lives (the run's copy closes; a lock taken with
//!   `flock` goes only when every copy has, or it's let go of);
//! - a pipe it says `ready` on once its sockets are bound and it has given
//!   up root (or why it can't);
//! - its own session (`setsid`): a Ctrl-C meant for the run doesn't stop it;
//! - its own cgroup, `shipyard/dns/<network>-<random>` (with small limits),
//!   joined before it runs anything: never the starter's, so a login
//!   session's end (logind's `KillUserProcesses`) or a unit's stop doesn't
//!   take it. If it can't be had, the server runs where it was started,
//!   said in its log.
//!
//! It's started before the run's supervisor is installed, so the supervisor
//! counts it as a child it inherited, never one the command left behind:
//! it outlives the run.

use std::fs::TryLockError;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use shipyard_network::dns::server;
use shipyard_network::store::{NetworkConfig, Networks};

use super::dns_server::{HELPER_ARG, IDLE_ENV, UPSTREAM_ENV};
use crate::Paths;
use crate::error::{Context as _, Result, bail};

/// How long a server may take to say it's ready: longer than it waits
/// for an address a stopping server holds.
const READY_DEADLINE: Duration = Duration::from_secs(5);

/// How long a run waits for the server's lock while others hold it a
/// moment (see `take`): longer than a server takes to notice its bridge
/// is gone.
const TAKE_WAIT: Duration = Duration::from_secs(3);

// --- The starter's side ----------------------------------------------

/// The memory a DNS server may use (its cgroup's `memory.max`).
const SERVER_MEMORY: u64 = 64 << 20;
/// The tasks a DNS server may have (its cgroup's `pids.max`): its worker's
/// most, its watchdog, and a few to spare (a worker started again while
/// the last is reaped).
const SERVER_TASKS: usize = shipyard_network::dns::server::MOST_TASKS + 1 + 4;

/// The file in a network's directory that names its DNS server's cgroup.
const CGROUP_FILE: &str = "dns.cgroup";

/// The most of [`CGROUP_FILE`] read: a cgroup's name, a few bytes.
const CGROUP_FILE_MAX: u64 = 4 * 1024;

/// How long an empty cgroup under `shipyard/dns` must have been there for
/// a server's start to remove it, as a network's left over: one made just
/// now is about to be joined.
const LEFT_OVER: Duration = Duration::from_secs(60);

/// The name of the cgroup the network `network`, in the directory `dir`,
/// has its DNS server in, under `shipyard/dns`: `<network>-<8 random hex
/// digits>`, made the first time and kept in [`CGROUP_FILE`]. A name of
/// the network's own wouldn't do: another store (`SHIPYARD_NETWORKS_DIR`)
/// can have a network of that name.
fn cgroup_name(dir: &Path, network: &str) -> std::result::Result<String, String> {
    if let Some(kept) = kept_cgroup_name(dir) {
        return Ok(kept);
    }
    let random = crate::random::bytes::<4>().map_err(|error| error.to_string())?;
    let made = format!("{network}-{}", crate::random::hex(&random));
    // (A name lost to a power cut is made again: nothing to say.)
    let _ = crate::fs::write_atomically(dir, CGROUP_FILE, made.as_bytes(), 0o600)
        .map_err(|error| error.to_string())?
        .unsynced();
    Ok(made)
}

/// The cgroup name kept in the network directory `dir`, if it's one: a
/// single path component.
fn kept_cgroup_name(dir: &Path) -> Option<String> {
    let kept = String::from_utf8(
        shipyard_sys::fs::read_bounded(&dir.join(CGROUP_FILE), CGROUP_FILE_MAX).ok()?,
    )
    .ok()?;
    let mut parts = Path::new(&kept).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(_)), None) => Some(kept),
        _ => None,
    }
}

/// The DNS server's cgroup for the network `network`, in the directory
/// `dir` (`shipyard/dns/<cgroup_name>`, made if it isn't there): its
/// `cgroup.procs`, open for the server to join as it starts. Its limits
/// are set too; if they can't be (a controller the kernel doesn't have),
/// that's said in `log`, and it's joined all the same. Root's tree only
/// (only root's networks have servers). On the way, other networks' empty
/// cgroups left over (their directories removed by hand) are removed.
fn own_cgroup(dir: &Path, network: &str, mut log: &File) -> std::result::Result<File, String> {
    use shipyard_runtime::cgroup::{Cgroup, DNS_PARENT, Tree};
    let tree = Tree::find().map_err(|why| why.to_string())?;
    let name = cgroup_name(dir, network)?;
    shipyard_runtime::cgroup::remove_empty_left_over(
        &tree.base().join(DNS_PARENT),
        &name,
        LEFT_OVER,
    );
    let relative = Path::new(DNS_PARENT).join(&name);
    let path = tree.base().join(&relative);
    let cgroup = if path.is_dir() {
        Cgroup::recorded(path, tree.base().to_owned()).map_err(|error| error.to_string())?
    } else {
        tree.create(&relative).map_err(|error| error.to_string())?
    };
    let mut limits = shipyard_runtime::resources::Resources::default();
    limits.memory_max = Some(SERVER_MEMORY);
    limits.pids_max = u64::try_from(SERVER_TASKS).ok();
    if let Err(error) = cgroup.limit(&limits) {
        let _ = writeln!(log, "shipyard-dns: its cgroup has no limits ({error})");
    }
    cgroup
        .procs_for_a_child()
        .map_err(|error| error.to_string())
}

/// Removes the DNS server's cgroup of the network in `dir` (its directory),
/// with what's left in it: the network is going. One that isn't there is
/// fine.
///
/// # Errors
///
/// It's still there: what's left in it couldn't be ended, or it couldn't
/// be removed.
pub(super) fn remove_cgroup(dir: &Path) -> std::result::Result<(), String> {
    use shipyard_runtime::cgroup::{Cgroup, DNS_PARENT, Tree};
    let (Some(name), Ok(tree)) = (kept_cgroup_name(dir), Tree::find()) else {
        return Ok(());
    };
    let path = tree.base().join(DNS_PARENT).join(name);
    match Cgroup::recorded(path, tree.base().to_owned()) {
        Ok(cgroup) => cgroup
            .remove(Duration::from_secs(2))
            .map_err(|error| error.to_string()),
        Err(_) => Ok(()),
    }
}

/// Makes sure the network `config`'s DNS server is running, starting it
/// if it isn't. Called by a run under the network's lock, once its lease
/// is held: a server stopping for want of containers checks under the
/// same lock, so it either sees the lease, or is gone (its own lock
/// free) before this looks.
pub(super) fn ensure(paths: &Paths, networks: &Networks, config: &NetworkConfig) -> Result<()> {
    let path = networks.dns_lock_path(&config.name);
    let lock = networks.open_dns_lock(&config.name)?;
    if !take(&lock, config).with_context(|| format!("locking {}", path.display()))? {
        return Ok(());
    }
    start(paths, networks, config, &lock)
        .with_context(|| format!("starting the network {}'s DNS server", config.name))
}

/// Takes the server's lock, `lock`, unless a server serving `config`'s
/// bridge holds it: whether it was taken. The lock is held for the
/// server to be: it's let go of only when every copy of it is closed
/// (std's lock, unlike nix's `Flock`, isn't let go of on drop).
///
/// Two things can hold it a moment, and are waited for (up to
/// [`TAKE_WAIT`]): `network ls` and `inspect`, which look with a shared
/// lock; and a server whose bridge was deleted (by hand) and made again
/// by this run, which it doesn't serve, and is about to leave.
fn take(lock: &File, config: &NetworkConfig) -> Result<bool> {
    let deadline = Instant::now() + TAKE_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        // Only a look, if a shared lock can be had: nobody holds it
        // exclusively.
        let looking = match lock.try_lock_shared() {
            Ok(()) => {
                lock.unlock()?;
                true
            }
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(error)) => return Err(error.into()),
        };
        if !looking {
            let bridge = nix::net::if_::if_nametoindex(config.bridge.as_str()).ok();
            if server::served_bridge(lock) == bridge {
                return Ok(false);
            }
        }
        if Instant::now() >= deadline {
            bail!(
                Network,
                "it was held for {}s by {}",
                TAKE_WAIT.as_secs(),
                if looking {
                    "other shipyard commands"
                } else {
                    "a server for a bridge that's gone"
                }
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Starts the server for `config`'s network, passing it `lock` (held),
/// and waits for its first line.
fn start(paths: &Paths, networks: &Networks, config: &NetworkConfig, lock: &File) -> Result<()> {
    let (from_server, server_out) =
        nix::unistd::pipe2(OFlag::O_CLOEXEC).context("making the DNS server's pipe")?;
    let dir = networks.dir().join(&config.name);
    let log_path = dir.join("dns.log");
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let passed = [lock.as_raw_fd(), server_out.as_raw_fd()];
    // Its cgroup: one that can't be had leaves it in the starter's (said
    // in its log, as everything it does).
    let cgroup = match own_cgroup(&dir, &config.name, &log) {
        Ok(procs) => Some(procs),
        Err(why) => {
            let _ = writeln!(
                &log,
                "shipyard-dns: not in a cgroup of its own ({why}): it ends with whatever \
                 started it"
            );
            None
        }
    };
    let procs = cgroup.as_ref().map(std::os::fd::AsRawFd::as_raw_fd);
    let mut command = Command::new("/proc/self/exe");
    // SAFETY: runs between `fork` and `exec`, and only makes system
    // calls (nix's `setsid` is the bare one; `write`, `sigemptyset`,
    // `sigprocmask`, `syscall` and `fcntl` allocate nothing and take no
    // lock), reading `errno`.
    unsafe {
        command.pre_exec(move || {
            // Into its own cgroup first, before it runs anything. If it
            // can't (the cgroup removed meanwhile, say), it runs where it
            // is, as without one: said in its log (its stderr, by now),
            // with a message that needs nothing allocated.
            const UNJOINED: &[u8] =
                b"shipyard-dns: its cgroup couldn't be joined: it ends with whatever started it\n";
            if let Some(procs) = procs
                && libc::write(procs, b"0".as_ptr().cast(), 1) != 1
            {
                libc::write(2, UNJOINED.as_ptr().cast(), UNJOINED.len());
            }
            // No signal blocked: a mask survives `fork` and `exec`, and a
            // detached run holds off Ctrl-C while it's set up.
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut none);
            if libc::sigprocmask(libc::SIG_SETMASK, &raw const none, std::ptr::null_mut()) == -1 {
                return Err(io::Error::last_os_error());
            }
            // A session of its own: no signal meant for the run's
            // terminal reaches it.
            nix::unistd::setsid()?;
            // Nothing it doesn't need: every descriptor this process
            // inherited from its own caller (a lock it holds, a pipe it
            // waits on) would be held for the server's whole life.
            // Close-on-exec for all (a kernel before 5.11 has no
            // `close_range`: they're left as they are)...
            let _ = libc::syscall(
                libc::SYS_close_range,
                3_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            );
            // ...but its two.
            for fd in passed {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    // Nor the run's environment: only what the server reads (`Paths` keeps
    // only those: asked again here, whatever made it).
    let environment = paths
        .dns_env()
        .iter()
        .filter(|(name, _)| [UPSTREAM_ENV, IDLE_ENV].iter().any(|kept| *name == **kept))
        .map(|(name, value)| (name, value));
    let mut child = command
        .arg0("shipyard-dns")
        .arg(HELPER_ARG)
        .arg(networks.dir())
        .arg(&config.name)
        .args(passed.map(|fd| fd.to_string()))
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .current_dir("/")
        .spawn()
        .context("starting it")?;
    drop((server_out, cgroup));
    let line = first_line(&from_server, &mut child, poll);
    match line {
        Ok(line) if line == "ready" => Ok(()),
        failed => {
            let _ = child.kill();
            let _ = child.wait();
            match failed {
                Ok(line) => bail!(Network, "{}", line.strip_prefix("error: ").unwrap_or(&line)),
                Err(error) => Err(error.context(format!("(see {})", log_path.display()))),
            }
        }
    }
}

/// The server's first line on `pipe`, waiting up to [`READY_DEADLINE`],
/// and no longer than it lives (`look` is `poll`: a test's may see
/// nothing).
fn first_line(
    pipe: &OwnedFd,
    child: &mut Child,
    mut look: impl FnMut(&mut [PollFd<'_>], PollTimeout) -> nix::Result<libc::c_int>,
) -> Result<String> {
    let deadline = Instant::now() + READY_DEADLINE;
    let mut line = Vec::new();
    // Once it's gone: what it said before it went (its error) is still
    // in the pipe, so it's read to the end first.
    let mut gone = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!(
                Network,
                "it didn't say it was ready in {}s",
                READY_DEADLINE.as_secs()
            );
        }
        let wait = PollTimeout::try_from(left.min(Duration::from_millis(100)))
            .unwrap_or(PollTimeout::ZERO);
        let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
        match look(&mut fds, wait) {
            Ok(0) | Err(Errno::EINTR) => {
                if let Some(status) = gone {
                    bail!(Network, "it stopped as it started ({status})");
                }
                gone = child.try_wait().context("checking on it")?;
                continue;
            }
            Ok(_) => {}
            Err(errno) => bail!(Network, "waiting for it: {}", errno.desc()),
        }
        let mut buffer = [0u8; 256];
        let read = match nix::unistd::read(pipe, &mut buffer) {
            Ok(read) => read,
            Err(Errno::EINTR) => continue,
            Err(errno) => bail!(Network, "reading from it: {}", errno.desc()),
        };
        let Some(bytes) = buffer.get(..read).filter(|bytes| !bytes.is_empty()) else {
            if let Some(status) = gone {
                bail!(Network, "it stopped as it started ({status})");
            }
            bail!(Network, "it stopped without a word");
        };
        line.extend_from_slice(bytes);
        if let Some(end) = line.iter().position(|&byte| byte == b'\n') {
            line.truncate(end);
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        if line.len() > 4096 {
            bail!(Network, "it said too much");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    /// A server that said why it can't start, then ended, between a look
    /// at its pipe that saw nothing and the check that it's still there:
    /// what it said is read (it's in the pipe), not "stopped as it
    /// started".
    #[test]
    fn what_a_gone_server_said_is_read() {
        let (from, to) = nix::unistd::pipe2(OFlag::O_CLOEXEC).unwrap();
        let mut server = Command::new("sh")
            .args(["-c", "echo 'error: no address'; exit 1"])
            .stdout(Stdio::from(to))
            .spawn()
            .unwrap();
        server.wait().unwrap();
        let mut first = true;
        let look = |fds: &mut [PollFd<'_>], wait: PollTimeout| {
            if std::mem::take(&mut first) {
                return Ok(0);
            }
            poll(fds, wait)
        };
        assert_eq!(
            first_line(&from, &mut server, look).unwrap(),
            "error: no address"
        );
    }
}
