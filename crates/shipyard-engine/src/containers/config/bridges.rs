//! The bridge networks a container's [`Config`] joins: read, and changed by
//! `network connect` and `disconnect`.
#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "network connect and disconnect are Linux's")
)]

use super::{Bridge, Config, Network};
use crate::error::{Result, bail};

impl Config {
    /// The bridge networks it joins, in order, by name: its default one
    /// (`default`) if it says none. `None` if its network isn't bridges
    /// (the host's, its own loopback only, or pasta's).
    pub(crate) fn bridges<'a>(&'a self, default: &'a str) -> Option<Vec<&'a str>> {
        if self.network.is_empty() {
            return Some(vec![default]);
        }
        let mut names = Vec::new();
        for network in &self.network {
            let Network::Bridges(bridges) = network else {
                return None;
            };
            names.extend(bridges.iter().map(|bridge| bridge.name.as_str()));
        }
        Some(names)
    }

    /// Whether it joins the bridge network `network` (its default one,
    /// `default`, if it says none).
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn joins(&self, network: &str, default: &str) -> bool {
        self.bridges(default)
            .is_some_and(|names| names.contains(&network))
    }

    /// Its settings on the bridge network `network` (its default one,
    /// `default`, if it says none), if it joins it.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn bridge(&self, network: &str, default: &str) -> Option<Bridge> {
        if self.network.is_empty() {
            return (network == default).then(|| Bridge::named(default));
        }
        self.network.iter().find_map(|joined| match joined {
            Network::Bridges(bridges) => bridges
                .iter()
                .find(|bridge| bridge.name == network)
                .cloned(),
            _ => None,
        })
    }

    /// The config with the bridge network `joined` joined too, last (`network
    /// connect`): its default network (`default`) said, if it said none, so
    /// it's still joined; and its addresses (`--ip`, `--ip6`,
    /// `--mac-address`, one network's) kept as its first network's own,
    /// where a run of several networks has them.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest): its
    /// network isn't bridges, it's on `joined` already, or it would be on
    /// more than 100 networks.
    pub(crate) fn with_bridge(&self, joined: Bridge, default: &str) -> Result<Self> {
        let Some(names) = self.bridges(default) else {
            bail!(
                InvalidRequest,
                "its network is the host's, its own loopback only, or pasta's: it can't join \
                 bridge networks"
            );
        };
        if names.contains(&joined.name.as_str()) {
            bail!(Exists, "it's on the network {} already", joined.name);
        }
        if names.len() >= crate::run::MAX_NETWORKS {
            bail!(
                InvalidRequest,
                "a container can join at most {} networks",
                crate::run::MAX_NETWORKS
            );
        }
        let mut config = self.clone();
        if config.network.is_empty() {
            config
                .network
                .push(Network::Bridges(vec![Bridge::named(default)]));
        }
        if let Some(Network::Bridges(bridges)) = config.network.first_mut()
            && let Some(first) = bridges.first_mut()
        {
            first.ip = first.ip.or(config.ip.take());
            first.ip6 = first.ip6.or(config.ip6.take());
            first.mac = first.mac.take().or(config.mac_address.take());
        }
        config.network.push(Network::Bridges(vec![joined]));
        Ok(config)
    }

    /// The config without the bridge network `left` (`network
    /// disconnect`).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound): it isn't on
    /// `left`; [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest):
    /// `left` is its only network (with none, it would join its default
    /// one at its next start), or its network isn't bridges.
    pub(crate) fn without_bridge(&self, left: &str, default: &str) -> Result<Self> {
        let Some(names) = self.bridges(default) else {
            bail!(
                InvalidRequest,
                "its network is the host's, its own loopback only, or pasta's"
            );
        };
        if !names.contains(&left) {
            bail!(NotFound, "it isn't on the network {left}");
        }
        if names.len() == 1 {
            bail!(InvalidRequest, "{left} is its only network");
        }
        let mut config = self.clone();
        for network in &mut config.network {
            if let Network::Bridges(bridges) = network {
                bridges.retain(|bridge| bridge.name != left);
            }
        }
        config
            .network
            .retain(|network| !matches!(network, Network::Bridges(bridges) if bridges.is_empty()));
        Ok(config)
    }
}
