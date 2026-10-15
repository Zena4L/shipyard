//! A run's network as the request asks for it: the mode (with Docker's
//! defaults), the plan the network module sets up, the namespace's
//! settings.

use std::path::Path;

use shipyard_network::files::DnsSettings;
use shipyard_runtime::isolation::{IdMaps, Isolation, Namespace};

use super::RunRequest;
use crate::Engine;
use crate::error::{Context as _, Result, bail};
use crate::network::{Joined, NetworkMode, NetworkPlan, RunId};
#[cfg(target_os = "linux")]
use crate::report::WarningKind;

impl RunRequest {
    /// Gives `isolation` the network `mode` (a namespace of its own, with
    /// Docker's settings for one, for any mode but `host`), and the
    /// `--sysctl` settings.
    pub(super) fn add_network(&self, isolation: &mut Isolation, mode: &NetworkMode) -> Result<()> {
        if let NetworkMode::Bridges(networks) = mode
            && isolation.id_maps.is_some()
            && !crate::network::host_root()
        {
            let names: Vec<&str> = networks.iter().map(|joined| joined.name.as_str()).collect();
            bail!(
                NeedsRoot,
                "--network {}: {}",
                names.join(", "),
                crate::network::NETWORKS_NEED_ROOT
            );
        }
        if mode.own_namespace() {
            isolation.namespaces.push(Namespace::Network);
            // The groups the container has: its own user namespace's, or
            // without one, the namespace shipyard is in (all of them only
            // for the host's root).
            #[cfg(target_os = "linux")]
            let current = (isolation.id_maps.is_none() && !crate::network::host_root())
                .then(|| shipyard_runtime::process::current_id_maps().ok())
                .flatten();
            #[cfg(not(target_os = "linux"))]
            let current: Option<IdMaps> = None;
            let maps = isolation.id_maps.as_ref().or(current.as_ref());
            isolation.sysctls = with_network_defaults(&self.sysctl, maps, |key| {
                Path::new("/proc/sys").join(key.replace('.', "/")).exists()
            });
        } else {
            isolation.sysctls.clone_from(&self.sysctl);
        }
        Ok(())
    }

    /// The run's network as the request asks for it, checked.
    pub(super) fn network_plan(&self, engine: &Engine, run: RunId) -> Result<NetworkPlan> {
        let mode = self.network_mode(engine)?;
        if (!self.publish.is_empty() || self.publish_all)
            && !matches!(mode, NetworkMode::Bridges(_) | NetworkMode::Pasta)
        {
            bail!(
                InvalidRequest,
                "--publish and --publish-all need a network the host reaches (bridge, a \
                 network's name, or pasta): with {}",
                match (&mode, self.network.is_empty()) {
                    (NetworkMode::Host, false) => {
                        "--network host, the container's ports are the host's already"
                    }
                    (NetworkMode::Host, true) => {
                        "the host's network (the default here), the container's ports are the \
                         host's already"
                    }
                    _ => "--network none, there's nothing to publish to",
                }
            );
        }
        let has_root = self.has_root();
        let etc_flags = !self.add_host.is_empty()
            || !self.dns.is_empty()
            || !self.dns_search.is_empty()
            || !self.dns_option.is_empty();
        if etc_flags && !has_root {
            bail!(
                InvalidRequest,
                "--add-host and the --dns flags write the container's /etc files, which need a \
                 root filesystem of its own (an image or --rootfs)"
            );
        }
        let pasta = if mode == NetworkMode::Pasta {
            let Some(program) = shipyard_network::pasta::find(engine.paths().program_path()) else {
                let needs = if self.network.is_empty() {
                    "a rootless container's network needs pasta"
                } else {
                    "--network pasta needs pasta"
                };
                bail!(
                    Environment,
                    "{needs}: install the passt package, or run it with --network host (the \
                     host's network) or --network none"
                );
            };
            // Asked now, before anything is made: never while a container
            // waits for its network.
            let features = shipyard_network::pasta::Features::of(&program)?;
            Some((program, features))
        } else {
            None
        };
        let hostname = match self.hostname_for(&run, &mode) {
            Some(hostname) => hostname,
            // The host's network, and its name with it.
            None => nix::unistd::gethostname()
                .context("reading the host's name")?
                .to_string_lossy()
                .into_owned(),
        };
        Ok(NetworkPlan {
            hostname,
            mode,
            run,
            etc_files: has_root,
            hosts: self.add_host.clone(),
            dns: DnsSettings {
                servers: self.dns.clone(),
                search: self.dns_search.clone(),
                options: self.dns_option.clone(),
            },
            ports: self.publish.clone(),
            publish_all: self.publish_all,
            pasta,
            defaulted: self.network.is_empty(),
            name: self.name.clone(),
            container: self.container.clone(),
        })
    }

    /// The run's network: what `network` asks for, with
    /// `--ip`, `--ip6`, `--mac-address` and `--network-alias` given to the networks
    /// they're for ([`combine_networks`]); else the default, as Docker's:
    /// the default bridge network for the host's root (where it may make
    /// one), pasta for everyone else; the host's network for a root that
    /// can't have a bridge (no `CAP_NET_ADMIN`: in a container), and off
    /// Linux.
    pub(super) fn network_mode(&self, engine: &Engine) -> Result<NetworkMode> {
        if self.network.is_empty() {
            return Ok(self
                .default_mode
                .get_or_init(|| Self::default_network(engine))
                .clone());
        }
        combine_networks(
            &self.network,
            Addresses {
                ip: self.ip,
                ip6: self.ip6,
                mac: self.mac_address,
            },
            &self.network_alias,
        )
    }

    /// The network a run gets without `--network` (see
    /// [`network_mode`](Self::network_mode)): worked out once a run, as it
    /// reads `/proc`.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "the warning is Linux's")
    )]
    fn default_network(engine: &Engine) -> NetworkMode {
        if !cfg!(target_os = "linux") {
            NetworkMode::Host
        } else if crate::network::can_network() {
            NetworkMode::bridge(shipyard_network::store::DEFAULT_NETWORK)
        } else if crate::network::host_root() {
            #[cfg(target_os = "linux")]
            if let Some(why) = crate::network::why_no_network() {
                engine.warn(
                    WarningKind::HostNetworkFallback,
                    format!(
                        "the container gets the host's network: the bridge network needs \
                         CAP_NET_ADMIN and a writable /proc/sys, and {why}. Pass --network host \
                         to choose it (without this warning), or --network none for no network"
                    ),
                );
            }
            NetworkMode::Host
        } else {
            NetworkMode::Pasta
        }
    }

    /// An isolated container's hostname: `--hostname`; else, with a network
    /// of its own, the run's ID (as Docker names containers); else none to
    /// set: with the host's network, it keeps the host's name (as Docker
    /// does), the one the host's `/etc/hosts` and DNS know.
    pub(super) fn hostname_for(&self, run: &RunId, mode: &NetworkMode) -> Option<String> {
        self.hostname
            .clone()
            .or_else(|| mode.own_namespace().then(|| run.as_str().to_owned()))
    }
}

/// `--ip`, `--ip6` and `--mac-address`: a container's addresses on its one
/// bridge network.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Addresses {
    pub(super) ip: Option<std::net::IpAddr>,
    pub(super) ip6: Option<std::net::Ipv6Addr>,
    pub(super) mac: Option<[u8; 6]>,
}

impl Addresses {
    /// Whether any is given.
    fn any(self) -> bool {
        self.ip.is_some() || self.ip6.is_some() || self.mac.is_some()
    }
}

/// The run's network from the networks asked for (`given`, one at least):
/// one `host`, `none` or `pasta` alone; or bridge networks, each once, in
/// order. `--ip`, `--ip6` and `--mac-address` (`addresses`) go to a bridge
/// network, when there's only one; `--network-alias` (`aliases`) to each
/// network that has names.
pub(super) fn combine_networks(
    given: &[NetworkMode],
    addresses: Addresses,
    aliases: &[String],
) -> Result<NetworkMode> {
    let mut networks: Vec<Joined> = Vec::new();
    for mode in given {
        match mode {
            NetworkMode::Bridges(more) => {
                for joined in more {
                    if networks.iter().any(|other| other.name == joined.name) {
                        bail!(InvalidRequest, "--network {}: given twice", joined.name);
                    }
                    networks.push(joined.clone());
                }
            }
            alone if given.len() == 1 => {
                if addresses.any() {
                    bail!(
                        InvalidRequest,
                        "--ip, --ip6 and --mac-address need a bridge network (--network bridge, \
                         or a name)"
                    );
                }
                if !aliases.is_empty() {
                    bail!(InvalidRequest, "{NO_NAMES_FOR_ALIASES}");
                }
                return Ok(alone.clone());
            }
            _ => bail!(
                InvalidRequest,
                "--network host, none and pasta can't be combined with other networks: a \
                 container on several is on bridge networks"
            ),
        }
    }
    if addresses.any() {
        let [only] = networks.as_mut_slice() else {
            bail!(
                InvalidRequest,
                "--ip, --ip6 and --mac-address are for one network: with several, give each its \
                 own (--network name=NETWORK,ip=IP,ip6=IP,mac-address=MAC)"
            );
        };
        if (addresses.ip.is_some() && only.ip.is_some())
            || (addresses.ip6.is_some() && only.ip6.is_some())
            || (addresses.mac.is_some() && only.mac.is_some())
        {
            bail!(
                InvalidRequest,
                "--network {}: its address is given twice",
                only.name
            );
        }
        only.ip = only.ip.or(addresses.ip);
        only.ip6 = only.ip6.or(addresses.ip6);
        only.mac = only.mac.or(addresses.mac);
    }
    let named = networks
        .iter()
        .any(|joined| shipyard_network::store::has_dns(&joined.name));
    if !aliases.is_empty() && !named {
        bail!(InvalidRequest, "{NO_NAMES_FOR_ALIASES}");
    }
    if networks.len() > MAX_NETWORKS {
        bail!(
            InvalidRequest,
            "a container can join at most {MAX_NETWORKS} networks"
        );
    }
    for joined in &mut networks {
        if !shipyard_network::store::has_dns(&joined.name) {
            if !joined.aliases.is_empty() {
                bail!(InvalidRequest, "{NO_NAMES_FOR_ALIASES}");
            }
            continue;
        }
        joined.aliases.extend(aliases.iter().cloned());
        // Each once, whatever its case.
        let mut seen = std::collections::HashSet::new();
        joined
            .aliases
            .retain(|alias| seen.insert(alias.to_ascii_lowercase()));
    }
    Ok(NetworkMode::Bridges(networks))
}

/// Why `--network-alias` (or `alias=`) can't be given here.
const NO_NAMES_FOR_ALIASES: &str = "--network-alias and alias= need a network made with \
                                    `shipyard network create`: only those have names (the \
                                    default bridge network has none, as Docker's)";

/// The most networks a container joins (its interfaces' and veths' names
/// have room for two digits).
pub(crate) const MAX_NETWORKS: usize = 100;

/// `user` sysctls, after Docker's defaults for a container with a network of
/// its own (a setting the user gives wins), each only if the host kernel has
/// it (`has`):
///
/// - `net.ipv4.ip_unprivileged_port_start=0`: any user in the container may
///   listen on a port below 1024 (it's the container's own network);
/// - `net.ipv4.ping_group_range`: any group may `ping` with an ICMP socket,
///   without `CAP_NET_RAW`. Every group ID from 0 up: the host's whole range
///   without a user namespace; with one, up to the top of its group map (the
///   kernel refuses a group it doesn't map, probe N2). The kernel compares
///   the two ends as host IDs, and quietly turns `ping` off if the top's is
///   below 0's, so the top is the highest group whose host ID is at least
///   0's; none at all if group 0 isn't mapped;
/// - `net.ipv4.ip_forward=0` and `net.ipv6.conf.all.forwarding=0`: the
///   container routes nothing. A new network namespace can copy the host's
///   settings, which are on wherever a bridge network is used: a container
///   on two networks would route between them, behind the firewall's back
///   (its frames never leave the bridges), and join networks kept apart.
pub(super) fn with_network_defaults(
    user: &[(String, String)],
    maps: Option<&IdMaps>,
    has: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    /// The highest group ID (`gid_t`'s positive range).
    const MAX_GROUP: u32 = 2_147_483_647;
    let port_start = "net.ipv4.ip_unprivileged_port_start";
    let ping_groups = "net.ipv4.ping_group_range";
    let top_group = match maps {
        None => Some(MAX_GROUP),
        Some(maps) => maps.host_gid(0).and_then(|zero| {
            maps.gids
                .iter()
                .map(|map| map.container.saturating_add(map.size.saturating_sub(1)))
                .filter(|&top| maps.host_gid(top).is_some_and(|host| host >= zero))
                .max()
        }),
    };
    let mut defaults = vec![(port_start.to_owned(), "0".to_owned())];
    if let Some(top) = top_group {
        defaults.push((ping_groups.to_owned(), format!("0 {top}")));
    }
    defaults.push(("net.ipv4.ip_forward".to_owned(), "0".to_owned()));
    defaults.push(("net.ipv6.conf.all.forwarding".to_owned(), "0".to_owned()));
    defaults
        .into_iter()
        .filter(|(key, _)| has(key) && !user.iter().any(|(given, _)| given == key))
        .chain(user.iter().cloned())
        .collect()
}
