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

pub use decode::Chain;
pub use objects::Element;
pub use ruleset::{ClosedRouting, NetworkRules, PortRule, TABLE};

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
        if userdata == ruleset::VERSION && rules == ruleset::rules().len() {
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
            let mut message = objects::message(NFT_MSG_GETTABLE, 0, FAMILY);
            message.attr_str(NFTA_TABLE_NAME, TABLE);
            let mut userdata = None;
            let found = self
                .socket
                .exchange("reading the table shipyard", &message, |reply| {
                    userdata = Some(decode::table_userdata(reply)?);
                    Ok(())
                });
            match found {
                Ok(()) => Ok(Ruleset::of(
                    userdata.as_deref().unwrap_or_default(),
                    self.rule_count()?,
                )),
                Err(error) if error.errno() == Some(Errno::ENOENT) => Ok(Ruleset::Missing),
                Err(error) => Err(error),
            }
        }

        /// How many rules Shipyard's table has.
        fn rule_count(&mut self) -> Result<usize, NetlinkError> {
            let mut message = objects::message(NFT_MSG_GETRULE, 0, FAMILY);
            message.attr_str(NFTA_RULE_TABLE, TABLE);
            Ok(self
                .socket
                .dump("listing the table shipyard's rules", &message, |_| {
                    Ok(Some(()))
                })?
                .len())
        }

        /// Makes Shipyard's table, with `networks` in its sets and routing
        /// `closed` as recorded, replacing whatever table of that name is
        /// there:
        /// in one batch, so the old rules give way to the new at once. Then
        /// `ports` are published again (in batches of their own: there can
        /// be many).
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
            self.batch("installing shipyard's firewall rules", &items)?;
            for item in ruleset::port_items(ports, false) {
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
