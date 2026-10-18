//! A container's logs (stage 8, D15): its output, as the monitor captures it,
//! in CRI ("k8s-file") records: [`cri`] for one record, [`time`] for its
//! timestamp, [`info`] for `log.json`, [`writer`] for the monitor's
//! writing and rotation, and [`reader`] for reading them back as lines.
//!
//! The files are pure CRI, which other tools read as they are (`crictl`,
//! fluent-bit's `cri` parser); what Shipyard needs to know of them besides
//! (the format's version, the rotation) is in a `log.json` beside them.

pub mod cri;
pub mod info;
pub mod reader;
pub mod time;
pub mod writer;
