//! What a container's /proc/<pid>/net/{tcp,tcp6,udp,udp6} says: lines cut
//! short, fields too wide or not hex, a table too large. Anything must be a
//! clear error, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_network::fuzzing;

fuzz_target!(|data: &[u8]| {
    fuzzing::proc_net(data);
});
