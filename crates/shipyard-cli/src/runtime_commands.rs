//! The `shipyard-runtime` commands that need more than a call: the cgroup
//! ones (stage 5, step 5: `ps`, `events` and `update`, the output and input
//! runc gives them; `pause`, `resume` and `kill --all` need nothing here),
//! and `logs` (stage 8, read by the engine).

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
                    status: Status::Stopped,
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

/// Whose logs `logs` reads.
#[derive(Debug, Clone)]
pub(crate) enum LogsSource {
    /// A container's, by ID.
    Container(String),
    /// Those in a directory (a container deleted since).
    Dir(std::path::PathBuf),
}

/// `logs`: prints the logs `target` names, as `options` say; the program's
/// stdout to stdout, its stderr to stderr. Returns how many lines weren't
/// records (skipped).
///
/// # Errors
///
/// No such container, one without logs, logs that can't be read, or
/// stdout or stderr.
pub(crate) fn logs(
    root: &Path,
    target: &LogsSource,
    options: &shipyard_engine::logs::Options,
    timestamps: bool,
) -> Result<usize> {
    use shipyard_runtime::logs::cri::Stream;
    let (dir, identity, instance) = match target {
        LogsSource::Container(id) => {
            let logs = container::logs_of(root, id)?;
            (logs.dir, logs.identity, logs.instance)
        }
        LogsSource::Dir(dir) => (dir.clone(), None, None),
    };
    // Only the directory the container was created with: not another put
    // at its path since.
    let options = shipyard_engine::logs::Options {
        identity,
        ..*options
    };
    // Done once the monitor has written the exit: everything is in. A
    // container deleted meanwhile is done too, and so is one whose ID now
    // names another container's logs (deleted and created again, at the
    // same path or another: another monitor, whatever inode its directory
    // got). Logs read by directory have nothing to say
    // when they're done: followed until interrupted.
    let finished = || match target {
        LogsSource::Container(id) => container::logs_of(root, id).map_or(true, |logs| {
            logs.finished
                || logs.dir != dir
                || logs.identity != identity
                || logs.instance != instance
        }),
        LogsSource::Dir(_) => false,
    };
    let (mut stdout, mut stderr) = (std::io::stdout().lock(), std::io::stderr().lock());
    // Whether each stream is in the middle of a line (a piece printed, its
    // rest to come): a time starts lines only.
    let mut midline = [false, false];
    let read = shipyard_engine::logs::read(&dir, &options, finished, |entry| {
        let out: &mut dyn std::io::Write = match entry.stream {
            Stream::Stdout => &mut stdout,
            Stream::Stderr => &mut stderr,
        };
        let midline = &mut midline[entry.stream.index()];
        if timestamps && !*midline {
            write!(out, "{} ", entry.time)?;
        }
        out.write_all(&entry.text)?;
        if entry.complete {
            out.write_all(b"\n")?;
        }
        *midline = !entry.complete;
        out.flush()
    });
    match read {
        Ok(read) => Ok(read.skipped),
        // `logs ID | head`: the reader has seen what it wanted.
        Err(shipyard_engine::logs::LogsError::Sink(err))
            if err.kind() == std::io::ErrorKind::BrokenPipe =>
        {
            Ok(0)
        }
        Err(err) => Err(err.into()),
    }
}

/// `--since`: RFC 3339, or a duration ago (`10m`, `2h`, `30s`).
pub(crate) fn parse_since(text: &str) -> Result<shipyard_runtime::logs::time::Timestamp, String> {
    use shipyard_runtime::logs::time::Timestamp;
    if let Ok(time) = Timestamp::parse(text) {
        return Ok(time);
    }
    let bad = || format!("{text:?} is neither a time (2026-10-10T06:16:19Z) nor a duration (10m)");
    let (number, unit) = number_and_unit(text);
    let number: i64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "s" => number,
        "m" => number.saturating_mul(60),
        "h" => number.saturating_mul(3600),
        _ => return Err(bad()),
    };
    let now = Timestamp::now();
    Timestamp::from_unix(now.secs().saturating_sub(seconds), now.nanos()).ok_or_else(bad)
}

/// `text`'s leading digits, and the rest (`10m`: `10`, `m`).
fn number_and_unit(text: &str) -> (&str, &str) {
    text.find(|c: char| !c.is_ascii_digit())
        .map_or((text, ""), |at| text.split_at(at))
}

/// `--interval`: a number of seconds or milliseconds, as runc's (Go)
/// durations most often are: `5s`, `500ms`, `1m`.
pub(crate) fn parse_interval(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} isn't a duration (such as 5s or 500ms)");
    let (number, unit) = number_and_unit(text);
    let number: u64 = number.parse().map_err(|_| bad())?;
    let interval = match unit {
        "ms" => Duration::from_millis(number),
        "s" | "" => Duration::from_secs(number),
        "m" => Duration::from_secs(number.saturating_mul(60)),
        _ => return Err(bad()),
    };
    if interval.is_zero() {
        return Err(format!("{text:?}: the interval can't be zero"));
    }
    Ok(interval)
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
    fn intervals_read_as_runc_writes_them() {
        assert_eq!(parse_interval("5s"), Ok(Duration::from_secs(5)));
        assert_eq!(parse_interval("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_interval("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_interval("3"), Ok(Duration::from_secs(3)));
        for bad in ["", "s", "5h", "1.5s", "-1s", "0s"] {
            assert!(parse_interval(bad).is_err(), "{bad}");
        }
    }

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
