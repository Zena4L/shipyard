//! An image archive read as `load` reads one (step 12.2a): any bytes, plain
//! or gzipped, read into a store's staging, then its images found. It may
//! fail, but never panics, never writes outside its staging (a canary
//! directory beside the store keeps its one file), and leaves nothing in
//! the store once the archive is dropped (nothing is promoted here). The
//! same bytes as a config's history, read leniently, never panic either.
//!
//! Run as a normal user, never root: an escape found here must not reach
//! anything that matters.
#![no_main]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use libfuzzer_sys::fuzz_target;
use shipyard_image::archive::load;
use shipyard_image::{Platform, Store};

/// A fresh directory for one input, removed afterwards.
fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("shipyard-fuzz-load-{}-{n}", std::process::id()));
    std::fs::create_dir(&dir).expect("making the scratch directory");
    dir
}

fn assert_alive(outside: &Path) {
    let canary = std::fs::read(outside.join("canary")).expect("the canary is gone");
    assert_eq!(canary, b"alive", "the canary outside the store was changed");
    let beside = std::fs::read_dir(outside).expect("reading beside the canary").count();
    assert_eq!(beside, 1, "something was made beside the canary");
}

/// What's in the store's `blobs/` but its `sha256/` (staging left), and
/// what's in `sha256/` (a blob promoted).
fn left(store: &Path) -> Vec<String> {
    let blobs = store.join("blobs");
    let mut left: Vec<String> = std::fs::read_dir(&blobs)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name != "sha256")
                .collect()
        })
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir(blobs.join("sha256")) {
        left.extend(entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()));
    }
    left
}

fuzz_target!(|data: &[u8]| {
    let _ = shipyard_image::history::history(data);
    let dir = scratch();
    let outside = dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("canary"), "alive").unwrap();
    let root = dir.join("store");
    let store = Store::open(&root).expect("opening the store");
    if let Ok(mut archive) = load::read(data, &store) {
        if let Ok(images) = archive.images(&Platform::host()) {
            for found in &images.found {
                let _ = archive.blobs_of(found);
            }
        }
    }
    assert_alive(&outside);
    assert_eq!(left(&root), Vec::<String>::new(), "the load left something in the store");
    let _ = std::fs::remove_dir_all(&dir);
});
