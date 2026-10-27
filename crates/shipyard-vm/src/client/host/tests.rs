use super::*;

/// A launchd start has ended when a run since its kick has ended with
/// an error; one that ended well (launchd's run at load), one still
/// running, or one from before the kick hasn't; with the runs unknown,
/// one that took the start's mark has. Kicked again at most every 2 s.
#[test]
fn a_launchd_start_ends_by_launchds_word() {
    let job = |running, runs, last_exit| crate::launchd::Job {
        running,
        runs,
        last_exit,
    };
    let failed = ended_in_launchd(Some(2), job(false, 3, Some(1)), false);
    assert_eq!(failed.and_then(|status| status.code()), Some(1));
    assert!(ended_in_launchd(Some(2), job(false, 3, Some(0)), true).is_none());
    assert!(ended_in_launchd(Some(2), job(true, 3, Some(1)), true).is_none());
    assert!(ended_in_launchd(Some(3), job(false, 3, Some(1)), true).is_none());
    // Runs unknown at the kick (F144): this start's run is one that took its
    // mark; while the mark is there, none has.
    assert!(ended_in_launchd(None, job(false, 3, Some(1)), false).is_none());
    let failed = ended_in_launchd(None, job(false, 3, Some(1)), true);
    assert_eq!(failed.and_then(|status| status.code()), Some(1));
    assert!(ended_in_launchd(None, job(true, 3, Some(1)), true).is_none());
    let kicked = Instant::now();
    let started = Started::Launchd {
        runs: Some(1),
        kicked,
        marked: true,
    };
    assert!(!started.may_start_again(None, kicked + Duration::from_millis(500)));
    assert!(started.may_start_again(None, kicked + KICK_AGAIN));
}
