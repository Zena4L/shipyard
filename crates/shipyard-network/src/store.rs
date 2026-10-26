//! Networks on disk: what each one is (its subnet, gateway and bridge), and
//! which of its addresses are taken (its leases).
//!
//! ```text
//! <dir>/.lock                  flock: exclusive to create or remove a network
//! <dir>/<name>/config.json     the network (written once, by a rename)
//! <dir>/<name>/lock            flock: exclusive to change the network
//! <dir>/<name>/next            the last address handed out
//! <dir>/<name>/leases/<ip>     one per container using an address
//! <dir>/<name>/dns.lock        flock: held by the network's DNS server
//! <dir>/<name>/dns.log         what the DNS server had to say
//! ```
//!
//! A lease is a file the run holds an exclusive `flock` on for its whole
//! life: the lock is the proof it's alive. When the run ends, it removes the
//! file; when it's killed, the kernel drops the lock, and the next one to
//! take the network's lock finds the lease stale and sweeps it (with what it
//! left behind: the host's end of its veth pair). No daemon keeps count.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ipnet::IpNet;

/// The default network: the one `--network bridge` names.
pub const DEFAULT_NETWORK: &str = "bridge";

/// The default network's bridge device.
pub const DEFAULT_BRIDGE: &str = "shipyard0";

/// How the names of the host's devices Shipyard makes start: other
/// networks' bridges (`shy-…`) and containers' veths (`shy…`).
pub const DEVICE_PREFIX: &str = "shy";

/// How long a change waits for a network another command is changing.
pub const LOCK_WAIT: Duration = Duration::from_secs(15);

/// The most of a network's `config.json` read: a few hundred bytes, its
/// labels and options among them.
const CONFIG_MAX: u64 = 1024 * 1024;

/// The most of a lease read: a few dozen bytes.
const LEASE_MAX: u64 = 64 * 1024;

/// The most of `next` read: one address.
const LINE_MAX: u64 = 4 * 1024;

/// The most of a `.routing-closed` record read: the boot's ID, then one line
/// for each of the host's interfaces (other engines' veths among them), so
/// room for tens of thousands.
const ROUTING_CLOSED_MAX: u64 = 1024 * 1024;

/// A file's text, read with a bound: as `fs::read_to_string` would have it.
fn read_text(path: &Path, cap: u64) -> io::Result<String> {
    shipyard_sys::fs::text(shipyard_sys::fs::read_bounded(path, cap)?)
}

/// The container's end of its veth pair on its first network (on the
/// others, `eth1`, `eth2`…).
pub const CONTAINER_INTERFACE: &str = "eth0";

/// The container's interface on its `n`th network (0 first): `eth{n}`.
#[must_use]
pub fn interface_name(n: usize) -> String {
    format!("eth{n}")
}

/// What [`Networks::routing_closed`] read: the closing its records say,
/// and each family whose record couldn't be read, with why (F65).
#[derive(Debug, Default)]
pub struct RoutingRecords {
    /// The routing closed, by the records read.
    pub closed: crate::nft::ClosedRouting,
    /// The families whose record is there but couldn't be read.
    pub unread: Vec<(crate::Family, StoreError)>,
}

/// The file that records [`Networks::routing_closed`] for IPv4.
const ROUTING_CLOSED: &str = ".routing-closed";
/// The same for IPv6.
const ROUTING_CLOSED6: &str = ".routing-closed-ipv6";

/// The file that records Shipyard closed `family`'s routing.
fn routing_closed_file(family: crate::Family) -> &'static str {
    match family {
        crate::Family::V4 => ROUTING_CLOSED,
        crate::Family::V6 => ROUTING_CLOSED6,
    }
}

/// This boot's ID: what a routing-closed file holds, so that one from
/// another boot (when forwarding was reset, and maybe turned on since by
/// someone else) counts for nothing. Empty on a system without one (not
/// Linux); one that can't be read is an error, never "another boot" (F65).
fn boot_id() -> io::Result<String> {
    match fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(id) => Ok(id.trim().to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

/// The longest network name.
const MAX_NAME: usize = 64;

/// How many IPv6 addresses after the one that goes with a container's IPv4
/// address are tried, when that one's taken (by an `--ip6`).
const MAX_NEXT6: u128 = 65_536;

/// What can go wrong with the networks on disk.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A file or directory couldn't be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A name a network can't have.
    #[error(
        "\"{0}\" can't be a network's name: letters, digits, _, . and -, starting with a letter or \
         digit, at most 64 characters"
    )]
    InvalidName(String),
    /// No network has this name.
    #[error("there's no network named {0}")]
    NotFound(String),
    /// A network with this name exists already.
    #[error("a network named {0} exists already")]
    Exists(String),
    /// Containers are using it.
    #[error("the network {name} is in use by {}", users_text(users))]
    InUse {
        /// Its name.
        name: String,
        /// The containers on it, as its DNS names their addresses: a
        /// container's name, else its short ID, else its run's ID.
        users: Vec<String>,
    },
    /// Another command has held it for longer than [`LOCK_WAIT`].
    #[error("the network {0} is busy: another shipyard command is changing it")]
    Busy(String),
    /// A subnet or address that can't be used as asked.
    #[error("{0}")]
    Address(String),
    /// Every address of the network is taken.
    #[error("the network {0} has no free address left")]
    Full(String),
    /// Labels and options too many to keep (their file would be over
    /// 64 KiB).
    #[error("the labels and options of the network {0} are too many: 64 KiB at most")]
    TooManyLabels(String),
    /// Its `labels.json` isn't one this Shipyard reads: a newer version,
    /// or not one at all.
    #[error("the labels of the network {name}: {source}")]
    Labels {
        /// The network.
        name: String,
        /// Why.
        #[source]
        source: shipyard_sys::versioned::Error,
    },
    /// A container on the network answers to the name already.
    #[error("the name {name} is taken on the network {network}, by a running container")]
    NameTaken {
        /// The name.
        name: String,
        /// The network.
        network: String,
    },
    /// A config file that isn't one.
    #[error(
        "{path}: {reason} (shipyard didn't write it so: remove the network's directory to \
         forget the network, and its bridge with it)"
    )]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What's wrong with it.
        reason: String,
    },
}

impl StoreError {
    fn io(context: impl Into<String>, source: io::Error) -> Self {
        StoreError::Io {
            context: context.into(),
            source,
        }
    }
}

/// A network, as created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Its name.
    pub name: String,
    /// Its addresses (the network address and prefix).
    pub subnet: IpNet,
    /// The bridge's own address in it: the containers' way out.
    pub gateway: IpAddr,
    /// The bridge device.
    pub bridge: String,
    /// No way out of it (no NAT): containers reach each other and the host.
    pub internal: bool,
    /// When it was created, in seconds since the epoch.
    pub created: u64,
    /// Its IPv6 prefix, if it has IPv6 (`--ipv6`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet6: Option<IpNet>,
    /// The bridge's IPv6 address in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway6: Option<Ipv6Addr>,
    /// Its containers kept from the host itself (its services, on any of
    /// its addresses) and from the cloud's metadata addresses, but for the
    /// network's DNS server (`--isolate-host`, stage 9, step 9.7c1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub isolate_host: bool,
    /// Each of its containers may have at most
    /// [`CONNTRACK_LIMIT`](crate::nft::CONNTRACK_LIMIT) connections open
    /// that it opened; more are refused (`--conntrack-limit`, stage 9, step
    /// 9.7c2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conntrack_limit: bool,
}

impl NetworkConfig {
    /// Whether it has a DNS server, and its containers names ([`has_dns`]).
    #[must_use]
    pub fn has_dns(&self) -> bool {
        has_dns(&self.name)
    }

    /// The gateway with the subnet's prefix: the bridge's address.
    #[must_use]
    pub fn gateway_net(&self) -> IpNet {
        self.host_net(self.gateway)
    }

    /// The IPv6 gateway with its prefix, if the network has IPv6.
    #[must_use]
    pub fn gateway6_net(&self) -> Option<IpNet> {
        let subnet6 = self.subnet6?;
        IpNet::new(IpAddr::V6(self.gateway6?), subnet6.prefix()).ok()
    }

    /// `ip` (one of the network's) with the subnet's prefix: an interface's
    /// address on it.
    #[must_use]
    pub fn host_net(&self, ip: IpAddr) -> IpNet {
        // Of the subnet's family (checked when it was handed out): the
        // prefix fits.
        IpNet::new(ip, self.subnet.prefix()).unwrap_or(self.subnet)
    }
}

/// What to create: a name, and the subnet and gateway if they're given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is one of `network create`'s flags, on or off independently"
)]
pub struct NetworkSpec {
    /// Its name.
    pub name: String,
    /// Its subnet; `None` picks a free one.
    pub subnet: Option<IpNet>,
    /// Its gateway; `None` is the subnet's first address.
    pub gateway: Option<IpAddr>,
    /// No way out (see [`NetworkConfig::internal`]).
    pub internal: bool,
    /// IPv6 too: a prefix of its own (`subnet6`, or a random ULA one).
    pub ipv6: bool,
    /// Its IPv6 prefix; `None` picks one (with `ipv6`).
    pub subnet6: Option<IpNet>,
    /// Its IPv6 gateway; `None` is the prefix's first address.
    pub gateway6: Option<Ipv6Addr>,
    /// Its containers kept from the host (see
    /// [`NetworkConfig::isolate_host`]).
    pub isolate_host: bool,
    /// Its containers' connections limited (see
    /// [`NetworkConfig::conntrack_limit`]).
    pub conntrack_limit: bool,
}

/// One container's address on a network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Its address.
    pub ip: IpAddr,
    /// Its hardware address.
    pub mac: [u8; 6],
    /// The run's ID.
    pub run: String,
    /// The container's hostname.
    pub hostname: String,
    /// The host's end of its veth pair.
    pub host_veth: String,
    /// The ports it publishes (none in a lease written before they could
    /// be).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<crate::ports::Published>,
    /// The container's name (`--name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its other names on the network (`--network-alias`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Its interface in the container (`eth0` on its first network).
    #[serde(default = "first_interface")]
    pub interface: String,
    /// Its IPv6 address, on a network with IPv6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip6: Option<Ipv6Addr>,
    /// The container's ID, for a container (stage 9; a foreground run
    /// has none): how `rename` finds its leases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// A lease's interface when it doesn't say (one written before a container
/// could be on several networks).
fn first_interface() -> String {
    CONTAINER_INTERFACE.to_owned()
}

impl Lease {
    /// The names its container answers to on the network, in order: its
    /// name, its aliases, its container's short ID (its hostname, as `ps`
    /// shows it), its run's ID.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        names(
            self.name.as_ref(),
            &self.aliases,
            self.container.as_ref(),
            &self.run,
        )
    }

    /// The name its address has (a reverse lookup's answer): its
    /// container's name, else its short ID, else its run's ID.
    #[must_use]
    pub fn address_name(&self) -> &str {
        self.name
            .as_deref()
            .or_else(|| short_container_id(self.container.as_ref()))
            .unwrap_or(&self.run)
    }

    /// Its addresses: its IPv4 one, and its IPv6 one if it has one.
    pub fn addresses(&self) -> impl Iterator<Item = IpAddr> {
        std::iter::once(self.ip).chain(self.ip6.map(IpAddr::V6))
    }

    /// Its port's guard: what its container may send as (see the module
    /// `nft::spoofing`): its addresses, from its hardware address.
    #[must_use]
    pub fn port_guard(&self) -> crate::nft::PortGuard {
        crate::nft::PortGuard {
            port: self.host_veth.clone(),
            mac: self.mac,
            ip: match self.ip {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(_) => None,
            },
            ip6: self.ip6,
        }
    }

    /// Its published ports, as the firewall's rules: a port on every
    /// address goes to its IPv4 address, and to its IPv6 one if it has one;
    /// a port on an address, to its address of that family (none if it has
    /// no IPv6).
    #[must_use]
    pub fn port_rules(&self) -> Vec<crate::nft::PortRule> {
        let ip6 = self.ip6.map(IpAddr::V6);
        let mut rules = Vec::new();
        for &published in &self.ports {
            let containers = match published.host_ip {
                None => [Some(self.ip), ip6],
                Some(IpAddr::V4(_)) => [Some(self.ip), None],
                Some(IpAddr::V6(_)) => [ip6, None],
            };
            rules.extend(
                containers
                    .into_iter()
                    .flatten()
                    .map(|container| crate::nft::PortRule {
                        published,
                        container,
                    }),
            );
        }
        rules
    }
}

/// What a run asks of a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRequest {
    /// The run's ID.
    pub run: String,
    /// The container's hostname.
    pub hostname: String,
    /// The host's end of its veth pair.
    pub host_veth: String,
    /// An address asked for (`--ip`); `None` takes the next free one.
    pub ip: Option<IpAddr>,
    /// A hardware address asked for (`--mac-address`); `None` derives one
    /// from the address.
    pub mac: Option<[u8; 6]>,
    /// The ports it publishes (already held).
    pub ports: Vec<crate::ports::Published>,
    /// The container's name (`--name`).
    pub name: Option<String>,
    /// Its other names (`--network-alias`).
    pub aliases: Vec<String>,
    /// Its interface in the container (`eth0`, `eth1`…).
    pub interface: String,
    /// An IPv6 address asked for (`--ip6`); `None` takes the one that goes
    /// with its IPv4 address.
    pub ip6: Option<Ipv6Addr>,
    /// The container's ID, for a container (see [`Lease::container`]).
    pub container: Option<String>,
}

impl LeaseRequest {
    /// The names its container would answer to (see [`Lease::names`]).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        names(
            self.name.as_ref(),
            &self.aliases,
            self.container.as_ref(),
            &self.run,
        )
    }
}

/// The most containers an in-use message names (then "and N more").
const MOST_USERS_SAID: usize = 5;

/// `users` (containers' names), for a message: `web, db`, the first five
/// and then `and 3 more`.
#[must_use]
pub fn users_text(users: &[String]) -> String {
    let said = users
        .iter()
        .take(MOST_USERS_SAID)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match users.len().saturating_sub(MOST_USERS_SAID) {
        0 => said,
        more => format!("{said} and {more} more"),
    }
}

/// How many characters of a container's ID are its short ID (`ps`'s, and
/// its hostname).
pub const SHORT_CONTAINER_ID: usize = 12;

/// A container's short ID, from its whole ID (`container`), if it has one
/// that long.
fn short_container_id(container: Option<&String>) -> Option<&str> {
    container.and_then(|id| id.get(..SHORT_CONTAINER_ID))
}

/// A container's names on a network, in order: its name, its aliases, its
/// short ID (a container's), its run's ID.
fn names<'a>(
    name: Option<&'a String>,
    aliases: &'a [String],
    container: Option<&'a String>,
    run: &'a str,
) -> impl Iterator<Item = &'a str> {
    name.into_iter()
        .chain(aliases)
        .map(String::as_str)
        .chain(short_container_id(container))
        .chain(std::iter::once(run))
}

/// The networks, in a directory.
#[derive(Debug, Clone)]
pub struct Networks {
    dir: PathBuf,
}

impl Networks {
    /// The networks in `dir` (made if missing).
    pub fn open(dir: &Path) -> Result<Networks, StoreError> {
        fs::create_dir_all(dir)
            .map_err(|error| StoreError::io(format!("making {}", dir.display()), error))?;
        // Listable by the networks' DNS servers, which aren't root (they
        // read their askers' other networks through it), and by no one else,
        // whatever the umask.
        share_with_servers(dir, 0o750)?;
        Ok(Networks {
            dir: dir.to_owned(),
        })
    }

    /// The networks in `dir`, if it's there: never made nor changed, for a
    /// reader that only looks (a container's view).
    #[must_use]
    pub fn existing(dir: &Path) -> Option<Networks> {
        dir.is_dir().then(|| Networks {
            dir: dir.to_owned(),
        })
    }

    /// The networks container `container` (its full ID) has a live lease
    /// on, each with that lease: read without any lock (a listing), and a
    /// network that can't be read left out, never failing the others.
    #[must_use]
    pub fn leases_of(&self, container: &str) -> Vec<(NetworkConfig, Lease)> {
        let mut found: Vec<(NetworkConfig, Lease)> = self
            .readable_with_leases()
            .filter_map(|(config, leases)| {
                let lease = leases
                    .into_iter()
                    .find(|lease| lease.container.as_deref() == Some(container))?;
                Some((config, lease))
            })
            .collect();
        found.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
        found
    }

    /// [`leases_of`](Self::leases_of) for every container at once, by its
    /// ID: each network read once, so a listing costs its networks' leases,
    /// not those times its containers (11.7: `docker ps` grew with their
    /// square).
    #[must_use]
    pub fn live_leases_by_container(&self) -> BTreeMap<String, Vec<(NetworkConfig, Lease)>> {
        let mut found: BTreeMap<String, Vec<(NetworkConfig, Lease)>> = BTreeMap::new();
        for (config, leases) in self.readable_with_leases() {
            for lease in leases {
                let Some(container) = lease.container.clone() else {
                    continue;
                };
                let theirs = found.entry(container).or_default();
                // Its first live lease on a network, as `leases_of` finds it.
                if !theirs.iter().any(|(had, _)| had.name == config.name) {
                    theirs.push((config.clone(), lease));
                }
            }
        }
        for theirs in found.values_mut() {
            theirs.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
        }
        found
    }

    /// Every network that can be read, with its live leases: one that
    /// can't (or goes meanwhile) left out, never failing the others.
    fn readable_with_leases(&self) -> impl Iterator<Item = (NetworkConfig, Vec<Lease>)> + '_ {
        fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| validate_name(name).is_ok())
            .filter_map(|name| {
                let config = self.get(&name).ok().flatten()?;
                let leases = self.live_leases(&name).ok()?;
                Some((config, leases))
            })
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every network, by name.
    pub fn list(&self) -> Result<Vec<NetworkConfig>, StoreError> {
        let context = || format!("reading {}", self.dir.display());
        let mut networks = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|error| StoreError::io(context(), error))? {
            let entry = entry.map_err(|error| StoreError::io(context(), error))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Only networks' directories: a stray file (someone's notes) is
            // nobody's network.
            if validate_name(&name).is_err() || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if let Some(config) = self.get(&name)? {
                networks.push(config);
            }
        }
        networks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(networks)
    }

    /// The network named `name`, if there is one.
    pub fn get(&self, name: &str) -> Result<Option<NetworkConfig>, StoreError> {
        validate_name(name)?;
        let path = self.dir.join(name).join("config.json");
        let text = match read_text(&path, CONFIG_MAX) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(StoreError::io(format!("reading {}", path.display()), error));
            }
        };
        read_config(name, &text, path).map(Some)
    }

    /// Creates the network `spec` asks for, with its subnet free of
    /// `taken` (the host's routes) and of every other network's.
    pub fn create(&self, spec: &NetworkSpec, taken: &[IpNet]) -> Result<NetworkConfig, StoreError> {
        self.create_with(spec, taken, &NetworkMeta::default())
    }

    /// [`create`](Self::create), with `meta` (Docker's API's labels and
    /// options) in its `labels.json`, written before its `config.json`: a
    /// network is there once its config is, so one cut short never lists
    /// with another's labels.
    pub fn create_with(
        &self,
        spec: &NetworkSpec,
        taken: &[IpNet],
        meta: &NetworkMeta,
    ) -> Result<NetworkConfig, StoreError> {
        validate_name(&spec.name)?;
        // One create or remove at a time: two creates choosing a subnet at
        // once would choose the same one.
        let _store = self.lock_store()?;
        let dir = self.dir.join(&spec.name);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if self.get(&spec.name)?.is_some() {
                    return Err(StoreError::Exists(spec.name.clone()));
                }
                // A create that died before its config (creates take turns,
                // so none is under way): take it over.
            }
            Err(error) => {
                return Err(StoreError::io(format!("making {}", dir.display()), error));
            }
        }
        let made = (|| {
            make_lock_file(&dir.join("lock"))?;
            let others: Vec<IpNet> = self
                .list()?
                .into_iter()
                .filter(|other| other.name != spec.name)
                .flat_map(|other| std::iter::once(other.subnet).chain(other.subnet6))
                .collect();
            let taken: Vec<IpNet> = taken.iter().copied().chain(others).collect();
            let subnet = match spec.subnet {
                Some(subnet) => check_subnet(subnet, &taken)?,
                None => choose_subnet(spec.name == DEFAULT_NETWORK, &taken).ok_or_else(|| {
                    StoreError::Address(
                        "no free subnet left in 10.88.0.0-10.255.255.255: give one (--subnet)"
                            .to_owned(),
                    )
                })?,
            };
            let gateway = check_gateway(subnet, spec.gateway)?;
            let ipv6 = spec.ipv6 || spec.subnet6.is_some();
            if spec.gateway6.is_some() && !ipv6 {
                return Err(StoreError::Address(
                    "an IPv6 gateway needs IPv6 (--ipv6)".to_owned(),
                ));
            }
            let subnet6 = match spec.subnet6 {
                Some(subnet6) => Some(check_subnet6(subnet6, &taken)?),
                None if ipv6 => Some(random_ula(&taken)?),
                None => None,
            };
            if let Some(subnet6) = subnet6
                && subnet6.size() < subnet.size()
            {
                // Each container's IPv6 address goes with its IPv4 one.
                return Err(StoreError::Address(format!(
                    "the IPv6 prefix {subnet6} is smaller than the subnet {subnet}: it needs an \
                     address for each of the subnet's"
                )));
            }
            let gateway6 = subnet6
                .map(|subnet6| check_gateway6(subnet6, spec.gateway6))
                .transpose()?;
            let config = NetworkConfig {
                name: spec.name.clone(),
                subnet,
                gateway,
                bridge: bridge_name(&spec.name),
                internal: spec.internal,
                created: shipyard_sys::time::unix_seconds(),
                subnet6,
                gateway6,
                isolate_host: spec.isolate_host,
                conntrack_limit: spec.conntrack_limit,
            };
            write_meta(&spec.name, &dir, meta)?;
            let json = serde_json::to_vec_pretty(&config).map_err(|error| {
                StoreError::io("writing the network's config", io::Error::other(error))
            })?;
            write_atomically(&dir.join("config.json"), &json)?;
            Ok(config)
        })();
        if made.is_err() {
            // Without a config, nothing uses it: no run can lock a network
            // that has none.
            let _ = fs::remove_dir_all(&dir);
        }
        made
    }

    /// The labels and options of the network `name` (none if it has no
    /// `labels.json`: one made by the CLI, or before 11.2c2b).
    ///
    /// # Errors
    ///
    /// [`StoreError::Labels`] for a newer version or not one at all;
    /// [`StoreError::Io`] if it can't be read (never through a symlink,
    /// never past 64 KiB).
    pub fn meta(&self, name: &str) -> Result<NetworkMeta, StoreError> {
        validate_name(name)?;
        let path = self.dir.join(name).join(META_FILE);
        match shipyard_sys::versioned::read_file::<MetaFile>(&path, META_MAX, META_VERSION) {
            Ok(file) => Ok(file.map_or_else(NetworkMeta::default, |file| NetworkMeta {
                id: file.id,
                labels: file.labels,
                options: file.options,
            })),
            Err(shipyard_sys::versioned::FileError::Io(error)) => {
                Err(StoreError::io(format!("reading {}", path.display()), error))
            }
            Err(shipyard_sys::versioned::FileError::Versioned(source)) => Err(StoreError::Labels {
                name: name.to_owned(),
                source,
            }),
        }
    }

    /// The network named `name`'s live leases, without its lock: for a
    /// listing, which mustn't wait for a run being set up.
    pub fn live_leases(&self, name: &str) -> Result<Vec<Lease>, StoreError> {
        validate_name(name)?;
        Ok(read_leases(&self.dir.join(name).join("leases"))?
            .into_iter()
            .filter_map(|(lease, live)| live.then_some(lease))
            .collect())
    }

    /// The guards of every network's live leases' ports (without any
    /// network's lock, as [`live_leases`]).
    ///
    /// # Errors
    ///
    /// The store can't be read.
    ///
    /// [`live_leases`]: Self::live_leases
    pub fn live_port_guards(&self) -> Result<Vec<crate::nft::PortGuard>, StoreError> {
        let mut guards = Vec::new();
        for network in self.list()? {
            for lease in self.live_leases(&network.name)? {
                guards.push(lease.port_guard());
            }
        }
        Ok(guards)
    }

    /// The ports every network's live leases publish, as the firewall's
    /// maps hold them (without any network's lock, as [`live_leases`]).
    ///
    /// [`live_leases`]: Self::live_leases
    pub fn live_port_rules(&self) -> Result<Vec<crate::nft::PortRule>, StoreError> {
        let mut rules = Vec::new();
        for network in self.list()? {
            for lease in self.live_leases(&network.name)? {
                rules.extend(lease.port_rules());
            }
        }
        Ok(rules)
    }

    /// The network named `name`, locked for a change (waiting up to
    /// [`LOCK_WAIT`] for another command to finish with it).
    pub fn lock(&self, name: &str) -> Result<LockedNetwork, StoreError> {
        let lock = self.lock_dir(name)?;
        let config = self
            .get(name)?
            .ok_or_else(|| StoreError::NotFound(name.to_owned()))?;
        Ok(LockedNetwork {
            dir: self.dir.join(name),
            config,
            _lock: lock,
        })
    }

    /// Removes the network named `name`, locked (`network`), if no
    /// container uses it: the caller removes its bridge first.
    pub fn remove(&self, network: LockedNetwork) -> Result<(), StoreError> {
        network.ensure_unused()?;
        // Held to the end: the locks go when the directory has. (The
        // network's lock is taken before the store's here, and a create takes
        // only the store's: no two commands wait for each other.)
        let _store = self.lock_store()?;
        let LockedNetwork { dir, _lock, .. } = network;
        // The config goes first: from then on, the network isn't there.
        let config = dir.join("config.json");
        fs::remove_file(&config)
            .map_err(|error| StoreError::io(format!("removing {}", config.display()), error))?;
        fs::remove_dir_all(&dir)
            .map_err(|error| StoreError::io(format!("removing {}", dir.display()), error))
    }

    /// The exclusive lock on `name`'s directory. Its lock file is never
    /// made here (only by [`create`](Self::create)), so a lock can't bring
    /// back a network being removed; and the file locked must still be the
    /// one at its path, or the lock holds nothing (the network was removed
    /// while this waited), and it's taken again.
    fn lock_dir(&self, name: &str) -> Result<Flock<File>, StoreError> {
        validate_name(name)?;
        lock_path(&self.dir.join(name).join("lock"), name)
    }

    /// The lock file the network `name`'s DNS server holds while it runs.
    #[must_use]
    pub fn dns_lock_path(&self, name: &str) -> PathBuf {
        self.dir.join(name).join("dns.lock")
    }

    /// Opens (making it if need be) the lock file the network `name`'s DNS
    /// server holds, for a starter to take for it.
    pub fn open_dns_lock(&self, name: &str) -> Result<File, StoreError> {
        validate_name(name)?;
        let path = self.dns_lock_path(name);
        shipyard_sys::fs::open_lock_file(&path)
            .map_err(|error| StoreError::io(format!("opening {}", path.display()), error))
    }

    /// Whether the network `name`'s DNS server is running: its lock is held.
    #[must_use]
    pub fn dns_running(&self, name: &str) -> bool {
        validate_name(name).is_ok()
            && OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(self.dns_lock_path(name))
                .is_ok_and(is_held)
    }

    /// The families Shipyard turned the host's forwarding on for (it was
    /// off) since the host started, and the interfaces the host had when it
    /// did: the firewall keeps the host from routing between those for
    /// anyone (see `nft`). Recorded, so a rebuilt firewall does too; but not
    /// past a reboot, after which the forwarding is someone else's to turn
    /// on.
    ///
    /// A family whose record is there but can't be read (its permissions,
    /// the disk, past its bound, not a regular file, a symbolic link, not
    /// text), or whose boot can't be told (this boot's ID unread), is in
    /// [`RoutingRecords::unread`], never taken as "not closed": a table
    /// rebuilt so would let the host route for anyone (F65). The engine
    /// decides what to close instead. No record is no closing.
    #[must_use]
    pub fn routing_closed(&self) -> RoutingRecords {
        let mut records = RoutingRecords::default();
        for family in [crate::Family::V4, crate::Family::V6] {
            let path = self.dir.join(routing_closed_file(family));
            let text = match read_text(&path, ROUTING_CLOSED_MAX) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    let error = StoreError::io(format!("reading {}", path.display()), error);
                    records.unread.push((family, error));
                    continue;
                }
            };
            let boot = match boot_id() {
                Ok(boot) => boot,
                Err(error) => {
                    let error = StoreError::io(
                        format!("reading this boot's ID, to tell {}'s boot", path.display()),
                        error,
                    );
                    records.unread.push((family, error));
                    continue;
                }
            };
            let mut lines = text.lines();
            if lines.next().map(str::trim) != Some(boot.as_str()) {
                continue;
            }
            let closed = &mut records.closed;
            closed.families.push(family);
            for interface in lines.map(str::trim).filter(|line| !line.is_empty()) {
                if !closed.interfaces.iter().any(|known| known == interface) {
                    closed.interfaces.push(interface.to_owned());
                }
            }
        }
        records
    }

    /// Records that Shipyard turned forwarding on for `family`, when the
    /// host's own interfaces were `interfaces`
    /// ([`routing_closed`](Self::routing_closed)).
    pub fn close_routing(
        &self,
        family: crate::Family,
        interfaces: &[String],
    ) -> Result<(), StoreError> {
        let path = self.dir.join(routing_closed_file(family));
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            return Err(StoreError::io(
                format!(
                    "writing {}: a directory is in its place (remove it: it's Shipyard's \
                     record of the routing it closed, never a directory)",
                    path.display()
                ),
                io::Error::from(io::ErrorKind::IsADirectory),
            ));
        }
        let mut text =
            boot_id().map_err(|error| StoreError::io("reading this boot's ID", error))?;
        for interface in interfaces {
            text.push('\n');
            text.push_str(interface);
        }
        text.push('\n');
        write_atomically(&path, text.as_bytes())
    }

    /// The lock that makes changes to the whole store, or to what all
    /// networks share (the firewall's table), one at a time. Held briefly;
    /// someone holding a network's lock may take it, never the other way.
    pub fn lock_all(&self) -> Result<StoreLock, StoreError> {
        Ok(StoreLock {
            _lock: self.lock_store()?,
        })
    }

    /// The exclusive lock on the store: to create or remove a network, or
    /// to rebuild the firewall's table (see [`lock_all`](Self::lock_all)).
    fn lock_store(&self) -> Result<Flock<File>, StoreError> {
        let path = self.dir.join(".lock");
        make_lock_file(&path)?;
        lock_path(&path, "(every network)")
    }
}

/// The store's own lock, held (see [`Networks::lock_all`]).
#[derive(Debug)]
pub struct StoreLock {
    _lock: Flock<File>,
}

/// A network, locked: while it's held, no other command changes it.
#[derive(Debug)]
pub struct LockedNetwork {
    dir: PathBuf,
    config: NetworkConfig,
    _lock: Flock<File>,
}

impl LockedNetwork {
    /// The network.
    #[must_use]
    pub fn config(&self) -> &NetworkConfig {
        &self.config
    }

    /// Every lease, and whether its run is alive.
    pub fn leases(&self) -> Result<Vec<(Lease, bool)>, StoreError> {
        read_leases(&self.dir.join("leases"))
    }

    /// [`StoreError::InUse`] if a live run holds a lease on it.
    pub fn ensure_unused(&self) -> Result<(), StoreError> {
        let mut users: Vec<String> = self
            .leases()?
            .iter()
            .filter(|(_, live)| *live)
            .map(|(lease, _)| lease.address_name().to_owned())
            .collect();
        if users.is_empty() {
            return Ok(());
        }
        users.sort();
        Err(StoreError::InUse {
            name: self.config.name.clone(),
            users,
        })
    }

    /// Removes the leases of runs that are gone, and returns them: what
    /// they left behind (their veth's host end) is the caller's to clean up.
    ///
    /// A lease file no run holds that can't be read (left empty or half
    /// written by a run killed as it made it, or not a lease at all) goes
    /// too: its address would otherwise never be free again.
    pub fn sweep(&self) -> Result<Vec<Lease>, StoreError> {
        let mut swept = Vec::new();
        for path in lease_paths(&self.dir.join("leases"))? {
            let Some((lease, file)) = open_lease(&path)? else {
                continue;
            };
            // Locked by us: nobody else has it, and nobody can take it now
            // (leases are made under the network's lock, which we hold).
            let Ok(_stale) = Flock::lock(file, FlockArg::LockExclusiveNonblock) else {
                continue;
            };
            match fs::remove_file(&path) {
                // Released meanwhile by its run (outside the network lock).
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                removed => removed.map_err(|error| {
                    StoreError::io(format!("removing {}", path.display()), error)
                })?,
            }
            swept.extend(lease);
        }
        Ok(swept)
    }

    /// `name` refused if a live container on the network (in `leases`) other
    /// than the one at `except` answers to it: names are unique on a
    /// network, ignoring case (as DNS does, RFC 4343).
    fn refuse_taken_name(
        &self,
        leases: &[(Lease, bool)],
        name: &str,
        except: Option<IpAddr>,
    ) -> Result<(), StoreError> {
        let taken = leases.iter().any(|(lease, live)| {
            *live
                && Some(lease.ip) != except
                && lease.names().any(|other| other.eq_ignore_ascii_case(name))
        });
        if taken {
            return Err(StoreError::NameTaken {
                name: name.to_owned(),
                network: self.config.name.clone(),
            });
        }
        Ok(())
    }

    /// Whether container `container` has a live lease on the network, and
    /// its name there: `Ok(None)` if it has none. Found by its ID, or, if
    /// `name` is given, by that name (a lease made before leases knew their
    /// container).
    ///
    /// # Errors
    ///
    /// The leases couldn't be read.
    pub fn name_of(
        &self,
        container: &str,
        name: Option<&str>,
    ) -> Result<Option<String>, StoreError> {
        let leases = self.leases()?;
        Ok(live_lease_of(&leases, container, name)
            .map(|lease| lease.name.clone().unwrap_or_default()))
    }

    /// Container `container`'s live lease here, if it has one: by its ID,
    /// or by `name` if it's given (as [`name_of`](Self::name_of)).
    ///
    /// # Errors
    ///
    /// The leases couldn't be read.
    pub fn live_lease(
        &self,
        container: &str,
        name: Option<&str>,
    ) -> Result<Option<Lease>, StoreError> {
        let leases = self.leases()?;
        Ok(live_lease_of(&leases, container, name).cloned())
    }

    /// Removes container `container`'s live lease here (by its ID, or by
    /// `name` if it's given, as [`name_of`](Self::name_of)), if it has one,
    /// and returns it: what it made (its veth, its rules) is the caller's to
    /// undo. Its holder (the container's monitor) keeps its
    /// lock on the file, which no longer has a name: the lease is gone for
    /// everyone else at once (its address, its names), and its holder's
    /// copy goes when it lets go of it. A lock can't be taken from another
    /// process (`flock` is its open file's, TLPI §55.2): unlinking the file
    /// is what's left to do.
    ///
    /// # Errors
    ///
    /// The leases couldn't be read, or the file removed.
    pub fn remove_live(
        &self,
        container: &str,
        name: Option<&str>,
    ) -> Result<Option<Lease>, StoreError> {
        let leases = self.leases()?;
        let Some(mine) = live_lease_of(&leases, container, name) else {
            return Ok(None);
        };
        let path = self.dir.join("leases").join(mine.ip.to_string());
        fs::remove_file(&path).map_err(|source| StoreError::Io {
            context: format!("removing {}", path.display()),
            source,
        })?;
        Ok(Some(mine.clone()))
    }

    /// Whether container `container` (found by its ID, or by `name` if it's
    /// given, as [`name_of`](Self::name_of)) could be renamed `new` on the
    /// network: `Ok(true)` if it has a live lease here and no other live
    /// container answers to `new` (its own names may be taken again);
    /// `Ok(false)` if it has no live lease here.
    ///
    /// # Errors
    ///
    /// [`StoreError::NameTaken`]; the leases couldn't be read.
    pub fn check_rename(
        &self,
        container: &str,
        name: Option<&str>,
        new: &str,
    ) -> Result<bool, StoreError> {
        let leases = self.leases()?;
        let Some(mine) = live_lease_of(&leases, container, name) else {
            return Ok(false);
        };
        self.refuse_taken_name(&leases, new, Some(mine.ip))?;
        Ok(true)
    }

    /// Renames container `container` (found by its ID, or by `name` if it's
    /// given, as [`name_of`](Self::name_of)) `new` on the network: its live
    /// lease rewritten **in place**, the name checked as
    /// [`check_rename`](Self::check_rename) does. `Ok(false)`: no live lease
    /// of it here (or it went meanwhile, its container ending).
    ///
    /// In place, because its run holds the lease file's inode locked (its
    /// liveness): a file renamed over it would be a new one, unlocked, that
    /// the next sweep removes. Only the lease's `name` (and `container`,
    /// for one made before leases knew their container) is changed, in its
    /// JSON as written: fields a newer Shipyard added survive. The new text
    /// is padded with spaces (JSON's whitespace) to at least the old length
    /// and written from the start, under a write lock on the file's bytes
    /// (an open file description lock, `F_OFD_SETLKW`: apart from the run's
    /// `flock`, which says it's alive) that every reader of a lease takes
    /// to read it (`read_lease`): no reader ever sees half of each.
    ///
    /// # Errors
    ///
    /// [`StoreError::NameTaken`]; the lease couldn't be read or written.
    pub fn rename(
        &self,
        container: &str,
        name: Option<&str>,
        new: &str,
    ) -> Result<bool, StoreError> {
        let leases = self.leases()?;
        let Some(mine) = live_lease_of(&leases, container, name) else {
            return Ok(false);
        };
        self.refuse_taken_name(&leases, new, Some(mine.ip))?;
        let path = self.dir.join("leases").join(mine.ip.to_string());
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(LEASE_FLAGS)
            .open(&path);
        let file = match opened {
            Ok(file) => file,
            // Released meanwhile: its container has ended.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(StoreError::io(format!("opening {}", path.display()), error));
            }
        };
        let failed = |what: &str, error: io::Error| {
            StoreError::io(format!("{what} {}", path.display()), error)
        };
        if !file
            .metadata()
            .map_err(|error| failed("checking", error))?
            .is_file()
        {
            return Ok(false);
        }
        // A range lock is Linux's alone (elsewhere, nothing): so is renaming
        // a running container, the one rewrite of a lease.
        shipyard_sys::lock::byte_range(&file, shipyard_sys::lock::ByteRange::Write)
            .map_err(|error| failed("locking", error))?;
        let text = shipyard_sys::fs::read_bounded_from(&file, LEASE_MAX)
            .map_err(|error| failed("reading", error))?;
        let mut json: serde_json::Value = serde_json::from_slice(&text).map_err(|error| {
            StoreError::io(
                format!("reading {}", path.display()),
                io::Error::other(error),
            )
        })?;
        // Anything but an object (an array, say: serde reads a struct from
        // one) is refused, never indexed into (F61: that panics).
        let Some(object) = json.as_object_mut() else {
            return Err(StoreError::io(
                format!("reading {}", path.display()),
                io::Error::other("not a JSON object"),
            ));
        };
        object.insert("name".to_owned(), serde_json::Value::from(new));
        object.insert("container".to_owned(), serde_json::Value::from(container));
        let mut written = serde_json::to_vec(&json)
            .map_err(|error| StoreError::io("writing the lease", io::Error::other(error)))?;
        if (written.len() as u64) < (text.len() as u64) {
            written.resize(text.len(), b' ');
        }
        {
            use std::os::unix::fs::FileExt as _;
            file.write_all_at(&written, 0)
                .map_err(|error| failed("writing", error))?;
        }
        file.sync_all().map_err(|error| failed("syncing", error))?;
        Ok(true)
    }

    /// A lease for `request`: the address asked for, or the next free one
    /// after the last handed out (round robin, so a freed address isn't
    /// reused at once). Sweep first: a dead run's address is free.
    ///
    /// The names the container would answer to (its name, aliases, short ID
    /// and run's ID) must not be any live container's on the network.
    pub fn allocate(&self, request: &LeaseRequest) -> Result<HeldLease, StoreError> {
        let dir = self.dir.join("leases");
        fs::create_dir_all(&dir)
            .map_err(|error| StoreError::io(format!("making {}", dir.display()), error))?;
        // Readable by the networks' DNS servers, which aren't root, and by
        // no one else (whatever the umask, and for a network made before).
        share_with_servers(&self.dir, 0o750)?;
        share_with_servers(&dir, 0o750)?;
        share_with_servers(&self.dir.join("config.json"), 0o640)?;
        let subnet = self.config.subnet;
        let leases = self.leases()?;
        for name in request.names() {
            self.refuse_taken_name(&leases, name, None)?;
        }
        let taken6: Vec<Ipv6Addr> = leases.iter().filter_map(|(lease, _)| lease.ip6).collect();
        let taken: Vec<IpAddr> = leases.into_iter().map(|(lease, _)| lease.ip).collect();
        let ip = if let Some(ip) = request.ip {
            check_host_address(subnet, self.config.gateway, ip)?;
            if taken.contains(&ip) {
                return Err(StoreError::Address(format!(
                    "{ip} is taken on the network {}",
                    self.config.name
                )));
            }
            ip
        } else {
            let last = self.read_next();
            next_free(subnet, self.config.gateway, &taken, last)
                .ok_or_else(|| StoreError::Full(self.config.name.clone()))?
        };
        let ip6 = self.address6(request.ip6, offset(subnet, ip), &taken6)?;
        let lease = Lease {
            ip,
            ip6,
            mac: request.mac.unwrap_or_else(|| mac_for(ip)),
            run: request.run.clone(),
            hostname: request.hostname.clone(),
            host_veth: request.host_veth.clone(),
            ports: request.ports.clone(),
            name: request.name.clone(),
            aliases: request.aliases.clone(),
            interface: request.interface.clone(),
            container: request.container.clone(),
        };
        let path = dir.join(ip.to_string());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o640)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| StoreError::io(format!("making {}", path.display()), error))?;
        let mut lock = file;
        lock.try_lock().map_err(|error| {
            StoreError::io(
                format!("locking {}", path.display()),
                io::Error::from(error),
            )
        })?;
        let json = serde_json::to_vec(&lease)
            .map_err(|error| StoreError::io("writing the lease", io::Error::other(error)))?;
        let written = {
            use std::os::unix::fs::PermissionsExt as _;
            lock.set_permissions(fs::Permissions::from_mode(0o640))
                .and_then(|()| servers_group(&lock))
                .and_then(|()| lock.write_all(&json))
                .and_then(|()| lock.sync_all())
        };
        if let Err(error) = written {
            let _ = fs::remove_file(&path);
            return Err(StoreError::io(format!("writing {}", path.display()), error));
        }
        if request.ip.is_none() {
            // Best effort: only the round robin's place.
            let _ = write_atomically(&self.dir.join("next"), ip.to_string().as_bytes());
        }
        Ok(HeldLease {
            lease,
            path,
            lock: shipyard_sys::lock::HeldFile::new(lock),
        })
    }

    /// The IPv6 address of a container whose IPv4 address is `offset` into
    /// the subnet: `asked` (`--ip6`), checked; else the one as far into the
    /// IPv6 prefix (unique as the IPv4 one is, with no state of its own),
    /// or the next free after it if one was asked for by another. `None`
    /// on a network without IPv6.
    fn address6(
        &self,
        asked: Option<Ipv6Addr>,
        offset: u128,
        taken: &[Ipv6Addr],
    ) -> Result<Option<Ipv6Addr>, StoreError> {
        let name = &self.config.name;
        let (Some(subnet6), Some(gateway6)) = (self.config.subnet6, self.config.gateway6) else {
            if asked.is_some() {
                return Err(StoreError::Address(format!(
                    "--ip6: the network {name} has no IPv6 (make one with --ipv6)"
                )));
            }
            return Ok(None);
        };
        if let Some(ip6) = asked {
            check_host_address(subnet6, IpAddr::V6(gateway6), IpAddr::V6(ip6))?;
            if taken.contains(&ip6) {
                return Err(StoreError::Address(format!(
                    "{ip6} is taken on the network {name}"
                )));
            }
            return Ok(Some(ip6));
        }
        (offset..offset.saturating_add(MAX_NEXT6))
            .filter_map(|n| match subnet6.nth(n) {
                Some(IpAddr::V6(ip6)) if n != 0 => Some(ip6),
                _ => None,
            })
            .find(|ip6| *ip6 != gateway6 && !taken.contains(ip6))
            .map(Some)
            .ok_or_else(|| StoreError::Full(name.clone()))
    }

    /// The last address handed out, if it's recorded.
    fn read_next(&self) -> Option<IpAddr> {
        read_text(&self.dir.join("next"), LINE_MAX)
            .ok()
            .and_then(|text| IpAddr::from_str(text.trim()).ok())
    }
}

/// A lease a run holds: its address is the run's until it's released, or the
/// run ends (or, given up to a container's monitor, until every copy of its
/// [`hold`](Self::hold) is closed).
#[derive(Debug)]
pub struct HeldLease {
    lease: Lease,
    path: PathBuf,
    /// The lease file, locked (`flock`, exclusive), to hand over.
    lock: shipyard_sys::lock::HeldFile,
}

impl HeldLease {
    /// The lease.
    #[must_use]
    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    /// The locked lease file, for a process that keeps the lease (a
    /// container's monitor), once this value is [given up](Self::give_up).
    #[must_use]
    pub fn hold(&self) -> std::os::fd::BorrowedFd<'_> {
        self.lock.hold()
    }

    /// Gives the lease up to whoever holds a copy of its
    /// [`hold`](Self::hold): this value's copy is closed, never unlocked, so
    /// the lease stays the run's while any copy is open, and the network's
    /// sweep reclaims it once none is.
    pub fn give_up(mut self) {
        self.lock.give_up();
    }

    /// Gives the address back: the file goes while it's still locked, so no
    /// one can take a lease that's half gone.
    pub fn release(self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The lease files in `dir` (none if it isn't there yet).
fn lease_paths(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let context = || format!("reading {}", dir.display());
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StoreError::io(context(), error)),
    };
    entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| StoreError::io(context(), error))
        })
        .collect()
}

/// The leases in `dir`, in address order, and whether each one's run is
/// alive ([`is_held`]). A lease too new to have its contents yet is a run
/// being set up, and is left out.
fn read_leases(dir: &Path) -> Result<Vec<(Lease, bool)>, StoreError> {
    let mut leases = Vec::new();
    for path in lease_paths(dir)? {
        if let Some((Some(lease), file)) = open_lease(&path)? {
            leases.push((lease, is_held(file)));
        }
    }
    leases.sort_by_key(|(lease, _)| ip_order(lease.ip));
    Ok(leases)
}

/// Container `container`'s live lease in `leases`: the one that says it's
/// its; else, if a `name` is given (the caller's for a container whose
/// leases were all made before leases knew their container), the one with
/// that name that says no container (unique on a network among live
/// leases). Without a name, never another's: a run with no record may have
/// the container's name.
fn live_lease_of<'a>(
    leases: &'a [(Lease, bool)],
    container: &str,
    name: Option<&str>,
) -> Option<&'a Lease> {
    let live = || {
        leases
            .iter()
            .filter(|(_, live)| *live)
            .map(|(lease, _)| lease)
    };
    live()
        .find(|lease| lease.container.as_deref() == Some(container))
        .or_else(|| {
            let name = name?;
            live().find(|lease| lease.container.is_none() && lease.name.as_deref() == Some(name))
        })
}

/// Whether a run holds the lease open as `file`: its exclusive lock is the
/// proof, so a shared one can't be had (tried without waiting, and let go
/// at once).
fn is_held(file: File) -> bool {
    Flock::lock(file, FlockArg::LockSharedNonblock).is_err()
}

/// The flags a lease file is opened with to be read: never through a
/// symbolic link, and never waiting (on a FIFO nothing writes).
const LEASE_FLAGS: libc::c_int = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// Opens a lease file: the open file (to try its lock), and the lease if
/// it reads as one: `None` for one still being written (empty), or one
/// that isn't a lease. `None` altogether if it's gone, or isn't a file.
fn open_lease(path: &Path) -> Result<Option<(Option<Lease>, File)>, StoreError> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(LEASE_FLAGS)
        .open(path);
    read_lease(opened, path)
}

/// [`open_lease`]'s reading, of the file `opened` at `path`.
fn read_lease(
    opened: io::Result<File>,
    path: &Path,
) -> Result<Option<(Option<Lease>, File)>, StoreError> {
    let file = match opened {
        Ok(file) => file,
        // Gone, a symbolic link (`O_NOFOLLOW`), or a FIFO nothing writes.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(StoreError::io(format!("opening {}", path.display()), error));
        }
    };
    let is_file = file
        .metadata()
        .map_err(|error| StoreError::io(format!("checking {}", path.display()), error))?
        .is_file();
    if !is_file {
        return Ok(None);
    }
    // Never half of a rename's rewrite (`LockedNetwork::rename`); a lock
    // that can't be had (an old kernel) reads as before.
    let _ = shipyard_sys::lock::byte_range(&file, shipyard_sys::lock::ByteRange::Read);
    // One too large to be a lease reads as one that isn't.
    let lease = match shipyard_sys::fs::read_bounded_from(&file, LEASE_MAX) {
        Ok(text) => serde_json::from_slice(&text).ok(),
        Err(_) => None,
    };
    let _ = shipyard_sys::lock::byte_range(&file, shipyard_sys::lock::ByteRange::Off);
    Ok(Some((lease, file)))
}

/// A network's leases directory, open: its live leases can be read through
/// it whoever this process is by then (the network's DNS server opens it as
/// root, then gives root up; the directories above it may be closed to
/// others).
#[derive(Debug)]
pub struct LeaseDir {
    dir: std::os::fd::OwnedFd,
    /// Its path, for messages.
    path: PathBuf,
}

/// A leases directory's version ([`LeaseDir::version`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeasesVersion(u64, i64, i64);

/// The networks' directory, open: a network's DNS server reads the other
/// networks' configs and leases through it (those its askers are on too),
/// whoever it is by then (see [`LeaseDir`]).
#[derive(Debug)]
pub struct NetworksDir {
    dir: std::os::fd::OwnedFd,
    /// Its path, for messages.
    path: PathBuf,
}

impl NetworksDir {
    /// Opens `networks`' directory.
    pub fn open(networks: &Networks) -> Result<NetworksDir, StoreError> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&networks.dir)
            .map_err(|error| {
                StoreError::io(format!("opening {}", networks.dir.display()), error)
            })?;
        Ok(NetworksDir {
            dir: dir.into(),
            path: networks.dir.clone(),
        })
    }

    /// The networks' names (every entry that can be one), sorted.
    pub fn names(&self) -> Result<Vec<String>, StoreError> {
        use nix::dir::Dir;
        use nix::fcntl::OFlag;
        use nix::sys::stat::Mode;
        let context = || format!("reading {}", self.path.display());
        let mut listing = Dir::openat(
            &self.dir,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| StoreError::io(context(), io::Error::from(errno)))?;
        let mut names = Vec::new();
        for entry in listing.iter() {
            let entry = entry.map_err(|errno| StoreError::io(context(), io::Error::from(errno)))?;
            if let Ok(name) = entry.file_name().to_str()
                && validate_name(name).is_ok()
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// The network `name`'s config, if it's there and reads as one.
    #[must_use]
    pub fn config(&self, name: &str) -> Option<NetworkConfig> {
        validate_name(name).ok()?;
        let relative = format!("{name}/config.json");
        let file = self.open_at(&relative, libc::O_RDONLY).ok()?;
        let text = shipyard_sys::fs::read_bounded_from(&File::from(file), CONFIG_MAX).ok()?;
        let text = String::from_utf8(text).ok()?;
        read_config(name, &text, self.path.join(relative)).ok()
    }

    /// The network `name`'s leases directory.
    pub fn leases(&self, name: &str) -> Result<LeaseDir, StoreError> {
        validate_name(name)?;
        let relative = format!("{name}/leases");
        let path = self.path.join(&relative);
        let dir = self
            .open_at(&relative, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|errno| {
                StoreError::io(
                    format!("opening {}", path.display()),
                    io::Error::from(errno),
                )
            })?;
        Ok(LeaseDir { dir, path })
    }

    /// Opens `relative` (a checked name, and a file of it) in the directory,
    /// never through a symbolic link at its end.
    fn open_at(&self, relative: &str, flags: libc::c_int) -> Result<std::os::fd::OwnedFd, Errno> {
        nix::fcntl::openat(
            &self.dir,
            relative,
            nix::fcntl::OFlag::from_bits_retain(
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            ),
            nix::sys::stat::Mode::empty(),
        )
    }
}

impl LeaseDir {
    /// Opens the network `name`'s leases directory, in `networks`.
    pub fn open(networks: &Networks, name: &str) -> Result<LeaseDir, StoreError> {
        validate_name(name)?;
        let path = networks.dir.join(name).join("leases");
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| StoreError::io(format!("opening {}", path.display()), error))?;
        Ok(LeaseDir {
            dir: dir.into(),
            path,
        })
    }

    /// What changes whenever a lease is made or removed, so a reader can
    /// tell its last [`live`](Self::live) is out of date: the directory's
    /// inode and modification time. `None` if it can't be read.
    #[must_use]
    pub fn version(&self) -> Option<LeasesVersion> {
        let stat = nix::sys::stat::fstat(&self.dir).ok()?;
        #[allow(
            clippy::useless_conversion,
            reason = "the stat field types differ between platforms"
        )]
        Some(LeasesVersion(
            u64::from(stat.st_ino),
            i64::from(stat.st_mtime),
            i64::from(stat.st_mtime_nsec),
        ))
    }

    /// The live leases (none once the directory is gone: the network was
    /// removed). An entry that can't be read is left out.
    pub fn live(&self) -> Result<Vec<Lease>, StoreError> {
        self.read_live().map(|read| read.leases)
    }

    /// The live leases, and whether the read is settled: false if a lease
    /// was still being written (made, locked, not filled in yet), which a
    /// moment later would be among them. A reader keeping what it read for
    /// a while (the DNS server) keeps only a settled read.
    pub fn read_live(&self) -> Result<LiveLeases, StoreError> {
        use nix::dir::Dir;
        use nix::fcntl::{OFlag, openat};
        use nix::sys::stat::Mode;
        let context = || format!("reading {}", self.path.display());
        let listing = Dir::openat(
            &self.dir,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        );
        let mut listing = match listing {
            Ok(listing) => listing,
            Err(Errno::ENOENT) => return Ok(LiveLeases::settled()),
            Err(errno) => return Err(StoreError::io(context(), io::Error::from(errno))),
        };
        let mut leases = Vec::new();
        let mut settled = true;
        for entry in listing.iter() {
            let entry = match entry {
                Ok(entry) => entry,
                // Removed while it was open (the network went): a removed
                // directory lists nothing.
                Err(Errno::ENOENT) => return Ok(LiveLeases::settled()),
                Err(errno) => return Err(StoreError::io(context(), io::Error::from(errno))),
            };
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let opened = openat(
                &self.dir,
                name,
                OFlag::from_bits_retain(libc::O_RDONLY | LEASE_FLAGS),
                Mode::empty(),
            )
            .map(File::from)
            .map_err(io::Error::from);
            let path = self.path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            // One this process can't open (a lease an older shipyard made
            // readable by root alone, something else put there) is skipped:
            // it mustn't hide every other container's.
            if let Ok(Some((lease, file))) = read_lease(opened, &path)
                && is_held(file)
            {
                match lease {
                    Some(lease) => leases.push(lease),
                    // Locked and empty: its run is writing it.
                    None => settled = false,
                }
            }
        }
        leases.sort_by_key(|lease| ip_order(lease.ip));
        Ok(LiveLeases { leases, settled })
    }
}

/// What [`LeaseDir::read_live`] read.
#[derive(Debug, Default)]
pub struct LiveLeases {
    /// The live leases.
    pub leases: Vec<Lease>,
    /// No lease was still being written.
    pub settled: bool,
}

impl LiveLeases {
    /// None, for certain.
    fn settled() -> Self {
        LiveLeases {
            leases: Vec::new(),
            settled: true,
        }
    }
}

/// The group the networks' DNS servers run as (`nobody`'s: see
/// `dns::server`). Besides root, it alone may read the networks' files: a
/// user who could open a lease could also lock it, and keep it live
/// forever.
pub const SERVERS_GROUP: u32 = 65_534;

/// Sets `path`'s permission bits to `mode`, for root and the DNS servers'
/// group ([`SERVERS_GROUP`]) alone.
fn share_with_servers(path: &Path, mode: u32) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .and_then(|()| File::open(path))
        .and_then(|file| servers_group(&file))
        .map_err(|error| StoreError::io(format!("setting {}'s mode", path.display()), error))
}

/// Gives `file` to the DNS servers' group, as root (the servers are only
/// root's). Not where that group isn't mapped (root in a user namespace,
/// whose networks have no servers).
fn servers_group(file: &File) -> io::Result<()> {
    if !nix::unistd::geteuid().is_root() {
        return Ok(());
    }
    match std::os::unix::fs::fchown(file, None, Some(SERVERS_GROUP)) {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

/// Makes the lock file `path` if it isn't there.
fn make_lock_file(path: &Path) -> Result<(), StoreError> {
    shipyard_sys::fs::open_lock_file(path)
        .map(drop)
        .map_err(|error| StoreError::io(format!("making {}", path.display()), error))
}

/// An exclusive `flock` on the lock file `path` (of the network `name`),
/// waiting up to [`LOCK_WAIT`], on the file that's at `path` once it's held.
fn lock_path(path: &Path, name: &str) -> Result<Flock<File>, StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => StoreError::NotFound(name.to_owned()),
                _ => StoreError::io(format!("opening {}", path.display()), error),
            })?;
        let lock = shipyard_sys::lock::lock_until(
            file,
            shipyard_sys::lock::Mode::Exclusive,
            deadline,
            Duration::from_millis(20),
        )
        .map_err(|errno| match errno {
            Errno::EWOULDBLOCK => StoreError::Busy(name.to_owned()),
            errno => StoreError::io(
                format!("locking {}", path.display()),
                io::Error::from(errno),
            ),
        })?;
        let held = lock
            .metadata()
            .map_err(|error| StoreError::io(format!("checking {}", path.display()), error))?;
        match fs::symlink_metadata(path) {
            Ok(now) if now.dev() == held.dev() && now.ino() == held.ino() => return Ok(lock),
            // Made again since: lock the new one.
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(name.to_owned()));
            }
            Err(error) => {
                return Err(StoreError::io(
                    format!("checking {}", path.display()),
                    error,
                ));
            }
        }
    }
}

/// A network's ID, labels and options (Docker's API's): kept in its
/// `labels.json` (step 11.2c2b), never read by the networking itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkMeta {
    /// Its ID, made at its making (64 hex): none for a network made before.
    pub id: Option<String>,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// Its driver options, as given (checked by the caller).
    pub options: BTreeMap<String, String>,
}

impl NetworkMeta {
    /// Whether it has nothing to keep.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.labels.is_empty() && self.options.is_empty()
    }
}

/// A network's labels and options, beside its config.
const META_FILE: &str = "labels.json";
/// The newest `labels.json` this Shipyard reads and writes.
const META_VERSION: u32 = 1;
/// The most a `labels.json` holds: written no bigger, read no further.
const META_MAX: u64 = 64 * 1024;

/// `labels.json`: its version, the labels, the options.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaFile {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    options: BTreeMap<String, String>,
}

/// Writes `meta` as the network `name`'s `labels.json` in `dir`: none if
/// it's empty (any a create cut short left removed).
fn write_meta(name: &str, dir: &Path, meta: &NetworkMeta) -> Result<(), StoreError> {
    let path = dir.join(META_FILE);
    if meta.is_empty() {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(StoreError::io(
                format!("removing {}", path.display()),
                error,
            )),
            _ => Ok(()),
        };
    }
    let file = MetaFile {
        version: META_VERSION,
        id: meta.id.clone(),
        labels: meta.labels.clone(),
        options: meta.options.clone(),
    };
    let json = serde_json::to_vec(&file)
        .map_err(|error| StoreError::io("writing the network's labels", io::Error::other(error)))?;
    if u64::try_from(json.len()).is_ok_and(|len| len > META_MAX) {
        return Err(StoreError::TooManyLabels(name.to_owned()));
    }
    // Root's alone: nothing of the networking (its DNS servers) reads it.
    shipyard_sys::fs::write_atomically(&path, &json, &shipyard_sys::fs::Write::new(0o600))
        .map_err(|error| StoreError::io(format!("writing {}", path.display()), error.into()))
}

/// Writes `bytes` to `path` through a temporary file and a rename, synced:
/// a reader sees the old file or the new one, never half of one.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    // Readable by the networks' DNS servers and no one else, whatever the
    // umask.
    let readable_by_servers = |file: &File| {
        shipyard_sys::fs::set_mode(file, 0o640)?;
        servers_group(file)
    };
    let how = shipyard_sys::fs::Write::new(0o640).prepared_by(&readable_by_servers);
    shipyard_sys::fs::write_atomically(path, bytes, &how)
        .map_err(|error| StoreError::io(format!("writing {}", path.display()), error.into()))
}

/// The network `name`'s config, from `text` (its `config.json`, at `path`),
/// checked: the bridge comes from the name, never from the file, so a config
/// edited to name another device (`docker0`) can't make shipyard adopt it,
/// or delete it with the network.
fn read_config(name: &str, text: &str, path: PathBuf) -> Result<NetworkConfig, StoreError> {
    let config: NetworkConfig =
        serde_json::from_str(text).map_err(|error| StoreError::Corrupt {
            path: path.clone(),
            reason: error.to_string(),
        })?;
    let reason = if config.name != name {
        Some(format!("it's the network {}'s", config.name))
    } else if config.bridge != bridge_name(name) {
        Some(format!("its bridge isn't {}", bridge_name(name)))
    } else if !config.subnet.is_ipv4()
        || config.subnet != config.subnet.network()
        || !config.subnet.contains(config.gateway)
    {
        Some("its subnet or gateway isn't one shipyard makes".to_owned())
    } else if config.subnet6.is_some() != config.gateway6.is_some()
        || config.subnet6.is_some_and(|subnet6| {
            subnet6.is_ipv4()
                || subnet6 != subnet6.network()
                || subnet6.prefix() > 112
                || !config
                    .gateway6
                    .is_some_and(|gateway6| subnet6.contains(IpAddr::V6(gateway6)))
        })
    {
        Some("its IPv6 prefix or gateway isn't one shipyard makes".to_owned())
    } else {
        None
    };
    match reason {
        Some(reason) => Err(StoreError::Corrupt { path, reason }),
        None => Ok(config),
    }
}

/// Whether the network named `name` has a DNS server, and its containers
/// names: every network but the default one (as Docker's).
#[must_use]
pub fn has_dns(name: &str) -> bool {
    name != DEFAULT_NETWORK
}

/// Checks a network's name.
pub fn validate_name(name: &str) -> Result<(), StoreError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidName(name.to_owned()))
    }
}

/// The bridge device of the network `name`: `shipyard0` for the default
/// network, else `shy-` and 10 hex characters of a hash of the name (an
/// interface name is at most 15 bytes; a network's is up to 64).
#[must_use]
pub fn bridge_name(name: &str) -> String {
    if name == DEFAULT_NETWORK {
        return DEFAULT_BRIDGE.to_owned();
    }
    format!("{DEVICE_PREFIX}-{:010x}", fnv1a(name.as_bytes()) >> 24)
}

/// FNV-1a, 64 bits: a stable hash, small enough to have here.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The hardware address for `ip`: `02:42` (locally administered, unicast)
/// and the address's last four bytes, as Docker does. So an address handed
/// out again comes with the same hardware address, and never meets another
/// one in a neighbour's ARP cache.
#[must_use]
pub fn mac_for(ip: IpAddr) -> [u8; 6] {
    let octets = match ip {
        IpAddr::V4(ip) => ip.octets(),
        IpAddr::V6(ip) => {
            let [.., a, b, c, d] = ip.octets();
            [a, b, c, d]
        }
    };
    let [a, b, c, d] = octets;
    [0x02, 0x42, a, b, c, d]
}

/// A hardware address written `aa:bb:cc:dd:ee:ff`, checked to be one a
/// container can use: unicast, and locally administered (so it can't be a
/// real card's).
pub fn parse_mac(text: &str) -> Result<[u8; 6], String> {
    let invalid = || format!("{text:?} isn't a hardware address (like 02:42:0a:58:00:02)");
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 6 {
        return Err(invalid());
    }
    let mut mac = [0u8; 6];
    for (byte, part) in mac.iter_mut().zip(&parts) {
        if part.len() != 2 {
            return Err(invalid());
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| invalid())?;
    }
    if mac[0] & 1 != 0 {
        return Err(format!(
            "{text}: a multicast address can't be a container's"
        ));
    }
    if mac[0] & 2 == 0 {
        return Err(format!(
            "{text}: not a locally administered address (the second-lowest bit of the first byte \
             set, like 02:…), so it could be a real card's"
        ));
    }
    Ok(mac)
}

/// A hardware address as `aa:bb:cc:dd:ee:ff`.
#[must_use]
pub fn format_mac(mac: [u8; 6]) -> String {
    mac.map(|byte| format!("{byte:02x}")).join(":")
}

/// `subnet` if it's an IPv4 network that overlaps nothing in `taken`.
fn check_subnet(subnet: IpNet, taken: &[IpNet]) -> Result<IpNet, StoreError> {
    if !subnet.is_ipv4() {
        return Err(StoreError::Address(format!(
            "{subnet} isn't an IPv4 subnet"
        )));
    }
    if subnet.prefix() > 30 {
        return Err(StoreError::Address(format!(
            "{subnet} is too small: a network needs a gateway and an address (a /30 or larger)"
        )));
    }
    let network = subnet.network();
    if let Some(other) = taken.iter().find(|other| other.overlaps(&network)) {
        return Err(StoreError::Address(format!(
            "{network} overlaps {other}, which is in use on this host"
        )));
    }
    Ok(network)
}

/// `subnet` if it's an IPv6 prefix a network can have (a /112 or larger;
/// not loopback, link-local or multicast) that overlaps nothing in `taken`.
/// (It must also be as large as the IPv4 subnet: checked by the caller.)
fn check_subnet6(subnet: IpNet, taken: &[IpNet]) -> Result<IpNet, StoreError> {
    let IpAddr::V6(address) = subnet.addr() else {
        return Err(StoreError::Address(format!(
            "{subnet} isn't an IPv6 prefix"
        )));
    };
    if subnet.prefix() > 112 {
        return Err(StoreError::Address(format!(
            "{subnet} is too small: an IPv6 network needs a /112 or larger (a /64 is usual)"
        )));
    }
    if address.is_loopback()
        || address.is_multicast()
        || address.is_unspecified()
        || address.is_unicast_link_local()
    {
        return Err(StoreError::Address(format!(
            "{subnet} can't be a network's: it's loopback, link-local or multicast"
        )));
    }
    let network = subnet.network();
    if let Some(other) = taken.iter().find(|other| other.overlaps(&network)) {
        return Err(StoreError::Address(format!(
            "{network} overlaps {other}, which is in use on this host"
        )));
    }
    Ok(network)
}

/// A random ULA /64 (RFC 4193: `fd`, a 40-bit global ID, a 16-bit subnet
/// ID) that overlaps nothing in `taken`.
fn random_ula(taken: &[IpNet]) -> Result<IpNet, StoreError> {
    for _ in 0..16 {
        let random = shipyard_sys::random::bytes::<7>()
            .map_err(|error| StoreError::io("reading /dev/urandom", error))?;
        let prefix = ula(random);
        if !taken.iter().any(|other| other.overlaps(&prefix)) {
            return Ok(prefix);
        }
    }
    Err(StoreError::Address(
        "no free IPv6 prefix found: give one (--subnet)".to_owned(),
    ))
}

/// The ULA /64 `fd` followed by `random`'s seven bytes.
fn ula(random: [u8; 7]) -> IpNet {
    let mut octets = [0u8; 16];
    octets[0] = 0xfd;
    for (to, from) in octets.iter_mut().skip(1).zip(random) {
        *to = from;
    }
    IpNet::new(IpAddr::V6(Ipv6Addr::from(octets)), 64)
        .unwrap_or_else(|_| IpNet::any(crate::Family::V6))
}

/// `gateway` (or the prefix's first host address, `::1`) if it's an
/// address a host of the IPv6 prefix `subnet` can have.
fn check_gateway6(subnet: IpNet, gateway: Option<Ipv6Addr>) -> Result<Ipv6Addr, StoreError> {
    match check_gateway(subnet, gateway.map(IpAddr::V6))? {
        IpAddr::V6(gateway) => Ok(gateway),
        IpAddr::V4(gateway) => Err(StoreError::Address(format!(
            "{gateway} isn't an IPv6 gateway"
        ))),
    }
}

/// `gateway` (or the subnet's first address) if it's an address a host of
/// the subnet can have.
fn check_gateway(subnet: IpNet, gateway: Option<IpAddr>) -> Result<IpAddr, StoreError> {
    let gateway = match gateway {
        Some(gateway) => gateway,
        // A /30 or larger has a first address.
        None => subnet.nth(1).unwrap_or(subnet.addr()),
    };
    if !subnet.contains(gateway) || is_network_or_broadcast(subnet, gateway) {
        return Err(StoreError::Address(format!(
            "the gateway {gateway} must be a host address of {subnet} (not its network or \
             broadcast address)"
        )));
    }
    Ok(gateway)
}

/// Checks that `ip` is an address a container on `subnet` can have: in it,
/// and not its network address, its broadcast address or the gateway.
fn check_host_address(subnet: IpNet, gateway: IpAddr, ip: IpAddr) -> Result<(), StoreError> {
    if !subnet.contains(ip) || is_network_or_broadcast(subnet, ip) || ip == gateway {
        return Err(StoreError::Address(format!(
            "{ip} can't be a container's address on {subnet} (gateway {gateway})"
        )));
    }
    Ok(())
}

/// Whether `ip` is `subnet`'s network or broadcast address: no host's.
fn is_network_or_broadcast(subnet: IpNet, ip: IpAddr) -> bool {
    ip == subnet.network().addr() || subnet.broadcast().map(IpAddr::V4) == Some(ip)
}

/// The first free host address of `subnet` after `last` (wrapping around),
/// skipping the gateway and the addresses in `taken`.
fn next_free(
    subnet: IpNet,
    gateway: IpAddr,
    taken: &[IpAddr],
    last: Option<IpAddr>,
) -> Option<IpAddr> {
    let size = subnet.size();
    // Host addresses are 1..size-1 (0: the network; size-1: broadcast).
    let hosts = size.checked_sub(2)?;
    let start = last
        .filter(|last| subnet.contains(*last))
        .map_or(0, |last| offset(subnet, last));
    (1..=hosts)
        .map(|step| (start + step - 1) % hosts + 1)
        .filter_map(|n| subnet.nth(n))
        .find(|ip| *ip != gateway && !taken.contains(ip))
}

/// How far `ip` is from the start of `subnet`.
fn offset(subnet: IpNet, ip: IpAddr) -> u128 {
    ip_order(ip).saturating_sub(ip_order(subnet.network().addr()))
}

/// An address as a number: a sort key, and for distances.
fn ip_order(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(ip) => u128::from(u32::from(ip)),
        IpAddr::V6(ip) => u128::from(ip),
    }
}

/// A free subnet: for the default network, a /16 from 10.88.0.0 to
/// 10.99.0.0; for others, a /24 from 10.89.0.0 on. Free: it overlaps none of
/// `taken` (the host's routes, other networks).
#[must_use]
pub fn choose_subnet(default: bool, taken: &[IpNet]) -> Option<IpNet> {
    let free = |net: IpNet| !taken.iter().any(|other| other.overlaps(&net));
    let net = |a: u8, b: u8, c: u8, prefix: u8| {
        IpNet::new(IpAddr::V4(Ipv4Addr::new(10, a, b, c)), prefix).ok()
    };
    if default {
        (88..=99)
            .filter_map(|a| net(a, 0, 0, 16))
            .find(|n| free(*n))
    } else {
        (89..=255u8)
            .flat_map(|a| (0..=255u8).map(move |b| (a, b)))
            .filter_map(|(a, b)| net(a, b, 0, 24))
            .find(|n| free(*n))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn net(text: &str) -> IpNet {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// A temporary directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "shipyard-networks-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn spec(name: &str) -> NetworkSpec {
        NetworkSpec {
            name: name.into(),
            ..NetworkSpec::default()
        }
    }

    fn request(run: &str) -> LeaseRequest {
        LeaseRequest {
            run: run.into(),
            hostname: run.into(),
            host_veth: format!("shy{run}"),
            ip: None,
            mac: None,
            ports: Vec::new(),
            name: None,
            aliases: Vec::new(),
            interface: "eth0".into(),
            ip6: None,
            container: None,
        }
    }

    /// A container renamed on a network: its live lease rewritten in place
    /// (the same file, still held by its run), with the new name and none
    /// of the old; a name another live container answers to (its name, an
    /// alias, its run's ID, in any case) refused, its own allowed; a
    /// shorter name still reads as a lease (the padding); a container with
    /// no lease there, nothing done.
    #[test]
    fn a_lease_is_renamed_in_place() {
        use std::os::unix::fs::MetadataExt as _;
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let mine = network
            .allocate(&LeaseRequest {
                name: Some("a-rather-long-name".into()),
                container: Some("c1".into()),
                ..request("a1")
            })
            .unwrap();
        let other = network
            .allocate(&LeaseRequest {
                name: Some("db".into()),
                aliases: vec!["database".into()],
                ..request("b1")
            })
            .unwrap();
        let inode = fs::metadata(&mine.path).unwrap().ino();
        for taken in ["db", "DATABASE", "b1"] {
            assert!(
                matches!(
                    network.rename("c1", Some("a-rather-long-name"), taken),
                    Err(StoreError::NameTaken { .. })
                ),
                "{taken}"
            );
        }
        assert!(
            network
                .rename("c1", Some("a-rather-long-name"), "w")
                .unwrap()
        );
        assert_eq!(fs::metadata(&mine.path).unwrap().ino(), inode, "in place");
        let (lease, live) = network
            .leases()
            .unwrap()
            .into_iter()
            .find(|(lease, _)| lease.container.as_deref() == Some("c1"))
            .unwrap();
        assert!(live, "still held by its run");
        assert_eq!(lease.name.as_deref(), Some("w"));
        assert!(!lease.names().any(|name| name == "a-rather-long-name"));
        // Its own names may be taken again: its run's ID, in another case.
        assert!(network.rename("c1", Some("w"), "A1").unwrap());
        assert!(!network.rename("nobody", Some("nobody"), "x").unwrap());
        drop((mine, other));
    }

    /// A lease made before leases knew their container is found by its
    /// container's name, and told its container as it's renamed; what the
    /// lease has that this Shipyard doesn't know (a newer one's field)
    /// survives the rewrite.
    #[test]
    fn a_lease_rename_keeps_what_it_doesnt_know() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let held = network
            .allocate(&LeaseRequest {
                name: Some("old".into()),
                ..request("a1")
            })
            .unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&held.path).unwrap()).unwrap();
        json["future"] = serde_json::Value::from(7);
        // Rewritten where it is (the run's lock is on this file).
        {
            use std::os::unix::fs::FileExt as _;
            let file = OpenOptions::new().write(true).open(&held.path).unwrap();
            file.write_all_at(&serde_json::to_vec(&json).unwrap(), 0)
                .unwrap();
        }
        assert!(network.rename("c9", Some("old"), "new-and-longer").unwrap());
        let text = fs::read_to_string(&held.path).unwrap();
        let renamed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(renamed["name"], "new-and-longer");
        assert_eq!(renamed["container"], "c9");
        assert_eq!(renamed["future"], 7, "{text}");
        drop(held);
    }

    /// A lease that's a JSON array (serde reads a lease from one, in its
    /// fields' order) is refused by a rename, not panicked on (F61), and
    /// left as it was; read the same, it's found (the control: the rename
    /// reached it).
    #[test]
    fn a_lease_that_isnt_an_object_is_refused_by_a_rename() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let held = network
            .allocate(&LeaseRequest {
                name: Some("old".into()),
                ..request("a1")
            })
            .unwrap();
        let object: serde_json::Value =
            serde_json::from_slice(&fs::read(&held.path).unwrap()).unwrap();
        let array = serde_json::json!([
            object["ip"],
            object["mac"],
            object["run"],
            object["hostname"],
            object["host_veth"],
            [],
            "old"
        ]);
        let text = serde_json::to_vec(&array).unwrap();
        assert!(
            serde_json::from_slice::<Lease>(&text).is_ok(),
            "read as a lease, as `rename` finds it"
        );
        {
            use std::os::unix::fs::FileExt as _;
            let file = OpenOptions::new()
                .write(true)
                .truncate(false)
                .open(&held.path)
                .unwrap();
            file.set_len(0).unwrap();
            file.write_all_at(&text, 0).unwrap();
        }
        let refused = network.rename("c9", Some("old"), "new");
        assert!(
            format!("{refused:?}").contains("not a JSON object"),
            "refused for being one: {refused:?}"
        );
        assert_eq!(fs::read(&held.path).unwrap(), text, "left as it was");
        drop(held);
    }

    /// A network made with labels and options keeps them in its
    /// `labels.json`, read back as written; one without (made by the CLI,
    /// or before 11.2c2b) has none, and no file.
    #[test]
    fn a_network_keeps_its_labels_and_options() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let meta = NetworkMeta {
            id: Some("ab".repeat(32)),
            labels: BTreeMap::from([("com.docker.compose.project".into(), "app".into())]),
            options: BTreeMap::from([("com.docker.network.driver.mtu".into(), "1500".into())]),
        };
        networks.create_with(&spec("app"), &[], &meta).unwrap();
        assert_eq!(networks.meta("app").unwrap(), meta);
        let written = fs::read_to_string(scratch.0.join("app").join(META_FILE)).unwrap();
        assert!(written.starts_with(r#"{"version":1,"#), "{written}");
        networks.create(&spec("plain"), &[]).unwrap();
        assert_eq!(networks.meta("plain").unwrap(), NetworkMeta::default());
        assert!(!scratch.0.join("plain").join(META_FILE).exists());
    }

    /// A `labels.json` of a newer version, or not one, is refused; one
    /// too big is never written, and nothing of the network is left.
    #[test]
    fn network_labels_that_cant_be_kept_are_refused() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let path = scratch.0.join("app").join(META_FILE);
        for bad in [r#"{"version":2,"labels":{}}"#, "nope", r#"{"labels":{}}"#] {
            fs::write(&path, bad).unwrap();
            assert!(
                matches!(networks.meta("app"), Err(StoreError::Labels { .. })),
                "{bad}"
            );
        }
        let big = NetworkMeta {
            labels: BTreeMap::from([("big".into(), "x".repeat(70 * 1024))]),
            ..NetworkMeta::default()
        };
        assert!(matches!(
            networks.create_with(&spec("huge"), &[], &big),
            Err(StoreError::TooManyLabels(_))
        ));
        assert!(networks.get("huge").unwrap().is_none());
        assert!(!scratch.0.join("huge").exists());
        // Never through a symlink.
        fs::remove_file(&path).unwrap();
        fs::write(
            scratch.0.join("elsewhere"),
            r#"{"version":1,"labels":{"a":"b"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(scratch.0.join("elsewhere"), &path).unwrap();
        assert!(matches!(networks.meta("app"), Err(StoreError::Io { .. })));
    }

    /// A create cut short (labels written, no config) is taken over: its
    /// labels go unless the new create gives its own.
    #[test]
    fn a_cut_short_create_s_labels_are_not_kept() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        fs::create_dir(scratch.0.join("app")).unwrap();
        fs::write(
            scratch.0.join("app").join(META_FILE),
            r#"{"version":1,"labels":{"stale":"yes"}}"#,
        )
        .unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        assert_eq!(networks.meta("app").unwrap(), NetworkMeta::default());
    }

    /// A container's live lease is the one that says its ID, before one of
    /// its name that says no container (a run with no record named so);
    /// that one is found by its name only when the caller gives it (all the
    /// container's leases older than leases' containers), never by its ID.
    #[test]
    fn a_lease_by_id_comes_first_and_by_name_only_when_asked() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let recordless = network
            .allocate(&LeaseRequest {
                name: Some("web".into()),
                ..request("a1")
            })
            .unwrap();
        // The container has none here: by its ID alone, nothing.
        assert_eq!(network.live_lease("c1", None).unwrap(), None);
        assert_eq!(network.name_of("c1", None).unwrap(), None);
        assert!(!network.check_rename("c1", None, "new").unwrap());
        assert_eq!(network.remove_live("c1", None).unwrap(), None);
        // Asked by its name too: the old kind of lease, found.
        assert_eq!(
            network
                .live_lease("c1", Some("web"))
                .unwrap()
                .map(|lease| lease.ip),
            Some(recordless.lease().ip)
        );
        drop(recordless);
        // Its own, beside one of its name that says no container: its own.
        let other = network
            .allocate(&LeaseRequest {
                name: Some("web".into()),
                ..request("b1")
            })
            .unwrap();
        let mine = network
            .allocate(&LeaseRequest {
                name: Some("web2".into()),
                container: Some("c1".into()),
                ..request("c1")
            })
            .unwrap();
        assert_eq!(
            network
                .live_lease("c1", Some("web"))
                .unwrap()
                .map(|lease| lease.ip),
            Some(mine.lease().ip)
        );
        drop((other, mine));
    }

    /// A lease written before leases knew their container still reads.
    #[test]
    fn a_lease_without_its_container_reads() {
        let old: Lease = serde_json::from_str(
            r#"{"ip":"10.89.0.9","mac":[2,66,0,0,0,9],"run":"r","hostname":"r","host_veth":"shyr"}"#,
        )
        .unwrap();
        assert_eq!(old.container, None);
    }

    #[test]
    fn names_follow_the_volume_rules() {
        for good in ["bridge", "app", "my_net.1", "a-b", "0"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", "-x", ".x", "a/b", "a b", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bridge_names_fit_an_interface() {
        assert_eq!(bridge_name("bridge"), "shipyard0");
        let name = bridge_name("app");
        assert_eq!(name.len(), 14);
        assert!(name.starts_with("shy-"));
        assert_eq!(name, bridge_name("app"));
        assert_ne!(name, bridge_name("app2"));
        assert!(crate::route::validate_name(&bridge_name(&"y".repeat(64))).is_ok());
    }

    #[test]
    fn a_subnet_is_chosen_free() {
        assert_eq!(choose_subnet(true, &[]), Some(net("10.88.0.0/16")));
        assert_eq!(
            choose_subnet(true, &[net("10.88.1.0/24")]),
            Some(net("10.89.0.0/16"))
        );
        assert_eq!(choose_subnet(false, &[]), Some(net("10.89.0.0/24")));
        assert_eq!(
            choose_subnet(false, &[net("10.89.0.0/24"), net("10.0.0.0/16")]),
            Some(net("10.89.1.0/24"))
        );
        // A default route overlaps everything: it isn't counted (the caller
        // leaves it out); a /8 route takes the whole pool.
        assert_eq!(choose_subnet(false, &[net("10.0.0.0/8")]), None);
    }

    #[test]
    fn subnets_and_gateways_are_checked() {
        assert!(check_subnet(net("10.88.0.0/16"), &[net("10.88.5.0/24")]).is_err());
        assert!(check_subnet(net("fd00::/64"), &[]).is_err());
        assert!(check_subnet(net("10.1.1.0/31"), &[]).is_err());
        assert_eq!(
            check_subnet(net("10.1.1.7/24"), &[]).unwrap(),
            net("10.1.1.0/24")
        );
        let subnet = net("10.1.1.0/24");
        assert_eq!(check_gateway(subnet, None).unwrap(), ip("10.1.1.1"));
        assert_eq!(
            check_gateway(subnet, Some(ip("10.1.1.254"))).unwrap(),
            ip("10.1.1.254")
        );
        for bad in ["10.1.1.0", "10.1.1.255", "10.1.2.1"] {
            assert!(check_gateway(subnet, Some(ip(bad))).is_err(), "{bad}");
        }
    }

    #[test]
    fn addresses_go_round_the_subnet() {
        let subnet = net("10.1.1.0/29"); // hosts .1 to .6
        let gateway = ip("10.1.1.1");
        assert_eq!(next_free(subnet, gateway, &[], None), Some(ip("10.1.1.2")));
        assert_eq!(
            next_free(subnet, gateway, &[], Some(ip("10.1.1.2"))),
            Some(ip("10.1.1.3"))
        );
        // Round robin: after .6 comes .2 again (.1 is the gateway).
        assert_eq!(
            next_free(subnet, gateway, &[], Some(ip("10.1.1.6"))),
            Some(ip("10.1.1.2"))
        );
        let taken = [ip("10.1.1.3"), ip("10.1.1.4")];
        assert_eq!(
            next_free(subnet, gateway, &taken, Some(ip("10.1.1.2"))),
            Some(ip("10.1.1.5"))
        );
        let full = [
            ip("10.1.1.2"),
            ip("10.1.1.3"),
            ip("10.1.1.4"),
            ip("10.1.1.5"),
            ip("10.1.1.6"),
        ];
        assert_eq!(next_free(subnet, gateway, &full, None), None);
    }

    #[test]
    fn a_requested_address_must_be_a_hosts() {
        let subnet = net("10.1.1.0/24");
        let gateway = ip("10.1.1.1");
        assert!(check_host_address(subnet, gateway, ip("10.1.1.9")).is_ok());
        for bad in ["10.1.1.0", "10.1.1.1", "10.1.1.255", "10.1.2.9"] {
            assert!(
                check_host_address(subnet, gateway, ip(bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn hardware_addresses() {
        assert_eq!(mac_for(ip("10.88.0.2")), [0x02, 0x42, 10, 88, 0, 2]);
        assert_eq!(format_mac(mac_for(ip("10.88.0.2"))), "02:42:0a:58:00:02");
        assert_eq!(
            parse_mac("02:42:0a:58:00:02"),
            Ok([0x02, 0x42, 10, 88, 0, 2])
        );
        for bad in [
            "02:42:0a:58:00",
            "02:42:0a:58:00:2",
            "zz:42:0a:58:00:02",
            "03:42:0a:58:00:02",
            "00:42:0a:58:00:02",
        ] {
            assert!(parse_mac(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_network_is_created_listed_and_removed() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let made = networks
            .create(&spec("app"), &[net("10.89.0.0/24")])
            .unwrap();
        assert_eq!(made.subnet, net("10.89.1.0/24"));
        assert_eq!(made.gateway, ip("10.89.1.1"));
        assert_eq!(networks.get("app").unwrap(), Some(made.clone()));
        assert!(matches!(
            networks.create(&spec("app"), &[]),
            Err(StoreError::Exists(_))
        ));
        // Another network's subnet is taken too.
        let second = networks
            .create(&spec("db"), &[net("10.89.0.0/24")])
            .unwrap();
        assert_eq!(second.subnet, net("10.89.2.0/24"));
        let names: Vec<String> = networks
            .list()
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(names, ["app", "db"]);
        networks.remove(networks.lock("app").unwrap()).unwrap();
        assert_eq!(networks.get("app").unwrap(), None);
        assert!(matches!(networks.lock("app"), Err(StoreError::NotFound(_))));
    }

    /// A lease handed over (a copy of its hold, as a container's monitor
    /// keeps) stays the run's once its holder has gone, and goes to the
    /// sweep once the copy is closed: dropping the holder only closes, never
    /// unlocks.
    #[test]
    fn a_lease_handed_over_outlives_its_holder() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let lease = network.allocate(&request("a")).unwrap();
        let copy = lease.hold().try_clone_to_owned().unwrap();
        lease.give_up();
        assert!(network.sweep().unwrap().is_empty(), "held by the copy");
        drop(copy);
        // Free once the copy is closed: once, too, any stray copy another
        // test's child holds between its fork and its exec is (AGENTS §10).
        let began = std::time::Instant::now();
        let swept = loop {
            let swept = network.sweep().unwrap();
            if !swept.is_empty() || began.elapsed() > std::time::Duration::from_secs(10) {
                break swept;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(swept.len(), 1, "free once the copy is closed");
        assert_eq!(swept[0].run, "a");
    }

    /// A lease dropped (not given up) is free at once, even while a stray
    /// copy of its lock's descriptor is open (another thread's child,
    /// between its fork and its exec): it's unlocked, not only closed.
    #[test]
    fn a_dropped_lease_is_free_despite_a_stray_copy() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let lease = network.allocate(&request("a")).unwrap();
        let stray = lease.hold().try_clone_to_owned().unwrap();
        drop(lease);
        assert_eq!(network.sweep().unwrap().len(), 1, "free despite the copy");
        drop(stray);
    }

    #[test]
    fn leases_are_held_released_and_swept() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let first = network.allocate(&request("a")).unwrap();
        assert_eq!(first.lease().ip, ip("10.89.0.2"));
        assert_eq!(first.lease().mac, mac_for(ip("10.89.0.2")));
        let second = network.allocate(&request("b")).unwrap();
        assert_eq!(second.lease().ip, ip("10.89.0.3"));
        // Both alive: nothing to sweep, and the network can't go.
        assert!(network.sweep().unwrap().is_empty());
        assert_eq!(
            network
                .leases()
                .unwrap()
                .iter()
                .filter(|(_, live)| *live)
                .count(),
            2
        );
        // Released: its address is free, but the next one goes on (round
        // robin).
        first.release();
        let third = network.allocate(&request("c")).unwrap();
        assert_eq!(third.lease().ip, ip("10.89.0.4"));
        // A run that died: its lock is gone, its file stays, until a sweep.
        drop(second);
        let swept = network.sweep().unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].run, "b");
        drop(network);
        assert!(matches!(
            networks.remove(networks.lock("app").unwrap()),
            Err(StoreError::InUse { users, .. }) if users == ["c"]
        ));
        third.release();
        networks.remove(networks.lock("app").unwrap()).unwrap();
    }

    /// A container's leases are read without making the store, and a
    /// network that can't be read is left out, never hiding the others (a
    /// view of it). The control: its lease on the good network is found.
    #[test]
    fn a_containers_leases_are_read_past_a_broken_network() {
        let scratch = Scratch::new();
        let missing = scratch.0.join("none");
        assert!(Networks::existing(&missing).is_none());
        assert!(!missing.exists(), "never made");
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        networks.create(&spec("broken"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let mut asked = request("a");
        asked.container = Some("c".repeat(64));
        let held = network.allocate(&asked).unwrap();
        drop(network);
        fs::write(scratch.0.join("broken").join("config.json"), b"{not json").unwrap();
        let existing = Networks::existing(&scratch.0).unwrap();
        let found = existing.leases_of(&"c".repeat(64));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].0.name.as_str(), found[0].1.ip),
            ("app", held.lease().ip)
        );
        assert!(existing.leases_of(&"d".repeat(64)).is_empty());
        held.release();
    }

    /// Every container's leases read at once are what each one's own read
    /// finds: on two networks, past a broken one, in the networks' order;
    /// a container without a lease isn't there.
    #[test]
    fn leases_by_container_are_each_ones_leases() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let mut held = Vec::new();
        for name in ["zeta", "alpha", "broken"] {
            networks.create(&spec(name), &[]).unwrap();
        }
        for (network, container) in [("zeta", "c"), ("alpha", "c"), ("alpha", "e")] {
            let locked = networks.lock(network).unwrap();
            let mut asked = request(container);
            asked.container = Some(container.repeat(64));
            held.push(locked.allocate(&asked).unwrap());
        }
        fs::write(scratch.0.join("broken").join("config.json"), b"{not json").unwrap();
        let existing = Networks::existing(&scratch.0).unwrap();
        let all = existing.live_leases_by_container();
        assert_eq!(all.len(), 2, "{all:?}");
        for container in ["c", "e"] {
            let id = container.repeat(64);
            assert_eq!(all[&id], existing.leases_of(&id), "{container}");
        }
        let names: Vec<&str> = all[&"c".repeat(64)]
            .iter()
            .map(|(config, _)| config.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(!all.contains_key(&"d".repeat(64)));
        for lease in held {
            lease.release();
        }
    }

    /// An in-use message names the containers, five at most.
    #[test]
    fn in_use_names_its_containers() {
        let users = |count: usize| (1..=count).map(|n| format!("c{n}")).collect::<Vec<_>>();
        let said = |count| {
            StoreError::InUse {
                name: "app".into(),
                users: users(count),
            }
            .to_string()
        };
        assert_eq!(said(2), "the network app is in use by c1, c2");
        assert_eq!(said(5), "the network app is in use by c1, c2, c3, c4, c5");
        assert_eq!(
            said(8),
            "the network app is in use by c1, c2, c3, c4, c5 and 3 more"
        );
    }

    #[test]
    fn an_ipv6_prefix_is_a_ula_64_or_one_given() {
        let prefix = ula([1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(prefix, net("fd01:203:405:607::/64"));
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let config = networks
            .create(
                &NetworkSpec {
                    ipv6: true,
                    ..spec("app")
                },
                &[],
            )
            .unwrap();
        let subnet6 = config.subnet6.unwrap();
        assert_eq!(subnet6.prefix(), 64);
        assert!(matches!(subnet6.addr(), IpAddr::V6(ip) if ip.octets()[0] == 0xfd));
        assert_eq!(
            config.gateway6_net().unwrap().addr(),
            subnet6.nth(1).unwrap()
        );
        // Read back as it was written.
        assert_eq!(networks.get("app").unwrap(), Some(config.clone()));
        // A prefix given: checked, and not another's.
        let given = |subnet6: &str| {
            networks.create(
                &NetworkSpec {
                    subnet6: Some(net(subnet6)),
                    ..spec("other")
                },
                &[],
            )
        };
        for bad in [
            "::1/128",
            "fe80::/64",
            "ff02::/64",
            "fd00::/120",
            "10.0.0.0/24",
        ] {
            assert!(matches!(given(bad), Err(StoreError::Address(_))), "{bad}");
        }
        assert!(matches!(
            given(&subnet6.to_string()),
            Err(StoreError::Address(_))
        ));
        assert!(given("fd00:1::/64").unwrap().subnet6.is_some());
        // None without --ipv6.
        assert_eq!(networks.create(&spec("plain"), &[]).unwrap().subnet6, None);
    }

    #[test]
    fn an_ipv6_prefix_and_gateway_are_checked() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let create = |name: &str, spec: NetworkSpec| {
            networks.create(
                &NetworkSpec {
                    name: name.into(),
                    ..spec
                },
                &[],
            )
        };
        // A gateway that's the prefix's network address, or outside it.
        for gateway in ["fd00:5::", "fd00:6::1"] {
            let made = create(
                "a",
                NetworkSpec {
                    subnet6: Some(net("fd00:5::/64")),
                    gateway6: Some(gateway.parse().unwrap()),
                    ..NetworkSpec::default()
                },
            );
            assert!(matches!(made, Err(StoreError::Address(_))), "{gateway}");
        }
        // A gateway without IPv6.
        let made = create(
            "b",
            NetworkSpec {
                gateway6: Some("fd00:5::1".parse().unwrap()),
                ..NetworkSpec::default()
            },
        );
        assert!(matches!(made, Err(StoreError::Address(_))));
        // A prefix smaller than the subnet: not an address for each.
        let made = create(
            "c",
            NetworkSpec {
                subnet: Some(net("10.80.0.0/15")),
                subnet6: Some(net("fd00:7::/112")),
                ..NetworkSpec::default()
            },
        );
        assert!(matches!(made, Err(StoreError::Address(_))));
        // A config edited to an IPv6 prefix that isn't one is refused.
        let config = create(
            "d",
            NetworkSpec {
                ipv6: true,
                ..NetworkSpec::default()
            },
        )
        .unwrap();
        let path = scratch.0.join("d/config.json");
        for edited in [
            NetworkConfig {
                gateway6: None,
                ..config.clone()
            },
            NetworkConfig {
                gateway6: Some("fd99::1".parse().unwrap()),
                ..config.clone()
            },
            NetworkConfig {
                subnet6: Some(net("10.9.0.0/24")),
                ..config.clone()
            },
        ] {
            fs::write(&path, serde_json::to_vec(&edited).unwrap()).unwrap();
            assert!(
                matches!(networks.get("d"), Err(StoreError::Corrupt { .. })),
                "{edited:?}"
            );
        }
    }

    #[test]
    fn a_stray_file_among_the_networks_is_no_network() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        fs::write(scratch.0.join("notes.txt"), "mine").unwrap();
        let names: Vec<String> = networks
            .list()
            .unwrap()
            .into_iter()
            .map(|config| config.name)
            .collect();
        assert_eq!(names, ["app"]);
    }

    /// The record of closed routing names every interface the host had, so
    /// a host with hundreds (another engine's veths) still reads as closed.
    #[test]
    fn routing_closed_on_a_host_of_many_interfaces_reads() {
        use crate::Family;
        if !Path::new("/proc/sys/kernel/random/boot_id").exists() {
            return;
        }
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let many: Vec<String> = (0..300).map(|n| format!("veth{n:011}")).collect();
        networks.close_routing(Family::V4, &many).unwrap();
        assert!(fs::metadata(scratch.0.join(ROUTING_CLOSED)).unwrap().len() > LINE_MAX);
        let closed = networks.routing_closed().closed;
        assert_eq!(
            (closed.families, closed.interfaces),
            (vec![Family::V4], many)
        );
    }

    #[test]
    fn routing_is_closed_per_family_and_for_this_boot_only() {
        use crate::Family;
        use crate::nft::ClosedRouting;
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let names = |names: &[&str]| -> Vec<String> { names.iter().map(|&n| n.into()).collect() };
        assert_eq!(networks.routing_closed().closed, ClosedRouting::default());
        networks
            .close_routing(Family::V6, &names(&["eth0", "wlan0"]))
            .unwrap();
        // Where the kernel says what boot this is (Linux), a file from
        // another boot is nothing.
        if Path::new("/proc/sys/kernel/random/boot_id").exists() {
            assert_eq!(
                networks.routing_closed().closed,
                ClosedRouting {
                    families: vec![Family::V6],
                    interfaces: names(&["eth0", "wlan0"]),
                }
            );
            // The interfaces of both closings, once each.
            networks
                .close_routing(Family::V4, &names(&["eth0", "eth1"]))
                .unwrap();
            assert_eq!(
                networks.routing_closed().closed,
                ClosedRouting {
                    families: vec![Family::V4, Family::V6],
                    interfaces: names(&["eth0", "eth1", "wlan0"]),
                }
            );
            fs::write(scratch.0.join(ROUTING_CLOSED6), "another-boot\nwlan0\n").unwrap();
            assert_eq!(
                networks.routing_closed().closed,
                ClosedRouting {
                    families: vec![Family::V4],
                    interfaces: names(&["eth0", "eth1"]),
                }
            );
        }
    }

    /// A record that's there but can't be read fails the reading, never
    /// reads as "not closed" (F65); a missing one is no closing, and a
    /// readable one this boot's is read (the positive control).
    #[test]
    fn routing_closed_fails_closed_on_a_record_it_cant_read() {
        use crate::Family;
        use std::os::unix::fs::PermissionsExt as _;
        if !Path::new("/proc/sys/kernel/random/boot_id").exists() {
            return;
        }
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        assert_eq!(networks.routing_closed().closed.families, vec![]);
        networks
            .close_routing(Family::V4, &["eth0".to_owned()])
            .unwrap();
        let record = scratch.0.join(ROUTING_CLOSED);
        assert_eq!(networks.routing_closed().closed.families, vec![Family::V4]);
        let fails = |what: &str| {
            let records = networks.routing_closed();
            assert!(
                matches!(
                    records.unread.as_slice(),
                    [(Family::V4, StoreError::Io { .. })]
                ),
                "{what} must be unread: {records:?}"
            );
            assert!(records.closed.families.is_empty(), "{what}: {records:?}");
        };
        // A symbolic link to a good record (read without following).
        let good = scratch.0.join("good");
        fs::rename(&record, &good).unwrap();
        std::os::unix::fs::symlink(&good, &record).unwrap();
        fails("a symbolic link");
        fs::remove_file(&record).unwrap();
        // Past its bound.
        let mut big = fs::read_to_string(&good).unwrap();
        big.push_str(&"x".repeat(usize::try_from(ROUTING_CLOSED_MAX).unwrap()));
        fs::write(&record, big).unwrap();
        fails("a record past its bound");
        fs::remove_file(&record).unwrap();
        // Not a regular file.
        fs::create_dir(&record).unwrap();
        fails("a directory");
        fs::remove_dir(&record).unwrap();
        // Not text.
        fs::write(&record, b"\xff\xfe\n").unwrap();
        fails("a record that isn't text");
        fs::remove_file(&record).unwrap();
        // A directory in its place can't be written over either: said.
        fs::create_dir(&record).unwrap();
        assert!(matches!(
            networks.close_routing(Family::V4, &["eth0".to_owned()]),
            Err(StoreError::Io { .. })
        ));
        fs::remove_dir(&record).unwrap();
        // One family's record unreadable keeps the other's.
        networks
            .close_routing(Family::V6, &["wlan0".to_owned()])
            .unwrap();
        fs::write(&record, b"\xff").unwrap();
        let records = networks.routing_closed();
        assert_eq!(records.closed.families, vec![Family::V6]);
        assert!(matches!(records.unread.as_slice(), [(Family::V4, _)]));
        fs::remove_file(&record).unwrap();
        fs::remove_file(scratch.0.join(ROUTING_CLOSED6)).unwrap();
        // Unreadable: only the mapping (the store is root's, and root reads
        // it all the same).
        fs::rename(&good, &record).unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o000)).unwrap();
        if nix::unistd::geteuid().is_root() {
            assert_eq!(networks.routing_closed().closed.families, vec![Family::V4]);
        } else {
            fails("an unreadable record");
        }
    }

    #[test]
    fn a_containers_ipv6_address_goes_with_its_ipv4_one() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks
            .create(
                &NetworkSpec {
                    subnet6: Some(net("fd00:1::/64")),
                    ..spec("app")
                },
                &[],
            )
            .unwrap();
        let network = networks.lock("app").unwrap();
        let first = network.allocate(&request("a")).unwrap();
        assert_eq!(first.lease().ip, ip("10.89.0.2"));
        assert_eq!(first.lease().ip6, Some("fd00:1::2".parse().unwrap()));
        // One asked for, then the one that would go with the next address.
        let asked = network
            .allocate(&LeaseRequest {
                ip6: Some("fd00:1::3".parse().unwrap()),
                ..request("b")
            })
            .unwrap();
        assert_eq!(asked.lease().ip, ip("10.89.0.3"));
        let next = network.allocate(&request("c")).unwrap();
        assert_eq!(next.lease().ip, ip("10.89.0.4"));
        assert_eq!(next.lease().ip6, Some("fd00:1::4".parse().unwrap()));
        // Taken, the gateway, outside the prefix.
        for bad in ["fd00:1::3", "fd00:1::1", "fd00:2::9"] {
            let refused = network.allocate(&LeaseRequest {
                ip6: Some(bad.parse().unwrap()),
                ..request("d")
            });
            assert!(matches!(refused, Err(StoreError::Address(_))), "{bad}");
        }
        drop((first, asked, next));
        // Without IPv6, none, and none can be asked for.
        networks.create(&spec("plain"), &[]).unwrap();
        let plain = networks.lock("plain").unwrap();
        assert_eq!(plain.allocate(&request("e")).unwrap().lease().ip6, None);
        assert!(
            plain
                .allocate(&LeaseRequest {
                    ip6: Some("fd00:1::9".parse().unwrap()),
                    ..request("f")
                })
                .is_err()
        );
    }

    #[test]
    fn a_port_goes_to_the_address_of_its_family() {
        use crate::ports::{Protocol, Published};
        let published = |host_ip: Option<&str>| Published {
            host_ip: host_ip.map(|ip| ip.parse().unwrap()),
            host_port: 8080,
            container_port: 80,
            protocol: Protocol::Tcp,
        };
        let lease = Lease {
            ip: ip("10.89.0.2"),
            mac: [2, 0x42, 10, 0x59, 0, 2],
            run: "a".into(),
            hostname: "a".into(),
            host_veth: "shya".into(),
            ports: vec![
                published(None),
                published(Some("127.0.0.1")),
                published(Some("2001:db8::1")),
            ],
            name: None,
            aliases: Vec::new(),
            interface: "eth0".into(),
            ip6: Some("fd00:1::2".parse().unwrap()),
            container: None,
        };
        let containers: Vec<IpAddr> = lease
            .port_rules()
            .iter()
            .map(|rule| rule.container)
            .collect();
        assert_eq!(
            containers,
            [
                ip("10.89.0.2"),
                ip("fd00:1::2"),
                ip("10.89.0.2"),
                ip("fd00:1::2")
            ]
        );
        let ipv4_only = Lease { ip6: None, ..lease };
        assert_eq!(ipv4_only.port_rules().len(), 2);
    }

    #[test]
    fn a_name_is_one_live_containers_on_a_network() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        networks.create(&spec("other"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let named = |run: &str, name: Option<&str>, aliases: &[&str]| LeaseRequest {
            name: name.map(str::to_owned),
            aliases: aliases.iter().map(|&alias| alias.to_owned()).collect(),
            ..request(run)
        };
        let db = network
            .allocate(&named("a1", Some("db"), &["database"]))
            .unwrap();
        assert_eq!(
            db.lease().names().collect::<Vec<_>>(),
            ["db", "database", "a1"]
        );
        // Its name, an alias, its run's ID: as a name or an alias, in any
        // case.
        for (name, aliases) in [
            (Some("DB"), &[][..]),
            (None, &["Database"][..]),
            (Some("web"), &["a1"][..]),
        ] {
            let taken = network.allocate(&named("b1", name, aliases));
            assert!(
                matches!(&taken, Err(StoreError::NameTaken { network, .. }) if network == "app"),
                "{taken:?}"
            );
        }
        // Another run's ID can't be taken either.
        assert!(matches!(
            network.allocate(&named("a1", None, &[])),
            Err(StoreError::NameTaken { .. })
        ));
        // On another network, it's free.
        let other = networks.lock("other").unwrap();
        let elsewhere = other.allocate(&named("c1", Some("db"), &[])).unwrap();
        // Once the container is gone (its run killed: the lease isn't
        // swept yet), it's free here too.
        drop(db);
        let again = network.allocate(&named("d1", Some("db"), &[])).unwrap();
        assert_eq!(again.lease().name.as_deref(), Some("db"));
        drop((elsewhere, again));
    }

    /// A container's live lease removed while its holder keeps the lock: the
    /// address is free at once (a new lease may take it), the holder's
    /// lock stays its own; none to remove is `None`.
    #[test]
    fn a_live_lease_is_removed_from_under_its_holder() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let held = network
            .allocate(&LeaseRequest {
                name: Some("web".to_owned()),
                container: Some("c".repeat(64)),
                ..request("a1")
            })
            .unwrap();
        let ip = held.lease().ip;
        let removed = network.remove_live(&"c".repeat(64), Some("web")).unwrap();
        assert_eq!(removed.map(|lease| lease.ip), Some(ip));
        assert!(network.leases().unwrap().is_empty(), "gone for everyone");
        assert!(
            network
                .remove_live(&"c".repeat(64), Some("web"))
                .unwrap()
                .is_none()
        );
        // Its address and names are free, its holder still holding its
        // (unnamed) file.
        let again = network
            .allocate(&LeaseRequest {
                ip: Some(ip),
                name: Some("web".to_owned()),
                ..request("b1")
            })
            .unwrap();
        assert_eq!(again.lease().ip, ip);
        drop((held, again));
    }

    /// A container's lease answers to its short ID (its hostname) too,
    /// after its name and aliases, and before its run's ID, and that name
    /// is its own on the network; its address's name is its name, else its
    /// short ID, else its run's ID.
    #[test]
    fn a_containers_short_id_is_one_of_its_names() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let id = "0123456789ab".to_owned() + &"c".repeat(52);
        let of = |run: &str, name: Option<&str>, container: Option<&str>| LeaseRequest {
            name: name.map(str::to_owned),
            aliases: vec!["api".to_owned()],
            container: container.map(str::to_owned),
            ..request(run)
        };
        let web = network.allocate(&of("a1", Some("web"), Some(&id))).unwrap();
        assert_eq!(
            web.lease().names().collect::<Vec<_>>(),
            ["web", "api", "0123456789ab", "a1"]
        );
        assert_eq!(web.lease().address_name(), "web");
        // Its short ID is taken, as a name or an alias, in any case.
        let taken = network.allocate(&LeaseRequest {
            aliases: Vec::new(),
            ..of("b1", Some("0123456789AB"), None)
        });
        assert!(
            matches!(taken, Err(StoreError::NameTaken { .. })),
            "{taken:?}"
        );
        drop(web);
        // Unnamed: its short ID names its address; without a container,
        // its run's ID does.
        let unnamed = network.allocate(&of("c1", None, Some(&id))).unwrap();
        assert_eq!(unnamed.lease().address_name(), "0123456789ab");
        drop(unnamed);
        let run = network.allocate(&of("d1", None, None)).unwrap();
        assert_eq!(run.lease().address_name(), "d1");
        assert_eq!(run.lease().names().collect::<Vec<_>>(), ["api", "d1"]);
        drop(run);
        // As an alias, it's taken too.
        let first = network.allocate(&of("e1", None, Some(&id))).unwrap();
        let alias = network.allocate(&LeaseRequest {
            aliases: vec!["0123456789ab".to_owned()],
            ..of("f1", None, None)
        });
        assert!(
            matches!(alias, Err(StoreError::NameTaken { .. })),
            "{alias:?}"
        );
        drop(first);
        // A container field too short to have a short ID (not one this
        // writes) gives none, and no panic.
        for odd in ["abc", "", "é"] {
            let lease = network.allocate(&of("g1", None, Some(odd))).unwrap();
            assert_eq!(lease.lease().address_name(), "g1", "{odd:?}");
            assert_eq!(lease.lease().names().collect::<Vec<_>>(), ["api", "g1"]);
        }
    }

    /// A lease still being written (made and locked, not filled in yet)
    /// makes a read unsettled: a moment later it would be among the live
    /// ones. Once it isn't held, it's nothing, and the read settled.
    #[test]
    fn a_lease_being_written_unsettles_a_read() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let live = network.allocate(&request("a")).unwrap();
        let leases = LeaseDir::open(&networks, "app").unwrap();
        let read = leases.read_live().unwrap();
        assert!(read.settled);
        assert_eq!(read.leases, vec![live.lease().clone()]);
        let writing = File::create(scratch.0.join("app/leases/10.89.0.9")).unwrap();
        let held = Flock::lock(writing, FlockArg::LockExclusiveNonblock).unwrap();
        let read = leases.read_live().unwrap();
        assert!(!read.settled);
        assert_eq!(read.leases, vec![live.lease().clone()]);
        drop(held);
        assert!(leases.read_live().unwrap().settled);
    }

    /// A lease larger than any Shipyard writes reads as no lease (one being
    /// written, or something else), never as an error that would hide the
    /// network's other leases.
    #[test]
    fn a_lease_over_its_bound_reads_as_none() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let live = network.allocate(&request("a")).unwrap();
        let path = scratch.0.join("app/leases/10.89.0.2");
        // The control: the lease as written reads.
        let (lease, _) = open_lease(&path).unwrap().unwrap();
        assert_eq!(lease.as_ref(), Some(live.lease()));
        let mut bloated = fs::read(&path).unwrap();
        bloated.resize(usize::try_from(LEASE_MAX).unwrap() + 1, b' ');
        fs::write(&path, &bloated).unwrap();
        let (lease, _) = open_lease(&path).unwrap().unwrap();
        assert_eq!(lease, None);
    }

    /// A config larger than any Shipyard writes is refused: an error, not
    /// a network made of part of it.
    #[test]
    fn a_config_over_its_bound_is_refused() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        assert!(networks.get("app").unwrap().is_some(), "the control");
        let path = scratch.0.join("app/config.json");
        let mut bloated = fs::read(&path).unwrap();
        bloated.resize(usize::try_from(CONFIG_MAX).unwrap() + 1, b' ');
        fs::write(&path, &bloated).unwrap();
        let err = networks.get("app").unwrap_err();
        let StoreError::Io { source, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(
            shipyard_sys::fs::refusal(source),
            Some(shipyard_sys::fs::Refusal::TooLarge { cap: CONFIG_MAX })
        );
        assert!(
            NetworksDir::open(&networks)
                .unwrap()
                .config("app")
                .is_none()
        );
    }

    /// A config is the DNS servers' to read from the moment it's written,
    /// not only once a lease is handed out (`allocate` shares it again).
    #[test]
    fn a_config_is_written_for_the_servers_to_read() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let config = fs::metadata(scratch.0.join("app/config.json")).unwrap();
        assert_eq!(config.permissions().mode() & 0o777, 0o640);
        if nix::unistd::geteuid().is_root() {
            assert_eq!(config.gid(), SERVERS_GROUP);
        }
    }

    #[test]
    fn a_lease_directory_lists_live_leases_through_its_descriptor() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let live = network.allocate(&request("a")).unwrap();
        let dead = network.allocate(&request("b")).unwrap();
        drop(dead);
        let leases = LeaseDir::open(&networks, "app").unwrap();
        assert_eq!(leases.live().unwrap(), vec![live.lease().clone()]);
        // Readable by the servers' group and no one else, whatever the umask.
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = scratch.0.join("app/leases");
        assert_eq!(mode(&dir), 0o750);
        assert_eq!(mode(&dir.join("10.89.0.2")), 0o640);
        assert_eq!(mode(&scratch.0), 0o750);
        assert_eq!(mode(&scratch.0.join("app")), 0o750);
        assert_eq!(mode(&scratch.0.join("app/config.json")), 0o640);
        if nix::unistd::geteuid().is_root() {
            use std::os::unix::fs::MetadataExt as _;
            let group = |path: &Path| fs::metadata(path).unwrap().gid();
            assert_eq!(group(&dir.join("10.89.0.2")), SERVERS_GROUP);
            assert_eq!(group(&scratch.0.join("app/config.json")), SERVERS_GROUP);
            assert_eq!(group(&dir), SERVERS_GROUP);
        }
        // Once the network's gone, there are none.
        live.release();
        drop(network);
        networks.remove(networks.lock("app").unwrap()).unwrap();
        assert!(leases.live().unwrap().is_empty());
    }

    #[test]
    fn a_dns_server_is_running_while_its_lock_is_held() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        assert!(!networks.dns_running("app"));
        let file = File::create(networks.dns_lock_path("app")).unwrap();
        assert!(!networks.dns_running("app"));
        let held = Flock::lock(file, FlockArg::LockExclusiveNonblock).unwrap();
        assert!(networks.dns_running("app"));
        drop(held);
        assert!(!networks.dns_running("app"));
    }

    #[test]
    fn a_lease_left_unreadable_is_swept_not_fatal() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let leases = scratch.0.join("app/leases");
        fs::create_dir_all(&leases).unwrap();
        // A run killed between making its lease and writing it, one cut
        // short, and something that isn't a lease at all.
        fs::write(leases.join("10.89.0.2"), "").unwrap();
        fs::write(leases.join("10.89.0.3"), "{\"ip\":").unwrap();
        fs::create_dir(leases.join("stray")).unwrap();
        let network = networks.lock("app").unwrap();
        // Listing skips them; sweeping removes the files no one holds.
        assert!(network.leases().unwrap().is_empty());
        assert!(network.sweep().unwrap().is_empty());
        assert!(!leases.join("10.89.0.2").exists());
        assert!(!leases.join("10.89.0.3").exists());
        // So the address is free again.
        let lease = network.allocate(&request("a")).unwrap();
        assert_eq!(lease.lease().ip, ip("10.89.0.2"));
        // A lease being made (empty, but held) is left alone.
        let held = Flock::lock(
            File::create(leases.join("10.89.0.9")).unwrap(),
            FlockArg::LockExclusiveNonblock,
        )
        .unwrap();
        network.sweep().unwrap();
        assert!(leases.join("10.89.0.9").exists());
        drop(held);
    }

    /// The options of later steps are written only when on, and read as
    /// off from a config written before them.
    #[test]
    fn options_are_kept_and_old_configs_read_them_off() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let limited = NetworkSpec {
            conntrack_limit: true,
            ..spec("limited")
        };
        let config = networks.create(&limited, &[]).unwrap();
        assert!(config.conntrack_limit);
        assert!(networks.get("limited").unwrap().unwrap().conntrack_limit);
        networks.create(&spec("plain"), &[]).unwrap();
        let written = fs::read_to_string(scratch.0.join("plain/config.json")).unwrap();
        assert!(!written.contains("conntrack_limit"), "{written}");
        assert!(!written.contains("isolate_host"), "{written}");
        // Read back from a config without them: off.
        let plain = networks.get("plain").unwrap().unwrap();
        assert!(!plain.conntrack_limit && !plain.isolate_host);
    }

    #[test]
    fn a_config_names_its_own_bridge() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let config = networks.create(&spec("app"), &[]).unwrap();
        let path = scratch.0.join("app/config.json");
        let edited = NetworkConfig {
            bridge: "docker0".into(),
            ..config.clone()
        };
        fs::write(&path, serde_json::to_vec(&edited).unwrap()).unwrap();
        assert!(matches!(
            networks.get("app"),
            Err(StoreError::Corrupt { .. })
        ));
        let elsewhere = NetworkConfig {
            name: "other".into(),
            ..config
        };
        fs::write(&path, serde_json::to_vec(&elsewhere).unwrap()).unwrap();
        assert!(matches!(
            networks.get("app"),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn creates_at_once_choose_different_subnets() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        let subnets: Vec<IpNet> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|n| {
                    let networks = &networks;
                    scope.spawn(move || networks.create(&spec(&format!("n{n}")), &[]).unwrap())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap().subnet)
                .collect()
        });
        for (i, a) in subnets.iter().enumerate() {
            for b in &subnets[i + 1..] {
                assert!(!a.overlaps(b), "{subnets:?}");
            }
        }
    }

    #[test]
    fn a_lock_on_a_removed_network_is_no_lock() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let locked = networks.lock("app").unwrap();
        // Waiting for the lock as the network goes: it finds it gone.
        let waiter = std::thread::scope(|scope| {
            let waiting = scope.spawn(|| networks.lock("app").map(|_| ()));
            std::thread::sleep(Duration::from_millis(100));
            networks.remove(locked).unwrap();
            waiting.join().unwrap()
        });
        assert!(matches!(waiter, Err(StoreError::NotFound(_))), "{waiter:?}");
        // A lock never makes one again.
        assert!(!scratch.0.join("app").exists());
    }

    #[test]
    fn a_lock_file_made_again_is_the_one_locked() {
        use std::os::unix::fs::MetadataExt as _;
        let scratch = Scratch::new();
        let path = scratch.0.join("lock");
        make_lock_file(&path).unwrap();
        let old = lock_path(&path, "app").unwrap();
        let locked = std::thread::scope(|scope| {
            let waiting = scope.spawn(|| lock_path(&path, "app").unwrap());
            std::thread::sleep(Duration::from_millis(100));
            // Removed and made again (as `network rm` then `create` do)
            // while the waiter waits on the old file.
            let new = scratch.0.join("lock.new");
            make_lock_file(&new).unwrap();
            fs::rename(&new, &path).unwrap();
            drop(old);
            waiting.join().unwrap()
        });
        assert_eq!(
            locked.metadata().unwrap().ino(),
            fs::metadata(&path).unwrap().ino()
        );
    }

    #[test]
    fn a_requested_address_must_be_free() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let network = networks.lock("app").unwrap();
        let mut asked = request("a");
        asked.ip = Some(ip("10.89.0.50"));
        let held = network.allocate(&asked).unwrap();
        assert_eq!(held.lease().ip, ip("10.89.0.50"));
        asked.run = "b".into();
        assert!(matches!(
            network.allocate(&asked),
            Err(StoreError::Address(_))
        ));
        asked.ip = Some(ip("10.89.0.1"));
        assert!(matches!(
            network.allocate(&asked),
            Err(StoreError::Address(_))
        ));
    }

    #[test]
    fn a_locked_network_makes_others_wait() {
        let scratch = Scratch::new();
        let networks = Networks::open(&scratch.0).unwrap();
        networks.create(&spec("app"), &[]).unwrap();
        let held = networks.lock("app").unwrap();
        let try_lock = || {
            let file = OpenOptions::new()
                .read(true)
                .open(scratch.0.join("app").join("lock"))
                .unwrap();
            shipyard_sys::lock::lock_until(
                file,
                shipyard_sys::lock::Mode::Exclusive,
                Instant::now() + Duration::from_millis(50),
                Duration::from_millis(20),
            )
        };
        assert_eq!(try_lock().unwrap_err(), Errno::EWOULDBLOCK);
        // The control: let go, it's had.
        drop(held);
        try_lock().unwrap();
    }
}
