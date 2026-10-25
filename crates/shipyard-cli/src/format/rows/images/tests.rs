use super::super::tests::writes_its_header_s_fields;
use super::*;

/// An image of 3.62 MB, made at the epoch (`created` unknown), its shared
/// size and containers not counted.
fn parts<'a>(id: &'a str, repo_tags: &'a [String], repo_digests: &'a [String]) -> ImageParts<'a> {
    ImageParts {
        id,
        repo_tags,
        repo_digests,
        created: 0,
        size: 3_620_000,
        shared_size: -1,
        containers: -1,
    }
}

/// An image's rows as Docker's `images` makes them: one per name (tag),
/// its digest by repository; one known by its digest alone `<none>` for its
/// tag; one with no name `<none>` for both; sizes and counts not known
/// `N/A`; the ID cut short, or whole with `sha256:` (`--no-trunc`).
#[test]
fn an_image_is_a_row_per_name() {
    let id = format!("sha256:{}", "ab".repeat(32));
    let tags = ["alpine:3.20".to_owned(), "localhost:5000/app:v1".to_owned()];
    let digests = ["alpine@sha256:feed".to_owned()];
    let now = SystemTime::UNIX_EPOCH;
    let rows = ImageRow::rows_of(&parts(&id, &tags, &digests), now, false);
    let named: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|row| {
            (
                row.repository.as_str(),
                row.tag.as_str(),
                row.digest.as_str(),
            )
        })
        .collect();
    assert_eq!(
        named,
        [
            ("alpine", "3.20", "sha256:feed"),
            ("localhost:5000/app", "v1", "<none>")
        ]
    );
    let first = &rows[0];
    assert_eq!(first.id, "ab".repeat(6));
    assert_eq!(first.size, "3.62MB");
    assert_eq!(
        (
            first.shared_size.as_str(),
            first.unique_size.as_str(),
            first.containers.as_str(),
            first.created_since.as_str()
        ),
        ("N/A", "N/A", "N/A", "N/A")
    );
    let digest_only = ImageRow::rows_of(&parts(&id, &[], &digests), now, true);
    assert_eq!(
        (
            digest_only[0].repository.as_str(),
            digest_only[0].tag.as_str(),
            digest_only[0].id.as_str()
        ),
        ("alpine", "<none>", id.as_str())
    );
    let nameless = ImageRow::rows_of(&parts(&id, &[], &[]), now, false);
    assert_eq!(
        (nameless[0].repository.as_str(), nameless[0].tag.as_str()),
        ("<none>", "<none>")
    );
}

/// A history row as Docker's (step 12.2a; checked against Docker 29's
/// `docker history` of the same image): `5 weeks ago`, its RFC 3339 time,
/// the instruction cut to 45 characters with `…`, the ID to 12 but for
/// `<missing>`; bytes and times as given with `-H=false`; whole with
/// `--no-trunc`.
#[test]
fn a_history_row_is_docker_s() {
    let created = 1_789_677_425; // 2026-09-17T20:37:05Z
    let now = unix_time(created + 5 * 604_800 + 3_600);
    let item = shipyard_engine::docker::ImageHistoryItem {
        id: format!("sha256:{}", "26".repeat(32)),
        created,
        created_by: "ADD alpine-minirootfs-3.24.2-aarch64.tar.gz / # buildkit".to_owned(),
        tags: None,
        size: 4_190_000,
        comment: "buildkit.dockerfile.v0".to_owned(),
    };
    let row = HistoryRow::of(&item, now, false, true);
    assert_eq!(row.id, "262626262626");
    assert_eq!(row.created_since, "5 weeks ago");
    assert_eq!(row.created_at, "2026-09-17T20:37:05Z");
    assert_eq!(
        row.created_by,
        "ADD alpine-minirootfs-3.24.2-aarch64.tar.gz …"
    );
    assert_eq!(row.size, "4.19MB");
    let raw = HistoryRow::of(&item, now, true, false);
    assert_eq!(raw.id, item.id);
    assert_eq!(raw.created_since, "2026-09-17T20:37:05Z");
    assert_eq!(raw.created_by, item.created_by);
    assert_eq!(raw.size, "4190000");
    let missing = shipyard_engine::docker::ImageHistoryItem {
        id: "<missing>".to_owned(),
        created: 0,
        ..item
    };
    let row = HistoryRow::of(&missing, now, false, true);
    assert_eq!(
        (row.id.as_str(), row.created_since.as_str()),
        ("<missing>", "")
    );
}

/// What it writes (`--format json`) is its header's fields.
#[test]
fn an_image_row_writes_its_header_s_fields() {
    let text = String::new;
    writes_its_header_s_fields(&ImageRow {
        id: text(),
        repository: text(),
        tag: text(),
        digest: text(),
        created_since: text(),
        created_at: text(),
        size: text(),
        shared_size: text(),
        unique_size: text(),
        containers: text(),
    });
}
