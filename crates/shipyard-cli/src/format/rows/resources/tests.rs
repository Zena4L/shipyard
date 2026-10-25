use super::super::tests::writes_its_header_s_fields;
use super::*;

/// `system df`'s rows, Docker's: a size at 4 digits, the reclaimable part
/// with its share (`15.94GB (78%)`), none of nothing; `≥` where something
/// couldn't be read.
#[test]
fn disk_usage_rows_are_docker_s() {
    let usage = |bytes, reclaimable, complete| shipyard_engine::image::Usage {
        total: 27,
        active: 1,
        bytes,
        reclaimable,
        complete,
    };
    let row = DfRow::new("Images", &usage(20_350_000_000, 15_940_000_000, true));
    assert_eq!(
        (
            row.kind,
            row.total_count.as_str(),
            row.active.as_str(),
            row.size.as_str(),
            row.reclaimable.as_str()
        ),
        ("Images", "27", "1", "20.35GB", "15.94GB (78%)")
    );
    assert_eq!(
        DfRow::new("Local Volumes", &usage(0, 0, true)).reclaimable,
        "0B"
    );
    let partial = DfRow::new("Containers", &usage(4096, 0, false));
    assert_eq!(
        (partial.size.as_str(), partial.reclaimable.as_str()),
        ("≥ 4.096kB", "≥ 0B (0%)")
    );
}

/// What it writes (`--format json`) is its header's fields.
#[test]
fn a_volume_row_writes_its_header_s_fields() {
    let text = String::new;
    writes_its_header_s_fields(&VolumeRow {
        name: text(),
        driver: text(),
        scope: text(),
        mountpoint: text(),
        labels: text(),
        links: text(),
        size: text(),
        group: text(),
        availability: text(),
        status: text(),
        label_map: BTreeMap::new(),
    });
}

/// What it writes (`--format json`) is its header's fields.
#[test]
fn a_network_row_writes_its_header_s_fields() {
    let text = String::new;
    writes_its_header_s_fields(&NetworkRow {
        id: text(),
        name: text(),
        driver: text(),
        scope: text(),
        ipv4: text(),
        ipv6: text(),
        internal: text(),
        labels: text(),
        created_at: text(),
        label_map: BTreeMap::new(),
    });
}
