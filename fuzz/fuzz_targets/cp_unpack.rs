//! A copied archive extracted from any bytes, as `cp` extracts one
//! (`unpack_tar`, with a top-level name to rename): whatever it holds, it
//! may fail, but nothing outside the destination changes. A canary
//! directory beside it must keep its one file, as it was.
//!
//! Run as a normal user, never root: an escape found here must not reach
//! anything that matters.
#![no_main]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use libfuzzer_sys::fuzz_target;
use shipyard_image::{CopyOptions, Limits, Ownership, Top, fsat, unpack_tar};

/// A fresh directory for one input, removed afterwards.
fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("shipyard-fuzz-cp-{}-{n}", std::process::id()));
    std::fs::create_dir(&dir).expect("making the scratch directory");
    dir
}

fn assert_alive(outside: &Path) {
    let canary = std::fs::read(outside.join("canary")).expect("the canary is gone");
    assert_eq!(canary, b"alive", "the canary outside the destination was changed");
    let beside = std::fs::read_dir(outside).expect("reading beside the canary").count();
    assert_eq!(beside, 1, "something was made beside the canary");
}

fuzz_target!(|data: &[u8]| {
    // The first byte picks the options: a top-level name or none, renamed
    // or not.
    let Some((&choice, archive)) = data.split_first() else {
        return;
    };
    let dir = scratch();
    let outside = dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("canary"), "alive").unwrap();
    std::fs::create_dir(dir.join("dest")).unwrap();
    // Something there already: replaced, merged or refused.
    std::fs::create_dir(dir.join("dest/a")).unwrap();
    std::fs::write(dir.join("dest/b"), "old").unwrap();

    let top = (choice & 1 == 1).then(|| Top {
        name: "a".into(),
        rename: (choice & 2 == 2).then(|| "c".into()),
        directory: choice & 4 == 4,
    });
    let options = CopyOptions {
        ownership: Ownership::Keep,
        owner: None,
        limits: Limits {
            min_free: Some(0),
            ..Limits::default()
        },
        top,
    };
    let dest = fsat::open_dir(&dir.join("dest")).unwrap();
    let _ = unpack_tar(archive, &dest, &options);
    drop(dest);

    assert_alive(&outside);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        2,
        "something was made beside the destination"
    );
    let parent = fsat::open_dir(&std::env::temp_dir()).unwrap();
    fsat::remove_tree_at(&parent, dir.file_name().unwrap()).expect("removing the scratch");
});
