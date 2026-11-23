//! The operation lock, waited for in slices that ask whether the caller
//! went (F202).

use std::time::{Duration, Instant};

use super::*;
use crate::ErrorKind;

/// Held by another command: a caller that goes is let go within a slice;
/// one that stays waits the whole bound.
#[test]
fn a_lock_wait_ends_when_its_caller_goes() {
    let (engine, _, dir) = engine("lock-until");
    let record = create_record(&engine, new(Some("web"))).unwrap();
    let held = operation_lock(&engine, &record.id, Duration::ZERO).unwrap();
    let began = Instant::now();
    let gone_after = |after: Duration| move || began.elapsed() >= after;
    let error = lock_again_until(
        &engine,
        &record.id,
        Duration::from_secs(120),
        &gone_after(Duration::from_millis(300)),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::Busy, "{error:#}");
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    // The control: a caller that stays waits it out.
    let began = Instant::now();
    let error = lock_again_until(&engine, &record.id, Duration::from_secs(1), &|| false)
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::Busy, "{error:#}");
    assert!(began.elapsed() >= Duration::from_secs(1));
    drop(held);
    assert!(lock_again_until(&engine, &record.id, Duration::from_secs(120), &|| false).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop of a container its monitor wasn't asked to stop (it isn't
/// running), given up as its caller went while another command held the
/// lock: nothing happened, so it leaves no "stopped by its user" mark, or
/// its restart policy would leave a container nobody stopped (F202); and
/// it never takes back one an earlier stop left.
#[test]
fn a_stop_given_up_unasked_leaves_no_mark() {
    let (engine, _, dir) = engine("stop-gone");
    // A runtime root of its own, nothing in it: the container isn't up.
    let engine = Engine::new(
        engine.paths().clone().with_runtime_root(dir.join("run")),
        std::sync::Arc::new(Collect::default()),
    );
    let record = create_record(&engine, new(Some("web"))).unwrap();
    let held = operation_lock(&engine, &record.id, Duration::ZERO).unwrap();
    let began = Instant::now();
    let gone = || began.elapsed() >= Duration::from_millis(300);
    let stopped = crate::run::stop_until(&engine, "web", crate::run::StopOptions::default(), &gone);
    assert_eq!(
        stopped.err().map(|error| error.kind()),
        Some(ErrorKind::Busy)
    );
    assert!(began.elapsed() < Duration::from_secs(3));
    let after = crate::containers::read_one(&engine, &record.id).unwrap();
    assert!(!after.stopped_by_user, "no mark");
    // Stopped by its user before: the same stop given up keeps that.
    crate::containers::update(&engine, &record.id, |record| record.stopped_by_user = true).unwrap();
    let began = Instant::now();
    let gone = || began.elapsed() >= Duration::from_millis(300);
    let stopped = crate::run::stop_until(&engine, "web", crate::run::StopOptions::default(), &gone);
    assert_eq!(
        stopped.err().map(|error| error.kind()),
        Some(ErrorKind::Busy)
    );
    let after = crate::containers::read_one(&engine, &record.id).unwrap();
    assert!(after.stopped_by_user, "an earlier stop's mark kept");
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 15.6b: a container waiting to restart (its policy's backoff), stopped
/// while another command holds its lock, has its restart cancelled at
/// once, even when the stop then gives up: the restart waiter, taking the
/// lock first, would otherwise start it once more.
#[test]
fn a_stop_cancels_a_waiting_restart_before_the_lock() {
    let (engine, _, dir) = engine("stop-backoff");
    let engine = Engine::new(
        engine.paths().clone().with_runtime_root(dir.join("run")),
        std::sync::Arc::new(Collect::default()),
    );
    let record = create_record(&engine, new(Some("web"))).unwrap();
    crate::containers::update(&engine, &record.id, |record| {
        record.next_start = Some(std::time::SystemTime::now() + Duration::from_secs(60));
    })
    .unwrap();
    let held = operation_lock(&engine, &record.id, Duration::ZERO).unwrap();
    let began = Instant::now();
    let gone = || began.elapsed() >= Duration::from_millis(300);
    let stopped = crate::run::stop_until(&engine, "web", crate::run::StopOptions::default(), &gone);
    assert_eq!(
        stopped.err().map(|error| error.kind()),
        Some(ErrorKind::Busy)
    );
    let after = crate::containers::read_one(&engine, &record.id).unwrap();
    assert!(after.stopped_by_user, "marked: the restart is cancelled");
    assert_eq!(after.next_start, None, "no restart waits");
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}
