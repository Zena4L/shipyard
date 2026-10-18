//! Making a container: the request checked, the record written, the image
//! pinned ([`create_holding`]).

use shipyard_runtime::container as runtime;

use super::held_off::HeldOff;
use crate::Engine;
use crate::containers::{self, Config, ContainerId, Labels, Name, NewContainer, Record};
use crate::error::{Result, bail};
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
    let config = Config::from_request(&planned.request)?;
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
            labels: Labels::default(),
            // In the record from the start: even if what follows is
            // cut short, every start runs this image, never a tag
            // moved since.
            image_id: image_id.clone(),
            auto_remove,
        },
        &avoid,
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

/// `request` checked, and refused if it's what a container can't be:
/// a bundle's, without isolation, without a root filesystem of its own,
/// with interface sysctls for an interface its networks don't make, or
/// publishing more ports than its monitor can hold (`-P`'s are checked
/// once its image is known).
pub(super) fn checked(engine: &Engine, request: RunRequest) -> Result<crate::run::Planned> {
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
            && let Err(error) = super::remove::unmake(self.engine, &id)
        {
            self.engine.warn(
                WarningKind::LeftForLater,
                format!("container {} couldn't be removed: {error:#}", id.short()),
            );
        }
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
            },
            &[],
        )
        .unwrap();
        drop(RecordGuard {
            engine: &engine,
            id: Some(record.id.clone()),
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
}
