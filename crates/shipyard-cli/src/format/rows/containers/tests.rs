use super::super::tests::writes_its_header_s_fields;
use super::*;

/// Ports as Docker's `ps` writes them (2026-10-20's capture: `0.0.0.0:18089->80/tcp,
/// [::]:18089->80/tcp`).
#[test]
fn ports_are_docker_s() {
    assert_eq!(
        port_text(Some(("0.0.0.0", 18089)), 80, "tcp"),
        "0.0.0.0:18089->80/tcp"
    );
    assert_eq!(
        port_text(Some(("::", 18089)), 80, "tcp"),
        "[::]:18089->80/tcp"
    );
    assert_eq!(port_text(None, 53, "udp"), "53/udp");
}

/// What it writes (`--format json`) is its header's fields.
#[test]
fn a_container_row_writes_its_header_s_fields() {
    let text = String::new;
    writes_its_header_s_fields(&ContainerRow {
        id: text(),
        names: text(),
        image: text(),
        command: text(),
        created_at: text(),
        running_for: text(),
        ports: text(),
        state: text(),
        status: text(),
        size: text(),
        labels: text(),
        mounts: text(),
        local_volumes: text(),
        networks: text(),
        platform: None,
        health_status: text(),
        label_map: BTreeMap::new(),
    });
}
