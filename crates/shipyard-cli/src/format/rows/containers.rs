//! `ps`'s rows: a container as Docker's `--format` sees it.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::Serialize;
use shipyard_engine::docker::ContainerSummary;

use crate::format::template::Row;

use super::{ellipsis, go_quote, go_time, joined_labels, short, unix, unix_time};

/// A container, Docker's `ContainerContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ContainerRow {
    #[serde(rename = "ID")]
    id: String,
    names: String,
    image: String,
    command: String,
    created_at: String,
    running_for: String,
    ports: String,
    state: String,
    status: String,
    size: String,
    labels: String,
    mounts: String,
    local_volumes: String,
    networks: String,
    platform: Option<()>,
    health_status: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl ContainerRow {
    /// `summary` as Docker's `ps` shows it at `now`, cut short unless
    /// `full` (`--no-trunc`).
    pub(crate) fn new(summary: &ContainerSummary, now: SystemTime, full: bool) -> Self {
        let command = if full {
            summary.command.clone()
        } else {
            ellipsis(&summary.command, 20)
        };
        Self {
            id: short(&summary.id, full),
            names: summary
                .names
                .iter()
                .map(|name| name.trim_start_matches('/'))
                .collect::<Vec<_>>()
                .join(","),
            image: summary.image.clone(),
            command: go_quote(&command),
            created_at: go_time(unix(summary.created), 0),
            running_for: format!(
                "{} ago",
                shipyard_engine::docker::ago(unix_time(summary.created), now)
            ),
            ports: summary
                .ports
                .iter()
                .map(|port| {
                    port_text(
                        port.ip.as_deref().zip(port.public_port),
                        port.private_port,
                        &port.kind,
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
            state: summary.state.to_owned(),
            status: summary.status.clone(),
            // Not measured (Docker's without `--size`).
            size: "0B".to_owned(),
            labels: joined_labels(&summary.labels),
            mounts: summary
                .mounts
                .iter()
                .map(|mount| {
                    let name = if mount.name.is_empty() {
                        &mount.source
                    } else {
                        &mount.name
                    };
                    if full {
                        name.clone()
                    } else {
                        ellipsis(name, 15)
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            local_volumes: summary
                .mounts
                .iter()
                .filter(|mount| mount.kind == "volume")
                .count()
                .to_string(),
            networks: summary
                .network_settings
                .networks
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
            platform: None,
            health_status: summary.health.to_owned(),
            label_map: summary.labels.clone(),
        }
    }
}

impl Row for ContainerRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "CONTAINER ID"),
            ("Names", "NAMES"),
            ("Image", "IMAGE"),
            ("Command", "COMMAND"),
            ("CreatedAt", "CREATED AT"),
            ("RunningFor", "CREATED"),
            ("Ports", "PORTS"),
            ("State", "STATE"),
            ("Status", "STATUS"),
            ("Size", "SIZE"),
            ("Labels", "LABELS"),
            ("Mounts", "MOUNTS"),
            ("LocalVolumes", "LOCAL VOLUMES"),
            ("Networks", "NETWORKS"),
            ("Platform", "PLATFORM"),
            ("HealthStatus", "HEALTH STATUS"),
        ]
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// `ps`'s default table (Docker's).
pub(crate) const CONTAINERS_TABLE: &str = "table {{.ID}}\t{{.Image}}\t{{.Command}}\t\
     {{.RunningFor}}\t{{.Status}}\t{{.Ports}}\t{{.Names}}";

/// A port as `ps` shows it: `0.0.0.0:8080->80/tcp`, `[::]:8080->80/tcp`
/// where it's published (`host`: the address and port), `80/tcp` where
/// it isn't.
fn port_text(host: Option<(&str, u16)>, private: u16, kind: &str) -> String {
    match host {
        Some((ip, public)) if ip.contains(':') => format!("[{ip}]:{public}->{private}/{kind}"),
        Some((ip, public)) => format!("{ip}:{public}->{private}/{kind}"),
        None => format!("{private}/{kind}"),
    }
}

#[cfg(test)]
mod tests;
