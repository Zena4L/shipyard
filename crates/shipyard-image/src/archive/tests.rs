#![allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::load;
use super::save::{self, ToSave};
use super::staging::{self, STAGING_LOCK, STAGING_NEW, STAGING_NEW_STALE, STAGING_PREFIX};
use super::*;
use crate::digest::Digest;
use crate::manifest::{ImageManifest, Platform};
use crate::reference::ImageReference;
use crate::store::Store;

/// A throwaway directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-archive-{name}-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> Store {
        Store::open(self.0.join("store")).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tar of one file `name` holding `content`.
fn tar_of(name: &str, content: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    builder.append_data(&mut header, name, content).unwrap();
    builder.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// An image's blobs: its gzipped layer, config and manifest (for `platform`).
struct Image {
    layer: Vec<u8>,
    config: Vec<u8>,
    manifest: Vec<u8>,
}

impl Image {
    fn new(content: &str, platform: &Platform) -> Self {
        let tar = tar_of("hello", content.as_bytes());
        let layer = gzip(&tar);
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": platform.architecture,
            "os": platform.os,
            "rootfs": {"type": "layers", "diff_ids": [Digest::sha256(&tar).to_string()]},
            "config": {"Cmd": ["/hello"]},
            "history": [{"created_by": "ADD hello"}],
        }))
        .unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": Digest::sha256(&config).to_string(), "size": config.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": Digest::sha256(&layer).to_string(), "size": layer.len()}],
        }))
        .unwrap();
        Self {
            layer,
            config,
            manifest,
        }
    }

    fn manifest_digest(&self) -> Digest {
        Digest::sha256(&self.manifest)
    }

    /// Stored in `store`, to save.
    fn stored(&self, store: &Store, names: &[&str]) -> ToSave {
        for blob in [&self.layer, &self.config, &self.manifest] {
            store
                .write_blob(&Digest::sha256(blob), &mut &blob[..])
                .unwrap();
        }
        ToSave {
            names: names
                .iter()
                .map(|name| ImageReference::parse(name).unwrap())
                .collect(),
            manifest_digest: self.manifest_digest(),
            index_digest: None,
            manifest: ImageManifest::parse(&self.manifest).unwrap(),
        }
    }
}

/// `images` saved from `store`.
fn saved(store: &Store, images: &[ToSave]) -> Vec<u8> {
    let mut out = Vec::new();
    save::open(store, images).unwrap().write(&mut out).unwrap();
    out
}

/// An archive built from `members`: (path, bytes) files, in order.
fn archive_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, path, *bytes).unwrap();
    }
    builder.into_inner().unwrap()
}

fn host() -> Platform {
    Platform::host()
}

/// What's in the store's `blobs/` besides `sha256` (a staging directory
/// left behind would show here).
fn left_in_blobs(store: &Store) -> Vec<String> {
    std::fs::read_dir(store.root().join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "sha256")
        .collect()
}

/// A saved archive read back keeps every image's ID (its manifest's
/// digest), its names, and its blobs, each once; gzipped too. The staging
/// directory goes with the read.
#[test]
fn a_saved_archive_loads_back_as_it_was() {
    let scratch = Scratch::new("round-trip");
    let store = scratch.store();
    let one = Image::new("one", &host());
    let two = Image::new("two", &host());
    let saving = [
        one.stored(&store, &["alpine:3.20", "registry.example/app:1"]),
        two.stored(&store, &["busybox:latest"]),
    ];
    let archive = saved(&store, &saving);
    assert_eq!(archive, saved(&store, &saving), "the same every time");

    let into = Scratch::new("round-trip-into");
    let target = into.store();
    for input in [archive.clone(), gzip(&archive)] {
        let mut read = load::read(&input[..], &target).unwrap();
        let images = read.images(&host()).unwrap();
        assert!(images.refused.is_empty(), "{:?}", images.refused);
        let ids: Vec<Digest> = images
            .found
            .iter()
            .map(|f| f.manifest_digest.clone())
            .collect();
        assert_eq!(ids, [one.manifest_digest(), two.manifest_digest()]);
        let names: Vec<String> = images.found[0]
            .names
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            ["docker.io/library/alpine:3.20", "registry.example/app:1"]
        );
        for found in &images.found {
            let blobs = read.blobs_of(found).unwrap();
            assert_eq!(blobs.len(), 3, "{blobs:?}");
            read.promote(&target, &blobs).unwrap();
            assert!(blobs.iter().all(|blob| target.contains(blob)));
        }
        drop(read);
        assert!(
            left_in_blobs(&target).is_empty(),
            "{:?}",
            left_in_blobs(&target)
        );
    }
}

/// An OCI layout alone (BuildKit's `type=oci`): names from containerd's
/// annotation; a `ref.name` that's a tag alone isn't a name, so an image
/// with only that is left out, said; the archive's others load.
#[test]
fn an_oci_layout_is_named_by_its_annotations() {
    let scratch = Scratch::new("oci");
    let store = scratch.store();
    let named = Image::new("named", &host());
    let bare = Image::new("bare", &host());
    let descriptor = |image: &Image, annotations: serde_json::Value| {
        serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": image.manifest_digest().to_string(),
            "size": image.manifest.len(),
            "annotations": annotations,
        })
    };
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": [
            descriptor(&named, serde_json::json!({"io.containerd.image.name": "docker.io/me/app:1"})),
            descriptor(&bare, serde_json::json!({"org.opencontainers.image.ref.name": "latest"})),
        ],
    }))
    .unwrap();
    let mut members: Vec<(String, Vec<u8>)> = vec![
        ("oci-layout".into(), OCI_LAYOUT.to_vec()),
        ("index.json".into(), index),
    ];
    for image in [&named, &bare] {
        for blob in [&image.layer, &image.config, &image.manifest] {
            members.push((
                format!("blobs/sha256/{}", Digest::sha256(blob).hex()),
                blob.clone(),
            ));
        }
    }
    let members: Vec<(&str, &[u8])> = members
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let mut read = load::read(&archive_of(&members)[..], &store).unwrap();
    let images = read.images(&host()).unwrap();
    assert_eq!(images.found.len(), 1);
    assert_eq!(images.found[0].names[0].to_string(), "docker.io/me/app:1");
    assert_eq!(images.refused.len(), 1, "{:?}", images.refused);
    assert!(
        matches!(images.refused[0], load::LeftOut::Unnamed(_)),
        "{:?}",
        images.refused
    );
}

/// Docker's legacy archive: a `<dir>/layer.tar`, a duplicate layer as a
/// link to it (as Docker writes one), the config `<hex>.json`: a manifest
/// is made for it, its layer the uncompressed tar as it is.
#[test]
fn a_legacy_archive_gets_a_manifest_of_its_own() {
    let scratch = Scratch::new("legacy");
    let store = scratch.store();
    let tar = tar_of("hello", b"legacy");
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": host().architecture, "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": [Digest::sha256(&tar).to_string(), Digest::sha256(&tar).to_string()]},
    }))
    .unwrap();
    let config_name = format!("{}.json", Digest::sha256(&config).hex());
    let manifest_json = serde_json::to_vec(&serde_json::json!([{
        "Config": config_name, "RepoTags": ["legacy:1"], "Layers": ["aaa/layer.tar", "bbb/layer.tar"],
    }]))
    .unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in [
        ("aaa/layer.tar", &tar[..]),
        (config_name.as_str(), &config[..]),
        ("manifest.json", &manifest_json[..]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, path, bytes).unwrap();
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    builder
        .append_link(&mut link, "bbb/layer.tar", "../aaa/layer.tar")
        .unwrap();
    let archive = builder.into_inner().unwrap();
    let mut read = load::read(&archive[..], &store).unwrap();
    let images = read.images(&host()).unwrap();
    assert_eq!(images.found.len(), 1, "{:?}", images.refused);
    let found = &images.found[0];
    assert_eq!(found.names[0].to_string(), "docker.io/library/legacy:1");
    let blobs = read.blobs_of(found).unwrap();
    // The manifest made, the config, the layer twice (one blob).
    assert!(blobs.contains(&Digest::sha256(&tar)), "{blobs:?}");
    read.promote(&store, &blobs).unwrap();
    let manifest =
        ImageManifest::parse(&store.read_blob(&found.manifest_digest, 1 << 20).unwrap()).unwrap();
    assert_eq!(manifest.layers.len(), 2);
    assert_eq!(manifest.layers[0].digest, Digest::sha256(&tar));
    assert_eq!(
        manifest.layers[0].media_type,
        "application/vnd.oci.image.layer.v1.tar"
    );
}

/// A hostile archive's members are names, never paths: a symlink or a
/// hard link out of it, and files named outside, write nothing outside
/// the store; a path that climbs is refused (the tar reader's rule). The
/// control: the same archive without them loads.
#[test]
fn an_archive_is_read_never_extracted() {
    let scratch = Scratch::new("hostile");
    let store = scratch.store();
    let image = Image::new("x", &host());
    let saving = [image.stored(&store, &["ok:1"])];
    let good = saved(&store, &saving);
    // Members prepended: a link out, a file named like a host path.
    let mut builder = tar::Builder::new(Vec::new());
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    builder
        .append_link(&mut link, "escape", &scratch.0)
        .unwrap();
    let mut file = tar::Header::new_gnu();
    file.set_size(4);
    file.set_mode(0o644);
    builder
        .append_data(&mut file, "escape/planted", &b"evil"[..])
        .unwrap();
    let mut evil = builder.into_inner().unwrap();
    // Without its end marker (two zero blocks), then the good archive.
    evil.truncate(evil.len() - 1024);
    evil.extend_from_slice(&good);
    let mut read = load::read(&evil[..], &store).unwrap();
    assert_eq!(read.images(&host()).unwrap().found.len(), 1);
    assert!(
        !scratch.0.join("planted").exists(),
        "written through the link"
    );

    // `../outside`, written raw: the tar crate refuses to make one.
    let mut header = tar::Header::new_old();
    header.as_old_mut().name[..10].copy_from_slice(b"../outside");
    header.set_size(1);
    header.set_mode(0o644);
    header.set_cksum();
    let mut climbing = header.as_bytes().to_vec();
    climbing.extend_from_slice(b"x");
    climbing.resize(1024, 0);
    climbing.extend_from_slice(&[0; 1024]);
    climbing.truncate(climbing.len() - 1024);
    climbing.extend_from_slice(&good);
    assert!(matches!(
        load::read(&climbing[..], &store),
        Err(ArchiveError::Tar(_))
    ));
    assert!(!scratch.0.join("outside").exists());
}

/// A blob whose bytes aren't its name's digest is refused, nothing staged
/// kept; a document named twice, a cut archive, an empty stream and a
/// bzip2 one are refused too.
#[test]
fn what_isnt_sound_is_refused() {
    let scratch = Scratch::new("refused");
    let store = scratch.store();
    let image = Image::new("x", &host());
    let wrong = format!("blobs/sha256/{}", Digest::sha256(b"other").hex());
    assert!(matches!(
        load::read(&archive_of(&[(&wrong, b"not other")])[..], &store),
        Err(ArchiveError::Malformed(_))
    ));
    assert!(
        left_in_blobs(&store).is_empty(),
        "{:?}",
        left_in_blobs(&store)
    );
    let twice = archive_of(&[("manifest.json", b"[]"), ("manifest.json", b"[]")]);
    assert!(matches!(
        load::read(&twice[..], &store),
        Err(ArchiveError::Malformed(_))
    ));
    let saving = [image.stored(&store, &["ok:1"])];
    let good = saved(&store, &saving);
    assert!(load::read(&good[..], &store).is_ok(), "the control");
    let cut = &good[..good.len() - 1024];
    assert!(matches!(load::read(cut, &store), Err(ArchiveError::Cut)));
    // Cut anywhere: inside a member, inside a header, a gzip stream cut;
    // each a cut archive, none taken for another fault.
    for at in [good.len() / 2, 700, 1] {
        let result = load::read(&good[..at], &store);
        assert!(
            matches!(result, Err(ArchiveError::Cut)),
            "at {at}: {result:?}"
        );
        let zipped = gzip(&good);
        let result = load::read(&zipped[..zipped.len() * at / good.len() + 10], &store);
        assert!(
            matches!(result, Err(ArchiveError::Cut)),
            "gzip at {at}: {result:?}"
        );
    }
    assert!(
        left_in_blobs(&store).is_empty(),
        "{:?}",
        left_in_blobs(&store)
    );
    assert!(matches!(
        load::read(&b""[..], &store),
        Err(ArchiveError::Cut)
    ));
    assert!(matches!(
        load::read(&b"BZh91AY&SY"[..], &store),
        Err(ArchiveError::Compression("bzip2"))
    ));
    // A gzip stream that isn't one, its input not run out: the input's
    // fault, never the store's (`Io`, `Store`), whether the tar reader
    // (`Tar`) or a member's read (`Read`) meets it: a header that isn't
    // gzip's, then a stream corrupted inside a member's bytes.
    let mut garbled = vec![0x1f, 0x8b];
    garbled.extend(std::iter::repeat_n(0xa5, 8192));
    let big = (0..256 * 1024u32)
        .map(|n| n.to_le_bytes()[0] ^ n.to_le_bytes()[1])
        .collect::<Vec<u8>>();
    let mut corrupt = gzip(&archive_of(&[("manifest.json", &big)]));
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0xff;
    corrupt[middle + 1] ^= 0xff;
    for input in [garbled, corrupt] {
        let result = load::read(&input[..], &store);
        assert!(
            matches!(result, Err(ArchiveError::Tar(_) | ArchiveError::Read(_))),
            "{result:?}"
        );
    }
}

/// A name with a digest loads only if the digest is the image's own (a
/// hostile archive can't make `alpine@sha256:<official>` name its image);
/// an image for another platform is left out, said; an archive with no
/// images at all is refused.
#[test]
fn names_and_platforms_are_checked() {
    let scratch = Scratch::new("names");
    let store = scratch.store();
    let image = Image::new("x", &host());
    let own = format!("me/app@{}", image.manifest_digest());
    let other = format!("me/app@{}", Digest::sha256(b"official"));
    let foreign = Image::new("y", &Platform::new("linux", "s390x"));
    let saving = [
        image.stored(&store, &[&own]),
        foreign.stored(&store, &["foreign:1"]),
    ];
    // `save` keeps a digest name out of manifest.json: read by index here.
    let archive = saved(&store, &saving);
    let mut read = load::read(&archive[..], &store).unwrap();
    let images = read.images(&host()).unwrap();
    assert_eq!(images.found.len(), 1, "{:?}", images.refused);
    assert!(
        images
            .refused
            .iter()
            .any(|why| matches!(why, load::LeftOut::OtherPlatform { theirs, .. } if theirs.contains("s390x"))),
        "{:?}",
        images.refused
    );

    let index = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "manifests": [{
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "digest": image.manifest_digest().to_string(), "size": image.manifest.len(),
        "annotations": {"io.containerd.image.name": other},
    }]}))
    .unwrap();
    let mut members: Vec<(String, Vec<u8>)> = vec![("index.json".into(), index)];
    for blob in [&image.layer, &image.config, &image.manifest] {
        members.push((
            format!("blobs/sha256/{}", Digest::sha256(blob).hex()),
            blob.clone(),
        ));
    }
    let members: Vec<(&str, &[u8])> = members
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let mut read = load::read(&archive_of(&members)[..], &store).unwrap();
    let images = read.images(&host()).unwrap();
    assert!(images.found.is_empty());
    assert!(
        matches!(images.refused[0], load::LeftOut::ForeignDigest(_)),
        "{:?}",
        images.refused
    );

    let mut read = load::read(&archive_of(&[("oci-layout", OCI_LAYOUT)])[..], &store).unwrap();
    assert!(matches!(
        read.images(&host()),
        Err(ArchiveError::Malformed(_))
    ));
}

/// A staging directory a killed load left is swept once its lock is free;
/// a live load's isn't, nor one still being made (`….new`: its lock not
/// held yet), unless it's old (a load killed making it).
#[test]
fn a_killed_load_s_staging_is_swept() {
    let scratch = Scratch::new("sweep");
    let store = scratch.store();
    let blobs = store.root().join("blobs");
    let left = blobs.join(format!("{STAGING_PREFIX}999999-0"));
    std::fs::create_dir(&left).unwrap();
    std::fs::write(left.join(STAGING_LOCK), b"").unwrap();
    std::fs::write(left.join("half"), b"x").unwrap();
    let image = Image::new("x", &host());
    let saving = [image.stored(&store, &["ok:1"])];
    let archive = saved(&store, &saving);
    let making = blobs.join(format!("{STAGING_PREFIX}999999-1{STAGING_NEW}"));
    std::fs::create_dir(&making).unwrap();
    let old = blobs.join(format!("{STAGING_PREFIX}999999-2{STAGING_NEW}"));
    std::fs::create_dir(&old).unwrap();
    let long_ago = std::time::SystemTime::now() - STAGING_NEW_STALE * 2;
    std::fs::File::open(&old)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let live = load::read(&archive[..], &store).unwrap();
    staging::sweep_staging(&blobs);
    assert!(!left.exists(), "the left one swept");
    assert!(making.exists(), "the one being made kept");
    assert!(!old.exists(), "the one a load was killed making swept");
    std::fs::remove_dir(&making).unwrap();
    assert_eq!(left_in_blobs(&store).len(), 1, "the live one kept");
    drop(live);
    assert!(left_in_blobs(&store).is_empty());
}

/// The archive's bounds on what it holds, each just past (the control:
/// just within reads): documents held past `MAX_DOCUMENTS`, more than
/// `MAX_NAMES` names.
#[test]
fn an_archive_past_its_bounds_is_refused() {
    let scratch = Scratch::new("bounds");
    let store = scratch.store();
    // Documents: an `index.json` past the bound on its own.
    let big = vec![b' '; usize::try_from(load::MAX_DOCUMENTS).unwrap() + 1];
    assert!(matches!(
        load::read(&archive_of(&[("index.json", &big)])[..], &store),
        Err(ArchiveError::TooLarge(_))
    ));
    let within = vec![b' '; 1024];
    assert!(load::read(&archive_of(&[("index.json", &within)])[..], &store).is_ok());

    // Names: one image given one more than the bound.
    let image = Image::new("x", &host());
    let tags = |count: usize| -> Vec<String> { (0..count).map(|n| format!("n:{n}")).collect() };
    let named = |count: usize| {
        let saving = [image.stored(&store, &["ok:1"])];
        let mut members: Vec<(String, Vec<u8>)> = Vec::new();
        let whole = saved(&store, &saving);
        let mut read = tar::Archive::new(&whole[..]);
        for entry in read.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            if path == "manifest.json" {
                let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                manifest[0]["RepoTags"] = serde_json::json!(tags(count));
                bytes = serde_json::to_vec(&manifest).unwrap();
            }
            if !path.ends_with('/') {
                members.push((path, bytes));
            }
        }
        let members: Vec<(&str, &[u8])> = members
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
            .collect();
        archive_of(&members)
    };
    // The index's own name (`ok:1`) is one of them.
    let mut read = load::read(&named(load::MAX_NAMES - 1)[..], &store).unwrap();
    assert_eq!(
        read.images(&host()).unwrap().found[0].names.len(),
        load::MAX_NAMES
    );
    drop(read);
    let mut read = load::read(&named(load::MAX_NAMES)[..], &store).unwrap();
    assert!(matches!(
        read.images(&host()),
        Err(ArchiveError::TooLarge(_))
    ));
    // Shadowed, not dropped: dropped here, their staging with them.
    drop(read);
}

/// The bounds on links and entries, each just past (the control: at the
/// bound, read without it): a legacy link loop (past `MAX_HOPS`) and a link
/// out of the archive, `MAX_ENTRIES`, `MAX_LAYERS`, a `layer.tar` given
/// twice.
#[test]
fn an_archive_past_its_entries_bounds_is_refused() {
    let scratch = Scratch::new("entries");
    let store = scratch.store();
    // Legacy links: a loop, and one out of the archive; the control is
    // a link to a real layer (`a_legacy_archive_gets_a_manifest_of_its_own`).
    let legacy = |links: &[(&str, &str)]| {
        let manifest = serde_json::to_vec(&serde_json::json!([{
            "Config": format!("{}.json", "c".repeat(64)), "RepoTags": ["l:1"],
            "Layers": [links[0].0],
        }]))
        .unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        for (path, target) in links {
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            builder.append_link(&mut link, path, target).unwrap();
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "manifest.json", &manifest[..])
            .unwrap();
        builder.into_inner().unwrap()
    };
    for links in [
        &[
            ("a/layer.tar", "../b/layer.tar"),
            ("b/layer.tar", "../a/layer.tar"),
        ][..],
        &[("a/layer.tar", "../../outside/layer.tar")],
    ] {
        let mut read = load::read(&legacy(links)[..], &store).unwrap();
        assert!(
            matches!(read.images(&host()), Err(ArchiveError::Malformed(_))),
            "{links:?}"
        );
    }
    // Entries, counted before any is read: `index.json`'s manifests,
    // `manifest.json`'s images (the control, at the bound, reads them).
    let entry = serde_json::json!({
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "digest": format!("sha256:{}", "a".repeat(64)), "size": 1,
    });
    for (count, refused) in [(load::MAX_ENTRIES, false), (load::MAX_ENTRIES + 1, true)] {
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "manifests": vec![entry.clone(); count],
        }))
        .unwrap();
        let mut read = load::read(&archive_of(&[("index.json", &index)])[..], &store).unwrap();
        let result = read.images(&host());
        assert_eq!(
            matches!(result, Err(ArchiveError::TooLarge(_))),
            refused,
            "{count}: {result:?}"
        );
        let images = vec![serde_json::json!({"Config": "c.json", "Layers": []}); count];
        let manifest_json = serde_json::to_vec(&images).unwrap();
        let mut read = load::read(
            &archive_of(&[("manifest.json", &manifest_json)])[..],
            &store,
        )
        .unwrap();
        let result = read.images(&host());
        assert_eq!(
            matches!(result, Err(ArchiveError::TooLarge(_))),
            refused,
            "{count}: {result:?}"
        );
    }
    // A `layer.tar` twice.
    let twice = archive_of(&[("a/layer.tar", b"x"), ("a/layer.tar", b"y")]);
    assert!(matches!(
        load::read(&twice[..], &store),
        Err(ArchiveError::Malformed(_))
    ));
    assert!(
        left_in_blobs(&store).is_empty(),
        "{:?}",
        left_in_blobs(&store)
    );
}

/// A legacy layer given as a hard link (Docker's legacy save writes a
/// duplicate so, the target named from the archive's root) is the layer it
/// names.
#[test]
fn a_legacy_hard_link_is_its_target() {
    let scratch = Scratch::new("hardlink");
    let store = scratch.store();
    let tar = tar_of("hello", b"legacy");
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": host().architecture, "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": [Digest::sha256(&tar).to_string(), Digest::sha256(&tar).to_string()]},
    }))
    .unwrap();
    let config_name = format!("{}.json", Digest::sha256(&config).hex());
    let manifest_json = serde_json::to_vec(&serde_json::json!([{
        "Config": config_name, "RepoTags": ["legacy:1"], "Layers": ["aaa/layer.tar", "bbb/layer.tar"],
    }]))
    .unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(tar.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, "aaa/layer.tar", &tar[..])
        .unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    builder
        .append_link(&mut link, "bbb/layer.tar", "aaa/layer.tar")
        .unwrap();
    for (path, bytes) in [
        (config_name.as_str(), &config[..]),
        ("manifest.json", &manifest_json[..]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, path, bytes).unwrap();
    }
    let archive = builder.into_inner().unwrap();
    let mut read = load::read(&archive[..], &store).unwrap();
    let images = read.images(&host()).unwrap();
    assert_eq!(images.found.len(), 1, "{:?}", images.refused);
    let blobs = read.blobs_of(&images.found[0]).unwrap();
    read.promote(&store, &blobs).unwrap();
    let manifest = ImageManifest::parse(
        &store
            .read_blob(&images.found[0].manifest_digest, 1 << 20)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.layers.len(), 2);
    assert_eq!(manifest.layers[1].digest, Digest::sha256(&tar));
}

/// A manifest of more layers than an image may have is refused as it's
/// parsed, before its layers are looked at (the control: `Image::new`'s, of
/// one layer, loads in the tests above).
#[test]
fn a_manifest_past_the_layers_bound_is_refused() {
    let scratch = Scratch::new("layers");
    let store = scratch.store();
    let layer = serde_json::json!({
        "mediaType": "application/vnd.oci.image.layer.v1.tar",
        "digest": format!("sha256:{}", "b".repeat(64)), "size": 1,
    });
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": format!("sha256:{}", "c".repeat(64)), "size": 1},
        "layers": vec![layer; crate::MAX_LAYERS + 1],
    }))
    .unwrap();
    let digest = Digest::sha256(&manifest);
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                       "digest": digest.to_string(), "size": manifest.len(),
                       "annotations": {CONTAINERD_NAME: "docker.io/library/many:1"}}],
    }))
    .unwrap();
    let blob = format!("blobs/sha256/{}", digest.hex());
    let mut read = load::read(
        &archive_of(&[(blob.as_str(), &manifest), ("index.json", &index)])[..],
        &store,
    )
    .unwrap();
    assert!(matches!(
        read.images(&host()),
        Err(ArchiveError::TooLarge(_))
    ));
    drop(read);
    assert!(
        left_in_blobs(&store).is_empty(),
        "{:?}",
        left_in_blobs(&store)
    );
}

/// A blob two images in one save share goes in once (here the same image
/// given twice, every blob shared): each member's name appears once.
#[test]
fn a_shared_blob_is_saved_once() {
    let scratch = Scratch::new("once");
    let store = scratch.store();
    let image = Image::new("x", &host());
    let saving = [
        image.stored(&store, &["a:1"]),
        image.stored(&store, &["b:1"]),
    ];
    let archive = saved(&store, &saving);
    let mut names: Vec<String> = tar::Archive::new(&archive[..])
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let all = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), all, "{names:?}");
    let blobs = names
        .iter()
        .filter(|name| name.starts_with("blobs/sha256/") && name.len() > 13);
    assert_eq!(blobs.count(), 3, "{names:?}");
}

/// A store that lost one of an image's own blobs can't save it: said as
/// that blob missing (the store's fault), never as a malformed archive;
/// the control, the same image whole, saves.
#[test]
fn a_blob_missing_from_the_store_is_the_store_s() {
    let scratch = Scratch::new("missing");
    let store = scratch.store();
    let image = Image::new("x", &host());
    let saving = [image.stored(&store, &["a:1"])];
    assert!(save::open(&store, &saving).is_ok(), "the control");
    let layer = Digest::sha256(&image.layer);
    std::fs::remove_file(store.root().join("blobs/sha256").join(layer.hex())).unwrap();
    assert!(
        matches!(save::open(&store, &saving), Err(ArchiveError::Missing(digest)) if digest == layer),
        "a missing blob"
    );
}

/// An image pulled through a multi-platform index is saved with that index
/// (this platform's manifest alone in the archive, as Docker 25's), so a
/// name with the index's digest loads back and the image keeps it (12.3:
/// BuildKit's image, pinned by its index's digest, loaded offline).
#[test]
fn an_image_saved_with_its_index_loads_by_its_digest() {
    let scratch = Scratch::new("index");
    let store = scratch.store();
    let image = Image::new("x", &host());
    let other = format!("sha256:{}", "e".repeat(64));
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {"mediaType": "application/vnd.oci.image.manifest.v1+json",
             "digest": image.manifest_digest().to_string(), "size": image.manifest.len(),
             "platform": {"architecture": host().architecture, "os": "linux"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json",
             "digest": other, "size": 1,
             "platform": {"architecture": "s390x", "os": "linux"}},
        ],
    }))
    .unwrap();
    let index_digest = Digest::sha256(&index);
    store.write_blob(&index_digest, &mut &index[..]).unwrap();
    let pinned = format!("tool:1@{index_digest}");
    let mut saving = image.stored(&store, &[pinned.as_str()]);
    saving.index_digest = Some(index_digest.clone());
    let archive = saved(&store, &[saving]);

    let into = Scratch::new("index-into");
    let target = into.store();
    let mut read = load::read(&archive[..], &target).unwrap();
    let images = read.images(&host()).unwrap();
    assert!(images.refused.is_empty(), "{:?}", images.refused);
    assert_eq!(images.found.len(), 1);
    let found = &images.found[0];
    assert_eq!(found.manifest_digest, image.manifest_digest());
    assert_eq!(found.index_digest.as_ref(), Some(&index_digest));
    assert_eq!(found.names[0].digest.as_ref(), Some(&index_digest));
}
