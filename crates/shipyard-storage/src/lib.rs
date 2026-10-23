//! Container storage for Shipyard: named volumes.
//!
//! A volume is a directory under the store that outlives the containers
//! that mount it (`--volume NAME:/path`); a bind mount is a host path given
//! the same way. Both become bind mounts in the container's root filesystem.
//! (An image's root filesystem itself is an overlay the runtime mounts
//! inside the container's namespaces: `Root::overlay` in
//! `shipyard-runtime`.)

#[cfg(not(unix))]
compile_error!("shipyard-storage only supports UNIX platforms (Linux, macOS)");

pub mod volume;

pub use volume::{
    Filling, Pruned, Volume, VolumeError, VolumeHold, VolumeMount, Volumes, is_trash,
};
