//! A line of a container's log, as `logs` reads it (stage 8, D15): its
//! records are partly the container's bytes, and a file may be torn by a
//! crash. Any line must be read or refused, never panic; any time text
//! too.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_runtime::logs::cri::parse_record;
use shipyard_runtime::logs::time::Timestamp;

fuzz_target!(|data: &[u8]| {
    // A line, as the reader hands it over: up to its newline.
    let line = data.split(|&byte| byte == b'\n').next().unwrap_or_default();
    if let Ok(record) = parse_record(line) {
        // What's read back is what would be written again.
        let mut again = Vec::new();
        shipyard_runtime::logs::cri::write_record(
            &mut again,
            record.time,
            record.stream,
            record.partial,
            record.text,
        );
        assert!(parse_record(&again[..again.len() - 1]).is_ok());
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(time) = Timestamp::parse(text) {
            assert_eq!(Timestamp::parse(&time.to_string()), Ok(time));
        }
    }
});
