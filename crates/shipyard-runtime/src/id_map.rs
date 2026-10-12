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
//!
//! Root may write any map. Anyone else may write only one line, mapping
//! their own ID, and only after denying `setgroups` (step 1's probe, U3).
//! More IDs come from the setuid helpers `newuidmap` and `newgidmap`, which
//! give a user the subordinate ranges root granted them in `/etc/subuid` and
//! `/etc/subgid` (`subuid(5)`): how a rootless container gets 65536 IDs.

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
            parts
                .next()
                .ok_or_else(malformed)?
                .parse()
                .map_err(|_| malformed())
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

/// A rootless caller's maps, and why they're short if they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootlessMaps {
    /// The maps.
    pub maps: IdMaps,
    /// Why only the caller's own ID is mapped, and how to get the full
    /// range: for the user to see. `None` with the full maps.
    pub shortfall: Option<String>,
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
    /// An empty map or one with more than the kernel's 340 ranges, an empty
    /// range, a range past the last ID, overlapping ranges (inside or
    /// outside), or no container uid or gid 0.
    pub fn validate(&self) -> Result<(), IdMapError> {
        for (kind, map) in self.each() {
            validate_map(kind, map)?;
        }
        Ok(())
    }

    /// Whether either map includes host ID 0: root inside, or some user
    /// inside, is then root (or group root) on the host's files, which a
    /// user namespace is usually there to prevent.
    #[must_use]
    pub fn maps_host_root(&self) -> bool {
        self.maps_host_uid(0) || self.maps_host_gid(0)
    }

    /// Both maps, with which each is.
    pub(crate) fn each(&self) -> [(MapKind, &[IdMap]); 2] {
        [(MapKind::Uids, &self.uids), (MapKind::Gids, &self.gids)]
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

    /// Whether host uid `id` is someone inside (some container uid maps to
    /// it).
    #[must_use]
    pub fn maps_host_uid(&self, id: u32) -> bool {
        self.uids
            .iter()
            .any(|range| range.outside().contains(&u64::from(id)))
    }

    /// Whether host gid `id` is some group inside.
    #[must_use]
    pub fn maps_host_gid(&self, id: u32) -> bool {
        self.gids
            .iter()
            .any(|range| range.outside().contains(&u64::from(id)))
    }

    /// The maps as a process's `/proc/PID/uid_map` and `gid_map` show them
    /// (`uids`, `gids`): a range a line, "inside outside count". `None` if
    /// either isn't that, or is empty (not written yet).
    #[must_use]
    pub fn from_proc(uids: &str, gids: &str) -> Option<Self> {
        let parse = |text: &str| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    let mut fields = line.split_whitespace().map(str::parse::<u32>);
                    match (fields.next(), fields.next(), fields.next(), fields.next()) {
                        (Some(Ok(container)), Some(Ok(host)), Some(Ok(size)), None) => {
                            Some(IdMap {
                                container,
                                host,
                                size,
                            })
                        }
                        _ => None,
                    }
                })
                .collect::<Option<Vec<_>>>()
        };
        // Unwritten maps (a namespace just made) map nothing yet.
        Some(Self {
            uids: parse(uids).filter(|map| !map.is_empty())?,
            gids: parse(gids).filter(|map| !map.is_empty())?,
        })
    }
}

impl IdMaps {
    /// A rootless caller's full maps: container 0 is the caller's own ID,
    /// then each subordinate range, in order, from container ID 1 (as
    /// Podman maps them). A range is cut short at the last container ID;
    /// ranges past it, or past the kernel's limit on ranges, are left out.
    /// The host side isn't checked here: [`IdMaps::validate`] does.
    #[must_use]
    pub fn rootless(
        (uid, gid): (u32, u32),
        subuids: &[(u32, u32)],
        subgids: &[(u32, u32)],
    ) -> Self {
        let map = |own: u32, ranges: &[(u32, u32)]| {
            let mut map = vec![IdMap {
                container: 0,
                host: own,
                size: 1,
            }];
            let mut next: u32 = 1;
            for &(start, count) in ranges.iter().take(MAX_RANGES - 1) {
                // Container IDs end at 2³²−2: (uid_t)-1 is "no ID".
                let size = count.min(u32::MAX - next);
                if size == 0 {
                    break;
                }
                map.push(IdMap {
                    container: next,
                    host: start,
                    size,
                });
                next += size;
            }
            map
        };
        Self {
            uids: map(uid, subuids),
            gids: map(gid, subgids),
        }
    }

    /// The one-ID maps a process may write for itself: container 0 is
    /// `uid` and `gid`, nothing else.
    #[must_use]
    pub fn single((uid, gid): (u32, u32)) -> Self {
        Self::rootless((uid, gid), &[], &[])
    }
}

/// The subordinate ranges `/etc/subuid` (or `/etc/subgid`) gives the user
/// named `name` with ID `id`: lines `name-or-id:start:count`, in order.
/// Lines that don't parse, and empty ranges, are skipped (as shadow-utils
/// does).
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub(crate) fn subordinate_ranges(text: &str, name: Option<&str>, id: u32) -> Vec<(u32, u32)> {
    let id = id.to_string();
    text.lines()
        .filter_map(|line| {
            let mut fields = line.trim().split(':');
            let owner = fields.next()?;
            let start = fields.next()?.parse().ok()?;
            let count = fields.next()?.parse().ok()?;
            let ours = owner == id || name == Some(owner);
            (ours && fields.next().is_none() && count > 0).then_some((start, count))
        })
        .collect()
}

fn host_id(map: &[IdMap], id: u32) -> Option<u32> {
    map.iter().find_map(|range| range.host_id(id))
}

fn validate_map(kind: MapKind, map: &[IdMap]) -> Result<(), IdMapError> {
    if map.is_empty() {
        return Err(IdMapError::Empty(kind));
    }
    if map.len() > MAX_RANGES {
        return Err(IdMapError::TooManyRanges(kind, map.len()));
    }
    let last = u64::from(u32::MAX);
    for (index, range) in map.iter().enumerate() {
        if range.size == 0 {
            return Err(IdMapError::EmptyRange(kind, *range));
        }
        // The kernel's last ID is 2³²−2: 2³²−1 is (uid_t)-1, "no ID".
        if range.inside().end > last || range.outside().end > last {
            return Err(IdMapError::PastTheEnd(kind, *range));
        }
        for earlier in &map[..index] {
            if overlap(earlier.inside(), range.inside()) {
                return Err(IdMapError::OverlapInside(kind, *earlier, *range));
            }
            if overlap(earlier.outside(), range.outside()) {
                return Err(IdMapError::OverlapOutside(kind, *earlier, *range));
            }
        }
    }
    if host_id(map, 0).is_none() {
        return Err(IdMapError::NoRoot(kind));
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
        let _ = writeln!(
            contents,
            "{} {} {}",
            range.container, range.host, range.size
        );
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
    #[error("the {0} has {1} ranges, more than the kernel's {MAX_RANGES}")]
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

    #[test]
    fn host_ids_inside_and_procs_maps() {
        let maps = IdMaps::from_proc(
            "         0       1000          1\n         1     100000      65536\n",
            "0 1000 1\n",
        )
        .unwrap();
        assert!(maps.maps_host_uid(1000));
        assert!(maps.maps_host_uid(100_000) && maps.maps_host_uid(165_535));
        assert!(!maps.maps_host_uid(165_536) && !maps.maps_host_uid(0));
        assert!(maps.maps_host_gid(1000) && !maps.maps_host_gid(100_000));
        // The host's own (`0 0 4294967295`): every ID.
        let host = IdMaps::from_proc("0 0 4294967295\n", "0 0 4294967295\n").unwrap();
        assert!(host.maps_host_uid(0) && host.maps_host_uid(u32::MAX - 1));
        for bad in ["0 1000", "0 1000 1 2", "a b c", "0 -1 1", "", "\n"] {
            assert_eq!(IdMaps::from_proc(bad, "0 0 1"), None, "{bad}");
        }
    }

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
        for bad in [
            "",
            "0:100000",
            "0:100000:65536:1",
            "a:b:c",
            "-1:0:1",
            "0:1:4294967296",
        ] {
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
    fn host_root_in_a_map_is_noticed() {
        assert!(!maps(&["0:100000:65536"]).maps_host_root());
        assert!(maps(&["0:0:65536"]).maps_host_root());
        assert!(maps(&["0:1000:1", "1:0:1"]).maps_host_root());
        let mut gids_only = maps(&["0:100000:65536"]);
        gids_only.gids = vec![map("0:0:1")];
        assert!(gids_only.maps_host_root());
    }

    #[test]
    fn subordinate_ranges_are_found_by_name_or_number() {
        let file = "alice:100000:65536\n\
                    1001:200000:65536\n\
                    alice:300000:1000\n\
                    # a comment\n\
                    bob:400000:65536\n\
                    alice:x:1\n\
                    alice:500000:0\n\
                    alice:1:2:3\n";
        assert_eq!(
            subordinate_ranges(file, Some("alice"), 1000),
            [(100_000, 65536), (300_000, 1000)]
        );
        assert_eq!(subordinate_ranges(file, None, 1001), [(200_000, 65536)]);
        assert_eq!(subordinate_ranges(file, Some("carol"), 1002), []);
        assert_eq!(subordinate_ranges("", Some("alice"), 1000), []);
    }

    #[test]
    fn rootless_maps_put_the_caller_at_0_then_the_ranges() {
        let maps = IdMaps::rootless(
            (1000, 1000),
            &[(100_000, 65536), (300_000, 1000)],
            &[(100_000, 65536)],
        );
        assert_eq!(
            maps.uids,
            [
                map("0:1000:1"),
                map("1:100000:65536"),
                map("65537:300000:1000")
            ]
        );
        assert_eq!(maps.gids, [map("0:1000:1"), map("1:100000:65536")]);
        maps.validate().unwrap();
        assert_eq!(maps.host_uid(1000), Some(100_999));
        let single = IdMaps::single((1000, 1001));
        assert_eq!(single.uids, [map("0:1000:1")]);
        assert_eq!(single.gids, [map("0:1001:1")]);
        single.validate().unwrap();
    }

    #[test]
    fn rootless_maps_stop_before_overflowing() {
        let huge = [(100_000, u32::MAX - 10), (200_000, 100)];
        let maps = IdMaps::rootless((1000, 1000), &huge, &[]);
        assert_eq!(
            maps.uids.len(),
            3,
            "the second range is cut short, not dropped"
        );
        assert_eq!(maps.uids[2], map("4294967286:200000:9"));
        let whole = IdMaps::rootless((1000, 1000), &[(100_000, u32::MAX)], &[]);
        assert_eq!(
            whole.uids[1].size,
            u32::MAX - 1,
            "cut at the last container ID"
        );
        let many: Vec<(u32, u32)> = (0..400).map(|i| (100_000 + i * 10, 10)).collect();
        assert_eq!(
            IdMaps::rootless((1000, 1000), &many, &[]).uids.len(),
            MAX_RANGES
        );
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
