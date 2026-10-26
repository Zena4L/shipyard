//! Restart policies (stage 9, step 9.5b): `--restart no|on-failure[:N]|
//! always|unless-stopped`, Docker's, kept without a daemon. (Linux only:
//! containers are.)
//!
//! A container's end is handled by its monitor's exit command
//! (`__cleanup`), engine code that runs once the monitor has gone. After its
//! last step (its holds let go, the end recorded, its `running` file free),
//! it asks [`decide`] whether the policy starts the container again. If so,
//! it waits out the backoff and starts it, from its record, as `start`
//! does. The new start's monitor runs a new exit command, which does the
//! same at the next end: the chain is the restart loop (conmon's
//! `--exit-command` and `podman container cleanup` work so). The chain runs
//! the binary that made the container (each monitor's `/proc/self/exe`),
//! with its maker's environment.
//!
//! While it waits, the clean-up holds the container's `restarting` lock,
//! and the record says when it'll try (`next_start`): the container reads as
//! `restarting`. The user stays in charge: a `stop` or `kill` records that
//! the user stopped it and clears `next_start`, a `start` starts it now (and
//! clears it too), `update --restart` changes the policy, an `rm -f` removes
//! it. The waiter reads the record again every tenth of a second and gives
//! up on any of these. It never waits for a lock either: it tries the
//! container's operation lock, then `running` (as `take` takes them), and
//! tries again next time round.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use shipyard_runtime::container as runtime;

use crate::Engine;
use crate::containers::{self, ContainerId, Record, RestartPolicy};
use crate::report::WarningKind;
use crate::run::start::Input;
use crate::run::start::{drop_failed_start, last_start, start_taken};
use crate::run::take::Taken;

/// The first restart's delay.
pub(in crate::run) const FIRST_DELAY: Duration = Duration::from_millis(100);
/// The longest delay: the backoff stops doubling there.
pub(in crate::run) const LONGEST_DELAY: Duration = Duration::from_secs(60);
/// A run at least this long resets the backoff.
const LONG_RUN: Duration = Duration::from_secs(10);
/// The starts' logs a start keeps (a policy's restart, or its user's
/// `start`, `restart` or boot's): its own, and the latest before it.
const KEPT_STARTS: usize = 10;
/// How often a waiting restart looks at the record and tries the locks.
pub(in crate::run) const STEP: Duration = Duration::from_millis(100);
/// How long a clean-up waits for another's restart to let go (one that
/// started the container a moment ago, and hasn't let go yet).
const RESTARTING_WAIT: Duration = Duration::from_secs(15);

/// How long to wait before starting a container again, as its `policy`
/// says (`None`: it's not started again), given how its last run went:
/// its exit code (`None`: not known), whether its user stopped it, the
/// restarts its policy made since its user last started it, how long it
/// ran, and the delay before the restart that started it (zero: none).
///
/// A user's stop is never undone. `on-failure` restarts an end with a code
/// other than 0 (or none known), up to its most. The delay doubles after
/// each short run, from 100 ms to at most a minute, and starts again at
/// 100 ms after a run of 10 s or more (Docker's).
#[must_use]
pub(crate) fn decide(
    policy: RestartPolicy,
    code: Option<i32>,
    stopped_by_user: bool,
    restart_count: u32,
    ran_for: Duration,
    last_delay: Duration,
) -> Option<Duration> {
    if stopped_by_user {
        return None;
    }
    match policy {
        RestartPolicy::No => return None,
        RestartPolicy::OnFailure { max } => {
            if code == Some(0) || max.is_some_and(|max| restart_count >= max) {
                return None;
            }
        }
        RestartPolicy::Always | RestartPolicy::UnlessStopped => {}
    }
    Some(if ran_for >= LONG_RUN || last_delay.is_zero() {
        FIRST_DELAY
    } else {
        last_delay.saturating_mul(2).min(LONGEST_DELAY)
    })
}

/// Removes the oldest starts' logs under `logs`, keeping the latest
/// `keep`.
///
/// # Errors
///
/// The directory couldn't be read, or a start's logs removed.
fn prune_starts(logs: &Path, keep: usize) -> std::io::Result<()> {
    let starts = containers::starts(logs)?;
    let surplus = starts.len().saturating_sub(keep);
    for (_, dir) in starts.into_iter().take(surplus) {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// Before every start (its policy's, or its user's): room for its start's
/// logs among the latest [`KEPT_STARTS`], so a crash loop, or a container
/// started again and again, doesn't fill the disk. (Its clean-up log is
/// rotated as each clean-up opens it, never here: this may run in a
/// clean-up that has it open, whose next lines would follow it.)
pub(in crate::run) fn prune_before_a_start(engine: &Engine, id: &ContainerId) {
    let pruned = containers::logs_dir(engine, id)
        .and_then(|logs| prune_starts(&logs, KEPT_STARTS - 1).map_err(Into::into));
    if let Err(error) = pruned {
        engine.warn(
            WarningKind::LeftForLater,
            format!(
                "container {}'s older starts' logs couldn't be removed: {error}",
                id.short()
            ),
        );
    }
}

/// What `__cleanup` does after its last step: starts container `id` again
/// if its policy says so, once the backoff is over (see the module's
/// documentation). What goes wrong is said to the clean-up's log; the
/// container is then left ended.
pub(in crate::run) fn after_end(
    engine: &Engine,
    id: &ContainerId,
    held: Option<nix::fcntl::Flock<std::fs::File>>,
) {
    let Ok(record) = containers::read_one(engine, id) else {
        return;
    };
    // An end its policy doesn't start again (no policy, a user's stop, a
    // success `on-failure` takes, its most reached): ended for good, so the
    // restart is never taken, nor waited for (a `stop` then `rm`, compose's
    // `down`, found it "restarting" a moment: 12.5c3's review).
    if next_delay(&record).is_none() {
        return;
    }
    let Ok(root) = engine.paths.runtime_root().map(Path::to_path_buf) else {
        return;
    };
    // The host is stopping: nothing restarts until it has started again.
    if crate::run::recover::host_stopping(&root) {
        return;
    }
    // The one restart that waits: another clean-up's may hold it a moment
    // (its start just ended), or for its whole backoff (its start failed,
    // and it goes on: then this one has nothing to do).
    // Held already (the clean-up took it before it let the container go),
    // or waited for.
    let Some(_restarting) =
        held.or_else(|| containers::restarting_lock(engine, id, RESTARTING_WAIT).ok())
    else {
        return;
    };
    // Again, under it: started again meanwhile, or gone.
    let Ok(record) = containers::read_one(engine, id) else {
        return;
    };
    if !stateless(&root, id) {
        return;
    }
    let Some(delay) = next_delay(&record) else {
        return;
    };
    restart_after(engine, &root, id, delay, record.restart_count);
}

/// The delay before `record`'s policy starts it again after the end it
/// records, as [`decide`] says; `None` if it won't, or if it never started
/// (its start failed before its command ran: no end).
pub(in crate::run) fn next_delay(record: &Record) -> Option<Duration> {
    let started = record.started?;
    let ran_for = record
        .finished
        .and_then(|finished| finished.duration_since(started).ok())
        .unwrap_or_default();
    decide(
        record.config.restart,
        record.exit.as_ref().and_then(|exit| exit.code),
        record.stopped_by_user,
        record.restart_count,
        ran_for,
        Duration::from_millis(record.restart_delay_ms),
    )
}

/// Whether container `id` has no runtime state (none started it since).
fn stateless(root: &Path, id: &ContainerId) -> bool {
    matches!(runtime::status(root, id.as_str()), Ok(None))
}

/// Waits `delay`, then starts container `id` as restart `count + 1`. A
/// start that fails counts as a failed run (its start's empty logs
/// removed), and is tried again after the next delay, while the policy
/// says so: all decided under its operation lock, as the start was, so a
/// user's `start` or `stop` comes before it or after it, never between.
fn restart_after(engine: &Engine, root: &Path, id: &ContainerId, delay: Duration, mut count: u32) {
    let Some(mut turn) = schedule(engine, id, delay) else {
        return;
    };
    loop {
        let Some(taken) = wait_for_turn(engine, root, id, turn.at, turn.deadline) else {
            return;
        };
        let last_start = last_start(engine, id);
        let mut next = None;
        let failed = |error: &crate::Error| {
            engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {}'s restart {} failed: {error:#}",
                    id.short(),
                    count.saturating_add(1)
                ),
            );
            drop_failed_start(engine, id, last_start.as_deref());
            // The attempt counts, whether another follows or not.
            count = count.saturating_add(1);
            let Ok(record) = containers::update(engine, id, |record| record.restart_count = count)
            else {
                return;
            };
            match decide(
                record.config.restart,
                None,
                record.stopped_by_user,
                count,
                Duration::ZERO,
                turn.delay,
            ) {
                Some(delay) => next = schedule(engine, id, delay),
                None => give_up(engine, id, turn.at),
            }
        };
        if start_taken(engine, taken, |_| {}, None, Input::AsMade, failed).is_ok() {
            return;
        }
        let Some(scheduled) = next else {
            return;
        };
        turn = scheduled;
    }
}

/// A restart's turn: when it's due (`at` names it in the record; the wait
/// is timed by `deadline`, on the monotonic clock, so setting the time
/// back doesn't stretch it), after `delay`.
struct Turn {
    at: SystemTime,
    deadline: Instant,
    delay: Duration,
}

/// Sets container `id`'s next start `delay` from now in its record: its
/// turn, or `None` if the record couldn't be changed (removed).
fn schedule(engine: &Engine, id: &ContainerId, delay: Duration) -> Option<Turn> {
    let deadline = Instant::now() + delay;
    let at = SystemTime::now() + delay;
    let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    containers::update(engine, id, |record| {
        record.next_start = Some(at);
        record.restart_delay_ms = delay_ms;
    })
    .ok()?;
    Some(Turn {
        at,
        deadline,
        delay,
    })
}

/// Waits until `deadline`, then for container `id`'s locks (tried, never
/// waited for), the record read again each step: the container taken to
/// start, or `None` if the wait was given up (removed, stopped by its user,
/// started, its policy changed so it wouldn't restart now, or the host is
/// stopping).
///
/// That the host is stopping is decided under the operation lock: `system
/// stop` writes its marker first, then takes each container's lock, so
/// either this sees the marker, or it held the lock first and `system stop`
/// waits for its start, then stops what it started.
fn wait_for_turn(
    engine: &Engine,
    root: &Path,
    id: &ContainerId,
    at: SystemTime,
    deadline: Instant,
) -> Option<Taken> {
    let host_stopping = || crate::run::recover::host_stopping(root);
    loop {
        let record = containers::read_one(engine, id).ok()?;
        if !still_ours(&record, at) {
            give_up(engine, id, at);
            return None;
        }
        let now = Instant::now();
        if now < deadline {
            // Not after its backoff for nothing.
            if host_stopping() {
                give_up(engine, id, at);
                return None;
            }
            std::thread::sleep((deadline - now).min(STEP));
            continue;
        }
        if let Ok(operation) = containers::operation_lock(engine, id, Duration::ZERO)
            && let Ok(Some(running)) = containers::running_lock(engine, id, Duration::ZERO)
        {
            // Under both: what was read may have changed.
            let record = containers::read_one(engine, id).ok()?;
            if !still_ours(&record, at) || !stateless(root, id) || host_stopping() {
                give_up(engine, id, at);
                return None;
            }
            return Some(Taken::for_restart(
                record,
                running,
                root.to_path_buf(),
                operation,
            ));
        }
        std::thread::sleep(STEP);
    }
}

/// Whether `record` still waits for the restart set for `at`: not started
/// or rescheduled since (its `next_start` still `at`), and its policy (as
/// `update` may have changed it) still restarts its last end.
fn still_ours(record: &Record, at: SystemTime) -> bool {
    record.next_start == Some(at)
        && decide(
            record.config.restart,
            record.exit.as_ref().and_then(|exit| exit.code),
            record.stopped_by_user,
            record.restart_count,
            Duration::ZERO,
            Duration::ZERO,
        )
        .is_some()
}

/// Clears container `id`'s `next_start` if it's still `at`'s (another
/// restart's is left alone): it's left ended.
fn give_up(engine: &Engine, id: &ContainerId, at: SystemTime) {
    let _ = containers::update(engine, id, |record| {
        if record.next_start == Some(at) {
            record.next_start = None;
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::time::Duration;

    use super::{FIRST_DELAY, LONGEST_DELAY, decide, prune_starts};
    use crate::containers::RestartPolicy;

    const SHORT: Duration = Duration::from_secs(1);
    const LONG: Duration = Duration::from_secs(10);

    #[test]
    fn policies_decide_as_dockers() {
        let first = Some(FIRST_DELAY);
        let decided = |policy, code, stopped, count| {
            decide(policy, code, stopped, count, SHORT, Duration::ZERO)
        };
        // `no`: never.
        assert_eq!(decided(RestartPolicy::No, Some(1), false, 0), None);
        // A user's stop: never, whatever the policy.
        for policy in [
            RestartPolicy::Always,
            RestartPolicy::UnlessStopped,
            RestartPolicy::OnFailure { max: None },
        ] {
            assert_eq!(decided(policy, Some(1), true, 0), None, "{policy}");
            assert_eq!(decided(policy, Some(1), false, 0), first, "{policy}");
        }
        // `always` and `unless-stopped`: a clean end too.
        assert_eq!(decided(RestartPolicy::Always, Some(0), false, 9), first);
        assert_eq!(
            decided(RestartPolicy::UnlessStopped, Some(0), false, 9),
            first
        );
        // `on-failure`: not a clean end; an unknown one is a failure.
        let on_failure = RestartPolicy::OnFailure { max: Some(2) };
        assert_eq!(decided(on_failure, Some(0), false, 0), None);
        assert_eq!(decided(on_failure, None, false, 0), first);
        assert_eq!(decided(on_failure, Some(137), false, 1), first);
        // ...up to its most.
        assert_eq!(decided(on_failure, Some(3), false, 2), None);
    }

    #[test]
    fn the_backoff_doubles_resets_and_stops_at_a_minute() {
        let next = |ran_for, last| decide(RestartPolicy::Always, Some(1), false, 0, ran_for, last);
        assert_eq!(next(SHORT, Duration::ZERO), Some(FIRST_DELAY));
        assert_eq!(next(SHORT, FIRST_DELAY), Some(FIRST_DELAY * 2));
        assert_eq!(
            next(SHORT, Duration::from_secs(16)),
            Some(Duration::from_secs(32))
        );
        assert_eq!(next(SHORT, Duration::from_secs(40)), Some(LONGEST_DELAY));
        assert_eq!(next(SHORT, LONGEST_DELAY), Some(LONGEST_DELAY));
        // A long run starts it over.
        assert_eq!(next(LONG, Duration::from_secs(40)), Some(FIRST_DELAY));
    }

    #[test]
    fn the_oldest_starts_logs_go() {
        let logs = std::env::temp_dir().join(format!("shipyard-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&logs);
        for start in 1..=12 {
            std::fs::create_dir_all(logs.join(start.to_string())).unwrap();
        }
        // Not a start's: left alone.
        std::fs::write(logs.join("notes"), b"").unwrap();
        prune_starts(&logs, 9).unwrap();
        let mut kept: Vec<u64> = std::fs::read_dir(&logs)
            .unwrap()
            .filter_map(|entry| entry.unwrap().file_name().to_str()?.parse().ok())
            .collect();
        kept.sort_unstable();
        assert_eq!(kept, (4..=12).collect::<Vec<_>>());
        assert!(logs.join("notes").exists());
        std::fs::remove_dir_all(&logs).unwrap();
    }
}
