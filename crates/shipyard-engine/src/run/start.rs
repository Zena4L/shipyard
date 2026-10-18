//! Starting a container: its plan, its holds, its networks, its monitor
//! ([`start_taken`]), and `run -d` ([`detached`]).

use std::os::fd::AsFd as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::time::SystemTime;

use shipyard_runtime::container::{self as runtime, CreateOptions, ExitCommand, LogOptions};
use shipyard_runtime::container::{Plan, StdinMode};

use super::create::{check_holds, check_interface_sysctls, checked, create_holding};
use super::held_off::HeldOff;
use super::take::{Taken, Took, delete_state, take_or_leave};
use crate::containers::{self, BoundPort, ContainerId, Record};
use crate::error::{Error, ErrorKind, Result, bail};
use crate::network::RunNetwork;
use crate::report::{Message, WarningKind};
use crate::run::RunRequest;
use crate::run::holds::{HoldKind, Holds};
use crate::{Engine, Paths};

/// How a start takes its input (`-i`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Input {
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
    pub(super) fn stdin(self, interactive: bool) -> StdinMode {
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
        super::Detached::started(record.id, record.name, held_off),
        started,
    ))
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
pub(super) fn start_recovered(engine: &Engine, reference: &str) -> Result<Record> {
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
    match take_or_leave(engine, reference)? {
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
pub(super) fn start_taken(
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
    running: shipyard_sys::lock::HeldFile,
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
        mut running,
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
        runtime::create_planned(&runtime_root, id.as_str(), &plan, &options).map_err(Error::from)
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
            // Unlocked: no one else was given a copy (F66).
            drop(running);
            return Err(error);
        }
    };
    // Its monitor holds them now: this process's copies are closed,
    // never unlocked, and nothing is released. `running` too: given up
    // here, the one place its copy is handed over (pasta's launcher gets
    // another from this same descriptor, still open, below).
    drop(options);
    drop(fds);
    drop(holds);
    running.give_up();
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
        // An older start's file, left, would be restored for this one.
        containers::forget_previous_start(engine, id);
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
                check_interface_sysctls(std::slice::from_ref(setting), planned.network()).is_ok();
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
            let digest = crate::image::pin(engine, id.as_str(), reference, image_id.as_deref())?;
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
        engine.report(Message::Warning(warning));
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
            format!(
                "container {}'s record couldn't be put back as it was before this start: {why:#}",
                id.short()
            ),
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

/// What the monitor is to hold: the network's (leases, ports, the run
/// directory), the volumes', the writable area's, and the container's
/// `running` file.
fn collect_holds<'a>(
    network: &'a RunNetwork,
    volumes: &'a [shipyard_storage::VolumeHold],
    area: Option<&'a crate::image::RunArea>,
    running: &'a shipyard_sys::lock::HeldFile,
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
    holds.add(HoldKind::Running, running.hold());
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
    running: &shipyard_sys::lock::HeldFile,
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
                Some(running.hold()),
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
        text(paths.networks()?)?,
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
