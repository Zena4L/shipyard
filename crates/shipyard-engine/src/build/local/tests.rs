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
                platform: None,
            },
            Local {
                keys: vec![
                    "ghcr.io/owner/app:1".to_owned(),
                    format!("ghcr.io/owner/app@{}", digest('b')),
                    format!("ghcr.io/owner/app@{}", digest('i')),
                ],
                manifest: digest('b'),
                blobs: vec![digest('b'), digest('c'), digest('1'), digest('2')],
                platform: None,
            },
        ]
    );
    assert!(wanted(&images, b"FROM busybox\n", &[]).is_empty());
}

/// A store image is never given by a name one of the Dockerfile's stages
/// has (BuildKit would build it in the stage's place): `base:latest` isn't
/// `base` where a stage is `AS Base`, but stays `base:1` and by digest; one
/// whose every name is a stage's isn't given at all.
#[test]
fn a_stage_s_name_is_never_a_store_image_s() {
    let images = [
        image("docker.io/library/base:latest", 'a', None),
        image("docker.io/library/base:1", 'a', None),
        image("docker.io/library/deps:latest", 'd', None),
    ];
    let dockerfile =
        b"FROM busybox AS Base\nfrom --platform=$BUILDPLATFORM alpine as deps\nFROM base\n";
    let found = wanted(&images, dockerfile, &[]);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(
        found[0].keys,
        [format!("base@{}", digest('a')), "base:1".to_owned()]
    );
    assert_eq!(found[1].keys, [format!("deps@{}", digest('d'))]);
    // The control: without the stages, `base` and `deps` are given.
    let plain = wanted(&images, b"FROM base\nFROM deps\n", &[]);
    assert_eq!(plain[0].keys[0], "base");
    assert_eq!(plain[1].keys[0], "deps");
}

/// A stage named on the line after its `FROM` (an instruction continued
/// with the escape character, `\\` or the directive's) is a stage.
#[test]
fn a_continued_from_s_stage_is_found() {
    assert_eq!(stage_names("FROM busybox \\\n  AS base\n"), ["base"]);
    assert_eq!(
        stage_names("# escape=`\nFROM busybox `\n  AS Deps\nRUN echo \\\n"),
        ["deps"]
    );
    assert!(
        stage_names("FROM busybox\nAS base\n").is_empty(),
        "no escape, no stage"
    );
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
        platform: None,
    };
    let missing = Local {
        keys: vec!["gone".to_owned()],
        manifest: digest('a'),
        blobs: vec![digest('a'), digest('9')],
        platform: None,
    };
    // One whose blob isn't a digest (the store's index read as any input).
    let bad = Local {
        keys: vec!["bad".to_owned()],
        manifest: digest('a'),
        blobs: vec!["sha256:../../etc/passwd".to_owned()],
        platform: None,
    };
    let laid = lay_out(&[whole.clone(), missing, bad], &blobs, &layout).unwrap();
    assert_eq!(laid.images, [whole]);
    // The blob gone meanwhile is nothing to say; the bad digest is said.
    assert_eq!(laid.left_out.len(), 1, "{:?}", laid.left_out);
    assert!(
        laid.left_out[0].contains("not a digest"),
        "{:?}",
        laid.left_out
    );
    let written = laid.written;
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
    let mut laid = [Local {
        keys: vec!["alpine".to_owned(), "alpine:3.20".to_owned()],
        manifest: digest('a'),
        blobs: Vec::new(),
        platform: None,
    }];
    // Its platform not read: given to no stage, so no layout either.
    assert!(buildctl_args(&laid, "/layout").is_empty());
    laid[0].platform = Some("linux/arm64".to_owned());
    assert_eq!(
        buildctl_args(&laid, "/layout"),
        [
            "--oci-layout".to_owned(),
            "store=/layout".to_owned(),
            "--opt".to_owned(),
            format!(
                "context:alpine::linux/arm64=oci-layout://store@{}",
                digest('a')
            ),
            "--opt".to_owned(),
            format!(
                "context:alpine:3.20::linux/arm64=oci-layout://store@{}",
                digest('a')
            ),
        ]
    );
}

/// A platform as BuildKit names a stage's: its architecture's baseline
/// variant left off, another kept.
#[test]
fn a_platform_is_named_as_buildkit_names_it() {
    use shipyard_image::Platform;
    let with = |architecture: &str, variant: Option<&str>| Platform {
        variant: variant.map(str::to_owned),
        ..Platform::new("linux", architecture)
    };
    for (platform, named) in [
        (with("arm64", Some("v8")), "linux/arm64"),
        (with("arm64", None), "linux/arm64"),
        (with("amd64", None), "linux/amd64"),
        (with("amd64", Some("v1")), "linux/amd64"),
        (with("amd64", Some("v3")), "linux/amd64/v3"),
        (with("arm", Some("v7")), "linux/arm/v7"),
    ] {
        assert_eq!(buildkit_platform(&platform), named, "{platform}");
    }
}
