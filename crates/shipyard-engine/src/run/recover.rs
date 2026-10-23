//! Crash recovery (stage 9, step 9.5a): containers that ended while
//! nothing watched them, settled.
//!
//! A container's end is normally recorded by its monitor's exit command
//! (`__cleanup`). When the monitor itself is killed outright, or the host
//! restarts, or a rootless user's runtime directory goes at their last
//! logout, nothing records it: the record still says the container started
//! and never finished, its runtime state (and cgroup) may be left behind,
//! and a `--rm` container is never removed.
//!
//! The commands that change containers (`create`, `run`, `start`,
//! `restart`, `rm`) first settle such containers, all of them, in one pass
//! that never waits for a lock. A container is settled only if its locks
//! can be taken at once: its operation lock, and its `running` file (its
//! last start's holders). Either busy means a command, a monitor or a
//! clean-up is on it, and it's left alone (TLPI §55.2: `LOCK_NB`). `ps` and
//! `inspect` take no locks and settle nothing: they show what's recorded.
//!
//! The host's own start and stop live here too (stage 9, step 9.5c):
//! [`boot`] settles, then starts what restart policies bring back, and
//! [`stop_for_the_host`] stops everything for a shutdown, holding restart
//! policies off until [`boot`].

#[cfg(target_os = "linux")]
pub(super) use linux::{recover_others, settle_unrecorded_end, undo_a_dead_start};

use crate::Engine;
use crate::containers::Name;
use crate::error::Result;

/// The marker in the runtime root that says the host is stopping
/// ([`stop_for_the_host`]): no restart policy fires while it's there. The
/// runtime root is gone with the reboot (a tmpfs), and [`boot`] removes it
/// too.
#[cfg(target_os = "linux")]
pub(crate) const HOST_STOPPING: &str = ".host-stopping";

/// Whether the host is stopping (see [`HOST_STOPPING`]), as the runtime
/// root `root` says.
#[cfg(target_os = "linux")]
pub(crate) fn host_stopping(root: &std::path::Path) -> bool {
    root.join(HOST_STOPPING).exists()
}

/// Whether restart policies are held off now: [`stop_for_the_host`] ran
/// (`shipyard system stop`) and [`boot`] hasn't since, so a container that
/// ends isn't restarted. `false` without a runtime state root.
#[must_use]
#[cfg(target_os = "linux")]
pub(crate) fn restarts_held_off(engine: &Engine) -> bool {
    engine.paths.runtime_root().is_ok_and(host_stopping)
}

/// What [`stop_for_the_host`] did.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Stopped {
    /// The containers it stopped, by name.
    pub stopped: Vec<Name>,
    /// Those it couldn't stop, and why.
    pub failed: Vec<(Name, crate::Error)>,
}

/// What the host's shutdown runs (`shipyard system stop`, the stop unit's
/// `ExecStop`, `shipyard-stop.service`: stage 9, step 9.5c; a unit of its
/// own since 10.2d1, F52): every container that's up stopped, all
/// at once, each with its own stop signal and timeout (`docker stop`'s).
/// Not as its user's stop: its restart policy brings it back when the host
/// starts again. Until [`boot`] (or the reboot), no restart policy fires.
///
/// # Errors
///
/// The store or the runtime root couldn't be used; off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). A container
/// that couldn't be stopped is in [`Stopped::failed`].
pub fn stop_for_the_host(engine: &Engine) -> Result<Stopped> {
    #[cfg(target_os = "linux")]
    return linux::stop_for_the_host(engine);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        Err(crate::error::linux_only("containers"))
    }
}

/// What [`boot`] did.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Booted {
    /// The containers it started, by name.
    pub started: Vec<Name>,
    /// Those it couldn't start, and why.
    pub failed: Vec<(Name, crate::Error)>,
}

/// What the host's boot runs (`shipyard system recover`, from the boot
/// unit: stage 9, step 9.5c), and what can be run by hand: every container
/// that ended unwatched settled (as a container-changing command settles
/// them), then each container its restart policy brings back when the host
/// starts again, started (Docker's rules, when its daemon starts):
///
/// - `always`: always, even after its user stopped it;
/// - `unless-stopped`: unless its user stopped it;
/// - `on-failure[:N]`: if its last end was a failure (one nothing recorded
///   counts), and its N isn't used up;
/// - `no`, one never started, and one that's up: not.
///
/// # Errors
///
/// The store couldn't be read; off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported). A container
/// that couldn't be started is in [`Booted::failed`], not an error.
pub fn boot(engine: &Engine) -> Result<Booted> {
    #[cfg(target_os = "linux")]
    return linux::boot(engine);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        Err(crate::error::linux_only("containers"))
    }
}

/// The boot this host is in: the kernel's random boot ID, new at each boot
/// (none: it can't be read).
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// When this host booted: `btime` in `/proc/stat` (TLPI §12.1), seconds
/// since the epoch.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn boot_time() -> Option<std::time::SystemTime> {
    btime_of(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// The boot time `/proc/stat`'s text `stat` says.
#[cfg(target_os = "linux")]
fn btime_of(stat: &str) -> Option<std::time::SystemTime> {
    let seconds = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::time::{Duration, SystemTime};

    use shipyard_runtime::container::{self as runtime, Status};

    use crate::Engine;
    use crate::containers::{self, ContainerId, Record, RestartPolicy};
    use crate::error::Result;
    use crate::report::WarningKind;

    /// The marker a container's monitor leaves in its runtime state when
    /// its exit command (`__cleanup`) couldn't be started: nothing recorded
    /// its end, or applied its restart policy.
    const EXIT_COMMAND_FAILED: &str = "exit-command-failed";

    pub(super) fn boot(engine: &Engine) -> Result<super::Booted> {
        // The host's stop is over (a stop of the stop unit, or a `system
        // stop` by hand, without a reboot).
        let root = engine.paths.runtime_root()?;
        match std::fs::remove_file(root.join(super::HOST_STOPPING)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(crate::Error::from(error)
                    .context("letting restart policies fire again")
                    .io_as(crate::ErrorKind::Storage));
            }
            _ => {}
        }
        // Once, for every container: each start below doesn't again (on a
        // host with many, a pass each would cost the square of their number).
        recover_others(engine, None);
        let mut booted = super::Booted::default();
        // Those that join another's network after the others: theirs must
        // be up first (a join of a join is refused, so one pass is enough).
        let mut records = containers::list(engine)?;
        records.sort_by_cached_key(|record| {
            containers::created(engine, &record.id)
                .is_ok_and(|created| created.network_from.is_some() || created.ipc_from.is_some())
        });
        for record in records {
            let up = containers::state(engine, &record).is_ok_and(|state| state.is_active());
            if !starts_at_boot(&record, up) {
                continue;
            }
            match crate::run::start::start_recovered(engine, record.id.as_str()) {
                Ok(_) => booted.started.push(record.name),
                Err(error) => booted.failed.push((record.name, error)),
            }
        }
        Ok(booted)
    }

    pub(super) fn stop_for_the_host(engine: &Engine) -> Result<super::Stopped> {
        let root = engine.paths.runtime_root()?;
        // No state root: no container ever started here (a user's first
        // login since the boot), so none is up, and none can restart.
        if !root.is_dir() {
            return Ok(super::Stopped::default());
        }
        // First: a container that ends from here on isn't restarted.
        std::fs::write(root.join(super::HOST_STOPPING), b"").map_err(|error| {
            crate::Error::from(error)
                .context("holding restart policies off")
                .io_as(crate::ErrorKind::Storage)
        })?;
        // Up, being started, or restarting: a start (a user's, or a restart
        // that took its lock before the marker was written) may be starting
        // it now. Its stop waits for that lock, then stops what started (one
        // that hasn't started gives up).
        let up: Vec<Record> = containers::list(engine)?
            .into_iter()
            .filter(|record| {
                runtime::peek_status(root, record.id.as_str())
                    .is_ok_and(|status| stopped_for_the_host(record, status))
            })
            .collect();
        // All at once: each waits out its own stop timeout.
        let results: Vec<_> = std::thread::scope(|scope| {
            let stopping: Vec<_> = up
                .iter()
                .map(|record| {
                    scope.spawn(move || {
                        let host_stop =
                            crate::run::take::WhenUp::HostStop(crate::run::StopOptions::default());
                        crate::run::take::take(engine, record.id.as_str(), host_stop).map(drop)
                    })
                })
                .collect();
            stopping
                .into_iter()
                .map(std::thread::ScopedJoinHandle::join)
                .collect()
        });
        let mut stopped = super::Stopped::default();
        for (record, result) in up.into_iter().zip(results) {
            match result {
                Ok(Ok(())) => stopped.stopped.push(record.name),
                Ok(Err(error)) => stopped.failed.push((record.name, error)),
                Err(_) => stopped.failed.push((
                    record.name,
                    crate::Error::new(crate::ErrorKind::Runtime, "its stop panicked"),
                )),
            }
        }
        Ok(stopped)
    }

    /// Whether the host's stop stops container `record`, its runtime saying
    /// `status`: one with runtime state that hasn't ended (running, frozen,
    /// stopping, or being started: `created` or `creating` with a start in
    /// flight, which reads as created), or a restart waiting.
    pub(super) fn stopped_for_the_host(record: &Record, status: Option<Status>) -> bool {
        record.next_start.is_some() || status.is_some_and(|status| status != Status::Stopped)
    }

    /// Whether the container `record` is (`up`: it's up already) is started
    /// when the host starts again, as its restart policy says (see
    /// [`super::boot`]).
    pub(super) fn starts_at_boot(record: &Record, up: bool) -> bool {
        if up || record.started.is_none() {
            return false;
        }
        match record.config.restart {
            RestartPolicy::Always => true,
            RestartPolicy::UnlessStopped => !record.stopped_by_user,
            policy @ RestartPolicy::OnFailure { .. } => crate::run::restart_policy::decide(
                policy,
                record.exit.as_ref().and_then(|exit| exit.code),
                record.stopped_by_user,
                record.restart_count,
                Duration::ZERO,
                Duration::ZERO,
            )
            .is_some(),
            _ => false,
        }
    }

    /// Settles every container that ended unwatched, but the one `except`
    /// names (`rm`'s own, which it removes itself). What's settled is said,
    /// as a warning. One that can't be settled now is left for the next
    /// pass (said too, if it failed). Never fails the command.
    pub(in crate::run) fn recover_others(engine: &Engine, except: Option<&str>) {
        let (Ok(records), Ok(root)) = (containers::list(engine), engine.paths.runtime_root())
        else {
            return;
        };
        let except = except.and_then(|reference| containers::find(engine, reference).ok());
        let boot = super::boot_id();
        for record in records {
            if except.as_ref().is_some_and(|except| except.id == record.id) {
                continue;
            }
            // Unreadable: up or not, nothing to go on (left alone).
            let Ok(status) = runtime::peek_status(root, record.id.as_str()) else {
                continue;
            };
            if !candidate(engine, &record, status) {
                continue;
            }
            let (kind, message) = match settle(engine, root, &record.id, boot.as_deref()) {
                Ok(None) => continue,
                Ok(Some(Recovery::Ended)) => (
                    WarningKind::Recovered,
                    "ended while nothing recorded its end (its monitor was killed, or the host \
                     restarted): recorded as exited"
                        .to_owned(),
                ),
                Ok(Some(Recovery::Removed)) => (
                    WarningKind::Recovered,
                    "ended while nothing recorded its end (its monitor was killed, or the host \
                     restarted): removed (--rm)"
                        .to_owned(),
                ),
                Ok(Some(Recovery::DeadStart)) => (
                    WarningKind::Recovered,
                    "was cut short as it started: what its start left is cleaned up".to_owned(),
                ),
                Ok(Some(Recovery::RestartGone)) => (
                    WarningKind::Recovered,
                    "was to be restarted, and what was to restart it is gone: left exited"
                        .to_owned(),
                ),
                Err(error) => (
                    WarningKind::LeftForLater,
                    format!("couldn't be checked or settled: {error}"),
                ),
            };
            engine.warn(kind, format!("container {} {message}", record.name));
        }
    }

    /// Whether `record`, its runtime saying `status`, may need settling: a
    /// look without locks ([`settle`] looks again under them). One that's
    /// up is never touched.
    fn candidate(engine: &Engine, record: &Record, status: Option<Status>) -> bool {
        match status {
            Some(Status::Running | Status::Paused | Status::Stopping) => false,
            // Ended (its exit recorded or not), or a start's: settled if
            // nothing holds it.
            Some(_) => true,
            // No runtime state (a reboot, a logout, or deleted): a start
            // never recorded as finished, a `--rm` one left (its removal
            // failed before), a restart waiting (its waiter may be gone),
            // or an end recorded but not said (F88).
            None => {
                record.started.is_some() && (record.finished.is_none() || record.auto_remove)
                    || record.next_start.is_some()
                    || containers::end_unsaid(engine, record)
            }
        }
    }

    /// What [`settle`] did.
    enum Recovery {
        /// Its end recorded.
        Ended,
        /// Its end recorded, and it removed (`--rm`).
        Removed,
        /// A start that died before it was let go, cleaned up.
        DeadStart,
        /// A restart that waited, gone (its `__cleanup` killed): the
        /// container left ended.
        RestartGone,
    }

    /// Settles container `id`, if its locks can be taken at once (else
    /// `None`, as when there's nothing to do).
    ///
    /// - A dead start (runtime state `created`): its state is deleted. Its
    ///   operation lock is enough.
    /// - An end: once its `running` file is free too, its runtime state's
    ///   end is copied into its record and the state deleted (`reconcile`).
    ///   An end nothing recorded gets a time ([`ended_at`]).
    /// - A `--rm` container that ran is then removed.
    /// - A restart waiting, whose `restarting` lock is free (what waited is
    ///   gone): its `next_start` cleared, the container left ended.
    fn settle(
        engine: &Engine,
        root: &std::path::Path,
        id: &ContainerId,
        boot: Option<&str>,
    ) -> Result<Option<Recovery>> {
        // Busy, or removed meanwhile: left alone. Read again under its lock:
        // the look without it may be out of date. (By its path: the whole
        // store isn't read again for each.)
        let Ok(containers::Locked {
            record,
            operation: _operation,
        }) = containers::lock_again(engine, id, Duration::ZERO)
        else {
            return Ok(None);
        };
        match runtime::status(root, id.as_str())? {
            Some(Status::Running | Status::Paused | Status::Stopping) => return Ok(None),
            // A start holds the operation lock until it returns, and a
            // container is `created` only while one does. With the lock
            // free, it's a start that died, its monitor still waiting at
            // the gate with what it was given. Deleted, as `take` deletes
            // one: its monitor with it, so its holds go.
            Some(Status::Created | Status::Creating) => {
                crate::run::take::delete_state(root, id)?;
                undo_a_dead_start(engine, &record)?;
                return Ok(Some(Recovery::DeadStart));
            }
            Some(_) => {}
            // A restart waits, but nothing holds its lock: what was to
            // start it again is gone.
            None if record.next_start.is_some() => {
                let Ok(_restarting) = containers::restarting_lock(engine, id, Duration::ZERO)
                else {
                    return Ok(None);
                };
                containers::update(engine, id, |record| record.next_start = None)?;
                return Ok(Some(Recovery::RestartGone));
            }
            // Or its end recorded but not said (F88), its runtime state
            // gone with a reboot.
            None if record.started.is_some()
                && (record.finished.is_none()
                    || record.auto_remove
                    || containers::end_unsaid(engine, &record)) => {}
            None => return Ok(None),
        }
        // Its last start's holders (its monitor, its clean-up) all gone.
        let Ok(Some(running)) = containers::running_lock(engine, id, Duration::ZERO) else {
            return Ok(None);
        };
        // Its monitor's word that its exit command never started: read
        // before `reconcile` deletes the runtime state it's in.
        let exit_command_failed = root.join(id.as_str()).join(EXIT_COMMAND_FAILED).exists();
        crate::run::take::reconcile(engine, root, id)?;
        // Only its `die` to say: said, and nothing to warn of.
        let only_unsaid = record.finished.is_some() && !record.auto_remove;
        let record = settle_unrecorded_end(engine, id, boot)?;
        if only_unsaid {
            return Ok(None);
        }
        if exit_command_failed {
            engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {}'s exit command couldn't start: its end was settled late, and \
                     its restart policy wasn't applied",
                    record.name
                ),
            );
        }
        // A `--rm` container that ran: removed, as its clean-up would have.
        if record.auto_remove && record.started.is_some() {
            crate::image::remove_owned_area(engine, id.as_str())
                .map_err(|error| error.context("removing the container's writable layer"))?;
            let made = crate::run::volumes::made_anonymous(engine, id);
            crate::run::remove::forget(engine, id)?;
            drop(running);
            crate::run::volumes::remove_anonymous(engine, &made);
            return Ok(Some(Recovery::Removed));
        }
        drop(running);
        Ok(Some(Recovery::Ended))
    }

    /// After a start that died with container `record` made (its runtime
    /// state `created`, deleted by the caller, who holds its operation
    /// lock): one that had recorded itself started gets what its last start
    /// left back (kept aside by the start: `previous-start.json`), so a
    /// container that ran reads as exited, its history kept, and a boot
    /// brings it back. Without it, as never started: its command never ran.
    ///
    /// # Errors
    ///
    /// The record can't be written.
    pub(in crate::run) fn undo_a_dead_start(engine: &Engine, record: &Record) -> Result<()> {
        let id = &record.id;
        if record.started.is_some() && record.finished.is_none() {
            containers::settle_never_ran(engine, id)?;
        } else {
            containers::forget_previous_start(engine, id);
        }
        Ok(())
    }

    /// Container `id`'s record, with an end nothing recorded settled: one
    /// that started and has no end, whose start's holders are all gone (the
    /// caller holds its operation lock and its `running` file, and has
    /// reconciled its runtime state), ended at [`ended_at`], how unknown,
    /// and said so. `boot`: this boot's ID.
    ///
    /// # Errors
    ///
    /// The record can't be read or written.
    pub(in crate::run) fn settle_unrecorded_end(
        engine: &Engine,
        id: &ContainerId,
        boot: Option<&str>,
    ) -> Result<Record> {
        let record = containers::read_one(engine, id)?;
        let (Some(started), None) = (record.started, record.finished) else {
            // Its end recorded, but not said (F88: its clean-up killed
            // between the two, its runtime state gone since): said now.
            if containers::end_unsaid(engine, &record) {
                crate::events::container_ended(engine, &record);
                containers::end_said(engine, id);
            }
            return Ok(record);
        };
        let ended = ended_at(started, record.boot_id.as_deref(), boot);
        let record = containers::update(engine, id, |record| record.finished = Some(ended))?;
        // It ran: what it kept aside is no start's to restore now.
        containers::forget_previous_start(engine, id);
        crate::events::emit(engine, &crate::events::container(&record, "die").at(ended));
        containers::end_said(engine, id);
        Ok(record)
    }

    /// When an end nothing recorded happened, as near as can be told. For
    /// a start in an earlier boot (`started_boot` isn't `boot`): when this
    /// boot began. Else now (its monitor was killed, or its runtime state
    /// removed, some time before). Never before it started (the clock may
    /// have been set back).
    fn ended_at(started: SystemTime, started_boot: Option<&str>, boot: Option<&str>) -> SystemTime {
        let earlier_boot = matches!((started_boot, boot), (Some(then), Some(now)) if then != now);
        let ended = if earlier_boot && let Some(booted) = super::boot_time() {
            booted
        } else {
            SystemTime::now()
        };
        ended.max(started)
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::time::{Duration, SystemTime};

    use shipyard_runtime::container::Status;

    use super::btime_of;
    use super::linux::{starts_at_boot, stopped_for_the_host};
    use crate::containers::{Exit, Record, RestartPolicy};

    /// A record of `policy`, started and ended with `code` (none: an end
    /// nothing recorded), stopped by its user or not.
    fn record(policy: RestartPolicy, code: Option<i32>, stopped_by_user: bool) -> Record {
        let request = crate::run::RunRequest {
            restart: policy,
            ..Default::default()
        };
        let mut record = Record::new(
            crate::containers::ContainerId::parse(&"ab".repeat(32)).unwrap(),
            crate::containers::Serial::new(7).unwrap(),
            crate::containers::Name::parse("box").unwrap(),
            crate::containers::Config::from_request(&request).unwrap(),
            crate::containers::Labels::default(),
        );
        record.started = Some(SystemTime::UNIX_EPOCH);
        record.exit = code.map(|code| Exit {
            code: Some(code),
            oom: None,
        });
        record.stopped_by_user = stopped_by_user;
        record
    }

    #[test]
    fn what_starts_at_boot_is_dockers() {
        use RestartPolicy::{Always, No, OnFailure, UnlessStopped};
        let on_failure = OnFailure { max: Some(2) };
        for (policy, code, stopped, starts) in [
            (Always, Some(0), false, true),
            (Always, Some(0), true, true),
            (UnlessStopped, Some(0), false, true),
            (UnlessStopped, Some(143), true, false),
            (on_failure, Some(1), false, true),
            (on_failure, None, false, true),
            (on_failure, Some(0), false, false),
            (on_failure, Some(1), true, false),
            (No, Some(1), false, false),
        ] {
            let record = record(policy, code, stopped);
            assert_eq!(
                starts_at_boot(&record, false),
                starts,
                "{policy} {code:?} stopped {stopped}"
            );
            // Up already: left alone.
            assert!(!starts_at_boot(&record, true), "{policy} up");
        }
        // `on-failure:2` used up.
        let mut used_up = record(on_failure, Some(1), false);
        used_up.restart_count = 2;
        assert!(!starts_at_boot(&used_up, false));
        // Never started: not.
        let mut never = record(Always, None, false);
        never.started = None;
        assert!(!starts_at_boot(&never, false));
    }

    /// The host's stop takes every container with runtime state that
    /// hasn't ended: a start in flight (`created`, its record never started)
    /// too, which reads as created (stage 9, step 9.9c, R1), and a restart
    /// waiting; not one ended, or without runtime state.
    #[test]
    fn the_hosts_stop_takes_a_start_in_flight() {
        let mut created = record(RestartPolicy::No, None, false);
        created.started = None;
        for (status, stopped) in [
            (Some(Status::Created), true),
            (Some(Status::Creating), true),
            (Some(Status::Running), true),
            (Some(Status::Paused), true),
            (Some(Status::Stopping), true),
            (Some(Status::Stopped), false),
            (None, false),
        ] {
            assert_eq!(
                stopped_for_the_host(&created, status),
                stopped,
                "{status:?}"
            );
        }
        let mut waiting = record(RestartPolicy::Always, Some(1), false);
        waiting.next_start = Some(SystemTime::now());
        assert!(stopped_for_the_host(&waiting, None), "a restart waiting");
    }

    #[test]
    fn the_boot_time_is_proc_stats_btime() {
        let stat = "cpu  1 2 3\nintr 5\nctxt 9\nbtime 1760000000\nprocesses 7\n";
        assert_eq!(
            btime_of(stat),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000))
        );
        assert_eq!(btime_of("cpu 1\n"), None);
        assert_eq!(btime_of("btime soon\n"), None);
    }
}
