//! Image manifests, configs, and multi-platform indexes.
//!
//! An image is described by JSON documents that point at each other by digest:
//!
//! - an **image manifest** lists the image's config blob and its layer blobs,
//!   each as a [`Descriptor`] (a media type, a digest, and a size);
//! - the **image config** holds the `rootfs` (the ordered `diff_id`s that
//!   identify the uncompressed layers), the platform (`architecture`/`os`),
//!   and the runtime defaults (`Env`, `Cmd`, `Entrypoint`, …);
//! - a **manifest list** (Docker) or **image index** (OCI) lists one manifest
//!   per platform, so a single name like `alpine:3.20` serves `linux/amd64`
//!   and `linux/arm64` from the same tag.
//!
//! Shipyard reads both the [OCI image spec][oci] forms and Docker's
//! [v2 schema 2][docker] forms; they share a shape and differ only in media
//! type, so one set of types parses both.
//!
//! The parsers are strict in the ways that matter for untrusted input: the
//! document is size-bounded by the caller (it comes from the store, which
//! bounds reads), every digest is parsed and validated, and anything
//! malformed is an error that names what was wrong — never a panic, and never
//! a silent default that would hide a corrupt or hostile manifest. Unknown
//! fields are ignored, as both specs require for forward compatibility.
//!
//! [oci]: https://github.com/opencontainers/image-spec/blob/main/manifest.md
//! [docker]: https://distribution.github.io/distribution/spec/manifest-v2-2/

use serde::Deserialize;

use crate::digest::{Digest, DigestError};

/// Media types the parsers recognise, OCI and Docker side by side.
pub mod media_type {
    /// OCI image manifest.
    pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    /// OCI image index (the multi-platform list).
    pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    /// OCI image config.
    pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";

    /// Docker v2 schema 2 image manifest.
    pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
    /// Docker v2 schema 2 manifest list (the multi-platform list).
    pub const DOCKER_MANIFEST_LIST: &str =
        "application/vnd.docker.distribution.manifest.list.v2+json";
    /// Docker image config.
    pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";

    /// Whether `media_type` names a single-platform image manifest (OCI or
    /// Docker).
    #[must_use]
    pub fn is_manifest(media_type: &str) -> bool {
        media_type == OCI_MANIFEST || media_type == DOCKER_MANIFEST
    }

    /// Whether `media_type` names a multi-platform index / manifest list.
    #[must_use]
    pub fn is_index(media_type: &str) -> bool {
        media_type == OCI_INDEX || media_type == DOCKER_MANIFEST_LIST
    }

    /// Whether `media_type` names a runnable image's config (OCI or
    /// Docker): anything else is an artifact (a Helm chart, a signature)
    /// stored in a registry, not an image.
    #[must_use]
    pub fn is_config(media_type: &str) -> bool {
        media_type == OCI_CONFIG || media_type == DOCKER_CONFIG
    }
}

/// A pointer to a blob: its media type, its digest, and its size in bytes.
///
/// This is the OCI "descriptor" (and Docker's identical object). It's how one
/// document refers to another — a manifest to its config and layers, an index
/// to its manifests — so a reader knows what it's fetching, can verify it
/// against the digest, and knows how big it should be before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// The blob's media type.
    pub media_type: String,
    /// The blob's content digest.
    pub digest: Digest,
    /// The blob's size in bytes.
    pub size: u64,
    /// The platform of the referenced manifest, in an index entry only.
    pub platform: Option<Platform>,
}

/// A platform an image or manifest targets: `os`/`architecture`, and an
/// optional `variant` (such as `v8` for `arm64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// The CPU architecture (`amd64`, `arm64`, …), in Go's `GOARCH` spelling.
    pub architecture: String,
    /// The operating system (`linux`, …), in Go's `GOOS` spelling.
    pub os: String,
    /// An architecture variant (`v8`, `v7`, …), if the index gives one.
    pub variant: Option<String>,
}

impl Platform {
    /// A platform from its `os` and `architecture`, no variant.
    #[must_use]
    pub fn new(os: impl Into<String>, architecture: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            architecture: architecture.into(),
            variant: None,
        }
    }

    /// The platform containers run on here: Linux (on macOS too: there,
    /// containers run in a Linux VM), on this machine's architecture in the
    /// names images use (Go's `GOARCH`, and its `GOARM`-style variant).
    #[must_use]
    pub fn host() -> Self {
        let (architecture, variant) = match std::env::consts::ARCH {
            "x86_64" => ("amd64", None),
            "x86" => ("386", None),
            "aarch64" => ("arm64", Some("v8")),
            "arm" => ("arm", Some("v7")),
            "powerpc64" if cfg!(target_endian = "little") => ("ppc64le", None),
            "loongarch64" => ("loong64", None),
            other => (other, None),
        };
        Self {
            variant: variant.map(str::to_owned),
            ..Self::new("linux", architecture)
        }
    }

    /// How well a build for `self` serves `wanted`, higher is better:
    /// `None` if it can't run there. The same OS and architecture, and then:
    /// the exact variant; a build naming none (or its architecture's
    /// baseline, `amd64/v1` or `arm64/v8`), which runs on any; or, on 32-bit arm, an older
    /// variant (a v7 CPU runs v6 code), the newest first. A variant newer
    /// than `wanted`'s (or than none: `amd64/v3` on a CPU of unknown level)
    /// may use instructions the CPU lacks, and is never taken.
    fn fitness(&self, wanted: &Platform) -> Option<(u8, u8)> {
        if self.os != wanted.os || self.architecture != wanted.architecture {
            return None;
        }
        let baseline = |variant: &str| {
            matches!(
                (self.architecture.as_str(), variant),
                ("amd64", "v1") | ("arm64", "v8")
            )
        };
        let have = self.variant.as_deref().filter(|variant| !baseline(variant));
        let want = wanted
            .variant
            .as_deref()
            .filter(|variant| !baseline(variant));
        match (want, have) {
            (_, None) => Some((2, 0)),
            (Some(want), Some(have)) if want == have => Some((3, 0)),
            (Some(want), Some(have)) if self.architecture == "arm" => {
                let level = |variant: &str| variant.strip_prefix('v')?.parse::<u8>().ok();
                let (want, have) = (level(want)?, level(have)?);
                (have < want).then_some((1, have))
            }
            _ => None,
        }
    }

    /// Whether an image built for `self` runs where `wanted` is (see
    /// `fitness`; a config that records no variant runs on
    /// any of its architecture).
    #[must_use]
    pub fn runs_on(&self, wanted: &Platform) -> bool {
        self.fitness(wanted).is_some()
    }
}

impl std::fmt::Display for Platform {
    /// `os/architecture[/variant]`, as Docker writes a platform.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.os, self.architecture)?;
        if let Some(variant) = &self.variant {
            write!(formatter, "/{variant}")?;
        }
        Ok(())
    }
}

/// Why a manifest, config, or index couldn't be parsed.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The bytes weren't valid JSON, or didn't match the expected shape.
    #[error("{context}: {source}")]
    Json {
        /// What was being parsed.
        context: String,
        /// The underlying serde error.
        source: serde_json::Error,
    },
    /// A digest field in the document couldn't be parsed.
    #[error("{context}: {source}")]
    Digest {
        /// Which field held the bad digest.
        context: String,
        /// Why it wasn't a valid digest.
        source: DigestError,
    },
    /// A media type that should name a manifest/index/config named something
    /// else.
    #[error("{0}")]
    Unexpected(String),
    /// No manifest in an index matched the platform asked for.
    #[error("no image for {wanted} in the index (it has: {available})")]
    NoMatchingPlatform {
        /// The platform that was requested.
        wanted: String,
        /// The platforms the index does offer.
        available: String,
    },
}

// --- Wire format (what the JSON actually holds) ---------------------------
//
// These mirror the documents byte for byte, then convert into the validated
// public types above. Keeping them separate means a digest string becomes a
// parsed `Digest` (or a named error) at the boundary, not deep in the code.

/// A descriptor as it appears in JSON.
#[derive(Debug, Deserialize)]
struct RawDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    platform: Option<RawPlatform>,
}

/// A platform object as it appears in JSON.
#[derive(Debug, Deserialize)]
struct RawPlatform {
    architecture: String,
    os: String,
    #[serde(default)]
    variant: Option<String>,
}

impl RawDescriptor {
    /// Validates the raw descriptor into a [`Descriptor`], parsing its digest.
    fn parse(self, context: &str) -> Result<Descriptor, ManifestError> {
        let digest = Digest::parse(&self.digest).map_err(|source| ManifestError::Digest {
            context: format!("the {context} descriptor's digest"),
            source,
        })?;
        Ok(Descriptor {
            media_type: self.media_type,
            digest,
            size: self.size,
            platform: self.platform.map(RawPlatform::into_platform),
        })
    }
}

impl RawPlatform {
    fn into_platform(self) -> Platform {
        Platform {
            architecture: self.architecture,
            os: self.os,
            variant: self.variant.filter(|v| !v.is_empty()),
        }
    }
}

/// A list that may be `null` in the document: Docker writes an unset list
/// (`Entrypoint`, `Cmd`, `Env`, …) that way, as Go marshals a nil slice.
fn null_is_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Parses `bytes` as JSON of type `T`, naming `context` on failure.
fn from_json<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    context: &str,
) -> Result<T, ManifestError> {
    serde_json::from_slice(bytes).map_err(|source| ManifestError::Json {
        context: context.to_owned(),
        source,
    })
}

/// An image manifest: the config blob and the ordered layer blobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageManifest {
    /// The descriptor of the image config blob.
    pub config: Descriptor,
    /// The layer blobs, base layer first.
    pub layers: Vec<Descriptor>,
}

/// An image manifest as it appears in JSON.
#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    config: RawDescriptor,
    #[serde(default, deserialize_with = "null_is_empty")]
    layers: Vec<RawDescriptor>,
}

impl ImageManifest {
    /// Parses an image manifest (OCI or Docker v2 schema 2) from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if the JSON is malformed, a descriptor's
    /// digest is invalid, or an embedded `mediaType` names something that
    /// isn't a manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let raw: RawManifest = from_json(bytes, "the image manifest")?;
        // The mediaType field is optional in the document, but if present it
        // must name a manifest, not an index served under the wrong type.
        if let Some(media_type) = &raw.media_type
            && !media_type::is_manifest(media_type)
        {
            return Err(ManifestError::Unexpected(format!(
                "expected an image manifest, found media type `{media_type}`"
            )));
        }
        let config = raw.config.parse("config")?;
        let layers = raw
            .layers
            .into_iter()
            .enumerate()
            .map(|(index, layer)| layer.parse(&format!("layer {index}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { config, layers })
    }
}

/// An image config: the platform, the ordered layer `diff_id`s, and the
/// runtime defaults the image was built with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageConfig {
    /// The CPU architecture the image targets (`amd64`, `arm64`, …).
    pub architecture: String,
    /// The operating system the image targets (`linux`, …).
    pub os: String,
    /// The uncompressed layer digests, base first; their order and count
    /// match the manifest's layers.
    pub diff_ids: Vec<Digest>,
    /// The default process configuration baked into the image.
    pub config: RuntimeConfig,
}

/// The runtime defaults from an image config's `config` object: what the
/// image runs if a `run` doesn't override it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Environment variables, each `KEY=value`.
    pub env: Vec<String>,
    /// The entrypoint, prepended to the command.
    pub entrypoint: Vec<String>,
    /// The default command (or the arguments to the entrypoint).
    pub cmd: Vec<String>,
    /// The working directory, if the image sets one.
    pub working_dir: Option<String>,
    /// The user (`name`, `uid`, or `uid:gid`), if the image sets one.
    pub user: Option<String>,
}

/// An image config as it appears in JSON.
#[derive(Debug, Deserialize)]
struct RawConfig {
    architecture: String,
    os: String,
    rootfs: RawRootfs,
    #[serde(default)]
    config: Option<RawRuntimeConfig>,
}

/// The `rootfs` object of an image config.
#[derive(Debug, Deserialize)]
struct RawRootfs {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_is_empty")]
    diff_ids: Vec<String>,
}

/// The `config` object of an image config.
#[derive(Debug, Deserialize, Default)]
struct RawRuntimeConfig {
    #[serde(rename = "Env", default, deserialize_with = "null_is_empty")]
    env: Vec<String>,
    #[serde(rename = "Entrypoint", default, deserialize_with = "null_is_empty")]
    entrypoint: Vec<String>,
    #[serde(rename = "Cmd", default, deserialize_with = "null_is_empty")]
    cmd: Vec<String>,
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
    #[serde(rename = "User", default)]
    user: Option<String>,
}

impl ImageConfig {
    /// Parses an image config from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if the JSON is malformed, the `rootfs.type`
    /// isn't `layers`, or a `diff_id` isn't a valid digest.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let raw: RawConfig = from_json(bytes, "the image config")?;
        if raw.rootfs.kind != "layers" {
            return Err(ManifestError::Unexpected(format!(
                "the image config's rootfs.type is `{}`, expected `layers`",
                raw.rootfs.kind
            )));
        }
        let diff_ids = raw
            .rootfs
            .diff_ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                Digest::parse(id).map_err(|source| ManifestError::Digest {
                    context: format!("rootfs.diff_ids[{index}]"),
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let runtime = raw.config.unwrap_or_default();
        Ok(Self {
            architecture: raw.architecture,
            os: raw.os,
            diff_ids,
            config: RuntimeConfig {
                env: runtime.env,
                entrypoint: runtime.entrypoint,
                cmd: runtime.cmd,
                working_dir: runtime.working_dir.filter(|d| !d.is_empty()),
                user: runtime.user.filter(|u| !u.is_empty()),
            },
        })
    }

    /// The platform this config targets.
    #[must_use]
    pub fn platform(&self) -> Platform {
        Platform::new(self.os.clone(), self.architecture.clone())
    }
}

/// A multi-platform index: one manifest descriptor per platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// The per-platform manifest descriptors.
    pub manifests: Vec<Descriptor>,
}

/// An index / manifest list as it appears in JSON.
#[derive(Debug, Deserialize)]
struct RawIndex {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    #[serde(default, deserialize_with = "null_is_empty")]
    manifests: Vec<RawDescriptor>,
}

impl Index {
    /// Parses an image index / manifest list (OCI or Docker) from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if the JSON is malformed, a descriptor's
    /// digest is invalid, or an embedded `mediaType` names something that
    /// isn't an index.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let raw: RawIndex = from_json(bytes, "the image index")?;
        if let Some(media_type) = &raw.media_type
            && !media_type::is_index(media_type)
        {
            return Err(ManifestError::Unexpected(format!(
                "expected an image index, found media type `{media_type}`"
            )));
        }
        let manifests = raw
            .manifests
            .into_iter()
            .enumerate()
            .map(|(index, entry)| entry.parse(&format!("manifest {index}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { manifests })
    }

    /// The descriptor of the manifest for `wanted`, if the index has one.
    ///
    /// Entries that aren't image manifests (nested indexes) or have no
    /// platform are skipped, and attestations fit none (`unknown/unknown`).
    /// The best fit wins (see `Platform::fitness`); among equals, the
    /// first, as the registry orders them by preference.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::NoMatchingPlatform`] naming what the index
    /// does offer if nothing matches.
    pub fn select(&self, wanted: &Platform) -> Result<&Descriptor, ManifestError> {
        self.manifests
            .iter()
            .filter(|descriptor| media_type::is_manifest(&descriptor.media_type))
            .filter_map(|descriptor| {
                let fitness = descriptor.platform.as_ref()?.fitness(wanted)?;
                Some((fitness, descriptor))
            })
            // `max_by_key` keeps the last of equals: reversed, the first.
            .rev()
            .max_by_key(|(fitness, _)| *fitness)
            .map(|(_, descriptor)| descriptor)
            .ok_or_else(|| ManifestError::NoMatchingPlatform {
                wanted: wanted.to_string(),
                available: self.available_platforms(),
            })
    }

    /// A human list of the platforms the index offers, for an error message.
    fn available_platforms(&self) -> String {
        let mut platforms: Vec<String> = self
            .manifests
            .iter()
            .filter_map(|descriptor| descriptor.platform.as_ref())
            .map(Platform::to_string)
            .collect();
        platforms.sort();
        platforms.dedup();
        if platforms.is_empty() {
            "none".to_owned()
        } else {
            platforms.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid sha256 digest string (the empty-input hash), reused in fixtures.
    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    /// A second valid digest (sha256 of "abc").
    const D2: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn an_oci_manifest_parses() {
        let json = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "{manifest}",
                "config": {{ "mediaType": "{config}", "digest": "{D}", "size": 7023 }},
                "layers": [
                    {{ "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": "{D2}", "size": 32654 }}
                ]
            }}"#,
            manifest = media_type::OCI_MANIFEST,
            config = media_type::OCI_CONFIG,
        );
        let manifest = ImageManifest::parse(json.as_bytes()).expect("valid OCI manifest");
        assert_eq!(manifest.config.size, 7023);
        assert_eq!(manifest.config.digest, Digest::parse(D).unwrap());
        assert_eq!(manifest.layers.len(), 1);
        assert_eq!(manifest.layers[0].digest, Digest::parse(D2).unwrap());
    }

    #[test]
    fn a_docker_v2_manifest_parses_the_same() {
        // Same shape, Docker media types: one set of types reads both.
        let json = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "{manifest}",
                "config": {{ "mediaType": "{config}", "digest": "{D}", "size": 1 }},
                "layers": []
            }}"#,
            manifest = media_type::DOCKER_MANIFEST,
            config = media_type::DOCKER_CONFIG,
        );
        let manifest = ImageManifest::parse(json.as_bytes()).expect("valid Docker manifest");
        assert!(manifest.layers.is_empty());
    }

    #[test]
    fn a_manifest_whose_media_type_is_an_index_is_refused() {
        let json = format!(
            r#"{{ "mediaType": "{index}", "config": {{ "mediaType": "x", "digest": "{D}", "size": 1 }} }}"#,
            index = media_type::OCI_INDEX,
        );
        let error = ImageManifest::parse(json.as_bytes()).expect_err("an index is not a manifest");
        assert!(matches!(error, ManifestError::Unexpected(_)), "{error}");
    }

    #[test]
    fn a_bad_digest_in_a_manifest_is_named() {
        let json = r#"{ "config": { "mediaType": "x", "digest": "sha256:short", "size": 1 }, "layers": [] }"#;
        let error = ImageManifest::parse(json.as_bytes()).expect_err("bad digest");
        match error {
            ManifestError::Digest { context, .. } => assert!(context.contains("config")),
            other => panic!("expected a digest error, got {other}"),
        }
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(ImageManifest::parse(b"not json").is_err());
        assert!(ImageManifest::parse(b"").is_err());
        assert!(ImageManifest::parse(b"{").is_err());
        // Missing the required `config`.
        assert!(ImageManifest::parse(br#"{"layers":[]}"#).is_err());
    }

    #[test]
    fn an_image_config_parses_with_its_defaults() {
        let json = format!(
            r#"{{
                "architecture": "arm64",
                "os": "linux",
                "rootfs": {{ "type": "layers", "diff_ids": ["{D}", "{D2}"] }},
                "config": {{
                    "Env": ["PATH=/usr/bin"],
                    "Entrypoint": ["/bin/sh"],
                    "Cmd": ["-c", "echo hi"],
                    "WorkingDir": "/work",
                    "User": "1000:1000"
                }}
            }}"#
        );
        let config = ImageConfig::parse(json.as_bytes()).expect("valid config");
        assert_eq!(config.architecture, "arm64");
        assert_eq!(config.platform(), Platform::new("linux", "arm64"));
        assert_eq!(config.diff_ids.len(), 2);
        assert_eq!(config.config.env, ["PATH=/usr/bin"]);
        assert_eq!(config.config.entrypoint, ["/bin/sh"]);
        assert_eq!(config.config.cmd, ["-c", "echo hi"]);
        assert_eq!(config.config.working_dir.as_deref(), Some("/work"));
        assert_eq!(config.config.user.as_deref(), Some("1000:1000"));
    }

    #[test]
    fn an_image_config_with_no_runtime_section_is_empty_defaults() {
        let json = format!(
            r#"{{ "architecture": "amd64", "os": "linux", "rootfs": {{ "type": "layers", "diff_ids": ["{D}"] }} }}"#
        );
        let config = ImageConfig::parse(json.as_bytes()).expect("minimal config");
        assert_eq!(config.config, RuntimeConfig::default());
        // Empty working_dir / user strings become None.
        assert!(config.config.working_dir.is_none());
    }

    #[test]
    fn a_rootfs_that_isnt_layers_is_refused() {
        let json = r#"{ "architecture": "amd64", "os": "linux", "rootfs": { "type": "something", "diff_ids": [] } }"#;
        let error = ImageConfig::parse(json.as_bytes()).expect_err("rootfs.type must be layers");
        assert!(matches!(error, ManifestError::Unexpected(_)), "{error}");
    }

    #[test]
    fn a_bad_diff_id_is_named() {
        let json = r#"{ "architecture": "amd64", "os": "linux", "rootfs": { "type": "layers", "diff_ids": ["nope"] } }"#;
        let error = ImageConfig::parse(json.as_bytes()).expect_err("bad diff_id");
        match error {
            ManifestError::Digest { context, .. } => assert!(context.contains("diff_ids[0]")),
            other => panic!("expected a digest error, got {other}"),
        }
    }

    /// An index with the two platforms a real multi-arch tag serves.
    fn two_platform_index() -> String {
        format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "{index}",
                "manifests": [
                    {{ "mediaType": "{m}", "digest": "{D}", "size": 1, "platform": {{ "architecture": "amd64", "os": "linux" }} }},
                    {{ "mediaType": "{m}", "digest": "{D2}", "size": 2, "platform": {{ "architecture": "arm64", "os": "linux", "variant": "v8" }} }},
                    {{ "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": "{D}", "size": 3 }}
                ]
            }}"#,
            index = media_type::OCI_INDEX,
            m = media_type::OCI_MANIFEST,
        )
    }

    #[test]
    fn an_index_selects_the_wanted_platform() {
        let index = Index::parse(two_platform_index().as_bytes()).expect("valid index");
        assert_eq!(index.manifests.len(), 3);

        let amd64 = index.select(&Platform::new("linux", "amd64")).unwrap();
        assert_eq!(amd64.digest, Digest::parse(D).unwrap());

        // A plain arm64 request matches the v8 entry (specific is a superset).
        let arm64 = index.select(&Platform::new("linux", "arm64")).unwrap();
        assert_eq!(arm64.digest, Digest::parse(D2).unwrap());
    }

    #[test]
    fn an_index_without_the_platform_names_what_it_has() {
        let index = Index::parse(two_platform_index().as_bytes()).unwrap();
        let error = index
            .select(&Platform::new("windows", "amd64"))
            .expect_err("no windows image");
        match error {
            ManifestError::NoMatchingPlatform { wanted, available } => {
                assert_eq!(wanted, "windows/amd64");
                assert!(available.contains("linux/amd64"), "{available}");
                assert!(available.contains("linux/arm64/v8"), "{available}");
            }
            other => panic!("expected NoMatchingPlatform, got {other}"),
        }
    }

    #[test]
    fn a_specific_variant_request_does_not_match_a_different_variant() {
        assert_eq!(chosen(&["linux/arm64/v9"], "linux/arm64/v8"), None);
        assert_eq!(chosen(&["linux/arm/v7"], "linux/arm/v6"), None);
    }

    #[test]
    fn the_host_platform_uses_image_architecture_names() {
        let host = Platform::host();
        assert_eq!(host.os, "linux", "containers are Linux's, on macOS too");
        let expected = if cfg!(target_arch = "x86_64") {
            "linux/amd64"
        } else if cfg!(target_arch = "aarch64") {
            "linux/arm64/v8"
        } else {
            return;
        };
        assert_eq!(host.to_string(), expected);
    }

    /// An index of image manifests for `platforms` (`os/arch[/variant]`),
    /// each with its own digest: entry `i` has the digest of `i`'s text.
    fn index_of(platforms: &[&str]) -> Index {
        let entries: Vec<String> = platforms
            .iter()
            .enumerate()
            .map(|(i, platform)| {
                let mut parts = platform.split('/');
                let (os, arch) = (parts.next().unwrap(), parts.next().unwrap());
                let variant = parts
                    .next()
                    .map(|v| format!(r#","variant":"{v}""#))
                    .unwrap_or_default();
                format!(
                    r#"{{"mediaType":"{m}","digest":"{d}","size":1,"platform":{{"os":"{os}","architecture":"{arch}"{variant}}}}}"#,
                    m = media_type::OCI_MANIFEST,
                    d = Digest::sha256(i.to_string().as_bytes()),
                )
            })
            .collect();
        let json = format!(r#"{{"manifests":[{}]}}"#, entries.join(","));
        Index::parse(json.as_bytes()).unwrap()
    }

    /// Which entry of `index_of(platforms)` serves `wanted`.
    fn chosen(platforms: &[&str], wanted: &str) -> Option<usize> {
        let mut parts = wanted.split('/');
        let mut platform = Platform::new(parts.next().unwrap(), parts.next().unwrap());
        platform.variant = parts.next().map(str::to_owned);
        let picked = index_of(platforms).select(&platform).ok()?.digest.clone();
        (0..platforms.len()).find(|i| Digest::sha256(i.to_string().as_bytes()) == picked)
    }

    #[test]
    fn the_best_variant_wins_and_a_newer_one_never_does() {
        let arm = [
            "linux/arm/v5",
            "linux/arm/v6",
            "linux/arm",
            "linux/arm/v7",
            "linux/arm/v8",
        ];
        assert_eq!(chosen(&arm, "linux/arm/v7"), Some(3), "the exact variant");
        assert_eq!(
            chosen(&arm[..3], "linux/arm/v7"),
            Some(2),
            "then one naming none"
        );
        assert_eq!(
            chosen(&arm[..2], "linux/arm/v7"),
            Some(1),
            "then the newest older"
        );
        assert_eq!(chosen(&arm[4..], "linux/arm/v7"), None, "never a newer one");
        assert_eq!(chosen(&arm[..2], "linux/arm/v5"), Some(0));

        let amd = ["linux/amd64/v3", "linux/amd64/v1"];
        assert_eq!(
            chosen(&amd[..1], "linux/amd64"),
            None,
            "v3 needs a CPU we can't vouch for"
        );
        assert_eq!(chosen(&amd, "linux/amd64"), Some(1), "v1 is the baseline");
        assert_eq!(chosen(&["linux/arm64/v8"], "linux/arm64"), Some(0));

        let same = ["linux/amd64", "linux/amd64"];
        assert_eq!(chosen(&same, "linux/amd64"), Some(0), "the first of equals");
    }

    #[test]
    fn a_config_runs_where_its_variant_fits() {
        let mut v3 = Platform::new("linux", "amd64");
        v3.variant = Some("v3".to_owned());
        assert!(!v3.runs_on(&Platform::new("linux", "amd64")));
        let mut arm64 = Platform::new("linux", "arm64");
        arm64.variant = Some("v8".to_owned());
        assert!(
            Platform::new("linux", "arm64").runs_on(&arm64),
            "a config naming no variant"
        );
    }

    #[test]
    fn docker_writes_unset_lists_as_null_and_strings_as_empty() {
        // hello-world's config, as Docker Hub serves it (trimmed).
        let hello = format!(
            r#"{{"architecture":"amd64","os":"linux",
                "config":{{"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                  "Cmd":["/hello"],"Entrypoint":null,"WorkingDir":"","User":"",
                  "Labels":null,"OnBuild":null,"Volumes":null}},
                "rootfs":{{"type":"layers","diff_ids":["{D}"]}}}}"#
        );
        let config = ImageConfig::parse(hello.as_bytes()).unwrap();
        assert_eq!(config.config.cmd, ["/hello"]);
        assert!(config.config.entrypoint.is_empty());
        assert_eq!(config.config.working_dir, None);
        assert_eq!(config.config.user, None);
        let bare = r#"{"architecture":"amd64","os":"linux",
            "config":{"Env":null,"Cmd":null,"Entrypoint":null},
            "rootfs":{"type":"layers","diff_ids":null}}"#;
        let config = ImageConfig::parse(bare.as_bytes()).unwrap();
        assert_eq!(config.config, RuntimeConfig::default());
        assert!(config.diff_ids.is_empty());
        let manifest = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"x","digest":"{D}","size":1}},"layers":null}}"#
        );
        assert!(
            ImageManifest::parse(manifest.as_bytes())
                .unwrap()
                .layers
                .is_empty()
        );
        assert!(
            Index::parse(br#"{"manifests":null}"#)
                .unwrap()
                .manifests
                .is_empty()
        );
    }

    #[test]
    fn an_index_whose_media_type_is_a_manifest_is_refused() {
        let json = format!(
            r#"{{ "mediaType": "{m}", "manifests": [] }}"#,
            m = media_type::OCI_MANIFEST,
        );
        let error = Index::parse(json.as_bytes()).expect_err("a manifest is not an index");
        assert!(matches!(error, ManifestError::Unexpected(_)), "{error}");
    }

    // Fuzz target: the parsers take untrusted bytes from a registry, so none
    // of them may panic, hang, or over-allocate on any input. proptest feeds
    // arbitrary bytes and arbitrary JSON-ish strings; the only acceptable
    // outcomes are Ok or a ManifestError.
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn the_parsers_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = ImageManifest::parse(&bytes);
            let _ = ImageConfig::parse(&bytes);
            let _ = Index::parse(&bytes);
        }

        #[test]
        fn the_parsers_never_panic_on_json_like_text(text in "\\PC{0,512}") {
            let bytes = text.as_bytes();
            let _ = ImageManifest::parse(bytes);
            let _ = ImageConfig::parse(bytes);
            let _ = Index::parse(bytes);
        }

        #[test]
        fn an_index_select_never_panics(
            os in "[a-z]{0,8}",
            arch in "[a-z0-9]{0,8}",
        ) {
            if let Ok(index) = Index::parse(two_platform_index().as_bytes()) {
                let _ = index.select(&Platform::new(os, arch));
            }
        }
    }
}
