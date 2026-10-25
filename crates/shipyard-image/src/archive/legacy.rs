//! A manifest made for an image in Docker's legacy archive (step 12.2a),
//! which carries none: its config and its layers (each `layer.tar`, plain
//! or gzipped, as it is) described as an OCI manifest, staged as a blob of
//! its own. The image's ID is that manifest's digest, so it differs from the
//! ID Docker gave it (its config's digest); its layers' `diff_id`s are still
//! checked against the config as they're unpacked.

use serde_json::json;

use super::ArchiveError;
use super::staging::Staging;
use crate::digest::Digest;
use crate::manifest::media_type;

/// The OCI media type of an uncompressed layer.
const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

/// The OCI media type of a gzipped layer.
const LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// Stages a manifest for the config `config` and the layers `layers`
/// (staged blobs): its digest.
///
/// # Errors
///
/// A staged blob can't be read, or the manifest staged.
pub(crate) fn make_manifest(
    staging: &mut Staging,
    config: &Digest,
    layers: &[Digest],
) -> Result<Digest, ArchiveError> {
    let described = |digest: &Digest, media: &str, staging: &Staging| {
        Ok::<_, ArchiveError>(json!({
            "mediaType": media,
            "digest": digest.to_string(),
            "size": staging.size(digest)?,
        }))
    };
    let mut layer_descriptors = Vec::new();
    for layer in layers {
        let media = if staging.starts_with(layer, &[0x1f, 0x8b])? {
            LAYER_GZIP
        } else {
            LAYER_TAR
        };
        layer_descriptors.push(described(layer, media, staging)?);
    }
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": media_type::OCI_MANIFEST,
        "config": described(config, media_type::OCI_CONFIG, staging)?,
        "layers": layer_descriptors,
    });
    // A `Value` always serializes.
    let bytes = serde_json::to_vec(&manifest).unwrap_or_default();
    staging.stage_bytes(&bytes)
}
