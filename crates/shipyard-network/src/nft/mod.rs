//! `nf_tables`: the kernel's firewall, spoken over netlink.
//!
//! `nf_tables` is a small virtual machine in the kernel: rules are programs of
//! expressions (the module `expr`) the kernel runs on packets at netfilter's hooks.
//! Every change is a *batch*: requests between a begin and an end marker,
//! applied all together or not at all, so the firewall is never half
//! changed. Shipyard keeps one table of its own (the module `ruleset`) and changes
//! only the contents of its sets after it's made.
//!
//! The encoders work on any UNIX (and are tested there); [`Nftables`], which
//! talks to the kernel, is Linux's.

// Used by the Linux client, and tested everywhere.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the client is Linux's")
)]
pub(crate) mod consts;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the client is Linux's")
)]
mod decode;
mod expr;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the client is Linux's")
)]
mod objects;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the client is Linux's")
)]
mod ruleset;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the client is Linux's")
)]
mod spoofing;

pub use decode::Chain;
pub use objects::Element;
pub use ruleset::{ClosedRouting, NetworkRules, PortRule, TABLE};
pub use spoofing::PortGuard;

#[cfg(target_os = "linux")]
pub use linux::Nftables;

/// Shipyard's table, as found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ruleset {
    /// It isn't there (a reboot, `nft flush ruleset`, or never made).
    Missing,
    /// It's an older version's (or damaged).
    Outdated,
    /// It's this version's.
    Current,
}

impl Ruleset {
    /// A table with `userdata` and `rules` rules: this version's only with
    /// this version's user data *and* every rule (`nft flush table` keeps
    /// the table and its user data, and drops the rules).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "the client is Linux's")
    )]
    pub(crate) fn of(userdata: &[u8], rules: usize) -> Ruleset {
        Self::of_table(userdata, rules, ruleset::VERSION, ruleset::rules().len())
    }

    /// The same for the bridge table (`spoofing`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "the client is Linux's")
    )]
    pub(crate) fn of_bridge_table(userdata: &[u8], rules: usize) -> Ruleset {
        Self::of_table(userdata, rules, spoofing::VERSION, spoofing::rules().len())
    }

    /// Current only with `version` as its user data and all `expected`
    /// rules.
    fn of_table(userdata: &[u8], rules: usize, version: &[u8], expected: usize) -> Ruleset {
        if userdata == version && rules == expected {
            Ruleset::Current
        } else {
            Ruleset::Outdated
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, reason = "tests")]
mod linux_tests;

#[cfg(test)]
mod tests {
    use super::{Ruleset, ruleset};

    #[test]
    fn only_this_versions_whole_table_is_current() {
        let all = ruleset::rules().len();
        assert_eq!(Ruleset::of(ruleset::VERSION, all), Ruleset::Current);
        // An older version's, even whole.
        assert_eq!(Ruleset::of(b"shipyard ruleset 1", all), Ruleset::Outdated);
        // This version's, flushed (or with a rule gone).
        assert_eq!(Ruleset::of(ruleset::VERSION, 0), Ruleset::Outdated);
        assert_eq!(Ruleset::of(ruleset::VERSION, all - 1), Ruleset::Outdated);
        assert_eq!(Ruleset::of(b"", all), Ruleset::Outdated);
        // The bridge table's version is its own.
        let bridge = super::spoofing::rules().len();
        assert_eq!(
            Ruleset::of_bridge_table(super::spoofing::VERSION, bridge),
            Ruleset::Current
        );
        assert_eq!(
            Ruleset::of_bridge_table(ruleset::VERSION, bridge),
            Ruleset::Outdated
        );
        assert_eq!(
            Ruleset::of_bridge_table(super::spoofing::VERSION, bridge - 1),
            Ruleset::Outdated
        );
    }
}

/// What the fuzz target reads.
#[cfg(fuzzing)]
pub(crate) mod fuzzing {
    use crate::netlink::Message;

    /// `message` as a table, a chain and set elements.
    pub(crate) fn parse(message: &Message<'_>) {
        let _ = super::decode::table_userdata(message);
        let _ = super::decode::chain(message);
        let _ = super::decode::elements(message);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use nix::errno::Errno;

    use super::Ruleset;
    use super::consts::{
        NFNL_SUBSYS_NFTABLES, NFTA_SET_ELEM_LIST_SET, NFTA_SET_ELEM_LIST_TABLE, NFTA_TABLE_NAME,
    };
    use super::consts::{
        NFPROTO_UNSPEC, NFT_MSG_GETCHAIN, NFT_MSG_GETRULE, NFT_MSG_GETSETELEM, NFT_MSG_GETTABLE,
        NFTA_RULE_TABLE,
    };
    use super::decode::{self, Chain};
    use super::objects::{self, Element, delete_elements, delete_table};
    use super::ruleset::{self, ClosedRouting, FAMILY, NetworkRules, PortRule, TABLE};
    use super::spoofing::{self, PortGuard};
    use crate::netlink::{BatchItem, NetlinkError, Socket};

    /// A connection to `nf_tables`, in the calling thread's network
    /// namespace.
    #[derive(Debug)]
    pub struct Nftables {
        socket: Socket,
    }

    impl Nftables {
        /// A new connection.
        pub fn new() -> Result<Nftables, NetlinkError> {
            Ok(Nftables {
                socket: Socket::netfilter()?,
            })
        }

        /// Whether Shipyard's table is there, and this version's.
        pub fn ruleset(&mut self) -> Result<Ruleset, NetlinkError> {
            Ok(match self.table(FAMILY, TABLE)? {
                None => Ruleset::Missing,
                Some((userdata, rules)) => Ruleset::of(&userdata, rules),
            })
        }

        /// Whether Shipyard's bridge table (the module `spoofing`) is
        /// there, and this version's.
        pub fn bridge_ruleset(&mut self) -> Result<Ruleset, NetlinkError> {
            Ok(match self.table(spoofing::FAMILY, spoofing::TABLE)? {
                None => Ruleset::Missing,
                Some((userdata, rules)) => Ruleset::of_bridge_table(&userdata, rules),
            })
        }

        /// The table `name` in `family`'s user data and number of rules, if
        /// it's there.
        fn table(
            &mut self,
            family: u8,
            name: &str,
        ) -> Result<Option<(Vec<u8>, usize)>, NetlinkError> {
            let mut message = objects::message(NFT_MSG_GETTABLE, 0, family);
            message.attr_str(NFTA_TABLE_NAME, name);
            let mut userdata = None;
            let found =
                self.socket
                    .exchange(&format!("reading the table {name}"), &message, |reply| {
                        userdata = Some(decode::table_userdata(reply)?);
                        Ok(())
                    });
            match found {
                Ok(()) => {
                    let mut message = objects::message(NFT_MSG_GETRULE, 0, family);
                    message.attr_str(NFTA_RULE_TABLE, name);
                    let rules = self
                        .socket
                        .dump(
                            &format!("listing the table {name}'s rules"),
                            &message,
                            |_| Ok(Some(())),
                        )?
                        .len();
                    Ok(Some((userdata.unwrap_or_default(), rules)))
                }
                Err(error) if error.errno() == Some(Errno::ENOENT) => Ok(None),
                Err(error) => Err(error),
            }
        }

        /// Makes Shipyard's bridge table, replacing whatever table of that
        /// name is there, in one batch of its own (a kernel without bridge
        /// filtering refuses it, and nothing else goes with it). Then
        /// `guards` are put in its sets, in batches of their own.
        pub fn install_bridge_table(&mut self, guards: &[PortGuard]) -> Result<(), NetlinkError> {
            let mut items = Vec::new();
            if self.bridge_ruleset()? != Ruleset::Missing {
                items.push(delete_table(spoofing::FAMILY, spoofing::TABLE));
            }
            items.extend(spoofing::install(&[]));
            self.batch("installing shipyard's bridge rules", &items)?;
            for guard in guards {
                self.guard(guard)?;
            }
            Ok(())
        }

        /// Lets the container on `guard`'s port send only as itself (what's
        /// there already stays).
        pub fn guard(&mut self, guard: &PortGuard) -> Result<(), NetlinkError> {
            self.batch(
                &format!("keeping {} to its own addresses", guard.port),
                &spoofing::guard_items(guard),
            )
        }

        /// Takes `guard`'s port out of the bridge table (what isn't there
        /// is fine): the port first, which stops it being checked.
        pub fn unguard(&mut self, guard: &PortGuard) -> Result<(), NetlinkError> {
            let items: Vec<BatchItem> = spoofing::guard_elements(guard)
                .iter()
                .rev()
                .map(|(set, elements)| {
                    delete_elements(spoofing::FAMILY, spoofing::TABLE, set, elements)
                })
                .collect();
            self.each_if_present(&format!("forgetting the port {}", guard.port), items)
        }

        /// Makes Shipyard's table, with `networks` in its sets and routing
        /// `closed` as recorded, replacing whatever table of that name is
        /// there: in one batch, so the old rules give way to the new at
        /// once. `ports` are published again in that batch too, so the new
        /// table never lacks them, unless there are too many for one, or one
        /// is refused (which would refuse the table with it): then in batches
        /// of their own, after.
        pub fn install(
            &mut self,
            networks: &[NetworkRules],
            ports: &[PortRule],
            closed: &ClosedRouting,
        ) -> Result<(), NetlinkError> {
            let mut items = Vec::new();
            if self.ruleset()? != Ruleset::Missing {
                items.push(delete_table(FAMILY, TABLE));
            }
            items.extend(ruleset::install(networks, &[], closed));
            let table = items.len();
            let (items, more) = ruleset::fit_ports(items, ruleset::port_items(ports, false));
            let more = match self.batch("installing shipyard's firewall rules", &items) {
                Ok(()) => more,
                Err(_) if items.len() > table => {
                    let (table, ports) = items.split_at(table);
                    self.batch("installing shipyard's firewall rules", table)?;
                    ports.to_vec()
                }
                Err(error) => return Err(error),
            };
            for item in more {
                self.batch("publishing the containers' ports again", &[item])?;
            }
            Ok(())
        }

        /// Closes routing in `family` to all but the containers, between
        /// the host's `interfaces` (see the module `ruleset`): for a host
        /// that didn't route before Shipyard turned forwarding on.
        pub fn close_routing(
            &mut self,
            family: crate::Family,
            interfaces: &[String],
        ) -> Result<(), NetlinkError> {
            self.batch(
                "closing routing to all but containers",
                &ruleset::close_routing(family, interfaces),
            )
        }

        /// Puts `network` in the sets (what's there already stays).
        pub fn add_network(&mut self, network: &NetworkRules) -> Result<(), NetlinkError> {
            let items = ruleset::add_network(network);
            self.batch(
                &format!("adding the bridge {} to the firewall", network.bridge),
                &items,
            )
        }

        /// Takes `network` out of the sets (what isn't there is fine).
        pub fn remove_network(&mut self, network: &NetworkRules) -> Result<(), NetlinkError> {
            let items: Vec<BatchItem> = ruleset::network_elements(network)
                .iter()
                .map(|(set, elements)| delete_elements(FAMILY, TABLE, set, elements))
                .collect();
            self.each_if_present(
                &format!("removing the bridge {} from the firewall", network.bridge),
                items,
            )
        }

        /// Publishes `ports`: one that's published already (by another
        /// run) is refused, EEXIST, and then none of a map's ports are.
        pub fn publish(&mut self, ports: &[PortRule]) -> Result<(), NetlinkError> {
            for item in ruleset::port_items(ports, true) {
                self.batch("publishing ports", &[item])?;
            }
            Ok(())
        }

        /// What each of `ports`' keys (its protocol, host port and
        /// address) maps to now: the rule published there, if any (maybe
        /// another run's).
        pub fn published(
            &mut self,
            ports: &[PortRule],
        ) -> Result<Vec<Option<PortRule>>, NetlinkError> {
            let mut listed: Vec<(&'static str, Vec<Element>)> = Vec::new();
            ports
                .iter()
                .map(|port| {
                    // A rule whose families differ is published nowhere.
                    let Some((set, ours)) = ruleset::port_element(port) else {
                        return Ok(None);
                    };
                    if !listed.iter().any(|(name, _)| *name == set) {
                        // No table (flushed): nothing's published.
                        let elements = match self.elements(set) {
                            Err(error) if error.errno() == Some(Errno::ENOENT) => Vec::new(),
                            other => other?,
                        };
                        listed.push((set, elements));
                    }
                    Ok(listed
                        .iter()
                        .find(|(name, _)| *name == set)
                        .and_then(|(_, elements)| {
                            elements.iter().find(|element| element.key == ours.key)
                        })
                        .and_then(|element| ruleset::port_rule(set, element)))
                })
                .collect()
        }

        /// Stops publishing `ports`: each element whose key *and* value are
        /// theirs (a port another run published since, at the same key,
        /// stays). One that isn't published is fine.
        pub fn unpublish(&mut self, ports: &[PortRule]) -> Result<(), NetlinkError> {
            let current = self.published(ports)?;
            let ours: Vec<PortRule> = ports
                .iter()
                .zip(current)
                .filter(|(port, now)| now.as_ref() == Some(*port))
                .map(|(port, _)| *port)
                .collect();
            let items = ruleset::by_map(&ours)
                .iter()
                .map(|(set, elements)| delete_elements(FAMILY, TABLE, set, elements))
                .collect();
            self.each_if_present("unpublishing ports", items)
        }

        /// The elements of Shipyard's set `set`.
        pub fn elements(&mut self, set: &str) -> Result<Vec<Element>, NetlinkError> {
            let mut message = objects::message(NFT_MSG_GETSETELEM, 0, FAMILY);
            message
                .attr_str(NFTA_SET_ELEM_LIST_TABLE, TABLE)
                .attr_str(NFTA_SET_ELEM_LIST_SET, set);
            let parts = self
                .socket
                .dump(&format!("listing the set {set}"), &message, |part| {
                    decode::elements(part).map(Some)
                })?;
            Ok(parts.into_iter().flatten().collect())
        }

        /// Every chain of every table, in every family: Shipyard's and
        /// other programs' (Docker's, a firewall's).
        pub fn chains(&mut self) -> Result<Vec<Chain>, NetlinkError> {
            let message = objects::message(NFT_MSG_GETCHAIN, 0, NFPROTO_UNSPEC);
            self.socket
                .dump("listing the firewall's chains", &message, |part| {
                    decode::chain(part).map(Some)
                })
        }

        fn batch(&mut self, operation: &str, items: &[BatchItem]) -> Result<(), NetlinkError> {
            self.socket.batch(operation, NFNL_SUBSYS_NFTABLES, items)
        }

        /// Sends each of `items` in a batch of its own, where one that finds
        /// nothing to remove (ENOENT) is fine: a batch with one such item
        /// would undo the others.
        fn each_if_present(
            &mut self,
            operation: &str,
            items: Vec<BatchItem>,
        ) -> Result<(), NetlinkError> {
            for item in items {
                match self.batch(operation, std::slice::from_ref(&item)) {
                    Err(error) if error.errno() == Some(Errno::ENOENT) => {}
                    other => other?,
                }
            }
            Ok(())
        }
    }
}
