//! The live numbers (step 14.8a2): what the host, each running container
//! and its monitor use now (`/shipyard/usage`, its version checked), read
//! into types of their own, so its keys are checked here, where the Linux
//! tests read a real one. Counters, never rates: the page divides two reads
//! for a CPU share, as Docker's CLI does.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::api::{Client, USAGE_VERSION, older_if_missing};

/// What everything uses now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Usage {
    /// The host's memory (on a Mac, its VM's).
    pub memory: HostMemory,
    /// The host's CPU time.
    pub cpu: HostCpu,
    /// Each running or paused container's whose cgroup could be read.
    pub containers: Vec<ContainerUsage>,
}

/// The host's memory, bytes (0: unread).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct HostMemory {
    /// In all.
    pub total: u64,
    /// What can be had without swapping (`MemAvailable`).
    pub available: u64,
}

/// The host's CPU time since boot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct HostCpu {
    /// In all its CPUs, nanoseconds.
    pub time: u64,
    /// Its CPUs online.
    pub cpus: u32,
}

/// What a container uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContainerUsage {
    /// Its ID.
    pub id: String,
    /// Its CPU time, nanoseconds.
    pub cpu_time: u64,
    /// Its memory, bytes (less `inactive_file`).
    pub memory: u64,
    /// Its monitor's memory, bytes (none: no cgroup of its own).
    pub monitor_memory: Option<u64>,
}

/// What everything uses now.
///
/// # Errors
///
/// The API's; [`crate::ClientError::Newer`] for a newer answer,
/// [`crate::ClientError::Older`] for a Shipyard that doesn't serve it;
/// [`crate::ClientError::Unreadable`] for an answer of another shape.
pub async fn usage(client: &Client) -> Result<Usage> {
    let answer = older_if_missing(
        client
            .own("/shipyard/usage", "the usage answer", USAGE_VERSION)
            .await,
    )?;
    Ok(serde_json::from_value(answer)?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The engine's keys read (a field it adds ignored).
    #[test]
    fn the_engine_s_keys_are_read() {
        let answer = serde_json::json!({
            "version": 1,
            "memory": {"total": 4096, "available": 1024},
            "cpu": {"time": 99, "cpus": 4},
            "containers": [
                {"id": "c1", "cpu_time": 7, "memory": 300, "monitor_memory": 800, "later": 1},
                {"id": "c2", "cpu_time": 7, "memory": 300, "monitor_memory": null},
            ],
        });
        let usage: Usage = serde_json::from_value(answer).unwrap();
        assert_eq!(usage.memory.available, 1024);
        assert_eq!(usage.cpu.cpus, 4);
        assert_eq!(usage.containers[0].cpu_time, 7);
        assert_eq!(usage.containers[0].monitor_memory, Some(800));
        assert_eq!(usage.containers[1].monitor_memory, None);
    }
}
