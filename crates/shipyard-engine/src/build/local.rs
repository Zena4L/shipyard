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
//!
//! Each is given for its own platform alone (`context:NAME::linux/arm64`,
//! 13.8c): BuildKit's frontend asks for a stage's base by the name and the
//! stage's platform first, and takes the plain name for any platform after,
//! so a plain one would put an arm64 image in a `FROM --platform=linux/amd64`
//! stage. Another platform's stage finds none, and its base is pulled, as
//! Docker's builder does.

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
    /// Every blob it is: its manifest, its config (second: what the build
    /// reads its platform from), then its layers.
    pub(super) blobs: Vec<String>,
    /// Its platform as BuildKit's frontend names a stage's
    /// ([`buildkit_platform`]), read from its config: `None` until it's
    /// been read, and then it's given to no stage.
    pub(super) platform: Option<String>,
}

/// `platform` as BuildKit's frontend writes a stage's in a named context's
/// key (containerd's `FormatAll(Normalize(…))`): `os/architecture`, and a
/// variant only past its architecture's baseline (`arm64`'s `v8` and
/// `amd64`'s `v1` are left off).
pub(super) fn buildkit_platform(platform: &shipyard_image::Platform) -> String {
    let baseline = matches!(
        (platform.architecture.as_str(), platform.variant.as_deref()),
        ("arm64", Some("v8")) | ("amd64", Some("v1"))
    );
    match platform.variant.as_deref().filter(|_| !baseline) {
        Some(variant) => format!("{}/{}/{variant}", platform.os, platform.architecture),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}

/// The images of `images` (the store's) that a build of `dockerfile`, with
/// `build_args`, could name: each whose repository's last part appears in
/// the Dockerfile or in an argument's value (a `FROM ${BASE}` is named by
/// an `ARG` or a `--build-arg`), by every name it has; at most
/// [`MAX_IMAGES`]. A name the Dockerfile doesn't use costs nothing:
/// BuildKit reads a context only when it's asked for. Never by a name one
/// of the Dockerfile's own stages has (`FROM … AS base`): BuildKit puts a
/// named context in place of the stage of its name, so a store's `base`
/// would build in place of the Dockerfile's (12.5c3's review); Docker's
/// rule is the stage's.
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
    let stages = stage_names(&text);
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
        keys.retain(|key| !stages.contains(&key.to_ascii_lowercase()));
        if keys.is_empty() {
            continue;
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
            platform: None,
        });
    }
    found
}

/// The names the Dockerfile `text` gives its stages (`FROM IMAGE AS
/// NAME`), lowercase, as BuildKit compares them; an instruction continued
/// over lines (its escape character, `\\` or the `# escape=` directive's,
/// at a line's end) read as one.
fn stage_names(text: &str) -> Vec<String> {
    joined(text)
        .iter()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let first = words.first()?;
            if !first.eq_ignore_ascii_case("FROM") {
                return None;
            }
            let at = words
                .iter()
                .position(|word| word.eq_ignore_ascii_case("AS"))?;
            words.get(at + 1).map(|name| name.to_ascii_lowercase())
        })
        .collect()
}

/// The Dockerfile `text`'s instructions, each a line: a line ending in its
/// escape character joined to the next.
fn joined(text: &str) -> Vec<String> {
    let escape = escape_of(text);
    let mut lines = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        // A comment (a directive among them, `# escape=\``) is a line of
        // its own, never continued.
        if line.trim_start().starts_with('#') {
            if pending.is_empty() {
                lines.push(line.to_owned());
            }
            continue;
        }
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix(escape) {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(line);
        lines.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        lines.push(pending);
    }
    lines
}

/// The escape character the Dockerfile `text` sets (`# escape=` among the
/// parser directives that open it), else `\\`.
fn escape_of(text: &str) -> char {
    for line in text.lines() {
        let Some(directive) = line.trim().strip_prefix('#') else {
            break;
        };
        let Some((key, value)) = directive.split_once('=') else {
            break;
        };
        if key.trim().eq_ignore_ascii_case("escape")
            && let Some(escape) = value.trim().chars().next()
        {
            return escape;
        }
    }
    '\\'
}

/// The name BuildKit's frontend asks for `name` by: as Docker shows it
/// (`alpine:3.20`, `ghcr.io/owner/app:1`), `:latest` left off.
fn key(name: &ImageReference) -> String {
    let shown = name.familiar();
    shown
        .strip_suffix(":latest")
        .map_or_else(|| shown.clone(), str::to_owned)
}

/// What [`lay_out`] laid out.
#[derive(Debug, Default)]
pub(super) struct Laid {
    /// The images given to the build.
    pub(super) images: Vec<Local>,
    /// The layout's own files and directories (the caller gives them to
    /// the builder; the links stay the store's).
    pub(super) written: Vec<std::path::PathBuf>,
    /// Why an image matched was left out (pulled as before), other than
    /// a blob removed meanwhile: said.
    pub(super) left_out: Vec<String>,
}

/// Lays `images` out in `layout` (an empty directory of the caller's) as
/// an OCI layout: `oci-layout`, `index.json`, each blob hard-linked from
/// the store at `blobs` (its `blobs/sha256`). The caller holds the store's
/// lock (shared), so no collection runs meanwhile. An image a blob of which
/// is gone (removed before the lock) is left out; one that can't be linked
/// (another filesystem, no room, a link count's most) too, said: it may be
/// a name the Dockerfile only mentions, and a pull is what a build did
/// before (12.5c3's Murphy review: never a copy of gigabytes under the
/// store's lock).
pub(super) fn lay_out(images: &[Local], blobs: &Path, layout: &Path) -> Result<Laid> {
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
    let mut laid = Laid::default();
    let mut manifests = Vec::new();
    'images: for image in images {
        for digest in &image.blobs {
            // The store's own index, parsed as any digest is before it
            // names a file.
            let Ok(parsed) = shipyard_image::Digest::parse(digest) else {
                laid.left_out
                    .push(format!("{}: not a digest: {digest}", image.manifest));
                continue 'images;
            };
            let hex = parsed.hex();
            match link(&blobs.join(&hex), &into.join(&hex)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue 'images,
                Err(error) => {
                    laid.left_out
                        .push(format!("{} (its blob {digest}: {error})", image.keys[0]));
                    continue 'images;
                }
            }
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
        laid.images.push(image.clone());
    }
    let oci_layout = layout.join("oci-layout");
    write_file(&oci_layout, br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    let index = layout.join("index.json");
    let text = serde_json::json!({ "schemaVersion": 2, "manifests": manifests }).to_string();
    write_file(&index, text.as_bytes())?;
    made.push(oci_layout);
    made.push(index);
    laid.written = made;
    Ok(laid)
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
/// blob, is it).
fn link(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
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
/// `layout` (the builder's path): the layout once, then a context a name,
/// for the image's own platform alone (one whose platform wasn't read is
/// given to no stage).
pub(super) fn buildctl_args(laid: &[Local], layout: &str) -> Vec<String> {
    let mut args = Vec::new();
    for image in laid {
        let Some(platform) = &image.platform else {
            continue;
        };
        for key in &image.keys {
            args.push("--opt".to_owned());
            args.push(format!(
                "context:{key}::{platform}=oci-layout://{LAYOUT_NAME}@{}",
                image.manifest
            ));
        }
    }
    if args.is_empty() {
        return args;
    }
    [
        vec!["--oci-layout".to_owned(), format!("{LAYOUT_NAME}={layout}")],
        args,
    ]
    .concat()
}

#[cfg(test)]
mod tests;
