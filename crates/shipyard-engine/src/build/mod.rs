//! `build` (stage 12, step 12.3, D28, D12-2, D12.3-*): a Dockerfile built by
//! BuildKit, in a container of its own the engine starts for the build and
//! removes at its end (D12.3-1), its image loaded into the store under the
//! build's names (`-t`), or its files written out (`-o`).
//!
//! The builder is `moby/buildkit` ([`BUILDKIT_IMAGE`], pinned by digest),
//! run as the 12.1 probe found it must be: in a user namespace (a rootless
//! caller's own; root's: IDs from 2100000000, which no user is given),
//! `CAP_SYS_ADMIN` in it
//! alone, seccomp unconfined (runc's keyring), `buildkitd` in its rootless
//! mode with no process sandbox. A build's `RUN` steps are BuildKit's
//! processes there: they see the builder's processes (and `buildctl`'s
//! arguments), not its files (the reach probe, 12.3). What the builder sees:
//! the context, read-only; this build's own directory (`in/`: the
//! Dockerfile and the secrets, copied; `out/`: what it writes); its
//! context's cache (D12.3-5: one a context, never shared across projects).
//! Its output is read as hostile: the image through `load`, under the
//! build's names alone; the files through the unpacker.

mod args;
#[cfg(target_os = "linux")]
mod builder;
pub mod cache;
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "a build runs on Linux only")
)]
mod local;

use std::path::PathBuf;

/// The builder's image: `moby/buildkit`, the version the reach probe ran,
/// by its digest (found by that digest in the store, whatever it's named:
/// a `load` of it serves an offline host).
pub const BUILDKIT_IMAGE: &str =
    "moby/buildkit:v0.34.1@sha256:99368f3e46177c6405b9740e4461d91cee004128daeecca78d4ceac50ec1da5c";

/// The most build args, labels or secrets one build takes, each.
pub const MAX_EACH: usize = 64;

/// The largest Dockerfile or secret a build copies in.
pub const MAX_FILE: u64 = 1024 * 1024;

/// Where a build's Dockerfile comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dockerfile {
    /// The context's own `Dockerfile`.
    Default,
    /// A file (`-f FILE`), anywhere the caller can read.
    File(PathBuf),
    /// Its text (`-f -`, from standard input).
    Text(Vec<u8>),
}

/// A secret a build's `RUN --mount=type=secret,id=ID` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secret {
    /// Its ID (letters, digits, `.`, `_`, `-`).
    pub id: String,
    /// The file it's read from.
    pub src: PathBuf,
}

/// Where a build's result goes besides (or instead of) an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Its files, into this directory (`-o type=local,dest=DIR`).
    Local(PathBuf),
    /// Its files as one tar archive, to the build's writer (`-o
    /// type=tar,dest=FILE`).
    Tar,
}

/// What a build is asked (`shipyard build`'s flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    /// The context: a directory.
    pub context: PathBuf,
    /// The Dockerfile.
    pub dockerfile: Dockerfile,
    /// The image's names (`-t`): at least one, unless `output` says where
    /// its files go (D12.3-2).
    pub tags: Vec<String>,
    /// `--build-arg KEY=VALUE`.
    pub build_args: Vec<(String, String)>,
    /// `--target STAGE`.
    pub target: Option<String>,
    /// `--no-cache`.
    pub no_cache: bool,
    /// `--pull`: base images fetched again.
    pub pull: bool,
    /// `--label KEY=VALUE`.
    pub labels: Vec<(String, String)>,
    /// `--platform`: one that runs here ([`crate::image::platforms_here`]).
    pub platform: Option<String>,
    /// `--secret id=ID,src=FILE`.
    pub secrets: Vec<Secret>,
    /// `-o`.
    pub output: Option<Output>,
}

impl BuildRequest {
    /// A request for `context` with nothing else set.
    #[must_use]
    pub fn new(context: impl Into<PathBuf>) -> Self {
        Self {
            context: context.into(),
            dockerfile: Dockerfile::Default,
            tags: Vec::new(),
            build_args: Vec::new(),
            target: None,
            no_cache: false,
            pull: false,
            labels: Vec::new(),
            platform: None,
            secrets: Vec::new(),
            output: None,
        }
    }
}

/// What a build made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Built {
    /// The image's ID, if one was loaded.
    pub id: Option<String>,
    /// Its names, as Docker shows them.
    pub names: Vec<String>,
    /// The names it moved from other images, and those images' IDs.
    pub moved: Vec<(String, String)>,
}

/// Builds `request` (see the module's doc): each line of BuildKit's
/// progress to the engine's reporter; an `-o type=tar` archive to `tar_out`.
///
/// # Errors
///
/// What's refused before anything runs ([`ErrorKind::InvalidRequest`](crate::ErrorKind::InvalidRequest):
/// no `-t` and no `-o`, a name with a digest, a context that isn't a
/// directory, too many args); the build's own failure
/// ([`ErrorKind::Build`](crate::ErrorKind::Build), with BuildKit's last
/// lines); BuildKit that didn't start ([`ErrorKind::Environment`](crate::ErrorKind::Environment));
/// the store's.
pub fn build(
    engine: &crate::Engine,
    request: &BuildRequest,
    tar_out: Option<&mut dyn std::io::Write>,
) -> crate::Result<Built> {
    let names = args::checked(request)?;
    #[cfg(target_os = "linux")]
    {
        builder::build(engine, request, &names, tar_out)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, names, tar_out);
        Err(crate::error::linux_only("building images"))
    }
}
