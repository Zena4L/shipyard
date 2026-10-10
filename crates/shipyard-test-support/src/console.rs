//! A console socket, as a shim makes one for runc's `--console-socket`: a
//! UNIX socket the runtime connects to, on which the container's init sends
//! its terminal's master (`SCM_RIGHTS`, TLPI §61.13.3). What runc's own
//! `recvtty` does, for one container.

use std::io::{IoSliceMut, Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};

use crate::TIMEOUT;

/// The most a name sent with a terminal may take (runc's `MaxNameLen`).
const MAX_NAME_LEN: usize = 4096;

/// A listening console socket in a directory of its own; removed on drop.
#[derive(Debug)]
pub struct ConsoleSocket {
    dir: PathBuf,
    path: PathBuf,
    listener: UnixListener,
}

/// A terminal received on a console socket.
#[derive(Debug)]
pub struct Console {
    /// The pseudo-terminal's master: what's written to it is typed into the
    /// container, and the container's output comes out of it.
    pub master: std::fs::File,
    /// The name it came with (Shipyard sends `/dev/ptmx`; runc the path it
    /// opened).
    pub name: Vec<u8>,
    /// The connection it came on, still open on this side.
    pub connection: UnixStream,
    output: Vec<u8>,
}

impl ConsoleSocket {
    /// A console socket at a short path in the temporary directory.
    #[must_use]
    pub fn new() -> Self {
        Self::in_subdirectory(None)
    }

    /// A console socket at a path longer than a socket address can hold
    /// (`sun_path`'s 108 bytes): in a directory with a long name.
    #[must_use]
    pub fn with_a_long_path() -> Self {
        Self::in_subdirectory(Some(&"d".repeat(120)))
    }

    fn in_subdirectory(subdirectory: Option<&str>) -> Self {
        static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-console-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let parent = subdirectory.map_or_else(|| dir.clone(), |sub| dir.join(sub));
        std::fs::create_dir_all(&parent).unwrap();
        let path = parent.join("console.sock");
        // A long path is bound through its directory, as the runtime
        // connects to it.
        let opened = std::fs::File::open(&parent).unwrap();
        let listener =
            UnixListener::bind(format!("/proc/self/fd/{}/console.sock", opened.as_raw_fd()))
                .unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            dir,
            path,
            listener,
        }
    }

    /// Where it is: `--console-socket`'s argument.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Waits (up to [`TIMEOUT`]) for a runtime to connect and send a
    /// terminal.
    ///
    /// # Panics
    ///
    /// If nothing connects, or what comes isn't one descriptor.
    #[must_use]
    pub fn receive(&self) -> Console {
        let deadline = Instant::now() + TIMEOUT;
        let connection = loop {
            match self.listener.accept() {
                Ok((connection, _)) => break connection,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "nothing connected to the console socket"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("accepting on the console socket: {err}"),
            }
        };
        connection.set_nonblocking(false).unwrap();
        connection.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut name = vec![0_u8; MAX_NAME_LEN];
        let mut space = nix::cmsg_space!([std::os::fd::RawFd; 1]);
        let (fds, length) = {
            let mut iov = [IoSliceMut::new(&mut name)];
            let message = recvmsg::<()>(
                connection.as_raw_fd(),
                &mut iov,
                Some(&mut space),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )
            .expect("receiving the terminal");
            let fds: Vec<_> = message
                .cmsgs()
                .unwrap()
                .filter_map(|cmsg| match cmsg {
                    ControlMessageOwned::ScmRights(fds) => Some(fds),
                    _ => None,
                })
                .flatten()
                .collect();
            (fds, message.bytes)
        };
        assert_eq!(fds.len(), 1, "one descriptor, the master: {fds:?}");
        name.truncate(length);
        // SAFETY: just received, this process's alone.
        let master = unsafe { std::fs::File::from(OwnedFd::from_raw_fd(fds[0])) };
        Console {
            master,
            name,
            connection,
            output: Vec::new(),
        }
    }
}

impl Default for ConsoleSocket {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ConsoleSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Console {
    /// The pseudo-terminal's number (`TIOCGPTN`): only a master has one.
    ///
    /// # Panics
    ///
    /// If it isn't a pseudo-terminal's master.
    #[must_use]
    pub fn pty_number(&self) -> u32 {
        let mut number: libc::c_uint = 0;
        // SAFETY: `TIOCGPTN` writes one `unsigned int`.
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGPTN, &raw mut number) };
        assert_eq!(
            rc,
            0,
            "not a pty master: {}",
            std::io::Error::last_os_error()
        );
        number
    }

    /// Types `keys` into the container's terminal.
    ///
    /// # Panics
    ///
    /// If the terminal is gone.
    pub fn type_keys(&mut self, keys: &[u8]) {
        self.master
            .write_all(keys)
            .expect("typing into the terminal");
    }

    /// Waits (up to [`TIMEOUT`]) until `text` is in the terminal's output.
    ///
    /// # Panics
    ///
    /// If it never comes.
    pub fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.output().contains(text) {
            assert!(
                Instant::now() < deadline,
                "never saw {text:?}: {:?}",
                self.output()
            );
            if !self.pump(deadline) {
                assert!(
                    self.output().contains(text),
                    "the terminal closed before {text:?}: {:?}",
                    self.output()
                );
            }
        }
    }

    /// Reads whatever the terminal puts out for `how_long`: for asserting
    /// what did *not* come.
    pub fn read_for(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while Instant::now() < deadline && self.pump(deadline) {}
    }

    /// Reads what's there, waiting at most until `deadline` (and 100 ms).
    /// False once the terminal is closed.
    fn pump(&mut self, deadline: Instant) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        let wait = u16::try_from(left.as_millis().min(100)).unwrap_or(100);
        let mut fds = [nix::poll::PollFd::new(
            self.master.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        let _ = nix::poll::poll(&mut fds, wait);
        if fds[0].revents().is_none_or(|events| events.is_empty()) {
            return true;
        }
        let mut buf = [0_u8; 4096];
        match self.master.read(&mut buf) {
            Ok(0) | Err(_) => false,
            Ok(read) => {
                self.output.extend_from_slice(&buf[..read]);
                true
            }
        }
    }

    /// Everything the terminal has put out so far.
    #[must_use]
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    /// Whether the connection was closed by the other side (end-of-file
    /// within [`TIMEOUT`]): the runtime and the container's init let go of it.
    ///
    /// # Panics
    ///
    /// If reading it fails.
    #[must_use]
    pub fn connection_closed(&mut self) -> bool {
        let mut byte = [0_u8; 1];
        match self.connection.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => false,
            Err(err) => panic!("reading the console connection: {err}"),
        }
    }
}
