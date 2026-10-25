use super::*;
use shipyard_image::index::ImageRecord;

/// A digest of `byte` repeated.
fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn image(reference: &str, manifest: char, index: Option<char>) -> ListedImage {
    ListedImage {
        reference: reference.to_owned(),
        record: ImageRecord {
            manifest: digest(manifest),
            config: digest('c'),
            layers: vec![digest('1'), digest('2')],
            index_digest: index.map(digest),
            tagged_at: None,
        },
    }
}

/// The images a Dockerfile could name are given by every name BuildKit's
/// frontend asks for (Docker's familiar name, `:latest` left off, and by
/// digest); one named only by a `--build-arg` too; one never mentioned
/// isn't.
#[test]
fn the_images_a_dockerfile_could_name_are_given() {
    let images = [
        image("docker.io/library/alpine:latest", 'a', None),
        image("docker.io/library/alpine:3.20", 'a', None),
        image("ghcr.io/owner/app:1", 'b', Some('i')),
        image("docker.io/library/redis:7", 'r', None),
    ];
    let build_args = [("APP".to_owned(), "ghcr.io/owner/app:1".to_owned())];
    let found = wanted(&images, b"FROM alpine\nARG APP\nFROM ${APP}\n", &build_args);
    assert_eq!(
        found,
        [
            Local {
                keys: vec![
                    "alpine".to_owned(),
                    format!("alpine@{}", digest('a')),
                    "alpine:3.20".to_owned(),
                ],
                manifest: digest('a'),
                blobs: vec![digest('a'), digest('c'), digest('1'), digest('2')],
            },
            Local {
                keys: vec![
                    "ghcr.io/owner/app:1".to_owned(),
                    format!("ghcr.io/owner/app@{}", digest('b')),
                    format!("ghcr.io/owner/app@{}", digest('i')),
                ],
                manifest: digest('b'),
                blobs: vec![digest('b'), digest('c'), digest('1'), digest('2')],
            },
        ]
    );
    assert!(wanted(&images, b"FROM busybox\n", &[]).is_empty());
}

/// One build is given at most [`MAX_IMAGES`] images; another name of one
/// it has is still added to it.
#[test]
fn a_build_is_given_a_bounded_number_of_images() {
    let mut images: Vec<ListedImage> = (0..=MAX_IMAGES)
        .map(|n| {
            let mut one = image(&format!("app:{n}"), 'a', None);
            one.record.manifest = format!("sha256:{n:064x}");
            one
        })
        .collect();
    let mut again = image("app:again", 'a', None);
    again.record.manifest = format!("sha256:{:064x}", 0);
    images.push(again);
    let found = wanted(&images, b"FROM app:3\n", &[]);
    assert_eq!(found.len(), MAX_IMAGES);
    assert!(found[0].keys.contains(&"app:again".to_owned()));
}

/// The layout is an OCI layout of the store's own blobs, linked (the same
/// file, never a copy); an image a blob of which is gone is left out.
#[test]
fn the_layout_links_the_store_s_blobs() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = std::env::temp_dir().join(format!("shipyard-layout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let blobs = dir.join("store");
    let layout = dir.join("layout");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::create_dir_all(&layout).unwrap();
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json"}"#;
    for byte in ['a', 'c', '1', '2'] {
        let content: &[u8] = if byte == 'a' { manifest } else { b"blob" };
        std::fs::write(blobs.join(byte.to_string().repeat(64)), content).unwrap();
    }
    let whole = Local {
        keys: vec!["alpine".to_owned()],
        manifest: digest('a'),
        blobs: vec![digest('a'), digest('c'), digest('1'), digest('2')],
    };
    let missing = Local {
        keys: vec!["gone".to_owned()],
        manifest: digest('a'),
        blobs: vec![digest('a'), digest('9')],
    };
    let (laid, written) = lay_out(&[whole.clone(), missing], &blobs, &layout).unwrap();
    assert_eq!(laid, [whole]);
    for byte in ['a', 'c', '1', '2'] {
        let name = byte.to_string().repeat(64);
        let (from, to) = (
            std::fs::metadata(blobs.join(&name)).unwrap(),
            std::fs::metadata(layout.join("blobs/sha256").join(&name)).unwrap(),
        );
        assert_eq!((from.dev(), from.ino()), (to.dev(), to.ino()), "{name}");
    }
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    assert_eq!(
        index["manifests"],
        serde_json::json!([{
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "digest": digest('a'),
            "size": manifest.len(),
        }])
    );
    assert_eq!(
        std::fs::read(layout.join("oci-layout")).unwrap(),
        br#"{"imageLayoutVersion":"1.0.0"}"#
    );
    // What the builder is given: the layout's own files, never a store blob.
    let into = layout.join("blobs/sha256");
    assert!(
        written
            .iter()
            .all(|path| path.parent() != Some(into.as_path()))
    );
    assert!(written.contains(&into));
    assert!(written.contains(&layout.join("index.json")));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `buildctl` is given the layout once and a context a name, or nothing.
#[test]
fn each_name_is_a_context_over_the_layout() {
    assert!(buildctl_args(&[], "/layout").is_empty());
    let laid = [Local {
        keys: vec!["alpine".to_owned(), "alpine:3.20".to_owned()],
        manifest: digest('a'),
        blobs: Vec::new(),
    }];
    assert_eq!(
        buildctl_args(&laid, "/layout"),
        [
            "--oci-layout".to_owned(),
            "store=/layout".to_owned(),
            "--opt".to_owned(),
            format!("context:alpine=oci-layout://store@{}", digest('a')),
            "--opt".to_owned(),
            format!("context:alpine:3.20=oci-layout://store@{}", digest('a')),
        ]
    );
}
