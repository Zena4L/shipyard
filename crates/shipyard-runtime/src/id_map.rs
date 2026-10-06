//! A user namespace's ID maps (`user_namespaces(7)`).
//!
//! A user namespace has UIDs and GIDs of its own. The kernel translates them
//! with two maps, each a list of ranges: container ID `container` is host ID
//! `host`, and so on for `size` IDs. Root inside (uid 0) can then be an
//! ordinary user outside (100000), with every capability over what the
//! namespace owns and none over the host's. An ID not in the map doesn't
//! exist inside: its files show up as the overflow ID (65534, `nobody`).
//!
//! The maps are written once, by the parent, into `/proc/PID/uid_map` and
//! `gid_map` (see `process/spawn.rs`), as lines of `container host size`.

use std::fmt;
#[cfg(any(target_os = "linux", test))]
use std::fmt::Write as _;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The most ranges a map may have (`user_namespaces(7)`: 340 since Linux
/// 4.15).
pub(crate) const MAX_RANGES: usize = 340;

/// One range of a map: `size` IDs from `container` inside are `host`
/// onwards outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdMap {
    /// The first ID inside the container.
    pub container: u32,
    /// The host ID it is.
    pub host: u32,
    /// How many IDs.
    pub size: u32,
}

impl IdMap {
    /// The host ID of `id` inside, if this range maps it.
    #[must_use]
    pub fn host_id(self, id: u32) -> Option<u32> {
        let offset = id.checked_sub(self.container)?;
        if offset < self.size {
            self.host.checked_add(offset)
        } else {
            None
        }
    }

    /// The IDs inside, as a range (`u64`: the end may be 2³²).
    fn inside(self) -> std::ops::Range<u64> {
        u64::from(self.container)..u64::from(self.container) + u64::from(self.size)
    }

    /// The IDs outside.
    fn outside(self) -> std::ops::Range<u64> {
        u64::from(self.host)..u64::from(self.host) + u64::from(self.size)
    }
}

impl fmt::Display for IdMap {
    /// `CONTAINER:HOST:SIZE`, as `--uidmap` takes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.container, self.host, self.size)
    }
}

impl FromStr for IdMap {
    type Err = IdMapError;

    /// `CONTAINER:HOST:SIZE` (Podman's `--uidmap`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let malformed = || IdMapError::Malformed(s.to_owned());
        let mut parts = s.split(':');
        let mut next = || -> Result<u32, IdMapError> {
            parts.next().ok_or_else(malformed)?.parse().map_err(|_| malformed())
        };
        let map = Self {
            container: next()?,
            host: next()?,
            size: next()?,
        };
        if parts.next().is_some() {
            return Err(malformed());
        }
        Ok(map)
    }
}

/// A user namespace's two maps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdMaps {
    /// User IDs.
    pub uids: Vec<IdMap>,
    /// Group IDs.
    pub gids: Vec<IdMap>,
}

/// Which of the two maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapKind {
    /// `uid_map`.
    Uids,
    /// `gid_map`.
    Gids,
}

impl fmt::Display for MapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Uids => "uid_map",
            Self::Gids => "gid_map",
        })
    }
}

impl IdMaps {
    /// Checks both maps as the kernel would, and that root inside exists:
    /// the container's setup runs as uid and gid 0 inside.
    ///
    /// # Errors
    ///
    /// An empty map or one with more than [`MAX_RANGES`] ranges, an empty
    /// range, a range past the last ID, overlapping ranges (inside or
    /// outside), or no container uid or gid 0.
    pub fn validate(&self) -> Result<(), IdMapError> {
        for (which, map) in [(MapKind::Uids, &self.uids), (MapKind::Gids, &self.gids)] {
            validate_map(which, map)?;
        }
        Ok(())
    }

    /// The host uid of container uid `id`, if it's mapped.
    #[must_use]
    pub fn host_uid(&self, id: u32) -> Option<u32> {
        host_id(&self.uids, id)
    }

    /// The host gid of container gid `id`, if it's mapped.
    #[must_use]
    pub fn host_gid(&self, id: u32) -> Option<u32> {
        host_id(&self.gids, id)
    }
}

fn host_id(map: &[IdMap], id: u32) -> Option<u32> {
    map.iter().find_map(|range| range.host_id(id))
}

fn validate_map(which: MapKind, map: &[IdMap]) -> Result<(), IdMapError> {
    if map.is_empty() {
        return Err(IdMapError::Empty(which));
    }
    if map.len() > MAX_RANGES {
        return Err(IdMapError::TooManyRanges(which, map.len()));
    }
    let last = u64::from(u32::MAX);
    for (index, range) in map.iter().enumerate() {
        if range.size == 0 {
            return Err(IdMapError::EmptyRange(which, *range));
        }
        // The kernel's last ID is 2³²−2: 2³²−1 is (uid_t)-1, "no ID".
        if range.inside().end > last || range.outside().end > last {
            return Err(IdMapError::PastTheEnd(which, *range));
        }
        for earlier in &map[..index] {
            if overlap(earlier.inside(), range.inside()) {
                return Err(IdMapError::OverlapInside(which, *earlier, *range));
            }
            if overlap(earlier.outside(), range.outside()) {
                return Err(IdMapError::OverlapOutside(which, *earlier, *range));
            }
        }
    }
    if host_id(map, 0).is_none() {
        return Err(IdMapError::NoRoot(which));
    }
    Ok(())
}

fn overlap(a: std::ops::Range<u64>, b: std::ops::Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

/// A map's contents as `/proc/PID/uid_map` takes them: one line per range.
/// The kernel takes a map in one `write`, once.
#[must_use]
#[cfg(any(target_os = "linux", test))]
pub(crate) fn render(map: &[IdMap]) -> String {
    let mut contents = String::new();
    for range in map {
        // Writing to a `String` can't fail.
        let _ = writeln!(contents, "{} {} {}", range.container, range.host, range.size);
    }
    contents
}

/// Why ID maps can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdMapError {
    /// Not `CONTAINER:HOST:SIZE`.
    #[error("\"{0}\" isn't CONTAINER:HOST:SIZE, three numbers (like 0:100000:65536)")]
    Malformed(String),
    /// No ranges.
    #[error("the {0} is empty")]
    Empty(MapKind),
    /// More ranges than the kernel takes.
    #[error("the {0} has {1} ranges, more than the kernel's 340")]
    TooManyRanges(MapKind, usize),
    /// A range of no IDs.
    #[error("the {0} range {1} is empty")]
    EmptyRange(MapKind, IdMap),
    /// A range past the last ID.
    #[error("the {0} range {1} goes past the last ID (4294967294)")]
    PastTheEnd(MapKind, IdMap),
    /// Two ranges share IDs inside.
    #[error("the {0} ranges {1} and {2} overlap inside the container")]
    OverlapInside(MapKind, IdMap, IdMap),
    /// Two ranges share host IDs.
    #[error("the {0} ranges {1} and {2} overlap on the host")]
    OverlapOutside(MapKind, IdMap, IdMap),
    /// No ID 0 inside.
    #[error("the {0} must map ID 0: the container's setup runs as root inside")]
    NoRoot(MapKind),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(s: &str) -> IdMap {
        s.parse().unwrap()
    }

    fn maps(uids: &[&str]) -> IdMaps {
        let ranges: Vec<IdMap> = uids.iter().map(|s| map(s)).collect();
        IdMaps {
            uids: ranges.clone(),
            gids: ranges,
        }
    }

    #[test]
    fn ranges_parse_as_container_host_size() {
        assert_eq!(
            map("0:100000:65536"),
            IdMap {
                container: 0,
                host: 100_000,
                size: 65536
            }
        );
        assert_eq!(map("0:100000:65536").to_string(), "0:100000:65536");
        for bad in ["", "0:100000", "0:100000:65536:1", "a:b:c", "-1:0:1", "0:1:4294967296"] {
            assert_eq!(
                bad.parse::<IdMap>(),
                Err(IdMapError::Malformed(bad.to_owned())),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_range_maps_its_ids_and_no_others() {
        let range = map("0:100000:65536");
        assert_eq!(range.host_id(0), Some(100_000));
        assert_eq!(range.host_id(1000), Some(101_000));
        assert_eq!(range.host_id(65535), Some(165_535));
        assert_eq!(range.host_id(65536), None);
        assert_eq!(map("1000:5000:1").host_id(999), None);
    }

    #[test]
    fn valid_maps_pass() {
        maps(&["0:100000:65536"]).validate().unwrap();
        maps(&["0:1000:1", "1:100000:65535"]).validate().unwrap();
        // Every ID but (uid_t)-1: what the host's own namespace shows.
        maps(&["0:0:4294967295"]).validate().unwrap();
    }

    #[test]
    fn what_the_kernel_would_refuse_is_refused() {
        let err = |uids: &[&str]| maps(uids).validate().unwrap_err().to_string();
        assert_eq!(err(&[]), "the uid_map is empty");
        assert!(err(&["0:100000:0"]).contains("is empty"));
        assert!(err(&["0:4294967295:1"]).contains("past the last ID"));
        assert!(err(&["0:1:4294967295"]).contains("past the last ID"));
        assert!(err(&["0:100000:10", "5:200000:10"]).contains("overlap inside"));
        assert!(err(&["0:100000:10", "20:100005:10"]).contains("overlap on the host"));
        assert!(err(&["1:100000:10"]).contains("must map ID 0"));
        let many: Vec<String> = (0..341).map(|i| format!("{i}:{}:1", 100_000 + i)).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(err(&many).contains("341 ranges"));
    }

    #[test]
    fn each_map_is_checked_on_its_own() {
        let mut both = maps(&["0:100000:65536"]);
        both.gids = vec![map("1:100000:65536")];
        assert_eq!(both.validate(), Err(IdMapError::NoRoot(MapKind::Gids)));
    }

    #[test]
    fn maps_render_one_line_per_range() {
        assert_eq!(
            render(&[map("0:100000:1"), map("1:200000:65535")]),
            "0 100000 1\n1 200000 65535\n"
        );
    }

    #[test]
    fn host_ids_come_from_the_right_map() {
        let mut both = maps(&["0:100000:65536"]);
        both.gids = vec![map("0:200000:65536")];
        assert_eq!(both.host_uid(5), Some(100_005));
        assert_eq!(both.host_gid(5), Some(200_005));
        assert_eq!(both.host_uid(70000), None);
    }
}
