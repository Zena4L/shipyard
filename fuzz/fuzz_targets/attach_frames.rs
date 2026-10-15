//! What an attach client sends the monitor (stage 8, step 8.3c): frames from
//! a peer that may be hostile, cut anywhere. Every frame is read or refused,
//! never a panic; what's held is never more than one frame not yet whole;
//! and the detach matcher detaches exactly when the keys are typed, passing
//! on everything before them and nothing of them. Every other frame read
//! is written back the same (stage 9's control frames too).
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_runtime::attach::{Decoder, DetachKeys, Frame, MAX_PAYLOAD};

/// A frame's header: its kind and length.
const HEADER: usize = 5;

const KEYS: &[u8] = b"\x10\x11";

fuzz_target!(|data: &[u8]| {
    let Some((&cut, rest)) = data.split_first() else {
        return;
    };
    let mut decoder = Decoder::new();
    let at = usize::from(cut).min(rest.len());
    let mut matcher = DetachKeys::default_keys().matcher();
    // Everything typed, and everything passed on, so far.
    let mut typed = Vec::new();
    let mut passed = Vec::new();
    for piece in [&rest[..at], &rest[at..]] {
        decoder.push(piece);
        loop {
            match decoder.next_frame() {
                Ok(Some(Frame::Stdin(bytes))) => {
                    typed.extend_from_slice(&bytes);
                    let detached = matcher.feed(&bytes, &mut passed);
                    let found = typed.windows(KEYS.len()).position(|w| w == KEYS);
                    assert_eq!(detached, found.is_some());
                    match found {
                        Some(at) => {
                            assert_eq!(passed, typed[..at]);
                            return;
                        }
                        // Held back: at most a ctrl-p at the end.
                        None => {
                            let held = usize::from(typed.ends_with(&KEYS[..1]));
                            assert_eq!(passed, typed[..typed.len() - held]);
                        }
                    }
                }
                // Output with nothing in it: read, but never written (an
                // empty write makes no frame), so nothing to write back.
                Ok(Some(Frame::Stdout(bytes) | Frame::Stderr(bytes))) if bytes.is_empty() => {}
                Ok(Some(frame)) => {
                    let mut again = Vec::new();
                    frame.encode(&mut again);
                    let mut check = Decoder::new();
                    check.push(&again);
                    assert_eq!(check.next_frame(), Ok(Some(frame)));
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
        assert!(decoder.held() < HEADER + MAX_PAYLOAD);
    }
});
