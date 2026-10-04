//! The pipes between shipyard and a child it's starting (TLPI §28.3
//! describes the idea).
//!
//! - **The report pipe** (child → parent): how a failure between `fork` and
//!   `exec` reaches the parent.
//! - **The go pipe** (parent → child), the setup handshake: the child waits
//!   for one byte before setting up, so the parent can finish its own part
//!   first. The parent keeps its end open until `exec` resolves, which also
//!   tells the child whether the parent is still alive (see `child.rs`).
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
        let flags = fcntl(&pipe, FcntlArg::F_GETFL)
            .map_err(|e| Error::sys("reading the report pipe's flags", e))?;
        let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
        fcntl(&pipe, FcntlArg::F_SETFL(flags))
            .map_err(|e| Error::sys("making the report pipe non-blocking", e))?;
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

    #[test]
    fn end_of_file_with_nothing_written_means_running() {
        let (mut reader, _) = reader_with(&[], true);
        assert_eq!(reader.read().unwrap(), Report::Running);
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
        assert!(reader.read().is_err());
    }
}
