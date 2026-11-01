//! Another container's namespaces, joined: its network (`--network
//! container:NAME`, step 11.4b2b1), its IPC namespace with its `/dev/shm`
//! (`--ipc container:NAME`, 11.4b2b2) and its PID namespace (`--pid
//! container:NAME`, 15.2b2, F134). A create finds the other container and
//! keeps its ID beside the record (`created.json`'s `network_from`,
//! `ipc_from`, `pid_from`); each start finds its init by PID and start
//! time ([`resolve_joins`]), and, for a network, what the other's `/etc`
//! files are made of (D11.4b2b-2). Nothing of this container's is made in
//! those namespaces: no address, no port, no name, no setting.

use super::RunRequest;
use crate::Engine;
use crate::containers::{self, Record};
use crate::error::{Result, bail};
#[cfg(target_os = "linux")]
use crate::network::NetworkMode;

/// The container `reference` names, for another to join its network: there,
/// and not joining another's itself (Docker's join of a join would be the
/// third's, a link this one wouldn't see change).
///
/// # Errors
///
/// No such container ([`NotFound`](crate::ErrorKind::NotFound)), or one
/// that joins another's network ([`InvalidRequest`](crate::ErrorKind::InvalidRequest)).
pub(crate) fn network_target(engine: &Engine, reference: &str) -> Result<Record> {
    target(engine, reference, Joins::Network)
}

/// The container `reference` names, for another to join its IPC namespace,
/// as [`network_target`].
///
/// # Errors
///
/// As [`network_target`]'s, for its IPC namespace.
pub(crate) fn ipc_target(engine: &Engine, reference: &str) -> Result<Record> {
    target(engine, reference, Joins::Ipc)
}

/// The container `reference` names, for another to join its PID namespace,
/// as [`network_target`].
///
/// # Errors
///
/// As [`network_target`]'s, for its PID namespace.
pub(crate) fn pid_target(engine: &Engine, reference: &str) -> Result<Record> {
    target(engine, reference, Joins::Pid)
}

/// What's joined of another container.
#[derive(Debug, Clone, Copy)]
enum Joins {
    Network,
    Ipc,
    Pid,
}

impl Joins {
    /// Its flag, for messages.
    fn flag(self) -> &'static str {
        match self {
            Joins::Network => "--network",
            Joins::Ipc => "--ipc",
            Joins::Pid => "--pid",
        }
    }

    /// Its word, for messages.
    fn word(self) -> &'static str {
        match self {
            Joins::Network => "network",
            Joins::Ipc => "IPC namespace",
            Joins::Pid => "PID namespace",
        }
    }
}

/// The container `reference` names, for another to join its `what`: there,
/// and not joining another's itself. A PID namespace, also not the host's,
/// and not one where a process may hold `CAP_SYS_PTRACE` (`--cap-add`):
/// its container's, or another's that joined it (a debugger's): with it,
/// they could trace the joining container's set-up (D15.2b-10). (The
/// runtime looks at every live process there too; this is what bounds them
/// all, `exec`s' included.)
fn target(engine: &Engine, reference: &str, what: Joins) -> Result<Record> {
    let record = containers::find(engine, reference)?;
    let created = containers::created(engine, &record.id)?;
    let joins_another = match what {
        Joins::Network => created.network_from.is_some(),
        Joins::Ipc => created.ipc_from.is_some(),
        Joins::Pid => created.pid_from.is_some(),
    };
    if joins_another {
        bail!(
            InvalidRequest,
            "{} container:{reference}: {} joins another container's {} itself: join that one",
            what.flag(),
            record.name,
            what.word()
        );
    }
    if let Joins::Pid = what {
        if created.pid_host {
            bail!(
                InvalidRequest,
                "--pid container:{reference}: {} shares the host's PID namespace: give --pid \
                 host",
                record.name
            );
        }
        if let Some(tracer) = tracer_in(engine, &record) {
            bail!(
                InvalidRequest,
                "--pid container:{reference}: {tracer} may hold CAP_SYS_PTRACE (--cap-add) in \
                 that PID namespace, with which it could trace this container as it's set up: \
                 join it once {tracer} has ended, or without that capability there"
            );
        }
    }
    Ok(record)
}

/// The container that may hold `CAP_SYS_PTRACE` in `record`'s PID
/// namespace, if any: `record`'s, or one up that joined it.
fn tracer_in(engine: &Engine, record: &Record) -> Option<String> {
    if may_trace(record) {
        return Some(record.name.as_str().to_owned());
    }
    containers::list(engine)
        .unwrap_or_default()
        .into_iter()
        .find(|other| {
            may_trace(other)
                && containers::created(engine, &other.id)
                    .is_ok_and(|created| created.pid_from.as_ref() == Some(&record.id))
                && containers::state(engine, other).is_ok_and(|state| state.is_active())
        })
        .map(|other| other.name.as_str().to_owned())
}

/// Whether `record`'s container may give its processes `CAP_SYS_PTRACE`:
/// `--cap-add ALL` or `SYS_PTRACE`, unless `--cap-drop` takes it (or all)
/// away again. (A name a record can't hold is none.)
fn may_trace(record: &Record) -> bool {
    use shipyard_runtime::security::Capability;
    let names = |names: &[String]| {
        names
            .iter()
            .any(|name| match name.parse::<super::CapabilityArg>() {
                Ok(super::CapabilityArg::All) => true,
                Ok(super::CapabilityArg::One(one)) => one == Capability::SYS_PTRACE,
                Err(_) => false,
            })
    };
    names(&record.config.cap_add) && !names(&record.config.cap_drop)
}

impl RunRequest {
    /// Asks for the namespaces `created` says it joins: other containers'
    /// (by ID), the host's PID namespace.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "containers are started on Linux")
    )]
    pub(crate) fn join_as_created(&mut self, created: &containers::Created) {
        self.join_by_ids(
            created.network_from.as_ref(),
            created.ipc_from.as_ref(),
            created.pid_from.as_ref(),
        );
        if created.pid_host {
            self.pid = super::PidMode::Host;
        }
    }

    /// Asks for the network, IPC and PID namespaces of the containers
    /// `network`, `ipc` and `pid` (by ID), where given.
    pub(crate) fn join_by_ids(
        &mut self,
        network: Option<&containers::ContainerId>,
        ipc: Option<&containers::ContainerId>,
        pid: Option<&containers::ContainerId>,
    ) {
        if let Some(network) = network {
            self.network = vec![crate::network::NetworkMode::Container(
                network.as_str().to_owned(),
            )];
        }
        if let Some(ipc) = ipc {
            self.ipc = super::IpcMode::Container(ipc.as_str().to_owned());
        }
        if let Some(pid) = pid {
            self.pid = super::PidMode::Container(pid.as_str().to_owned());
        }
    }
}

/// At a create: the containers `request` joins the namespaces of, found,
/// and named by their IDs from here on (a rename never re-points it).
/// Rootless, both must be one container's: a container joins one user
/// namespace, the one that owns what it joins.
///
/// # Errors
///
/// As [`network_target`] and [`ipc_target`]; rootless, a network and an IPC
/// namespace of two containers, and another's IPC namespace with
/// `--uidmap` ([`InvalidRequest`](crate::ErrorKind::InvalidRequest)).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "containers are made on Linux")
)]
pub(crate) fn name_the_joined(engine: &Engine, request: &mut RunRequest) -> Result<()> {
    let network = match request.network_container().map(str::to_owned) {
        Some(reference) => Some(network_target(engine, &reference)?.id),
        None => None,
    };
    let ipc = match request.ipc.joined_container().map(str::to_owned) {
        Some(reference) => {
            let other = ipc_target(engine, &reference)?;
            // Its `/dev/shm` belongs to its own user namespace, which this
            // container, made by root without one, has no IDs in.
            if !other.config.uidmap.is_empty() && crate::network::host_root() {
                bail!(
                    InvalidRequest,
                    "--ipc container:{reference}: {} has a user namespace of its own \
                     (--uidmap), whose shared memory no other container's IDs reach",
                    other.name
                );
            }
            Some(other.id)
        }
        None => None,
    };
    let pid = match request.pid.joined_container().map(str::to_owned) {
        Some(reference) => {
            if !request.uidmap.is_empty() {
                bail!(
                    InvalidRequest,
                    "--uidmap can't go with --pid container:{reference}: only root of the user \
                     namespace that owns that PID namespace may mount its /proc"
                );
            }
            Some(pid_target(engine, &reference)?.id)
        }
        None => None,
    };
    let named: Vec<&containers::ContainerId> =
        [&network, &ipc, &pid].into_iter().flatten().collect();
    if named.windows(2).any(|pair| pair[0] != pair[1]) && !crate::network::host_root() {
        bail!(
            InvalidRequest,
            "--network, --ipc and --pid container: name more than one container: without root, \
             a container joins one other's namespaces (its user namespace owns them)"
        );
    }
    check_host_pid(engine, request)?;
    // Its `/dev/shm` is cloned in the other's mount namespace, where a user
    // namespace of this one's own has no privileges.
    if let Some(reference) = request.ipc.joined_container()
        && ipc.is_some()
        && !request.uidmap.is_empty()
    {
        bail!(
            InvalidRequest,
            "--uidmap can't go with --ipc container:{reference}: that container's shared memory \
             is in its mount namespace, where this one's user namespace has no privileges"
        );
    }
    request.join_by_ids(network.as_ref(), ipc.as_ref(), pid.as_ref());
    Ok(())
}

/// `--pid host`, at a create: root's alone (rootless, no `/proc` of the
/// host's PID namespace can be mounted in a user namespace), never with
/// `--uidmap` (the same), and said: the container sees and can signal the
/// host's processes.
fn check_host_pid(engine: &Engine, request: &RunRequest) -> Result<()> {
    if request.pid != super::PidMode::Host {
        return Ok(());
    }
    if !crate::network::host_root() {
        bail!(
            InvalidRequest,
            "--pid host needs root: a container's /proc for the host's PID namespace can't be \
             mounted in a user namespace"
        );
    }
    if !request.uidmap.is_empty() {
        bail!(
            InvalidRequest,
            "--uidmap can't go with --pid host: a user namespace of the container's own can't \
             mount the host's PID namespace's /proc"
        );
    }
    engine.warn(
        crate::report::WarningKind::HostPids,
        "--pid host: the container sees and can signal every process of the host",
    );
    Ok(())
}

impl RunRequest {
    /// Gives `isolation` its PID namespace as asked: its own (as made), the
    /// host's (none of its own: the runtime then needs its cgroup), or
    /// another container's, whose init is found for each start.
    pub(super) fn add_pid(
        &self,
        isolation: &mut shipyard_runtime::isolation::Isolation,
    ) -> Result<()> {
        use shipyard_runtime::isolation::Namespace;
        match &self.pid {
            super::PidMode::Private => {}
            super::PidMode::Host => isolation.namespaces.retain(|&kind| kind != Namespace::Pid),
            super::PidMode::Container(_) => match &self.joined.pid {
                Some(process) => {
                    super::network::join_namespace(isolation, Namespace::Pid, process);
                }
                // A start always finds it first.
                None if self.container.is_some() => bail!(
                    Runtime,
                    "the container whose PID namespace it joins wasn't found for this start"
                ),
                // A create's check: nothing is started.
                None => {}
            },
        }
        Ok(())
    }

    /// Gives `isolation` another container's IPC namespace, if it joins one
    /// (its `/dev/shm` comes with it: the runtime's). Its settings are that
    /// one's: IPC `--sysctl` keys are refused.
    pub(super) fn add_ipc(
        &self,
        isolation: &mut shipyard_runtime::isolation::Isolation,
    ) -> Result<()> {
        use shipyard_runtime::isolation::Namespace;
        let Some(container) = self.ipc.joined_container() else {
            return Ok(());
        };
        if !self.has_root() {
            bail!(
                InvalidRequest,
                "--ipc container:{container} needs a root filesystem of its own (an image or \
                 --rootfs): that container's /dev/shm is mounted there"
            );
        }
        if let Some((key, _)) = self
            .sysctl
            .iter()
            .find(|(key, _)| shipyard_runtime::isolation::is_ipc_sysctl(key))
        {
            bail!(
                InvalidRequest,
                "--sysctl {key} can't be given with --ipc container:{container}: the IPC \
                 namespace and its settings are that container's"
            );
        }
        match &self.joined.ipc {
            Some(process) => super::network::join_namespace(isolation, Namespace::Ipc, process),
            // A start always finds it first.
            None if self.container.is_some() => bail!(
                Runtime,
                "the container whose IPC namespace it joins wasn't found for this start"
            ),
            // A create's check: nothing is started.
            None => {}
        }
        Ok(())
    }
}

/// The other containers' namespaces `request` joins (a network, an IPC
/// namespace, a PID namespace), found for this start: their inits (running
/// or paused), and what a network's `/etc` files are made of. Nothing for a
/// request that joins none.
///
/// # Errors
///
/// As [`network_target`], [`ipc_target`] and [`pid_target`]; another
/// container not running ([`NotRunning`](crate::ErrorKind::NotRunning),
/// Docker's "cannot join network (IPC, PID) of a non running container");
/// its maps unreadable.
pub(crate) fn resolve_joins(engine: &Engine, request: &mut RunRequest) -> Result<()> {
    if let Some(reference) = request.network_container().map(str::to_owned) {
        let other = network_target(engine, &reference)?;
        request.joined.network = Some(joined(engine, &other)?);
    }
    if let Some(reference) = request.ipc.joined_container().map(str::to_owned) {
        // (Found again either way: a join of a join is refused.)
        let other = ipc_target(engine, &reference)?;
        request.joined.ipc = Some(match found_already(request, &reference) {
            Some(init) => init,
            None => init_of(engine, &other, Joins::Ipc)?,
        });
    }
    if let Some(reference) = request.pid.joined_container().map(str::to_owned) {
        let other = pid_target(engine, &reference)?;
        request.joined.pid = Some(match found_already(request, &reference) {
            Some(init) => init,
            None => init_of(engine, &other, Joins::Pid)?,
        });
    }
    Ok(())
}

/// The init of `reference`'s container, if this start found it already
/// for another of its namespaces: read once (it could restart between two
/// reads).
fn found_already(request: &RunRequest, reference: &str) -> Option<crate::network::JoinedProcess> {
    [
        (
            request.network_container(),
            request.joined.network.as_ref().map(|net| &net.init),
        ),
        (request.ipc.joined_container(), request.joined.ipc.as_ref()),
    ]
    .into_iter()
    .find_map(|(named, init)| init.filter(|_| named == Some(reference)))
    .cloned()
}

/// `other`'s init, for a start to join its `what`: running or paused, with
/// its user namespace's maps where that's joined too.
#[cfg(target_os = "linux")]
fn init_of(engine: &Engine, other: &Record, what: Joins) -> Result<crate::network::JoinedProcess> {
    use shipyard_runtime::container as runtime;
    // Docker's words.
    let not_running = || {
        crate::Error::new(
            crate::ErrorKind::NotRunning,
            match what {
                Joins::Network => format!(
                    "cannot join network of a non running container: {}",
                    other.name
                ),
                Joins::Ipc => format!("cannot join IPC of a non running container: {}", other.name),
                Joins::Pid => format!("cannot join PID of a non running container: {}", other.name),
            },
        )
    };
    let (pid, start_time) =
        match runtime::running_init(engine.paths().runtime_root()?, other.id.as_str()) {
            Ok(init) => init,
            Err(
                runtime::ContainerError::NotFound(_) | runtime::ContainerError::WrongStatus { .. },
            ) => {
                return Err(not_running());
            }
            Err(error) => return Err(error.into()),
        };
    // Its user namespace, with its maps from its init: joined with its
    // namespaces where this one isn't the host's root (which enters any) and
    // it isn't this process's own (root in a container: `setns` refuses
    // the one it's in).
    let maps = if crate::network::host_root() || !own_user_namespace_differs(pid)? {
        None
    } else {
        Some(maps_of(pid, &not_running)?)
    };
    Ok(crate::network::JoinedProcess {
        pid,
        start_time,
        maps,
    })
}

/// Off Linux: nothing to join (containers run in the VM).
#[cfg(not(target_os = "linux"))]
fn init_of(engine: &Engine, other: &Record, what: Joins) -> Result<crate::network::JoinedProcess> {
    let _ = (engine, other, what);
    Err(crate::error::linux_only(
        "joining another container's namespaces",
    ))
}

/// What a start joins of `other`'s network.
#[cfg(target_os = "linux")]
fn joined(engine: &Engine, other: &Record) -> Result<crate::network::JoinedNetwork> {
    let init = init_of(engine, other, Joins::Network)?;
    let its = other.to_request()?;
    // Its mode as asked, or the default as a view says it (never the
    // run's own default's warnings, at each of this one's starts).
    let mode = if its.network.is_empty() {
        match crate::network::defaulted() {
            crate::network::Defaulted::Bridge => {
                NetworkMode::bridge(shipyard_network::store::DEFAULT_NETWORK)
            }
            crate::network::Defaulted::Pasta => NetworkMode::Pasta,
            crate::network::Defaulted::Host => NetworkMode::Host,
        }
    } else {
        its.network_mode(engine)?
    };
    let (addresses, resolver) = match &mode {
        NetworkMode::Bridges(networks) => bridge_addresses(engine, other, networks)?,
        NetworkMode::Pasta => (
            Vec::new(),
            Some(std::net::IpAddr::V4(shipyard_network::pasta::DNS_ADDRESS)),
        ),
        _ => (Vec::new(), None),
    };
    Ok(crate::network::JoinedNetwork {
        init,
        // As its starts name it (`planned_start`).
        hostname: its
            .hostname
            .clone()
            .unwrap_or_else(|| other.id.short().to_owned()),
        addresses,
        resolver,
        hosts: its.add_host.clone(),
        dns: shipyard_network::files::DnsSettings {
            servers: its.dns.clone(),
            search: its.dns_search.clone(),
            options: its.dns_option.clone(),
        },
        own: mode.own_namespace(),
        pasta: mode == NetworkMode::Pasta,
    })
}

/// Whether process `pid`'s user namespace isn't this process's own.
#[cfg(target_os = "linux")]
fn own_user_namespace_differs(pid: u32) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let of = |path: &str| {
        std::fs::metadata(path)
            .map(|meta| (meta.dev(), meta.ino()))
            .map_err(|error| crate::Error::from(error).context(format!("reading {path}")))
    };
    Ok(of(&format!("/proc/{pid}/ns/user"))? != of("/proc/self/ns/user")?)
}

/// The ID maps of process `pid`'s user namespace, from its `/proc`: gone
/// meanwhile is `gone()`; another failure is said as it is.
#[cfg(target_os = "linux")]
fn maps_of(
    pid: u32,
    gone: &dyn Fn() -> crate::Error,
) -> Result<shipyard_runtime::isolation::IdMaps> {
    /// The most of a map file read (the kernel takes at most 340 lines).
    const MAPS_MAX: u64 = 64 * 1024;
    let read = |name: &str| -> Result<String> {
        let path = std::path::PathBuf::from(format!("/proc/{pid}/{name}"));
        let bytes = match shipyard_sys::fs::read_bounded(&path, MAPS_MAX) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(gone()),
            Err(error) => {
                return Err(
                    crate::Error::from(error).context(format!("reading {}", path.display()))
                );
            }
        };
        String::from_utf8(bytes).map_err(|_| {
            crate::Error::new(
                crate::ErrorKind::Runtime,
                format!("{} isn't text", path.display()),
            )
        })
    };
    shipyard_runtime::isolation::IdMaps::from_proc(&read("uid_map")?, &read("gid_map")?).ok_or_else(
        || {
            crate::Error::new(
                crate::ErrorKind::Runtime,
                format!("process {pid}'s ID maps can't be read"),
            )
        },
    )
}

/// Off Linux: nothing to join (containers run in the VM).
#[cfg(not(target_os = "linux"))]
fn joined(engine: &Engine, other: &Record) -> Result<crate::network::JoinedNetwork> {
    let _ = (engine, other);
    Err(crate::error::linux_only(
        "joining another container's network",
    ))
}

/// `other`'s addresses on its first network, and that network's DNS server:
/// what its own `/etc/hosts` and `/etc/resolv.conf` say.
#[cfg(target_os = "linux")]
fn bridge_addresses(
    engine: &Engine,
    other: &Record,
    networks: &[crate::network::Joined],
) -> Result<(Vec<std::net::IpAddr>, Option<std::net::IpAddr>)> {
    let Some(first) = networks.first() else {
        return Ok((Vec::new(), None));
    };
    let addresses = crate::network::container_leases(engine, other)?
        .into_iter()
        .find(|(network, _)| *network == first.name)
        .map(|(_, lease)| lease.addresses().collect())
        .unwrap_or_default();
    let store = crate::network::networks(engine)?;
    let configs: Vec<_> = networks
        .iter()
        .filter_map(|joined| store.get(&joined.name).ok().flatten())
        .collect();
    let configs: Vec<_> = configs.iter().collect();
    Ok((addresses, crate::network::resolver(&configs)))
}

/// The containers up that join `record`'s network, IPC or PID namespace (their
/// `created.json` names it), by name: none if they can't be listed.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "containers stop and go on Linux")
)]
pub(crate) fn joiners(engine: &Engine, record: &Record) -> Vec<String> {
    containers::list(engine)
        .unwrap_or_default()
        .into_iter()
        .filter(|other| {
            joins_any_of(engine, other, record)
                && containers::state(engine, other).is_ok_and(|state| state.is_active())
        })
        .map(|other| other.name.as_str().to_owned())
        .collect()
}

/// Whether `joiner` joins any of `joined`'s namespaces (its network, its
/// IPC or PID namespace).
fn joins_any_of(engine: &Engine, joiner: &Record, joined: &Record) -> bool {
    containers::created(engine, &joiner.id).is_ok_and(|created| {
        created.network_from.as_ref() == Some(&joined.id)
            || created.ipc_from.as_ref() == Some(&joined.id)
            || created.pid_from.as_ref() == Some(&joined.id)
    })
}

/// Says, once `record`'s container has stopped or gone (`what` it did),
/// that those that join its namespaces have lost them (D11.4b2-4: allowed,
/// said, as Docker allows it).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "containers stop and go on Linux")
)]
pub(crate) fn warn_joiners(engine: &Engine, record: &Record, what: &str) {
    let joiners = joiners(engine, record);
    if !joiners.is_empty() {
        engine.warn(
            crate::report::WarningKind::SharedNamespacesLost,
            format!(
                "container {} {what}: {} {} its namespaces, and lose{} them until started again",
                record.name,
                joiners.join(", "),
                if joiners.len() == 1 {
                    "shares"
                } else {
                    "share"
                },
                if joiners.len() == 1 { "s" } else { "" },
            ),
        );
    }
}
