use crate::format::template::Row;

use super::*;

/// Go's `Time.String` in UTC, the fraction only when there's one
/// (Docker's `CreatedAt`: `2026-10-20 13:54:44 +0000 UTC`).
#[test]
fn times_are_go_s() {
    assert_eq!(go_time(1_792_504_484, 0), "2026-10-20 13:54:44 +0000 UTC");
    assert_eq!(
        go_time(1_792_504_484, 32_406_375),
        "2026-10-20 13:54:44.032406375 +0000 UTC"
    );
    assert_eq!(
        go_time(1_792_504_484, 500_000_000),
        "2026-10-20 13:54:44.5 +0000 UTC"
    );
}

/// `ps`'s COMMAND: cut to 20 with an ellipsis (`"sh -c 'echo a long …"`),
/// then quoted as Go quotes it.
#[test]
fn commands_are_cut_and_quoted_as_docker_s() {
    let command = "sh -c 'echo a long command with words'";
    assert_eq!(go_quote(&ellipsis(command, 20)), "\"sh -c 'echo a long …\"");
    assert_eq!(go_quote("sleep 600"), "\"sleep 600\"");
    assert_eq!(
        go_quote("sh -c \"a\\b\"\n\u{1b}\u{202e}"),
        "\"sh -c \\\"a\\\\b\\\"\\n\\x1b\\u202e\""
    );
    assert_eq!(ellipsis("shipyard-fmt-vol", 15), "shipyard-fmt-v…");
    assert_eq!(ellipsis("short", 15), "short");
}

/// Every row's header names every field it has, so a table's header never
/// writes `<no value>`; and the fields are Docker 29.8's (its `--format
/// json`, 2026-10-20), no more, no fewer.
#[test]
fn every_field_is_docker_s_and_has_a_header() {
    fn names<R: Row>() -> Vec<&'static str> {
        let mut names: Vec<&str> = R::headers().iter().map(|&(name, _)| name).collect();
        names.sort_unstable();
        names
    }
    assert_eq!(
        names::<ContainerRow>(),
        [
            "Command",
            "CreatedAt",
            "HealthStatus",
            "ID",
            "Image",
            "Labels",
            "LocalVolumes",
            "Mounts",
            "Names",
            "Networks",
            "Platform",
            "Ports",
            "RunningFor",
            "Size",
            "State",
            "Status"
        ]
    );
    assert_eq!(
        names::<ImageRow>(),
        [
            "Containers",
            "CreatedAt",
            "CreatedSince",
            "Digest",
            "ID",
            "Repository",
            "SharedSize",
            "Size",
            "Tag",
            "UniqueSize"
        ]
    );
    assert_eq!(
        names::<VolumeRow>(),
        [
            "Availability",
            "Driver",
            "Group",
            "Labels",
            "Links",
            "Mountpoint",
            "Name",
            "Scope",
            "Size",
            "Status"
        ]
    );
    assert_eq!(
        names::<NetworkRow>(),
        [
            "CreatedAt",
            "Driver",
            "ID",
            "IPv4",
            "IPv6",
            "Internal",
            "Labels",
            "Name",
            "Scope"
        ]
    );
}

/// What a row writes (`--format json`) is its header's fields, no more, no
/// fewer: a field added to one and not the other fails its row's test.
pub(super) fn writes_its_header_s_fields<R: Row>(row: &R) {
    let written = serde_json::to_value(row).unwrap();
    let mut keys: Vec<&str> = written
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut names: Vec<&str> = R::headers().iter().map(|&(name, _)| name).collect();
    names.sort_unstable();
    assert_eq!(keys, names);
}
