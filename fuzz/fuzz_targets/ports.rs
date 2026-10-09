//! What a user types for `-p` (`[IP:][HOST_PORT:]CONTAINER_PORT[/tcp|udp]`)
//! and an image's `ExposedPorts` keys: anything must be a clear error,
//! never a panic (an overflowing range, a stray colon).
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_network::fuzzing;

fuzz_target!(|data: &[u8]| {
    fuzzing::ports(data);
});
