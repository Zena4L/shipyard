//! A netlink socket: requests out, the kernel's answers in.
//!
//! The socket is `SOCK_RAW` (netlink is datagrams: each `recv` takes one
//! datagram, which can hold several messages), bound to a port ID the kernel
//! picks. What a request and its answer look like on it is
//! [`conversation`](super::conversation)'s.
//!
//! A socket belongs to the network namespace it was made in, whatever
//! namespace its user is in later: made in a container's namespace, it keeps
//! acting on that namespace (probe N4).

use std::io::IoSliceMut;
use std::os::fd::{AsRawFd, OwnedFd};

use nix::errno::Errno;
use nix::sys::socket::{
    AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, getsockname,
    recvmsg, sendto, setsockopt, socket, sockopt,
};
use nix::sys::time::{TimeVal, TimeValLike as _};

use super::conversation::{Conversation, NetlinkError, TIMEOUT_SECS, Transport};
use super::message::{Message, MessageBuilder, ParseError};

/// The netlink protocols Shipyard speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    /// rtnetlink: links, addresses, routes (`NETLINK_ROUTE`).
    Route,
    /// `nf_tables` and conntrack (`NETLINK_NETFILTER`).
    Netfilter,
}

/// `SOL_NETLINK` options (`linux/netlink.h`; libc's Linux targets lack them).
const NETLINK_CAP_ACK: libc::c_int = 10;
const NETLINK_EXT_ACK: libc::c_int = 11;
const NETLINK_GET_STRICT_CHK: libc::c_int = 12;

/// A netlink socket.
#[derive(Debug)]
pub(crate) struct Socket {
    conversation: Conversation<Fd>,
}

/// The socket's descriptor, as a [`Transport`].
#[derive(Debug)]
struct Fd(OwnedFd);

impl Socket {
    /// A new socket for `protocol`, in the calling thread's network
    /// namespace.
    pub(crate) fn open(protocol: Protocol) -> Result<Socket, NetlinkError> {
        let operation = "opening a netlink socket";
        let fail = |error| NetlinkError::socket(operation, error);
        let protocol = match protocol {
            Protocol::Route => SockProtocol::NetlinkRoute,
            Protocol::Netfilter => SockProtocol::NetlinkNetFilter,
        };
        let fd = socket(
            AddressFamily::Netlink,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC,
            protocol,
        )
        .map_err(fail)?;
        // Port ID 0: the kernel picks a free one.
        bind(fd.as_raw_fd(), &NetlinkAddr::new(0, 0)).map_err(fail)?;
        let port = getsockname::<NetlinkAddr>(fd.as_raw_fd())
            .map_err(fail)?
            .pid();
        // Where the kernel has them: its reason with every error (Linux
        // 4.12), errors without the whole request echoed back (4.3), and
        // dump filters checked strictly (4.20). Without them, errors come
        // without a reason and the request is skipped over; filters are only
        // hints, which the callers don't rely on.
        for option in [NETLINK_EXT_ACK, NETLINK_CAP_ACK, NETLINK_GET_STRICT_CHK] {
            let _ = set_netlink_option(&fd, option);
        }
        // The kernel answers at once; the timeout only keeps a lost answer
        // from hanging the caller, which may be running with every signal
        // blocked (the network setup before a container starts).
        // (In seconds as an `i64`: `time_t` itself is deprecated on musl,
        // whose width changed.)
        let timeout = TimeVal::seconds(i64::try_from(TIMEOUT_SECS).unwrap_or(i64::MAX));
        setsockopt(&fd, sockopt::ReceiveTimeout, &timeout).map_err(fail)?;
        Ok(Socket {
            conversation: Conversation::new(Fd(fd), port),
        })
    }

    /// A new rtnetlink socket.
    pub(crate) fn route() -> Result<Socket, NetlinkError> {
        Socket::open(Protocol::Route)
    }

    /// A new netfilter socket (`nf_tables`, conntrack).
    pub(crate) fn netfilter() -> Result<Socket, NetlinkError> {
        Socket::open(Protocol::Netfilter)
    }

    /// Sends `items` as one batch for the nfnetlink `subsystem`: all are
    /// applied, or none.
    pub(crate) fn batch(
        &mut self,
        operation: &str,
        subsystem: u16,
        items: &[super::conversation::BatchItem],
    ) -> Result<(), NetlinkError> {
        self.conversation.batch(operation, subsystem, items)
    }

    /// Sends `message` and waits for the kernel's acknowledgement.
    pub(crate) fn request(
        &mut self,
        operation: &str,
        message: &MessageBuilder,
    ) -> Result<(), NetlinkError> {
        self.conversation.exchange(operation, message, |_| Ok(()))
    }

    /// Sends `message`, passes each object the kernel answers with to `each`,
    /// and waits for the acknowledgement (a get request for one object).
    pub(crate) fn exchange(
        &mut self,
        operation: &str,
        message: &MessageBuilder,
        each: impl FnMut(&Message<'_>) -> Result<(), ParseError>,
    ) -> Result<(), NetlinkError> {
        self.conversation.exchange(operation, message, each)
    }

    /// Sends `message` as a dump request and returns what `parse` makes of
    /// each object the kernel lists (`None` skips one). A dump the kernel
    /// flags as interrupted (the table changed while it was listed) is
    /// redone, so the list is never a mix of before and after.
    pub(crate) fn dump<T>(
        &mut self,
        operation: &str,
        message: &MessageBuilder,
        parse: impl FnMut(&Message<'_>) -> Result<Option<T>, ParseError>,
    ) -> Result<Vec<T>, NetlinkError> {
        self.conversation.dump(operation, message, parse)
    }
}

impl Transport for Fd {
    fn send(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        sendto(
            self.0.as_raw_fd(),
            bytes,
            &NetlinkAddr::new(0, 0),
            MsgFlags::empty(),
        )
        .map(drop)
    }

    fn receive(&mut self, buffer: &mut [u8]) -> Result<(usize, u32), Errno> {
        self.receive_with(buffer, MsgFlags::empty())
    }

    fn receive_now(&mut self, buffer: &mut [u8]) -> Result<(usize, u32), Errno> {
        self.receive_with(buffer, MsgFlags::MSG_DONTWAIT)
    }
}

impl Fd {
    fn receive_with(&mut self, buffer: &mut [u8], flags: MsgFlags) -> Result<(usize, u32), Errno> {
        let mut iov = [IoSliceMut::new(buffer)];
        // `MSG_TRUNC` makes netlink return the datagram's real length, so a
        // cut one is seen.
        let received = recvmsg::<NetlinkAddr>(
            self.0.as_raw_fd(),
            &mut iov,
            None,
            MsgFlags::MSG_TRUNC | flags,
        )?;
        // A datagram without an address isn't the kernel's: port ID 0 is.
        let sender = received.address.map_or(u32::MAX, |address| address.pid());
        Ok((received.bytes, sender))
    }
}

/// Turns on the `SOL_NETLINK` option `name` on `fd`.
fn set_netlink_option(fd: &OwnedFd, name: libc::c_int) -> Result<(), Errno> {
    let on: libc::c_int = 1;
    let len = libc::socklen_t::try_from(size_of::<libc::c_int>()).map_err(|_| Errno::EINVAL)?;
    // SAFETY: `on` is a c_int of `len` bytes for the call's duration; the
    // kernel only reads it.
    let result = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_NETLINK,
            name,
            std::ptr::from_ref(&on).cast(),
            len,
        )
    };
    Errno::result(result).map(drop)
}
