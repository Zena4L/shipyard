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
//! the commands that change it (`start`, `rm`, `stop`, `rename`, `update`,
//! `connect`, `cp`). Its `running` file is locked by a start and handed to
//! the monitor, then to `__cleanup`, which closes it last: free, everything
//! of the last start is let go, and a new start (or a removal) may begin.
//! The order is [`containers`](crate::containers)' (the operation lock, the
//! `running` file, networks' locks, the image store's, the container
//! store's). `__cleanup` takes neither of the
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
    _held: super::held_off::HeldOff,
}

#[cfg(target_os = "linux")]
impl Detached {
    /// Container `id`, named `name`, started, with the signals `held` holds
    /// off until the caller drops it.
    pub(super) fn started(id: ContainerId, name: Name, held: super::held_off::HeldOff) -> Self {
        Self {
            id,
            name,
            _held: held,
        }
    }
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
    return super::create::create(engine, request, auto_remove);
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
    return super::start::start(engine, reference, between);
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
    options: &crate::attach::AttachOptions,
) -> Result<crate::attach::Settled> {
    #[cfg(target_os = "linux")]
    {
        use crate::attach::{self, Settled};
        use crate::report::WarningKind;
        let root = engine.paths.runtime_root()?.to_owned();
        // `None`: it was up already, and never made here.
        let mut early = None;
        let record = start_with(engine, reference, |id| {
            early = Some(super::foreground::attach_before_start(&root, id, options));
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
    return super::remove::remove(engine, reference, force);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference, force);
        Err(crate::error::linux_only("containers"))
    }
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
    return super::start::detached(
        engine,
        request,
        auto_remove,
        super::start::Input::AsMade,
        between,
    )
    .map(|(detached, _)| detached);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, request, auto_remove, between);
        Err(crate::error::linux_only("containers"))
    }
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
pub struct Finished {
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
) -> Result<Finished> {
    #[cfg(target_os = "linux")]
    return super::foreground::attached(engine, request, auto_remove, options);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, request, auto_remove, options);
        Err(crate::error::linux_only("containers"))
    }
}
