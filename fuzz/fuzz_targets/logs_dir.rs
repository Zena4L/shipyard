//! A log directory's `container.log` with any content, read by the engine
//! as `logs` does (stage 8): torn lines, lines too long, partials never
//! ended, records out of order. Read or skipped, never a panic, and the
//! reader's memory stays bounded.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use shipyard_runtime::logs::reader::{LogsOptions, read};

/// One scratch directory for the run, its `log.json` written once.
fn dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("shipyard-fuzz-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("log.json"),
            r#"{"format":"cri","version":1,"maxSize":65536,"maxFiles":2}"#,
        )
        .unwrap();
        dir
    })
}

fuzz_target!(|data: &[u8]| {
    let dir = dir();
    std::fs::write(dir.join("container.log"), data).unwrap();
    for tail in [None, Some(3)] {
        let options = LogsOptions {
            tail,
            ..LogsOptions::default()
        };
        let _ = read(dir, &options, || true, |entry| {
            assert!(entry.text.len() <= shipyard_runtime::logs::reader::MAX_LINE);
            Ok(())
        });
    }
});
