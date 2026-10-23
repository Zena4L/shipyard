//! `__cleanup`: what a detached container's monitor runs once the container
//! has ended (its exit command), to let go of what it held and record how
//! it ended.
//!
//! It's this binary again (`/proc/self/exe` in the monitor), started in a
//! session of its own, with `/dev/null` for its standard streams, the
//! monitor's holds at 3 and up (`SHIPYARD_HOLDS` says how many) and the
//! container's end (`SHIPYARD_EXIT_RECORD`). Its arguments say where the
//! stores are and what each hold is ([`HoldKind`]). It lets go in the
//! order a foreground run does: the network (each lease under its
//! network's lock, swept with what it made), the ports, the run directory,
//! the volumes, the writable area (a container's own is only closed, kept
//! for its next start, unless `--auto-remove`); then the record, then the
//! runtime's state; with `--auto-remove` (`--rm`), then the container
//! itself (its image's pin, its record); and last, its `running` file, so
//! a `start` or an `rm` waiting for it finds everything let go. A step that
//! fails is said in `cleanup.log`, beside the record, and the others go on:
//! what's left is the next run's sweep's.

#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by Linux's detached containers only")
)]

use super::holds::HoldKind;

/// The argument that makes this binary `__cleanup`.
pub(crate) const CLEANUP_ARG: &str = "__cleanup";

/// Runs `__cleanup` if this process was started as one (its first argument
/// `__cleanup`), and exits; returns otherwise. Call it in `main`, right
/// after the init's entry point, in every binary that makes detached containers
/// ([`detached`](fn@super::detached)): their monitors run the binary that made
/// them as their exit command.
pub fn cleanup_entry_point() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(CLEANUP_ARG)) {
        return;
    }
    // Not for a privileged start (setuid, file capabilities): its
    // arguments name what it removes.
    if shipyard_runtime::process::refuse_privileged_start().is_err() {
        std::process::exit(1);
    }
    #[cfg(target_os = "linux")]
    std::process::exit(linux::main(args));
    #[cfg(not(target_os = "linux"))]
    std::process::exit(1);
}

/// Whether the runtime state a retry of an end finds is its own start's
/// (F102): made by the monitor that started this `__cleanup` (`mine`: its
/// PID, and its start time if it could be read while it was there), as
/// that state records its monitor (`recorded`). Another start's state (a
/// monitor of its own) is never settled with this start's end; a state
/// that records none can't be told, so isn't either.
fn own_start(recorded: Option<(i32, u64)>, mine: (i32, Option<u64>)) -> bool {
    let (my_pid, my_start) = mine;
    recorded
        .is_some_and(|(pid, started)| pid == my_pid && my_start.is_none_or(|own| own == started))
}

/// What `__cleanup` is told, from its arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Arguments {
    /// The container's ID.
    pub(crate) id: String,
    /// The container store.
    pub(crate) containers: std::path::PathBuf,
    /// The runtime's state root.
    pub(crate) runtime_root: std::path::PathBuf,
    /// The networks' store.
    pub(crate) networks: std::path::PathBuf,
    /// The image store, if there is one.
    pub(crate) store: Option<std::path::PathBuf>,
    /// The run directory, if the container has one.
    pub(crate) run_dir: Option<std::path::PathBuf>,
    /// The events journal's directory (none: beside the container store).
    pub(crate) events: Option<std::path::PathBuf>,
    /// What each hold is, in order.
    pub(crate) holds: Vec<HoldKind>,
    /// The container goes with its end (`--rm`): its own writable area, its
    /// image's pin and its record are removed too.
    pub(crate) auto_remove: bool,
}

impl Arguments {
    /// Reads them (after [`CLEANUP_ARG`]).
    pub(crate) fn parse(
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        let (mut id, mut containers, mut runtime_root, mut networks) = (None, None, None, None);
        while let Some(flag) = args.next() {
            let flag = flag.to_string_lossy().into_owned();
            if flag == "--auto-remove" {
                parsed.auto_remove = true;
                continue;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let path = std::path::PathBuf::from(&value);
            match flag.as_str() {
                "--id" => id = Some(value.to_string_lossy().into_owned()),
                "--containers" => containers = Some(path),
                "--runtime-root" => runtime_root = Some(path),
                "--networks" => networks = Some(path),
                "--store" => parsed.store = Some(path),
                "--run-dir" => parsed.run_dir = Some(path),
                "--events" => parsed.events = Some(path),
                "--hold" => {
                    let kind = value.to_string_lossy();
                    parsed.holds.push(
                        HoldKind::from_arg(&kind)
                            .ok_or_else(|| format!("--hold {kind}: no such hold"))?,
                    );
                }
                _ => return Err(format!("{flag}: no such argument")),
            }
        }
        let missing = |what: &str| format!("{what} is missing");
        parsed.id = id.ok_or_else(|| missing("--id"))?;
        parsed.containers = containers.ok_or_else(|| missing("--containers"))?;
        parsed.runtime_root = runtime_root.ok_or_else(|| missing("--runtime-root"))?;
        parsed.networks = networks.ok_or_else(|| missing("--networks"))?;
        Ok(parsed)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{OwnedFd, RawFd};
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::Arc;
    use std::time::SystemTime;

    use shipyard_runtime::container::{self as runtime, ContainerError, ExitRecord};

    use super::{Arguments, HoldKind};
    use crate::containers::{self, ContainerId, Exit};
    use crate::report::Reporter;
    use crate::run::helper::Log;
    use crate::{Engine, Paths};

    /// Says the end `record_end` recorded, if it's this start's (F80): a
    /// start that never ran has no `die`, though its record has its last
    /// start's again. Then its mark goes (F88): said, or nothing to say.
    /// Whether this start's command ran (the runtime's word: its gate
    /// opened): only then is its end one the restart policy follows (F81).
    fn say_end(
        engine: &Engine,
        id: Option<&ContainerId>,
        recorded: &crate::Result<(containers::Record, bool)>,
    ) -> bool {
        let (Some(id), Ok((record, wrote))) = (id, recorded) else {
            return false;
        };
        if *wrote {
            test_pause_after_the_end();
            crate::events::container_ended(engine, record);
        }
        containers::end_said(engine, id);
        *wrote
    }

    /// Records how container `id`'s start ended, from the runtime's `exit`
    /// record: its end and exit if it started; if it never did, what its
    /// last start left put back (F64). Its kept-aside last start is let go.
    /// With the record, whether this end was written: only then is it said
    /// as an event (F80).
    fn record_end(
        engine: &Engine,
        id: &ContainerId,
        exit: Option<&ExitRecord>,
    ) -> crate::Result<(containers::Record, bool)> {
        // What its last start left, kept aside (a start that never ran
        // gets it back: F64).
        let previous = containers::previous_start(engine, id);
        let newer = previous.newer();
        let mut wrote = false;
        let updated = containers::update(engine, id, |record| {
            // A forced end of a container that never started isn't an
            // exit. Whether it started is the runtime's word (its gate
            // opened), not the record's: a caller killed between
            // recording the start and making it left `started` set for a
            // container that never ran. An older exit record can't say:
            // the record decides then.
            let started = exit
                .and_then(|exit| exit.started)
                .unwrap_or(record.started.is_some());
            // A start that failed, whose caller has put back what the
            // record said of the start before (an end): left as it is.
            if !started && record.finished.is_some() {
                return;
            }
            wrote = started;
            if started {
                record.finished = exit
                    .and_then(|exit| exit.finished)
                    .or_else(|| Some(SystemTime::now()));
                record.exit = exit.map(Exit::from);
            } else {
                // Never started: its last start's end back, or none.
                containers::settle_unstarted(record, previous);
            }
        });
        if updated.is_ok() {
            if wrote {
                // It ran: what it kept aside is no start's to restore now.
                containers::forget_previous_start(engine, id);
            } else {
                containers::done_with_previous_start(engine, id, newer);
            }
        }
        updated.map(|record| (record, wrote))
    }

    /// The test hook (`SHIPYARD_TEST_CLEANUP_PAUSE`, a path): the end
    /// recorded and not yet said, this `__cleanup` writes its PID there and
    /// waits a minute, for a test to kill it in F88's window. Unset: nothing.
    const PAUSE_VAR: &str = "SHIPYARD_TEST_CLEANUP_PAUSE";

    /// [`PAUSE_VAR`]'s pause.
    fn test_pause_after_the_end() {
        pause_at(PAUSE_VAR);
    }

    /// Where the hook `var` names, this process's PID (written whole:
    /// renamed into place, so a test never reads half of it), then a
    /// minute's wait, for a test to act in the window. Unset, or a release
    /// build: nothing.
    fn pause_at(var: &str) {
        if let Some(path) = pause_path(std::env::var_os(var), cfg!(debug_assertions)) {
            crate::test_pause::announce(&path, &std::process::id().to_string());
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    /// Where a test hook's `value` asks for a pause: in a debug build (the
    /// tests') only, as the other hooks (`Paths::from_env`), so a release's
    /// `__cleanup` never holds a container's locks for a variable it was
    /// started with (11.7).
    fn pause_path(
        value: Option<std::ffi::OsString>,
        debug_build: bool,
    ) -> Option<std::path::PathBuf> {
        value
            .filter(|value| debug_build && !value.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Container `id`'s restart, taken (if no one else has it) before its
    /// clean-up lets it go: a `rm` that waits on `running` then finds it
    /// restarting, never ended for good in the moment before its policy
    /// decides. Handed to the policy, which holds it while it waits.
    fn restart_held(engine: &Engine, id: &str) -> Option<nix::fcntl::Flock<std::fs::File>> {
        let id = ContainerId::parse(id).ok()?;
        // A moment's wait, not none: a `restart_pending` look (a `network
        // rm`'s, a listing's) takes this lock for an instant, and a try that
        // met it would let the container go unheld (11.6b's engine review).
        crate::containers::restarting_lock(engine, &id, std::time::Duration::from_millis(200)).ok()
    }

    /// The test hook (`SHIPYARD_TEST_CLEANUP_RESTART_PAUSE`, a path): the
    /// container let go, its restart not yet decided, this `__cleanup`
    /// pauses as [`test_pause_after_the_end`] does. Unset: nothing.
    const RESTART_PAUSE_VAR: &str = "SHIPYARD_TEST_CLEANUP_RESTART_PAUSE";

    /// [`RESTART_PAUSE_VAR`]'s pause.
    fn test_pause_before_the_restart() {
        pause_at(RESTART_PAUSE_VAR);
    }

    /// Whether a failure to record an end is worth trying again (F45): the
    /// store refused the write (a full disk) or was held past its wait.
    fn retryable(error: &crate::Error) -> bool {
        matches!(
            error.kind(),
            crate::ErrorKind::Storage | crate::ErrorKind::Busy
        )
    }

    /// An end this `__cleanup` couldn't record, retried with the restart
    /// policy's backoff (F45): recorded as a first try would (`record_end`,
    /// from `exit`, this process's own copy of how it ended: the runtime's
    /// file may be lost to the same full disk), its runtime state deleted,
    /// then its restart policy. Like the waiter, it only tries the
    /// container's locks, never waits for them, and holds nothing between
    /// tries. It stops when the host is stopping, the container is removed,
    /// its runtime state is gone (nothing left to record: the recovery
    /// pass settles it), or another settled the end (then the policy, as
    /// any end's, if this start ran: F81). A disk that never frees keeps it trying, a minute apart
    /// at most, each step of the wait checking whether to stop.
    fn retry_end(
        engine: &Engine,
        arguments: &Arguments,
        exit: Option<&ExitRecord>,
        mine: Option<(i32, Option<u64>)>,
        log: &Log,
    ) {
        use crate::run::restart_policy::{FIRST_DELAY, LONGEST_DELAY, STEP, after_end};
        let Ok(id) = ContainerId::parse(&arguments.id) else {
            return;
        };
        let root = &arguments.runtime_root;
        let mut delay = FIRST_DELAY;
        loop {
            let until = std::time::Instant::now() + delay;
            while std::time::Instant::now() < until {
                if crate::run::recover::host_stopping(root)
                    || containers::read_one(engine, &id).is_err()
                {
                    log.line(
                        "the end's retry: given up (the container removed, or the host stopping)",
                    );
                    return;
                }
                std::thread::sleep(STEP);
            }
            delay = delay.saturating_mul(2).min(LONGEST_DELAY);
            let Ok(operation) = containers::operation_lock(engine, &id, std::time::Duration::ZERO)
            else {
                continue;
            };
            let Ok(Some(running)) =
                containers::running_lock(engine, &id, std::time::Duration::ZERO)
            else {
                continue;
            };
            // Read again under the locks: a command's recovery pass may have
            // settled it since (and a new start ended), whose end mustn't be
            // written over, nor said twice.
            let Ok(record) = containers::read_one(engine, &id) else {
                continue;
            };
            if record.finished.is_some() {
                drop((running, operation));
                // (F81: as below, a start that never ran sets off no
                // policy, though another settled its end.)
                if exit.and_then(|exit| exit.started) != Some(false) {
                    after_end(engine, &id, None);
                }
                return;
            }
            if !matches!(runtime::status(root, id.as_str()), Ok(Some(_))) {
                log.line("the end's retry: given up (its runtime state is gone: the next command settles it)");
                return;
            }
            // Its own start's state, not a later start's (its end failed to
            // be written too): this end is never written over that one's.
            let why = match (mine, runtime::peek_monitor(root, id.as_str())) {
                (None, _) => Some("its monitor isn't known"),
                (_, Err(_)) => Some("the runtime's state can't be read"),
                (Some(mine), Ok(recorded)) if !super::own_start(recorded, mine) => {
                    Some("the runtime's state is another start's")
                }
                _ => None,
            };
            if let Some(why) = why {
                log.line(&format!(
                    "the end's retry: given up ({why}: that start's clean-up, or the next \
                     command, settles it)"
                ));
                return;
            }
            let recorded = record_end(engine, &id, exit);
            let ran = say_end(engine, Some(&id), &recorded);
            match recorded {
                Ok(_) => {
                    match runtime::delete(root, id.as_str(), false) {
                        Ok(_) | Err(ContainerError::NotFound(_)) => {}
                        Err(error) => log.line(&format!("the runtime's state: {error}")),
                    }
                    drop((running, operation));
                    log.line("the end recorded, on a retry");
                    // (F81: a start that never ran sets off no policy.)
                    if ran {
                        after_end(engine, &id, None);
                    }
                    return;
                }
                Err(error) if retryable(&error) => {
                    log.line(&format!("the end's retry: still refused: {error:#}"));
                }
                Err(error) => {
                    log.line(&format!("the end's retry: given up: {error:#}"));
                    return;
                }
            }
        }
    }

    /// The stores `arguments` name.
    fn paths(arguments: &Arguments) -> Paths {
        let mut paths = Paths::from_env()
            .with_containers(&arguments.containers)
            .with_runtime_root(&arguments.runtime_root)
            .with_networks(&arguments.networks);
        if let Some(store) = &arguments.store {
            paths = paths.with_image_store(store);
        }
        if let Some(events) = &arguments.events {
            paths = paths.with_events(events);
        }
        paths
    }

    /// `__cleanup`'s work: its exit code (0 if all went, 1 if anything was
    /// left for later).
    pub(super) fn main(args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
        let holds = std::env::var(runtime::HOLDS_ENV)
            .ok()
            .and_then(|count| count.parse::<usize>().ok())
            .unwrap_or(0);
        // First, before anything is opened: every hold close-on-exec (the
        // helpers this starts mustn't inherit one), and owned.
        let fds = take_holds(holds);
        let Ok(arguments) = Arguments::parse(args) else {
            return 1;
        };
        // Which monitor started this, while it may still be there to ask:
        // what tells this start's runtime state from a later one's (F102).
        let mine = own_monitor();
        // The monitor that started this holds copies of the holds until it
        // has exited (it doesn't wait for this): a lease or an area isn't
        // free before then, and a sweep would pass it by.
        monitor_gone(mine.map(|(pid, _)| pid), std::time::Duration::from_secs(10));
        let log = Log::open(&arguments.containers, &arguments.id, "cleanup.log");
        let engine = Engine::new(
            paths(&arguments),
            Arc::new(log.clone()) as Arc<dyn Reporter>,
        );
        let exit = std::env::var_os(runtime::EXIT_RECORD_ENV)
            .map(|record| ExitRecord::parse(record.as_encoded_bytes()));
        let mut failed = false;
        let mut retry = false;
        let mut fail = |what: String| {
            log.line(&what);
            failed = true;
        };
        let (held, refused) = sorted(&kinds_from_env(&arguments.holds), fds);
        // What's refused is closed: the next runs' sweeps reclaim it.
        for why in refused {
            fail(why);
        }
        for (network, hold) in held.leases {
            if let Err(error) = crate::network::release_lease_hold(&engine, &network, hold) {
                fail(format!("the network {network}: {error:#}"));
            }
        }
        drop(held.ports);
        if let Err(why) = remove_run_dir(arguments.run_dir.as_deref(), held.run_dir) {
            fail(why);
        }
        drop(held.volumes);
        if let Some((name, area)) = held.area
            && let Err(error) = crate::image::release_area(&engine, &name, area)
        {
            fail(format!("the writable layer: {error:#}"));
        }
        // A container's own is kept for its next start (closed only),
        // unless it goes with its end.
        if let Some((name, area)) = held.owned_area
            && arguments.auto_remove
            && let Err(error) = crate::image::release_area(&engine, &name, area)
        {
            fail(format!("the writable layer: {error:#}"));
        }
        let (id, recorded) = match ContainerId::parse(&arguments.id) {
            Ok(id) => {
                let recorded = record_end(&engine, &id, exit.as_ref());
                (Some(id), recorded)
            }
            Err(error) => (None, Err(error)),
        };
        let ran = say_end(&engine, id.as_ref(), &recorded);
        let ended = match recorded {
            // Only now: until the record has it, the runtime's exit record is
            // the only copy of how it ended.
            Ok(_) => match runtime::delete(&arguments.runtime_root, &arguments.id, false) {
                Ok(_) | Err(ContainerError::NotFound(_)) => true,
                Err(error) => {
                    fail(format!("the runtime's state: {error}"));
                    false
                }
            },
            Err(error) => {
                // A full disk, or the store held past its wait: retried
                // below, once all is let go (F45).
                retry = retryable(&error);
                fail(format!("the record: {error:#}"));
                false
            }
        };
        // Only once its end is recorded and its runtime state gone: else a
        // later `rm` (or start) finds what's left and settles it.
        let mut made = Vec::new();
        if arguments.auto_remove && ended {
            // Then the container itself (`--rm`): its pin, and its record
            // (with this log, and its logs).
            match ContainerId::parse(&arguments.id).and_then(|id| removed_itself(&engine, &id)) {
                Ok(anonymous) => made = anonymous,
                Err(error) => fail(format!("removing the container: {error:#}")),
            }
        }
        // Only for an end the policy follows (F81): else a waiting `rm`
        // would find it restarting for a start that never ran.
        let restarting = (!arguments.auto_remove && ended && ran)
            .then(|| restart_held(&engine, &arguments.id))
            .flatten();
        // Last: free, it says everything of this start is let go (a
        // `start` or `rm` waits for it).
        drop((held.unknown, held.running));
        // Then its anonymous volumes, as Docker's `--rm` takes them: only
        // now, its holds on them let go.
        crate::run::volumes::remove_anonymous(&engine, &made);
        test_pause_before_the_restart();
        // Then its restart policy, if it has one and it ended (and wasn't
        // removed): this process waits out the backoff and starts it again.
        // Not after a start that never ran (F81: a user's `start` that
        // failed, its last end put back; Docker's policy follows a run, not
        // a failed start; a policy's own failed restart is retried by its
        // waiter). An end that couldn't be recorded is retried first.
        if !arguments.auto_remove
            && let Ok(id) = ContainerId::parse(&arguments.id)
        {
            if ended {
                if ran {
                    crate::run::restart_policy::after_end(&engine, &id, restarting);
                }
            } else if retry {
                retry_end(&engine, &arguments, exit.as_ref(), mine, &log);
            }
        }
        i32::from(failed)
    }

    /// The monitor that started this: its PID (`SHIPYARD_MONITOR_PID`), and
    /// its start time, as it told it (`SHIPYARD_MONITOR_STARTED`), or read
    /// while it's still this process's parent (an older monitor tells
    /// none; once it has exited, its PID may be another's). `None` without
    /// the PID.
    fn own_monitor() -> Option<(i32, Option<u64>)> {
        let pid = std::env::var(runtime::MONITOR_PID_ENV)
            .ok()?
            .parse::<i32>()
            .ok()?;
        let told = std::env::var(runtime::MONITOR_STARTED_ENV)
            .ok()
            .and_then(|started| started.parse::<u64>().ok());
        let parent = nix::unistd::getppid().as_raw() == pid;
        // Read, then asked again: a parent gone in between may have had its
        // PID taken by another process.
        let started = told.or_else(|| {
            runtime::process_started(pid)
                .filter(|_| parent && nix::unistd::getppid().as_raw() == pid)
        });
        Some((pid, started))
    }

    /// Waits (up to `wait`) for the monitor that started this (`monitor`:
    /// its PID, from `SHIPYARD_MONITOR_PID`) to exit: while it lives, it's this process's
    /// parent; once gone, this one is someone else's (`getppid` changes).
    /// Its PID is told, not taken from `getppid` here: the monitor may be
    /// gone already, and whoever took this process on then isn't it (a PID
    /// reused since can't be: the one that took it on existed before).
    fn monitor_gone(monitor: Option<i32>, wait: std::time::Duration) {
        let Some(monitor) = monitor.map(nix::unistd::Pid::from_raw) else {
            return;
        };
        let deadline = std::time::Instant::now() + wait;
        while nix::unistd::getppid() == monitor && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The `count` descriptors from 3 up, made close-on-exec and owned. One
    /// that isn't open is left out (in order: its place stays, as `None`).
    fn take_holds(count: usize) -> Vec<Option<OwnedFd>> {
        (0..count)
            .map(|index| {
                let fd = RawFd::try_from(index).ok()?.checked_add(3)?;
                // SAFETY: inherited from the monitor at this number for this
                // process to own, and nothing here has opened anything yet
                // that could be it.
                unsafe { shipyard_sys::fd::take_inherited(fd, shipyard_sys::fd::OnExec::Close) }
                    .ok()
            })
            .collect()
    }

    /// A `--rm` container `id`, removed: the anonymous volumes its create
    /// made, for the caller to remove once its holds on them are let go.
    fn removed_itself(
        engine: &crate::Engine,
        id: &ContainerId,
    ) -> crate::error::Result<Vec<String>> {
        let made = crate::run::volumes::made_anonymous(engine, id);
        crate::run::remove::forget(engine, id)?;
        Ok(made)
    }

    /// The holds, by what they are.
    #[derive(Default)]
    struct Held {
        leases: Vec<(String, OwnedFd)>,
        ports: Vec<OwnedFd>,
        run_dir: Vec<(HoldKind, OwnedFd)>,
        volumes: Vec<OwnedFd>,
        area: Option<(String, OwnedFd)>,
        owned_area: Option<(String, OwnedFd)>,
        running: Option<OwnedFd>,
        /// Holds whose kinds can't be told (their count isn't what's said
        /// of them): kept until `running` goes, last, so no lock of this
        /// start is free before its end is recorded.
        unknown: Vec<OwnedFd>,
    }

    /// What the holds are ([`kinds`]), as the monitor says
    /// ([`HOLD_LABELS_ENV`](runtime::HOLD_LABELS_ENV)) or `arguments` do.
    fn kinds_from_env(arguments: &[HoldKind]) -> Vec<Option<HoldKind>> {
        kinds(
            arguments,
            std::env::var(runtime::HOLD_LABELS_ENV).ok().as_deref(),
        )
    }

    /// Whether a kind is one of a start's alone (its `running`, its run
    /// directory, its area): only believed of a hold if `create`'s
    /// arguments, the engine's own, said so (a label is anyone's of the
    /// container's user to give, a later hold's too).
    fn one_of_a_kind(kind: &HoldKind) -> bool {
        !matches!(
            kind,
            HoldKind::Lease { .. } | HoldKind::Port | HoldKind::Volume
        )
    }

    /// What the holds are, in order: as their labels say (`labels`, the
    /// monitor's [`HOLD_LABELS_ENV`](runtime::HOLD_LABELS_ENV): those since
    /// `create` too), else as the arguments do (`arguments`: a monitor from
    /// before 9.7b1 says no labels). A label that isn't a kind's, or names
    /// one of a start's alone that the arguments don't, is `None`: that
    /// hold is closed, the others kept (see [`sorted`]).
    fn kinds(arguments: &[HoldKind], labels: Option<&str>) -> Vec<Option<HoldKind>> {
        match labels {
            None => arguments.iter().cloned().map(Some).collect(),
            Some("") => Vec::new(),
            Some(labels) => labels
                .split(',')
                .map(|label| {
                    HoldKind::from_arg(label)
                        .filter(|kind| !one_of_a_kind(kind) || arguments.contains(kind))
                })
                .collect(),
        }
    }

    /// The descriptors `fds` sorted as `kinds` say, each checked to be what
    /// its kind is (a file, a directory, a socket), and why any was
    /// refused. One that isn't open, isn't its kind, has no kind known, or
    /// is a second of a start's alone (a second `running`), is refused (and
    /// closed), the others kept; a count that isn't what's said of them
    /// keeps them all unknown (which is which can't be known), closed last.
    fn sorted(kinds: &[Option<HoldKind>], fds: Vec<Option<OwnedFd>>) -> (Held, Vec<String>) {
        let mut held = Held::default();
        if kinds.len() != fds.len() {
            let said = format!(
                "{} holds, and what's said of them describes {}: closed last",
                fds.len(),
                kinds.len()
            );
            held.unknown = fds.into_iter().flatten().collect();
            return (held, vec![said]);
        }
        let mut refused = Vec::new();
        for (place, (kind, fd)) in kinds.iter().zip(fds).enumerate() {
            let Some(fd) = fd else {
                refused.push(format!("hold {place} isn't open"));
                continue;
            };
            let Some(kind) = kind else {
                refused.push(format!("hold {place} is of no kind known here: closed"));
                continue;
            };
            let mode = nix::sys::stat::fstat(&fd).map_or(0, |stat| stat.st_mode);
            if !kind.fits(mode) {
                refused.push(format!("hold {place} isn't a {}: closed", kind.to_arg()));
                continue;
            }
            let twice = match kind {
                HoldKind::Area { .. } => held.area.is_some(),
                HoldKind::OwnedArea { .. } => held.owned_area.is_some(),
                HoldKind::Running => held.running.is_some(),
                HoldKind::RunDirLock | HoldKind::RunDir => {
                    held.run_dir.iter().any(|(other, _)| other == kind)
                }
                HoldKind::Lease { .. } | HoldKind::Port | HoldKind::Volume => false,
            };
            if twice {
                refused.push(format!(
                    "hold {place} is a second {}: closed",
                    kind.to_arg()
                ));
                continue;
            }
            match kind {
                HoldKind::Lease { network } => held.leases.push((network.clone(), fd)),
                HoldKind::Port => held.ports.push(fd),
                HoldKind::RunDirLock | HoldKind::RunDir => held.run_dir.push((kind.clone(), fd)),
                HoldKind::Volume => held.volumes.push(fd),
                HoldKind::Area { name } => held.area = Some((name.clone(), fd)),
                HoldKind::OwnedArea { name } => held.owned_area = Some((name.clone(), fd)),
                HoldKind::Running => held.running = Some(fd),
            }
        }
        (held, refused)
    }

    /// Removes the run directory at `path`, if it's still the one held
    /// (`holds`: its lock file's and its own descriptors): its identity
    /// checked against the directory's descriptor, so a path that now
    /// leads elsewhere is left alone. Removed while still locked (a sweep
    /// never sees it half gone); the locks go after.
    fn remove_run_dir(
        path: Option<&std::path::Path>,
        holds: Vec<(HoldKind, OwnedFd)>,
    ) -> Result<(), String> {
        let Some(path) = path else {
            return Ok(());
        };
        let Some((_, dir)) = holds.iter().find(|(kind, _)| *kind == HoldKind::RunDir) else {
            return Ok(());
        };
        let held = nix::sys::stat::fstat(dir)
            .map_err(|errno| format!("the run directory: {}", errno.desc()))?;
        let there = std::fs::symlink_metadata(path)
            .map_err(|error| format!("the run directory {}: {error}", path.display()))?;
        if there.dev() != held.st_dev || there.ino() != held.st_ino || !there.is_dir() {
            return Err(format!(
                "the run directory {} isn't the one held any more: left as it is",
                path.display()
            ));
        }
        let removed = std::fs::remove_dir_all(path)
            .map_err(|error| format!("removing the run directory {}: {error}", path.display()));
        drop(holds);
        removed
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
    mod tests {
        use super::*;

        fn scratch(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("shipyard-cleanup-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn open(path: &std::path::Path) -> OwnedFd {
            OwnedFd::from(std::fs::File::open(path).unwrap())
        }

        /// A test hook pauses a debug build only: a release's variable,
        /// set or not, is nothing; an empty one is nothing either.
        #[test]
        fn pauses_are_debug_builds_only() {
            let set = || Some(std::ffi::OsString::from("/tmp/pause"));
            assert_eq!(
                pause_path(set(), true),
                Some(std::path::PathBuf::from("/tmp/pause"))
            );
            assert_eq!(pause_path(set(), false), None);
            assert_eq!(pause_path(None, true), None);
            assert_eq!(pause_path(Some(std::ffi::OsString::new()), true), None);
        }

        /// The holds' kinds come from the monitor's labels when it gives
        /// them (those held since `create` too), else from the arguments;
        /// a label that isn't a kind's, or names one of a start's alone the
        /// arguments don't, is no kind (that hold alone is refused).
        #[test]
        fn kinds_come_from_labels_or_arguments() {
            let area = |name: &str| HoldKind::Area {
                name: name.to_owned(),
            };
            let arguments = [HoldKind::Port, HoldKind::Running, area("run-1")];
            assert_eq!(
                kinds(&arguments, None),
                arguments.iter().cloned().map(Some).collect::<Vec<_>>()
            );
            let db = HoldKind::Lease {
                network: "db".to_owned(),
            };
            assert_eq!(
                kinds(&arguments, Some("port,running,lease:db,area:run-1")),
                [
                    Some(HoldKind::Port),
                    Some(HoldKind::Running),
                    Some(db),
                    Some(area("run-1"))
                ]
            );
            assert_eq!(kinds(&arguments, Some("")), []);
            assert_eq!(
                kinds(&arguments, Some("port,fd-3,area:someone-elses,run-dir")),
                [Some(HoldKind::Port), None, None, None]
            );
        }

        /// Each hold goes where its kind says, if it is that kind; one that
        /// isn't, isn't open, has no kind, or is a second of a start's
        /// alone, is refused and the others kept; a count that isn't
        /// what's said of them keeps them all, unknown, to close last.
        #[test]
        fn holds_are_sorted_and_checked() {
            let dir = scratch("sorted");
            std::fs::write(dir.join("lock"), "").unwrap();
            let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let kinds = [
                HoldKind::Lease {
                    network: "web".to_owned(),
                },
                HoldKind::Port,
                HoldKind::Volume,
                HoldKind::Area {
                    name: "run-1".to_owned(),
                },
            ];
            let fds = || {
                vec![
                    Some(open(&dir.join("lock"))),
                    Some(OwnedFd::from(socket.try_clone().unwrap())),
                    Some(open(&dir.join("lock"))),
                    Some(open(&dir)),
                ]
            };
            let kinds: Vec<Option<HoldKind>> = kinds.into_iter().map(Some).collect();
            let (held, refused) = sorted(&kinds, fds());
            assert!(refused.is_empty(), "the control: {refused:?}");
            assert_eq!(held.leases.len(), 1);
            assert_eq!(held.leases[0].0, "web");
            assert_eq!((held.ports.len(), held.volumes.len()), (1, 1));
            assert_eq!(
                held.area.as_ref().map(|(name, _)| name.as_str()),
                Some("run-1")
            );
            // The volume's slot given a directory, the port's none.
            let mut wrong = fds();
            wrong[2] = Some(open(&dir));
            wrong[1] = None;
            let (held, refused) = sorted(&kinds, wrong);
            assert_eq!(refused.len(), 2, "{refused:?}");
            assert!(held.volumes.is_empty() && held.ports.is_empty());
            assert_eq!(held.leases.len(), 1, "the others kept");
            assert!(held.area.is_some());
            // A count that isn't what's said: none known, all kept to be
            // closed last.
            let (held, refused) = sorted(&kinds[..3], fds());
            assert_eq!(refused.len(), 1, "{refused:?}");
            assert!(held.leases.is_empty() && held.area.is_none());
            assert_eq!(held.unknown.len(), 4, "kept, closed last");
            // A hold of no kind known: it alone refused; a second of a
            // start's alone (an area here): refused, the first kept.
            let mut odd = kinds.clone();
            odd[0] = None;
            odd.push(Some(HoldKind::Area {
                name: "run-1".to_owned(),
            }));
            let mut more = fds();
            more.push(Some(open(&dir)));
            let (held, refused) = sorted(&odd, more);
            assert_eq!(refused.len(), 2, "{refused:?}");
            assert!(held.leases.is_empty());
            assert_eq!(held.ports.len(), 1, "the others kept");
            assert!(held.area.is_some(), "the first area kept");
            std::fs::remove_dir_all(dir).unwrap();
        }

        /// The run directory goes only if its path still leads to the one
        /// held: one put there since is left as it is.
        #[test]
        fn the_run_dir_goes_only_if_it_is_the_one_held() {
            let dir = scratch("rundir");
            let path = dir.join("run");
            std::fs::create_dir_all(&path).unwrap();
            let held = vec![(HoldKind::RunDir, open(&path))];
            // Replaced since: moved away, another made in its place.
            std::fs::rename(&path, dir.join("moved")).unwrap();
            std::fs::create_dir_all(&path).unwrap();
            assert!(remove_run_dir(Some(&path), held).is_err());
            assert!(path.is_dir(), "the other one is left");
            // The one held: removed.
            let held = vec![(HoldKind::RunDir, open(&path))];
            let identity = std::fs::metadata(&path).unwrap().ino();
            assert_ne!(identity, 0);
            assert!(remove_run_dir(Some(&path), held).is_ok());
            assert!(!path.exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A retry settles only its own start's state (F102): the monitor that
    /// started it, by PID and start time; another start's monitor (another
    /// PID, or the same PID reused), or a state without one, isn't.
    #[test]
    fn a_retry_settles_only_its_own_starts_state() {
        assert!(own_start(Some((40, 7)), (40, Some(7))));
        assert!(
            own_start(Some((40, 7)), (40, None)),
            "its start time unread"
        );
        assert!(!own_start(Some((41, 7)), (40, Some(7))), "another monitor");
        assert!(!own_start(Some((40, 9)), (40, Some(7))), "its PID reused");
        assert!(!own_start(None, (40, Some(7))), "no monitor recorded");
    }

    fn args(list: &[&str]) -> Vec<std::ffi::OsString> {
        list.iter().map(std::ffi::OsString::from).collect()
    }

    /// The arguments `detached` gives read back; one missing, or unknown,
    /// is refused.
    #[test]
    fn arguments_read_back() {
        let parsed = Arguments::parse(args(&[
            "--id",
            "abc",
            "--containers",
            "/c",
            "--runtime-root",
            "/r",
            "--networks",
            "/n",
            "--store",
            "/s",
            "--run-dir",
            "/d",
            "--events",
            "/e",
            "--hold",
            "lease:web",
            "--hold",
            "port",
            "--hold",
            "area:run-1",
            "--auto-remove",
            "--hold",
            "owned-area:ctr-1",
            "--hold",
            "running",
        ]))
        .unwrap();
        assert_eq!(parsed.id, "abc");
        assert!(parsed.auto_remove, "a flag alone, anywhere");
        assert_eq!(parsed.store.as_deref(), Some(std::path::Path::new("/s")));
        assert_eq!(parsed.events.as_deref(), Some(std::path::Path::new("/e")));
        assert_eq!(
            parsed.holds,
            [
                HoldKind::Lease {
                    network: "web".to_owned()
                },
                HoldKind::Port,
                HoldKind::Area {
                    name: "run-1".to_owned()
                },
                HoldKind::OwnedArea {
                    name: "ctr-1".to_owned()
                },
                HoldKind::Running,
            ]
        );
        assert!(
            Arguments::parse(args(&["--id", "abc"])).is_err(),
            "the stores are needed"
        );
        assert!(Arguments::parse(args(&["--id"])).is_err());
        assert!(
            Arguments::parse(args(&[
                "--id",
                "a",
                "--containers",
                "/c",
                "--runtime-root",
                "/r",
                "--networks",
                "/n",
                "--hold",
                "socket"
            ]))
            .is_err()
        );
        assert!(Arguments::parse(args(&["--what", "x"])).is_err());
    }
}
