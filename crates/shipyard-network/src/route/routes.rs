//! Routes: `RTM_NEWROUTE` and what the kernel lists.
//!
//! A route message starts with `rtmsg` (family, destination prefix length,
//! table, protocol, scope, type), then `RTA_*` attributes: the destination,
//! the gateway, the output interface. Shipyard only adds routes to the main
//! table, and reads them (to find a free subnet).

use std::net::IpAddr;

use super::address::{ip, ip_bytes};
use super::consts::{
    AF_INET, AF_INET6, RT_SCOPE_LINK, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN, RTA_DST, RTA_GATEWAY,
    RTA_OIF, RTA_TABLE, RTM_GETROUTE, RTM_NEWROUTE, RTN_BLACKHOLE, RTN_PROHIBIT, RTN_UNICAST,
    RTN_UNREACHABLE, RTPROT_BOOT,
};
use crate::ipnet::{Family, IpNet};
use crate::netlink::message::read_u32;
use crate::netlink::{Message, MessageBuilder, ParseError, flag};

/// `rtmsg`: the fixed header of a route message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RouteHeader {
    /// `AF_INET` or `AF_INET6`.
    pub(crate) family: u8,
    /// The destination's prefix length.
    pub(crate) destination_prefix: u8,
    /// The source's prefix length.
    pub(crate) source_prefix: u8,
    /// The type of service.
    pub(crate) tos: u8,
    /// The routing table (`RT_TABLE_MAIN`; past 255, see `RTA_TABLE`).
    pub(crate) table: u8,
    /// Who added it (`RTPROT_*`).
    pub(crate) protocol: u8,
    /// How far it reaches (`RT_SCOPE_*`).
    pub(crate) scope: u8,
    /// Its type (`RTN_UNICAST`, …).
    pub(crate) route_type: u8,
    /// `RTM_F_*` flags.
    pub(crate) flags: u32,
}

impl RouteHeader {
    /// Its size.
    pub(crate) const LEN: usize = 12;

    /// Its bytes.
    pub(crate) fn encode(&self) -> [u8; Self::LEN] {
        let [f0, f1, f2, f3] = self.flags.to_ne_bytes();
        [
            self.family,
            self.destination_prefix,
            self.source_prefix,
            self.tos,
            self.table,
            self.protocol,
            self.scope,
            self.route_type,
            f0,
            f1,
            f2,
            f3,
        ]
    }

    /// It, from exactly its bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<RouteHeader, ParseError> {
        let length = || ParseError::Length("route header");
        let fields: [u8; 8] = bytes
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(length)?;
        if bytes.len() != Self::LEN {
            return Err(length());
        }
        let [
            family,
            destination_prefix,
            source_prefix,
            tos,
            table,
            protocol,
            scope,
            route_type,
        ] = fields;
        Ok(RouteHeader {
            family,
            destination_prefix,
            source_prefix,
            tos,
            table,
            protocol,
            scope,
            route_type,
            flags: read_u32(bytes, 8).ok_or_else(length)?,
        })
    }
}

/// What a route does with what it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteType {
    /// Sends it on (directly, or through a gateway).
    Unicast,
    /// Drops it silently.
    Blackhole,
    /// Refuses it: "network unreachable".
    Unreachable,
    /// Refuses it: "prohibited".
    Prohibit,
}

/// A route in the main table.
///
/// A multipath route (`RTA_MULTIPATH`) or one through a nexthop object
/// (`RTA_NH_ID`) is read with no gateway and no interface: its destination
/// is what matters for the overlap checks it's read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// Where it leads: a network (`0.0.0.0/0` for the default route).
    pub destination: IpNet,
    /// The next hop, for a route through a gateway.
    pub gateway: Option<IpAddr>,
    /// The output interface's index.
    pub index: Option<u32>,
    /// What it does with what it matches.
    pub route_type: RouteType,
}

impl Route {
    /// The default route of `gateway`'s family, through it.
    #[must_use]
    pub fn default_via(gateway: IpAddr) -> Route {
        Route {
            destination: IpNet::any(Family::of(gateway)),
            gateway: Some(gateway),
            index: None,
            route_type: RouteType::Unicast,
        }
    }

    /// The main table's route an `RTM_NEWROUTE` describes: one that sends
    /// on, or refuses, what it matches (`None` for another message, table,
    /// family or type: local, broadcast, …). Refusing ones are read too:
    /// their destination is still taken.
    pub(crate) fn parse(message: &Message<'_>) -> Result<Option<Route>, ParseError> {
        if message.kind != RTM_NEWROUTE {
            return Ok(None);
        }
        let (header, attrs) = message.split(RouteHeader::LEN)?;
        let header = RouteHeader::decode(header)?;
        let route_type = match header.route_type {
            RTN_UNICAST => RouteType::Unicast,
            RTN_BLACKHOLE => RouteType::Blackhole,
            RTN_UNREACHABLE => RouteType::Unreachable,
            RTN_PROHIBIT => RouteType::Prohibit,
            _ => return Ok(None),
        };
        if header.family != AF_INET && header.family != AF_INET6 {
            return Ok(None);
        }
        let mut table = u32::from(header.table);
        let (mut destination, mut gateway, mut index) = (None, None, None);
        for attr in attrs {
            let attr = attr?;
            match attr.kind {
                RTA_DST => destination = Some(ip(header.family, &attr)?),
                RTA_GATEWAY => gateway = Some(ip(header.family, &attr)?),
                RTA_OIF => index = Some(attr.u32()?),
                RTA_TABLE => table = attr.u32()?,
                _ => {}
            }
        }
        if table != u32::from(RT_TABLE_MAIN) {
            return Ok(None);
        }
        // No destination is the default route: the unspecified address.
        let family = if header.family == AF_INET6 {
            Family::V6
        } else {
            Family::V4
        };
        let destination = match destination {
            Some(destination) => IpNet::new(destination, header.destination_prefix)
                .map_err(|_| ParseError::Invalid("prefix"))?,
            None => IpNet::any(family),
        };
        Ok(Some(Route {
            destination,
            gateway,
            index,
            route_type,
        }))
    }
}

/// `RTM_NEWROUTE` adding `route` to the main table, of its type.
pub(crate) fn add(route: &Route) -> MessageBuilder {
    let header = RouteHeader {
        family: route.destination.family().af(),
        destination_prefix: route.destination.prefix(),
        table: RT_TABLE_MAIN,
        protocol: RTPROT_BOOT,
        // Through a gateway, it reaches past this link; without one, the
        // destination is on the link itself. One that refuses what it
        // matches is on no link (`ip` makes them so).
        scope: if route.gateway.is_some() || route.route_type != RouteType::Unicast {
            RT_SCOPE_UNIVERSE
        } else {
            RT_SCOPE_LINK
        },
        route_type: match route.route_type {
            RouteType::Unicast => RTN_UNICAST,
            RouteType::Blackhole => RTN_BLACKHOLE,
            RouteType::Unreachable => RTN_UNREACHABLE,
            RouteType::Prohibit => RTN_PROHIBIT,
        },
        ..RouteHeader::default()
    };
    let mut message = MessageBuilder::new(RTM_NEWROUTE, flag::CREATE | flag::EXCL);
    message.raw(&header.encode());
    if route.destination.prefix() > 0 {
        message.attr(RTA_DST, &ip_bytes(route.destination.network().addr()));
    }
    if let Some(gateway) = route.gateway {
        message.attr(RTA_GATEWAY, &ip_bytes(gateway));
    }
    if let Some(index) = route.index {
        message.attr_u32(RTA_OIF, index);
    }
    message
}

/// `RTM_GETROUTE` for the routes of `family`: a dump.
pub(crate) fn get(family: Family) -> MessageBuilder {
    let header = RouteHeader {
        family: family.af(),
        ..RouteHeader::default()
    };
    let mut message = MessageBuilder::new(RTM_GETROUTE, 0);
    message.raw(&header.encode());
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::message::Messages;

    fn parsed(message: &MessageBuilder) -> Option<Route> {
        let bytes = message.finish(1, 0).unwrap();
        // what the kernel lists looks like what was added, as `RTM_NEWROUTE`
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        Route::parse(&message).unwrap()
    }

    #[test]
    fn the_header_round_trips() {
        let header = RouteHeader {
            family: AF_INET,
            destination_prefix: 16,
            source_prefix: 0,
            tos: 0,
            table: 254,
            protocol: 2,
            scope: 253,
            route_type: 1,
            flags: 0x400,
        };
        assert_eq!(RouteHeader::decode(&header.encode()), Ok(header));
    }

    #[test]
    fn a_default_route_has_no_destination_and_is_universe() {
        let route = Route::default_via("10.88.0.1".parse().unwrap());
        let message = add(&route);
        let bytes = message.finish(1, 0).unwrap();
        let read = Messages::new(&bytes).next().unwrap().unwrap();
        let (header, attrs) = read.split(RouteHeader::LEN).unwrap();
        let header = RouteHeader::decode(header).unwrap();
        assert_eq!(
            (header.destination_prefix, header.scope, header.table),
            (0, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN)
        );
        assert_eq!(attrs.count(), 1); // the gateway only
        assert_eq!(parsed(&message), Some(route));
    }

    /// Each type is added as itself, with the scope `ip` gives it: a route
    /// through a gateway or one that refuses what it matches reaches past
    /// any link (universe); one without a gateway is on its link.
    #[test]
    fn a_route_is_added_as_its_type_and_scope() {
        let header = |route: &Route| {
            let bytes = add(route).finish(1, 0).unwrap();
            let read = Messages::new(&bytes).next().unwrap().unwrap();
            let (header, _) = read.split(RouteHeader::LEN).unwrap();
            RouteHeader::decode(header).unwrap()
        };
        let route = |gateway: Option<&str>, route_type| Route {
            destination: "10.90.0.0/16".parse().unwrap(),
            gateway: gateway.map(|gateway| gateway.parse().unwrap()),
            index: None,
            route_type,
        };
        for (route, kind, scope) in [
            (
                route(Some("10.0.0.1"), RouteType::Unicast),
                RTN_UNICAST,
                RT_SCOPE_UNIVERSE,
            ),
            (route(None, RouteType::Unicast), RTN_UNICAST, RT_SCOPE_LINK),
            (
                route(None, RouteType::Blackhole),
                RTN_BLACKHOLE,
                RT_SCOPE_UNIVERSE,
            ),
            (
                route(None, RouteType::Unreachable),
                RTN_UNREACHABLE,
                RT_SCOPE_UNIVERSE,
            ),
            (
                route(None, RouteType::Prohibit),
                RTN_PROHIBIT,
                RT_SCOPE_UNIVERSE,
            ),
        ] {
            let added = header(&route);
            assert_eq!((added.route_type, added.scope), (kind, scope), "{route:?}");
            assert_eq!(parsed(&add(&route)), Some(route));
        }
    }

    #[test]
    fn a_link_route_names_its_network_and_interface() {
        let route = Route {
            destination: "10.88.3.9/16".parse().unwrap(),
            gateway: None,
            index: Some(4),
            route_type: RouteType::Unicast,
        };
        let back = parsed(&add(&route)).unwrap();
        assert_eq!(back.destination, "10.88.0.0/16".parse().unwrap());
        assert_eq!((back.gateway, back.index), (None, Some(4)));
    }

    #[test]
    fn other_tables_and_types_are_skipped() {
        let mut message = MessageBuilder::new(RTM_NEWROUTE, 0);
        message.raw(
            &RouteHeader {
                family: AF_INET,
                table: 255,
                route_type: RTN_UNICAST,
                ..RouteHeader::default()
            }
            .encode(),
        );
        assert_eq!(parsed(&message), None);
        let mut message = MessageBuilder::new(RTM_NEWROUTE, 0);
        message
            .raw(
                &RouteHeader {
                    family: AF_INET,
                    table: 0,
                    route_type: RTN_UNICAST,
                    ..RouteHeader::default()
                }
                .encode(),
            )
            .attr_u32(RTA_TABLE, 1000);
        assert_eq!(parsed(&message), None);
        let mut message = MessageBuilder::new(RTM_NEWROUTE, 0);
        message.raw(
            &RouteHeader {
                family: AF_INET,
                table: RT_TABLE_MAIN,
                route_type: 2, // RTN_LOCAL
                ..RouteHeader::default()
            }
            .encode(),
        );
        assert_eq!(parsed(&message), None);
    }

    #[test]
    fn a_blackhole_route_is_read_its_destination_is_taken() {
        let mut message = MessageBuilder::new(RTM_NEWROUTE, 0);
        message
            .raw(
                &RouteHeader {
                    family: AF_INET,
                    destination_prefix: 16,
                    table: RT_TABLE_MAIN,
                    route_type: RTN_BLACKHOLE,
                    ..RouteHeader::default()
                }
                .encode(),
            )
            .attr(RTA_DST, &[10, 88, 0, 0]);
        let route = parsed(&message).unwrap();
        assert_eq!(route.route_type, RouteType::Blackhole);
        assert_eq!(route.destination, "10.88.0.0/16".parse().unwrap());
    }

    #[test]
    fn ipv6_routes() {
        let route = Route {
            destination: "fd00:1::/64".parse().unwrap(),
            gateway: Some("fd00:1::1".parse().unwrap()),
            index: None,
            route_type: RouteType::Unicast,
        };
        assert_eq!(parsed(&add(&route)), Some(route));
    }
}
