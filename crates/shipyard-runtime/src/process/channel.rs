//! The child's report pipe: how a failure between `fork` and `exec` reaches the
//! parent (TLPI §28.3 describes the idea).
//!
//! Both ends are close-on-exec. If `exec` succeeds, the kernel closes the
//! child's write end and the parent reads end-of-file. If it fails, the child
//! writes one 16-byte record first. 16 bytes is far below `PIPE_BUF`, so the
//! write is atomic: the parent sees all of it or none of it.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, fcntl};

use super::failure::{ChildFailure, ChildStep};
use crate::{Error, Result};

/// Size of one failure record.
pub(crate) const RECORD_LEN: usize = 16;

/// Creates the pipe, returning `(read, write)`. Both ends are close-on-exec,
/// and the write end is at fd 3 or above.
pub(crate) fn report_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let (read, write) = cloexec_pipe()?;
    Ok((read, above_stdio(write)?))
}

#[cfg(target_os = "linux")]
fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
        .map_err(|e| Error::sys("creating the child's report pipe", e))
}

/// macOS has no `pipe2`, so the flag is set separately. Another thread forking
/// in between could inherit the pipe; spawning only from the main thread of a
/// single-threaded process avoids that.
#[cfg(not(target_os = "linux"))]
fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    use nix::fcntl::FdFlag;
    let (read, write) =
        nix::unistd::pipe().map_err(|e| Error::sys("creating the child's report pipe", e))?;
    for fd in [&read, &write] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
            .map_err(|e| Error::sys("marking the report pipe close-on-exec", e))?;
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
        .map_err(|e| Error::sys("moving the report pipe above the standard descriptors", e))?;
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

/// Waits for the child's verdict: `None` if `exec` succeeded, or the failure
/// it reported.
pub(crate) fn read_report(pipe: &OwnedFd) -> Result<Option<ChildFailure>> {
    let mut record = [0u8; RECORD_LEN];
    let mut filled = 0;
    while let Some(rest) = record.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match nix::unistd::read(pipe.as_fd(), rest) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(Errno::EINTR) => {}
            Err(e) => return Err(Error::sys("reading the child's startup report", e)),
        }
    }
    match filled {
        0 => Ok(None),
        RECORD_LEN => decode(record).map(Some).ok_or_else(|| {
            Error::sys(
                "the child's startup report names an unknown step",
                Errno::EPROTO,
            )
        }),
        partial => Err(Error::sys(
            format!("the child's startup report was cut short ({partial} of {RECORD_LEN} bytes)"),
            Errno::EPROTO,
        )),
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
    fn pipe_ends_are_close_on_exec_and_above_stdio() {
        let (read, write) = report_pipe().unwrap();
        for fd in [&read, &write] {
            let flags = fcntl(fd, FcntlArg::F_GETFD).unwrap();
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        assert!(write.as_raw_fd() >= 3);
    }

    #[test]
    fn report_reads_eof_record_and_short_writes() {
        // EOF with nothing written: exec succeeded.
        let (read, write) = report_pipe().unwrap();
        drop(write);
        assert_eq!(read_report(&read).unwrap(), None);

        // A full record.
        let (read, write) = report_pipe().unwrap();
        nix::unistd::write(&write, &encode(ChildStep::Chdir, libc::ENOENT, 0)).unwrap();
        drop(write);
        let failure = read_report(&read).unwrap().unwrap();
        assert_eq!(failure.step, ChildStep::Chdir);

        // A truncated record is a protocol error, not a silent success.
        let (read, write) = report_pipe().unwrap();
        nix::unistd::write(&write, &[1, 0, 0]).unwrap();
        drop(write);
        assert!(read_report(&read).is_err());
    }
}
