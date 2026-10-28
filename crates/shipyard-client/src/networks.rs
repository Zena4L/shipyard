//! The Networks section's data (step 14.6b): Shipyard's own view of a
//! bridge network (`/shipyard/networks/{name}`, its version checked), read
//! into a type of its own, so its keys (the engine's, Docker-like) are
//! checked here, where the Linux tests read a real one, and handed to the
//! page in the client's own shape.

use serde::{Deserialize, Serialize};

use crate::api::{Client, NETWORK_VERSION, name_ref};
use crate::{ClientError, Result};

/// A bridge network as Shipyard sees it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct NetworkView {
    /// Its name.
    #[serde(rename(deserialize = "Name"))]
    pub name: String,
    /// Its bridge's name.
    #[serde(rename(deserialize = "Bridge"))]
    pub bridge: String,
    /// Its IPv4 gateway.
    #[serde(rename(deserialize = "Gateway"))]
    pub gateway: String,
    /// Whether nothing on it reaches beyond it.
    #[serde(rename(deserialize = "Internal"))]
    pub internal: bool,
    /// Whether its DNS server runs.
    #[serde(rename(deserialize = "DNSRunning"))]
    pub dns_running: bool,
    /// The containers on it now.
    #[serde(rename(deserialize = "Containers"))]
    pub endpoints: Vec<Endpoint>,
}

/// A container on a bridge network: one live lease.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Endpoint {
    /// The container's ID (none for a run with no record).
    #[serde(rename(deserialize = "Container"), default)]
    pub container: Option<String>,
    /// Its IPv4 address.
    #[serde(rename(deserialize = "IPAddress"))]
    pub address: String,
    /// The host's end of its veth pair.
    #[serde(rename(deserialize = "HostVeth"))]
    pub host_veth: String,
}

/// Shipyard's own view of the bridge network `name`: its bridge, gateway,
/// DNS server and each container on it with the host's end of its veth
/// pair; `None` where bridges aren't served (rootless: 403).
///
/// # Errors
///
/// The API's; [`ClientError::Newer`] for a newer answer;
/// [`ClientError::BadReference`] for a name of another shape;
/// [`ClientError::Unreadable`] for an answer of another shape.
pub async fn view(client: &Client, name: &str) -> Result<Option<NetworkView>> {
    let path = format!("/shipyard/networks/{}", name_ref(name)?);
    match client.own(&path, "the network view", NETWORK_VERSION).await {
        Err(ClientError::Api { status: 403, .. }) => Ok(None),
        Err(error) => Err(error),
        Ok(answer) => Ok(Some(serde_json::from_value(answer)?)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The engine's keys, as `network inspect --shipyard` writes them (a
    /// field it adds is ignored), read into the client's own.
    #[test]
    fn the_engine_s_keys_are_read() {
        let answer = serde_json::json!({
            "version": 1, "Name": "net", "Subnet": "10.250.12.0/24", "Gateway": "10.250.12.1",
            "Bridge": "shybr1", "Internal": false, "IsolateHost": false, "Created": 0,
            "DNSRunning": true,
            "Containers": [{
                "Container": "abc", "Run": "r", "Name": "tap", "Aliases": [], "Hostname": "h",
                "Interface": "eth0", "IPAddress": "10.250.12.2", "IPv6Address": null,
                "MacAddress": "02:42:00:00:00:01", "HostVeth": "shyabc", "Ports": []
            }]
        });
        let view: NetworkView = serde_json::from_value(answer).unwrap();
        assert!(view.dns_running);
        assert_eq!(view.endpoints[0].host_veth, "shyabc");
        assert_eq!(view.endpoints[0].container.as_deref(), Some("abc"));
        // Handed on in the client's own keys.
        let given = serde_json::to_value(&view).unwrap();
        assert_eq!(given["dns_running"], true);
        assert_eq!(given["endpoints"][0]["address"], "10.250.12.2");
    }
}
