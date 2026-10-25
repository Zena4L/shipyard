use super::*;
use crate::changes::Changes;

/// A scratch store, removed at the end.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("shipyard-import-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> Store {
        Store::open(self.0.join("store")).unwrap()
    }

    /// What's in `blobs/` beside `sha256` (staging left behind).
    fn left(&self) -> Vec<String> {
        std::fs::read_dir(self.0.join("store/blobs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "sha256")
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tar of `files` (path, contents).
fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, path, *bytes).unwrap();
    }
    builder.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn about(changes: &[&str]) -> About {
    About {
        changes: Changes::parse(&changes.iter().map(|&c| c.to_owned()).collect::<Vec<_>>())
            .unwrap(),
        message: None,
        created: "2026-10-24T00:00:00Z".to_owned(),
        platform: Platform::host(),
    }
}

/// The layer's `diff_id` is the tar's own digest, plain or gzipped in; its
/// blob is gzip's, which decodes to the same tar; the config says so, with
/// the changes and Docker's history; the manifest names both.
#[test]
fn an_imported_tar_is_a_layer_of_its_own_digest() {
    let scratch = Scratch::new("digests");
    let store = scratch.store();
    let tar = tar_of(&[("bin/app", b"#!/bin/sh\n"), ("etc/x", b"y")]);
    for input in [tar.clone(), gzip(&tar)] {
        let imported = read(
            &input[..],
            &store,
            &about(&["CMD [\"/bin/app\"]", "ENV A=1"]),
        )
        .unwrap();
        imported.promote(&store).unwrap();
        let manifest =
            crate::ImageManifest::parse(&store.read_blob(&imported.manifest, 1 << 20).unwrap())
                .unwrap();
        assert_eq!(manifest.layers.len(), 1);
        assert_eq!(
            manifest.layers[0].media_type,
            "application/vnd.oci.image.layer.v1.tar+gzip"
        );
        let blob = store
            .read_blob(&manifest.layers[0].digest, 1 << 20)
            .unwrap();
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&blob[..]), &mut decoded)
            .unwrap();
        assert_eq!(decoded, tar, "the blob decodes to the tar");
        let config: serde_json::Value =
            serde_json::from_slice(&store.read_blob(&manifest.config.digest, 1 << 20).unwrap())
                .unwrap();
        assert_eq!(
            config["rootfs"]["diff_ids"][0],
            Digest::sha256(&tar).to_string()
        );
        assert_eq!(config["config"]["Cmd"], serde_json::json!(["/bin/app"]));
        assert_eq!(config["config"]["Env"], serde_json::json!(["A=1"]));
        assert_eq!(config["architecture"], Platform::host().architecture);
        assert_eq!(config["history"][0]["comment"], "Imported from -");
        crate::ImageConfig::parse(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(!imported.looks_like_an_archive);
        drop(imported);
        assert!(scratch.left().is_empty(), "{:?}", scratch.left());
    }
}

/// What isn't a whole tar is never an image, and leaves nothing: empty,
/// cut at an entry's boundary or inside one, random bytes, a gzip cut;
/// bzip2 refused by name. The control: the whole tar reads (above).
#[test]
fn what_isnt_a_whole_tar_imports_nothing() {
    let scratch = Scratch::new("unsound");
    let store = scratch.store();
    let tar = tar_of(&[("a", &[7; 2000]), ("b", b"x")]);
    let random: Vec<u8> = (0..4096u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let zipped = gzip(&tar);
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("cut at a boundary", tar[..tar.len() - 1024].to_vec()),
        ("cut inside", tar[..1000].to_vec()),
        ("gzip cut", zipped[..zipped.len() / 2].to_vec()),
    ];
    for (what, input) in inputs {
        let result = read(&input[..], &store, &about(&[]));
        assert!(
            matches!(result, Err(ArchiveError::Cut)),
            "{what}: {result:?}"
        );
    }
    let result = read(&random[..], &store, &about(&[]));
    assert!(
        matches!(result, Err(ArchiveError::Tar(_))),
        "random: {result:?}"
    );
    assert!(matches!(
        read(&b"BZh91AY&SY"[..], &store, &about(&[])),
        Err(ArchiveError::Compression("bzip2"))
    ));
    assert!(scratch.left().is_empty(), "{:?}", scratch.left());
    let blobs = std::fs::read_dir(scratch.0.join("store/blobs/sha256"))
        .unwrap()
        .count();
    assert_eq!(blobs, 0, "nothing reached the store");
}

/// A gzip of two members (pigz's, or two streams cat'ed) is read whole:
/// the layer is both halves.
#[test]
fn a_gzip_of_two_members_is_read_whole() {
    let scratch = Scratch::new("members");
    let store = scratch.store();
    let tar = tar_of(&[("a", &[1; 3000]), ("b", b"two")]);
    let (first, second) = tar.split_at(1536);
    let mut input = gzip(first);
    input.extend(gzip(second));
    let imported = read(&input[..], &store, &about(&[])).unwrap();
    imported.promote(&store).unwrap();
    let manifest =
        crate::ImageManifest::parse(&store.read_blob(&imported.manifest, 1 << 20).unwrap())
            .unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&store.read_blob(&manifest.config.digest, 1 << 20).unwrap())
            .unwrap();
    assert_eq!(
        config["rootfs"]["diff_ids"][0],
        Digest::sha256(&tar).to_string()
    );
}

/// An image archive given to `import` by mistake is said (it was meant for
/// `load`); a root filesystem isn't.
#[test]
fn an_image_archive_imported_is_noticed() {
    let scratch = Scratch::new("archive");
    let store = scratch.store();
    let archive = tar_of(&[("oci-layout", b"{}"), ("index.json", b"{}")]);
    assert!(
        read(&archive[..], &store, &about(&[]))
            .unwrap()
            .looks_like_an_archive
    );
    let rootfs = tar_of(&[("etc/os-release", b"x")]);
    assert!(
        !read(&rootfs[..], &store, &about(&[]))
            .unwrap()
            .looks_like_an_archive
    );
}

/// A reader that gives one byte a read (a pipe whose first writes are short).
struct OneByte<'a>(&'a [u8]);

impl std::io::Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some((&first, rest)) = self.0.split_first() else {
            return Ok(0);
        };
        if buf.is_empty() {
            return Ok(0);
        }
        buf[0] = first;
        self.0 = rest;
        Ok(1)
    }
}

/// A gzip whose first read is a byte is still read as gzip: what it is is
/// decided on its first bytes however the stream splits them (Murphy's
/// review of 12.2b).
#[test]
fn a_gzip_read_a_byte_at_a_time_is_gzip() {
    let scratch = Scratch::new("bytes");
    let store = scratch.store();
    let tar = tar_of(&[("a", b"x")]);
    let zipped = gzip(&tar);
    let imported = read(OneByte(&zipped), &store, &about(&[])).unwrap();
    imported.promote(&store).unwrap();
}
