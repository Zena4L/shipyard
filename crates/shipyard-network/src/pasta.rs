//! Rootless networking with pasta (passt's namespace mode): one per
//! container.
//!
//! A rootless container can't have a bridge and a veth pair: those are the
//! host's devices, and changing them needs root. pasta gives it a network
//! another way. It joins the container's user and network namespaces (by its
//! init's PID: the user owns that user namespace, so it may), makes a tap
//! device there, and gives it the host's own address and routes
//! (`--config-net`). Every connection the container makes, pasta makes again
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
}

impl Features {
    /// The features `help` (pasta's `--help`) lists.
    #[must_use]
    pub fn from_help(help: &str) -> Features {
        Features {
            ns_ifname: help.contains("--ns-ifname"),
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

/// The container pasta serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Its init's PID: pasta joins its user and network namespaces.
    Pid(i32),
    /// Its network namespace alone: the container's user namespace is
    /// pasta's own already (one can't enter the namespace one is in).
    Netns(PathBuf),
}

/// pasta's arguments for the container `target`, publishing `ports`, with
/// its process ID file at `pid_file` (see the module).
#[must_use]
pub fn arguments(
    target: &Target,
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
    match target {
        Target::Pid(pid) => arguments.push(pid.to_string().into()),
        Target::Netns(path) => {
            arguments.push("--netns".into());
            arguments.push(path.into());
        }
    }
    arguments
}

#[cfg(target_os = "linux")]
pub use linux::Pasta;

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{Features, LOG_FILE, PID_FILE, PastaError, Target, arguments};
    use crate::ports::Published;

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
        /// waits until it has set the container's network up.
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
            let pid_file = dir.join(PID_FILE);
            let log_path = dir.join(LOG_FILE);
            let log = File::create(&log_path).map_err(|source| PastaError::Io {
                context: format!("making {}", log_path.display()),
                source,
            })?;
            // A container without a user namespace of its own (root's, or
            // one made inside another: `unshare -U -r`) shares pasta's.
            let user_namespace = |pid: &str| {
                use std::os::unix::fs::MetadataExt as _;
                std::fs::metadata(format!("/proc/{pid}/ns/user")).map(|ns| (ns.dev(), ns.ino()))
            };
            let target = match (user_namespace("self"), user_namespace(&pid.to_string())) {
                (Ok(ours), Ok(theirs)) if ours == theirs => {
                    Target::Netns(format!("/proc/{pid}/ns/net").into())
                }
                _ => Target::Pid(pid),
            };
            let parent = std::process::id();
            let mut command = Command::new(pasta);
            command
                .args(arguments(&target, ports, &pid_file, features))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                // A process group of its own: the terminal's signals (Ctrl-C)
                // are the container's to handle, not pasta's to die of while
                // the container stops.
                .process_group(0);
            // SAFETY: only async-signal-safe calls between fork and exec:
            // `prctl` and `getppid`, then `_exit`.
            unsafe {
                command.pre_exec(move || {
                    // pasta dies with shipyard, even killed outright: it
                    // never quits by itself (an older one) while the
                    // container's namespace is held.
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Shipyard died before the line above: it's too late.
                    if u32::try_from(libc::getppid()).ok() != Some(parent) {
                        libc::_exit(1);
                    }
                    Ok(())
                });
            }
            let child = command.spawn().map_err(|source| PastaError::Spawn {
                path: pasta.to_owned(),
                source,
            })?;
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

    impl Drop for Pasta {
        fn drop(&mut self) {
            match &self.pidfd {
                // ESRCH (gone already) is fine.
                Some(pidfd) => {
                    let _ = crate::pidfd::send_signal(pidfd, libc::SIGKILL);
                }
                // SAFETY: no memory involved. Before Linux 5.3 only: an
                // unreaped child's PID isn't reused, and the supervisor
                // only reaps it after the command (this runs before).
                None => unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                },
            }
            // Bounded: a pasta stuck in the kernel is left to the
            // supervisor's sweep rather than hang the run's end.
            let deadline = Instant::now() + REAP_WAIT;
            while !self.reaped() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
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
            &Target::Pid(42),
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
        assert!(text.ends_with("--pid /run/x/pasta.pid 42"), "{text}");
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
            &Target::Netns("/proc/7/ns/net".into()),
            &ports,
            Path::new("p"),
            Features { ns_ifname: true },
        )
        .join(std::ffi::OsStr::new(" "));
        let text = text.to_string_lossy();
        assert!(text.ends_with("--pid p --netns /proc/7/ns/net"), "{text}");
        assert!(text.contains("-t 8080:80 -u 127.0.0.1/5353:53"), "{text}");
        // An IPv6 address goes as it is, before the `/`.
        let v6 = arguments(
            &Target::Pid(1),
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
    }

    #[test]
    fn features_come_from_the_help() {
        assert!(Features::from_help("  -I, --ns-ifname NAME  ...").ns_ifname);
        assert!(!Features::from_help("-t, --tcp-ports SPEC").ns_ifname);
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
