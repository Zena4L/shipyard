//! rtnetlink (rtnetlink(7)): the kernel's links, addresses and routes.
//!
//! The submodules build each request and read each answer, on any UNIX;
//! `Rtnetlink` (Linux) sends them over a socket of the calling thread's
//! network namespace, or of the namespace it was made in
//! (`Rtnetlink::in_namespace`).

// The requests and parsers are the Linux side's (and the fuzz targets'); the
// types are everyone's.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by the Linux side")
)]
pub(crate) mod address;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by the Linux side")
)]
pub(crate) mod consts;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by the Linux side")
)]
pub(crate) mod link;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "used by the Linux side")
)]
pub(crate) mod routes;

pub use address::Address;
pub use link::{Link, LinkChange, MAX_NAME, NameError, validate_name};
pub use routes::{Route, RouteType};

#[cfg(target_os = "linux")]
pub use linux::{Rtnetlink, RtnetlinkError};

#[cfg(test)]
#[cfg(target_os = "linux")]
mod linux_tests;

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{AsRawFd, BorrowedFd};

    use nix::errno::Errno;
    use nix::sched::{CloneFlags, setns};
    use thiserror::Error;

    use super::link::{self, Link, LinkChange, NameError, validate_name};
    use super::{Address, Route, address, routes};
    use crate::ipnet::{Family, IpNet};
    use crate::netlink::{NetlinkError, Socket};

    /// What went wrong with an rtnetlink request.
    #[derive(Debug, Error)]
    pub enum RtnetlinkError {
        /// The kernel (or the socket) failed it.
        #[error(transparent)]
        Netlink(#[from] NetlinkError),
        /// An interface name the kernel would refuse.
        #[error(transparent)]
        Name(#[from] NameError),
        /// A descriptor that doesn't fit a netlink attribute.
        #[error("{0}: the namespace's descriptor is out of range")]
        Descriptor(String),
        /// Entering the network namespace failed.
        #[error("entering the network namespace: {}", .0.desc())]
        Namespace(Errno),
        /// The thread that enters the namespace couldn't be started (out of
        /// processes, say).
        #[error("starting a thread to enter the network namespace: {0}")]
        Thread(#[source] std::io::Error),
    }

    impl RtnetlinkError {
        /// The kernel's own words on why it refused (its extended ACK), if
        /// it gave them.
        #[must_use]
        pub fn kernel_reason(&self) -> Option<&str> {
            match self {
                RtnetlinkError::Netlink(NetlinkError::Refused { reason, .. }) => reason.as_deref(),
                _ => None,
            }
        }

        /// The errno the kernel refused the request (or the namespace switch)
        /// with.
        #[must_use]
        pub fn errno(&self) -> Option<Errno> {
            match self {
                RtnetlinkError::Netlink(error) => error.errno(),
                RtnetlinkError::Namespace(errno) => Some(*errno),
                _ => None,
            }
        }
    }

    /// An rtnetlink socket, and the requests Shipyard makes over it.
    #[derive(Debug)]
    pub struct Rtnetlink {
        socket: Socket,
    }

    impl Rtnetlink {
        /// A socket of the calling thread's network namespace.
        pub fn new() -> Result<Rtnetlink, RtnetlinkError> {
            Ok(Rtnetlink {
                socket: Socket::route()?,
            })
        }

        /// A socket of the network namespace `netns` refers to (probe N4: a
        /// socket stays in the namespace it was made in).
        ///
        /// A thread of its own enters the namespace and makes the socket, so
        /// the caller's thread never leaves its own namespace: it can't be
        /// stranded in the container's (where a caller in a user namespace may
        /// have no right to come back from), and nothing it does later acts on
        /// the wrong network. The thread has ended when this returns, so a
        /// caller that forks afterwards is single-threaded again.
        pub fn in_namespace(netns: BorrowedFd<'_>) -> Result<Rtnetlink, RtnetlinkError> {
            std::thread::scope(|scope| {
                // Never a panic if the thread can't be had: this runs where a
                // child waits to be killed and reaped on an error.
                std::thread::Builder::new()
                    .spawn_scoped(scope, || {
                        setns(netns, CloneFlags::CLONE_NEWNET)
                            .map_err(RtnetlinkError::Namespace)?;
                        Ok(Rtnetlink {
                            socket: Socket::route()?,
                        })
                    })
                    .map_err(RtnetlinkError::Thread)?
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
        }

        /// Every link.
        pub fn links(&mut self) -> Result<Vec<Link>, RtnetlinkError> {
            Ok(self
                .socket
                .dump("listing network interfaces", &link::get_all(), |message| {
                    Link::parse(message)
                })?)
        }

        /// The link named `name`, if there is one.
        pub fn link(&mut self, name: &str) -> Result<Option<Link>, RtnetlinkError> {
            validate_name(name)?;
            let operation = format!("looking up the network interface {name}");
            let mut found = None;
            match self
                .socket
                .exchange(&operation, &link::get_by_name(name), |message| {
                    if let Some(link) = Link::parse(message)? {
                        found = Some(link);
                    }
                    Ok(())
                }) {
                Ok(()) => Ok(found),
                Err(error) if error.errno() == Some(Errno::ENODEV) => Ok(None),
                Err(error) => Err(error.into()),
            }
        }

        /// Creates a bridge named `name` (down, with no ports).
        pub fn create_bridge(&mut self, name: &str) -> Result<(), RtnetlinkError> {
            validate_name(name)?;
            let operation = format!("creating the bridge {name}");
            Ok(self.socket.request(&operation, &link::new_bridge(name))?)
        }

        /// Creates a veth pair: `name` here, and `peer_name` born in the
        /// network namespace `peer_netns` (here, for `None`), with the
        /// hardware address `peer_mac` (the kernel's random one, for
        /// `None`).
        pub fn create_veth(
            &mut self,
            name: &str,
            peer_name: &str,
            peer_netns: Option<BorrowedFd<'_>>,
            peer_mac: Option<[u8; 6]>,
        ) -> Result<(), RtnetlinkError> {
            validate_name(name)?;
            validate_name(peer_name)?;
            let operation = format!("creating the veth pair {name} and {peer_name}");
            let fd = peer_netns
                .map(|fd| fd_attribute(fd, &operation))
                .transpose()?;
            Ok(self
                .socket
                .request(&operation, &link::new_veth(name, peer_name, fd, peer_mac))?)
        }

        /// Changes the link with `index`.
        pub fn change(&mut self, index: u32, change: LinkChange<'_>) -> Result<(), RtnetlinkError> {
            if let LinkChange::Name(name) = change {
                validate_name(name)?;
            }
            let operation = change.describe(index);
            Ok(self
                .socket
                .request(&operation, &link::change(index, change))?)
        }

        /// Moves the link with `index` into the network namespace `netns`,
        /// renamed `name` there if one is given.
        pub fn move_to(
            &mut self,
            index: u32,
            netns: BorrowedFd<'_>,
            name: Option<&str>,
        ) -> Result<(), RtnetlinkError> {
            if let Some(name) = name {
                validate_name(name)?;
            }
            let operation = match name {
                Some(name) => {
                    format!("moving the network interface {index} into a namespace, as {name}")
                }
                None => format!("moving the network interface {index} into a namespace"),
            };
            let fd = fd_attribute(netns, &operation)?;
            Ok(self
                .socket
                .request(&operation, &link::move_to(index, fd, name))?)
        }

        /// Turns hairpin mode on or off for the bridge port with `index`.
        pub fn set_hairpin(&mut self, index: u32, on: bool) -> Result<(), RtnetlinkError> {
            let operation = format!("setting hairpin mode on the bridge port {index}");
            Ok(self
                .socket
                .request(&operation, &link::set_hairpin(index, on))?)
        }

        /// Deletes the link with `index` (a veth's peer goes with it).
        pub fn delete(&mut self, index: u32) -> Result<(), RtnetlinkError> {
            let operation = format!("deleting the network interface {index}");
            Ok(self.socket.request(&operation, &link::delete(index))?)
        }

        /// Adds `net` to the interface with `index`.
        pub fn add_address(&mut self, index: u32, net: IpNet) -> Result<(), RtnetlinkError> {
            let operation = format!("adding the address {net} to the network interface {index}");
            Ok(self.socket.request(&operation, &address::add(index, net))?)
        }

        /// The addresses of the interface with `index` (every interface's for
        /// `None`; none for an interface that isn't there).
        pub fn addresses(&mut self, index: Option<u32>) -> Result<Vec<Address>, RtnetlinkError> {
            let found =
                match self
                    .socket
                    .dump("listing addresses", &address::get(index), |message| {
                        Address::parse(message)
                    }) {
                    Ok(found) => found,
                    // With strict checking, a filter on a missing interface.
                    Err(error) if index.is_some() && error.errno() == Some(Errno::ENODEV) => {
                        Vec::new()
                    }
                    Err(error) => return Err(error.into()),
                };
            // Without strict checking (before Linux 4.20) the index filter
            // is a hint the kernel may ignore.
            Ok(found
                .into_iter()
                .filter(|address| index.is_none_or(|index| address.index == index))
                .collect())
        }

        /// Adds `route` to the main table.
        pub fn add_route(&mut self, route: &Route) -> Result<(), RtnetlinkError> {
            let operation = match route.gateway {
                Some(gateway) => format!("adding the route to {} via {gateway}", route.destination),
                None => format!("adding the route to {}", route.destination),
            };
            Ok(self.socket.request(&operation, &routes::add(route))?)
        }

        /// The main table's routes of `family` that send on or refuse what
        /// they match.
        pub fn routes(&mut self, family: Family) -> Result<Vec<Route>, RtnetlinkError> {
            Ok(self
                .socket
                .dump("listing routes", &routes::get(family), |message| {
                    Route::parse(message)
                })?)
        }
    }

    /// `fd` as the `u32` an `IFLA_NET_NS_FD` attribute holds.
    fn fd_attribute(fd: BorrowedFd<'_>, operation: &str) -> Result<u32, RtnetlinkError> {
        u32::try_from(fd.as_raw_fd()).map_err(|_| RtnetlinkError::Descriptor(operation.to_owned()))
    }
}
