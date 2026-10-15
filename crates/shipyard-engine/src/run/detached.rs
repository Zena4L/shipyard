//! Containers that outlive the process that made them: made ([`create`]),
//! started ([`start`], as often as asked), removed ([`remove`]); and
//! [`detached`], a container made and started at once (`run -d`).
//!
//! What a start holds (its writable layer, its volumes, its network leases
//! and published ports, its run directory, its `running` file) is held by
//! `flock`s and bound sockets, handed to its monitor when it's created (the
//! runtime's holds): the monitor keeps them while the container lives, then
//! gives them to `__cleanup` (its exit command, this binary again), which
//! lets go of everything and records how the container ended.
//!
//! A container is a record (its config, the image it was made from, pinned
//! in the image store) and, while it runs, a runtime state. Its writable
//! layer is its own (`ctr-<id>`): made at its first start, taken again by
//! every later one, removed only with the container.
//!
//! **Two locks per container.** Its operation lock (`op.lock`) serializes
//! `start` and `rm`. Its `running` file is locked by a start and handed to
//! the monitor, then to `__cleanup`, which closes it last: free, everything
//! of the last start is let go, and a new start (or a removal) may begin.
//! The order is: the operation lock, the `running` file, the image store's
//! lock, the container store's (briefly). `__cleanup` takes neither of the
//! container's: nothing waits on it while holding what it needs.
//!
//! A start's order: the image (by the digest the container was made from),
//! the network and the run made ready, the container created from a plan
//! with the holds, connected to its networks through its init's network
//! namespace, recorded started, then started. A failure once it exists ends
//! it (`delete --force`), and its monitor's `__cleanup` lets go: one way for
//! every end.

use crate::Engine;
use crate::containers::{ContainerId, Name, Record};
use crate::error::Result;

use super::RunRequest;

/// A detached container, started. While this value lives, SIGINT,
/// SIGTERM, SIGHUP and SIGQUIT are still held off (as they were while it was made):
/// the caller says what it has to (the container's ID) first; dropping it
/// lets one sent meanwhile through.
#[derive(Debug)]
#[non_exhaustive]
pub struct Detached {
    /// Its ID.
    pub id: ContainerId,
    /// Its name.
    pub name: Name,
    /// The signals, held off until this goes.
    #[cfg(target_os = "linux")]
    _held: linux::HeldOff,
}

/// Makes a container of `request`, to be started later ([`start`]): the
/// request checked, its image pulled as `--pull` says (and extracted), the
/// record written, the image pinned (kept in the store while the container
/// exists, whatever happens to its tag). Nothing runs, nothing is held.
/// `auto_remove`: the container goes with its first end (`--rm`).
///
/// # Errors
///
/// What [`check`](super::check) refuses; a bundle, or no isolation
/// ([`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported): not
/// containers); the image can't be had; the name is taken
/// ([`ErrorKind::Exists`](crate::ErrorKind::Exists)); the store can't be
/// written.
pub fn create(engine: &Engine, request: RunRequest, auto_remove: bool) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::create(engine, request, auto_remove);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, request, auto_remove);
        Err(crate::error::linux_only("containers"))
    }
}

/// Starts the container `reference` names (an ID, a name, a serial or an
/// ID's prefix): it runs on once this process has gone, its output kept in
/// its logs (a new directory for each start), what it holds let go after it
/// ends (by `__cleanup`: the calling binary's `main` must call
/// [`cleanup_entry_point`](super::cleanup_entry_point) first, besides the
/// runtime's monitor entry point and the others `run` needs). A container
/// that's running is left as it is. Returns its record, as started.
///
/// A start that fails once the container exists ends it, and its record
/// says it never started this time (`Created`, as Docker leaves one).
///
/// # Errors
///
/// No such container ([`ErrorKind::NotFound`](crate::ErrorKind::NotFound));
/// another start or removal of it holds it, or its last start is still
/// being let go ([`ErrorKind::Busy`](crate::ErrorKind::Busy)); what
/// couldn't be made ready, created, connected or started.
pub fn start(engine: &Engine, reference: &str) -> Result<Record> {
    start_with(engine, reference, |_| {})
}

/// [`start`], calling `between` once the container is created, before
/// it's connected and started (not if it's up already: it isn't made):
/// [`start_attached`] attaches there, and tests fail it there.
///
/// # Errors
///
/// As [`start`].
#[doc(hidden)]
pub fn start_with(
    engine: &Engine,
    reference: &str,
    between: impl FnOnce(&ContainerId),
) -> Result<Record> {
    #[cfg(target_os = "linux")]
    return linux::start(engine, reference, between);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, between);
        Err(crate::error::linux_only("containers"))
    }
}

/// Starts the container `reference` names with the caller attached
/// (`start -a`): let in by its monitor after the container is made and
/// before its start gate opens, so nothing its command writes is missed;
/// then attached until it ends (its exit code) or the detach keys are typed
/// ([`Settled::Left`](crate::attach::Settled::Left)). One that's up
/// already is attached to as it is. If it can't be let in before the start
/// (its monitor refused), it's said
/// ([`WarningKind::AttachedLate`](crate::report::WarningKind::AttachedLate)) and
/// attached to once started.
///
/// # Errors
///
/// As [`start`] and [`attach::to_container`](crate::attach::to_container).
pub fn start_attached(
    engine: &Engine,
    reference: &str,
    options: &crate::attach::Options,
) -> Result<crate::attach::Settled> {
    #[cfg(target_os = "linux")]
    {
        use crate::attach::{self, Settled};
        use crate::report::WarningKind;
        let root = engine.paths.runtime_root()?.to_owned();
        // `None`: it was up already, and never made here.
        let mut early = None;
        let record = start_with(engine, reference, |id| {
            early = Some(attach_before_start(&root, id, options));
        })?;
        if let Some(Ok(welcomed)) = early {
            return attach::settle(Ok(welcomed.session()), || {
                attach::recorded_end(engine, &record)
            });
        }
        if let Some(Err(why)) = early {
            engine.warn(
                WarningKind::AttachedLate,
                format!(
                    "container {} couldn't be attached to before it started ({why}): what it \
                     wrote first may be missing",
                    record.name
                ),
            );
        }
        // Attached as it is now; one that has ended already ends with its
        // recorded code.
        attach::to_container(engine, record.id.as_str(), options).or_else(|error| {
            match error.kind() {
                crate::ErrorKind::NotRunning => attach::recorded_end(engine, &record)
                    .map(Settled::Exited)
                    .ok_or(error),
                _ => Err(error),
            }
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, options);
        Err(crate::error::linux_only("containers"))
    }
}

/// Removes the container `reference` names: its record, its writable
/// layer, its image's pin. A running one is refused unless `force` (then
/// it's killed first, and its clean-up waited for). Its ID.
///
/// One whose record can't be read (damaged, or a newer Shipyard's) is
/// found only by its full ID, and removed only with `force`: what its ID
/// names (what's left of its last start, killed; its writable layer, its
/// pin, its directory). Its volumes and networks are left as they are.
///
/// # Errors
///
/// No such container ([`ErrorKind::NotFound`](crate::ErrorKind::NotFound));
/// its record can't be read, and not `force`
/// ([`ErrorKind::Storage`](crate::ErrorKind::Storage));
/// it's running ([`ErrorKind::InUse`](crate::ErrorKind::InUse)); its last
/// start is still being let go, or another command holds it
/// ([`ErrorKind::Busy`](crate::ErrorKind::Busy)); what couldn't be removed.
pub fn remove(engine: &Engine, reference: &str, force: bool) -> Result<ContainerId> {
    #[cfg(target_os = "linux")]
    return linux::remove(engine, reference, force);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, force);
        Err(crate::error::linux_only("containers"))
    }
}

/// Lets go of container `id`'s image (its pin), then removes its record
/// (with its logs): the last of a container's removal, its writable layer
/// already gone, saying it's destroyed. `rm`'s, `--rm`'s (`__cleanup`) and
/// recovery's.
///
/// # Errors
///
/// The pin or the record couldn't be removed (the pin first: a record left
/// is removed again by a later `rm`).
#[cfg(target_os = "linux")]
pub(crate) fn forget(engine: &Engine, id: &ContainerId) -> Result<()> {
    // Read first: once it's gone, its name is too.
    let gone = crate::containers::find(engine, id.as_str())
        .map(|record| crate::events::container(&record, "destroy"));
    unmake(engine, id)?;
    if let Ok(event) = gone {
        crate::events::emit(engine, &event);
    }
    Ok(())
}

/// [`forget`]'s work, unsaid: for a create undone before it was said to
/// be made.
#[cfg(target_os = "linux")]
fn unmake(engine: &Engine, id: &ContainerId) -> Result<()> {
    crate::image::unpin(engine, id.as_str())
        .map_err(|error| error.context("letting go of the container's image"))?;
    crate::containers::remove_record(engine, id)
}

/// Makes and starts a container of `request` at once (`run -d`): as
/// [`create`], then [`start`]. A start that fails leaves the container
/// made, never started (Docker's), unless `auto_remove`, or it was
/// interrupted (a Ctrl-C before the container was made): then it goes.
///
/// # Errors
///
/// As [`create`] and [`start`].
pub fn detached(engine: &Engine, request: RunRequest, auto_remove: bool) -> Result<Detached> {
    detached_with(engine, request, auto_remove, |_| {})
}

/// [`detached`], calling `between` once the container is created, before
/// it's connected and started: for tests that fail it there.
///
/// # Errors
///
/// As [`detached`].
#[doc(hidden)]
pub fn detached_with(
    engine: &Engine,
    request: RunRequest,
    auto_remove: bool,
    between: impl FnOnce(&ContainerId),
) -> Result<Detached> {
    #[cfg(target_os = "linux")]
    return linux::detached(engine, request, auto_remove, linux::Input::AsMade, between)
        .map(|(detached, _)| detached);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, request, auto_remove, between);
        Err(crate::error::linux_only("containers"))
    }
}

/// Let in by container `id`'s monitor (its runtime state under `root`)
/// before it starts, as `options` say: what [`start_attached`] and
/// [`attached`] do between its creation and its start gate's opening, so
/// nothing it writes is missed. Why not, if it isn't.
#[cfg(target_os = "linux")]
fn attach_before_start(
    root: &std::path::Path,
    id: &ContainerId,
    options: &crate::attach::Options,
) -> std::result::Result<crate::attach::Welcomed, String> {
    shipyard_runtime::container::attach_stream(root, id.as_str())
        .map_err(|error| error.to_string())
        .and_then(|stream| {
            crate::attach::welcomed(stream, options).map_err(|error| error.to_string())
        })
}

/// What a foreground run asks ([`attached`]), beyond its request (whose
/// `-i` says whether it takes the caller's input).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Foreground {
    /// Pass on the signals the caller gets (`--sig-proxy`: Ctrl-C, a
    /// `kill`), to the container: SIGTERM as a stop (its stop signal, then
    /// SIGKILL once its stop timeout is up), the others as they are.
    pub sig_proxy: bool,
    /// The keys that detach a run with a terminal it's typed into (`-it`):
    /// `None` for Docker's, Ctrl-P Ctrl-Q. Without `-it` none detach (any
    /// bytes may be input).
    pub detach_keys: Option<shipyard_runtime::attach::DetachKeys>,
}

/// How a foreground run ended ([`attached`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The container's exit code: its command's, or 128 + N for signal N;
    /// 0 if the run detached, 129 if the user's terminal hung up (the
    /// container running on, both times).
    pub code: u8,
    /// Its ID.
    pub id: ContainerId,
    /// Its name.
    pub name: Name,
    /// The signal passed on that ended it (SIGINT or SIGQUIT, its code
    /// 128 + that), if one did: the caller ends from it too, so a shell
    /// loop around the run stops as it would around the command.
    pub interrupted: Option<shipyard_runtime::process::Signal>,
}

/// Runs `request` in the foreground as a container (stage 9, step 9.4c3a;
/// Docker's model): made and started as `run -d` makes one (a record, a
/// generated name unless it has one, its monitor), the caller let in before
/// its start gate opens (nothing it writes is missed), its output to this
/// process's, its input this process's if `options` say (ending at its end
/// of file), the signals this process gets passed on, until it ends. Then
/// its end is waited for until it's recorded (with `auto_remove`, until
/// it's removed). Ends with its exit code.
///
/// With `-t` (step 9.4c3b) its terminal is its monitor's: sized as this
/// process's and resized with it, and raw with `-it` (from the background,
/// once in the foreground). Then `options`' detach keys (Ctrl-P Ctrl-Q by
/// default) end the run with 0, and a hang-up of this process's terminal
/// with 129; the container runs on.
///
/// The container is its monitor's, not this process's: should this process
/// end first, it runs on, as a detached one does.
///
/// A client that falls behind (more than the monitor holds for it: a
/// stopped one, a slow reader) is let go of: said
/// ([`WarningKind::FellBehind`](crate::report::WarningKind::FellBehind)),
/// then the end is waited for. One whose output can't be written any more
/// (`run … | head`) waits for the end too.
///
/// # Errors
///
/// As [`detached`]; an attachment that failed otherwise.
pub fn attached(
    engine: &Engine,
    request: RunRequest,
    auto_remove: bool,
    options: Foreground,
) -> Result<Ran> {
    #[cfg(target_os = "linux")]
    return linux::attached(engine, request, auto_remove, options);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, request, auto_remove, options);
        Err(crate::error::linux_only("containers"))
    }
}

/// How a copy reaches a container's files ([`container_files`]).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
pub(crate) enum Files<'a> {
    /// It's up: by its runtime state root and ID (its monitor holds its
    /// files; a helper joins its namespaces).
    Live {
        /// The runtime's state root.
        root: &'a std::path::Path,
        /// The runtime's container ID.
        id: &'a str,
    },
    /// It isn't: its root as a start would mount it, held (its operation
    /// lock, its writable layer, its volumes) while this lasts.
    Stopped(&'a shipyard_runtime::container::RootView),
}

/// Calls `copy` with the files of the container `reference` names (stage 9,
/// step 9.4c2): through the container if it's up; else its root as a start
/// would mount it (its writable layer over its image, its volumes), with
/// the container held meanwhile, so a start waits (on its operation lock)
/// and nothing else mounts that layer. A created container that never
/// started gets its writable layer made here, as its first start would.
///
/// # Errors
///
/// No such container ([`ErrorKind::NotFound`](crate::ErrorKind::NotFound),
/// also one its `--rm` removed meanwhile); one being worked on
/// ([`ErrorKind::Busy`](crate::ErrorKind::Busy)); what its root needs
/// (its image gone from the store, a volume in use); `copy`'s.
#[cfg(target_os = "linux")]
pub(crate) fn container_files<T>(
    engine: &Engine,
    reference: &str,
    copy: impl FnMut(Files<'_>) -> Result<T>,
) -> Result<T> {
    linux::container_files(engine, reference, copy)
}

#[cfg(target_os = "linux")]
pub(super) mod linux {
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::fs::DirBuilderExt as _;
    use std::time::{Duration, SystemTime};

    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
    use shipyard_runtime::container::{self as runtime, CreateOptions, ExitCommand, LogOptions};
    use shipyard_runtime::container::{Plan, Status, StdinMode};

    use crate::containers::{
        self, BoundPort, Config, ContainerId, Exit, Labels, Name, NewContainer, Record,
    };
    use crate::error::{Context as _, Error, ErrorKind, Result, bail};
    use crate::image::PullPolicy;
    use crate::network::{NetworkMode, RunNetwork};
    use crate::report::{Event, WarningKind};
    use crate::run::holds::{HoldKind, Holds};
    use crate::run::{Checked, RunRequest, check};
    use crate::{Engine, Paths};

    /// How long a command working on a container (`start`, `stop`, `rm`,
    /// `rename`…) waits for another's operation on it: a start may pull and
    /// extract meanwhile.
    pub(in crate::run) const OP_WAIT: Duration = Duration::from_secs(120);
    /// How long they wait for the last start's holders (its monitor, its
    /// `__cleanup`) to let go: `__cleanup` waits up to 10 s for its
    /// monitor, then releases, each step waiting up to 15 s for a lock.
    const RUNNING_WAIT: Duration = Duration::from_secs(60);

    pub(super) fn create(
        engine: &Engine,
        request: RunRequest,
        auto_remove: bool,
    ) -> Result<Record> {
        create_holding(engine, request, auto_remove).map(|(record, _)| record)
    }

    /// How a start takes its input (`-i`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::run) enum Input {
        /// As the container was made: open for whoever attaches, or none.
        AsMade,
        /// Ending at the first typist's end of file (a foreground `run
        /// -i`'s, Docker's).
        OnceItEnds,
    }

    impl Input {
        /// The container's input, `interactive` (`-i`) or not: a foreground
        /// `run -i`'s ends the container's at its end (Docker's); a later
        /// start's stays open for whoever attaches.
        fn stdin(self, interactive: bool) -> StdinMode {
            match (interactive, self) {
                (false, _) => StdinMode::Closed,
                (true, Self::OnceItEnds) => StdinMode::Once,
                (true, Self::AsMade) => StdinMode::Open,
            }
        }
    }

    /// `run -d`: made and started, a Ctrl-C held off from the record on
    /// until it's all done (or undone: interrupted before the container
    /// was made, the record goes too, before the Ctrl-C ends this process).
    /// With its record as started.
    pub(super) fn detached(
        engine: &Engine,
        request: RunRequest,
        auto_remove: bool,
        input: Input,
        between: impl FnOnce(&ContainerId),
    ) -> Result<(super::Detached, Record)> {
        let (record, held_off) = create_holding(engine, request, auto_remove)?;
        let started = start_inner(engine, record.id.as_str(), between, Some(&held_off), input);
        let started = match started {
            Ok(started) => started,
            Err(error) => {
                // With `--rm`, the container goes too (its clean-up removes
                // it if it was created; else this does).
                let removed = (auto_remove || error.kind() == ErrorKind::Interrupted)
                    && super::remove(engine, record.id.as_str(), true).is_ok();
                drop(held_off);
                // Else it's left, made (Docker's): said, with its name.
                return Err(if removed || error.kind() == ErrorKind::Interrupted {
                    error
                } else {
                    error.context(format!(
                        "container {} is left, made but not started (`shipyard rm {}` removes \
                         it)",
                        record.name, record.name
                    ))
                });
            }
        };
        // Still held: the caller prints the ID before a Ctrl-C ends it.
        Ok((
            super::Detached {
                id: record.id,
                name: record.name,
                _held: held_off,
            },
            started,
        ))
    }

    /// [`create`], its record returned with the Ctrl-C hold that began as
    /// it was made, still held.
    fn create_holding(
        engine: &Engine,
        mut request: RunRequest,
        auto_remove: bool,
    ) -> Result<(Record, HeldOff)> {
        // Its starts' streams are its monitor's: no terminal of the
        // caller's is looked for (`create -it` from a script).
        request.detached = true;
        // Removed as it ends, it can't be started again (Docker's rule).
        if auto_remove && request.restart != crate::containers::RestartPolicy::No {
            bail!(
                InvalidRequest,
                "--rm can't go with a restart policy (--restart {}): a container removed as it \
                 ends can't be started again",
                request.restart
            );
        }
        let mut planned = checked(engine, request)?;
        // A request refused settles nothing.
        crate::run::recover::recover_others(engine, None);
        // The image first: a pull that fails, or is interrupted, leaves no
        // record behind (Docker's order).
        let made_ready = match &planned.request.image {
            Some(reference) => {
                let maps = planned
                    .isolation
                    .as_ref()
                    .and_then(|isolation| isolation.id_maps.as_ref())
                    .filter(|_| nix::unistd::geteuid().is_root());
                Some(crate::image::make_ready(
                    engine,
                    reference,
                    planned.request.pull.unwrap_or(PullPolicy::Missing),
                    maps,
                )?)
            }
            None => None,
        };
        let (image_id, image_config) = made_ready.unzip();
        // Again, with `-P`'s ports, now the image's are known.
        if let Some(config) = &image_config {
            check_holds(&planned, &config.exposed_ports)?;
        }
        settle_the_health_check(engine, &mut planned.request, image_config.as_ref());
        let name = planned
            .request
            .name
            .as_deref()
            .map(Name::parse)
            .transpose()?;
        // From the record on, a Ctrl-C waits: it ends the call cleanly
        // (the record and its pin undone).
        let held_off = HeldOff::new();
        let record = containers::create_record(
            engine,
            NewContainer {
                name,
                config: Config::from_request(&planned.request)?,
                labels: Labels::default(),
                // In the record from the start: even if what follows is
                // cut short, every start runs this image, never a tag
                // moved since.
                image_id: image_id.clone(),
                auto_remove,
            },
        )?;
        let mut guard = RecordGuard {
            engine,
            id: Some(record.id.clone()),
        };
        if let (Some(reference), Some(digest)) = (&planned.request.image, &image_id) {
            crate::image::pin(engine, record.id.as_str(), reference, Some(digest))?;
        }
        if held_off.asked_to_stop() {
            bail!(Interrupted, "interrupted before the container was made");
        }
        guard.id = None;
        drop(guard);
        crate::events::emit(engine, &crate::events::container(&record, "create"));
        Ok((record, held_off))
    }

    /// The container's health check, settled once, at create (Docker's):
    /// what's asked (the flags) merged with its image's own
    /// ([`HealthCheck::merged`](crate::health::HealthCheck::merged)); a
    /// shell-form test runs with the image's shell (`SHELL`), if it sets
    /// one. Flags that tune a check there isn't are said to be ignored, and
    /// so is an image's check that can't be run (a container is made all the
    /// same, as Docker's; one asked is refused when the record is made).
    fn settle_the_health_check(
        engine: &Engine,
        request: &mut RunRequest,
        image: Option<&shipyard_image::RuntimeConfig>,
    ) {
        let asked = request.health.take();
        let image_check = image.and_then(|config| config.healthcheck.as_ref());
        let mut settled = crate::health::HealthCheck::merged(asked.as_ref(), image_check);
        if let (Some(check), Some(image)) = (&mut settled, image) {
            check.shell.clone_from(&image.shell);
        }
        let asked_a_test = asked.as_ref().is_some_and(|asked| !asked.test.is_empty());
        let mut unrunnable = false;
        if let Some(check) = &settled
            && !asked_a_test
            && let Err(error) = check.command()
        {
            engine.warn(
                WarningKind::HealthCheckIgnored,
                format!(
                    "the image's health check can't be run ({error}): it's left out \
                     (--health-cmd gives one)"
                ),
            );
            settled = None;
            unrunnable = true;
        }
        if asked.as_ref().is_some_and(|asked| !asked.is_disabled())
            && settled.is_none()
            && !unrunnable
        {
            let why = match (image, image_check) {
                (None, _) => "there's no image",
                (Some(_), Some(check)) if check.is_none() => "the image's own is NONE",
                _ => "the image has none of its own",
            };
            engine.warn(
                WarningKind::HealthCheckIgnored,
                format!(
                    "--health-*: no --health-cmd was given, and {why}: there's no health check \
                     to tune, so they're ignored"
                ),
            );
        }
        request.health = settled;
    }

    pub(super) fn start(
        engine: &Engine,
        reference: &str,
        between: impl FnOnce(&ContainerId),
    ) -> Result<Record> {
        crate::run::recover::recover_others(engine, None);
        start_inner(engine, reference, between, None, Input::AsMade)
    }

    /// [`start`], for a caller that has just run the recovery pass (boot's):
    /// not again for each container started.
    pub(in crate::run) fn start_recovered(engine: &Engine, reference: &str) -> Result<Record> {
        start_inner(engine, reference, |_| {}, None, Input::AsMade)
    }

    /// [`start`], under the caller's Ctrl-C hold if it gives one (`run
    /// -d`'s, from its record on), else its own from its image on.
    fn start_inner(
        engine: &Engine,
        reference: &str,
        between: impl FnOnce(&ContainerId),
        held: Option<&HeldOff>,
        input: Input,
    ) -> Result<Record> {
        match take(engine, reference, WhenUp::Leave)? {
            Took::Taken(taken) => start_taken(engine, taken, between, held, input, |_| {}),
            Took::Up(record) => Ok(record),
            Took::Gone(record) => bail!(NotFound, "container {} was removed", record.name),
        }
    }

    /// Starts the container `taken` holds (its operation lock, its last
    /// start let go): [`start_inner`] once it's taken, and `restart` once
    /// it's stopped (under the same hold, so nothing comes between), its
    /// input (`-i`) as `input` says. A start that fails is given to
    /// `failed` before the operation lock is let go: what follows it (a
    /// restart's count, its next try) is part of it, never another
    /// command's to come between.
    pub(in crate::run) fn start_taken(
        engine: &Engine,
        taken: Taken,
        between: impl FnOnce(&ContainerId),
        held: Option<&HeldOff>,
        input: Input,
        failed: impl FnOnce(&Error),
    ) -> Result<Record> {
        let Taken {
            record,
            running,
            runtime_root,
            _operation,
            restarts,
        } = taken;
        let holding = Holding {
            record,
            running,
            runtime_root,
            restarts,
        };
        let started = start_holding(engine, holding, between, held, input);
        if let Err(error) = &started {
            failed(error);
        }
        started
    }

    /// What [`start_taken`] gives [`start_holding`] of what [`Taken`]
    /// holds: all but its operation lock, which it keeps until the start,
    /// and what follows a failed one, is done.
    struct Holding {
        record: Record,
        running: std::fs::File,
        runtime_root: std::path::PathBuf,
        restarts: Option<u32>,
    }

    /// [`start_taken`]'s start, its operation lock held by its caller.
    fn start_holding(
        engine: &Engine,
        holding: Holding,
        between: impl FnOnce(&ContainerId),
        held: Option<&HeldOff>,
        input: Input,
    ) -> Result<Record> {
        let Holding {
            record,
            running,
            runtime_root,
            restarts,
        } = holding;
        let id = record.id.clone();
        // What its last start left, put back if this one fails: a container
        // that ran keeps its history (its end, its code; boot starts it
        // again), never reading as never started.
        let before = containers::PreviousStart::of(&record);
        // Room for this start's logs, whoever starts it.
        crate::run::restart_policy::prune_before_a_start(engine, &id);
        let PlannedStart {
            planned,
            image,
            interface_sysctls,
        } = planned_start(engine, &record)?;
        let (defaults, area) = image.unzip();
        // From here, a Ctrl-C waits: before `create`, it ends the call
        // cleanly (everything undone); after, once the container runs on.
        let own = held.is_none().then(HeldOff::new);
        let held_off = held.or(own.as_ref());
        let exposed = defaults
            .as_ref()
            .map_or(&[][..], |defaults: &crate::run::ImageDefaults| {
                &defaults.config.exposed_ports
            });
        // With `-P`'s ports, before its network is set up.
        check_holds(&planned, exposed)?;
        let network = RunNetwork::set_up(engine, planned.network(), exposed)?;
        let ports = network.bound();
        let streams = planned.request.streams;
        let stdin = input.stdin(streams.interactive);
        // Its health check, for its companion (`finish` takes the plan).
        let health = planned.request.health.clone();
        let (invocation, volumes) = planned.finish(engine, defaults, network.etc())?;
        let plan = plan_of(engine, invocation, streams.tty)?;
        let holds = collect_holds(&network, &volumes, area.as_ref(), &running);
        let fds = holds.for_runtime();
        let settings = StartSettings {
            stdin,
            auto_remove: record.auto_remove,
            health: health.as_ref(),
        };
        let options = create_options(engine, &id, &network, &holds, &fds, settings)?;
        let created = if held_off.is_some_and(HeldOff::asked_to_stop) {
            Err(Error::new(
                ErrorKind::Interrupted,
                "interrupted before the container was made",
            ))
        } else {
            runtime::create_planned(&runtime_root, id.as_str(), &plan, &options)
                .map_err(Error::from)
        };
        let created = match created {
            Ok(created) => created,
            Err(error) => {
                // Not made: everything is let go here (the writable layer
                // kept: it's the container's), before a held signal can end
                // this process.
                drop(options);
                drop(fds);
                drop(holds);
                drop(network);
                drop(volumes);
                drop(area);
                drop(running);
                return Err(error);
            }
        };
        // Its monitor holds them now: this process's copies are closed,
        // never unlocked, and nothing is released.
        drop(options);
        drop(fds);
        drop(holds);
        let mut detached = network.into_detached();
        detached.interface_sysctls = interface_sysctls;
        for volume in volumes {
            volume.give_up();
        }
        if let Some(area) = area {
            area.give_up();
        }
        for warning in created.warnings {
            engine.warn(WarningKind::Runtime, warning);
        }
        between(&id);
        keep_aside(engine, &id, &before);
        // `running` kept until pasta's launcher has its copy too.
        let started = connect_and_start(
            engine,
            &runtime_root,
            &id,
            &detached,
            &ports,
            &running,
            restarts,
        );
        drop(running);
        let started = match started {
            Ok(record) => record,
            Err(error) => {
                unstarted(engine, &runtime_root, &id, before);
                return Err(error);
            }
        };
        containers::forget_previous_start(engine, &id);
        Ok(started)
    }

    /// Keeps `before` (what container `id`'s last start left) on disk
    /// before the record says it started: a start killed from here on (its
    /// container left made) is put back by the next recovery pass, never
    /// read as never started. What can't be kept is said.
    fn keep_aside(engine: &Engine, id: &ContainerId, before: &containers::PreviousStart) {
        if let Err(error) = containers::keep_previous_start(engine, id, before) {
            engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {}: what its last start left couldn't be kept aside (a start \
                     cut short now would read as never started): {error}",
                    id.short()
                ),
            );
        }
    }

    /// What container `record`'s start runs: its request (from its config,
    /// its hostname its short ID unless it has one, never pulling), checked;
    /// and its image, by the digest it was made from, with its own writable
    /// area (a version 1 record's image pinned now).
    fn planned_start(engine: &Engine, record: &Record) -> Result<PlannedStart> {
        let id = &record.id;
        let mut request = record.to_request()?;
        // Its interfaces' own settings (`net.ipv4.conf.eth1.*`): not its
        // init's to set (its interfaces are made after it has set the
        // others), but this start's, once it has connected them.
        let interface_sysctls: Vec<(String, String)>;
        (interface_sysctls, request.sysctl) = std::mem::take(&mut request.sysctl)
            .into_iter()
            .partition(|(key, _)| shipyard_network::attach::interface_of_sysctl(key).is_some());
        request.pull = None;
        request.detached = true;
        request.container = Some(id.clone());
        // Docker's: the short ID, the same at every start.
        if request.hostname.is_none() {
            request.hostname = Some(id.short().to_owned());
        }
        let planned = checked(engine, request)?;
        // One for an interface its networks no longer make (a network left
        // since: `network disconnect`) is passed over, said: never a start
        // refused for good.
        let interface_sysctls: Vec<(String, String)> = interface_sysctls
            .into_iter()
            .filter(|setting| {
                let made =
                    check_interface_sysctls(std::slice::from_ref(setting), planned.network())
                        .is_ok();
                if !made {
                    engine.warn(
                        WarningKind::Runtime,
                        format!(
                            "--sysctl {}: the container has no such interface now (its networks \
                             changed): not set",
                            setting.0
                        ),
                    );
                }
                made
            })
            .collect();
        let owner = containers::require_store_id(engine)?;
        // Pinned again (nothing new if it is: a `create` cut short after its
        // record left none); a record without its image's digest (one made
        // before 9.4a) gets it now, from its tag.
        let image_id = match (&planned.request.image, &record.image_id) {
            (Some(reference), image_id) => {
                let digest =
                    crate::image::pin(engine, id.as_str(), reference, image_id.as_deref())?;
                if image_id.is_none() {
                    containers::update(engine, id, |record| {
                        record.image_id = Some(digest.clone());
                    })?;
                }
                Some(digest)
            }
            (None, _) => None,
        };
        let image = match &image_id {
            Some(digest) => {
                crate::run::prepare_owned_image(engine, &planned, id.as_str(), &owner, digest)?
            }
            None => None,
        };
        Ok(PlannedStart {
            planned,
            image,
            interface_sysctls,
        })
    }

    /// What a start of a container runs ([`planned_start`]).
    struct PlannedStart {
        /// Its run, checked.
        planned: crate::run::Planned,
        /// Its image's defaults and its writable area, if it has an image.
        image: Option<(crate::run::ImageDefaults, crate::image::RunArea)>,
        /// Its interfaces' own settings, for once they're made.
        interface_sysctls: Vec<(String, String)>,
    }

    /// Refuses interface settings (`net.ipv4.conf.IF.*`) for an interface
    /// the container's network doesn't make: one past its bridge networks'
    /// `eth{n}`, or any with pasta's network or none of its own.
    fn check_interface_sysctls(
        sysctls: &[(String, String)],
        plan: Option<&crate::network::NetworkPlan>,
    ) -> Result<()> {
        let count = match plan.map(|plan| &plan.mode) {
            Some(NetworkMode::Bridges(networks)) => networks.len(),
            _ => 0,
        };
        for (key, _) in sysctls {
            let interface = shipyard_network::attach::interface_of_sysctl(key).unwrap_or_default();
            let made = (0..count).any(|n| shipyard_network::store::interface_name(n) == interface);
            if !made {
                bail!(
                    InvalidRequest,
                    "--sysctl {key}: the container has no interface {interface} (its networks \
                     make {})",
                    if count == 0 {
                        "none of their own".to_owned()
                    } else {
                        format!("eth0 to eth{}", count - 1)
                    }
                );
            }
        }
        Ok(())
    }

    /// How a foreground run attaches (its held client's options), and how
    /// its container takes its input: by its `-i` and `-t`.
    fn foreground_streams(
        request: &RunRequest,
        detach_keys: Option<shipyard_runtime::attach::DetachKeys>,
    ) -> (crate::attach::Options, Input) {
        use shipyard_runtime::attach::DetachKeys;
        let (stdin, terminal) = (request.streams.interactive, request.streams.tty);
        let attaching = crate::attach::Options {
            // Typed into a terminal (`-it`): Docker's keys, or those asked
            // for. Otherwise none: any bytes may be input.
            detach_keys: if stdin && terminal {
                detach_keys.unwrap_or_else(DetachKeys::default_keys)
            } else {
                DetachKeys::none()
            },
            stdin,
            // The container waits for it: a slow reader loses no output, a
            // fast writer no input.
            held: true,
        };
        // Its input ends at the caller's end of file, unless it's a
        // terminal's (closing a terminal is a hang-up; its Ctrl-D is a key).
        let input = if stdin && !terminal {
            Input::OnceItEnds
        } else {
            Input::AsMade
        };
        (attaching, input)
    }

    pub(super) fn attached(
        engine: &Engine,
        request: RunRequest,
        auto_remove: bool,
        options: super::Foreground,
    ) -> Result<super::Ran> {
        use shipyard_runtime::process::SignalWatch;
        let root = engine.paths.runtime_root()?.to_owned();
        let super::Foreground {
            sig_proxy,
            detach_keys,
        } = options;
        let terminal = request.streams.tty;
        // `-it` needs a terminal to type on (Docker's rule): refused before
        // anything is made.
        if request.streams.interactive && terminal && !crate::streams::Terminals::detect().stdin {
            return Err(crate::StreamsError::InputNotATerminal.into());
        }
        let (attaching, input) = foreground_streams(&request, detach_keys);
        // A terminal's window resizes, from the watch (it has SIGWINCH): to
        // the session as a pipe, never to the container as a signal. Made
        // before the container: a failure leaves nothing behind.
        let resizes = resize_pipe(terminal && sig_proxy)?;
        // Put back at the end: the watch's own restore is the mask under
        // the Ctrl-C hold it's made in.
        let mask = SigSet::thread_get_mask().ok();
        let mut early = None;
        let mut watch = None;
        let mut waiter = None;
        let made = detached(engine, request, auto_remove, input, |id| {
            // Told its exit code however its attachment ends (`--rm` may
            // remove it before this would read its record).
            waiter = runtime::waiter(&root, id.as_str()).unwrap_or_else(|error| {
                engine.warn(
                    WarningKind::Runtime,
                    format!(
                        "couldn't wait for container {} as it ends: {error}",
                        id.short()
                    ),
                );
                None
            });
            // Once it exists: a Ctrl-C before ends the run cleanly (it's
            // undone); from here on it's the container's.
            if sig_proxy {
                watch = SignalWatch::install()
                    .map_err(|error| {
                        engine.warn(
                            WarningKind::Runtime,
                            format!("signals won't be passed on to the container: {error}"),
                        );
                    })
                    .ok();
            }
            early = Some(super::attach_before_start(&root, id, &attaching));
        });
        let (made, record) = match made {
            Ok(made) => made,
            Err(error) => {
                drop(watch);
                restore_mask(mask.as_ref());
                return Err(error);
            }
        };
        // Its hold on Ctrl-C let go (the watch, if any, has them now).
        drop(made);
        let interrupted = std::sync::atomic::AtomicI32::new(0);
        let forwarding = Forwarding {
            root: &root,
            record: &record,
            resizes: resizes.as_ref().map(|(_, write)| write),
            interrupted: &interrupted,
        };
        let ending = std::thread::scope(|scope| {
            let forwarder = watch
                .as_ref()
                .map(|watch| Forwarder::start(scope, watch, forwarding))
                .transpose();
            // The forwarder stops (dropped) once its end is known.
            forwarder.and_then(|_forwarder| {
                // (Only with the watch: without, the session has its own.)
                let resized = resizes
                    .as_ref()
                    .filter(|_| watch.is_some())
                    .map(|(read, _)| read.as_fd());
                attached_until_its_end(engine, &root, &record, early, waiter, resized)
            })
        });
        // Signals as they were before the run, before its end is waited
        // for: a Ctrl-C now ends this process (the container has ended).
        drop(watch);
        if matches!(ending, Ok(Ending::HungUp)) {
            ignore_hangups();
        }
        restore_mask(mask.as_ref());
        let code = match ending? {
            // It runs on: nothing to wait for.
            Ending::Detached => return Ok(left(record, 0)),
            // As the in-process run's (128 + SIGHUP).
            Ending::HungUp => return Ok(left(record, 129)),
            Ending::Exited(code) => {
                // Its end recorded (and, with `--rm`, it removed) before
                // this returns: `ps -a` right after says how it ended.
                drop(containers::running_lock(engine, &record.id, RUNNING_WAIT));
                code
            }
            Ending::Settled(code) => code,
        };
        if auto_remove {
            warn_if_left(engine, &record);
        }
        let code = u8::try_from(code).unwrap_or(shipyard_runtime::process::EXIT_ENGINE_ERROR);
        let interrupted = shipyard_runtime::process::Signal::from_raw(interrupted.into_inner())
            .ok()
            .filter(|signal| i32::from(code) == 128 + signal.as_raw());
        Ok(super::Ran {
            code,
            id: record.id,
            name: record.name,
            interrupted,
        })
    }

    /// A foreground run's container has ended: said if it ran without a
    /// network for a while (its pasta died). Read as soon as the run's
    /// session ends, before its end is waited for: with `--rm`, its logs go
    /// with it.
    fn say_if_its_network_was_lost(engine: &Engine, record: &Record) {
        if let Some(lost) = containers::last_start_lost(engine, record) {
            engine.warn(
                WarningKind::NetworkLost,
                format!(
                    "container {} lost its network at {}: {}",
                    record.name, lost.time, lost.how
                ),
            );
        }
    }

    /// A foreground run left with `code`, its container running on.
    fn left(record: Record, code: u8) -> super::Ran {
        super::Ran {
            code,
            id: record.id,
            name: record.name,
            interrupted: None,
        }
    }

    /// How a foreground run's attachment ended.
    enum Ending {
        /// Its detach keys were typed: the container runs on.
        Detached,
        /// The user's terminal hung up: the container runs on (it was
        /// passed SIGHUP with `--sig-proxy`).
        HungUp,
        /// The container ended, with this code: its clean-up still to wait
        /// for.
        Exited(i32),
        /// With this code, its end settled already.
        Settled(i32),
    }

    /// A foreground run's container `record`, attached to (`early`: let in
    /// before it started, or why not) until it ends, `waiter` told its code.
    fn attached_until_its_end(
        engine: &Engine,
        root: &std::path::Path,
        record: &Record,
        early: Option<std::result::Result<crate::attach::Welcomed, String>>,
        waiter: Option<runtime::Waiter>,
        resizes: Option<std::os::fd::BorrowedFd<'_>>,
    ) -> Result<Ending> {
        use crate::attach::{AttachError, Ended, Settled};
        let name = &record.name;
        let ended = match early {
            Some(Ok(welcomed)) => match resizes {
                Some(resizes) => welcomed.session_resized(resizes),
                None => welcomed.session(),
            },
            Some(Err(why)) => {
                engine.warn(
                    WarningKind::AttachedLate,
                    format!(
                        "container {name} couldn't be attached to before it started ({why}): \
                         its output is in its logs"
                    ),
                );
                say_if_its_network_was_lost(engine, record);
                return end_code(engine, root, record, waiter).map(Ending::Settled);
            }
            // Its start never let it in: up already, which a container just
            // made can't be.
            None => bail!(
                Runtime,
                "container {name} was up before this run started it: it runs on (`shipyard \
                 attach {name}`)"
            ),
        };
        // Ended (not left running): said at once if it ran without a
        // network, before its end is waited for (with `--rm`, its logs go).
        if !matches!(ended, Ok(Ended::Detached | Ended::HungUp)) {
            say_if_its_network_was_lost(engine, record);
        }
        match ended {
            Ok(Ended::Exited(code)) => Ok(Ending::Exited(code)),
            Ok(Ended::Detached) => Ok(Ending::Detached),
            Ok(Ended::HungUp) => Ok(Ending::HungUp),
            // Held, it's never let go of for falling behind; else, the rest
            // of its output is in the logs.
            Ok(Ended::Dropped(reason)) => {
                engine.warn(
                    WarningKind::FellBehind,
                    format!(
                        "container {name}'s monitor let go of this run ({reason}): the rest of \
                         its output is in its logs"
                    ),
                );
                end_code(engine, root, record, waiter).map(Ending::Settled)
            }
            // Its output can't be written any more (`run … | head`): as in a
            // pipeline, the container's writer gets SIGPIPE (it writes to
            // its monitor, which never closes on it).
            Err(AttachError::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                if let Ok(pipe) = shipyard_runtime::process::Signal::from_raw(libc::SIGPIPE) {
                    pass_on(root, record, pipe);
                }
                end_code(engine, root, record, waiter).map(Ending::Settled)
            }
            ended => {
                let settled = crate::attach::settle(Ok(ended), || {
                    crate::attach::recorded_end(engine, record)
                })
                .map_err(|error| {
                    if containers::is_up(engine, &record.id) {
                        error.context(format!(
                            "container {name} runs on (`shipyard stop {name}` ends it)"
                        ))
                    } else {
                        error
                    }
                })?;
                Ok(match settled {
                    Settled::Exited(code) => Ending::Settled(code),
                    Settled::Left => Ending::Settled(end_code(engine, root, record, waiter)?),
                })
            }
        }
    }

    /// A pipe for a terminal's window resizes (`wanted`): non-blocking, a
    /// byte written for each SIGWINCH, drained for one resize.
    fn resize_pipe(wanted: bool) -> Result<Option<(OwnedFd, OwnedFd)>> {
        use nix::fcntl::OFlag;
        Ok(if wanted {
            Some(nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?)
        } else {
            None
        })
    }

    /// What a [`Forwarder`] passes signals on to, and notes.
    #[derive(Clone, Copy)]
    struct Forwarding<'scope> {
        /// The runtime's state root.
        root: &'scope std::path::Path,
        /// The container.
        record: &'scope Record,
        /// Where a terminal's window resizes go (a byte each), instead of
        /// to the container as SIGWINCH; and this process's resumes
        /// (SIGCONT).
        resizes: Option<&'scope std::os::fd::OwnedFd>,
        /// The last SIGINT or SIGQUIT passed on.
        interrupted: &'scope std::sync::atomic::AtomicI32,
    }

    /// Passes the signals `watch` catches on to a foreground run's
    /// container (`--sig-proxy`), from a thread of its own, until dropped:
    /// also while this process waits on a slow reader of its output, or for
    /// the container's end. SIGTERM is a stop (the container's stop signal,
    /// then SIGKILL once its stop timeout is up: `docker stop`'s); SIGWINCH
    /// of a terminal container is a resize (its session's); the others go
    /// as they are. The last SIGINT or SIGQUIT is noted.
    struct Forwarder<'scope> {
        stop: std::os::fd::OwnedFd,
        thread: Option<std::thread::ScopedJoinHandle<'scope, ()>>,
    }

    impl<'scope> Forwarder<'scope> {
        fn start<'env>(
            scope: &'scope std::thread::Scope<'scope, 'env>,
            watch: &'scope shipyard_runtime::process::SignalWatch,
            to: Forwarding<'scope>,
        ) -> Result<Self> {
            use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
            let (stopped, stop) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
            // SIGTSTP held off on the thread (it inherits this mask): the
            // terminal's raw mode handles it, on this thread only.
            let mask = SigSet::thread_get_mask()?;
            let mut held = mask;
            held.add(nix::sys::signal::Signal::SIGTSTP);
            held.thread_set_mask()?;
            let thread = scope.spawn(move || {
                loop {
                    let mut fds = [
                        PollFd::new(watch.fd(), PollFlags::POLLIN),
                        PollFd::new(std::os::fd::AsFd::as_fd(&stopped), PollFlags::POLLIN),
                    ];
                    match poll(&mut fds, PollTimeout::NONE) {
                        Ok(_) => {}
                        Err(nix::errno::Errno::EINTR) => continue,
                        // Can't watch: nothing more is passed on.
                        Err(_) => return,
                    }
                    let asked_to_stop = fds
                        .get(1)
                        .and_then(PollFd::revents)
                        .is_some_and(|events| !events.is_empty());
                    let taken = watch.take();
                    // Resumed (`fg`, after the shell set the terminal as it
                    // keeps it for the job): the session looks at raw mode
                    // again, and at the window's size.
                    if taken.continued
                        && let Some(resizes) = to.resizes
                    {
                        let _ = nix::unistd::write(resizes, &[1]);
                    }
                    for signal in taken.signals {
                        if signal.as_raw() == libc::SIGWINCH
                            && let Some(resizes) = to.resizes
                        {
                            // Full already: a resize is due anyway.
                            let _ = nix::unistd::write(resizes, &[1]);
                            continue;
                        }
                        if matches!(signal.as_raw(), libc::SIGINT | libc::SIGQUIT) {
                            to.interrupted
                                .store(signal.as_raw(), std::sync::atomic::Ordering::Relaxed);
                        }
                        pass_on(to.root, to.record, signal);
                    }
                    if asked_to_stop {
                        return;
                    }
                }
            });
            mask.thread_set_mask()?;
            Ok(Self {
                stop,
                thread: Some(thread),
            })
        }
    }

    impl Drop for Forwarder<'_> {
        fn drop(&mut self) {
            let _ = nix::unistd::write(&self.stop, &[1]);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Passes `signal` on to container `record` (under the runtime state
    /// `root`): SIGTERM as its stop, the others to its init. One that has
    /// ended meanwhile takes none.
    fn pass_on(root: &std::path::Path, record: &Record, signal: shipyard_runtime::process::Signal) {
        tracing::debug!(%signal, "forwarding a signal to the container");
        let passed = if signal.as_raw() == libc::SIGTERM {
            crate::run::stop::StopOptions::default()
                .policy(record)
                .and_then(|policy| crate::run::stop::stop_up(root, record, policy))
        } else {
            runtime::kill(root, record.id.as_str(), signal).map_err(Error::from)
        };
        if let Err(error) = passed
            && !matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotRunning)
        {
            tracing::debug!("couldn't pass {signal} on: {error:#}");
        }
    }

    /// Container `record`'s exit code, once it ends: from its monitor (a
    /// waiter's: `waiter`, let in before it started, else one now; told
    /// before its clean-up, so even a `--rm` one's), its end then let go
    /// of; else, ended already, from its record.
    ///
    /// # Errors
    ///
    /// Its monitor refusing to be waited on; ended and removed (`--rm`)
    /// before its code could be read ([`ErrorKind::NotFound`]).
    fn end_code(
        engine: &Engine,
        root: &std::path::Path,
        record: &Record,
        waiter: Option<runtime::Waiter>,
    ) -> Result<i32> {
        let told = match waiter {
            Some(waiter) => waiter.exit_code(),
            None => runtime::wait(root, record.id.as_str()),
        };
        match told {
            Ok(Some(code)) => {
                drop(containers::running_lock(engine, &record.id, RUNNING_WAIT));
                return Ok(code);
            }
            // Up still, and its monitor won't say (too many waiters).
            Err(error) if containers::is_up(engine, &record.id) => {
                return Err(Error::from(error)
                    .context(format!("waiting for container {} to end", record.name)));
            }
            _ => {}
        }
        if let Some(code) = crate::attach::recorded_end(engine, record) {
            return Ok(code);
        }
        bail!(
            NotFound,
            "container {} ended and was removed (--rm) before its exit code could be read",
            record.name
        )
    }

    /// Says so if `--rm` didn't remove `record`'s container as it ended.
    fn warn_if_left(engine: &Engine, record: &Record) {
        if containers::find(engine, record.id.as_str()).is_ok() {
            engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {} wasn't removed as it ended (--rm)",
                    record.name
                ),
            );
        }
    }

    /// After a hang-up: a later SIGHUP (the terminal's session leader sends
    /// its foreground group one as it ends) mustn't end this process before
    /// it says how the run went. Called while SIGHUP is still held off (the
    /// watch put back the mask it was made under): ignored, a pending one
    /// goes too.
    fn ignore_hangups() {
        // SAFETY: ignoring a signal installs no handler.
        let _ = unsafe {
            nix::sys::signal::signal(
                nix::sys::signal::Signal::SIGHUP,
                nix::sys::signal::SigHandler::SigIgn,
            )
        };
    }

    /// Puts the signal mask `mask` back, if there is one.
    fn restore_mask(mask: Option<&SigSet>) {
        if let Some(mask) = mask {
            let _ = mask.thread_set_mask();
        }
    }

    pub(super) fn container_files<T>(
        engine: &Engine,
        reference: &str,
        mut copy: impl FnMut(super::Files<'_>) -> Result<T>,
    ) -> Result<T> {
        for _ in 0..3 {
            match take(engine, reference, WhenUp::Leave)? {
                Took::Up(record) => {
                    // Held for the copy: its helper may outlive the
                    // container's init (it isn't in its PID namespace), and
                    // no start may mount the same writable layer under it
                    // meanwhile.
                    let _operation = containers::operation_lock(engine, &record.id, OP_WAIT)?;
                    let root = engine.paths.runtime_root()?;
                    let id = record.id.as_str();
                    let up = matches!(
                        runtime::peek_status(root, id)?,
                        Some(Status::Running | Status::Paused)
                    );
                    if up {
                        match copy(super::Files::Live { root, id }) {
                            // Ended before its helper could join it
                            // (nothing copied yet: the helper comes first).
                            Err(error) if never_joined(&error) => {}
                            copied => return copied,
                        }
                    }
                    // Ending: its end let go of (its `running` file free:
                    // the clean-up done) before it's taken again, stopped.
                    // Else, its init gone and not yet reaped, it still reads
                    // as running, again and again.
                    drop(containers::running_lock(engine, &record.id, RUNNING_WAIT));
                }
                Took::Taken(taken) => {
                    let (view, held) = stopped_view(engine, &taken.record)?;
                    let copied = copy(super::Files::Stopped(&view));
                    // Its layer and volumes let go, then the container.
                    drop(held);
                    drop(taken);
                    return copied;
                }
                Took::Gone(record) => bail!(
                    NotFound,
                    "container {} was removed as it ended (--rm)",
                    record.name
                ),
            }
        }
        bail!(
            Busy,
            "container {reference} kept starting and ending while it was being copied: try again"
        )
    }

    /// Whether `error` is a helper refused for a container that ended before
    /// the helper joined it.
    fn never_joined(error: &Error) -> bool {
        matches!(
            error.find::<runtime::ContainerError>(),
            Some(
                runtime::ContainerError::WrongStatus { .. }
                    | runtime::ContainerError::HelperNeedsMount(_)
            )
        )
    }

    /// What a stopped container's root view holds: its writable layer, its
    /// volumes.
    struct ViewHeld {
        _area: Option<crate::image::RunArea>,
        _volumes: Vec<shipyard_storage::VolumeHold>,
    }

    /// Container `record`'s root as its start would mount it, for a copy:
    /// its image's layers under its own writable layer (taken, and made if
    /// it has none yet: a created container's first), or its `--rootfs`
    /// directory; its volumes bound and held (made only for a container
    /// never started, as its first start would: one removed since is said);
    /// its user namespace's maps, chosen as a start chooses them.
    /// Nothing else of a start's: no network, devices or security profile
    /// is looked at, the image isn't pinned again, the record isn't written.
    fn stopped_view(engine: &Engine, record: &Record) -> Result<(runtime::RootView, ViewHeld)> {
        let request = record.to_request()?;
        let is_root = nix::unistd::geteuid().is_root();
        let (id_maps, _) =
            request.user_namespace(is_root, shipyard_runtime::process::rootless_id_maps)?;
        let (mut root, area) = match (&request.image, &request.rootfs) {
            (Some(reference), _) => {
                let Some(image_id) = &record.image_id else {
                    bail!(
                        InvalidRequest,
                        "container {} was made by an older Shipyard: start it once, then copy",
                        record.name
                    );
                };
                let owner = containers::require_store_id(engine)?;
                // Real root's maps (`--uidmap`): its layer given to the
                // container's root, as a start gives it.
                let maps = id_maps.as_ref().filter(|_| is_root);
                let prepared = crate::image::prepare_owned(
                    engine,
                    record.id.as_str(),
                    &owner,
                    reference,
                    image_id,
                    maps,
                )?;
                let mut root = shipyard_runtime::isolation::Root::new(prepared.root);
                root.overlay = Some(prepared.overlay);
                (root, Some(prepared.area))
            }
            (None, Some(dir)) => {
                let path = std::fs::canonicalize(dir)
                    .map_err(|error| Error::wrap(ErrorKind::InvalidRequest, error))
                    .with_context(|| format!("its --rootfs {}", dir.display()))?;
                (shipyard_runtime::isolation::Root::new(path), None)
            }
            (None, None) => bail!(
                Unsupported,
                "container {} has no root filesystem of its own to copy",
                record.name
            ),
        };
        root.read_only = request.read_only;
        let mounts = request
            .volume_mounts()
            .with_context(|| format!("container {}'s volumes", record.name))?;
        let named = crate::run::volumes::bind_volumes(engine, &mut root, mounts)?;
        // Never started: its volumes made, as its first start would make
        // them (Docker's `create` does). Else one missing was removed since.
        let volumes = if record.started.is_none() {
            crate::run::volumes::hold_volumes(engine, &named)?
        } else {
            crate::run::volumes::hold_existing_volumes(engine, &named)?
        };
        Ok((
            runtime::RootView { root, id_maps },
            ViewHeld {
                _area: area,
                _volumes: volumes,
            },
        ))
    }

    pub(super) fn remove(engine: &Engine, reference: &str, force: bool) -> Result<ContainerId> {
        crate::run::recover::recover_others(engine, Some(reference));
        let when_up = if force { WhenUp::Kill } else { WhenUp::Refuse };
        let took = match take(engine, reference, when_up) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return match unreadable(engine, reference) {
                    Some(id) if force => remove_unreadable(engine, &id).map(|()| id),
                    Some(id) => Err(containers::read_one(engine, &id)
                        .err()
                        .map_or(error, |why| {
                            why.context(format!(
                                "container {}'s record can't be read: `rm -f {id}` removes what's \
                             left of it",
                                id.short()
                            ))
                        })),
                    None => Err(error),
                };
            }
            took => took?,
        };
        // `_operation` bound, not `..`: the operation lock is held until the
        // removal is done (a field left to `..` would go at once).
        let Taken {
            record,
            running,
            runtime_root: _,
            _operation,
            restarts: _,
        } = match took {
            Took::Taken(taken) => taken,
            // Its own `--rm` clean-up removed it meanwhile: removed (but for
            // its monitor's cgroup, which that clean-up was inside).
            Took::Gone(record) => {
                runtime::remove_monitor_cgroup(record.id.as_str());
                return Ok(record.id);
            }
            Took::Up(record) => bail!(InUse, "container {} is running", record.name),
        };
        // Its policy about to start it again: up, as Docker sees it. (Unless
        // what was to restart it is gone: its lock free.)
        if !force && record.next_start.is_some() {
            if containers::restarting_lock(engine, &record.id, Duration::ZERO).is_ok() {
                containers::update(engine, &record.id, |record| record.next_start = None)?;
            } else {
                bail!(
                    InUse,
                    "container {} is restarting: stop it first, or remove it with -f",
                    record.name
                );
            }
        }
        let id = record.id.clone();
        crate::image::remove_owned_area(engine, id.as_str())
            .map_err(|error| error.context("removing the container's writable layer"))?;
        super::forget(engine, &id)?;
        drop(running);
        // Its monitor's, from outside it: its last processes have let go.
        runtime::remove_monitor_cgroup(id.as_str());
        Ok(id)
    }

    /// The container `reference` is the full ID of, if it has a directory
    /// in the store but no record [`containers::find`] could read.
    fn unreadable(engine: &Engine, reference: &str) -> Option<ContainerId> {
        ContainerId::parse(reference)
            .ok()
            .filter(|id| containers::exists(engine, id.as_str()))
    }

    /// Removes container `id`, whose record can't be read, by what its ID
    /// names: under its operation lock, what's left of its last start
    /// killed and its clean-up waited for (its `running` file free), then
    /// its writable layer, its pin and its directory, as [`remove`] does.
    fn remove_unreadable(engine: &Engine, id: &ContainerId) -> Result<()> {
        let _operation = containers::operation_lock(engine, id, OP_WAIT)?;
        delete_state(engine.paths.runtime_root()?, id)?;
        let Some(running) = containers::running_lock(engine, id, RUNNING_WAIT)? else {
            bail!(
                Busy,
                "container {}'s last start is still being let go: try again",
                id.short()
            );
        };
        crate::image::remove_owned_area(engine, id.as_str())
            .map_err(|error| error.context("removing the container's writable layer"))?;
        super::unmake(engine, id)?;
        drop(running);
        runtime::remove_monitor_cgroup(id.as_str());
        crate::events::emit_about(
            engine,
            crate::events::Kind::Container,
            "destroy",
            id.as_str(),
        );
        Ok(())
    }

    /// What a container being worked on is held by: its operation lock,
    /// its `running` file (its last start let go).
    pub(in crate::run) struct Taken {
        pub(in crate::run) record: Record,
        pub(in crate::run) running: std::fs::File,
        pub(in crate::run) runtime_root: std::path::PathBuf,
        pub(in crate::run) _operation: nix::fcntl::Flock<std::fs::File>,
        /// A restart its policy makes (its number since its user last
        /// started it), or `None`: its user's own start.
        pub(in crate::run) restarts: Option<u32>,
    }

    /// What [`take`] found.
    pub(in crate::run) enum Took {
        /// Taken, to start, stop or remove.
        Taken(Taken),
        /// Up, and asked to be left so (a `start` of a running container).
        Up(Record),
        /// Removed meanwhile (by its own `--rm` clean-up, which takes no
        /// operation lock).
        Gone(Record),
    }

    /// What [`take`] does with a container that's up.
    #[derive(Clone, Copy)]
    pub(in crate::run) enum WhenUp {
        /// Leaves it: [`Took::Up`].
        Leave,
        /// Refuses ([`ErrorKind::InUse`]).
        Refuse,
        /// Kills it, then waits for its clean-up.
        Kill,
        /// Stops it (its own stop signal and timeout, unless these say
        /// otherwise), then waits for its clean-up.
        Stop(crate::run::StopOptions),
        /// Stops it as `Stop` does, for the host's shutdown (`system stop`):
        /// not its user's stop (its restart policy brings it back at boot).
        HostStop(crate::run::StopOptions),
    }

    /// Container `reference`, taken to work on (start, stop, restart or
    /// remove it): its operation lock, then (one up dealt with as `when_up`
    /// says) its last start let go (its `running` file free), what that
    /// start left settled (`reconcile`). A runtime state `created` or `creating` is a dead
    /// start's (a start holds the operation lock until it returns, and the
    /// container is `created` only while it does): it's deleted, its
    /// monitor with it, so its holds go.
    pub(in crate::run) fn take(engine: &Engine, reference: &str, when_up: WhenUp) -> Result<Took> {
        let found = containers::find(engine, reference)?;
        // A stop (or a kill) is the user's: recorded before anything is
        // sent, so a clean-up racing it never restarts the container.
        let stopping = matches!(when_up, WhenUp::Stop(_) | WhenUp::Kill);
        if stopping {
            mark_stopped_by_user(engine, &found.id);
        }
        let asked = ask_first(engine, &found, when_up)?;
        let operation = containers::operation_lock(engine, &found.id, OP_WAIT)?;
        // Again, under the lock: a start that held it meanwhile cleared the
        // mark as it started.
        if stopping {
            mark_stopped_by_user(engine, &found.id);
        }
        // Again, by its ID, now nothing else works on it: a removal that
        // went first (this one waited for its lock) left nothing, not even
        // the directory the `running` file below is in. Read by its ID
        // alone (no other record read): a record is replaced whole.
        let record = containers::read_one(engine, &found.id)?;
        let id = record.id.clone();
        let runtime_root = engine.paths.runtime_root()?.to_owned();
        let delete = |what: &str| -> Result<()> {
            delete_state(&runtime_root, &id)
                .map_err(|error| error.context(format!("container {}: {what}", id.short())))
        };
        // A stop's timeout is waited for too, before its clean-up's.
        let mut running_wait = RUNNING_WAIT;
        match runtime::status(&runtime_root, id.as_str())? {
            Some(Status::Running | Status::Paused) => match when_up {
                WhenUp::Leave => return Ok(Took::Up(record)),
                WhenUp::Refuse => bail!(
                    InUse,
                    "container {} is running: stop it first, or remove it with -f",
                    record.name
                ),
                // Its monitor's `__cleanup` lets go, then frees `running`,
                // which is waited for below. Killed by its monitor if it was
                // asked (its exit recorded, as any end's); else forced.
                WhenUp::Kill if asked => {}
                WhenUp::Kill => delete("killing it")?,
                WhenUp::Stop(options) | WhenUp::HostStop(options) => {
                    let policy = options.policy(&record)?;
                    running_wait = RUNNING_WAIT.saturating_add(policy.timeout);
                    if !asked {
                        crate::run::stop::stop_up(&runtime_root, &record, policy)?;
                    }
                }
            },
            Some(Status::Created | Status::Creating) => {
                delete("ending a dead start's")?;
                // What its last start left, back (as the recovery pass does).
                crate::run::recover::undo_a_dead_start(engine, &record)?;
            }
            _ => {}
        }
        let running = match containers::running_lock(engine, &id, running_wait) {
            Ok(Some(running)) => running,
            Ok(None) => bail!(
                Busy,
                "container {} is still ending (its clean-up holds it): try again",
                record.name
            ),
            // Removed meanwhile, by its own `--rm` clean-up.
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Took::Gone(record)),
            Err(error) => return Err(error),
        };
        if containers::read_one(engine, &id).is_err() {
            return Ok(Took::Gone(record));
        }
        reconcile(engine, &runtime_root, &id)?;
        // An end nothing recorded (its monitor killed): settled, and said.
        let boot = crate::run::recover::boot_id();
        let record = crate::run::recover::settle_unrecorded_end(engine, &id, boot.as_deref())?;
        Ok(Took::Taken(Taken {
            record,
            running,
            runtime_root,
            _operation: operation,
            restarts: None,
        }))
    }

    /// Records that container `id`'s user stopped it (`stop`, `kill`,
    /// `restart`, `rm -f`): its restart policy leaves it so (Docker's).
    /// Removed meanwhile: nothing to record. A record that can't be written
    /// (a full disk) is said, and the stop goes on: never kept from it.
    pub(in crate::run) fn mark_stopped_by_user(engine: &Engine, id: &ContainerId) {
        let marked = containers::update(engine, id, |record| {
            record.stopped_by_user = true;
            // A restart that waits gives up (and the container reads as
            // exited at once).
            record.next_start = None;
        });
        match marked {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {}: that you stopped it couldn't be recorded (its restart policy \
                     may start it again): {error}",
                    id.short()
                ),
            ),
        }
    }

    /// A stop (`WhenUp::Stop`) or a kill (`WhenUp::Kill`, `rm -f`) of a
    /// container that's up, asked of its monitor before the operation lock
    /// is waited for: another command (a long `stop`) may hold it, and the
    /// monitor keeps the earliest SIGKILL of every stop it's asked. Whether
    /// it was asked. A kill that can't be asked so is done under the lock
    /// anyway (a forced delete).
    fn ask_first(engine: &Engine, found: &Record, when_up: WhenUp) -> Result<bool> {
        let runtime_root = engine.paths.runtime_root()?;
        let up = matches!(
            runtime::peek_status(runtime_root, found.id.as_str()),
            Ok(Some(Status::Running | Status::Paused))
        );
        match when_up {
            WhenUp::Stop(options) | WhenUp::HostStop(options) if up => {
                let policy = options.policy(found)?;
                crate::run::stop::stop_up(runtime_root, found, policy)?;
                Ok(true)
            }
            WhenUp::Kill if up => {
                let kill = shipyard_runtime::process::StopPolicy {
                    signal: shipyard_runtime::process::Signal::KILL,
                    timeout: Duration::ZERO,
                };
                // `rm -f`'s kill, as Docker says it.
                crate::events::emit(
                    engine,
                    &crate::events::container(found, "kill").with("signal", "SIGKILL"),
                );
                Ok(crate::run::stop::stop_up(runtime_root, found, kill).is_ok())
            }
            _ => Ok(false),
        }
    }

    /// Container `id`'s runtime state, left by a start whose holders have
    /// all let go (its `running` file free): a `__cleanup` killed or failed
    /// part way, a monitor killed outright, a crash before a start's end.
    /// How it ended is copied into the record if it ran, and the state is
    /// deleted (forced: a `created` one can only be stale here).
    pub(in crate::run) fn reconcile(
        engine: &Engine,
        runtime_root: &std::path::Path,
        id: &ContainerId,
    ) -> Result<()> {
        if runtime::status(runtime_root, id.as_str())?.is_none() {
            return Ok(());
        }
        if let Ok(Some(exit)) = runtime::exit_record(runtime_root, id.as_str()) {
            match exit.started {
                Some(true) => {
                    // Settled here only if its clean-up didn't (killed before
                    // it wrote the record): its end is said here, then.
                    let mut settled = false;
                    let record = containers::update(engine, id, |record| {
                        settled = record.exit.is_none();
                        record.finished = exit.finished.or_else(|| Some(SystemTime::now()));
                        record.exit = Some(Exit::from(&exit));
                    })?;
                    if settled {
                        crate::events::container_ended(engine, &record);
                    }
                }
                // Ended before it started: its record says it never did.
                Some(false) => {
                    containers::update(engine, id, |record| {
                        record.started = None;
                        record.finished = None;
                        record.exit = None;
                    })?;
                }
                None => {}
            }
        }
        delete_state(runtime_root, id).map_err(|error| {
            error.context(format!(
                "container {}'s runtime state, left by its last start",
                id.short()
            ))
        })
    }

    /// Deletes container `id`'s runtime state, forced: what's left of it
    /// is killed, its cgroup removed. None left is fine.
    pub(in crate::run) fn delete_state(
        runtime_root: &std::path::Path,
        id: &ContainerId,
    ) -> Result<()> {
        match runtime::delete(runtime_root, id.as_str(), true) {
            Ok(_) | Err(runtime::ContainerError::NotFound(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// What a start's monitor is given, besides the holds: how its input
    /// is kept, whether the container goes with its end, and its health
    /// check (run by a companion beside it).
    #[derive(Clone, Copy)]
    struct StartSettings<'h> {
        stdin: StdinMode,
        auto_remove: bool,
        health: Option<&'h crate::health::HealthCheck>,
    }

    /// What `create` is given besides the plan: the container's logs (a new
    /// directory for this start, stdin as `settings` say), the holds `fds`
    /// (`holds`'s), `__cleanup` to run after it, told what they are (and
    /// whether the container goes too), and, with a health check,
    /// `__health` beside it.
    fn create_options<'a>(
        engine: &Engine,
        id: &ContainerId,
        network: &RunNetwork,
        holds: &Holds<'_>,
        fds: &'a [shipyard_runtime::container::Hold<'a>],
        settings: StartSettings<'_>,
    ) -> Result<CreateOptions<'a>> {
        let StartSettings {
            stdin,
            auto_remove,
            health,
        } = settings;
        let exit_command = cleanup_command(
            &engine.paths,
            id,
            network.etc().map(crate::network::RunDir::path),
            holds,
            auto_remove,
        )?;
        let start_dir = logs_for_settings(engine, id)?;
        // Its checks, beside it, while this start runs.
        let companion = health
            .map(|_| crate::run::health::health_command(&engine.paths, id, &start_dir))
            .transpose()?;
        Ok(CreateOptions {
            logs: Some(LogOptions {
                dir: start_dir,
                settings: shipyard_runtime::logs::info::LogSettings::default(),
                stdin,
            }),
            holds: fds,
            exit_command: Some(exit_command),
            companion,
            ..CreateOptions::default()
        })
    }

    /// The plan the runtime runs: `invocation`'s process and isolation, with
    /// a terminal if `tty`. Its warnings are reported.
    fn plan_of(engine: &Engine, invocation: crate::run::Invocation, tty: bool) -> Result<Plan> {
        for warning in invocation.warnings {
            engine.report(Event::Warning(warning));
        }
        let Some(isolation) = invocation.isolation else {
            bail!(Unsupported, "a detached container needs isolation");
        };
        let mut plan = Plan::new(invocation.spec, isolation);
        if tty {
            plan.terminal = Some(shipyard_runtime::oci::BundleTerminal::default());
        }
        Ok(plan)
    }

    /// After container `id` was created and couldn't be started: its record
    /// says what it said `before` again (its clean-up may have run already,
    /// its init ended by itself: what it recorded isn't an exit either; a
    /// container that ran before keeps its last end, so boot still brings
    /// it back), and it's ended (its monitor's `__cleanup` lets go of what
    /// it held). What can't be done is said.
    fn unstarted(
        engine: &Engine,
        runtime_root: &std::path::Path,
        id: &ContainerId,
        before: containers::PreviousStart,
    ) {
        let reset = containers::update(engine, id, |record| before.restore(record));
        if reset.is_ok() {
            containers::forget_previous_start(engine, id);
        }
        if let Err(why) = reset {
            engine.warn(
                WarningKind::LeftForLater,
                format!("container {} reads as started: {why:#}", id.short()),
            );
        }
        // Gone already is ended: its clean-up ran (its init ended by itself).
        if let Err(why) = delete_state(runtime_root, id) {
            engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {} couldn't be ended, and holds what it was given: {why}",
                    id.short()
                ),
            );
        }
    }

    /// `request` checked, and refused if it's what a container can't be:
    /// a bundle's, without isolation, without a root filesystem of its own,
    /// with interface sysctls for an interface its networks don't make, or
    /// publishing more ports than its monitor can hold (`-P`'s are checked
    /// once its image is known).
    fn checked(engine: &Engine, request: RunRequest) -> Result<crate::run::Planned> {
        let planned = match check(engine, request)? {
            Checked::Bundle(_) => bail!(
                Unsupported,
                "a container from a bundle has no record: run it with `shipyard run --bundle`, \
                 or `shipyard-runtime`"
            ),
            Checked::Command(planned) => *planned,
        };
        if planned.request.no_isolation {
            bail!(
                Unsupported,
                "a container needs isolation (its root filesystem: an image or --rootfs): \
                 --no-isolation runs only in the foreground"
            );
        }
        // A container's plan needs a root of its own: without one it would
        // share the host's (the runtime refuses it at start).
        if planned.request.image.is_none() && planned.request.rootfs.is_none() {
            bail!(
                Unsupported,
                "a container needs a root filesystem of its own: give it an image or --rootfs \
                 (--no-image runs only in the foreground)"
            );
        }
        // Its interfaces' own settings: only for interfaces its networks
        // make (refused before anything is made).
        let interface: Vec<(String, String)> = planned
            .request
            .sysctl
            .iter()
            .filter(|(key, _)| shipyard_network::attach::interface_of_sysctl(key).is_some())
            .cloned()
            .collect();
        check_interface_sysctls(&interface, planned.network())?;
        check_holds(&planned, &[])?;
        Ok(planned)
    }

    /// Refuses, before anything is made, a container whose monitor would
    /// hold more than it can: at most [`MOST_HOLDS`](runtime::MOST_HOLDS),
    /// and fewer than this process's descriptor limit (the monitor inherits
    /// it), leaving room for the rest. Its published ports' sockets count
    /// (two a port on every address: IPv4 and IPv6), `-P`'s too (the image's
    /// `exposed` ports `-p` doesn't publish: none known yet, none counted),
    /// with an upper bound of its other holds ([`collect_holds`]'s): a lease
    /// per network, a volume each, the run directory (two), the writable
    /// area and `running`.
    fn check_holds(planned: &crate::run::Planned, exposed: &[String]) -> Result<()> {
        /// Descriptors kept free for everything else a run holds and opens.
        const SPARE: u64 = 64;
        /// The run directory (its lock and itself), the writable area and
        /// `running`.
        const FIXED: usize = 4;
        let publish = &planned.request.publish;
        let mut ports: Vec<&shipyard_network::ports::PortRequest> = publish.iter().collect();
        let published_all: Vec<shipyard_network::ports::PortRequest> =
            if planned.request.publish_all {
                exposed
                    .iter()
                    .filter_map(|key| shipyard_network::ports::parse_exposed(key))
                    .filter(|request| {
                        !publish.iter().any(|given| {
                            given.container_port == request.container_port
                                && given.protocol == request.protocol
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
        ports.extend(&published_all);
        let sockets: usize = ports
            .iter()
            .map(|port| if port.host_ip.is_none() { 2 } else { 1 })
            .sum();
        let leases = match planned.network().map(|plan| &plan.mode) {
            Some(NetworkMode::Bridges(networks)) => networks.len(),
            _ => 0,
        };
        let others = FIXED
            .saturating_add(leases)
            .saturating_add(planned.volumes.len());
        let (soft, _) = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
            .unwrap_or((u64::MAX, u64::MAX));
        let room = usize::try_from(soft.saturating_sub(SPARE))
            .unwrap_or(usize::MAX)
            .min(runtime::MOST_HOLDS);
        if sockets.saturating_add(others) > room {
            bail!(
                InvalidRequest,
                "{} published ports take up to {sockets} sockets, which a container's monitor \
                 holds with up to {others} other holds (its networks, volumes and files): at \
                 most {room} in all here{}",
                ports.len(),
                if room < runtime::MOST_HOLDS {
                    " (raise the descriptor limit, `ulimit -n`, for more)"
                } else {
                    ""
                }
            );
        }
        Ok(())
    }

    /// What the monitor is to hold: the network's (leases, ports, the run
    /// directory), the volumes', the writable area's, and the container's
    /// `running` file.
    fn collect_holds<'a>(
        network: &'a RunNetwork,
        volumes: &'a [shipyard_storage::VolumeHold],
        area: Option<&'a crate::image::RunArea>,
        running: &'a std::fs::File,
    ) -> Holds<'a> {
        let mut holds = Holds::default();
        network.holds(&mut holds);
        for volume in volumes {
            holds.add(HoldKind::Volume, volume.hold());
        }
        if let Some(area) = area
            && let (Some(name), Some(fd)) = (area.name(), area.hold())
        {
            let name = name.to_owned();
            let kind = if area.is_owned() {
                HoldKind::OwnedArea { name }
            } else {
                HoldKind::Area { name }
            };
            holds.add(kind, fd);
        }
        holds.add(HoldKind::Running, running.as_fd());
        holds
    }

    /// The log directory for this start of container `id` (the runtime
    /// makes it), its parent made if it isn't there.
    fn logs_for_settings(engine: &Engine, id: &ContainerId) -> Result<std::path::PathBuf> {
        let logs = containers::logs_dir(engine, id)?;
        // Not recursive: the record's directory must be there (one made
        // here for a record removed meanwhile would trouble the store).
        match std::fs::DirBuilder::new().mode(0o700).create(&logs) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(Error::from(error)
                    .context(format!("making {}", logs.display()))
                    .io_as(ErrorKind::Storage));
            }
            _ => {}
        }
        Ok(logs.join(containers::next_logs_name(&logs)))
    }

    /// Connects container `id` to its networks (its veths, or a pasta that
    /// follows its init), records it started (with the `ports` it
    /// publishes, its last end cleared), and starts it. pasta's launcher
    /// gets a copy of `running`, held until pasta has ended.
    fn connect_and_start(
        engine: &Engine,
        runtime_root: &std::path::Path,
        id: &ContainerId,
        network: &crate::network::DetachedNetwork,
        ports: &[BoundPort],
        running: &std::fs::File,
        restarts: Option<u32>,
    ) -> Result<Record> {
        warn_if_subreaper(engine);
        if !network.connects.is_empty() {
            let netns = runtime::network_namespace(runtime_root, id.as_str())?;
            for connect in &network.connects {
                connect
                    .connect(netns.as_fd())
                    .map_err(|message| Error::new(ErrorKind::Network, message))?;
            }
            // Its interfaces' own settings, now they're there.
            if !network.interface_sysctls.is_empty() {
                shipyard_network::attach::set_interface_sysctls(
                    netns.as_fd(),
                    &network.interface_sysctls,
                )
                .map_err(|error| {
                    Error::new(
                        ErrorKind::InvalidRequest,
                        format!("the container's interface settings: {error}"),
                    )
                })?;
            }
        }
        if network.has_pasta() {
            let init = runtime::init_process(runtime_root, id.as_str())?;
            // This start's log directory (`create_options` made it: the
            // last): where pasta's death while it runs is said.
            let start_dir = containers::logs_dir(engine, id)
                .ok()
                .and_then(|logs| containers::last_logs_name(&logs).map(|name| logs.join(name)));
            network
                .start_pasta(
                    &shipyard_network::pasta::InitToFollow {
                        pid: init.pid,
                        start_time: init.start_time,
                        pidfd: init.pidfd.as_ref(),
                        lost_to: start_dir.as_deref(),
                    },
                    // Held by its launcher too: the container's end (its
                    // `running` free) waits for pasta to have let go of
                    // its ports.
                    Some(running.as_fd()),
                )
                .map_err(|message| Error::new(ErrorKind::Network, message))?;
        }
        // Before the start: a command that ends at once must find it set.
        let record = containers::update(engine, id, |record| {
            record.started = Some(SystemTime::now());
            record.finished = None;
            record.exit = None;
            record.boot_id = crate::run::recover::boot_id();
            record.stopped_by_user = false;
            // A user's start counts its policy's restarts afresh (and its
            // backoff); a restart counts itself.
            record.restart_count = restarts.unwrap_or(0);
            if restarts.is_none() {
                record.restart_delay_ms = 0;
            }
            record.next_start = None;
            record.ports = ports.to_vec();
        })?;
        for warning in runtime::start(runtime_root, id.as_str())? {
            engine.warn(WarningKind::Runtime, warning);
        }
        // At its start's time (before it: an end written meanwhile, by a
        // command that ended at once, is later).
        let mut started = crate::events::container(&record, "start");
        if let Some(at) = record.started {
            started = started.at(at);
        }
        crate::events::emit(
            engine,
            &match restarts {
                Some(count) => started.with("restartCount", count),
                None => started,
            },
        );
        Ok(record)
    }

    /// Warns if this process is a child subreaper (one that ran a
    /// foreground run made itself one, for good): it would adopt the
    /// container's monitor, its exit command and its pasta launcher (their
    /// double forks hand them to the nearest), and a later run's sweep of
    /// its leftover children would kill them.
    fn warn_if_subreaper(engine: &Engine) {
        if nix::sys::prctl::get_child_subreaper().unwrap_or(false) {
            engine.warn(
                WarningKind::Runtime,
                "this process reaps its orphans (it ran a container in the foreground): it adopts \
                 the container's monitor and pasta, and a later foreground run here would end \
                 them",
            );
        }
    }

    /// The exit command that cleans up after container `id`: this very
    /// binary (`/proc/self/exe`, in the monitor: the binary that made it,
    /// even upgraded or removed since), with what it needs to find and let
    /// go of what the monitor holds, and whether the container goes too.
    fn cleanup_command(
        paths: &Paths,
        id: &ContainerId,
        run_dir: Option<&std::path::Path>,
        holds: &Holds<'_>,
        auto_remove: bool,
    ) -> Result<ExitCommand> {
        let text =
            |path: &std::path::Path| crate::run::helper::arg_text(path, "a container's clean-up");
        let mut args = vec![
            crate::run::cleanup::CLEANUP_ARG.to_owned(),
            "--id".to_owned(),
            id.as_str().to_owned(),
            "--containers".to_owned(),
            text(paths.containers()?)?,
            "--runtime-root".to_owned(),
            text(paths.runtime_root()?)?,
            "--networks".to_owned(),
            text(paths.networks())?,
        ];
        if let Ok(store) = paths.image_store() {
            args.extend(["--store".to_owned(), text(store)?]);
        }
        if let Some(dir) = run_dir {
            args.extend(["--run-dir".to_owned(), text(dir)?]);
        }
        args.extend(["--events".to_owned(), text(&paths.events()?)?]);
        if auto_remove {
            args.push("--auto-remove".to_owned());
        }
        args.extend(holds.args());
        Ok(ExitCommand::new("/proc/self/exe", args)?)
    }

    /// Removes a record (and its image's pin) made for a container that
    /// then couldn't be made whole, unless disarmed (`id` taken).
    struct RecordGuard<'a> {
        engine: &'a Engine,
        id: Option<ContainerId>,
    }

    impl Drop for RecordGuard<'_> {
        fn drop(&mut self) {
            // Never said to be made: removed unsaid.
            if let Some(id) = self.id.take()
                && let Err(error) = super::unmake(self.engine, &id)
            {
                self.engine.warn(
                    WarningKind::LeftForLater,
                    format!("container {} couldn't be removed: {error:#}", id.short()),
                );
            }
        }
    }

    /// SIGINT, SIGTERM, SIGHUP and SIGQUIT held off in this thread while it
    /// lives, then as they were (one sent meanwhile is delivered then). A
    /// mask is inherited across `fork` and `exec`: every process started
    /// meanwhile (the container's monitor, a network's DNS server) empties
    /// its own before its `exec`.
    #[derive(Debug)]
    pub(in crate::run) struct HeldOff {
        before: Option<SigSet>,
    }

    impl HeldOff {
        /// Whether one of them was sent meanwhile (it waits, pending:
        /// `sigpending`).
        fn asked_to_stop(&self) -> bool {
            if self.before.is_none() {
                return false;
            }
            // SAFETY: an all-zero `sigset_t` is a valid one to fill.
            let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
            // SAFETY: `pending` is valid for writing.
            if unsafe { libc::sigpending(&raw mut pending) } != 0 {
                return false;
            }
            [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
                .into_iter()
                // SAFETY: `pending` was filled by `sigpending`.
                .any(|signal| unsafe { libc::sigismember(&raw const pending, signal) } == 1)
        }

        pub(in crate::run) fn new() -> Self {
            let mut set = SigSet::empty();
            for signal in [
                Signal::SIGINT,
                Signal::SIGTERM,
                Signal::SIGHUP,
                Signal::SIGQUIT,
            ] {
                set.add(signal);
            }
            let mut before = SigSet::empty();
            let held = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), Some(&mut before));
            Self {
                before: held.ok().map(|()| before),
            }
        }
    }

    impl Drop for HeldOff {
        fn drop(&mut self) {
            if let Some(before) = &self.before {
                let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(before), None);
            }
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
    mod tests {
        use super::*;

        /// A create undone before it was said to be made (the guard
        /// dropped armed): its record goes, and nothing is said of it.
        #[test]
        fn an_undone_create_is_never_said() {
            let dir = std::env::temp_dir().join(format!(
                "shipyard-guard-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let engine = Engine::new(
                Paths::from_env()
                    .with_containers(dir.join("containers"))
                    .with_image_store(dir.join("images")),
                std::sync::Arc::new(crate::report::Collect::default()),
            );
            let request = RunRequest {
                command: vec!["true".into()],
                ..RunRequest::default()
            };
            let record = containers::create_record(
                &engine,
                containers::NewContainer {
                    name: None,
                    config: Config::from_request(&request).unwrap(),
                    labels: Labels::default(),
                    image_id: None,
                    auto_remove: false,
                },
            )
            .unwrap();
            drop(RecordGuard {
                engine: &engine,
                id: Some(record.id.clone()),
            });
            assert!(containers::find(&engine, record.id.as_str()).is_err());
            let mut said = Vec::new();
            let everything = crate::events::Options {
                until: Some(SystemTime::now()),
                ..Default::default()
            };
            crate::events::read(&engine, &everything, |event| {
                said.push(event);
                Ok(())
            })
            .unwrap();
            assert!(said.is_empty(), "{said:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
