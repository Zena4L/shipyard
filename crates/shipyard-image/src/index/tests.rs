//! The index's tests: recording, pins, removal, garbage collection and
//! disk usage.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};

use super::gc::layer_unneeded;
use super::*;
use crate::store::{LayerOwners, StoreLock};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("shipyard-index-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        std::fs::create_dir_all(dir.join("layers")).unwrap();
        // Private, as a store's root must be (for its lock).
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        Self(dir)
    }

    /// Writes a fake blob with the given hex name and byte length.
    fn blob(&self, hex: &str, len: usize) {
        std::fs::write(self.0.join("blobs/sha256").join(hex), vec![0u8; len]).unwrap();
    }

    /// Creates a fake extracted-layer directory.
    fn layer_dir(&self, hex: &str) {
        std::fs::create_dir_all(self.0.join("layers").join(hex)).unwrap();
    }

    /// The store's lock, exclusive, as a collection holds it.
    fn exclusive(&self) -> StoreLock {
        crate::Store::open(&self.0)
            .unwrap()
            .lock(LockMode::Exclusive, std::time::Duration::ZERO)
            .unwrap()
    }

    /// Removes layer directories by name, as the CLI's remover does.
    fn remover(
        &self,
    ) -> impl FnMut(&[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> + use<> {
        let layers = self.0.join("layers");
        move |names: &[String]| {
            for name in names {
                std::fs::remove_dir_all(layers.join(name))?;
            }
            Ok(())
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A record from short fake hex digests.
fn record(manifest: &str, config: &str, layers: &[&str]) -> ImageRecord {
    let d = |hex: &str| format!("sha256:{}", hex.repeat(64 / hex.len().max(1)));
    ImageRecord {
        manifest: d(manifest),
        config: d(config),
        layers: layers.iter().map(|h| d(h)).collect(),
    }
}

fn hex(short: &str) -> String {
    short.repeat(64 / short.len().max(1))
}

/// A layer's directory, under any of its names (its digest's, a
/// caller's, a mapped one's, on given lowers), is kept while an image
/// lists the layer or a container stacks it, and only then.
#[test]
fn every_name_of_a_layer_is_kept_while_it_is_needed() {
    let digest = crate::Digest::parse(&format!("sha256:{}", hex("c"))).unwrap();
    let referenced: BTreeSet<String> = [hex("c")].into();
    let none = BTreeSet::new();
    let lowers = [hex("a")];
    for name in [
        LayerOwners::Image.name(&digest),
        LayerOwners::Caller.name(&digest),
        LayerOwners::Mapped(1).name(&digest),
        LayerOwners::Image.name_on(&digest, &lowers),
        LayerOwners::Mapped(1).name_on(&digest, &lowers),
    ] {
        assert!(!layer_unneeded(&name, &none, &referenced), "{name}");
        assert!(layer_unneeded(&name, &none, &none), "{name}");
        let in_use: BTreeSet<String> = [name.clone()].into();
        assert!(!layer_unneeded(&name, &in_use, &none), "{name}");
    }
}

/// A container's pin keeps its image's blobs and layers through a
/// collection, but isn't listed; who pins an image is found; once
/// forgotten, the next collection takes what only it kept.
#[test]
fn a_pin_keeps_an_image_unlisted_until_forgotten() {
    let scratch = Scratch::new();
    scratch.blob(&hex("a"), 10);
    scratch.blob(&hex("b"), 10);
    scratch.blob(&hex("c"), 10);
    scratch.layer_dir(&hex("c"));
    let id = "1".repeat(64);
    Index::pin(&scratch.0, "store", &id, record("a", "b", &["c"])).unwrap();
    let pin = format!("{PIN_PREFIX}store:{id}");
    let mut index = Index::open(&scratch.0).unwrap();
    assert!(index.images().is_empty(), "pins aren't listed");
    assert_eq!(
        index.pinned_by(&format!("sha256:{}", hex("a"))),
        ["1".repeat(64)]
    );
    assert_eq!(index.pins(), [("store".to_owned(), "1".repeat(64))]);
    let removed = index
        .prune(
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    assert!(removed.is_empty(), "kept: {removed:?}");
    assert!(scratch.0.join("layers").join(hex("c")).is_dir());
    // Not a reference: never found, nor removed, by one (`rmi`).
    assert!(index.get(&pin).is_none());
    assert!(matches!(
        index.remove(
            &pin,
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover()
        ),
        Err(IndexError::NoSuchImage(_))
    ));
    assert!(!Index::unpin(&scratch.0, "another store", &id).unwrap());
    assert!(Index::unpin(&scratch.0, "store", &id).unwrap());
    assert!(
        !Index::unpin(&scratch.0, "store", &id).unwrap(),
        "already gone"
    );
    let mut index = Index::open(&scratch.0).unwrap();
    let removed = index
        .prune(
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    assert_eq!(removed.len(), 3, "{removed:?}");
    assert!(!scratch.0.join("layers").join(hex("c")).exists());
}

#[test]
fn images_are_listed_after_being_put() {
    let scratch = Scratch::new();
    Index::record(&scratch.0, "alpine:3.20", record("a", "b", &["c"])).unwrap();
    let images = Index::open(&scratch.0).unwrap().images();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].reference, "alpine:3.20");
}

#[test]
fn removing_an_image_gcs_only_its_unshared_blobs() {
    let scratch = Scratch::new();
    // Two images sharing layer "c"; image1 also has unique "a1"/"b1".
    scratch.blob(&hex("a1"), 10);
    scratch.blob(&hex("b1"), 10);
    scratch.blob(&hex("a2"), 10);
    scratch.blob(&hex("b2"), 10);
    scratch.blob(&hex("c"), 100);
    scratch.layer_dir(&hex("c"));

    Index::record(&scratch.0, "img1", record("a1", "b1", &["c"])).unwrap();
    Index::record(&scratch.0, "img2", record("a2", "b2", &["c"])).unwrap();
    let mut index = Index::open(&scratch.0).unwrap();

    let removed = index
        .remove(
            "img1",
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    // img1's manifest and config go; the shared layer "c" stays.
    assert!(removed.iter().any(|d| d.contains(&hex("a1"))));
    assert!(removed.iter().any(|d| d.contains(&hex("b1"))));
    assert!(
        !removed.iter().any(|d| d.contains(&hex("c"))),
        "shared layer kept"
    );
    assert!(scratch.0.join("blobs/sha256").join(hex("c")).exists());
    assert!(scratch.0.join("layers").join(hex("c")).exists());
    // img2's blobs survive.
    assert!(scratch.0.join("blobs/sha256").join(hex("a2")).exists());
}

#[test]
fn removing_the_last_image_gcs_the_layer_dir_too() {
    let scratch = Scratch::new();
    scratch.blob(&hex("c"), 100);
    scratch.layer_dir(&hex("c"));
    Index::record(&scratch.0, "only", record("a", "b", &["c"])).unwrap();
    let mut index = Index::open(&scratch.0).unwrap();
    index
        .remove(
            "only",
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    assert!(
        !scratch.0.join("layers").join(hex("c")).exists(),
        "layer dir gone"
    );
}

#[test]
fn garbage_collection_leaves_what_isnt_the_stores() {
    let scratch = Scratch::new();
    scratch.layer_dir("lost+found");
    scratch.layer_dir(".tmp-in-progress");
    scratch.layer_dir(&format!("{}-caller", hex("d")));
    scratch.layer_dir(".trash-left-1-0");
    let mut index = Index::open(&scratch.0).unwrap();
    index
        .prune(
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    let layers = scratch.0.join("layers");
    assert!(layers.join("lost+found").exists(), "not the store's");
    assert!(
        !layers.join(".tmp-in-progress").exists(),
        "under the exclusive lock, no extraction is in progress"
    );
    assert!(!layers.join(format!("{}-caller", hex("d"))).exists());
    assert!(!layers.join(".trash-left-1-0").exists());
}

#[test]
fn removing_an_unknown_image_is_an_error() {
    let scratch = Scratch::new();
    let mut index = Index::open(&scratch.0).unwrap();
    assert!(matches!(
        index.remove(
            "ghost",
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover()
        ),
        Err(IndexError::NoSuchImage(_))
    ));
}

#[test]
fn prune_reports_reclaimable_and_removes_orphans() {
    let scratch = Scratch::new();
    scratch.blob(&hex("a"), 10); // referenced
    scratch.blob(&hex("f"), 50); // orphan
    Index::record(&scratch.0, "img", record("a", "a", &["a"])).unwrap();
    let mut index = Index::open(&scratch.0).unwrap();

    let usage = index.disk_usage(&BTreeSet::new()).unwrap();
    assert_eq!(usage.blob_count, 2);
    assert_eq!(usage.blob_reclaimable, 50);

    index
        .prune(
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    assert!(
        !scratch.0.join("blobs/sha256").join(hex("f")).exists(),
        "orphan pruned"
    );
    assert!(
        scratch.0.join("blobs/sha256").join(hex("a")).exists(),
        "referenced kept"
    );
}

#[test]
fn the_index_survives_reopening() {
    let scratch = Scratch::new();
    Index::record(&scratch.0, "img", record("a", "b", &["c"])).unwrap();
    let index = Index::open(&scratch.0).unwrap();
    assert_eq!(index.images().len(), 1);
}

#[test]
fn records_made_at_once_are_all_kept() {
    let scratch = Scratch::new();
    std::thread::scope(|threads| {
        for n in 0..8 {
            let root = &scratch.0;
            threads.spawn(move || {
                Index::record(root, &format!("img{n}"), record("a", "b", &["c"])).unwrap();
            });
        }
    });
    assert_eq!(Index::open(&scratch.0).unwrap().images().len(), 8);
}

#[test]
fn a_corrupt_index_is_moved_aside_and_stops_collection() {
    let scratch = Scratch::new();
    scratch.blob(&hex("a"), 10);
    std::fs::write(scratch.0.join(INDEX_FILE), "{ not json").unwrap();
    let moved = Index::record(&scratch.0, "img", record("b", "b", &["b"])).unwrap();
    assert!(moved.is_some_and(|path| path.exists()));
    let mut index = Index::open(&scratch.0).unwrap();
    assert!(matches!(
        index.prune(
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover()
        ),
        Err(IndexError::CorruptMovedAside(_))
    ));
    assert!(
        scratch.0.join("blobs/sha256").join(hex("a")).exists(),
        "nothing collected while the old index waits"
    );
}

/// What can't be an index Shipyard wrote (larger than its bound, a
/// symlink, not a file) is moved aside as a corrupt one is, so the store
/// mends itself.
#[test]
fn an_index_that_isnt_one_is_moved_aside() {
    let scratch = Scratch::new();
    let path = scratch.0.join(INDEX_FILE);
    let plant: [(&str, &dyn Fn()); 3] = [
        ("larger than its bound", &|| {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(INDEX_MAX + 1).unwrap();
        }),
        ("a symlink", &|| {
            std::fs::write(scratch.0.join("elsewhere"), "{}").unwrap();
            std::os::unix::fs::symlink("elsewhere", &path).unwrap();
        }),
        ("a directory", &|| std::fs::create_dir(&path).unwrap()),
    ];
    for (what, plant) in plant {
        plant();
        assert!(
            matches!(Index::open(&scratch.0), Err(IndexError::Corrupt { .. })),
            "{what}"
        );
        let moved = Index::record(&scratch.0, "img", record("b", "b", &["b"])).unwrap();
        let aside = moved.unwrap();
        assert!(std::fs::symlink_metadata(&aside).is_ok(), "{what}");
        // Its name has the second in it: gone, so the next can have it.
        let _ = std::fs::remove_dir(&aside).or_else(|_| std::fs::remove_file(&aside));
        // The control for the next: a real index reads.
        assert!(
            Index::open(&scratch.0).unwrap().images.contains_key("img"),
            "{what}"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn layers_running_containers_use_are_kept() {
    let scratch = Scratch::new();
    scratch.layer_dir(&hex("c"));
    scratch.layer_dir(&format!("{}-caller", hex("d")));
    let mut index = Index::open(&scratch.0).unwrap();
    let in_use = BTreeSet::from([hex("c"), format!("{}-caller", hex("d"))]);
    index
        .prune(&scratch.exclusive(), &in_use, &mut scratch.remover())
        .unwrap();
    assert!(scratch.0.join("layers").join(hex("c")).exists());
    assert!(
        scratch
            .0
            .join("layers")
            .join(format!("{}-caller", hex("d")))
            .exists()
    );
}

#[test]
fn temporaries_go_and_are_not_reported() {
    let scratch = Scratch::new();
    let temporary = format!(".{}.1.0.tmp", hex("e"));
    scratch.blob(&temporary, 5);
    let partial = format!(".{}.partial", hex("f"));
    scratch.blob(&partial, 5);
    let mut index = Index::open(&scratch.0).unwrap();
    let removed = index
        .prune(
            &scratch.exclusive(),
            &BTreeSet::new(),
            &mut scratch.remover(),
        )
        .unwrap();
    assert!(removed.is_empty(), "{removed:?}");
    assert!(!scratch.0.join("blobs/sha256").join(temporary).exists());
    assert!(!scratch.0.join("blobs/sha256").join(partial).exists());
}
