//! A layer extracted from any (uncompressed) archive: whatever it holds, it
//! may fail, but nothing outside the layer's directory changes. A canary
//! directory beside it must keep its one file, as it was.
//!
//! Run as a normal user, never root: an escape found here must not reach
//! anything that matters.
#![no_main]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use libfuzzer_sys::fuzz_target;
use shipyard_image::{Digest, LayerSpec, Limits, Ownership, UnpackOptions, fsat, unpack_layer};

/// A fresh directory for one input, removed afterwards.
fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("shipyard-fuzz-{}-{n}", std::process::id()));
    std::fs::create_dir(&dir).expect("making the scratch directory");
    dir
}

fn assert_alive(outside: &Path) {
    let canary = std::fs::read(outside.join("canary")).expect("the canary is gone");
    assert_eq!(canary, b"alive", "the canary outside the layer was changed");
    let beside = std::fs::read_dir(outside).expect("reading beside the canary").count();
    assert_eq!(beside, 1, "something was made beside the canary");
}

fuzz_target!(|data: &[u8]| {
    let dir = scratch();
    let outside = dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("canary"), "alive").unwrap();
    std::fs::create_dir(dir.join("layer")).unwrap();

    let spec = LayerSpec {
        // Wrong on purpose: the digests are checked after the last entry, so
        // every entry is placed first.
        digest: Digest::sha256(b""),
        media_type: "application/vnd.oci.image.layer.v1.tar".to_owned(),
        diff_id: Digest::sha256(b""),
    };
    let options = UnpackOptions {
        ownership: Ownership::Keep,
        limits: Limits {
            min_free: Some(0),
            ..Limits::default()
        },
        trusted_xattrs: false,
    };
    let layer = fsat::open_dir(&dir.join("layer")).unwrap();
    let _ = unpack_layer(data, &spec, &layer, &[], &options);
    drop(layer);

    assert_alive(&outside);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        2,
        "something was made beside the layer"
    );
    let parent = fsat::open_dir(&std::env::temp_dir()).unwrap();
    fsat::remove_tree_at(&parent, dir.file_name().unwrap()).expect("removing the scratch");
});
