//! Whether a pipe's reader is gone: asked of the kernel, so a closed pipe
//! that is stdout's own (`… | head`) ends quietly and any other is a
//! failure (AGENTS §5). Both binaries' broken-pipe checks use it.

use std::os::fd::BorrowedFd;

/// Whether `fd` is a pipe or socket nobody reads any more. The kernel says
/// so without a write: `poll` on a write end whose readers have all closed
/// returns `POLLERR` (Linux) or `POLLHUP` (macOS), and never blocks with a
/// zero timeout.
#[must_use]
pub fn reader_gone(fd: BorrowedFd<'_>) -> bool {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut fds = [PollFd::new(fd, PollFlags::POLLOUT)];
    poll(&mut fds, PollTimeout::ZERO).is_ok_and(|_| {
        fds[0]
            .revents()
            .is_some_and(|got| got.intersects(PollFlags::POLLERR | PollFlags::POLLHUP))
    })
}

/// Whether stdout's reader has gone (`… -f | head -n1`, once `head` has
/// its line): a follower's `gone` hook, so it ends without waiting for its
/// next write.
#[must_use]
pub fn stdout_gone() -> bool {
    reader_gone(std::os::fd::AsFd::as_fd(&std::io::stdout()))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd as _;

    use super::*;

    /// A pipe's write end says when its reader has gone, without a write
    /// (and doesn't while it's there: the control).
    #[test]
    fn a_pipe_says_its_reader_is_gone() {
        let (read, write) = std::io::pipe().unwrap();
        assert!(!reader_gone(write.as_fd()));
        drop(read);
        assert!(reader_gone(write.as_fd()));
    }
}
