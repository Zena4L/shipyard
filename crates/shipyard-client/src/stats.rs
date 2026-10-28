//! A container's live numbers (step 14.4a): Docker's stats stream for the
//! inspector's Resources, each sample said as Docker's CLI says it: CPU as a
//! percent of one CPU (`cpu_delta / system_delta × online CPUs × 100`, both
//! deltas from the sample before, which the API gives as `precpu_stats`),
//! memory used less what the kernel can drop at once (`inactive_file`), and
//! the limit.

use futures_util::StreamExt as _;
use serde::Serialize;

use crate::api::Client;
use crate::{ClientError, Result};

/// One sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Sample {
    /// CPU, percent of one CPU (four busy CPUs: 400).
    pub cpu: f64,
    /// Memory in use, bytes.
    pub mem: u64,
    /// The memory limit, bytes (the host's when the container has none).
    pub limit: u64,
}

/// A stats answer as a [`Sample`].
#[must_use]
pub fn sample(stats: &bollard::models::ContainerStatsResponse) -> Sample {
    let memory = stats.memory_stats.as_ref();
    let usage = memory.and_then(|memory| memory.usage).unwrap_or(0);
    let inactive = memory
        .and_then(|memory| memory.stats.as_ref())
        .and_then(|stats| stats.get("inactive_file").copied())
        .unwrap_or(0);
    Sample {
        cpu: cpu_percent(stats),
        mem: usage.saturating_sub(inactive),
        limit: memory.and_then(|memory| memory.limit).unwrap_or(0),
    }
}

/// CPU as Docker's CLI computes it: 0 for a first sample (no `precpu`), or
/// when the system's time didn't move.
#[must_use]
pub fn cpu_percent(stats: &bollard::models::ContainerStatsResponse) -> f64 {
    let total = |cpu: Option<&bollard::models::ContainerCpuStats>| {
        cpu.and_then(|cpu| cpu.cpu_usage.as_ref())
            .and_then(|usage| usage.total_usage)
            .unwrap_or(0)
    };
    let system = |cpu: Option<&bollard::models::ContainerCpuStats>| {
        cpu.and_then(|cpu| cpu.system_cpu_usage).unwrap_or(0)
    };
    let (now, before) = (stats.cpu_stats.as_ref(), stats.precpu_stats.as_ref());
    let cpu_delta = total(now).saturating_sub(total(before));
    let system_delta = system(now).saturating_sub(system(before));
    if system_delta == 0 || system(before) == 0 {
        return 0.0;
    }
    let cpus = now.and_then(|cpu| cpu.online_cpus).unwrap_or(1).max(1);
    #[expect(
        clippy::cast_precision_loss,
        reason = "nanoseconds well within f64's exact range for a ratio"
    )]
    let ratio = cpu_delta as f64 / system_delta as f64;
    ratio * f64::from(cpus) * 100.0
}

/// Follows container `id`'s stats, each sample handed to `on_sample`,
/// until the stream ends: its end's reason.
///
/// # Errors
///
/// [`ClientError::StreamEnded`] at its end (the container stopped);
/// [`ClientError::BadReference`] for an ID of another shape; the API's.
pub async fn follow(client: &Client, id: &str, mut on_sample: impl FnMut(Sample)) -> Result<()> {
    let options = bollard::query_parameters::StatsOptionsBuilder::default()
        .stream(true)
        .build();
    let id = crate::api::container_ref(id)?;
    let mut stats = client.docker().stats(id, Some(options));
    while let Some(stats) = stats.next().await {
        on_sample(sample(&stats?));
    }
    Err(ClientError::StreamEnded)
}

#[cfg(test)]
mod tests {
    use bollard::models::{
        ContainerCpuStats, ContainerCpuUsage, ContainerMemoryStats, ContainerStatsResponse,
    };

    use super::*;

    fn cpu(total: u64, system: u64, cpus: u32) -> ContainerCpuStats {
        ContainerCpuStats {
            cpu_usage: Some(ContainerCpuUsage {
                total_usage: Some(total),
                ..ContainerCpuUsage::default()
            }),
            system_cpu_usage: Some(system),
            online_cpus: Some(cpus),
            ..ContainerCpuStats::default()
        }
    }

    /// Docker's CLI's numbers: a quarter of the system's time on a host of
    /// four CPUs is 100 % (one CPU busy); a first sample, 0; memory less
    /// `inactive_file`.
    #[test]
    fn a_sample_is_said_as_docker_s_cli_says_it() {
        let stats = ContainerStatsResponse {
            cpu_stats: Some(cpu(2_500, 20_000, 4)),
            precpu_stats: Some(cpu(500, 12_000, 4)),
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(300),
                limit: Some(1000),
                stats: Some([("inactive_file".to_owned(), 100)].into_iter().collect()),
                ..ContainerMemoryStats::default()
            }),
            ..ContainerStatsResponse::default()
        };
        let got = sample(&stats);
        assert!((got.cpu - 100.0).abs() < 1e-9, "{got:?}");
        assert_eq!((got.mem, got.limit), (200, 1000));
        let first = ContainerStatsResponse {
            cpu_stats: Some(cpu(2_500, 20_000, 4)),
            precpu_stats: Some(ContainerCpuStats::default()),
            ..ContainerStatsResponse::default()
        };
        assert!(cpu_percent(&first).abs() < f64::EPSILON);
    }
}
