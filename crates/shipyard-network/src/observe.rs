//! What a container's network is doing: its TCP and UDP sockets, read from
//! `/proc/<pid>/net/{tcp,tcp6,udp,udp6}`.
//!
//! Those files are the kernel's tables of one network namespace's sockets:
//! the namespace of process `<pid>` (since Linux 2.6.25). So a container's
//! sockets are read through its init's PID, with no `setns` and no
//! privilege beyond reading `/proc`.
//!
//! The format: a header line, then a line per socket:
//!
//! ```text
//!   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode …
//!    0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 31337 …
//! ```
//!
//! The first ten columns are the same in all four tables (UDP's adds its
//! own after them). An address is hex: IPv4's one 32-bit word, IPv6's four.
//! Each word is the kernel's bytes in memory, printed as a number in the
//! host's byte order (on a little-endian machine, 127.0.0.1 is `0100007F`):
//! a word is read as a number, and turned back into bytes in the host's
//! order. A port is its value, in hex. `st` is the kernel's TCP state; UDP
//! uses "established" for a connected socket and "close" for one that
//! isn't. `uid` and `inode` are decimal.
//!
//! A table is read line by line, keeping only the sockets asked for, so a
//! busy container's (thousands of connections) costs no more than what's
//! kept. Lines and their count are bounded, and nothing is indexed: what's
//! read is the kernel's, but the parser is fuzzed all the same
//! (`proc_net`). The kernel writes a table across several reads, so one
//! that changes meanwhile can list a socket twice, or miss one: what's read
//! is a close look, not an atomic one.

#![deny(clippy::indexing_slicing)]
// The parsing is the Linux reader's, and tested everywhere.
#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the reader is Linux's")
)]

use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use thiserror::Error;

/// The longest line read (the kernel's are 150 bytes, or about 180 for
/// IPv6, with room for its later columns).
const MAX_LINE: usize = 4096;

/// The most lines a table may have: far past a busy host's (each kept
/// socket is about 64 bytes; a line not kept costs nothing).
const MAX_LINES: usize = 1 << 20;

/// One of the tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    /// IPv4 TCP sockets.
    Tcp,
    /// IPv6 TCP sockets.
    Tcp6,
    /// IPv4 UDP sockets.
    Udp,
    /// IPv6 UDP sockets.
    Udp6,
}

impl Table {
    /// Every table, in the order they're read.
    pub const ALL: [Table; 4] = [Table::Tcp, Table::Tcp6, Table::Udp, Table::Udp6];

    /// Its file's name, under `/proc/<pid>/net`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Table::Tcp => "tcp",
            Table::Tcp6 => "tcp6",
            Table::Udp => "udp",
            Table::Udp6 => "udp6",
        }
    }

    /// Its sockets' protocol.
    #[must_use]
    pub fn protocol(self) -> Protocol {
        match self {
            Table::Tcp | Table::Tcp6 => Protocol::Tcp,
            Table::Udp | Table::Udp6 => Protocol::Udp,
        }
    }

    /// Whether it may be missing: IPv6's, on a kernel without IPv6.
    fn optional(self) -> bool {
        matches!(self, Table::Tcp6 | Table::Udp6)
    }
}

/// A socket's transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

/// A socket's state: the kernel's TCP states (UDP uses `Established` for a
/// connected socket, and `Close` for one that isn't).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum State {
    /// Connected.
    Established,
    /// Connecting.
    SynSent,
    /// Being connected to (a request not yet accepted is listed so too).
    SynRecv,
    /// Closing (this side first).
    FinWait1,
    /// Closing, waiting for the other side.
    FinWait2,
    /// Closed, waiting for stray packets.
    TimeWait,
    /// Not connected.
    Close,
    /// Closing (the other side first).
    CloseWait,
    /// Closing, waiting for the last acknowledgement.
    LastAck,
    /// Listening.
    Listen,
    /// Both sides closing at once.
    Closing,
    /// The kernel's state for a request (the tables list those as
    /// `SynRecv`).
    NewSynRecv,
    /// Another number.
    Other(u8),
}

impl State {
    /// The state numbered `number` (`include/net/tcp_states.h`).
    fn from_number(number: u8) -> State {
        match number {
            1 => State::Established,
            2 => State::SynSent,
            3 => State::SynRecv,
            4 => State::FinWait1,
            5 => State::FinWait2,
            6 => State::TimeWait,
            7 => State::Close,
            8 => State::CloseWait,
            9 => State::LastAck,
            10 => State::Listen,
            11 => State::Closing,
            12 => State::NewSynRecv,
            other => State::Other(other),
        }
    }
}

/// One socket, as a table lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Socket {
    /// Its protocol.
    pub protocol: Protocol,
    /// Its own address and port.
    pub local: SocketAddr,
    /// The other side's (unspecified, port 0, for one that isn't connected).
    pub remote: SocketAddr,
    /// Its state.
    pub state: State,
    /// Its owner, as the reader's user namespace sees it (one it doesn't
    /// map shows as the overflow ID, 65534; a connection in `TimeWait`, or
    /// a request, as 0).
    pub uid: u32,
    /// Its inode: `/proc/<pid>/fd` links of `socket:[N]` name it (0 for a
    /// connection in `TimeWait`, or a request: no socket file).
    pub inode: u64,
}

impl Socket {
    /// Whether it waits for others: a TCP socket listening, or a UDP one
    /// bound and not connected. (A UDP client that only sends, unconnected,
    /// has a port of its own too, and looks the same: the tables can't
    /// tell them apart.)
    #[must_use]
    pub fn is_listening(&self) -> bool {
        match self.protocol {
            Protocol::Tcp => self.state == State::Listen,
            Protocol::Udp => self.state == State::Close,
        }
    }
}

/// What can go wrong reading sockets.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObserveError {
    /// A line that isn't one the kernel writes.
    #[error("line {line} of the {table} table can't be read: {reason}")]
    Malformed {
        /// Which table.
        table: &'static str,
        /// Its number (1: the header).
        line: usize,
        /// What's wrong.
        reason: &'static str,
    },
    /// A table too large to read (more lines, or longer ones, than any
    /// the kernel writes).
    #[error("the {0} table is too large to read")]
    TooLarge(&'static str),
    /// Not a process's ID.
    #[error("{0} isn't a process's ID")]
    NotAProcess(i32),
    /// The process is gone.
    #[error("process {0} is gone")]
    Gone(i32),
    /// The process runs, but its `/proc` entry is hidden from this user
    /// (`/proc` mounted with `hidepid`).
    #[error("process {0}'s /proc entry is hidden from this user (hidepid)")]
    Hidden(i32),
    /// A file couldn't be read.
    #[error("reading {path}: {source}")]
    Io {
        /// Which.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// The sockets `table` lists, from its `text`.
pub fn parse(text: &str, table: Table) -> Result<Vec<Socket>, ObserveError> {
    read(text.as_bytes(), table, |_| true)
}

/// The sockets `table` lists, from `reader`, that `keep` keeps (each read,
/// kept or not, and let go).
pub fn read(
    mut reader: impl BufRead,
    table: Table,
    keep: impl Fn(&Socket) -> bool,
) -> Result<Vec<Socket>, ObserveError> {
    use std::io::Read as _;
    let name = table.name();
    let mut sockets = Vec::new();
    let mut line = Vec::with_capacity(256);
    // One byte past the longest line tells a line that's too long.
    let limit = u64::try_from(MAX_LINE)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    for number in 1.. {
        line.clear();
        let read = (&mut reader)
            .take(limit)
            .read_until(b'\n', &mut line)
            .map_err(|source| ObserveError::Io {
                path: name.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_LINE || number > MAX_LINES {
            return Err(ObserveError::TooLarge(name));
        }
        // The first line is the header.
        if number == 1 {
            continue;
        }
        let malformed = |reason| ObserveError::Malformed {
            table: name,
            line: number,
            reason,
        };
        let text = std::str::from_utf8(&line).map_err(|_| malformed("it isn't text"))?;
        if text.trim().is_empty() {
            continue;
        }
        let socket = parse_line(text, table.protocol()).map_err(malformed)?;
        if keep(&socket) {
            sockets.push(socket);
        }
    }
    Ok(sockets)
}

/// One table line's socket.
fn parse_line(line: &str, protocol: Protocol) -> Result<Socket, &'static str> {
    let mut fields = line.split_whitespace();
    let mut next = |what| fields.next().ok_or(what);
    // `sl`: the line's number, with a colon.
    next("no number")?;
    let local = parse_address(next("no local address")?)?;
    let remote = parse_address(next("no remote address")?)?;
    let state = u8::from_str_radix(next("no state")?, 16).map_err(|_| "a state isn't hex")?;
    // `tx_queue:rx_queue`, `tr:tm->when`, `retrnsmt`.
    for what in ["no queues", "no timer", "no retransmits"] {
        next(what)?;
    }
    let uid = next("no owner")?
        .parse()
        .map_err(|_| "an owner isn't a number")?;
    next("no timeout")?;
    let inode = next("no inode")?
        .parse()
        .map_err(|_| "an inode isn't a number")?;
    Ok(Socket {
        protocol,
        local,
        remote,
        state: State::from_number(state),
        uid,
        inode,
    })
}

/// An `ADDRESS:PORT` field.
fn parse_address(field: &str) -> Result<SocketAddr, &'static str> {
    let (address, port) = field.split_once(':').ok_or("an address has no port")?;
    let port = u16::from_str_radix(port, 16).map_err(|_| "a port isn't hex")?;
    if !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("an address isn't hex");
    }
    let ip = match address.len() {
        8 => IpAddr::V4(Ipv4Addr::from(host_word(address)?)),
        32 => {
            let mut octets = [0u8; 16];
            for (chunk, word) in octets.as_chunks_mut::<4>().0.iter_mut().zip(0..4) {
                let digits = address
                    .get(word * 8..word * 8 + 8)
                    .ok_or("a short address")?;
                *chunk = host_word(digits)?;
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return Err("an address is neither IPv4's nor IPv6's width"),
    };
    Ok(SocketAddr::new(ip, port))
}

/// A word of an address: 8 hex digits, a number in the host's byte order
/// whose bytes in memory are the address's.
fn host_word(digits: &str) -> Result<[u8; 4], &'static str> {
    u32::from_str_radix(digits, 16)
        .map(u32::to_ne_bytes)
        .map_err(|_| "an address isn't hex")
}

#[cfg(target_os = "linux")]
pub use linux::{listening, sockets};

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::io::BufReader;
    use std::os::fd::OwnedFd;

    use nix::errno::Errno;
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;

    use super::{ObserveError, Socket, Table, read};

    /// Every TCP and UDP socket of the network namespace process `pid` is
    /// in (a container's, through its init's PID), each once.
    ///
    /// `pid` is numbered in the PID namespace of the `/proc` this process
    /// sees (the host's, for the CLI): a reader in another one reads
    /// another process. Its tables are all read through one handle on
    /// `/proc/<pid>/net`, so a PID reused meanwhile can't mix two
    /// namespaces' tables; and a process that's gone by the end of the read
    /// is [`Gone`](ObserveError::Gone), never an empty or partial answer.
    pub fn sockets(pid: i32) -> Result<Vec<Socket>, ObserveError> {
        let mut sockets = read_all(pid, |_| true)?;
        // A table changing as it's read can list a socket twice. (Those
        // without a socket file, inode 0, can't be told apart: kept.)
        let mut seen = HashSet::new();
        sockets.retain(|socket| socket.inode == 0 || seen.insert((socket.protocol, socket.inode)));
        Ok(sockets)
    }

    /// The sockets of process `pid`'s network namespace that wait for
    /// others ([`Socket::is_listening`]), sorted, each once: only those are
    /// kept as the tables are read (see [`sockets`]).
    pub fn listening(pid: i32) -> Result<Vec<Socket>, ObserveError> {
        let mut listening = read_all(pid, Socket::is_listening)?;
        listening.sort();
        listening.dedup();
        Ok(listening)
    }

    /// Every table's sockets that `keep` keeps.
    fn read_all(
        pid: i32,
        keep: impl Fn(&Socket) -> bool + Copy,
    ) -> Result<Vec<Socket>, ObserveError> {
        if pid <= 0 {
            return Err(ObserveError::NotAProcess(pid));
        }
        // Held from before the read to after: whether the same process is
        // still there at the end (before Linux 5.3, unchecked).
        let pidfd = match crate::pidfd::open(pid) {
            Ok(pidfd) => Some(pidfd),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                return Err(ObserveError::Gone(pid));
            }
            Err(_) => None,
        };
        let path = format!("/proc/{pid}/net");
        let dir = open(
            path.as_str(),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| missing(pid, &path, errno))?;
        let mut all = Vec::new();
        for table in Table::ALL {
            let file = match openat(
                &dir,
                table.name(),
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => file,
                // No IPv6 in this kernel.
                Err(Errno::ENOENT) if table.optional() => continue,
                // A process gone (a zombie's has no namespace left), or one
                // whose namespace is being torn down.
                Err(Errno::ENOENT | Errno::ENXIO) => return Err(ObserveError::Gone(pid)),
                Err(errno) => {
                    return Err(ObserveError::Io {
                        path: format!("{path}/{}", table.name()),
                        source: std::io::Error::from(errno),
                    });
                }
            };
            all.extend(read(
                BufReader::new(std::fs::File::from(file)),
                table,
                keep,
            )?);
        }
        if let Some(pidfd) = &pidfd {
            still_there(pid, pidfd)?;
        }
        Ok(all)
    }

    /// What `/proc/<pid>/net` (`path`) not opening (`errno`) means: the
    /// process is gone, or its entry is hidden from this user (`hidepid`:
    /// it's there, but not to be signalled by this user), or another
    /// error.
    fn missing(pid: i32, path: &str, errno: Errno) -> ObserveError {
        match errno {
            Errno::ENOENT | Errno::ESRCH => {
                // SAFETY: signal 0 sends nothing; it only checks.
                let checked = unsafe { libc::kill(pid, 0) };
                if checked == -1 && Errno::last() == Errno::EPERM {
                    ObserveError::Hidden(pid)
                } else {
                    ObserveError::Gone(pid)
                }
            }
            errno => ObserveError::Io {
                path: path.to_owned(),
                source: std::io::Error::from(errno),
            },
        }
    }

    /// [`ObserveError::Gone`] if the process `pidfd` is has ended.
    fn still_there(pid: i32, pidfd: &OwnedFd) -> Result<(), ObserveError> {
        match crate::pidfd::send_signal(pidfd, 0) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Err(ObserveError::Gone(pid)),
            // Running (EPERM: someone else's, running).
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when \
                          retrnsmt   uid  timeout inode";

    /// A line as the kernel writes it, with `local`, `remote`, `state`.
    fn line(local: &str, remote: &str, state: &str) -> String {
        format!(
            "   0: {local} {remote} {state} 00000000:00000000 00:00000000 00000000  1000        0 \
             31337 1 0000000000000000 100 0 0 10 0"
        )
    }

    /// The hex the kernel writes for an IPv4 address on this machine.
    fn hex4(ip: Ipv4Addr) -> String {
        format!("{:08X}", u32::from_ne_bytes(ip.octets()))
    }

    /// The same for IPv6: four words.
    fn hex6(ip: Ipv6Addr) -> String {
        use std::fmt::Write as _;
        ip.octets()
            .as_chunks::<4>()
            .0
            .iter()
            .fold(String::new(), |mut hex, word| {
                let _ = write!(hex, "{:08X}", u32::from_ne_bytes(*word));
                hex
            })
    }

    #[test]
    fn a_listener_and_a_connection_read_back() {
        let localhost = hex4(Ipv4Addr::LOCALHOST);
        let text = [
            HEADER.to_owned(),
            line(&format!("{localhost}:1F90"), "00000000:0000", "0A"),
            line(
                &format!("{}:0050", hex4(Ipv4Addr::new(10, 89, 0, 2))),
                &format!("{}:D431", hex4(Ipv4Addr::new(192, 0, 2, 9))),
                "01",
            ),
        ]
        .join("\n");
        let sockets = parse(&text, Table::Tcp).unwrap();
        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].local, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(sockets[0].remote, "0.0.0.0:0".parse().unwrap());
        assert_eq!(sockets[0].state, State::Listen);
        assert_eq!(sockets[0].uid, 1000);
        assert_eq!(sockets[0].inode, 31337);
        assert!(sockets[0].is_listening());
        assert_eq!(sockets[1].remote, "192.0.2.9:54321".parse().unwrap());
        assert_eq!(sockets[1].state, State::Established);
        assert!(!sockets[1].is_listening());
        // Kept as they're read.
        let kept = read(text.as_bytes(), Table::Tcp, Socket::is_listening).unwrap();
        assert_eq!(kept, [sockets[0]]);
    }

    /// Lines as a little-endian kernel writes them (from the test
    /// container's tables): what the conversions must undo.
    #[test]
    fn the_kernels_own_lines_read_as_they_mean() {
        if !cfg!(target_endian = "little") {
            return;
        }
        let tcp = format!(
            "{HEADER}\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 \
             00000000  1000        0 40717 1 0000000000000000 100 0 0 10 0\n"
        );
        let socket = parse(&tcp, Table::Tcp).unwrap()[0];
        assert_eq!(socket.local, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(socket.inode, 40717);
        let tcp6 = format!(
            "{HEADER}\n   0: 00000000000000000000000001000000:0016 \
             00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     \
             0        0 21321 1 0000000000000000 100 0 0 10 0\n"
        );
        let socket = parse(&tcp6, Table::Tcp6).unwrap()[0];
        assert_eq!(socket.local, "[::1]:22".parse().unwrap());
        let udp = format!(
            "{HEADER}\n  531: 0100590A:0035 00000000:0000 07 00000000:00000000 00:00000000 \
             00000000     0        0 9182 2 0000000000000000 0\n"
        );
        let socket = parse(&udp, Table::Udp).unwrap()[0];
        assert_eq!(socket.local, "10.89.0.1:53".parse().unwrap());
        assert_eq!(socket.protocol, Protocol::Udp);
        assert!(socket.is_listening());
    }

    #[test]
    fn ipv6_addresses_are_four_words() {
        let ip: Ipv6Addr = "fd00:1::2".parse().unwrap();
        let text = [
            HEADER.to_owned(),
            line(
                &format!("{}:0050", hex6(ip)),
                &format!("{}:0000", hex6(Ipv6Addr::UNSPECIFIED)),
                "0A",
            ),
        ]
        .join("\n");
        let sockets = parse(&text, Table::Tcp6).unwrap();
        assert_eq!(sockets[0].local, SocketAddr::new(IpAddr::V6(ip), 80));
    }

    #[test]
    fn udp_listens_unconnected() {
        let any = hex4(Ipv4Addr::UNSPECIFIED);
        let text = [
            HEADER.to_owned(),
            line(&format!("{any}:0035"), "00000000:0000", "07"),
            line(
                &format!("{any}:9C40"),
                &format!("{}:0035", hex4(Ipv4Addr::new(10, 0, 0, 1))),
                "01",
            ),
        ]
        .join("\n");
        let sockets = parse(&text, Table::Udp).unwrap();
        assert!(
            sockets
                .iter()
                .all(|socket| socket.protocol == Protocol::Udp)
        );
        let listening: Vec<bool> = sockets.iter().map(Socket::is_listening).collect();
        assert_eq!(listening, [true, false]);
    }

    #[test]
    fn an_empty_table_has_no_sockets() {
        assert!(parse(HEADER, Table::Tcp).unwrap().is_empty());
        assert!(parse("", Table::Udp6).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_say_which() {
        for (bad, reason) in [
            ("   0: 0100007F", "an address has no port"),
            (
                "   0: 0100007G:0016 00000000:0000 0A",
                "an address isn't hex",
            ),
            (
                "   0: 0100007F00:0016 00000000:0000 0A",
                "an address is neither",
            ),
            ("   0: 0100007F:0016 00000000:0000", "no state"),
            ("   0: 0100007F:XX16 00000000:0000 0A", "a port isn't hex"),
        ] {
            let text = format!("{HEADER}\n{bad}");
            let error = parse(&text, Table::Tcp).unwrap_err();
            assert!(
                matches!(&error, ObserveError::Malformed { line: 2, reason: r, .. } if r.starts_with(reason)),
                "{bad}: {error}"
            );
        }
        // The rest of a line, cut short.
        let short = line("0100007F:0016", "00000000:0000", "0A");
        let cut = &short[..short.len() / 2];
        assert!(parse(&format!("{HEADER}\n{cut}"), Table::Tcp).is_err());
    }

    #[test]
    fn a_line_too_long_or_too_many_is_refused() {
        let long = format!("{HEADER}\n{}", "0".repeat(MAX_LINE + 1));
        assert!(matches!(
            parse(&long, Table::Tcp),
            Err(ObserveError::TooLarge("tcp"))
        ));
        // Empty lines are the cheapest: past the count, refused all the
        // same; up to it, fine.
        let many = "\n".repeat(MAX_LINES + 1);
        assert!(matches!(
            parse(&many, Table::Udp),
            Err(ObserveError::TooLarge("udp"))
        ));
        assert!(
            parse(&"\n".repeat(MAX_LINES), Table::Udp)
                .unwrap()
                .is_empty()
        );
    }

    proptest::proptest! {
        /// A real line with a byte changed anywhere: an error or a socket,
        /// never a panic.
        #[test]
        fn a_line_changed_anywhere_never_panics(at in 0usize..200, byte in proptest::prelude::any::<u8>()) {
            let mut bytes = format!("{HEADER}\n{}", line("0100007F:1F90", "00000000:0000", "0A")).into_bytes();
            if let Some(place) = bytes.get_mut(HEADER.len() + 1 + at) {
                *place = byte;
            }
            for table in Table::ALL {
                let _ = read(&bytes[..], table, |_| true);
            }
        }
    }

    /// This process's own sockets, found in its network namespace's tables
    /// by their inodes (`/proc/self/fd`'s `socket:[N]` links).
    #[cfg(target_os = "linux")]
    #[test]
    fn this_processs_sockets_are_found() {
        use std::os::fd::AsRawFd as _;
        let inode = |fd: std::os::fd::RawFd| -> u64 {
            let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
            link.to_str()
                .and_then(|link| link.strip_prefix("socket:["))
                .and_then(|link| link.strip_suffix(']'))
                .unwrap()
                .parse()
                .unwrap()
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(at).unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let listener6 = std::net::TcpListener::bind("[::1]:0").ok();
        let pid = i32::try_from(std::process::id()).unwrap();
        let all = sockets(pid).unwrap();
        let find = |fd| {
            let inode = inode(fd);
            *all.iter()
                .find(|socket| socket.inode == inode)
                .unwrap_or_else(|| panic!("no socket with inode {inode}: {all:?}"))
        };
        let me = nix::unistd::getuid().as_raw();
        let listening_one = find(listener.as_raw_fd());
        assert_eq!(listening_one.state, State::Listen);
        assert_eq!(listening_one.local, at);
        assert_eq!(listening_one.uid, me);
        let connected = find(client.as_raw_fd());
        assert_eq!(connected.state, State::Established);
        assert_eq!(connected.remote, at);
        let bound = find(udp.as_raw_fd());
        assert_eq!(bound.protocol, Protocol::Udp);
        assert!(bound.is_listening());
        if let Some(listener6) = &listener6 {
            let six = find(listener6.as_raw_fd());
            assert_eq!(six.local, listener6.local_addr().unwrap());
        }
        let waiting = listening(pid).unwrap();
        assert!(waiting.contains(&listening_one));
        assert!(!waiting.contains(&connected));
        // Each once (by inode).
        let mut inodes: Vec<u64> = all
            .iter()
            .map(|socket| socket.inode)
            .filter(|&inode| inode != 0)
            .collect();
        let count = inodes.len();
        inodes.sort_unstable();
        inodes.dedup();
        assert_eq!(inodes.len(), count);
        // Not a process, or gone.
        assert!(matches!(sockets(0), Err(ObserveError::NotAProcess(0))));
        assert!(matches!(sockets(i32::MAX), Err(ObserveError::Gone(_))));
    }
}
