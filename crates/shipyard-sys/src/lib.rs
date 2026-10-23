//! Shipyard's low-level plumbing, written once for every crate: the files
//! Shipyard keeps its own state in are written whole or not at all, read
//! with a bound and without following a link someone else planted, and
//! locked through files opened the same safe way, waited for with one loop
//! and kept or let go by one rule (`lock`); descriptors passed to another
//! process (`fdpass`), taken over across `exec` (`fd`), and processes
//! signalled through a pidfd (`pidfd`); the bytes between two sockets,
//! both ways (`relay`); a versioned JSON document read, a newer one refused
//! (`versioned`); a socket systemd listened on, taken (`listen`); the
//! kernel's random bytes (`random`).
//!
//! A leaf crate: it depends on nothing of Shipyard's, so the runtime, the
//! network, image and storage crates and the engine all use the same code.

#[cfg(not(unix))]
compile_error!("shipyard-sys only supports UNIX platforms (Linux, macOS)");

pub mod fd;
pub mod fdpass;
pub mod fs;
pub mod listen;
pub mod lock;
#[cfg(target_os = "linux")]
pub mod pidfd;
pub mod random;
pub mod relay;
pub mod versioned;
