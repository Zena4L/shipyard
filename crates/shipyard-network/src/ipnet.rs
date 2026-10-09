//! IP networks: an address with a prefix length (`10.88.0.1/16`), and the
//! arithmetic on them that addresses and subnets need.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use thiserror::Error;

/// An address family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// IPv4.
    V4,
    /// IPv6.
    V6,
}

impl Family {
    /// The family of `addr`.
    #[must_use]
    pub fn of(addr: IpAddr) -> Family {
        if addr.is_ipv4() {
            Family::V4
        } else {
            Family::V6
        }
    }

    /// Linux's number for it (`AF_INET`, `AF_INET6`), as netlink carries it.
    #[must_use]
    pub(crate) fn af(self) -> u8 {
        match self {
            Family::V4 => crate::route::consts::AF_INET,
            Family::V6 => crate::route::consts::AF_INET6,
        }
    }
}

impl std::fmt::Display for Family {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Family::V4 => "IPv4",
            Family::V6 => "IPv6",
        })
    }
}

/// An address and a prefix length: an interface's address
/// (`10.88.0.2/16`), or a network (`10.88.0.0/16`). Stored as its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

/// What's wrong with a network.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IpNetError {
    /// Not `ADDRESS/PREFIX`.
    #[error("{0:?} isn't an address with a prefix length (like 10.88.0.0/16)")]
    Syntax(String),
    /// A prefix longer than the address.
    #[error("a prefix length of {prefix} is longer than an IPv{version} address")]
    Prefix {
        /// The prefix length given.
        prefix: u8,
        /// 4 or 6.
        version: u8,
    },
}

impl IpNet {
    /// `addr/prefix`, if the prefix fits the address.
    pub fn new(addr: IpAddr, prefix: u8) -> Result<IpNet, IpNetError> {
        let (max, version) = if addr.is_ipv4() { (32, 4) } else { (128, 6) };
        if prefix > max {
            return Err(IpNetError::Prefix { prefix, version });
        }
        Ok(IpNet { addr, prefix })
    }

    /// Every address of `family`: `0.0.0.0/0`, or `::/0`.
    #[must_use]
    pub fn any(family: Family) -> IpNet {
        let addr = match family {
            Family::V4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Family::V6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        IpNet { addr, prefix: 0 }
    }

    /// Its address family.
    #[must_use]
    pub fn family(&self) -> Family {
        Family::of(self.addr)
    }

    /// The address.
    #[must_use]
    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    /// The prefix length.
    #[must_use]
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Whether it's IPv4.
    #[must_use]
    pub fn is_ipv4(&self) -> bool {
        self.addr.is_ipv4()
    }

    /// The network it's in: the address with every host bit cleared.
    #[must_use]
    pub fn network(&self) -> IpNet {
        IpNet {
            addr: from_bits(self.addr, bits(self.addr) & self.mask()),
            prefix: self.prefix,
        }
    }

    /// Whether `addr` is in this network.
    #[must_use]
    pub fn contains(&self, addr: IpAddr) -> bool {
        addr.is_ipv4() == self.addr.is_ipv4()
            && bits(addr) & self.mask() == bits(self.addr) & self.mask()
    }

    /// Whether this network and `other` share any address (one holds the
    /// other: networks never partly overlap).
    #[must_use]
    pub fn overlaps(&self, other: &IpNet) -> bool {
        if self.is_ipv4() != other.is_ipv4() {
            return false;
        }
        let (shorter, longer) = if self.prefix <= other.prefix {
            (self, other)
        } else {
            (other, self)
        };
        shorter.contains(longer.addr)
    }

    /// The `n`th address of the network (the network address is the 0th),
    /// if the network has that many.
    #[must_use]
    pub fn nth(&self, n: u128) -> Option<IpAddr> {
        let host_bits = self.width() - u32::from(self.prefix);
        if host_bits < 128 && n >> host_bits != 0 {
            return None;
        }
        let base = bits(self.addr) & self.mask();
        Some(from_bits(self.addr, base | n))
    }

    /// How many addresses the network has (saturating at `u128::MAX`).
    #[must_use]
    pub fn size(&self) -> u128 {
        let host_bits = self.width() - u32::from(self.prefix);
        1u128.checked_shl(host_bits).unwrap_or(u128::MAX)
    }

    /// The IPv4 broadcast address (none for IPv6, or a /31 or /32).
    #[must_use]
    pub fn broadcast(&self) -> Option<Ipv4Addr> {
        match self.addr {
            IpAddr::V4(addr) if self.prefix < 31 => {
                Some(Ipv4Addr::from(u32::from(addr) | !mask32(self.prefix)))
            }
            _ => None,
        }
    }

    fn width(&self) -> u32 {
        if self.is_ipv4() { 32 } else { 128 }
    }

    /// The prefix as a mask over the address's bits (an IPv4 address in the
    /// low 32).
    fn mask(&self) -> u128 {
        match self.addr {
            IpAddr::V4(_) => u128::from(mask32(self.prefix)),
            IpAddr::V6(_) => u128::MAX
                .checked_shl(128 - u32::from(self.prefix))
                .unwrap_or(0),
        }
    }
}

fn mask32(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

fn bits(addr: IpAddr) -> u128 {
    match addr {
        IpAddr::V4(addr) => u128::from(u32::from(addr)),
        IpAddr::V6(addr) => u128::from(addr),
    }
}

/// `bits` as an address of `like`'s family (an IPv4 address takes the low
/// 32 bits).
fn from_bits(like: IpAddr, bits: u128) -> IpAddr {
    match like {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the low 32 bits are the address"
        )]
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(bits as u32)),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(bits)),
    }
}

impl FromStr for IpNet {
    type Err = IpNetError;

    fn from_str(text: &str) -> Result<IpNet, IpNetError> {
        let syntax = || IpNetError::Syntax(text.to_owned());
        let (addr, prefix) = text.split_once('/').ok_or_else(syntax)?;
        let addr: IpAddr = addr.parse().map_err(|_| syntax())?;
        // Digits only: `u8::from_str` would take a `+`.
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(syntax());
        }
        let prefix: u8 = prefix.parse().map_err(|_| syntax())?;
        IpNet::new(addr, prefix)
    }
}

impl TryFrom<String> for IpNet {
    type Error = IpNetError;

    fn try_from(text: String) -> Result<IpNet, IpNetError> {
        text.parse()
    }
}

impl From<IpNet> for String {
    fn from(net: IpNet) -> String {
        net.to_string()
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(text: &str) -> IpNet {
        text.parse().unwrap()
    }

    #[test]
    fn parsing_and_display() {
        assert_eq!(net("10.88.0.1/16").to_string(), "10.88.0.1/16");
        assert_eq!(net("fd00::1/64").to_string(), "fd00::1/64");
        assert_eq!(net("0.0.0.0/0").prefix(), 0);
        for bad in [
            "10.88.0.1",
            "10.88.0.1/",
            "10.88.0.1/+8",
            "x/8",
            "10.88.0.1/8/8",
            "10.88.0.1/256",
        ] {
            assert!(
                matches!(bad.parse::<IpNet>(), Err(IpNetError::Syntax(_))),
                "{bad}"
            );
        }
        assert_eq!(
            "10.0.0.0/33".parse::<IpNet>(),
            Err(IpNetError::Prefix {
                prefix: 33,
                version: 4
            })
        );
        assert_eq!(
            "::/129".parse::<IpNet>(),
            Err(IpNetError::Prefix {
                prefix: 129,
                version: 6
            })
        );
    }

    #[test]
    fn the_network_clears_host_bits() {
        assert_eq!(net("10.88.3.7/16").network(), net("10.88.0.0/16"));
        assert_eq!(net("10.88.3.7/32").network(), net("10.88.3.7/32"));
        assert_eq!(net("10.88.3.7/0").network(), net("0.0.0.0/0"));
        assert_eq!(net("fd00:1:2:3::9/48").network(), net("fd00:1:2::/48"));
        assert_eq!(net("fd00::9/0").network(), net("::/0"));
    }

    #[test]
    fn containment() {
        let subnet = net("10.88.0.0/16");
        assert!(subnet.contains("10.88.255.255".parse().unwrap()));
        assert!(!subnet.contains("10.89.0.0".parse().unwrap()));
        assert!(!subnet.contains("::ffff:10.88.0.1".parse().unwrap()));
        assert!(net("0.0.0.0/0").contains("192.0.2.1".parse().unwrap()));
        assert!(net("fd00::/8").contains("fdff::1".parse().unwrap()));
        assert!(!net("fd00::/8").contains("fe00::1".parse().unwrap()));
    }

    #[test]
    fn overlap() {
        assert!(net("10.88.0.0/16").overlaps(&net("10.88.4.0/24")));
        assert!(net("10.88.4.0/24").overlaps(&net("10.88.0.0/16")));
        assert!(!net("10.88.0.0/16").overlaps(&net("10.89.0.0/16")));
        assert!(net("0.0.0.0/0").overlaps(&net("10.0.0.0/8")));
        assert!(!net("10.0.0.0/8").overlaps(&net("fd00::/8")));
        assert!(net("10.0.0.0/8").overlaps(&net("10.0.0.0/8")));
    }

    #[test]
    fn nth_address() {
        let subnet = net("10.88.0.0/16");
        assert_eq!(subnet.nth(1), Some("10.88.0.1".parse().unwrap()));
        assert_eq!(subnet.nth(65535), Some("10.88.255.255".parse().unwrap()));
        assert_eq!(subnet.nth(65536), None);
        assert_eq!(net("10.0.0.5/32").nth(0), Some("10.0.0.5".parse().unwrap()));
        assert_eq!(net("10.0.0.5/32").nth(1), None);
        assert_eq!(net("fd00::/64").nth(2), Some("fd00::2".parse().unwrap()));
        assert_eq!(
            net("::/0").nth(u128::MAX),
            Some("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap())
        );
    }

    #[test]
    fn sizes_and_broadcast() {
        assert_eq!(net("10.88.0.0/16").size(), 65536);
        assert_eq!(net("10.88.0.0/32").size(), 1);
        assert_eq!(net("::/0").size(), u128::MAX);
        assert_eq!(
            net("10.88.1.2/16").broadcast(),
            Some("10.88.255.255".parse().unwrap())
        );
        assert_eq!(net("10.88.1.2/31").broadcast(), None);
        assert_eq!(net("0.0.0.0/0").broadcast(), Some(Ipv4Addr::BROADCAST));
        assert_eq!(net("fd00::1/64").broadcast(), None);
    }
}
