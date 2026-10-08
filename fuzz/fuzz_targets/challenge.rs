//! A registry's `WWW-Authenticate` header, and its token server's answer.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_image::fuzzing;

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = std::str::from_utf8(data) {
        fuzzing::challenges(header);
    }
    fuzzing::token(data);
});
