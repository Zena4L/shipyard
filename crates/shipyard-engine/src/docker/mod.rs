//! Shipyard's objects as Docker's Engine API shapes them (step 11.1, D25):
//! the types `docker inspect` prints, and that stage 11's API serves, built
//! from the engine's own (a container's record, its state, its leases).
//! Their JSON is Docker's (its field names, its zero values, its `null`s),
//! so a client written for Docker reads it; what Shipyard has that Docker
//! hasn't stays in Shipyard's own views ([`crate::containers::inspect`]).
//!
//! Docker's Engine API reference (v1.43, `ContainerInspectResponse`) is the
//! contract, checked against a real Docker's `inspect` of the same flags
//! (step 11.1's differential): every key Shipyard writes is Docker's, with
//! Docker's type, its empty lists `null` as Docker's are. Keys Docker has
//! and Shipyard can't fill yet (its runtime's paths, its log driver's
//! settings, the image manifest's descriptor) are left out, as Docker
//! leaves out what it hasn't.

mod container;
mod event;
pub mod filters;
mod image;
mod inspect;
mod network;
#[cfg(target_os = "linux")]
pub(crate) mod proc;
mod standing;
pub(crate) mod stats;
mod summary;
mod system;
mod top;
mod volume;

pub use container::{
    ContainerConfig, ContainerInspect, ContainerState, EndpointIpamConfig, EndpointSettings,
    GraphDriver as ContainerGraphDriver, Health, HealthConfig, HealthcheckResult, HostConfig,
    LogConfig, MountPoint, NetworkSettings, PortBinding, RestartPolicy, inspect_container,
};
pub use event::{Actor, EventMessage};
pub use image::{
    GraphDriver, ImageConfig, ImageHistoryItem, ImageInspect, ImageSummary, Metadata, RootFs,
    image_history, inspect_image, list_images, list_images_where,
};
pub use inspect::{Inspected, Kind, inspect_any};
pub use network::{
    CONNTRACK_LIMIT, ConfigFrom, Found, Honoured, ISOLATE_HOST, Ipam, IpamConfig, NetworkContainer,
    NetworkCreate, NetworkResource, check_driver as check_network_driver, check_options,
    create_network, endpoint_id, find_network, inspect_network, is_predefined, list_networks,
    list_networks_where, network_id,
};
pub use standing::{End, LastEnd, Standing, last_end, standing};
pub use stats::{
    BlkioEntry, BlkioStats, ContainerStats, CpuStats, CpuUsage, MemoryStats, NetworkStats,
    PidsStats, ThrottlingData, stats,
};
pub use summary::{
    ContainerSummary, SummaryHostConfig, SummaryNetworks, SummaryPort, ago, list_containers,
    list_containers_where, status_text,
};
pub use system::{
    API_VERSION, Build, Component, ComponentDetails, Info, MIN_API_VERSION, Platform,
    SystemVersion, go_arch, system_info, system_version,
};
pub use top::{TOP_TITLES, Top, top};
pub use volume::{
    ANONYMOUS_VOLUME, Volume, VolumeList, inspect_volume, list_volumes, list_volumes_where,
};

/// Docker's zero time, for a start or an end there hasn't been.
pub const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// A time as Docker writes one: RFC 3339 in UTC with its nanoseconds, the
/// trailing zeros left out (Go's `RFC3339Nano`: `…:20Z`, `…:00.000000005Z`).
#[must_use]
pub fn time(at: std::time::SystemTime) -> String {
    let full = crate::events::rfc3339(at);
    match full.strip_suffix('Z').and_then(|t| t.split_once('.')) {
        Some((seconds, nanos)) => {
            let nanos = nanos.trim_end_matches('0');
            if nanos.is_empty() {
                format!("{seconds}Z")
            } else {
                format!("{seconds}.{nanos}Z")
            }
        }
        None => full,
    }
}

/// Now, in Unix seconds: what a prune's `until` duration counts back from.
pub use shipyard_sys::time::unix_now;

/// A new ID, as Docker writes one: 64 hex characters from the kernel's
/// random bytes (an exec instance's).
///
/// # Errors
///
/// The kernel's random bytes couldn't be read.
pub fn new_id() -> crate::Result<String> {
    Ok(shipyard_sys::hex::lower(&crate::random::bytes::<32>()?))
}

/// A time as Docker's clients send one (`since`, `until`): Unix seconds
/// with an optional fraction of up to nine digits (`1760000000.5`: its CLI
/// turns `10m` into one), or RFC 3339. `None` if it's neither.
#[must_use]
pub fn parse_time(text: &str) -> Option<shipyard_runtime::logs::time::Timestamp> {
    use shipyard_runtime::logs::time::Timestamp;
    if let Ok(time) = Timestamp::parse(text) {
        return Some(time);
    }
    let (secs, fraction) = match text.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (text, ""),
    };
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if secs.is_empty() || !digits(secs) || !digits(fraction) || fraction.len() > 9 {
        return None;
    }
    let nanos = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<9}").parse().ok()?
    };
    Timestamp::from_unix(secs.parse().ok()?, nanos)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    /// Unix seconds with a fraction, or RFC 3339; nothing else.
    #[test]
    fn times_are_read_as_docker_s_clients_send_them() {
        let unix = |text| super::parse_time(text).map(|time| (time.secs(), time.nanos()));
        assert_eq!(unix("1760000000"), Some((1_760_000_000, 0)));
        assert_eq!(unix("1760000000.5"), Some((1_760_000_000, 500_000_000)));
        assert_eq!(unix("1760000000.000000001"), Some((1_760_000_000, 1)));
        assert_eq!(unix("2025-10-09T08:53:20Z"), Some((1_760_000_000, 0)));
        for bad in [
            "",
            ".5",
            "1760000000.",
            "17600000a0",
            "1.0000000001",
            "-1",
            "10m",
            "1e9",
        ] {
            assert_eq!(unix(bad), None, "{bad:?}");
        }
    }

    /// Go's `RFC3339Nano`: nanoseconds without their trailing zeros, none
    /// at all for a whole second.
    #[test]
    fn times_are_written_as_go_writes_them() {
        let at = |secs, nanos| SystemTime::UNIX_EPOCH + Duration::new(secs, nanos);
        assert_eq!(super::time(at(1_760_000_000, 0)), "2025-10-09T08:53:20Z");
        assert_eq!(
            super::time(at(1_760_000_100, 5)),
            "2025-10-09T08:55:00.000000005Z"
        );
        assert_eq!(
            super::time(at(1_760_000_100, 120_000_000)),
            "2025-10-09T08:55:00.12Z"
        );
    }
}
