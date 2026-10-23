//! Printing a container's logs, for `shipyard logs` and `shipyard-runtime
//! logs` alike: the program's stdout to stdout, its stderr to stderr, each
//! line as it was (with its time first, if asked). The runtime's reader
//! reads them (by name, through the engine).

use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::logs::reader::{Entry, LineWriter, LogsError, ReadSummary};

/// Prints what `read` hands on (`timestamps`: each line's time first):
/// how many lines weren't records (skipped). A reader of stdout that
/// closes its end (`logs NAME | head`, `2>&1 | head` too) has seen what it
/// wanted: not an error. Stderr's reader gone while stdout's is there is a
/// failure, as any other (F70): asked of the kernel, whichever stream's
/// write failed.
///
/// # Errors
///
/// What `read` fails with, stdout's reader gone aside.
pub fn printed(
    timestamps: bool,
    read: impl FnOnce(&mut dyn FnMut(Entry) -> std::io::Result<()>) -> anyhow::Result<ReadSummary>,
) -> anyhow::Result<usize> {
    let (mut stdout, mut stderr) = (std::io::stdout().lock(), std::io::stderr().lock());
    let mut lines = LineWriter::new(timestamps);
    let mut sink = |entry: Entry| {
        let out: &mut dyn std::io::Write = match entry.stream {
            Stream::Stdout => &mut stdout,
            Stream::Stderr => &mut stderr,
        };
        out.write_all(&lines.bytes(&entry))?;
        out.flush()
    };
    // Stdout's reader gone (`logs | head`) ends it quietly. Here, not in
    // `shipyard`'s `fail` alone: `shipyard-runtime logs` shares this file,
    // not that `main`.
    match read(&mut sink) {
        Ok(read) => Ok(read.skipped),
        Err(error) if quiet(&error, crate::pipe::stdout_gone()) => Ok(0),
        Err(error) => Err(error),
    }
}

/// Whether a read that failed with `error` ends quietly: the sink's
/// `EPIPE`, with stdout's reader gone (`stdout_gone`).
fn quiet(error: &anyhow::Error, stdout_gone: bool) -> bool {
    stdout_gone && is_sink_epipe(error)
}

/// Whether `error` is the sink's `EPIPE`: the runtime's own
/// ([`LogsError::Sink`], `shipyard-runtime logs`), or in the engine's
/// error (`shipyard logs`).
fn is_sink_epipe(error: &anyhow::Error) -> bool {
    let logs = error.downcast_ref::<LogsError>().or_else(|| {
        error
            .downcast_ref::<shipyard_engine::Error>()
            .and_then(shipyard_engine::Error::find::<LogsError>)
    });
    matches!(logs, Some(LogsError::Sink(io)) if io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// The warning for `skipped` lines that weren't records (none if none were).
#[must_use]
pub fn skipped_warning(skipped: usize) -> Option<String> {
    let lines = match skipped {
        0 => return None,
        1 => "line",
        _ => "lines",
    };
    Some(format!(
        "{skipped} {lines} in the logs weren't records (torn by a crash?): skipped"
    ))
}

/// `--since` (and `events --until`): RFC 3339, Unix seconds with an
/// optional fraction (`1760000000.5`), or a duration ago (`10m`, `2h`,
/// `30s`, `3d`).
pub fn parse_since(text: &str) -> Result<shipyard_runtime::logs::time::Timestamp, String> {
    use shipyard_runtime::logs::time::Timestamp;
    if let Some(time) = shipyard_engine::docker::parse_time(text) {
        return Ok(time);
    }
    let bad = || {
        format!(
            "{text:?} is neither a time (2026-10-10T06:16:19Z, or Unix seconds) nor a duration \
             (10m, 1h30m)"
        )
    };
    // A Go duration, as Docker's (`1h30m`, `1.5h`, `90s`); then `d`, which
    // Shipyard took before (Go has no days).
    if let Some(seconds) = shipyard_engine::docker::filters::go_duration(text) {
        let now = Timestamp::now();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a duration's whole seconds, bounded by go_duration (a century)"
        )]
        let seconds = seconds as i64;
        return Timestamp::from_unix(now.secs().saturating_sub(seconds), now.nanos())
            .ok_or_else(bad);
    }
    let (number, unit) = number_and_unit(text);
    let number: i64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "s" => number,
        "m" => number.saturating_mul(60),
        "h" => number.saturating_mul(3600),
        "d" => number.saturating_mul(86_400),
        _ => return Err(bad()),
    };
    let now = Timestamp::now();
    Timestamp::from_unix(now.secs().saturating_sub(seconds), now.nanos()).ok_or_else(bad)
}

/// A duration, as Docker's and runc's (Go) durations most often are: a
/// number and its unit, `500ms`, `5s` (or `5`), `1m`, `2h`; never zero.
pub fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    use std::time::Duration;
    let bad = || format!("{text:?} isn't a duration (such as 5s, 500ms or 1m)");
    let (number, unit) = number_and_unit(text);
    let number: u64 = number.parse().map_err(|_| bad())?;
    let duration = match unit {
        "ms" => Duration::from_millis(number),
        "s" | "" => Duration::from_secs(number),
        "m" => Duration::from_secs(number.saturating_mul(60)),
        "h" => Duration::from_secs(number.saturating_mul(3600)),
        _ => return Err(bad()),
    };
    if duration.is_zero() {
        return Err(format!("{text:?}: a duration can't be zero"));
    }
    Ok(duration)
}

/// `text`'s leading digits, and the rest (`10m`: `10`, `m`).
fn number_and_unit(text: &str) -> (&str, &str) {
    text.find(|c: char| !c.is_ascii_digit())
        .map_or((text, ""), |at| text.split_at(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    use shipyard_runtime::logs::time::Timestamp;

    /// Only stdout's reader gone is quiet: the sink's `EPIPE` with stdout
    /// still read (stderr's reader gone: its lines lost while stdout's go
    /// on) isn't, nor another failure with stdout's gone (F70).
    #[test]
    fn only_stdouts_gone_reader_is_quiet() {
        let pipe = || anyhow::Error::from(LogsError::Sink(std::io::ErrorKind::BrokenPipe.into()));
        assert!(quiet(&pipe(), true));
        assert!(!quiet(&pipe(), false));
        let replaced = anyhow::Error::from(LogsError::Replaced {
            dir: "/logs".into(),
        });
        assert!(!quiet(&replaced, true));
    }

    /// The sink's `EPIPE` is found whether the runtime's error says so
    /// (`shipyard-runtime logs`) or the engine's holds it (`shipyard logs`);
    /// any other failure isn't one.
    #[test]
    fn a_sinks_epipe_is_found_in_either_error() {
        let pipe = || LogsError::Sink(std::io::ErrorKind::BrokenPipe.into());
        assert!(is_sink_epipe(&anyhow::Error::from(pipe())));
        assert!(is_sink_epipe(&anyhow::Error::from(
            shipyard_engine::Error::from(pipe())
        )));
        let denied = || LogsError::Sink(std::io::ErrorKind::PermissionDenied.into());
        assert!(!is_sink_epipe(&anyhow::Error::from(denied())));
        assert!(!is_sink_epipe(&anyhow::Error::from(
            shipyard_engine::Error::from(denied())
        )));
        let replaced = LogsError::Replaced {
            dir: "/logs".into(),
        };
        assert!(!is_sink_epipe(&anyhow::Error::from(replaced)));
    }

    /// `--interval` and the `--health-*` times: as runc and Docker write
    /// them.
    #[test]
    fn durations_read_as_runc_writes_them() {
        use std::time::Duration;
        assert_eq!(parse_duration("5s"), Ok(Duration::from_secs(5)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("3"), Ok(Duration::from_secs(3)));
        for bad in ["", "s", "5d", "1.5s", "-1s", "0s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    /// `--since`: a time as written, or that long ago; nothing else.
    #[test]
    fn since_is_a_time_or_a_duration_ago() {
        assert_eq!(
            parse_since("2026-10-10T06:16:19Z").unwrap(),
            Timestamp::parse("2026-10-10T06:16:19Z").unwrap()
        );
        // Go's durations, as Docker's: `1h30m` and `1.5h` too.
        for (text, seconds) in [
            ("30s", 30),
            ("10m", 600),
            ("2h", 7200),
            ("1h30m", 5400),
            ("1.5h", 5400),
        ] {
            let before = Timestamp::now().secs();
            let ago = before - parse_since(text).unwrap().secs();
            assert!((seconds..=seconds + 1).contains(&ago), "{text}: {ago}");
        }
        assert_eq!(
            parse_since("1760000000").unwrap(),
            Timestamp::from_unix(1_760_000_000, 0).unwrap()
        );
        let ago = Timestamp::now().secs() - parse_since("3d").unwrap().secs();
        assert!((259_200..=259_201).contains(&ago), "3d: {ago}");
        // Docker's clients send seconds with a fraction.
        assert_eq!(
            parse_since("1760000000.5").unwrap(),
            Timestamp::from_unix(1_760_000_000, 500_000_000).unwrap()
        );
        for bad in ["", "m", "10w", "-1m", "yesterday", "-1", "1760000000."] {
            assert!(parse_since(bad).is_err(), "{bad:?}");
        }
        assert_eq!(number_and_unit("10m"), ("10", "m"));
        assert_eq!(number_and_unit("42"), ("42", ""));
    }
}
