//! Running a request, start to end: planned ([`check`]), its
//! image and network made ready, its command supervised until it ends, and
//! everything the run held let go, in order, before [`run`] returns.
//!
//! **Process-wide state.** A run installs the runtime's supervisor: signal
//! handlers (it passes on the user's signals) and, on Linux, the process's
//! subreaper (it reaps what the command leaves). So [`run`] is for a
//! process that runs containers one at a time, from a binary whose `main`
//! called the entry points first (`shipyard_runtime::process::
//! init_entry_point`, the storage helper's, the DNS server's, pasta's).

use shipyard_runtime::isolation::{Isolation, Namespace};
use shipyard_runtime::process::{self, ExitStatus, Signal};
use shipyard_runtime::security::Security;
use shipyard_storage::VolumeHold;

use super::{Checked, Generated, Invocation, RunRequest, check};
use crate::Engine;
use crate::error::{Error, Result, bail};
use crate::network::{HOST_INSTEAD, NetworkMode, RunNetwork};
use crate::report::Event;

/// How a run ended: what its caller ends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The command exited with this code (`128 + N` if signal N killed it).
    Exited(u8),
    /// Interrupted (Ctrl-C): the caller ends the same way, so that a shell
    /// loop around it stops too (`die_from(signal)`), or with `code` if it
    /// can't.
    Interrupted {
        /// The signal.
        signal: Signal,
        /// The code, if the caller can't die from it.
        code: u8,
    },
    /// The command couldn't be started or supervised: said as it happened
    /// ([`Event::Failure`]); this is the code to end with (125, 126, 127).
    Failed(u8),
}

/// Runs `request`: checks it, makes its image and network ready, supervises
/// its command, and lets go of what the run held (its network, its volumes,
/// its writable layer, in that order) before it returns. `passed` is what
/// the caller was given to pass on (`--preserve-fds`, socket activation),
/// taken first thing in its `main`, before anything was opened.
///
/// A failure of the command itself (not found, an isolation refused) is
/// reported when it happens and gives [`Outcome::Failed`].
///
/// It installs the runtime's supervisor (signal handlers; on Linux, the
/// process's subreaper): one run at a time in a process, from a binary
/// whose `main` called the entry points first (see the module's docs).
///
/// # Errors
///
/// What's wrong with the request, or what couldn't be made ready (the
/// image, the network, the volumes), before the command ran.
pub fn run(
    engine: &Engine,
    request: RunRequest,
    passed: process::passed::PassedFds,
) -> Result<Outcome> {
    let planned = match check(engine, request)? {
        Checked::Bundle(invocation) => return launch(engine, *invocation, None, passed),
        Checked::Command(planned) => planned,
    };
    // The image made ready: pulled if it must be, its layers extracted, and
    // the run's writable area made. The overlay is mounted by the runtime
    // inside the container's namespaces (not here). The area and the
    // volumes are the run's for its whole life: let go when it ends (or on
    // an error on the way, when they drop).
    #[cfg(target_os = "linux")]
    let (defaults, area) = super::prepare_image(engine, &planned)?.unzip();
    #[cfg(not(target_os = "linux"))]
    let (defaults, area) = (None, None);
    // A bridge network's address and the /etc files, made now: the
    // container is connected once its namespace exists.
    let exposed = defaults
        .as_ref()
        .map_or(&[][..], |defaults: &super::ImageDefaults| {
            &defaults.config.exposed_ports
        });
    let network = match RunNetwork::set_up(engine, planned.network(), exposed) {
        Ok(network) => network,
        Err(error) => {
            // The default bridge network couldn't be made (the kernel
            // refused), not one asked for; not a mistake in a request (a
            // port in use).
            if planned
                .network()
                .is_some_and(|plan| plan.defaulted && matches!(plan.mode, NetworkMode::Bridges(_)))
                && network_refused(&error)
            {
                engine.report(Event::Hint(HOST_INSTEAD.to_owned()));
            }
            return Err(error);
        }
    };
    let (invocation, volumes) = planned.finish(engine, defaults, network.etc())?;
    let held = Held {
        network,
        volumes,
        area,
    };
    held.network.release_ports();
    let mut connect = held.network.hook();
    let hook = connect
        .as_mut()
        .map(|connect| connect as process::NetworkHook<'_>);
    let outcome = launch(engine, invocation, hook, passed);
    drop(connect);
    held.end();
    outcome
}

/// What a run holds while its command runs, let go when it ends. Its
/// fields drop in order (on an error on the way); [`end`](Self::end) lets
/// go of them in the same order, its writable layer removed as it goes.
struct Held {
    /// Its network: its address given back and its veth gone first.
    network: RunNetwork,
    /// Its named volumes' holds: nothing removes one while it's held.
    volumes: Vec<VolumeHold>,
    /// Its writable layer (an image's run), removed last.
    #[cfg(target_os = "linux")]
    area: Option<crate::image::RunArea>,
    #[cfg(not(target_os = "linux"))]
    area: Option<std::convert::Infallible>,
}

impl Held {
    /// Lets go of everything, in order: the network, the volumes, then the
    /// writable layer (removed now, while the caller can still say why if
    /// it can't be).
    fn end(self) {
        let Self {
            network,
            volumes,
            area,
        } = self;
        drop(network);
        drop(volumes);
        #[cfg(target_os = "linux")]
        if let Some(area) = area {
            area.remove();
        }
        #[cfg(not(target_os = "linux"))]
        let _ = area;
    }
}

/// Runs `invocation` under the supervisor, with `network` setting up the
/// container's network before it starts (connecting it to a bridge
/// network, or starting its pasta).
fn launch(
    engine: &Engine,
    invocation: Invocation,
    network: Option<process::NetworkHook<'_>>,
    passed: process::passed::PassedFds,
) -> Result<Outcome> {
    let Invocation {
        spec,
        stop,
        isolation,
        stdio,
        warnings,
        generated,
    } = invocation;
    for warning in warnings {
        engine.report(Event::Warning(warning));
    }
    tracing::debug!(?spec, ?stop, ?isolation, "running");
    let mut supervisor = process::Supervisor::install()?;
    supervisor.set_stdio(stdio);
    supervisor.pass(passed);
    let supervised = match (network, isolation.as_ref()) {
        (Some(network), Some(isolation)) => {
            supervisor.supervise_with_network(&spec, stop, isolation, network)
        }
        (Some(_), None) => bail!(Runtime, "a network needs an isolated run"),
        (None, isolation) => supervisor.supervise(&spec, stop, isolation),
    };
    let status = match supervised {
        Ok(status) => status,
        Err(err) => {
            let code = err.exit_code();
            let isolation_failed = is_isolation_failure(&err);
            engine.report(Event::Failure(format!("{:#}", Error::from(err))));
            // The runtime can't name the request's setting; this is where
            // it's known. Only when it would work: no isolation can't be
            // combined with a root filesystem, a hostname, a domain name or
            // sysctls.
            if isolation_failed
                && isolation
                    .as_ref()
                    .is_some_and(|isolation| only_namespaces(isolation, generated))
            {
                engine.report(Event::Hint(
                    "--no-isolation runs the command without namespaces".to_owned(),
                ));
            }
            return Ok(Outcome::Failed(code));
        }
    };
    let code = status.code();
    if let ExitStatus::Signaled { signal, .. } = status
        && signal == Signal::INT
    {
        return Ok(Outcome::Interrupted { signal, code });
    }
    Ok(Outcome::Exited(code))
}

/// Whether `isolation` asks for nothing but the namespaces themselves, so
/// that no isolation could replace it: no limits or devices either, which
/// need the container's cgroup. What Shipyard made up (`generated`: the
/// run's ID as hostname, the default network and its settings) isn't asked
/// for.
fn only_namespaces(isolation: &Isolation, generated: Generated) -> bool {
    (!isolation.has(Namespace::Network) || generated.network)
        && isolation.root.is_none()
        && (isolation.hostname.is_none() || generated.hostname)
        && isolation.domainname.is_none()
        && (isolation.sysctls.is_empty() || (generated.network && generated.sysctls))
        && isolation.security == Security::standard()
        && isolation.id_maps.is_none()
        && isolation.resources.is_empty()
        && !isolation.devices.asked
}

/// Whether `err` is isolation going wrong (missing privileges, a refusal,
/// or a failed namespace step), which no isolation avoids.
fn is_isolation_failure(err: &shipyard_runtime::Error) -> bool {
    use shipyard_runtime::Error;
    match err {
        Error::IsolationNeedsRoot
        | Error::IsolationRefused
        | Error::UserNamespacesRefused { .. }
        | Error::InitDidNotStart { .. } => true,
        Error::Spawn(spawn) => spawn.kind() == process::SpawnErrorKind::Isolation,
        _ => false,
    }
}

/// Whether `error` is the kernel refusing to set up a network (no right to,
/// no `nf_tables`), rather than something the request can fix.
fn network_refused(error: &Error) -> bool {
    // Asked by type (`Error::find`): not by kind, which a file of the run's
    // could have too.
    error.find::<shipyard_network::NetlinkError>().is_some() || {
        #[cfg(target_os = "linux")]
        let attach = error
            .find::<shipyard_network::attach::AttachError>()
            .is_some()
            || error
                .find::<shipyard_network::route::RtnetlinkError>()
                .is_some();
        #[cfg(not(target_os = "linux"))]
        let attach = false;
        attach
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The kernel refusing a network is told from the engine's error by the
    /// netlink error inside it, however deep (the hint `--network host` is
    /// given for it alone); a network that's merely in use isn't.
    #[test]
    fn a_network_the_kernel_refused_is_found_inside_the_engines_error() {
        let refused = Error::from(shipyard_network::NetlinkError::Timeout {
            operation: "making the bridge".to_owned(),
        })
        .context("the network bridge")
        .context("joining it");
        assert!(network_refused(&refused));
        let in_use = Error::from(shipyard_network::store::StoreError::InUse {
            name: "app".to_owned(),
            users: vec!["web".into()],
        })
        .context("the network app");
        assert!(!network_refused(&in_use));
    }

    /// `--no-isolation` is suggested only where it could replace what was
    /// asked: never for limits or device flags, which it can't be combined
    /// with.
    #[test]
    fn no_isolation_replaces_only_bare_namespaces() {
        assert!(only_namespaces(
            &Isolation::standard(),
            Generated::default()
        ));
        // A hostname that's the run's ID isn't asked for: the hint still fits.
        let hostname = Generated {
            hostname: true,
            ..Generated::default()
        };
        let mut named = Isolation::standard();
        named.hostname = Some("a1b2c3d4e5f6".into());
        assert!(only_namespaces(&named, hostname));
        assert!(!only_namespaces(&named, Generated::default()));
        let mut limited = Isolation::standard();
        limited.resources.pids_max = Some(1);
        assert!(!only_namespaces(&limited, Generated::default()));
        let mut devices = Isolation::standard();
        devices.devices.asked = true;
        assert!(!only_namespaces(&devices, Generated::default()));
        // --no-isolation would give it the host's network instead.
        let mut network = Isolation::standard();
        network.namespaces.push(Namespace::Network);
        assert!(!only_namespaces(&network, Generated::default()));
        // Unless it's the default one, not asked for, with its own default
        // settings; a setting of the user's is asked for.
        let default_network = Generated {
            network: true,
            sysctls: true,
            ..Generated::default()
        };
        assert!(only_namespaces(&network, default_network));
        network.sysctls = vec![("net.ipv4.ping_group_range".into(), "0 0".into())];
        assert!(only_namespaces(&network, default_network));
        // A setting of the user's (even one the default network has).
        let asked = Generated {
            sysctls: false,
            ..default_network
        };
        assert!(!only_namespaces(&network, asked));
    }
}
