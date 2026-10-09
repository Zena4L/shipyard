//! What a container sends a network's DNS server, and what an upstream
//! resolver sends back: names compressed into loops, labels and names too
//! long, counts that lie. Anything must be a clear error, never a panic or
//! a hang, and an answer the server gives must read back.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_network::fuzzing;

fuzz_target!(|data: &[u8]| {
    fuzzing::dns(data);
});
