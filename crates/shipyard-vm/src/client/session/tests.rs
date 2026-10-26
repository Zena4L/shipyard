use std::time::Duration;

use super::*;

/// Three Ctrl-C's let go (130), and so does a SIGTERM's grace from the
/// first one (143); fewer, or within the grace, are passed on.
#[test]
fn the_ways_out_are_a_third_interrupt_and_a_terms_grace() {
    let start = Instant::now();
    let mut leaving = Leaving::default();
    assert_eq!(leaving.on(libc::SIGINT, start), None);
    assert_eq!(leaving.on(libc::SIGINT, start), None);
    assert_eq!(leaving.on(libc::SIGINT, start), Some(Way::Interrupted));
    assert_eq!(Way::Interrupted.code(), 130);

    let mut leaving = Leaving::default();
    assert_eq!(leaving.over(start + TERM_GRACE * 2), None, "no SIGTERM yet");
    assert_eq!(leaving.on(libc::SIGTERM, start), None);
    // A second one, in the grace, is only passed on: the grace runs
    // from the first.
    let later = start + TERM_GRACE / 2;
    assert_eq!(leaving.on(libc::SIGTERM, later), None);
    assert_eq!(leaving.over(later), None);
    let end = start + TERM_GRACE;
    let just_before = start + TERM_GRACE.checked_sub(Duration::from_millis(1)).unwrap();
    assert_eq!(leaving.over(just_before), None);
    assert_eq!(leaving.over(end), Some(Way::Unterminated));
    assert_eq!(Way::Unterminated.code(), 143);
}

/// The wire's numbers are Linux's: the Mac's that differ (USR1, USR2)
/// are translated, the same stay, and one without a Linux twin (INFO,
/// Ctrl-T's) isn't passed on.
#[test]
fn signals_go_by_their_linux_numbers() {
    assert_eq!(linux_number(libc::SIGINT), Some(2));
    assert_eq!(linux_number(libc::SIGTERM), Some(15));
    assert_eq!(linux_number(libc::SIGUSR1), Some(10));
    assert_eq!(linux_number(libc::SIGUSR2), Some(12));
    assert_eq!(linux_number(libc::SIGINFO), None);
    assert_eq!(linux_number(libc::SIGKILL), None);
}
