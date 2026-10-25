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
//! bind mount is the launcher's mount namespace's alone (its mounts are
//! slaves of the host's): the host never sees it, and the host's unmounts
//! still reach the launcher's copy. The launcher stays, as pasta's parent:
//! under SELinux, pasta can't keep a parent-death signal of its own (see
//! `watch_over_pasta`). The run's directory is never under `~/.local`, which
//! `pasta_t` may not reach either.
//!
//! **Whose life it follows.** A foreground run's pasta follows the Shipyard
//! process that started it ([`Pasta::start`]): the launcher dies with it
//! (`PR_SET_PDEATHSIG`), and dropping the [`Pasta`] stops it. A detached
//! container's follows the container's init instead
//! (`Pasta::start_detached`, Linux, `--follow-init`): the launcher is
//! started in a session of its own, with no signal blocked or ignored, and
//! forks once more at once (its first process ends, so whoever adopts the
//! second, init or a subreaper, isn't the caller: no zombie is left there).
//! It says over a pipe when pasta is ready, or why not; then it watches the
//! init's pidfd (or, without one, its start time), and when the init ends it
//! kills pasta and ends. pasta then isn't given `--no-netns-quit`: removing
//! the run's directory (the container's clean-up) ends it too, should its
//! launcher have been killed.

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
        Self::asked(pasta).map(|(features, _)| features)
    }

    /// [`of`](Self::of), and whether the answer can be kept: pasta's
    /// `--help` ended well (exit 0) and said something. One killed, or cut
    /// short, says no features, true for this run at worst, and never kept
    /// for the next (F50).
    fn asked(pasta: &Path) -> Result<(Features, bool), PastaError> {
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
        let keepable = help.status.success() && !text.trim().is_empty();
        Ok((Features::from_help(&text), keepable))
    }

    /// [`of`](Self::of), kept in the file `at` (the caller's runtime
    /// directory: gone at logout or a reboot) for as long as `pasta` is the
    /// same binary (its path, device, inode, size, and modification and
    /// change times: the change time no one can set, and any write moves):
    /// asking runs it, and pasta writes its help to the system log too,
    /// about 60 lines a run (F50). With no `at`, or `pasta` that can't be
    /// looked at, it's asked. A record that doesn't match, or can't be
    /// read, is asked again and written anew; a newer Shipyard's (a later
    /// version) is asked again and left as it is. A record that can't be
    /// written (the directory not made yet: a first run) costs only the
    /// log lines next time; an answer from a `--help` that failed is never
    /// kept. Two callers finding pasta at two paths (two `PATH`s) keep
    /// replacing each other's record, and ask as before F50.
    ///
    /// # Errors
    ///
    /// `pasta` can't be run ([`of`](Self::of)).
    pub fn cached(pasta: &Path, at: Option<&Path>) -> Result<Features, PastaError> {
        use std::os::unix::fs::MetadataExt as _;
        let (Some(at), Ok(metadata)) = (at, std::fs::metadata(pasta)) else {
            return Self::of(pasta);
        };
        let record = |features: Features| KeptFeatures {
            v: KEPT_VERSION,
            path: pasta.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
            ns_ifname: features.ns_ifname,
            no_netns_quit: features.no_netns_quit,
        };
        let text = shipyard_sys::fs::read_bounded(at, KEPT_MAX).ok();
        let version = text
            .as_deref()
            .and_then(|text| serde_json::from_slice::<KeptVersion>(text).ok())
            .map(|kept| kept.v);
        if version == Some(KEPT_VERSION)
            && let Some(kept) = text
                .as_deref()
                .and_then(|text| serde_json::from_slice::<KeptFeatures>(text).ok())
        {
            let features = Features {
                ns_ifname: kept.ns_ifname,
                no_netns_quit: kept.no_netns_quit,
            };
            if kept == record(features) {
                return Ok(features);
            }
        }
        let (features, keepable) = Self::asked(pasta)?;
        if keepable
            && version.is_none_or(|version| version <= KEPT_VERSION)
            && let Ok(json) = serde_json::to_vec(&record(features))
        {
            let _ =
                shipyard_sys::fs::write_atomically(at, &json, &shipyard_sys::fs::Write::new(0o600));
        }
        Ok(features)
    }
}

/// The version of [`KeptFeatures`] this Shipyard writes (H2).
const KEPT_VERSION: u32 = 1;

/// The most a kept record is read of.
const KEPT_MAX: u64 = 4096;

/// What a pasta binary can do, as [`Features::cached`] keeps it: the
/// binary (by path, and by what `stat` says of it), and its features.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeptFeatures {
    v: u32,
    path: PathBuf,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
    ns_ifname: bool,
    no_netns_quit: bool,
}

/// A kept record's version, read before the rest (a newer one's fields
/// aren't known here).
#[derive(serde::Deserialize)]
struct KeptVersion {
    v: u32,
}

/// Where programs are looked for without a `PATH` (as `execvp` does).
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// pasta, where `PATH` (`path`, as the environment variable says; the
/// usual directories without one) has it: an absolute path (an empty or
/// relative entry, the current directory's, doesn't count).
#[must_use]
pub fn find(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path.unwrap_or(std::ffi::OsStr::new(DEFAULT_PATH));
    shipyard_sys::path::find_program("pasta", Some(path))
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

/// The word that says a launcher follows the container's init
/// ([`Follow::Init`]), then its PID, start time, pidfd (`-` for none), the
/// ready pipe, pasta's process ID file, the lock it holds (`-` for none)
/// and where pasta's death is said (`-` for nowhere).
#[cfg(any(target_os = "linux", test))]
const FOLLOW_INIT: &str = "--follow-init";

/// What the launcher is told: the container's user and network namespaces
/// (descriptors it inherited), the file to bind-mount the latter on, whom
/// it follows, and pasta and its arguments.
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
    /// Whose life pasta's follows.
    pub(crate) follow: Follow,
    /// pasta's arguments ([`arguments`]).
    pub(crate) arguments: Vec<std::ffi::OsString>,
}

/// Whose life a launcher's pasta follows.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Follow {
    /// The Shipyard process that started it (a foreground run's): the
    /// launcher dies with it (`PR_SET_PDEATHSIG`), and it ends the launcher
    /// when the run does.
    Caller,
    /// The container's init (a detached container's): the launcher forks
    /// once more at once, so whoever adopts it isn't the caller, and stops
    /// pasta when the init ends.
    Init {
        /// The init's PID.
        pid: i32,
        /// Its start time (with the PID, it names it alone).
        start_time: u64,
        /// A pidfd for it (inherited), where the kernel has them.
        pidfd: Option<i32>,
        /// The pipe the launcher says "ready" (or why not) on (inherited).
        ready: i32,
        /// pasta's process ID file: written once it's ready.
        pid_file: PathBuf,
        /// A lock the launcher holds until it ends, pasta reaped
        /// (inherited): the container's `running` file, so a start's end
        /// (the file free) waits for pasta to have let go of its ports.
        hold: Option<i32>,
        /// Where pasta's death while the init runs is said (the start's
        /// log directory): given to the binary's hook ([`Lost`]).
        lost_to: Option<PathBuf>,
    },
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
        ];
        if let Follow::Init {
            pid,
            start_time,
            pidfd,
            ready,
            pid_file,
            hold,
            lost_to,
        } = &self.follow
        {
            let optional =
                |fd: &Option<i32>| fd.map_or_else(|| "-".to_owned(), |fd| fd.to_string());
            args.extend([
                FOLLOW_INIT.into(),
                pid.to_string().into(),
                start_time.to_string().into(),
                optional(pidfd).into(),
                ready.to_string().into(),
                pid_file.clone().into(),
                optional(hold).into(),
                lost_to
                    .clone()
                    .map_or_else(|| "-".into(), std::ffi::OsString::from),
            ]);
        }
        args.push("--".into());
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
        let number = |arg: Option<&std::ffi::OsString>, what: &str| {
            arg.and_then(|arg| arg.to_str())
                .and_then(|text| text.parse::<u64>().ok())
                .ok_or_else(|| format!("no {what}"))
        };
        let userns = fd(args.first(), "user namespace")?;
        let netns = fd(args.get(1), "network namespace")?;
        if netns == userns {
            return Err("the two namespaces are one descriptor".to_owned());
        }
        let netns_file = path(args.get(2), "namespace file")?;
        let pasta = path(args.get(3), "pasta")?;
        let is = |index: usize, word: &str| {
            args.get(index).map(std::ffi::OsString::as_os_str) == Some(std::ffi::OsStr::new(word))
        };
        let (follow, rest) = if is(4, FOLLOW_INIT) {
            let pid = number(args.get(5), "init's PID")
                .and_then(|pid| i32::try_from(pid).map_err(|_| "no init's PID".to_owned()))
                .and_then(|pid| {
                    (pid > 0)
                        .then_some(pid)
                        .ok_or_else(|| "no init's PID".to_owned())
                })?;
            let start_time = number(args.get(6), "init's start time")?;
            let pidfd = if is(7, "-") {
                None
            } else {
                Some(fd(args.get(7), "init's pidfd")?)
            };
            let ready = fd(args.get(8), "ready pipe")?;
            let pid_file = path(args.get(9), "pasta's process ID file")?;
            let hold = if is(10, "-") {
                None
            } else {
                Some(fd(args.get(10), "hold")?)
            };
            let lost_to = if is(11, "-") {
                None
            } else {
                Some(path(args.get(11), "directory to say pasta's death in")?)
            };
            let fds = [Some(userns), Some(netns), pidfd, Some(ready), hold];
            let distinct = fds.iter().flatten().enumerate().all(|(index, fd)| {
                !fds.iter()
                    .flatten()
                    .skip(index + 1)
                    .any(|other| other == fd)
            });
            if !distinct {
                return Err("two of its descriptors are one".to_owned());
            }
            let follow = Follow::Init {
                pid,
                start_time,
                pidfd,
                ready,
                pid_file,
                hold,
                lost_to,
            };
            (follow, 12)
        } else {
            (Follow::Caller, 4)
        };
        if !is(rest, "--") {
            return Err("no `--` before pasta's arguments".to_owned());
        }
        Ok(Launch {
            userns,
            netns,
            netns_file,
            pasta,
            follow,
            arguments: args.get(rest + 1..).unwrap_or_default().to_vec(),
        })
    }
}

/// Where pasta's launcher takes over, if this process is one: never
/// returns then. Call it first thing in `main`, before any thread starts
/// (joining a user namespace needs a single-threaded process).
/// `refuse_privileged_start` is the binary's own check: a launcher started
/// setuid or with file capabilities gets no further.
/// `lost` is called (in the launcher, once, before it ends) when a
/// detached container's pasta dies while its init runs: the binary says
/// so where [`Lost::start_dir`] is.
#[cfg(target_os = "linux")]
pub fn entry_point(refuse_privileged_start: impl FnOnce() -> Result<(), String>, lost: fn(&Lost)) {
    let mut args = std::env::args_os();
    if args.nth(1).as_deref() != Some(std::ffi::OsStr::new(LAUNCHER_ARG)) {
        return;
    }
    let args: Vec<std::ffi::OsString> = args.collect();
    // Only returns on failure (the launcher becomes pasta otherwise).
    let Err(error) = refuse_privileged_start()
        .and_then(|()| Launch::from_args(&args))
        .and_then(|launch| linux::launch(&launch, lost));
    // pasta's log: Shipyard reports it.
    let _ = std::io::Write::write_all(
        &mut std::io::stderr(),
        format!("shipyard's pasta launcher: {error}\n").as_bytes(),
    );
    std::process::exit(125);
}

/// Elsewhere there's no pasta.
#[cfg(not(target_os = "linux"))]
pub fn entry_point(
    _refuse_privileged_start: impl FnOnce() -> Result<(), String>,
    _lost: fn(&Lost),
) {
}

#[cfg(target_os = "linux")]
pub use linux::Pasta;

/// The container init a detached container's pasta follows
/// (`Pasta::start_detached`): it runs while the init does.
#[derive(Debug, Clone, Copy)]
pub struct InitToFollow<'a> {
    /// Its PID.
    pub pid: i32,
    /// Its start time (with the PID, it names it alone).
    pub start_time: u64,
    /// A pidfd for it, where the kernel has them.
    pub pidfd: Option<&'a std::os::fd::OwnedFd>,
    /// Where pasta's death while the init runs is said (the start's log
    /// directory), to the binary's hook ([`entry_point`]'s `lost`).
    pub lost_to: Option<&'a Path>,
}

/// A detached container's pasta died while its init ran: the container is
/// left without a network. What the launcher tells the binary's hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost {
    /// Where to say it: the start's log directory.
    pub start_dir: PathBuf,
    /// How pasta ended, in words: `ended (code 1)`, `was killed (SIGKILL)`.
    pub how: String,
}

/// Process `pid`'s start time (`/proc/PID/stat`'s 22nd field), if it's
/// alive (a zombie, or one dead, has none worth following: it's ended).
#[cfg(target_os = "linux")]
fn start_time(pid: i32) -> Option<u64> {
    shipyard_sys::proc::start_time(pid)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
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
    /// How long the init is given, once pasta has ended, to be seen ending
    /// too (then it's the container's end, not a lost network).
    const LOSS_GRACE: Duration = Duration::from_millis(200);
    /// How long a killed pasta has to end before it's left to the
    /// supervisor's sweep.
    const REAP_WAIT: Duration = Duration::from_secs(1);

    /// A pasta serving a foreground run's container: killed when dropped (a
    /// detached container's isn't held here: it follows the container's
    /// init, see `start_detached`).
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
            let Prepared {
                pid_file,
                log_path,
                netns_file,
                log,
                userns,
                netns,
            } = Prepared::new(dir, pid)?;
            let launch = Launch {
                userns: userns.as_raw_fd(),
                netns: netns.as_raw_fd(),
                netns_file: netns_file.clone(),
                pasta: pasta.to_owned(),
                follow: super::Follow::Caller,
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
                pidfd: shipyard_sys::pidfd::open(pid).ok(),
            };
            pasta.wait_ready(&pid_file, &log_path)?;
            Ok(pasta)
        }

        /// Starts `pasta` for a detached container, following its `init`:
        /// pasta runs while the init does, and ends with it, whatever
        /// becomes of this process. As [`start`](Self::start), but the
        /// launcher is in a session of its own, with no signal blocked or
        /// ignored (a mask and ignored dispositions survive `exec`), forks
        /// once more at once (so whoever adopts it isn't this process: no
        /// zombie is left here), and says over a pipe when pasta is ready,
        /// or why not. pasta doesn't get `--no-netns-quit`: removing the
        /// run's directory (`__cleanup`) ends it too, should its launcher
        /// be killed. Nothing is returned to drop: pasta isn't this
        /// process's to stop. The launcher holds `hold` (a lock, shared by
        /// the open file description) until it ends, pasta reaped.
        ///
        /// # Errors
        ///
        /// Its files couldn't be made; the init is gone (before or while
        /// pasta starts); pasta couldn't be started, or wasn't ready in
        /// time (said in its own words, its log).
        pub fn start_detached(
            pasta: &Path,
            features: Features,
            init: &super::InitToFollow<'_>,
            ports: &[Published],
            dir: &Path,
            hold: Option<std::os::fd::BorrowedFd<'_>>,
        ) -> Result<(), PastaError> {
            Self::start_detached_in(pasta, features, init, ports, dir, hold, None)
        }

        /// As [`start_detached`](Self::start_detached), with the launcher
        /// (and so pasta) in the cgroup whose `cgroup.procs` is open at
        /// `cgroup_procs`: the container's monitor's (`shipyard/monitors/<id>`),
        /// out of this process's. In the caller's, a login session's or a
        /// unit's, pasta would end with it (a logout, `systemctl stop`) and
        /// leave the container, its monitor living on elsewhere, with no
        /// network. Joined by writing "0" before the launcher runs any code;
        /// a failure leaves it where it is, said in pasta's log.
        ///
        /// # Errors
        ///
        /// As [`start_detached`](Self::start_detached).
        pub fn start_detached_in(
            pasta: &Path,
            features: Features,
            init: &super::InitToFollow<'_>,
            ports: &[Published],
            dir: &Path,
            hold: Option<std::os::fd::BorrowedFd<'_>>,
            cgroup_procs: Option<std::os::fd::BorrowedFd<'_>>,
        ) -> Result<(), PastaError> {
            let io = |context: String| move |source| PastaError::Io { context, source };
            let Prepared {
                pid_file,
                log_path,
                netns_file,
                log,
                userns,
                netns,
            } = Prepared::new(dir, init.pid)?;
            // Opened by PID: still the init followed? (One reused in
            // between would be another process's namespaces.)
            let alive = match init.pidfd {
                Some(pidfd) => shipyard_sys::pidfd::send_signal(pidfd, 0).is_ok(),
                None => super::start_time(init.pid) == Some(init.start_time),
            };
            if !alive {
                return Err(PastaError::Failed(
                    "the container's init ended before pasta started".to_owned(),
                ));
            }
            let (ready_read, ready_write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
                .map_err(|errno| io("making a pipe".to_owned())(errno.into()))?;
            let launch = Launch {
                userns: userns.as_raw_fd(),
                netns: netns.as_raw_fd(),
                netns_file: netns_file.clone(),
                pasta: pasta.to_owned(),
                follow: super::Follow::Init {
                    pid: init.pid,
                    start_time: init.start_time,
                    pidfd: init.pidfd.map(std::os::fd::AsRawFd::as_raw_fd),
                    ready: ready_write.as_raw_fd(),
                    pid_file: pid_file.clone(),
                    hold: hold.map(|fd| fd.as_raw_fd()),
                    lost_to: init.lost_to.map(Path::to_owned),
                },
                arguments: super::detached_arguments(&netns_file, ports, &pid_file, features),
            };
            let passed = [
                launch.userns,
                launch.netns,
                init.pidfd.map_or(-1, std::os::fd::AsRawFd::as_raw_fd),
                ready_write.as_raw_fd(),
                hold.map_or(-1, |fd| fd.as_raw_fd()),
            ];
            let mut command =
                detached_launcher(&launch, passed, cgroup_procs.map(|fd| fd.as_raw_fd()), log);
            let mut child = command.spawn().map_err(|source| PastaError::Spawn {
                path: "/proc/self/exe (pasta's launcher)".into(),
                source,
            })?;
            drop((userns, netns, ready_write));
            let said = || said_in(&log_path);
            // Its first process ends at once (the second goes on): a failure
            // before that fork is its exit code.
            let first = child
                .wait()
                .map_err(io("waiting for pasta's launcher".to_owned()))?;
            if !first.success() {
                return Err(PastaError::Failed(said()));
            }
            match read_ready(ready_read, READY_WAIT + Duration::from_secs(1)) {
                Ok(word) if word.trim() == "ready" => Ok(()),
                Ok(word) if !word.trim().is_empty() => {
                    Err(PastaError::Failed(format!("{} ({})", word.trim(), said())))
                }
                Ok(_) => Err(PastaError::Failed(said())),
                Err(why) => Err(PastaError::Failed(format!("{why} ({})", said()))),
            }
        }

        /// Waits for pasta's process ID file: it's written once the
        /// container's network is set up. pasta ending first is a failure,
        /// said in its own words (its log).
        fn wait_ready(&self, pid_file: &Path, log: &Path) -> Result<(), PastaError> {
            let deadline = Instant::now() + READY_WAIT;
            let said = || said_in(log);
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

    /// What a launcher is started with: pasta's files in the run's
    /// directory, its log made, and the container's namespaces.
    struct Prepared {
        pid_file: PathBuf,
        log_path: PathBuf,
        netns_file: PathBuf,
        log: File,
        userns: File,
        netns: File,
    }

    impl Prepared {
        /// Makes pasta's log and namespace file in `dir`, and opens the
        /// namespaces of process `pid`: opened here (Shipyard may: pasta,
        /// confined, may not), for the launcher to inherit.
        fn new(dir: &Path, pid: libc::pid_t) -> Result<Self, PastaError> {
            let io = |context: String| move |source| PastaError::Io { context, source };
            let pid_file = dir.join(PID_FILE);
            let log_path = dir.join(LOG_FILE);
            let netns_file = dir.join(NETNS_FILE);
            let log =
                File::create(&log_path).map_err(io(format!("making {}", log_path.display())))?;
            super::make_netns_file(&netns_file)
                .map_err(io(format!("making {}", netns_file.display())))?;
            let open = |kind: &str| {
                let path = format!("/proc/{pid}/ns/{kind}");
                File::open(&path).map_err(io(format!("opening {path}")))
            };
            Ok(Self {
                pid_file,
                log_path,
                netns_file,
                userns: open("user")?,
                netns: open("net")?,
                log,
            })
        }
    }

    /// The command that starts a detached launcher (see
    /// [`Pasta::start_detached_in`]): `launch`'s, with its stderr `log`,
    /// `passed` (the descriptors it inherits; -1 for none) kept open, and in
    /// the cgroup whose `cgroup.procs` is open at `cgroup_procs`, if any.
    pub(super) fn detached_launcher(
        launch: &Launch,
        passed: [i32; 5],
        cgroup_procs: Option<i32>,
        log: File,
    ) -> Command {
        let mut command = Command::new("/proc/self/exe");
        command
            .arg0("shipyard-pasta")
            .arg(LAUNCHER_ARG)
            .args(launch.to_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            // No working directory of the caller's held for the container's
            // life (it would keep that filesystem busy), as the monitor.
            .current_dir("/");
        // SAFETY: only async-signal-safe calls between fork and exec:
        // `write`, `sigemptyset`, `sigprocmask`, `signal`, `setsid`,
        // `syscall` and `fcntl`, reading `errno`; what's written is static.
        unsafe {
            command.pre_exec(move || {
                // Into the container's monitor's cgroup first: the launcher,
                // and pasta forked from it, live there (as the monitor does),
                // not in the caller's. Joined by writing "0", this process;
                // a failure leaves it where it is, said (stderr is pasta's
                // log).
                if let Some(procs) = cgroup_procs
                    && libc::write(procs, b"0".as_ptr().cast(), 1) != 1
                {
                    const SAID: &[u8] =
                        b"shipyard: pasta couldn't join the container's monitor's cgroup\n";
                    let _ = libc::write(libc::STDERR_FILENO, SAID.as_ptr().cast(), SAID.len());
                }
                // No signal blocked (this process may hold some off), and
                // none it stops on ignored (a `nohup` caller's SIGHUP): both
                // survive `exec`, and pasta inherits the dispositions (an
                // ignored signal not blocked is discarded as it's sent).
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut none);
                if libc::sigprocmask(libc::SIG_SETMASK, &raw const none, std::ptr::null_mut()) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                // A session of its own: this one's terminal and process
                // group never reach it. (Not `process_group(0)` too: a
                // group leader can't `setsid`.)
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = libc::syscall(
                    libc::SYS_close_range,
                    3_u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                );
                for fd in passed.into_iter().filter(|fd| *fd >= 0) {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        command
    }

    /// What pasta's log says (or that it said nothing).
    fn said_in(log: &Path) -> String {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            "it said nothing".to_owned()
        } else {
            text.to_owned()
        }
    }

    /// What a detached launcher wrote on its ready pipe (`ready`, or why
    /// not) before closing it, waiting up to `wait`.
    fn read_ready(pipe: OwnedFd, wait: Duration) -> Result<String, String> {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::io::Read as _;
        let deadline = Instant::now() + wait;
        let mut file = File::from(pipe);
        let mut said = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no word from pasta's launcher after {} s",
                    wait.as_secs()
                ));
            }
            let timeout = PollTimeout::from(u16::try_from(left.as_millis()).unwrap_or(u16::MAX));
            let mut fds = [PollFd::new(
                std::os::fd::AsFd::as_fd(&file),
                PollFlags::POLLIN,
            )];
            match poll(&mut fds, timeout) {
                Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
                Err(errno) => return Err(format!("waiting for pasta's launcher: {errno}")),
                Ok(_) => {}
            }
            let mut chunk = [0_u8; 512];
            match file.read(&mut chunk) {
                Ok(0) => return Ok(String::from_utf8_lossy(&said).into_owned()),
                Ok(read) => said.extend_from_slice(chunk.get(..read).unwrap_or_default()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("reading pasta's launcher: {error}")),
            }
        }
    }

    /// The launcher's work (see the module), then pasta: the container's
    /// user namespace joined (unless it's this one), a mount namespace of
    /// its own with nothing propagating out, the network namespace
    /// bind-mounted on the file, and pasta started and watched over.
    pub(super) fn launch(
        launch: &Launch,
        lost: fn(&super::Lost),
    ) -> Result<std::convert::Infallible, String> {
        use nix::mount::{MsFlags, mount};
        use nix::sched::{CloneFlags, setns, unshare};
        use std::os::unix::fs::MetadataExt as _;

        // Following the init: the first process ends at once (its caller
        // reaps it), and the second, adopted by init or a subreaper, goes on
        // (no zombie is ever left in the caller). Before anything else: it's
        // single-threaded still.
        if matches!(launch.follow, super::Follow::Init { .. }) {
            // SAFETY: single-threaded (the launcher runs first thing in
            // `main`); the parent only calls `_exit`.
            match unsafe { nix::unistd::fork() } {
                Ok(nix::unistd::ForkResult::Parent { .. }) => {
                    // SAFETY: `_exit` ends the process at once, as the
                    // fork's parent should (no destructors, no buffers).
                    unsafe { libc::_exit(0) }
                }
                Ok(nix::unistd::ForkResult::Child) => {}
                Err(errno) => return Err(format!("forking the launcher: {errno}")),
            }
        }
        let parent = nix::unistd::getppid();
        let own = |fd: i32, what: &str| -> Result<OwnedFd, String> {
            use shipyard_sys::fd::{OnExec, take_inherited};
            // SAFETY: distinct (`from_args`), left for this process by
            // Shipyard (`Pasta::start`, `start_detached`); nothing else here
            // owns them.
            unsafe { take_inherited(fd, OnExec::Keep) }
                .map_err(|_| format!("no {what} namespace descriptor {fd} open"))
        };
        let userns = own(launch.userns, "user")?;
        let netns = own(launch.netns, "network")?;
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
        // Slaves, not private: the bind below doesn't reach the host, but
        // the host's unmounts reach this copy. A private copy would keep
        // every filesystem the host unmounts alive (a USB disk, a LUKS
        // volume) for as long as this lives: a detached container's life.
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_SLAVE,
            None::<&str>,
        )
        .map_err(|e| format!("making its mounts slaves: {e}"))?;
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
        match &launch.follow {
            super::Follow::Caller => watch_over_pasta(launch, parent, host_root),
            super::Follow::Init {
                pid,
                start_time,
                pidfd,
                ready,
                pid_file,
                hold,
                lost_to,
            } => {
                let following =
                    Following::take(*pid, *start_time, *pidfd, *ready, pid_file, *hold)?;
                follow_init(launch, following, host_root, lost_to.as_deref(), lost)
            }
        }
    }

    /// What a detached launcher follows and says: the init (its pidfd, or
    /// its PID and start time), the pipe it says "ready" on, and the file
    /// that says pasta is.
    struct Following {
        pid: i32,
        start_time: u64,
        pidfd: Option<OwnedFd>,
        ready: Option<File>,
        pid_file: std::path::PathBuf,
        /// Held until this process ends (never read): see `Follow::Init`.
        _hold: Option<OwnedFd>,
    }

    impl Following {
        /// Takes the inherited descriptors (`pidfd`, `ready`, `hold`),
        /// checked open, made close-on-exec (pasta mustn't have them).
        fn take(
            pid: i32,
            start_time: u64,
            pidfd: Option<i32>,
            ready: i32,
            pid_file: &Path,
            hold: Option<i32>,
        ) -> Result<Following, String> {
            let own = |fd: i32, what: &str| -> Result<OwnedFd, String> {
                use shipyard_sys::fd::{OnExec, take_inherited};
                // SAFETY: distinct (`from_args`), left for this process by
                // Shipyard (`start_detached`).
                unsafe { take_inherited(fd, OnExec::Close) }
                    .map_err(|_| format!("no {what} descriptor {fd} open"))
            };
            Ok(Following {
                pid,
                start_time,
                pidfd: pidfd.map(|fd| own(fd, "init's pidfd")).transpose()?,
                ready: Some(File::from(own(ready, "ready pipe")?)),
                pid_file: pid_file.to_owned(),
                _hold: hold.map(|fd| own(fd, "hold")).transpose()?,
            })
        }

        /// Whether the init is still the process followed, running (by its
        /// PID and start time: a zombie has none) after `grace`. pasta
        /// ending as the init does isn't a loss: it's the end; and an init
        /// being killed may read as running a while (a namespace's init
        /// ends every other process in it first), so it's given time.
        fn init_survives(&self, grace: Duration) -> bool {
            let alive = || super::start_time(self.pid) == Some(self.start_time);
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if !alive() {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            alive()
        }

        /// Whether the init has ended: its pidfd readable, or (without
        /// one) its PID no longer the process that started then.
        fn init_gone(&self, pidfd_readable: bool) -> bool {
            match &self.pidfd {
                Some(_) => pidfd_readable,
                None => super::start_time(self.pid) != Some(self.start_time),
            }
        }

        /// Says `word` on the ready pipe, and closes it: whether it was
        /// heard (the caller gone, `EPIPE`, it wasn't).
        fn say(&mut self, word: &str) -> bool {
            use std::io::Write as _;
            self.ready
                .take()
                .is_some_and(|mut pipe| pipe.write_all(format!("{word}\n").as_bytes()).is_ok())
        }
    }

    /// A detached launcher's last part: pasta started as its child, and
    /// followed with the container's init, in one loop from pasta's fork on
    /// (its signals, the init's pidfd, and until it's ready, pasta's process
    /// ID file): ready, it says so; the init gone, or this told to stop,
    /// pasta is killed and reaped, then this ends; pasta gone, this ends as
    /// it did. Every way out kills pasta first: none may leave it running
    /// (SELinux's `pasta_t` clears its parent-death signal).
    fn follow_init(
        launch: &Launch,
        mut following: Following,
        host_root: bool,
        lost_to: Option<&Path>,
        lost: fn(&super::Lost),
    ) -> Result<std::convert::Infallible, String> {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use nix::sys::signal::Signal;
        use nix::sys::signalfd::SfdFlags;

        let (signals, before) = watch_signals(SfdFlags::SFD_NONBLOCK)?;
        let pasta = spawn_pasta(launch, &before, host_root)?;
        let deadline = Instant::now() + READY_WAIT;
        loop {
            // Waiting for pasta: every 5 ms; then, with a pidfd, for an
            // event; without one, every 100 ms (the init's start time).
            let timeout = if following.ready.is_some() {
                PollTimeout::from(5_u8)
            } else if following.pidfd.is_some() {
                PollTimeout::NONE
            } else {
                PollTimeout::from(100_u8)
            };
            let (signalled, init_readable) = {
                let mut fds = vec![PollFd::new(
                    std::os::fd::AsFd::as_fd(&signals),
                    PollFlags::POLLIN,
                )];
                if let Some(pidfd) = &following.pidfd {
                    fds.push(PollFd::new(
                        std::os::fd::AsFd::as_fd(pidfd),
                        PollFlags::POLLIN,
                    ));
                }
                match poll(&mut fds, timeout) {
                    Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => end(pasta),
                }
                let readable = |fd: Option<&PollFd<'_>>| {
                    fd.and_then(PollFd::revents)
                        .is_some_and(|events| !events.is_empty())
                };
                (readable(fds.first()), readable(fds.get(1)))
            };
            if signalled {
                while let Ok(Some(arrived)) = signals.read_signal() {
                    if arrived.ssi_signo != Signal::SIGCHLD as u32 {
                        // Told to stop: pasta goes, then this.
                        end(pasta);
                    }
                }
                if let Some(ended) = Ended::reaped(pasta) {
                    if following.ready.is_some() {
                        following.say(&format!("pasta {ended} before it was ready"));
                    } else if let Some(start_dir) = lost_to
                        && following.init_survives(LOSS_GRACE)
                    {
                        // While the container runs: its network is gone.
                        lost(&super::Lost {
                            start_dir: start_dir.to_owned(),
                            how: ended.to_string(),
                        });
                    }
                    std::process::exit(ended.code());
                }
            }
            if following.init_gone(init_readable) {
                following.say("the container's init ended before pasta was ready");
                end(pasta);
            }
            if following.ready.is_some() {
                if std::fs::metadata(&following.pid_file).is_ok_and(|file| file.len() > 0) {
                    // Heard, or the caller is gone: then nothing will start
                    // the container, and pasta has no one to serve.
                    if !following.say("ready") {
                        end(pasta);
                    }
                } else if Instant::now() >= deadline {
                    following.say(&format!(
                        "pasta wasn't ready after {} s",
                        READY_WAIT.as_secs()
                    ));
                    end(pasta);
                }
            }
        }
    }

    /// Blocks the signals a launcher handles (SIGTERM, SIGHUP, SIGINT,
    /// SIGQUIT: told to stop, pasta goes first; SIGCHLD), and opens a signalfd for them (with `flags` besides close-on-exec):
    /// it, and the signal mask from before, for pasta to be given back.
    fn watch_signals(
        flags: nix::sys::signalfd::SfdFlags,
    ) -> Result<(nix::sys::signalfd::SignalFd, nix::sys::signal::SigSet), String> {
        use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
        use nix::sys::signalfd::{SfdFlags, SignalFd};

        let mut watched = SigSet::empty();
        for signal in [
            Signal::SIGTERM,
            Signal::SIGHUP,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGCHLD,
        ] {
            watched.add(signal);
        }
        let mut before = SigSet::empty();
        sigprocmask(SigmaskHow::SIG_BLOCK, Some(&watched), Some(&mut before))
            .map_err(|e| format!("blocking its signals: {e}"))?;
        let signals = SignalFd::with_flags(&watched, SfdFlags::SFD_CLOEXEC | flags)
            .map_err(|e| format!("a signalfd: {e}"))?;
        Ok((signals, before))
    }

    /// How pasta ended.
    enum Ended {
        /// It exited, with this code.
        Exited(i32),
        /// A signal killed it.
        Killed(nix::sys::signal::Signal),
    }

    impl Ended {
        /// Reaps pasta if it has ended (without waiting): how.
        fn reaped(pasta: nix::unistd::Pid) -> Option<Ended> {
            use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
            match waitpid(pasta, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => Some(Ended::Exited(code)),
                Ok(WaitStatus::Signaled(_, signal, _)) => Some(Ended::Killed(signal)),
                _ => None,
            }
        }

        /// The launcher's exit code for it: pasta's, or 128 plus the signal
        /// (as a shell says).
        fn code(&self) -> i32 {
            match self {
                Ended::Exited(code) => *code,
                Ended::Killed(signal) => 128 + *signal as i32,
            }
        }
    }

    impl std::fmt::Display for Ended {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Ended::Exited(code) => write!(f, "ended (code {code})"),
                Ended::Killed(signal) => write!(f, "was killed ({signal})"),
            }
        }
    }

    /// Kills pasta, reaps it, and ends this launcher.
    fn end(pasta: nix::unistd::Pid) -> ! {
        use nix::sys::signal::{Signal, kill};
        let _ = kill(pasta, Signal::SIGKILL);
        let _ = nix::sys::wait::waitpid(pasta, None);
        std::process::exit(0);
    }

    /// Forks pasta, as `launch` says: its own parent-death signal (it dies
    /// with this process), the signal mask `before`, and (the host's root)
    /// `--runas 0`.
    fn spawn_pasta(
        launch: &Launch,
        before: &nix::sys::signal::SigSet,
        host_root: bool,
    ) -> Result<nix::unistd::Pid, String> {
        use nix::sys::signal::{SigmaskHow, sigprocmask};
        use nix::unistd::{ForkResult, fork};
        // SAFETY: this process is single-threaded (the launcher runs first
        // thing in `main`), so the child may run anything.
        match unsafe { fork() }.map_err(|e| format!("forking pasta: {e}"))? {
            ForkResult::Child => {
                // SAFETY: `prctl` with plain integers.
                unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
                let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(before), None);
                let error = Command::new(&launch.pasta)
                    .arg0("pasta")
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
            ForkResult::Parent { child } => Ok(child),
        }
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
        use nix::sys::signal::Signal;
        use nix::sys::signalfd::SfdFlags;
        use nix::unistd::getppid;

        let (signals, before) = watch_signals(SfdFlags::empty())?;
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
        let pasta = spawn_pasta(launch, &before, host_root)?;
        loop {
            let Ok(Some(arrived)) = signals.read_signal() else {
                continue;
            };
            if arrived.ssi_signo == Signal::SIGCHLD as u32 {
                match Ended::reaped(pasta) {
                    Some(ended) => std::process::exit(ended.code()),
                    None => continue,
                }
            }
            // Told to stop, or Shipyard died: pasta goes, then this.
            end(pasta);
        }
    }

    impl Pasta {
        /// Sends `signal` to the launcher (pasta's parent).
        fn signal(&self, signal: libc::c_int) {
            match &self.pidfd {
                // ESRCH (gone already) is fine.
                Some(pidfd) => {
                    let _ = shipyard_sys::pidfd::send_signal(pidfd, signal);
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

/// pasta's arguments for a detached container ([`arguments`]'s), never
/// with `--no-netns-quit`: removing the namespace file (`__cleanup`
/// removing the run's directory) ends pasta, even one whose launcher was
/// killed and whose parent-death signal SELinux cleared.
#[cfg(any(target_os = "linux", test))]
fn detached_arguments(
    netns_file: &Path,
    ports: &[Published],
    pid_file: &Path,
    features: Features,
) -> Vec<std::ffi::OsString> {
    arguments(
        netns_file,
        ports,
        pid_file,
        Features {
            no_netns_quit: false,
            ..features
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// What pasta can do is asked once per binary (F50): a fake pasta that
    /// counts its runs is run once for two asks, again once it's another
    /// binary (its size changed), again when the record is torn (and the
    /// record is written anew), again when the record is a newer
    /// Shipyard's (left as it is), every time without a record, and every
    /// time its `--help` fails (never kept).
    #[test]
    fn what_pasta_can_do_is_asked_once_per_binary() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!(
            "shipyard-pasta-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pasta = dir.join("pasta");
        let runs = dir.join("runs");
        // Written under another name, then renamed into place: never run
        // while a descriptor of it is open for writing in a process a
        // test beside this one forks (`ETXTBSY`).
        let write_pasta = |extra: &str| {
            let writing = dir.join("pasta.new");
            std::fs::write(
                &writing,
                format!(
                    "#!/bin/sh\necho run >> '{}'\necho '--ns-ifname --no-netns-quit'{extra}\n",
                    runs.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&writing, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&writing, &pasta).unwrap();
        };
        let ran = || std::fs::read_to_string(&runs).map_or(0, |text| text.lines().count());
        let kept = dir.join("pasta-features.json");
        let all = Features {
            ns_ifname: true,
            no_netns_quit: true,
        };
        write_pasta("");
        assert_eq!(Features::cached(&pasta, Some(&kept)).unwrap(), all);
        assert_eq!(Features::cached(&pasta, Some(&kept)).unwrap(), all);
        assert_eq!(ran(), 1, "asked once for two");
        // Another binary (an upgrade): asked again.
        write_pasta("\n# another build");
        assert_eq!(Features::cached(&pasta, Some(&kept)).unwrap(), all);
        assert_eq!(ran(), 2, "asked again of another binary");
        // A torn record: asked again, and written anew.
        std::fs::write(&kept, b"{\"v\": 1, \"pa").unwrap();
        Features::cached(&pasta, Some(&kept)).unwrap();
        assert_eq!(ran(), 3);
        Features::cached(&pasta, Some(&kept)).unwrap();
        assert_eq!(ran(), 3, "the record written anew");
        // A newer Shipyard's record: asked, and left as it is.
        let newer = br#"{"v": 2, "what": "a later version's"}"#;
        std::fs::write(&kept, newer).unwrap();
        Features::cached(&pasta, Some(&kept)).unwrap();
        assert_eq!(ran(), 4);
        assert_eq!(std::fs::read(&kept).unwrap(), newer);
        // No record: asked every time.
        Features::cached(&pasta, None).unwrap();
        Features::cached(&pasta, None).unwrap();
        assert_eq!(ran(), 6);
        // A `--help` that fails: its answer (no features) never kept.
        std::fs::remove_file(&kept).unwrap();
        write_pasta("\nexit 1");
        Features::cached(&pasta, Some(&kept)).unwrap();
        Features::cached(&pasta, Some(&kept)).unwrap();
        assert_eq!(ran(), 8, "a failed --help is asked again");
        assert!(!kept.exists(), "and never kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A detached container's pasta never gets `--no-netns-quit`, even
    /// from a pasta that has it (a foreground run's does).
    #[test]
    fn a_detached_pasta_quits_with_its_namespace_file() {
        let features = Features {
            ns_ifname: true,
            no_netns_quit: true,
        };
        let text = |arguments: Vec<std::ffi::OsString>| {
            arguments
                .join(std::ffi::OsStr::new(" "))
                .to_string_lossy()
                .into_owned()
        };
        let (netns, pid) = (Path::new("/run/x/netns"), Path::new("/run/x/pasta.pid"));
        assert!(text(arguments(netns, &[], pid, features)).contains("--no-netns-quit"));
        let detached = text(detached_arguments(netns, &[], pid, features));
        assert!(!detached.contains("--no-netns-quit"), "{detached}");
        assert!(detached.contains("-I eth0"), "the rest as features say");
    }

    /// The init's namespaces are opened by PID, then the init checked
    /// alive through its pidfd: one that has ended (its PID could be
    /// another process's by then) is refused before anything starts.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_detached_pasta_for_an_ended_init_is_refused() {
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let pidfd = shipyard_sys::pidfd::open(libc::pid_t::try_from(ended.id()).unwrap()).unwrap();
        ended.wait().unwrap();
        let dir = std::env::temp_dir().join(format!("shipyard-pasta-ended-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // This process's namespaces open fine: only the pidfd says no.
        let init = InitToFollow {
            pid: libc::pid_t::try_from(std::process::id()).unwrap(),
            start_time: 0,
            pidfd: Some(&pidfd),
            lost_to: None,
        };
        let error = Pasta::start_detached(
            Path::new("/bin/false"),
            Features::default(),
            &init,
            &[],
            &dir,
            None,
        )
        .unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            error.to_string().contains("ended before pasta started"),
            "{error}"
        );
    }

    /// A detached launcher joins the cgroup it's given first thing, by
    /// writing "0" to its `cgroup.procs` (here a pipe, read back), before
    /// it runs any code of its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_detached_launcher_joins_the_cgroup_it_is_given() {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let log = std::env::temp_dir().join(format!("shipyard-pasta-join-{}", std::process::id()));
        let launch = Launch {
            userns: -1,
            netns: -1,
            netns_file: PathBuf::from("/nonexistent/netns"),
            pasta: PathBuf::from("/bin/false"),
            follow: Follow::Caller,
            arguments: Vec::new(),
        };
        let mut command = linux::detached_launcher(
            &launch,
            [-1; 5],
            Some(write.as_raw_fd()),
            std::fs::File::create(&log).unwrap(),
        );
        // This test binary, re-executed with the launcher's arguments: it
        // refuses them, but the join came before.
        let _ = command.status().unwrap();
        drop(write);
        let mut written = String::new();
        std::fs::File::from(read)
            .read_to_string(&mut written)
            .unwrap();
        std::fs::remove_file(&log).unwrap();
        assert_eq!(written, "0");
    }

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
        let caller = Launch {
            userns: 5,
            netns: 6,
            netns_file: "/run/user/1000/shipyard/runs/a/netns".into(),
            pasta: "/usr/bin/pasta".into(),
            follow: Follow::Caller,
            arguments: vec!["--foreground".into(), "--netns".into(), "x".into()],
        };
        assert_eq!(Launch::from_args(&caller.to_args()), Ok(caller.clone()));
        for (pidfd, hold, lost_to) in [
            (
                Some(7),
                Some(9),
                Some("/home/u/.local/share/shipyard/containers/a/logs/0000000001"),
            ),
            (None, None, None),
        ] {
            let init = Launch {
                follow: Follow::Init {
                    pid: 4321,
                    start_time: 987_654,
                    pidfd,
                    ready: 8,
                    pid_file: "/run/user/1000/shipyard/runs/a/pasta.pid".into(),
                    hold,
                    lost_to: lost_to.map(PathBuf::from),
                },
                ..caller.clone()
            };
            assert_eq!(Launch::from_args(&init.to_args()), Ok(init));
        }
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
        let follow = |rest: &[&str]| {
            let mut all = vec!["5", "6", "/n", "/p", FOLLOW_INIT];
            all.extend(rest);
            if rest.len() == 5 {
                // No hold, unless the case gives one.
                all.push("-");
            }
            if rest.len() <= 6 {
                // Nowhere to say pasta's death, unless the case gives it.
                all.push("-");
            }
            all.push("--");
            args(&all)
        };
        for (bad, why) in [
            (follow(&["0", "9", "7", "8", "/p.pid"]), "no init's PID"),
            (
                follow(&["42", "x", "7", "8", "/p.pid"]),
                "no init's start time",
            ),
            (
                follow(&["42", "9", "2", "8", "/p.pid"]),
                "no init's pidfd descriptor",
            ),
            (
                follow(&["42", "9", "7", "-", "/p.pid"]),
                "no ready pipe descriptor",
            ),
            (
                follow(&["42", "9", "7", "8", "p.pid"]),
                "no absolute pasta's process ID file",
            ),
            // A pidfd or the pipe that's a namespace, or each other.
            (
                follow(&["42", "9", "5", "8", "/p.pid"]),
                "two of its descriptors are one",
            ),
            (
                follow(&["42", "9", "7", "7", "/p.pid"]),
                "two of its descriptors are one",
            ),
            (
                follow(&["42", "9", "-", "6", "/p.pid"]),
                "two of its descriptors are one",
            ),
            (
                follow(&["42", "9", "7", "8", "/p.pid", "1"]),
                "no hold descriptor",
            ),
            (
                follow(&["42", "9", "7", "8", "/p.pid", "8"]),
                "two of its descriptors are one",
            ),
            (
                follow(&["42", "9", "7", "8", "/p.pid", "-", "logs/1"]),
                "no absolute directory to say pasta's death in",
            ),
        ] {
            assert_eq!(Launch::from_args(&bad), Err(why.to_owned()), "{bad:?}");
        }
        assert!(Launch::from_args(&follow(&["42", "9", "-", "8", "/p.pid"])).is_ok());
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
