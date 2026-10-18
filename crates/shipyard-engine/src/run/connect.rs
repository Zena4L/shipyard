//! `network connect` and `disconnect` (stage 9, step 9.7b2): a container
//! joins a bridge network, or leaves one, while it runs or not.
//!
//! Under the container's operation lock (no start, stop or removal of it
//! meanwhile):
//!
//! - **Running (or paused):** at once. Connecting takes a lease on the
//!   network for it and makes its interface in its network namespace
//!   (`network::connect_live`), then gives the lease's lock to its monitor
//!   (`runtime::hold`, labelled as a start's lease is, `lease:NAME`): the
//!   monitor keeps it until the container ends, and its clean-up releases
//!   it then, as a start's. Disconnecting takes its lease away
//!   (`network::disconnect_live`), then has its monitor let go of its copy
//!   (`runtime::release`). Its default route, its published ports and its
//!   `/etc/hosts` and `resolv.conf` stay as its start made them (D9.7b-1):
//!   a network connected later is reached by names through the DNS server
//!   it has already (each answers its other networks' names too).
//! - **Created or exited:** its record only, for its next start.
//!
//! Either way its record keeps the change (`config.network`), so its next
//! start (a restart too) joins what it's on now. The record is written
//! first: a command killed after it leaves the record saying what the
//! container isn't yet, and the same command run again finishes it (F57: a
//! `connect` whose record names the network, its lease not there, joins;
//! a `disconnect` whose record no longer does, its lease still there,
//! leaves). Lock order: the operation lock, a network's (one at a time),
//! the container store's.

use std::net::{IpAddr, Ipv6Addr};

use crate::Engine;
use crate::error::Result;

/// What `network connect` asks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnectRequest {
    /// The network.
    pub network: String,
    /// The container: its name, ID, serial or ID prefix.
    pub container: String,
    /// Its address there (`--ip`); the next free one if not given.
    pub ip: Option<IpAddr>,
    /// Its IPv6 address there (`--ip6`).
    pub ip6: Option<Ipv6Addr>,
    /// Its other names there (`--alias`).
    pub aliases: Vec<String>,
}

impl ConnectRequest {
    /// A request to connect `container` to `network`, with no settings
    /// there.
    #[must_use]
    pub fn new(network: &str, container: &str) -> Self {
        Self {
            network: network.to_owned(),
            container: container.to_owned(),
            ..Self::default()
        }
    }
}

/// `shipyard network connect`: the container joins the network, at once if
/// it's running (or paused), at its next start otherwise; its record keeps
/// it.
///
/// # Errors
///
/// - [`ErrorKind::NeedsRoot`](crate::ErrorKind::NeedsRoot): bridge networks
///   are root's.
/// - [`ErrorKind::NotFound`](crate::ErrorKind::NotFound): no such
///   container or network.
/// - [`ErrorKind::Exists`](crate::ErrorKind::Exists): it's on the network
///   already.
/// - [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): the
///   network is `host`, `none` or `pasta`, the container's network isn't
///   bridges, an alias on a network without names, or too many networks.
/// - [`ErrorKind::Busy`](crate::ErrorKind::Busy): it's stopping or
///   restarting, or another command holds it.
/// - [`ErrorKind::Network`](crate::ErrorKind::Network): the network
///   couldn't be joined (its address taken, the monitor not holding the
///   lease: then nothing is left joined).
pub fn connect(engine: &Engine, request: &ConnectRequest) -> Result<()> {
    connect_with(engine, request, || {})
}

/// [`connect`], `between` run after its record is written and before the
/// container joins (tests cut it short there).
///
/// # Errors
///
/// As [`connect`].
#[doc(hidden)]
pub fn connect_with(
    engine: &Engine,
    request: &ConnectRequest,
    between: impl FnOnce(),
) -> Result<()> {
    #[cfg(target_os = "linux")]
    return linux::connect(engine, request, between);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, request, between);
        crate::error::bail!(Unsupported, "networks need Linux");
    }
}

/// `shipyard network disconnect`: the container leaves the network, at once
/// if it's running (or paused), at its next start otherwise; its record
/// keeps it. The network that carries its way out (its default route, its
/// published ports) is refused while it runs, unless `force`.
///
/// # Errors
///
/// As [`connect`]; [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if
/// it isn't on the network; [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest)
/// if it's its only network, or (without `force`) the one its way out goes
/// through.
pub fn disconnect(engine: &Engine, network: &str, container: &str, force: bool) -> Result<()> {
    disconnect_with(engine, network, container, force, || {})
}

/// [`disconnect`], `between` run after its record is written and before
/// the container leaves (tests cut it short there).
///
/// # Errors
///
/// As [`disconnect`].
#[doc(hidden)]
pub fn disconnect_with(
    engine: &Engine,
    network: &str,
    container: &str,
    force: bool,
    between: impl FnOnce(),
) -> Result<()> {
    #[cfg(target_os = "linux")]
    return linux::disconnect(engine, network, container, force, between);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, network, container, force, between);
        crate::error::bail!(Unsupported, "networks need Linux");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::AsFd as _;

    use shipyard_network::store::DEFAULT_NETWORK;
    use shipyard_runtime::container::{self as runtime, ContainerError};

    use super::ConnectRequest;
    use crate::Engine;
    use crate::containers::{self, Bridge, ContainerState, Record};
    use crate::error::{Error, ErrorKind, Result, bail};
    use crate::events::{self, Event, Kind};
    use crate::network::{self, Joined, Joining, RunId};
    use crate::report::WarningKind;
    use crate::run::held_off::HeldOff;
    use crate::run::holds::HoldKind;

    /// What's done with a container, by its state now.
    enum Now {
        /// At once, then its record.
        Live,
        /// Its record only.
        Later,
    }

    /// The container `reference` names, under its operation lock (held as
    /// long as the guard is), read again by its ID; and what's done with it
    /// now. Bridge networks are root's: refused otherwise.
    fn found(
        engine: &Engine,
        reference: &str,
    ) -> Result<(nix::fcntl::Flock<std::fs::File>, Record, Now)> {
        let _root = network::networks(engine)?;
        let containers::Locked {
            record,
            operation: lock,
        } = containers::lock_found(engine, reference, containers::OP_WAIT)?;
        let now = match containers::state(engine, &record)? {
            ContainerState::Running { .. } | ContainerState::Paused => Now::Live,
            ContainerState::Created | ContainerState::Exited { .. } => Now::Later,
            other => bail!(
                Busy,
                "container {} is {}: try again once it has",
                record.name,
                if matches!(other, ContainerState::Stopping) {
                    "stopping"
                } else {
                    "restarting"
                }
            ),
        };
        Ok((lock, record, now))
    }

    /// Whether running container `record` has a live lease on the network
    /// `name` (its monitor holds it: it's on it now).
    fn leased(engine: &Engine, record: &Record, name: &str) -> Result<bool> {
        Ok(network::container_leases(engine, record)?
            .iter()
            .any(|(network, _)| network == name))
    }

    /// Whether `asked` (a `connect`'s settings) asks for what `recorded`
    /// has: each setting given equal to the recorded one (none given, any).
    fn asks_the_same(recorded: &Bridge, asked: &Bridge) -> bool {
        asked.ip.is_none_or(|ip| recorded.ip == Some(ip))
            && asked.ip6.is_none_or(|ip| recorded.ip6 == Some(ip))
            && (asked.aliases.is_empty() || asked.aliases == recorded.aliases)
    }

    /// The label its monitor holds a network's lease by: a start's own
    /// (`lease:NAME`), so its clean-up releases one connected later alike.
    fn label(network: &str) -> String {
        HoldKind::Lease {
            network: network.to_owned(),
        }
        .to_arg()
    }

    pub(super) fn connect(
        engine: &Engine,
        request: &ConnectRequest,
        between: impl FnOnce(),
    ) -> Result<()> {
        let name = request.network.as_str();
        network::check_bridge(name)?;
        let (_lock, record, now) = found(engine, &request.container)?;
        let networks = network::networks(engine)?;
        let config = networks.get(name)?.ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("there's no network named {name}"),
            )
        })?;
        check_addresses(request, &config)?;
        let named = shipyard_network::store::has_dns(name);
        let aliases = checked_aliases(&request.aliases, named)?;
        let mut bridge = Bridge::named(name);
        bridge.ip = request.ip;
        bridge.ip6 = request.ip6;
        bridge.aliases.clone_from(&aliases);
        let live = matches!(now, Now::Live);
        // On it already: a `connect` cut short (killed after its record's
        // write) if it's up and has no lease there, asked again as it was
        // asked then (F57): finished, its record as it is.
        let (changed, settings) = match record.config.with_bridge(bridge.clone(), DEFAULT_NETWORK) {
            Ok(changed) => (Some(changed), bridge),
            Err(error)
                if error.kind() == ErrorKind::Exists && live && !leased(engine, &record, name)? =>
            {
                match record.config.bridge(name, DEFAULT_NETWORK) {
                    Some(recorded) if asks_the_same(&recorded, &bridge) => (None, recorded),
                    _ => return Err(error.context(format!("container {}", record.name))),
                }
            }
            Err(error) => return Err(error.context(format!("container {}", record.name))),
        };
        let finishing = changed.is_none();
        // From here a Ctrl-C waits: the record and the container agree at
        // the end. The record first: one that says a network the container
        // isn't on yet is what a failure leaves at worst (and is put back,
        // or the same `connect` finishes).
        let _held = HeldOff::new();
        let original = record.config.clone();
        let updated = match changed {
            Some(changed) => containers::update(engine, &record.id, |kept| kept.config = changed)?,
            None => record.clone(),
        };
        between();
        if live {
            // Its aliases there: those asked (or recorded), and its own for
            // every network with names (as a start gives them), each name
            // once.
            let mut aliases = settings.aliases;
            if named {
                aliases.extend(record.config.network_alias.iter().cloned());
            }
            let mut seen = std::collections::HashSet::new();
            aliases.retain(|alias| seen.insert(alias.to_ascii_lowercase()));
            let joined = Joined {
                name: config.name.clone(),
                ip: settings.ip,
                ip6: settings.ip6,
                mac: None,
                aliases,
            };
            if let Err(error) = join_now(engine, &record, &joined) {
                if !finishing {
                    put_back(engine, &record, original);
                }
                return Err(error);
            }
        }
        if finishing {
            engine.report(crate::report::Message::Notice(format!(
                "finished a `network connect` of {} to {name} that was cut short",
                record.name
            )));
        }
        events::emit(engine, &events::container(&updated, "update"));
        events::emit(
            engine,
            &Event::new(Kind::Network, "connect", name)
                .with("name", name)
                .with("container", updated.id.as_str()),
        );
        Ok(())
    }

    /// Refuses addresses the network can't give: `--ip` not IPv4 or not in
    /// its subnet, `--ip6` on a network without IPv6 or not in its prefix
    /// (checked now, not only at a start: a stopped container's would fail
    /// every start).
    fn check_addresses(
        request: &ConnectRequest,
        config: &shipyard_network::store::NetworkConfig,
    ) -> Result<()> {
        if let Some(ip) = request.ip
            && (!ip.is_ipv4() || !config.subnet.contains(ip))
        {
            bail!(
                InvalidRequest,
                "--ip {ip}: not an address in the network {}'s subnet ({})",
                config.name,
                config.subnet
            );
        }
        if let Some(ip6) = request.ip6 {
            match config.subnet6 {
                Some(subnet6) if subnet6.contains(ip6.into()) => {}
                Some(subnet6) => bail!(
                    InvalidRequest,
                    "--ip6 {ip6}: not an address in the network {}'s prefix ({subnet6})",
                    config.name
                ),
                None => bail!(
                    InvalidRequest,
                    "--ip6: the network {} has no IPv6 (made without --ipv6)",
                    config.name
                ),
            }
        }
        Ok(())
    }

    /// The aliases asked for, each a name (`--name`'s rule), on a network
    /// with names only.
    fn checked_aliases(aliases: &[String], named: bool) -> Result<Vec<String>> {
        if !aliases.is_empty() && !named {
            bail!(
                InvalidRequest,
                "--alias: the default network has no names: use a network made with `network \
                 create`"
            );
        }
        aliases
            .iter()
            .map(|alias| {
                shipyard_network::dns::parse_name(alias)
                    .map_err(|why| Error::new(ErrorKind::InvalidRequest, format!("--alias: {why}")))
            })
            .collect()
    }

    /// Puts container `record`'s networks back as they were (`original`):
    /// what was done to it live failed. Said if it can't be.
    fn put_back(engine: &Engine, record: &Record, original: crate::containers::Config) {
        if let Err(error) = containers::update(engine, &record.id, |kept| kept.config = original) {
            engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {}'s record says networks it isn't on: {error:#}",
                    record.name
                ),
            );
        }
    }

    /// Joins running container `record` to `joined` at once: its lease
    /// taken and its interface made, then the lease given to its monitor.
    /// (The caller holds a Ctrl-C off.)
    fn join_now(engine: &Engine, record: &Record, joined: &Joined) -> Result<()> {
        let runtime_root = engine.paths.runtime_root()?;
        let leases = network::container_leases(engine, record)?;
        let Some(run) = leases
            .iter()
            .find_map(|(_, lease)| RunId::from_lease(&lease.run))
        else {
            bail!(
                Network,
                "container {}'s networks can't be found: restart it to connect it",
                record.name
            );
        };
        let Some(n) = network::next_interface(&leases, crate::run::MAX_NETWORKS) else {
            bail!(
                InvalidRequest,
                "a container can join at most {} networks",
                crate::run::MAX_NETWORKS
            );
        };
        let interface = shipyard_network::store::interface_name(n);
        let sysctls: Vec<(String, String)> = record
            .config
            .sysctl
            .iter()
            .filter(|sysctl| {
                shipyard_network::attach::interface_of_sysctl(&sysctl.key)
                    == Some(interface.as_str())
            })
            .map(|sysctl| (sysctl.key.clone(), sysctl.value.clone()))
            .collect();
        let hostname = record
            .config
            .hostname
            .clone()
            .unwrap_or_else(|| record.id.short().to_owned());
        let netns = runtime::network_namespace_live(runtime_root, record.id.as_str())?;
        let joining = Joining {
            record,
            run: &run,
            hostname: &hostname,
            n,
            netns: netns.as_fd(),
            sysctls: &sysctls,
        };
        let connected = network::connect_live(engine, &joining, joined)?;
        let label = label(&joined.name);
        let Some(hold) = connected.hold() else {
            bail!(Network, "the network {}'s lease was let go of", joined.name);
        };
        if let Err(error) = runtime::hold(runtime_root, record.id.as_str(), &label, hold) {
            // Not answered: it may hold it. Let go of it, in case (the
            // lease goes with this process's copy, the attachment undone).
            // Never after a definite no: a hold of this label it has
            // already is another's to keep.
            if matches!(error, ContainerError::HoldUnanswered { .. }) {
                let _ = runtime::release(runtime_root, record.id.as_str(), &label);
            }
            let error = match error {
                ContainerError::NotHeld {
                    reason: shipyard_runtime::attach::HoldRefusal::Full,
                    ..
                } => Error::new(
                    ErrorKind::InvalidRequest,
                    format!(
                        "container {}'s monitor holds as much as it can (its networks, \
                             ports and volumes): restart it with this network to join it",
                        record.name
                    ),
                ),
                other => Error::from(other),
            };
            return Err(error.context(format!(
                "container {}'s monitor holding its lease on {}",
                record.name, joined.name
            )));
        }
        connected.given();
        Ok(())
    }

    pub(super) fn disconnect(
        engine: &Engine,
        name: &str,
        container: &str,
        force: bool,
        between: impl FnOnce(),
    ) -> Result<()> {
        network::check_bridge(name)?;
        let (_lock, record, now) = found(engine, container)?;
        let live = matches!(now, Now::Live);
        // Not on it: a `disconnect` cut short (killed after its record's
        // write) if it's up and still has a lease there (F57): finished.
        let changed = match record.config.without_bridge(name, DEFAULT_NETWORK) {
            Ok(changed) => Some(changed),
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    && live
                    && leased(engine, &record, name)? =>
            {
                None
            }
            Err(error) => return Err(error.context(format!("container {}", record.name))),
        };
        let finishing = changed.is_none();
        if live {
            check_way_out(engine, &record, name, force)?;
        }
        // As for a connect: a Ctrl-C waits; the record first (a record that
        // no longer says a network the container is still on is undone if
        // the rest fails, or the same `disconnect` finishes).
        let _held = HeldOff::new();
        let original = record.config.clone();
        let updated = match changed {
            Some(changed) => containers::update(engine, &record.id, |kept| kept.config = changed)?,
            None => record.clone(),
        };
        between();
        if live && let Err(error) = leave_now(engine, &record, name) {
            if !finishing {
                put_back(engine, &record, original);
            }
            return Err(error);
        }
        if finishing {
            engine.report(crate::report::Message::Notice(format!(
                "finished a `network disconnect` of {} from {name} that was cut short",
                record.name
            )));
        }
        events::emit(engine, &events::container(&updated, "update"));
        events::emit(
            engine,
            &Event::new(Kind::Network, "disconnect", name)
                .with("name", name)
                .with("container", updated.id.as_str()),
        );
        Ok(())
    }

    /// Refuses (unless `force`) to take running container `record` off the
    /// network `name` if its default route goes through it: read from its
    /// namespace (IPv4's, and IPv6's), as its start, and anything since,
    /// left it. With `force`, said.
    fn check_way_out(engine: &Engine, record: &Record, name: &str, force: bool) -> Result<()> {
        let runtime_root = engine.paths.runtime_root()?;
        let leases = network::container_leases(engine, record)?;
        let Some((_, lease)) = leases.iter().find(|(network, _)| network == name) else {
            return Ok(());
        };
        let netns = runtime::network_namespace_live(runtime_root, record.id.as_str())?;
        let (v4, v6) = network::way_out(netns.as_fd())?;
        let families: Vec<&str> = [(v4, "IPv4"), (v6, "IPv6")]
            .into_iter()
            .filter(|(through, _)| through.as_deref() == Some(lease.interface.as_str()))
            .map(|(_, family)| family)
            .collect();
        if families.is_empty() {
            return Ok(());
        }
        let routes = families.join(" and ");
        if !force {
            bail!(
                InvalidRequest,
                "{name} carries container {}'s way out (its {routes} default route, and any \
                 published ports): --force disconnects it anyway",
                record.name
            );
        }
        engine.warn(
            WarningKind::LeftForLater,
            format!(
                "container {} has no {routes} default route now (it was on {name})",
                record.name
            ),
        );
        Ok(())
    }

    /// Takes running container `record` off the network `name` at once: its
    /// lease away (its interface, its rules), then its monitor's copy let
    /// go of. (The caller holds a Ctrl-C off.)
    fn leave_now(engine: &Engine, record: &Record, name: &str) -> Result<()> {
        let runtime_root = engine.paths.runtime_root()?;
        if !network::disconnect_live(engine, record, name)? {
            // No live lease there (one its record said, never joined): its
            // monitor holds nothing of it.
            return Ok(());
        }
        match runtime::release(runtime_root, record.id.as_str(), &label(name)) {
            Ok(()) => {}
            // Its monitor holds it under no such label (an older one's), or
            // can't be asked: the lease is gone all the same; its copy of
            // an unnamed file goes with the monitor.
            Err(error) => engine.warn(
                WarningKind::LeftForLater,
                format!(
                    "container {}'s monitor keeps its old lease on {name} until it ends: {error}",
                    record.name
                ),
            ),
        }
        Ok(())
    }
}
