//! Making a container: the request checked, the record written, the image
//! pinned ([`create_holding`]).

use shipyard_runtime::container as runtime;

use super::held_off::HeldOff;
use crate::Engine;
use crate::containers::{self, Config, ContainerId, Labels, Name, NewContainer, Record};
use crate::error::{ErrorKind, Result, bail};
use crate::image::PullPolicy;
use crate::network::NetworkMode;
use crate::report::WarningKind;
use crate::run::{Checked, RunRequest, check};

pub(super) fn create(engine: &Engine, request: RunRequest, auto_remove: bool) -> Result<Record> {
    create_holding(engine, request, auto_remove).map(|(record, _)| record)
}

/// [`create`], its record returned with the Ctrl-C hold that began as
/// it was made, still held.
pub(super) fn create_holding(
    engine: &Engine,
    request: RunRequest,
    auto_remove: bool,
) -> Result<(Record, HeldOff)> {
    let Prepared {
        planned,
        no_copy,
        anonymous,
        labels,
        image_id,
        name,
    } = prepare(engine, request, auto_remove)?;
    // From the record on, a Ctrl-C waits: it ends the call cleanly
    // (the record and its pin undone).
    let held_off = HeldOff::new();
    let config = Config::from_request(&planned.request)?;
    // Undone if what follows fails: the volumes made for it, its record.
    let mut guard = RecordGuard {
        engine,
        id: None,
        anonymous: Vec::new(),
        operation: None,
    };
    // Held until the record names them (a prune meanwhile skips them);
    // let go first if this fails, so the guard can remove them.
    let _anonymous_held = super::volumes::make_anonymous(engine, &anonymous, &mut guard.anonymous)?;
    // Only a made-up name is drawn: a given one is refused at the start if
    // it's taken.
    let avoid = if name.is_none() {
        names_on_its_networks(engine, planned.network())
    } else {
        Vec::new()
    };
    let record = containers::create_record_avoiding(
        engine,
        NewContainer {
            name,
            config,
            labels,
            // In the record from the start: even if what follows is
            // cut short, every start runs this image, never a tag
            // moved since.
            image_id: image_id.clone(),
            auto_remove,
            created: settled_beside(&planned.request, &anonymous)?,
        },
        &avoid,
    )?;
    guard.id = Some(record.id.clone());
    // Its own, until it's made whole or undone: no `start` or `rm` of it
    // meanwhile (it's there to be found).
    guard.operation = Some(containers::operation_lock(
        engine,
        &record.id,
        containers::OP_WAIT,
    )?);
    if let (Some(reference), Some(digest)) = (&planned.request.image, &image_id) {
        pin_and_fill(
            engine,
            &record,
            (reference.as_str(), digest.as_str()),
            &no_copy,
            &held_off,
        )?;
    }
    if held_off.asked_to_stop() {
        bail!(Interrupted, "interrupted before the container was made");
    }
    // Last: a file naming it only once nothing undoes it.
    if let Some(path) = &planned.request.cidfile {
        write_cidfile(path, &record.id)?;
    }
    guard.id = None;
    guard.anonymous.clear();
    drop(guard);
    crate::events::emit(engine, &crate::events::container(&record, "create"));
    Ok((record, held_off))
}

/// What [`prepare`] settles for a create before anything is made.
struct Prepared {
    /// The run, checked, with the image's health check and volumes.
    planned: crate::run::Planned,
    /// The volumes not to fill from the image (`nocopy`).
    no_copy: Vec<String>,
    /// The anonymous volumes to make: the request's and the image's.
    anonymous: Vec<String>,
    /// Its labels, checked.
    labels: Labels,
    /// Its image's ID, if it has an image.
    image_id: Option<String>,
    /// The name it's given, if one is.
    name: Option<Name>,
}

/// A create's checks and its image, before anything of it is made: the
/// request refused here settles nothing (but a bind's source it asked to
/// make), and an image pull that fails, or is interrupted, leaves no record
/// behind (Docker's order).
fn prepare(engine: &Engine, mut request: RunRequest, auto_remove: bool) -> Result<Prepared> {
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
    let (no_copy, mut anonymous) = settle_volumes(&mut request)?;
    // Another container's network or IPC namespace: that one found now,
    // and named by its ID from here on (a rename never re-points it).
    super::joined::name_the_joined(engine, &mut request)?;
    let mut planned = checked(engine, request)?;
    if let Some(path) = &planned.request.cidfile {
        check_cidfile(path)?;
    }
    // Its labels checked before anything is pulled.
    let labels = Labels::new(planned.request.labels.clone())?;
    // A request refused settles nothing (but a bind's source it asked
    // to make).
    crate::run::recover::recover_others(engine, None);
    // The image first: a pull that fails, or is interrupted, leaves no
    // record behind (Docker's order).
    let made_ready = ready_image(engine, &planned)?;
    let (image_id, image_config) = made_ready.unzip();
    // Again, with `-P`'s ports, now the image's are known.
    if let Some(config) = &image_config {
        check_holds(
            &planned,
            &containers::exposed_ports(&config.exposed_ports, &planned.request.expose),
        )?;
    }
    settle_the_health_check(engine, &mut planned.request, image_config.as_ref());
    // The image's own `VOLUME`s no mount covers: an anonymous one each
    // (Docker's, D11.4a2-1).
    if let Some(config) = &image_config {
        anonymous.extend(super::volumes::image_volumes(
            engine,
            &config.volumes,
            &mut planned.request.volume,
            &planned.request.tmpfs,
        )?);
    }
    let name = planned
        .request
        .name
        .as_deref()
        .map(Name::parse)
        .transpose()?;
    Ok(Prepared {
        planned,
        no_copy,
        anonymous,
        labels,
        image_id,
        name,
    })
}

/// F62: whether a container's starts give it the default process limit
/// ([`DEFAULT_PIDS_MAX`](crate::run::DEFAULT_PIDS_MAX)): it asked none
/// (`--pids-limit`, the API's `PidsLimit`; `-1` asks for none, and gets
/// none). Decided here, applied at each start where the `pids` controller
/// reaches its cgroup (D11.3b-2): never a start refused for a limit
/// nobody asked, wherever it's started from.
fn wants_default_pids(limits: &crate::run::Limits) -> bool {
    !limits.pids_given
        && limits
            .resources
            .as_ref()
            .is_ok_and(|resources| resources.pids_max.is_none())
}

/// Refuses an ID file (`--cidfile`) that's there already, before anything
/// is pulled or made (Docker's); [`write_cidfile`] refuses one made since.
///
/// # Errors
///
/// [`ErrorKind::Exists`].
fn check_cidfile(path: &std::path::Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        bail!(
            Exists,
            "the container ID file {} is there already: remove it, or give another",
            path.display()
        );
    }
    Ok(())
}

/// Writes `id` to `path` (`--cidfile`): a new file only, never one there
/// already nor through a symbolic link (`O_EXCL`), `0644` as Docker's. One
/// written in part is removed.
///
/// # Errors
///
/// [`ErrorKind::Exists`] if it's there (made since the check); an I/O
/// error writing it (the container is then undone by the caller).
fn write_cidfile(path: &std::path::Path, id: &containers::ContainerId) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .map_err(|error| {
            let kind = if error.kind() == std::io::ErrorKind::AlreadyExists {
                ErrorKind::Exists
            } else {
                ErrorKind::Io
            };
            crate::Error::from(error)
                .context(format!("writing the container ID file {}", path.display()))
                .io_as(kind)
        })?;
    file.write_all(id.as_str().as_bytes()).map_err(|error| {
        // Ours (made just now): never left half written.
        let _ = std::fs::remove_file(path);
        crate::Error::from(error)
            .context(format!("writing the container ID file {}", path.display()))
    })
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
    if asked.as_ref().is_some_and(|asked| !asked.is_disabled()) && settled.is_none() && !unrunnable
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

/// Refuses interface settings (`net.ipv4.conf.IF.*`) for an interface
/// the container's network doesn't make: one past its bridge networks'
/// `eth{n}`, or any with pasta's network or none of its own.
pub(super) fn check_interface_sysctls(
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

/// The bridge networks `mode` joins, by name (the default one too: `mode`
/// is the plan's, resolved as the start resolves it).
fn bridge_names(mode: &NetworkMode) -> Vec<&str> {
    match mode {
        NetworkMode::Bridges(joined) => joined.iter().map(|joined| joined.name.as_str()).collect(),
        _ => Vec::new(),
    }
}

/// The names live leases hold on the bridge networks the run's plan
/// (`plan`) joins: a made-up name avoids them (F63: a container of another
/// store on the same network may have drawn it). None off Linux, as anyone
/// but root (no bridge networks), or where a network can't be read: the
/// draw is then as before, and a start still refuses a name taken there.
fn names_on_its_networks(
    engine: &Engine,
    plan: Option<&crate::network::NetworkPlan>,
) -> Vec<String> {
    let names = plan
        .map(|plan| bridge_names(&plan.mode))
        .unwrap_or_default();
    if names.is_empty() {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(networks) = crate::network::networks(engine) else {
            return Vec::new();
        };
        let mut held = Vec::new();
        for network in names {
            for lease in networks.live_leases(network).unwrap_or_default() {
                held.extend(lease.names().map(str::to_owned));
            }
        }
        held
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = engine;
        Vec::new()
    }
}

/// The ports a container is asked to expose (`--expose`, the API's
/// `ExposedPorts`), each one port in one form (`80/tcp`: a bare `80` is
/// TCP's), each once, at most [`MAX_EXPOSED`](containers::MAX_EXPOSED).
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`] for a key that isn't one, or too many.
fn checked_exposed(asked: &[String]) -> Result<Vec<String>> {
    if asked.len() > containers::MAX_EXPOSED {
        bail!(
            InvalidRequest,
            "{} ports to expose: at most {} can be",
            asked.len(),
            containers::MAX_EXPOSED
        );
    }
    let mut keys = Vec::new();
    for key in asked {
        let Some(key) = shipyard_network::ports::exposed_key(key) else {
            bail!(
                InvalidRequest,
                "exposed port {key}: give a port, PORT/tcp or PORT/udp"
            );
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// Refuses, before anything is made, a container whose monitor would
/// hold more than it can: at most [`MOST_HOLDS`](runtime::MOST_HOLDS),
/// and fewer than this process's descriptor limit (they're all open here
/// first, then in the monitor before it raises its own), leaving room for
/// the rest. Its published ports' sockets count (two a port on every
/// address: IPv4 and IPv6), `-P`'s too (the image's `exposed` ports `-p`
/// doesn't publish: none known yet, none counted), with an upper bound of
/// its other holds (`start.rs`'s `collect_holds`): a lease per network, a
/// volume each, the run directory (two), the writable area and `running`.
pub(super) fn check_holds(planned: &crate::run::Planned, exposed: &[String]) -> Result<()> {
    /// Descriptors kept free for everything else a run holds and opens.
    const SPARE: u64 = 64;
    /// The run directory (its lock and itself), the writable area and
    /// `running`.
    const FIXED: usize = 4;
    let publish = &planned.request.publish;
    let mut ports: Vec<&shipyard_network::ports::PortRequest> = publish.iter().collect();
    let published_all: Vec<shipyard_network::ports::PortRequest> = if planned.request.publish_all {
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
    let (soft, hard) = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .unwrap_or((u64::MAX, u64::MAX));
    // Open here first (the soft limit), and handed on by the monitor to
    // its exit command at the end (the hard one: twice over).
    let room = usize::try_from(soft.saturating_sub(SPARE))
        .unwrap_or(usize::MAX)
        .min(runtime::most_holds_under(hard));
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

/// `request` checked, and refused if it's what a container can't be:
/// a bundle's, without isolation, without a root filesystem of its own,
/// with interface sysctls for an interface its networks don't make, or
/// publishing more ports than its monitor can hold (`-P`'s are checked
/// once its image is known).
pub(super) fn checked(engine: &Engine, request: RunRequest) -> Result<crate::run::Planned> {
    let mut planned = match check(engine, request)? {
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
    planned.request.expose = checked_exposed(&planned.request.expose)?;
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

/// `planned`'s image, made ready (pulled if its policy says so, its
/// layers given the container's root's IDs where root maps them): its
/// digest and config; none for a run without an image.
///
/// # Errors
///
/// As [`crate::image::make_ready`].
fn ready_image(
    engine: &Engine,
    planned: &crate::run::Planned,
) -> Result<Option<(String, shipyard_image::RuntimeConfig)>> {
    Ok(match &planned.request.image {
        Some(reference) => {
            let maps = planned
                .isolation
                .as_ref()
                .and_then(|isolation| isolation.id_maps.as_ref())
                .filter(|_| nix::unistd::geteuid().is_root());
            Some(crate::image::make_ready(
                engine,
                reference,
                (
                    planned.request.pull.unwrap_or(PullPolicy::Missing),
                    planned.request.platform.clone(),
                ),
                maps,
            )?)
        }
        None => None,
    })
}

/// A new container `record`'s image, pinned (`reference` at `digest`),
/// then its volumes still empty filled from it (Docker's copy-up, at the
/// create: D11.4a2b-1), but at `no_copy`'s places; a wait stopped when
/// `held_off` is asked to.
///
/// # Errors
///
/// The pin or a fill failed, or the create was interrupted.
fn pin_and_fill(
    engine: &Engine,
    record: &Record,
    (reference, digest): (&str, &str),
    no_copy: &[String],
    held_off: &HeldOff,
) -> Result<()> {
    crate::image::pin(engine, record.id.as_str(), reference, Some(digest))?;
    super::volumes::fill_volumes(
        engine,
        record,
        no_copy,
        super::volumes::Fill::Create,
        &|| held_off.asked_to_stop(),
    )
}

/// What `request`'s create settles beside its record (`created.json`),
/// with the anonymous volumes it made.
///
/// # Errors
///
/// As [`kept_sources`].
fn settled_beside(request: &RunRequest, anonymous: &[String]) -> Result<containers::Created> {
    Ok(containers::Created {
        exposed: request.expose.clone(),
        default_pids: wants_default_pids(&request.limits),
        tmpfs: request.tmpfs.clone(),
        anonymous: anonymous.to_vec(),
        make_sources: kept_sources(&request.make_sources)?,
        made_sources: request.made_sources.clone(),
        network_from: request
            .network_container()
            .map(containers::ContainerId::parse)
            .transpose()?,
        ipc_from: request
            .ipc
            .joined_container()
            .map(containers::ContainerId::parse)
            .transpose()?,
        pid_from: request
            .pid
            .joined_container()
            .map(containers::ContainerId::parse)
            .transpose()?,
        pid_host: request.pid == crate::run::PidMode::Host,
        stdin_once: request.streams.once,
    })
}

/// The bind sources to make at each start, as `created.json` keeps them
/// (UTF-8).
///
/// # Errors
///
/// One that isn't UTF-8 ([`ErrorKind::InvalidRequest`]).
fn kept_sources(sources: &[std::path::PathBuf]) -> Result<Vec<String>> {
    sources
        .iter()
        .map(|source| {
            source.to_str().map(str::to_owned).ok_or_else(|| {
                crate::Error::new(
                    ErrorKind::InvalidRequest,
                    format!("{}: a bind's source must be UTF-8", source.display()),
                )
            })
        })
        .collect()
}

/// What a create settles of `request`'s volumes before its checks:
/// `nocopy` taken off (the record never holds it; the fill leaves those
/// places alone: D11.4a2b-3),
/// its anonymous volumes named (made before its record), a bind's missing
/// source made when asked (D11.4a2b-4). The `nocopy` places, and the
/// anonymous volumes' names.
fn settle_volumes(request: &mut RunRequest) -> Result<(Vec<String>, Vec<String>)> {
    let no_copy = super::volumes::take_no_copy(&mut request.volume)?;
    let anonymous = super::volumes::name_anonymous(&mut request.volume)?;
    // Last, once nothing above can refuse: the checks resolve every
    // bind's source, so a missing one is made before them (and stays if a
    // later check refuses the request).
    for source in &request.make_sources {
        if let Some(made) = super::volumes::make_bind_source(source)? {
            request.made_sources.push(made);
        }
    }
    Ok((no_copy, anonymous))
}

/// Undoes a create that couldn't be made whole: its writable layer (made
/// for its fill), its record and its image's pin, unless disarmed (`id`
/// taken), then the anonymous volumes made for it, unless disarmed
/// (`anonymous` emptied).
struct RecordGuard<'a> {
    engine: &'a Engine,
    id: Option<ContainerId>,
    /// The anonymous volumes made for it.
    anonymous: Vec<String>,
    /// Its operation lock, held until the guard is done (its undoing
    /// too).
    operation: Option<nix::fcntl::Flock<std::fs::File>>,
}

impl Drop for RecordGuard<'_> {
    fn drop(&mut self) {
        // Never said to be made: removed unsaid, with the writable layer
        // its fill made.
        if let Some(id) = self.id.take()
            && let Err(error) = crate::image::remove_owned_area(self.engine, id.as_str())
                .and_then(|()| super::remove::unmake(self.engine, &id))
        {
            // Its record still names its volumes: `rm -v` takes both.
            self.engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {} couldn't be removed (`rm -v` removes it and its volumes): \
                     {error:#}",
                    id.short()
                ),
            );
            return;
        }
        // Then its anonymous volumes (no container names them now).
        super::volumes::remove_anonymous(self.engine, &std::mem::take(&mut self.anonymous));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    /// A made-up name avoids the names on the bridge networks the plan
    /// joins, the default one included (F63: a run with no `--network` is
    /// on `bridge`, and its plan says so); none for any other network.
    #[test]
    fn the_networks_a_plan_joins_are_named() {
        let joined = |name: &str| crate::network::Joined {
            name: name.to_owned(),
            ..crate::network::Joined::default()
        };
        assert_eq!(
            super::bridge_names(&NetworkMode::Bridges(vec![joined("bridge"), joined("app")])),
            ["bridge", "app"]
        );
        for other in [NetworkMode::Host, NetworkMode::None, NetworkMode::Pasta] {
            assert!(super::bridge_names(&other).is_empty(), "{other:?}");
        }
    }

    use std::time::SystemTime;

    use super::*;
    use crate::Paths;

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
        let record = containers::create_record_avoiding(
            &engine,
            containers::NewContainer {
                name: None,
                config: Config::from_request(&request).unwrap(),
                labels: Labels::default(),
                image_id: None,
                auto_remove: false,
                created: crate::containers::Created::default(),
            },
            &[],
        )
        .unwrap();
        drop(RecordGuard {
            engine: &engine,
            id: Some(record.id.clone()),
            anonymous: Vec::new(),
            operation: None,
        });
        assert!(containers::find(&engine, record.id.as_str()).is_err());
        let mut said = Vec::new();
        let everything = crate::events::EventsOptions {
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

    /// A create undone after its anonymous volumes were made: those go,
    /// once let go; another volume (not its own) stays.
    #[test]
    fn an_undone_create_removes_its_volumes() {
        let dir = std::env::temp_dir().join(format!(
            "shipyard-guard-volumes-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = Engine::new(
            Paths::from_env()
                .with_containers(dir.join("containers"))
                .with_image_store(dir.join("images"))
                .with_events(dir.join("events")),
            std::sync::Arc::new(crate::report::Collect::default()),
        );
        let own = crate::docker::new_id().unwrap();
        let other = crate::docker::new_id().unwrap();
        crate::volumes::create_anonymous(&engine, &other).unwrap();
        let held = crate::volumes::create_anonymous(&engine, &own).unwrap();
        drop(held);
        assert_eq!(
            crate::volumes::create_anonymous(&engine, &own)
                .unwrap_err()
                .kind(),
            crate::error::ErrorKind::Exists,
            "only a new one is its own"
        );
        let names = || -> Vec<String> {
            crate::volumes::open(&engine)
                .unwrap()
                .list()
                .unwrap()
                .into_iter()
                .map(|volume| volume.name)
                .collect()
        };
        assert!(names().contains(&own), "{:?}", names());
        drop(RecordGuard {
            engine: &engine,
            id: None,
            anonymous: vec![own.clone()],
            operation: None,
        });
        assert_eq!(names(), [other]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
