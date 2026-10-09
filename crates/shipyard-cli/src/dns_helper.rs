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
//! - its own session (`setsid`): a Ctrl-C meant for the run doesn't stop it.
//!
//! It's started before the run's supervisor is installed, so the supervisor
//! counts it as a child it inherited, never one the command left behind:
//! it outlives the run.

#[cfg(target_os = "linux")]
pub(crate) use linux::*;

/// Runs this process as a network's DNS server, if that's what it was
/// started as; otherwise returns at once. Called first thing in `main`.
#[cfg(not(target_os = "linux"))]
pub(crate) fn entry_point() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs::TryLockError;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write as _};
    use std::net::SocketAddr;
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result, anyhow, bail};
    use nix::errno::Errno;
    use nix::fcntl::OFlag;
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use shipyard_network::dns::answer::Zone;
    use shipyard_network::dns::server::{self, Ending, Setup, Sockets, Upstreams};
    use shipyard_network::store::{LeaseDir, NetworkConfig, Networks, NetworksDir};

    /// The hidden first argument the server is started with.
    const HELPER_ARG: &str = "__shipyard-dns";

    /// How long a server may take to say it's ready: longer than it waits
    /// for an address a stopping server holds.
    const READY_DEADLINE: Duration = Duration::from_secs(5);

    /// How long a run waits for the server's lock while others hold it a
    /// moment (see `take`): longer than a server takes to notice its bridge
    /// is gone.
    const TAKE_WAIT: Duration = Duration::from_secs(3);

    /// Upstream resolvers to use instead of the host's (`IP[:PORT],…`): for
    /// tests.
    const UPSTREAM_ENV: &str = "SHIPYARD_DNS_UPSTREAM";

    /// How many seconds a network may have no container before its server
    /// stops, instead of [`server::IDLE`]: for tests.
    const IDLE_ENV: &str = "SHIPYARD_DNS_IDLE";

    // --- The starter's side ----------------------------------------------

    /// Makes sure the network `config`'s DNS server is running, starting it
    /// if it isn't. Called by a run under the network's lock, once its lease
    /// is held: a server stopping for want of containers checks under the
    /// same lock, so it either sees the lease, or is gone (its own lock
    /// free) before this looks.
    pub(crate) fn ensure(networks: &Networks, config: &NetworkConfig) -> Result<()> {
        let path = networks.dns_lock_path(&config.name);
        let lock = networks.open_dns_lock(&config.name)?;
        if !take(&lock, config).with_context(|| format!("locking {}", path.display()))? {
            return Ok(());
        }
        start(networks, config, &lock)
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
    fn start(networks: &Networks, config: &NetworkConfig, lock: &File) -> Result<()> {
        let (from_server, server_out) =
            nix::unistd::pipe2(OFlag::O_CLOEXEC).context("making the DNS server's pipe")?;
        let log_path = networks.dir().join(&config.name).join("dns.log");
        let log = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let passed = [lock.as_raw_fd(), server_out.as_raw_fd()];
        let mut command = Command::new("/proc/self/exe");
        // SAFETY: runs between `fork` and `exec`, and only makes system
        // calls (nix's `setsid` is the bare one; `syscall` and `fcntl`
        // allocate nothing and take no lock), reading `errno`.
        unsafe {
            command.pre_exec(move || {
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
        // Nor the run's environment: only what the server reads.
        let environment = std::env::vars_os().filter(|(name, _)| {
            [UPSTREAM_ENV, IDLE_ENV]
                .iter()
                .any(|kept| name.as_os_str() == OsStr::new(kept))
        });
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
        drop(server_out);
        let line = first_line(&from_server, &mut child);
        match line {
            Ok(line) if line == "ready" => Ok(()),
            failed => {
                let _ = child.kill();
                let _ = child.wait();
                match failed {
                    Ok(line) => bail!("{}", line.strip_prefix("error: ").unwrap_or(&line)),
                    Err(error) => Err(error.context(format!("(see {})", log_path.display()))),
                }
            }
        }
    }

    /// The server's first line on `pipe`, waiting up to [`READY_DEADLINE`],
    /// and no longer than it lives.
    fn first_line(pipe: &OwnedFd, child: &mut Child) -> Result<String> {
        let deadline = Instant::now() + READY_DEADLINE;
        let mut line = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "it didn't say it was ready in {}s",
                    READY_DEADLINE.as_secs()
                );
            }
            let wait = PollTimeout::try_from(left.min(Duration::from_millis(100)))
                .unwrap_or(PollTimeout::ZERO);
            let mut fds = [PollFd::new(pipe.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, wait) {
                Ok(0) | Err(Errno::EINTR) => {
                    if let Some(status) = child.try_wait().context("checking on it")? {
                        bail!("it stopped as it started ({status})");
                    }
                    continue;
                }
                Ok(_) => {}
                Err(errno) => bail!("waiting for it: {}", errno.desc()),
            }
            let mut buffer = [0u8; 256];
            let read = match nix::unistd::read(pipe, &mut buffer) {
                Ok(read) => read,
                Err(Errno::EINTR) => continue,
                Err(errno) => bail!("reading from it: {}", errno.desc()),
            };
            let Some(bytes) = buffer.get(..read).filter(|bytes| !bytes.is_empty()) else {
                bail!("it stopped without a word");
            };
            line.extend_from_slice(bytes);
            if let Some(end) = line.iter().position(|&byte| byte == b'\n') {
                line.truncate(end);
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }
            if line.len() > 4096 {
                bail!("it said too much");
            }
        }
    }

    // --- The server's side -----------------------------------------------

    /// Runs this process as a network's DNS server, if that's what it was
    /// started as; otherwise returns at once. Called first thing in `main`.
    pub(crate) fn entry_point() {
        let mut args = std::env::args_os();
        if args.nth(1).as_deref() != Some(OsStr::new(HELPER_ARG)) {
            return;
        }
        let args: Vec<OsString> = args.collect();
        // As for the CLI itself: a privileged start (setuid, file
        // capabilities) gets no further. Anyone could start it, with a
        // directory and resolvers of their choosing.
        if let Err(error) = shipyard_runtime::process::refuse_privileged_start() {
            let _ = writeln!(io::stderr(), "shipyard-dns: {error}");
            std::process::exit(125);
        }
        std::process::exit(serve(&args));
    }

    /// The server's `main`: its arguments are the networks' directory, the
    /// network's name, and the descriptors of its lock and of its ready
    /// pipe.
    fn serve(args: &[OsString]) -> i32 {
        let [dir, name, lock, ready] = args else {
            let _ = writeln!(
                io::stderr(),
                "shipyard-dns: started without its four arguments"
            );
            return 2;
        };
        let (Some(lock), Some(ready)) = (inherited(lock), inherited(ready)) else {
            let _ = writeln!(
                io::stderr(),
                "shipyard-dns: started without its descriptors"
            );
            return 2;
        };
        let mut ready = File::from(ready);
        let started = name
            .to_str()
            .context("the network's name isn't UTF-8")
            .and_then(|name| set_up(Path::new(dir), name, File::from(lock)));
        let (setup, sockets) = match started {
            Ok(started) => started,
            Err(error) => {
                let _ = writeln!(ready, "error: {error:#}");
                return 1;
            }
        };
        if writeln!(ready, "ready").is_err() {
            return 1;
        }
        drop(ready);
        match server::serve(setup, sockets) {
            // With the network's lock still held: it goes as the process
            // does, after everything else.
            Ok(Ending::Idle(_held)) => std::process::exit(0),
            Ok(Ending::BridgeGone) => 0,
            Err(error) => {
                let _ = writeln!(io::stderr(), "shipyard-dns: {error}");
                1
            }
        }
    }

    /// The descriptor numbered `text`, inherited from the starter, if it's
    /// open.
    fn inherited(text: &OsStr) -> Option<OwnedFd> {
        let fd: RawFd = text.to_str()?.parse().ok()?;
        if fd <= 2 {
            return None;
        }
        // SAFETY: `F_GETFD` only reads the descriptor's flags; an fd that
        // isn't open is EBADF.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            return None;
        }
        // SAFETY: the starter passed it for this process to own, and nothing
        // else here uses it (it's open, as just checked).
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // Close-on-exec, now it's here.
        nix::fcntl::fcntl(
            &fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )
        .ok()?;
        Some(fd)
    }

    /// Everything the server needs, opened while it's root: then its
    /// sockets are bound, and root is given up.
    fn set_up(dir: &Path, name: &str, dns_lock: File) -> Result<(Setup, Sockets)> {
        let networks = Networks::open(dir)?;
        let config = networks
            .get(name)?
            .ok_or_else(|| anyhow!("there's no network named {name}"))?;
        let leases = LeaseDir::open(&networks, name)?;
        let all = NetworksDir::open(&networks)?;
        let lock_path = dir.join(name).join("lock");
        let network_lock = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        let upstreams = match std::env::var(UPSTREAM_ENV) {
            Ok(list) if !list.is_empty() => Upstreams::Fixed(parse_upstreams(&list)?),
            _ => Upstreams::Host,
        };
        let idle = match std::env::var(IDLE_ENV) {
            Ok(seconds) => Duration::from_secs(
                seconds
                    .parse()
                    .with_context(|| format!("{IDLE_ENV}={seconds}"))?,
            ),
            Err(_) => server::IDLE,
        };
        let sockets = server::bind(&config)?;
        // Which bridge it serves, before it says it's ready (see `take`).
        server::record_bridge(&dns_lock, sockets.bridge_index())
            .with_context(|| format!("writing {}", networks.dns_lock_path(name).display()))?;
        server::drop_privileges()?;
        let setup = Setup {
            zone: Zone {
                subnet: config.subnet,
                subnet6: config.subnet6,
                internal: config.internal,
            },
            gateway: config.gateway,
            bridge: config.bridge,
            name: name.to_owned(),
            leases,
            networks: all,
            network_lock,
            dns_lock,
            idle,
            upstreams,
        };
        Ok((setup, sockets))
    }

    /// [`UPSTREAM_ENV`]'s list: `IP` or `IP:PORT` (`[IP]:PORT` for IPv6),
    /// separated by commas.
    fn parse_upstreams(list: &str) -> Result<Vec<SocketAddr>> {
        list.split(',')
            .map(|item| {
                let item = item.trim();
                item.parse::<SocketAddr>()
                    .or_else(|_| item.parse().map(|ip| SocketAddr::new(ip, 53)))
                    .map_err(|_| anyhow!("{UPSTREAM_ENV}: {item:?} isn't an address"))
            })
            .collect()
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "tests")]
    mod tests {
        use super::*;

        #[test]
        fn upstreams_for_tests_have_port_53_unless_given() {
            assert_eq!(
                parse_upstreams("192.0.2.2, 192.0.2.3:5353,[::1]:53").unwrap(),
                ["192.0.2.2:53", "192.0.2.3:5353", "[::1]:53"].map(|addr| addr.parse().unwrap())
            );
            assert!(parse_upstreams("example.com").is_err());
        }
    }
}
