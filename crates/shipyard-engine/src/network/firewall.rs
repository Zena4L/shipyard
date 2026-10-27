//! The firewall around a network: Shipyard's own tables (made current
//! before a run joins), forwarding for the containers' way out, and other
//! programs' firewalls that would get in the way (warned about, never
//! changed).

#[cfg(target_os = "linux")]
use super::{bridged_traffic_filtered, default_route_interface};
#[cfg(target_os = "linux")]
use crate::error::Result;
#[cfg(target_os = "linux")]
use crate::report::{Reporter, WarningKind, warn};

/// Makes Shipyard's firewall table current, then puts `config`'s network
/// in its sets. A missing table (after a reboot, or `nft flush ruleset`) or
/// an older one is made whole again, from every network and every live
/// run's published ports, under the store's lock, so two runs never build
/// it at once. The host's routing Shipyard closed is closed again; if its
/// record can't be read, it's closed for the forwarding that's on now, and
/// said ([`closed_now`]).
#[cfg(target_os = "linux")]
pub(super) fn ensure_firewall(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    config: &shipyard_network::store::NetworkConfig,
    reporter: &dyn Reporter,
) -> Result<()> {
    use shipyard_network::nft::{NetworkRules, Ruleset};
    // A newer Shipyard's table is left as it is (rebuilt by this one, it
    // would lose what that one added); its networks' elements go in it.
    let stale = |found: Ruleset| matches!(found, Ruleset::Missing | Ruleset::Outdated);
    if stale(nft.ruleset()?) {
        let _all = networks.lock_all()?;
        if stale(nft.ruleset()?) {
            let rules: Vec<NetworkRules> =
                networks.list()?.iter().map(NetworkRules::from).collect();
            let ports = networks.live_port_rules()?;
            let closed = closed_now(networks.routing_closed(), reporter)?;
            nft.install(&rules, &ports, &closed, &outside_links()?)?;
        }
    }
    nft.add_network(&NetworkRules::from(config))?;
    Ok(())
}

/// The links no published port is reached from: the macOS guest's NAT
/// link in the guest ([`forward::GUEST_LINK`], F108), none on a Linux host.
///
/// [`forward::GUEST_LINK`]: shipyard_runtime::attach::forward::GUEST_LINK
///
/// # Errors
///
/// In the guest, if its command line can't be read (never taken for "not
/// the guest": the link would be left open).
#[cfg(target_os = "linux")]
fn outside_links() -> Result<Vec<String>> {
    Ok(if crate::guest::booted_by_shipyard()? {
        vec![shipyard_runtime::attach::forward::GUEST_LINK.to_owned()]
    } else {
        Vec::new()
    })
}

/// The routing to close, from `records`: what they say, and, for each
/// family whose record can't be read and whose forwarding is on (whoever
/// turned it on: a table rebuilt open would let the host route for anyone,
/// F65), the host's own links now, bridges and their ports left out
/// (another engine's, routed by it). Said, for each. A family whose
/// forwarding is off needs nothing: [`open_routing`] closes routing before
/// it turns forwarding on, and records it again.
#[cfg(target_os = "linux")]
fn closed_now(
    records: shipyard_network::store::RoutingRecords,
    reporter: &dyn Reporter,
) -> Result<shipyard_network::nft::ClosedRouting> {
    use shipyard_network::attach;
    let mut closed = records.closed;
    for (family, unread) in records.unread {
        if attach::forwarding(family)? != Some(true) {
            continue;
        }
        if !closed.families.contains(&family) {
            closed.families.push(family);
        }
        for interface in attach::unbridged_host_interfaces()? {
            if !closed.interfaces.contains(&interface) {
                closed.interfaces.push(interface);
            }
        }
        warn(
            reporter,
            WarningKind::RoutingRecordUnreadable,
            format!(
                "the record of the host's {family} routing Shipyard closed couldn't be read \
                 ({unread}): its routing between its own links (bridges and their ports left \
                 out, another engine's) is closed, as if Shipyard had turned forwarding on, until \
                 Shipyard's table is next rebuilt (a reboot); if this host is meant to route, \
                 delete Shipyard's routing records (`.routing-closed*` in its networks \
                 directory), so the next rebuild doesn't close it again"
            ),
        );
    }
    Ok(closed)
}

/// Makes Shipyard's bridge table current (the module `nft::spoofing`:
/// each container sends only as itself), as [`ensure_firewall`] does the
/// other: whether it's there. A kernel without bridge filtering refuses
/// it; then containers can still send as one another, as before it, and
/// the run goes on, warned.
#[cfg(target_os = "linux")]
pub(super) fn ensure_bridge_table(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    reporter: &dyn Reporter,
) -> bool {
    use shipyard_network::nft::Ruleset;
    let unguarded = |why: &str| {
        warn(
            reporter,
            WarningKind::AntiSpoofing,
            format!("containers on a network can send as one another here: {why}"),
        );
        false
    };
    match nft.bridge_ruleset() {
        Ok(Ruleset::Current | Ruleset::Newer) => return true,
        Ok(_) => {}
        Err(error) => return unguarded(&format!("reading shipyard's bridge table: {error}")),
    }
    let _all = match networks.lock_all() {
        Ok(lock) => lock,
        Err(error) => return unguarded(&error.to_string()),
    };
    match nft.bridge_ruleset() {
        Ok(Ruleset::Current | Ruleset::Newer) => return true,
        Ok(_) => {}
        Err(error) => return unguarded(&format!("reading shipyard's bridge table: {error}")),
    }
    if let Err(error) = nft.install_bridge_table(&[]) {
        // Only the kernel's refusal of the table itself says it has none.
        let no_filtering = matches!(
            error.errno(),
            Some(nix::errno::Errno::EAFNOSUPPORT | nix::errno::Errno::EOPNOTSUPP)
        );
        return unguarded(&if no_filtering {
            format!("the kernel has no bridge filtering ({error})")
        } else {
            format!("making shipyard's bridge table: {error}")
        });
    }
    // Every live container's port, read only now that the table is there: a
    // run guarding its port meanwhile is among them, or guards it after.
    match networks.live_port_guards() {
        Ok(guards) => {
            for guard in guards {
                if let Err(error) = nft.guard(&guard) {
                    warn(
                        reporter,
                        WarningKind::AntiSpoofing,
                        format!(
                            "the container on the port {} can send as another: {error}",
                            guard.port
                        ),
                    );
                }
            }
        }
        Err(error) => {
            warn(
                reporter,
                WarningKind::AntiSpoofing,
                format!(
                    "running containers can send as one another until they're run again: {error}"
                ),
            );
        }
    }
    true
}

/// Turns forwarding on for each of `config`'s families where the host
/// doesn't route: the containers' way out. The host still won't route
/// between its own interfaces (those it has now) for anyone: routing is
/// closed first, so that it's never an open router, even if the run goes no
/// further.
#[cfg(target_os = "linux")]
pub(super) fn open_routing(
    networks: &shipyard_network::store::Networks,
    nft: &mut shipyard_network::nft::Nftables,
    config: &shipyard_network::store::NetworkConfig,
) -> Result<()> {
    use shipyard_network::{Family, attach};
    let families: &[Family] = if config.subnet6.is_some() {
        &[Family::V4, Family::V6]
    } else {
        &[Family::V4]
    };
    for &family in families {
        if attach::forwarding(family)? != Some(false) {
            continue;
        }
        let interfaces = attach::host_interfaces()?;
        networks.close_routing(family, &interfaces)?;
        nft.close_routing(family, &interfaces)?;
        let kept = attach::turn_forwarding_on(family)?;
        if family == Family::V4 {
            // That turned it on for every interface: internal networks'
            // bridges don't forward, whatever the rules.
            for internal in networks.list()?.iter().filter(|other| other.internal) {
                match attach::stop_forwarding(&internal.bridge) {
                    // Not made since the last reboot: done when it is.
                    Err(attach::AttachError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound => {}
                    other => other?,
                }
            }
        }
        tracing::info!("turned on {family} forwarding for containers' way out, and only for them");
        if !kept.is_empty() {
            tracing::info!(
                "kept taking router advertisements while forwarding (accept_ra=2) on: {}",
                kept.join(", ")
            );
        }
    }
    Ok(())
}

/// Warns about other programs' firewalls that would drop the containers'
/// traffic on `config`'s network (firewalld, Docker's, ufw, any chain
/// dropping by default), each with the fix in that firewall's own terms.
/// Shipyard never changes them: see `shipyard_network::firewalls`.
#[cfg(target_os = "linux")]
pub(super) fn warn_about_other_firewalls(
    nft: &mut shipyard_network::nft::Nftables,
    config: &shipyard_network::store::NetworkConfig,
    reporter: &dyn Reporter,
) {
    let (uplink, uplink6) = if config.internal {
        (None, None)
    } else {
        (
            default_route_interface(shipyard_network::Family::V4),
            default_route_interface(shipyard_network::Family::V6),
        )
    };
    let view = shipyard_network::firewalls::NetworkView {
        bridge: &config.bridge,
        uplink: uplink.as_deref(),
        uplink6: uplink6.as_deref(),
        outward: !config.internal,
        ipv6: config.subnet6.is_some(),
        bridged: bridged_traffic_filtered(),
        dns: config.has_dns().then_some(config.gateway),
    };
    match shipyard_network::firewalls::conflicts(nft, &view) {
        Ok(conflicts) => {
            for conflict in conflicts.iter().filter(|conflict| !conflict.fixed) {
                warn(
                    reporter,
                    WarningKind::OtherFirewall,
                    describe_conflict(conflict),
                );
            }
        }
        // A warning that can't be given isn't the run's failure.
        Err(error) => tracing::debug!("couldn't read the other firewalls: {error}"),
    }
}

/// What to tell the user about another firewall in the way: whose, what it
/// keeps from the containers, and the fix.
#[must_use]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "doctor's firewall check is Linux's")
)]
pub(crate) fn describe_conflict(conflict: &shipyard_network::firewalls::Conflict) -> String {
    use shipyard_network::firewalls::Firewall;
    let blocks = conflict.blocks;
    let drops = if blocks.way_out || blocks.ports || blocks.each_other {
        "drops forwarded traffic by default"
    } else {
        "drops what comes in to the host by default"
    };
    let who = match &conflict.firewall {
        Firewall::Firewalld => {
            "firewalld rejects what comes from an interface it doesn't trust".to_owned()
        }
        Firewall::Docker { ipv6: true } => {
            "Docker's firewall drops IPv6 forwarded traffic by default".to_owned()
        }
        Firewall::Docker { .. } => {
            "Docker's firewall drops forwarded traffic by default".to_owned()
        }
        Firewall::Ufw => format!("ufw {drops}"),
        Firewall::Chain { at } => format!("the firewall's chain {at} {drops}"),
        _ => format!("another firewall {drops}"),
    };
    let mut kept = Vec::new();
    if blocks.way_out {
        kept.push("would have no way out".to_owned());
    }
    if blocks.ports {
        kept.push("wouldn't be reached on their published ports".to_owned());
    }
    if blocks.each_other {
        kept.push("couldn't reach each other".to_owned());
    }
    if blocks.dns {
        kept.push(conflict.dns.map_or_else(
            || "would get no answer from their DNS server".to_owned(),
            |dns| format!("would get no answer from their DNS server (on {dns}:53)"),
        ));
    }
    let kept = match kept.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}, and {last}", rest.join(", ")),
        None => String::new(),
    };
    let how = match &conflict.firewall {
        Firewall::Firewalld => "to put the bridge in firewalld's trusted zone",
        Firewall::Docker { .. } => {
            "to let them through (in DOCKER-USER, the chain Docker keeps for this)"
        }
        Firewall::Ufw => "to let them through with ufw's own rules",
        _ => "to let them through",
    };
    format!(
        "{who}: containers on {} {kept}; {how}: {}",
        conflict.bridge,
        conflict.fix.join("; ")
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn another_firewall_is_described_by_what_it_keeps_from_the_containers() {
        use shipyard_network::firewalls::{Blocks, Conflict, Firewall};
        let conflict = |firewall, blocks| Conflict {
            firewall,
            bridge: "shy-x".to_owned(),
            blocks,
            dns: Some("10.89.0.1".parse().unwrap()),
            fixed: false,
            fix: vec!["sudo a".to_owned(), "sudo b".to_owned()],
        };
        let everything = Blocks {
            way_out: true,
            ports: true,
            each_other: true,
            dns: false,
        };
        assert_eq!(
            describe_conflict(&conflict(Firewall::Docker { ipv6: false }, everything)),
            "Docker's firewall drops forwarded traffic by default: containers on shy-x would \
             have no way out, wouldn't be reached on their published ports, and couldn't reach \
             each other; to let them through (in DOCKER-USER, the chain Docker keeps for this): \
             sudo a; sudo b"
        );
        // An internal network: only each other is at stake.
        let internal = Blocks {
            each_other: true,
            ..Blocks::default()
        };
        assert_eq!(
            describe_conflict(&conflict(
                Firewall::Chain {
                    at: "ip filter FORWARD".to_owned()
                },
                internal
            )),
            "the firewall's chain ip filter FORWARD drops forwarded traffic by default: \
             containers on shy-x couldn't reach each other; to let them through: sudo a; sudo b"
        );
        let dns = Blocks {
            dns: true,
            ..Blocks::default()
        };
        assert_eq!(
            describe_conflict(&conflict(Firewall::Ufw, dns)),
            "ufw drops what comes in to the host by default: containers on shy-x would get no \
             answer from their DNS server (on 10.89.0.1:53); to let them through with ufw's own \
             rules: sudo a; sudo b"
        );
        assert!(
            describe_conflict(&conflict(Firewall::Firewalld, dns)).starts_with(
                "firewalld rejects what comes from an interface it doesn't trust: containers on \
                 shy-x would get no answer"
            )
        );
    }

    /// A routing record that can't be read (F65): routing is closed for the
    /// forwarding that's on now, between the host's own links (a bridge
    /// and its port left out; a link with no master in), and said; with
    /// forwarding off, nothing's closed and nothing's said (the control).
    /// In a network namespace of the test's own.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root"]
    fn an_unread_routing_record_closes_the_forwarding_on_now() {
        use crate::report::Collect;
        use shipyard_network::Family;
        use shipyard_network::route::{LinkChange, Rtnetlink};
        use shipyard_network::store::{RoutingRecords, StoreError};
        shipyard_test_support::in_new_netns(|| {
            let mut links = Rtnetlink::new().unwrap();
            links.create_bridge("br-other").unwrap();
            links.create_veth("v-own", "v-port", None, None).unwrap();
            let bridge = links.link("br-other").unwrap().unwrap().index;
            let port = links.link("v-port").unwrap().unwrap().index;
            links.change(port, LinkChange::Master(bridge)).unwrap();
            let unread = || RoutingRecords {
                closed: shipyard_network::nft::ClosedRouting::default(),
                unread: vec![(
                    Family::V4,
                    StoreError::Io {
                        context: "reading .routing-closed".to_owned(),
                        source: std::io::ErrorKind::PermissionDenied.into(),
                    },
                )],
            };
            let set = |path: &str, on: &str| {
                // IPv6 may be off in the kernel: then there's nothing to set.
                let _ = std::fs::write(path, on);
            };
            set("/proc/sys/net/ipv4/ip_forward", "0");
            set("/proc/sys/net/ipv6/conf/all/forwarding", "0");
            let quiet = Collect::default();
            let closed = closed_now(unread(), &quiet).unwrap();
            assert!(closed.families.is_empty(), "{closed:?}");
            assert!(quiet.warnings().is_empty());

            set("/proc/sys/net/ipv4/ip_forward", "1");
            let said = Collect::default();
            let closed = closed_now(unread(), &said).unwrap();
            assert_eq!(closed.families, vec![Family::V4]);
            assert_eq!(
                closed.interfaces,
                ["v-own"],
                "a bridge and its port left out"
            );
            let warnings = said.warnings();
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert_eq!(warnings[0].kind, WarningKind::RoutingRecordUnreadable);
        });
    }
}
