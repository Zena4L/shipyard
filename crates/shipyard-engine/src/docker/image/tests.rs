use super::*;

const MANIFEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const CONFIG: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const LAYER: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
const DIFF: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";

/// An image of two references, its config as `runtime` (JSON) says.
fn read(runtime: &str) -> Read {
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{CONFIG}","size":7}},
        "layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{LAYER}","size":100}},
                  {{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{LAYER}","size":23}}]}}"#
    );
    let config = format!(
        r#"{{"architecture":"arm64","variant":"v8","os":"linux","created":"2026-10-06T04:47:34.795670471Z",
        "rootfs":{{"type":"layers","diff_ids":["{DIFF}"]}},"config":{runtime}}}"#
    );
    let reference = |text: &str| ImageReference::parse(text).unwrap();
    Read {
        references: vec![
            reference("alpine:3.20"),
            reference("docker.io/library/alpine:latest"),
        ],
        index_digests: vec![None, None],
        pulled: Pulled {
            reference: reference("alpine:3.20"),
            platform: Platform::new("linux", "arm64"),
            manifest: shipyard_image::ImageManifest::parse(manifest.as_bytes()).unwrap(),
            config: shipyard_image::ImageConfig::parse(config.as_bytes()).unwrap(),
            manifest_digest: shipyard_image::Digest::parse(MANIFEST).unwrap(),
            index_digest: None,
        },
        tagged: None,
    }
}

/// The keys of `value`'s object, sorted.
fn keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// `GET /images/{name}/json` has Docker 29.8's keys at API 1.43 (its
/// inspect of an image there), its ID the manifest's digest, its names as
/// Docker shows them, its empty parts `null` as Docker's are.
#[test]
fn an_image_is_inspected_as_docker_shows_it() {
    let shown = serde_json::to_value(inspected(&read(r#"{"Cmd":["sh"],"Env":["A=1"]}"#))).unwrap();
    assert_eq!(
        keys(&shown),
        [
            "Architecture",
            "Author",
            "Comment",
            "Config",
            "Container",
            "ContainerConfig",
            "Created",
            "DockerVersion",
            "GraphDriver",
            "Id",
            "Metadata",
            "Os",
            "Parent",
            "RepoDigests",
            "RepoTags",
            "RootFS",
            "Size",
            "Variant",
            "VirtualSize"
        ]
    );
    assert_eq!(
        keys(&shown["Config"]),
        [
            "AttachStderr",
            "AttachStdin",
            "AttachStdout",
            "Cmd",
            "Domainname",
            "Entrypoint",
            "Env",
            "Hostname",
            "Image",
            "Labels",
            "OnBuild",
            "OpenStdin",
            "StdinOnce",
            "Tty",
            "User",
            "Volumes",
            "WorkingDir"
        ]
    );
    assert_eq!(shown["Id"], MANIFEST);
    assert_eq!(
        shown["RepoTags"],
        serde_json::json!(["alpine:3.20", "alpine:latest"])
    );
    assert_eq!(
        shown["RepoDigests"],
        serde_json::json!([format!("alpine@{MANIFEST}")])
    );
    assert_eq!(
        (shown["Size"].clone(), shown["VirtualSize"].clone()),
        (123.into(), 123.into())
    );
    assert_eq!(shown["Created"], "2026-10-06T04:47:34.795670471Z");
    assert_eq!(
        shown["RootFS"],
        serde_json::json!({"Type": "layers", "Layers": [DIFF]})
    );
    assert_eq!(
        shown["GraphDriver"],
        serde_json::json!({"Name": "overlayfs", "Data": null})
    );
    for null in ["Entrypoint", "Labels", "Volumes", "OnBuild"] {
        assert!(shown["Config"][null].is_null(), "{null}");
    }
    assert_eq!(shown["Config"]["Cmd"], serde_json::json!(["sh"]));
    assert_eq!(shown["Metadata"]["LastTagTime"], "0001-01-01T00:00:00Z");
}

/// F118: an image pulled through a multi-platform index is known to its
/// registry by the index's digest (`RepoDigests`; its ID stays its
/// manifest's, D11.2c-2); its last tag's time is shown.
#[test]
fn an_index_digest_and_a_tag_time_are_shown() {
    const INDEX: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let mut read = read(r#"{"Cmd":["sh"]}"#);
    // `alpine:3.20` pulled through an index; `alpine:latest` not (a tag,
    // or one platform's manifest): each name by its own.
    read.index_digests = vec![Some(INDEX.to_owned()), None];
    read.tagged = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000));
    let shown = serde_json::to_value(inspected(&read)).unwrap();
    assert_eq!(shown["Id"], MANIFEST);
    let mut wanted = vec![format!("alpine@{INDEX}"), format!("alpine@{MANIFEST}")];
    wanted.sort();
    assert_eq!(shown["RepoDigests"], serde_json::json!(wanted));
    assert_eq!(shown["Metadata"]["LastTagTime"], "2025-10-09T08:53:20Z");
}

/// What an image sets shows: labels, volumes, ports, health check, stop
/// signal, shell; and the keys Docker leaves out when unset appear.
#[test]
fn what_an_image_sets_is_shown() {
    let shown = serde_json::to_value(inspected(&read(
        r#"{"Labels":{"a":"1"},"Volumes":{"/data":{}},"ExposedPorts":{"80/tcp":{}},
        "StopSignal":"SIGQUIT","Shell":["/bin/bash","-c"],"Entrypoint":["/init"],
        "Healthcheck":{"Test":["CMD","true"],"Interval":30000000000,"Retries":3}}"#,
    )))
    .unwrap();
    let config = &shown["Config"];
    assert_eq!(config["Labels"], serde_json::json!({"a": "1"}));
    assert_eq!(config["Volumes"], serde_json::json!({"/data": {}}));
    assert_eq!(config["ExposedPorts"], serde_json::json!({"80/tcp": {}}));
    assert_eq!(config["StopSignal"], "SIGQUIT");
    assert_eq!(config["Shell"], serde_json::json!(["/bin/bash", "-c"]));
    assert_eq!(config["Entrypoint"], serde_json::json!(["/init"]));
    assert_eq!(config["Healthcheck"]["Interval"], 30_000_000_000_u64);
    assert_eq!(config["Healthcheck"]["Retries"], 3);
}

/// An image's created time in Unix seconds, 0 when it doesn't say.
#[test]
fn an_image_s_created_time_is_in_seconds() {
    let image = read("{}");
    assert_eq!(created_seconds(&image.pulled), 1_791_262_054);
    let mut unsaid = image.pulled;
    unsaid.config.about.created = None;
    assert_eq!(created_seconds(&unsaid), 0);
}
