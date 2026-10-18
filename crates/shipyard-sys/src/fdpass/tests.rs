use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};

use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};

use super::*;

// Close-on-exec, both (AGENTS §10): the limit test runs this binary again.
fn pair() -> (OwnedFd, OwnedFd) {
    #[cfg(target_os = "linux")]
    let flags = SockFlag::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = SockFlag::empty();
    socketpair(AddressFamily::Unix, SockType::Stream, None, flags).unwrap()
}

fn pipe() -> (std::io::PipeReader, std::io::PipeWriter) {
    std::io::pipe().unwrap()
}

#[cfg(target_os = "linux")]
const CLOEXEC: MsgFlags = MsgFlags::MSG_CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const CLOEXEC: MsgFlags = MsgFlags::empty();

#[test]
fn a_descriptor_passed_is_the_same_open_file() {
    let (ours, theirs) = pair();
    let (mut read, write) = pipe();
    let sent = send(ours.as_fd(), &[write.as_fd()], b"name", MsgFlags::empty()).unwrap();
    assert_eq!(sent, 4);
    drop(write);
    let mut buffer = [0_u8; 16];
    let mut control = Room::new(2);
    let received = receive(theirs.as_fd(), &mut buffer, &mut control, CLOEXEC).unwrap();
    assert_eq!(
        (&buffer[..received.bytes], received.truncated),
        (&b"name"[..], false)
    );
    let [fd] = <[OwnedFd; 1]>::try_from(received.fds).unwrap();
    // Written through what was received, read from the pipe kept here.
    std::fs::File::from(fd).write_all(b"through").unwrap();
    let mut text = String::new();
    read.read_to_string(&mut text).unwrap();
    assert_eq!(text, "through");
}

#[cfg(target_os = "linux")]
#[test]
fn a_descriptor_received_close_on_exec_is_so() {
    let (ours, theirs) = pair();
    let (_read, write) = pipe();
    send(ours.as_fd(), &[write.as_fd()], b"x", MsgFlags::empty()).unwrap();
    let mut control = Room::new(1);
    let received = receive(theirs.as_fd(), &mut [0; 4], &mut control, CLOEXEC).unwrap();
    let flags = nix::fcntl::fcntl(&received.fds[0], nix::fcntl::FcntlArg::F_GETFD).unwrap();
    assert!(nix::fcntl::FdFlag::from_bits_truncate(flags).contains(nix::fcntl::FdFlag::FD_CLOEXEC));
}

/// More passed than there's room for: said, and what fitted is ours, to
/// close (F54).
#[cfg(target_os = "linux")]
#[test]
fn more_passed_than_room_for_is_said() {
    let (ours, theirs) = pair();
    // Past room for one and its alignment's slack.
    let pipes: Vec<_> = (0..16).map(|_| pipe()).collect();
    let fds: Vec<_> = pipes.iter().map(|(_, write)| write.as_fd()).collect();
    send(ours.as_fd(), &fds, b"x", MsgFlags::empty()).unwrap();
    let mut control = Room::new(1);
    let received = receive(theirs.as_fd(), &mut [0; 4], &mut control, CLOEXEC).unwrap();
    assert!(received.truncated);
    assert!(
        (1..fds.len()).contains(&received.fds.len()),
        "{} of {}",
        received.fds.len(),
        fds.len()
    );
}

/// At the receiver's descriptor limit (`EMFILE`), the kernel installs what
/// it can of a message and says `MSG_CTRUNC`: those it did are in `fds`,
/// and go when they do, leaving nothing open (F54). The control: a message
/// that fits under the same limit comes whole. In a process of its own
/// (this test binary, on this test alone): a limit lowered here would
/// starve the tests beside it.
#[cfg(target_os = "linux")]
#[test]
fn a_message_cut_short_at_the_limit_leaves_nothing_open() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    const CHILD: &str = "SHIPYARD_TEST_FDPASS_AT_THE_LIMIT";
    const NAME: &str = "fdpass::tests::a_message_cut_short_at_the_limit_leaves_nothing_open";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads", "1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{output:?}"
        );
        return;
    }
    // The descriptors open here, by number (`read_dir`'s own among them
    // while it reads: closed after).
    let open = || -> Vec<i32> {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect()
    };
    let (ours, theirs) = pair();
    let pipes = [pipe(), pipe(), pipe(), pipe(), pipe()];
    let fds: Vec<_> = pipes.iter().map(|(_, write)| write.as_fd()).collect();
    let mut control = Room::new(fds.len());
    let before = open();
    // Room under the limit for a few more (2 above the highest open, the
    // holes below it, and `read_dir`'s, free again): `free`, exactly.
    let limit = before.iter().max().unwrap() + 3;
    let below = before.iter().filter(|&&fd| fd < limit).count() - 1;
    let free = usize::try_from(limit).unwrap() - below;
    assert!(free < fds.len(), "{free} free: nothing would be cut short");
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    setrlimit(Resource::RLIMIT_NOFILE, u64::try_from(limit).unwrap(), hard).unwrap();
    // The control: as many as there's room for come whole (`free` is
    // right, not one too many).
    send(ours.as_fd(), &fds[..free], b"x", MsgFlags::empty()).unwrap();
    let whole = receive(theirs.as_fd(), &mut [0; 4], &mut control, CLOEXEC).unwrap();
    assert_eq!(
        (whole.fds.len(), whole.truncated),
        (free, false),
        "the control"
    );
    drop(whole);
    send(ours.as_fd(), &fds, b"x", MsgFlags::empty()).unwrap();
    let cut = receive(theirs.as_fd(), &mut [0; 4], &mut control, CLOEXEC).unwrap();
    assert!(cut.truncated);
    assert_eq!(cut.fds.len(), free, "every one installed, ours");
    drop(cut);
    assert_eq!(open(), before, "nothing left open");
}

#[test]
fn a_message_without_descriptors_has_none() {
    let (ours, theirs) = pair();
    send(ours.as_fd(), &[], b"plain", MsgFlags::empty()).unwrap();
    let mut buffer = [0_u8; 16];
    let mut control = Room::new(1);
    let received = receive(theirs.as_fd(), &mut buffer, &mut control, CLOEXEC).unwrap();
    assert_eq!(received.bytes, 5);
    assert!(received.fds.is_empty() && !received.truncated);
}
