//! `import --change` (step 12.2b): any bytes as changes, a line each, read
//! as an import reads them. It may refuse, never panic; what it makes stays
//! within its bounds (an `EXPOSE` range of at most 1024 ports a word).
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_image::changes::{Changes, MAX_CHANGES, MAX_RANGE};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let changes: Vec<String> = text.lines().map(str::to_owned).collect();
    if let Ok(made) = Changes::parse(&changes) {
        assert!(changes.len() <= MAX_CHANGES);
        let words: usize = changes.iter().map(|change| change.split_whitespace().count()).sum();
        let most = usize::try_from(MAX_RANGE).unwrap_or(usize::MAX).saturating_mul(words);
        assert!(made.exposed.len() <= most, "more ports than the ranges allow");
    }
});
