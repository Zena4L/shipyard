//! The container engine for Shipyard: the logic between what a user asks
//! for and the runtime that does it (the "Engine" layer of
//! `docs/FEATURES.md`; the hardening track's H1).
//!
//! **What goes here:** decisions and orchestration, behind typed APIs that
//! return data. The CLI only parses arguments and prints what comes back;
//! stage 11's API and stage 13's UI will call the same functions. Nothing
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
//! Stage 8 starts it with what it adds (following and reading logs, the
//! attach client, `cp`'s streams, `run -it`'s orchestration); stage 9 moves
//! the rest of the CLI's logic in.

#[cfg(not(unix))]
compile_error!("shipyard-engine only supports UNIX platforms (Linux, macOS)");

pub mod attach;
pub mod cp;
mod error;
pub mod logs;
pub mod streams;

pub use error::StreamsError;
