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

/// Relays `one` and `other` until both directions have ended. Once one
/// has, the other has `linger` from its last byte to end too (a side still
/// sending is never cut: F180(f)), and is then cut (both
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
            bound: None,
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
    /// `other`'s bytes all reached `one`: read to `other`'s own end, each
    /// write taken, before any cut. `false`: a write to `one` failed (its
    /// peer gone), or the relay was cut first, so `other`'s side may still
    /// be sending, and a caller whose `other` can't be told of `one`'s end
    /// says so another way (F180(f): the Mac hangs up a forwarded port's
    /// relay in the guest then, and only then).
    pub other_clean: bool,
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
    /// The longest a direction still sending holds the relay past the
    /// other's end (`None`: as long as bytes come, F180(f)): a backstop for
    /// a relay whose other end may never be told the first has gone (the
    /// guest's forwarded ports, should the Mac's hang-up be lost).
    pub bound: Option<Duration>,
}

/// [`both_ways`], the wait for the second end set by which side ended
/// first ([`Lingers`]); how it ended.
///
/// # Errors
///
/// As [`both_ways`]'s.
pub fn both_ways_lingering(one: OwnedFd, other: OwnedFd, lingers: Lingers) -> io::Result<Ended> {
    let ends = Ends {
        fds: (one.as_raw_fd(), other.as_raw_fd()),
        cut: Arc::new(AtomicBool::new(false)),
    };
    let ended = Arc::new((Mutex::new(0_u8), Condvar::new()));
    let moved = Arc::new(Moved::new());
    let one_gone = Arc::new(AtomicBool::new(false));
    let (one_in, other_out) = (File::from(one.try_clone()?), File::from(other.try_clone()?));
    let theirs = (Arc::clone(&ended), Arc::clone(&moved), ends.clone());
    let gone = Arc::clone(&one_gone);
    let (after_one, bound) = (lingers.after_one, lingers.bound);
    let outward = std::thread::Builder::new().spawn(move || {
        let (ended, moved, ends) = theirs;
        let (_, cut) = copy_then_end(
            one_in,
            other_out,
            (&ended, &moved),
            &ends,
            (after_one, bound),
        );
        if cut == Cut::PeerGone {
            gone.store(true, Ordering::Release);
        }
    })?;
    let (other_clean, _) = copy_then_end(
        File::from(other),
        File::from(one),
        (&ended, &moved),
        &ends,
        (lingers.after_other, lingers.bound),
    );
    let _ = outward.join();
    Ok(Ended {
        one_gone: one_gone.load(Ordering::Acquire),
        other_clean,
    })
}

/// A relay's two sockets, and whether it has cut them.
#[derive(Clone)]
struct Ends {
    fds: (RawFd, RawFd),
    cut: Arc<AtomicBool>,
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
/// each write stamped in `moved`: whether it was `from`'s end (`false`: a
/// read or a write failed). A read, then a write: never `io::copy`, whose
/// `splice` on Linux can hold bytes back (AGENTS.md §10).
fn copy_stamping(from: &mut File, to: &mut File, moved: &Moved) -> bool {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return true,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        };
        if to
            .write_all(buffer.get(..read).unwrap_or_default())
            .is_err()
        {
            return false;
        }
        moved.stamp();
    }
}

/// When a direction that has ended cuts the other, which `moved` last at
/// `last`: `linger` after that, or after its own end (`ended_at`) if that's
/// later, however long bytes keep coming (F180(f): a Mac client reading a
/// long download after its half-close; one that died is hung up instead,
/// `shipyard-vm`'s forwarded ports), but past `bound` after its end (or
/// `linger`, if longer) where there is one. `None`: never (an unlimited
/// linger).
fn cut_at(
    ended_at: Instant,
    last: Instant,
    (linger, bound): (Duration, Option<Duration>),
) -> Option<Instant> {
    let from_last = last.max(ended_at).checked_add(linger)?;
    let Some(bound) = bound else {
        return Some(from_last);
    };
    Some(from_last.min(ended_at.checked_add(linger.max(bound))?))
}

/// Copies `from` to `to`, half-closes `to`, then gives the other direction
/// `linger` from its last byte ([`cut_at`]) to end, and cuts both `ends`
/// if it hasn't, or as soon as
/// `from`'s peer has gone altogether (11.7: a Docker client killed in a
/// quiet `exec` on the Mac only ever reached the guest's service as a
/// half-close, so its exec ran on and its relays stayed). The descriptors
/// behind `ends` stay open as long as either direction runs (each holds a
/// copy), so a cut never reaches another socket.
///
/// Says whether `from`'s bytes all reached `to`, read to its own end before
/// any cut ([`Ended::other_clean`]), and how its wait ended.
fn copy_then_end(
    mut from: File,
    mut to: File,
    (ended, moved): (&(Mutex<u8>, Condvar), &Moved),
    ends: &Ends,
    linger: (Duration, Option<Duration>),
) -> (bool, Cut) {
    // A cut's shutdown ends this copy's read as a plain end would: asked
    // after it, as it can only have come before.
    let clean = copy_stamping(&mut from, &mut to, moved) && !ends.cut.load(Ordering::Acquire);
    let (count, changed) = ended;
    let Ok(mut done) = count.lock() else {
        return (clean, Cut::No);
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
            return (clean, Cut::Lingered);
        }
        let look = left.map_or(HANG_UP_LOOK, |left| left.min(HANG_UP_LOOK));
        let Ok((again, _)) = changed.wait_timeout_while(done, look, |done| *done < 2) else {
            return (clean, Cut::No);
        };
        done = again;
        // Under the count's lock, while the other direction hasn't ended
        // (which half-closes `from` only under it, counted at once).
        if *done < 2 && peer_gone(from.as_fd()) {
            cut(ends);
            return (clean, Cut::PeerGone);
        }
    }
    (clean, Cut::No)
}

/// Both ends shut down, both ways: each direction's reads and writes end.
/// Marked first, so a copy that ends by it isn't taken for a clean end.
fn cut(ends: &Ends) {
    ends.cut.store(true, Ordering::Release);
    for end in [ends.fds.0, ends.fds.1] {
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
            bound: None,
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
        assert!(relay.join().unwrap().one_gone);
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

    /// `other`'s bytes all reaching `one`, read to its end, is a clean end;
    /// a write to `one` that fails (its peer gone, `other` still sending)
    /// isn't ([`Ended::other_clean`], F180(f)).
    #[test]
    fn a_relay_says_whether_other_s_bytes_all_reached_one() {
        let run = |kill_client: bool| {
            let (near_one, mut client) = UnixStream::pair().unwrap();
            let (near_other, mut server) = UnixStream::pair().unwrap();
            let lingers = Lingers {
                after_one: Duration::from_secs(5),
                after_other: Duration::from_secs(5),
                bound: None,
            };
            let relay = std::thread::spawn(move || {
                both_ways_lingering(near_one.into(), near_other.into(), lingers).unwrap()
            });
            if kill_client {
                drop(client);
                // The service streams on to a client that went.
                for _ in 0..200 {
                    if server.write_all(&[b'x'; 4096]).is_err() {
                        break;
                    }
                }
                drop(server);
            } else {
                server.write_all(b"all").unwrap();
                drop(server);
                let mut got = Vec::new();
                client.read_to_end(&mut got).unwrap();
                assert_eq!(got, b"all");
                drop(client);
            }
            relay.join().unwrap()
        };
        assert!(run(false).other_clean, "a clean end");
        assert!(!run(true).other_clean, "a write to a gone client");
        // Cut: `other` never ended (a service that says nothing, its client
        // half-closed past the linger): its copy's end is the cut's, no clean
        // one.
        let (near_one, client) = UnixStream::pair().unwrap();
        let (near_other, server) = UnixStream::pair().unwrap();
        let lingers = Lingers {
            after_one: Duration::from_millis(300),
            after_other: Duration::from_secs(5),
            bound: None,
        };
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let ended = both_ways_lingering(near_one.into(), near_other.into(), lingers).unwrap();
        assert!(!ended.other_clean, "a cut");
        drop((client, server));
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
            bound: None,
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

    /// The cut comes a linger after the last byte, or the end if later,
    /// however long after the end bytes still came (F180(f)); an unlimited
    /// linger never.
    #[test]
    fn the_cut_is_a_linger_after_the_last_byte() {
        let ended = Instant::now();
        let linger = Duration::from_secs(60);
        assert_eq!(cut_at(ended, ended, (linger, None)), Some(ended + linger));
        let before = ended.checked_sub(Duration::from_secs(5)).unwrap();
        assert_eq!(cut_at(ended, before, (linger, None)), Some(ended + linger));
        let later = ended + Duration::from_secs(30);
        assert_eq!(cut_at(ended, later, (linger, None)), Some(later + linger));
        // An hour of bytes after the end: still a linger after the last.
        let much_later = ended + Duration::from_secs(3600);
        assert_eq!(
            cut_at(ended, much_later, (linger, None)),
            Some(much_later + linger),
            "never cut while bytes come"
        );
        // With a bound (the guest's forwarded ports' backstop): at most that
        // long past the end, however late the last byte.
        let bound = Duration::from_secs(600);
        assert_eq!(
            cut_at(ended, much_later, (linger, Some(bound))),
            Some(ended + bound)
        );
        assert_eq!(
            cut_at(ended, later, (linger, Some(bound))),
            Some(later + linger),
            "within the bound, a linger after the last byte"
        );
        assert_eq!(cut_at(ended, ended, (Duration::MAX, None)), None);
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
