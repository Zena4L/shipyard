//! The store's own images, given to a build (12.5's review): as Docker's
//! builder does, a `FROM`, `COPY --from` or `RUN --mount=from=` naming an
//! image Shipyard has is built from it, never pulled (unless `--pull`).
//! Each image the Dockerfile could name is a BuildKit named context
//! (`context:NAME=oci-layout://store@DIGEST`), over an OCI layout of this
//! build's own (`layout/`) whose blobs are the store's, hard-linked
//! (0444: never copied, never given to the builder's ID). Probed on
//! BuildKit v0.34.1: the frontend asks for a context by the name as Docker
//! shows it, `:latest` left off, so `FROM localbase:1` and `FROM
//! docker.io/library/localbase:1` find the same one.

use std::path::Path;

use shipyard_image::ImageReference;
use shipyard_image::index::ListedImage;

use crate::error::{Context as _, Error, ErrorKind, Result};

/// The most images one build is given (each an argument of `buildctl`'s);
/// past it, a name is pulled as before.
pub(super) const MAX_IMAGES: usize = 256;

/// The layout's name for itself, in `buildctl --oci-layout store=…`.
pub(super) const LAYOUT_NAME: &str = "store";

/// An image of the store a build is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Local {
    /// The names BuildKit's frontend may ask for it by.
    pub(super) keys: Vec<String>,
    /// Its manifest's digest.
    pub(super) manifest: String,
    /// Every blob it is: its manifest, config and layers.
    pub(super) blobs: Vec<String>,
}

/// The images of `images` (the store's) that a build of `dockerfile`, with
/// `build_args`, could name: each whose repository's last part appears in
/// the Dockerfile or in an argument's value (a `FROM ${BASE}` is named by
/// an `ARG` or a `--build-arg`), by every name it has; at most
/// [`MAX_IMAGES`]. A name the Dockerfile doesn't use costs nothing:
/// BuildKit reads a context only when it's asked for.
pub(super) fn wanted(
    images: &[ListedImage],
    dockerfile: &[u8],
    build_args: &[(String, String)],
) -> Vec<Local> {
    let text = String::from_utf8_lossy(dockerfile);
    let mentioned = |part: &str| {
        !part.is_empty()
            && (text.contains(part) || build_args.iter().any(|(_, value)| value.contains(part)))
    };
    let mut found: Vec<Local> = Vec::new();
    for image in images {
        let Ok(name) = ImageReference::parse(&image.reference) else {
            continue;
        };
        let last = name.repository.rsplit('/').next().unwrap_or_default();
        if !mentioned(last) {
            continue;
        }
        let record = &image.record;
        let mut keys = vec![key(&name)];
        for digest in std::iter::once(&record.manifest).chain(&record.index_digest) {
            keys.push(format!("{}@{digest}", name.familiar_name()));
        }
        if let Some(had) = found.iter_mut().find(|had| had.manifest == record.manifest) {
            for key in keys {
                if !had.keys.contains(&key) {
                    had.keys.push(key);
                }
            }
            continue;
        }
        if found.len() == MAX_IMAGES {
            continue;
        }
        let mut blobs = vec![record.manifest.clone(), record.config.clone()];
        blobs.extend(record.layers.iter().cloned());
        found.push(Local {
            keys,
            manifest: record.manifest.clone(),
            blobs,
        });
    }
    found
}

/// The name BuildKit's frontend asks for `name` by: as Docker shows it
/// (`alpine:3.20`, `ghcr.io/owner/app:1`), `:latest` left off.
fn key(name: &ImageReference) -> String {
    let shown = name.familiar();
    shown
        .strip_suffix(":latest")
        .map_or_else(|| shown.clone(), str::to_owned)
}

/// Lays `images` out in `layout` (an empty directory of the caller's) as
/// an OCI layout: `oci-layout`, `index.json`, each blob hard-linked from
/// the store at `blobs` (its `blobs/sha256`), copied where a link can't
/// cross filesystems. The caller holds the store's lock (shared), so no
/// collection runs meanwhile; an image a blob of which is gone (removed
/// before the lock) is left out. What was laid out, and the files written
/// (the caller gives them to the builder; the links stay the store's).
pub(super) fn lay_out(
    images: &[Local],
    blobs: &Path,
    layout: &Path,
) -> Result<(Vec<Local>, Vec<std::path::PathBuf>)> {
    use std::os::unix::fs::DirBuilderExt as _;
    let into = layout.join("blobs").join("sha256");
    let mut made = Vec::new();
    for dir in [layout.join("blobs"), into.clone()] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("making {}", dir.display()))?;
        made.push(dir);
    }
    let mut laid = Vec::new();
    let mut manifests = Vec::new();
    for image in images {
        let mut whole = true;
        for digest in &image.blobs {
            let Some(hex) = digest.strip_prefix("sha256:") else {
                whole = false;
                break;
            };
            match link_or_copy(&blobs.join(hex), &into.join(hex)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    whole = false;
                    break;
                }
                Err(error) => {
                    return Err(Error::wrap(ErrorKind::Storage, error))
                        .with_context(|| format!("giving the build the blob {digest}"));
                }
            }
        }
        if !whole {
            continue;
        }
        let hex = image.manifest.trim_start_matches("sha256:");
        let bytes =
            shipyard_sys::fs::read_bounded(&into.join(hex), shipyard_image::MANIFEST_SIZE_LIMIT)
                .map_err(|error| Error::wrap(ErrorKind::Storage, error))
                .with_context(|| format!("reading the manifest {}", image.manifest))?;
        manifests.push(serde_json::json!({
            "mediaType": media_type(&bytes),
            "digest": image.manifest,
            "size": bytes.len(),
        }));
        laid.push(image.clone());
    }
    let oci_layout = layout.join("oci-layout");
    write_file(&oci_layout, br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    let index = layout.join("index.json");
    let text = serde_json::json!({ "schemaVersion": 2, "manifests": manifests }).to_string();
    write_file(&index, text.as_bytes())?;
    made.push(oci_layout);
    made.push(index);
    Ok((laid, made))
}

/// The media type a manifest's bytes say it is (the OCI one if they say
/// none).
fn media_type(manifest: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(manifest)
        .ok()
        .and_then(|value| value.get("mediaType")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_owned())
}

/// `from` hard-linked at `to` (one there already, another image's same
/// blob, is it); copied, read-only, where they're on two filesystems.
fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::copy(from, to)?;
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o444))
        }
        other => other,
    }
}

/// Writes `bytes` to a new file at `path`, read-only.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .with_context(|| format!("writing {}", path.display()))
}

/// `buildctl`'s arguments for the images `laid` out in the layout at
/// `layout` (the builder's path): the layout once, then a context a name.
pub(super) fn buildctl_args(laid: &[Local], layout: &str) -> Vec<String> {
    if laid.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["--oci-layout".to_owned(), format!("{LAYOUT_NAME}={layout}")];
    for image in laid {
        for key in &image.keys {
            args.push("--opt".to_owned());
            args.push(format!(
                "context:{key}=oci-layout://{LAYOUT_NAME}@{}",
                image.manifest
            ));
        }
    }
    args
}

#[cfg(test)]
mod tests;
