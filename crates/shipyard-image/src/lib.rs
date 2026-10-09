//! OCI and Docker images for Shipyard: the engine's image layer.
//!
//! This crate is the Podman-equivalent image handling that the engine calls:
//! it pulls an image from a registry, verifies and decompresses its blobs,
//! extracts its layers safely, and keeps both in a content-addressed store.
//! It doesn't mount anything (the runtime mounts an image's layers with
//! overlayfs, inside the container's namespaces: `Root::overlay` in
//! `shipyard-runtime`), so this crate is portable: it builds, runs and is
//! tested on macOS and Linux, which lets most of stage 6 (and its fuzzing)
//! happen without the Linux VM.
//!
//! Built in stage 6; see `docs/stages/06-images.md`.

// UNIX only, like the rest of Shipyard: paths, permissions and the store's
// atomic rename semantics assume a POSIX filesystem.
#[cfg(not(unix))]
compile_error!("shipyard-image only supports UNIX platforms (Linux, macOS)");

pub mod containers;
pub mod credentials;
pub mod digest;
pub mod fsat;
pub mod images;
pub mod index;
pub mod manifest;
pub mod pull;
pub mod reference;
pub mod registry;
pub mod store;
pub mod tar_reader;
pub mod unpack;

pub use containers::{Area, AreaError, ContainerArea, LiveArea, areas, remove_areas, trash_areas};
pub use credentials::{Credential, CredentialError, Credentials, FoundCredential};
pub use digest::{Digest, DigestError, Verifier, VerifyError};
pub use images::{
    ExtractError, Extracted, extract_layers, layer_directories, layer_specs, placed_layers,
    remove_layers, trash_layers,
};
// `index::Index` (the store's reference index) is reached through the module
// path to avoid colliding with `manifest::Index` (a multi-platform image
// index); its other types are re-exported.
pub use index::{DiskUsage, ImageRecord, IndexError, ListedImage};
pub use manifest::{
    Descriptor, ImageConfig, ImageManifest, Index, ManifestError, Platform, RuntimeConfig,
};
pub use pull::{PullError, PullOptions, Pulled, from_store, pull};
pub use reference::{ImageReference, ReferenceError};
pub use registry::{BlobError, BlobSink, Registry, RegistryError};
pub use store::{
    BlobWriter, DIFF, GC_WAIT, LayerOwners, LockMode, Store, StoreError, StoreLock, USE_WAIT,
    map_id,
};
pub use unpack::{
    IdRanges, LayerReport, LayerSpec, Limits, Ownership, Shift, UnpackError, UnpackOptions,
    unpack_layer,
};

/// Entry points for the fuzz targets (`fuzz/`) into what isn't public: built
/// only by cargo-fuzz (`--cfg fuzzing`), never in a release.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    /// Reads a `WWW-Authenticate` value as a registry's 401 is read: every
    /// challenge, and the parameters a Bearer one is asked for.
    pub fn challenges(header: &str) {
        for challenge in crate::registry::parse_challenges(header) {
            for name in ["realm", "service", "scope"] {
                let _ = challenge.param(name);
            }
        }
    }

    /// Reads a token server's answer.
    pub fn token(bytes: &[u8]) {
        let _ = crate::registry::parse_token(bytes);
    }
}

/// The largest manifest or config blob read into memory from the store.
/// These are small JSON documents; a layer streams instead of being read
/// whole, so this cap only ever applies to metadata.
pub const MANIFEST_SIZE_LIMIT: u64 = 4 * 1024 * 1024;

/// The most layers an image may have: Docker builds at most 127 (overlayfs
/// once stacked no more), and every layer costs a directory, its files and
/// a lower in the mount. Checked before any layer is fetched.
pub const MAX_LAYERS: usize = 128;
