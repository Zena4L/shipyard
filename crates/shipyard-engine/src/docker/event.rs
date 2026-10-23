//! An event as Docker's `GET /events` writes it (`events.Message`), one JSON
//! object a line: its type, action and actor, the time in seconds and in
//! nanoseconds, and the older top-level `status`, `id` and `from` Docker
//! still writes for containers and images.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::events::{Event, Kind};

/// An event, Docker's `events.Message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct EventMessage {
    /// The action (containers' and images' only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The subject's ID (containers' and images' only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The container's image (containers' only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// `container`, `image`, `volume` or `network`.
    #[serde(rename = "Type")]
    pub kind: String,
    /// What happened (`start`, `die`, `pull`, …).
    #[serde(rename = "Action")]
    pub action: String,
    /// What it happened to.
    #[serde(rename = "Actor")]
    pub actor: Actor,
    /// Always `local` (Docker's swarm scope is the other).
    pub scope: &'static str,
    /// When, in seconds since the epoch.
    pub time: i64,
    /// When, in nanoseconds since the epoch.
    #[serde(rename = "timeNano")]
    pub time_nano: i64,
}

/// An event's subject, Docker's `events.Actor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Actor {
    /// Its ID.
    #[serde(rename = "ID")]
    pub id: String,
    /// More about it (`name`, `image`, `exitCode`, …).
    pub attributes: BTreeMap<String, String>,
}

impl From<&Event> for EventMessage {
    fn from(event: &Event) -> Self {
        let since = event
            .time
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let legacy = matches!(event.kind, Kind::Container | Kind::Image);
        Self {
            status: legacy.then(|| event.action.clone()),
            id: legacy.then(|| event.id.clone()),
            from: (event.kind == Kind::Container)
                .then(|| event.attributes.get("image").cloned())
                .flatten(),
            kind: event.kind.to_string(),
            action: event.action.clone(),
            actor: Actor {
                id: event.id.clone(),
                attributes: event.attributes.clone(),
            },
            scope: "local",
            time: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            time_nano: i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    /// As Docker 29.8 answers `GET /v1.43/events` (read off its socket,
    /// 2026-10-19): a container's die with the older `status`, `id` and
    /// `from`; a network's connect without them.
    #[test]
    fn events_are_written_as_docker_writes_them() {
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_760_000_000, 123_456_789);
        let die = Event::new(Kind::Container, "die", "0123abcd")
            .with("name", "web")
            .with("image", "alpine")
            .with("exitCode", 3)
            .at(at);
        assert_eq!(
            serde_json::to_value(EventMessage::from(&die)).unwrap(),
            serde_json::json!({
                "status": "die", "id": "0123abcd", "from": "alpine",
                "Type": "container", "Action": "die",
                "Actor": {"ID": "0123abcd", "Attributes": {"exitCode": "3", "image": "alpine", "name": "web"}},
                "scope": "local", "time": 1_760_000_000_i64, "timeNano": 1_760_000_000_123_456_789_i64
            })
        );
        let connect = Event::new(Kind::Network, "connect", "backend")
            .with("container", "0123abcd")
            .at(at);
        assert_eq!(
            serde_json::to_value(EventMessage::from(&connect)).unwrap(),
            serde_json::json!({
                "Type": "network", "Action": "connect",
                "Actor": {"ID": "backend", "Attributes": {"container": "0123abcd"}},
                "scope": "local", "time": 1_760_000_000_i64, "timeNano": 1_760_000_000_123_456_789_i64
            })
        );
    }
}
