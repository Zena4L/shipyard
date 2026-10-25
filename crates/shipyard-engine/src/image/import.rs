//! `import` (step 12.2b, D12.2-4, D12.2-6, D12.2-9): a root filesystem's
//! tarball made a one-layer image, named. What the tarball is and how it's
//! read are `shipyard_image::import`'s; here, the name, the store's lock,
//! the extraction and the record, as a load's.

use std::io::Read;

use shipyard_image::changes::Changes;
use shipyard_image::index::Index;
use shipyard_image::{ImageRecord, ImageReference, LockMode, Platform, USE_WAIT};

use crate::Engine;
use crate::error::{Result, bail};
use crate::report::WarningKind;

/// Whether `asked` (`--platform`, the API's `platform`) names this host's
/// platform: its OS alone (`linux`), its architecture alone (`arm64`),
/// `os/arch`, or `os/arch/variant` with the host's variant (any, where
/// the host's architecture has none). One rule for pull, import and build.
#[must_use]
pub fn is_host_platform(asked: &str) -> bool {
    let host = Platform::host();
    let parts: Vec<&str> = asked.split('/').collect();
    let (os, arch, variant) = match parts[..] {
        [one] if one == host.os => (one, None, None),
        [arch] => (host.os.as_str(), Some(arch), None),
        [os, arch] => (os, Some(arch), None),
        [os, arch, variant] => (os, Some(arch), Some(variant)),
        _ => return false,
    };
    os == host.os
        && arch.is_none_or(|arch| arch == host.architecture)
        && variant.is_none_or(|variant| host.variant.as_deref().is_none_or(|host| host == variant))
}

/// This host's platform as Docker names it (`linux/arm64`): what's said
/// when another is refused.
#[must_use]
pub fn host_platform_name() -> String {
    let host = Platform::host();
    format!("{}/{}", host.os, host.architecture)
}

/// What an import is asked to make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportRequest {
    /// Its name (`REPOSITORY[:TAG]`): required (D12.2-9: Shipyard keeps no
    /// untagged image).
    pub name: Option<String>,
    /// Its config's changes (`--change`: `CMD ["sh"]`…).
    pub changes: Vec<String>,
    /// Its history's comment (`--message`).
    pub message: Option<String>,
    /// The platform asked for (`--platform`): this host's only.
    pub platform: Option<String>,
}

/// What an import made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// The image's ID (its manifest's digest).
    pub id: String,
    /// Its name, as Docker shows it (`app:1`).
    pub name: String,
    /// The image the name was another's before, if it was (its ID): the
    /// name moved, as a pull's or a load's does.
    pub moved: Option<String>,
}

/// Imports the tarball `input` as `request` asks: read whole into the
/// store's staging first (its name and changes checked before a byte of it
/// is read), then, under the store's shared lock, its blobs moved in, its
/// layer extracted (Linux) and its name recorded.
///
/// # Errors
///
/// No name, a name with a digest, a change `import` doesn't take
/// ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest)); input
/// that isn't a whole tar (plain or gzip), or past its bounds (the same);
/// the store's.
pub fn import(engine: &Engine, input: impl Read, request: &ImportRequest) -> Result<Imported> {
    let Some(given) = request.name.as_deref().filter(|name| !name.is_empty()) else {
        bail!(
            InvalidRequest,
            "give the image a name: shipyard import FILE NAME[:TAG] (Shipyard keeps no untagged \
             images)"
        );
    };
    let name = ImageReference::parse(given)?;
    if name.digest.is_some() {
        bail!(
            InvalidRequest,
            "an import's name can't have a digest: {given} (name it repository[:tag])"
        );
    }
    let host = Platform::host();
    if let Some(asked) = &request.platform
        && !is_host_platform(asked)
    {
        bail!(
            InvalidRequest,
            "only this host's platform can be imported ({}/{}), not {asked:?}",
            host.os,
            host.architecture
        );
    }
    let about = shipyard_image::import::About {
        changes: Changes::parse(&request.changes)?,
        message: request.message.clone(),
        created: shipyard_runtime::logs::time::Timestamp::now().to_string(),
        platform: host,
    };
    let store = super::open_store(engine)?;
    let imported = shipyard_image::import::read(input, &store, &about)?;
    if imported.looks_like_an_archive {
        engine.warn(
            WarningKind::ImportedArchive,
            format!(
                "{given}: this looks like an image archive (its first files are an image's): \
                 `shipyard load` reads those; `import` made it a filesystem holding them"
            ),
        );
    }
    #[cfg_attr(
        not(target_os = "linux"),
        allow(unused_variables, reason = "macOS keeps the blobs: nothing to extract")
    )]
    let shared = store.lock(LockMode::Shared, USE_WAIT)?;
    // What the name named before (none if the index is corrupt: as a
    // pull's, the import goes on, and the record sets it aside, said).
    let before = Index::open(store.root()).ok();
    imported.promote(&store)?;
    let pulled = super::archive::stored_image(&store, Some(&name), &imported.manifest)?;
    #[cfg(target_os = "linux")]
    {
        use crate::error::Context as _;
        let layers = shipyard_image::layer_specs(&pulled.manifest, &pulled.config)
            .context("reading the imported layer")?;
        super::extract(engine, &store, &shared, &layers, None)
            .context("extracting the imported layer")?;
    }
    let key = name.to_string();
    let id = pulled.manifest_digest.to_string();
    let moved = before
        .as_ref()
        .and_then(|index| index.get(&key))
        .filter(|had| had.manifest != id)
        .map(|had| had.manifest.clone());
    let record = ImageRecord::of(&pulled, Some(std::time::SystemTime::now()));
    if let Some(aside) = Index::record_many(store.root(), &[(key.clone(), record)])? {
        super::warn_index_reset(engine, &aside);
    }
    crate::events::emit_about(engine, crate::events::Kind::Image, "import", &key);
    Ok(Imported {
        id,
        name: name.familiar(),
        moved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One rule for pull, import and build: the host's OS alone, its
    /// architecture alone, `os/arch`, `os/arch/variant` name it; another
    /// OS, architecture or variant, or a malformed one, doesn't.
    #[test]
    fn only_this_host_s_platform_is_its_own() {
        let host = Platform::host();
        let name = host_platform_name();
        assert_eq!(name, format!("linux/{}", host.architecture));
        for own in ["linux", host.architecture.as_str(), name.as_str()] {
            assert!(is_host_platform(own), "{own}");
        }
        if let Some(variant) = &host.variant {
            assert!(is_host_platform(&format!("{name}/{variant}")));
            assert!(!is_host_platform(&format!("{name}/v1")), "another variant");
        }
        let other = if host.architecture == "arm64" {
            "amd64"
        } else {
            "arm64"
        };
        for not in [
            "windows",
            "windows/amd64",
            other,
            &format!("linux/{other}"),
            "linux/arm64/v8/x",
            "",
        ] {
            assert!(!is_host_platform(not), "{not:?}");
        }
    }
}
