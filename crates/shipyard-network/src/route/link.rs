//! Links (network interfaces): what an `RTM_NEWLINK` says about one, and the
//! requests that make, change and remove them.
//!
//! A link message starts with `ifinfomsg` (family, device type, index, flags
//! and which flags to change), then `IFLA_*` attributes: the name, MTU, the
//! bridge it's a port of (`IFLA_MASTER`), and `IFLA_LINKINFO`, which nests
//! the kind (`bridge`, `veth`) and that kind's own settings.

use thiserror::Error;

use super::consts::{
    AF_BRIDGE, AF_UNSPEC, IFF_UP, IFLA_ADDRESS, IFLA_BRPORT_MODE, IFLA_IFNAME, IFLA_INFO_DATA,
    IFLA_INFO_KIND, IFLA_LINKINFO, IFLA_MASTER, IFLA_MTU, IFLA_NET_NS_FD, IFLA_PROTINFO,
    RTM_DELLINK, RTM_GETLINK, RTM_NEWLINK, RTM_SETLINK, VETH_INFO_PEER,
};
use crate::netlink::message::{read_array, read_u16, read_u32};
use crate::netlink::{Message, MessageBuilder, ParseError, flag};

/// `ifinfomsg`: the fixed header of a link message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LinkHeader {
    /// The address family (`AF_UNSPEC`, or `AF_BRIDGE` for bridge-port settings).
    pub(crate) family: u8,
    /// The device type (`ARPHRD_*`).
    pub(crate) device_type: u16,
    /// The interface index (0: none, or "pick one" when creating).
    pub(crate) index: u32,
    /// Its flags (`IFF_*`).
    pub(crate) flags: u32,
    /// Which flags a request changes.
    pub(crate) change: u32,
}

impl LinkHeader {
    /// Its size.
    pub(crate) const LEN: usize = 16;

    /// Its bytes.
    pub(crate) fn encode(&self) -> [u8; Self::LEN] {
        let mut bytes = [0; Self::LEN];
        bytes[0] = self.family;
        bytes[2..4].copy_from_slice(&self.device_type.to_ne_bytes());
        bytes[4..8].copy_from_slice(&self.index.to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.flags.to_ne_bytes());
        bytes[12..16].copy_from_slice(&self.change.to_ne_bytes());
        bytes
    }

    /// It, from exactly its bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<LinkHeader, ParseError> {
        let length = || ParseError::Length("link header");
        if bytes.len() != Self::LEN {
            return Err(length());
        }
        let [family] = read_array(bytes, 0).ok_or_else(length)?;
        Ok(LinkHeader {
            family,
            device_type: read_u16(bytes, 2).ok_or_else(length)?,
            index: read_u32(bytes, 4).ok_or_else(length)?,
            flags: read_u32(bytes, 8).ok_or_else(length)?,
            change: read_u32(bytes, 12).ok_or_else(length)?,
        })
    }
}

/// A link, as the kernel describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Its interface index.
    pub index: u32,
    /// Its name. The kernel allows any bytes but a few; one that isn't UTF-8
    /// is shown with U+FFFD in their place (no link Shipyard makes has one).
    pub name: String,
    /// Its kind (`bridge`, `veth`, …), for links made by a driver that says.
    pub kind: Option<String>,
    /// Its flags (`IFF_*`).
    pub flags: u32,
    /// Its MTU.
    pub mtu: Option<u32>,
    /// The bridge it's a port of.
    pub master: Option<u32>,
    /// Its hardware address.
    pub mac: Option<Vec<u8>>,
}

impl Link {
    /// The link an `RTM_NEWLINK` describes (`None` for another message).
    pub(crate) fn parse(message: &Message<'_>) -> Result<Option<Link>, ParseError> {
        if message.kind != RTM_NEWLINK {
            return Ok(None);
        }
        let (header, attrs) = message.split(LinkHeader::LEN)?;
        let header = LinkHeader::decode(header)?;
        let mut link = Link {
            index: header.index,
            name: String::new(),
            kind: None,
            flags: header.flags,
            mtu: None,
            master: None,
            mac: None,
        };
        let mut named = false;
        for attr in attrs {
            let attr = attr?;
            match attr.kind {
                // Lossy: one odd name on the host mustn't fail every listing.
                IFLA_IFNAME => {
                    link.name = String::from_utf8_lossy(attr.c_bytes()?).into_owned();
                    named = true;
                }
                IFLA_MTU => link.mtu = Some(attr.u32()?),
                IFLA_MASTER => link.master = Some(attr.u32()?),
                IFLA_ADDRESS => link.mac = Some(attr.value.to_vec()),
                IFLA_LINKINFO => {
                    for info in attr.nested() {
                        let info = info?;
                        if info.kind == IFLA_INFO_KIND {
                            link.kind = Some(String::from_utf8_lossy(info.c_bytes()?).into_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        if !named {
            return Err(ParseError::Missing("link name"));
        }
        Ok(Some(link))
    }

    /// Whether it's administratively up.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.flags & IFF_UP != 0
    }
}

/// What's wrong with an interface name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{name:?} can't be a network interface's name: {reason}")]
pub struct NameError {
    /// The name.
    pub name: String,
    /// Why not.
    pub reason: String,
}

/// The longest interface name (`IFNAMSIZ` - 1).
pub const MAX_NAME: usize = 15;

/// Checks `name` at least as strictly as the kernel (`dev_valid_name`,
/// `dev_get_valid_name`), so a name it would refuse, or change, gets a clear
/// error before any request: 1–15 printable ASCII characters, not `.` or
/// `..`, with no `/`, `:` or space, and no `%` (the kernel reads `%d` as "the
/// next free number", so the link would get another name).
pub fn validate_name(name: &str) -> Result<(), NameError> {
    let reason = if name.is_empty() {
        Some("it's empty".to_owned())
    } else if name.len() > MAX_NAME {
        Some(format!("it's longer than {MAX_NAME} bytes"))
    } else if name == "." || name == ".." {
        Some("it's . or ..".to_owned())
    } else if !name.bytes().all(|byte| byte.is_ascii_graphic()) {
        Some("it has a space, a control character or a character outside ASCII".to_owned())
    } else if name.contains(['/', ':', '%']) {
        Some("it has a /, : or %".to_owned())
    } else {
        None
    };
    match reason {
        Some(reason) => Err(NameError {
            name: name.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// A link request with `header`.
fn link_message(kind: u16, flags: u16, header: LinkHeader) -> MessageBuilder {
    let mut message = MessageBuilder::new(kind, flags);
    message.raw(&header.encode());
    message
}

/// `RTM_GETLINK` for the link named `name`.
pub(crate) fn get_by_name(name: &str) -> MessageBuilder {
    let mut message = link_message(RTM_GETLINK, 0, LinkHeader::default());
    message.attr_str(IFLA_IFNAME, name);
    message
}

/// `RTM_GETLINK` for every link (a dump).
pub(crate) fn get_all() -> MessageBuilder {
    link_message(RTM_GETLINK, 0, LinkHeader::default())
}

/// `RTM_NEWLINK` for a new bridge named `name`.
pub(crate) fn new_bridge(name: &str) -> MessageBuilder {
    let mut message = link_message(
        RTM_NEWLINK,
        flag::CREATE | flag::EXCL,
        LinkHeader::default(),
    );
    message
        .attr_str(IFLA_IFNAME, name)
        .nest(IFLA_LINKINFO, |info| {
            info.attr_str(IFLA_INFO_KIND, "bridge");
        });
    message
}

/// `RTM_NEWLINK` for a veth pair: `name` here, and its peer `peer_name`,
/// born in the network namespace `peer_netns` (a descriptor of it) when one
/// is given, so it never exists here, under any name, and with the hardware
/// address `peer_mac` if one is given.
pub(crate) fn new_veth(
    name: &str,
    peer_name: &str,
    peer_netns: Option<u32>,
    peer_mac: Option<[u8; 6]>,
) -> MessageBuilder {
    let mut message = link_message(
        RTM_NEWLINK,
        flag::CREATE | flag::EXCL,
        LinkHeader::default(),
    );
    message
        .attr_str(IFLA_IFNAME, name)
        .nest(IFLA_LINKINFO, |info| {
            info.attr_str(IFLA_INFO_KIND, "veth")
                .nest(IFLA_INFO_DATA, |data| {
                    data.nest(VETH_INFO_PEER, |peer| {
                        // The peer is described as a link message of its own: its
                        // `ifinfomsg`, then its attributes.
                        peer.raw(&LinkHeader::default().encode())
                            .attr_str(IFLA_IFNAME, peer_name);
                        if let Some(fd) = peer_netns {
                            peer.attr_u32(IFLA_NET_NS_FD, fd);
                        }
                        if let Some(mac) = peer_mac {
                            peer.attr(IFLA_ADDRESS, &mac);
                        }
                    });
                });
        });
    message
}

/// A change to an existing link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkChange<'a> {
    /// Bring it up or down.
    Up(bool),
    /// Make it a port of the bridge with this index (0: of none).
    Master(u32),
    /// Set its MTU.
    Mtu(u32),
    /// Rename it (it must be down).
    Name(&'a str),
    /// Set its hardware address.
    Address([u8; 6]),
}

impl LinkChange<'_> {
    /// What making this change to the link with `index` is, for messages
    /// ("renaming the network interface 5 to eth0").
    pub(crate) fn describe(&self, index: u32) -> String {
        let link = format!("the network interface {index}");
        match self {
            LinkChange::Up(true) => format!("bringing up {link}"),
            LinkChange::Up(false) => format!("bringing down {link}"),
            LinkChange::Master(0) => format!("removing {link} from its bridge"),
            LinkChange::Master(bridge) => format!("adding {link} to the bridge {bridge}"),
            LinkChange::Mtu(mtu) => format!("setting the MTU of {link} to {mtu}"),
            LinkChange::Name(name) => format!("renaming {link} to {name}"),
            LinkChange::Address(mac) => format!(
                "setting the hardware address of {link} to {}",
                crate::store::format_mac(*mac)
            ),
        }
    }
}

/// `RTM_NEWLINK` changing the link with `index`.
pub(crate) fn change(index: u32, change: LinkChange<'_>) -> MessageBuilder {
    let mut header = LinkHeader {
        family: AF_UNSPEC,
        index,
        ..LinkHeader::default()
    };
    if let LinkChange::Up(up) = change {
        header.change = IFF_UP;
        header.flags = if up { IFF_UP } else { 0 };
    }
    let mut message = link_message(RTM_NEWLINK, 0, header);
    match change {
        LinkChange::Up(_) => {}
        LinkChange::Master(master) => {
            message.attr_u32(IFLA_MASTER, master);
        }
        LinkChange::Mtu(mtu) => {
            message.attr_u32(IFLA_MTU, mtu);
        }
        LinkChange::Name(name) => {
            message.attr_str(IFLA_IFNAME, name);
        }
        LinkChange::Address(mac) => {
            message.attr(IFLA_ADDRESS, &mac);
        }
    }
    message
}

/// `RTM_NEWLINK` moving the link with `index` into the network namespace
/// the descriptor `netns` refers to, renamed `name` there if one is given
/// (one request: it never appears there under its old name).
pub(crate) fn move_to(index: u32, netns: u32, name: Option<&str>) -> MessageBuilder {
    let mut message = link_message(
        RTM_NEWLINK,
        0,
        LinkHeader {
            index,
            ..LinkHeader::default()
        },
    );
    message.attr_u32(IFLA_NET_NS_FD, netns);
    if let Some(name) = name {
        message.attr_str(IFLA_IFNAME, name);
    }
    message
}

/// `RTM_SETLINK` turning hairpin mode on or off for the bridge port with
/// `index`: whether the bridge sends a frame back out of the port it came
/// in on (a container reaching itself through a published port). The
/// settings must be flagged as nested: unflagged, the kernel would take the
/// value as the port's state.
pub(crate) fn set_hairpin(index: u32, on: bool) -> MessageBuilder {
    let header = LinkHeader {
        family: AF_BRIDGE,
        index,
        ..LinkHeader::default()
    };
    let mut message = link_message(RTM_SETLINK, 0, header);
    message.nest(IFLA_PROTINFO, |info| {
        info.attr_u8(IFLA_BRPORT_MODE, u8::from(on));
    });
    message
}

/// `RTM_DELLINK` for the link with `index`.
pub(crate) fn delete(index: u32) -> MessageBuilder {
    link_message(
        RTM_DELLINK,
        0,
        LinkHeader {
            index,
            ..LinkHeader::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::message::{Attrs, Messages};

    #[test]
    fn the_header_round_trips() {
        let header = LinkHeader {
            family: 7,
            device_type: 1,
            index: 42,
            flags: 0x1043,
            change: 1,
        };
        assert_eq!(LinkHeader::decode(&header.encode()), Ok(header));
        assert!(LinkHeader::decode(&[0; 15]).is_err());
    }

    #[test]
    fn interface_names_follow_the_kernels_rules() {
        for good in ["eth0", "shipyard0", "shy-0123456789a", "a"] {
            assert_eq!(validate_name(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "shy-0123456789ab",
            ".",
            "..",
            "a/b",
            "a:1",
            "a b",
            "a\tb",
            "\u{7f}",
            "shy%d",
            "a%b",
            "à",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_name_that_isnt_utf8_is_read_lossily() {
        // The kernel allows it (`ip link add $'\xff' type dummy`); one such
        // link mustn't make every listing fail.
        let mut message = MessageBuilder::new(RTM_NEWLINK, flag::MULTI);
        message
            .raw(&LinkHeader::default().encode())
            .attr(IFLA_IFNAME, b"a\xff\0");
        let bytes = message.finish(1, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(Link::parse(&message).unwrap().unwrap().name, "a\u{fffd}");
    }

    #[test]
    fn a_change_is_described_in_order() {
        assert_eq!(
            LinkChange::Name("eth0").describe(5),
            "renaming the network interface 5 to eth0"
        );
        assert_eq!(
            LinkChange::Master(3).describe(5),
            "adding the network interface 5 to the bridge 3"
        );
        assert_eq!(
            LinkChange::Up(true).describe(5),
            "bringing up the network interface 5"
        );
    }

    /// An `RTM_NEWLINK` for `lo` shaped like the kernel's: the attributes
    /// this parser reads, and one it skips. (The Linux tests read the
    /// kernel's own.)
    fn loopback() -> Vec<u8> {
        let mut message = MessageBuilder::new(RTM_NEWLINK, flag::MULTI);
        message
            .raw(
                &LinkHeader {
                    family: 0,
                    device_type: 772,
                    index: 1,
                    flags: 0x10049,
                    change: 0,
                }
                .encode(),
            )
            .attr_str(IFLA_IFNAME, "lo")
            .attr_u32(13, 1000) // IFLA_TXQLEN: skipped
            .attr_u32(IFLA_MTU, 65536)
            .attr(IFLA_ADDRESS, &[0; 6]);
        message.finish(1, 77).unwrap()
    }

    #[test]
    fn a_link_is_read_from_its_message() {
        let bytes = loopback();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        let link = Link::parse(&message).unwrap().unwrap();
        assert_eq!(
            link,
            Link {
                index: 1,
                name: "lo".into(),
                kind: None,
                flags: 0x10049,
                mtu: Some(65536),
                master: None,
                mac: Some(vec![0; 6]),
            }
        );
        assert!(link.is_up());
    }

    #[test]
    fn a_link_without_a_name_is_an_error() {
        let mut message = MessageBuilder::new(RTM_NEWLINK, 0);
        message.raw(&LinkHeader::default().encode());
        let bytes = message.finish(0, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert!(Link::parse(&message).is_err());
    }

    #[test]
    fn another_message_is_not_a_link() {
        let bytes = MessageBuilder::new(RTM_DELLINK, 0).finish(0, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(Link::parse(&message), Ok(None));
    }

    #[test]
    fn a_veth_peer_is_a_nested_link_message_in_its_namespace() {
        let bytes = new_veth("shy-a", "eth0", Some(9), Some([2, 0x42, 10, 88, 0, 2]))
            .finish(1, 0)
            .unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(message.flags, flag::CREATE | flag::EXCL);
        let (_, attrs) = message.split(LinkHeader::LEN).unwrap();
        let attrs: Vec<_> = attrs.collect::<Result<_, _>>().unwrap();
        assert_eq!(attrs[0].string().unwrap(), "shy-a");
        let info: Vec<_> = attrs[1].nested().collect::<Result<_, _>>().unwrap();
        assert!(attrs[1].nested);
        assert_eq!(
            (info[0].kind, info[0].string().unwrap()),
            (IFLA_INFO_KIND, "veth")
        );
        let data: Vec<_> = info[1].nested().collect::<Result<_, _>>().unwrap();
        assert_eq!(data[0].kind, VETH_INFO_PEER);
        // the peer: an ifinfomsg, then its own attributes
        let peer = data[0].value;
        assert_eq!(
            LinkHeader::decode(&peer[..LinkHeader::LEN]),
            Ok(LinkHeader::default())
        );
        let peer: Vec<_> = Attrs::new(&peer[LinkHeader::LEN..])
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(peer[0].string().unwrap(), "eth0");
        assert_eq!((peer[1].kind, peer[1].u32().unwrap()), (IFLA_NET_NS_FD, 9));
        assert_eq!(
            (peer[2].kind, peer[2].value),
            (IFLA_ADDRESS, &[2, 0x42, 10, 88, 0, 2][..])
        );
    }

    #[test]
    fn bringing_up_changes_only_the_up_flag() {
        let bytes = change(5, LinkChange::Up(true)).finish(0, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        let (header, _) = message.split(LinkHeader::LEN).unwrap();
        let header = LinkHeader::decode(header).unwrap();
        assert_eq!(
            (header.index, header.flags, header.change),
            (5, IFF_UP, IFF_UP)
        );
        let bytes = change(5, LinkChange::Up(false)).finish(0, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        let header = LinkHeader::decode(message.split(LinkHeader::LEN).unwrap().0).unwrap();
        assert_eq!((header.flags, header.change), (0, IFF_UP));
    }

    #[test]
    fn hairpin_is_a_bridge_port_setting() {
        let bytes = set_hairpin(5, true).finish(0, 0).unwrap();
        let message = Messages::new(&bytes).next().unwrap().unwrap();
        assert_eq!(message.kind, RTM_SETLINK);
        let (header, attrs) = message.split(LinkHeader::LEN).unwrap();
        assert_eq!(LinkHeader::decode(header).unwrap().family, AF_BRIDGE);
        let protinfo = attrs.into_iter().next().unwrap().unwrap();
        assert_eq!(protinfo.kind, IFLA_PROTINFO);
        assert!(protinfo.nested);
        let mode = protinfo.nested().next().unwrap().unwrap();
        assert_eq!((mode.kind, mode.u8().unwrap()), (IFLA_BRPORT_MODE, 1));
    }
}
