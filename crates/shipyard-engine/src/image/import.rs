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

/// The platform among `here` (what runs here, the best first:
/// [`platforms_here`](super::platforms_here)) that `asked` (`--platform`,
/// the API's `platform`) names: an OS alone (`linux`: the first), an
/// architecture alone (`arm64`), `os/arch`, or `os/arch/variant` with its
/// variant (any, where it has none). `None` if it names none of them. One
/// rule for pull, run, import and build.
#[must_use]
pub fn named_in(asked: &str, here: &[Platform]) -> Option<Platform> {
    let parts: Vec<&str> = asked.split('/').collect();
    here.iter()
        .find(|one| {
            let (os, arch, variant) = match parts[..] {
                [os] if os == one.os => (os, None, None),
                [arch] => (one.os.as_str(), Some(arch), None),
                [os, arch] => (os, Some(arch), None),
                [os, arch, variant] => (os, Some(arch), Some(variant)),
                _ => return false,
            };
            os == one.os
                && arch.is_none_or(|arch| arch == one.architecture)
                && variant
                    .is_none_or(|variant| one.variant.as_deref().is_none_or(|own| own == variant))
        })
        .cloned()
}

/// Whether `asked` names this host's own platform ([`named_in`] of it
/// alone).
#[must_use]
pub fn is_host_platform(asked: &str) -> bool {
    named_in(asked, &[Platform::host()]).is_some()
}

/// This host's platform as Docker names it (`linux/arm64`): what's said
/// when another is refused.
#[must_use]
pub fn host_platform_name() -> String {
    let host = Platform::host();
    format!("{}/{}", host.os, host.architecture)
}

/// What runs here as Docker names them (`linux/arm64/v8 or linux/amd64`): what's
/// said when another platform is refused.
#[must_use]
pub fn here_names() -> String {
    shipyard_image::manifest::named(&super::platforms_here())
}

/// The platform `asked` names among what runs here, or the refusal of
/// `what` (`imported`, `built`) for it.
///
/// # Errors
///
/// [`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest), naming
/// what runs here.
pub fn platform_here(asked: &str, what: &str) -> Result<Platform> {
    named_in(asked, &super::platforms_here()).ok_or_else(|| {
        crate::Error::new(
            crate::ErrorKind::InvalidRequest,
            format!(
                "only a platform that runs here can be {what} ({}), not {asked:?}{}",
                here_names(),
                super::rosetta_hint(asked)
            ),
        )
    })
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
    /// The platform asked for (`--platform`): one that runs here.
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
    let platform = match &request.platform {
        None => Platform::host(),
        Some(asked) => platform_here(asked, "imported")?,
    };
    let about = shipyard_image::import::About {
        changes: Changes::parse(&request.changes)?,
        message: request.message.clone(),
        created: shipyard_runtime::logs::time::Timestamp::now().to_string(),
        platform,
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

    /// What runs here: the host's platform, then amd64 on an arm64 host
    /// where Rosetta is registered (never on another host); and one rule
    /// naming them: an OS alone the first, an architecture, `os/arch`.
    #[test]
    fn what_runs_here_is_the_host_s_then_rosetta_s() {
        use crate::image::platforms_given;
        let mut arm64 = Platform::new("linux", "arm64");
        arm64.variant = Some("v8".to_owned());
        let amd64 = Platform::new("linux", "amd64");
        assert_eq!(
            platforms_given(arm64.clone(), false),
            std::slice::from_ref(&arm64)
        );
        let here = platforms_given(arm64.clone(), true);
        assert_eq!(here, [arm64.clone(), amd64.clone()]);
        assert_eq!(
            platforms_given(amd64.clone(), true),
            std::slice::from_ref(&amd64),
            "an amd64 host"
        );
        assert_eq!(named_in("linux/amd64", &here), Some(amd64.clone()));
        assert_eq!(named_in("amd64", &here), Some(amd64));
        assert_eq!(named_in("linux", &here), Some(arm64.clone()), "the first");
        assert_eq!(named_in("linux/arm64/v8", &here), Some(arm64));
        for not in ["linux/s390x", "windows/amd64", "linux/amd64/v1/x"] {
            assert_eq!(named_in(not, &here), None, "{not}");
        }
    }

    /// Rosetta counts only as Shipyard's guest registers it: its entry
    /// enabled, its interpreter the guest's share's, `binfmt_misc` enabled; a
    /// Lima VM's entry of the same name (another interpreter) doesn't.
    #[test]
    fn only_shipyard_s_rosetta_counts() {
        use crate::image::shipyards_rosetta;
        let ours = "enabled\ninterpreter /run/rosetta/rosetta\nflags: OCF\noffset 0\n";
        assert!(shipyards_rosetta(ours, "enabled\n"));
        assert!(!shipyards_rosetta(ours, "disabled\n"), "binfmt_misc off");
        assert!(!shipyards_rosetta(
            &ours.replacen("enabled", "disabled", 1),
            "enabled\n"
        ));
        let lima = "enabled\ninterpreter /mnt/lima-rosetta/rosetta\nflags: OCF\n";
        assert!(!shipyards_rosetta(lima, "enabled\n"), "Lima's");
        assert!(!shipyards_rosetta("", "enabled\n"), "none");
    }

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
