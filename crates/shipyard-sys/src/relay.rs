//! The bytes between two connected sockets, both ways (step 10.3c1): a
//! forwarded port's (the Mac's TCP connection and the VM's vsock one, the
//! guest's vsock one and the container's TCP one) and a forwarded
//! command's. One direction is copied on a thread of its own, the other on
//! the caller's; each one's end is passed on as a half-close
//! (`shutdown(SHUT_WR)`: the other side's reads end, its writes go on).
//! A side whose peer has gone altogether (closed, not half-closed) ends
//! the relay (`peer_gone`: a zero-length send fails): what it would wait for can never come.

use std::fs::File;
use std::io;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How often a relay waiting for its second end asks whether the side
/// that ended has gone altogether.
const HANG_UP_LOOK: Duration = Duration::from_secs(1);

/// The longest a finite linger is drawn out by bytes still coming (F109):
/// a direction still sending holds the relay this long past the other's
/// end at most (or the linger, if that's longer).
const LINGER_BOUND: Duration = Duration::from_secs(600);

/// Relays `one` and `other` until both directions have ended. Once one
/// has, the other has `linger` from its last byte to end too (a side still
/// sending isn't cut, for 10 minutes at most), and is then cut (both
/// sockets shut down): a peer that never closes its side (an idle pooled
/// client whose server went) doesn't hold the relay for good. The caller ignores
/// `SIGPIPE` (Rust's default): a write to a gone peer is an `EPIPE` then,
/// on macOS too, which has no `MSG_NOSIGNAL`.
///
/// # Errors
///
/// The sockets can't be copied, or no thread can be made: nothing was
/// relayed (both are closed as they're dropped).
pub fn both_ways(one: OwnedFd, other: OwnedFd, linger: Duration) -> io::Result<()> {
    both_ways_lingering(
        one,
        other,
        Lingers {
            after_one: linger,
            after_other: linger,
        },
    )
    .map(drop)
}

/// How a relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ended {
    /// `one`'s peer had gone altogether (a zero-length send failed): the relay was cut
    /// for it. A caller whose `other` can't tell (the Mac's vsock passes a
    /// close on as a half-close) says so another way.
    pub one_gone: bool,
}

/// How long a relay waits for its other direction's end, by which side
/// ended first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lingers {
    /// Once `one` has ended (its bytes all passed on): `other` still
    /// answering a request whose input ended (`echo x | docker run -i`)
    /// may need as long as it takes.
    pub after_one: Duration,
    /// Once `other` has ended: `one` that never closes (a pooled client
    /// whose server went) is cut after this.
    pub after_other: Duration,
}

/// [`both_ways`], the wait for the second end set by which side ended
/// first ([`Lingers`]); how it ended.
///
/// # Errors
///
/// As [`both_ways`]'s.
pub fn both_ways_lingering(one: OwnedFd, other: OwnedFd, lingers: Lingers) -> io::Result<Ended> {
    let ends = (one.as_raw_fd(), other.as_raw_fd());
    let ended = Arc::new((Mutex::new(0_u8), Condvar::new()));
    let moved = Arc::new(Moved::new());
    let one_gone = Arc::new(AtomicBool::new(false));
    let (one_in, other_out) = (File::from(one.try_clone()?), File::from(other.try_clone()?));
    let theirs = (Arc::clone(&ended), Arc::clone(&moved));
    let gone = Arc::clone(&one_gone);
    let outward = std::thread::Builder::new().spawn(move || {
        let (ended, moved) = theirs;
        if copy_then_end(one_in, other_out, (&ended, &moved), ends, lingers.after_one)
            == Cut::PeerGone
        {
            gone.store(true, Ordering::Release);
        }
    })?;
    copy_then_end(
        File::from(other),
        File::from(one),
        (&ended, &moved),
        ends,
        lingers.after_other,
    );
    let _ = outward.join();
    Ok(Ended {
        one_gone: one_gone.load(Ordering::Acquire),
    })
}

/// How one direction's wait for the other ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// The other ended too (or the wait couldn't be had).
    No,
    /// The linger ran out.
    Lingered,
    /// The side it read from had gone altogether.
    PeerGone,
}

/// When a relay's bytes last moved, either way.
struct Moved {
    began: Instant,
    /// Milliseconds after `began`.
    last: AtomicU64,
}

impl Moved {
    fn new() -> Self {
        Self {
            began: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    /// Bytes moved now.
    fn stamp(&self) {
        let now = u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.fetch_max(now, Ordering::Relaxed);
    }

    /// When they last did.
    fn last(&self) -> Instant {
        self.began + Duration::from_millis(self.last.load(Ordering::Relaxed))
    }
}

/// Copies `from` to `to` until `from`'s end (or a failure either side),
/// each write stamped in `moved`. A read, then a write: never `io::copy`,
/// whose `splice` on Linux can hold bytes back (AGENTS.md §10).
fn copy_stamping(from: &mut File, to: &mut File, moved: &Moved) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if to
            .write_all(buffer.get(..read).unwrap_or_default())
            .is_err()
        {
            return;
        }
        moved.stamp();
    }
}

/// When a direction that has ended cuts the other, which `moved` last at
/// `last`: `linger` after that, or after its own end (`ended_at`) if that's
/// later; never past [`LINGER_BOUND`] (or `linger`, if longer) after its
/// end. `None`: never (an unlimited linger).
fn cut_at(ended_at: Instant, last: Instant, linger: Duration) -> Option<Instant> {
    let from_last = last.max(ended_at).checked_add(linger)?;
    let bound = ended_at.checked_add(linger.max(LINGER_BOUND))?;
    Some(from_last.min(bound))
}

/// Copies `from` to `to`, half-closes `to`, then gives the other direction
/// `linger` from its last byte ([`cut_at`]) to end, and cuts both `ends`
/// if it hasn't, or as soon as
/// `from`'s peer has gone altogether (11.7: a Docker client killed in a
/// quiet `exec` on the Mac only ever reached the guest's service as a
/// half-close, so its exec ran on and its relays stayed). The descriptors
/// behind `ends` stay open as long as either direction runs (each holds a
/// copy), so a cut never reaches another socket.
fn copy_then_end(
    mut from: File,
    mut to: File,
    (ended, moved): (&(Mutex<u8>, Condvar), &Moved),
    ends: (RawFd, RawFd),
    linger: Duration,
) -> Cut {
    copy_stamping(&mut from, &mut to, moved);
    let (count, changed) = ended;
    let Ok(mut done) = count.lock() else {
        return Cut::No;
    };
    // The half-close under the count's lock: the other direction's probe of
    // this same socket (its `from`) never sees this side's own shutdown as
    // its peer gone (11.7's Murphy review).
    let _ = nix::sys::socket::shutdown(to.as_raw_fd(), nix::sys::socket::Shutdown::Write);
    *done += 1;
    changed.notify_all();
    let ended_at = Instant::now();
    while *done < 2 {
        // Again each look: the other direction's bytes move it on. None: no
        // limit (`Duration::MAX`).
        let deadline = cut_at(ended_at, moved.last(), linger);
        let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if left == Some(Duration::ZERO) {
            cut(ends);
            return Cut::Lingered;
        }
        let look = left.map_or(HANG_UP_LOOK, |left| left.min(HANG_UP_LOOK));
        let Ok((again, _)) = changed.wait_timeout_while(done, look, |done| *done < 2) else {
            return Cut::No;
        };
        done = again;
        // Under the count's lock, while the other direction hasn't ended
        // (which half-closes `from` only under it, counted at once).
        if *done < 2 && peer_gone(from.as_fd()) {
            cut(ends);
            return Cut::PeerGone;
        }
    }
    Cut::No
}

/// Both ends shut down, both ways: each direction's reads and writes end.
fn cut(ends: (RawFd, RawFd)) {
    for end in [ends.0, ends.1] {
        let _ = nix::sys::socket::shutdown(end, nix::sys::socket::Shutdown::Both);
    }
}

/// Whether the peer of `socket` has gone altogether: a zero-length send
/// fails (`EPIPE`, `ECONNRESET`, `ENOTCONN`) once it has closed, and
/// succeeds while it has only half-closed. Not `POLLHUP`: checked in 11.7,
/// macOS's UNIX sockets and Linux's vsock ones don't say it for a closed
/// peer, while all three kinds this relays fail the send. Any other answer
/// (not a socket, a TCP peer whose close hasn't reset it) isn't a hang-up.
/// macOS has no `MSG_NOSIGNAL`: a `SIGPIPE` there is ignored, as the relay's
/// own writes already need (Rust's default, which nothing here changes).
fn peer_gone(socket: BorrowedFd<'_>) -> bool {
    use nix::errno::Errno;
    #[cfg(target_os = "linux")]
    let flags = nix::sys::socket::MsgFlags::MSG_NOSIGNAL | nix::sys::socket::MsgFlags::MSG_DONTWAIT;
    #[cfg(not(target_os = "linux"))]
    let flags = nix::sys::socket::MsgFlags::MSG_DONTWAIT;
    matches!(
        nix::sys::socket::send(socket.as_raw_fd(), &[], flags),
        Err(Errno::EPIPE | Errno::ECONNRESET | Errno::ENOTCONN)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    use super::*;

    /// A pair of relayed sockets: the relay's ends in, the far ends out.
    fn relayed(linger: Duration) -> (UnixStream, UnixStream, std::thread::JoinHandle<()>) {
        let (near_one, far_one) = UnixStream::pair().unwrap();
        let (near_other, far_other) = UnixStream::pair().unwrap();
        let relay = std::thread::spawn(move || {
            both_ways(near_one.into(), near_other.into(), linger).unwrap();
        });
        (far_one, far_other, relay)
    }

    /// No limit (`Duration::MAX`, an API connection's after its client's
    /// end): a client that half-closed still gets what the server says long
    /// after, and the relay ends with the server's end (the long wait
    /// neither panics nor cuts).
    #[test]
    fn an_unlimited_linger_waits_for_the_other_end() {
        let (mut client, mut server, relay) = relayed(Duration::MAX);
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut nothing = Vec::new();
        server.read_to_end(&mut nothing).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        server.write_all(b"later").unwrap();
        drop(server);
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"later");
        relay.join().unwrap();
    }

    /// A client that has gone altogether (closed, not half-closed) ends a
    /// relay whose linger after it is unlimited, within a few looks, and
    /// its server sees its connection end (11.7: a killed `docker exec`
    /// client on the Mac). The control: one that only half-closed is still
    /// served after several looks, its answer delivered.
    #[test]
    fn a_client_gone_ends_the_relay_and_a_half_close_doesnt() {
        let (near_one, client) = UnixStream::pair().unwrap();
        let (near_other, mut server) = UnixStream::pair().unwrap();
        let lingers = Lingers {
            after_one: Duration::MAX,
            after_other: Duration::MAX,
        };
        let relay = std::thread::spawn(move || {
            both_ways_lingering(near_one.into(), near_other.into(), lingers).unwrap()
        });
        drop(client);
        let mut nothing = Vec::new();
        server.read_to_end(&mut nothing).unwrap();
        let began = Instant::now();
        while !relay.is_finished() {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "a gone client still relayed after 10 s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(relay.join().unwrap(), Ended { one_gone: true });
        // Cut: the server's side reads its end, and can't write on.
        let mut rest = Vec::new();
        assert_eq!(server.read_to_end(&mut rest).unwrap(), 0);

        let (mut client, mut server, relay) = relayed(Duration::MAX);
        client.shutdown(std::net::Shutdown::Write).unwrap();
        server.read_to_end(&mut nothing).unwrap();
        std::thread::sleep(HANG_UP_LOOK * 3);
        assert!(!relay.is_finished(), "a half-closed client was cut");
        server.write_all(b"later").unwrap();
        drop(server);
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"later");
        relay.join().unwrap();
    }

    /// Lingers by side: `one` (the client) that never closes, once
    /// `other` (the service) has ended, is cut after `after_other`, though
    /// `after_one` is unlimited (an API connection's pooled client).
    #[test]
    fn a_client_that_never_closes_is_cut_after_the_services_end() {
        let (near_one, client) = UnixStream::pair().unwrap();
        let (near_other, server) = UnixStream::pair().unwrap();
        let lingers = Lingers {
            after_one: Duration::MAX,
            after_other: Duration::from_millis(300),
        };
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            both_ways_lingering(near_one.into(), near_other.into(), lingers).unwrap();
            let _ = done.send(());
        });
        drop(server);
        // (Bounded: a relay that waits for the client fails, not hangs.)
        assert!(
            ended.recv_timeout(Duration::from_secs(5)).is_ok(),
            "still relaying 5 s after the service's end"
        );
        drop(client);
    }

    /// Bytes go both ways, and each side's close ends the other's reading,
    /// while what it still sends goes on (a half-close).
    #[test]
    fn bytes_go_both_ways_and_a_close_is_half() {
        let (mut client, mut server, relay) = relayed(Duration::from_secs(60));
        client.write_all(b"request").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"request");
        server.write_all(b"answer").unwrap();
        drop(server);
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"answer");
        relay.join().unwrap();
    }

    /// The cut comes a linger after the last byte, or the end if later, and
    /// never past the bound; an unlimited linger never.
    #[test]
    fn the_cut_is_a_linger_after_the_last_byte_within_a_bound() {
        let ended = Instant::now();
        let linger = Duration::from_secs(60);
        assert_eq!(cut_at(ended, ended, linger), Some(ended + linger));
        let before = ended.checked_sub(Duration::from_secs(5)).unwrap();
        assert_eq!(cut_at(ended, before, linger), Some(ended + linger));
        let later = ended + Duration::from_secs(30);
        assert_eq!(cut_at(ended, later, linger), Some(later + linger));
        let much_later = ended + LINGER_BOUND;
        assert_eq!(
            cut_at(ended, much_later, linger),
            Some(ended + LINGER_BOUND),
            "bounded"
        );
        let long = LINGER_BOUND * 2;
        assert_eq!(cut_at(ended, much_later, long), Some(ended + long));
        assert_eq!(cut_at(ended, ended, Duration::MAX), None);
    }

    /// A side still sending after the other's end isn't cut while its bytes
    /// keep coming within the linger (F109: a command's output past its
    /// input's end), and is once they stop.
    #[test]
    fn a_side_still_sending_is_not_cut() {
        let linger = Duration::from_secs(2);
        let (mut client, mut server, relay) = relayed(linger);
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut nothing = Vec::new();
        server.read_to_end(&mut nothing).unwrap();
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            client.read_to_end(&mut got).unwrap();
            got
        });
        // Past a linger and a half in all, a byte every eighth of one.
        for _ in 0..12 {
            server.write_all(b"x").unwrap();
            std::thread::sleep(linger / 8);
        }
        assert!(!relay.is_finished(), "cut while bytes still came");
        let began = Instant::now();
        while !relay.is_finished() {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "never cut once the bytes stopped"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        relay.join().unwrap();
        assert_eq!(reader.join().unwrap(), b"x".repeat(12));
    }

    /// One side gone and the other idle, open: the relay ends after the
    /// linger, not never; the idle side's reading ends.
    #[test]
    fn an_idle_side_is_cut_after_the_linger() {
        let (mut idle, server, relay) = relayed(Duration::from_millis(200));
        drop(server);
        let began = Instant::now();
        while !relay.is_finished() {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "the relay waits for the idle side for good"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        relay.join().unwrap();
        let mut rest = Vec::new();
        idle.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }
}
