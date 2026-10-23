//! A run, from a request (the CLI's `shipyard run` flags, or an API's) to
//! its end: [`run`] does it all. Its steps are here too, for a caller that
//! runs a container another way ([`detached`](fn@detached): one that outlives its
//! caller): the request
//! checked as far as it can be before an image is pulled ([`check`]), the
//! image made ready (`prepare_image`, Linux), then the run finished with what
//! the image brings ([`Planned::finish`]) into an [`Invocation`] the
//! runtime runs.
//!
//! A mistake in a request costs no download: everything an image doesn't
//! change is checked first. Nothing here prints: what a run has to say goes
//! to the engine's reporter, or comes back as data.

mod bundle;
mod cleanup;
pub(crate) mod command;
mod connect;
#[cfg(target_os = "linux")]
mod create;
mod detached;
mod devices;
#[cfg(target_os = "linux")]
mod files;
mod finish;
#[cfg(target_os = "linux")]
mod foreground;
mod health;
#[cfg(target_os = "linux")]
mod held_off;
mod helper;
pub(crate) mod holds;
mod isolation;
mod joined;
mod launch;
mod mounts;
mod network;
mod pause;
mod plan;
mod prepare;
mod recover;
#[cfg(target_os = "linux")]
mod remove;
mod rename;
mod request;
#[cfg(target_os = "linux")]
mod restart_policy;
#[cfg(target_os = "linux")]
mod start;
mod stop;
#[cfg(target_os = "linux")]
mod take;
#[cfg(test)]
mod tests;
mod update;
mod validate;
mod volumes;

pub use cleanup::cleanup_entry_point;
pub use command::{parse_name_or_id, parse_user};
pub use connect::{ConnectRequest, connect, connect_with, disconnect, disconnect_with};
pub use detached::{
    Detached, Finished, Foreground, RemoveOptions, attached, create, detached, detached_with,
    remove, remove_with, start, start_attached, start_with,
};
pub(crate) use detached::{remove_for_prune, settle_for_prune};
pub use devices::DeviceArg;
#[cfg(target_os = "linux")]
pub(crate) use files::{Files, container_files};
pub use health::health_entry_point;
pub(crate) use joined::resolve_joins;
pub use launch::{Outcome, run};
pub use mounts::{DockerMount, MountRequest, TmpfsMount, parse_mount};
pub(crate) use network::MAX_NETWORKS;
pub use pause::{pause, unpause};
pub use plan::{Checked, Invocation, Planned, check};
pub use prepare::ImageDefaults;
#[cfg(target_os = "linux")]
pub(crate) use prepare::{prepare_image, prepare_owned_image};
#[cfg(target_os = "linux")]
pub(crate) use recover::boot_time;
#[cfg(target_os = "linux")]
pub(crate) use recover::restarts_held_off;
pub use recover::{Booted, Stopped, boot, stop_for_the_host};
pub use rename::rename;
pub(crate) use request::Joined;
pub use request::{
    CapabilityArg, DEFAULT_PIDS_MAX, IpcMode, Limits, RunRequest, RunStreams, SecurityOpt,
    Supervision,
};
pub use stop::{StopOptions, kill, restart, stop, wait};
pub use update::{UpdateRequest, update};
pub use volumes::{MadeSource, bind_source, covers, make_bind_source};
