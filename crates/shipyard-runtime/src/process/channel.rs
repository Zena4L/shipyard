//! The pipes between shipyard and a child it's starting (TLPI §28.3
//! describes the idea).
//!
//! - **The report pipe** (child → parent): how a failure between `fork` and
//!   `exec` reaches the parent.
//! - **The go pipe** (parent → child), the setup handshake: the child waits
//!   for one byte before setting up, so the parent can finish its own part
//!   first. The parent keeps its end open until `exec` resolves, which also
//!   tells the child whether the parent is still alive (see `child.rs`).
//! - **The status pipe** (container init → parent): the command's raw wait
//!   status, which the init can't pass on as its own (see `init.rs`).
//!
//! Both ends are close-on-exec. If `exec` succeeds, the kernel closes the
//! child's write end and the parent reads end-of-file. If it fails, the child
//! writes one 16-byte record first. 16 bytes is far below `PIPE_BUF`, so the
//! write is atomic: the parent sees all of it or none of it.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, fcntl};

use super::failure::{ChildFailure, ChildStep};
use crate::{Error, Result};

/// Size of one failure record.
pub(crate) const RECORD_LEN: usize = 16;

/// Creates the report pipe, returning `(read, write)`. Both ends are
/// close-on-exec, and the child's (write) end is at fd 3 or above.
pub(crate) fn report_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let (read, write) = cloexec_pipe()?;
    Ok((read, above_stdio(write)?))
}

/// Creates the go pipe, returning `(read, write)`. Both ends are
/// close-on-exec and at fd 3 or above: the child holds both until it closes
/// the parent's copy.
pub(crate) fn go_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let (read, write) = cloexec_pipe()?;
    Ok((above_stdio(read)?, above_stdio(write)?))
}

/// Creates the status pipe, returning `(read, write)`: close-on-exec, above
/// stdio, and both ends non-blocking. Reading it never hangs, and neither
/// does the init's writing: whatever is in the container can reach the
/// pipe through `/proc/1/fd` and fill it, and a blocked init would stop
/// forwarding signals and reaping.
#[cfg(target_os = "linux")]
pub(crate) fn status_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let (read, write) = cloexec_pipe()?;
    let read = above_stdio(read)?;
    let write = above_stdio(write)?;
    set_nonblocking(&read)?;
    set_nonblocking(&write)?;
    Ok((read, write))
}

/// Makes reads from and writes to `pipe` return EAGAIN instead of blocking.
fn set_nonblocking(pipe: &OwnedFd) -> Result<()> {
    let flags =
        fcntl(pipe, FcntlArg::F_GETFL).map_err(|e| Error::sys("reading a pipe's flags", e))?;
    let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
    fcntl(pipe, FcntlArg::F_SETFL(flags))
        .map_err(|e| Error::sys("making a pipe non-blocking", e))?;
    Ok(())
}

// The status pipe, from the container's clean image to shipyard. It sends
// [`START_MARK`] as soon as it runs; then, as the init, one status record
// (the raw wait status) when the command has exited, or [`GAVE_UP_MARK`] if
// it can't wait for it. Nothing else, ever: anything more or different was
// written by someone else (see `read_init_report`).

/// Size of a status record: one raw wait status.
pub(crate) const STATUS_LEN: usize = 4;

/// Size of a complete report: the start mark and the status record.
const REPORT_LEN: usize = 1 + STATUS_LEN;

/// The clean image's first byte on the status pipe: it runs.
pub(crate) const START_MARK: u8 = b'R';

/// The init's last byte on the status pipe when a system call its loop needs
/// fails (a seccomp profile that refuses it): it can't wait for the command,
/// so it ends the container.
pub(crate) const GAVE_UP_MARK: u8 = b'X';

/// Encodes a raw wait status. Runs in the init (Linux), so it only moves
/// bytes.
#[cfg(target_os = "linux")]
pub(crate) fn encode_status(raw: libc::c_int) -> [u8; STATUS_LEN] {
    raw.to_le_bytes()
}

/// What the status pipe said, read once the init has exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitReport {
    /// Nothing: the init never got to its loop.
    Nothing,
    /// It started, but sent no status: there was no init (the command was
    /// the process shipyard reaped), or the init was killed first.
    Started,
    /// It started, and the command ended with this raw wait status.
    Finished(libc::c_int),
    /// It started, then couldn't wait for the command and gave up.
    GaveUp,
    /// Bytes that aren't the init's: forged or garbled. Not to be trusted.
    Unexpected,
}

/// Reads what the init sent, without blocking, and accepts only exactly
/// what the init sends.
pub(crate) fn read_init_report(pipe: &OwnedFd) -> InitReport {
    // Room for one byte more than a full report, to notice extra bytes.
    let mut buf = [0u8; REPORT_LEN + 1];
    let mut len = 0;
    while let Some(free) = buf.get_mut(len..).filter(|free| !free.is_empty()) {
        match nix::unistd::read(pipe.as_fd(), free) {
            Ok(0) | Err(Errno::EAGAIN) => break,
            Ok(n) => len += n,
            Err(Errno::EINTR) => {}
            // Unreadable: nothing in it can be trusted either.
            Err(_) => return InitReport::Unexpected,
        }
    }
    let Some(report) = buf.get(..len) else {
        return InitReport::Unexpected;
    };
    match report {
        [] => InitReport::Nothing,
        [START_MARK] => InitReport::Started,
        [START_MARK, GAVE_UP_MARK] => InitReport::GaveUp,
        [START_MARK, record @ ..] if report.len() == REPORT_LEN => {
            <[u8; STATUS_LEN]>::try_from(record).map_or(InitReport::Unexpected, |raw| {
                InitReport::Finished(libc::c_int::from_le_bytes(raw))
            })
        }
        _ => InitReport::Unexpected,
    }
}

/// Tells the child to go ahead. If the child already died, the write fails
/// (EPIPE): that's fine, its exit is reported the usual way.
pub(crate) fn send_go(pipe: &OwnedFd) {
    loop {
        match nix::unistd::write(pipe, b"g") {
            Err(Errno::EINTR) => {}
            _ => return,
        }
    }
}

#[cfg(target_os = "linux")]
fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .map_err(|e| Error::sys("creating a pipe to the child", e))
}

/// macOS has no `pipe2`, so the flag is set separately. Another thread forking
/// in between could inherit the pipe; spawning only from the main thread of a
/// single-threaded process avoids that.
#[cfg(not(target_os = "linux"))]
fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    use nix::fcntl::FdFlag;
    let (read, write) =
        nix::unistd::pipe().map_err(|e| Error::sys("creating a pipe to the child", e))?;
    for fd in [&read, &write] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
            .map_err(|e| Error::sys("marking a pipe to the child close-on-exec", e))?;
    }
    Ok((read, write))
}

/// Moves `fd` to 3 or above if it landed on 0–2 (possible when a library
/// caller closed its standard descriptors). There, the command would inherit
/// our pipe as its stdin, stdout or stderr.
fn above_stdio(fd: OwnedFd) -> Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    let moved = fcntl(&fd, FcntlArg::F_DUPFD_CLOEXEC(3))
        .map_err(|e| Error::sys("moving a pipe above the standard descriptors", e))?;
    // SAFETY: `F_DUPFD_CLOEXEC` just returned this new descriptor, and nothing
    // else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

/// Encodes a failure record. Runs in the child after `fork`, so it only moves
/// bytes around: no allocation, indexing or arithmetic that could panic.
pub(crate) fn encode(step: ChildStep, errno: i32, detail: u64) -> [u8; RECORD_LEN] {
    let [s0, s1] = (step as u16).to_le_bytes();
    let [e0, e1, e2, e3] = errno.to_le_bytes();
    let [d0, d1, d2, d3, d4, d5, d6, d7] = detail.to_le_bytes();
    [s0, s1, 0, 0, e0, e1, e2, e3, d0, d1, d2, d3, d4, d5, d6, d7]
}

fn decode(record: [u8; RECORD_LEN]) -> Option<ChildFailure> {
    let [s0, s1, _, _, e0, e1, e2, e3, d0, d1, d2, d3, d4, d5, d6, d7] = record;
    Some(ChildFailure {
        step: ChildStep::from_raw(u16::from_le_bytes([s0, s1]))?,
        errno: Errno::from_raw(i32::from_le_bytes([e0, e1, e2, e3])),
        detail: u64::from_le_bytes([d0, d1, d2, d3, d4, d5, d6, d7]),
    })
}

/// What the report pipe has said so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Report {
    /// Nothing conclusive yet.
    Pending,
    /// End-of-file with no record: `exec` succeeded.
    Running,
    /// The child reported a failure.
    Failed(ChildFailure),
}

/// Reads the report pipe without blocking, collecting the record across
/// calls. A blocking read would let a child that stopped half-way (SIGSTOP
/// can't be blocked) freeze the supervisor.
#[derive(Debug)]
pub(crate) struct ReportReader {
    pipe: OwnedFd,
    record: [u8; RECORD_LEN],
    filled: usize,
}

impl ReportReader {
    pub(crate) fn new(pipe: OwnedFd) -> Result<Self> {
        set_nonblocking(&pipe)?;
        Ok(Self {
            pipe,
            record: [0; RECORD_LEN],
            filled: 0,
        })
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.pipe.as_fd()
    }

    /// Reads whatever is available.
    pub(crate) fn read(&mut self) -> Result<Report> {
        loop {
            let Some(rest) = self.record.get_mut(self.filled..).filter(|r| !r.is_empty()) else {
                // A whole record: no need to wait for end-of-file.
                return decode(self.record).map(Report::Failed).ok_or_else(|| {
                    Error::sys(
                        "the child's startup report names an unknown step",
                        Errno::EPROTO,
                    )
                });
            };
            match nix::unistd::read(self.pipe.as_fd(), rest) {
                Ok(0) => return self.at_end_of_file(),
                Ok(n) => self.filled += n,
                Err(Errno::EAGAIN) => return Ok(Report::Pending),
                Err(Errno::EINTR) => {}
                Err(e) => return Err(Error::sys("reading the child's startup report", e)),
            }
        }
    }

    fn at_end_of_file(&self) -> Result<Report> {
        match self.filled {
            0 => Ok(Report::Running),
            partial => Err(Error::sys(
                format!(
                    "the child's startup report was cut short ({partial} of {RECORD_LEN} bytes)"
                ),
                Errno::EPROTO,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let record = encode(ChildStep::Rlimit, libc::EPERM, 7);
        assert_eq!(
            decode(record),
            Some(ChildFailure {
                step: ChildStep::Rlimit,
                errno: Errno::EPERM,
                detail: 7,
            })
        );
    }

    #[test]
    fn unknown_steps_are_rejected() {
        let mut record = encode(ChildStep::Exec, libc::ENOENT, 0);
        record[0] = 0xff;
        assert_eq!(decode(record), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn init_reports_parse_strictly() {
        let report = |bytes: &[u8]| {
            let (read, write) = status_pipe().unwrap();
            if !bytes.is_empty() {
                nix::unistd::write(&write, bytes).unwrap();
            }
            read_init_report(&read)
        };
        assert_eq!(report(b""), InitReport::Nothing, "and no blocking");
        assert_eq!(report(b"R"), InitReport::Started);
        assert_eq!(report(b"RX"), InitReport::GaveUp);
        assert_eq!(report(b"RXX"), InitReport::Unexpected);
        assert_eq!(
            report(b"C"),
            InitReport::Unexpected,
            "stage 2's copy mark is gone"
        );
        for raw in [0, 3 << 8, libc::SIGKILL, libc::SIGSEGV | 0x80] {
            let mut bytes = vec![b'R'];
            bytes.extend(encode_status(raw));
            assert_eq!(report(&bytes), InitReport::Finished(raw));
        }
        // Forged or garbled: no marker first, a short record, extra bytes.
        assert_eq!(report(&[0, 0, 0, 0]), InitReport::Unexpected);
        assert_eq!(report(b"R\x00\x07"), InitReport::Unexpected);
        assert_eq!(report(b"R\x00\x07\x00\x00\x00"), InitReport::Unexpected);
        // A full pipe doesn't block the writer.
        let (_read, write) = status_pipe().unwrap();
        let chunk = [0u8; 4096];
        let full = (0..64).any(|_| nix::unistd::write(&write, &chunk) == Err(Errno::EAGAIN));
        assert!(full, "the write end blocks instead of failing");
    }

    #[test]
    fn go_pipe_ends_are_close_on_exec_and_above_stdio() {
        let (read, write) = go_pipe().unwrap();
        for fd in [&read, &write] {
            let flags = fcntl(fd, FcntlArg::F_GETFD).unwrap();
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            assert!(fd.as_raw_fd() >= 3);
        }
        send_go(&write);
        let mut byte = [0u8; 1];
        assert_eq!(nix::unistd::read(&read, &mut byte), Ok(1));
        // A child that already died: no panic, no error to handle.
        drop(read);
        send_go(&write);
    }

    #[test]
    fn pipe_ends_are_close_on_exec_and_above_stdio() {
        let (read, write) = report_pipe().unwrap();
        for fd in [&read, &write] {
            let flags = fcntl(fd, FcntlArg::F_GETFD).unwrap();
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        assert!(write.as_raw_fd() >= 3);
    }

    fn reader_with(bytes: &[u8], close: bool) -> (ReportReader, Option<OwnedFd>) {
        let (read, write) = report_pipe().unwrap();
        if !bytes.is_empty() {
            nix::unistd::write(&write, bytes).unwrap();
        }
        let write = if close {
            drop(write);
            None
        } else {
            Some(write)
        };
        (ReportReader::new(read).unwrap(), write)
    }

    /// Reads until the result is final. Another test in this process may
    /// fork at any moment, and the forked child briefly holds a copy of this
    /// pipe's write end: until it exits, end-of-file is (correctly) not there
    /// yet, and the report is still pending.
    fn read_settled(reader: &mut ReportReader) -> Result<Report> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let report = reader.read();
            if !matches!(report, Ok(Report::Pending)) || std::time::Instant::now() > deadline {
                return report;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn end_of_file_with_nothing_written_means_running() {
        let (mut reader, _) = reader_with(&[], true);
        assert_eq!(read_settled(&mut reader).unwrap(), Report::Running);
    }

    #[test]
    fn an_open_empty_pipe_is_pending_not_blocking() {
        let (mut reader, _write) = reader_with(&[], false);
        assert_eq!(reader.read().unwrap(), Report::Pending);
    }

    #[test]
    fn a_record_arriving_in_pieces_is_collected() {
        let record = encode(ChildStep::Chdir, libc::ENOENT, 0);
        let (first, second) = record.split_at(5);
        let (mut reader, write) = reader_with(first, false);
        assert_eq!(reader.read().unwrap(), Report::Pending);
        nix::unistd::write(write.as_ref().unwrap(), second).unwrap();
        match reader.read().unwrap() {
            Report::Failed(failure) => assert_eq!(failure.step, ChildStep::Chdir),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_record_is_a_protocol_error() {
        let (mut reader, _) = reader_with(&[1, 0, 0], true);
        assert!(read_settled(&mut reader).is_err());
    }
}
