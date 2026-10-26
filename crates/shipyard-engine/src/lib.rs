//! The container engine for Shipyard: the logic between what a user asks
//! for and the runtime that does it (the "Engine" layer of
//! `docs/FEATURES.md`; the hardening track's H1).
//!
//! **What goes here:** decisions and orchestration, behind typed APIs that
//! return data. The CLI only parses arguments and prints what comes back;
//! stage 11's API and stage 14's UI will call the same functions. Nothing
//! here prints.
//!
//! **What doesn't:** anything that talks to the kernel directly (ptys and
//! termios, `SCM_RIGHTS`, `setns`, mounts, cgroups, the per-container
//! monitor) stays in `shipyard-runtime`, the OCI runtime. That includes the
//! user's own terminal: the engine decides when it goes raw (`run -it`,
//! `attach`), and the runtime's terminal primitives do it and restore it.
//!
//! **The layering goes one way:** the engine uses the runtime, never the
//! reverse, as Podman calls runc. `deny.toml` enforces it: only
//! `shipyard-cli` may depend on this crate. That package also holds the
//! `shipyard-runtime` binary, a front end that may use the engine; the
//! `shipyard-runtime` crate never does.
//!
//! **How it talks back:** operations take an [`Engine`]: where things are
//! kept ([`Paths`]), and a [`Reporter`] for what the engine has to say while
//! it works (warnings, notices, hints, a run's failure, a pull's progress). They return data, or an
//! [`Error`] with a [kind](ErrorKind) a caller can act on.
//!
//! **Helpers are this binary, re-executed:** the storage helper (a rootless
//! caller's layer work, [`image::storage_helper_entry_point`]), a network's
//! DNS server ([`network::dns_entry_point`]), `cp`'s helper
//! ([`cp::helper_entry_point`]), a detached container's clean-up
//! (`__cleanup`, [`run::cleanup_entry_point`]), its health checks
//! (`__health`, [`run::health_entry_point`]) and `doctor`'s probe
//! ([`doctor::probe_entry_point`]) start the running binary
//! again, so a binary that uses them calls [`entry_points`] first thing in
//! `main`, before anything starts a thread: every helper's, in their one
//! order. Pasta's entry point (in `shipyard-network`) is given
//! [`network::pasta_lost`] as its hook.
//!
//! It holds what the CLI's commands do: runs and containers, images,
//! volumes, networks, logs, attach, `exec`, `cp`, events, health checks and
//! `doctor`'s checks of the host;
//! the CLI parses and prints.

// Nothing here prints (`tests/boundaries.rs` checks what this can't).
#![deny(clippy::print_stdout, clippy::print_stderr)]

#[cfg(not(unix))]
compile_error!("shipyard-engine only supports UNIX platforms (Linux, macOS)");

pub mod api_service;
pub mod attach;
pub mod build;
pub mod containers;
pub mod cp;
pub mod docker;
pub mod doctor;
mod entry;
mod error;
pub mod events;
pub mod exec;
mod fs;
pub mod guest;
pub mod health;
pub mod image;
pub mod logs;
pub mod network;
mod paths;
pub mod prune;
mod random;
pub mod report;
pub mod run;
pub(crate) mod streams;
mod test_pause;
pub mod volumes;

use std::sync::Arc;

pub use entry::{Hooks, entry_points};
pub use error::{Error, ErrorKind, Result, StreamsError, said_once};
pub use paths::Paths;

use report::{Message, Reporter, WarningKind};

/// What every engine operation works with: where things are kept, and where
/// what it has to say goes. Cheap to clone (the reporter is shared).
#[derive(Clone)]
pub struct Engine {
    paths: Paths,
    reporter: Arc<dyn Reporter>,
}

impl Engine {
    /// An engine keeping things at `paths`, reporting to `reporter`.
    #[must_use]
    pub fn new(paths: Paths, reporter: Arc<dyn Reporter>) -> Self {
        Self { paths, reporter }
    }

    /// Where it keeps things.
    #[must_use]
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Where what it has to say goes.
    #[must_use]
    pub fn reporter(&self) -> &Arc<dyn Reporter> {
        &self.reporter
    }

    /// Reports `message`.
    pub(crate) fn report(&self, message: Message) {
        self.reporter.report(message);
    }

    /// Reports a warning of `kind`.
    pub(crate) fn warn(&self, kind: WarningKind, message: impl Into<String>) {
        report::warn(self.reporter.as_ref(), kind, message);
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

/// What the crate's tests share.
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
pub(crate) mod testing {
    use std::sync::Arc;

    use crate::report::Collect;
    use crate::{Engine, Paths};

    /// An engine on a store of its own, in a fresh temporary directory.
    ///
    /// Not for a rootless extraction: the storage helper is this binary
    /// re-executed, and a test binary doesn't call its entry point.
    pub(crate) fn engine_on_a_new_store(name: &str) -> (Engine, Arc<Collect>, std::path::PathBuf) {
        let parent = std::env::temp_dir().join(format!(
            "shipyard-engine-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&parent).unwrap();
        let collect = Arc::new(Collect::default());
        let engine = Engine::new(
            Paths::from_env().with_image_store(parent.join("store")),
            Arc::clone(&collect) as Arc<dyn crate::report::Reporter>,
        );
        (engine, collect, parent)
    }
}
