//! The datagram frames a forwarded UDP port's stream carries (step 13.5a):
//! what the guest sends the Mac's host, read as it comes. Every frame is
//! read whole or refused, never a panic and never more than its length
//! held; each read back is the datagram it says, and written again it's
//! the same bytes. An `Inbox` (13.8c) fed the same bytes in any pieces
//! (the first byte says how big) hands on the very same frames.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_runtime::attach::forward::{Inbox, MAX_DATAGRAM, read_datagram, write_datagram};

fuzz_target!(|data: &[u8]| {
    let Some((&piece, data)) = data.split_first() else {
        return;
    };
    let mut stream = data;
    let mut datagram = Vec::new();
    let mut consumed = 0;
    let mut frames = Vec::new();
    while let Ok(Some(peer)) = read_datagram(&mut stream, &mut datagram) {
        assert!(datagram.len() <= MAX_DATAGRAM);
        let length = 6 + datagram.len();
        let mut again = Vec::new();
        write_datagram(&mut again, peer, &datagram).unwrap();
        assert_eq!(again, data[consumed..consumed + length]);
        consumed += length;
        frames.push((peer, datagram.clone()));
    }
    let mut inbox = Inbox::default();
    let mut taken = Vec::new();
    for chunk in data.chunks(usize::from(piece).max(1)) {
        inbox.feed(chunk);
        while let Some(peer) = inbox.next(&mut datagram) {
            taken.push((peer, datagram.clone()));
        }
    }
    assert_eq!(taken, frames);
});
