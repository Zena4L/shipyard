//! A network's DNS server's side: this binary, re-executed as `shipyard
//! __shipyard-dns` by a run (`network::dns` starts it), serving until its
//! network has no container for a while, or its bridge is gone (Linux). See
//! `shipyard_network::dns::server` for what it answers and how it lives.
//!
//! Its arguments are the networks' directory, the network's name, and the
//! descriptors of its lock (held, passed on by the starter) and of its
//! ready pipe.
//!
//! Set up (its sockets bound, root given up), it's two processes (stage 9):
//! a watchdog, and the worker it forks that serves. A worker that's killed,
//! or fails, is started again, after a wait that grows; one that ends
//! because its network is idle, or gone, ends the watchdog too.

/// The hidden first argument the server is started with.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a network's DNS server is Linux's")
)]
pub(super) const HELPER_ARG: &str = "__shipyard-dns";

/// Upstream resolvers to use instead of the host's (`IP[:PORT],…`): for
/// tests.
pub(crate) const UPSTREAM_ENV: &str = "SHIPYARD_DNS_UPSTREAM";

/// How many seconds a network may have no container before its server
/// stops, instead of `shipyard_network::dns::server::IDLE`: for tests.
pub(crate) const IDLE_ENV: &str = "SHIPYARD_DNS_IDLE";

/// Runs this process as a network's DNS server, if that's what it was
/// started as; otherwise returns at once. Called first thing in `main`.
#[cfg(not(target_os = "linux"))]
pub fn entry_point() {}

#[cfg(target_os = "linux")]
pub use linux::entry_point;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write as _};
    use std::net::SocketAddr;
    use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::Path;
    use std::time::Duration;

    use shipyard_network::dns::answer::Zone;
    use shipyard_network::dns::server::{self, Ending, Setup, Sockets, Upstreams};
    use shipyard_network::store::{LeaseDir, Networks, NetworksDir};

    use super::{HELPER_ARG, IDLE_ENV, UPSTREAM_ENV};
    use crate::error::{Context as _, Error, ErrorKind, Result};

    /// Runs this process as a network's DNS server, if that's what it was
    /// started as; otherwise returns at once. Called first thing in `main`.
    pub fn entry_point() {
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
        // Two different descriptors: one number twice would be owned, and
        // closed, twice.
        let number = |text: &OsString| text.to_str().and_then(|text| text.parse::<RawFd>().ok());
        if number(lock).is_some() && number(lock) == number(ready) {
            let _ = writeln!(
                io::stderr(),
                "shipyard-dns: started without its descriptors"
            );
            return 2;
        }
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
            .ok_or_else(|| Error::new(ErrorKind::InvalidRequest, "the network's name isn't UTF-8"))
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
        watch(setup, sockets)
    }

    /// The first wait before a worker is started again.
    const FIRST_BACKOFF: Duration = Duration::from_millis(100);
    /// The longest wait.
    const MOST_BACKOFF: Duration = Duration::from_secs(30);
    /// How long a worker must serve for its end to start the waits afresh.
    const STEADY: Duration = Duration::from_secs(60);
    /// How many workers in a row may end without serving [`STEADY`]ly
    /// before the watchdog gives up (about seven minutes of them): it lets
    /// go of the network, and the next run starts a server afresh, saying
    /// why it fails if it does.
    const MOST_FAILURES: u32 = 20;

    /// The waits before a worker is started again: 100 ms, doubling, at
    /// most 30 s; afresh after a worker that served a minute. Pure, so
    /// every case is tested.
    #[derive(Debug)]
    struct Backoff {
        /// The next wait.
        next: Duration,
        /// The workers that ended without serving steadily, in a row.
        failures: u32,
    }

    impl Default for Backoff {
        fn default() -> Self {
            Self {
                next: FIRST_BACKOFF,
                failures: 0,
            }
        }
    }

    impl Backoff {
        /// After a worker that served for `served` ended (killed, or
        /// failing): how long to wait before the next, or `None` to give
        /// up.
        fn after(&mut self, served: Duration) -> Option<Duration> {
            if served >= STEADY {
                *self = Self::default();
            }
            self.failures += 1;
            if self.failures > MOST_FAILURES {
                return None;
            }
            let wait = self.next;
            self.next = (self.next * 2).min(MOST_BACKOFF);
            Some(wait)
        }
    }

    /// The server's watchdog (this process, set up: its sockets bound,
    /// root given up). Serving is a worker's, forked from it, which
    /// inherits all of it and dies with it (`PR_SET_PDEATHSIG`): a worker
    /// that ends cleanly (its network idle, or its bridge gone) ends this
    /// too; one that's killed, or fails, is started again after a
    /// [`Backoff`], said in the server's log. This keeps the server's lock
    /// meanwhile (taken again on its own copy, in case the worker let go of
    /// it as it was killed): no run starts another.
    fn watch(setup: Setup, sockets: Sockets) -> i32 {
        use nix::sys::signal::{SigHandler, Signal};
        use nix::sys::wait::{WaitStatus, waitpid};
        use nix::unistd::ForkResult;
        // A disposition of `SIG_IGN` survives `exec`: inherited, the
        // kernel would reap the workers itself, and `waitpid` could never
        // tell a clean end from a crash.
        // SAFETY: the default disposition runs no handler.
        if let Err(errno) = unsafe { nix::sys::signal::signal(Signal::SIGCHLD, SigHandler::SigDfl) }
        {
            let _ = writeln!(io::stderr(), "shipyard-dns: SIGCHLD: {errno}");
            return 1;
        }
        let (bridge, index) = (setup.bridge.clone(), sockets.bridge_index());
        let bridge_is_there = || nix::net::if_::if_nametoindex(bridge.as_str()).ok() == Some(index);
        let watchdog = nix::unistd::getpid();
        let mut kept = Some((setup, sockets));
        let mut backoff = Backoff::default();
        loop {
            let began = std::time::Instant::now();
            // SAFETY: this process is single-threaded (the threads are the
            // worker's, started after the fork), so the child may run
            // ordinary Rust.
            let child = match unsafe { nix::unistd::fork() } {
                Ok(ForkResult::Child) => {
                    // With the watchdog, or not at all: an orphan would
                    // serve unwatched, holding the lock (or be reaped by a
                    // run's subreaper, killing what it left).
                    let ours = nix::sys::prctl::set_pdeathsig(Signal::SIGKILL).is_ok()
                        && nix::unistd::getppid() == watchdog;
                    // (`kept` is always there in a worker: only the
                    // watchdog's loop, never a worker, takes it.)
                    let Some((setup, sockets)) = kept.take().filter(|_| ours) else {
                        std::process::exit(1);
                    };
                    std::process::exit(work(setup, sockets));
                }
                Ok(ForkResult::Parent { child }) => child,
                Err(errno) => {
                    let _ = writeln!(io::stderr(), "shipyard-dns: no worker ({errno})");
                    return 1;
                }
            };
            let how = loop {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, 0)) => return 0,
                    Ok(WaitStatus::Exited(_, code)) => break format!("ended (code {code})"),
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        break format!("was killed ({signal})");
                    }
                    // Stopped, continued: it's still there.
                    Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                    Err(errno) => {
                        let _ = writeln!(
                            io::stderr(),
                            "shipyard-dns: its worker can't be waited for ({errno})"
                        );
                        return 1;
                    }
                }
            };
            // Its network removed meanwhile: nothing to serve.
            if !bridge_is_there() {
                return 0;
            }
            let Some(wait) = backoff.after(began.elapsed()) else {
                let _ = writeln!(
                    io::stderr(),
                    "shipyard-dns: its worker {how}, {MOST_FAILURES} times in a row: given up"
                );
                return 1;
            };
            if let Some((setup, _)) = &kept {
                // The server's lock: the worker may have let go of it as it
                // was killed (stopping), on the open file this shares, and
                // another server have it now. Then this goes, its sockets
                // with it, for that one to bind.
                // SAFETY: `flock` on a descriptor this owns, open.
                let held = unsafe {
                    libc::flock(setup.dns_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
                };
                if held != 0 {
                    let _ = writeln!(
                        io::stderr(),
                        "shipyard-dns: its worker {how}, and another server has the network"
                    );
                    return 0;
                }
                // The network's lock, if it died holding it (checking
                // whether it's idle): on the open file this shares, it
                // would stay held, every run on the network waiting. Runs
                // lock open files of their own: this lets go of only the
                // server's hold, if any.
                // SAFETY: `flock` on a descriptor this owns, open.
                unsafe { libc::flock(setup.network_lock.as_raw_fd(), libc::LOCK_UN) };
            }
            let _ = writeln!(
                io::stderr(),
                "shipyard-dns: its worker {how}: started again in {} ms",
                wait.as_millis()
            );
            std::thread::sleep(wait);
            if !bridge_is_there() {
                return 0;
            }
        }
    }

    /// A worker's life: it serves until its network is idle (exit 0) or its
    /// bridge is gone (exit 0); a failure is 1. Idle, it exits at once,
    /// holding the network's lock, never returning: a return would drop the
    /// lock first, letting a run in before the server is gone. (The
    /// watchdog's copy goes as it exits too, straight after.)
    fn work(setup: Setup, sockets: Sockets) -> i32 {
        match server::serve(setup, sockets) {
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
        // Close-on-exec, now it's here.
        // SAFETY: the starter passed it for this process to own, a number
        // apart from the other's (checked in `serve`), and nothing else here
        // uses it.
        unsafe { shipyard_sys::fd::take_inherited(fd, shipyard_sys::fd::OnExec::Close) }.ok()
    }

    /// Everything the server needs, opened while it's root: then its
    /// sockets are bound, and root is given up.
    fn set_up(dir: &Path, name: &str, dns_lock: File) -> Result<(Setup, Sockets)> {
        let networks = Networks::open(dir)?;
        let config = networks.get(name)?.ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("there's no network named {name}"),
            )
        })?;
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
                    .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
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
                    .map_err(|_| {
                        Error::new(
                            ErrorKind::InvalidRequest,
                            format!("{UPSTREAM_ENV}: {item:?} isn't an address"),
                        )
                    })
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

        /// The waits double from 100 ms to 30 s, start afresh after a minute
        /// of service, and give out after twenty quick ends in a row.
        #[test]
        fn backoff_doubles_resets_after_a_minute_and_gives_up() {
            let quick = Duration::from_secs(1);
            let mut backoff = Backoff::default();
            let waits: Vec<u128> = (0..10)
                .map(|_| backoff.after(quick).unwrap().as_millis())
                .collect();
            assert_eq!(
                waits,
                [100, 200, 400, 800, 1600, 3200, 6400, 12800, 25600, 30000]
            );
            assert_eq!(backoff.after(quick), Some(MOST_BACKOFF));
            // A steady worker: afresh.
            assert_eq!(backoff.after(STEADY), Some(FIRST_BACKOFF));
            assert_eq!(
                backoff.after(Duration::from_secs(59)),
                Some(FIRST_BACKOFF * 2)
            );
            // Twenty quick ends in a row, then no more.
            let mut backoff = Backoff::default();
            for _ in 0..MOST_FAILURES {
                assert!(backoff.after(quick).is_some());
            }
            assert_eq!(backoff.after(quick), None);
            // A steady one in between starts the count again.
            assert_eq!(backoff.after(STEADY), Some(FIRST_BACKOFF));
        }
    }
}
