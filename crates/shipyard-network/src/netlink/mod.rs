//! Netlink: how a process talks to the kernel's networking.
//!
//! A netlink socket (`AF_NETLINK`, netlink(7)) is a datagram socket whose
//! other end is the kernel. A request is a message; the kernel answers with
//! messages: the objects asked for, an acknowledgement, or an error with the
//! kernel's own reason. Each family of the kernel speaks its own protocol over
//! it: rtnetlink (`NETLINK_ROUTE`) for links, addresses and routes, `nf_tables`
//! and conntrack (`NETLINK_NETFILTER`) for the firewall.
//!
//! [`message`] builds and reads messages and [`conversation`] holds the rules
//! of a request and its answer, on any UNIX; `Socket` (Linux) sends them.

// Used by the Linux socket, and tested everywhere.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the socket is Linux's")
)]
pub(crate) mod conversation;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the socket is Linux's")
)]
pub(crate) mod message;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "the socket is Linux's")
)]
mod reply;
#[cfg(target_os = "linux")]
mod socket;

pub use conversation::NetlinkError;
pub(crate) use conversation::{BatchItem, NFGENMSG_LEN, nfnl_message};
#[cfg(target_os = "linux")]
pub(crate) use message::attr_flag;
pub use message::{BuildError, ParseError};
pub(crate) use message::{Message, MessageBuilder, flag};
#[cfg(target_os = "linux")]
pub(crate) use socket::Socket;
