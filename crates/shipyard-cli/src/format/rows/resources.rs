//! The rows of `volume ls`, `network ls` and `system df`, as Docker's
//! `--format` sees them.

use std::collections::BTreeMap;

use serde::Serialize;
use shipyard_engine::docker::{NetworkResource, Volume};

use crate::format::template::Row;

use super::units::human_size;
use super::{go_time, joined_labels, short};

/// A volume, Docker's `volumeContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct VolumeRow {
    name: String,
    driver: String,
    scope: String,
    mountpoint: String,
    labels: String,
    links: String,
    size: String,
    group: String,
    availability: String,
    status: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl From<&Volume> for VolumeRow {
    fn from(volume: &Volume) -> Self {
        let labels = volume.labels.clone().unwrap_or_default();
        let not_known = || "N/A".to_owned();
        Self {
            name: volume.name.clone(),
            driver: volume.driver.clone(),
            scope: volume.scope.clone(),
            mountpoint: volume.mountpoint.clone(),
            labels: joined_labels(&labels),
            links: not_known(),
            size: not_known(),
            group: not_known(),
            availability: not_known(),
            status: not_known(),
            label_map: labels,
        }
    }
}

impl Row for VolumeRow {
    fn id(&self) -> &str {
        &self.name
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("Name", "VOLUME NAME"),
            ("Driver", "DRIVER"),
            ("Scope", "SCOPE"),
            ("Mountpoint", "MOUNTPOINT"),
            ("Labels", "LABELS"),
            ("Links", "LINKS"),
            ("Size", "SIZE"),
            ("Group", "GROUP"),
            ("Availability", "AVAILABILITY"),
            ("Status", "STATUS"),
        ]
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// `volume ls`' default table (Docker's).
pub(crate) const VOLUMES_TABLE: &str = "table {{.Driver}}\t{{.Name}}";

/// A network, Docker's `networkContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct NetworkRow {
    #[serde(rename = "ID")]
    id: String,
    name: String,
    driver: String,
    scope: String,
    #[serde(rename = "IPv4")]
    ipv4: String,
    #[serde(rename = "IPv6")]
    ipv6: String,
    internal: String,
    labels: String,
    created_at: String,
    #[serde(skip)]
    label_map: BTreeMap<String, String>,
}

impl NetworkRow {
    /// `network` as Docker's `network ls` shows it, its ID cut short unless
    /// `full`.
    pub(crate) fn new(network: &NetworkResource, full: bool) -> Self {
        let created_at = shipyard_runtime::logs::time::Timestamp::parse(&network.created)
            .map(|stamp| go_time(stamp.secs(), stamp.nanos()))
            .unwrap_or_default();
        Self {
            id: short(&network.id, full),
            name: network.name.clone(),
            driver: network.driver.clone(),
            scope: network.scope.clone(),
            ipv4: network.enable_ipv4.to_string(),
            ipv6: network.enable_ipv6.to_string(),
            internal: network.internal.to_string(),
            labels: joined_labels(&network.labels),
            created_at,
            label_map: network.labels.clone(),
        }
    }
}

impl Row for NetworkRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "NETWORK ID"),
            ("Name", "NAME"),
            ("Driver", "DRIVER"),
            ("Scope", "SCOPE"),
            ("IPv4", "IPV4"),
            ("IPv6", "IPV6"),
            ("Internal", "INTERNAL"),
            ("Labels", "LABELS"),
            ("CreatedAt", "CREATED AT"),
        ]
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        Some(&self.label_map)
    }
}

/// `network ls`' default table (Docker's).
pub(crate) const NETWORKS_TABLE: &str = "table {{.ID}}\t{{.Name}}\t{{.Driver}}\t{{.Scope}}";

/// A kind of thing `system df` counts, Docker's `diskUsage…Context`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DfRow {
    #[serde(rename = "Type")]
    kind: &'static str,
    total_count: String,
    active: String,
    size: String,
    reclaimable: String,
}

impl DfRow {
    /// `usage` of `kind` (`Images`, `Containers`, `Local Volumes`), its
    /// sizes Docker's (`20.35GB`), the reclaimable part with its share of
    /// the whole (`15.94GB (78%)`). What couldn't all be read is at least
    /// what's said: `≥`.
    pub(crate) fn new(kind: &'static str, usage: &shipyard_engine::image::Usage) -> Self {
        let at_least = if usage.complete { "" } else { "≥ " };
        let reclaimable = match usage
            .reclaimable
            .saturating_mul(100)
            .checked_div(usage.bytes)
        {
            Some(share) => format!("{at_least}{} ({share}%)", human_size(usage.reclaimable, 4)),
            None => format!("{at_least}{}", human_size(usage.reclaimable, 4)),
        };
        Self {
            kind,
            total_count: usage.total.to_string(),
            active: usage.active.to_string(),
            size: format!("{at_least}{}", human_size(usage.bytes, 4)),
            reclaimable,
        }
    }
}

impl Row for DfRow {
    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("Type", "TYPE"),
            ("TotalCount", "TOTAL"),
            ("Active", "ACTIVE"),
            ("Size", "SIZE"),
            ("Reclaimable", "RECLAIMABLE"),
        ]
    }

    fn id(&self) -> &str {
        self.kind
    }
}

/// `system df`'s default table (Docker's).
pub(crate) const DF_TABLE: &str =
    "table {{.Type}}\t{{.TotalCount}}\t{{.Active}}\t{{.Size}}\t{{.Reclaimable}}";

#[cfg(test)]
mod tests;
