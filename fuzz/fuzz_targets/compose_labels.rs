//! Compose's `depends_on` label (step 14.1a), as a container's labels carry
//! it: any text, read as the map reads it. It never panics; it reads at
//! most 256 entries, each with a service and a condition and no separator
//! in either.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let label = String::from_utf8_lossy(data);
    let entries = shipyard_engine::fuzzing::depends_on(&label);
    assert!(entries.len() <= 256);
    for (service, condition) in entries {
        assert!(!service.is_empty() && !condition.is_empty());
        assert!(!service.contains([',', ':']) && !condition.contains([',', ':']));
    }
});
