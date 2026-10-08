//! Shipyard's own tar reader over any bytes: every entry it gives, within
//! its limits, and an entry name cleaned the way paths are.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_image::tar_reader::{TarLimits, TarReader, clean_path};

fuzz_target!(|data: &[u8]| {
    let limits = TarLimits::default();
    let _ = clean_path(data, &limits);
    let mut reader = TarReader::new(data, limits);
    while let Ok(Some(_entry)) = reader.next_entry() {}
});
