use std::os::fd::{AsFd as _, AsRawFd as _, IntoRawFd as _};
use std::os::unix::fs::MetadataExt as _;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

use super::*;

/// A descriptor of its own, without close-on-exec (as one inherited is).
fn inheritable() -> (std::fs::File, RawFd) {
    let file = std::fs::File::open("/dev/null").unwrap();
    let copy = nix::unistd::dup(file.as_fd()).unwrap();
    (file, copy.into_raw_fd())
}

fn close_on_exec(fd: &OwnedFd) -> bool {
    FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD).unwrap()).contains(FdFlag::FD_CLOEXEC)
}

#[test]
fn an_inherited_descriptor_is_taken_as_it_is_or_made_close_on_exec() {
    let (original, raw) = inheritable();
    // SAFETY: `raw` is a copy made here and owned by nothing else.
    let kept = unsafe { take_inherited(raw, OnExec::Keep) }.unwrap();
    assert!(!close_on_exec(&kept), "left as it came");
    let same = |fd: &OwnedFd| {
        let ours = original.metadata().unwrap();
        let theirs = std::fs::File::from(fd.try_clone().unwrap())
            .metadata()
            .unwrap();
        (ours.dev(), ours.ino()) == (theirs.dev(), theirs.ino())
    };
    assert!(same(&kept), "the same open file");
    let raw = kept.into_raw_fd();
    // SAFETY: as above: given back by `kept`, owned by nothing else.
    let closed = unsafe { take_inherited(raw, OnExec::Close) }.unwrap();
    assert!(close_on_exec(&closed));
    assert_eq!(closed.as_raw_fd(), raw, "the same number");
}

#[test]
fn a_number_that_isnt_open_is_refused() {
    // High: the kernel gives the lowest free number, so no other test's
    // open lands on it meanwhile (a number just closed could be taken).
    let raw: RawFd = 977;
    // SAFETY: borrowed for the one call, which only reads its flags.
    assert!(fcntl(unsafe { BorrowedFd::borrow_raw(raw) }, FcntlArg::F_GETFD).is_err());
    for on_exec in [OnExec::Keep, OnExec::Close] {
        // SAFETY: not open (checked above): nothing owns it, and the check
        // refuses it.
        let error = unsafe { take_inherited(raw, on_exec) }.unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF), "{on_exec:?}");
    }
    // SAFETY: never a descriptor.
    let error = unsafe { take_inherited(-1, OnExec::Keep) }.unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
}
