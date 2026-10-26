use super::*;

/// A launchd start has ended when a run since its kick has ended with
/// an error; one that ended well (launchd's run at load), one still
/// running, or one from before the kick hasn't. Kicked again at most
/// every 2 s.
#[test]
fn a_launchd_start_ends_by_launchds_word() {
    let job = |running, runs, last_exit| crate::launchd::Job {
        running,
        runs,
        last_exit,
    };
    let failed = ended_in_launchd(Some(2), job(false, 3, Some(1)));
    assert_eq!(failed.and_then(|status| status.code()), Some(1));
    assert!(ended_in_launchd(Some(2), job(false, 3, Some(0))).is_none());
    assert!(ended_in_launchd(Some(2), job(true, 3, Some(1))).is_none());
    assert!(ended_in_launchd(Some(3), job(false, 3, Some(1))).is_none());
    assert!(ended_in_launchd(None, job(false, 3, Some(1))).is_none());
    let kicked = Instant::now();
    let started = Started::Launchd {
        runs: Some(1),
        kicked,
    };
    assert!(!started.may_start_again(None, kicked + Duration::from_millis(500)));
    assert!(started.may_start_again(None, kicked + KICK_AGAIN));
}
