//! A UDP port the guest publishes, on the Mac's loopback (step 13.5a,
//! F96). One stream to the guest's port carries every Mac peer's
//! datagrams ([`forward::FORWARD_UDP_REQUEST`]), each tagged with the ID
//! this side gives its peer (its source address and port,
//! [`Peers`]); what the guest sends back with an ID goes to that peer. A
//! peer no datagram has come from or gone to for [`forward::UDP_IDLE`] is
//! forgotten, and past [`forward::MAX_UDP_PEERS`] the least recently heard
//! one is. The stream is joined at the first datagram, and again after it
//! breaks (at most every [`RETRY`]); a datagram that can't cross is dropped,
//! as UDP drops them. Neither side ever waits on the stream, to write or to
//! read the rest of a frame ([`forward::Outbox`], [`forward::Inbox`]: 13.8c),
//! so two-way traffic never stalls both; and at most [`ROUND`] of either
//! side's datagrams are taken at once, so neither starves the other, nor
//! holds the relay from its stop.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shipyard_runtime::attach::forward::{self, MAX_UDP_PEERS, UDP_IDLE};

/// How soon a stream that failed (to be joined, or to take a datagram) is
/// tried again.
const RETRY: Duration = Duration::from_secs(1);

/// How often the listener looks whether it should stop, and forgets idle
/// peers.
const TICK: Duration = Duration::from_secs(1);

/// The most datagrams taken from either side in a round, before the
/// listener looks at its stop again.
const ROUND: usize = 64;

/// What a relay needs of its host.
pub(crate) trait Guest: Send + Sync + 'static {
    /// A stream joined to the guest's port, and the bytes already read past
    /// its welcome.
    ///
    /// # Errors
    ///
    /// Why the guest didn't take it.
    fn join(&self) -> Result<(UnixStream, Vec<u8>), String>;
    /// Says `line` (once, the host's log).
    fn say(&self, line: String);
}

/// The Mac's peers of one published address, each with its ID and when a
/// datagram last crossed for it.
#[derive(Debug, Default)]
pub(crate) struct Peers {
    by_address: HashMap<SocketAddr, u32>,
    by_id: HashMap<u32, (SocketAddr, Instant)>,
    next: u32,
}

impl Peers {
    /// `peer`'s ID, given now if it has none (the least recently heard
    /// peer forgotten past [`MAX_UDP_PEERS`]), heard at `now`.
    pub(crate) fn id_of(&mut self, peer: SocketAddr, now: Instant) -> u32 {
        if let Some(&id) = self.by_address.get(&peer) {
            self.by_id.insert(id, (peer, now));
            return id;
        }
        if self.by_id.len() >= MAX_UDP_PEERS
            && let Some(oldest) = self
                .by_id
                .iter()
                .min_by_key(|(_, (_, heard))| *heard)
                .map(|(id, _)| *id)
        {
            self.forget(oldest);
        }
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.by_address.insert(peer, id);
        self.by_id.insert(id, (peer, now));
        id
    }

    /// The peer `id` names, heard at `now`: `None` if it's been forgotten.
    pub(crate) fn peer_of(&mut self, id: u32, now: Instant) -> Option<SocketAddr> {
        let (peer, heard) = self.by_id.get_mut(&id)?;
        *heard = now;
        Some(*peer)
    }

    /// Forgets the peers not heard since [`UDP_IDLE`] before `now`.
    pub(crate) fn sweep(&mut self, now: Instant) {
        let idle: Vec<u32> = self
            .by_id
            .iter()
            .filter(|(_, (_, heard))| now.saturating_duration_since(*heard) >= UDP_IDLE)
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            self.forget(id);
        }
    }

    /// How many are remembered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    fn forget(&mut self, id: u32) {
        if let Some((peer, _)) = self.by_id.remove(&id) {
            self.by_address.remove(&peer);
        }
    }
}

/// The stream to the guest, with what's come in on it and what waits to go
/// out (neither ever waited for: [`forward::Inbox`], [`forward::Outbox`]).
struct Link {
    stream: UnixStream,
    inbox: forward::Inbox,
    outbox: forward::Outbox,
}

/// The relay's state between rounds: its peers, its stream to the guest,
/// when that last failed, and the guest's answer being handed on.
struct Relay<'a> {
    socket: &'a UdpSocket,
    guest: &'a Arc<dyn Guest>,
    peers: Peers,
    link: Option<Link>,
    failed: Option<Instant>,
    answer: Vec<u8>,
}

impl Relay<'_> {
    /// Reads what the guest has sent and writes what waits for it, waiting
    /// for neither; a stream that broke, left a frame unfinished, or took
    /// nothing, past [`forward::FRAME_WAIT`], is let go (joined again for
    /// the next datagram). One the guest ended is written to no more: its
    /// answers are handed on, then it's let go quietly ([`Self::answers`]).
    fn exchange(&mut self) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let broke = match link.inbox.fill(&link.stream) {
            Err(error) => Some(format!("the guest's side broke off: {error}")),
            Ok(()) if link.inbox.ended() => None,
            Ok(()) => match link.outbox.flush(&link.stream) {
                Err(error) => Some(format!("the guest's side broke off: {error}")),
                Ok(()) if link.inbox.stuck() => {
                    Some("the guest's side stopped mid-datagram".to_owned())
                }
                Ok(()) if link.outbox.stalled() => {
                    Some("the guest's side stopped reading".to_owned())
                }
                Ok(()) => None,
            },
        };
        if let Some(why) = broke {
            self.broke(Some(&why));
        }
    }

    /// Sends up to [`ROUND`] of the guest's answers that are whole now to
    /// their peers: never more at once, so a guest that never stops
    /// answering never holds the relay from its stop or the Mac's own
    /// datagrams (13.8c). A stream that ended, its answers sent, is let go.
    fn answers(&mut self) {
        for _ in 0..ROUND {
            let Some(link) = self.link.as_mut() else {
                return;
            };
            // A peer forgotten meanwhile: its answer has nowhere to go. A
            // full buffer drops it.
            let Some(id) = link.inbox.next(&mut self.answer) else {
                if link.inbox.ended() {
                    self.broke(None);
                }
                return;
            };
            if let Some(peer) = self.peers.peer_of(id, Instant::now()) {
                let _ = self.socket.send_to(&self.answer, peer);
            }
        }
    }

    /// The stream to the guest broken off (said, if `why`): tried again
    /// after [`RETRY`].
    fn broke(&mut self, why: Option<&str>) {
        if let Some(why) = why {
            self.guest.say(why.to_owned());
        }
        self.link = None;
        self.failed = Some(Instant::now());
    }

    /// Takes up to [`ROUND`] of the Mac's datagrams and queues each, with
    /// its peer's ID, for the guest (the stream joined if need be, unless it
    /// failed less than [`RETRY`] ago: then they're dropped, as is one the
    /// queue has no room for), handing on the guest's answers between; until
    /// `stop`.
    fn take_the_macs(&mut self, datagram: &mut [u8], stop: &AtomicBool) {
        for _ in 0..ROUND {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let (length, peer) = match self.socket.recv_from(datagram) {
                Ok(received) => received,
                Err(error) => {
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) {
                        self.guest.say(format!("receiving a datagram: {error}"));
                    }
                    return;
                }
            };
            let id = self.peers.id_of(peer, Instant::now());
            if self.link.is_none() && self.failed.is_none_or(|at| at.elapsed() >= RETRY) {
                match joined(self.guest) {
                    Ok(joined) => {
                        self.link = Some(joined);
                        self.failed = None;
                    }
                    Err(why) => self.broke(Some(&format!("datagrams not forwarded: {why}"))),
                }
            }
            let Some(link) = self.link.as_mut() else {
                continue;
            };
            let _ = link.outbox.push(id, &datagram[..length]);
            self.exchange();
            self.answers();
        }
    }
}

/// Relays `socket`'s datagrams to `guest`'s port, and its answers back,
/// until `stop` (`woken` then).
pub(crate) fn relay(
    socket: &UdpSocket,
    woken: &std::io::PipeReader,
    stop: &AtomicBool,
    guest: &Arc<dyn Guest>,
) {
    let mut relay = Relay {
        socket,
        guest,
        peers: Peers::default(),
        link: None,
        failed: None,
        answer: Vec::new(),
    };
    // The Mac's datagrams are taken into one buffer of the most a datagram
    // holds; the guest's answers into another, sized to each (a shared one,
    // left at an answer's size, would cut the Mac's next datagram short).
    let mut datagram = vec![0u8; forward::MAX_DATAGRAM];
    let tick = nix::poll::PollTimeout::try_from(TICK).unwrap_or(nix::poll::PollTimeout::NONE);
    loop {
        let waited = {
            let mut fds = vec![
                nix::poll::PollFd::new(socket.as_fd(), nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(woken.as_fd(), nix::poll::PollFlags::POLLIN),
            ];
            if let Some(link) = &relay.link {
                let mut wanted = nix::poll::PollFlags::POLLIN;
                if !link.outbox.is_empty() {
                    wanted |= nix::poll::PollFlags::POLLOUT;
                }
                fds.push(nix::poll::PollFd::new(link.stream.as_fd(), wanted));
            }
            let pending = relay
                .link
                .as_ref()
                .is_some_and(|link| link.inbox.has_frame());
            nix::poll::poll(
                &mut fds,
                if pending {
                    nix::poll::PollTimeout::ZERO
                } else {
                    tick
                },
            )
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Err(error) = waited
            && error != nix::errno::Errno::EINTR
        {
            guest.say(format!("waiting for datagrams: {error}"));
            std::thread::sleep(TICK);
            continue;
        }
        relay.peers.sweep(Instant::now());
        relay.exchange();
        relay.answers();
        relay.take_the_macs(&mut datagram, stop);
    }
    if let Some(link) = relay.link {
        let _ = link.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// A new stream to the guest, what was read past its welcome taken first;
/// non-blocking (this relay's own connection: nothing else reads or writes
/// it).
fn joined(guest: &Arc<dyn Guest>) -> Result<Link, String> {
    let (stream, unread) = guest.join()?;
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(Link {
        stream,
        inbox: forward::Inbox::with(&unread),
        outbox: forward::Outbox::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// A guest whose port echoes each datagram back, `echo:` before it, to
    /// the peer it came from; a stream ended after `last` frames, if set.
    #[derive(Default)]
    struct Echo {
        joined: AtomicUsize,
        last: Option<usize>,
        said: Mutex<Vec<String>>,
    }

    impl Guest for Echo {
        fn join(&self) -> Result<(UnixStream, Vec<u8>), String> {
            let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
            self.joined.fetch_add(1, Ordering::SeqCst);
            let last = self.last;
            std::thread::spawn(move || {
                let mut reader = theirs.try_clone().unwrap();
                let mut writer = theirs;
                let mut datagram = Vec::new();
                let mut frames = 0;
                while let Ok(Some(id)) = forward::read_datagram(&mut reader, &mut datagram) {
                    let mut back = b"echo:".to_vec();
                    back.extend_from_slice(&datagram);
                    if forward::write_datagram(&mut writer, id, &back).is_err() {
                        break;
                    }
                    frames += 1;
                    if last == Some(frames) {
                        break;
                    }
                }
            });
            Ok((ours, Vec::new()))
        }
        fn say(&self, line: String) {
            self.said.lock().unwrap().push(line);
        }
    }

    /// A relay of `echo` on a socket of its own; its address, and how to
    /// stop it.
    fn relaying(echo: &Arc<Echo>) -> (SocketAddr, impl FnOnce()) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let address = socket.local_addr().unwrap();
        let (woken, mut wake) = std::io::pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let guest: Arc<dyn Guest> = Arc::clone(echo) as Arc<dyn Guest>;
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || relay(&socket, &woken, &stopped, &guest));
        let end = move || {
            stop.store(true, Ordering::SeqCst);
            std::io::Write::write_all(&mut wake, b"x").unwrap();
            thread.join().unwrap();
        };
        (address, end)
    }

    fn peer() -> UdpSocket {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        peer
    }

    /// Two peers each get their own replies, over one stream to the guest.
    #[test]
    fn each_peer_gets_its_own_replies_over_one_stream() {
        let echo = Arc::new(Echo::default());
        let (address, end) = relaying(&echo);
        let peers = [peer(), peer()];
        let mut reply = [0u8; 64];
        // Each round's longer than the last: an answer's size never cuts
        // the next datagram short.
        for round in 0..3 {
            for (n, peer) in peers.iter().enumerate() {
                let sent = format!("{n}-{round}{}", "x".repeat(round * 10));
                peer.send_to(sent.as_bytes(), address).unwrap();
                let got = peer.recv(&mut reply).unwrap();
                assert_eq!(&reply[..got], format!("echo:{sent}").as_bytes());
            }
        }
        assert_eq!(echo.joined.load(Ordering::SeqCst), 1, "one stream");
        end();
    }

    /// A stream the guest ends is joined again for the next datagram.
    #[test]
    fn a_broken_stream_is_joined_again() {
        let echo = Arc::new(Echo {
            last: Some(1),
            ..Echo::default()
        });
        let (address, end) = relaying(&echo);
        let peer = peer();
        let mut reply = [0u8; 64];
        peer.send_to(b"one", address).unwrap();
        let got = peer.recv(&mut reply).unwrap();
        assert_eq!(&reply[..got], b"echo:one");
        // The relay sees the end before it's sent anything more: then the
        // next datagram joins a new stream.
        let began = Instant::now();
        loop {
            peer.send_to(b"two", address).unwrap();
            peer.set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            if let Ok(got) = peer.recv(&mut reply)
                && &reply[..got] == b"echo:two"
            {
                break;
            }
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "never joined again"
            );
        }
        assert_eq!(echo.joined.load(Ordering::SeqCst), 2);
        end();
    }

    /// A guest that answers peer 0 without end, far faster than a relay
    /// can hand answers on (10-byte answers, 512 a write), reading the Mac's
    /// datagrams only once it has answered for `holds` (at once: zero),
    /// each counted.
    struct Flood {
        holds: Duration,
        joined: AtomicUsize,
        read: Arc<AtomicUsize>,
        said: Mutex<Vec<String>>,
    }

    impl Flood {
        fn new(holds: Duration) -> Self {
            Self {
                holds,
                joined: AtomicUsize::new(0),
                read: Arc::new(AtomicUsize::new(0)),
                said: Mutex::new(Vec::new()),
            }
        }
    }

    impl Guest for Flood {
        fn join(&self) -> Result<(UnixStream, Vec<u8>), String> {
            let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
            self.joined.fetch_add(1, Ordering::SeqCst);
            let mut reader = theirs.try_clone().unwrap();
            let read = Arc::clone(&self.read);
            let holds = self.holds;
            let started = Arc::new(std::sync::Barrier::new(2));
            let go = Arc::clone(&started);
            std::thread::spawn(move || {
                go.wait();
                let mut datagram = Vec::new();
                while let Ok(Some(_)) = forward::read_datagram(&mut reader, &mut datagram) {
                    read.fetch_add(1, Ordering::SeqCst);
                }
            });
            std::thread::spawn(move || {
                let mut writer = theirs;
                let mut answers = Vec::new();
                for _ in 0..512 {
                    forward::write_datagram(&mut answers, 0, &[7u8; 10]).unwrap();
                }
                let began = Instant::now();
                let mut reading = false;
                loop {
                    if !reading && began.elapsed() >= holds {
                        reading = true;
                        started.wait();
                    }
                    if std::io::Write::write_all(&mut writer, &answers).is_err() {
                        break;
                    }
                }
            });
            Ok((ours, Vec::new()))
        }
        fn say(&self, line: String) {
            self.said.lock().unwrap().push(line);
        }
    }

    /// A relay of `guest` on a socket of its own: its address, and how to
    /// stop it (failing if the relay hasn't ended 5 s after it's told to).
    fn relaying_any(guest: Arc<dyn Guest>) -> (SocketAddr, impl FnOnce()) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let address = socket.local_addr().unwrap();
        let (woken, mut wake) = std::io::pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let (ended, end_seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            relay(&socket, &woken, &stopped, &guest);
            let _ = ended.send(());
        });
        let end = move || {
            stop.store(true, Ordering::SeqCst);
            std::io::Write::write_all(&mut wake, b"x").unwrap();
            assert!(
                end_seen.recv_timeout(Duration::from_secs(5)).is_ok(),
                "the relay hasn't stopped 5 s after it was told to"
            );
        };
        (address, end)
    }

    /// A guest that never stops answering never holds the relay from its
    /// stop (13.8c: `answers` took them while any was there): it stops
    /// within a few seconds, with answers still coming.
    #[test]
    fn a_flooding_guest_never_holds_a_stop() {
        let flood = Arc::new(Flood::new(Duration::ZERO));
        let (address, end) = relaying_any(Arc::clone(&flood) as Arc<dyn Guest>);
        let peer = peer();
        peer.send_to(b"start", address).unwrap();
        // The control: the flood reaches the peer.
        let mut reply = [0u8; 2048];
        assert_eq!(peer.recv(&mut reply).unwrap(), 10);
        end();
    }

    /// A guest that writes for a while before it reads (its answers filling
    /// the stream while the Mac's datagrams fill the other way) keeps its
    /// stream: neither side waits to write, so nothing stalls into a reset
    /// (13.8c: the Mac's write waited 1 s, then broke the stream off). The
    /// Mac's datagrams reach it once it reads (some dropped by a full queue,
    /// as UDP drops them), over the one stream.
    #[test]
    fn a_guest_that_writes_without_reading_keeps_its_stream() {
        let flood = Arc::new(Flood::new(Duration::from_secs(2)));
        let (address, end) = relaying_any(Arc::clone(&flood) as Arc<dyn Guest>);
        let peer = peer();
        let datagram = [1u8; 1000];
        let began = Instant::now();
        while flood.read.load(Ordering::SeqCst) < 50 {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "the guest read {} of the Mac's",
                flood.read.load(Ordering::SeqCst)
            );
            peer.send_to(&datagram, address).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        end();
        assert_eq!(flood.joined.load(Ordering::SeqCst), 1, "one stream");
        assert!(
            flood.said.lock().unwrap().is_empty(),
            "{:?}",
            flood.said.lock().unwrap()
        );
    }

    /// A peer's ID is its own while it's heard; past [`MAX_UDP_PEERS`] the
    /// least recently heard is forgotten; one idle past [`UDP_IDLE`] is too.
    #[test]
    fn peers_are_remembered_while_heard() {
        let at = |port: u16| SocketAddr::from(([127, 0, 0, 1], port));
        let start = Instant::now();
        let mut peers = Peers::default();
        let first = peers.id_of(at(1), start);
        assert_eq!(peers.id_of(at(1), start), first);
        assert_ne!(peers.id_of(at(2), start), first);
        assert_eq!(peers.peer_of(first, start), Some(at(1)));
        for port in 3..=u16::try_from(MAX_UDP_PEERS).unwrap() {
            peers.id_of(at(port), start + Duration::from_millis(u64::from(port)));
        }
        assert_eq!(peers.len(), MAX_UDP_PEERS);
        // Peer 1 heard again, later than any: peer 2 is the least recent.
        peers.peer_of(first, start + Duration::from_secs(1));
        peers.id_of(at(9999), start + Duration::from_secs(1));
        assert_eq!(peers.len(), MAX_UDP_PEERS);
        assert_eq!(
            peers.peer_of(first, start),
            Some(at(1)),
            "heard lately, kept"
        );
        assert!(!peers.by_address.contains_key(&at(2)), "the least recent");
        peers.sweep(start + Duration::from_secs(1) + UDP_IDLE);
        assert_eq!(peers.len(), 0, "all idle");
    }
}
