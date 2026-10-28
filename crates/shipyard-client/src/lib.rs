//! Shipyard's desktop app's client of the API (step 14.2, D14.2-1): all of
//! the app's own logic, outside its window, so it's tested where the rest
//! of Shipyard is. Where the API is ([`place`]), Docker's endpoints through
//! `bollard` and Shipyard's own `/shipyard/…` ([`api`]), the event stream
//! ([`events`]), a container's live numbers ([`stats`]), its logs and a
//! shell in it ([`console`]), the Run sheet's run
//! ([`run`]), Linux's service started when it's silent ([`service`]), the images, volumes and networks ([`images`], [`volumes`],
//! [`networks`]), the macOS VM's phase and start ([`vm`]), and the app's
//! settings ([`settings`]). Shipyard's own formats are read at the versions
//! this knows: a newer one is refused, never guessed at (D14.2-5).
//!
//! It never uses the engine (`deny.toml`): the app is a client of the API,
//! the same on Linux and on the Mac (D14-6).

pub mod api;
pub mod console;
mod error;
pub mod events;
pub mod images;
pub mod networks;
pub mod place;
pub mod run;
pub mod service;
pub mod settings;
pub mod stats;
pub mod usage;
pub mod vm;
pub mod volumes;

pub use error::{ClientError, ErrorKind, Result};
