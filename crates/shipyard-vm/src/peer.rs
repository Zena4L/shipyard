//! Who is at the other end of the host's socket: only the user who owns the
//! VM may use it (it's root in the guest, with the user's home).

/// Whether a peer of uid `peer` may use a VM run by `own`: only the same
/// user. (Root gets no exception: a root process on the Mac has other ways
/// in, and none is needed.)
#[must_use]
pub fn same_user(peer: u32, own: u32) -> bool {
    peer == own
}

/// The uid of the process at the other end of `stream`, as the kernel says
/// (`getpeereid`), if it says.
#[cfg(target_os = "macos")]
pub(crate) fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    nix::unistd::getpeereid(stream)
        .ok()
        .map(|(uid, _)| uid.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the same user: another, or root, is refused.
    #[test]
    fn only_the_same_user_is_let_in() {
        assert!(same_user(501, 501));
        assert!(!same_user(502, 501));
        assert!(!same_user(0, 501));
    }

    /// The kernel names a socket pair's peer: this process's own uid.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_peers_uid_is_the_kernels_word() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
        assert_eq!(peer_uid(&a), Some(nix::unistd::getuid().as_raw()));
    }
}
