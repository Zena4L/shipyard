//! Where the guest's agent sends what the Mac forwards to a published port
//! (step 13.5a): to the container itself, at the address its network lease
//! gives that publish ([`target_of`]), not through the guest's own DNAT
//! (which skips the guest's `::1`: F97). And a UDP port's datagrams
//! ([`relay_udp`]): every Mac peer's over one stream, each peer's on a UDP
//! socket of its own connected to the container, so its answers are told
//! apart.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::fd::AsFd as _;
use std::time::{Duration, Instant};

use shipyard_network::store::Lease;
use shipyard_runtime::attach::forward::{self, Protocol, PublishedPort};

/// How often the container's address is looked up again: one that changed
/// (the container restarted, another took its address) drops every socket,
/// never sending to an address no longer its. A new peer's socket looks it
/// up as it's made.
const RECHECK: Duration = Duration::from_secs(2);

/// How often the relay looks again with nothing to do (idle sockets, the
/// address).
const TICK: Duration = Duration::from_secs(1);

/// The most of the Mac's datagrams sent on in a round, before the
/// containers' answers are looked at again.
const ROUND: usize = 64;

/// The most peer sockets every relay of the guest's agent holds at once:
/// within its descriptors, whatever the published ports and their peers.
const MAX_SOCKETS: usize = 4096;

/// The peer sockets held now, every relay's.
static SOCKETS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The container's address and port for `wanted` (a port published in the
/// guest): the live lease whose ports hold it (on every address when its
/// address is unspecified, else on that one), its IPv4 address first,
/// its IPv6 one for a publish only IPv6 reaches. `None` if no lease holds
/// it.
#[must_use]
pub(crate) fn target_of(leases: &[Lease], wanted: &PublishedPort) -> Option<SocketAddr> {
    let protocol = match wanted.protocol {
        Protocol::Tcp => shipyard_network::ports::Protocol::Tcp,
        Protocol::Udp => shipyard_network::ports::Protocol::Udp,
    };
    let host_ip = (!wanted.ip.is_unspecified()).then_some(wanted.ip);
    leases.iter().find_map(|lease| {
        let published = lease.ports.iter().find(|published| {
            published.host_port == wanted.port
                && published.protocol == protocol
                && published.host_ip.filter(|ip| !ip.is_unspecified()) == host_ip
        })?;
        let mut containers: Vec<IpAddr> = lease
            .port_rules()
            .into_iter()
            .filter(|rule| rule.published == *published)
            .map(|rule| rule.container)
            .collect();
        containers.sort_by_key(IpAddr::is_ipv6);
        containers
            .first()
            .map(|&ip| SocketAddr::new(ip, published.container_port))
    })
}

/// [`target_of`] the guest's live leases now (none readable: `None`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the guest's"))]
pub(crate) fn find_target(engine: &crate::Engine, wanted: &PublishedPort) -> Option<SocketAddr> {
    let networks = engine
        .paths()
        .networks()
        .ok()
        .and_then(shipyard_network::store::Networks::existing)?;
    let leases: Vec<Lease> = networks
        .live_leases_by_container()
        .into_values()
        .flatten()
        .map(|(_, lease)| lease)
        .collect();
    target_of(&leases, wanted)
}

/// A Mac peer's socket, connected to the container, and when a datagram
/// last crossed it; counted in [`SOCKETS`] while it lives.
struct PeerSocket {
    socket: UdpSocket,
    heard: Instant,
}

impl PeerSocket {
    /// A socket connected to `to`, if one more is within [`MAX_SOCKETS`].
    fn to(to: SocketAddr) -> Option<Self> {
        // Taken first, given back if it was the one too many: never past
        // the bound, whatever other relays take meanwhile.
        if SOCKETS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_SOCKETS {
            SOCKETS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return None;
        }
        if let Ok(socket) = connected(to) {
            Some(Self::counted(socket))
        } else {
            SOCKETS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            None
        }
    }

    /// `socket`, its place in [`SOCKETS`] already taken (given back by its
    /// drop).
    fn counted(socket: UdpSocket) -> Self {
        Self {
            socket,
            heard: Instant::now(),
        }
    }
}

impl Drop for PeerSocket {
    fn drop(&mut self) {
        SOCKETS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What a UDP relay holds: its peers' sockets and where they send.
struct Relay<F> {
    peers: HashMap<u32, PeerSocket>,
    address: Option<SocketAddr>,
    checked: Instant,
    target: F,
}

impl<F: FnMut() -> Option<SocketAddr>> Relay<F> {
    /// The container's address looked up again (every [`RECHECK`], or
    /// `now`): changed, every socket's dropped.
    fn recheck(&mut self, now: bool) {
        if now || self.checked.elapsed() >= RECHECK {
            let found = (self.target)();
            if found != self.address {
                self.peers.clear();
                self.address = found;
            }
            self.checked = Instant::now();
        }
    }

    /// Sends `datagram`, the Mac peer `id`'s, to the container: on its
    /// socket, made now (the address looked up first) if it has none, the
    /// least recently heard let go past [`forward::MAX_UDP_PEERS`]. Dropped
    /// when there's no address or socket for it, or the send fails (UDP's
    /// own losses: a closed port, a datagram too long).
    fn send_on(&mut self, id: u32, datagram: &[u8]) {
        if !self.peers.contains_key(&id) {
            self.recheck(true);
            let Some(to) = self.address else {
                return;
            };
            if self.peers.len() >= forward::MAX_UDP_PEERS
                && let Some(oldest) = self
                    .peers
                    .iter()
                    .min_by_key(|(_, peer)| peer.heard)
                    .map(|(id, _)| *id)
            {
                self.peers.remove(&oldest);
            }
            let Some(socket) = PeerSocket::to(to) else {
                return;
            };
            self.peers.insert(id, socket);
        }
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.heard = Instant::now();
            let _ = peer.socket.send(datagram);
        }
    }

    /// Queues each answer the containers' sockets in `answered` have for
    /// the Mac, with its peer's ID: one the queue has no room for is
    /// dropped, as UDP drops it, never waited on (13.8c).
    fn answers_back(
        &mut self,
        answered: &[u32],
        to_mac: &mut forward::Outbox,
        datagram: &mut [u8],
    ) {
        for id in answered {
            let Some(peer) = self.peers.get_mut(id) else {
                continue;
            };
            // A refused one (nothing listens yet: ICMP) is UDP's loss.
            if let Ok(length) = peer.socket.recv(datagram) {
                peer.heard = Instant::now();
                let _ = to_mac.push(*id, &datagram[..length]);
            }
        }
    }
}

/// Whether `fd` came back from a poll with something to read (or ended).
fn ready(fd: &nix::poll::PollFd<'_>) -> bool {
    fd.revents().is_some_and(|got| {
        got.intersects(
            nix::poll::PollFlags::POLLIN
                | nix::poll::PollFlags::POLLHUP
                | nix::poll::PollFlags::POLLERR,
        )
    })
}

/// Relays the Mac's datagrams on `stream` (`unread` first) to the
/// container at `target` (asked again every [`RECHECK`], and for each new
/// peer), and its answers back, until the stream ends, breaks, or leaves a
/// frame unfinished past [`forward::FRAME_WAIT`]. Neither way ever waits on
/// the stream (13.8c: a relay that waited to write while the Mac did the
/// same stalled both); a datagram that can't be sent is dropped, as UDP
/// drops them.
///
/// # Errors
///
/// The stream's, but its end.
pub(crate) fn relay_udp(
    stream: &std::fs::File,
    unread: &[u8],
    target: impl FnMut() -> Option<SocketAddr>,
) -> Result<(), String> {
    // This relay's own connection: nothing else reads or writes it.
    {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        fcntl(stream, FcntlArg::F_GETFL)
            .and_then(|flags| {
                fcntl(
                    stream,
                    FcntlArg::F_SETFL(OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK),
                )
            })
            .map_err(|e| format!("the stream made non-blocking: {e}"))?;
    }
    let mut from_mac = forward::Inbox::with(unread);
    let mut to_mac = forward::Outbox::default();
    let mut relay = Relay {
        peers: HashMap::new(),
        address: None,
        checked: Instant::now(),
        target,
    };
    relay.recheck(true);
    let mut datagram = vec![0u8; forward::MAX_DATAGRAM];
    let mut buffer = Vec::new();
    let tick = nix::poll::PollTimeout::try_from(TICK).unwrap_or(nix::poll::PollTimeout::NONE);
    loop {
        relay.recheck(false);
        relay
            .peers
            .retain(|_, peer| peer.heard.elapsed() < forward::UDP_IDLE);
        let ids: Vec<u32> = relay.peers.keys().copied().collect();
        let answered = {
            let mut wanted = nix::poll::PollFlags::POLLIN;
            if !to_mac.is_empty() {
                wanted |= nix::poll::PollFlags::POLLOUT;
            }
            let mut fds = vec![nix::poll::PollFd::new(stream.as_fd(), wanted)];
            fds.extend(ids.iter().filter_map(|id| relay.peers.get(id)).map(|peer| {
                nix::poll::PollFd::new(peer.socket.as_fd(), nix::poll::PollFlags::POLLIN)
            }));
            let timeout = if from_mac.has_frame() {
                nix::poll::PollTimeout::ZERO
            } else {
                tick
            };
            match nix::poll::poll(&mut fds, timeout) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(format!("polling: {error}")),
            }
            ids.iter()
                .zip(fds.iter().skip(1))
                .filter(|(_, fd)| ready(fd))
                .map(|(id, _)| *id)
                .collect::<Vec<u32>>()
        };
        relay.answers_back(&answered, &mut to_mac, &mut datagram);
        from_mac
            .fill(stream)
            .map_err(|e| format!("from the Mac: {e}"))?;
        for _ in 0..ROUND {
            let Some(id) = from_mac.next(&mut buffer) else {
                break;
            };
            relay.send_on(id, &buffer);
        }
        // The Mac's end before its writes: answers still coming then would
        // meet a closed stream (`EPIPE`), which is that end too. (The Mac's
        // host never half-closes a UDP stream: its end is the relay's, and
        // answers queued then are dropped, as a closed port drops them.)
        if from_mac.ended() {
            return Ok(());
        }
        match to_mac.flush(stream) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(format!("to the Mac: {error}")),
        }
        if from_mac.stuck() {
            return Err("from the Mac: a datagram left unfinished".to_owned());
        }
        if to_mac.stalled() {
            return Err("to the Mac: nothing taken for a while".to_owned());
        }
    }
}

/// A UDP socket of `to`'s family, connected to it (its answers alone
/// taken), non-blocking.
fn connected(to: SocketAddr) -> std::io::Result<UdpSocket> {
    let any: SocketAddr = match to {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(any)?;
    socket.connect(to)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;
    use shipyard_network::ports::{Protocol as NetProtocol, Published};

    fn lease(ip: &str, ip6: Option<&str>, ports: Vec<Published>) -> Lease {
        Lease {
            ip: ip.parse().unwrap(),
            mac: [2, 0, 0, 0, 0, 1],
            run: "run".to_owned(),
            hostname: "h".to_owned(),
            host_veth: "veth0".to_owned(),
            ports,
            name: None,
            aliases: Vec::new(),
            interface: "eth0".to_owned(),
            ip6: ip6.map(|ip| ip.parse().unwrap()),
            container: Some("c".repeat(64)),
        }
    }

    fn published(host_ip: Option<&str>, host_port: u16, protocol: NetProtocol) -> Published {
        Published {
            host_ip: host_ip.map(|ip| ip.parse().unwrap()),
            host_port,
            container_port: 80,
            protocol,
        }
    }

    fn wanted(ip: &str, port: u16, protocol: Protocol) -> PublishedPort {
        PublishedPort {
            ip: ip.parse().unwrap(),
            port,
            protocol,
        }
    }

    /// A publish on every address goes to the container's IPv4 (with IPv6
    /// or not); one on the guest's `::1` to its IPv6, and nowhere without
    /// one; the protocol and the port must match; the lease whose ports
    /// hold it is the one taken, whichever comes first.
    #[test]
    fn the_container_is_found_by_its_lease() {
        let other = lease(
            "10.88.0.9",
            None,
            vec![published(None, 9000, NetProtocol::Tcp)],
        );
        let dual = lease(
            "10.88.0.2",
            Some("fd00::2"),
            vec![
                published(None, 8080, NetProtocol::Tcp),
                published(None, 5353, NetProtocol::Udp),
                published(Some("::1"), 6379, NetProtocol::Tcp),
            ],
        );
        let v4_only = lease(
            "10.88.0.3",
            None,
            vec![published(Some("::1"), 7000, NetProtocol::Tcp)],
        );
        let leases = [other, dual, v4_only];
        let at = |address: &str| Some(address.parse::<SocketAddr>().unwrap());
        assert_eq!(
            target_of(&leases, &wanted("0.0.0.0", 8080, Protocol::Tcp)),
            at("10.88.0.2:80")
        );
        assert_eq!(
            target_of(&leases, &wanted("::", 8080, Protocol::Tcp)),
            at("10.88.0.2:80"),
            "every address, as `::` too"
        );
        assert_eq!(
            target_of(&leases, &wanted("0.0.0.0", 5353, Protocol::Udp)),
            at("10.88.0.2:80")
        );
        assert_eq!(
            target_of(&leases, &wanted("0.0.0.0", 5353, Protocol::Tcp)),
            None
        );
        assert_eq!(
            target_of(&leases, &wanted("::1", 6379, Protocol::Tcp)),
            at("[fd00::2]:80")
        );
        assert_eq!(
            target_of(&leases, &wanted("::1", 7000, Protocol::Tcp)),
            None,
            "IPv6 alone reaches it, and the container has none"
        );
        assert_eq!(
            target_of(&leases, &wanted("0.0.0.0", 1, Protocol::Tcp)),
            None
        );
        assert_eq!(
            target_of(&leases, &wanted("127.0.0.1", 8080, Protocol::Tcp)),
            None,
            "published on every address, not this one alone"
        );
    }

    /// The Mac's datagrams reach the container, each peer on a socket of its
    /// own, its answers back with its ID; bytes read past the request go
    /// first; a changed address drops the sockets and the next datagram
    /// goes to the new one; the stream's end ends the relay.
    #[test]
    fn datagrams_reach_the_container_and_come_back() {
        let echo = |tag: &'static str| {
            let server = UdpSocket::bind("127.0.0.1:0").unwrap();
            let address = server.local_addr().unwrap();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 256];
                while let Ok((length, from)) = server.recv_from(&mut buffer) {
                    let mut back = tag.as_bytes().to_vec();
                    back.extend_from_slice(&buffer[..length]);
                    let _ = server.send_to(&back, from);
                }
            });
            address
        };
        let (first, second) = (echo("a:"), echo("b:"));
        let (mac, guest) = std::os::unix::net::UnixStream::pair().unwrap();
        let guest = std::fs::File::from(std::os::fd::OwnedFd::from(guest));
        let mut unread = Vec::new();
        forward::write_datagram(&mut unread, 1, b"early").unwrap();
        let current = std::sync::Arc::new(std::sync::Mutex::new(first));
        let seen = std::sync::Arc::clone(&current);
        let relaying =
            std::thread::spawn(move || relay_udp(&guest, &unread, || Some(*seen.lock().unwrap())));
        let mut reader = mac.try_clone().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut writer = mac.try_clone().unwrap();
        let mut buffer = Vec::new();
        let mut answers = std::collections::BTreeSet::new();
        forward::write_datagram(&mut writer, 2, b"late").unwrap();
        for _ in 0..2 {
            let id = forward::read_datagram(&mut reader, &mut buffer)
                .unwrap()
                .unwrap();
            answers.insert((id, String::from_utf8(buffer.clone()).unwrap()));
        }
        assert_eq!(
            answers,
            [(1, "a:early".to_owned()), (2, "a:late".to_owned())]
                .into_iter()
                .collect()
        );
        // Another address now: within a recheck, peer 1's next goes there
        // (an answer in flight as it moves is lost, as UDP loses it: sent
        // again).
        *current.lock().unwrap() = second;
        reader
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let began = Instant::now();
        loop {
            assert!(began.elapsed() < Duration::from_secs(10), "never moved");
            forward::write_datagram(&mut writer, 1, b"again").unwrap();
            let Ok(id) = forward::read_datagram(&mut reader, &mut buffer) else {
                continue;
            };
            assert_eq!(id, Some(1));
            if buffer == b"b:again" {
                break;
            }
            assert_eq!(buffer, b"a:again");
            std::thread::sleep(Duration::from_millis(200));
        }
        mac.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(relaying.join().unwrap(), Ok(()));
    }

    /// A Mac that writes its datagrams without reading the container's
    /// answers for a while (both ways full at once) never stalls the relay
    /// into its end (13.8c: it waited 2 s to write an answer, then ended):
    /// every datagram of the Mac's is taken, and once the Mac reads, the
    /// answers come, on the same stream.
    #[test]
    fn a_mac_that_writes_without_reading_keeps_the_relay() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A container that answers its first peer without end.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let heard = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&heard);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            let (_, from) = server.recv_from(&mut buffer).unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            let answers = server.try_clone().unwrap();
            std::thread::spawn(move || while answers.send_to(&[9u8; 1000], from).is_ok() {});
            while server.recv_from(&mut buffer).is_ok() {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        let (mac, guest) = std::os::unix::net::UnixStream::pair().unwrap();
        let guest = std::fs::File::from(std::os::fd::OwnedFd::from(guest));
        let relaying = std::thread::spawn(move || relay_udp(&guest, &[], || Some(address)));
        let mut writer = mac.try_clone().unwrap();
        writer
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        for _ in 0..2000 {
            forward::write_datagram(&mut writer, 1, &[1u8; 1000]).unwrap();
        }
        assert!(!relaying.is_finished(), "{:?}", relaying.join());
        let mut reader = mac.try_clone().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buffer = Vec::new();
        for _ in 0..100 {
            assert_eq!(
                forward::read_datagram(&mut reader, &mut buffer).unwrap(),
                Some(1)
            );
            assert_eq!(buffer, [9u8; 1000]);
        }
        // The control that the container heard the Mac all along (the
        // kernel may drop some of its datagrams, as UDP may).
        assert!(heard.load(Ordering::SeqCst) > 100, "{heard:?}");
        mac.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(relaying.join().unwrap(), Ok(()));
    }
}
