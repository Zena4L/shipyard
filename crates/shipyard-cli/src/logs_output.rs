//! Printing a container's logs, for `shipyard logs` and `shipyard-runtime
//! logs` alike: the program's stdout to stdout, its stderr to stderr, each
//! line as it was (with its time first, if asked). The engine reads them.

use shipyard_engine::logs::{Entry, LogsError, ReadSummary};
use shipyard_runtime::logs::cri::Stream;

/// Prints what `read` hands on (`timestamps`: each line's time first):
/// how many lines weren't records (skipped). A reader that closes its end
/// (`logs NAME | head`) has seen what it wanted: not an error.
///
/// # Errors
///
/// What `read` fails with, stdout or stderr aside.
pub(crate) fn printed(
    timestamps: bool,
    read: impl FnOnce(
        &mut dyn FnMut(Entry) -> std::io::Result<()>,
    ) -> shipyard_engine::Result<ReadSummary>,
) -> anyhow::Result<usize> {
    let (mut stdout, mut stderr) = (std::io::stdout().lock(), std::io::stderr().lock());
    // Whether each stream is in the middle of a line (a piece printed, its
    // rest to come): a time starts lines only.
    let mut midline = [false, false];
    let mut sink = |entry: Entry| {
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
    };
    // A reader gone (`logs | head`) ends it quietly. Here, not in
    // `shipyard`'s `fail` alone: `shipyard-runtime logs` shares this file,
    // not that `main`.
    match read(&mut sink) {
        Ok(read) => Ok(read.skipped),
        Err(error)
            if error.find::<LogsError>().is_some_and(|error| {
                matches!(error, LogsError::Sink(io) if io.kind() == std::io::ErrorKind::BrokenPipe)
            }) =>
        {
            Ok(0)
        }
        Err(error) => Err(error.into()),
    }
}

/// The warning for `skipped` lines that weren't records (none if none were).
pub(crate) fn skipped_warning(skipped: usize) -> Option<String> {
    let lines = match skipped {
        0 => return None,
        1 => "line",
        _ => "lines",
    };
    Some(format!(
        "{skipped} {lines} in the logs weren't records (torn by a crash?): skipped"
    ))
}

/// `--since` (and `events --until`): RFC 3339, Unix seconds
/// (`1760000000`), or a duration ago (`10m`, `2h`, `30s`, `3d`).
pub(crate) fn parse_since(text: &str) -> Result<shipyard_runtime::logs::time::Timestamp, String> {
    use shipyard_runtime::logs::time::Timestamp;
    if let Ok(time) = Timestamp::parse(text) {
        return Ok(time);
    }
    let bad = || {
        format!(
            "{text:?} is neither a time (2026-10-10T06:16:19Z, or Unix seconds) nor a duration \
             (10m)"
        )
    };
    let (number, unit) = number_and_unit(text);
    let number: i64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "" => return Timestamp::from_unix(number, 0).ok_or_else(bad),
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
pub(crate) fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
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
pub(crate) fn number_and_unit(text: &str) -> (&str, &str) {
    text.find(|c: char| !c.is_ascii_digit())
        .map_or((text, ""), |at| text.split_at(at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shipyard_runtime::logs::time::Timestamp;

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
        for (text, seconds) in [("30s", 30), ("10m", 600), ("2h", 7200)] {
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
        for bad in ["", "m", "10w", "-1m", "1.5h", "yesterday"] {
            assert!(parse_since(bad).is_err(), "{bad:?}");
        }
        assert_eq!(number_and_unit("10m"), ("10", "m"));
        assert_eq!(number_and_unit("42"), ("42", ""));
    }
}
