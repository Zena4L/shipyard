//! The guest's manifest (`manifest.json`, written by
//! `scripts/guest/build.sh`): what the guest was made of, versioned from
//! its first write (H2). The host boots only a guest whose manifest it
//! reads, and whose files are there.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use sha2::{Digest as _, Sha256};

use crate::VmError;

/// The manifest's version this binary reads.
pub const MANIFEST_VERSION: u32 = 1;

/// The most a manifest is read as (it's a few hundred bytes).
const MANIFEST_CAP: u64 = 64 * 1024;

/// Why a manifest can't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// A version newer than [`MANIFEST_VERSION`].
    #[error(
        "a manifest of version {0}, newer than this binary's {MANIFEST_VERSION}: update Shipyard"
    )]
    Newer(u32),
    /// Not a manifest this version reads.
    #[error("not a guest manifest: {0}")]
    Unreadable(String),
}

/// The guest's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// The Alpine release the guest is built from.
    pub alpine: String,
    /// The minirootfs's sha256.
    pub minirootfs_sha256: String,
    /// The packages installed, as `NAME-VERSION`.
    pub packages: Vec<String>,
    /// The kernel.
    pub kernel: Kernel,
    /// The initramfs.
    pub initramfs: Hashed,
    /// The `shipyard` inside the guest.
    pub shipyard: Build,
}

/// The guest's kernel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kernel {
    /// Its version (`6.18.57-0-virt`).
    pub version: String,
    /// Its raw image's sha256.
    pub sha256: String,
}

/// A file by its sha256.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hashed {
    /// Its sha256.
    pub sha256: String,
}

/// The guest's `shipyard`: the build it came from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// The repository's revision (`-dirty` with uncommitted changes).
    pub revision: String,
    /// The binary's sha256.
    pub sha256: String,
}

/// The guest's files, found and read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guest {
    /// Its manifest.
    pub manifest: Manifest,
    /// The raw kernel.
    pub kernel: PathBuf,
    /// The initramfs.
    pub initramfs: PathBuf,
}

/// The manifest's file name.
pub const MANIFEST: &str = "manifest.json";

impl Guest {
    /// The guest in `dir`: its manifest read, its kernel and initramfs
    /// there.
    ///
    /// # Errors
    ///
    /// [`VmError::Guest`]: no guest there (with how to build one), a
    /// manifest that can't be read, a newer one, or a file missing.
    pub fn find(dir: &Path) -> Result<Self, VmError> {
        let path = dir.join(MANIFEST);
        let bytes = shipyard_sys::fs::read_bounded(&path, MANIFEST_CAP).map_err(|error| {
            VmError::Guest(format!(
                "no guest at {} ({error}): build one with scripts/guest/build.sh, or set \
                 SHIPYARD_GUEST_DIR",
                dir.display()
            ))
        })?;
        let manifest = Manifest::parse(&bytes)
            .map_err(|why| VmError::Guest(format!("{}: {why}", path.display())))?;
        let guest = Self {
            manifest,
            kernel: dir.join("Image"),
            initramfs: dir.join("initramfs.cpio.gz"),
        };
        for file in [&guest.kernel, &guest.initramfs] {
            if !file.is_file() {
                return Err(VmError::Guest(format!(
                    "{} is missing beside its manifest",
                    file.display()
                )));
            }
        }
        Ok(guest)
    }

    /// Checks the kernel's and the initramfs's contents against the
    /// manifest's sha256s: a guest whose files and manifest don't match (a
    /// build cut short, files copied from two builds) isn't booted.
    ///
    /// # Errors
    ///
    /// [`VmError::Guest`], naming the file; [`VmError::Io`] if one can't be
    /// read.
    pub fn verify(&self) -> Result<(), VmError> {
        for (file, expected) in [
            (&self.kernel, &self.manifest.kernel.sha256),
            (&self.initramfs, &self.manifest.initramfs.sha256),
        ] {
            let mut hasher = Sha256::new();
            let mut reader = std::fs::File::open(file).map_err(|e| VmError::io(file, e))?;
            let mut buffer = vec![0; 1 << 20];
            loop {
                let read = std::io::Read::read(&mut reader, &mut buffer)
                    .map_err(|e| VmError::io(file, e))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            let found = hex(&hasher.finalize());
            if &found != expected {
                return Err(VmError::Guest(format!(
                    "{} isn't the file its manifest names (sha256 {found}, the manifest's \
                     {expected}): build the guest again",
                    file.display()
                )));
            }
        }
        Ok(())
    }
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

impl Manifest {
    /// A manifest read from `bytes`: a newer version is refused, never
    /// guessed at.
    ///
    /// # Errors
    ///
    /// [`ManifestError`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let unreadable = |error: serde_json::Error| ManifestError::Unreadable(error.to_string());
        let versioned: Versioned = serde_json::from_slice(bytes).map_err(unreadable)?;
        if versioned.version > MANIFEST_VERSION {
            return Err(ManifestError::Newer(versioned.version));
        }
        serde_json::from_slice(bytes).map_err(unreadable)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    const V1: &str = r#"{
      "version": 1,
      "alpine": "3.24.2",
      "minirootfs_sha256": "9bf7",
      "packages": ["linux-virt-6.18.57-r0"],
      "kernel": {"version": "6.18.57-0-virt", "sha256": "1993"},
      "initramfs": {"sha256": "e1c3"},
      "shipyard": {"revision": "bd6eb6fd2d36", "sha256": "209b"}
    }"#;

    /// Version 1, as the build script writes it, is read; a newer one is
    /// refused; a field this version doesn't know is refused too.
    #[test]
    fn a_manifest_of_version_1_is_read_and_a_newer_refused() {
        let manifest = Manifest::parse(V1.as_bytes()).unwrap();
        assert_eq!(manifest.kernel.version, "6.18.57-0-virt");
        assert_eq!(manifest.shipyard.revision, "bd6eb6fd2d36");
        let newer = V1.replace("\"version\": 1", "\"version\": 2");
        assert_eq!(
            Manifest::parse(newer.as_bytes()),
            Err(ManifestError::Newer(2))
        );
        let unknown = V1.replace("\"alpine\"", "\"extra\": 1, \"alpine\"");
        assert!(matches!(
            Manifest::parse(unknown.as_bytes()),
            Err(ManifestError::Unreadable(_))
        ));
    }

    /// A guest is its manifest and both its files; without one of them,
    /// it's refused, naming what's missing; files that aren't the ones its
    /// manifest names are refused too, the kernel named.
    #[test]
    fn a_guest_needs_its_manifest_and_its_own_files() {
        let dir = std::env::temp_dir().join(format!("shipyard-guest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(Guest::find(&dir), Err(VmError::Guest(_))));
        std::fs::write(dir.join(MANIFEST), V1).unwrap();
        std::fs::write(dir.join("Image"), b"k").unwrap();
        let Err(VmError::Guest(why)) = Guest::find(&dir) else {
            panic!("found without its initramfs");
        };
        assert!(why.contains("initramfs.cpio.gz"), "{why}");
        std::fs::write(dir.join("initramfs.cpio.gz"), b"i").unwrap();
        let guest = Guest::find(&dir).unwrap();
        assert_eq!(guest.manifest.alpine, "3.24.2");
        let Err(VmError::Guest(why)) = guest.verify() else {
            panic!("a guest that doesn't match its manifest, accepted");
        };
        assert!(why.contains("Image"), "{why}");
        let matched = V1
            .replace("\"1993\"", &format!("\"{}\"", hex(&Sha256::digest(b"k"))))
            .replace("\"e1c3\"", &format!("\"{}\"", hex(&Sha256::digest(b"i"))));
        std::fs::write(dir.join(MANIFEST), matched).unwrap();
        Guest::find(&dir).unwrap().verify().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
