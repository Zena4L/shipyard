//! Rootless networking with pasta (passt's namespace mode): one per
//! container.
//!
//! A rootless container can't have a bridge and a veth pair: those are the
//! host's devices, and changing them needs root. pasta gives it a network
//! another way. It runs in the container's user namespace, joins its network
//! namespace, makes a tap device there, and gives it the host's own address
//! and routes (`--config-net`). Every connection the container makes, pasta makes again
//! from the host with an ordinary socket, and copies the data across
//! (*splicing*, not routing): to the world, the traffic is the user's own
//! programs'. A published port is a socket pasta listens on, on the host,
//! whose connections it carries into the container.
//!
//! What it's never given (pasta's defaults would):
//!
//! - `-T none -U none`: no ports of the host's own namespace forwarded into
//!   the container (with them, the container would reach the host's loopback
//!   services through its own 127.0.0.1);
//! - `--no-map-gw`: the gateway's address isn't the host's loopback either;
//! - `-t none -u none` unless ports are published: the host's ports aren't
//!   all forwarded in (a newer pasta's default).
//!
//! IPv4 and IPv6, as the host has them: pasta copies the host's addresses
//! and routes of each family, and publishes a port on every address on
//! both.
//!
//! DNS goes to `169.254.1.1`, which pasta forwards to the host's resolver
//! (`--dns-forward`): even one only the host reaches, like systemd's
//! `127.0.0.53`.
//!
//! **How pasta gets the container (stage 8.1c, F32).** Not by its init's
//! PID: under SELinux, Fedora confines pasta (`pasta_t`), which may not open
//! another process's `/proc/<pid>/ns`. A small launcher (this binary again,
//! [`LAUNCHER_ARG`]) joins the container's user namespace, makes a mount
//! namespace of its own, bind-mounts the container's network namespace onto
//! a file in the run's directory ([`NETNS_FILE`]), and starts pasta with
//! `--netns FILE`: pasta stays in the user namespace it starts in (no
//! `--userns` is given), and opens a namespace file, as with Podman. The
//! bind mount is the launcher's mount namespace's alone (propagation
//! private): the host never sees it. The launcher stays, as pasta's parent:
//! under SELinux, pasta can't keep a parent-death signal of its own (see
//! `watch_over_pasta`). The run's directory is never under `~/.local`, which
//! `pasta_t` may not reach either.

use std::path::{Path, PathBuf};

use crate::ports::{Protocol, Published};

/// The address the container's resolver is told to ask (pasta forwards
/// it to the host's resolver).
pub const DNS_ADDRESS: std::net::Ipv4Addr = std::net::Ipv4Addr::new(169, 254, 1, 1);

/// The name of pasta's process ID file, in its run's directory: written
/// once the container's network is set up.
pub const PID_FILE: &str = "pasta.pid";

/// The name of pasta's log (its own error messages).
pub const LOG_FILE: &str = "pasta.log";

/// The name of the file the container's network namespace is bind-mounted
/// on, for pasta alone (see the module).
pub const NETNS_FILE: &str = "netns";

/// The hidden first argument pasta's launcher is started with (see the
/// module): the binary that starts pasta calls [`entry_point`] first thing.
pub const LAUNCHER_ARG: &str = "__shipyard-pasta";

/// What can go wrong with pasta.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PastaError {
    /// It couldn't be started.
    #[error("starting pasta ({path}): {source}")]
    Spawn {
        /// Where it is.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// It ended, or never said it was ready.
    #[error("pasta couldn't set up the container's network: {0}")]
    Failed(String),
    /// One of its files (its log, in the run's directory) couldn't be made.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// What the installed pasta can do (its options differ by version).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    /// `-I`, `--ns-ifname`: the container's interface's name.
    pub ns_ifname: bool,
    /// `--no-netns-quit`: given a namespace file, pasta otherwise watches
    /// its directory and quits when the file goes (Shipyard ends pasta
    /// itself, and a file removed under a live run mustn't cut its
    /// container off).
    pub no_netns_quit: bool,
}

impl Features {
    /// The features `help` (pasta's `--help`) lists.
    #[must_use]
    pub fn from_help(help: &str) -> Features {
        Features {
            ns_ifname: help.contains("--ns-ifname"),
            no_netns_quit: help.contains("--no-netns-quit"),
        }
    }

    /// What `pasta` can do, from its `--help` (on stdout or stderr,
    /// depending on the version): asked once, when a run is checked, never
    /// while a container waits for its network.
    pub fn of(pasta: &Path) -> Result<Features, PastaError> {
        let help = std::process::Command::new(pasta)
            .arg("--help")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|source| PastaError::Spawn {
                path: pasta.to_owned(),
                source,
            })?;
        let mut text = String::from_utf8_lossy(&help.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&help.stderr));
        Ok(Features::from_help(&text))
    }
}

/// Where programs are looked for without a `PATH` (as `execvp` does).
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// pasta, where `PATH` (`path`, as the environment variable says; the
/// usual directories without one) has it: an absolute path (an empty or
/// relative entry, the current directory's, doesn't count).
#[must_use]
pub fn find(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path.unwrap_or(std::ffi::OsStr::new(DEFAULT_PATH));
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("pasta"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// pasta's arguments for the container whose network namespace is
/// bind-mounted on `netns_file`, publishing `ports`, with its process ID file
/// at `pid_file` (see the module).
#[must_use]
pub fn arguments(
    netns_file: &Path,
    ports: &[Published],
    pid_file: &Path,
    features: Features,
) -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<std::ffi::OsString> = [
        "--foreground",
        "--config-net",
        "--quiet",
        "-T",
        "none",
        "-U",
        "none",
        "--no-map-gw",
        "--dns-forward",
    ]
    .iter()
    .map(Into::into)
    .collect();
    arguments.push(DNS_ADDRESS.to_string().into());
    if features.ns_ifname {
        arguments.extend(["-I".into(), "eth0".into()]);
    }
    if features.no_netns_quit {
        arguments.push("--no-netns-quit".into());
    }
    for (flag, protocol) in [("-t", Protocol::Tcp), ("-u", Protocol::Udp)] {
        let mut any = false;
        for port in ports.iter().filter(|port| port.protocol == protocol) {
            any = true;
            let address = port.host_ip.map(|ip| format!("{ip}/")).unwrap_or_default();
            arguments.push(flag.into());
            arguments.push(format!("{address}{}:{}", port.host_port, port.container_port).into());
        }
        if !any {
            arguments.extend([flag.into(), "none".into()]);
        }
    }
    arguments.push("--pid".into());
    arguments.push(pid_file.into());
    // Its network namespace alone: no `--userns`, so pasta stays in the user
    // namespace the launcher put it in (`--netns-only` is implied).
    arguments.push("--netns".into());
    arguments.push(netns_file.into());
    arguments
}

/// Makes the file the namespace is bind-mounted on (see the module): new,
/// never one that's there already, nor through a link someone left.
#[cfg(any(target_os = "linux", test))]
fn make_netns_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map(drop)
}

/// What the launcher is told: the container's user and network namespaces
/// (descriptors it inherited), the file to bind-mount the latter on, and
/// pasta and its arguments.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    /// The container's user namespace.
    pub(crate) userns: i32,
    /// The container's network namespace.
    pub(crate) netns: i32,
    /// The file it's bind-mounted on.
    pub(crate) netns_file: PathBuf,
    /// pasta itself.
    pub(crate) pasta: PathBuf,
    /// pasta's arguments ([`arguments`]).
    pub(crate) arguments: Vec<std::ffi::OsString>,
}

#[cfg(any(target_os = "linux", test))]
impl Launch {
    /// The launcher's command line after [`LAUNCHER_ARG`].
    #[must_use]
    pub(crate) fn to_args(&self) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec![
            self.userns.to_string().into(),
            self.netns.to_string().into(),
            self.netns_file.clone().into(),
            self.pasta.clone().into(),
            "--".into(),
        ];
        args.extend(self.arguments.iter().cloned());
        args
    }

    /// A launch from its command line (after [`LAUNCHER_ARG`]), or what's
    /// wrong with it.
    pub(crate) fn from_args(args: &[std::ffi::OsString]) -> Result<Launch, String> {
        let fd = |arg: Option<&std::ffi::OsString>, what: &str| {
            arg.and_then(|arg| arg.to_str())
                .and_then(|text| text.parse::<i32>().ok())
                .filter(|fd| *fd > 2)
                .ok_or_else(|| format!("no {what} descriptor"))
        };
        let path = |arg: Option<&std::ffi::OsString>, what: &str| {
            arg.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| format!("no absolute {what}"))
        };
        let userns = fd(args.first(), "user namespace")?;
        let netns = fd(args.get(1), "network namespace")?;
        if netns == userns {
            return Err("the two namespaces are one descriptor".to_owned());
        }
        let netns_file = path(args.get(2), "namespace file")?;
        let pasta = path(args.get(3), "pasta")?;
        if args.get(4).map(std::ffi::OsString::as_os_str) != Some(std::ffi::OsStr::new("--")) {
            return Err("no `--` before pasta's arguments".to_owned());
        }
        Ok(Launch {
            userns,
            netns,
            netns_file,
            pasta,
            arguments: args.get(5..).unwrap_or_default().to_vec(),
        })
    }
}

/// Where pasta's launcher takes over, if this process is one: never
/// returns then. Call it first thing in `main`, before any thread starts
/// (joining a user namespace needs a single-threaded process).
/// `refuse_privileged_start` is the binary's own check: a launcher started
/// setuid or with file capabilities gets no further.
#[cfg(target_os = "linux")]
pub fn entry_point(refuse_privileged_start: impl FnOnce() -> Result<(), String>) {
    let mut args = std::env::args_os();
    if args.nth(1).as_deref() != Some(std::ffi::OsStr::new(LAUNCHER_ARG)) {
        return;
    }
    let args: Vec<std::ffi::OsString> = args.collect();
    // Only returns on failure (the launcher becomes pasta otherwise).
    let Err(error) = refuse_privileged_start()
        .and_then(|()| Launch::from_args(&args))
        .and_then(|launch| linux::launch(&launch));
    // pasta's log: Shipyard reports it.
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        format!("shipyard's pasta launcher: {error}\n").as_bytes(),
    );
    std::process::exit(125);
}

/// Elsewhere there's no pasta.
#[cfg(not(target_os = "linux"))]
pub fn entry_point(_refuse_privileged_start: impl FnOnce() -> Result<(), String>) {}

#[cfg(target_os = "linux")]
pub use linux::Pasta;

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{
        Features, LAUNCHER_ARG, LOG_FILE, Launch, NETNS_FILE, PID_FILE, PastaError, arguments,
    };
    use crate::ports::Published;

    /// The initial user namespace's inode number (`PROC_USER_INIT_INO`):
    /// the host's.
    const INITIAL_USER_NAMESPACE: u64 = 0xEFFF_FFFD;

    /// How long pasta has to set the container's network up.
    const READY_WAIT: Duration = Duration::from_secs(5);
    /// How long a killed pasta has to end before it's left to the
    /// supervisor's sweep.
    const REAP_WAIT: Duration = Duration::from_secs(1);

    /// A pasta serving a container: killed when dropped.
    #[derive(Debug)]
    pub struct Pasta {
        pid: libc::pid_t,
        /// A pidfd: signals reach this pasta even once its PID is reused
        /// (the supervisor reaps it, if it dies, without telling us).
        /// `None` before Linux 5.3: then the PID is used.
        pidfd: Option<OwnedFd>,
    }

    impl Pasta {
        /// Starts `pasta` (which can do `features`) for the container whose
        /// init is `pid`, publishing `ports`, with its files in `dir`, and
        /// waits until it has set the container's network up. Through the
        /// launcher (see the module): this binary must call
        /// [`super::entry_point`] first thing in `main`.
        ///
        /// Call it from shipyard's main thread: pasta is killed when the
        /// thread that started it ends (`PR_SET_PDEATHSIG`).
        pub fn start(
            pasta: &Path,
            features: Features,
            pid: libc::pid_t,
            ports: &[Published],
            dir: &Path,
        ) -> Result<Pasta, PastaError> {
            let io = |context: String| move |source| PastaError::Io { context, source };
            let pid_file = dir.join(PID_FILE);
            let log_path = dir.join(LOG_FILE);
            let netns_file = dir.join(NETNS_FILE);
            let log =
                File::create(&log_path).map_err(io(format!("making {}", log_path.display())))?;
            super::make_netns_file(&netns_file)
                .map_err(io(format!("making {}", netns_file.display())))?;
            // The container's namespaces, opened here (Shipyard may: pasta,
            // confined, may not), for the launcher to inherit.
            let open = |kind: &str| {
                let path = format!("/proc/{pid}/ns/{kind}");
                File::open(&path).map_err(io(format!("opening {path}")))
            };
            let userns = open("user")?;
            let netns = open("net")?;
            let launch = Launch {
                userns: userns.as_raw_fd(),
                netns: netns.as_raw_fd(),
                netns_file: netns_file.clone(),
                pasta: pasta.to_owned(),
                arguments: arguments(&netns_file, ports, &pid_file, features),
            };
            let passed = [launch.userns, launch.netns];
            let parent = std::process::id();
            let mut command = Command::new("/proc/self/exe");
            command
                .arg0("shipyard-pasta")
                .arg(LAUNCHER_ARG)
                .args(launch.to_args())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                // A process group of its own: the terminal's signals (Ctrl-C)
                // are the container's to handle, not pasta's to die of while
                // the container stops.
                .process_group(0);
            // SAFETY: only async-signal-safe calls between fork and exec:
            // `prctl`, `getppid`, `syscall` and `fcntl`, then `_exit`.
            unsafe {
                command.pre_exec(move || {
                    // pasta dies with shipyard, even killed outright: it
                    // never quits by itself (an older one) while the
                    // container's namespace is held. (The launcher sets it
                    // again after joining the user namespace.)
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Shipyard died before the line above: it's too late.
                    if u32::try_from(libc::getppid()).ok() != Some(parent) {
                        libc::_exit(1);
                    }
                    // Nothing it doesn't need (a kernel before 5.11 has no
                    // `close_range`: they're left as they are)...
                    let _ = libc::syscall(
                        libc::SYS_close_range,
                        3_u32,
                        u32::MAX,
                        libc::CLOSE_RANGE_CLOEXEC,
                    );
                    // ...but the two namespaces.
                    for fd in passed {
                        if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            let child = command.spawn().map_err(|source| PastaError::Spawn {
                // The launcher: this binary again (`/proc/self/exe`).
                path: "/proc/self/exe (pasta's launcher)".into(),
                source,
            })?;
            drop((userns, netns));
            let pid = libc::pid_t::try_from(child.id())
                .map_err(|_| PastaError::Failed("an impossible process ID".to_owned()))?;
            let pasta = Pasta {
                pid,
                pidfd: crate::pidfd::open(pid).ok(),
            };
            pasta.wait_ready(&pid_file, &log_path)?;
            Ok(pasta)
        }

        /// Waits for pasta's process ID file: it's written once the
        /// container's network is set up. pasta ending first is a failure,
        /// said in its own words (its log).
        fn wait_ready(&self, pid_file: &Path, log: &Path) -> Result<(), PastaError> {
            let deadline = Instant::now() + READY_WAIT;
            let said = || {
                let text = std::fs::read_to_string(log).unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    "it said nothing".to_owned()
                } else {
                    text.to_owned()
                }
            };
            loop {
                if std::fs::metadata(pid_file).is_ok_and(|file| file.len() > 0) {
                    return Ok(());
                }
                if self.reaped() {
                    return Err(PastaError::Failed(said()));
                }
                if Instant::now() >= deadline {
                    return Err(PastaError::Failed(format!(
                        "not ready after {} s ({})",
                        READY_WAIT.as_secs(),
                        said()
                    )));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Reaps pasta if it has ended (without waiting): true if it had.
        /// Through its pidfd (never another child, whatever its PID), or its
        /// PID before Linux 5.4 (`P_PIDFD`): pasta is our own child, and
        /// one the supervisor reaped already gives ECHILD.
        fn reaped(&self) -> bool {
            if let Some(pidfd) = &self.pidfd
                && let Ok(id) = libc::id_t::try_from(pidfd.as_raw_fd())
            {
                // SAFETY: zeroed `siginfo_t` is valid storage for `waitid`.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: `info` is valid storage; the pidfd is ours.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PIDFD,
                        id,
                        &raw mut info,
                        libc::WEXITED | libc::WNOHANG,
                    )
                };
                let errno = (result != 0).then(|| std::io::Error::last_os_error().raw_os_error());
                match errno {
                    // Reaped already: by the supervisor, which reaps what
                    // the run leaves (it's a child of this process).
                    Some(Some(libc::ECHILD)) => return true,
                    // Before Linux 5.4: no `P_PIDFD`.
                    Some(Some(libc::EINVAL)) => {}
                    // SAFETY: `si_pid` is set by `waitid` (0 if nothing ended).
                    _ => return result == 0 && unsafe { info.si_pid() } != 0,
                }
            }
            let mut status = 0;
            // SAFETY: `status` is valid storage; WNOHANG never blocks.
            let result = unsafe { libc::waitpid(self.pid, &raw mut status, libc::WNOHANG) };
            result == self.pid
                || (result == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        }
    }

    /// The launcher's work (see the module), then pasta: the container's
    /// user namespace joined (unless it's this one), a mount namespace of
    /// its own with nothing propagating out, the network namespace
    /// bind-mounted on the file, and pasta started and watched over.
    pub(super) fn launch(launch: &Launch) -> Result<std::convert::Infallible, String> {
        use nix::mount::{MsFlags, mount};
        use nix::sched::{CloneFlags, setns, unshare};
        use std::os::unix::fs::MetadataExt as _;

        let parent = nix::unistd::getppid();
        for (fd, what) in [(launch.userns, "user"), (launch.netns, "network")] {
            // SAFETY: `F_GETFD` only reads the descriptor's flags; one that
            // isn't open is EBADF.
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
                return Err(format!("no {what} namespace descriptor {fd} open"));
            }
        }
        // SAFETY: both are open (just checked) and distinct (`from_args`),
        // left for this process by Shipyard (see `Pasta::start`); nothing
        // else here owns them.
        let (userns, netns) = unsafe {
            (
                OwnedFd::from_raw_fd(launch.userns),
                OwnedFd::from_raw_fd(launch.netns),
            )
        };
        let same = |fd: &OwnedFd| -> Result<bool, String> {
            let id = |path: String| {
                std::fs::metadata(&path)
                    .map(|ns| (ns.dev(), ns.ino()))
                    .map_err(|e| format!("{path}: {e}"))
            };
            Ok(id(format!("/proc/self/fd/{}", fd.as_raw_fd()))?
                == id("/proc/self/ns/user".to_owned())?)
        };
        // One can't join the namespace one is in (root's container shares
        // it); and the host's root needn't (its capabilities reach every
        // namespace): pasta then starts as root, as it always did for root,
        // never as an ID the container's map may not have.
        let host_root = nix::unistd::geteuid().is_root()
            && std::fs::metadata("/proc/self/ns/user")
                .is_ok_and(|ns| ns.ino() == INITIAL_USER_NAMESPACE);
        if !host_root && !same(&userns)? {
            setns(&userns, CloneFlags::CLONE_NEWUSER)
                .map_err(|e| format!("joining the container's user namespace: {e}"))?;
        }
        unshare(CloneFlags::CLONE_NEWNS).map_err(|e| format!("making a mount namespace: {e}"))?;
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )
        .map_err(|e| format!("making its mounts private: {e}"))?;
        mount(
            Some(format!("/proc/self/fd/{}", netns.as_raw_fd()).as_str()),
            &launch.netns_file,
            None::<&str>,
            MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| {
            format!(
                "binding the network namespace on {}: {e}",
                launch.netns_file.display()
            )
        })?;
        drop((userns, netns));
        watch_over_pasta(launch, parent, host_root)
    }

    /// The launcher's last part: pasta started as its child, and watched
    /// over. pasta can't simply die with Shipyard: under SELinux its exec
    /// changes domain (`pasta_t`), which clears the parent-death signal as
    /// a setuid exec would. The launcher keeps its own (it stays as it is),
    /// so it's told when Shipyard dies (SIGTERM), as when Shipyard asks it
    /// to stop (SIGTERM too): then it kills pasta, and ends. When pasta ends
    /// first, it ends the same way (Shipyard's `wait_ready` reads that).
    fn watch_over_pasta(
        launch: &Launch,
        parent: nix::unistd::Pid,
        host_root: bool,
    ) -> Result<std::convert::Infallible, String> {
        use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, sigprocmask};
        use nix::sys::signalfd::{SfdFlags, SignalFd};
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
        use nix::unistd::{ForkResult, fork, getppid};

        let mut watched = SigSet::empty();
        for signal in [Signal::SIGTERM, Signal::SIGHUP, Signal::SIGCHLD] {
            watched.add(signal);
        }
        let mut before = SigSet::empty();
        sigprocmask(SigmaskHow::SIG_BLOCK, Some(&watched), Some(&mut before))
            .map_err(|e| format!("blocking its signals: {e}"))?;
        let signals = SignalFd::with_flags(&watched, SfdFlags::SFD_CLOEXEC)
            .map_err(|e| format!("a signalfd: {e}"))?;
        // Set again after the user namespace was joined (a change of
        // credentials can clear it); and Shipyard may have died meanwhile.
        // SAFETY: `prctl` with plain integers.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == -1 {
            return Err(format!(
                "setting the parent-death signal: {}",
                std::io::Error::last_os_error()
            ));
        }
        if getppid() != parent {
            return Err("Shipyard is gone".to_owned());
        }
        // SAFETY: this process is single-threaded (the launcher runs first
        // thing in `main`), so the child may run anything.
        let pasta = match unsafe { fork() }.map_err(|e| format!("forking pasta: {e}"))? {
            ForkResult::Child => {
                // pasta's own, as without SELinux it keeps: it dies with the
                // launcher too.
                // SAFETY: `prctl` with plain integers.
                unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
                let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&before), None);
                let error = Command::new(&launch.pasta)
                    .arg0("pasta")
                    // The host's root: pasta would switch to `nobody`
                    // before joining the namespace, which `nobody` can't.
                    // It stays root, keeping only the capabilities it
                    // needs (probe: root's pasta works only so).
                    .args(if host_root {
                        &["--runas", "0"][..]
                    } else {
                        &[]
                    })
                    .args(&launch.arguments)
                    .exec();
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("starting {}: {error}\n", launch.pasta.display()).as_bytes(),
                );
                std::process::exit(127);
            }
            ForkResult::Parent { child } => child,
        };
        loop {
            let Ok(Some(arrived)) = signals.read_signal() else {
                continue;
            };
            if arrived.ssi_signo == Signal::SIGCHLD as u32 {
                match waitpid(pasta, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, code)) => std::process::exit(code),
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        std::process::exit(128 + signal as i32)
                    }
                    _ => continue,
                }
            }
            // Told to stop, or Shipyard died: pasta goes, then this.
            let _ = kill(pasta, Signal::SIGKILL);
            let _ = waitpid(pasta, None);
            std::process::exit(0);
        }
    }

    impl Pasta {
        /// Sends `signal` to the launcher (pasta's parent).
        fn signal(&self, signal: libc::c_int) {
            match &self.pidfd {
                // ESRCH (gone already) is fine.
                Some(pidfd) => {
                    let _ = crate::pidfd::send_signal(pidfd, signal);
                }
                // SAFETY: no memory involved. Before Linux 5.3 only: an
                // unreaped child's PID isn't reused, and the supervisor
                // only reaps it after the command (this runs before).
                None => unsafe {
                    libc::kill(self.pid, signal);
                },
            }
        }

        /// Waits, bounded, for the launcher to have ended: whether it had.
        fn reaped_within(&self, wait: Duration) -> bool {
            let deadline = Instant::now() + wait;
            loop {
                if self.reaped() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Pasta {
        /// The launcher is asked to stop (it kills pasta, then ends); if it
        /// doesn't in time, it's killed. Bounded: one stuck in the kernel is
        /// left to the supervisor's sweep rather than hang the run's end.
        fn drop(&mut self) {
            self.signal(libc::SIGTERM);
            if !self.reaped_within(REAP_WAIT) {
                self.signal(libc::SIGKILL);
                self.reaped_within(REAP_WAIT);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn nothing_published_means_no_port_forwarded_either_way() {
        let arguments = arguments(
            Path::new("/run/x/netns"),
            &[],
            Path::new("/run/x/pasta.pid"),
            Features::default(),
        );
        let text = arguments.join(std::ffi::OsStr::new(" "));
        let text = text.to_string_lossy();
        assert!(text.contains("-T none -U none --no-map-gw"), "{text}");
        assert!(!text.contains("--ipv4-only"), "{text}");
        assert!(text.contains("-t none -u none"), "{text}");
        assert!(text.contains("--dns-forward 169.254.1.1"), "{text}");
        assert!(!text.contains("-I"), "{text}");
        // Its network namespace by file, never a PID: no `/proc/<pid>` to
        // open (see the module).
        assert!(
            text.ends_with("--pid /run/x/pasta.pid --netns /run/x/netns"),
            "{text}"
        );
        assert!(!text.contains("--userns"), "{text}");
    }

    #[test]
    fn published_ports_replace_none() {
        let ports = [
            Published {
                host_ip: None,
                host_port: 8080,
                container_port: 80,
                protocol: Protocol::Tcp,
            },
            Published {
                host_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                host_port: 5353,
                container_port: 53,
                protocol: Protocol::Udp,
            },
        ];
        let text = arguments(
            Path::new("/run/7/netns"),
            &ports,
            Path::new("p"),
            Features {
                ns_ifname: true,
                no_netns_quit: true,
            },
        )
        .join(std::ffi::OsStr::new(" "));
        let text = text.to_string_lossy();
        assert!(text.ends_with("--pid p --netns /run/7/netns"), "{text}");
        assert!(text.contains("-t 8080:80 -u 127.0.0.1/5353:53"), "{text}");
        // An IPv6 address goes as it is, before the `/`.
        let v6 = arguments(
            Path::new("/run/1/netns"),
            &[Published {
                host_ip: Some("2001:db8::1".parse().unwrap()),
                ..ports[0]
            }],
            Path::new("p"),
            Features::default(),
        )
        .join(std::ffi::OsStr::new(" "));
        assert!(v6.to_string_lossy().contains("-t 2001:db8::1/8080:80"));
        assert!(!text.contains("-t none"), "{text}");
        assert!(!text.contains("-u none"), "{text}");
        assert!(text.contains("-I eth0"), "{text}");
        assert!(text.contains("--no-netns-quit"), "{text}");
    }

    #[test]
    fn the_namespace_file_is_always_a_new_one() {
        let dir = std::env::temp_dir().join(format!("shipyard-netns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(NETNS_FILE);
        make_netns_file(&file).unwrap();
        // One that's there already (a planted file) is refused...
        assert_eq!(
            make_netns_file(&file).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        // ...and so is a link, even to nothing (`O_EXCL` alone refuses it;
        // `O_NOFOLLOW` too).
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &file).unwrap();
        assert!(make_netns_file(&file).is_err());
        assert!(!dir.join("elsewhere").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_launch_goes_through_its_command_line() {
        let launch = Launch {
            userns: 5,
            netns: 6,
            netns_file: "/run/user/1000/shipyard/runs/a/netns".into(),
            pasta: "/usr/bin/pasta".into(),
            arguments: vec!["--foreground".into(), "--netns".into(), "x".into()],
        };
        assert_eq!(Launch::from_args(&launch.to_args()), Ok(launch));
    }

    #[test]
    fn a_launch_refuses_what_it_wasnt_given() {
        let args =
            |list: &[&str]| -> Vec<std::ffi::OsString> { list.iter().map(Into::into).collect() };
        for (bad, why) in [
            (args(&[]), "no user namespace descriptor"),
            (
                args(&["5", "5", "/n", "/p", "--"]),
                "the two namespaces are one descriptor",
            ),
            // The standard descriptors are never a namespace.
            (
                args(&["0", "6", "/n", "/p", "--"]),
                "no user namespace descriptor",
            ),
            (
                args(&["5", "x", "/n", "/p", "--"]),
                "no network namespace descriptor",
            ),
            (
                args(&["5", "6", "n", "/p", "--"]),
                "no absolute namespace file",
            ),
            (args(&["5", "6", "/n", "p", "--"]), "no absolute pasta"),
            (
                args(&["5", "6", "/n", "/p", "-f"]),
                "no `--` before pasta's arguments",
            ),
        ] {
            assert_eq!(Launch::from_args(&bad), Err(why.to_owned()), "{bad:?}");
        }
        assert!(Launch::from_args(&args(&["5", "6", "/n", "/p", "--"])).is_ok());
    }

    #[test]
    fn features_come_from_the_help() {
        assert!(Features::from_help("  -I, --ns-ifname NAME  ...").ns_ifname);
        assert!(!Features::from_help("-t, --tcp-ports SPEC").ns_ifname);
        assert!(Features::from_help("  --no-netns-quit  ...").no_netns_quit);
        assert!(!Features::from_help("-I, --ns-ifname NAME").no_netns_quit);
    }

    #[test]
    fn pasta_is_found_on_the_path_or_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("shipyard-pasta-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find(Some(dir.as_os_str())), None);
        let pasta = dir.join("pasta");
        std::fs::write(&pasta, "#!/bin/sh\n").unwrap();
        // Not executable: not it.
        assert_eq!(find(Some(dir.as_os_str())), None);
        std::fs::set_permissions(&pasta, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), &dir]).unwrap();
        assert_eq!(find(Some(&path)), Some(pasta));
        // A relative entry never counts, even with a pasta in it: `dir`,
        // reached from the current directory as `../../…/tmp/…`.
        let up = std::env::current_dir().unwrap().components().count() - 1;
        let relative = Path::new(&"../".repeat(up)).join(dir.strip_prefix("/").unwrap());
        assert!(relative.is_relative() && relative.join("pasta").exists());
        assert_eq!(find(Some(relative.as_os_str())), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
