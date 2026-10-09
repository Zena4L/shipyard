//! Interface addresses: `RTM_NEWADDR` and what the kernel lists.
//!
//! An address message starts with `ifaddrmsg` (family, prefix length, flags,
//! scope, interface index), then `IFA_*` attributes. For IPv4, `IFA_LOCAL` is
//! the interface's own address and `IFA_ADDRESS` its peer's (the same, except
//! on point-to-point links); for IPv6 only `IFA_ADDRESS` is used.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::consts::{
    AF_INET, AF_INET6, AF_UNSPEC, IFA_ADDRESS, IFA_BROADCAST, IFA_F_NODAD, IFA_LOCAL, RTM_GETADDR,
    RTM_NEWADDR,
};
use crate::ipnet::IpNet;
use crate::netlink::message::Attr;
use crate::netlink::{Message, MessageBuilder, ParseError, flag};

/// `ifaddrmsg`: the fixed header of an address message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AddressHeader {
    /// `AF_INET` or `AF_INET6` (`AF_UNSPEC` to list both).
    pub(crate) family: u8,
    /// The prefix length.
    pub(crate) prefix: u8,
    /// `IFA_F_*` flags.
    pub(crate) flags: u8,
    /// Its scope (`RT_SCOPE_*`).
    pub(crate) scope: u8,
    /// The interface's index.
    pub(crate) index: u32,
}

impl AddressHeader {
    /// Its size.
    pub(crate) const LEN: usize = 8;

    /// Its bytes.
    pub(crate) fn encode(self) -> [u8; Self::LEN] {
        let [i0, i1, i2, i3] = self.index.to_ne_bytes();
        [
            self.family,
            self.prefix,
            self.flags,
            self.scope,
            i0,
            i1,
            i2,
            i3,
        ]
    }

    /// It, from exactly its bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<AddressHeader, ParseError> {
        let bytes: &[u8; Self::LEN] = bytes
            .try_into()
            .map_err(|_| ParseError::Length("address header"))?;
        let [family, prefix, flags, scope, i0, i1, i2, i3] = *bytes;
        Ok(AddressHeader {
            family,
            prefix,
            flags,
            scope,
            index: u32::from_ne_bytes([i0, i1, i2, i3]),
        })
    }
}

/// An address on an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    /// The interface's index.
    pub index: u32,
    /// The address, with its prefix length.
    pub net: IpNet,
}

impl Address {
    /// The address an `RTM_NEWADDR` describes (`None` for another message,
    /// or a family other than IPv4 and IPv6).
    pub(crate) fn parse(message: &Message<'_>) -> Result<Option<Address>, ParseError> {
        if message.kind != RTM_NEWADDR {
            return Ok(None);
        }
        let (header, attrs) = message.split(AddressHeader::LEN)?;
        let header = AddressHeader::decode(header)?;
        if header.family != AF_INET && header.family != AF_INET6 {
            return Ok(None);
        }
        let (mut local, mut address) = (None, None);
        for attr in attrs {
            let attr = attr?;
            match attr.kind {
                IFA_LOCAL => local = Some(ip(header.family, &attr)?),
                IFA_ADDRESS => address = Some(ip(header.family, &attr)?),
                _ => {}
            }
        }
        let addr = local.or(address).ok_or(ParseError::Missing("address"))?;
        let net = IpNet::new(addr, header.prefix).map_err(|_| ParseError::Invalid("prefix"))?;
        Ok(Some(Address {
            index: header.index,
            net,
        }))
    }
}

/// An address attribute's value, of `family`.
pub(crate) fn ip(family: u8, attr: &Attr<'_>) -> Result<IpAddr, ParseError> {
    Ok(if family == AF_INET6 {
        IpAddr::V6(Ipv6Addr::from(attr.fixed::<16>()?))
    } else {
        IpAddr::V4(Ipv4Addr::from(attr.fixed::<4>()?))
    })
}

/// An address's bytes, in network order (as rtnetlink wants them).
pub(crate) fn ip_bytes(addr: IpAddr) -> Vec<u8> {
    match addr {
        IpAddr::V4(addr) => addr.octets().to_vec(),
        IpAddr::V6(addr) => addr.octets().to_vec(),
    }
}

/// `RTM_NEWADDR` adding `net` (an address and its prefix length) to the
/// interface with `index`. IPv4 addresses get their network's broadcast
/// address, as `ip address add … broadcast +` gives. IPv6 addresses skip
/// duplicate address detection (`IFA_F_NODAD`), as container engines'
/// do: otherwise the address is "tentative" for a second or two, and a
/// program binding it in that time gets `EADDRNOTAVAIL`. The network's
/// addresses are handed out by Shipyard, so there's nothing to detect.
pub(crate) fn add(index: u32, net: IpNet) -> MessageBuilder {
    let header = AddressHeader {
        family: net.family().af(),
        prefix: net.prefix(),
        flags: if net.is_ipv4() { 0 } else { IFA_F_NODAD },
        index,
        ..AddressHeader::default()
    };
    let mut message = MessageBuilder::new(RTM_NEWADDR, flag::CREATE | flag::EXCL);
    let addr = ip_bytes(net.addr());
    message
        .raw(&header.encode())
        .attr(IFA_LOCAL, &addr)
        .attr(IFA_ADDRESS, &addr);
    if let Some(broadcast) = net.broadcast() {
        message.attr(IFA_BROADCAST, &broadcast.octets());
    }
    message
}

/// `RTM_GETADDR` for the addresses of the interface with `index` (every
/// interface's for `None`): a dump.
pub(crate) fn get(index: Option<u32>) -> MessageBuilder {
    let header = AddressHeader {
        family: AF_UNSPEC,
        index: index.unwrap_or(0),
        ..AddressHeader::default()
    };
    let mut message = MessageBuilder::new(RTM_GETADDR, 0);
    message.raw(&header.encode());
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::message::Messages;

    #[test]
    fn the_header_round_trips() {
        let header = AddressHeader {
            family: AF_INET6,
            prefix: 64,
            flags: 0x80,
            scope: 253,
            index: 9,
        };
        assert_eq!(AddressHeader::decode(&header.encode()), Ok(header));
    }

    #[test]
    fn an_ipv4_address_carries_its_broadcast() {
        let bytes = add(3, "10.88.0.1/16".parse().unwrap())
            .finish(1, 0)
            .unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        let (header, attrs) = message.split(AddressHeader::LEN).unwrap();
        let header = AddressHeader::decode(header).unwrap();
        assert_eq!(
            (header.family, header.prefix, header.index),
            (AF_INET, 16, 3)
        );
        let attrs: Vec<_> = attrs.collect::<Result<_, _>>().unwrap();
        let values: Vec<_> = attrs.iter().map(|attr| (attr.kind, attr.value)).collect();
        assert_eq!(
            values,
            [
                (IFA_LOCAL, &[10, 88, 0, 1][..]),
                (IFA_ADDRESS, &[10, 88, 0, 1][..]),
                (IFA_BROADCAST, &[10, 88, 255, 255][..])
            ]
        );
    }

    #[test]
    fn an_ipv6_address_has_no_broadcast_and_skips_dad() {
        let bytes = add(3, "fd00::2/64".parse().unwrap()).finish(1, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        let (header, attrs) = message.split(AddressHeader::LEN).unwrap();
        assert_eq!(AddressHeader::decode(header).unwrap().flags, IFA_F_NODAD);
        assert_eq!(attrs.count(), 2);
    }

    #[test]
    fn an_address_is_read_back() {
        // What the kernel lists for that request.
        let mut message = MessageBuilder::new(RTM_NEWADDR, flag::MULTI);
        message
            .raw(
                &AddressHeader {
                    family: AF_INET,
                    prefix: 16,
                    flags: 0x80,
                    scope: 0,
                    index: 3,
                }
                .encode(),
            )
            .attr(IFA_ADDRESS, &[10, 88, 0, 1])
            .attr(IFA_LOCAL, &[10, 88, 0, 1])
            .attr(IFA_BROADCAST, &[10, 88, 255, 255])
            .attr_str(3, "shipyard0"); // IFA_LABEL: skipped
        let bytes = message.finish(1, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(
            Address::parse(&message),
            Ok(Some(Address {
                index: 3,
                net: "10.88.0.1/16".parse().unwrap()
            }))
        );
    }

    #[test]
    fn a_wrong_sized_address_is_an_error() {
        let mut message = MessageBuilder::new(RTM_NEWADDR, 0);
        message
            .raw(
                &AddressHeader {
                    family: AF_INET6,
                    prefix: 64,
                    ..AddressHeader::default()
                }
                .encode(),
            )
            .attr(IFA_ADDRESS, &[10, 88, 0, 1]);
        let bytes = message.finish(1, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert!(Address::parse(&message).is_err());
    }

    #[test]
    fn an_impossible_prefix_is_an_error() {
        let mut message = MessageBuilder::new(RTM_NEWADDR, 0);
        message
            .raw(
                &AddressHeader {
                    family: AF_INET,
                    prefix: 33,
                    ..AddressHeader::default()
                }
                .encode(),
            )
            .attr(IFA_ADDRESS, &[10, 88, 0, 1]);
        let bytes = message.finish(1, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert!(Address::parse(&message).is_err());
    }
}
