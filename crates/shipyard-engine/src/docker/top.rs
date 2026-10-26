//! `docker top` (step 11.3d2): a container's processes, as Docker's daemon
//! shows them with the host's `ps -ef`, read from `/proc` here (D11.3d-2):
//! its PIDs from its cgroup, each one's `stat`, `status` and `cmdline`.

use serde::Serialize;

use crate::Engine;
use crate::error::Result;

/// `ps -ef`'s columns, Docker's `top`'s by default.
pub const TOP_TITLES: [&str; 8] = ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"];

/// A container's processes, Docker's `ContainerTopResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub struct Top {
    /// The columns' titles.
    pub titles: Vec<String>,
    /// A row per process, in the titles' order.
    pub processes: Vec<Vec<String>>,
}

/// The processes of the container `reference` names, in `ps -ef`'s columns.
/// One that ends while it's read is left out.
///
/// # Errors
///
/// No such container; one that isn't running or paused
/// ([`ErrorKind::NotRunning`](crate::ErrorKind::NotRunning)); one without
/// a cgroup of its own (rootless without delegation); off Linux,
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
pub fn top(engine: &Engine, reference: &str) -> Result<Top> {
    #[cfg(target_os = "linux")]
    return linux::top(engine, reference);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (engine, reference);
        Err(crate::error::linux_only("containers"))
    }
}

/// A terminal's name as `ps` gives it, from its device number: `pts/3`,
/// `tty1`, or `?` (none, or one `ps` wouldn't name).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read by `top`, which is Linux's (and the tests)")
)]
fn tty_name(device: u64) -> String {
    let major = (device >> 8) & 0xfff;
    let minor = (device & 0xff) | ((device >> 12) & 0xfff00);
    match major {
        // None (0), or one `ps` wouldn't name: `?`.
        136..=143 => format!("pts/{}", (major - 136) * 256 + minor),
        4 if minor < 64 => format!("tty{minor}"),
        _ => "?".to_owned(),
    }
}

/// `ps -ef`'s `C`: the CPU a process has used over its life, in percent
/// of one CPU, counted in clock ticks before it's divided (procps'
/// `pcpu`), at most 99 (as procps shows it).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read by `top`, which is Linux's (and the tests)")
)]
fn cpu_share(used_ticks: u64, ticks: u64, alive_seconds: u64) -> u64 {
    (used_ticks.saturating_mul(100) / ticks.max(1) / alive_seconds.max(1)).min(99)
}

/// CPU time as `ps -ef`'s `TIME` writes it: `HH:MM:SS`, `D-HH:MM:SS` past a
/// day.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read by `top`, which is Linux's (and the tests)")
)]
fn cpu_time(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let clock = format!(
        "{:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    if days > 0 {
        format!("{days}-{clock}")
    } else {
        clock
    }
}

/// A start as `ps -ef`'s `STIME` writes it (UTC): `HH:MM` today, else the
/// month and day (`Oct20`).
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "read by `top`, which is Linux's (and the tests)")
)]
fn start_time(started: i64, now: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some(stamp) = shipyard_runtime::logs::time::Timestamp::from_unix(started, 0) else {
        return "?".to_owned();
    };
    // `YYYY-MM-DDTHH:MM:SS…`
    let text = stamp.to_string();
    if started.div_euclid(86_400) == now.div_euclid(86_400) {
        return text.get(11..16).unwrap_or("?").to_owned();
    }
    let month = text
        .get(5..7)
        .and_then(|month| month.parse::<usize>().ok())
        .and_then(|month| MONTHS.get(month.checked_sub(1)?));
    match (month, text.get(8..10)) {
        (Some(month), Some(day)) => format!("{month}{day}"),
        _ => "?".to_owned(),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;

    use super::{TOP_TITLES, Top, cpu_share, cpu_time, start_time, tty_name};
    use crate::Engine;
    use crate::containers::{self, ContainerState};
    use crate::docker::proc;
    use crate::error::{Result, bail};

    /// The most of a `/proc` file read: a long command line is cut.
    const PROC_READ_MAX: u64 = 64 << 10;

    pub(super) fn top(engine: &Engine, reference: &str) -> Result<Top> {
        let record = containers::find(engine, reference)?;
        if !matches!(
            containers::state(engine, &record)?,
            ContainerState::Running { .. } | ContainerState::Paused
        ) {
            bail!(NotRunning, "container {} is not running", record.name);
        }
        let root = engine.paths.runtime_root()?;
        let pids = shipyard_runtime::container::ps(root, record.id.as_str())?;
        let host = Host::now();
        let processes = pids
            .into_iter()
            .filter_map(|pid| host.process(pid))
            .collect();
        Ok(Top {
            titles: TOP_TITLES.iter().map(|&title| title.to_owned()).collect(),
            processes,
        })
    }

    /// What every process's columns are counted against.
    struct Host {
        /// Clock ticks a second.
        ticks: u64,
        /// When the host booted, in seconds since the epoch.
        boot: i64,
        /// Now, in seconds since the epoch.
        now: i64,
        /// Users' names by UID, each looked up once.
        users: std::cell::RefCell<HashMap<u32, String>>,
    }

    impl Host {
        fn now() -> Self {
            let seconds = |at: std::time::SystemTime| {
                at.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| {
                    i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
                })
            };
            Self {
                ticks: proc::clock_ticks(),
                boot: crate::run::boot_time().map_or(0, seconds),
                now: seconds(std::time::SystemTime::now()),
                users: std::cell::RefCell::default(),
            }
        }

        /// The name of user `uid` (its number without one), as `ps` shows
        /// the effective user.
        fn user(&self, uid: u32) -> String {
            self.users
                .borrow_mut()
                .entry(uid)
                .or_insert_with(|| {
                    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                        .ok()
                        .flatten()
                        .map_or_else(|| uid.to_string(), |user| user.name)
                })
                .clone()
        }

        /// Process `pid`'s row, if it's still there.
        fn process(&self, pid: i32) -> Option<Vec<String>> {
            let dir = format!("/proc/{pid}");
            // Bytes: a process may name itself what isn't text.
            let stat = shipyard_sys::proc::stat(pid)?;
            // `Uid:`'s real, effective, saved and filesystem UIDs: `ps -ef`
            // shows the effective one.
            let user = read(&format!("{dir}/status"))
                .and_then(|text| {
                    text.lines().find_map(|line| {
                        line.strip_prefix("Uid:")?
                            .split_whitespace()
                            .nth(1)?
                            .parse::<u32>()
                            .ok()
                    })
                })
                .map_or_else(|| "?".to_owned(), |uid| self.user(uid));
            let used = stat.utime.saturating_add(stat.stime);
            let started = self.boot + i64::try_from(stat.start / self.ticks).unwrap_or(0);
            let alive = u64::try_from(self.now - started).unwrap_or(0);
            let command = read(&format!("{dir}/cmdline"))
                .map(|line| {
                    line.split('\0')
                        .filter(|word| !word.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|line| !line.is_empty())
                .or_else(|| {
                    read(&format!("{dir}/comm")).map(|name| format!("[{}]", name.trim()))
                })?;
            Some(vec![
                user,
                pid.to_string(),
                stat.ppid.to_string(),
                cpu_share(used, self.ticks, alive).to_string(),
                start_time(started, self.now),
                tty_name(u64::try_from(stat.tty).unwrap_or(0)),
                cpu_time(used / self.ticks),
                command,
            ])
        }
    }

    /// A process's `/proc` file, at most [`PROC_READ_MAX`] of it.
    fn read(path: &str) -> Option<String> {
        proc::read(path, PROC_READ_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminals, CPU time and start times as `ps -ef` writes them.
    #[test]
    fn columns_read_as_ps_s() {
        assert_eq!(tty_name(0), "?");
        assert_eq!(tty_name(34_816), "pts/0", "136:0");
        assert_eq!(tty_name(34_819), "pts/3");
        assert_eq!(tty_name(1025), "tty1", "4:1");
        // 0.9 s of CPU over a second's life is 90, not 0; never past 99.
        assert_eq!(cpu_share(90, 100, 1), 90);
        assert_eq!(cpu_share(250, 100, 1), 99);
        assert_eq!(cpu_share(150, 100, 3), 50);
        assert_eq!(cpu_share(5, 100, 0), 5, "a process younger than a second");
        assert_eq!(cpu_time(0), "00:00:00");
        assert_eq!(cpu_time(3_725), "01:02:05");
        assert_eq!(cpu_time(90_061), "1-01:01:01");
        let now = 1_792_504_484; // 2026-10-20T13:54:44Z
        assert_eq!(start_time(now - 60, now), "13:53");
        assert_eq!(start_time(now - 3 * 86_400, now), "Oct17");
    }
}
