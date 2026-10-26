//! Push tests against the in-memory registry of `shipyard-test-support`
//! (`push_registry`): what's sent, in what order, with which credentials,
//! and what the registry ends up holding; each misbehaviour the 12.4
//! review named.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use shipyard_image::push::{self, PushEvent, ToPush};
use shipyard_image::{
    Credential, Credentials, Digest, ImageManifest, ImageReference, Platform, PullOptions,
    PushError, Registry, RegistryError, Store,
};
use shipyard_test_support::push_registry::{Auth, Misbehave, PushRegistry};

/// A throwaway directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-push-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn store(&self, name: &str) -> Store {
        Store::open(self.0.join(name)).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A gzipped tar of one file.
fn layer(name: &str, content: &str) -> (Vec<u8>, Vec<u8>) {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    builder
        .append_data(&mut header, name, content.as_bytes())
        .unwrap();
    let tar = builder.into_inner().unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&tar).unwrap();
    (gzip.finish().unwrap(), tar)
}

/// An image of `layers` (gzipped, tar) stored in `store`: what to push.
fn stored(store: &Store, layers: &[(Vec<u8>, Vec<u8>)]) -> ToPush {
    let host = Platform::host();
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": host.architecture,
        "os": host.os,
        "rootfs": {"type": "layers",
                   "diff_ids": layers.iter().map(|(_, tar)| Digest::sha256(tar).to_string()).collect::<Vec<_>>()},
        "config": {"Cmd": ["/hello"]},
    }))
    .unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": Digest::sha256(&config).to_string(), "size": config.len()},
        "layers": layers.iter().map(|(gz, _)| serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": Digest::sha256(gz).to_string(), "size": gz.len()})).collect::<Vec<_>>(),
    }))
    .unwrap();
    for blob in layers.iter().map(|(gz, _)| gz).chain([&config, &manifest]) {
        store
            .write_blob(&Digest::sha256(blob), &mut &blob[..])
            .unwrap();
    }
    ToPush {
        manifest_digest: Digest::sha256(&manifest),
        index_digest: None,
        manifest: ImageManifest::parse(&manifest).unwrap(),
    }
}

/// A client of `registry`, signing in as `me:pw` when asked.
fn client(registry: &PushRegistry, password: &str) -> Registry {
    Registry::with_base(&registry.base, &format!("http://{}", registry.base))
        .with_credentials(
            Credentials::given(
                Credential::Basic {
                    username: "me".to_owned(),
                    password: password.to_owned(),
                },
                Some(&registry.base),
            ),
            &registry.base,
        )
        .for_push()
}

fn basic(password: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("me:{password}"))
    )
}

/// Pushes `image` from `store` to `registry` as `app:1`, collecting what
/// it says of each blob.
fn pushed(
    store: &Store,
    image: &ToPush,
    registry: &Registry,
) -> (Result<shipyard_image::Pushed, PushError>, Vec<PushEvent>) {
    let events = std::sync::Mutex::new(Vec::new());
    let opened = push::open(store, image).unwrap();
    let result = push::push(&opened, registry, "app", "1", &|event| {
        events.lock().unwrap().push(event.clone());
    });
    (result, events.into_inner().unwrap())
}

fn three() -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..3)
        .map(|n| layer(&format!("f{n}"), &format!("layer {n}")))
        .collect()
}

/// A pushed image pulls back with the same ID, its layers and config
/// whole; pushed again, every blob is skipped.
#[test]
fn a_pushed_image_pulls_back_the_same() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let image = stored(&store, &three());
    let registry = PushRegistry::start(Auth::None);
    let (result, events) = pushed(&store, &image, &client(&registry, "pw"));
    assert_eq!(result.unwrap().digest, image.manifest_digest);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, PushEvent::Pushed { .. }))
            .count(),
        4
    );
    let back = scratch.store("back");
    let pulled = shipyard_image::pull::pull_with(
        &ImageReference::parse(&format!("{}/app:1", registry.base)).unwrap(),
        &Platform::host(),
        &back,
        &Registry::with_base(&registry.base, &format!("http://{}", registry.base)),
        PullOptions::default(),
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(pulled.manifest_digest, image.manifest_digest);
    let (_, again) = pushed(&store, &image, &client(&registry, "pw"));
    assert!(
        again.iter().all(|e| matches!(e, PushEvent::Exists { .. })),
        "{again:?}"
    );
}

/// Behind a token service: every write carries a push token (one scope for
/// all three workers, never traded for a pull one); a wrong password is
/// refused by the service; an account that may not push is denied, not
/// told to sign in again.
#[test]
fn a_push_signs_in_for_push_and_says_denied_from_refused() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let image = stored(&store, &three());
    let registry = PushRegistry::start(Auth::Bearer {
        credential: Some(basic("pw")),
        push_denied: false,
    });
    pushed(&store, &image, &client(&registry, "pw")).0.unwrap();
    let writes: Vec<_> = registry
        .state()
        .log
        .iter()
        .filter(|seen| matches!(seen.method.as_str(), "POST" | "PATCH" | "PUT"))
        .filter(|seen| seen.authorization.as_deref() != Some("Bearer tok-push"))
        .cloned()
        .collect();
    assert!(
        writes.iter().all(|seen| seen.authorization.is_none()),
        "{writes:?}"
    );
    let refused = pushed(&store, &image, &client(&registry, "wrong"))
        .0
        .unwrap_err();
    assert!(
        matches!(refused, PushError::Registry(RegistryError::Refused { .. })),
        "{refused}"
    );
    let denied_registry = PushRegistry::start(Auth::Bearer {
        credential: Some(basic("pw")),
        push_denied: true,
    });
    let denied = pushed(&store, &image, &client(&denied_registry, "pw"))
        .0
        .unwrap_err();
    assert!(
        matches!(denied, PushError::Registry(RegistryError::Denied { .. })),
        "{denied}"
    );
}

/// An upload's `Location` on another host: refused, and nothing is sent
/// there (the control: the same registry without it takes the push).
#[test]
fn an_upload_led_elsewhere_is_never_sent_there() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let image = stored(&store, &three());
    let elsewhere = PushRegistry::start(Auth::None);
    let registry = PushRegistry::start(Auth::Basic(basic("pw")));
    registry.state().misbehave.location_elsewhere = Some(elsewhere.base.clone());
    let err = pushed(&store, &image, &client(&registry, "pw"))
        .0
        .unwrap_err();
    assert!(
        matches!(err, PushError::Registry(RegistryError::Body { .. })),
        "{err}"
    );
    assert!(
        elsewhere.state().log.is_empty(),
        "{:?}",
        elsewhere.state().log
    );
    registry.state().misbehave.location_elsewhere = None;
    pushed(&store, &image, &client(&registry, "pw")).0.unwrap();
}

/// A redirected `POST` is refused (never taken for a start), and no body
/// follows it.
#[test]
fn a_redirected_upload_is_refused() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let image = stored(&store, &three());
    let registry = PushRegistry::start(Auth::None);
    registry.state().misbehave.redirect_post = true;
    let err = pushed(&store, &image, &client(&registry, "pw"))
        .0
        .unwrap_err();
    assert!(
        matches!(err, PushError::Registry(RegistryError::Body { .. })),
        "{err}"
    );
    let state = registry.state();
    assert!(
        state.log.iter().all(|seen| seen.method != "PATCH"),
        "{:?}",
        state.log
    );
    assert!(state.manifests.is_empty());
}

/// What goes wrong mid-upload is got over: a `PATCH` that fails (500), a
/// token that expires at a `PATCH` (401: signed in again, the upload
/// started over, the blob whole), a commit whose answer is lost (the blob
/// isn't sent again). Each upload step uses the `Location` the one before
/// gave (the registry refuses a stale state).
#[test]
fn an_upload_gets_over_what_goes_wrong_midway() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let image = stored(&store, &[layer("only", "one layer")]);
    let registry = PushRegistry::start(Auth::Bearer {
        credential: Some(basic("pw")),
        push_denied: false,
    });
    registry.state().misbehave = Misbehave {
        fail_patches: 1,
        patch_unauthorized_once: true,
        lose_commits: 1,
        ..Misbehave::default()
    };
    let (result, _) = pushed(&store, &image, &client(&registry, "pw"));
    result.unwrap();
    let state = registry.state();
    assert_eq!(state.blobs.len(), 2, "the layer and the config");
    for (digest, bytes) in &state.blobs {
        assert_eq!(Digest::sha256(bytes).to_string(), *digest);
    }
    // Each blob committed once: the one whose answer was lost was found by
    // the retry's `HEAD`, never uploaded again.
    for digest in state.blobs.keys() {
        let commits = state
            .log
            .iter()
            .filter(|seen| seen.method == "PUT" && seen.path.ends_with(&format!("digest={digest}")))
            .count();
        assert_eq!(commits, 1, "{digest}: {:?}", state.log);
    }
    assert_eq!(
        state.misbehave.lose_commits, 0,
        "a commit's answer was lost"
    );
    assert!(!state.misbehave.patch_unauthorized_once && state.misbehave.fail_patches == 0);
}

/// A layer listed twice is sent once.
#[test]
fn a_shared_layer_is_pushed_once() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let same = layer("same", "twice");
    let image = stored(&store, &[same.clone(), same]);
    let registry = PushRegistry::start(Auth::None);
    pushed(&store, &image, &client(&registry, "pw")).0.unwrap();
    let starts = registry
        .state()
        .log
        .iter()
        .filter(|seen| seen.method == "POST")
        .count();
    assert_eq!(starts, 2, "the layer once, and the config");
}

/// Pushed back to a tag that names the index it was pulled through, an
/// image is refused (it would make that multi-platform image its own
/// platform's alone); the control: another tag takes it.
#[test]
fn a_push_never_flattens_the_index_it_came_from() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let mut image = stored(&store, &three());
    let index = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
    image.index_digest = Some(Digest::sha256(index));
    let registry = PushRegistry::start(Auth::None);
    registry.state().manifests.insert(
        ("app".to_owned(), "1".to_owned()),
        (
            "application/vnd.oci.image.index.v1+json".to_owned(),
            index.to_vec(),
        ),
    );
    let err = pushed(&store, &image, &client(&registry, "pw"))
        .0
        .unwrap_err();
    assert!(matches!(err, PushError::WouldFlatten { .. }), "{err}");
    assert!(registry.state().blobs.is_empty(), "nothing sent");
    let opened = push::open(&store, &image).unwrap();
    push::push(&opened, &client(&registry, "pw"), "app", "2", &|_| {}).unwrap();
}

/// A token that expires while a blob's bytes go (the commit answered 401):
/// signed in again, the commit alone sent again, the bytes once; an
/// upload the registry lost (a 404 at its `PATCH`) is started over.
#[test]
fn an_expired_token_at_the_commit_resends_only_the_commit() {
    let scratch = Scratch::new();
    let store = scratch.store("from");
    let image = stored(&store, &[layer("only", "one layer")]);
    let registry = PushRegistry::start(Auth::Bearer {
        credential: Some(basic("pw")),
        push_denied: false,
    });
    registry.state().misbehave = Misbehave {
        commit_unauthorized_once: true,
        ..Misbehave::default()
    };
    pushed(&store, &image, &client(&registry, "pw")).0.unwrap();
    let patches = registry
        .state()
        .log
        .iter()
        .filter(|seen| seen.method == "PATCH")
        .count();
    assert_eq!(patches, 2, "each blob's bytes once (the layer, the config)");
    let lost = PushRegistry::start(Auth::None);
    lost.state().misbehave.lose_upload_once = true;
    pushed(&store, &image, &client(&lost, "pw")).0.unwrap();
    assert_eq!(lost.state().blobs.len(), 2, "both blobs there");
}
