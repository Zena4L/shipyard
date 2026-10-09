//! rtnetlink's numbers (`linux/rtnetlink.h`, `linux/if_link.h`,
//! `linux/if_addr.h`), as Shipyard's own constants: macOS's libc has none of
//! them, and the message code builds everywhere. A Linux test checks them
//! against libc's, where libc has them.

// Message types.
pub(crate) const RTM_NEWLINK: u16 = 16;
pub(crate) const RTM_DELLINK: u16 = 17;
pub(crate) const RTM_GETLINK: u16 = 18;
pub(crate) const RTM_SETLINK: u16 = 19;
pub(crate) const RTM_NEWADDR: u16 = 20;
pub(crate) const RTM_GETADDR: u16 = 22;
pub(crate) const RTM_NEWROUTE: u16 = 24;
pub(crate) const RTM_GETROUTE: u16 = 26;

// Address families.
pub(crate) const AF_UNSPEC: u8 = 0;
pub(crate) const AF_INET: u8 = 2;
pub(crate) const AF_BRIDGE: u8 = 7;
pub(crate) const AF_INET6: u8 = 10;

// Link flags.
pub(crate) const IFF_UP: u32 = 0x1;

// Link attributes.
pub(crate) const IFLA_ADDRESS: u16 = 1;
pub(crate) const IFLA_IFNAME: u16 = 3;
pub(crate) const IFLA_MTU: u16 = 4;
pub(crate) const IFLA_MASTER: u16 = 10;
pub(crate) const IFLA_PROTINFO: u16 = 12;
pub(crate) const IFLA_LINKINFO: u16 = 18;
pub(crate) const IFLA_NET_NS_FD: u16 = 28;
pub(crate) const IFLA_INFO_KIND: u16 = 1;
pub(crate) const IFLA_INFO_DATA: u16 = 2;
pub(crate) const VETH_INFO_PEER: u16 = 1;
pub(crate) const IFLA_BRPORT_MODE: u16 = 4;

// Address flags and attributes.
pub(crate) const IFA_F_NODAD: u8 = 0x02;
pub(crate) const IFA_ADDRESS: u16 = 1;
pub(crate) const IFA_LOCAL: u16 = 2;
pub(crate) const IFA_BROADCAST: u16 = 4;

// Route attributes and values.
pub(crate) const RTA_DST: u16 = 1;
pub(crate) const RTA_OIF: u16 = 4;
pub(crate) const RTA_GATEWAY: u16 = 5;
pub(crate) const RTA_TABLE: u16 = 15;
pub(crate) const RT_TABLE_MAIN: u8 = 254;
pub(crate) const RTPROT_BOOT: u8 = 3;
pub(crate) const RT_SCOPE_UNIVERSE: u8 = 0;
pub(crate) const RT_SCOPE_LINK: u8 = 253;
pub(crate) const RTN_UNICAST: u8 = 1;
pub(crate) const RTN_BLACKHOLE: u8 = 6;
pub(crate) const RTN_UNREACHABLE: u8 = 7;
pub(crate) const RTN_PROHIBIT: u8 = 8;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn they_are_the_kernels() {
        assert_eq!(RTM_NEWLINK, libc::RTM_NEWLINK);
        assert_eq!(RTM_DELLINK, libc::RTM_DELLINK);
        assert_eq!(RTM_GETLINK, libc::RTM_GETLINK);
        assert_eq!(RTM_SETLINK, libc::RTM_SETLINK);
        assert_eq!(RTM_NEWADDR, libc::RTM_NEWADDR);
        assert_eq!(RTM_GETADDR, libc::RTM_GETADDR);
        assert_eq!(RTM_NEWROUTE, libc::RTM_NEWROUTE);
        assert_eq!(RTM_GETROUTE, libc::RTM_GETROUTE);
        assert_eq!(i32::from(AF_UNSPEC), libc::AF_UNSPEC);
        assert_eq!(i32::from(AF_INET), libc::AF_INET);
        assert_eq!(i32::from(AF_BRIDGE), libc::AF_BRIDGE);
        assert_eq!(i32::from(AF_INET6), libc::AF_INET6);
        assert_eq!(IFF_UP, libc::IFF_UP as u32);
        assert_eq!(IFLA_ADDRESS, libc::IFLA_ADDRESS);
        assert_eq!(IFLA_IFNAME, libc::IFLA_IFNAME);
        assert_eq!(IFLA_MTU, libc::IFLA_MTU);
        assert_eq!(IFLA_MASTER, libc::IFLA_MASTER);
        assert_eq!(IFLA_PROTINFO, libc::IFLA_PROTINFO);
        assert_eq!(IFLA_LINKINFO, libc::IFLA_LINKINFO);
        assert_eq!(IFLA_NET_NS_FD, libc::IFLA_NET_NS_FD);
        assert_eq!(IFLA_INFO_KIND, libc::IFLA_INFO_KIND);
        assert_eq!(IFLA_INFO_DATA, libc::IFLA_INFO_DATA);
        assert_eq!(IFA_ADDRESS, libc::IFA_ADDRESS);
        assert_eq!(IFA_LOCAL, libc::IFA_LOCAL);
        assert_eq!(IFA_BROADCAST, libc::IFA_BROADCAST);
        assert_eq!(RTA_DST, libc::RTA_DST);
        assert_eq!(RTA_OIF, libc::RTA_OIF);
        assert_eq!(RTA_GATEWAY, libc::RTA_GATEWAY);
        assert_eq!(RTA_TABLE, libc::RTA_TABLE);
        assert_eq!(RT_TABLE_MAIN, libc::RT_TABLE_MAIN);
        assert_eq!(RTPROT_BOOT, libc::RTPROT_BOOT);
        assert_eq!(RT_SCOPE_UNIVERSE, libc::RT_SCOPE_UNIVERSE);
        assert_eq!(RT_SCOPE_LINK, libc::RT_SCOPE_LINK);
        assert_eq!(RTN_UNICAST, libc::RTN_UNICAST);
        assert_eq!(RTN_BLACKHOLE, libc::RTN_BLACKHOLE);
        assert_eq!(RTN_UNREACHABLE, libc::RTN_UNREACHABLE);
        assert_eq!(RTN_PROHIBIT, libc::RTN_PROHIBIT);
    }
}
