//! What the `shipyard` and `shipyard-runtime` binaries share: the resource
//! limit flags, the printing of a container's logs (with the time and
//! duration flags' parsers, which `events` and the health flags use too),
//! and the pipe checks (a closed stdout's reader).
//! Internal to this package, not an API.

pub mod limits;
pub mod logs_output;
pub mod pipe;
