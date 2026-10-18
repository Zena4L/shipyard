use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, Command};

use super::*;

fn sleeper() -> Child {
    Command::new("sleep").arg("30").spawn().unwrap()
}

fn pid_of(child: &Child) -> i32 {
    i32::try_from(child.id()).unwrap()
}

/// How `child` ended: the signal that killed it (std's status: nix's
/// `WaitStatus` has no real-time signals).
fn killed_by(child: &mut Child) -> i32 {
    let status = child.wait().unwrap();
    status.signal().unwrap_or_else(|| panic!("{status:?}"))
}

#[test]
fn a_signal_reaches_the_process_and_0_only_asks() {
    let mut child = sleeper();
    let pidfd = open(pid_of(&child)).unwrap();
    send_signal(pidfd.as_fd(), 0).unwrap();
    send_signal(pidfd.as_fd(), libc::SIGTERM).unwrap();
    assert_eq!(killed_by(&mut child), libc::SIGTERM);
}

#[test]
fn a_real_time_signal_is_sent_by_number() {
    let mut child = sleeper();
    let pidfd = open(pid_of(&child)).unwrap();
    // 35: a real-time signal neither glibc (32, 33) nor musl (32 to 34)
    // keeps for itself.
    send_signal(pidfd.as_fd(), 35).unwrap();
    assert_eq!(killed_by(&mut child), 35);
}

#[test]
fn an_ended_process_is_reached_no_more() {
    let mut child = sleeper();
    let pid = pid_of(&child);
    let pidfd = open(pid).unwrap();
    send_signal(pidfd.as_fd(), libc::SIGKILL).unwrap();
    assert_eq!(killed_by(&mut child), libc::SIGKILL);
    // Its pidfd, opened before: the process it means has ended.
    let error = send_signal(pidfd.as_fd(), 0).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
    // Reaped: its PID is no process now (until it's handed out again).
    assert_eq!(open(pid).unwrap_err().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn a_pid_that_cant_be_one_is_refused() {
    for pid in [0, -1, i32::MIN] {
        assert_eq!(
            open(pid).unwrap_err().raw_os_error(),
            Some(libc::EINVAL),
            "{pid}"
        );
    }
}

/// Only "no pidfds here" falls back to the PID: an old kernel's `ENOSYS`
/// and a policy's `EPERM`; gone or out of descriptors is the caller's.
#[test]
fn only_no_pidfds_falls_back() {
    let error = io::Error::from_raw_os_error;
    assert!(unsupported(&error(libc::ENOSYS)));
    assert!(unsupported(&error(libc::EPERM)));
    for code in [libc::ESRCH, libc::EMFILE, libc::EINVAL, libc::ENOMEM] {
        assert!(!unsupported(&error(code)), "{code}");
    }
}
