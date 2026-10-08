//! A registry's documents: a manifest, an index (and the platform chosen
//! from it), an image's config. Any bytes give an error or a value, never a
//! panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_image::{ImageConfig, ImageManifest, Index, Platform};

fuzz_target!(|data: &[u8]| {
    let _ = ImageManifest::parse(data);
    if let Ok(index) = Index::parse(data) {
        let _ = index.select(&Platform::host());
    }
    if let Ok(config) = ImageConfig::parse(data) {
        let _ = config.platform().runs_on(&Platform::host());
    }
});
