//! The bytes between two connected sockets, both ways (step 10.3c1): a
//! forwarded port's (the Mac's TCP connection and the VM's vsock one, the
//! guest's vsock one and the container's TCP one) and a forwarded
//! command's. One direction is copied on a thread of its own, the other on
//! the caller's; each one's end is passed on as a half-close
//! (`shutdown(SHUT_WR)`: the other side's reads end, its writes go on).

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Relays `one` and `other` until both directions have ended. Once one
/// has, the other has `linger` to end too, and is then cut (both sockets
/// shut down): a peer that never closes its side (an idle pooled client
/// whose server went) doesn't hold the relay for good.
///
/// # Errors
///
/// The sockets can't be copied, or no thread can be made: nothing was
/// relayed (both are closed as they're dropped).
pub fn both_ways(one: OwnedFd, other: OwnedFd, linger: Duration) -> io::Result<()> {
    let ends = (one.as_raw_fd(), other.as_raw_fd());
    let ended = Arc::new((Mutex::new(0_u8), Condvar::new()));
    let (one_in, other_out) = (File::from(one.try_clone()?), File::from(other.try_clone()?));
    let theirs = Arc::clone(&ended);
    let outward = std::thread::Builder::new()
        .spawn(move || copy_then_end(one_in, other_out, &theirs, ends, linger))?;
    copy_then_end(File::from(other), File::from(one), &ended, ends, linger);
    let _ = outward.join();
    Ok(())
}

/// Copies `from` to `to`, half-closes `to`, then gives the other direction
/// `linger` to end, and cuts both `ends` if it hasn't. The descriptors
/// behind `ends` stay open as long as either direction runs (each holds a
/// copy), so a cut never reaches another socket.
fn copy_then_end(
    mut from: File,
    mut to: File,
    ended: &(Mutex<u8>, Condvar),
    ends: (RawFd, RawFd),
    linger: Duration,
) {
    let _ = io::copy(&mut from, &mut to);
    let _ = nix::sys::socket::shutdown(to.as_raw_fd(), nix::sys::socket::Shutdown::Write);
    let (count, changed) = ended;
    let Ok(mut done) = count.lock() else {
        return;
    };
    *done += 1;
    changed.notify_all();
    let Ok((done, waited)) = changed.wait_timeout_while(done, linger, |done| *done < 2) else {
        return;
    };
    if waited.timed_out() && *done < 2 {
        for end in [ends.0, ends.1] {
            let _ = nix::sys::socket::shutdown(end, nix::sys::socket::Shutdown::Both);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use std::io::{Read as _, Write as _};
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
