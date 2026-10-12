//! A runc `process.json` (`exec --process`, stage 8, step 8.4a): what a
//! caller gives a privileged `exec`, cut and garbled anywhere. Read or
//! refused, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = shipyard_runtime::oci::process_from_json(text, std::path::Path::new("process.json"));
    }
});
