//! The `shipyard-runtime` commands that need more than a call: the cgroup
//! ones (`ps`, `events` and `update`, the output and input runc gives them;
//! `pause`, `resume` and `kill --all` need nothing here).

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::json;
use shipyard_runtime::container::{self, ContainerError, Status};
use shipyard_runtime::resources::Resources;

use crate::limits::LimitArgs;

/// How `ps` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PsFormat {
    /// PID, user ID and command, one process per line.
    Table,
    /// A JSON array of PIDs (what engines read).
    Json,
}

/// `ps`: the processes in container `id`'s cgroup, by host PID.
///
/// # Errors
///
/// No such container, one that's stopped or has no cgroup, or stdout.
pub(crate) fn ps(root: &Path, id: &str, format: PsFormat) -> Result<()> {
    let pids = container::ps(root, id)?;
    let mut out = std::io::stdout().lock();
    match format {
        PsFormat::Json => writeln!(out, "{}", serde_json::to_string(&pids)?)?,
        PsFormat::Table => {
            writeln!(out, "{:>8} {:>8}  COMMAND", "PID", "UID")?;
            for pid in pids {
                // A process gone since it was listed: left out.
                let Some((uid, command)) = describe(pid) else {
                    continue;
                };
                writeln!(out, "{pid:>8} {uid:>8}  {command}")?;
            }
        }
    }
    Ok(())
}

/// Process `pid`'s user ID and command line, from `/proc` (its name in
/// brackets, as `ps` shows it, when it has no command line: a kernel thread
/// or a zombie). `None` if it's gone.
fn describe(pid: i32) -> Option<(String, String)> {
    let proc = Path::new("/proc").join(pid.to_string());
    let status = std::fs::read_to_string(proc.join("status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
    };
    let uid = field("Uid:")?.split_whitespace().next()?.to_owned();
    let cmdline = std::fs::read(proc.join("cmdline")).ok()?;
    let command = if cmdline.is_empty() {
        format!("[{}]", field("Name:").unwrap_or_default())
    } else {
        cmdline
            .split(|&byte| byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some((uid, command))
}

/// `events`: container `id`'s usage as runc's JSON lines, every `interval`
/// until it stops, or once with `once` (`--stats`); an `oom` line whenever
/// the OOM killer struck since the last.
///
/// The OOM count is read each tick: an OOM kill that ends the container (its
/// init) shows in its exit record instead, the container gone before the
/// next tick.
///
/// # Errors
///
/// No such container, one without a cgroup, or stdout.
pub(crate) fn events(root: &Path, id: &str, once: bool, interval: Duration) -> Result<()> {
    let mut kills: Option<u64> = None;
    loop {
        let (stats, oom_kills) = match container::stats(root, id) {
            Ok(read) => read,
            // It stopped (or was deleted too) since the last tick: the
            // stream ends there.
            Err(
                ContainerError::WrongStatus {
                    status: Status::Stopping | Status::Stopped,
                    ..
                }
                | ContainerError::NotFound(_),
            ) if kills.is_some() => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let mut out = std::io::stdout().lock();
        if kills.is_some_and(|before| oom_kills > before) {
            writeln!(out, "{}", json!({"type": "oom", "id": id}))?;
        }
        kills = Some(oom_kills);
        writeln!(out, "{}", json!({"type": "stats", "id": id, "data": stats}))?;
        out.flush()?;
        // Not held across the wait: nothing else may need stdout, but a lock
        // held while sleeping is a trap for whoever adds something that does.
        drop(out);
        if once {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

/// `update`: container `id`'s new limits, from a `linux.resources` JSON
/// object (`file`, `-` for stdin) or from the flags, as runc takes them.
/// Returns what was skipped, to warn about.
///
/// # Errors
///
/// No limits given, a file that can't be read, a value that can't work, or
/// what [`container::update`] refuses.
pub(crate) fn update(
    root: &Path,
    id: &str,
    file: Option<&Path>,
    flags: &LimitArgs,
) -> Result<Vec<String>> {
    let resources = match file {
        Some(file) => resources_from(file)?,
        // `--memory-swap` alone is memory plus swap over the container's
        // memory limit now, as runc takes it.
        None if flags.swap_without_memory() => {
            flags.resources_as_given(container::memory_limit(root, id)?)?
        }
        None => flags.resources_as_given(None)?,
    };
    if resources.is_empty() {
        bail!("no limits given: --resources FILE, or flags such as --memory");
    }
    Ok(container::update(root, id, &resources)?)
}

/// A `linux.resources` object read from `file` (`-`: stdin).
fn resources_from(file: &Path) -> Result<Resources> {
    let mut text = String::new();
    if file == Path::new("-") {
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading the resources from stdin")?;
    } else {
        text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    }
    Ok(shipyard_runtime::oci::resources_from_json(&text, file)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_described_from_proc() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let pid = i32::try_from(std::process::id()).unwrap();
        let (uid, command) = describe(pid).unwrap();
        // SAFETY: `getuid` has no preconditions.
        assert_eq!(uid, unsafe { libc::getuid() }.to_string());
        assert!(!command.is_empty());
        assert_eq!(describe(i32::MAX), None, "no such process");
    }
}
