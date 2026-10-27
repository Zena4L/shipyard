//! Connection tracking (conntrack), over ctnetlink: the kernel's table of
//! connections, which NAT works through.
//!
//! The kernel decides NAT once per connection, on its first packet, and
//! remembers it (with the connection's two directions: the *original* tuple,
//! as it was sent, and the *reply* one, as answers come back). An entry
//! outlives its container: a UDP "connection" to a published port, kept
//! alive by a client that keeps sending, goes on being DNAT'd to the dead
//! container's address, and so to whatever container gets that address
//! next. So when a run's address is given back, the entries that involve it
//! are deleted: those it started (original source) and those that reached it
//! (reply source, after the DNAT).

#![deny(clippy::indexing_slicing)]
// The parsing is the Linux client's, and tested everywhere.
#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the client is Linux's")
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::netlink::message::Attr;
use crate::netlink::{Message, NFGENMSG_LEN, ParseError};
use crate::nft::consts::{
    CTA_IP_V4_SRC, CTA_IP_V6_SRC, CTA_TUPLE_IP, CTA_TUPLE_ORIG, CTA_TUPLE_REPLY, CTA_ZONE,
};

/// One connection, as the kernel lists it: enough to tell whose it is, and
/// to name it again to delete it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The original tuple's attributes, as listed (sent back to delete it).
    orig: Vec<u8>,
    /// Its zone, if it has one.
    zone: Option<u16>,
    /// Where it started.
    pub orig_source: Option<IpAddr>,
    /// Where its replies come from (after a DNAT, the container).
    pub reply_source: Option<IpAddr>,
}

impl Connection {
    /// Whether `ip` is in it: as its source, or as its replies'.
    #[must_use]
    pub fn involves(&self, ip: IpAddr) -> bool {
        self.orig_source == Some(ip) || self.reply_source == Some(ip)
    }
}

/// A connection from a ctnetlink `NEWCONNTRACK` message.
pub(crate) fn parse(message: &Message<'_>) -> Result<Connection, ParseError> {
    let (_, attrs) = message.split(NFGENMSG_LEN)?;
    let mut connection = Connection {
        orig: Vec::new(),
        zone: None,
        orig_source: None,
        reply_source: None,
    };
    for attr in attrs {
        let attr = attr?;
        match attr.kind {
            CTA_TUPLE_ORIG => {
                connection.orig = attr.value.to_vec();
                connection.orig_source = source(&attr)?;
            }
            CTA_TUPLE_REPLY => connection.reply_source = source(&attr)?,
            CTA_ZONE => connection.zone = Some(attr.be16()?),
            _ => {}
        }
    }
    if connection.orig.is_empty() {
        return Err(ParseError::Missing("original tuple"));
    }
    Ok(connection)
}

/// A tuple's source address (IPv4 or IPv6).
fn source(tuple: &Attr<'_>) -> Result<Option<IpAddr>, ParseError> {
    for part in tuple.nested() {
        let part = part?;
        if part.kind != CTA_TUPLE_IP {
            continue;
        }
        for address in part.nested() {
            let address = address?;
            match address.kind {
                CTA_IP_V4_SRC => {
                    return Ok(Some(IpAddr::V4(Ipv4Addr::from(address.fixed::<4>()?))));
                }
                CTA_IP_V6_SRC => {
                    return Ok(Some(IpAddr::V6(Ipv6Addr::from(address.fixed::<16>()?))));
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
pub use linux::{connections, flush_address};

#[cfg(target_os = "linux")]
mod linux {
    use std::net::IpAddr;

    use crate::Family;

    use nix::errno::Errno;

    use super::{Connection, parse};
    use crate::netlink::{NetlinkError, Socket, attr_flag, nfnl_message};
    use crate::nft::consts::{
        CTA_TUPLE_ORIG, CTA_ZONE, IPCTNL_MSG_CT_DELETE, IPCTNL_MSG_CT_GET, NFNL_SUBSYS_CTNETLINK,
    };

    /// Every connection of `family` the kernel tracks (in this network
    /// namespace).
    pub fn connections(family: Family) -> Result<Vec<Connection>, NetlinkError> {
        list(&mut Socket::netfilter()?, family.af(), |_| true)
    }

    /// The connections of `family` that `keep` keeps, as they're listed (a
    /// busy host's table is large: only those are held).
    fn list(
        socket: &mut Socket,
        family: u8,
        keep: impl Fn(&Connection) -> bool,
    ) -> Result<Vec<Connection>, NetlinkError> {
        let list = nfnl_message(NFNL_SUBSYS_CTNETLINK, IPCTNL_MSG_CT_GET, 0, family);
        socket.dump("listing connections", &list, |message| {
            parse(message).map(|connection| keep(&connection).then_some(connection))
        })
    }

    /// Deletes every connection `ip` (IPv4 or IPv6) is in (see the
    /// module). Returns how many were deleted; one that ended meanwhile is
    /// fine, and one that can't be deleted doesn't stop the others (the
    /// first error is returned, after).
    pub fn flush_address(ip: IpAddr) -> Result<usize, NetlinkError> {
        let mut socket = Socket::netfilter()?;
        let family = Family::of(ip).af();
        let connections = list(&mut socket, family, |connection| connection.involves(ip))?;
        let mut deleted = 0;
        let mut failed = None;
        for connection in connections {
            let mut delete = nfnl_message(NFNL_SUBSYS_CTNETLINK, IPCTNL_MSG_CT_DELETE, 0, family);
            // The tuple as it was listed, flagged nested again.
            delete.attr(CTA_TUPLE_ORIG | attr_flag::NESTED, &connection.orig);
            if let Some(zone) = connection.zone {
                delete.attr_be16(CTA_ZONE, zone);
            }
            match socket.request(&format!("forgetting a connection of {ip}"), &delete) {
                Ok(()) => deleted += 1,
                Err(error) if error.errno() == Some(Errno::ENOENT) => {}
                Err(error) => {
                    failed.get_or_insert(error);
                }
            }
        }
        failed.map_or(Ok(deleted), Err)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;
    use crate::netlink::MessageBuilder;
    use crate::netlink::message::Messages;
    use crate::nft::consts::{CTA_IP_V4_DST, CTA_IP_V6_DST};

    /// A tuple from `source` to `destination` (both of one family).
    fn tuple(builder: &mut MessageBuilder, kind: u16, source: IpAddr, destination: IpAddr) {
        builder.nest(kind, |tuple| {
            tuple.nest(CTA_TUPLE_IP, |ip| match (source, destination) {
                (IpAddr::V4(source), IpAddr::V4(destination)) => {
                    ip.attr(CTA_IP_V4_SRC, &source.octets())
                        .attr(CTA_IP_V4_DST, &destination.octets());
                }
                (IpAddr::V6(source), IpAddr::V6(destination)) => {
                    ip.attr(CTA_IP_V6_SRC, &source.octets())
                        .attr(CTA_IP_V6_DST, &destination.octets());
                }
                _ => panic!("two families"),
            });
        });
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_dnated_connection_names_the_container_as_its_replies_source() {
        // A client 192.0.2.9 reaching the host 192.0.2.1, DNATed to 10.88.0.2.
        let mut message = MessageBuilder::new(0x100, 0);
        message.raw(&[2, 0, 0, 0]);
        tuple(
            &mut message,
            CTA_TUPLE_ORIG,
            ip("192.0.2.9"),
            ip("192.0.2.1"),
        );
        tuple(
            &mut message,
            CTA_TUPLE_REPLY,
            ip("10.88.0.2"),
            ip("192.0.2.9"),
        );
        message.attr_be16(CTA_ZONE, 3);
        let bytes = message.finish(1, 0).unwrap();
        let connection = parse(&Messages::new(&bytes).next().unwrap().unwrap()).unwrap();
        assert_eq!(connection.orig_source, Some(ip("192.0.2.9")));
        assert_eq!(connection.reply_source, Some(ip("10.88.0.2")));
        assert_eq!(connection.zone, Some(3));
        assert!(connection.involves(ip("10.88.0.2")));
        assert!(connection.involves(ip("192.0.2.9")));
        assert!(!connection.involves(ip("192.0.2.1")));
        // The original tuple is kept as it came, to name it again.
        assert!(!connection.orig.is_empty());
    }

    #[test]
    fn an_ipv6_connection_names_its_sources_too() {
        let mut message = MessageBuilder::new(0x100, 0);
        message.raw(&[10, 0, 0, 0]);
        tuple(
            &mut message,
            CTA_TUPLE_ORIG,
            ip("2001:db8::9"),
            ip("2001:db8::1"),
        );
        tuple(
            &mut message,
            CTA_TUPLE_REPLY,
            ip("fd00:1::2"),
            ip("2001:db8::9"),
        );
        let bytes = message.finish(1, 0).unwrap();
        let connection = parse(&Messages::new(&bytes).next().unwrap().unwrap()).unwrap();
        assert_eq!(connection.orig_source, Some(ip("2001:db8::9")));
        assert!(connection.involves(ip("fd00:1::2")));
        assert!(!connection.involves(ip("2001:db8::1")));
    }

    #[test]
    fn a_connection_without_its_tuple_is_an_error() {
        let mut message = MessageBuilder::new(0x100, 0);
        message.raw(&[2, 0, 0, 0]);
        let bytes = message.finish(1, 0).unwrap();
        assert!(parse(&Messages::new(&bytes).next().unwrap().unwrap()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root"]
    fn a_flush_forgets_an_addresses_connections() {
        crate::linux_test::in_new_netns(|| {
            // lo up, and a table with NAT chains, so the namespace tracks
            // connections.
            crate::route::Rtnetlink::new()
                .unwrap()
                .change(1, crate::route::LinkChange::Up(true))
                .unwrap();
            crate::nft::Nftables::new()
                .unwrap()
                .install(&[], &[], &crate::nft::ClosedRouting::default(), &[])
                .unwrap();
            let client = std::net::UdpSocket::bind("127.0.0.2:0").unwrap();
            client.send_to(b"x", "127.0.0.1:9").unwrap();
            let first = super::flush_address(ip("127.0.0.2")).unwrap();
            assert!(first >= 1, "{first}");
            assert_eq!(super::flush_address(ip("127.0.0.2")).unwrap(), 0);
            // IPv6 too.
            let client = std::net::UdpSocket::bind("[::1]:0").unwrap();
            client.send_to(b"x", "[::1]:9").unwrap();
            assert!(super::flush_address(ip("::1")).unwrap() >= 1);
        });
    }
}
